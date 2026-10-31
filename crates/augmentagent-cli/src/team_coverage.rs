//! #1700 — the reconcile sweep's teammate-coverage rule for email cards.
//!
//! A pending email card C (inbound I from asker S, inbound time t0) is
//! *covered* when some stored message M satisfies ALL of:
//! 1. M is on C's thread in the same mailbox;
//! 2. M's sender classifies as a rostered teammate (#1698);
//! 3. M arrived strictly after t0;
//! 4. S is in M's To ∪ Cc — the teammate answered the asker, not just the
//!    owner internally;
//! 5. M's new text (quoted history stripped) names no owner alias — a
//!    teammate handing the request back ("<owner>, can you take this?") must
//!    not retire the owner's card;
//! 6. M's recipients are known (#1699). Unknown never covers.
//!
//! Inert unless the roster AND the owner aliases are configured: without
//! aliases condition 5 cannot be checked, so the rule refuses to run rather
//! than retire hand-backs. `AUGMENTAGENT_TEAM_COVERAGE` selects
//! `off | shadow | on` (default `shadow`: decisions are recorded and logged,
//! nothing is retired).

use augmentagent_messages::team::{OwnerIdentities, TeamRoster};
use augmentagent_store::{PendingActionRow, Store};

pub const MODE_ENV: &str = "AUGMENTAGENT_TEAM_COVERAGE";
pub const ALIASES_ENV: &str = "AUGMENTAGENT_OWNER_ALIASES";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Shadow,
    On,
}

impl Mode {
    /// Unset or unrecognised → `Shadow`: a typo must never switch it on.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|r| r.trim().to_ascii_lowercase()).as_deref() {
            Some("on") => Mode::On,
            Some("off") => Mode::Off,
            _ => Mode::Shadow,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Shadow => "shadow",
            Mode::On => "on",
        }
    }
}

pub struct Config {
    pub mode: Mode,
    pub roster: TeamRoster,
    pub owner: OwnerIdentities,
    pub aliases: Vec<String>,
}

impl Config {
    /// From the environment plus the owner's connected mailboxes.
    pub fn from_env(store: &Store) -> Self {
        let mut owner_ids: Vec<String> = Vec::new();
        match store.get_active_gmail_accounts() {
            Ok(accounts) => {
                for a in accounts {
                    owner_ids.push(a.entity_id);
                    if !a.email.is_empty() {
                        owner_ids.push(a.email);
                    }
                }
            }
            Err(e) => tracing::warn!("team coverage: loading owner accounts failed: {e:#}"),
        }
        Config {
            mode: Mode::parse(std::env::var(MODE_ENV).ok().as_deref()),
            roster: TeamRoster::from_env(),
            owner: OwnerIdentities::new(owner_ids),
            aliases: std::env::var(ALIASES_ENV)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(String::from)
                .collect(),
        }
    }

    /// Whether the rule may run at all.
    pub fn active(&self) -> bool {
        self.mode != Mode::Off && !self.roster.is_empty() && !self.aliases.is_empty()
    }
}

/// The message that covered a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub teammate: String,
    pub covering_message_id: String,
    pub covered_at_ms: i64,
}

/// Body with quoted reply history removed (Gmail `On … wrote:`, `>` lines,
/// Outlook `-----Original Message-----` / `From:`+`Sent:` headers).
pub fn new_text(body: &str) -> String {
    let text = augmentagent_channel_email::sigextract::strip_quoted_reply(body);
    let lines: Vec<&str> = text.lines().collect();
    let cut = lines.iter().enumerate().position(|(i, l)| {
        let l = l.trim();
        l.starts_with("-----Original Message-----")
            || l.starts_with("________________________________")
            || (l.starts_with("From:")
                && lines.get(i + 1).is_some_and(|n| n.trim_start().starts_with("Sent:")))
    });
    lines[..cut.unwrap_or(lines.len())].join("\n").trim().to_string()
}

