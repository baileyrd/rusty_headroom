//! Finding content that silently destroys the prompt cache — gap row X17.
//!
//! # The bug this module exists because of, and does not repeat
//!
//! A timestamp in a system prompt changes every request. The provider caches on an
//! exact prefix match, so the prefix never matches and the customer pays full price
//! for their entire conversation every single turn — while a dashboard shows the proxy
//! compressing happily and saving tokens on the live zone.
//!
//! The reference records that the original implementation tried to *fix* this by
//! rewriting the volatile value. That was the defect. Rewriting a customer's system
//! prompt:
//!
//! - changes what the model is told, silently, which is not a compression decision;
//! - modifies the cache hot zone, which invariant I2 forbids outright;
//! - and busts the cache *itself* on the turn it takes effect.
//!
//! So this module **only reports**. It has no function that returns modified content,
//! and that absence is the design. A human decides whether a timestamp in their system
//! prompt is worth removing; this proxy is not entitled to decide for them.

use serde_json::Value;

/// What kind of volatility was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VolatileKind {
    /// A date or time.
    Timestamp,
    /// A UUID.
    Uuid,
    /// A long hex run — a session id, request id, or hash.
    HexToken,
    /// A counter or sequence number in a field that names itself as one.
    Counter,
}

impl VolatileKind {
    /// A stable identifier, for logs and telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timestamp => "timestamp",
            Self::Uuid => "uuid",
            Self::HexToken => "hex_token",
            Self::Counter => "counter",
        }
    }
}

/// One piece of volatile content found in the cache hot zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Which member it was found in — `system`, `tools`, `instructions`, or
    /// `system message`.
    pub location: String,
    /// What kind of volatility it is.
    pub kind: VolatileKind,
    /// The matched text, truncated.
    ///
    /// Truncated because a finding goes into a log line, and a system prompt is
    /// customer content that should not be reproduced wholesale somewhere it will be
    /// aggregated and retained.
    pub sample: String,
}

/// How much of a match is reproduced in a finding.
const SAMPLE_CHARS: usize = 24;

/// Scans the cache hot zone of a request body, in any dialect this proxy relays.
///
/// The system instruction lives somewhere different on each surface, and this used to
/// know only Anthropic's:
///
/// | surface | where |
/// | --- | --- |
/// | Anthropic | `system` |
/// | OpenAI Responses | `instructions` |
/// | OpenAI chat completions | a `system` or `developer` message |
/// | all three | `tools` |
///
/// Only the hot zone is scanned. Volatile content in the *live* zone is expected and
/// harmless: that content was never cached, so nothing is invalidated by it changing.
/// Reporting it would bury the findings that matter under noise from every request
/// carrying a fresh tool result — which is also why a `user` message is skipped even
/// though it sits in the prefix. It is not there by the operator's choice, and there is
/// nothing for them to do about it.
///
/// # Never returns modified content
///
/// There is no counterpart to this function that rewrites what it finds. See the module
/// documentation for why.
///
/// # Example
///
/// ```
/// use headroom_proxy::volatile::{scan, VolatileKind};
///
/// let body = br#"{"system":"Current time: 2026-08-03T04:00:00Z","messages":[]}"#;
/// let findings = scan(body);
///
/// assert_eq!(findings.len(), 1);
/// assert_eq!(findings[0].kind, VolatileKind::Timestamp);
/// ```
pub fn scan(body: &[u8]) -> Vec<Finding> {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        // Unparseable input is not this module's problem to report. The compressor
        // already forwards it untouched, and a warning about JSON nobody could read
        // would be noise on a path that is already handled.
        return Vec::new();
    };

    let mut findings = Vec::new();

    // Anthropic: a top-level `system` string or block array.
    if let Some(system) = parsed.get("system") {
        collect(system, "system", &mut findings);
    }
    // Every surface puts tool definitions at the top level, and they sit in the cached
    // prefix on all three.
    if let Some(tools) = parsed.get("tools") {
        collect(tools, "tools", &mut findings);
    }
    // OpenAI Responses: the system instruction is `instructions`.
    if let Some(instructions) = parsed.get("instructions") {
        collect(instructions, "instructions", &mut findings);
    }
    // OpenAI chat completions: the system instruction is a message with a `system` or
    // `developer` role. Scanned rather than the whole array, deliberately — a timestamp
    // in a *user* message is ordinary, and warning about it would train an operator to
    // ignore this log line entirely.
    //
    // This is where the module was blind. It knew `system` and `tools`, both
    // Anthropic-shaped, so an OpenAI system prompt carrying a timestamp reported nothing
    // — on surfaces that cache the prefix automatically, where the customer never opted
    // in and so has even less reason to suspect it.
    if let Some(messages) = parsed.get("messages").and_then(Value::as_array) {
        for message in messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if role != "system" && role != "developer" {
                continue;
            }
            if let Some(content) = message.get("content") {
                collect(content, "system message", &mut findings);
            }
        }
    }

    // Sorted so the same body always reports the same findings in the same order.
    // A log line that reorders between runs is one nobody can diff.
    findings.sort_by(|a, b| {
        a.location
            .cmp(&b.location)
            .then(a.kind.cmp(&b.kind))
            .then(a.sample.cmp(&b.sample))
    });
    findings.dedup();
    findings
}

