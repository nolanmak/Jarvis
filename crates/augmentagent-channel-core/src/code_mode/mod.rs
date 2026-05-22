//! Code Mode — let Claude write a TypeScript program that orchestrates typed
//! tool calls in a Deno sandbox, instead of emitting JSON intents.
//!
//! This module owns the **tool manifest** that defines the v1 tool surface
//! exposed to the model:
//!
//! - [`manifest::ToolDef`] / [`manifest::ToolManifest`] — pure data: a flat
//!   list of dotted tool names (`wiki.draftHint`, `db.recentEmailsFrom`, …)
//!   with their TypeScript signature and JSDoc.
//! - [`manifest::manifest_v1`] — the hard-coded v1 surface (wiki / db /
//!   calendar / terminal `draft`).
//! - [`ToolManifest::to_dts`] — emits the `declare const tools: { … }` block
//!   injected verbatim into the Code Mode system prompt.
//! - [`ToolManifest::to_runner_manifest`] — emits the flat allowlist passed
//!   over NDJSON to the Deno runner sidecar.
//!
//! Sibling modules (dispatcher, runner, reasoner glue) live alongside and
//! consume this manifest. The manifest itself has **no runtime dependencies**
//! — it's pure string construction so it can be unit-tested in isolation.

pub mod manifest;

pub use manifest::{manifest_v1, ToolDef, ToolManifest};
