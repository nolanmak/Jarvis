//! Issue #2 — `augmentagent channel <name> <op> [args…]` router.
//!
//! Phase 1 of the /setup skill + maintenance surface. Lets the skill stop
//! hand-coding "is it `gmail login` or `slack add-workspace`?" — one shape
//! for every channel.
//!
//! The router is a *thin alias* over the existing per-channel subcommands:
//! `augmentagent channel gmail status --json` is byte-identical to
//! `augmentagent gmail status --json`. We achieve that without touching any
//! per-channel handler by re-invoking the current binary with the rewritten
//! argv. That keeps this file inside its allowlist (NEW file + main.rs only)
//! and guarantees parity by construction — there's only one code path.
//!
//! Op coverage today: `Arm | Disarm | Login | PollOnce | Recent | Status |
//! Validate`. `arm/disarm` (#7) flip per-channel arming flags in the sqlite
//! `config` table — the same row the dashboard's `getConfig()` reads (see
//! `src/dashboard.ts:78` `getConfigStatus()`) so the CLI and dashboard agree
//! on which channels are live. The daemon picks the flip up on next restart;
//! the `arm` op emits JSON the `/setup` skill keys off (`restart_required`,
//! `restart_cmd`) so it can offer the restart inline.
//!
//! Channels without an arming gate (e.g. `gmail`, which is on-by-default once
//! Composio is connected) return a clean `"channel 'X' has no arming gate"`
//! error rather than no-op silently. Other ops a channel doesn't expose
//! return the existing "does not support op" error.

use anyhow::{bail, Context, Result};
use augmentagent_store::rusqlite;
use clap::ValueEnum;
use serde_json::json;
use std::process::Stdio;
use tokio::process::Command;

/// Every channel the daemon knows about. Order matches the alphabetical layout
/// of `main.rs`' `Cmd::*` variants so `--help` reads naturally.
///
/// Keep this in sync with the per-channel `Cmd::*` variants in `main.rs`. When
/// a new channel ships, add it here AND map at least its `Status` (or another
/// inspection-only op) into `dispatch` below.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ChannelName {
    Browser,
    Calendar,
    Compose,
    Contacts,
    Discord,
    Gdrive,
    Github,
    Gmail,
    Linkedin,
    Meetup,
    Proactive,
    Reddit,
    Slack,
    TelegramBot,
    Twitter,
    Voice,
    Whatsapp,
}

impl ChannelName {
    /// The top-level `augmentagent <subcommand>` token this channel routes to.
    /// (`telegram-bot`, not `telegrambot`; clap derive lowercases kebab-style.)
    pub fn as_subcommand(self) -> &'static str {
        match self {
            ChannelName::Browser => "browser",
            ChannelName::Calendar => "calendar",
            ChannelName::Compose => "compose",
            ChannelName::Contacts => "contacts",
            ChannelName::Discord => "discord",
            ChannelName::Gdrive => "gdrive",
            ChannelName::Github => "github",
            ChannelName::Gmail => "gmail",
            ChannelName::Linkedin => "linkedin",
            ChannelName::Meetup => "meetup",
            ChannelName::Proactive => "proactive",
            ChannelName::Reddit => "reddit",
            ChannelName::Slack => "slack",
            ChannelName::TelegramBot => "telegram-bot",
            ChannelName::Twitter => "twitter",
            ChannelName::Voice => "voice",
            ChannelName::Whatsapp => "whatsapp",
        }
    }
}

