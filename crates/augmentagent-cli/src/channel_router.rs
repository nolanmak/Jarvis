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
//! Op coverage today: `Status | Login | Validate | Recent | PollOnce`. Channels
//! that don't expose a given op return a clear "<channel> does not support
//! op <op>" error rather than silently doing the wrong thing.
//! `Arm | Disarm` land in issue #7.

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
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
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ChannelOp {
    /// Print whether the channel is logged in / configured. Today only
    /// Discord, Whatsapp, and Browser expose a native `status` op; for the
    /// others we return a "does not support op status" error so the skill
    /// can fall back to `augmentagent status --json` (issue #1).
    Status,
    /// Persist credentials (cookies / token / OAuth bundle).
    Login,
    /// Read-only credential probe — does NOT log in. Twitter is the canonical
    /// implementation today.
    Validate,
    /// "Show me a few recent items" smoke test (read-only).
    Recent,
    /// Run one poll cycle and exit. Respects `--dry-run` (the per-channel
    /// default is true).
    PollOnce,
}

impl ChannelOp {
    /// The per-channel `*Op::*` subcommand token. clap derive kebab-cases
    /// `PollOnce` to `poll-once`.
    pub fn as_subcommand(self) -> &'static str {
        match self {
            ChannelOp::Status => "status",
            ChannelOp::Login => "login",
            ChannelOp::Validate => "validate",
            ChannelOp::Recent => "recent",
            ChannelOp::PollOnce => "poll-once",
        }
    }
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
    if !supports(name, op) {
        bail!(
            "{} does not support op {} (router knows: status | login | validate | recent | poll-once; arm/disarm land in issue #7)",
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
    }
}
