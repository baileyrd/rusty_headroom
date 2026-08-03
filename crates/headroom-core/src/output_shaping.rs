//! Steering what the model *writes*, not just what it reads.
//!
//! Every other compressor in this crate reduces input tokens. Output tokens are
//! typically the more expensive half of a bill and nothing here can compress them
//! after the fact — by the time the response exists it has already been paid for. The
//! only lever is to ask for less before generation starts.
//!
//! # The constraint that shapes the whole module
//!
//! The obvious way to ask a model for terser output is to append a line to the system
//! prompt. That is the one place it must not go. The system prompt is the first thing
//! in the cached prefix, so editing it invalidates the entire cache for every
//! subsequent request — invariant I2. A terseness note that saves 200 output tokens
//! and re-bills 20,000 cached input tokens has made things worse while appearing to
//! help, and the metric it moves is the one people watch.
//!
//! So the note goes in the **live zone tail**: appended to the newest user message,
//! the one region compression is already permitted to touch. It costs a few input
//! tokens on an uncached message and invalidates nothing.

use crate::conversation::Conversation;

/// How much output to ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    /// Ask for the shortest useful answer.
    Terse,
    /// Say nothing; let the model decide.
    Default,
    /// Ask for full working.
    Full,
}

impl Verbosity {
    /// The note to append to the live-zone tail, if any.
    ///
    /// [`Verbosity::Default`] yields `None` rather than an empty string, so "add
    /// nothing" and "add nothing visible" cannot be confused at the call site — an
    /// empty append still modifies the message, and a modified message is a different
    /// message.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Self::Terse => Some(
                "\n\nRespond as briefly as the question allows. \
                 Skip preamble, restatement, and summary.",
            ),
            Self::Default => None,
            Self::Full => Some("\n\nShow your full reasoning and working."),
        }
    }
}

/// How hard to ask the model to think.
///
/// Distinct from verbosity: a terse answer to a hard question still needs the
/// reasoning, and conflating the two produces short wrong answers — the failure mode
/// that makes an output-shaping feature something users switch off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    /// Routine follow-up.
    Low,
    /// The default.
    Medium,
    /// A new problem, or a recovery from failure.
    High,
}

impl Effort {
    /// The OpenAI `reasoning_effort` value.
    pub fn as_openai(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// The Anthropic `thinking.budget_tokens` value.
    pub fn as_thinking_budget(self) -> u32 {
        match self {
            Self::Low => 1024,
            Self::Medium => 4096,
            Self::High => 16384,
        }
    }
}

/// Signals that force full effort regardless of anything else.
///
/// Deliberately weighted toward errors. Under-thinking a recovery is how an agent
/// loops on the same failure, which costs far more than the tokens saved by routing it
/// cheaply.
const ESCALATING: [&str; 10] = [
    "error",
    "exception",
    "traceback",
    "panic",
    "failed",
    "failure",
    "does not work",
    "doesn't work",
    "still broken",
    "why did",
];

/// Signals that a turn is a routine continuation.
const ROUTINE: [&str; 8] = [
    "thanks",
    "thank you",
    "ok",
    "okay",
    "continue",
    "go on",
    "next",
    "sounds good",
];

/// Chooses the effort level for the newest turn of `conversation`.
///
/// # The asymmetry
///
/// Routing too high costs some thinking tokens. Routing too low produces a shallow
/// answer to a hard question, and an agent that has to ask again has spent the whole
/// exchange twice over — plus the user's patience. So every uncertain case routes
/// [`Effort::Medium`] or above, and only an unambiguously routine turn routes low.
///
/// # Example
///
/// ```
/// use headroom_core::conversation::{Conversation, Message, Role};
/// use headroom_core::block::{Block, BlockKind};
/// use headroom_core::output_shaping::{route_effort, Effort};
///
/// let user = |text: &str| Message::new(Role::User, vec![Block::new(BlockKind::Text, text)]);
/// let model = |text: &str| Message::new(Role::Assistant, vec![Block::new(BlockKind::Text, text)]);
///
/// let broken = Conversation::new(None, Vec::new(), vec![user("it panics with an index error")]);
/// assert_eq!(route_effort(&broken), Effort::High);
///
/// // A routine turn needs prior context to be routine — the opening turn of any
/// // conversation is a new problem by definition, whatever it says.
/// let routine = Conversation::new(
///     None,
///     Vec::new(),
///     vec![user("write the parser"), model("done"), user("thanks, continue")],
/// );
/// assert_eq!(route_effort(&routine), Effort::Low);
/// ```
pub fn route_effort(conversation: &Conversation) -> Effort {
    let Some(text) = newest_user_text(conversation) else {
        // Nothing to read. A conversation this code cannot make sense of gets the
        // middle setting rather than the cheap one.
        return Effort::Medium;
    };
    let lowered = text.to_lowercase();

    // Errors are checked first and unconditionally. A turn that says "thanks, but it
    // still errors" is a recovery, not a pleasantry, and testing the routine list
    // first would route it cheaply on the strength of its first word.
    if ESCALATING.iter().any(|signal| lowered.contains(signal)) {
        return Effort::High;
    }

    // The first turn of a conversation is a new problem by definition.
    if conversation.messages().len() <= 1 {
        return Effort::High;
    }

    // Routine only when the whole turn is routine. A long message that happens to open
    // with "ok" is not a continuation, and length is the cheap signal that says so.
    let trimmed = lowered.trim();
    if trimmed.len() <= 40 && ROUTINE.iter().any(|signal| trimmed.contains(signal)) {
        return Effort::Low;
    }

    Effort::Medium
}

/// The text of the newest user message.
fn newest_user_text(conversation: &Conversation) -> Option<String> {
    let message = conversation
        .messages()
        .iter()
        .rev()
        .find(|message| message.role() == crate::conversation::Role::User)?;

    let text: Vec<&str> = message
        .blocks()
        .iter()
        .filter(|block| block.kind() == crate::block::BlockKind::Text)
        .map(|block| block.content())
        .collect();

    (!text.is_empty()).then(|| text.join("\n"))
}

/// Appends `verbosity`'s note to the newest user message's last text block.
///
/// Returns the index of the message and block that changed, so a caller rewriting raw
/// JSON knows exactly what to touch — and so this function does not need to know how
/// the request was serialized.
///
/// Returns `None` when nothing should change: a [`Verbosity::Default`] setting, no
/// user message, no text block to append to, or a note that is already present.
///
/// # Why re-appending is guarded against
///
/// An agent loop calls this on every turn. Without the check, a long session
/// accumulates the same instruction a dozen times over — which is both wasted tokens
/// and a genuinely worse prompt, since a repeated instruction reads as emphasis.
pub fn verbosity_append(
    conversation: &Conversation,
    verbosity: Verbosity,
) -> Option<(usize, usize, String)> {
    let note = verbosity.note()?;

    let (message_index, message) = conversation
        .messages()
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message.role() == crate::conversation::Role::User)?;

