//! SmartCrusher — structural compression for JSON.
//!
//! The highest-value compressor in the system. Agent tool output is overwhelmingly
//! JSON, and JSON is overwhelmingly repetitive: a search returning 200 hits, a list
//! of 500 files, a batch of near-identical records. The model needs to know what the
//! data looks like and what stands out in it — not to read every row.
//!
//! This module holds the foundations the rest of SmartCrusher builds on: the
//! [`CrushConfig`] tuning surface, and the [`Document`] / [`Shape`] IR that
//! analysis, planning, and formatting all operate over.
//!
//! Analysis and classification decide what is worth doing about a document. Anchor
//! selection, planning, and the compaction formatter arrive in later issues.

mod analyzer;
mod config;
mod field_detect;
mod formatter;
mod ir;
mod outliers;
mod planning;

pub use analyzer::{analyze_record_set, classify, FieldKind, FieldStat, Pattern, RecordSetStats};
pub use config::CrushConfig;
pub use field_detect::{classify_field, FieldRole};
pub use formatter::{format_plan, SmartCrusher};
pub use ir::{Document, Shape};
pub use outliers::{rank_outliers, Outlier, OutlierReason};
pub use planning::{plan, plan_with_query, CrushPlan, FieldPlan};
