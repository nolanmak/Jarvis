//! Fill-blanks-only person-page upsert for CRM ingestion (#61, #62, #64).
//!
//! Three ingestion sources — LinkedIn 1st-degree connections, Google /
//! CardDAV contacts, and email-signature parsing — all need to merge
//! self-reported third-party facts into `wiki/people/<slug>.md` under one
//! **iron rule**: *never overwrite, never invent.* An empty wiki field may be
//! filled; a non-empty one is left exactly as the human (or a higher-trust
//! source) left it. Running any ingest twice is a no-op.
//!
//! This module is the single deterministic, unit-tested implementation of
//! that merge. It owns *only* the in-memory string transform — no IO, no LLM,
//! no network. Callers (`augmentagent-channel-linkedin`, `-contacts`,
//! `-email`) decide *what* to merge and *whether* to write; this decides
//! *how* a merge changes the page text and *whether anything changed*.
//!
//! Page model:
//!
//! - **YAML frontmatter** carries the machine-readable `identities:` block
//!   ([`crate::identity::Identities`]). We append missing identity keys there
//!   so [`IdentityIndex`](crate::identity::IdentityIndex) keeps resolving.
//! - **Markdown body** carries human-facing facts under headed sections. We
//!   maintain a `## Profile` section (Role / Company / LinkedIn URL /
//!   Connected / Phone / Address / Website) and a `## Source` provenance
//!   section. A key already present with a non-empty value is never touched.
//!
//! New stub pages get a minimal frontmatter (`kind: person`) + the two
//! sections so the very next [`IdentityIndex::build`] resolves them.

use std::collections::BTreeMap;

/// One fill-blanks-only patch against a single person page.
///
/// Every field is "set only if the page has no value for it". `identities`
/// are merged into frontmatter; `profile` rows into the `## Profile` section;
/// `sources` are *appended* to `## Source` (provenance is additive — multiple
/// imports legitimately each leave a line, deduped on exact text).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonPatch {
    /// Display name — only used when creating a brand-new stub (`# <name>`).
    pub display_name: Option<String>,
    /// `platform -> id` identity pairs for the frontmatter `identities:` map.
    /// `email` is multi-valued; everything else is single-valued (matches
    /// [`crate::identity::Identities`]).
    pub identities: Vec<(String, String)>,
    /// Ordered profile rows rendered as `- **Key:** value` under `## Profile`.
    /// Order is preserved for *new* keys; existing keys keep their position.
    pub profile: Vec<(String, String)>,
    /// Provenance lines appended to `## Source` (deduped on exact text).
    pub sources: Vec<String>,
}

impl PersonPatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// Add an identity pair. `platform` is one of the keys
    /// [`crate::identity::Identities`] understands (`email`, `linkedin`,
    /// `discord`, `twitter`, `slack`, `whatsapp`, `instagram`, plus the
    /// CRM-only `phone` / `address`). Empty ids are dropped.
    pub fn identity(mut self, platform: impl Into<String>, id: impl Into<String>) -> Self {
        let id = id.into();
        if !id.trim().is_empty() {
            self.identities.push((platform.into(), id));
        }
        self
    }

    /// Add a profile row. Empty values are dropped (we never write a blank
    /// `- **Role:**` line — that would defeat fill-blanks detection).
    pub fn profile_row(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let value = value.into();
        if !value.trim().is_empty() {
            self.profile.push((key.into(), value));
        }
        self
    }

    pub fn source(mut self, line: impl Into<String>) -> Self {
        let line = line.into();
        if !line.trim().is_empty() {
            self.sources.push(line);
        }
        self
    }

    /// True if the patch carries nothing to write.
    pub fn is_empty(&self) -> bool {
        self.identities.is_empty() && self.profile.is_empty() && self.sources.is_empty()
    }
}

/// What [`merge_person_page`] changed. The boolean `changed` gates whether the
/// caller writes the file at all (no-op merges must not churn git / mtime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    /// The full new page text. Equal to the input when `!changed`.
    pub content: String,
    /// True iff at least one field/identity/source line was actually added.
    pub changed: bool,
    /// True iff there was no prior page (a stub was created).
    pub created: bool,
    /// Human-readable list of what was filled, for the dry-run JSON dump and
    /// the Discord summary card ("Role, Company, linkedin").
    pub filled: Vec<String>,
}

