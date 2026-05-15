//! v2 schema migration helpers for `wiki/people/*.md`.
//!
//! The CLI tool (`augmentagent wiki migrate --to v2`) does the orchestration
//! — concurrency, git commits, model dispatch. This module owns the
//! per-page deterministic plumbing that's worth unit-testing in isolation:
//!
//! 1. **Detection** — split a page into frontmatter + body, decide whether
//!    it's already migrated (presence of any v2 field, or an explicit
//!    `migrated:` marker).
//! 2. **Patch parsing** — accept the model's text response in a few shapes
//!    (raw YAML, fenced ``` ```yaml ``` ``` block, or `---` delimited) and
//!    decode it as a `serde_yaml_ng::Value` map.
//! 3. **Citation validation** — for every claim that names a
//!    `source_message_id`, verify that ID appears in the page's existing
//!    `sources:` list. Drop uncited claims; count them so the caller can
//!    surface the number to stderr.
//! 4. **Patch application** — splice the new keys into the existing
//!    frontmatter block as raw YAML text, *byte-for-byte preserving* the
//!    original keys. The spec (§2 step 6 + §8 risk register) explicitly
//!    forbids round-tripping the whole frontmatter, which would reorder.
//!
//! The module is reasoner-agnostic: callers pass in the model output as a
//! `&str`. Tests use canned responses; production calls Haiku.
//!
//! Today only person pages are in scope; threads/projects don't have v2
//! fields per #78 §2.

use std::collections::BTreeSet;

use anyhow::{anyhow, Context, Result};
use serde_yaml_ng::Value;

/// v2 frontmatter keys whose presence makes a page "already-v2" for the
/// purpose of skipping during migration. The `migrated:` marker is its own
/// signal handled separately.
pub const V2_FIELDS: &[&str] = &[
    "affiliations",
    "events",
    "introduced_by",
    "topics",
    "cadence",
    "trust",
    "strength",
];

/// Outcome of inspecting a page before deciding whether to call the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationDecision {
    /// Page has no v2 fields and no `migrated:` marker — eligible for migration.
    Eligible,
    /// Page already has at least one v2 field populated; skip without spend.
    AlreadyV2,
    /// Page carries a `migrated: <date>` marker from a prior run; skip.
    AlreadyMigrated,
    /// Frontmatter could not be located (no leading `---` delimiters). Caller
    /// should record this and move on; we do not try to repair v1 garbage.
    NoFrontmatter,
}

/// Split a page into `(frontmatter_inner, body_with_closing_delim_offset)`.
///
/// `frontmatter_inner` is the text between the opening `---\n` and the
/// closing `\n---` (delimiters NOT included). `body_offset` is the byte
/// offset of the opening `\n---` of the closing delimiter — that's the
/// splice point we'll insert new keys before.
///
/// Returns `None` if the page doesn't start with `---\n` or has no closing
/// `---` on its own line. We deliberately do NOT support Windows line
/// endings — the wiki is Linux-only and ingest writes Unix newlines.
pub fn split_frontmatter(page: &str) -> Option<(&str, usize)> {
    let rest = page.strip_prefix("---\n")?;
    let mut offset = 4; // length of "---\n"
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

/// Inspect frontmatter text and decide whether the page should be migrated.
pub fn classify(page: &str) -> MigrationDecision {
    let Some((fm, _)) = split_frontmatter(page) else {
        return MigrationDecision::NoFrontmatter;
    };

    if has_top_level_key(fm, "migrated") {
        return MigrationDecision::AlreadyMigrated;
    }
    for k in V2_FIELDS {
        if has_top_level_key(fm, k) {
            return MigrationDecision::AlreadyV2;
        }
    }
    MigrationDecision::Eligible
}

/// Return `true` if `fm` contains `<key>:` at the start of a line.
/// Used by `classify` to detect already-migrated pages without paying a YAML
/// parse. Requires the colon and either EOL or whitespace after it so
/// `eventsource:` doesn't false-positive for `events`.
fn has_top_level_key(fm: &str, key: &str) -> bool {
    for line in fm.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(after) = rest.strip_prefix(':') {
                if after.is_empty() || after.starts_with(|c: char| c.is_whitespace()) {
                    return true;
                }
            }
        }
    }
    false
}