/// Walks a JSON value, reporting volatile strings.
fn collect(value: &Value, location: &str, findings: &mut Vec<Finding>) {
    match value {
        Value::String(text) => {
            for (kind, sample) in detect_in(text) {
                findings.push(Finding {
                    location: location.to_owned(),
                    kind,
                    sample,
                });
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, location, findings);
            }
        }
        Value::Object(members) => {
            for (key, member) in members {
                // A counter is only recognizable from its *field name*: a bare `47`
                // could be anything, and flagging every integer in a tool schema would
                // make the whole report useless. `"request_number": 47` is different —
                // the field says it increments.
                if let Some(number) = member.as_u64() {
                    if names_a_counter(key) {
                        findings.push(Finding {
                            location: location.to_owned(),
                            kind: VolatileKind::Counter,
                            sample: format!("{key}: {number}"),
                        });
                    }
                }
                collect(member, location, findings);
            }
        }
        _ => {}
    }
}

/// Whether a field name announces itself as a counter.
fn names_a_counter(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "count",
        "seq",
        "index",
        "number",
        "iteration",
        "turn",
        "nonce",
    ]
    .iter()
    .any(|marker| key.contains(marker))
}

/// Finds volatile patterns within one string.
fn detect_in(text: &str) -> Vec<(VolatileKind, String)> {
    let mut found = Vec::new();

    for token in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '"')) {
        let trimmed = token.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | '.'));
        if trimmed.len() < 8 {
            continue;
        }

        if looks_like_uuid(trimmed) {
            found.push((VolatileKind::Uuid, truncate(trimmed)));
        } else if looks_like_timestamp(trimmed) {
            found.push((VolatileKind::Timestamp, truncate(trimmed)));
        } else if looks_like_hex_token(trimmed) {
            found.push((VolatileKind::HexToken, truncate(trimmed)));
        }
    }

    found
}

/// `8-4-4-4-12` hex groups.
fn looks_like_uuid(token: &str) -> bool {
    let groups: Vec<&str> = token.split('-').collect();
    groups.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(expected, group)| {
                group.len() == *expected && group.chars().all(|c| c.is_ascii_hexdigit())
            })
}

/// An ISO-8601 date, with or without a time.
///
/// Anchored on the `YYYY-MM-DD` shape rather than on containing digits and dashes,
/// because a version string like `4-20250514` is not a timestamp and flagging it would
/// make every model identifier in a system prompt a finding.
fn looks_like_timestamp(token: &str) -> bool {
    let date = token.split(['T', ' ']).next().unwrap_or(token);
    let parts: Vec<&str> = date.split('-').collect();

    parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts
            .iter()
            .all(|part| part.chars().all(|c| c.is_ascii_digit()))
}