/// Fill-blanks-only merge of `patch` into `existing` page text.
///
/// - `existing == None` → a fresh stub is created with frontmatter +
///   `## Profile` + `## Source`.
/// - A profile key already present with a non-empty value is **left
///   untouched** (even if `patch` carries a different value — we never
///   overwrite third-party self-reported data over the human's).
/// - An identity key already present in frontmatter is left untouched;
///   `email` is union-merged (multi-valued) but never deduped-away.
/// - `## Source` lines are appended unless the exact line already exists.
///
/// Idempotent: `merge(merge(p)) == merge(p)` and the second call reports
/// `changed == false`.
pub fn merge_person_page(existing: Option<&str>, patch: &PersonPatch) -> MergeResult {
    match existing {
        None => create_stub(patch),
        Some(src) => merge_into_existing(src, patch),
    }
}

fn create_stub(patch: &PersonPatch) -> MergeResult {
    let name = patch.display_name.as_deref().unwrap_or("Unknown");

    let mut fm = String::from("---\nkind: person\n");
    if !patch.identities.is_empty() {
        fm.push_str("identities:\n");
        for (plat, id) in collapse_identities(&patch.identities) {
            if plat == "email" {
                fm.push_str("  email:\n");
                for e in id.split('\u{1f}') {
                    fm.push_str(&format!("    - {}\n", yaml_scalar(e)));
                }
            } else {
                fm.push_str(&format!("  {plat}: {}\n", yaml_scalar(&id)));
            }
        }
    }
    fm.push_str("---\n");

    let mut body = format!("\n# {name}\n\n## Profile\n\n");
    let mut filled: Vec<String> = Vec::new();
    for (k, v) in dedupe_profile(&patch.profile) {
        body.push_str(&format!("- **{k}:** {v}\n"));
        filled.push(k);
    }
    body.push_str("\n## Source\n\n");
    for s in dedupe_preserve_order(&patch.sources) {
        body.push_str(&format!("- {s}\n"));
    }
    for (plat, _) in collapse_identities(&patch.identities) {
        filled.push(plat);
    }

    MergeResult {
        content: format!("{fm}{body}"),
        changed: true,
        created: true,
        filled,
    }
}

fn merge_into_existing(src: &str, patch: &PersonPatch) -> MergeResult {
    let mut filled: Vec<String> = Vec::new();

    // 1. Frontmatter identity merge (fill-blanks; email is union).
    let (mut page, fm_filled) = merge_frontmatter_identities(src, &patch.identities);
    filled.extend(fm_filled);

    // 2. `## Profile` rows — add only keys that are absent / blank.
    let (page2, prof_filled) = merge_section_rows(&page, "Profile", &patch.profile);
    page = page2;
    filled.extend(prof_filled);

    // 3. `## Source` provenance — append unseen lines.
    let (page3, src_added) = append_source_lines(&page, &patch.sources);
    page = page3;
    if src_added {
        filled.push("source".to_string());
    }

    let changed = page != src;
    MergeResult {
        content: page,
        changed,
        created: false,
        filled,
    }
}

/// Collapse a `Vec<(platform,id)>` into ordered unique platforms. For `email`
/// we keep every distinct value joined by US (`\u{1f}`) — an in-band
/// separator that can't appear in an address — so the caller renders a YAML
/// list. All other platforms keep first-seen value (single-valued).
fn collapse_identities(pairs: &[(String, String)]) -> Vec<(String, String)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (plat, id) in pairs {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        if plat == "email" {
            let entry = map.entry(plat.clone()).or_default();
            let already = entry.split('\u{1f}').any(|e| e.eq_ignore_ascii_case(id));
            if !already {
                if entry.is_empty() {
                    *entry = id.to_string();
                } else {
                    entry.push('\u{1f}');
                    entry.push_str(id);
                }
            }
        } else {
            map.entry(plat.clone()).or_insert_with(|| id.to_string());
        }
        if !order.contains(plat) {
            order.push(plat.clone());
        }
    }
    order
        .into_iter()
        .map(|p| {
            let v = map.get(&p).cloned().unwrap_or_default();
            (p, v)
        })
        .collect()
}

