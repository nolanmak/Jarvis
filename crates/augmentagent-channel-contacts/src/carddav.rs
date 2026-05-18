//! Backend B — generic CardDAV (#62).
//!
//! Vendor-neutral: works against Nextcloud, Fastmail, Radicale, Baïkal —
//! anything that speaks RFC 6352. Configured purely from env so no schema /
//! UI change is needed:
//!
//! - `AUGMENTAGENT_CARDDAV_URL`  — the addressbook collection URL
//! - `AUGMENTAGENT_CARDDAV_USER` — basic-auth user
//! - `AUGMENTAGENT_CARDDAV_PASS` — basic-auth password
//!
//! Sync strategy:
//! 1. `PROPFIND Depth:0` on the collection → read `getctag`. If it equals the
//!    persisted token, **nothing changed** — return an empty pull (cheap
//!    no-op, the issue's "getctag change detection").
//! 2. `PROPFIND Depth:1` → list every `.vcf` href.
//! 3. `GET` each href, concatenate, hand the buffer to the shared
//!    [`crate::vcard::parse_vcards`].
//!
//! The new `getctag` is returned as `next_sync_token`.
//!
//! **iCloud is explicitly out of scope for v1** (documented known gap): it
//! requires app-specific passwords and its CardDAV endpoint is reportedly
//! flaky from Linux hosts (opaque 5xx, principal-discovery quirks). Use the
//! Google People backend for iCloud-synced contacts instead.

use async_trait::async_trait;
use reqwest::Method;

use crate::source::{ContactsError, ContactsPull, ContactsSource};
use crate::vcard::parse_vcards;

/// Env var names — public so the CLI can surface a clear "not configured"
/// message listing exactly what to set.
pub const ENV_URL: &str = "AUGMENTAGENT_CARDDAV_URL";
pub const ENV_USER: &str = "AUGMENTAGENT_CARDDAV_USER";
pub const ENV_PASS: &str = "AUGMENTAGENT_CARDDAV_PASS";

pub struct CardDavSource {
    http: reqwest::Client,
    collection_url: String,
    user: String,
    pass: String,
}

impl CardDavSource {
    pub fn new(collection_url: String, user: String, pass: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            collection_url,
            user,
            pass,
        }
    }

    /// Build from env. Returns `None` (not an error) when unconfigured so
    /// the caller can cleanly skip Backend B.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var(ENV_URL).ok().filter(|s| !s.is_empty())?;
        let user = std::env::var(ENV_USER).ok().unwrap_or_default();
        let pass = std::env::var(ENV_PASS).ok().unwrap_or_default();
        Some(Self::new(url, user, pass))
    }

    async fn propfind(
        &self,
        depth: &str,
        body: &str,
    ) -> Result<String, ContactsError> {
        let method = Method::from_bytes(b"PROPFIND")
            .expect("PROPFIND is a valid method token");
        let resp = self
            .http
            .request(method, &self.collection_url)
            .basic_auth(&self.user, Some(&self.pass))
            .header("Depth", depth)
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body.to_string())
            .send()
            .await?;
        if !resp.status().is_success() && resp.status().as_u16() != 207 {
            let s = resp.status();
            let t = resp.text().await.unwrap_or_default();
            return Err(ContactsError::Backend(format!(
                "PROPFIND {depth} → {s}: {t}"
            )));
        }
        Ok(resp.text().await?)
    }
}

/// Extract the first `<getctag>` value (any namespace prefix). Cheap string
/// scan — a full XML parser is overkill for one element and adds a
/// dependency. Returns `""` if absent (forces a full pull, which is safe).
pub fn parse_getctag(xml: &str) -> String {
    extract_tag(xml, "getctag").unwrap_or_default()
}

/// Collect every `<href>…/something.vcf</href>` in a multistatus body.
pub fn parse_vcf_hrefs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = find_tag_open(rest, "href") {
        let after = &rest[start..];
        let Some(close_rel) = after.find('>') else { break };
        let content_start = start + close_rel + 1;
        let Some(end_rel) = rest[content_start..].find('<') else { break };
        let href = rest[content_start..content_start + end_rel].trim();
        if href.to_ascii_lowercase().ends_with(".vcf") {
            out.push(href.to_string());
        }
        rest = &rest[content_start + end_rel..];
    }
    out
}

fn extract_tag(xml: &str, local: &str) -> Option<String> {
    let open = find_tag_open(xml, local)?;
    let after = &xml[open..];
    let gt = after.find('>')?;
    let content_start = open + gt + 1;
    let end = xml[content_start..].find('<')?;
    Some(xml[content_start..content_start + end].trim().to_string())
}

