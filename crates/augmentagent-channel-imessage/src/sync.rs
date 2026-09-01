//! Backfill (#885) and incremental poll (#886) over the iMessage bundle.
//!
//! Backfill feeds person pages (fill-blanks-only via `merge_person_page`,
//! `updated:` stamped with the *last message date* — see `page::bump_updated`
//! for why today would mute the stale-contact engine). Poll feeds `emails`
//! rows (`platform = "imessage"`) so `search_conversation_history` covers
//! texting history, advancing a per-conversation entries-seen cursor.

use std::path::PathBuf;

use anyhow::Result;
use augmentagent_store::{PhoneIdentity, Store};
use augmentagent_wiki::{
    merge_person_page, slug_from_email, IdentityIndex, PersonPatch, WikiLayout,
};
use chrono::DateTime;
use serde::Serialize;
use tracing::warn;

use crate::bundle::{entry_date, synthetic_imessage_email, Bundle, Conversation, MessageEntry};
use crate::page::bump_updated;

#[derive(Debug, Default, Serialize)]
pub struct ImessageReport {
    pub conversations_seen: usize,
    pub pages_created: usize,
    pub pages_updated: usize,
    pub noop: usize,
    pub skipped: usize,
    pub phones_indexed: usize,
    /// DMs whose page was found by full-name match rather than by phone or
    /// identity index (see `ImessageSyncer::resolve_by_name`).
    pub resolved_by_name: usize,
    pub applied: bool,
    pub diffs: Vec<ImessageDiff>,
}

#[derive(Debug, Serialize)]
pub struct ImessageDiff {
    pub title: String,
    pub slug: Option<String>,
    pub action: String,
    pub last_message: Option<String>,
    pub filled: Vec<String>,
}

/// Person-page backfill. Dry-run by default (`apply: false`): full report,
/// zero writes — the `ContactsSyncer` contract.
pub struct ImessageSyncer<'a> {
    pub bundle: &'a Bundle,
    pub layout: &'a WikiLayout,
    pub store: &'a Store,
    pub apply: bool,
}