    let (block_index, block) = message
        .blocks()
        .iter()
        .enumerate()
        .rev()
        .find(|(_, block)| block.kind() == crate::block::BlockKind::Text)?;

    if block.content().contains(note.trim()) {
        return None;
    }

    Some((
        message_index,
        block_index,
        format!("{}{note}", block.content()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Block, BlockKind};
    use crate::conversation::{Message, Role};

    fn user(text: &str) -> Message {
        Message::new(Role::User, vec![Block::new(BlockKind::Text, text)])
    }

    fn assistant(text: &str) -> Message {
        Message::new(Role::Assistant, vec![Block::new(BlockKind::Text, text)])
    }

    fn convo(messages: Vec<Message>) -> Conversation {
        Conversation::new(Some("You are helpful.".into()), Vec::new(), messages)
    }

    // ---- verbosity ----

    #[test]
    fn the_note_targets_the_newest_user_message_not_the_system_prompt() {
        // The whole point. The system prompt is the first thing in the cached prefix,
        // so a note appended there invalidates the entire cache on every request — a
        // change that saves 200 output tokens and re-bills 20,000 input ones.
        let conversation = convo(vec![user("first"), assistant("reply"), user("second")]);

        let (message, block, _) = verbosity_append(&conversation, Verbosity::Terse).unwrap();
        assert_eq!(message, 2, "the note landed outside the live zone");
        assert_eq!(block, 0);

        // And the system prompt is untouched, because there is no accessor that could
        // have touched it.
        assert_eq!(conversation.system(), Some("You are helpful."));
    }

    #[test]
    fn the_original_text_is_preserved_and_the_note_appended() {
        let conversation = convo(vec![user("what does this do?")]);
        let (_, _, rewritten) = verbosity_append(&conversation, Verbosity::Terse).unwrap();

        assert!(rewritten.starts_with("what does this do?"));
        assert!(rewritten.contains("briefly"));
    }

    #[test]
    fn the_note_is_not_appended_twice() {
        // An agent loop calls this every turn. Without the guard a long session
        // accumulates the instruction a dozen times — wasted tokens, and a worse prompt,
        // since a repeated instruction reads as emphasis.
        let conversation = convo(vec![user("what does this do?")]);
        let (_, _, once) = verbosity_append(&conversation, Verbosity::Terse).unwrap();

        let already = convo(vec![user(&once)]);
        assert_eq!(
            verbosity_append(&already, Verbosity::Terse),
            None,
            "the note was appended a second time"
        );
    }

    #[test]
    fn the_default_verbosity_changes_nothing_at_all() {
        // `None` rather than an empty append. An empty append still modifies the
        // message, and a modified message is a different message.
        assert_eq!(Verbosity::Default.note(), None);
        assert_eq!(
            verbosity_append(&convo(vec![user("hi")]), Verbosity::Default),
            None
        );
    }

    #[test]
    fn a_conversation_with_nothing_to_append_to_is_left_alone() {
        for messages in [
            vec![],
            vec![assistant("only the model spoke")],
            vec![Message::new(
                Role::User,
                vec![Block::new(BlockKind::Attachment, "an image")],
            )],
        ] {
            assert_eq!(verbosity_append(&convo(messages), Verbosity::Terse), None);
        }
    }

    // ---- effort ----

    #[test]
    fn an_error_forces_full_effort() {
        for text in [
            "it panics with an index error",
            "Traceback (most recent call last)",
            "the build failed",
            "why did that not work",
        ] {
            let conversation = convo(vec![user("earlier"), assistant("ok"), user(text)]);
            assert_eq!(route_effort(&conversation), Effort::High, "{text:?}");
        }
    }

    #[test]
    fn a_pleasantry_wrapped_around_an_error_is_still_an_error() {
        // The ordering that matters. Testing the routine list first would route
        // "thanks, but it still errors" cheaply on the strength of its first word,
        // which is exactly the turn that needs the most thought.
        let conversation = convo(vec![
            user("earlier"),
            assistant("ok"),
            user("thanks, but it still errors"),
        ]);
        assert_eq!(route_effort(&conversation), Effort::High);
    }

    #[test]
    fn a_short_routine_continuation_routes_low() {
        for text in ["thanks", "ok continue", "next", "sounds good"] {
            let conversation = convo(vec![user("earlier"), assistant("ok"), user(text)]);
            assert_eq!(route_effort(&conversation), Effort::Low, "{text:?}");
        }
    }

    #[test]
    fn a_long_message_that_merely_opens_with_ok_is_not_routine() {
        // Length is the cheap signal that separates "ok" from "ok, now here is the
        // actual problem", and routing the latter low produces a shallow answer to the
        // real question.
        let conversation = convo(vec![
            user("earlier"),
            assistant("sure"),
            user(
                "ok, now rewrite the scheduler so it handles the backpressure case \
                  without dropping work on the floor",
            ),
        ]);
        assert_ne!(route_effort(&conversation), Effort::Low);
    }

    #[test]
    fn the_opening_turn_of_a_conversation_is_a_new_problem() {
        assert_eq!(
            route_effort(&convo(vec![user("design a rate limiter")])),
            Effort::High
        );
    }

    #[test]
    fn an_unreadable_conversation_routes_medium_not_low() {
        // Wrong in the safe direction. Routing too high costs thinking tokens; routing
        // too low produces a shallow answer and the exchange happens twice.
        assert_eq!(route_effort(&convo(vec![])), Effort::Medium);
        assert_eq!(
            route_effort(&convo(vec![assistant("only the model spoke")])),
            Effort::Medium
        );
    }

    #[test]
    fn effort_routing_is_deterministic() {
        // Invariant I4. Nothing here may consult a clock or an RNG.
        let conversation = convo(vec![user("earlier"), assistant("ok"), user("thanks")]);
        let first = route_effort(&conversation);
        for _ in 0..25 {
            assert_eq!(route_effort(&conversation), first);
        }
    }

    #[test]
    fn effort_and_verbosity_are_separate_dials() {
        // Conflating them produces short wrong answers, which is the failure mode that
        // makes users switch an output-shaping feature off entirely.
        let broken = convo(vec![
            user("earlier"),
            assistant("ok"),
            user("it errors on startup"),
        ]);

        assert_eq!(route_effort(&broken), Effort::High);
        // A terse answer to a hard question is still a legitimate request.
        assert!(verbosity_append(&broken, Verbosity::Terse).is_some());
    }

    #[test]
    fn every_effort_level_maps_to_both_provider_dialects() {
        for effort in [Effort::Low, Effort::Medium, Effort::High] {
            assert!(!effort.as_openai().is_empty());
            assert!(effort.as_thinking_budget() > 0);
        }
        // And the ordering survives the mapping, or the levels mean nothing.
        assert!(Effort::Low.as_thinking_budget() < Effort::Medium.as_thinking_budget());
        assert!(Effort::Medium.as_thinking_budget() < Effort::High.as_thinking_budget());
    }
}
