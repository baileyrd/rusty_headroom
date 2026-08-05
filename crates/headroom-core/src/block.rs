//! The unit of content a transform operates on.
//!
//! A [`Block`] is one entry in a message's content array — a text block, a tool
//! result, a thinking block, and so on. Transforms mutate a block's *content* in
//! place and can never touch anything else about it.
//!
//! Two design choices here carry invariants I6 and I8, and both are enforced by the
//! type rather than by review discipline.

use std::fmt;

/// What kind of content a block carries, as the provider wire format labels it.
///
/// The distinction that matters most is [`BlockKind::is_sacrosanct`]: some block
/// kinds carry provider-generated cryptographic material, and touching them at all
/// invalidates the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    /// Plain text authored by the user or the model.
    Text,
    /// The result of a tool call, returned to the model.
    ToolResult,
    /// The output of a function call, in the OpenAI Responses item shape.
    ///
    /// Semantically the same thing as [`BlockKind::ToolResult`] — bulky text coming
    /// back from a tool — but a distinct item type on the wire, so the live-zone
    /// dispatcher has to recognize it by name.
    FunctionCallOutput,
    /// The output of a local shell call, in the OpenAI Responses item shape.
    ///
    /// Note this is the *output*. The corresponding `local_shell_call.action.command`
    /// argv array is passthrough-only and is not modeled as a compressible block.
    LocalShellCallOutput,
    /// The output of an apply-patch call, in the OpenAI Responses item shape.
    ///
    /// The output is compressible; the V4A patch body in the *call* is not, and is
    /// not modeled here.
    ApplyPatchCallOutput,
    /// A tool invocation emitted by the model.
    ToolUse,
    /// Extended-thinking content carrying a provider `signature`.
    Thinking,
    /// Thinking content the provider redacted, carrying opaque `data`.
    RedactedThinking,
    /// A reasoning item carrying `encrypted_content`.
    Reasoning,
    /// A conversation-compaction item carrying `encrypted_content`.
    Compaction,
    /// An image, document, or other binary attachment.
    Attachment,
}

impl BlockKind {
    /// Whether this block is passthrough-only under invariant I8.
    ///
    /// Thinking signatures, redacted-thinking payloads, and encrypted reasoning or
    /// compaction content are provider-generated and cryptographically bound to the
    /// conversation. They are never inspected, never decoded, and never transformed
    /// — a single mutated byte invalidates them, and the failure surfaces later as a
    /// provider error rather than as anything traceable to compression.
    ///
    /// Attachments are included because the architecture puts images, base64 blobs,
    /// and audio out of scope for compression entirely.
    pub fn is_sacrosanct(self) -> bool {
        matches!(
            self,
            Self::Thinking
                | Self::RedactedThinking
                | Self::Reasoning
                | Self::Compaction
                | Self::Attachment
        )
    }

    /// Whether a transform may ever be offered this block.
    pub fn is_compressible(self) -> bool {
        matches!(
            self,
            Self::Text
                | Self::ToolResult
                | Self::FunctionCallOutput
                | Self::LocalShellCallOutput
                | Self::ApplyPatchCallOutput
        )
    }

    /// Whether this block carries output returned from a tool.
    ///
    /// Tool output is where the tokens actually are — a directory listing, a test
    /// run, a search result set. It is the primary target of compression, and the
    /// live zone is defined largely in terms of it.
    pub fn is_tool_output(self) -> bool {
        matches!(
            self,
            Self::ToolResult
                | Self::FunctionCallOutput
                | Self::LocalShellCallOutput
                | Self::ApplyPatchCallOutput
        )
    }

    /// A stable identifier for telemetry and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::ToolResult => "tool_result",
            Self::FunctionCallOutput => "function_call_output",
            Self::LocalShellCallOutput => "local_shell_call_output",
            Self::ApplyPatchCallOutput => "apply_patch_call_output",
            Self::ToolUse => "tool_use",
            Self::Thinking => "thinking",
            Self::RedactedThinking => "redacted_thinking",
            Self::Reasoning => "reasoning",
            Self::Compaction => "compaction",
            Self::Attachment => "attachment",
        }
    }
}

impl fmt::Display for BlockKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry in a message's content array.
///
/// # Why the fields are private
///
/// Invariant I6 says compression is *position-preserving*: it never reorders blocks,
/// never splits one block into several, and never adds fields to an existing block.
/// The sibling fields below — [`kind`](Block::kind), [`tool_use_id`](Block::tool_use_id),
/// [`is_error`](Block::is_error) — are what identify a block to the provider, and a
/// transform that changed any of them would break the association between a tool
/// result and the call it answers.
///
/// So transforms get [`content_mut`](Block::content_mut), and that is all they get.
/// Everything else is read-only from a transform's perspective. "Do not touch the
/// sibling fields" stops being a rule anyone has to remember and becomes something
/// the compiler will not allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    kind: BlockKind,
    content: String,
    tool_use_id: Option<String>,
    is_error: bool,
    /// The question this content answers, when a caller had one to give.
    ///
    /// Read by compressors that can use it to decide what survives; never written
    /// back into the block's content. See [`Block::with_query`].
    query: Option<String>,
}

