//! SmartCrusher configuration.

/// Tuning for JSON compression.
///
/// The defaults aim at the shape that dominates agent tool output: an array of many
/// near-identical records, where the model needs to know *what the data looks like*
/// and *what stands out*, not to read all 500 rows.
///
/// # Example
///
/// ```
/// use headroom_core::smart_crusher::CrushConfig;
///
/// let config = CrushConfig::default();
/// assert!(config.min_records_to_summarize >= 2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrushConfig {
    /// How many representative records to keep verbatim when summarizing an array.
    ///
    /// Kept from the head, because a truncated list's first entries are what the
    /// model uses to infer the shape of the rest.
    pub sample_records: usize,

    /// Fewest records an array must have before summarizing is considered.
    ///
    /// Below this, describing the structure costs more than printing the rows. Two
    /// is the floor at which "these are all the same shape" is even a claim worth
    /// making.
    pub min_records_to_summarize: usize,

    /// Longest string value kept verbatim, in bytes. Longer values are candidates
    /// for elision behind a CCR marker.
    pub max_string_len: usize,

    /// Deepest nesting level to analyze.
    ///
    /// A bound rather than an optimization: JSON can nest arbitrarily, and recursion
    /// over attacker-influenced input without a depth limit is a stack-overflow
    /// waiting to happen. Tool output is not trusted input.
    pub max_depth: usize,

    /// Most distinct values a field may have and still count as low-cardinality.
    ///
    /// Low-cardinality fields are the ones worth enumerating — "`status` is one of
    /// `ok`, `retry`, `failed`" says more in fewer tokens than repeating the value on
    /// every record. Set this too high and the "enumeration" is longer than the data
    /// it replaces.
    pub max_low_cardinality: usize,

    /// Fewest fields an object needs before it counts as wide.
    ///
    /// Wide objects tend to be envelope boilerplate wrapped around the part that
    /// matters, which is a different compression opportunity from a record set.
    pub wide_object_fields: usize,

    /// Total string bytes above which a document is treated as scalar-heavy.
    ///
    /// A document dominated by a few long strings has no structure worth exploiting;
    /// it is a CCR offload candidate rather than something to summarize.
    pub scalar_heavy_bytes: usize,

    /// Whether fields that look like errors are always preserved verbatim.
    ///
    /// Defaults to `true`. A summarized success payload costs the model a little
    /// context; a summarized stack trace can cost it the entire debugging session.
    /// The asymmetry justifies the special case.
    pub preserve_error_fields: bool,
}

impl Default for CrushConfig {
    fn default() -> Self {
        Self {
            sample_records: 3,
            min_records_to_summarize: 5,
            max_string_len: 512,
            max_depth: 64,
            max_low_cardinality: 8,
            wide_object_fields: 12,
            scalar_heavy_bytes: 1024,
            preserve_error_fields: true,
        }
    }
}

impl CrushConfig {
    /// Configuration with the documented defaults.
    pub fn new() -> Self {
        Self::default()
    }
}