/// Merge identity pairs into the YAML frontmatter `identities:` block,
/// fill-blanks-only. Returns the new page + the list of platforms newly
/// added. Single-valued platforms already present are skipped; `email`
/// gains only addresses not already listed (case-insensitive).
fn merge_frontmatter_identities(
    src: &str,
    pairs: &[(String, String)],
) -> (String, Vec<String>) {
    if pairs.is_empty() {
        return (src.to_string(), Vec::new());
    }
    let collapsed = collapse_identities(pairs);

    let Some((fm_inner, close_off)) = split_frontmatter(src) else {
        // No frontmatter — synthesize a minimal one at the very top so the
        // identity index can resolve this page. Body is preserved verbatim.
        let mut fm = String::from("---\nkind: person\nidentities:\n");
        let mut added = Vec::new();
        for (plat, id) in &collapsed {
            if plat == "email" {
                fm.push_str("  email:\n");
                for e in id.split('\u{1f}') {
                    fm.push_str(&format!("    - {}\n", yaml_scalar(e)));
                }
            } else {
                fm.push_str(&format!("  {plat}: {}\n", yaml_scalar(id)));
            }
            added.push(plat.clone());
        }
        fm.push_str("---\n");
        return (format!("{fm}{src}"), added);
    };

    let fm_inner = fm_inner.to_string();
    let mut added: Vec<String> = Vec::new();
    let mut new_lines: Vec<String> = Vec::new();

    let has_identities_key = fm_inner
        .lines()
        .any(|l| l.trim_end() == "identities:" || l.starts_with("identities:"));

    for (plat, id) in &collapsed {
        if plat == "email" {
            let existing = extract_email_values(&fm_inner);
            for e in id.split('\u{1f}') {
                if !existing.iter().any(|x| x.eq_ignore_ascii_case(e)) {
                    new_lines.push(format!("    - {}", yaml_scalar(e)));
                    if !added.contains(&"email".to_string()) {
                        added.push("email".to_string());
                    }
                }
            }
        } else if !identity_key_present(&fm_inner, plat) {
            new_lines.push(format!("  {plat}: {}", yaml_scalar(id)));
            added.push(plat.clone());
        }
    }

    if new_lines.is_empty() {
        return (src.to_string(), Vec::new());
    }

    // Splice strategy: keep existing frontmatter byte-for-byte; insert the
    // new identity lines just before the closing `---`. If there was no
    // `identities:` key we add the header line first.
    let mut insert = String::new();
    if !has_identities_key {
        insert.push_str("identities:\n");
    }
    // When appending email entries to an existing `email:` list we need the
    // `email:` parent to already exist; if it doesn't (only non-email keys
    // present) the entries we generated start with 4-space indent which is
    // still valid as a fresh list only when preceded by `  email:`.
    let needs_email_header = collapsed.iter().any(|(p, _)| p == "email")
        && !fm_inner.lines().any(|l| {
            let t = l.trim_start();
            t == "email:" || t.starts_with("email:")
        });
    if needs_email_header
        && new_lines.iter().any(|l| l.starts_with("    - "))
    {
        // Insert `  email:` header right before the first email entry.
        let first_email = new_lines
            .iter()
            .position(|l| l.starts_with("    - "))
            .unwrap();
        new_lines.insert(first_email, "  email:".to_string());
    }
    insert.push_str(&new_lines.join("\n"));
    insert.push('\n');

    let mut out = String::with_capacity(src.len() + insert.len());
    out.push_str(&src[..close_off]);
    out.push_str(&insert);
    out.push_str(&src[close_off..]);
    (out, added)
}

/// Split into `(frontmatter_inner, byte_offset_of_closing_delim)`.
/// Mirrors `migrate::split_frontmatter` but kept local so `crm` has no
/// intra-crate coupling to the migration tool.
fn split_frontmatter(page: &str) -> Option<(&str, usize)> {
    let rest = page.strip_prefix("---\n")?;
    let mut offset = 4;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        if trimmed == "---" {
            let inner = &page[4..offset];
            return Some((inner, offset));
        }
        offset += line.len();
    }
    None
}

fn identity_key_present(fm_inner: &str, key: &str) -> bool {
    let prefix = format!("{key}:");
    fm_inner.lines().any(|l| {
        let t = l.trim_start();
        t == prefix || t.starts_with(&format!("{key}: "))
    })
}

