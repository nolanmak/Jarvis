//! Identity index over `people/*.md` pages.
//!
//! Each page may declare an `identities:` block in its YAML front-matter linking
//! the same person across platforms (email, linkedin urn, discord snowflake, …).
//! The index walks `people/` once, parses front-matter, and answers
//! `(platform, id) → PersonPage` queries.
//!
//! Scale: hundreds of people pages. Linear scan on lookup is fine; no caching.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_yaml_ng::Value;
use tracing::warn;

use crate::layout::WikiLayout;

/// Multi-platform identity block lifted from a `people/<slug>.md` front-matter.
///
/// `email` is a `Vec<String>` because one person often has multiple addresses.
/// The other fields are `Option<String>` — one account per platform is the
/// normal case; upgrade to `Vec` only if that changes.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Identities {
    #[serde(default, deserialize_with = "one_or_many")]
    pub email: Vec<String>,
    #[serde(default)]
    pub linkedin: Option<String>,
    #[serde(default)]
    pub discord: Option<String>,
    #[serde(default)]
    pub twitter: Option<String>,
    #[serde(default)]
    pub slack: Option<String>,
    #[serde(default)]
    pub whatsapp: Option<String>,
    #[serde(default)]
    pub instagram: Option<String>,
    /// E.164-normalized phone (#62). Multi-valued — a person commonly has a
    /// mobile + a work line; CRM ingestion union-merges them.
    #[serde(default, deserialize_with = "one_or_many")]
    pub phone: Vec<String>,
    /// Free-form mailing address (#62). Single-valued in practice; CRM
    /// ingestion only fills it when blank.
    #[serde(default)]
    pub address: Option<String>,
    /// iMessage handles (#883): E.164 phone numbers or Apple-ID email
    /// addresses. Multi-valued — a person can text from both. Lookup also
    /// falls back to `phone`, so Contacts-imported people resolve without
    /// duplicating numbers here.
    #[serde(default, deserialize_with = "one_or_many")]
    pub imessage: Vec<String>,
}

/// List-valued identity fields also accept a bare scalar. Pages written by
/// the CRM merge before it rendered lists (`phone: "+1…"`), hand edits on
/// GitHub and an unquoted numeric short code (`imessage: 29694`) all occur
/// on disk, and one such page must not silently vanish from the index —
/// `IdentityIndex::build` skips anything that fails to parse.
fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    fn scalar(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
    match Value::deserialize(d)? {
        Value::Null => Ok(Vec::new()),
        Value::Sequence(items) => Ok(items.iter().filter_map(scalar).collect()),
        other => scalar(&other)
            .map(|s| vec![s])
            .ok_or_else(|| D::Error::custom("expected a string or a list of strings")),
    }
}

impl Identities {
    /// `true` if this identity block has an entry for `platform` matching `id`.
    /// Case-sensitive for IDs (emails are lowercased before comparison since
    /// address case is non-significant per RFC 5321, but we don't normalize
    /// LinkedIn URNs / Discord snowflakes / etc.).
    pub fn matches(&self, platform: &str, id: &str) -> bool {
        match platform {
            "email" => {
                let target = id.to_ascii_lowercase();
                self.email.iter().any(|e| e.to_ascii_lowercase() == target)
            }
            "linkedin" => self.linkedin.as_deref() == Some(id),
            "discord" => self.discord.as_deref() == Some(id),
            "twitter" => self.twitter.as_deref() == Some(id),
            "slack" => self.slack.as_deref() == Some(id),
            "whatsapp" => self.whatsapp.as_deref() == Some(id),
            "instagram" => self.instagram.as_deref() == Some(id),
            "phone" => {
                // E.164 ids compared verbatim (already normalized upstream).
                self.phone.iter().any(|p| p == id)
            }
            "imessage" => {
                // Handles are E.164 phones (verbatim) or Apple-ID emails
                // (case-insensitive). Phone-shaped handles also match the
                // `phone` array — most texters arrive via Contacts import.
                let email_like = id.contains('@');
                self.imessage.iter().any(|h| {
                    if email_like && h.contains('@') {
                        h.eq_ignore_ascii_case(id)
                    } else {
                        h == id
                    }
                }) || (!email_like && self.phone.iter().any(|p| p == id))
            }
            "address" => self.address.as_deref() == Some(id),
            _ => false,
        }
    }
}

