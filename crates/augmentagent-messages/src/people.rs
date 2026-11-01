//! Handle → person resolution through the wiki identity index (#1101).
//!
//! A person is many handles: a phone in texts, a JID in one chat app, a user
//! id in another, several email addresses. Person pages already list them
//! (`identities:` front matter). This module mirrors that mapping into a
//! derived cache (`message_people`) keyed by the same canonical handles the
//! message index uses, so `with:<person>` can reach every platform.
//!
//! The cache is fully rebuildable from the wiki. A handle claimed by two
//! pages maps to neither: a stale cache is acceptable, a wrong merge is not.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use augmentagent_store::rusqlite::{params, Connection, OptionalExtension};
use augmentagent_store::Store;
use augmentagent_wiki::{Identities, IdentityIndex, WikiLayout};
use serde::Serialize;

use crate::handles;

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct PeopleReport {
    pub pages: usize,
    pub people_with_handles: usize,
    pub handles: usize,
    pub names: usize,
    /// Handles claimed by more than one page (mapped to nobody).
    pub conflicts: usize,
    /// Handles claimed by one named page plus id-only stub pages (e.g. a
    /// `16105551234.md` created before the person was named): mapped to the
    /// named page, the same resolution the identity-merge flow proposes.
    pub stub_duplicates_resolved: usize,
    /// `aliases:` names collected across pages (#1539).
    pub aliases: usize,
    /// `household:` links kept, counting each direction once (#1539).
    pub links: usize,
    /// `household:` entries naming the page itself or a slug with no page.
    pub dangling_links: usize,
    #[serde(skip)]
    pub conflict_handles: Vec<(String, Vec<String>)>,
}

/// Everything the cache needs, computed from the wiki without touching the store.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PeopleSnapshot {
    pub handles: BTreeMap<String, String>,
    /// `(person_key, normalized name)` from the page slug and its title.
    /// Deliberately alias-free: the Slack send path resolves recipients from
    /// this set and must keep naming exactly the people it named before.
    pub names: BTreeSet<(String, String)>,
    /// `(person_key, normalized alias)` from `aliases:` front matter (#1539).
    /// Searchable like a name, but only through the message-search cache.
    pub alias_names: BTreeSet<(String, String)>,
    /// `(person_key, other_key)` household pairs (#1539), both directions.
    pub links: BTreeSet<(String, String)>,
    pub report: PeopleReport,
}

/// Lowercase, alphanumeric words separated by single spaces.
pub fn normalize_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Slugs like `16105551234`, `whatsapp-1203…`, `phone-1415…`, `wp-…`,
/// `1415…_at_contact` are ids, not names.
fn slug_is_name(slug: &str) -> bool {
    let has_letters = slug.chars().filter(|c| c.is_alphabetic()).count() >= 2;
    let id_prefix = ["whatsapp-", "phone-", "wp-", "iphone-"]
        .iter()
        .any(|p| slug.starts_with(p));
    has_letters && !id_prefix && !slug.ends_with("_at_contact")
}

fn identity_handles(ids: &Identities) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |platform: &str, v: &str| {
        if let Some(h) = handles::identity(platform, v) {
            out.push(h);
        }
    };
    for v in &ids.email {
        push("email", v);
    }
    for v in &ids.phone {
        push("phone", v);
    }
    for v in &ids.imessage {
        push("imessage", v);
    }
    for v in &ids.whatsapp {
        push("whatsapp", v);
    }
    for (platform, v) in [
        ("discord", &ids.discord),
        ("linkedin", &ids.linkedin),
        ("instagram", &ids.instagram),
        ("twitter", &ids.twitter),
        ("slack", &ids.slack),
    ] {
        if let Some(v) = v {
            push(platform, v);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// `(is_redirect, first "# Heading" after the front matter)`.
fn page_meta(path: &Path) -> (bool, Option<String>) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return (false, None);
    };
    let (front, body) = match raw.strip_prefix("---\n").and_then(|after| {
        after
            .find("\n---")
            .map(|end| (&after[..end], &after[end + 4..]))
    }) {
        Some(parts) => parts,
        None => ("", raw.as_str()),
    };
    let redirect = front.lines().any(|l| l.trim() == "kind: redirect");
    let title = body
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    (redirect, title)
}

