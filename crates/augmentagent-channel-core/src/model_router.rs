//! Optional local gateway. A dispatch snapshots configuration so dashboard changes
//! affect the next call, never a provider midway through a fallback/handoff.
use crate::providers::{ModelTier, ProviderKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Deserialize, Serialize)]
pub struct Models {
    pub quality: String,
    pub fast: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct ProviderModels {
    pub claude: Models,
    pub codex: Models,
}
// Deliberately no Debug: the gateway key is a secret. Dashboard administration
// credentials are ignored during deserialization and never reach subprocesses.
#[derive(Clone, Deserialize, Serialize)]
pub struct RouterConfig {
    pub version: u32,
    pub mode: String,
    pub base_url: String,
    pub api_key: String,
    pub models: ProviderModels,
}

tokio::task_local! { pub(crate) static SNAPSHOT: Option<RouterConfig>; }

pub fn config_path() -> PathBuf {
    std::env::var_os("AUGMENTAGENT_MODEL_ROUTER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let root = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                });
            root.join("augmentagent/model-router.json")
        })
}

pub fn load() -> anyhow::Result<Option<RouterConfig>> {
    // Unit tests must never use the operator's live routing/accounts.
    #[cfg(test)]
    if std::env::var_os("AUGMENTAGENT_MODEL_ROUTER_CONFIG").is_none() {
        return Ok(None);
    }
    load_from(&config_path())
}

pub fn current() -> anyhow::Result<Option<RouterConfig>> {
    match SNAPSHOT.try_with(Clone::clone) {
        Ok(value) => Ok(value),
        Err(_) => load(),
    }
}

/// A Discord/default profile applies only to this call's cloned router
/// configuration. The persisted account settings and other calls are intact.
pub fn select_profile(config: Option<RouterConfig>, selected: Option<ProviderKind>) -> anyhow::Result<Option<RouterConfig>> {
    let native_codex_available = selected == Some(ProviderKind::Codex)
        && crate::codex::codex_auth_available();
    select_profile_with_auth(config, selected, native_codex_available)
}

fn select_profile_with_auth(mut config: Option<RouterConfig>, selected: Option<ProviderKind>, native_codex_available: bool) -> anyhow::Result<Option<RouterConfig>> {
    if let Some(profile) = selected {
        anyhow::ensure!(matches!(profile, ProviderKind::Codex | ProviderKind::Qwen | ProviderKind::Glm), "unsupported model profile");
        if let Some(router) = config.as_mut() {
            // Keep an existing Codex login on its subscription transport.
            // A configured 9Router is needed for Runpod but does not imply
            // consent to send Codex calls to a paid gateway account.
            router.mode = if profile == ProviderKind::Codex && native_codex_available {
                "direct".into()
            } else {
                profile.name().into()
            };
        } else {
            anyhow::ensure!(profile == ProviderKind::Codex,
                "Selected Runpod model requires a configured 9Router endpoint");
        }
    }
    Ok(config)
}

pub fn load_from(path: &Path) -> anyhow::Result<Option<RouterConfig>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => parse(&raw).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => anyhow::bail!("Cannot read model router configuration"),
    }
}

pub fn parse(raw: &str) -> anyhow::Result<RouterConfig> {
    let config: RouterConfig = serde_json::from_str(raw)
        .map_err(|_| anyhow::anyhow!("Invalid model router configuration"))?;
    anyhow::ensure!(
        config.version == 1
            && ["direct", "auto", "claude", "codex", "qwen", "glm"].contains(&config.mode.as_str()),
        "Unsupported model router configuration"
    );
    let url = reqwest::Url::parse(&config.base_url)
        .map_err(|_| anyhow::anyhow!("Invalid router endpoint"))?;
    anyhow::ensure!(
        url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/v1",
        "Router endpoint must be a loopback HTTP /v1 endpoint"
    );
    anyhow::ensure!(
        !config.api_key.trim().is_empty() && !config.api_key.chars().any(char::is_control),
        "Missing or invalid router API key"
    );
    for (models, prefix) in [
        (&config.models.claude, "cc/"),
        (&config.models.codex, "cx/"),
    ] {
        for model in [&models.quality, &models.fast] {
            anyhow::ensure!(
                model.starts_with(prefix)
                    && model.len() > prefix.len()
                    && model.len() <= 160
                    && model
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_/.:".contains(c)),
                "Router models must stay within their provider (cc/ or cx/)"
            );
        }
    }
    Ok(config)
}