/// Minimal front-matter subset we care about. `#[serde(default)]` makes every
/// field optional so legacy pages (no `identities:` block) deserialize cleanly.
#[derive(Debug, Clone, Default, Deserialize)]
struct FrontMatter {
    #[serde(default)]
    identities: Identities,
    /// User-set engagement opt-in (#13). When `true` *and* the page carries
    /// a `linkedin:` identity, the LinkedIn feed trigger watches this
    /// person's posts for supportive-comment engagement. USER-SET ONLY —
    /// ingest never writes this.
    #[serde(default)]
    close: bool,
}

/// One `people/<slug>.md` page + its parsed identities.
#[derive(Debug, Clone)]
pub struct PersonPage {
    pub slug: String,
    pub path: PathBuf,
    pub identities: Identities,
    /// `close: true` front-matter marker (#13). Drives LinkedIn feed-
    /// engagement opt-in.
    pub close: bool,
}

/// In-memory index of every `people/*.md` page's identity block. Rebuild on
/// demand — the walk is cheap and there's no invalidation logic to get wrong.
#[derive(Debug, Clone, Default)]
pub struct IdentityIndex {
    people: Vec<PersonPage>,
}

impl IdentityIndex {
    /// Walk `layout.people_dir()`, parse each `.md` file's front-matter, and
    /// collect every page with its parsed (possibly empty) identity block.
    ///
    /// Files that fail to parse (malformed YAML, missing delimiters, etc.)
    /// are logged and skipped — one bad page must not break the whole index.
    pub fn build(layout: &WikiLayout) -> io::Result<Self> {
        let dir = layout.people_dir();
        if !dir.exists() {
            return Ok(Self::default());
        }

        let mut people = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let slug = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };

            match parse_front_matter(&path) {
                Ok((identities, close)) => {
                    people.push(PersonPage { slug, path, identities, close })
                }
                Err(e) => warn!(path = %path.display(), error = %e, "skipping page with bad front-matter"),
            }
        }
        Ok(Self { people })
    }

    /// O(n) scan — hundreds of pages, irrelevant at our scale.
    pub fn lookup(&self, platform: &str, id: &str) -> Option<&PersonPage> {
        self.people.iter().find(|p| p.identities.matches(platform, id))
    }

    pub fn len(&self) -> usize {
        self.people.len()
    }

    pub fn is_empty(&self) -> bool {
        self.people.is_empty()
    }

    pub fn pages(&self) -> &[PersonPage] {
        &self.people
    }

    /// Pages flagged `close: true` that also carry a `linkedin:` identity —
    /// the watch-list for #13 feed engagement. Returns `(slug, linkedin_urn)`
    /// pairs.
    pub fn close_linkedin_people(&self) -> Vec<(String, String)> {
        self.people
            .iter()
            .filter(|p| p.close)
            .filter_map(|p| {
                p.identities
                    .linkedin
                    .clone()
                    .map(|urn| (p.slug.clone(), urn))
            })
            .collect()
    }
}

/// Extract the YAML front-matter from a markdown file and deserialize it.
/// Accepts files without front-matter (returns `(Identities::default(),
/// false)`). Returns `(identities, close)`.
fn parse_front_matter(path: &Path) -> anyhow::Result<(Identities, bool)> {
    let raw = fs::read_to_string(path)?;
    let yaml = match extract_yaml_block(&raw) {
        Some(y) => y,
        None => return Ok((Identities::default(), false)),
    };
    let fm: FrontMatter = serde_yaml_ng::from_str(yaml)?;
    Ok((fm.identities, fm.close))
}