impl Block {
    /// Creates a block.
    pub fn new(kind: BlockKind, content: impl Into<String>) -> Self {
        Self {
            kind,
            content: content.into(),
            tool_use_id: None,
            is_error: false,
            query: None,
        }
    }

    /// Creates a tool-result block bound to the call it answers.
    pub fn tool_result(content: impl Into<String>, tool_use_id: impl Into<String>) -> Self {
        Self {
            kind: BlockKind::ToolResult,
            content: content.into(),
            tool_use_id: Some(tool_use_id.into()),
            is_error: false,
            query: None,
        }
    }

    /// Marks this block as carrying an error result.
    pub fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// Attaches the question this block's content was produced in answer to.
    ///
    /// Optional context, never required. A block without it compresses exactly as it
    /// did before this existed — which is what lets the CLI, the MCP server and the
    /// Python binding, none of which have a conversation to draw a query from, keep
    /// working unchanged.
    ///
    /// The query is *read* by compressors and never emitted: it steers what survives,
    /// and adding it to the output would put text the model never sent into the
    /// conversation.
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        let query = query.into();
        self.query = if query.trim().is_empty() {
            // An empty query is the absence of one. Storing it would make
            // `query()` return `Some("")`, and a caller checking `is_some()` would
            // take the relevance path with nothing to be relevant to.
            None
        } else {
            Some(query)
        };
        self
    }

    /// The question this block's content answers, if the caller supplied one.
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    /// The block's kind.
    pub fn kind(&self) -> BlockKind {
        self.kind
    }

    /// The block's content.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// The identifier of the tool call this block answers, if any.
    pub fn tool_use_id(&self) -> Option<&str> {
        self.tool_use_id.as_deref()
    }

    /// Whether this block carries an error result.
    ///
    /// Error results are worth compressing conservatively: a truncated stack trace
    /// is far more damaging to the model's reasoning than a truncated success
    /// payload.
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// The content's length in bytes.
    pub fn byte_len(&self) -> usize {
        self.content.len()
    }

    /// Mutable access to the content — the only mutation a transform can perform.
    ///
    /// This is deliberately the sole `&mut` accessor on the type. See the type-level
    /// documentation for why.
    pub fn content_mut(&mut self) -> &mut String {
        &mut self.content
    }

    /// Replaces the content, returning the previous value.
    ///
    /// The returned original is what the invariant I5 fallback path restores when a
    /// compression turns out not to have helped.
    pub fn replace_content(&mut self, new: impl Into<String>) -> String {
        std::mem::replace(&mut self.content, new.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sacrosanct_kinds_are_exactly_the_signed_encrypted_and_binary_ones() {
        // If a new block kind is added and this test is not updated, the default
        // should be caught here rather than in production. Signed and encrypted
        // content is unrecoverable once mutated.
        for kind in [
            BlockKind::Thinking,
            BlockKind::RedactedThinking,
            BlockKind::Reasoning,
            BlockKind::Compaction,
            BlockKind::Attachment,
        ] {
            assert!(kind.is_sacrosanct(), "{kind} must be sacrosanct");
            assert!(!kind.is_compressible(), "{kind} must not be compressible");
        }

        for kind in [BlockKind::Text, BlockKind::ToolResult] {
            assert!(!kind.is_sacrosanct(), "{kind} must not be sacrosanct");
            assert!(kind.is_compressible(), "{kind} must be compressible");
        }
    }

    #[test]
    fn tool_use_is_neither_sacrosanct_nor_compressible() {
        // tool_use carries the model's arguments as a JSON string. It is not
        // cryptographic, but re-serializing it would reorder keys and bust the
        // cache, so it is passthrough too — just for a different reason.
        assert!(!BlockKind::ToolUse.is_sacrosanct());
        assert!(!BlockKind::ToolUse.is_compressible());
    }

    #[test]
    fn sibling_fields_survive_a_content_mutation() {
        // The I6 guarantee in test form: mutating content leaves the block's
        // identity to the provider completely untouched.
        let mut block = Block::tool_result("original output", "toolu_abc123").with_error(true);

        let before_kind = block.kind();
        let before_id = block.tool_use_id().map(str::to_owned);
        let before_error = block.is_error();

        block.content_mut().push_str(" plus more");
        block.replace_content("something else entirely");

        assert_eq!(block.kind(), before_kind);
        assert_eq!(block.tool_use_id().map(str::to_owned), before_id);
        assert_eq!(block.is_error(), before_error);
    }

    #[test]
    fn replace_content_returns_the_original_for_the_fallback_path() {
        let mut block = Block::new(BlockKind::Text, "the original bytes");
        let original = block.replace_content("compressed");
        assert_eq!(original, "the original bytes");

        // Restoring is exact — invariant I1 requires the fallback to put back the
        // literal original bytes, not a re-rendering of them.
        block.replace_content(original);
        assert_eq!(block.content(), "the original bytes");
    }

    #[test]
    fn byte_len_reflects_content_only() {
        let block = Block::tool_result("12345", "toolu_with_a_very_long_identifier");
        assert_eq!(block.byte_len(), 5);
    }
}
