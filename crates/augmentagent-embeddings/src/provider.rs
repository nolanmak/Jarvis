//! Provider selection: `AUGMENTAGENT_EMBEDDINGS_PROVIDER=local|hosted`,
//! default `local`. Selecting `hosted` without a key fails loudly; nothing
//! ever falls back across providers, because that would mix vector spaces.

use std::sync::Arc;

use crate::embedder::{Embedder, ModelId};
use crate::hosted::{self, HostedConfig, HostedEmbedder};
use crate::local::LocalEmbedder;
use crate::model::{thread_count, DEFAULT_MODEL};

pub const ENV_PROVIDER: &str = "AUGMENTAGENT_EMBEDDINGS_PROVIDER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Local,
    Hosted,
}

impl Provider {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::parse(std::env::var(ENV_PROVIDER).ok().as_deref())
    }

    pub fn parse(raw: Option<&str>) -> anyhow::Result<Self> {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("local") => Ok(Self::Local),
            Some("hosted") | Some("openai") => Ok(Self::Hosted),
            Some(other) => {
                anyhow::bail!("{ENV_PROVIDER} must be `local` or `hosted`, got `{other}`")
            }
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Hosted => "hosted",
        }
    }
}

/// The vector space the active configuration writes to, without loading
/// anything (used by `check`).
pub fn active_model_id() -> anyhow::Result<ModelId> {
    Ok(match Provider::from_env()? {
        Provider::Local => ModelId {
            provider: "local".into(),
            model: DEFAULT_MODEL.name.into(),
            dim: DEFAULT_MODEL.dim,
        },
        Provider::Hosted => {
            let c = HostedConfig::default();
            ModelId {
                provider: "hosted".into(),
                model: c.model,
                dim: c.dim,
            }
        }
    })
}

/// Build the configured embedder. Local: needs weights (typed error names
/// `fetch-model`). Hosted: needs a key (typed error; no fallback).
pub fn build_embedder(dry_run: bool) -> anyhow::Result<Arc<dyn Embedder>> {
    Ok(match Provider::from_env()? {
        Provider::Local => Arc::new(LocalEmbedder::load(
            &DEFAULT_MODEL,
            &DEFAULT_MODEL.dir(),
            thread_count(),
        )?),
        Provider::Hosted => {
            let cfg = HostedConfig {
                dry_run,
                ..HostedConfig::default()
            };
            Arc::new(HostedEmbedder::new(cfg, hosted::load_key())?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_local_and_rejects_unknown_values() {
        assert_eq!(Provider::parse(None).unwrap(), Provider::Local);
        assert_eq!(Provider::parse(Some("")).unwrap(), Provider::Local);
        assert_eq!(Provider::parse(Some("LOCAL")).unwrap(), Provider::Local);
        assert_eq!(Provider::parse(Some("hosted")).unwrap(), Provider::Hosted);
        assert!(Provider::parse(Some("cloud")).is_err());
    }
}