pub fn snapshot(wiki_root: &Path) -> anyhow::Result<PeopleSnapshot> {
    let index = IdentityIndex::build(&WikiLayout::new(wiki_root))?;
    let mut claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut names = BTreeSet::new();
    let mut alias_names = BTreeSet::new();
    let mut household: Vec<(String, Vec<String>)> = Vec::new();
    let mut live_slugs: BTreeSet<&str> = BTreeSet::new();
    let mut with_handles = 0usize;
    for page in index.pages() {
        let (redirect, title) = page_meta(&page.path);
        if redirect {
            continue;
        }
        live_slugs.insert(page.slug.as_str());
        for alias in &page.aliases {
            let n = normalize_name(alias);
            if !n.is_empty() {
                alias_names.insert((page.slug.clone(), n));
            }
        }
        if !page.household.is_empty() {
            household.push((page.slug.clone(), page.household.clone()));
        }
        let hs = identity_handles(&page.identities);
        if !hs.is_empty() {
            with_handles += 1;
        }
        for h in hs {
            claims.entry(h).or_default().insert(page.slug.clone());
        }
        if slug_is_name(&page.slug) {
            let n = normalize_name(&page.slug);
            if !n.is_empty() {
                names.insert((page.slug.clone(), n));
            }
        }
        if let Some(t) = title {
            let n = normalize_name(&t);
            if !n.is_empty() {
                names.insert((page.slug.clone(), n));
            }
        }
    }
    let mut map = BTreeMap::new();
    let mut conflicts = Vec::new();
    let mut stub_resolved = 0usize;
    for (handle, owners) in claims {
        if owners.len() == 1 {
            map.insert(handle, owners.into_iter().next().unwrap_or_default());
            continue;
        }
        let named: Vec<&String> = owners.iter().filter(|o| slug_is_name(o)).collect();
        if named.len() == 1 {
            map.insert(handle, named[0].clone());
            stub_resolved += 1;
        } else {
            conflicts.push((handle, owners.into_iter().collect()));
        }
    }
    // A link to the page itself, to a redirect, or to a slug with no page is
    // owner typo territory: drop it rather than fail the whole snapshot.
    let mut links = BTreeSet::new();
    let mut dangling = 0usize;
    for (slug, others) in &household {
        for other in others {
            if other == slug || !live_slugs.contains(other.as_str()) {
                dangling += 1;
                continue;
            }
            links.insert((slug.clone(), other.clone()));
            links.insert((other.clone(), slug.clone()));
        }
    }
    let report = PeopleReport {
        pages: index.len(),
        people_with_handles: with_handles,
        handles: map.len(),
        names: names.len(),
        conflicts: conflicts.len(),
        stub_duplicates_resolved: stub_resolved,
        aliases: alias_names.len(),
        links: links.len() / 2,
        dangling_links: dangling,
        conflict_handles: conflicts,
    };
    Ok(PeopleSnapshot {
        handles: map,
        names,
        alias_names,
        links,
        report,
    })
}

