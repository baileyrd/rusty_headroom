//! Request compression — where every piece finally meets.
//!
//! A real Anthropic request arrives, the live zone is identified, type-aware
//! compressors run on it, the frozen prefix is forwarded byte-identical.
//!
//! # Shape
//!
//! [`compress_request`] is a **pure function over bytes**. The axum handler is a thin
//! wrapper that calls it and forwards the result. Every invariant lives in the pure
//! function, which means every invariant is testable without a network, a mock, or a
//! running server — and the tests below are the actual guarantees, not a proxy for
//! them.
//!
//! # Passthrough is the fallback for everything
//!
//! Compression disabled, an unparseable body, no live zone, a compressor that
//! declines, a result that is not smaller — all of them forward the original bytes.
//! There is no input for which this function errors; the worst case is that it does
//! nothing.
//!
//! # `"stream": true` is not one of those cases
//!
//! It used to be. The reasoning was that compressing a stream would mean buffering it,
//! which breaks the thing the client asked for — and that reasoning conflated two
//! different bodies. The *request* body arrives complete before this function is
//! called, whatever the client wants the *response* to look like; `stream` describes
//! the response framing and says nothing about whether the request can be compressed.
//!
//! The distinction is not academic. Streaming is the common agent case, so treating it
//! as passthrough exempted most real traffic from compression while the tests
//! confirmed compression worked.

use std::borrow::Cow;
use std::sync::Arc;

use headroom_core::auth_mode::CompressionPolicy;
use headroom_core::block::{Block, BlockKind};
use headroom_core::ccr::CcrStore;
use headroom_core::conversation::{Conversation, Message, Role};
use headroom_core::live_zone::live_zone;
use headroom_core::output_shaping::{self, Verbosity};
use headroom_core::pipeline::{Orchestrator, Routing};
use headroom_core::validate::validated_apply;
use headroom_core::Transform;
use serde_json::Value;

use crate::body::FaithfulBody;
use crate::frozen::frozen_message_count;

/// The compressor set the proxy dispatches through.
///
/// A thin wrapper over [`Orchestrator`], which owns the routing decision. It used to be
/// a private dispatcher here — meaning the CLI carried its own copy and the two could
/// drift without anything failing. The decision now lives in `headroom-core`, so
/// `headroom compress` and `POST /v1/messages` route identically by construction.
pub struct Compressors {
    orchestrator: Orchestrator,
    /// Memories to inject into the live-zone tail, loaded once at startup.
    ///
    /// Empty unless configured, and an empty store injects nothing — so a proxy with no
    /// memory file behaves exactly as it did before injection existed.
    memories: headroom_core::memory::MemoryStore,
    /// How many memories one injection may carry.
    memory_limit: usize,
    /// A linked `rusty_remind_me` backend, resolved once at startup, if configured.
    ///
    /// Takes precedence over `memories` when present — see [`Self::memory_candidates`].
    /// `None` covers both "not built with the `linked-memory` feature" and "the feature
    /// is on but `HEADROOM_LINKED_MEMORY_DB` is unset", the same "unconfigured is off"
    /// shape every other optional backend in this crate uses.
    linked_memory: Option<Arc<crate::linked_memory::LinkedMemory>>,
    /// Where routing outcomes are counted, when the caller wants them.
    ///
    /// Optional so a test — or the CLI, or a library caller — can build a compressor set
    /// without inventing a metrics sink it will never read. `None` means the routing
    /// reason is computed for the decision and not recorded, which is what every caller
    /// outside the proxy wants.
    metrics: Option<Arc<crate::metrics::Metrics>>,
    /// Where per-shape observations accumulate, when the caller wants them.
    ///
    /// Optional for the same reason `metrics` is: the CLI and library callers have no
    /// endpoint to read aggregates from. Invariant I9 governs what this may do — it
    /// observes, and nothing here reads it back to decide anything.
    aggregator: Option<Arc<std::sync::Mutex<headroom_core::telemetry::Aggregator>>>,
}

impl Compressors {
    /// Builds the set, sharing one CCR store between every compressor.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            orchestrator: Orchestrator::new(store).with_limits(crate::config::safety_limits()),
            memories: Default::default(),
            memory_limit: crate::config::DEFAULT_MEMORY_LIMIT,
            linked_memory: None,
            metrics: None,
            aggregator: None,
        }
    }

    /// Builds the set with recommendations learned from a previous run.
    pub fn with_recommendations(
        store: Arc<dyn CcrStore>,
        recommendations: headroom_core::telemetry::Recommendations,
    ) -> Self {
        Self {
            orchestrator: Orchestrator::new(store)
                .with_limits(crate::config::safety_limits())
                .with_recommendations(recommendations),
            memories: Default::default(),
            memory_limit: crate::config::DEFAULT_MEMORY_LIMIT,
            linked_memory: None,
            metrics: None,
            aggregator: None,
        }
    }

    /// Records routing outcomes into `metrics`.
    ///
    /// Invariant I9: observation only. Nothing here changes what is compressed.
    pub fn with_metrics(mut self, metrics: Arc<crate::metrics::Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Accumulates per-shape observations into `aggregator`.
    ///
    /// Invariant I9: observation only. Nothing here changes what is compressed, and a
    /// test asserts the compressed bytes are identical with and without it.
    pub fn with_aggregator(
        mut self,
        aggregator: Arc<std::sync::Mutex<headroom_core::telemetry::Aggregator>>,
    ) -> Self {
        self.aggregator = Some(aggregator);
        self
    }

    /// Attaches memories for live-zone injection.
    pub fn with_memories(
        mut self,
        memories: headroom_core::memory::MemoryStore,
        limit: usize,
    ) -> Self {
        self.memories = memories;
        self.memory_limit = limit;
        self
    }

    /// Attaches a linked `rusty_remind_me` backend, resolved once at startup.
    ///
    /// Takes precedence over `with_memories` at injection time when `Some` — see
    /// [`Self::memory_candidates`].
    pub fn with_linked_memory(
        mut self,
        linked_memory: Option<Arc<crate::linked_memory::LinkedMemory>>,
    ) -> Self {
        self.linked_memory = linked_memory;
        self
    }

    /// Renders the memory block for `query`, from whichever source is configured.
    ///
    /// The linked backend wins when present: it is a strict capability superset (BM25
    /// over a real index, RRF-fused with an optional semantic tier) of the static
    /// `HEADROOM_MEMORY` file, and running both would mean deciding how to merge two
    /// independently ranked lists for no benefit — nobody configures both meaning "use
    /// only one, some of the time."
    ///
    /// The two sources render differently on purpose: the static store's corroboration
    /// marker reports something the static store can actually observe (the same fact
    /// recorded by more than one agent), and a linked search result — one shot, one
    /// source, by construction — has nothing truthful to put there.
    fn inject_memory_block(&self, query: Option<&str>) -> Option<String> {
        match &self.linked_memory {
            Some(linked) => {
                let query = query.map(str::trim).filter(|q| !q.is_empty())?;
                headroom_core::memory::inject_block_ranked(&linked.search(query, self.memory_limit))
            }
            None => headroom_core::memory::inject_block_for_query(
                &self.memories,
                self.memory_limit,
                query,
            ),
        }
    }

    /// Splices [`Self::inject_memory_block`]'s output into the live-zone tail.
    fn inject_memory_append(
        &self,
        conversation: &Conversation,
        frozen: usize,
        query: Option<&str>,
    ) -> Option<(usize, usize, String)> {
        match &self.linked_memory {
            Some(linked) => {
                let query = query.map(str::trim).filter(|q| !q.is_empty())?;
                headroom_core::memory::inject_append_ranked(
                    conversation,
                    &linked.search(query, self.memory_limit),
                    frozen,
                )
            }
            None => headroom_core::memory::inject_append(
                conversation,
                &self.memories,
                self.memory_limit,
                frozen,
                query,
            ),
        }
    }

    /// The transform for `content` under `policy`, if any applies.
    ///
    /// Invariant I10 is enforced inside the orchestrator: restricted traffic is routed
    /// the lossless reformatter, never a lossy compressor.
    fn route_block(
        &self,
        block: &headroom_core::block::Block,
        policy: CompressionPolicy,
        model: &str,
    ) -> Option<&dyn Transform> {
        self.orchestrator.transform_for_block(block, policy, model)
    }

    /// The transform for `block` under `policy`, for callers outside the request path.
    ///
    /// The same routing the proxy uses, exposed so the HTTP compress endpoint cannot
    /// develop its own — a second copy of the routing decision is what check 6 of the
    /// reachability audit fails the build over.
    pub fn routed_transform(
        &self,
        block: &headroom_core::block::Block,
        policy: CompressionPolicy,
    ) -> Option<&dyn Transform> {
        self.route_block(block, policy, "")
    }

    /// Why `content` was routed as it was, for telemetry.
    pub fn routing(&self, content: &str, policy: CompressionPolicy, model: &str) -> Routing {
        self.orchestrator.route(content, policy, model)
    }

    /// The tokenizer to measure `model` with.
    pub fn tokenizer_for(
        &self,
        model: &str,
    ) -> std::sync::Arc<dyn headroom_core::tokenizer::Tokenizer> {
        self.orchestrator.tokenizer_for(model)
    }
}

