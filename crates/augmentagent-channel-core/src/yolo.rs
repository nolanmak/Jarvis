//! Yolo profile: configuration and readiness (#2213, spec #2243 section 5.2).
//!
//! Yolo is a hosted, unfiltered model reached through the Codex CLI. It is
//! dispatched only for turns in a yolo thread; this module answers "is it
//! configured well enough to try", with no network call.

pub const ENABLED_ENV: &str = "AUGMENTAGENT_MODEL_YOLO_ENABLED";
pub const BASE_URL_ENV: &str = "AUGMENTAGENT_YOLO_BASE_URL";
pub const ALLOWED_HOSTS_ENV: &str = "AUGMENTAGENT_YOLO_ALLOWED_HOSTS";
pub const MODEL_ENV: &str = "AUGMENTAGENT_YOLO_MODEL";
pub const ZDR_ENV: &str = "AUGMENTAGENT_YOLO_ZDR";
pub const DEFAULT_BASE_URL: &str = "https://api.experientiallabs.ai/v1";
pub const DEFAULT_ALLOWED_HOSTS: &str = "api.experientiallabs.ai:443";
pub const DEFAULT_MODEL: &str = "glm-5.3-uncensored";
/// Tried in order; the first present value wins.
pub const KEY_NAMES: [&str; 2] = ["EXPLABS_API_KEY", "EXPERIENTIAL_API_KEY"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    Disabled,
    MissingKey,
    InvalidEndpoint,
    InvalidModel,
    CodexMissing,
    WorkspaceUnavailable,
}

impl NotReady {
    /// One fixed sentence per cause, shown to the owner and by `doctor`.
    pub fn sentence(self) -> &'static str {
        match self {
            NotReady::Disabled => "yolo is disabled (AUGMENTAGENT_MODEL_YOLO_ENABLED is not 1).",
            NotReady::MissingKey => "no Experiential API key is configured (EXPLABS_API_KEY).",
            NotReady::InvalidEndpoint => {
                "the yolo endpoint is not an allowed https /v1 URL (AUGMENTAGENT_YOLO_BASE_URL, AUGMENTAGENT_YOLO_ALLOWED_HOSTS)."
            }
            NotReady::InvalidModel => "the yolo model id is not valid (AUGMENTAGENT_YOLO_MODEL).",
            NotReady::CodexMissing => "the Codex CLI is not installed.",
            NotReady::WorkspaceUnavailable => "no state directory is available for yolo workspaces.",
        }
    }
}

/// A credential or setting is present when it is non-empty after trimming
/// and holds no control characters.
fn present(raw: Option<String>) -> Option<String> {
    let value = raw?.trim().to_string();
    (!value.is_empty() && !value.chars().any(char::is_control)).then_some(value)
}

fn valid_model(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'-'))
}

/// Everything readiness reads, so tests never touch the process environment.
pub struct Inputs<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub key: &'a dyn Fn(&str) -> Option<String>,
    pub codex_installed: bool,
    pub state_dir: bool,
}

impl Inputs<'_> {
    pub fn enabled(&self) -> bool {
        (self.env)(ENABLED_ENV).as_deref() == Some("1")
    }

    /// The model id sent to the vendor, shown in the reply tag and audited.
    pub fn model(&self) -> Result<String, NotReady> {
        match (self.env)(MODEL_ENV) {
            None => Ok(DEFAULT_MODEL.to_string()),
            Some(raw) => {
                let id = raw.trim().to_string();
                if id.is_empty() {
                    Ok(DEFAULT_MODEL.to_string())
                } else if valid_model(&id) {
                    Ok(id)
                } else {
                    Err(NotReady::InvalidModel)
                }
            }
        }
    }

    pub fn endpoint(&self) -> Result<reqwest::Url, NotReady> {
        let url = present((self.env)(BASE_URL_ENV)).unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let allowed = present((self.env)(ALLOWED_HOSTS_ENV))
            .unwrap_or_else(|| DEFAULT_ALLOWED_HOSTS.to_string());
        crate::model_router::validate_remote_v1(&url, &allowed)
            .map_err(|_| NotReady::InvalidEndpoint)
    }

    pub fn api_key(&self) -> Option<String> {
        KEY_NAMES.iter().find_map(|name| present((self.key)(name)))
    }

    /// Zero data retention is requested unless the setting is exactly `0`.
    pub fn zdr_requested(&self) -> bool {
        (self.env)(ZDR_ENV).as_deref() != Some("0")
    }

    pub fn readiness(&self) -> Result<(), NotReady> {
        if !self.enabled() {
            return Err(NotReady::Disabled);
        }
        if self.api_key().is_none() {
            return Err(NotReady::MissingKey);
        }
        self.endpoint()?;
        self.model()?;
        if !self.codex_installed {
            return Err(NotReady::CodexMissing);
        }
        if !self.state_dir {
            return Err(NotReady::WorkspaceUnavailable);
        }
        Ok(())
    }
}