/// Pull the YAML between the opening `---\n` and the next `\n---` line.
/// Returns `None` if there is no front-matter block.
fn extract_yaml_block(src: &str) -> Option<&str> {
    let after_open = src.strip_prefix("---\n").or_else(|| src.strip_prefix("---\r\n"))?;
    // Scan line-by-line for a closing `---` on its own line.
    let mut offset = 0usize;
    for line in after_open.lines() {
        if line.trim_end() == "---" {
            return Some(&after_open[..offset]);
        }
        // +1 for the newline that `lines()` strips. On the last line without a
        // trailing newline this would overshoot, but we'd only get here if the
        // closing `---` never appeared — in which case we return None below.
        offset += line.len() + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_page(dir: &Path, slug: &str, front_matter: &str) {
        let path = dir.join(format!("{slug}.md"));
        let body = format!("---\n{front_matter}\n---\n\n# {slug}\n");
        fs::write(path, body).unwrap();
    }

    fn layout_with_pages(pages: &[(&str, &str)]) -> (TempDir, WikiLayout) {
        let dir = TempDir::new().unwrap();
        let layout = WikiLayout::new(dir.path().to_path_buf());
        fs::create_dir_all(layout.people_dir()).unwrap();
        for (slug, fm) in pages {
            write_page(&layout.people_dir(), slug, fm);
        }
        (dir, layout)
    }

    #[test]
    fn parses_identities_block() {
        let (_d, layout) = layout_with_pages(&[(
            "jane",
            "kind: person\nkey: jane\nidentities:\n  email: [jane@corp.com, jane@personal.com]\n  linkedin: urn:li:fsd_profile:XYZ\n  discord: \"999\"",
        )]);
        let index = IdentityIndex::build(&layout).unwrap();
        let page = index.lookup("linkedin", "urn:li:fsd_profile:XYZ").unwrap();
        assert_eq!(page.slug, "jane");
        assert_eq!(page.identities.linkedin.as_deref(), Some("urn:li:fsd_profile:XYZ"));
        assert_eq!(page.identities.email.len(), 2);
    }

    #[test]
    fn close_flag_parsed_and_filters_to_linkedin_people() {
        let (_d, layout) = layout_with_pages(&[
            (
                "jane",
                "kind: person\nkey: jane\nclose: true\nidentities:\n  linkedin: urn:li:fsd_profile:JANE",
            ),
            (
                "bob",
                // close but no linkedin identity → excluded from watch-list
                "kind: person\nkey: bob\nclose: true\nidentities:\n  email: [bob@x.com]",
            ),
            (
                "carol",
                // linkedin but not close → excluded
                "kind: person\nkey: carol\nidentities:\n  linkedin: urn:li:fsd_profile:CAROL",
            ),
        ]);
        let index = IdentityIndex::build(&layout).unwrap();
        let jane = index.lookup("linkedin", "urn:li:fsd_profile:JANE").unwrap();
        assert!(jane.close);
        let watch = index.close_linkedin_people();
        assert_eq!(watch.len(), 1);
        assert_eq!(watch[0].0, "jane");
        assert_eq!(watch[0].1, "urn:li:fsd_profile:JANE");
    }

    #[test]
    fn close_defaults_false_when_absent() {
        let (_d, layout) = layout_with_pages(&[(
            "jane",
            "kind: person\nkey: jane\nidentities:\n  linkedin: urn:li:fsd_profile:JANE",
        )]);
        let index = IdentityIndex::build(&layout).unwrap();
        assert!(!index.pages()[0].close);
        assert!(index.close_linkedin_people().is_empty());
    }

    #[test]
    fn legacy_page_without_identities_block_deserializes_to_default() {
        let (_d, layout) = layout_with_pages(&[(
            "aadit",
            "kind: person\nkey: aadit\ncreated: 2026-04-18",
        )]);
        let index = IdentityIndex::build(&layout).unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index.pages()[0].identities, Identities::default());
    }

    #[test]
    fn lookup_by_email_is_case_insensitive() {
        let (_d, layout) = layout_with_pages(&[(
            "jane",
            "kind: person\nkey: jane\nidentities:\n  email: [Jane@Corp.COM]",
        )]);
        let index = IdentityIndex::build(&layout).unwrap();
        assert!(index.lookup("email", "jane@corp.com").is_some());
        assert!(index.lookup("email", "JANE@CORP.COM").is_some());
    }

    #[test]
    fn lookup_by_discord_matches_exact() {
        let (_d, layout) = layout_with_pages(&[(
            "bob",
            "kind: person\nkey: bob\nidentities:\n  discord: \"123456789012345678\"",
        )]);
        let index = IdentityIndex::build(&layout).unwrap();
        assert!(index.lookup("discord", "123456789012345678").is_some());
        assert!(index.lookup("discord", "000").is_none());
    }

    #[test]
    fn lookup_miss_returns_none() {
        let (_d, layout) = layout_with_pages(&[(
            "jane",
            "kind: person\nkey: jane\nidentities:\n  discord: \"999\"",
        )]);
        let index = IdentityIndex::build(&layout).unwrap();
        assert!(index.lookup("twitter", "999").is_none());
    }

    #[test]
    fn unknown_platform_returns_none() {
        let (_d, layout) = layout_with_pages(&[(
            "jane",
            "kind: person\nkey: jane\nidentities:\n  discord: \"999\"",
        )]);
        let index = IdentityIndex::build(&layout).unwrap();
        assert!(index.lookup("signal", "999").is_none());
    }

    #[test]
    fn lookup_by_imessage_handle_matches_declared_handles() {
        let (_d, layout) = layout_with_pages(&[(
            "jane",
            "kind: person\nkey: jane\nidentities:\n  imessage: [\"+14155550123\", \"Jane@iCloud.com\"]",
        )]);
        let index = IdentityIndex::build(&layout).unwrap();
        // E.164 handles compare verbatim (normalized upstream)
        assert!(index.lookup("imessage", "+14155550123").is_some());
        // Apple-ID email handles are case-insensitive like email
        assert!(index.lookup("imessage", "jane@icloud.com").is_some());
        assert!(index.lookup("imessage", "+10000000000").is_none());
    }

    #[test]
    fn imessage_lookup_falls_back_to_phone_identities() {
        // most iMessage handles are plain phone numbers; a person imported
        // from Contacts with only `phone:` must resolve without duplicate
        // bookkeeping in `imessage:`
        let (_d, layout) = layout_with_pages(&[(
            "bob",
            "kind: person\nkey: bob\nidentities:\n  phone: [\"+14155550999\"]",
        )]);
        let index = IdentityIndex::build(&layout).unwrap();
        assert!(index.lookup("imessage", "+14155550999").is_some());
        // but not the reverse: an imessage email handle is not a phone
        assert!(index.lookup("phone", "bob@icloud.com").is_none());
    }

    #[test]
    fn list_identity_fields_accept_legacy_scalars() {
        let dir = tempfile::TempDir::new().unwrap();
        let layout = WikiLayout::new(dir.path().to_path_buf());
        std::fs::create_dir_all(layout.people_dir()).unwrap();
        std::fs::write(
            layout.people_dir().join("legacy.md"),
            "---\nkind: person\nidentities:\n  email: a@example.com\n  phone: \"+15550001\"\n  imessage: 29694\n---\n",
        )
        .unwrap();
        let index = IdentityIndex::build(&layout).unwrap();
        assert_eq!(index.len(), 1, "scalar-shaped page must still index");
        let p = index.lookup("phone", "+15550001").expect("scalar phone");
        assert_eq!(p.identities.email, vec!["a@example.com"]);
        assert_eq!(p.identities.imessage, vec!["29694"], "number stringified");
        assert!(index.lookup("imessage", "29694").is_some());
    }

    #[test]
    fn malformed_yaml_is_skipped_not_fatal() {
        let dir = TempDir::new().unwrap();
        let layout = WikiLayout::new(dir.path().to_path_buf());
        fs::create_dir_all(layout.people_dir()).unwrap();
        // Valid page first.
        write_page(
            &layout.people_dir(),
            "ok",
            "kind: person\nkey: ok\nidentities:\n  discord: \"111\"",
        );
        // Intentionally broken YAML (unclosed array).
        let broken = layout.people_dir().join("broken.md");
        fs::write(broken, "---\nidentities:\n  email: [one,\n---\n\n# broken\n").unwrap();

        let index = IdentityIndex::build(&layout).unwrap();
        // Broken skipped, good one present.
        assert_eq!(index.len(), 1);
        assert!(index.lookup("discord", "111").is_some());
    }

    #[test]
    fn empty_people_dir_yields_empty_index() {
        let dir = TempDir::new().unwrap();
        let layout = WikiLayout::new(dir.path().to_path_buf());
        let index = IdentityIndex::build(&layout).unwrap();
        assert!(index.is_empty());
    }

    #[test]
    fn missing_people_dir_yields_empty_index_not_error() {
        let dir = TempDir::new().unwrap();
        let layout = WikiLayout::new(dir.path().to_path_buf());
        // Note: never created layout.people_dir()
        let index = IdentityIndex::build(&layout).unwrap();
        assert!(index.is_empty());
    }
}