/// Which provider's request shape a body is written in.
///
/// The two differ in more than field names, and the difference that matters is how
/// each provider decides what to cache — see [`Dialect::frozen_floor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `POST /v1/messages`. Caching is explicit, via customer `cache_control` markers.
    Anthropic,
    /// `POST /v1/chat/completions`. Caching is automatic and prefix-based.
    OpenAi,
    /// `POST /v1/responses`. Items in `input`, caching automatic and prefix-based.
    OpenAiResponses,
}

impl Dialect {
    /// The index below which messages are never eligible for compression.
    ///
    /// # Why the two providers need different answers
    ///
    /// Anthropic caches what the customer marks, so the floor is derived from their
    /// `cache_control` breakpoints and the absence of markers legitimately means
    /// nothing is pinned.
    ///
    /// OpenAI caches automatically: any sufficiently long prompt prefix is cached
    /// without anyone asking. Applying the Anthropic rule to an OpenAI body would read
    /// "no markers, so nothing is frozen" — which is exactly backwards, because
    /// *everything* the customer has already sent is a candidate prefix. So the floor
    /// is every message but the newest, and that is not a conservative guess so much
    /// as the only correct reading of automatic prefix caching.
    fn frozen_floor(self, body: &[u8], message_count: usize) -> usize {
        match self {
            Self::Anthropic => frozen_message_count(body),
            // Both OpenAI surfaces cache prefixes automatically, so the reasoning is
            // identical: everything already sent is a candidate cached prefix.
            Self::OpenAi | Self::OpenAiResponses => message_count.saturating_sub(1),
        }
    }
}

/// Compresses an Anthropic-shaped request body.
///
/// Returns the original bytes unchanged whenever compression does not apply or does
/// not help.
pub fn compress_request<'a>(
    body: &'a [u8],
    compressors: &Compressors,
    enabled: bool,
    policy: CompressionPolicy,
) -> Cow<'a, [u8]> {
    compress_dialect(
        Dialect::Anthropic,
        body,
        compressors,
        enabled,
        policy,
        Verbosity::Default,
    )
}

/// Compresses a request body written in `dialect`.
///
/// Returns the original bytes unchanged whenever compression does not apply or does
/// not help.
pub fn compress_dialect<'a>(
    dialect: Dialect,
    body: &'a [u8],
    compressors: &Compressors,
    enabled: bool,
    policy: CompressionPolicy,
    verbosity: Verbosity,
) -> Cow<'a, [u8]> {
    if !enabled {
        return Cow::Borrowed(body);
    }

    let faithful = FaithfulBody::parse(body);
    if !faithful.is_understood() {
        return Cow::Borrowed(body);
    }

    let frozen = dialect.frozen_floor(body, faithful.message_count());

    let Some((conversation, shapes, query)) = read_conversation(&faithful, dialect) else {
        return Cow::Borrowed(body);
    };

    let zone = live_zone(&conversation, frozen);
    if zone.is_empty() {
        return Cow::Borrowed(body);
    }

    // Decide everything before writing anything. `validated_apply` enforces I5 per
    // block, so a compressor that declines or fails to help leaves no trace here.
    //
    // The tokenizer is chosen by the request's own model. An exact count lets a
    // compressor keep a result the heuristic's over-count would have rejected, and the
    // heuristic remains the answer for any model without one — reporting an
    // approximation as a measurement would be worse than the approximation.
    let model = model_of(body);
    let estimator = compressors.tokenizer_for(model);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    for location in zone.locations() {
        let Some(block) = conversation
            .messages()
            .get(location.message)
            .and_then(|m| m.blocks().get(location.block))
        else {
            continue;
        };
        // The reason is recorded whether or not a compressor runs. An operator whose
        // traffic is not shrinking needs to tell "nothing handles this type" from "your
        // credential forbids it" from "we measured this shape and it does not help" —
        // three answers with three different actions, and two of them are "no action".
        //
        // Invariant I9: this observes. It takes the reason the router already computed
        // and changes no decision and no byte.
        if let Some(metrics) = compressors.metrics.as_ref() {
            metrics.record_routing(compressors.routing(block.content(), policy, model).as_str());
        }

        // Block-aware: prose is compressed only when the block is tool output. The
        // prose compressor is lossy, and `BlockKind::Text` is what somebody typed.
        let Some(transform) = compressors.route_block(block, policy, model) else {
            continue;
        };

        let mut candidate = block.clone();
        match validated_apply(transform, &mut candidate, estimator.as_ref()) {
            Ok(outcome) if outcome.is_compressed() => {
                observe_shape(
                    compressors,
                    block,
                    model,
                    policy,
                    Some((
                        estimator.count(block.content()) as u64,
                        estimator.count(candidate.content()) as u64,
                    )),
                );
                edits.push((
                    location.message,
                    location.block,
                    candidate.content().to_owned(),
                ));
            }
            // Declined, or not smaller. Either way the original stands.
            Ok(_) => {
                // Recorded too, and this is the half that matters most: a shape that
                // consistently declines is exactly what `recommend` needs to stop the
                // proxy attempting it, and counting only the successes would make every
                // measured ratio an average over the cases that worked.
                observe_shape(compressors, block, model, policy, None);
            }
            // An invariant violation is a bug, not something to paper over — but it
            // must not take down a customer's request either. Drop the edit, log, and
            // forward what arrived.
            Err(err) => {
                tracing::warn!(%err, "compressor failed; forwarding original block");
            }
        }
    }

    // Memory injection, gated on the *lossy* permission rather than the lossless one.
    // Adding content is not a transform of anything: the lossless permission is granted
    // on OAuth because a meaning-preserving change cannot exceed a granted scope, and
    // injecting facts the client never sent plainly can. So this runs only where lossy
    // rewriting is already permitted — invariant I10.
    if policy.lossy_transforms {
        if let Some((message, block, injected)) =
            compressors.inject_memory_append(&conversation, frozen, query.as_deref())
        {
            match edits
                .iter_mut()
                .find(|(m, b, _)| *m == message && *b == block)
            {
                // A compressor already rewrote this block. Append to *its* output, not
                // to the original, or the compression would be discarded.
                Some((_, _, content)) => {
                    if !content.contains("<memory>") {
                        if let Some(memories) = compressors.inject_memory_block(query.as_deref()) {
                            content.push_str(&memories);
                        }
                    }
                }
                None => edits.push((message, block, injected)),
            }
        }
    }

    // Output shaping runs after compression, and after rather than before deliberately:
    // the note must survive into the bytes that go out, and a compressor running over a
    // block that already carries it could summarize the instruction away.
    if let Some((message, block, shaped)) =
        output_shaping::verbosity_append(&conversation, verbosity, frozen)
    {
        match edits
            .iter_mut()
            .find(|(m, b, _)| *m == message && *b == block)
        {
            // A compressor already rewrote this block. Append to *its* output, not to
            // the original, or the compression would be discarded.
            Some((_, _, content)) => {
                if let Some(note) = verbosity.note() {
                    content.push_str(note);
                }
            }
            None => edits.push((message, block, shaped)),
        }
    }

    // Invariant I2, checked once for every edit regardless of which producer made it.
    //
    // # Why this is here as well as at each call site
    //
    // Three things write into `edits`: the live-zone loop, memory injection and the
    // verbosity note. The first is bounded by `live_zone` and the other two now check the
    // floor themselves — but they check it because two separate commits went and added it,
    // after both were found relying on the `zone.is_empty()` early return above, which
    // exists for an unrelated reason.
    //
    // A fourth producer would arrive with the same gap and nothing to catch it. This makes
    // the invariant structural: an edit below the floor cannot reach the rewriter, whoever
    // produced it. Same shape as I8, which `live_zone` and `apply_guarded` both enforce —
    // removing either alone still leaves signed content protected.
    //
    // Logged at `error` rather than dropped quietly: reaching here means a producer has a
    // bug, and a silently discarded edit would present as compression mysteriously not
    // happening. Not a panic — this is a customer's request, and refusing to serve it is a
    // worse outcome than forwarding it uncompressed.
    let before_floor = edits.len();
    edits.retain(|(message, _, _)| *message >= frozen);
    if edits.len() != before_floor {
        tracing::error!(
            dropped = before_floor - edits.len(),
            frozen,
            "an edit targeted a frozen message and was discarded; this is a bug in \
             whatever produced it, not in the request"
        );
    }

    if edits.is_empty() {
        return Cow::Borrowed(body);
    }

    // Rewrite only the messages that actually changed. Everything else — including
    // every frozen message — comes back as its original bytes.
    let mut replacements: Vec<(usize, String)> = Vec::new();
    for message_index in unique_message_indices(&edits) {
        let Some(raw) = faithful.message(message_index) else {
            continue;
        };
        let Some(shape) = shapes.get(message_index) else {
            continue;
        };
        let block_edits: Vec<(usize, &str)> = edits
            .iter()
            .filter(|(m, _, _)| *m == message_index)
            .map(|(_, b, content)| (*b, content.as_str()))
            .collect();

        if let Some(rewritten) = rewrite_message(raw, *shape, &block_edits) {
            replacements.push((message_index, rewritten));
        }
    }

    if replacements.is_empty() {
        return Cow::Borrowed(body);
    }

    faithful.rebuild(&replacements)
}

