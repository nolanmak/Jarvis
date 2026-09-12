//! Hint strings for drafting-time wiki navigation.
//!
//! The drafting Claude call gets `--add-dir wiki/` + Read/Grep/Glob tools, so
//! it can open any page it wants. `WikiReader` doesn't do the reading — it
//! just produces a short hint string that points at likely-relevant pages, so
//! Claude doesn't have to guess file paths.

use std::path::{Path, PathBuf};

use augmentagent_store::Email;

use crate::identity::IdentityIndex;
use crate::layout::WikiLayout;

pub struct WikiReader<'a> {
    pub layout: &'a WikiLayout,
}

impl<'a> WikiReader<'a> {
    pub fn new(layout: &'a WikiLayout) -> Self {
        Self { layout }
    }

    /// Produce a hint string describing which wiki pages the drafting call
    /// should consider opening for this email. Empty-string return means "no
    /// prior context yet — rely on the raw email only".
    pub fn draft_hint(&self, email: &Email) -> String {
        self.draft_hint_with_index(email, None)
    }

    /// [`Self::draft_hint`], plus an `IdentityIndex` fallback for senders
    /// whose page isn't at the email-derived slug (#887): iMessage/Contacts
    /// pages are phone-keyed under kebab-name slugs, and email senders may
    /// only be listed in a kebab page's `identities.email`.
    pub fn draft_hint_with_index(&self, email: &Email, index: Option<&IdentityIndex>) -> String {
        let mut lines: Vec<String> = Vec::new();

        if let Some(person_page) = self.person_page_for(email, index) {
            lines.push(format!(
                "- {} (sender history + preferred tone)",
                relative_to_root(&self.layout.root, &person_page)
            ));
        }

        if let Some(tid) = &email.thread_id {
            let thread_page = self.layout.thread_page(tid);
            if exists(&thread_page) {
                lines.push(format!(
                    "- {} (prior messages in this thread)",
                    relative_to_root(&self.layout.root, &thread_page)
                ));
            }
        }

        if lines.is_empty() {
            return String::new();
        }

        format!(
            "Relevant wiki pages (you MAY open these with the Read tool; you may also Grep/Glob the wiki for additional context):\n{}\n\nAlways prefer wiki facts over assumptions. If the wiki contradicts the email, trust the email and flag the contradiction in your reasoning.",
            lines.join("\n")
        )
    }

    /// Produce a short nudge for the triage call. Single-line-per-page, no
    /// prose — triage is cost-sensitive so we keep the token footprint under
    /// 100 chars. Empty string when no relevant wiki page exists.
    pub fn triage_hint(&self, email: &Email) -> String {
        self.triage_hint_with_index(email, None)
    }

    /// [`Self::triage_hint`] with the same `IdentityIndex` fallback as
    /// [`Self::draft_hint_with_index`].
    pub fn triage_hint_with_index(&self, email: &Email, index: Option<&IdentityIndex>) -> String {
        let mut lines: Vec<String> = Vec::new();

        if let Some(person_page) = self.person_page_for(email, index) {
            lines.push(format!(
                "- Sender has a wiki page ({}) — open with Read; weight importance by Relationship/Tone.",
                relative_to_root(&self.layout.root, &person_page)
            ));
        }

        if let Some(tid) = &email.thread_id {
            let thread_page = self.layout.thread_page(tid);
            if exists(&thread_page) {
                lines.push(format!(
                    "- Prior thread context at {} — open with Read if the email is a follow-up.",
                    relative_to_root(&self.layout.root, &thread_page)
                ));
            }
        }

        if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n")
        }
    }

    /// The sender's people page, if any. The email-derived slug wins; the
    /// index is consulted only when that page is absent, so a sender is
    /// never listed twice (see [`IdentityIndex::lookup_sender`] for how the
    /// handle picks the platform).
    fn person_page_for(&self, email: &Email, index: Option<&IdentityIndex>) -> Option<PathBuf> {
        let by_slug = self.layout.person_page(&email.from);
        if exists(&by_slug) {
            return Some(by_slug);
        }
        index?
            .lookup_sender(&email.from)
            .map(|page| page.path.clone())
            .filter(|p| exists(p))
    }
}

fn exists(p: &Path) -> bool {
    p.is_file()
}

