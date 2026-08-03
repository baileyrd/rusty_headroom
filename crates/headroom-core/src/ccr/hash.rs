//! Content addressing and the CCR marker format.

use std::fmt;

use crate::error::{Error, Result};

/// A content-addressed identifier for a stored original.
///
/// # Why content-addressed
///
/// The hash is derived from the content and nothing else — no counter, no
/// timestamp, no session identifier. That is what makes CCR replay-safe: the same
/// request produces the same markers on every run, so the bytes sent upstream are
/// stable and the provider's prompt cache keeps hitting.
///
/// A sequential or time-based identifier would make each replay of an identical
/// request produce different bytes, which is precisely the cache-busting behavior
/// invariant I4 exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash([u8; Self::LEN]);

impl ContentHash {
    /// Length of the raw digest in bytes.
    ///
    /// Truncated from BLAKE3's 32-byte output. 16 bytes is 128 bits: collision
    /// resistance far beyond what a per-session content store needs, and it halves
    /// the marker's token cost, which matters because the marker is paid for on
    /// every compressed block.
    pub const LEN: usize = 16;

    /// Computes the hash of `content`.
    ///
    /// Deterministic by construction — BLAKE3 over the raw bytes, with no salt and
    /// no keying.
    pub fn of(content: &[u8]) -> Self {
        let full = blake3::hash(content);
        let mut truncated = [0u8; Self::LEN];
        truncated.copy_from_slice(&full.as_bytes()[..Self::LEN]);
        Self(truncated)
    }

    /// The raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }

    /// Renders the hash as lowercase hex.
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(Self::LEN * 2);
        for byte in self.0 {
            // Two lowercase hex digits per byte, always — no `{:x}` width surprises
            // on bytes below 0x10.
            out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
            out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
        }
        out
    }

    /// Parses a lowercase-hex digest.
    pub fn from_hex(hex: &str) -> Result<Self> {
        if hex.len() != Self::LEN * 2 {
            return Err(Error::Malformed {
                content_type: "ccr-hash",
                detail: format!("expected {} hex chars, got {}", Self::LEN * 2, hex.len()),
            });
        }

        let mut bytes = [0u8; Self::LEN];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let hi = hex_digit(hex.as_bytes()[i * 2])?;
            let lo = hex_digit(hex.as_bytes()[i * 2 + 1])?;
            *byte = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        // Lowercase only. Accepting uppercase would mean two spellings of the same
        // hash, and a marker that round-trips to different bytes than it arrived as
        // is an I1 violation waiting to happen.
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        other => Err(Error::Malformed {
            content_type: "ccr-hash",
            detail: format!("invalid hex digit {:?}", other as char),
        }),
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// The prefix of a CCR retrieval marker.
const MARKER_PREFIX: &str = "<<ccr:";
/// The suffix of a CCR retrieval marker.
const MARKER_SUFFIX: &str = ">>";

/// Renders the retrieval marker for `hash`.
///
/// The marker tells the model that content was elided here and gives it the key to
/// retrieve the original through the `ccr_retrieve` tool.
///
/// # Example
///
/// ```
/// use headroom_core::ccr::{marker, ContentHash};
///
/// let hash = ContentHash::of(b"the original tool output");
/// let m = marker(hash);
/// assert!(m.starts_with("<<ccr:"));
/// assert!(m.ends_with(">>"));
/// ```
pub fn marker(hash: ContentHash) -> String {
    format!("{MARKER_PREFIX}{}{MARKER_SUFFIX}", hash.to_hex())
}

/// Parses a single marker, returning the hash it carries.
///
/// Returns [`Error::Malformed`] if `text` is not exactly one well-formed marker.
pub fn parse_marker(text: &str) -> Result<ContentHash> {
    let inner = text
        .strip_prefix(MARKER_PREFIX)
        .and_then(|rest| rest.strip_suffix(MARKER_SUFFIX))
        .ok_or_else(|| Error::Malformed {
            content_type: "ccr-marker",
            detail: format!("not a well-formed marker: {text:?}"),
        })?;
    ContentHash::from_hex(inner)
}

/// Finds every marker in `text`, in order of appearance.
///
/// Malformed marker-like fragments are skipped rather than raised: user content can
/// legitimately contain something resembling a marker, and refusing the whole block
/// over it would be worse than ignoring it.
pub fn find_markers(text: &str) -> Vec<ContentHash> {
    let mut found = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find(MARKER_PREFIX) {
        let after_prefix = &rest[start + MARKER_PREFIX.len()..];
        let Some(end) = after_prefix.find(MARKER_SUFFIX) else {
            break;
        };
        if let Ok(hash) = ContentHash::from_hex(&after_prefix[..end]) {
            found.push(hash);
        }
        rest = &after_prefix[end + MARKER_SUFFIX.len()..];
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_hashes_identically() {
        // The property the whole design rests on. If this ever fails, replaying an
        // identical request produces different bytes and the prompt cache misses.
        let a = ContentHash::of(b"some tool output");
        let b = ContentHash::of(b"some tool output");
        assert_eq!(a, b);
    }

    #[test]
    fn different_content_hashes_differently() {
        assert_ne!(ContentHash::of(b"alpha"), ContentHash::of(b"beta"));
        // Single-byte difference must still separate them.
        assert_ne!(ContentHash::of(b"output"), ContentHash::of(b"outpuT"));
    }

    #[test]
    fn empty_content_has_a_stable_hash() {
        assert_eq!(ContentHash::of(b""), ContentHash::of(b""));
    }

    #[test]
    fn hex_round_trips() {
        let hash = ContentHash::of(b"round trip me");
        let hex = hash.to_hex();
        assert_eq!(hex.len(), ContentHash::LEN * 2);
        assert_eq!(ContentHash::from_hex(&hex).unwrap(), hash);
    }

    #[test]
    fn hex_is_always_zero_padded() {
        // A byte below 0x10 must render as two digits. Emitting one would shift
        // every subsequent digit and silently corrupt the hash.
        let hash = ContentHash([0x00, 0x0f, 0x01, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(hash.to_hex().starts_with("000f01ff"));
        assert_eq!(hash.to_hex().len(), ContentHash::LEN * 2);
    }

    #[test]
    fn from_hex_rejects_wrong_length() {
        assert!(ContentHash::from_hex("").is_err());
        assert!(ContentHash::from_hex("abcd").is_err());
        assert!(ContentHash::from_hex(&"a".repeat(ContentHash::LEN * 2 + 1)).is_err());
    }

    #[test]
    fn from_hex_rejects_non_hex_and_uppercase() {
        assert!(ContentHash::from_hex(&"g".repeat(ContentHash::LEN * 2)).is_err());
        // Uppercase is rejected so a hash has exactly one spelling.
        assert!(ContentHash::from_hex(&"A".repeat(ContentHash::LEN * 2)).is_err());
    }

    #[test]
    fn marker_round_trips() {
        let hash = ContentHash::of(b"content behind the marker");
        assert_eq!(parse_marker(&marker(hash)).unwrap(), hash);
    }

    #[test]
    fn parse_marker_rejects_malformed_and_truncated_input() {
        let hash = ContentHash::of(b"x");
        let good = marker(hash);

        assert!(parse_marker("").is_err());
        assert!(parse_marker("plain text").is_err());
        assert!(
            parse_marker(&good[..good.len() - 1]).is_err(),
            "truncated tail"
        );
        assert!(parse_marker(&good[1..]).is_err(), "truncated head");
        assert!(parse_marker("<<ccr:>>").is_err(), "empty hash");
        assert!(parse_marker("<<ccr:nothex0000000000000000000000>>").is_err());
    }

    #[test]
    fn find_markers_returns_them_in_order() {
        let a = ContentHash::of(b"first");
        let b = ContentHash::of(b"second");
        let text = format!("head {} middle {} tail", marker(a), marker(b));
        assert_eq!(find_markers(&text), vec![a, b]);
    }

    #[test]
    fn find_markers_skips_lookalikes_without_failing() {
        // User content can contain something marker-shaped. Ignoring it is correct;
        // refusing the whole block over it would not be.
        let real = ContentHash::of(b"real");
        let text = format!("<<ccr:zzzz>> and <<ccr: >> and {}", marker(real));
        assert_eq!(find_markers(&text), vec![real]);
    }

    #[test]
    fn find_markers_on_content_without_any_returns_empty() {
        assert!(find_markers("nothing to see here").is_empty());
        assert!(find_markers("<<ccr: unterminated").is_empty());
    }
}