/// How a message carried its content, so it can be written back the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentShape {
    /// `"content": "text"`.
    Scalar,
    /// `"content": [ ... ]`.
    Blocks,
    /// A Responses-API item whose payload lives in `output`.
    ResponsesOutput,
}

/// Builds a [`Conversation`] view for deciding what is live.
///
/// The conversation is only ever used to *decide*. Edits are applied back to the raw
/// JSON, so a message this view models imperfectly is still forwarded byte-exact
/// unless something explicitly changed it.
///
/// Returns the conversation query alongside, rather than only stamping it onto blocks.
/// Memory selection needs it too, and it is a property of the whole conversation — a
/// second derivation at the injection site would be the same walk over the same
/// messages, free to drift from this one.
fn read_conversation(
    faithful: &FaithfulBody<'_>,
    dialect: Dialect,
) -> Option<(Conversation, Vec<ContentShape>, Option<String>)> {
    let mut messages = Vec::with_capacity(faithful.message_count());
    let mut shapes = Vec::with_capacity(faithful.message_count());

    // Read once, before the loop, because it is a property of the whole conversation
    // rather than of any one message — and because the message a tool result answers
    // sits *earlier* in the array than the result itself.
    let query = conversation_query(faithful, dialect);

    for index in 0..faithful.message_count() {
        let raw = faithful.message(index)?;
        let value: Value = serde_json::from_str(raw).ok()?;

        let declared_role = value.get("role").and_then(Value::as_str);
        let role = match declared_role {
            Some("assistant") => Role::Assistant,
            // Anything else is treated as user-side. Tool results arrive in
            // user-role messages, and an unrecognized role should not accidentally
            // widen what is considered live.
            _ => Role::User,
        };

        // The Responses API carries a tool result as a standalone item with no `role`
        // at all: `{"type":"function_call_output","call_id":...,"output":"..."}`. Read
        // through the chat-completions path it has no content and no kind, so the
        // bulkiest item in a Responses conversation would never be compressed.
        if dialect == Dialect::OpenAiResponses {
            if let Some(kind) = responses_item_kind(&value) {
                let output = value
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                messages.push(Message::new(
                    role,
                    vec![with_query(Block::new(kind, output), query.as_deref())],
                ));
                shapes.push(ContentShape::ResponsesOutput);
                continue;
            }
        }

        let (blocks, shape) = match value.get("content") {
            Some(Value::String(text)) => {
                // OpenAI carries a tool result as a whole message with `role: "tool"`
                // and a plain string body, where Anthropic nests a typed block inside a
                // user message. Reading it as ordinary text would leave it out of the
                // live zone entirely, so the bulkiest thing in an OpenAI conversation —
                // the tool output — would never be compressed.
                let kind = if dialect == Dialect::OpenAi && declared_role == Some("tool") {
                    BlockKind::ToolResult
                } else {
                    BlockKind::Text
                };
                (
                    vec![with_query(Block::new(kind, text.clone()), query.as_deref())],
                    ContentShape::Scalar,
                )
            }
            Some(Value::Array(items)) => (
                items
                    .iter()
                    .map(|item| with_query(read_block(item), query.as_deref()))
                    .collect(),
                ContentShape::Blocks,
            ),
            _ => (Vec::new(), ContentShape::Blocks),
        };

        messages.push(Message::new(role, blocks));
        shapes.push(shape);
    }

    Some((Conversation::new(None, Vec::new(), messages), shapes, query))
}

/// The block kind for a Responses-API item, if it is one this code compresses.
///
/// Only output-carrying items qualify. A `function_call` (the *request* to run a tool)
/// carries arguments the provider parses as JSON, and compressing those would produce
/// a call the provider rejects rather than a shorter one.
fn responses_item_kind(item: &Value) -> Option<BlockKind> {
    match item.get("type").and_then(Value::as_str)? {
        "function_call_output" => Some(BlockKind::FunctionCallOutput),
        "local_shell_call_output" => Some(BlockKind::LocalShellCallOutput),
        "apply_patch_call_output" => Some(BlockKind::ApplyPatchCallOutput),
        _ => None,
    }
}

/// Attaches the conversation's query to a block that can use it.
///
/// Only tool output gets one. A `text` block is what a person or the model wrote, and
/// "how relevant is this to the question" is not a question worth asking about the
/// question itself — the same asymmetry D24 already applies to prose summarization.
fn with_query(block: Block, query: Option<&str>) -> Block {
    match query {
        Some(query) if block.kind().is_tool_output() => block.with_query(query),
        _ => block,
    }
}

/// The question the live tool output is answering.
///
/// Two parts, joined:
///
/// 1. **The newest user-authored text.** What the person actually asked.
/// 2. **The newest assistant turn's tool-call arguments.** What the model asked the
///    tool for on their behalf — usually literal identifiers, paths and filters, which
///    is exactly the material keyword scoring is best at matching.
///
/// The second part carries most of the weight in practice. A user saying "check that
/// order" is vague; the `{"order_id":"a3f9"}` the model derived from it is not.
///
/// # A deliberate approximation
///
/// Arguments are taken from the newest assistant turn as a whole rather than matched to
/// each tool result by call id. In the overwhelmingly common single-call turn the two
/// are identical. In a parallel-call turn this gives every result the union of that
/// turn's arguments, which can pin a record relevant to a *sibling* call — keeping a few
/// extra records, never dropping a relevant one. Erring toward keeping is the right
/// direction for a bounded pin set, and per-call matching would need `tool_use_id`
/// threaded through the reader, which is a larger change than the accuracy justifies
/// today.
///
/// Returns `None` when there is nothing to ask about, so the relevance pass is skipped
/// entirely rather than run against an empty string.
fn conversation_query(faithful: &FaithfulBody<'_>, dialect: Dialect) -> Option<String> {
    let mut user_text: Option<String> = None;
    let mut arguments: Option<String> = None;

    for index in (0..faithful.message_count()).rev() {
        let Some(raw) = faithful.message(index) else {
            continue;
        };
        let Ok(value): std::result::Result<Value, _> = serde_json::from_str(raw) else {
            continue;
        };

        if arguments.is_none() {
            let found = tool_call_arguments(&value, dialect);
            if !found.is_empty() {
                arguments = Some(found.join(" "));
            }
        }

        if user_text.is_none() && value.get("role").and_then(Value::as_str) != Some("assistant") {
            let found = user_authored_text(&value);
            if !found.is_empty() {
                user_text = Some(found.join(" "));
            }
        }

        if user_text.is_some() && arguments.is_some() {
            break;
        }
    }

    let combined = [user_text, arguments]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");

    let combined = combined.trim();
    if combined.is_empty() {
        None
    } else {
        Some(combined.to_owned())
    }
}

