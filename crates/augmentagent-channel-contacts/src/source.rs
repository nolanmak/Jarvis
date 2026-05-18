//! `ContactsSource` trait + the fill-blanks-only wiki upsert engine (#62).
//!
//! Both backends (Google People API, generic CardDAV) produce a
//! `Vec<VCard>`; the engine here normalizes phones to E.164, merges into
//! `wiki/people/<slug>.md` via the shared
//! [`augmentagent_wiki::merge_person_page`] (never overwrites, empty stays
//! empty), and maintains the `identity_phone` reverse index so message
//! triage can resolve an inbound number to an existing page before forking a
//! new one.
//!
//! Default is a **dry run**: a JSON [`ContactsReport`] and no writes.

use async_trait::async_trait;
use serde::Serialize;
use tracing::warn;

use augmentagent_store::{PhoneIdentity, Store};
use augmentagent_wiki::{merge_person_page, slug_from_email, PersonPatch, WikiLayout};

use crate::phone;
use crate::vcard::VCard;

#[derive(Debug, thiserror::Error)]
pub enum ContactsError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("backend: {0}")]
    Backend(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(String),
}

/// One pull of contacts from a backend. `sync_token` is the opaque cursor
/// (Google `syncToken` / CardDAV `getctag`) to persist for the next delta;
/// `None` means the backend doesn't support delta (full every time).
pub struct ContactsPull {
    pub cards: Vec<VCard>,
    pub next_sync_token: Option<String>,
}

/// The seam both backends implement and tests fake.
#[async_trait]
pub trait ContactsSource: Send + Sync {
    /// Stable backend id for the `contacts_sync_state` key
    /// (`google_people` | `carddav`).
    fn backend_id(&self) -> &'static str;

    /// Fetch contacts. `since_token` is the previously-persisted cursor (or
    /// `None` for a full pull).
    async fn list_contacts(
        &self,
        since_token: Option<&str>,
    ) -> Result<ContactsPull, ContactsError>;
}

/// Per-contact plan for the dry-run report.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContactDiff {
    pub name: String,
    pub slug: String,
    /// `create` | `update` | `noop`.
    pub action: String,
    pub filled: Vec<String>,
    /// E.164 phones indexed for reverse lookup.
    pub phones_indexed: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ContactsReport {
    pub backend: String,
    pub contacts_seen: usize,
    pub created: usize,
    pub updated: usize,
    pub noop: usize,
    pub phones_indexed: usize,
    pub applied: bool,
    pub diffs: Vec<ContactDiff>,
}

impl ContactsReport {
    pub fn discord_summary(&self) -> String {
        format!(
            "**Contacts sync** ({})\n{} new · {} updated · {} unchanged\n\
             {} phone numbers indexed{}",
            self.backend,
            self.created,
            self.updated,
            self.noop,
            self.phones_indexed,
            if self.applied {
                ""
            } else {
                "\n_(dry run — no wiki writes)_"
            },
        )
    }
}

/// Wiki slug for a contact. Prefers the first email (shares the slug space
/// with email-sourced pages so the same person doesn't fork); else a
/// name-derived slug.
pub fn contact_slug(c: &VCard) -> String {
    if let Some(first_email) = c.emails.first() {
        return slug_from_email(first_email);
    }
    // Name → slug via the same normalizer fed a synthetic local-part.
    let base = if !c.full_name.is_empty() {
        c.full_name.clone()
    } else if !c.phones.is_empty() {
        c.phones[0].clone()
    } else {
        "unknown".to_string()
    };
    slug_from_email(&format!("{base}@contact"))
}

/// Build the fill-blanks patch for a contact. `e164_phones` are the
/// already-normalized numbers. Pure — unit-tested without IO.
pub fn contact_patch(c: &VCard, e164_phones: &[String], today: &str) -> PersonPatch {
    let mut p = PersonPatch::new()
        .with_display_name(if c.full_name.is_empty() {
            "Unknown".to_string()
        } else {
            c.full_name.clone()
        })
        .source(format!("Imported from contacts on {today}"));

    for e in e164_phones {
        p = p.identity("phone", e);
    }
    for email in &c.emails {
        p = p.identity("email", email);
    }
    if let Some(addr) = &c.address {
        p = p.identity("address", addr);
        p = p.profile_row("Address", addr);
    }
    if let Some(org) = &c.organization {
        p = p.profile_row("Company", org);
    }
    if let Some(title) = &c.title {
        p = p.profile_row("Role", title);
    }
    if let Some(bday) = &c.birthday {
        p = p.profile_row("Birthday", bday);
    }
    if let Some(first) = e164_phones.first() {
        p = p.profile_row("Phone", first);
    }
    p
}

