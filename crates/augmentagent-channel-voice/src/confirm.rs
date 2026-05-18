//! Send a short ack back to the capturing chat so the user knows the memo
//! landed (and what the agent heard). Best-effort: a failed ack must never
//! fail the capture — the memo is already on its way to the wiki.

use tracing::warn;

use crate::extract::MemoRecord;
use crate::telegram::VoiceTelegramClient;

/// Compose the confirmation text. Kept tiny — this is a receipt, not a digest.
pub fn confirm_text(rec: &MemoRecord) -> String {
    let title = if rec.title.trim().is_empty() {
        "Voice memo"
    } else {
        rec.title.trim()
    };
    let mut t = format!("Captured: {title}");
    if !rec.commitments.is_empty() {
        t.push_str(&format!(
            "\nTracking {} commitment(s).",
            rec.commitments.len()
        ));
    }
    t
}

/// Best-effort ack. Returns `false` if the send failed (logged, not fatal).
pub async fn send_confirmation(
    client: &VoiceTelegramClient,
    chat_id: i64,
    rec: &MemoRecord,
) -> bool {
    match client.send_message(chat_id, &confirm_text(rec)).await {
        Ok(()) => true,
        Err(e) => {
            warn!(chat_id, "voice confirm send failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_text_uses_title() {
        let mut r = MemoRecord {
            title: "Call Sam".into(),
            ..Default::default()
        };
        assert_eq!(confirm_text(&r), "Captured: Call Sam");
        r.commitments = vec!["call Sam".into()];
        assert!(confirm_text(&r).contains("1 commitment"));
    }

    #[test]
    fn confirm_text_falls_back_when_no_title() {
        let r = MemoRecord::default();
        assert_eq!(confirm_text(&r), "Captured: Voice memo");
    }
}