/// Text a person wrote in this message, ignoring tool results.
///
/// A tool result lives in a user-role message on the Anthropic wire, and including its
/// body would make every record score against the very content being compressed —
/// every item would look relevant, the pin cap would fill with the first few records,
/// and relevance would be worse than useless.
fn user_authored_text(message: &Value) -> Vec<String> {
    match message.get("content") {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// The arguments of every tool call in this message, in each dialect's shape.
fn tool_call_arguments(message: &Value, dialect: Dialect) -> Vec<String> {
    match dialect {
        // `{"type":"tool_use","name":...,"input":{...}}` inside an assistant message.
        Dialect::Anthropic => match message.get("content") {
            Some(Value::Array(items)) => items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
                .filter_map(|item| item.get("input"))
                .map(Value::to_string)
                .collect(),
            _ => Vec::new(),
        },

        // `{"tool_calls":[{"function":{"arguments":"{...}"}}]}` — arguments are a JSON
        // *string*, not an object, so they are taken as text rather than re-parsed.
        Dialect::OpenAi => message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| {
                        call.get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),

        // A standalone `{"type":"function_call","arguments":"{...}"}` item.
        Dialect::OpenAiResponses => {
            if message.get("type").and_then(Value::as_str) == Some("function_call") {
                message
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(|arguments| vec![arguments.to_owned()])
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        }
    }
}

/// Records one compression outcome against its shape.
///
/// # Invariant I9
///
/// This observes and returns nothing. It is called *after* the decision it describes, is
/// handed the outcome rather than consulted for one, and holds the aggregator lock only
/// long enough to add. A poisoned lock drops the observation rather than failing the
/// request — telemetry must never be able to take down the request path it is watching.
fn observe_shape(
    compressors: &Compressors,
    block: &Block,
    model: &str,
    policy: CompressionPolicy,
    compressed: Option<(u64, u64)>,
) {
    let Some(aggregator) = compressors.aggregator.as_ref() else {
        return;
    };

    let detected = headroom_core::detection::detect(block.content().as_bytes());
    let key = headroom_core::telemetry::AggregationKey::new(
        policy.mode,
        model,
        headroom_core::telemetry::StructureHash::of(block.content(), detected.content_type),
    );

    let Ok(mut aggregator) = aggregator.lock() else {
        return;
    };

    use headroom_core::telemetry::Telemetry;
    match compressed {
        Some((before, after)) => aggregator.record(&key, before, after),
        None => aggregator.record_decline(&key),
    }
}

/// Reads one content block.
fn read_block(item: &Value) -> Block {
    let kind = match item.get("type").and_then(Value::as_str) {
        Some("text") => BlockKind::Text,
        Some("tool_result") => BlockKind::ToolResult,
        Some("tool_use") => BlockKind::ToolUse,
        Some("thinking") => BlockKind::Thinking,
        Some("redacted_thinking") => BlockKind::RedactedThinking,
        Some("function_call_output") => BlockKind::FunctionCallOutput,
        Some("local_shell_call_output") => BlockKind::LocalShellCallOutput,
        Some("apply_patch_call_output") => BlockKind::ApplyPatchCallOutput,
        // Images, documents, and anything unrecognized. Attachment is not
        // compressible, so an unknown block type is excluded from the live zone —
        // the safe default for a type this code has never seen.
        _ => BlockKind::Attachment,
    };

    let content = match kind {
        BlockKind::Text => item.get("text").and_then(Value::as_str).unwrap_or_default(),
        _ => item
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    };

    Block::new(kind, content)
}

/// Applies block edits to one raw message, returning its new JSON text.
fn rewrite_message(raw: &str, shape: ContentShape, edits: &[(usize, &str)]) -> Option<String> {
    let mut value: Value = serde_json::from_str(raw).ok()?;

    match shape {
        ContentShape::ResponsesOutput => {
            // Written back to `output`, leaving `type` and `call_id` untouched — the
            // provider matches the result to its call by `call_id`, so losing it turns
            // a compressed tool result into an orphan the model cannot attribute.
            let (_, replacement) = edits.iter().find(|(index, _)| *index == 0)?;
            value
                .as_object_mut()?
                .insert("output".into(), Value::String((*replacement).to_owned()));
        }
        ContentShape::Scalar => {
            // A scalar message has exactly one conceptual block.
            let (_, replacement) = edits.iter().find(|(index, _)| *index == 0)?;
            value
                .as_object_mut()?
                .insert("content".into(), Value::String((*replacement).to_owned()));
        }
        ContentShape::Blocks => {
            let items = value.get_mut("content")?.as_array_mut()?;
            for (index, replacement) in edits {
                let item = items.get_mut(*index)?;
                let object = item.as_object_mut()?;
                // Write back to whichever field this block type reads from, leaving
                // every sibling field — `type`, `tool_use_id`, `is_error` — untouched.
                let field = if object.get("type").and_then(Value::as_str) == Some("text") {
                    "text"
                } else {
                    "content"
                };
                object.insert(field.into(), Value::String((*replacement).to_owned()));
            }
        }
    }

    serde_json::to_string(&value).ok()
}

/// The model identifier a request names, for tokenizer selection.
///
/// Read rather than rewritten, so the usual byte-faithfulness concern does not apply —
/// this parse never produces output. An absent or unreadable model yields an empty
/// string, which resolves to the heuristic.
pub(crate) fn model_of(body: &[u8]) -> &str {
    // A borrowed `&str` from the raw bytes rather than an owned `String`, since this is
    // called once per request and the value is used immediately.
    serde_json::from_slice::<&serde_json::value::RawValue>(body)
        .ok()
        .and_then(|_| {
            let text = std::str::from_utf8(body).ok()?;
            let start = text.find(r#""model""#)? + r#""model""#.len();
            let rest = text[start..].trim_start().strip_prefix(':')?.trim_start();
            let quoted = rest.strip_prefix('"')?;
            let end = quoted.find('"')?;
            Some(&quoted[..end])
        })
        .unwrap_or_default()
}

/// Message indices touched by `edits`, ascending and deduplicated.
fn unique_message_indices(edits: &[(usize, usize, String)]) -> Vec<usize> {
    let mut indices: Vec<usize> = edits.iter().map(|(m, _, _)| *m).collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::ccr::InMemoryCcrStore;
    use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};
    use sha2::{Digest, Sha256};

    fn compressors() -> Compressors {
        Compressors::new(Arc::new(InMemoryCcrStore::new()))
    }

    fn payg() -> CompressionPolicy {
        CompressionPolicy::for_mode(headroom_core::AuthMode::PayAsYouGo)
    }

    fn sha(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    /// A bulky JSON tool result, the shape SmartCrusher exists for.
    fn bulky_tool_output() -> String {
        let records: Vec<String> = (0..120)
            .map(|i| {
                format!(
                    r#"{{"path":"src/module_{i}.rs","kind":"file","status":"ok","size":{}}}"#,
                    1000 + i
                )
            })
            .collect();
        format!("[{}]", records.join(",")).replace('"', "\\\"")
    }

    /// system + tools + 5 historical turns + a new bulky tool result.
    fn request() -> String {
        format!(
            r#"{{"model":"claude-opus-4","max_tokens":4096,"system":"You are a careful assistant.","tools":[{{"name":"read_file","input_schema":{{"type":"object","properties":{{"path":{{"type":"string"}}}}}}}}],"messages":[{{"role":"user","content":"turn one question"}},{{"role":"assistant","content":"turn one answer"}},{{"role":"user","content":"turn two question"}},{{"role":"assistant","content":"turn two answer"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t_old","content":"small older result"}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t_new","content":"{}"}}]}}]}}"#,
            bulky_tool_output()
        )
    }

    // ---- query relevance reaches a real request (#176/#177) ----

    /// 60 structurally identical orders. Nothing about the data marks any one out.
    fn orders_tool_output() -> String {
        let records: Vec<String> = (0..60)
            .map(|i| format!(r#"{{"order":"ord-{i:04}","state":"pending","items":2}}"#))
            .collect();
        format!("[{}]", records.join(",")).replace('"', "\\\"")
    }

    /// An Anthropic request whose newest turn is a tool result answering `arguments`.
    fn request_asking(user_text: &str, arguments: &str) -> String {
        format!(
            r#"{{"model":"claude-opus-4","max_tokens":4096,"messages":[{{"role":"user","content":"{user_text}"}},{{"role":"assistant","content":[{{"type":"tool_use","id":"t_new","name":"list_orders","input":{arguments}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t_new","content":"{}"}}]}}]}}"#,
            orders_tool_output()
        )
    }

    #[test]
    fn the_record_the_user_asked_about_survives_a_real_request() {
        // The end-to-end claim of #176/#177, asserted through `compress_request` rather
        // than against the planner — a scorer reachable only from its own unit test is
        // the exact defect that produced #71, #73, #75, #82 and #84.
        //
        // Record `ord-0042` is structurally identical to its 59 peers, so only the
        // query can keep it.
        let asked = request_asking("check that order for me", r#"{"order":"ord-0042"}"#);
        let out = compress_request(asked.as_bytes(), &compressors(), true, payg());
        let out = String::from_utf8(out.into_owned()).unwrap();

        assert_ne!(
            out, asked,
            "nothing was compressed; the test proves nothing"
        );
        assert!(
            out.contains("ord-0042"),
            "the record the user asked about did not survive compression"
        );
    }

    #[test]
    fn the_same_body_without_a_question_elides_that_record() {
        // The control. Without it the test above could pass because 60 records happen
        // to fit, rather than because relevance did anything.
        let unasked = request_asking("go ahead", r#"{"state":"pending"}"#);
        let out = compress_request(unasked.as_bytes(), &compressors(), true, payg());
        let out = String::from_utf8(out.into_owned()).unwrap();

        assert_ne!(
            out, unasked,
            "nothing was compressed; the test proves nothing"
        );
        assert!(
            !out.contains("ord-0042"),
            "record 42 survived with no query naming it, so the sibling test is vacuous"
        );
    }

    #[test]
    fn the_tool_result_being_compressed_is_not_itself_the_query() {
        // A tool result arrives inside a *user-role* message on the Anthropic wire. If
        // the query were built from that message's content, every record would score
        // against the very bytes being compressed, every record would look relevant,
        // and the pin cap would fill with whichever happened to sort first.
        let asked = request_asking("check that order for me", r#"{"order":"ord-0042"}"#);
        let out = compress_request(asked.as_bytes(), &compressors(), true, payg());
        let out = String::from_utf8(out.into_owned()).unwrap();

        // Still a summary, not a near-copy.
        assert!(
            out.len() < asked.len() / 2,
            "output is {} bytes against an input of {} — relevance pinned too much",
            out.len(),
            asked.len()
        );
    }

    #[test]
    fn compression_stays_deterministic_with_a_query() {
        // I4 on the path that newly introduces float comparison and a sort.
        let asked = request_asking("check that order", r#"{"order":"ord-0042"}"#);
        let first = compress_request(asked.as_bytes(), &compressors(), true, payg()).into_owned();
        for _ in 0..5 {
            let again =
                compress_request(asked.as_bytes(), &compressors(), true, payg()).into_owned();
            assert_eq!(sha(&first), sha(&again));
        }
    }

    // ---- the headline guarantee ----

    #[test]
    fn the_hot_zone_and_every_frozen_turn_survive_byte_identical() {
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        let out = String::from_utf8(out.into_owned()).unwrap();

        // Something must actually have happened, or this passes vacuously.
        assert_ne!(out, source, "nothing was compressed");

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_str(&out).unwrap();

        // The hot zone, hashed rather than compared loosely.
        for member in ["system", "tools", "model", "max_tokens"] {
            assert_eq!(
                sha(serde_json::to_string(&before[member]).unwrap().as_bytes()),
                sha(serde_json::to_string(&after[member]).unwrap().as_bytes()),
                "{member} was modified"
            );
        }

        // Every historical turn, byte for byte.
        for index in 0..5 {
            assert_eq!(
                sha(serde_json::to_string(&before["messages"][index])
                    .unwrap()
                    .as_bytes()),
                sha(serde_json::to_string(&after["messages"][index])
                    .unwrap()
                    .as_bytes()),
                "historical turn {index} was modified"
            );
        }
    }

    #[test]
    fn the_live_tool_result_measurably_shrinks() {
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());

        let estimator = HeuristicEstimator::new();
        let before = estimator.count(&source);
        let after = estimator.count(&String::from_utf8_lossy(&out));

        assert!(
            after < before / 2,
            "expected a real cut: {before} -> {after}"
        );
    }

    #[test]
    fn the_untouched_prefix_is_literally_the_original_bytes() {
        // Stronger than the parsed comparison above: the raw substring of the request
        // up to the live message must appear verbatim in the output.
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        let out = String::from_utf8(out.into_owned()).unwrap();

        let prefix_end = source
            .find(r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"t_new""#)
            .unwrap();
        assert!(
            out.starts_with(&source[..prefix_end]),
            "the frozen prefix was re-serialized rather than copied"
        );
    }

    // ---- passthrough cases ----

    #[test]
    fn compression_disabled_returns_the_original_bytes_untouched() {
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), false, payg());
        assert!(matches!(out, Cow::Borrowed(_)), "should not have allocated");
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_streaming_request_is_compressed_like_any_other() {
        // Regression against the version of this function that bailed out on
        // `"stream": true`. `stream` describes the *response* framing; the request body
        // arrived complete either way. Since streaming is the common agent case, the
        // bail-out exempted most real traffic from compression while every test kept
        // confirming that compression worked.
        let source =
            request().replace(r#""max_tokens":4096"#, r#""max_tokens":4096,"stream":true"#);
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());

        assert_ne!(
            sha(&out),
            sha(source.as_bytes()),
            "a streaming request was left uncompressed"
        );

        // And the flag itself survives — compressing the body must not change what the
        // client asked the provider for.
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["stream"], Value::Bool(true));
    }

    #[test]
    fn a_streaming_request_still_protects_the_frozen_prefix() {
        // The guarantee has to hold on the path that now compresses more traffic, not
        // only on the one the original tests covered.
        let source =
            request().replace(r#""max_tokens":4096"#, r#""max_tokens":4096,"stream":true"#);
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        let out = String::from_utf8(out.into_owned()).unwrap();

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_str(&out).unwrap();

        for member in ["system", "tools", "model", "max_tokens"] {
            assert_eq!(before[member], after[member], "{member} was modified");
        }
        for index in 0..5 {
            assert_eq!(
                before["messages"][index], after["messages"][index],
                "historical turn {index} was modified"
            );
        }
    }

    #[test]
    fn a_malformed_body_forwards_untouched() {
        for source in [&b"{not json"[..], &b""[..], &b"{\"no\":\"messages\"}"[..]] {
            let out = compress_request(source, &compressors(), true, payg());
            assert_eq!(sha(&out), sha(source));
        }
    }

    #[test]
    fn a_request_with_nothing_worth_compressing_forwards_untouched() {
        let source = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_cache_control_marker_on_the_last_message_freezes_it() {
        // The customer pinned everything. Nothing is live, so nothing is touched even
        // though the payload is bulky enough to be worth compressing.
        let source = request().replace(
            r#""tool_use_id":"t_new""#,
            r#""tool_use_id":"t_new","cache_control":{"type":"ephemeral"}"#,
        );
        // The marker sits on the block, and the block's message is the last one.
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        assert_eq!(
            sha(&out),
            sha(source.as_bytes()),
            "a pinned request was modified"
        );
    }

    // ---- routing telemetry ----

    #[test]
    fn observing_the_routing_reason_changes_no_byte() {
        // Invariant I9: telemetry observes, it never alters. The reason is the one the
        // router already computed for the decision, so recording it must not be able to
        // move a byte.
        let source = code_request();
        let plain = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        let metrics = std::sync::Arc::new(crate::metrics::Metrics::new());
        let observed = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors().with_metrics(metrics.clone()),
            true,
            payg(),
            Verbosity::Default,
        );

        assert_eq!(sha(&observed), sha(&plain));
        assert!(
            metrics.render().contains(r#"reason="compress"} 1"#),
            "nothing was recorded, so this test proves nothing"
        );
    }

    #[test]
    fn a_declined_block_is_still_counted() {
        // The whole point. A compressed block is visible in the savings numbers already;
        // the one an operator cannot otherwise explain is the block that was declined.
        let metrics = std::sync::Arc::new(crate::metrics::Metrics::new());
        let restricted = CompressionPolicy::for_mode(headroom_core::AuthMode::Subscription);

        let _ = compress_dialect(
            Dialect::Anthropic,
            code_request().as_bytes(),
            &compressors().with_metrics(metrics.clone()),
            true,
            restricted,
            Verbosity::Default,
        );

        assert!(metrics.render().contains(r#"reason="policy_forbids"} 1"#));
    }

    // ---- code compression (gap rows C11-C13) ----

    /// A source file large enough to clear the code size threshold.
    fn code_request() -> String {
        let code = concat!(
            "pub fn handle(input: &str) -> Result<String, Error> {\n",
            "    let parsed = parse(input)?;\n",
            "    let checked = validate(&parsed)?;\n",
            "    Ok(render(&checked))\n",
            "}\n"
        )
        .repeat(80);

        format!(
            r#"{{"model":"claude-opus-4","messages":[{{"role":"user","content":"a"}},{{"role":"assistant","content":"b"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":{}}}]}}]}}"#,
            serde_json::to_string(&code).unwrap()
        )
    }

    #[test]
    fn the_proxy_compresses_code() {
        // It did not, for the whole life of the pipeline refactor. `Orchestrator` held
        // no code compressor, so every source file a tool returned was forwarded whole
        // — the largest single category of agent traffic, silently exempt.
        let source = code_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert!(
            out.len() < source.len(),
            "code was forwarded unchanged: {} -> {} bytes",
            source.len(),
            out.len()
        );
    }

    #[test]
    fn code_routing_agrees_with_the_orchestrator_the_cli_uses() {
        // `headroom compress --dry-run` exists to predict this. It used to carry its own
        // routing table with a code arm the orchestrator lacked, so it promised a saving
        // the proxy never delivered. One table now answers both.
        let code = concat!(
            "pub fn handle(input: &str) -> Result<String, Error> {\n",
            "    let parsed = parse(input)?;\n",
            "    Ok(render(&parsed))\n",
            "}\n"
        )
        .repeat(80);

        assert!(
            compressors().routing(&code, payg(), "").will_compress(),
            "the proxy would not compress content the CLI reports a saving for"
        );
    }

    // ---- memory injection (gap row Y3) ----

    /// A compressor set carrying two memories.
    fn with_memories() -> Compressors {
        use headroom_core::memory::{MemoryStore, Provenance};

        let mut store = MemoryStore::new();
        store.remember(
            "The auth module uses tokens, not sessions.",
            Provenance::new("reader", "s1"),
        );
        store.remember(
            "Tests live under crates/*/tests.",
            Provenance::new("reader", "s1"),
        );

        compressors().with_memories(store, 8)
    }

    #[test]
    fn memories_land_on_the_newest_user_message() {
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );
        let parsed: Value = serde_json::from_slice(&out).unwrap();

        let newest = parsed["messages"][2]["content"].as_str().unwrap();
        assert!(newest.starts_with("what does this function do?"));
        assert!(
            newest.contains("uses tokens"),
            "the memory block is missing: {newest}"
        );
    }

    #[test]
    fn the_conversation_query_decides_which_memory_is_injected() {
        // Reachability, not scoring. `recall_for_query` is unit-tested in
        // `headroom_core::memory`; what this asserts is that the query derived from the
        // request actually arrives there. Selection that nothing calls is the failure
        // class #71 was filed for, and it passes every unit test it has.
        //
        // The two decoys carry two distinct sources each and the relevant memory carries
        // one, so the corroboration ranking prefers a decoy outright. With `limit` at 1,
        // an unwired query cannot produce the asserted answer by luck.
        use headroom_core::memory::{MemoryStore, Provenance};

        let mut store = MemoryStore::new();
        for source in ["reader", "planner"] {
            store.remember(
                "Tests live under crates/*/tests.",
                Provenance::new(source, "s1"),
            );
            store.remember(
                "Releases are cut from main on Fridays.",
                Provenance::new(source, "s1"),
            );
        }
        store.remember(
            "This function normalizes whitespace before hashing.",
            Provenance::new("reader", "s1"),
        );

        let source = prose_request(); // newest user text: "what does this function do?"
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors().with_memories(store, 1),
            true,
            payg(),
            Verbosity::Default,
        );
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        let newest = parsed["messages"][2]["content"].as_str().unwrap();

        assert!(
            newest.contains("normalizes whitespace"),
            "the query did not reach memory selection: {newest}"
        );
        assert!(
            !newest.contains("Fridays"),
            "an unrelated memory spent the budget: {newest}"
        );
    }

    // ---- linked memory (gap #215): the call site must actually prefer it ----

    #[cfg(feature = "linked-memory")]
    #[test]
    fn a_configured_linked_backend_is_used_instead_of_the_static_store() {
        // Reachability, the same class `the_conversation_query_decides_which_memory_is_
        // injected` guards for the static store: a `Compressors` with a linked backend
        // attached must actually reach `LinkedMemory::search` at the compression call
        // site, not silently keep serving from `self.memories`. A wiring bug here — the
        // call site checking the wrong field, or not checking at all — would still pass
        // every test that only exercises `LinkedMemory` in isolation.
        use remind_me_core::db::queries;
        use remind_me_core::{Database, MemoryAddInput};
        use rusqlite::{Connection, OpenFlags};

        let path = std::env::temp_dir().join(format!(
            "headroom-compression-linked-memory-test-{}.sqlite3",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let db = Database::open(&path).unwrap();
            queries::add_memory(
                &db.conn(),
                MemoryAddInput {
                    sensitive: false,
                    content: "This function is reachable only through the linked backend."
                        .to_string(),
                    category: "general".into(),
                    tags: vec![],
                    source: "manual".into(),
                    metadata: serde_json::json!({}),
                    subject: None,
                    predicate: None,
                    object: None,
                    entities: vec![],
                },
            )
            .unwrap();
        }
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let linked = crate::linked_memory::LinkedMemory::for_test(conn, None);

        use headroom_core::memory::{MemoryStore, Provenance};
        let mut static_store = MemoryStore::new();
        static_store.remember(
            "This function is only in the static store and must not appear.",
            Provenance::new("reader", "s1"),
        );

        let source = prose_request(); // newest user text: "what does this function do?"
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors()
                .with_memories(static_store, 8)
                .with_linked_memory(Some(Arc::new(linked))),
            true,
            payg(),
            Verbosity::Default,
        );
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        let newest = parsed["messages"][2]["content"].as_str().unwrap();

        assert!(
            newest.contains("reachable only through the linked backend"),
            "the linked backend was not consulted at the compression call site: {newest}"
        );
        assert!(
            !newest.contains("only in the static store"),
            "the static store was used even though a linked backend was configured: {newest}"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn memory_injection_never_touches_the_cached_prefix() {
        // Invariants I2 and I3. The system prompt heads the cached prefix and a memory
        // set is *designed* to change, so writing there would bust the cache on every
        // turn that learned something — paying full price for the whole conversation in
        // exchange for a sentence.
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_slice(&out).unwrap();

        for member in ["system", "tools", "model"] {
            assert_eq!(before[member], after[member], "{member} was modified");
        }
        assert_eq!(
            before["messages"][0], after["messages"][0],
            "a frozen turn was modified"
        );
        assert_eq!(before["messages"][1], after["messages"][1]);
    }

    #[test]
    fn an_empty_memory_store_leaves_the_request_byte_identical() {
        // Invariant I1, and the reason this feature is safe to ship on by default: a
        // proxy with no memory file configured must be indistinguishable from one built
        // before injection existed.
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_restricted_policy_injects_nothing() {
        // Invariant I10. Injection adds content the client never sent, which is not a
        // transform of anything — so the lossless permission does not cover it and only
        // pay-as-you-go traffic gets it.
        let source = prose_request();
        let restricted = CompressionPolicy::for_mode(headroom_core::AuthMode::Subscription);
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            restricted,
            Verbosity::Default,
        );

        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn memory_and_output_shaping_reach_every_dialect() {
        // Every other test for both features runs `Dialect::Anthropic` and stops. That is
        // the exact shape of four gaps this repository has already shipped — a capability
        // built and tested against one surface, then described as if it covered all three.
        // See CONTRIBUTING's fourth lesson.
        //
        // These two turned out to be fine. That is worth a test rather than a note,
        // because "fine" is a fact about today: `read_conversation` grows a branch per
        // dialect, and the day one of them stops yielding a user message to append to,
        // both features go quiet on that surface with nothing failing.
        let chat = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"turn one"},{"role":"assistant","content":"reply one"},{"role":"user","content":"what does this function do?"}]}"#;
        let responses = r#"{"model":"gpt-4o","input":[{"role":"user","content":"turn one"},{"role":"assistant","content":"reply one"},{"role":"user","content":"what does this function do?"}]}"#;

        for (dialect, source) in [
            (Dialect::Anthropic, prose_request()),
            (Dialect::OpenAi, chat.to_owned()),
            (Dialect::OpenAiResponses, responses.to_owned()),
        ] {
            let injected = compress_dialect(
                dialect,
                source.as_bytes(),
                &with_memories(),
                true,
                payg(),
                Verbosity::Default,
            );
            let injected = String::from_utf8(injected.into_owned()).unwrap();
            assert!(
                injected.contains("uses tokens"),
                "{dialect:?} dropped the memory block: {injected}"
            );

            // Asked of a compressor set with *no* memories, so the growth below is the
            // terseness note and not the injection above leaking into this assertion.
            let terse = compress_dialect(
                dialect,
                source.as_bytes(),
                &compressors(),
                true,
                payg(),
                Verbosity::Terse,
            );
            let terse = String::from_utf8(terse.into_owned()).unwrap();
            assert_ne!(
                terse, source,
                "{dialect:?} ignored Verbosity::Terse and forwarded the request untouched"
            );

            // The control: the same request at the default verbosity is left alone, so
            // the inequality above is the shaper acting rather than any rewrite at all.
            let plain = compress_dialect(
                dialect,
                source.as_bytes(),
                &compressors(),
                true,
                payg(),
                Verbosity::Default,
            );
            assert_eq!(
                String::from_utf8(plain.into_owned()).unwrap(),
                source,
                "{dialect:?} rewrote a request that needed nothing, so the terse \
                 assertion above proves nothing"
            );
        }
    }

    #[test]
    fn memories_are_not_injected_twice_across_turns() {
        // An agent loop calls this every turn. Without the guard a long session
        // accumulates the same facts a dozen times over — wasted tokens, and a worse
        // prompt, since repetition reads as emphasis.
        let source = prose_request();
        let once = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );
        let twice = compress_dialect(
            Dialect::Anthropic,
            &once,
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert_eq!(
            sha(&twice),
            sha(&once),
            "the memory block was injected again"
        );
    }

    #[test]
    fn injection_is_deterministic() {
        // Invariant I4. These bytes go upstream, so an injection order that varied would
        // bust the very cache the live-zone placement exists to protect.
        let source = prose_request();
        let first = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &with_memories(),
            true,
            payg(),
            Verbosity::Default,
        );

        for _ in 0..25 {
            let again = compress_dialect(
                Dialect::Anthropic,
                source.as_bytes(),
                &with_memories(),
                true,
                payg(),
                Verbosity::Default,
            );
            assert_eq!(sha(&again), sha(&first));
        }
    }

    // ---- output shaping ----

    /// A request whose newest message is plain prose, so the note has somewhere to go.
    fn prose_request() -> String {
        r#"{"model":"claude-opus-4","system":"You are a careful assistant.","tools":[{"name":"read_file"}],"messages":[{"role":"user","content":"turn one"},{"role":"assistant","content":"reply one"},{"role":"user","content":"what does this function do?"}]}"#.to_owned()
    }

    #[test]
    fn the_terseness_note_never_touches_the_cached_prefix() {
        // The headline constraint. The system prompt is the first thing in the cached
        // prefix, so a note appended there invalidates the whole cache on every
        // request — saving a couple of hundred output tokens while re-billing tens of
        // thousands of input ones, and moving the metric people watch in the wrong
        // direction invisibly.
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        );
        let out = String::from_utf8(out.into_owned()).unwrap();

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_str(&out).unwrap();

        for member in ["system", "tools", "model"] {
            assert_eq!(before[member], after[member], "{member} was modified");
        }
        assert_eq!(
            before["messages"][0], after["messages"][0],
            "a frozen turn was modified"
        );
        assert_eq!(before["messages"][1], after["messages"][1]);
    }

    #[test]
    fn the_terseness_note_lands_on_the_newest_user_message() {
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        );
        let parsed: Value = serde_json::from_slice(&out).unwrap();

        let newest = parsed["messages"][2]["content"].as_str().unwrap();
        assert!(newest.starts_with("what does this function do?"));
        assert!(newest.contains("briefly"), "the note is missing: {newest}");
    }

    #[test]
    fn the_default_verbosity_leaves_the_body_byte_identical() {
        // Output shaping changes what the model *writes*, which is a visible change to
        // the customer's product rather than an invisible saving. Off unless asked for.
        let source = prose_request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        assert_eq!(sha(&out), sha(source.as_bytes()));
        assert!(matches!(out, Cow::Borrowed(_)), "should not have rebuilt");
    }

    #[test]
    fn shaping_does_not_discard_a_compressors_work() {
        // Both want the same block when the newest message is a bulky tool result the
        // compressor rewrote. Appending to the *original* rather than to the compressed
        // output would silently throw the compression away.
        let source = request();
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        );

        let estimator = HeuristicEstimator::new();
        let before = estimator.count(&source);
        let after = estimator.count(&String::from_utf8_lossy(&out));
        assert!(
            after < before / 2,
            "compression was discarded by shaping: {before} -> {after}"
        );
    }

    #[test]
    fn shaping_is_deterministic() {
        // Invariant I4 — the note must not depend on anything but the input.
        let source = prose_request();
        let first = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        )
        .into_owned();

        for _ in 0..20 {
            let again = compress_dialect(
                Dialect::Anthropic,
                source.as_bytes(),
                &compressors(),
                true,
                payg(),
                Verbosity::Terse,
            )
            .into_owned();
            assert_eq!(sha(&again), sha(&first));
        }
    }

    #[test]
    fn shaping_twice_does_not_append_the_note_twice() {
        // Invariant I3, and the practical failure it prevents: an agent loop that
        // accumulates the same instruction a dozen times over a long session.
        let source = prose_request();
        let once = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        )
        .into_owned();
        let twice = compress_dialect(
            Dialect::Anthropic,
            &once,
            &compressors(),
            true,
            payg(),
            Verbosity::Terse,
        )
        .into_owned();

        assert_eq!(sha(&twice), sha(&once));
    }

    // ---- tokenizer selection ----

    #[test]
    fn the_model_in_the_request_selects_the_tokenizer() {
        // The wiring that makes gap row T2 take effect. Without it the exact
        // vocabularies are compiled in and never consulted.
        let compressors = compressors();

        assert!(compressors.tokenizer_for("gpt-4o").is_exact());
        assert_eq!(compressors.tokenizer_for("gpt-4o").name(), "o200k_base");
    }

    #[test]
    fn a_model_with_no_exact_tokenizer_keeps_the_heuristic() {
        // An OpenAI vocabulary applied to an Anthropic model would be a wrong count
        // reported as exact, which is worse than an honest upper bound.
        let compressors = compressors();

        for model in ["claude-opus-4", "gemini-2.5-pro", "", "something-new"] {
            assert!(
                !compressors.tokenizer_for(model).is_exact(),
                "{model:?} claimed an exact tokenizer"
            );
        }
    }

    #[test]
    fn the_model_is_read_from_the_request_body() {
        assert_eq!(model_of(br#"{"model":"gpt-4o","messages":[]}"#), "gpt-4o");
        assert_eq!(
            model_of(br#"{"messages":[], "model" : "claude-opus-4" }"#),
            "claude-opus-4"
        );
    }

    #[test]
    fn a_body_with_no_model_resolves_to_the_heuristic() {
        // Wrong in the safe direction: no model means no exact vocabulary to claim.
        for body in [
            &br#"{"messages":[]}"#[..],
            &b"{not json"[..],
            &b""[..],
            &br#"{"model":123}"#[..],
        ] {
            assert_eq!(model_of(body), "", "{body:?}");
            assert!(!compressors().tokenizer_for(model_of(body)).is_exact());
        }
    }

    #[test]
    fn compression_still_holds_every_invariant_with_an_exact_tokenizer() {
        // The exact count accepts results the heuristic would have rejected, so the
        // guarantees have to be re-checked on that path rather than assumed to carry.
        let source = request().replace(r#""model":"claude-opus-4""#, r#""model":"gpt-4o""#);
        let out = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(before["system"], after["system"], "I2: hot zone modified");
        assert_eq!(before["tools"], after["tools"]);
        for index in 0..5 {
            assert_eq!(
                before["messages"][index], after["messages"][index],
                "I2: frozen turn {index} modified"
            );
        }

        // I4, on the exact path.
        let again = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );
        assert_eq!(sha(&out), sha(&again));
    }

    // ---- invariants ----

    #[test]
    fn sacrosanct_blocks_are_never_touched() {
        let source = format!(
            r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"...","signature":"sig","content":"{}"}}]}}]}}"#,
            bulky_tool_output()
        );
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn a_restricted_policy_forwards_everything_untouched() {
        // Invariant I10 at the dispatch point. Every compressor wired here is lossy,
        // so subscription-mode traffic must come back byte-identical however
        // compressible the payload looks.
        let source = request();
        let restricted = CompressionPolicy::for_mode(headroom_core::AuthMode::Subscription);
        let out = compress_request(source.as_bytes(), &compressors(), true, restricted);

        assert_eq!(sha(&out), sha(source.as_bytes()));
        assert!(matches!(out, Cow::Borrowed(_)), "should not have rebuilt");
    }

    #[test]
    fn oauth_traffic_now_gets_lossless_compression() {
        // Before this, OAuth traffic received *no* compression at all: every wired
        // compressor was lossy, so the policy routed nothing. A meaning-preserving
        // reformat cannot exceed the granted scope, which is the OAuth hazard — so
        // OAuth now means less compression rather than none.
        let records: Vec<String> = (0..120)
            .map(|i| format!(r#"{{ \"path\" : \"src/m_{i}.rs\" , \"size\" : {i} }}"#))
            .collect();
        let source = format!(
            r#"{{"model":"claude-opus-4","messages":[{{"role":"user","content":"q"}},{{"role":"assistant","content":"a"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":"[ {} ]"}}]}}]}}"#,
            records.join(" , ")
        );

        let oauth = CompressionPolicy::for_mode(headroom_core::AuthMode::OAuth);
        let out = compress_request(source.as_bytes(), &compressors(), true, oauth);

        assert!(
            out.len() < source.len(),
            "OAuth traffic got nothing: {} -> {}",
            source.len(),
            out.len()
        );

        // And subscription still gets nothing, deliberately — reflowing bytes is a
        // fingerprint change, whatever it does to the meaning.
        let subscription = CompressionPolicy::for_mode(headroom_core::AuthMode::Subscription);
        let out = compress_request(source.as_bytes(), &compressors(), true, subscription);
        assert_eq!(out.as_ref(), source.as_bytes());
    }

    #[test]
    fn lossless_compression_does_not_change_what_the_model_reads() {
        // The property that makes it safe on restricted traffic at all. Checked by
        // comparing the *decoded* tool result, not the bytes — the bytes are supposed
        // to differ.
        let records: Vec<String> = (0..120)
            .map(|i| format!(r#"{{ \"path\" : \"src/m_{i}.rs\" , \"size\" : {i} }}"#))
            .collect();
        let source = format!(
            r#"{{"model":"claude-opus-4","messages":[{{"role":"user","content":"q"}},{{"role":"assistant","content":"a"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t","content":"[ {} ]"}}]}}]}}"#,
            records.join(" , ")
        );

        let oauth = CompressionPolicy::for_mode(headroom_core::AuthMode::OAuth);
        let out = compress_request(source.as_bytes(), &compressors(), true, oauth);

        let before: Value = serde_json::from_str(&source).unwrap();
        let after: Value = serde_json::from_slice(&out).unwrap();

        let decode = |v: &Value| -> Value {
            serde_json::from_str(v["messages"][2]["content"][0]["content"].as_str().unwrap())
                .unwrap()
        };
        assert_eq!(
            decode(&before),
            decode(&after),
            "the reformatter changed the decoded content"
        );

        // And the frozen prefix is still untouched, lossless or not.
        assert_eq!(before["messages"][0], after["messages"][0]);
        assert_eq!(before["system"], after["system"]);
    }

    #[test]
    fn an_oauth_policy_still_forbids_the_lossy_compressors() {
        // Lossless reformatting is permitted on OAuth; lossy compression is not. This
        // fixture is already minified, so the reformatter declines and the body comes
        // back byte-identical — which is what proves no *lossy* compressor ran.
        let source = request();
        let oauth = CompressionPolicy::for_mode(headroom_core::AuthMode::OAuth);
        let out = compress_request(source.as_bytes(), &compressors(), true, oauth);
        assert_eq!(sha(&out), sha(source.as_bytes()));
    }

    #[test]
    fn compression_is_deterministic() {
        // Invariant I4, end to end through the proxy path.
        let source = request();
        let first = compress_request(source.as_bytes(), &compressors(), true, payg()).into_owned();
        for _ in 0..20 {
            let again =
                compress_request(source.as_bytes(), &compressors(), true, payg()).into_owned();
            assert_eq!(sha(&again), sha(&first));
        }
    }

    #[test]
    fn compressing_twice_is_stable() {
        // Invariant I3. A second pass over already-compressed output must not reach
        // further back than the first did.
        let source = request();
        let once = compress_request(source.as_bytes(), &compressors(), true, payg()).into_owned();
        let twice = compress_request(&once, &compressors(), true, payg()).into_owned();
        assert_eq!(sha(&twice), sha(&once));
    }

    #[test]
    fn the_output_remains_valid_json_with_its_structure_intact() {
        let source = request();
        let out = compress_request(source.as_bytes(), &compressors(), true, payg());
        let parsed: Value = serde_json::from_slice(&out).expect("valid json");

        assert_eq!(parsed["messages"].as_array().unwrap().len(), 6);
        // Sibling fields on the rewritten block survive.
        let block = &parsed["messages"][5]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "t_new");
    }

    #[test]
    fn observing_a_request_does_not_change_what_is_forwarded() {
        // Invariant I9, asked the way the invariant is worded: does *observation* alter
        // the bytes? `invariants.rs` sends the same request through two proxies and
        // SHA-compares them, but `AppState::new` always attaches metrics — so that
        // compares observed against observed, which is determinism (I4) rather than this.
        // Here the only difference between the two runs is whether anything is watching.
        let records: Vec<String> = (0..200)
            .map(|i| format!(r#"{{\"id\":{i},\"path\":\"src/m{i}.rs\",\"status\":\"ok\"}}"#))
            .collect();
        let source = format!(
            r#"{{"model":"claude-opus-4","messages":[{{"role":"user","content":"list"}},{{"role":"assistant","content":"ok"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"[{}]"}}]}}]}}"#,
            records.join(",")
        );

        let unobserved = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors(),
            true,
            payg(),
            Verbosity::Default,
        );

        let metrics = std::sync::Arc::new(crate::metrics::Metrics::new());
        let observed = compress_dialect(
            Dialect::Anthropic,
            source.as_bytes(),
            &compressors().with_metrics(metrics.clone()),
            true,
            payg(),
            Verbosity::Default,
        );

        // Both guards are load-bearing. A passthrough is trivially identical, and a
        // metrics sink that recorded nothing would make the comparison meaningless — the
        // first version of this test used a fixture whose newest message is typed prose,
        // so the block-kind gate declined it and 245 bytes came back as 245.
        assert!(
            unobserved.len() < source.len(),
            "nothing was compressed, so identical bytes prove nothing"
        );
        let rendered = metrics.render();
        assert!(
            rendered
                .lines()
                .any(|line| line.contains("routing_total{") && !line.ends_with(" 0")),
            "no routing reason was recorded, so nothing was observed:\n{rendered}"
        );

        assert_eq!(unobserved, observed, "observation changed the bytes");
    }
}