/// Replace the cache atomically: readers see the old or the new mapping,
/// never an empty table.
///
/// Every row is rewritten from the wiki, so a cache built before a front-
/// matter feature existed (#1539) needs no migration step: the daemon's
/// `people_loop` refresh (immediate at startup, then every 15 minutes) fills
/// in the new aliases and household links on its own.
pub fn write_snapshot(store: &Store, snap: &PeopleSnapshot) -> anyhow::Result<()> {
    store.with_conn(|c| {
        c.execute_batch("BEGIN IMMEDIATE")?;
        let r = (|| {
            c.execute("DELETE FROM message_people", [])?;
            c.execute("DELETE FROM message_person_names", [])?;
            c.execute("DELETE FROM message_person_links", [])?;
            {
                let mut ins =
                    c.prepare("INSERT INTO message_people (handle, person_key) VALUES (?1, ?2)")?;
                for (h, k) in &snap.handles {
                    ins.execute(params![h, k])?;
                }
                let mut ins = c.prepare(
                    "INSERT OR IGNORE INTO message_person_names (person_key, name) VALUES (?1, ?2)",
                )?;
                for (k, n) in snap.names.iter().chain(snap.alias_names.iter()) {
                    ins.execute(params![k, n])?;
                }
                let mut ins = c.prepare(
                    "INSERT OR IGNORE INTO message_person_links (person_key, other_key) VALUES (?1, ?2)",
                )?;
                for (a, b) in &snap.links {
                    ins.execute(params![a, b])?;
                }
            }
            c.execute(
                "INSERT INTO message_people_meta (key, value) VALUES ('built_at_ms', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [chrono::Utc::now().timestamp_millis().to_string()],
            )?;
            Ok(())
        })();
        match r {
            Ok(()) => c.execute_batch("COMMIT"),
            Err(e) => {
                let _ = c.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    })?;
    Ok(())
}

pub fn resolve_people(store: &Store, wiki_root: &Path) -> anyhow::Result<PeopleReport> {
    let snap = snapshot(wiki_root)?;
    write_snapshot(store, &snap)?;
    Ok(snap.report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// Query was a handle (address, number, platform id).
    Handle,
    /// Full name, title or page key matched exactly.
    Exact,
    /// Matched a word of a name (e.g. a first name).
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersonMatch {
    /// Person page key, or the canonical handle for an unresolved handle.
    pub key: String,
    pub name: Option<String>,
    pub kind: MatchKind,
    /// `false` when the query was a handle no person page claims.
    pub resolved: bool,
    /// Newest message exchanged with this person, RFC 3339 (#1539): set on
    /// ambiguity candidates only, and the resolver never picks by recency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

fn looks_like_handle(q: &str) -> bool {
    let q = q.trim();
    q.contains('@')
        || [
            "discord:",
            "linkedin:",
            "instagram:",
            "whatsapp:",
            "phone:",
            "email:",
            "slack:",
            "twitter:",
        ]
        .iter()
        .any(|p| q.starts_with(p))
        || q.chars().filter(|c| c.is_ascii_digit()).count() >= 7
}

fn handle_of(q: &str) -> String {
    let q = q.trim();
    for ns in [
        "phone:",
        "email:",
        "discord:",
        "linkedin:",
        "instagram:",
        "whatsapp:",
        "slack:",
        "twitter:",
    ] {
        if q.starts_with(ns) {
            return q.to_string();
        }
    }
    handles::canonical(q)
}

/// Candidates for a person reference. Exact matches win over partial ones,
/// except that a single-word reference also carries every partial match
/// (#1668): a page keyed by a bare first name (`jordan`) must not silently
/// swallow `jordan-blake`, whose handles hold the thread. Several candidates
/// mean the reference is ambiguous and the caller must not pick one.
pub fn resolve_person(
    c: &Connection,
    query: &str,
) -> augmentagent_store::rusqlite::Result<Vec<PersonMatch>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    if looks_like_handle(q) {
        let h = handle_of(q);
        let key: Option<String> = c
            .query_row(
                "SELECT person_key FROM message_people WHERE handle = ?1",
                [&h],
                |r| r.get(0),
            )
            .optional()?;
        return Ok(vec![match key {
            Some(k) => PersonMatch {
                name: first_name_for(c, &k)?,
                key: k,
                kind: MatchKind::Handle,
                resolved: true,
                last_seen: None,
            },
            None => PersonMatch {
                key: h,
                name: None,
                kind: MatchKind::Handle,
                resolved: false,
                last_seen: None,
            },
        }]);
    }
    let n = normalize_name(q);
    if n.is_empty() {
        return Ok(Vec::new());
    }
    let mut exact: BTreeMap<String, String> = BTreeMap::new();
    {
        let mut stmt = c.prepare(
            "SELECT person_key, name FROM message_person_names WHERE name = ?1 OR person_key = ?2",
        )?;
        for row in stmt.query_map(params![n, q], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (k, name) = row?;
            exact.entry(k).or_insert(name);
        }
    }
    // A multi-word reference that matches exactly is settled. A single word
    // is only settled when nobody else's name contains that word: otherwise
    // the bare-key page wins by accident of slug, not by evidence (#1668).
    if !exact.is_empty() && n.contains(' ') {
        return Ok(exact_matches(exact));
    }
    let mut partial: BTreeMap<String, String> = BTreeMap::new();
    let mut stmt = c.prepare(
        "SELECT person_key, name FROM message_person_names \
         WHERE name LIKE ?1 || ' %' OR name LIKE '% ' || ?1 OR name LIKE '% ' || ?1 || ' %'",
    )?;
    for row in stmt.query_map([&n], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (k, name) = row?;
        if !exact.contains_key(&k) {
            partial.entry(k).or_insert(name);
        }
    }
    let mut out = exact_matches(exact);
    out.extend(partial.into_iter().map(|(key, name)| PersonMatch {
        key,
        name: Some(name),
        kind: MatchKind::Partial,
        resolved: true,
        last_seen: None,
    }));
    Ok(out)
}

fn exact_matches(exact: BTreeMap<String, String>) -> Vec<PersonMatch> {
    exact
        .into_iter()
        .map(|(key, name)| PersonMatch {
            key,
            name: Some(name),
            kind: MatchKind::Exact,
            resolved: true,
            last_seen: None,
        })
        .collect()
}

fn first_name_for(
    c: &Connection,
    key: &str,
) -> augmentagent_store::rusqlite::Result<Option<String>> {
    c.query_row(
        "SELECT name FROM message_person_names WHERE person_key = ?1 ORDER BY length(name) DESC LIMIT 1",
        [key],
        |r| r.get(0),
    )
    .optional()
}

/// Every canonical handle for a person key (or the handle itself when the
/// key is an unresolved handle).
pub fn handles_for(c: &Connection, key: &str) -> augmentagent_store::rusqlite::Result<Vec<String>> {
    let mut stmt =
        c.prepare("SELECT handle FROM message_people WHERE person_key = ?1 ORDER BY handle")?;
    let hs: Vec<String> = stmt
        .query_map([key], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(if hs.is_empty() {
        vec![key.to_string()]
    } else {
        hs
    })
}

/// Whether two person keys are documented as sharing a household (#1539).
pub fn linked(c: &Connection, a: &str, b: &str) -> augmentagent_store::rusqlite::Result<bool> {
    c.query_row(
        "SELECT 1 FROM message_person_links WHERE person_key = ?1 AND other_key = ?2",
        [a, b],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

/// Timestamp of the newest message exchanged with a person, for ordering
/// ambiguity candidates. `None` when nothing in the index mentions them.
pub fn last_seen_ms(
    c: &Connection,
    key: &str,
) -> augmentagent_store::rusqlite::Result<Option<i64>> {
    let hs = handles_for(c, key)?;
    let marks = (1..=hs.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let bound: Vec<&dyn augmentagent_store::rusqlite::ToSql> = hs
        .iter()
        .map(|h| h as &dyn augmentagent_store::rusqlite::ToSql)
        .collect();
    c.query_row(
        &format!(
            "SELECT MAX(ts_ms) FROM message_index \
             WHERE sender_handle IN ({marks}) OR counterpart_handle IN ({marks})"
        ),
        bound.as_slice(),
        |r| r.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::Email;
    use std::fs;

    fn page(root: &Path, slug: &str, front: &str, body: &str) {
        let dir = root.join("people");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{slug}.md")),
            format!("---\nkind: person\n{front}---\n{body}"),
        )
        .unwrap();
    }

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("t.db")).unwrap();
        (d, s)
    }

    fn email(
        id: &str,
        platform: &str,
        from: &str,
        thread: &str,
        subject: &str,
        kind: &str,
    ) -> Email {
        Email {
            message_id: id.into(),
            thread_id: Some(thread.into()),
            from: from.into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: subject.into(),
            body: "hi".into(),
            date: "2026-08-26T14:32:05-04:00".into(),
            account_entity_id: Some("discord:900".into()),
            platform: platform.into(),
            kind: kind.into(),
        }
    }

    fn wiki() -> tempfile::TempDir {
        let w = tempfile::tempdir().unwrap();
        page(
            w.path(),
            "jane-doe",
            "identities:\n  email: [\"Jane@Example.com\"]\n  phone: [\"+14155550123\"]\n  discord: \"500\"\n  whatsapp: [\"14155550123@s.whatsapp.net\", \"8877@lid\"]\n",
            "",
        );
        page(
            w.path(),
            "jane-roe",
            "identities:\n  email: [\"jroe@example.org\"]\n",
            "# Jane Roe\n",
        );
        page(
            w.path(),
            "16105550199",
            "identities:\n  imessage: [\"+16105550199\"]\n",
            "# Sam Smith\n",
        );
        // An id-only stub and the named page both claim the same number.
        page(
            w.path(),
            "16105550142",
            "identities:\n  imessage: [\"+16105550142\"]\n",
            "",
        );
        page(
            w.path(),
            "pat-lee",
            "identities:\n  phone: [\"+16105550142\"]\n",
            "",
        );
        // Conflict: both pages claim the same address.
        page(
            w.path(),
            "alex-one",
            "identities:\n  email: [\"shared@example.net\"]\n",
            "",
        );
        page(
            w.path(),
            "alex-two",
            "identities:\n  email: [\"shared@example.net\"]\n",
            "",
        );
        fs::write(
            w.path().join("people/old.md"),
            "---\nkind: redirect\nredirect_to: jane-doe.md\n---\nmoved",
        )
        .unwrap();
        fs::write(
            w.path().join("people/broken.md"),
            "---\nidentities: [unclosed\n---\n",
        )
        .unwrap();
        w
    }

    #[test]
    fn identity_handles_canonicalize_like_message_senders() {
        let snap = snapshot(wiki().path()).unwrap();
        for h in [
            "email:jane@example.com",
            "phone:+14155550123",
            "discord:500",
            "whatsapp:8877@lid",
            "phone:+16105550199",
        ] {
            assert!(
                snap.handles.contains_key(h),
                "{h} missing: {:?}",
                snap.handles
            );
        }
        assert_eq!(snap.handles["phone:+14155550123"], "jane-doe");
        assert_eq!(
            handles::canonical("Jane <jane@example.com>"),
            "email:jane@example.com",
            "sender side must produce the same key"
        );
    }

    #[test]
    fn handle_claimed_by_two_pages_maps_to_neither_and_is_reported() {
        let snap = snapshot(wiki().path()).unwrap();
        assert!(!snap.handles.contains_key("email:shared@example.net"));
        assert_eq!(snap.report.conflicts, 1);
        assert_eq!(
            snap.report.conflict_handles[0].1,
            vec!["alex-one", "alex-two"]
        );
    }

    #[test]
    fn stub_page_duplicate_resolves_to_the_named_page() {
        let snap = snapshot(wiki().path()).unwrap();
        assert_eq!(snap.handles["phone:+16105550142"], "pat-lee");
        assert_eq!(snap.report.stub_duplicates_resolved, 1);
        assert_eq!(
            snap.report.conflicts, 1,
            "two named pages sharing an address stay unresolved"
        );
    }

    #[test]
    fn page_with_bad_front_matter_is_skipped_not_fatal_and_redirects_have_no_names() {
        let snap = snapshot(wiki().path()).unwrap();
        assert!(!snap.names.iter().any(|(k, _)| k == "old" || k == "broken"));
        assert!(snap
            .names
            .contains(&("16105550199".into(), "sam smith".into())));
        assert!(
            !snap.names.iter().any(|(_, n)| n == "16105550199"),
            "id slugs aren't names"
        );
    }

    /// Three pages, one documented pair, plus the two household entries that
    /// must be dropped: a self-link and a slug with no page.
    fn pair_wiki(aliases: bool) -> tempfile::TempDir {
        let w = tempfile::tempdir().unwrap();
        let front = format!(
            "{}household: [\"[[sam-doe]]\", alex-rivera, ghost-doe]\nidentities:\n  discord: \"510\"\n",
            if aliases { "aliases: [Lex, \"Al\"]\n" } else { "" }
        );
        page(w.path(), "alex-rivera", &front, "");
        page(w.path(), "sam-doe", "identities:\n  discord: \"610\"\n", "");
        page(w.path(), "sam-park", "identities:\n  discord: \"620\"\n", "");
        w
    }

    fn joined(pairs: &BTreeSet<(String, String)>) -> Vec<String> {
        pairs.iter().map(|(a, b)| format!("{a}/{b}")).collect()
    }

    #[test]
    fn aliases_stay_off_the_send_path_and_household_links_go_both_ways() {
        let with = snapshot(pair_wiki(true).path()).unwrap();
        let plain = snapshot(pair_wiki(false).path()).unwrap();
        assert_eq!(
            joined(&with.alias_names),
            ["alex-rivera/al", "alex-rivera/lex"]
        );
        assert!(plain.alias_names.is_empty());
        assert_eq!(
            with.names, plain.names,
            "`names` drives Slack recipient resolution: aliases must stay out"
        );
        assert_eq!(
            joined(&with.links),
            ["alex-rivera/sam-doe", "sam-doe/alex-rivera"],
            "one documented direction links both ways"
        );
        assert_eq!((with.report.aliases, with.report.links), (2, 1));
        assert_eq!(with.report.dangling_links, 2, "self-link and ghost slug");

        let (_d, s) = store();
        write_snapshot(&s, &with).unwrap();
        s.with_conn(|c| {
            assert!(linked(c, "sam-doe", "alex-rivera")? && linked(c, "alex-rivera", "sam-doe")?);
            assert!(!linked(c, "alex-rivera", "sam-park")?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn write_replaces_atomically_and_rebuild_is_identical() {
        let (_d, s) = store();
        let w = wiki();
        let first = resolve_people(&s, w.path()).unwrap();
        let count = |s: &Store| -> i64 {
            s.with_conn(|c| c.query_row("SELECT COUNT(*) FROM message_people", [], |r| r.get(0)))
                .unwrap()
        };
        let n1 = count(&s);
        assert_eq!(n1 as usize, first.handles);
        s.with_conn(|c| {
            c.execute_batch(
                "DELETE FROM message_people; DELETE FROM message_person_names; \
                 DELETE FROM message_person_links;",
            )
        })
        .unwrap();
        resolve_people(&s, w.path()).unwrap();
        assert_eq!(count(&s), n1);
        // A failing rebuild leaves the old mapping in place.
        let bad = PeopleSnapshot {
            handles: [("email:x@example.com".to_string(), "x".to_string())].into(),
            names: [("k".to_string(), "dup".to_string())].into(),
            ..PeopleSnapshot::default()
        };
        s.with_conn(|c| c.execute_batch("DROP TABLE message_person_names"))
            .unwrap();
        assert!(write_snapshot(&s, &bad).is_err());
        assert_eq!(count(&s), n1, "rolled back, old mapping intact");
    }

    #[test]
    fn editing_identities_changes_resolution_without_reindexing_messages() {
        let (_d, s) = store();
        let w = wiki();
        resolve_people(&s, w.path()).unwrap();
        s.with_conn(|c| {
            assert!(resolve_person(c, "jroe@example.org")?[0].resolved);
            Ok(())
        })
        .unwrap();
        page(
            w.path(),
            "jane-roe",
            "identities:\n  email: [\"new@example.org\"]\n",
            "# Jane Roe\n",
        );
        resolve_people(&s, w.path()).unwrap();
        s.with_conn(|c| {
            assert!(!resolve_person(c, "jroe@example.org")?[0].resolved);
            assert!(resolve_person(c, "new@example.org")?[0].resolved);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn resolve_person_is_case_insensitive_and_reports_ambiguity() {
        let (_d, s) = store();
        resolve_people(&s, wiki().path()).unwrap();
        s.with_conn(|c| {
            let exact = resolve_person(c, "JANE doe")?;
            assert_eq!(exact.len(), 1);
            assert_eq!(
                (exact[0].key.as_str(), exact[0].kind),
                ("jane-doe", MatchKind::Exact)
            );
            let by_key = resolve_person(c, "jane-roe")?;
            assert_eq!(by_key[0].key, "jane-roe");
            let first_name = resolve_person(c, "jane")?;
            assert_eq!(
                first_name
                    .iter()
                    .map(|m| m.key.as_str())
                    .collect::<Vec<_>>(),
                ["jane-doe", "jane-roe"],
                "two Janes: both returned, neither chosen"
            );
            assert!(first_name.iter().all(|m| m.kind == MatchKind::Partial));
            assert!(resolve_person(c, "nobody")?.is_empty());
            let raw = resolve_person(c, "+1 (212) 555-0100")?;
            assert!(
                !raw[0].resolved,
                "unknown handles stay usable as raw handles"
            );
            assert_eq!(raw[0].key, handles::canonical("+1 (212) 555-0100"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn bare_first_name_keyed_page_does_not_swallow_longer_named_people() {
        // #1668: `jordan.md` (key `jordan`, no identities) sat next to
        // `jordan-blake.md`, which holds the thread's number. `with:jordan`
        // matched the bare key exactly, bound to a page with no handles, and
        // search answered zero hits behind an empty ambiguity list.
        let (_d, s) = store();
        let w = tempfile::tempdir().unwrap();
        page(w.path(), "jordan", "", "");
        page(
            w.path(),
            "jordan-blake",
            "identities:\n  phone: [\"+14155550177\"]\n",
            "",
        );
        resolve_people(&s, w.path()).unwrap();
        s.with_conn(|c| {
            let got = resolve_person(c, "jordan")?;
            assert_eq!(
                got.iter()
                    .map(|m| (m.key.as_str(), m.kind))
                    .collect::<Vec<_>>(),
                [
                    ("jordan", MatchKind::Exact),
                    ("jordan-blake", MatchKind::Partial),
                ],
                "the bare-key page is a candidate, never the silent winner"
            );
            assert_eq!(handles_for(c, "jordan")?, ["jordan"], "nothing to search");
            assert_eq!(handles_for(c, "jordan-blake")?, ["phone:+14155550177"]);
            // Either full spelling still settles the reference.
            for q in ["Jordan Blake", "jordan-blake"] {
                let settled = resolve_person(c, q)?;
                assert_eq!(settled.len(), 1, "{q}");
                assert_eq!(settled[0].key, "jordan-blake");
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn with_person_matches_owner_messages_in_their_dm_and_group_participants() {
        let (_d, s) = store();
        resolve_people(&s, wiki().path()).unwrap();
        // DM with Jane on WhatsApp: only the owner spoke; the conversation id
        // carries Jane's number.
        s.upsert_email(&email(
            "w1",
            "whatsapp",
            "me",
            "whatsapp-history:14155550123@s.whatsapp.net",
            "WhatsApp: Jane [me]",
            "dm",
        ))
        .unwrap();
        // Discord group with Jane and an unknown person.
        s.upsert_email(&email(
            "d1",
            "discord",
            "Jane <discord:500>",
            "77",
            "Discord group DM: Jane, Kim [Jane]",
            "group",
        ))
        .unwrap();
        s.upsert_email(&email(
            "d2",
            "discord",
            "Kim <discord:600>",
            "77",
            "Discord group DM: Jane, Kim [Kim]",
            "group",
        ))
        .unwrap();
        crate::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
        s.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT conversation_id, handle, person_key FROM conversation_people ORDER BY conversation_id, handle",
            )?;
            let rows: Vec<(String, String, Option<String>)> =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            assert_eq!(
                rows,
                vec![
                    ("77".into(), "discord:500".into(), Some("jane-doe".into())),
                    ("77".into(), "discord:600".into(), None),
                    (
                        "whatsapp-history:14155550123@s.whatsapp.net".into(),
                        "phone:+14155550123".into(),
                        Some("jane-doe".into())
                    ),
                ]
            );
            assert_eq!(
                handles_for(c, "jane-doe")?,
                ["discord:500", "email:jane@example.com", "phone:+14155550123", "whatsapp:8877@lid"]
            );
            assert_eq!(handles_for(c, "raw:someone")?, ["raw:someone"]);
            Ok(())
        })
        .unwrap();
    }
}