/// What one Codex launch needs to reach the vendor. Built only when ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub base_url: String,
    pub api_key: String,
    pub zdr: bool,
}

/// Name of the environment variable the Codex child reads the key from.
pub const CHILD_KEY_ENV: &str = "AUGMENTAGENT_YOLO_API_KEY";

impl Launch {
    /// Codex `-c` values for a dedicated provider block. The model gateway's
    /// block is never combined with this one. The key is not here: it goes
    /// in the child's environment under [`CHILD_KEY_ENV`].
    pub fn codex_overrides(&self) -> Vec<String> {
        let mut values = vec![
            "model_provider=\"augmentagent_yolo\"".to_string(),
            "model_providers.augmentagent_yolo.name=\"Experiential\"".to_string(),
            format!(
                "model_providers.augmentagent_yolo.base_url={}",
                serde_json::to_string(&self.base_url).expect("string")
            ),
            "model_providers.augmentagent_yolo.wire_api=\"responses\"".to_string(),
            format!("model_providers.augmentagent_yolo.env_key=\"{CHILD_KEY_ENV}\""),
            "model_providers.augmentagent_yolo.requires_openai_auth=false".to_string(),
        ];
        if self.zdr {
            values.push(
                "model_providers.augmentagent_yolo.http_headers={ \"X-Gateway-ZDR\" = \"true\" }"
                    .to_string(),
            );
        }
        values
    }
}

impl Inputs<'_> {
    pub fn launch(&self) -> Result<Launch, NotReady> {
        self.readiness()?;
        Ok(Launch {
            base_url: self.endpoint()?.as_str().trim_end_matches('/').to_string(),
            api_key: self.api_key().ok_or(NotReady::MissingKey)?,
            zdr: self.zdr_requested(),
        })
    }
}

#[cfg(test)]
tokio::task_local! {
    /// Test seam: a launch that does not depend on the process environment.
    pub(crate) static LAUNCH_FOR_TESTS: Launch;
}

