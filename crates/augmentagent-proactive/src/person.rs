//! Read-only parser for `wiki/people/<slug>.md` pages.
//!
//! The proactive rules need a handful of CRM facts off each page: the
//! user-set `cadence`, the `events:` ledger, the page's last-touched date,
//! and the free-text `## Commitments` section. We parse only what we need and
//! tolerate everything else — a v1 page with none of the v2 frontmatter is
//! still a valid input (every field defaults).
//!
//! Per `schema/wiki-skill.md`, `cadence` is a USER-SET named bucket
//! (`weekly | bi-weekly | monthly | quarterly | ad-hoc`). We also accept a
//! numeric `cadence_days:` escape hatch and fall back to a default when the
//! field is absent (the issue's "default if missing" requirement).

use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::Deserialize;

/// Default contact cadence when a page declares none. ~Quarterly.
pub const DEFAULT_CADENCE_DAYS: i64 = 90;

#[derive(Debug, Clone, Default, Deserialize)]
struct RawFrontMatter {
    #[serde(default)]
    cadence: Option<String>,
    #[serde(default)]
    cadence_days: Option<i64>,
    #[serde(default)]
    updated: Option<String>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    events: Vec<RawEvent>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawEvent {
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// One life-event ledger entry, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifeEvent {
    pub date: NaiveDate,
    pub kind: String,
}

/// One parsed person page.
#[derive(Debug, Clone)]
pub struct PersonPage {
    pub slug: String,
    pub path: PathBuf,
    pub name: Option<String>,
    pub cadence_days: i64,
    pub cadence_explicit: bool,
    pub last_touch: Option<NaiveDate>,
    pub events: Vec<LifeEvent>,
    pub commitments_block: Option<String>,
}

impl PersonPage {
    pub fn display_name(&self) -> String {
        if let Some(n) = &self.name {
            if !n.trim().is_empty() {
                return n.clone();
            }
        }
        deslug(&self.slug)
    }
}

fn cadence_to_days(name: &str) -> Option<i64> {
    match name.trim().to_ascii_lowercase().as_str() {
        "daily" => Some(1),
        "weekly" => Some(7),
        "bi-weekly" | "biweekly" | "fortnightly" => Some(14),
        "monthly" => Some(30),
        "quarterly" => Some(90),
        "ad-hoc" | "adhoc" | "none" => None,
        _ => None,
    }
}

/// Parse one `people/<slug>.md`. `None` only if unreadable.
pub fn parse_person_page(path: &Path) -> Option<PersonPage> {
    let raw = fs::read_to_string(path).ok()?;
    let slug = path.file_stem()?.to_string_lossy().into_owned();

    let fm: RawFrontMatter = extract_yaml_block(&raw)
        .and_then(|y| serde_yaml_ng::from_str(y).ok())
        .unwrap_or_default();

    let (cadence_days, cadence_explicit) = match (fm.cadence_days, fm.cadence.as_deref()) {
        (Some(d), _) if d > 0 => (d, true),
        (_, Some(name)) => match cadence_to_days(name) {
            Some(d) => (d, true),
            None => (DEFAULT_CADENCE_DAYS, false),
        },
        _ => (DEFAULT_CADENCE_DAYS, false),
    };

    let last_touch = fm
        .updated
        .as_deref()
        .or(fm.created.as_deref())
        .and_then(parse_date);

    let events = fm
        .events
        .iter()
        .filter_map(|e| {
            let date = e.date.as_deref().and_then(parse_date)?;
            let kind = e.kind.clone().unwrap_or_else(|| "other".into());
            Some(LifeEvent { date, kind })
        })
        .collect();

    let commitments_block = extract_section(&raw, "Commitments");

    Some(PersonPage {
        slug,
        path: path.to_path_buf(),
        name: fm.name,
        cadence_days,
        cadence_explicit,
        last_touch,
        events,
        commitments_block,
    })
}

/// Walk `people/` and parse every `.md` page. Missing dir => empty vec.
pub fn parse_people_dir(people_dir: &Path) -> Vec<PersonPage> {
    let Ok(rd) = fs::read_dir(people_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if let Some(p) = parse_person_page(&path) {
            out.push(p);
        }
    }
    out
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y/%m/%d"))
        .ok()
}

fn deslug(slug: &str) -> String {
    slug.replace('_', " ")
}

fn extract_yaml_block(src: &str) -> Option<&str> {
    let after_open = src
        .strip_prefix("---\n")
        .or_else(|| src.strip_prefix("---\r\n"))?;
    let mut offset = 0usize;
    for line in after_open.lines() {
        if line.trim_end() == "---" {
            return Some(&after_open[..offset]);
        }
        offset += line.len() + 1;
    }
    None
}

/// Return the body under a `## <heading>` section up to the next `## `.
fn extract_section(src: &str, heading: &str) -> Option<String> {
    let want = heading.trim().to_ascii_lowercase();
    let mut lines = src.lines();
    let mut found = false;
    let mut body: Vec<&str> = Vec::new();
    for line in lines.by_ref() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("## ") {
            if found {
                break;
            }
            if rest.trim().to_ascii_lowercase() == want {
                found = true;
                continue;
            }
        }
        if found {
            body.push(line);
        }
    }
    if !found {
        return None;
    }
    let joined = body.join("\n");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write(dir: &Path, slug: &str, body: &str) -> PathBuf {
        let p = dir.join(format!("{slug}.md"));
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn named_cadence_maps_to_days() {
        let d = TempDir::new().unwrap();
        let p = write(
            d.path(),
            "jane_at_corp_com",
            "---\nname: Jane Corp\ncadence: monthly\nupdated: 2026-01-01\n---\n\n# Jane\n",
        );
        let pp = parse_person_page(&p).unwrap();
        assert_eq!(pp.cadence_days, 30);
        assert!(pp.cadence_explicit);
        assert_eq!(pp.display_name(), "Jane Corp");
        assert_eq!(pp.last_touch, NaiveDate::from_ymd_opt(2026, 1, 1));
    }

    #[test]
    fn missing_cadence_defaults() {
        let d = TempDir::new().unwrap();
        let p = write(d.path(), "bob", "---\nkind: person\n---\n\n# Bob\n");
        let pp = parse_person_page(&p).unwrap();
        assert_eq!(pp.cadence_days, DEFAULT_CADENCE_DAYS);
        assert!(!pp.cadence_explicit);
        assert_eq!(pp.display_name(), "bob");
    }

    #[test]
    fn numeric_override_wins() {
        let d = TempDir::new().unwrap();
        let p = write(
            d.path(),
            "x",
            "---\ncadence: quarterly\ncadence_days: 10\n---\n\n# X\n",
        );
        let pp = parse_person_page(&p).unwrap();
        assert_eq!(pp.cadence_days, 10);
        assert!(pp.cadence_explicit);
    }

    #[test]
    fn adhoc_is_non_explicit() {
        let d = TempDir::new().unwrap();
        let p = write(d.path(), "y", "---\ncadence: ad-hoc\n---\n\n# Y\n");
        let pp = parse_person_page(&p).unwrap();
        assert!(!pp.cadence_explicit);
    }

    #[test]
    fn parses_events_ledger() {
        let d = TempDir::new().unwrap();
        let p = write(
            d.path(),
            "z",
            "---\nevents:\n  - date: 2026-03-15\n    kind: birthday\n  - date: 2025-11-04\n    kind: new_job\n---\n\n# Z\n",
        );
        let pp = parse_person_page(&p).unwrap();
        assert_eq!(pp.events.len(), 2);
        assert!(pp
            .events
            .iter()
            .any(|e| e.kind == "birthday"
                && e.date == NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()));
    }

    #[test]
    fn extracts_commitments_section() {
        let d = TempDir::new().unwrap();
        let p = write(
            d.path(),
            "c",
            "---\nname: C\n---\n\n# C\n\n## Identity\nfoo\n\n## Commitments\n- [ ] send deck (due: 2026-01-10)\n- [x] intro to Sam\n\n## Tone\nwarm\n",
        );
        let pp = parse_person_page(&p).unwrap();
        let block = pp.commitments_block.unwrap();
        assert!(block.contains("send deck"));
        assert!(block.contains("intro to Sam"));
        assert!(!block.contains("warm"));
        assert!(!block.contains("foo"));
    }

    #[test]
    fn no_frontmatter_is_tolerated() {
        let d = TempDir::new().unwrap();
        let p = write(d.path(), "raw", "# Just a heading\n\nsome text\n");
        let pp = parse_person_page(&p).unwrap();
        assert_eq!(pp.cadence_days, DEFAULT_CADENCE_DAYS);
        assert!(pp.events.is_empty());
        assert!(pp.commitments_block.is_none());
    }

    #[test]
    fn parse_people_dir_skips_non_md() {
        let d = TempDir::new().unwrap();
        write(d.path(), "a", "---\nname: A\n---\n# A\n");
        fs::write(d.path().join("notes.txt"), "ignore me").unwrap();
        let people = parse_people_dir(d.path());
        assert_eq!(people.len(), 1);
    }

    #[test]
    fn missing_people_dir_is_empty() {
        let d = TempDir::new().unwrap();
        let missing = d.path().join("nope");
        assert!(parse_people_dir(&missing).is_empty());
    }
}
