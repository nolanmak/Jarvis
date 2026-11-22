//! Draft archetypes (#36): hot-reloadable prompt fragments + a fast Haiku
//! picker.
//!
//! A long tail of drafts cluster into ~7 archetypes (intro, decline,
//! follow-up, scheduling, thanks, fyi, holding). Each owns a small markdown
//! fragment under `skills/draft-archetypes/<id>.md` (intent + exemplars +
//! slot hints). At draft time, when `AUGMENTAGENT_DRAFT_ARCHETYPES=1`, a fast
//! Haiku call picks the best-fitting archetype; if confidence clears the
//! floor the fragment is composed into `draft_user_message`. Below the floor
//! we fall back to today's behavior (no fragment).
//!
//! Fragments are read at request time (no caching) so the user can edit one
//! mid-day without a rebuild/restart — mirroring the
//! `skills/email-triage/learned/*.json` hot-reload precedent.

use std::path::{Path, PathBuf};

use augmentagent_store::Email;

use crate::reasoner::{archetype_pick_opts, Reasoner};

/// The canonical archetype ids. Order is irrelevant; this is the closed set
/// the picker must choose from (or `none`).
pub const ARCHETYPE_IDS: &[&str] = &[
    "intro",
    "decline",
    "follow-up",
    "scheduling",
    "thanks",
    "fyi",
    "holding",
];

/// Confidence floor (#36 open question: "<0.6 → don't append any archetype").
pub const CONFIDENCE_FLOOR: f64 = 0.6;

/// System prompt for the Haiku archetype picker. Pure classifier — JSON only.
pub const ARCHETYPE_PICKER_SYSTEM: &str = r#"You classify an inbound email into one drafting archetype, to anchor the structure of the reply that will be written next.

Pick exactly one:
- "intro"       — an introduction was made (double opt-in / "thanks for connecting us")
- "decline"     — a job offer, sales pitch, or invitation the recipient will likely turn down
- "follow-up"   — a stalled thread being re-poked, or a follow-up the user owes
- "scheduling"  — proposing / confirming / rescheduling a meeting time
- "thanks"      — acknowledging a delivered doc/answer/favor or a kind note; no action needed
- "fyi"         — a forward purely for visibility; no reply expected
- "holding"     — needs a full reply later; for now acknowledge + commit to a timeframe
- "none"        — does not cleanly fit any of the above

Return ONLY a single JSON object with this exact shape:
  {"archetype": "<one of the ids or none>", "confidence": <0.0-1.0>}

No prose, no markdown fences, no extra fields. Be conservative: when the email
is ambiguous or general-purpose, return "none" with a low confidence.
"#;

/// Outcome of a picker call.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchetypeChoice {
    /// Chosen id, or `None` for "none" / below the confidence floor.
    pub id: Option<String>,
    pub confidence: f64,
}

/// Parse the picker's JSON response. Tolerant of stray prose / fences — we
/// scan for the first `{...}` object. Returns `None` (no archetype) on any
/// parse failure or unknown id, which is the safe fallback (today's behavior).
pub fn parse_choice(raw: &str) -> ArchetypeChoice {
    let none = ArchetypeChoice {
        id: None,
        confidence: 0.0,
    };
    let Some(start) = raw.find('{') else {
        return none;
    };
    let Some(end) = raw.rfind('}') else {
        return none;
    };
    if end < start {
        return none;
    }
    let slice = &raw[start..=end];
    let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) else {
        return none;
    };
    let confidence = v
        .get("confidence")
        .and_then(|c| c.as_f64())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let id = v
        .get("archetype")
        .and_then(|a| a.as_str())
        .map(|s| s.trim().to_ascii_lowercase());
    match id.as_deref() {
        Some("none") | None | Some("") => ArchetypeChoice {
            id: None,
            confidence,
        },
        Some(picked) if ARCHETYPE_IDS.contains(&picked) => ArchetypeChoice {
            id: Some(picked.to_string()),
            confidence,
        },
        // Unknown id from the model — treat as no archetype.
        Some(_) => ArchetypeChoice {
            id: None,
            confidence,
        },
    }
}

/// Default fragment directory, relative to repo root. Override via
/// `AUGMENTAGENT_DRAFT_ARCHETYPES_DIR` (used by tests).
fn fragment_dir() -> PathBuf {
    std::env::var("AUGMENTAGENT_DRAFT_ARCHETYPES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("skills/draft-archetypes"))
}

/// Hot-reload a fragment by id from disk. Read at call time (no cache) so the
/// user can edit a fragment without restarting — mirrors the
/// `skills/email-triage/learned/*.json` reload pattern. Returns `None` if the
/// id is unknown or the file is missing/empty (→ no fragment composed).
pub fn load_fragment(id: &str) -> Option<String> {
    load_fragment_from(&fragment_dir(), id)
}

/// Testable core of [`load_fragment`].
pub fn load_fragment_from(dir: &Path, id: &str) -> Option<String> {
    if !ARCHETYPE_IDS.contains(&id) {
        return None;
    }
    let path = dir.join(format!("{id}.md"));
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => Some(s),
        _ => None,
    }
}

