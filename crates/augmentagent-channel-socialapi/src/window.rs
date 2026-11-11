//! Instagram's 24-hour reply window (#1982).
//!
//! A business account may message someone only within 24 hours of that
//! person's last message. Past that, SocialAPI.ai refuses the send with
//! `platform.instagram.window_closed`. The rule is Instagram's and can't be
//! lifted here, so the daemon checks it before drafting or sending and, when
//! it is closed, tells the owner how to reply by hand.

use augmentagent_store::rusqlite::params;
use augmentagent_store::Store;
use chrono::{DateTime, Duration, Utc};

/// How long after the person's last message Instagram accepts a reply.
pub const INSTAGRAM_REPLY_WINDOW: Duration = Duration::hours(24);

/// Instagram's reply window for one SocialAPI conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyWindow {
    /// The other party as the inbox shows them (a username or display name).
    pub with: String,
    /// Newest message the other party sent.
    pub last_inbound: DateTime<Utc>,
}

impl ReplyWindow {
    pub fn closes_at(&self) -> DateTime<Utc> {
        self.last_inbound + INSTAGRAM_REPLY_WINDOW
    }

    pub fn is_closed(&self, now: DateTime<Utc>) -> bool {
        now >= self.closes_at()
    }

    /// The window for `conversation_id` from stored rows, or `None` when the
    /// conversation isn't Instagram or no inbound message is stored (then
    /// the caller should just try the send).
    pub fn for_conversation(store: &Store, conversation_id: &str) -> Option<Self> {
        let rows: Vec<(String, String, Option<String>)> = store
            .with_conn(|c| {
                let mut stmt = c.prepare(
                    "SELECT fromEmail, subject, receivedAt FROM emails \
                     WHERE platform = 'socialapi' AND threadId = ?1",
                )?;
                let rows = stmt.query_map(params![conversation_id], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?;
                rows.collect()
            })
            .ok()?;
        let instagram = rows.iter().any(|(from, subject, _)| {
            from.contains("<socialapi:instagram") || subject.starts_with("[Instagram DM")
        });
        if !instagram {
            return None;
        }
        // Inbound = a row the DM source ingested: carries the network handle.
        // Owner composes (`[… DM from <name>]` with no handle) never count.
        rows.into_iter()
            .filter(|(from, _, _)| from.contains("<socialapi:instagram:"))
            .filter_map(|(from, _, at)| {
                let at = DateTime::parse_from_rfc3339(at?.trim()).ok()?.with_timezone(&Utc);
                Some((display_name(&from), at))
            })
            .max_by_key(|(_, at)| *at)
            .map(|(with, last_inbound)| Self { with, last_inbound })
    }
}

/// `"jane <socialapi:instagram:1>"` → `"jane"`.
fn display_name(from: &str) -> String {
    from.split_once(" <")
        .map(|(n, _)| n)
        .unwrap_or(from)
        .trim()
        .to_string()
}

/// Whether a send error is Instagram's closed-window refusal.
pub fn is_window_closed_error(err: &str) -> bool {
    err.contains("window_closed") || err.contains("2534022")
}

/// Where the owner can reply by hand: the person's thread when `with` is a
/// username, otherwise the Instagram inbox.
pub fn instagram_reply_link(with: &str) -> String {
    let u = with.trim().trim_start_matches('@');
    let username = !u.is_empty()
        && u.len() <= 30
        && u.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
    if username {
        format!("https://ig.me/m/{u}")
    } else {
        "https://www.instagram.com/direct/inbox/".to_string()
    }
}

/// Owner-facing explanation for a reply Instagram won't accept.
pub fn window_closed_message(with: &str, closed_at: Option<DateTime<Utc>>, draft: &str) -> String {
    let who = if with.trim().is_empty() { "this person" } else { with.trim() };
    let when = closed_at
        .map(|t| format!(" The window closed {} UTC.", t.format("%Y-%m-%d %H:%M")))
        .unwrap_or_default();
    format!(
        "Instagram only allows replies within 24 hours of {who}'s last message, so nothing was sent.{when} \
         Reply in the Instagram app instead: {link}\n\nDraft to copy:\n{draft}",
        link = instagram_reply_link(with),
        draft = draft.trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::Email;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn put(s: &Store, id: &str, thread: &str, from: &str, subject: &str, at: &str) {
        s.upsert_email(&Email {
            message_id: id.into(),
            thread_id: Some(thread.into()),
            from: from.into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: subject.into(),
            body: "hi".into(),
            date: at.into(),
            account_entity_id: Some("acc-1".into()),
            platform: "socialapi".into(),
            kind: "dm".into(),
        })
        .unwrap();
    }

    #[test]
    fn window_closes_24_hours_after_the_last_inbound_message() {
        let w = ReplyWindow { with: "jane".into(), last_inbound: t("2026-01-01T10:00:00Z") };
        assert!(!w.is_closed(t("2026-01-02T09:59:00Z")));
        assert!(w.is_closed(t("2026-01-02T10:01:00Z")));
        assert_eq!(w.closes_at(), t("2026-01-02T10:00:00Z"));
    }

    #[test]
    fn window_comes_from_newest_inbound_row_ignoring_composes() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("t.db")).unwrap();
        put(&s, "m1", "c1", "jane_ig <socialapi:instagram:1>", "[Instagram DM from jane_ig]", "2026-01-01T10:00:00Z");
        put(&s, "m2", "c1", "jane_ig <socialapi:instagram:1>", "[Instagram DM from jane_ig]", "2026-01-03T08:00:00Z");
        put(&s, "compose:socialapi:dm:c1", "c1", "Jane", "[Instagram DM from Jane]", "2026-01-05T00:00:00Z");
        let w = ReplyWindow::for_conversation(&s, "c1").unwrap();
        assert_eq!(w.with, "jane_ig");
        assert_eq!(w.last_inbound, t("2026-01-03T08:00:00Z"));
        // Not Instagram → no rule; nothing stored → unknown.
        put(&s, "x1", "c2", "pat <socialapi:x:9>", "[X DM from pat]", "2026-01-01T10:00:00Z");
        assert!(ReplyWindow::for_conversation(&s, "c2").is_none());
        assert!(ReplyWindow::for_conversation(&s, "missing").is_none());
    }

    #[test]
    fn classifies_the_real_window_closed_refusal() {
        let real = r#"socialapi 400 Bad Request: {"error":{"code":"platform.instagram.window_closed","message":"24-hour messaging window has closed, the user must message you first to reopen it","meta":{"failed_index":0,"platform":"instagram","platform_code":10,"platform_subcode":2534022,"sent_count":0}}}"#;
        assert!(is_window_closed_error(real));
        assert!(!is_window_closed_error(
            r#"socialapi 400 Bad Request: {"error":{"code":"validation.text_too_long"}}"#
        ));
    }

    #[test]
    fn message_explains_links_and_keeps_the_draft() {
        let m = window_closed_message("jane_ig", Some(t("2026-01-04T08:00:00Z")), "  see you friday!\n");
        assert!(m.contains("jane_ig"), "{m}");
        assert!(m.contains("nothing was sent"), "{m}");
        assert!(m.contains("2026-01-04 08:00 UTC"), "{m}");
        assert!(m.contains("https://ig.me/m/jane_ig"), "{m}");
        assert!(m.ends_with("see you friday!"), "{m}");
        let m = window_closed_message("Jane Doe", None, "x");
        assert!(m.contains("https://www.instagram.com/direct/inbox/"), "{m}");
        assert!(!m.contains("ig.me"), "{m}");
    }
}