/// Run a contacts sync end-to-end: pull → normalize → merge → index.
pub struct ContactsSyncer<'a> {
    pub source: &'a dyn ContactsSource,
    pub layout: &'a WikiLayout,
    pub store: &'a Store,
    pub today: String,
    pub apply: bool,
}

impl<'a> ContactsSyncer<'a> {
    pub async fn run(
        &self,
        account_id: &str,
    ) -> Result<ContactsReport, ContactsError> {
        let backend = self.source.backend_id();
        let since = self
            .store
            .get_contacts_sync_token(backend, account_id)
            .map_err(|e| ContactsError::Store(e.to_string()))?;

        let pull = self
            .source
            .list_contacts(since.as_deref())
            .await?;

        let mut report = ContactsReport {
            backend: backend.to_string(),
            applied: self.apply,
            ..Default::default()
        };

        for card in &pull.cards {
            report.contacts_seen += 1;
            let e164: Vec<String> = card
                .phones
                .iter()
                .filter_map(|raw| phone::normalize(raw))
                .collect();
            // De-dupe E.164 within a card.
            let mut seen = std::collections::BTreeSet::new();
            let e164: Vec<String> =
                e164.into_iter().filter(|p| seen.insert(p.clone())).collect();

            let slug = contact_slug(card);
            let path = self
                .layout
                .people_dir()
                .join(format!("{slug}.md"));
            let existing = std::fs::read_to_string(&path).ok();
            let patch = contact_patch(card, &e164, &self.today);
            let merged = merge_person_page(existing.as_deref(), &patch);

            let action = if !merged.changed {
                "noop"
            } else if merged.created {
                "create"
            } else {
                "update"
            };
            match action {
                "create" => report.created += 1,
                "update" => report.updated += 1,
                _ => report.noop += 1,
            }

            if self.apply {
                if merged.changed {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, &merged.content)?;
                }
                // Reverse index every valid phone → this person, regardless
                // of whether the page changed (the index is a separate
                // concern and must stay fresh).
                for e in &e164 {
                    self.store
                        .upsert_phone_identity(&PhoneIdentity {
                            phone: e.clone(),
                            person_slug: slug.clone(),
                            display_name: if card.full_name.is_empty() {
                                None
                            } else {
                                Some(card.full_name.clone())
                            },
                            source: backend.to_string(),
                        })
                        .map_err(|e| ContactsError::Store(e.to_string()))?;
                }
            }
            report.phones_indexed += e164.len();
            report.diffs.push(ContactDiff {
                name: card.full_name.clone(),
                slug,
                action: action.to_string(),
                filled: merged.filled,
                phones_indexed: e164,
            });
        }