impl ImessageSyncer<'_> {
    pub fn run(&self) -> Result<ImessageReport> {
        let index = IdentityIndex::build(self.layout)?;
        let mut report = ImessageReport {
            applied: self.apply,
            ..Default::default()
        };

        for conv in self.bundle.conversations()? {
            report.conversations_seen += 1;
            let entries = match self.bundle.entries(&conv) {
                Ok(e) if !e.is_empty() => e,
                Ok(_) => continue,
                Err(e) => {
                    warn!(conversation = %conv.identifier, error = %e, "skipping unreadable conversation");
                    continue;
                }
            };
            let last_date = entries.iter().rev().find_map(|e| entry_date(&e.timestamp));

            if conv.is_group() {
                self.touch_group_participants(&conv, &index, last_date, &mut report)?;
                continue;
            }

            let handle = conv
                .participants
                .first()
                .cloned()
                .unwrap_or_else(|| conv.identifier.clone());

            let title = clean_title(&conv.title);
            let (slug, existing_path) = match self.resolve(&index, &handle) {
                Some(hit) => hit,
                None => {
                    // The bundle resolves contact names itself: a title equal
                    // to the raw identifier means "not in the operator's
                    // contacts" — short codes, delivery bots. Never page those.
                    // Same for a title with nothing slug-able in it (an emoji
                    // contact) — it would collapse to `at_contact.md`.
                    if conv.title == conv.identifier
                        || !title.chars().any(|c| c.is_ascii_alphanumeric())
                    {
                        report.skipped += 1;
                        report.diffs.push(ImessageDiff {
                            title: conv.title.clone(),
                            slug: None,
                            action: "skip".into(),
                            last_message: last_date.map(str::to_string),
                            filled: Vec::new(),
                        });
                        continue;
                    }
                    // No phone/identity hit, but the wiki may already hold
                    // this person under the LLM-ingest slug (kebab full name,
                    // `chris-crimi.md`). At first backfill only 3 of ~1,350
                    // people pages carried a phone, so without this step
                    // every texter with an email-only page got a duplicate
                    // `<name>_at_contact.md` stub.
                    if let Some(hit) = self.resolve_by_name(&title) {
                        report.resolved_by_name += 1;
                        hit
                    } else {
                        let slug = slug_from_email(&format!("{title}@contact"));
                        let path = self.layout.people_dir().join(format!("{slug}.md"));
                        (slug, path)
                    }
                }
            };

            let existing = std::fs::read_to_string(&existing_path).ok();
            let mut patch = PersonPatch::new()
                .with_display_name(title.clone())
                .identity("imessage", &handle)
                .source(format!(
                    "iMessage history: {} messages through {} ({})",
                    entries.len(),
                    last_date.unwrap_or("unknown"),
                    conv.service
                ));
            if handle.starts_with('+') {
                patch = patch.identity("phone", &handle);
            }
            let merged = merge_person_page(existing.as_deref(), &patch);

            // Stamp `updated:` with the last message date on the merged
            // content — never today, never backwards (#885).
            let mut content = merged.content.clone();
            let mut bumped = false;
            if let Some(date) = last_date {
                if let Some(newer) = bump_updated(&content, date) {
                    content = newer;
                    bumped = true;
                }
            }

            let action = if merged.created {
                report.pages_created += 1;
                "create"
            } else if merged.changed || bumped {
                report.pages_updated += 1;
                "update"
            } else {
                report.noop += 1;
                "noop"
            };

            if self.apply {
                if merged.changed || bumped {
                    if let Some(parent) = existing_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&existing_path, &content)?;
                }
                if handle.starts_with('+') {
                    self.store.upsert_phone_identity(&PhoneIdentity {
                        phone: handle.clone(),
                        person_slug: slug.clone(),
                        display_name: Some(title.clone()),
                        source: "imessage".into(),
                    })?;
                    report.phones_indexed += 1;
                }
            } else if handle.starts_with('+') {
                report.phones_indexed += 1;
            }

            report.diffs.push(ImessageDiff {
                title,
                slug: Some(slug),
                action: action.into(),
                last_message: last_date.map(str::to_string),
                filled: merged.filled,
            });
        }
        Ok(report)
    }

    /// Groups: append a provenance line to each *resolvable* participant's
    /// page. Never create pages from group membership — a handle seen only
    /// in a group is weak identity evidence (#885).
    fn touch_group_participants(
        &self,
        conv: &Conversation,
        index: &IdentityIndex,
        last_date: Option<&str>,
        report: &mut ImessageReport,
    ) -> Result<()> {
        for handle in &conv.participants {
            let Some((slug, path)) = self.resolve(index, handle) else {
                continue;
            };
            let Ok(existing) = std::fs::read_to_string(&path) else {
                continue;
            };
            let patch = PersonPatch::new().source(format!(
                "iMessage group '{}' member (as {})",
                conv.title, handle
            ));
            let merged = merge_person_page(Some(&existing), &patch);
            if merged.changed {
                report.pages_updated += 1;
                if self.apply {
                    std::fs::write(&path, &merged.content)?;
                }
                report.diffs.push(ImessageDiff {
                    title: conv.title.clone(),
                    slug: Some(slug),
                    action: "group-source".into(),
                    last_message: last_date.map(str::to_string),
                    filled: merged.filled,
                });
            }
        }
        Ok(())
    }

    /// Phone reverse-index first (`identity_phone`, #62 — finally wired),
    /// then the wiki identity index (`imessage` falls back to `phone`).
    fn resolve(&self, index: &IdentityIndex, handle: &str) -> Option<(String, PathBuf)> {
        if handle.starts_with('+') {
            if let Ok(Some(hit)) = self.store.lookup_person_by_phone(handle) {
                let path = self
                    .layout
                    .people_dir()
                    .join(format!("{}.md", hit.person_slug));
                return Some((hit.person_slug, path));
            }
        }
        index
            .lookup("imessage", handle)
            .map(|p| (p.slug.clone(), p.path.clone()))
    }

    /// Last resort for DMs: an existing `people/<kebab-full-name>.md` page.
    /// Conservative on purpose — `name_slug` refuses single-token names, and
    /// the file must already exist. A miss here means a fresh stub, never a
    /// guess.
    fn resolve_by_name(&self, title: &str) -> Option<(String, PathBuf)> {
        let slug = name_slug(title)?;
        let path = self.layout.people_dir().join(format!("{slug}.md"));
        path.is_file().then_some((slug, path))
    }
}