/// Readiness is re-checked here, immediately before every Yolo dispatch.
pub fn launch() -> Result<Launch, NotReady> {
    #[cfg(test)]
    if let Ok(launch) = LAUNCH_FOR_TESTS.try_with(Clone::clone) {
        return Ok(launch);
    }
    system(|inputs| inputs.launch())
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn system<T>(read: impl FnOnce(&Inputs<'_>) -> T) -> T {
    read(&Inputs {
        env: &process_env,
        key: &crate::secret_loader::load_provider_key,
        codex_installed: crate::providers::bin_resolves(&crate::codex::codex_bin()),
        state_dir: crate::state_dir::state_dir().is_some(),
    })
}

pub fn enabled() -> bool {
    process_env(ENABLED_ENV).as_deref() == Some("1")
}

/// The configured model id, or the default when the setting is invalid.
/// Readiness refuses an invalid id before any dispatch.
pub fn model_id() -> String {
    system(|inputs| inputs.model()).unwrap_or_else(|_| DEFAULT_MODEL.to_string())
}

pub fn readiness() -> Result<(), NotReady> {
    system(|inputs| inputs.readiness())
}

/// Whether launches ask the vendor for zero data retention.
pub fn zdr_requested() -> bool {
    system(|inputs| inputs.zdr_requested())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(
        env: &'a dyn Fn(&str) -> Option<String>,
        key: &'a dyn Fn(&str) -> Option<String>,
    ) -> Inputs<'a> {
        Inputs {
            env,
            key,
            codex_installed: true,
            state_dir: true,
        }
    }

    fn enabled_env(name: &str) -> Option<String> {
        (name == ENABLED_ENV).then(|| "1".to_string())
    }

    fn primary_key(name: &str) -> Option<String> {
        (name == "EXPLABS_API_KEY").then(|| "xpl_fixture".to_string())
    }

    #[test]
    fn ready_with_the_flag_a_key_and_defaults() {
        let ready = inputs(&enabled_env, &primary_key);
        assert_eq!(ready.readiness(), Ok(()));
        assert_eq!(ready.model().unwrap(), DEFAULT_MODEL);
        assert_eq!(ready.endpoint().unwrap().as_str(), DEFAULT_BASE_URL);
        assert!(ready.zdr_requested());
    }

    #[test]
    fn the_flag_must_be_exactly_one() {
        for value in [None, Some("0"), Some("true"), Some("1 "), Some("")] {
            let env = move |name: &str| {
                (name == ENABLED_ENV)
                    .then(|| value.map(str::to_string))
                    .flatten()
            };
            assert_eq!(
                inputs(&env, &primary_key).readiness(),
                Err(NotReady::Disabled),
                "{value:?}"
            );
        }
    }

    #[test]
    fn empty_and_control_character_keys_are_missing_and_the_second_name_is_tried() {
        for bad in ["", "   ", "xpl_\u{7}bad", "line\nbreak"] {
            let key = move |name: &str| (name == "EXPLABS_API_KEY").then(|| bad.to_string());
            assert_eq!(
                inputs(&enabled_env, &key).readiness(),
                Err(NotReady::MissingKey),
                "{bad:?}"
            );
        }
        assert_eq!(
            inputs(&enabled_env, &|_| None).readiness(),
            Err(NotReady::MissingKey)
        );
        // An unusable first value does not hide a usable second name.
        let key = |name: &str| match name {
            "EXPLABS_API_KEY" => Some(" ".to_string()),
            "EXPERIENTIAL_API_KEY" => Some(" xpl_second \n".to_string()),
            _ => None,
        };
        let ready = inputs(&enabled_env, &key);
        assert_eq!(ready.api_key().as_deref(), Some("xpl_second"));
        assert_eq!(ready.readiness(), Ok(()));
    }

    #[test]
    fn the_endpoint_must_be_an_allowed_https_v1_url() {
        let with = |url: &'static str, hosts: Option<&'static str>| {
            move |name: &str| match name {
                ENABLED_ENV => Some("1".to_string()),
                BASE_URL_ENV => Some(url.to_string()),
                ALLOWED_HOSTS_ENV => hosts.map(str::to_string),
                _ => None,
            }
        };
        for bad in [
            "http://127.0.0.1:1/v1",
            "http://api.experientiallabs.ai/v1",
            "https://evil.example/v1",
            "https://api.experientiallabs.ai/v2",
            "https://api.experientiallabs.ai/v1?x=1",
            "https://api.experientiallabs.ai:8443/v1",
            "not a url",
        ] {
            let env = with(bad, None);
            assert_eq!(
                inputs(&env, &primary_key).readiness(),
                Err(NotReady::InvalidEndpoint),
                "{bad}"
            );
        }
        // The default port is implied and hosts compare case-insensitively.
        let env = with("https://API.Experientiallabs.ai/v1", None);
        assert_eq!(inputs(&env, &primary_key).readiness(), Ok(()));
        let env = with(
            "https://gw.example:8443/v1",
            Some("other.example:443, GW.example:8443"),
        );
        assert_eq!(inputs(&env, &primary_key).readiness(), Ok(()));
        // Userinfo is refused even on an allowed host.
        let env = with("https://user@gw.example:8443/v1", Some("gw.example:8443"));
        assert_eq!(
            inputs(&env, &primary_key).readiness(),
            Err(NotReady::InvalidEndpoint)
        );
    }

    #[test]
    fn the_model_id_is_validated_and_zdr_is_off_only_for_zero() {
        let model = |value: &'static str| {
            move |name: &str| match name {
                ENABLED_ENV => Some("1".to_string()),
                MODEL_ENV => Some(value.to_string()),
                _ => None,
            }
        };
        let env = model("vendor/model-2.1:beta");
        assert_eq!(
            inputs(&env, &primary_key).model().unwrap(),
            "vendor/model-2.1:beta"
        );
        for bad in ["has space", "new\nline", "quote\"", &"x".repeat(129)] {
            let bad: &'static str = Box::leak(bad.to_string().into_boxed_str());
            let env = model(bad);
            assert_eq!(
                inputs(&env, &primary_key).readiness(),
                Err(NotReady::InvalidModel),
                "{bad:?}"
            );
        }
        for (value, requested) in [
            (None, true),
            (Some("0"), false),
            (Some("1"), true),
            (Some("false"), true),
            (Some(""), true),
        ] {
            let env = move |name: &str| {
                (name == ZDR_ENV)
                    .then(|| value.map(str::to_string))
                    .flatten()
            };
            assert_eq!(
                inputs(&env, &primary_key).zdr_requested(),
                requested,
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_launch_carries_the_endpoint_and_key_but_never_puts_the_key_in_an_override() {
        let launch = inputs(&enabled_env, &primary_key).launch().unwrap();
        assert_eq!(launch.base_url, DEFAULT_BASE_URL);
        assert_eq!(launch.api_key, "xpl_fixture");
        let values = launch.codex_overrides();
        for wanted in [
            "model_provider=\"augmentagent_yolo\"".to_string(),
            format!("model_providers.augmentagent_yolo.base_url=\"{DEFAULT_BASE_URL}\""),
            "model_providers.augmentagent_yolo.wire_api=\"responses\"".to_string(),
            "model_providers.augmentagent_yolo.env_key=\"AUGMENTAGENT_YOLO_API_KEY\"".to_string(),
            "model_providers.augmentagent_yolo.requires_openai_auth=false".to_string(),
        ] {
            assert!(values.contains(&wanted), "{wanted} missing from {values:?}");
        }
        assert!(values
            .iter()
            .any(|value| value.contains("\"X-Gateway-ZDR\" = \"true\"")));
        assert!(!values.iter().any(|value| value.contains("xpl_fixture")));
        // With retention requests off, no header is sent at all.
        let env = |name: &str| match name {
            ENABLED_ENV => Some("1".to_string()),
            ZDR_ENV => Some("0".to_string()),
            _ => None,
        };
        let values = inputs(&env, &primary_key)
            .launch()
            .unwrap()
            .codex_overrides();
        assert!(!values.iter().any(|value| value.contains("ZDR")));
        // Not ready means no launch.
        assert_eq!(
            inputs(&|_| None, &primary_key).launch(),
            Err(NotReady::Disabled)
        );
    }

    #[test]
    fn a_missing_codex_cli_or_state_directory_is_reported() {
        let mut missing = inputs(&enabled_env, &primary_key);
        missing.codex_installed = false;
        assert_eq!(missing.readiness(), Err(NotReady::CodexMissing));
        let mut missing = inputs(&enabled_env, &primary_key);
        missing.state_dir = false;
        assert_eq!(missing.readiness(), Err(NotReady::WorkspaceUnavailable));
        for cause in [
            NotReady::Disabled,
            NotReady::MissingKey,
            NotReady::InvalidEndpoint,
            NotReady::InvalidModel,
            NotReady::CodexMissing,
            NotReady::WorkspaceUnavailable,
        ] {
            assert!(cause.sentence().ends_with('.'));
        }
    }
}
