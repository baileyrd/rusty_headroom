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
//! Analysis, statistics, anchor selection, and the compaction formatter arrive in
//! later issues; this is the substrate they share.

mod config;
mod ir;

pub use config::CrushConfig;
pub use ir::{Document, Shape};
