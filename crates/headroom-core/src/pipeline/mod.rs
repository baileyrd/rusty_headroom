//! The compression pipeline — routing, safety, and reformatting.
//!
//! # Where the routing decision belongs
//!
//! Choosing a compressor for a block used to live in the proxy, as a private dispatcher
//! alongside the axum handler. That put a decision every consumer needs behind a crate
//! nothing but the proxy depends on: the CLI reimplemented it, and the two could drift
//! without anything failing.
//!
//! It lives here now, so `headroom compress` and `POST /v1/messages` route identically
//! by construction rather than by two people remembering to update both.

pub mod orchestrator;
pub mod reformats;
pub mod safety;

pub use orchestrator::{Orchestrator, Routing};
pub use safety::{check, Hazard, Limits};
