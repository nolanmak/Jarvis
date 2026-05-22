//! Code-Mode tool manifest and TypeScript declaration emitter.
//!
//! This module owns the v1 tool surface that the LLM is allowed to call from
//! generated TypeScript. It produces two artifacts:
//!
//! * a `.d.ts` block injected into the system prompt so the model has type
//!   signatures and JSDoc for every tool, and
//! * a flat allowlist of dotted tool names that the Deno runner uses to
//!   gate host-bridge calls.
//!
//! No runtime / Deno wiring lives here — this is pure string emission so it
//! can be unit-tested without spawning subprocesses.

pub mod manifest;

pub use manifest::{manifest_v1, ToolDef, ToolManifest};