        if self.apply {
            if let Some(tok) = &pull.next_sync_token {
                if let Err(e) =
                    self.store
                        .set_contacts_sync_token(backend, account_id, tok)
                {
                    warn!("failed to persist contacts sync token: {e}");
                }
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcard::VCard;

    struct FakeSource {
        cards: Vec<VCard>,
        token: Option<String>,
    }
    #[async_trait]
    impl ContactsSource for FakeSource {
        fn backend_id(&self) -> &'static str {
            "google_people"
        }
        async fn list_contacts(
            &self,
            _since: Option<&str>,
        ) -> Result<ContactsPull, ContactsError> {
            Ok(ContactsPull {
                cards: self.cards.clone(),
                next_sync_token: self.token.clone(),
            })
        }
    }

    fn card(name: &str, phone: &str, email: &str) -> VCard {
        VCard {
            full_name: name.into(),
            phones: vec![phone.into()],
            emails: if email.is_empty() {
                vec![]
            } else {
                vec![email.into()]
            },
            ..Default::default()
        }
    }

    fn env() -> (tempfile::TempDir, WikiLayout, Store) {
        let d = tempfile::TempDir::new().unwrap();
        let l = WikiLayout::new(d.path().join("wiki"));
        l.bootstrap().unwrap();
        // Minimal schema the store's migrate() needs (mirrors store tests).
        let dbp = d.path().join("data.db");
        let store = {
            let conn = rusqlite::Connection::open(&dbp).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS emails (messageId TEXT PRIMARY KEY, threadId TEXT, fromEmail TEXT, subject TEXT, body TEXT, receivedAt TEXT, accountEntityId TEXT, firstSeenAt INTEGER);\
                 CREATE TABLE IF NOT EXISTS actions (id TEXT PRIMARY KEY, messageId TEXT, threadId TEXT, fromEmail TEXT, subject TEXT, originalBody TEXT, draftBody TEXT, status TEXT, errorMessage TEXT, createdAt INTEGER, updatedAt INTEGER);\
                 CREATE TABLE IF NOT EXISTS learned_patterns (patternType TEXT, pattern TEXT, action TEXT, reason TEXT);\
                 CREATE TABLE IF NOT EXISTS gmail_accounts (id TEXT PRIMARY KEY, connectionId TEXT, email TEXT, label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1, createdAt INTEGER NOT NULL);",
            )
            .unwrap();
            drop(conn);
            Store::open(&dbp).unwrap()
        };
        (d, l, store)
    }

    #[tokio::test]
    async fn dry_run_writes_nothing_but_reports() {
        let (_d, l, s) = env();
        let src = FakeSource {
            cards: vec![card("Jane Doe", "(415) 555-2671", "jane@x.com")],
            token: Some("tok1".into()),
        };
        let syncer = ContactsSyncer {
            source: &src,
            layout: &l,
            store: &s,
            today: "2026-05-18".into(),
            apply: false,
        };
        let r = syncer.run("acc1").await.unwrap();
        assert_eq!(r.created, 1);
        assert_eq!(r.phones_indexed, 1);
        assert!(!r.applied);
        // Nothing on disk, no token persisted, no phone index row.
        assert!(std::fs::read_dir(l.people_dir()).unwrap().next().is_none());
        assert!(s
            .get_contacts_sync_token("google_people", "acc1")
            .unwrap()
            .is_none());
        assert!(s.lookup_person_by_phone("+14155552671").unwrap().is_none());
    }

    #[tokio::test]
    async fn apply_writes_indexes_and_is_idempotent() {
        let (_d, l, s) = env();
        let src = FakeSource {
            cards: vec![card("Jane Doe", "+1 415 555 2671", "jane@x.com")],
            token: Some("tok1".into()),
        };
        let syncer = ContactsSyncer {
            source: &src,
            layout: &l,
            store: &s,
            today: "2026-05-18".into(),
            apply: true,
        };
        let r1 = syncer.run("acc1").await.unwrap();
        assert_eq!(r1.created, 1);
        let page = l
            .people_dir()
            .join(format!("{}.md", contact_slug(&card("Jane Doe", "", "jane@x.com"))));
        assert!(page.is_file());
        let body = std::fs::read_to_string(&page).unwrap();
        assert!(body.contains("phone: \"+14155552671\""));
        assert!(body.contains("- **Phone:** +14155552671"));

        // Reverse index resolves.
        let pid = s.lookup_person_by_phone("+14155552671").unwrap().unwrap();
        assert_eq!(pid.display_name.as_deref(), Some("Jane Doe"));
        // Token persisted.
        assert_eq!(
            s.get_contacts_sync_token("google_people", "acc1").unwrap(),
            Some("tok1".to_string())
        );

        // Second run: fill-blanks → noop, page byte-identical.
        let r2 = syncer.run("acc1").await.unwrap();
        assert_eq!(r2.created, 0);
        assert_eq!(r2.noop, 1);
        assert_eq!(std::fs::read_to_string(&page).unwrap(), body);
    }

    #[test]
    fn slug_prefers_email_for_shared_space() {
        let c = card("Jane Doe", "+1555", "jane@x.com");
        assert_eq!(contact_slug(&c), slug_from_email("jane@x.com"));
    }
}
