//! `augmentagent handoff-prune` (#1035): the on-demand journal retention pass.
//!
//! The daemon already sweeps at start and hourly, and removes a journal only
//! when two consecutive passes agree. This pass removes immediately, so a
//! removing run needs explicit confirmation and refuses while the daemon runs.

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Result};
use augmentagent_channel_core::handoff::{self, RetentionSetting};

/// The unit whose own sweep makes an on-demand removing pass unnecessary.
pub const DAEMON_UNIT: &str = "augmentagent.service";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Active,
    Inactive,
    /// `systemctl` missing, no user bus, or an unrecognised state.
    Unknown,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub dry_run: bool,
    pub yes: bool,
    pub force: bool,
    pub json: bool,
}

/// `systemctl --user is-active <unit>` prints one state. A unit that is
/// starting, restarting or stopping still counts as running.
pub fn parse_is_active(stdout: &str) -> DaemonState {
    match stdout.trim() {
        "active" | "reloading" | "activating" | "deactivating" | "refreshing" => DaemonState::Active,
        "inactive" | "failed" => DaemonState::Inactive,
        _ => DaemonState::Unknown,
    }
}

/// The production probe: a read-only systemd query.
pub fn daemon_state() -> DaemonState {
    match std::process::Command::new("systemctl")
        .args(["--user", "is-active", DAEMON_UNIT])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => parse_is_active(&String::from_utf8_lossy(&output.stdout)),
        Err(_) => DaemonState::Unknown,
    }
}

