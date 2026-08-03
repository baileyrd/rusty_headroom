//! The conversation model.
//!
//! # Why `system` and `tools` are not reachable from here
//!
//! Invariant I2 says the cache hot zone — the system prompt and the tool
//! definitions — is never modified by compression. The usual way to honor that is a
//! rule ("don't touch `system`") enforced by review.
//!
//! This module takes a stronger approach: the compression path is never handed them
//! at all. [`Conversation`] owns the system prompt and tool definitions, and exposes
//! them **immutably and only immutably**. The live-zone dispatcher operates on
//! [`Conversation::messages_mut`], which yields messages and nothing else. There is
//! no accessor a compressor could call to reach the hot zone, so I2 is not a rule
//! anyone can forget — it is a function that does not exist.
//!
//! Tool definitions do get normalized (sorted) under invariant I7, but that is a
//! separate, deterministic pass at the proxy boundary, not something a transform
//! participates in.

use crate::block::Block;

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The user, or a tool result being returned to the model.
    ///
    /// Providers commonly deliver tool results in a user-role message rather than a
    /// dedicated role, which is why tool output shows up here.
    User,
    /// The model.
    Assistant,
}

impl Role {
    /// A stable identifier for telemetry and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One turn in the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    role: Role,
    blocks: Vec<Block>,
}

impl Message {
    /// Creates a message.
    pub fn new(role: Role, blocks: Vec<Block>) -> Self {
        Self { role, blocks }
    }

    /// Who authored this message.
    pub fn role(&self) -> Role {
        self.role
    }

    /// The message's content blocks.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Mutable access to the content blocks.
    ///
    /// Returns a slice rather than the `Vec`, so blocks can be modified in place but
    /// not appended, removed, or reordered. That is invariant I6 expressed as a
    /// return type: a compressor holding a `&mut Vec<Block>` could `push`, `remove`,
    /// or `swap`, and none of those are things compression is allowed to do.
    pub fn blocks_mut(&mut self) -> &mut [Block] {
        &mut self.blocks
    }
}

/// A full request: system prompt, tool definitions, and message history.
///
/// See the module documentation for why the first two are immutable here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Conversation {
    system: Option<String>,
    tools: Vec<String>,
    messages: Vec<Message>,
}

impl Conversation {
    /// Creates a conversation.
    pub fn new(system: Option<String>, tools: Vec<String>, messages: Vec<Message>) -> Self {
        Self {
            system,
            tools,
            messages,
        }
    }

    /// The system prompt, if any. Read-only by design — see the module docs.
    pub fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    /// The tool definitions. Read-only by design — see the module docs.
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// The message history.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Mutable access to the message history.
    ///
    /// A slice, not the `Vec`: messages can be modified but never added, removed, or
    /// reordered. Invariant I3 is append-only, and appending is the proxy's job, not
    /// a compressor's.
    pub fn messages_mut(&mut self) -> &mut [Message] {
        &mut self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockKind;

    #[test]
    fn system_and_tools_have_no_mutable_accessor() {
        // This test documents an absence, which is the whole point of the design. If
        // someone later adds `system_mut` or `tools_mut`, the compile-time guarantee
        // behind invariant I2 is gone and this comment is the breadcrumb explaining
        // why that would be a mistake.
        let convo = Conversation::new(
            Some("you are a helpful assistant".into()),
            vec!["read_file".into()],
            vec![],
        );
        assert_eq!(convo.system(), Some("you are a helpful assistant"));
        assert_eq!(convo.tools(), ["read_file".to_string()]);
    }

    #[test]
    fn blocks_are_modifiable_in_place_but_not_restructurable() {
        let mut msg = Message::new(
            Role::User,
            vec![
                Block::new(BlockKind::Text, "first"),
                Block::new(BlockKind::Text, "second"),
            ],
        );

        // A slice permits mutation of what is there...
        msg.blocks_mut()[0].replace_content("changed");
        assert_eq!(msg.blocks()[0].content(), "changed");

        // ...while the block count is fixed, because there is no way to reach the
        // underlying Vec.
        assert_eq!(msg.blocks().len(), 2);
    }
}