/// Parse the model's response into a YAML map. Tolerates three shapes:
/// raw YAML, a fenced ` ```yaml … ``` ` block, or a `---`-delimited block.
pub fn parse_patch(response: &str) -> Result<serde_yaml_ng::Mapping> {
    let inner = extract_yaml_block(response.trim());
    let value: Value = serde_yaml_ng::from_str(inner)
        .with_context(|| format!("parse model patch as YAML: {inner:?}"))?;
    match value {
        Value::Mapping(m) => Ok(m),
        Value::Null => Ok(serde_yaml_ng::Mapping::new()),
        other => Err(anyhow!(
            "model patch must be a YAML mapping, got: {:?}",
            other
        )),
    }
}

/// Strip a fenced or delimited wrapper around the YAML payload, if present.
fn extract_yaml_block(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("```yaml") {
        let rest = rest.trim_start_matches('\n');
        if let Some(idx) = rest.rfind("```") {
            return rest[..idx].trim_end();
        }
    }
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.trim_start_matches('\n');
        if let Some(idx) = rest.rfind("```") {
            return rest[..idx].trim_end();
        }
    }
    if let Some(rest) = s.strip_prefix("---\n") {
        if let Some(idx) = rest.rfind("\n---") {
            return &rest[..idx];
        }
    }
    s
}

/// Extract the `sources:` list from a frontmatter inner block. Returns an
/// empty set if absent or malformed — defensive: a malformed `sources:` line
/// shouldn't crash the migration of a page whose v2 patch doesn't cite any
/// IDs anyway.
pub fn parse_sources(fm: &str) -> BTreeSet<String> {
    let value: Value = match serde_yaml_ng::from_str(fm) {
        Ok(v) => v,
        Err(_) => return BTreeSet::new(),
    };
    let Some(map) = value.as_mapping() else {
        return BTreeSet::new();
    };
    let Some(sources) = map.get(Value::String("sources".into())) else {
        return BTreeSet::new();
    };
    let Some(seq) = sources.as_sequence() else {
        return BTreeSet::new();
    };
    seq.iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

/// Outcome of citation-filtering a parsed patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationFilter {
    /// Patch with uncited claims removed.
    pub filtered: serde_yaml_ng::Mapping,
    /// Number of individual claims that named a source_message_id absent
    /// from the page's `sources:` list and were dropped. Surfaced to the
    /// user via stderr so the quality bar (§6 of #78) can be evaluated.
    pub dropped: usize,
}

/// Walk the patch map and drop any list entry that names a
/// `source_message_id` (or `messageId`) absent from `allowed`. Whole keys
/// that have no allowed entries left are dropped entirely.
///
/// Conservative about *which* fields get cited: only `events`,
/// `affiliations`, and `introduced_by` are written by ingest per the
/// schema. `topics`, `cadence`, `trust`, `strength` are user/derived
/// fields and shouldn't show up in a Haiku patch — if they do, drop
/// silently as out-of-scope (they'd be hallucinations).
pub fn validate_citations(
    patch: serde_yaml_ng::Mapping,
    allowed: &BTreeSet<String>,
) -> CitationFilter {
    let mut out = serde_yaml_ng::Mapping::new();
    let mut dropped: usize = 0;

    for (k, v) in patch {
        let Some(key) = k.as_str() else {
            dropped += 1;
            continue;
        };
        match key {
            "cadence" | "trust" | "topics" | "strength" => {
                dropped += 1;
                continue;
            }
            "events" | "affiliations" => {
                let Some(seq) = v.as_sequence() else {
                    dropped += 1;
                    continue;
                };
                let mut kept: Vec<Value> = Vec::new();
                for entry in seq {
                    if entry_is_cited(entry, allowed) {
                        kept.push(entry.clone());
                    } else {
                        dropped += 1;
                    }
                }
                if !kept.is_empty() {
                    out.insert(Value::String(key.into()), Value::Sequence(kept));
                }
            }
            "introduced_by" => {
                out.insert(Value::String(key.into()), v);
            }
            _ => {
                dropped += 1;
            }
        }
    }

    CitationFilter {
        filtered: out,
        dropped,
    }
}