impl RouterConfig {
    pub fn enabled(&self) -> bool {
        self.mode != "direct"
    }
    pub fn allows(&self, provider: ProviderKind) -> bool {
        !self.enabled()
            || match self.mode.as_str() {
                "auto" => matches!(provider, ProviderKind::Claude | ProviderKind::Codex),
                "claude" => provider == ProviderKind::Claude,
                "codex" => provider == ProviderKind::Codex,
                "qwen" => provider == ProviderKind::Qwen,
                "glm" => provider == ProviderKind::Glm,
                _ => false,
            }
    }
    pub fn model(&self, provider: ProviderKind, tier: ModelTier) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        let models = match provider {
            ProviderKind::Claude => &self.models.claude,
            ProviderKind::Codex => &self.models.codex,
            _ => return None,
        };
        Some(
            match tier {
                ModelTier::Quality => &models.quality,
                ModelTier::Fast => &models.fast,
            }
            .clone(),
        )
    }
    pub fn codex_overrides(&self) -> Vec<String> {
        vec![
            "model_provider=\"augmentagent_router\"".into(),
            "model_providers.augmentagent_router.name=\"9Router\"".into(),
            format!("model_providers.augmentagent_router.base_url={}", serde_json::to_string(&self.base_url).expect("string")),
            "model_providers.augmentagent_router.wire_api=\"responses\"".into(),
            "model_providers.augmentagent_router.env_key=\"AUGMENTAGENT_ROUTER_API_KEY\"".into(),
            "model_providers.augmentagent_router.requires_openai_auth=false".into(),
            "model_providers.augmentagent_router.http_headers={ \"X-9Router-Token-Saver\" = \"off\" }".into(),
        ]
    }
    pub fn configure_codex(&self, cmd: &mut tokio::process::Command) {
        cmd.env("AUGMENTAGENT_ROUTER_API_KEY", &self.api_key)
            .env_remove("CODEX_API_KEY");
    }
    pub fn configure_claude(&self, cmd: &mut tokio::process::Command) {
        cmd.env("ANTHROPIC_BASE_URL", self.base_url.trim_end_matches("/v1"))
            .env("ANTHROPIC_AUTH_TOKEN", &self.api_key)
            .env("ANTHROPIC_CUSTOM_HEADERS", "X-9Router-Token-Saver: off")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::providers::{ModelTier, ProviderKind};
    pub(crate) fn fixture() -> serde_json::Value {
        serde_json::json!({"version":1,"mode":"auto","base_url":"http://127.0.0.1:20128/v1",
          "api_key":"router-secret","models":{"claude":{"quality":"cc/claude-opus-4-6","fast":"cc/claude-haiku-4-5"},
          "codex":{"quality":"cx/gpt-5.4","fast":"cx/gpt-5.4-mini"}}})
    }
    #[test]
    fn missing_config_preserves_direct_mode_but_invalid_config_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("router.json");
        assert!(load_from(&path).unwrap().is_none());
        std::fs::write(&path, "{broken").unwrap();
        assert!(load_from(&path).is_err());
    }
    #[test]
    fn config_reloads_switches_and_models_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("router.json");
        let mut value = fixture();
        std::fs::write(&path, value.to_string()).unwrap();
        let config = load_from(&path).unwrap().unwrap();
        assert!(config.allows(ProviderKind::Claude));
        assert!(config.allows(ProviderKind::Codex));
        assert!(!config.allows(ProviderKind::Gemini));
        assert_eq!(
            config.model(ProviderKind::Codex, ModelTier::Fast).unwrap(),
            "cx/gpt-5.4-mini"
        );
        value["mode"] = "codex".into();
        std::fs::write(&path, value.to_string()).unwrap();
        let config = load_from(&path).unwrap().unwrap();
        assert!(!config.allows(ProviderKind::Claude));
        assert!(config.allows(ProviderKind::Codex));
    }
    #[tokio::test]
    async fn routing_snapshot_is_stable_across_yields() {
        let config = parse(&fixture().to_string()).unwrap();
        SNAPSHOT
            .scope(Some(config), async {
                tokio::task::yield_now().await;
                assert_eq!(current().unwrap().unwrap().mode, "auto");
            })
            .await;
        assert!(current().unwrap().is_none());
    }
    #[test]
    fn rejects_external_endpoints_and_cross_provider_models() {
        for url in [
            "https://evil.example/v1",
            "http://127.0.0.1:20128/v1?key=x",
            "http://user:pass@127.0.0.1:20128/v1",
        ] {
            let mut value = fixture();
            value["base_url"] = url.into();
            assert!(parse(&value.to_string()).is_err());
        }
        let mut value = fixture();
        value["models"]["claude"]["quality"] = "cx/gpt-5.4".into();
        assert!(parse(&value.to_string()).is_err());
    }
    #[test]
    fn codex_uses_responses_and_secret_env_not_argv() {
        let config = parse(&fixture().to_string()).unwrap();
        let args = config.codex_overrides();
        assert!(args
            .contains(&"model_providers.augmentagent_router.wire_api=\"responses\"".to_string()));
        assert!(args.iter().any(|s| s.contains("env_key")));
        assert!(!args.join(" ").contains("router-secret"));
        let mut cmd = tokio::process::Command::new("codex");
        cmd.env_clear();
        config.configure_codex(&mut cmd);
        let env: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(env
            .iter()
            .any(|(k, v)| *k == "AUGMENTAGENT_ROUTER_API_KEY" && v.unwrap() == "router-secret"));
    }
    #[test]
    fn claude_gateway_auth_survives_restricted_environment() {
        let config = parse(&fixture().to_string()).unwrap();
        let mut cmd = tokio::process::Command::new("claude");
        cmd.env_clear();
        config.configure_claude(&mut cmd);
        let env: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(env
            .iter()
            .any(|(k, v)| *k == "ANTHROPIC_AUTH_TOKEN" && v.unwrap() == "router-secret"));
        assert!(env
            .iter()
            .any(|(k, v)| *k == "ANTHROPIC_BASE_URL" && v.unwrap() == "http://127.0.0.1:20128"));
        assert!(!env
            .iter()
            .any(|(k, v)| *k == "ANTHROPIC_API_KEY" && v.is_some()));
    }

    #[test]
    fn selected_profile_is_pinned_without_changing_router_accounts() {
        let original = parse(&fixture().to_string()).unwrap();
        let selected = select_profile(Some(original.clone()), Some(ProviderKind::Qwen)).unwrap().unwrap();
        assert!(selected.allows(ProviderKind::Qwen));
        assert!(!selected.allows(ProviderKind::Codex));
        assert!(!selected.allows(ProviderKind::Glm));
        assert_eq!(original.mode, "auto");
        assert_eq!(original.models.codex.quality, selected.models.codex.quality);
    }

    #[test]
    fn explicit_codex_uses_existing_account_when_available() {
        let original = parse(&fixture().to_string()).unwrap();
        let native = select_profile_with_auth(Some(original.clone()), Some(ProviderKind::Codex), true)
            .unwrap().unwrap();
        assert_eq!(native.mode, "direct");
        let gateway = select_profile_with_auth(Some(original.clone()), Some(ProviderKind::Codex), false)
            .unwrap().unwrap();
        assert_eq!(gateway.mode, "codex");
        assert_eq!(original.mode, "auto");
    }
}