/// Cross-channel verbs the /setup skill and dashboard rely on. Each variant
/// maps to the matching op on the per-channel `*Op` enum when one exists.
///
/// Order is alphabetical to match the `--help` rendering.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ChannelOp {
    /// Flip the channel's arming flag ON in the sqlite `config` table. The
    /// flip is picked up by the daemon on next restart — `arm` prints JSON
    /// the `/setup` skill keys off (`restart_required`, `restart_cmd`) so it
    /// can offer the restart inline. Channels without an arming gate return
    /// a clean error.
    Arm,
    /// Inverse of `arm` — flip the channel's arming flag OFF in the sqlite
    /// `config` table.
    Disarm,
    /// Persist credentials (cookies / token / OAuth bundle).
    Login,
    /// Run one poll cycle and exit. Respects `--dry-run` (the per-channel
    /// default is true).
    PollOnce,
    /// "Show me a few recent items" smoke test (read-only).
    Recent,
    /// Print whether the channel is logged in / configured. Today only
    /// Discord, Whatsapp, and Browser expose a native `status` op; for the
    /// others we return a "does not support op status" error so the skill
    /// can fall back to `augmentagent status --json` (issue #1).
    Status,
    /// Read-only credential probe — does NOT log in. Twitter is the canonical
    /// implementation today.
    Validate,
}

impl ChannelOp {
    /// The per-channel `*Op::*` subcommand token. clap derive kebab-cases
    /// `PollOnce` to `poll-once`.
    pub fn as_subcommand(self) -> &'static str {
        match self {
            ChannelOp::Arm => "arm",
            ChannelOp::Disarm => "disarm",
            ChannelOp::Login => "login",
            ChannelOp::PollOnce => "poll-once",
            ChannelOp::Recent => "recent",
            ChannelOp::Status => "status",
            ChannelOp::Validate => "validate",
        }
    }
}

/// Arming-key map. Returns `(sqlite_config_key, env_var)` for channels with a
/// known arming gate, `None` for channels that are on-by-default (e.g.
/// `gmail`, which arms itself once Composio is connected). The map mirrors
/// the env-gate names already in `.env.example`; for `twitter` we coin
/// `AUGMENTAGENT_TWITTER_REAL_ENABLED` to follow the same `_REAL_ENABLED` /
/// `_ENABLED` pattern instagram + whatsapp use.
///
/// Exposed as a free function so `status.rs` can re-use it to surface
/// `armed: true/false` per channel without duplicating the table.
pub fn arming_keys_for(channel: &str) -> Option<(&'static str, &'static str)> {
    match channel {
        "instagram" => Some(("instagram_real_account_enabled", "INSTAGRAM_REAL_ACCOUNT_ENABLED")),
        "twitter" => Some(("twitter_real_enabled", "AUGMENTAGENT_TWITTER_REAL_ENABLED")),
        "linkedin" => Some(("linkedin_post_confirm", "AUGMENTAGENT_LINKEDIN_POST_CONFIRM")),
        "whatsapp" => Some(("whatsapp_control_enabled", "AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED")),
        _ => None,
    }
}

