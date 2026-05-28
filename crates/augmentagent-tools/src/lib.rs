//! Operator-side eval/calibration utilities for AugmentAgent.
//!
//! Currently houses the tone-mirroring eval harness (#73 §7). Each tool is
//! a self-contained binary in `src/bin/`; this lib crate exposes shared
//! scoring primitives so future eval harnesses can reuse them.

pub mod scoring;
pub mod text;