/// Pull existing `email:` list entries (`    - x@y` or inline `email: [a, b]`).
fn extract_email_values(fm_inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_email = false;
    for line in fm_inner.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("email:") {
            let rest = rest.trim();
            if rest.starts_with('[') {
                // inline flow list
                for part in rest.trim_matches(|c| c == '[' || c == ']').split(',') {
                    let v = part.trim().trim_matches('"').trim();
                    if !v.is_empty() {
                        out.push(v.to_string());
                    }
                }
                in_email = false;
            } else {
                in_email = true;
            }
            continue;
        }
        if in_email {
            if let Some(item) = trimmed.strip_prefix("- ") {
                out.push(item.trim().trim_matches('"').to_string());
            } else if !trimmed.is_empty() && !line.starts_with(' ') {
                in_email = false;
            } else if !trimmed.starts_with('-') && !trimmed.is_empty() {
                in_email = false;
            }
        }
    }
    out
}

/// YAML-quote a scalar only when it could be misparsed (`:` `#` leading `-`,
/// etc). Emails / urns / phone are safe bare in practice but we quote
/// defensively when in doubt.
fn yaml_scalar(s: &str) -> String {
    let needs = s.is_empty()
        || s.contains(": ")
        || s.contains(" #")
        // Leading `+` / `-` (phone numbers) — quoted so no YAML parser ever
        // coerces `+14155550100` toward a number-ish scalar.
        || s.starts_with(['-', '+', '?', '*', '&', '!', '@', '`', '"', '\'', '[', '{', '|', '>'])
        || s.contains('\n');
    if needs {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Ensure a `## <heading>` section exists and contains a `- **Key:** value`
/// row for every key in `rows` that is *not already present with a non-empty
/// value*. Returns the new page + the list of keys actually added.
fn merge_section_rows(
    page: &str,
    heading: &str,
    rows: &[(String, String)],
) -> (String, Vec<String>) {
    if rows.is_empty() {
        return (page.to_string(), Vec::new());
    }
    let rows = dedupe_profile(rows);

    let header = format!("## {heading}");
    let mut added: Vec<String> = Vec::new();

    // Locate the section span: from the heading line to the next `## ` (or EOF).
    let lines: Vec<&str> = page.lines().collect();
    let sec_start = lines.iter().position(|l| l.trim_end() == header);

    let existing_keys: Vec<String> = if let Some(start) = sec_start {
        let mut keys = Vec::new();
        for l in &lines[start + 1..] {
            if l.starts_with("## ") {
                break;
            }
            if let Some(k) = parse_row_key(l) {
                keys.push(k.to_ascii_lowercase());
            }
        }
        keys
    } else {
        Vec::new()
    };

    let mut to_add: Vec<String> = Vec::new();
    for (k, v) in &rows {
        if !existing_keys.contains(&k.to_ascii_lowercase()) {
            to_add.push(format!("- **{k}:** {v}"));
            added.push(k.clone());
        }
    }
    if to_add.is_empty() {
        return (page.to_string(), Vec::new());
    }

    let new_page = match sec_start {
        Some(start) => {
            // Insert after the last existing row in the section (before the
            // blank line that precedes the next heading, or section end).
            let mut insert_at = start + 1;
            for (i, l) in lines.iter().enumerate().skip(start + 1) {
                if l.starts_with("## ") {
                    break;
                }
                if parse_row_key(l).is_some() {
                    insert_at = i + 1;
                }
            }
            let mut out: Vec<String> = lines[..insert_at].iter().map(|s| s.to_string()).collect();
            out.extend(to_add.iter().cloned());
            out.extend(lines[insert_at..].iter().map(|s| s.to_string()));
            join_preserving_trailing_nl(page, &out)
        }
        None => {
            // Append a fresh section at the end.
            let mut base = page.trim_end().to_string();
            base.push_str(&format!("\n\n{header}\n\n"));
            base.push_str(&to_add.join("\n"));
            base.push('\n');
            base
        }
    };
    (new_page, added)
}

/// Append every line in `sources` to a `## Source` section unless that exact
/// rendered bullet already exists anywhere in the page.
fn append_source_lines(page: &str, sources: &[String]) -> (String, bool) {
    if sources.is_empty() {
        return (page.to_string(), false);
    }
    let wanted = dedupe_preserve_order(sources);
    let to_add: Vec<String> = wanted
        .into_iter()
        .filter(|s| {
            let bullet = format!("- {s}");
            !page.lines().any(|l| l.trim_end() == bullet)
        })
        .collect();
    if to_add.is_empty() {
        return (page.to_string(), false);
    }

    let header = "## Source";
    let lines: Vec<&str> = page.lines().collect();
    let sec_start = lines.iter().position(|l| l.trim_end() == header);

    let new_page = match sec_start {
        Some(start) => {
            let mut insert_at = start + 1;
            for (i, l) in lines.iter().enumerate().skip(start + 1) {
                if l.starts_with("## ") {
                    break;
                }
                if l.trim_start().starts_with("- ") {
                    insert_at = i + 1;
                }
            }
            let mut out: Vec<String> =
                lines[..insert_at].iter().map(|s| s.to_string()).collect();
            for s in &to_add {
                out.push(format!("- {s}"));
            }
            out.extend(lines[insert_at..].iter().map(|s| s.to_string()));
            join_preserving_trailing_nl(page, &out)
        }
        None => {
            let mut base = page.trim_end().to_string();
            base.push_str("\n\n## Source\n\n");
            for s in &to_add {
                base.push_str(&format!("- {s}\n"));
            }
            base
        }
    };
    (new_page, true)
}

fn parse_row_key(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let rest = t.strip_prefix("- **")?;
    let end = rest.find(":**")?;
    Some(rest[..end].trim())
}

fn dedupe_profile(rows: &[(String, String)]) -> Vec<(String, String)> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (k, v) in rows {
        let lk = k.to_ascii_lowercase();
        if v.trim().is_empty() || seen.contains(&lk) {
            continue;
        }
        seen.push(lk);
        out.push((k.clone(), v.trim().to_string()));
    }
    out
}

fn dedupe_preserve_order(items: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for s in items {
        let s = s.trim().to_string();
        if s.is_empty() || seen.contains(&s) {
            continue;
        }
        seen.push(s.clone());
        out.push(s);
    }
    out
}

/// Re-join `lines` while restoring the original page's trailing-newline state
/// (`str::lines()` drops it).
fn join_preserving_trailing_nl(original: &str, lines: &[String]) -> String {
    let mut s = lines.join("\n");
    if original.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_stub_with_sections_and_identities() {
        let patch = PersonPatch::new()
            .with_display_name("Jane Doe")
            .identity("linkedin", "jane-doe-123")
            .profile_row("Role", "Staff Engineer")
            .profile_row("Company", "Acme")
            .source("Imported from LinkedIn 1st-degree connections on 2026-05-18");
        let r = merge_person_page(None, &patch);
        assert!(r.created && r.changed);
        assert!(r.content.starts_with("---\nkind: person\n"));
        assert!(r.content.contains("linkedin: jane-doe-123"));
        assert!(r.content.contains("# Jane Doe"));
        assert!(r.content.contains("## Profile"));
        assert!(r.content.contains("- **Role:** Staff Engineer"));
        assert!(r.content.contains("- **Company:** Acme"));
        assert!(r.content.contains("## Source"));
        assert!(r
            .content
            .contains("- Imported from LinkedIn 1st-degree connections on 2026-05-18"));
    }

    #[test]
    fn idempotent_second_merge_is_noop() {
        let patch = PersonPatch::new()
            .with_display_name("Jane")
            .identity("linkedin", "jane-1")
            .profile_row("Role", "Eng")
            .source("Imported from LinkedIn on 2026-05-18");
        let first = merge_person_page(None, &patch);
        let second = merge_person_page(Some(&first.content), &patch);
        assert!(!second.changed, "second merge must be a no-op");
        assert_eq!(second.content, first.content);
        assert!(second.filled.is_empty());
    }

    #[test]
    fn never_overwrites_existing_profile_value() {
        let existing = "---\nkind: person\n---\n\n# Jane\n\n## Profile\n\n- **Role:** Founder\n\n## Source\n\n";
        let patch = PersonPatch::new().profile_row("Role", "Intern");
        let r = merge_person_page(Some(existing), &patch);
        assert!(!r.changed);
        assert!(r.content.contains("- **Role:** Founder"));
        assert!(!r.content.contains("Intern"));
    }

    #[test]
    fn fills_only_missing_profile_row() {
        let existing = "---\nkind: person\n---\n\n# Jane\n\n## Profile\n\n- **Role:** Founder\n";
        let patch = PersonPatch::new()
            .profile_row("Role", "Intern")
            .profile_row("Company", "Acme");
        let r = merge_person_page(Some(existing), &patch);
        assert!(r.changed);
        assert!(r.content.contains("- **Role:** Founder"));
        assert!(r.content.contains("- **Company:** Acme"));
        assert_eq!(r.filled, vec!["Company".to_string()]);
    }

    #[test]
    fn frontmatter_identity_fill_blanks_only() {
        let existing = "---\nkind: person\nidentities:\n  linkedin: old-handle\n---\n\n# Jane\n";
        let patch = PersonPatch::new()
            .identity("linkedin", "new-handle")
            .identity("phone", "+14155550100");
        let r = merge_person_page(Some(existing), &patch);
        assert!(r.changed);
        // linkedin already present → untouched.
        assert!(r.content.contains("linkedin: old-handle"));
        assert!(!r.content.contains("new-handle"));
        // phone was blank → added.
        assert!(r.content.contains("phone: \"+14155550100\""));
    }

    #[test]
    fn email_identity_is_union_merged() {
        let existing = "---\nkind: person\nidentities:\n  email:\n    - a@x.com\n---\n\n# Jane\n";
        let patch = PersonPatch::new()
            .identity("email", "a@x.com")
            .identity("email", "b@y.com");
        let r = merge_person_page(Some(existing), &patch);
        assert!(r.changed);
        assert!(r.content.contains("- a@x.com"));
        assert!(r.content.contains("- b@y.com"));
        // a@x.com not duplicated.
        assert_eq!(r.content.matches("a@x.com").count(), 1);
    }

    #[test]
    fn synthesizes_frontmatter_when_absent() {
        let existing = "# Jane\n\nSome notes.\n";
        let patch = PersonPatch::new().identity("linkedin", "jane-9");
        let r = merge_person_page(Some(existing), &patch);
        assert!(r.changed);
        assert!(r.content.starts_with("---\nkind: person\nidentities:\n"));
        assert!(r.content.contains("linkedin: jane-9"));
        assert!(r.content.contains("Some notes."));
    }

    #[test]
    fn appends_source_lines_dedup() {
        let existing = "---\nkind: person\n---\n\n# Jane\n\n## Source\n\n- Imported from LinkedIn on 2026-01-01\n";
        let patch = PersonPatch::new()
            .source("Imported from LinkedIn on 2026-01-01")
            .source("Imported from Google Contacts on 2026-05-18");
        let r = merge_person_page(Some(existing), &patch);
        assert!(r.changed);
        assert_eq!(
            r.content
                .matches("Imported from LinkedIn on 2026-01-01")
                .count(),
            1
        );
        assert!(r
            .content
            .contains("- Imported from Google Contacts on 2026-05-18"));
    }

    #[test]
    fn creates_profile_section_when_missing() {
        let existing = "---\nkind: person\n---\n\n# Jane\n\nFreeform note.\n";
        let patch = PersonPatch::new().profile_row("Phone", "+14155550100");
        let r = merge_person_page(Some(existing), &patch);
        assert!(r.changed);
        assert!(r.content.contains("## Profile"));
        assert!(r.content.contains("- **Phone:** +14155550100"));
        assert!(r.content.contains("Freeform note."));
    }

    #[test]
    fn empty_patch_never_changes() {
        let existing = "---\nkind: person\n---\n\n# Jane\n";
        let r = merge_person_page(Some(existing), &PersonPatch::new());
        assert!(!r.changed);
        assert_eq!(r.content, existing);
    }

    #[test]
    fn identity_index_resolves_created_stub() {
        use crate::identity::IdentityIndex;
        use crate::layout::WikiLayout;
        let dir = tempfile::TempDir::new().unwrap();
        let layout = WikiLayout::new(dir.path().to_path_buf());
        std::fs::create_dir_all(layout.people_dir()).unwrap();
        let patch = PersonPatch::new()
            .with_display_name("Jane Doe")
            .identity("linkedin", "jane-doe-xyz");
        let r = merge_person_page(None, &patch);
        std::fs::write(layout.people_dir().join("jane.md"), &r.content).unwrap();
        let idx = IdentityIndex::build(&layout).unwrap();
        assert!(idx.lookup("linkedin", "jane-doe-xyz").is_some());
    }
}
