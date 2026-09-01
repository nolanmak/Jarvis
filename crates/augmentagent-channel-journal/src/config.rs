//! Opt-in ShadowNote configuration.
//!
//! All values load via `secret_loader` conventions (Linux Secret Service
//! slot `augmentagent/api-key` first, env/`.env` fallback). None of them
//! may be hardcoded — this repo is public; the actual values live in the
//! private ShadowNote repo issue and on the box.

use augmentagent_channel_core::secret_loader::load_provider_key;

pub const ENV_APPSYNC_URL: &str = "SHADOWNOTE_APPSYNC_URL";
pub const ENV_OWNER_ID: &str = "SHADOWNOTE_OWNER_ID";
pub const ENV_OWNER_FIELD: &str = "SHADOWNOTE_OWNER_FIELD";
pub const ENV_KMS_KEY_ARN: &str = "SHADOWNOTE_KMS_KEY_ARN";

#[derive(Debug, Clone)]
pub struct JournalConfig {
    /// AppSync GraphQL endpoint, e.g. `https://<id>.appsync-api.<region>.amazonaws.com/graphql`.
    pub appsync_url: String,
    /// `Entry.ownerId` partition-key value — every query is scoped to it.
    pub owner_id: String,
    /// Cognito `owner` field value stamped on created entries so they stay
    /// visible to the app's owner-auth reads. Defaults to `owner_id`; set
    /// explicitly if existing rows use the `sub::username` form.
    pub owner_field: String,
    /// CMK for `GenerateDataKey` on the write path. Reads don't need it
    /// (KMS infers the key from the ciphertext blob).
    pub kms_key_arn: Option<String>,
    /// Signing region; `AWS_REGION` overrides, else derived from the URL.
    pub region: String,
}

impl JournalConfig {
    /// `None` = integration not configured. The daemon treats that as
    /// "feature off" and must start cleanly. Missing `owner_id` also
    /// returns `None` (fail closed — see the owner-scoping invariant in
    /// the crate docs) rather than falling back to an unscoped client.
    pub fn load() -> Option<Self> {
        let appsync_url = load_provider_key(ENV_APPSYNC_URL)?;
        let owner_id = load_provider_key(ENV_OWNER_ID)?;
        let owner_field = load_provider_key(ENV_OWNER_FIELD).unwrap_or_else(|| owner_id.clone());
        let kms_key_arn = load_provider_key(ENV_KMS_KEY_ARN);
        let region = std::env::var("AWS_REGION")
            .ok()
            .or_else(|| region_from_appsync_url(&appsync_url))
            .unwrap_or_else(|| "us-east-1".to_string());
        Some(Self {
            appsync_url,
            owner_id,
            owner_field,
            kms_key_arn,
            region,
        })
    }
}

/// `<api-id>.appsync-api.<region>.amazonaws.com` → `<region>`.
pub(crate) fn region_from_appsync_url(url: &str) -> Option<String> {
    let host = reqwest::Url::parse(url).ok()?.host_str()?.to_string();
    let parts: Vec<&str> = host.split('.').collect();
    let idx = parts.iter().position(|p| *p == "appsync-api")?;
    parts.get(idx + 1).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_parses_from_appsync_host() {
        assert_eq!(
            region_from_appsync_url("https://abc123.appsync-api.us-east-1.amazonaws.com/graphql"),
            Some("us-east-1".to_string())
        );
    }

    #[test]
    fn region_absent_for_non_appsync_host() {
        assert_eq!(region_from_appsync_url("http://127.0.0.1:8080/graphql"), None);
    }
}