/// Whole-word, case-insensitive alias match (multi-word aliases allowed).
pub fn mentions_alias(text: &str, aliases: &[String]) -> bool {
    let words = |s: &str| -> String {
        let normalized: String = s
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect();
        format!(" {} ", normalized.split_whitespace().collect::<Vec<_>>().join(" "))
    };
    let haystack = words(text);
    aliases.iter().any(|a| {
        let needle = words(a);
        needle.trim() != "" && haystack.contains(&needle)
    })
}

/// Conditions 1–6 for one card. `after_ms` is the card's inbound bound
/// (receivedAt, else createdAt — the same bound Rule 1 uses).
pub fn find_coverage(
    store: &Store,
    row: &PendingActionRow,
    after_ms: i64,
    cfg: &Config,
) -> anyhow::Result<Option<Coverage>> {
    use augmentagent_channel_email::outbound::parse_rfc2822_or_ms;
    use augmentagent_messages::team::TeamClass;

    let (Some(thread), Some(account)) = (row.thread_id.as_deref(), row.account_entity_id.as_deref())
    else {
        return Ok(None);
    };
    let Some(asker) = augmentagent_store::recipients::parse_addresses(&row.from_email)
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let mut best: Option<Coverage> = None;
    for m in store.thread_messages_in_mailbox(thread, account)? {
        let TeamClass::Teammate { label } = cfg.roster.classify(&m.from, &cfg.owner) else {
            continue;
        };
        let at = parse_rfc2822_or_ms(&m.received_at);
        if at <= 0 || at <= after_ms {
            continue;
        }
        let Some(r) = &m.recipients else {
            continue;
        };
        if !r.to.iter().chain(&r.cc).any(|a| *a == asker) {
            continue;
        }
        if mentions_alias(&new_text(&m.body), &cfg.aliases) {
            continue;
        }
        // Earliest covering message: the moment the asker was answered.
        if best.as_ref().is_none_or(|b| at < b.covered_at_ms) {
            best = Some(Coverage { teammate: label, covering_message_id: m.message_id, covered_at_ms: at });
        }
    }
    Ok(best)
}

// ---- #1702: shadow report, review and the objective rollout gate ----

/// Totals behind the rollout gate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub decisions: usize,
    pub reviewed: usize,
    pub wrong: usize,
    pub replied_anyway: usize,
    /// Earliest decision in the window; shadow mode started no later.
    pub first_decided_ms: Option<i64>,
}

pub fn summarize(rows: &[augmentagent_store::team_coverage::TeamCoverageReportRow]) -> Summary {
    let _ = rows;
    Summary::default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub pass: bool,
    /// Every failed criterion, in a fixed order; empty when `pass`.
    pub reasons: Vec<String>,
}

pub const GATE_MIN_DAYS: i64 = 7;
pub const GATE_MIN_DECISIONS: usize = 10;
pub const GATE_SPARSE_MIN_DAYS: i64 = 14;

/// The #1702 gate for switching the default from `shadow` to `on`.
pub fn gate(s: &Summary, now_ms: i64) -> Gate {
    let _ = (s, now_ms);
    Gate { pass: false, reasons: vec![] }
}