pub fn run(
    opts: Options,
    setting: &RetentionSetting,
    root: &Path,
    daemon: &dyn Fn() -> DaemonState,
    out: &mut dyn Write,
) -> Result<()> {
    let mut forced_past = None;
    if !opts.dry_run {
        if !opts.yes {
            bail!(
                "handoff-prune removes finished journals immediately, without the daemon's confirming pass, \
                 so a turn replayed after an outage (a loop occurrence interrupted by a crash) would find no \
                 receipts. Review `augmentagent handoff-prune --dry-run` first, then re-run with --yes."
            );
        }
        match daemon() {
            DaemonState::Inactive => {}
            state if opts.force => forced_past = Some(state),
            DaemonState::Active => bail!(
                "{DAEMON_UNIT} is active and already sweeps hourly with a confirming pass; refusing an \
                 immediate removing pass. Use --dry-run, or pass --force to prune anyway."
            ),
            DaemonState::Unknown => bail!(
                "could not tell whether {DAEMON_UNIT} is active (systemctl --user is-active); refusing a \
                 removing pass. Pass --force if the daemon is not running."
            ),
        }
    }
    const DAEMON_NOTE: &str = "this process's environment (shell and .env); the daemon reads its own, which may differ";
    let grace_hours = setting.grace.as_secs() / 3600;
    let report = handoff::sweep_finished(root, setting.grace, opts.dry_run)?;
    if opts.json {
        let forced = forced_past.map(|state| format!("{state:?}").to_lowercase());
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "root": root.display().to_string(),
                "grace_hours": grace_hours,
                "grace_source": setting.source.to_string(),
                "grace_note": DAEMON_NOTE,
                "dry_run": opts.dry_run,
                "forced_past_daemon_state": forced,
                "report": report,
            }))?
        )?;
        return Ok(());
    }
    const MIB: u64 = 1024 * 1024;
    if let Some(state) = forced_past {
        let why = match state {
            DaemonState::Active => "is active",
            _ => "may be running",
        };
        writeln!(out, "warning: {DAEMON_UNIT} {why}; pruning anyway (--force)")?;
    }
    writeln!(out, "handoff journals: {}", root.display())?;
    writeln!(out, "grace {grace_hours}h ({}), from {DAEMON_NOTE}", setting.source)?;
    writeln!(
        out,
        "{} request dirs ({} other entries), {} MB; {} finished past grace by over two sweep intervals",
        report.requests,
        report.entries - report.requests,
        report.bytes / MIB,
        report.finished_overdue,
    )?;
    writeln!(
        out,
        "{} {} ({} MB); keep {} (recent {}, lifecycle markers {}, unfinished {}, busy {}, untrusted {})",
        if opts.dry_run { "would remove" } else { "removed" },
        report.removed,
        report.removed_bytes / MIB,
        report.kept(),
        report.kept_recent,
        report.kept_active,
        report.kept_unfinished,
        report.kept_busy,
        report.kept_untrusted,
    )?;
    if opts.dry_run {
        writeln!(out, "\n(dry run — nothing was locked, created or removed)")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_channel_core::handoff::RetentionSource;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    const HOURS: u64 = 3600;

    /// A private root holding one finished request idle for two days.
    fn finished_root() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("reasoner-handoffs");
        let request = root.join("ab".repeat(32));
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&request).unwrap();
        let journal = request.join("operations.json");
        let mut file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&journal).unwrap();
        file.write_all(serde_json::json!({"version": 1, "operations": [{"tool": "mcp__fixture__create",
            "arguments": {}, "status": "completed", "result": {"content": []}}]}).to_string().as_bytes()).unwrap();
        let old = SystemTime::now() - Duration::from_secs(48 * HOURS);
        file.set_modified(old).unwrap();
        std::fs::File::open(&request).unwrap().set_modified(old).unwrap();
        (temp, root, request)
    }

    fn setting(hours: u64, source: RetentionSource) -> RetentionSetting {
        RetentionSetting { grace: Duration::from_secs(hours * HOURS), source }
    }

    fn default_grace() -> RetentionSetting {
        setting(24, RetentionSource::Default)
    }

    fn prune(opts: Options, state: DaemonState) -> (Result<()>, String, bool) {
        let (_temp, root, request) = finished_root();
        let mut out = Vec::new();
        let result = run(opts, &default_grace(), &root, &|| state, &mut out);
        (result, String::from_utf8(out).unwrap(), request.exists())
    }

    #[test]
    fn a_removing_pass_requires_explicit_confirmation() {
        let (result, _, kept) = prune(Options::default(), DaemonState::Inactive);
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("--yes") && error.contains("--dry-run"), "{error}");
        assert!(kept);
        let (result, out, kept) = prune(Options { yes: true, ..Default::default() }, DaemonState::Inactive);
        result.unwrap();
        assert!(!kept);
        assert!(out.contains("removed 1"), "{out}");
    }

    #[test]
    fn a_removing_pass_refuses_while_the_daemon_runs_unless_forced() {
        for state in [DaemonState::Active, DaemonState::Unknown] {
            let (result, _, kept) = prune(Options { yes: true, ..Default::default() }, state);
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains(DAEMON_UNIT) && error.contains("--force"), "{state:?}: {error}");
            assert!(kept, "{state:?}");
            let (result, out, kept) = prune(Options { yes: true, force: true, ..Default::default() }, state);
            result.unwrap();
            assert!(!kept, "{state:?}");
            assert!(out.contains("--force"), "{state:?}: {out}");
        }
        // --force alone is not confirmation.
        let (result, _, kept) = prune(Options { force: true, ..Default::default() }, DaemonState::Inactive);
        assert!(result.is_err() && kept);
    }

    #[test]
    fn a_dry_run_needs_no_confirmation_and_never_probes_the_daemon() {
        let (_temp, root, request) = finished_root();
        let mut out = Vec::new();
        let probe = || -> DaemonState { panic!("a dry run must not probe the daemon") };
        run(Options { dry_run: true, ..Default::default() }, &default_grace(), &root, &probe, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(request.exists());
        assert!(out.contains("would remove 1"), "{out}");
    }

    #[test]
    fn output_names_the_effective_grace_and_where_it_came_from() {
        let cases = [
            (default_grace(), vec!["grace 24h", "default"]),
            (setting(72, RetentionSource::Env("72".into())), vec!["grace 72h", "AUGMENTAGENT_HANDOFF_RETENTION_HOURS=72"]),
            (setting(24, RetentionSource::Rejected("1".into())), vec!["grace 24h", "\"1\" rejected"]),
        ];
        for (grace, wanted) in cases {
            for json in [false, true] {
                let (_temp, root, _) = finished_root();
                let mut out = Vec::new();
                let opts = Options { dry_run: true, json, ..Default::default() };
                run(opts, &grace, &root, &|| DaemonState::Unknown, &mut out).unwrap();
                let out = String::from_utf8(out).unwrap();
                assert!(out.contains("daemon"), "must say the daemon reads its own environment: {out}");
                for text in &wanted {
                    let text = if json { text.replace('"', "\\\"").replace("grace 24h", "\"grace_hours\": 24")
                        .replace("grace 72h", "\"grace_hours\": 72") } else { text.to_string() };
                    assert!(out.contains(&text), "{text:?} missing from {out}");
                }
            }
        }
    }

    #[test]
    fn systemctl_states_map_to_daemon_states() {
        for (stdout, state) in [
            ("active\n", DaemonState::Active),
            ("reloading\n", DaemonState::Active),
            ("activating\n", DaemonState::Active),
            ("deactivating\n", DaemonState::Active),
            ("inactive\n", DaemonState::Inactive),
            ("failed\n", DaemonState::Inactive),
            ("", DaemonState::Unknown),
            ("Failed to connect to bus: No medium found\n", DaemonState::Unknown),
        ] {
            assert_eq!(parse_is_active(stdout), state, "{stdout:?}");
        }
    }

    #[test]
    fn cli_flags_parse() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["augmentagent", "handoff-prune", "--yes", "--force", "--root", "/synthetic"]).unwrap();
        let crate::Cmd::HandoffPrune { dry_run, yes, force, json, root } = cli.cmd else { panic!("expected handoff-prune") };
        assert_eq!((dry_run, yes, force, json), (false, true, true, false));
        assert_eq!(root, Some(PathBuf::from("/synthetic")));
    }
}
