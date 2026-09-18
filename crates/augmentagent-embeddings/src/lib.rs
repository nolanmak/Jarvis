//! Semantic layer (#1126): text embeddings behind a provider trait.
//!
//! Default provider is a small local ONNX model: no key, no network at
//! runtime, message text never leaves the host. Weights are fetched only by
//! the explicit `augmentagent embeddings fetch-model` command, verified
//! against a pinned SHA-256; nothing downloads implicitly.
//!
//! Nothing here calls a reasoner. Embedding text never executes it.

pub mod chunk;
pub mod embedder;
pub mod fetch;
pub mod hosted;
pub mod local;
pub mod model;
pub mod prepare;
pub mod provider;
pub mod triage_prefilter;
pub mod vectors;

pub use embedder::{Embedder, Embedding, ModelId};
pub use hosted::HostedEmbedder;
pub use local::LocalEmbedder;
pub use model::{ModelSpec, DEFAULT_MODEL};
pub use provider::{active_model_id, build_embedder, Provider, ENV_PROVIDER};

#[cfg(any(test, feature = "stub"))]
pub use embedder::StubEmbedder;
