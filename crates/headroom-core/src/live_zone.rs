//! The live-zone dispatcher — the component that decides what may be touched.
//!
//! Everything else in this crate is machinery: compressors, tokenizers, the CCR
//! store. This module answers the question those depend on — *which bytes are
//! eligible for compression at all?*
//!
//! # The failure this prevents
//!
//! Providers cache the prompt prefix. Compressing a message the provider has already
//! seen invalidates that cached prefix, so the request costs **more** than it would
//! have with no compression at all, while simultaneously giving the model less
//! context to work with. Losing on both axes at once is the specific outcome this
//! module exists to make impossible.
//!
//! # What the live zone is
//!
//! Scanning from the tail, the live zone is the newest instance of each of:
//!
//! - the latest user message's text
//! - the latest tool result
//! - the latest function-call output
//! - the latest local-shell-call output
//! - the latest apply-patch-call output
//!
//! # Why only the *newest* instance
//!
//! `frozen_message_count` is a floor the caller supplies, derived from the
//! customer's `cache_control` markers. Taking every eligible block above that floor
//! would be the obvious reading, but it is not the safe one: a message can sit above
//! the floor and still have been sent upstream in an earlier request, and the floor
//! only tells us where the customer explicitly asked for a cache breakpoint.
//!
//! So the newest-instance rule is applied *on top of* the floor. The two failure
//! directions are not symmetric:
//!
//! - Compress too little → some tokens go uncompressed. Costs money.
//! - Compress too much → a cached prefix is invalidated. Costs money **and** context,
//!   and does so silently.
//!
//! When one side of a tradeoff fails loudly and cheaply and the other fails silently
//! and expensively, the choice is not close.

use crate::block::{Block, BlockKind};
use crate::conversation::{Conversation, Role};

/// Where a block lives in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Location {
    /// Index into the message array.
    pub message: usize,
    /// Index into that message's block array.
    pub block: usize,
}

/// The set of blocks eligible for compression.
///
/// Locations are held rather than references so the set can be computed once from an
/// immutable borrow and then applied through a mutable one, without the dispatcher
/// itself ever holding a mutable handle on the conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveZone {
    locations: Vec<Location>,
}

impl LiveZone {
    /// The eligible locations, in ascending conversation order.
    ///
    /// The ordering is stable and explicit rather than whatever the scan happened to
    /// produce, because invariant I4 requires the same input to yield byte-equal
    /// output — and a compressor that saw blocks in a different order could make
    /// different decisions.
    pub fn locations(&self) -> &[Location] {
        &self.locations
    }

    /// How many blocks are eligible.
    pub fn len(&self) -> usize {
        self.locations.len()
    }

    /// Whether nothing is eligible.
    ///
    /// The common and correct outcome for a request whose newest turn is small, or
    /// whose entire history is frozen.
    pub fn is_empty(&self) -> bool {
        self.locations.is_empty()
    }

    /// Runs `f` against each eligible block, in conversation order.
    ///
    /// This is the only way to mutate through a `LiveZone`, so a caller cannot
    /// accidentally apply a transform to a block the dispatcher excluded.
    pub fn for_each_mut(&self, conversation: &mut Conversation, mut f: impl FnMut(&mut Block)) {
        let messages = conversation.messages_mut();
        for location in &self.locations {
            // Indices came from this same conversation, but the conversation is
            // borrowed mutably in between, so they are re-checked rather than
            // indexed blindly. A stale index should yield nothing, not a panic.
            if let Some(message) = messages.get_mut(location.message) {
                if let Some(block) = message.blocks_mut().get_mut(location.block) {
                    f(block);
                }
            }
        }
    }
}

/// The categories that are independently "latest".
///
/// Each is tracked separately because a conversation can have its newest tool result
/// and its newest user text in different messages, and both are live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    UserText,
    ToolOutput(BlockKind),
}

