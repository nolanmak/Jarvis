//! Operator-owned, offline GitHub repository identities. Never read aliases
//! from candidate Git config, issue text, model options, or a network lookup.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(super) const ENV: &str = "AUGMENTAGENT_REPOSITORY_ALIASES";

pub(super) fn slug(value: &str) -> Result<String> {
    let lower = value.to_ascii_lowercase();
    let value = lower.as_str();
    let parts: Vec<_> = value.split('/').collect();
    anyhow::ensure!(
        parts.len() == 2
            && parts.iter().all(|part| !part.is_empty()
                && !matches!(*part, "." | "..")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))),
        "invalid repository identity"
    );
    Ok(value.to_ascii_lowercase())
}

pub(super) fn github(origin: &str) -> Result<Option<String>> {
    anyhow::ensure!(
        !origin.is_empty()
            && origin.len() <= 16384
            && !origin.bytes().any(|b| b.is_ascii_control()),
        "invalid repository URL envelope"
    );
    if origin.starts_with('/') || origin.starts_with("file://") {
        return Ok(None);
    }
    let path = if let Some(scp) = origin.strip_prefix("git@") {
        let (host, path) = scp
            .split_once(':')
            .context("invalid GitHub SSH destination")?;
        anyhow::ensure!(
            host.eq_ignore_ascii_case("github.com"),
            "repository host is not GitHub"
        );
        path.to_owned()
    } else {
        let url = reqwest::Url::parse(origin).context("invalid repository URL")?;
        anyhow::ensure!(
            url.host_str() == Some("github.com")
                && url.port().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.password().is_none()
                && ((url.scheme() == "https" && url.username().is_empty())
                    || (url.scheme() == "ssh" && url.username() == "git")),
            "unsupported GitHub repository URL"
        );
        url.path()
            .strip_prefix('/')
            .context("repository path unavailable")?
            .to_owned()
    };
    let lower = path.to_ascii_lowercase();
    slug(lower.strip_suffix(".git").unwrap_or(&lower)).map(Some)
}

pub(super) struct Policy {
    names: BTreeMap<String, String>,
    legacy_urls: BTreeMap<String, Vec<String>>,
}
impl Policy {
    pub(super) fn from_env() -> Result<Self> {
        match std::env::var(ENV) {
            Ok(raw) => Self::parse(&raw),
            Err(std::env::VarError::NotPresent) => Self::parse(""),
            Err(error) => Err(error).context("repository alias policy is not UTF-8"),
        }
    }
    fn parse(raw: &str) -> Result<Self> {
        anyhow::ensure!(raw.len() <= 16384, "repository alias policy exceeds bound");
        let groups: BTreeMap<String, Vec<String>> = if raw.trim().is_empty() {
            BTreeMap::new()
        } else {
            serde_json::from_str(raw).context("invalid repository alias policy")?
        };
        let mut names = BTreeMap::new();
        let mut legacy_urls = BTreeMap::<String, Vec<String>>::new();
        for canonical in groups.keys() {
            let canonical = slug(canonical)?;
            anyhow::ensure!(
                names.insert(canonical.clone(), canonical).is_none(),
                "duplicate canonical repository identity"
            );
        }
        for (canonical, urls) in groups {
            let canonical = slug(&canonical)?;
            for url in urls {
                let alias = github(&url)?.context("repository aliases must be GitHub URLs")?;
                if let Some(prior) = names.get(&alias) {
                    anyhow::ensure!(prior == &canonical, "ambiguous or chained repository alias");
                }
                names.insert(alias, canonical.clone());
                legacy_urls.entry(canonical.clone()).or_default().push(url);
            }
        }
        Ok(Self { names, legacy_urls })
    }
    pub(super) fn canonical(&self, name: &str) -> Result<String> {
        let name = slug(name)?;
        Ok(self.names.get(&name).cloned().unwrap_or(name))
    }
    pub(super) fn origin(&self, origin: &str) -> Result<Option<String>> {
        github(origin)?
            .map(|name| self.canonical(&name))
            .transpose()
    }
    pub(super) fn identity_hash_matches(&self, canonical: &str, hash: &str) -> bool {
        let digest =
            |name: &str| format!("{:x}", Sha256::digest(format!("github:{name}").as_bytes()));
        digest(canonical) == hash
            || self
                .names
                .iter()
                .any(|(name, target)| target == canonical && digest(name) == hash)
    }
    pub(super) fn legacy_hash_matches(
        &self,
        canonical: &str,
        current_url: &str,
        hash: &str,
    ) -> bool {
        let digest = |url: &str| format!("{:x}", Sha256::digest(url.as_bytes()));
        if digest(current_url) == hash {
            return true;
        }
        if self
            .legacy_urls
            .get(canonical)
            .is_some_and(|urls| urls.iter().any(|url| digest(url) == hash))
        {
            return true;
        }
        // Also recognize the conventional spellings of the canonical slug.
        [
            format!("https://github.com/{canonical}"),
            format!("https://github.com/{canonical}.git"),
            format!("git@github.com:{canonical}.git"),
            format!("ssh://git@github.com/{canonical}.git"),
        ]
        .iter()
        .any(|url| digest(url) == hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_are_offline_explicit_and_unambiguous() {
        assert_eq!(
            github("https://github.com/fixture/current.git.git")
                .unwrap()
                .as_deref(),
            Some("fixture/current.git")
        );
        let policy =
            Policy::parse(r#"{"fixture/current":["https://github.com/fixture/Before.git"]}"#)
                .unwrap();
        for url in [
            "https://github.com/fixture/Before.git",
            "git@github.com:FIXTURE/before",
            "ssh://git@github.com/fixture/current.GIT",
        ] {
            assert_eq!(
                policy.origin(url).unwrap().as_deref(),
                Some("fixture/current")
            );
        }
        assert_eq!(
            policy
                .origin("https://github.com/fixture/foreign.git")
                .unwrap()
                .as_deref(),
            Some("fixture/foreign")
        );
        for raw in [
            r#"{"fixture/a":["https://github.com/fixture/b"],"fixture/b":[]}"#,
            r#"{"fixture/a":["https://github.com/fixture/old"],"fixture/b":["https://github.com/fixture/OLD.git"]}"#,
            r#"{"fixture/a":["/local/mirror"]}"#,
        ] {
            assert!(Policy::parse(raw).is_err());
        }
        for url in [
            "https://github.com.evil.test/fixture/current",
            "https://github.com/fixture/current?x=y",
            "https://github.com/fixture/current#fragment",
            "https://github.com/fixture/../current",
        ] {
            assert!(policy.origin(url).is_err());
        }
    }
}
