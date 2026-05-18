//! Quick-refine presets for the approval card (#34).
//!
//! Each preset is a short, stable `id` paired with a human label (shown in the
//! Discord `StringSelect`) and a canned feedback string. The feedback string
//! is routed through the SAME `redraft_message` path the free-form Revise
//! modal uses — there is no new LLM call shape; we're only removing the typing
//! step for the ~6 nudges that recur on >80% of revised drafts.
//!
//! Ids are kept short and `[a-z-]` so they fit comfortably inside the
//! `custom_id` budget when embedded as the preset selector value.

/// A single canned refinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// Stable id; persisted to `actions.lastPresetId` for analytics and used
    /// as the `StringSelect` option value.
    pub id: &'static str,
    /// Label shown in the dropdown.
    pub label: &'static str,
    /// Canned feedback fed verbatim into `redraft_message`.
    pub feedback: &'static str,
}

/// The canned preset table. Order here is the order shown in the dropdown.
/// 11 entries — under Discord's 25-option `StringSelect` ceiling.
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "shorter",
        label: "Shorter",
        feedback: "Make this roughly 30% shorter. Cut filler and hedging, but preserve every commitment, date, and the closing question.",
    },
    Preset {
        id: "longer",
        label: "Longer / more detail",
        feedback: "Expand this with one or two more sentences of useful specificity. Do not pad — add substance, not filler.",
    },
    Preset {
        id: "warmer",
        label: "Warmer",
        feedback: "Make the tone warmer and more personable while keeping it concise. A friendlier opener and closer; do not become effusive.",
    },
    Preset {
        id: "formal",
        label: "More formal",
        feedback: "Make this more formal and professional. Remove contractions and casual phrasing; keep it crisp, not stiff.",
    },
    Preset {
        id: "casual",
        label: "More casual",
        feedback: "Make this more casual and conversational, like a quick note to a peer. Contractions are fine; drop any stiffness.",
    },
    Preset {
        id: "add-calendar-link",
        label: "Add calendar link",
        feedback: "Add a sentence offering to meet and pointing to my scheduling link. Use the placeholder {CALENDAR_LINK} for the URL — do not invent one.",
    },
    Preset {
        id: "decline-politely",
        label: "Decline politely",
        feedback: "Reframe this as a polite decline. Acknowledge specifically what was offered, decline cleanly without over-explaining, and leave the door open only briefly.",
    },
    Preset {
        id: "ack-defer",
        label: "Acknowledge + defer",
        feedback: "Make this a short acknowledgement that defers a full response. Confirm receipt, signal it matters, and commit to a concrete follow-up timeframe.",
    },
    Preset {
        id: "ask-clarifying",
        label: "Ask one clarifying question",
        feedback: "Trim this to a brief reply that asks exactly one focused clarifying question needed before I can respond properly. Do not ask more than one.",
    },
    Preset {
        id: "fix-grammar",
        label: "Tighten grammar/clarity",
        feedback: "Fix any grammar, punctuation, or awkward phrasing. Do not change the meaning, tone, or length materially.",
    },
    Preset {
        id: "remove-fluff",
        label: "Remove fluff / get to the point",
        feedback: "Strip throat-clearing and pleasantries down to the essential point. Lead with the answer or the ask.",
    },
];

/// Hard cap on stacked preset/redraft iterations per action (#34 Phase 2).
/// After this many redrafts the card stops offering the menu and the user
/// must Approve / Skip / free-form Revise.
pub const MAX_REDRAFT_ITERATIONS: i64 = 5;

/// Look up a preset by id. `None` for unknown ids (stale card, bad input).
pub fn lookup(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_url_safe() {
        let mut seen = std::collections::HashSet::new();
        for p in PRESETS {
            assert!(seen.insert(p.id), "duplicate preset id: {}", p.id);
            assert!(
                p.id.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "preset id not [a-z-]: {}",
                p.id
            );
            assert!(!p.label.is_empty());
            assert!(!p.feedback.is_empty());
        }
    }

    #[test]
    fn within_discord_select_ceiling() {
        // Discord StringSelect allows max 25 options.
        assert!(PRESETS.len() <= 25, "too many presets: {}", PRESETS.len());
        assert!(!PRESETS.is_empty());
    }

    #[test]
    fn lookup_roundtrips_every_preset() {
        for p in PRESETS {
            let got = lookup(p.id).expect("preset should resolve");
            assert_eq!(got.id, p.id);
            assert_eq!(got.feedback, p.feedback);
        }
        assert!(lookup("definitely-not-a-preset").is_none());
    }
}