/// Boolean parse matching the existing channel-crate gate semantics (see
/// `is_control_enabled` in `crates/augmentagent-channel-whatsapp/src/channel.rs`).
/// Public so `status.rs` agrees on what "armed" means when surfacing the flag
/// in `--json` output.
pub fn is_truthy(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    !matches!(
        s.to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Resolve the sqlite path the way `main.rs` does (matches `AUGMENTAGENT_DB`
/// env-or-`./data.db`). Kept private — the only caller is the arming-flag
/// reader/writer below.
fn db_path() -> String {
    std::env::var("AUGMENTAGENT_DB").unwrap_or_else(|_| "data.db".to_string())
}

/// Write one row to the dashboard's `config` table. Schema mirrors
/// `src/db.ts:45`: `(key TEXT PK, value TEXT NOT NULL, updatedAt INTEGER
/// NOT NULL)`. We `CREATE TABLE IF NOT EXISTS` defensively so this works on
/// a box where the dashboard has never started (the daemon shares the same
/// sqlite file but `Store::migrate` doesn't create `config` — that's the
/// dashboard's responsibility today).
fn write_config_value(key: &str, value: &str) -> Result<()> {
    let conn = rusqlite::Connection::open(db_path())
        .with_context(|| format!("open sqlite at {}", db_path()))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS config (\
             key TEXT PRIMARY KEY,\
             value TEXT NOT NULL,\
             updatedAt INTEGER NOT NULL\
         )",
        [],
    )
    .context("create config table")?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO config (key, value, updatedAt) VALUES (?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updatedAt = excluded.updatedAt",
        rusqlite::params![key, value, now_ms],
    )
    .with_context(|| format!("upsert config row {key}"))?;
    Ok(())
}

/// Flip the arming flag for a channel and print the JSON receipt the
/// `/setup` skill keys off. `armed=true` ⇒ `arm`; `armed=false` ⇒ `disarm`.
fn flip_armed(name: ChannelName, armed: bool) -> Result<()> {
    let ch = name.as_subcommand();
    let Some((sqlite_key, _env_key)) = arming_keys_for(ch) else {
        bail!("channel '{ch}' has no arming gate");
    };
    let value = if armed { "true" } else { "false" };
    write_config_value(sqlite_key, value)?;
    println!(
        "{}",
        json!({
            "channel": ch,
            "armed": armed,
            "config_key": sqlite_key,
            "restart_required": true,
            "restart_cmd": "augmentagent service restart",
        })
    );
    Ok(())
}

/// `true` ⇒ this `(channel, op)` pair has a real handler in `main.rs`. Used
/// both for the fast-fail unsupported error and by the unit tests so we don't
/// re-derive the table in two places.
///
/// Anything not in this table returns an "X does not support op Y" error
/// without spawning a subprocess. That lets the /setup skill probe support
/// up-front instead of catching opaque clap parse failures.
pub fn supports(name: ChannelName, op: ChannelOp) -> bool {
    use ChannelName::*;
    use ChannelOp::*;
    match (name, op) {
        // ----- Arm / Disarm -------------------------------------------------
        // Handled inline (no subprocess) — see `dispatch`. The router itself
        // is the implementation, so `supports` is `true` exactly for the
        // channels with a known arming gate. Channels without a gate still
        // get a clean error from `dispatch`, just via a different code path.
        (_, Arm) | (_, Disarm) => arming_keys_for(name.as_subcommand()).is_some(),
        // ----- Login --------------------------------------------------------
        (Discord, Login)
        | (Github, Login)
        | (Gmail, Login) // routed to `gmail`'s OAuth flow when added; currently no op
        | (Linkedin, Login)
        | (Slack, Login)
        | (TelegramBot, Login)
        | (Twitter, Login)
        | (Voice, Login)
        | (Whatsapp, Login) => true,
        // ----- Status -------------------------------------------------------
        (Browser, Status) | (Discord, Status) | (Whatsapp, Status) => true,
        // ----- Validate -----------------------------------------------------
        (Twitter, Validate) => true,
        // ----- Recent -------------------------------------------------------
        (Linkedin, Recent) => true,
        // ----- PollOnce -----------------------------------------------------
        (Calendar, PollOnce)
        | (Discord, PollOnce)
        | (Gdrive, PollOnce)
        | (Github, PollOnce)
        | (Linkedin, PollOnce)
        | (Meetup, PollOnce)
        | (Slack, PollOnce)
        | (TelegramBot, PollOnce)
        | (Twitter, PollOnce)
        | (Voice, PollOnce)
        | (Whatsapp, PollOnce) => true,
        _ => false,
    }
}

/// Pure argv translator. `argv0` is the binary path the caller intends to
/// re-invoke; the trailing args are the user's pass-through flags. Returns
/// the argv vector that, when executed, is byte-identical to a direct
/// `augmentagent <channel> <op> …` invocation.
///
/// Pulled out as a free function so it has no side effects and can be unit
/// tested without spawning a subprocess.
pub fn build_argv<S: AsRef<str>>(
    argv0: S,
    name: ChannelName,
    op: ChannelOp,
    args: &[String],
) -> Vec<String> {
    let mut out = Vec::with_capacity(3 + args.len());
    out.push(argv0.as_ref().to_string());
    out.push(name.as_subcommand().to_string());
    out.push(op.as_subcommand().to_string());
    out.extend(args.iter().cloned());
    out
}

/// Translate `(name, op)` into the equivalent per-channel CLI invocation and
/// run it inline by re-execing the current binary. The child inherits stdout +
/// stderr so JSON-mode consumers (`--json`) see byte-identical bytes to a
/// direct call. Exit code is mirrored.
pub async fn dispatch(name: ChannelName, op: ChannelOp, args: Vec<String>) -> Result<()> {
    // Arm/Disarm are inline — they write to the sqlite `config` table and
    // print the restart-required JSON. No subprocess. `args` is ignored
    // (these ops take no pass-through flags today).
    match op {
        ChannelOp::Arm => {
            let _ = args; // explicit: no pass-through args for arm
            return flip_armed(name, true);
        }
        ChannelOp::Disarm => {
            let _ = args;
            return flip_armed(name, false);
        }
        _ => {}
    }

    if !supports(name, op) {
        bail!(
            "{} does not support op {} (router knows: arm | disarm | status | login | validate | recent | poll-once)",
            name.as_subcommand(),
            op.as_subcommand()
        );
    }

    // Re-invoke the same binary. `current_exe` is what every Linux process
    // manager (systemd, the /setup skill, the dashboard shell-outs) ends up
    // calling anyway, so behavior is identical.
    let exe = std::env::current_exe().context("locate current augmentagent binary")?;
    let argv = build_argv(
        exe.to_string_lossy().as_ref(),
        name,
        op,
        &args,
    );

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawn {} {} {}", argv[0], name.as_subcommand(), op.as_subcommand()))?;

    if !status.success() {
        // Propagate the child's non-zero exit so `augmentagent channel …`
        // exits the same way the underlying command would have. anyhow turns
        // this into a non-zero process exit via main()'s `?`.
        bail!(
            "augmentagent {} {} exited with status {}",
            name.as_subcommand(),
            op.as_subcommand(),
            status
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_round_trip_gmail_status() {
        // `channel gmail status --json` must reduce to `gmail status --json`.
        // (Gmail does not actually have a `status` op today — this test is
        // about the *translation*, not the supports() table.)
        let argv = build_argv(
            "/usr/local/bin/augmentagent",
            ChannelName::Gmail,
            ChannelOp::Status,
            &["--json".to_string()],
        );
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/augmentagent",
                "gmail",
                "status",
                "--json",
            ]
        );
    }

    #[test]
    fn argv_round_trip_discord_poll_once() {
        // PollOnce kebab-cases — verify and also that pass-through flags
        // survive in the exact order given.
        let argv = build_argv(
            "augmentagent",
            ChannelName::Discord,
            ChannelOp::PollOnce,
            &["--dry-run".to_string(), "false".to_string()],
        );
        assert_eq!(
            argv,
            vec![
                "augmentagent",
                "discord",
                "poll-once",
                "--dry-run",
                "false",
            ]
        );
    }

    #[test]
    fn argv_round_trip_telegram_bot_login() {
        // TelegramBot ⇒ `telegram-bot` (clap-derive default). This is the
        // one channel where the channel name is multi-word.
        let argv = build_argv(
            "augmentagent",
            ChannelName::TelegramBot,
            ChannelOp::Login,
            &["--token".to_string(), "abc".to_string()],
        );
        assert_eq!(
            argv,
            vec![
                "augmentagent",
                "telegram-bot",
                "login",
                "--token",
                "abc",
            ]
        );
    }

    #[test]
    fn supports_matches_known_ops() {
        // Two channels' Status: Discord supports it (real handler), Gmail
        // does not (router returns a typed error so the /setup skill can
        // fall back to `status --json`).
        assert!(supports(ChannelName::Discord, ChannelOp::Status));
        assert!(!supports(ChannelName::Gmail, ChannelOp::Status));
    }

    #[test]
    fn supports_validate_twitter_only() {
        // Only Twitter exposes a `validate` op today (#14 operator harness).
        assert!(supports(ChannelName::Twitter, ChannelOp::Validate));
        assert!(!supports(ChannelName::Slack, ChannelOp::Validate));
        assert!(!supports(ChannelName::Discord, ChannelOp::Validate));
    }

    #[test]
    fn supports_recent_linkedin_only() {
        assert!(supports(ChannelName::Linkedin, ChannelOp::Recent));
        assert!(!supports(ChannelName::Gmail, ChannelOp::Recent));
        assert!(!supports(ChannelName::Discord, ChannelOp::Recent));
    }

    #[test]
    fn supports_poll_once_broad_coverage() {
        // PollOnce is the broadest verb — most channels back a daemon. Spot-
        // check a handful instead of every entry.
        for ch in [
            ChannelName::Discord,
            ChannelName::Slack,
            ChannelName::Gmail, // no PollOnce — uses top-level `poll-once`
        ] {
            let got = supports(ch, ChannelOp::PollOnce);
            let want = !matches!(ch, ChannelName::Gmail);
            assert_eq!(got, want, "PollOnce support mismatch for {ch:?}");
        }
    }

    #[test]
    fn value_enum_round_trip() {
        // clap-derive must accept the canonical strings the skill will pass.
        let parsed = ChannelName::from_str("gmail", true).unwrap();
        assert_eq!(parsed, ChannelName::Gmail);
        let parsed = ChannelName::from_str("telegram-bot", true).unwrap();
        assert_eq!(parsed, ChannelName::TelegramBot);

        let parsed = ChannelOp::from_str("poll-once", true).unwrap();
        assert_eq!(parsed, ChannelOp::PollOnce);

        // #7 — arm / disarm parse as kebab-case singles.
        assert_eq!(ChannelOp::from_str("arm", true).unwrap(), ChannelOp::Arm);
        assert_eq!(
            ChannelOp::from_str("disarm", true).unwrap(),
            ChannelOp::Disarm
        );
    }

    #[test]
    fn arming_keys_cover_known_gates() {
        // The four channels with an existing env-gate in `.env.example`.
        // Twitter inherits the `_REAL_ENABLED` pattern; the daemon doesn't
        // consume it yet — the flag's role today is to drive `status.armed`
        // so the /setup skill can offer the restart.
        assert!(arming_keys_for("instagram").is_some());
        assert!(arming_keys_for("twitter").is_some());
        assert!(arming_keys_for("linkedin").is_some());
        assert!(arming_keys_for("whatsapp").is_some());
        // Channels on-by-default once their credential is in place.
        assert!(arming_keys_for("gmail").is_none());
        assert!(arming_keys_for("slack").is_none());
        assert!(arming_keys_for("discord").is_none());
    }

    #[test]
    fn supports_arm_disarm_for_gated_channels() {
        for ch in [
            ChannelName::Twitter,
            ChannelName::Linkedin,
            ChannelName::Whatsapp,
        ] {
            assert!(supports(ch, ChannelOp::Arm), "Arm support for {ch:?}");
            assert!(supports(ch, ChannelOp::Disarm), "Disarm support for {ch:?}");
        }
        // Gmail is on-by-default — `supports` is false, `dispatch` returns
        // a clean "no arming gate" error.
        assert!(!supports(ChannelName::Gmail, ChannelOp::Arm));
        assert!(!supports(ChannelName::Gmail, ChannelOp::Disarm));
    }

    #[test]
    fn truthy_parsing_matches_existing_gate_semantics() {
        assert!(is_truthy("1"));
        assert!(is_truthy("true"));
        assert!(is_truthy("yes"));
        assert!(is_truthy("on"));
        assert!(!is_truthy(""));
        assert!(!is_truthy("0"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy("FALSE"));
        assert!(!is_truthy("off"));
        assert!(!is_truthy("no"));
    }
}