/// Find the byte offset of `<` for an element with the given *local* name,
/// tolerating namespace prefixes (`<d:getctag`, `<getctag`, `<CS:getctag`).
fn find_tag_open(xml: &str, local: &str) -> Option<usize> {
    let bytes = xml.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // skip optional prefix `xxx:`
            let mut j = i + 1;
            let tag_start = j;
            while j < bytes.len()
                && bytes[j] != b'>'
                && bytes[j] != b' '
                && bytes[j] != b'/'
            {
                j += 1;
            }
            let raw = &xml[tag_start..j];
            let name = raw.rsplit(':').next().unwrap_or(raw);
            if name.eq_ignore_ascii_case(local) {
                return Some(i);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

#[async_trait]
impl ContactsSource for CardDavSource {
    fn backend_id(&self) -> &'static str {
        "carddav"
    }

    async fn list_contacts(
        &self,
        since_token: Option<&str>,
    ) -> Result<ContactsPull, ContactsError> {
        // 1. getctag — cheap change detection.
        let ctag_body = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop><cs:getctag/></d:prop>
</d:propfind>"#;
        let ctag_xml = self.propfind("0", ctag_body).await?;
        let ctag = parse_getctag(&ctag_xml);
        if !ctag.is_empty() && Some(ctag.as_str()) == since_token {
            // Collection unchanged — nothing to do.
            return Ok(ContactsPull {
                cards: Vec::new(),
                next_sync_token: Some(ctag),
            });
        }

        // 2. List .vcf hrefs.
        let list_body = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:getetag/><d:resourcetype/></d:prop>
</d:propfind>"#;
        let list_xml = self.propfind("1", list_body).await?;
        let hrefs = parse_vcf_hrefs(&list_xml);

        // 3. GET each .vcf, concatenate, parse once.
        let base = url_origin(&self.collection_url);
        let mut buf = String::new();
        for href in hrefs {
            let url = if href.starts_with("http") {
                href
            } else {
                format!("{base}{href}")
            };
            match self
                .http
                .get(&url)
                .basic_auth(&self.user, Some(&self.pass))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    if let Ok(t) = r.text().await {
                        buf.push_str(&t);
                        buf.push('\n');
                    }
                }
                Ok(r) => {
                    tracing::warn!(%url, status = %r.status(), "carddav vcf GET failed");
                }
                Err(e) => tracing::warn!(%url, "carddav vcf GET error: {e}"),
            }
        }

        Ok(ContactsPull {
            cards: parse_vcards(&buf),
            next_sync_token: if ctag.is_empty() { None } else { Some(ctag) },
        })
    }
}

/// `https://host:port/path/...` → `https://host:port` for resolving
/// collection-relative hrefs.
fn url_origin(u: &str) -> String {
    let after_scheme = match u.find("://") {
        Some(i) => i + 3,
        None => return String::new(),
    };
    let rest = &u[after_scheme..];
    let host_end = rest.find('/').unwrap_or(rest.len());
    format!("{}{}", &u[..after_scheme], &rest[..host_end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getctag_extracted_with_any_prefix() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
          <d:response><d:propstat><d:prop><cs:getctag>"abc-123"</cs:getctag></d:prop></d:propstat></d:response>
        </d:multistatus>"#;
        assert_eq!(parse_getctag(xml), "\"abc-123\"");
    }

    #[test]
    fn vcf_hrefs_collected_only_for_vcf() {
        let xml = r#"<multistatus>
          <response><href>/addr/jane.vcf</href></response>
          <response><href>/addr/</href></response>
          <response><href>/addr/john.VCF</href></response>
        </multistatus>"#;
        let hrefs = parse_vcf_hrefs(xml);
        assert_eq!(hrefs, vec!["/addr/jane.vcf", "/addr/john.VCF"]);
    }

    #[test]
    fn url_origin_strips_path() {
        assert_eq!(
            url_origin("https://dav.example.com:8443/addressbooks/u/default/"),
            "https://dav.example.com:8443"
        );
    }

    #[test]
    fn from_env_none_when_unset() {
        // Ensure unset → None (don't clobber a real env in CI: use a guard).
        let prev = std::env::var(ENV_URL).ok();
        std::env::remove_var(ENV_URL);
        assert!(CardDavSource::from_env().is_none());
        if let Some(p) = prev {
            std::env::set_var(ENV_URL, p);
        }
    }

    #[tokio::test]
    async fn unchanged_ctag_short_circuits() {
        let mut server = mockito::Server::new_async().await;
        let ctag_resp = r#"<multistatus xmlns:cs="http://calendarserver.org/ns/">
          <response><propstat><prop><cs:getctag>v7</cs:getctag></prop></propstat></response>
        </multistatus>"#;
        let _m = server
            .mock("PROPFIND", "/")
            .with_status(207)
            .with_body(ctag_resp)
            .create_async()
            .await;
        let src =
            CardDavSource::new(format!("{}/", server.url()), "u".into(), "p".into());
        let pull = src.list_contacts(Some("v7")).await.unwrap();
        assert!(pull.cards.is_empty());
        assert_eq!(pull.next_sync_token.as_deref(), Some("v7"));
    }
}