/// A long run of hex — a session id, request id, or hash.
///
/// The 16-character floor is deliberate. Shorter runs are common in ordinary prose
/// (`deadbeef`, `cafe`) and in legitimate constants, so a lower threshold would report
/// findings on documents containing no volatile content at all — and a report with
/// false positives is one people stop reading.
fn looks_like_hex_token(token: &str) -> bool {
    token.len() >= 16 && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// Truncates a sample for logging.
fn truncate(text: &str) -> String {
    if text.chars().count() <= SAMPLE_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(SAMPLE_CHARS).collect();
    format!("{head}…")
}

/// Logs a warning for each finding.
///
/// Separated from [`scan`] so the scan stays a pure function — testable without a
/// tracing subscriber, and callable from a context that wants the findings rather than
/// the log lines.
pub fn warn_about(findings: &[Finding]) {
    for finding in findings {
        tracing::warn!(
            location = %finding.location,
            kind = finding.kind.as_str(),
            sample = %finding.sample,
            "volatile content in the cache hot zone; the prompt cache will miss on \
             every request until it is removed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(system: &str) -> Vec<u8> {
        serde_json::json!({ "system": system, "messages": [] })
            .to_string()
            .into_bytes()
    }

    // ---- what it finds ----

    #[test]
    fn a_timestamp_in_the_system_prompt_is_reported() {
        // The headline case. It changes every request, so the cached prefix never
        // matches and the customer pays full price for the whole conversation every
        // turn — while the savings dashboard looks healthy.
        let findings = scan(&body("Current time: 2026-08-03T04:00:00Z. Be helpful."));

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, VolatileKind::Timestamp);
        assert_eq!(findings[0].location, "system");
    }

    #[test]
    fn a_uuid_is_reported() {
        let findings = scan(&body(
            "Session 550e8400-e29b-41d4-a716-446655440000 begins.",
        ));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, VolatileKind::Uuid);
    }

    #[test]
    fn a_long_hex_token_is_reported() {
        let findings = scan(&body(
            "Request id a3f5c8d91b2e4f6079ab8cd12ef34567 recorded.",
        ));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, VolatileKind::HexToken);
    }

    #[test]
    fn volatile_content_inside_the_tools_array_is_found() {
        // Tool definitions are part of the cached prefix too, and a generated schema is
        // a common place for a build timestamp to end up.
        let source = serde_json::json!({
            "tools": [{"name": "read", "description": "Generated 2026-08-03"}],
            "messages": [],
        })
        .to_string();

        let findings = scan(source.as_bytes());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].location, "tools");
    }

    #[test]
    fn a_counter_is_recognized_from_its_field_name() {
        // A bare `47` could be anything. `"turn_number": 47` is different — the field
        // says it increments.
        let source = serde_json::json!({
            "tools": [{"name": "x", "turn_number": 47, "max_results": 47}],
            "messages": [],
        })
        .to_string();

        let findings = scan(source.as_bytes());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].kind, VolatileKind::Counter);
        assert!(findings[0].sample.contains("turn_number"));
    }

    // ---- what it must NOT find ----

    #[test]
    fn an_ordinary_system_prompt_reports_nothing() {
        // A report with false positives is one people stop reading, and then the real
        // finding arrives and nobody looks.
        for prompt in [
            "You are a careful assistant. Answer concisely.",
            "Use the read_file tool to inspect source before editing it.",
            "Prefer Rust 2021 edition idioms. MSRV is 1.80.",
            "The deadbeef constant marks an uninitialized region.",
            "Version 4-20250514 of the model.",
        ] {
            assert!(
                scan(&body(prompt)).is_empty(),
                "false positive on {prompt:?}: {:?}",
                scan(&body(prompt))
            );
        }
    }

    #[test]
    fn a_model_identifier_is_not_mistaken_for_a_timestamp() {
        // `claude-opus-4-20250514` has digits and dashes and is not volatile. Flagging
        // it would make every system prompt naming a model a finding.
        assert!(scan(&body("You are claude-opus-4-20250514.")).is_empty());
    }

    #[test]
    fn short_hex_runs_are_not_reported() {
        // Common in prose and in legitimate constants.
        for text in ["cafe", "deadbeef", "abc123", "0xFF00"] {
            assert!(
                scan(&body(&format!("The value {text} is a constant."))).is_empty(),
                "{text}"
            );
        }
    }

    #[test]
    fn volatile_content_in_the_live_zone_is_not_reported() {
        // Expected and harmless: that content was never cached, so nothing is
        // invalidated by it changing. Reporting it would bury the findings that matter
        // under noise from every request carrying a fresh tool result.
        let source = serde_json::json!({
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "Ran at 2026-08-03T04:00:00Z, id 550e8400-e29b-41d4-a716-446655440000"}
            ],
        })
        .to_string();

        assert!(scan(source.as_bytes()).is_empty());
    }

    // ---- the constraint that defines the module ----

    #[test]
    fn scanning_never_returns_content_to_substitute() {
        // Asserted structurally: a `Finding` carries a location, a kind, and a
        // truncated sample. There is nowhere for a replacement to live, so no caller
        // can accidentally apply one — which is the defect the reference records, where
        // the original implementation rewrote the volatile value and thereby modified
        // the cache hot zone that invariant I2 protects.
        let findings = scan(&body("Time: 2026-08-03T04:00:00Z"));
        let finding = &findings[0];

        let _: &String = &finding.location;
        let _: VolatileKind = finding.kind;
        let _: &String = &finding.sample;
    }

    #[test]
    fn the_sample_is_truncated_rather_than_reproducing_the_prompt() {
        // Findings go into log lines, and a system prompt is customer content that
        // should not be reproduced wholesale somewhere it will be aggregated and
        // retained.
        let long = "a".repeat(200);
        let findings = scan(&body(&format!("id {}", "f".repeat(200))));

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].sample.chars().count() <= SAMPLE_CHARS + 1,
            "sample was {} chars",
            findings[0].sample.chars().count()
        );
        assert!(!findings[0].sample.contains(&long));
    }

    // ---- behavior ----

    #[test]
    fn scanning_is_deterministic() {
        // Findings go in a log an operator diffs between deploys. A report that
        // reorders between runs is one nobody can read.
        let source = serde_json::json!({
            "system": "At 2026-08-03 session 550e8400-e29b-41d4-a716-446655440000 id a3f5c8d91b2e4f6079ab8cd1",
            "messages": [],
        })
        .to_string();

        let first = scan(source.as_bytes());
        assert!(first.len() >= 2);
        for _ in 0..25 {
            assert_eq!(scan(source.as_bytes()), first);
        }
    }

    #[test]
    fn the_same_finding_twice_is_reported_once() {
        // A timestamp repeated through a long prompt is one problem, not twelve.
        let findings = scan(&body(
            "2026-08-03T04:00:00Z and again 2026-08-03T04:00:00Z and again 2026-08-03T04:00:00Z",
        ));
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn a_malformed_body_reports_nothing_rather_than_erroring() {
        // The compressor already forwards it untouched; a warning about JSON nobody
        // could read is noise on a path that is already handled.
        for source in [&b"{not json"[..], &b""[..], &b"[1,2,3]"[..]] {
            assert!(scan(source).is_empty());
        }
    }

    #[test]
    fn a_body_with_no_hot_zone_reports_nothing() {
        assert!(scan(br#"{"messages":[]}"#).is_empty());
    }

    #[test]
    fn every_surfaces_system_instruction_is_scanned() {
        // The module knew `system` and `tools`, both Anthropic-shaped. An OpenAI system
        // prompt carrying a timestamp reported nothing — on the two surfaces that cache
        // the prefix *automatically*, where the customer never opted in and so has even
        // less reason to suspect it is costing them full price every turn.
        //
        // Measured before the fix: 0 findings for the two OpenAI shapes, 1 for Anthropic.
        for (label, body) in [
            (
                "anthropic system",
                r#"{"system":"Session started 2026-08-03T12:00:00Z","messages":[]}"#,
            ),
            (
                "openai chat system message",
                r#"{"messages":[{"role":"system","content":"Session started 2026-08-03T12:00:00Z"}]}"#,
            ),
            (
                "openai chat developer message",
                r#"{"messages":[{"role":"developer","content":"Session started 2026-08-03T12:00:00Z"}]}"#,
            ),
            (
                "openai responses instructions",
                r#"{"instructions":"Session started 2026-08-03T12:00:00Z","input":"hi"}"#,
            ),
            (
                "tools, on every surface",
                r#"{"tools":[{"description":"as of 2026-08-03T12:00:00Z"}],"messages":[]}"#,
            ),
        ] {
            let found = scan(body.as_bytes());
            assert_eq!(found.len(), 1, "{label} reported {found:?}");
            assert_eq!(found[0].kind, VolatileKind::Timestamp, "{label}");
        }
    }

    #[test]
    fn a_timestamp_in_a_user_message_is_not_reported() {
        // The other half, and the reason this scans by role rather than walking the whole
        // array. A timestamp in what a person typed is ordinary — it is not in the cached
        // prefix by their choice, and there is nothing for them to do about it. Warning
        // would train an operator to ignore the log line that matters.
        let body = r#"{"messages":[
            {"role":"user","content":"what happened at 2026-08-03T12:00:00Z?"},
            {"role":"assistant","content":"checked at 2026-08-03T12:00:01Z"}
        ]}"#;

        assert!(
            scan(body.as_bytes()).is_empty(),
            "{:?}",
            scan(body.as_bytes())
        );
    }
}