/// Computes the live zone for `conversation`.
///
/// `frozen_message_count` is the exclusive floor: messages at an index below it are
/// never eligible, whatever they contain.
///
/// # Example
///
/// ```
/// use headroom_core::block::{Block, BlockKind};
/// use headroom_core::conversation::{Conversation, Message, Role};
/// use headroom_core::live_zone::live_zone;
///
/// let convo = Conversation::new(
///     Some("system prompt".into()),
///     vec!["read_file".into()],
///     vec![
///         Message::new(Role::User, vec![Block::new(BlockKind::Text, "old question")]),
///         Message::new(Role::Assistant, vec![Block::new(BlockKind::ToolUse, "{}")]),
///         Message::new(Role::User, vec![Block::tool_result("fresh output", "t1")]),
///     ],
/// );
///
/// // Only the newest tool result is eligible; the older turn is frozen.
/// let zone = live_zone(&convo, 0);
/// assert_eq!(zone.len(), 1);
/// assert_eq!(zone.locations()[0].message, 2);
/// ```
pub fn live_zone(conversation: &Conversation, frozen_message_count: usize) -> LiveZone {
    let messages = conversation.messages();

    // Nothing above the floor means nothing is live. This is the ordinary case for a
    // request whose history the customer has fully pinned.
    if frozen_message_count >= messages.len() {
        return LiveZone::default();
    }

    // The single newest user message. Text is live only if it is in *this* message.
    //
    // The distinction matters and is easy to get wrong: "the latest user text" and
    // "the text of the latest user message" are different things. In an agent loop
    // the newest user message usually carries tool results and no prose, while the
    // most recent message containing prose can be several turns back — already sent
    // upstream, already cached. Compressing that older prose is exactly the
    // cache-invalidating mistake this module exists to prevent, so text outside the
    // newest user message is frozen even though it is above the floor.
    let latest_user_message = (frozen_message_count..messages.len())
        .rev()
        .find(|&i| messages[i].role() == Role::User);

    // Which message index claimed each category. The first message encountered
    // scanning backwards is the newest one, and it holds the claim thereafter.
    let mut claimed: Vec<(Category, usize)> = Vec::new();
    let mut locations: Vec<Location> = Vec::new();

    for message_index in (frozen_message_count..messages.len()).rev() {
        let message = &messages[message_index];

        for (block_index, block) in message.blocks().iter().enumerate() {
            let Some(category) = categorize(message.role(), block) else {
                continue;
            };

            if category == Category::UserText && Some(message_index) != latest_user_message {
                continue;
            }

            match claimed.iter().find(|(c, _)| *c == category) {
                // A newer message already owns this category.
                Some((_, owner)) if *owner != message_index => continue,
                // This message owns it — keep going, so parallel tool calls that put
                // several results in one message all stay live. Skipping the siblings
                // would leave the bulkiest content in the request uncompressed.
                Some(_) => {}
                None => claimed.push((category, message_index)),
            }

            locations.push(Location {
                message: message_index,
                block: block_index,
            });
        }
    }

    // Ascending order regardless of the reverse scan — see `LiveZone::locations`.
    locations.sort_unstable();
    LiveZone { locations }
}