/// Contact display names sometimes arrive with the surname doubled
/// ("Derek Meegan Meegan") when the Contacts card's first-name field already
/// holds the full name. Collapse an immediately repeated trailing token when
/// there are at least three tokens; two-token names ("Sirhan Sirhan") are
/// left alone. Whitespace is normalized either way.
pub(crate) fn clean_title(title: &str) -> String {
    let toks: Vec<&str> = title.split_whitespace().collect();
    let n = toks.len();
    if n >= 3 && toks[n - 1].eq_ignore_ascii_case(toks[n - 2]) {
        return toks[..n - 1].join(" ");
    }
    toks.join(" ")
}

/// The `people/<slug>.md` convention LLM ingest uses: lowercase ASCII kebab
/// of the full name. `None` unless at least two alphabetic segments survive —
/// "Chase", "Pop" or "📫" are not enough evidence to claim an existing page.
/// Non-ASCII letters are dropped rather than transliterated, so an accented
/// name yields a slug that will not match anything and falls through to a
/// stub (a miss, never a false positive).
pub(crate) fn name_slug(title: &str) -> Option<String> {
    let mut out = String::with_capacity(title.len());
    let mut last_dash = true;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    let alpha_segments = out
        .split('-')
        .filter(|seg| seg.chars().any(|c| c.is_ascii_alphabetic()))
        .count();
    (alpha_segments >= 2).then_some(out)
}

#[derive(Debug, Default, Serialize)]
pub struct PollStats {
    pub conversations_with_new: usize,
    pub emails_inserted: usize,
}

/// New entries of one conversation since the stored cursor. `first_run`
/// flags a conversation never seen before — callers must not fan those out
/// to per-conversation LLM ingest (a first pass over a full history would
/// mean hundreds of calls); the `emails` rows still land for search.
pub struct PollDelta {
    pub conversation: Conversation,
    pub new_entries: Vec<(usize, MessageEntry)>,
    pub first_run: bool,
}

/// Read every conversation's tail past the cursor, persist the messages as
/// `emails` rows (historical `firstSeenAt`), advance the cursor.
pub fn poll_once(bundle: &Bundle, store: &Store) -> Result<(PollStats, Vec<PollDelta>)> {
    let mut stats = PollStats::default();
    let mut deltas = Vec::new();

    for conv in bundle.conversations()? {
        let seen = store.get_imessage_entries_seen(&conv.identifier)? as usize;
        let entries = match bundle.entries(&conv) {
            Ok(e) => e,
            Err(e) => {
                warn!(conversation = %conv.identifier, error = %e, "skipping unreadable conversation");
                continue;
            }
        };
        if entries.len() <= seen {
            continue;
        }

        let mut new_entries = Vec::new();
        for (idx, entry) in entries.iter().enumerate().skip(seen) {
            let email = synthetic_imessage_email(&conv, idx, entry);
            let ts_ms = DateTime::parse_from_rfc3339(&entry.timestamp)
                .map(|t| t.timestamp_millis())
                .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis());
            if store.upsert_email_backfill(&email, ts_ms)? {
                stats.emails_inserted += 1;
            }
            new_entries.push((idx, entry.clone()));
        }
        store.set_imessage_entries_seen(&conv.identifier, entries.len() as i64)?;
        stats.conversations_with_new += 1;
        deltas.push(PollDelta {
            conversation: conv,
            new_entries,
            first_run: seen == 0,
        });
    }
    Ok((stats, deltas))
}

