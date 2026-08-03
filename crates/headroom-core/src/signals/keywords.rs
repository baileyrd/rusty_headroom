//! Keyword-based importance.

/// Words that mark a line as reporting something going wrong.
///
/// Lowercase; matching is case-insensitive and whole-word. Substring matching would
/// fire on `terror`, `mirrored`, and `information`, which would pull ordinary lines
/// into the keep set and quietly undo the compression.
pub const ERROR_KEYWORDS: [&str; 18] = [
    "error",
    "err",
    "failed",
    "failure",
    "fatal",
    "panic",
    "exception",
    "traceback",
    "assert",
    "refused",
    "denied",
    "timeout",
    "timedout",
    "unreachable",
    "corrupt",
    "invalid",
    "unauthorized",
    "forbidden",
];

/// Words that mark a line as a warning rather than an outright failure.
const WARNING_KEYWORDS: [&str; 6] = ["warn", "warning", "deprecated", "retry", "retrying", "slow"];

/// How strongly a line signals trouble, from `0` upward.
///
/// Errors outweigh warnings deliberately: when a budget forces a choice, the failure
/// should survive and the deprecation notice should not.
pub fn keyword_score(line: &str) -> u32 {
    let lowered = line.to_ascii_lowercase();
    let mut score = 0;

    for keyword in ERROR_KEYWORDS {
        if contains_word(&lowered, keyword) {
            score += 3;
        }
    }
    for keyword in WARNING_KEYWORDS {
        if contains_word(&lowered, keyword) {
            score += 1;
        }
    }

    score
}

/// Whether the line reports an outright failure.
pub fn is_error_line(line: &str) -> bool {
    let lowered = line.to_ascii_lowercase();
    ERROR_KEYWORDS
        .iter()
        .any(|keyword| contains_word(&lowered, keyword))
}

/// Whole-word containment. `haystack` must already be lowercase.
fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(index, _)| {
        let bytes = haystack.as_bytes();
        let before_ok =
            index == 0 || (!bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'_');
        let after = index + needle.len();
        let after_ok = after >= haystack.len()
            || (!bytes[after].is_ascii_alphanumeric() && bytes[after] != b'_');
        before_ok && after_ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_words_score_above_warning_words() {
        // When a budget forces a choice, the failure survives and the deprecation
        // notice does not.
        assert!(keyword_score("ERROR: connection refused") > keyword_score("warning: deprecated"));
    }

    #[test]
    fn plain_lines_score_zero() {
        assert_eq!(keyword_score("starting service on port 8080"), 0);
        assert_eq!(keyword_score(""), 0);
    }

    #[test]
    fn matching_is_whole_word() {
        // The classic false positives. Each of these would pull an ordinary line into
        // the keep set and undo the compression.
        assert_eq!(keyword_score("the terror of it"), 0);
        assert_eq!(keyword_score("mirrored display"), 0);
        assert_eq!(keyword_score("information about the run"), 0);
        assert_eq!(keyword_score("asserting dominance"), 0);
    }

    #[test]
    fn matching_is_case_insensitive() {
        for spelling in ["error", "ERROR", "Error", "eRRoR"] {
            assert!(
                is_error_line(&format!("an {spelling} happened")),
                "{spelling}"
            );
        }
    }

    #[test]
    fn punctuation_does_not_hide_a_keyword() {
        assert!(is_error_line("[ERROR] boom"));
        assert!(is_error_line("status=failed;"));
        assert!(is_error_line("(timeout)"));
    }

    #[test]
    fn several_keywords_accumulate() {
        assert!(keyword_score("FATAL error: connection refused") > keyword_score("error"));
    }

    #[test]
    fn scoring_is_deterministic() {
        let line = "ERROR: request failed after timeout, retrying";
        let first = keyword_score(line);
        for _ in 0..25 {
            assert_eq!(keyword_score(line), first);
        }
    }
}