/// Wrap a loaded fragment in the `<draft_archetype>` block that
/// `draft_user_message` composes in. Kept separate so the prompt builder
/// stays a pure string function.
pub fn fragment_block(id: &str, fragment: &str) -> String {
    format!(
        "<draft_archetype id=\"{id}\">\n{}\n</draft_archetype>",
        fragment.trim()
    )
}

/// Run the picker and resolve a composable fragment block, end to end.
///
/// Gated by `AUGMENTAGENT_DRAFT_ARCHETYPES=1` (caller may also check). Returns
/// `String::new()` — meaning "no archetype, today's behavior" — when: the flag
/// is off, the picker errors, the choice is `none`, confidence is below
/// [`CONFIDENCE_FLOOR`], or the fragment file is missing. The picker's choice
/// is always logged so hit-rate can be evaluated against approval outcomes.
pub async fn resolve_archetype_block<R: Reasoner + ?Sized>(
    reasoner: &R,
    email: &Email,
    triage_label: &str,
) -> String {
    if std::env::var("AUGMENTAGENT_DRAFT_ARCHETYPES").as_deref() != Ok("1") {
        return String::new();
    }
    let opts = archetype_pick_opts();
    let user_msg = format!(
        "Triage label: {triage_label}\n\n<email>\nFrom: {from}\nSubject: {subject}\n\n{body}\n</email>\n",
        from = email.from,
        subject = email.subject,
        // Cap the body the picker sees — classification doesn't need the full
        // text and Haiku latency scales with input.
        body = truncate_for_picker(&email.body, 4_000),
    );
    let choice = match reasoner.call(&opts, &user_msg).await {
        Ok(raw) => parse_choice(&raw),
        Err(e) => {
            tracing::warn!(
                message_id = %email.message_id,
                "archetype picker call failed, drafting without archetype: {e}"
            );
            return String::new();
        }
    };
    match &choice.id {
        Some(id) if choice.confidence >= CONFIDENCE_FLOOR => {
            match load_fragment(id) {
                Some(frag) => {
                    tracing::info!(
                        message_id = %email.message_id,
                        archetype = %id,
                        confidence = choice.confidence,
                        "archetype picker: composing fragment"
                    );
                    fragment_block(id, &frag)
                }
                None => {
                    tracing::warn!(
                        message_id = %email.message_id,
                        archetype = %id,
                        "archetype picker chose {id} but fragment missing; drafting without it"
                    );
                    String::new()
                }
            }
        }
        Some(id) => {
            tracing::info!(
                message_id = %email.message_id,
                archetype = %id,
                confidence = choice.confidence,
                floor = CONFIDENCE_FLOOR,
                "archetype picker: below confidence floor, no archetype"
            );
            String::new()
        }
        None => {
            tracing::info!(
                message_id = %email.message_id,
                confidence = choice.confidence,
                "archetype picker: none, no archetype"
            );
            String::new()
        }
    }
}

fn truncate_for_picker(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_json() {
        let c = parse_choice(r#"{"archetype": "decline", "confidence": 0.82}"#);
        assert_eq!(c.id.as_deref(), Some("decline"));
        assert!((c.confidence - 0.82).abs() < 1e-9);
    }

    #[test]
    fn parse_tolerates_fences_and_prose() {
        let c = parse_choice("Sure!\n```json\n{\"archetype\":\"thanks\",\"confidence\":0.9}\n```");
        assert_eq!(c.id.as_deref(), Some("thanks"));
    }

    #[test]
    fn parse_none_returns_no_id() {
        let c = parse_choice(r#"{"archetype":"none","confidence":0.2}"#);
        assert_eq!(c.id, None);
    }

    #[test]
    fn parse_unknown_id_is_no_archetype() {
        let c = parse_choice(r#"{"archetype":"banana","confidence":0.99}"#);
        assert_eq!(c.id, None);
    }

    #[test]
    fn parse_garbage_is_safe() {
        assert_eq!(parse_choice("not json at all").id, None);
        assert_eq!(parse_choice("").id, None);
    }

    #[test]
    fn load_fragment_rejects_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bogus.md"), "hi").unwrap();
        assert!(load_fragment_from(dir.path(), "bogus").is_none());
    }

    #[test]
    fn load_fragment_reads_known_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("decline.md"), "# Decline\nbody").unwrap();
        let got = load_fragment_from(dir.path(), "decline").unwrap();
        assert!(got.contains("Decline"));
    }

    #[test]
    fn load_fragment_empty_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fyi.md"), "   \n").unwrap();
        assert!(load_fragment_from(dir.path(), "fyi").is_none());
    }

    #[test]
    fn fragment_block_wraps_with_id() {
        let b = fragment_block("intro", "  intent text  ");
        assert!(b.starts_with("<draft_archetype id=\"intro\">"));
        assert!(b.trim_end().ends_with("</draft_archetype>"));
        assert!(b.contains("intent text"));
    }
}