fn relative_to_root(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn email(from: &str, thread_id: Option<&str>) -> Email {
        Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "m1".into(),
            thread_id: thread_id.map(str::to_string),
            from: from.into(),
            subject: "s".into(),
            body: "b".into(),
            date: "2026-04-14".into(),
            account_entity_id: None,
            platform: "gmail".into(),
            kind: "dm".into(),
        }
    }

    #[test]
    fn empty_hint_when_no_pages_exist() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        let r = WikiReader::new(&layout);
        assert_eq!(r.draft_hint(&email("a@b.example.com", Some("t1"))), "");
    }

    #[test]
    fn references_existing_person_page() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("a@b.example.com"), "# A\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.draft_hint(&email("a@b.example.com", None));
        assert!(hint.contains("people/a_at_b_example_com.md"));
    }

    #[test]
    fn references_existing_thread_page() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.draft_hint(&email("a@b.example.com", Some("t1")));
        assert!(hint.contains("threads/t1.md"));
    }

    #[test]
    fn triage_hint_empty_when_no_pages_exist() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        let r = WikiReader::new(&layout);
        assert_eq!(r.triage_hint(&email("a@b.example.com", Some("t1"))), "");
    }

    #[test]
    fn triage_hint_references_person_page() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("a@b.example.com"), "# A\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.triage_hint(&email("a@b.example.com", None));
        assert!(hint.contains("people/a_at_b_example_com.md"));
        assert!(hint.contains("Relationship"));
    }

    #[test]
    fn triage_hint_includes_thread_when_present() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.triage_hint(&email("a@b.example.com", Some("t1")));
        assert!(hint.contains("threads/t1.md"));
    }

    #[test]
    fn triage_hint_stays_short() {
        // Token cost sanity — hint should fit inside ~200 chars per page so
        // triage prompts don't balloon. Two pages max = 400 chars.
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("a@b.example.com"), "# A\n").unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.triage_hint(&email("a@b.example.com", Some("t1")));
        assert!(hint.len() < 400, "triage hint too long: {} chars", hint.len());
    }

    /// Bootstrapped wiki with one kebab-slug people page carrying the given
    /// `identities:` front-matter lines, plus the index built over it.
    fn wiki_with_person(slug: &str, identities: &str) -> (TempDir, WikiLayout, IdentityIndex) {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(
            layout.people_dir().join(format!("{slug}.md")),
            format!("---\nkind: person\nkey: {slug}\nidentities:\n{identities}\n---\n\n# {slug}\n"),
        )
        .unwrap();
        let index = IdentityIndex::build(&layout).unwrap();
        (d, layout, index)
    }

    #[test]
    fn phone_keyed_sender_resolves_via_identity_index() {
        // iMessage backfill (#883) keys people by phone under kebab slugs —
        // the email-slug path can never find them.
        let (_d, layout, index) = wiki_with_person("bob-park", "  phone: [\"+14155550999\"]");
        let r = WikiReader::new(&layout);
        let e = email("Bob Park <+14155550999>", None);
        assert_eq!(r.triage_hint(&e), "", "index-less path must stay byte-identical");
        assert_eq!(r.draft_hint(&e), "");
        let triage = r.triage_hint_with_index(&e, Some(&index));
        assert!(triage.contains("people/bob-park.md"), "got: {triage}");
        assert!(triage.contains("Relationship"));
        assert!(triage.len() < 200, "resolved line too long: {} chars", triage.len());
        let draft = r.draft_hint_with_index(&e, Some(&index));
        assert!(draft.contains("people/bob-park.md"), "got: {draft}");
    }

    #[test]
    fn bare_phone_sender_resolves_via_identity_index() {
        // `channel-imessage` synthesises `from` as the bare handle, no
        // display name.
        let (_d, layout, index) = wiki_with_person("bob-park", "  imessage: [\"+14155550999\"]");
        let r = WikiReader::new(&layout);
        let hint = r.triage_hint_with_index(&email("+14155550999", None), Some(&index));
        assert!(hint.contains("people/bob-park.md"), "got: {hint}");
    }

    #[test]
    fn email_keyed_kebab_page_resolves_only_with_index() {
        let (_d, layout, index) =
            wiki_with_person("jane-doe", "  email: [jane@corp.example.com]");
        let r = WikiReader::new(&layout);
        let e = email("Jane Doe <jane@corp.example.com>", None);
        assert_eq!(r.triage_hint(&e), "");
        assert_eq!(r.triage_hint_with_index(&e, None), "");
        let hint = r.triage_hint_with_index(&e, Some(&index));
        assert!(hint.contains("people/jane-doe.md"), "got: {hint}");
        assert!(!hint.contains("jane_at_corp_example_com"));
    }

    #[test]
    fn index_is_not_consulted_when_email_slug_page_exists() {
        // At most one person line: the email-slug page wins and the index
        // fallback never runs, so a person with both pages isn't double-listed.
        let (_d, layout, index) =
            wiki_with_person("jane-doe", "  email: [jane@corp.example.com]");
        std::fs::write(layout.person_page("jane@corp.example.com"), "# J\n").unwrap();
        let r = WikiReader::new(&layout);
        let e = email("jane@corp.example.com", Some("t1"));
        let hint = r.triage_hint_with_index(&e, Some(&index));
        assert!(hint.contains("people/jane_at_corp_example_com.md"));
        assert!(!hint.contains("people/jane-doe.md"));
        assert_eq!(hint.matches("people/").count(), 1);
    }

    #[test]
    fn unknown_sender_with_index_yields_empty_hint() {
        let (_d, layout, index) = wiki_with_person("bob-park", "  phone: [\"+14155550999\"]");
        let r = WikiReader::new(&layout);
        assert_eq!(
            r.triage_hint_with_index(&email("Stranger <+10000000000>", None), Some(&index)),
            ""
        );
        assert_eq!(
            r.draft_hint_with_index(&email("nobody@example.org", None), Some(&index)),
            ""
        );
    }
}