/// One-line health summary (autopr-health style).
pub fn summary_line(s: &Summary, g: &Gate) -> String {
    let _ = (s, g);
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::{ActionStatus, Email};

    const ASKER: &str = "Dana Rivera <dana@labs.example.com>"; // pii-ok: synthetic
    const T0: &str = "2026-01-01T00:00:00Z";
    const AFTER: &str = "2026-01-02T00:00:00Z";

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data.db")).unwrap();
        (dir, store)
    }

    fn cfg(mode: Mode) -> Config {
        Config {
            mode,
            roster: TeamRoster::parse("Ana=ana@team.example.net"),
            owner: OwnerIdentities::new(["owner@example.com"]),
            aliases: vec!["Sam".into(), "Sam Owner".into()],
        }
    }

    fn upsert(store: &Store, id: &str, from: &str, to: &str, cc: &str, date: &str, body: &str) {
        store
            .upsert_email(&Email {
                message_id: id.into(),
                thread_id: Some("T1".into()),
                from: from.into(),
                to: to.into(),
                cc: cc.into(),
                attachments: Vec::new(),
                subject: "Proposal".into(),
                body: body.into(),
                date: date.into(),
                account_entity_id: Some("acc".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            })
            .unwrap();
    }

    /// The owner's pending card for the asker's inbound at T0.
    fn seed_card(store: &Store, id: &str, date: &str) -> String {
        upsert(store, id, ASKER, "owner@example.com", "", date, "Can you send the proposal?");
        store
            .log_action(id, Some("T1"), ASKER, "Proposal", Some("Can you send the proposal?"), Some("a draft"), ActionStatus::Pending)
            .unwrap()
    }

    fn teammate_reply(store: &Store, to: &str, date: &str, body: &str) {
        upsert(store, "m-team", "Ana Example <ana@team.example.net>", to, "", date, body);
    }

    fn status(store: &Store, id: &str) -> (String, Option<String>) {
        store
            .with_conn(|c| {
                c.query_row(
                    "SELECT status, errorMessage FROM actions WHERE id = ?1",
                    augmentagent_store::rusqlite::params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .unwrap()
    }

    fn tick(store: &Store, c: &Config) -> usize {
        crate::reconcile_stale_approvals_tick_with(store, c).unwrap()
    }

    #[test]
    fn teammate_reply_to_asker_retires_card() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", T0);
        teammate_reply(&store, "dana@labs.example.com, owner@example.com", AFTER, "Attached, Dana!"); // pii-ok
        assert_eq!(tick(&store, &cfg(Mode::On)), 1);
        assert_eq!(
            status(&store, &card),
            ("superseded".into(), Some("superseded: covered by teammate Ana".into()))
        );
        let decisions = store.team_coverage_decisions(0).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!((decisions[0].mode.as_str(), decisions[0].teammate.as_str()), ("on", "Ana"));
    }

    #[test]
    fn group_participant_reply_does_not_retire() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", T0);
        upsert(&store, "m-other", "Eli <eli@vendor.example.org>", "dana@labs.example.com", "", AFTER, "Sending it now"); // pii-ok
        assert_eq!(tick(&store, &cfg(Mode::On)), 0);
        assert_eq!(status(&store, &card).0, "pending");
    }

    #[test]
    fn teammate_message_before_inbound_does_not_retire() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", AFTER);
        teammate_reply(&store, "dana@labs.example.com", T0, "Earlier note"); // pii-ok
        assert_eq!(tick(&store, &cfg(Mode::On)), 0);
        // Equal timestamps do not retire either (strictly after).
        teammate_reply(&store, "dana@labs.example.com", AFTER, "Same instant"); // pii-ok
        assert_eq!(tick(&store, &cfg(Mode::On)), 0);
        assert_eq!(status(&store, &card).0, "pending");
    }

    #[test]
    fn teammate_internal_forward_to_owner_does_not_retire() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", T0);
        teammate_reply(&store, "owner@example.com", AFTER, "FYI, see below");
        assert_eq!(tick(&store, &cfg(Mode::On)), 0);
        assert_eq!(status(&store, &card).0, "pending");
    }

    #[test]
    fn teammate_hands_back_to_owner_does_not_retire() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", T0);
        teammate_reply(&store, "dana@labs.example.com, owner@example.com", AFTER, "sam, can you take this one?"); // pii-ok
        assert_eq!(tick(&store, &cfg(Mode::On)), 0);
        assert_eq!(status(&store, &card).0, "pending");
    }

    #[test]
    fn alias_only_in_quoted_history_still_retires() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", T0);
        teammate_reply(
            &store,
            "dana@labs.example.com", // pii-ok
            AFTER,
            "Handled, attached.\n\nOn Thu, Jan 1, 2026 Sam Owner <owner@example.com> wrote:\n> Sam here",
        );
        assert_eq!(tick(&store, &cfg(Mode::On)), 1);
        assert_eq!(status(&store, &card).0, "superseded");
    }

    #[test]
    fn unknown_recipients_do_not_retire() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", T0);
        teammate_reply(&store, "", AFTER, "Attached");
        assert_eq!(tick(&store, &cfg(Mode::On)), 0);
        assert_eq!(status(&store, &card).0, "pending");
    }

    #[test]
    fn newer_inbound_card_survives() {
        let (_d, store) = temp_store();
        let older = seed_card(&store, "m-in", T0);
        teammate_reply(&store, "dana@labs.example.com", AFTER, "Attached"); // pii-ok
        let newer = seed_card(&store, "m-in-2", "2026-01-03T00:00:00Z");
        assert_eq!(tick(&store, &cfg(Mode::On)), 1);
        assert_eq!(status(&store, &older).0, "superseded");
        assert_eq!(status(&store, &newer).0, "pending");
    }

    #[test]
    fn recomposed_card_is_never_re_retired() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", T0);
        teammate_reply(&store, "dana@labs.example.com", AFTER, "Attached"); // pii-ok
        assert_eq!(tick(&store, &cfg(Mode::On)), 1);
        assert!(store.recompose_action(&card, "test").unwrap());
        assert_eq!(tick(&store, &cfg(Mode::On)), 0, "owner restored it; never re-retire");
        assert_eq!(status(&store, &card).0, "pending");
    }

    #[test]
    fn shadow_mode_logs_but_changes_nothing() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", T0);
        teammate_reply(&store, "dana@labs.example.com", AFTER, "Attached"); // pii-ok
        assert_eq!(tick(&store, &cfg(Mode::Shadow)), 0);
        assert_eq!(status(&store, &card).0, "pending");
        let d = store.team_coverage_decisions(0).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].action_id.as_str(), d[0].mode.as_str()), (card.as_str(), "shadow"));
        assert_eq!(d[0].covering_message_id, "m-team");
    }

    #[test]
    fn off_mode_and_missing_config_are_inert() {
        let (_d, store) = temp_store();
        let card = seed_card(&store, "m-in", T0);
        teammate_reply(&store, "dana@labs.example.com", AFTER, "Attached"); // pii-ok
        assert_eq!(tick(&store, &cfg(Mode::Off)), 0);
        let no_aliases = Config { aliases: vec![], ..cfg(Mode::On) };
        assert!(!no_aliases.active());
        assert_eq!(tick(&store, &no_aliases), 0);
        let no_roster = Config { roster: TeamRoster::parse(""), ..cfg(Mode::On) };
        assert_eq!(tick(&store, &no_roster), 0);
        assert_eq!(status(&store, &card).0, "pending");
        assert!(store.team_coverage_decisions(0).unwrap().is_empty());
    }

    #[test]
    fn mode_parse_defaults_to_shadow() {
        assert_eq!(Mode::parse(None), Mode::Shadow);
        assert_eq!(Mode::parse(Some(" ON ")), Mode::On);
        assert_eq!(Mode::parse(Some("off")), Mode::Off);
        assert_eq!(Mode::parse(Some("bogus")), Mode::Shadow);
    }

    #[test]
    fn new_text_strips_gmail_and_outlook_history() {
        assert_eq!(new_text("Done.\nOn Mon, X wrote:\n> old"), "Done.");
        assert_eq!(new_text("Done.\n-----Original Message-----\nFrom: Sam"), "Done.");
        assert_eq!(new_text("Done.\nFrom: Sam Owner\nSent: Monday\nTo: Ana"), "Done.");
        assert_eq!(new_text("Done.\n> quoted\nmore"), "Done.\nmore");
    }

    #[test]
    fn mentions_alias_is_whole_word_and_case_insensitive() {
        let a = vec!["Sam".to_string(), "Sam Owner".to_string()];
        assert!(mentions_alias("hey SAM, thoughts?", &a));
        assert!(mentions_alias("cc sam owner.", &a));
        assert!(!mentions_alias("same here, samples attached", &a));
    }
    // ---- #1702 ----

    const DAY: i64 = 86_400_000;

    fn summary(decisions: usize, reviewed: usize, wrong: usize, anyway: usize, age_days: i64) -> (Summary, i64) {
        let now = 100 * DAY;
        (
            Summary { decisions, reviewed, wrong, replied_anyway: anyway, first_decided_ms: Some(now - age_days * DAY) },
            now,
        )
    }

    #[test]
    fn gate_check_passes_when_all_criteria_met() {
        let (s, now) = summary(10, 10, 0, 0, 7);
        assert_eq!(gate(&s, now), Gate { pass: true, reasons: vec![] });
        // Fewer than 10 decisions is fine once 14 days have passed.
        let (s, now) = summary(3, 3, 0, 0, 14);
        assert!(gate(&s, now).pass);
    }

    #[test]
    fn gate_check_fails_on_any_wrong_or_replied_anyway() {
        let (s, now) = summary(12, 12, 1, 0, 9);
        assert_eq!(gate(&s, now).reasons, vec!["1 decision(s) marked wrong".to_string()]);
        let (s, now) = summary(12, 12, 0, 2, 9);
        assert_eq!(gate(&s, now).reasons, vec!["owner replied anyway on 2 decision(s)".to_string()]);
    }

    #[test]
    fn gate_check_fails_on_time_volume_and_review_coverage() {
        let (s, now) = summary(10, 10, 0, 0, 6);
        assert_eq!(gate(&s, now).reasons, vec!["shadow for 6 day(s); need 7".to_string()]);
        let (s, now) = summary(4, 4, 0, 0, 9);
        assert_eq!(gate(&s, now).reasons, vec!["only 4 decision(s); need 10, or 14 days of shadow".to_string()]);
        let (s, now) = summary(10, 9, 0, 0, 8);
        assert_eq!(gate(&s, now).reasons, vec!["9/10 decision(s) reviewed; need all".to_string()]);
        let none = Summary::default();
        assert_eq!(gate(&none, 100 * DAY).reasons, vec!["no shadow decisions yet".to_string()]);
    }

    #[test]
    fn report_rows_summarize_and_render_a_summary_line() {
        use augmentagent_store::team_coverage::{TeamCoverageDecision, TeamCoverageReportRow};
        let row = |id: &str, decided: i64, review: Option<&str>, anyway: bool| TeamCoverageReportRow {
            decision: TeamCoverageDecision {
                action_id: id.into(),
                mode: "shadow".into(),
                teammate: "Ana".into(),
                covering_message_id: "m".into(),
                thread_id: "T".into(),
                covered_at_ms: decided - 1,
                decided_at_ms: decided,
            },
            platform: "gmail".into(),
            owner_replied_anyway: anyway,
            review: review.map(String::from),
        };
        let rows = vec![row("a", 5 * DAY, Some("correct"), false), row("b", 3 * DAY, Some("wrong"), true), row("c", 4 * DAY, None, false)];
        let s = summarize(&rows);
        assert_eq!(
            s,
            Summary { decisions: 3, reviewed: 2, wrong: 1, replied_anyway: 1, first_decided_ms: Some(3 * DAY) }
        );
        let g = gate(&s, 20 * DAY);
        assert_eq!(
            summary_line(&s, &g),
            "team-coverage: 3 decision(s), 2 reviewed, 1 wrong, 1 replied-anyway, shadow 17 day(s) — gate FAIL"
        );
    }

}