/// One synthetic `Email` for a whole conversation delta — the wiki ingest
/// runs once per conversation per poll, not once per message. Body is the
/// new entries in their on-disk header format, capped to keep the ingest
/// prompt bounded (oldest entries drop first; the `emails` rows already
/// hold everything).
pub fn batched_delta_email(delta: &PollDelta) -> augmentagent_store::Email {
    const MAX_CHARS: usize = 8_000;
    let conv = &delta.conversation;
    let mut sections: Vec<String> = delta
        .new_entries
        .iter()
        .map(|(_, e)| {
            let mut s = format!("### [{}] {}\n{}", e.timestamp, e.sender, e.body);
            for a in &e.attachments {
                s.push('\n');
                s.push_str(a);
            }
            s
        })
        .collect();
    let mut total: usize = sections.iter().map(|s| s.len() + 2).sum();
    while sections.len() > 1 && total > MAX_CHARS {
        let dropped = sections.remove(0);
        total -= dropped.len() + 2;
    }
    let last_idx = delta.new_entries.last().map(|(i, _)| *i).unwrap_or(0);
    augmentagent_store::Email {
        message_id: format!("imessage:{}:batch:{}", conv.identifier, last_idx),
        thread_id: Some(format!("imessage:{}", conv.identifier)),
        from: conv
            .participants
            .first()
            .cloned()
            .unwrap_or_else(|| conv.identifier.clone()),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: format!("iMessage: {} ({} new)", conv.title, delta.new_entries.len()),
        body: sections.join("\n\n"),
        date: delta
            .new_entries
            .last()
            .map(|(_, e)| e.timestamp.clone())
            .unwrap_or_default(),
        account_entity_id: Some("imessage".into()),
        platform: "imessage".into(),
        kind: "dm".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture_bundle(dir: &std::path::Path, convs: &[(&str, &str, &str, &str)]) -> Bundle {
        // (identifier, dir, title, messages.md content)
        let conv_root = dir.join("conversations");
        std::fs::create_dir_all(&conv_root).unwrap();
        let mut index = serde_json::Map::new();
        for (ident, cdir, title, md) in convs {
            let is_group = ident.starts_with("chat");
            let participants: Vec<String> = if is_group {
                vec!["+14155550123".into(), "+14155550999".into()]
            } else {
                vec![ident.to_string()]
            };
            index.insert(
                ident.to_string(),
                serde_json::json!({
                    "identifier": ident, "dir": cdir, "title": title,
                    "participants": participants, "service": "iMessage",
                    "path": format!("conversations/{cdir}/messages.md"),
                }),
            );
            let d = conv_root.join(cdir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("messages.md"), md).unwrap();
        }
        std::fs::write(
            conv_root.join("index.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();
        Bundle::open(dir)
    }

    fn fresh_env() -> (TempDir, WikiLayout, Store) {
        let dir = TempDir::new().unwrap();
        let layout = WikiLayout::new(dir.path().join("wiki"));
        std::fs::create_dir_all(layout.people_dir()).unwrap();
        let store = Store::open(dir.path().join("test.db")).unwrap();
        (dir, layout, store)
    }

    const DM_MD: &str = "---\ntitle: 'John Smith'\n---\n\n### [2026-08-20T10:00:00-04:00] me\nhey\n\n### [2026-08-26T11:00:00-04:00] +14155550123\nyo\n";

    #[test]
    fn backfill_creates_page_with_last_message_date() {
        let (dir, layout, store) = fresh_env();
        let bundle = fixture_bundle(
            dir.path(),
            &[("+14155550123", "John_Smith", "John Smith", DM_MD)],
        );
        let syncer = ImessageSyncer {
            bundle: &bundle,
            layout: &layout,
            store: &store,
            apply: true,
        };
        let report = syncer.run().unwrap();
        assert_eq!(report.pages_created, 1);
        let page =
            std::fs::read_to_string(layout.people_dir().join("john_smith_at_contact.md")).unwrap();
        // list-valued per schema/wiki-skill.md — a scalar here is unreadable
        // by the identity index (see crm::MULTI_VALUED)
        assert!(
            page.contains("  imessage:\n    - \"+14155550123\"\n"),
            "page:\n{page}"
        );
        assert!(
            page.contains("  phone:\n    - \"+14155550123\"\n"),
            "page:\n{page}"
        );
        // CRM rule: updated == last message date, never today
        assert!(page.contains("updated: 2026-08-26"), "page:\n{page}");
        // phone reverse index written
        assert!(store
            .lookup_person_by_phone("+14155550123")
            .unwrap()
            .is_some());
    }

    #[test]
    fn backfill_skips_unresolved_short_codes() {
        let (dir, layout, store) = fresh_env();
        let md = "---\n---\n\n### [2026-08-26T11:00:00-04:00] 72849\nreply Y to subscribe\n";
        let bundle = fixture_bundle(dir.path(), &[("72849", "72849", "72849", md)]);
        let syncer = ImessageSyncer {
            bundle: &bundle,
            layout: &layout,
            store: &store,
            apply: true,
        };
        let report = syncer.run().unwrap();
        assert_eq!(report.skipped, 1);
        assert_eq!(report.pages_created, 0);
        assert_eq!(std::fs::read_dir(layout.people_dir()).unwrap().count(), 0);
    }

    #[test]
    fn backfill_resolves_existing_page_via_phone_index_and_never_regresses_updated() {
        let (dir, layout, store) = fresh_env();
        std::fs::write(
            layout.people_dir().join("john_smith.md"),
            "---\nkind: person\nkey: john_smith\nupdated: 2026-12-01\n---\n\n# John Smith\n\n## Profile\n\n## Source\n",
        )
        .unwrap();
        store
            .upsert_phone_identity(&PhoneIdentity {
                phone: "+14155550123".into(),
                person_slug: "john_smith".into(),
                display_name: Some("John Smith".into()),
                source: "google_people".into(),
            })
            .unwrap();
        let bundle = fixture_bundle(
            dir.path(),
            &[("+14155550123", "John_Smith", "John Smith", DM_MD)],
        );
        let syncer = ImessageSyncer {
            bundle: &bundle,
            layout: &layout,
            store: &store,
            apply: true,
        };
        let report = syncer.run().unwrap();
        assert_eq!(report.pages_created, 0, "must reuse the phone-indexed page");
        let page = std::fs::read_to_string(layout.people_dir().join("john_smith.md")).unwrap();
        assert!(page.contains("imessage:"), "identity appended:\n{page}");
        // page was touched more recently than the last text — keep it
        assert!(page.contains("updated: 2026-12-01"), "page:\n{page}");
    }

    #[test]
    fn backfill_resolves_existing_page_by_full_name_instead_of_stubbing() {
        // Email-only page (the common case: 1,348 of ~1,350 pages had no
        // phone at first backfill). Title carries the doubled-surname
        // artifact too.
        let (dir, layout, store) = fresh_env();
        std::fs::write(
            layout.people_dir().join("derek-meegan.md"),
            "---\nkind: person\nkey: derek-meegan\nupdated: 2026-05-01\nidentities:\n  email: [derek@example.com]\n---\n\n# Derek Meegan\n\n## Profile\n\n## Source\n",
        )
        .unwrap();
        let bundle = fixture_bundle(
            dir.path(),
            &[("+14155550123", "Derek_Meegan", "Derek Meegan Meegan", DM_MD)],
        );
        let syncer = ImessageSyncer {
            bundle: &bundle,
            layout: &layout,
            store: &store,
            apply: true,
        };
        let report = syncer.run().unwrap();
        assert_eq!(report.pages_created, 0, "must not stub a duplicate");
        assert_eq!(report.resolved_by_name, 1);
        assert_eq!(report.diffs[0].slug.as_deref(), Some("derek-meegan"));
        assert_eq!(report.diffs[0].title, "Derek Meegan", "surname collapsed");
        assert!(!layout
            .people_dir()
            .join("derek_meegan_meegan_at_contact.md")
            .exists());
        let page = std::fs::read_to_string(layout.people_dir().join("derek-meegan.md")).unwrap();
        assert!(page.contains("imessage:"), "identity appended:\n{page}");
        assert!(page.contains("updated: 2026-08-26"), "bumped to last text:\n{page}");
        // phone reverse index now points at the canonical page
        assert_eq!(
            store
                .lookup_person_by_phone("+14155550123")
                .unwrap()
                .unwrap()
                .person_slug,
            "derek-meegan"
        );
    }

    #[test]
    fn single_token_title_never_claims_an_existing_page() {
        let (dir, layout, store) = fresh_env();
        std::fs::write(
            layout.people_dir().join("chase.md"),
            "---\nkind: person\nkey: chase\n---\n\n# Chase (bank)\n",
        )
        .unwrap();
        let md = "---\ntitle: 'Chase'\n---\n\n### [2026-08-26T11:00:00-04:00] +14155550777\nhey\n";
        let bundle = fixture_bundle(dir.path(), &[("+14155550777", "Chase", "Chase", md)]);
        let syncer = ImessageSyncer {
            bundle: &bundle,
            layout: &layout,
            store: &store,
            apply: true,
        };
        let report = syncer.run().unwrap();
        assert_eq!(report.resolved_by_name, 0);
        assert_eq!(report.pages_created, 1);
        assert!(layout.people_dir().join("chase_at_contact.md").exists());
        let untouched = std::fs::read_to_string(layout.people_dir().join("chase.md")).unwrap();
        assert!(!untouched.contains("imessage"), "existing page must be left alone");
    }

    #[test]
    fn backfill_skips_titles_with_nothing_to_slug() {
        let (dir, layout, store) = fresh_env();
        let md = "---\ntitle: '📫'\n---\n\n### [2026-08-26T11:00:00-04:00] +14155550555\nhi\n";
        let bundle = fixture_bundle(dir.path(), &[("+14155550555", "mailbox", "📫", md)]);
        let syncer = ImessageSyncer {
            bundle: &bundle,
            layout: &layout,
            store: &store,
            apply: true,
        };
        let report = syncer.run().unwrap();
        assert_eq!(report.skipped, 1);
        assert_eq!(report.pages_created, 0);
        assert!(!layout.people_dir().join("at_contact.md").exists());
    }

    #[test]
    fn clean_title_collapses_doubled_surname_only() {
        assert_eq!(clean_title("Derek Meegan Meegan"), "Derek Meegan");
        assert_eq!(clean_title("Aunt Laurie laurie"), "Aunt Laurie");
        assert_eq!(clean_title("Sirhan Sirhan"), "Sirhan Sirhan");
        assert_eq!(clean_title("Kevin Mckenna Netrality"), "Kevin Mckenna Netrality");
        assert_eq!(clean_title("  Chris   Crimi "), "Chris Crimi");
        assert_eq!(clean_title("+14155550123"), "+14155550123");
    }

    #[test]
    fn name_slug_requires_two_alphabetic_segments() {
        assert_eq!(name_slug("Chris Crimi").as_deref(), Some("chris-crimi"));
        assert_eq!(
            name_slug("Emmett Madden-Prado").as_deref(),
            Some("emmett-madden-prado")
        );
        assert_eq!(name_slug("Taylor Yates 💚🩵").as_deref(), Some("taylor-yates"));
        assert_eq!(name_slug("lasya tarini").as_deref(), Some("lasya-tarini"));
        assert_eq!(name_slug("Chase"), None);
        assert_eq!(name_slug("📫"), None);
        assert_eq!(name_slug("+14155550123"), None);
        assert_eq!(name_slug("KP 2"), None, "digits are not a name segment");
    }

    #[test]
    fn backfill_dry_run_writes_nothing() {
        let (dir, layout, store) = fresh_env();
        let bundle = fixture_bundle(
            dir.path(),
            &[("+14155550123", "John_Smith", "John Smith", DM_MD)],
        );
        let syncer = ImessageSyncer {
            bundle: &bundle,
            layout: &layout,
            store: &store,
            apply: false,
        };
        let report = syncer.run().unwrap();
        assert_eq!(report.pages_created, 1); // reported…
        assert!(!report.applied);
        assert_eq!(std::fs::read_dir(layout.people_dir()).unwrap().count(), 0); // …not written
        assert!(store
            .lookup_person_by_phone("+14155550123")
            .unwrap()
            .is_none());
    }

    #[test]
    fn backfill_touches_resolvable_group_members_without_creating_pages() {
        let (dir, layout, store) = fresh_env();
        std::fs::write(
            layout.people_dir().join("jane.md"),
            "---\nkind: person\nkey: jane\nidentities:\n  phone: [\"+14155550999\"]\n---\n\n# Jane\n\n## Source\n",
        )
        .unwrap();
        let md = "---\n---\n\n### [2026-08-26T11:00:00-04:00] +14155550999\nski this weekend?\n";
        let bundle = fixture_bundle(dir.path(), &[("chat0001", "chat0001", "Ski Trip", md)]);
        let syncer = ImessageSyncer {
            bundle: &bundle,
            layout: &layout,
            store: &store,
            apply: true,
        };
        let report = syncer.run().unwrap();
        assert_eq!(report.pages_created, 0);
        assert_eq!(report.pages_updated, 1);
        let page = std::fs::read_to_string(layout.people_dir().join("jane.md")).unwrap();
        assert!(page.contains("iMessage group 'Ski Trip'"), "page:\n{page}");
    }

    #[test]
    fn poll_inserts_tail_and_advances_cursor() {
        let (dir, _layout, store) = fresh_env();
        let bundle = fixture_bundle(
            dir.path(),
            &[("+14155550123", "John_Smith", "John Smith", DM_MD)],
        );
        let (stats, deltas) = poll_once(&bundle, &store).unwrap();
        assert_eq!(stats.emails_inserted, 2);
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].first_run);
        // historical firstSeenAt from the message timestamp
        let first_seen = store
            .email_first_seen_at("imessage:+14155550123:0")
            .unwrap()
            .unwrap();
        assert_eq!(
            first_seen,
            DateTime::parse_from_rfc3339("2026-08-20T10:00:00-04:00")
                .unwrap()
                .timestamp_millis()
        );
        // second poll: nothing new
        let (stats2, deltas2) = poll_once(&bundle, &store).unwrap();
        assert_eq!(stats2.emails_inserted, 0);
        assert!(deltas2.is_empty());
    }

    #[test]
    fn batched_delta_email_joins_entries_and_caps_size() {
        let conv = Conversation {
            identifier: "+14155550123".into(),
            dir: "John_Smith".into(),
            title: "John Smith".into(),
            participants: vec!["+14155550123".into()],
            service: "iMessage".into(),
        };
        let entry = |i: usize, body: &str| {
            (
                i,
                MessageEntry {
                    timestamp: format!("2026-08-26T11:{:02}:00-04:00", i),
                    sender: "me".into(),
                    body: body.into(),
                    attachments: Vec::new(),
                },
            )
        };
        let delta = PollDelta {
            conversation: conv.clone(),
            new_entries: vec![entry(5, "first"), entry(6, "second")],
            first_run: false,
        };
        let email = batched_delta_email(&delta);
        assert_eq!(email.message_id, "imessage:+14155550123:batch:6");
        assert!(email.body.contains("first") && email.body.contains("second"));
        assert_eq!(email.date, "2026-08-26T11:06:00-04:00");
        assert_eq!(email.platform, "imessage");

        // oversized deltas drop oldest entries but keep the newest
        let big = "x".repeat(3_000);
        let delta = PollDelta {
            conversation: conv,
            new_entries: (0..5).map(|i| entry(i, &big)).collect(),
            first_run: false,
        };
        let email = batched_delta_email(&delta);
        assert!(email.body.len() <= 9_000);
        assert!(email.body.contains("2026-08-26T11:04"), "newest entry kept");
    }

    #[test]
    fn poll_delta_after_append_is_not_first_run() {
        let (dir, _layout, store) = fresh_env();
        let bundle = fixture_bundle(
            dir.path(),
            &[("+14155550123", "John_Smith", "John Smith", DM_MD)],
        );
        poll_once(&bundle, &store).unwrap();
        // append one entry to the conversation file
        let md_path = dir
            .path()
            .join("conversations")
            .join("John_Smith")
            .join("messages.md");
        let mut md = std::fs::read_to_string(&md_path).unwrap();
        md.push_str("\n### [2026-08-27T09:00:00-04:00] me\nfollow-up\n");
        std::fs::write(&md_path, md).unwrap();

        let (stats, deltas) = poll_once(&bundle, &store).unwrap();
        assert_eq!(stats.emails_inserted, 1);
        assert_eq!(deltas.len(), 1);
        assert!(!deltas[0].first_run);
        assert_eq!(deltas[0].new_entries.len(), 1);
        assert_eq!(deltas[0].new_entries[0].1.body, "follow-up");
    }
}