/// Which live-zone category a block belongs to, if any.
fn categorize(role: Role, block: &Block) -> Option<Category> {
    // Invariant I8. Signed, encrypted, and redacted content is refused here as well
    // as at `apply_guarded`, so a block that must never be touched is never even
    // offered to a transform.
    if block.kind().is_sacrosanct() || !block.kind().is_compressible() {
        return None;
    }

    if block.kind().is_tool_output() {
        return Some(Category::ToolOutput(block.kind()));
    }

    match (role, block.kind()) {
        // Only *user* text is live. Assistant text is content the model already
        // produced and the provider has already seen; rewriting it would modify
        // history for no benefit, since it is not where the tokens are.
        (Role::User, BlockKind::Text) => Some(Category::UserText),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::Message;

    fn text(content: &str) -> Block {
        Block::new(BlockKind::Text, content)
    }

    /// system + tools + 5 historical turns + a new tool result.
    fn fixture() -> Conversation {
        Conversation::new(
            Some("you are a helpful assistant".into()),
            vec!["read_file".into(), "run_shell".into()],
            vec![
                Message::new(Role::User, vec![text("turn 1 question")]),
                Message::new(Role::Assistant, vec![text("turn 1 answer")]),
                Message::new(Role::User, vec![text("turn 2 question")]),
                Message::new(Role::Assistant, vec![text("turn 2 answer")]),
                Message::new(Role::User, vec![Block::tool_result("turn 3 output", "t3")]),
                Message::new(
                    Role::User,
                    vec![Block::tool_result("the newest output", "t_new")],
                ),
            ],
        )
    }

    #[test]
    fn the_newest_tool_result_is_live_and_the_older_one_is_not() {
        let convo = fixture();
        let zone = live_zone(&convo, 0);

        let tool_result_messages: Vec<usize> = zone
            .locations()
            .iter()
            .filter(|l| {
                convo.messages()[l.message].blocks()[l.block].kind() == BlockKind::ToolResult
            })
            .map(|l| l.message)
            .collect();

        assert_eq!(tool_result_messages, vec![5], "only the newest tool result");
    }

    #[test]
    fn system_tools_and_history_are_byte_identical_after_compressing_the_live_zone() {
        // The acceptance test from the issue. Compress everything the dispatcher
        // offers, then assert nothing outside the live zone moved.
        let mut convo = fixture();
        let before = convo.clone();

        let zone = live_zone(&convo, 0);
        zone.for_each_mut(&mut convo, |block| {
            block.replace_content("COMPRESSED");
        });

        assert_eq!(convo.system(), before.system(), "system prompt modified");
        assert_eq!(convo.tools(), before.tools(), "tool definitions modified");

        for index in 0..5 {
            assert_eq!(
                convo.messages()[index],
                before.messages()[index],
                "historical turn {index} modified"
            );
        }

        // ...and the newest turn actually was compressed, so the test cannot pass
        // vacuously by the dispatcher simply doing nothing.
        assert_eq!(convo.messages()[5].blocks()[0].content(), "COMPRESSED");
    }

    #[test]
    fn nothing_below_the_frozen_floor_is_ever_offered() {
        let convo = fixture();
        for floor in 0..=convo.messages().len() {
            let zone = live_zone(&convo, floor);
            for location in zone.locations() {
                assert!(
                    location.message >= floor,
                    "floor {floor} yielded message {}",
                    location.message
                );
            }
        }
    }

    #[test]
    fn a_fully_frozen_conversation_has_an_empty_live_zone() {
        let convo = fixture();
        let zone = live_zone(&convo, convo.messages().len());
        assert!(zone.is_empty());

        // And a floor beyond the end is handled rather than panicking.
        assert!(live_zone(&convo, 999).is_empty());
    }

    #[test]
    fn an_empty_conversation_has_an_empty_live_zone() {
        assert!(live_zone(&Conversation::default(), 0).is_empty());
    }

    #[test]
    fn parallel_tool_results_in_one_message_are_all_live() {
        // The case a naive "latest tool_result" reading gets wrong: parallel tool
        // calls return several results in a single message, and skipping the
        // siblings would leave the bulkiest content in the request uncompressed.
        let convo = Conversation::new(
            None,
            vec![],
            vec![Message::new(
                Role::User,
                vec![
                    Block::tool_result("first result", "t1"),
                    Block::tool_result("second result", "t2"),
                    Block::tool_result("third result", "t3"),
                ],
            )],
        );

        let zone = live_zone(&convo, 0);
        assert_eq!(zone.len(), 3);
        assert_eq!(
            zone.locations().iter().map(|l| l.block).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn each_output_shape_is_tracked_independently() {
        // A conversation can have its newest function-call output and its newest
        // local-shell output in different messages. Both are live.
        let convo = Conversation::new(
            None,
            vec![],
            vec![
                Message::new(
                    Role::User,
                    vec![Block::new(BlockKind::FunctionCallOutput, "fn output")],
                ),
                Message::new(
                    Role::User,
                    vec![Block::new(BlockKind::LocalShellCallOutput, "shell output")],
                ),
                Message::new(
                    Role::User,
                    vec![Block::new(BlockKind::ApplyPatchCallOutput, "patch output")],
                ),
            ],
        );

        let zone = live_zone(&convo, 0);
        assert_eq!(zone.len(), 3, "all three shapes should be live");
    }

    #[test]
    fn an_older_output_shape_is_frozen_by_a_newer_one_of_the_same_shape() {
        let convo = Conversation::new(
            None,
            vec![],
            vec![
                Message::new(
                    Role::User,
                    vec![Block::new(BlockKind::FunctionCallOutput, "older")],
                ),
                Message::new(
                    Role::User,
                    vec![Block::new(BlockKind::FunctionCallOutput, "newer")],
                ),
            ],
        );

        let zone = live_zone(&convo, 0);
        assert_eq!(zone.len(), 1);
        assert_eq!(zone.locations()[0].message, 1);
    }

    #[test]
    fn user_text_from_an_older_turn_is_frozen_by_a_newer_user_message() {
        // The case that caught a real bug during development. "The latest user text"
        // and "the text of the latest user message" are different things: in an
        // agent loop the newest user message carries tool results and no prose,
        // while the most recent prose can be several turns back — already sent
        // upstream and already cached. Compressing it busts the prefix.
        let convo = Conversation::new(
            None,
            vec![],
            vec![
                Message::new(Role::User, vec![text("a question from several turns ago")]),
                Message::new(Role::Assistant, vec![text("an answer")]),
                Message::new(Role::User, vec![Block::tool_result("newest output", "t1")]),
            ],
        );

        let zone = live_zone(&convo, 0);
        assert_eq!(zone.len(), 1, "only the newest tool result should be live");
        assert_eq!(zone.locations()[0].message, 2);
    }

    #[test]
    fn user_text_is_live_when_it_is_in_the_newest_user_message() {
        // The complement: prose in the newest user message is exactly what should be
        // compressed, so the guard above must not over-reach.
        let convo = Conversation::new(
            None,
            vec![],
            vec![
                Message::new(Role::User, vec![text("older question")]),
                Message::new(Role::Assistant, vec![text("older answer")]),
                Message::new(Role::User, vec![text("the newest question")]),
            ],
        );

        let zone = live_zone(&convo, 0);
        assert_eq!(zone.len(), 1);
        assert_eq!(zone.locations()[0].message, 2);
    }

    #[test]
    fn assistant_text_is_never_live() {
        // Model output the provider has already seen. Rewriting it modifies history
        // for no benefit.
        let convo = Conversation::new(
            None,
            vec![],
            vec![Message::new(
                Role::Assistant,
                vec![text(
                    "a long assistant response with plenty of content in it",
                )],
            )],
        );
        assert!(live_zone(&convo, 0).is_empty());
    }

    #[test]
    fn sacrosanct_blocks_are_never_live_even_in_the_newest_message() {
        for kind in [
            BlockKind::Thinking,
            BlockKind::RedactedThinking,
            BlockKind::Reasoning,
            BlockKind::Compaction,
            BlockKind::Attachment,
        ] {
            let convo = Conversation::new(
                None,
                vec![],
                vec![Message::new(
                    Role::User,
                    vec![Block::new(kind, "opaque provider material")],
                )],
            );
            assert!(live_zone(&convo, 0).is_empty(), "{kind} was offered");
        }
    }

    #[test]
    fn tool_use_is_never_live() {
        // Re-serializing tool arguments reorders JSON keys and busts the cache.
        let convo = Conversation::new(
            None,
            vec![],
            vec![Message::new(
                Role::Assistant,
                vec![Block::new(BlockKind::ToolUse, r#"{"path":"/tmp"}"#)],
            )],
        );
        assert!(live_zone(&convo, 0).is_empty());
    }

    #[test]
    fn locations_are_returned_in_ascending_order() {
        let convo = Conversation::new(
            None,
            vec![],
            vec![Message::new(
                Role::User,
                vec![
                    text("user question"),
                    Block::tool_result("result a", "t1"),
                    Block::tool_result("result b", "t2"),
                ],
            )],
        );

        let zone = live_zone(&convo, 0);
        let mut sorted = zone.locations().to_vec();
        sorted.sort_unstable();
        assert_eq!(zone.locations(), sorted.as_slice());
    }

    #[test]
    fn the_dispatcher_is_deterministic() {
        // Invariant I4. Same conversation, same floor, same eligible set every time.
        let convo = fixture();
        let first = live_zone(&convo, 0);
        for _ in 0..50 {
            assert_eq!(live_zone(&convo, 0), first);
        }
    }

    #[test]
    fn recomputing_after_compression_yields_the_same_locations() {
        // Invariant I3, append-only: compressing the live zone must not shift what
        // the live zone is. If it did, a second pass would reach further back into
        // history than the first.
        let mut convo = fixture();
        let before = live_zone(&convo, 0);

        before.for_each_mut(&mut convo, |block| {
            block.replace_content("COMPRESSED");
        });

        assert_eq!(live_zone(&convo, 0), before);
    }

    #[test]
    fn for_each_mut_visits_exactly_the_eligible_blocks() {
        let mut convo = fixture();
        let zone = live_zone(&convo, 0);
        let expected = zone.len();

        let mut visited = 0;
        zone.for_each_mut(&mut convo, |_| visited += 1);
        assert_eq!(visited, expected);
    }
}
