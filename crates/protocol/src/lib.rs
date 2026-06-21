//! Foundational protocols that make LLM interactions robust on flaky APIs.
//!
//! - [`guided`] — ① Guided Decoding: constrain output to a JSON schema so
//!   parse errors are physically impossible.
//! - [`sentinel`] — ④ Sentinel + Overlap Diff: detect mid-stream truncation
//!   via a trailing marker and stitch continuations together.
//! - [`hedge`] — ⑤ Hedge Requests: speculatively fire duplicate requests when
//!   TTFT exceeds a threshold to dodge tail latency.

pub mod guided;
pub mod hedge;
pub mod sentinel;

pub use guided::{constrain_request, validate_against_schema, ConstraintError, JsonSchema};
pub use hedge::{HedgeConfig, HedgingExecutor};
pub use sentinel::{SentinelConfig, SentinelStream, StitchError, stitch};
