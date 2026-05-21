//! `augmentagent logs` — thin wrapper over `journalctl --user -u <unit>` so
//! the `/setup` skill (and humans) can stream or dump daemon logs without
//! memorizing systemd unit names.
//!
//! Linux-only by design: AugmentAgent ships as a `--user` systemd service
//! (see `scripts/install-service.sh`), and `journalctl` is the canonical
//! query tool. We deliberately keep this a passthrough — no parsing, no
//! buffering, no re-emission — so `--follow` stays live and `--json`
//! preserves journalctl's exact one-object-per-line schema.
//!
//! Companion to `service` (systemctl wrapper) and `status` (aggregator).

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

/// Default unit when `--unit` is omitted (the main poll-loop daemon).
const DEFAULT_UNIT: &str = "augmentagent.service";

/// Expand a short alias into its real systemd unit name.
///
/// Accepts:
///   - `daemon` | `agent` | `main`      → `augmentagent.service`
///   - `dashboard` | `web` | `ui`       → `augmentagent-dashboard.service`
///   - any value already containing `.` → returned verbatim (e.g.
///     `augmentagent-foo.service`, `my.timer`)
///   - any other bare name `X`          → `augmentagent-X.service`
///
/// Kept pure + total for unit tests; no I/O.
pub fn expand_unit(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_UNIT.to_string();
    }
    match trimmed {
        "daemon" | "agent" | "main" => "augmentagent.service".to_string(),
        "dashboard" | "web" | "ui" => "augmentagent-dashboard.service".to_string(),
        s if s.contains('.') => s.to_string(),
        s => format!("augmentagent-{s}.service"),
    }
}

/// Spawn `journalctl --user` with the requested flags and stream its output
/// straight to our stdout/stderr. Returns once the child exits.
///
/// We use `Stdio::inherit()` (not `output()` / piped) for two reasons:
///   1. `--follow` must stay live — buffering until exit defeats the point.
///   2. `--json` already emits one object per line; we don't want to
///      re-parse it, just hand it to the terminal / pipe as-is.
pub async fn run_logs(
    unit: String,
    follow: bool,
    lines: u32,
    since: Option<String>,
    json: bool,
) -> Result<()> {
    let resolved = expand_unit(&unit);

    let mut cmd = Command::new("journalctl");
    cmd.arg("--user")
        .arg("-u")
        .arg(&resolved)
        .arg("-n")
        .arg(lines.to_string());

    if follow {
        cmd.arg("-f");
    }
    if let Some(s) = since.as_deref() {
        cmd.arg("--since").arg(s);
    }
    if json {
        cmd.arg("-o").arg("json");
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = cmd
        .spawn()
        .with_context(|| "spawning `journalctl` (is it on PATH? this is a Linux-only command)")?;

    let status = child
        .wait()
        .await
        .context("waiting on journalctl")?;

    if !status.success() {
        // journalctl exits 1 when it can't find the unit; surface that
        // explicitly so the user knows whether the issue is the alias or
        // the service simply hasn't been installed.
        anyhow::bail!(
            "journalctl exited with status {status} (unit `{resolved}` — check `systemctl --user list-units`)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_expand_to_real_units() {
        assert_eq!(expand_unit("daemon"), "augmentagent.service");
        assert_eq!(expand_unit("agent"), "augmentagent.service");
        assert_eq!(expand_unit("main"), "augmentagent.service");
        assert_eq!(expand_unit("dashboard"), "augmentagent-dashboard.service");
        assert_eq!(expand_unit("web"), "augmentagent-dashboard.service");
        assert_eq!(expand_unit("ui"), "augmentagent-dashboard.service");
    }

    #[test]
    fn bare_names_get_augmentagent_prefix() {
        assert_eq!(expand_unit("scheduler"), "augmentagent-scheduler.service");
        assert_eq!(expand_unit("invoice"), "augmentagent-invoice.service");
    }

    #[test]
    fn dotted_names_pass_through() {
        assert_eq!(expand_unit("custom.service"), "custom.service");
        assert_eq!(expand_unit("backup.timer"), "backup.timer");
        assert_eq!(
            expand_unit("augmentagent.service"),
            "augmentagent.service"
        );
    }

    #[test]
    fn empty_falls_back_to_default() {
        assert_eq!(expand_unit(""), DEFAULT_UNIT);
        assert_eq!(expand_unit("   "), DEFAULT_UNIT);
    }
}
