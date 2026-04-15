//! Hint strings for drafting-time wiki navigation.
//!
//! The drafting Claude call gets `--add-dir wiki/` + Read/Grep/Glob tools, so
//! it can open any page it wants. `WikiReader` doesn't do the reading — it
//! just produces a short hint string that points at likely-relevant pages, so
//! Claude doesn't have to guess file paths.

use std::path::Path;

use augmentagent_store::Email;

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
        let mut lines: Vec<String> = Vec::new();

        let person_page = self.layout.person_page(&email.from);
        if exists(&person_page) {
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
            message_id: "m1".into(),
            thread_id: thread_id.map(str::to_string),
            from: from.into(),
            subject: "s".into(),
            body: "b".into(),
            date: "2026-04-14".into(),
            account_entity_id: None,
        }
    }

    #[test]
    fn empty_hint_when_no_pages_exist() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        let r = WikiReader::new(&layout);
        assert_eq!(r.draft_hint(&email("a@b.com", Some("t1"))), "");
    }

    #[test]
    fn references_existing_person_page() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("a@b.com"), "# A\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.draft_hint(&email("a@b.com", None));
        assert!(hint.contains("people/a_at_b_com.md"));
    }

    #[test]
    fn references_existing_thread_page() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.draft_hint(&email("a@b.com", Some("t1")));
        assert!(hint.contains("threads/t1.md"));
    }
}