fn entry_is_cited(entry: &Value, allowed: &BTreeSet<String>) -> bool {
    let Some(map) = entry.as_mapping() else {
        return false;
    };
    for key in &["source_message_id", "messageId"] {
        if let Some(v) = map.get(Value::String((*key).into())) {
            if let Some(s) = v.as_str() {
                if allowed.contains(s) {
                    return true;
                }
            }
        }
    }
    false
}

/// Render a YAML mapping as line-by-line frontmatter additions to splice
/// in, plus the `migrated:` marker. We do NOT use `serde_yaml_ng::to_string`
/// on the existing frontmatter — only on these new keys — so v1 content
/// stays untouched byte-for-byte.
pub fn render_patch_lines(patch: &serde_yaml_ng::Mapping, today_iso: &str) -> Result<String> {
    let mut out = String::new();
    if !patch.is_empty() {
        let serialized = serde_yaml_ng::to_string(&Value::Mapping(patch.clone()))
            .context("serialize migration patch")?;
        out.push_str(serialized.trim_end_matches('\n'));
        out.push('\n');
    }
    out.push_str(&format!("migrated: {today_iso}\n"));
    Ok(out)
}

/// Apply a rendered patch to a page's full text. Inserts the patch lines
/// immediately before the closing `---` of the frontmatter block.
///
/// Returns the new full page text. Errors if the page has no frontmatter
/// (caller should have checked with `classify` first).
pub fn apply_patch(page: &str, rendered_patch: &str) -> Result<String> {
    let Some((_, splice_at)) = split_frontmatter(page) else {
        return Err(anyhow!("page has no frontmatter; cannot apply patch"));
    };

    let mut out = String::with_capacity(page.len() + rendered_patch.len());
    out.push_str(&page[..splice_at]);
    out.push_str(rendered_patch);
    out.push_str(&page[splice_at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_PAGE: &str = "---\nkind: person\nkey: alice\ncreated: 2026-01-01\nupdated: 2026-05-13\nsources: [abc123, def456]\n---\n\n## Identity\n\nAlice does things.\n";

    const V2_PAGE: &str = "---\nkind: person\nkey: bob\nsources: [abc123]\naffiliations:\n  - org: anthropic\n---\n\n## Identity\n\nBob.\n";

    const ALREADY_MIGRATED_PAGE: &str = "---\nkind: person\nkey: carol\nsources: [abc123]\nmigrated: 2026-05-14\n---\n\n## Identity\n\nCarol.\n";

    const NO_FM_PAGE: &str = "## Identity\n\nDan has no frontmatter.\n";

    #[test]
    fn split_frontmatter_finds_inner_and_closing_offset() {
        let (inner, offset) = split_frontmatter(V1_PAGE).unwrap();
        assert!(inner.contains("kind: person"));
        assert!(inner.ends_with('\n'));
        assert!(V1_PAGE[offset..].starts_with("---\n"));
    }

    #[test]
    fn split_frontmatter_rejects_missing_delimiters() {
        assert!(split_frontmatter(NO_FM_PAGE).is_none());
        assert!(split_frontmatter("---\nno closing\n").is_none());
    }

    #[test]
    fn classify_detects_v1_v2_migrated_and_garbage() {
        assert_eq!(classify(V1_PAGE), MigrationDecision::Eligible);
        assert_eq!(classify(V2_PAGE), MigrationDecision::AlreadyV2);
        assert_eq!(
            classify(ALREADY_MIGRATED_PAGE),
            MigrationDecision::AlreadyMigrated
        );
        assert_eq!(classify(NO_FM_PAGE), MigrationDecision::NoFrontmatter);
    }

    #[test]
    fn classify_does_not_false_positive_on_substring_keys() {
        let page = "---\nkind: person\nkey: x\neventsource: foo\n---\n\nbody\n";
        assert_eq!(classify(page), MigrationDecision::Eligible);
    }

    #[test]
    fn parse_sources_extracts_message_ids() {
        let (fm, _) = split_frontmatter(V1_PAGE).unwrap();
        let s = parse_sources(fm);
        assert!(s.contains("abc123"));
        assert!(s.contains("def456"));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn parse_patch_handles_raw_fenced_and_delimited() {
        let raw = "events:\n  - kind: birthday\n";
        let fenced = "```yaml\nevents:\n  - kind: birthday\n```";
        let delim = "---\nevents:\n  - kind: birthday\n---";
        for s in [raw, fenced, delim] {
            let m = parse_patch(s).unwrap();
            assert!(m.contains_key(Value::String("events".into())));
        }
    }

    #[test]
    fn validate_citations_drops_uncited_events_and_unknown_keys() {
        let patch_yaml = "\
events:
  - kind: birthday
    source_message_id: abc123
  - kind: new_job
    source_message_id: ghost999
affiliations:
  - org: anthropic
    role: PM
    source_message_id: abc123
topics: [ai-agents]
cadence: weekly
introduced_by: sarah-chen
random_extra: foo
";
        let patch = parse_patch(patch_yaml).unwrap();
        let mut allowed = BTreeSet::new();
        allowed.insert("abc123".to_string());

        let res = validate_citations(patch, &allowed);
        let events = res
            .filtered
            .get(Value::String("events".into()))
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(res
            .filtered
            .contains_key(Value::String("affiliations".into())));
        assert!(!res.filtered.contains_key(Value::String("topics".into())));
        assert!(!res.filtered.contains_key(Value::String("cadence".into())));
        assert!(!res
            .filtered
            .contains_key(Value::String("random_extra".into())));
        assert!(res
            .filtered
            .contains_key(Value::String("introduced_by".into())));
        // Dropped count: ghost event + topics + cadence + random_extra = 4.
        assert_eq!(res.dropped, 4);
    }

    #[test]
    fn apply_patch_preserves_v1_content_byte_for_byte() {
        let patch_yaml = "events:\n  - kind: birthday\n    source_message_id: abc123\n";
        let patch = parse_patch(patch_yaml).unwrap();
        let mut allowed = BTreeSet::new();
        allowed.insert("abc123".to_string());
        let filtered = validate_citations(patch, &allowed).filtered;
        let rendered = render_patch_lines(&filtered, "2026-05-15").unwrap();
        let out = apply_patch(V1_PAGE, &rendered).unwrap();

        assert!(out.contains("kind: person\nkey: alice"));
        assert!(out.contains("sources: [abc123, def456]"));
        assert!(out.contains("## Identity\n\nAlice does things."));
        assert!(out.contains("events:"));
        assert!(out.contains("migrated: 2026-05-15"));
        let close_idx = out.find("\n---\n\n## Identity").unwrap();
        let migrated_idx = out.find("migrated: 2026-05-15").unwrap();
        assert!(migrated_idx < close_idx);
    }

    #[test]
    fn apply_patch_with_only_marker_still_inserts() {
        let empty_patch = serde_yaml_ng::Mapping::new();
        let rendered = render_patch_lines(&empty_patch, "2026-05-15").unwrap();
        let out = apply_patch(V1_PAGE, &rendered).unwrap();
        assert!(out.contains("migrated: 2026-05-15"));
        assert_eq!(classify(&out), MigrationDecision::AlreadyMigrated);
    }
}
