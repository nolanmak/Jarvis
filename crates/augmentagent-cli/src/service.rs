//! `augmentagent service …` — thin wrapper around `systemctl --user`, or
//! `launchctl` on macOS (#1079).
//!
//! On Linux AugmentAgent is deployed as a set of user-scope systemd units
//! (`systemctl --user`); on macOS the same jobs are launchd agents under
//! `~/Library/LaunchAgents` (see [`crate::platform`]). This subcommand hides the unit-name
//! sprawl so the `/setup` skill (and humans) can say `service restart
//! --unit dashboard` instead of remembering `augmentagent-dashboard.service`.
//!
//! Known units (see deploy/systemd/):
//!   augmentagent.service                     -- the main daemon
//!   augmentagent-dashboard.service           -- localhost dashboard
//!   augmentagent-update.service              -- auto-updater oneshot
//!   augmentagent-update.timer                -- auto-updater schedule
//!   augmentagent-digest.service              -- daily wiki digest
//!   augmentagent-digest.timer                -- daily wiki digest schedule
//!   augmentagent-tone-refresh.service        -- tone-mirror refresh oneshot
//!   augmentagent-tone-refresh.timer          -- tone-mirror refresh schedule
//!   augmentagent-browser-sidecar.service     -- headless-browser sidecar
//!   augmentagent-tenant-<name>.service       -- per-tenant daemon (multi-tenant)
//!
//! `--unit all` resolves dynamically via
//!   `systemctl --user list-units 'augmentagent*' --no-legend --plain --all`
//! so newly-installed tenants are picked up without code changes.

use std::process::Stdio;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;
use tokio::process::Command;

use crate::platform::ServiceManager;

/// The verbs we forward to `systemctl --user`.
#[derive(Subcommand, Clone, Copy, Debug)]
pub enum ServiceOp {
    /// `systemctl --user start <unit>`.
    Start,
    /// `systemctl --user stop <unit>`.
    Stop,
    /// `systemctl --user restart <unit>`.
    Restart,
    /// `systemctl --user status <unit>` (or JSON state via `show` when --json).
    Status,
    /// `systemctl --user enable <unit>` (persist across reboots).
    Enable,
    /// `systemctl --user disable <unit>`.
    Disable,
}

impl ServiceOp {
    fn verb(self) -> &'static str {
        match self {
            ServiceOp::Start => "start",
            ServiceOp::Stop => "stop",
            ServiceOp::Restart => "restart",
            ServiceOp::Status => "status",
            ServiceOp::Enable => "enable",
            ServiceOp::Disable => "disable",
        }
    }
}

/// Entrypoint for `augmentagent service <op> [--unit <name>] [--json]`.
///
/// `unit` is the user-facing alias. `all` expands to every installed
/// `augmentagent*` unit; bare friendly names (`daemon`, `dashboard`, `updater`,
/// `digest`, `tone-refresh`, `browser-sidecar`, `tenant:<name>`) are mapped to
/// the concrete service/timer pair. Anything containing a `.` is forwarded
/// verbatim so power users can target a specific unit file.
pub async fn run_service(op: ServiceOp, unit: &str, json: bool) -> Result<()> {
    if ServiceManager::detect().is_launchd() {
        return launchd::run(op, unit, json).await;
    }
    let units = resolve_units(unit).await?;
    if units.is_empty() {
        anyhow::bail!(
            "no augmentagent units matched `--unit {unit}` (try `--unit all` or \
             one of: daemon, dashboard, updater, digest, tone-refresh, \
             browser-sidecar, tenant:<name>)"
        );
    }

    if matches!(op, ServiceOp::Status) && json {
        let report = collect_status_json(&units).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    // For everything else we just forward to systemctl and inherit its exit
    // code. `status` returns non-zero when a unit is stopped, which is not an
    // error condition for us — surface stdout/stderr but don't bail.
    let mut cmd = Command::new("systemctl");
    cmd.arg("--user").arg(op.verb());
    for u in &units {
        cmd.arg(u);
    }
    cmd.stdin(Stdio::null());
    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawning systemctl --user {} (is systemd available?)", op.verb()))?;
    if !status.success() && !matches!(op, ServiceOp::Status) {
        anyhow::bail!(
            "systemctl --user {} {} exited with status {}",
            op.verb(),
            units.join(" "),
            status
        );
    }
    Ok(())
}

/// Expand a `--unit` flag value into one or more concrete systemd unit names.
///
/// Pure function modulo the `all` branch (which shells `systemctl list-units`).
async fn resolve_units(unit: &str) -> Result<Vec<String>> {
    let trimmed = unit.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
        return list_all_units().await;
    }
    Ok(resolve_alias(trimmed))
}

/// Pure alias → unit-name mapping. Tested below.
fn resolve_alias(unit: &str) -> Vec<String> {
    // Full unit name with extension (e.g. `augmentagent-digest.timer`).
    if unit.contains('.') {
        return vec![unit.to_string()];
    }
    // tenant:<name> → augmentagent-tenant-<name>.service
    if let Some(name) = unit.strip_prefix("tenant:") {
        let name = name.trim();
        if name.is_empty() {
            return vec![];
        }
        return vec![format!("augmentagent-tenant-{name}.service")];
    }
    match unit {
        // The main daemon. Accept a few obvious spellings.
        "daemon" | "augmentagent" | "main" => vec!["augmentagent.service".into()],
        "dashboard" => vec!["augmentagent-dashboard.service".into()],
        // For schedules we operate on the timer + its oneshot service together
        // so `restart` actually re-arms the timer and `disable` stops both
        // halves cleanly.
        "updater" | "update" => vec![
            "augmentagent-update.timer".into(),
            "augmentagent-update.service".into(),
        ],
        "digest" => vec![
            "augmentagent-digest.timer".into(),
            "augmentagent-digest.service".into(),
        ],
        "tone-refresh" | "tone" => vec![
            "augmentagent-tone-refresh.timer".into(),
            "augmentagent-tone-refresh.service".into(),
        ],
        "browser-sidecar" | "browser" => vec!["augmentagent-browser-sidecar.service".into()],
        // Bare `augmentagent-foo` → `augmentagent-foo.service` (best-effort).
        other if other.starts_with("augmentagent-") || other.starts_with("augmentagent.") => {
            vec![format!("{other}.service")]
        }
        _ => vec![],
    }
}

/// `systemctl --user list-units 'augmentagent*' --no-legend --plain --all` →
/// unique unit names. `--all` is important so stopped/inactive units still
/// appear (otherwise `service status --json all` would silently drop them).
async fn list_all_units() -> Result<Vec<String>> {
    let out = Command::new("systemctl")
        .arg("--user")
        .arg("list-units")
        .arg("augmentagent*")
        .arg("--no-legend")
        .arg("--plain")
        .arg("--all")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("spawning systemctl --user list-units")?;
    if !out.status.success() {
        anyhow::bail!(
            "systemctl --user list-units failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut units: Vec<String> = stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .filter(|u| u.starts_with("augmentagent"))
        .collect();
    units.sort();
    units.dedup();
    Ok(units)
}

/// Build a JSON report for each unit. Always returns an array; each entry
/// has the same shape so dashboards can consume it without conditionals.
async fn collect_status_json(units: &[String]) -> Result<serde_json::Value> {
    let mut entries = Vec::with_capacity(units.len());
    for unit in units {
        entries.push(show_one(unit).await?);
    }
    Ok(json!({ "units": entries }))
}

/// `systemctl show --user <unit> --property=...` and parse `key=value` lines.
/// We deliberately don't pass `--output=json` since older systemd builds
/// silently ignore it; `key=value` is universally supported.
async fn show_one(unit: &str) -> Result<serde_json::Value> {
    let props = [
        "ActiveState",
        "SubState",
        "LoadState",
        "MainPID",
        "ActiveEnterTimestamp",
        "ActiveEnterTimestampMonotonic",
        "UnitFileState",
    ];
    let out = Command::new("systemctl")
        .arg("show")
        .arg("--user")
        .arg(unit)
        .arg(format!("--property={}", props.join(",")))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("spawning systemctl show --user {unit}"))?;
    if !out.status.success() {
        return Ok(json!({
            "unit": unit,
            "error": String::from_utf8_lossy(&out.stderr).trim().to_string(),
        }));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut obj = serde_json::Map::new();
    obj.insert("unit".into(), json!(unit));
    for line in stdout.lines() {
        if let Some((k, v)) = line.split_once('=') {
            // MainPID is the one numeric field we promise in the schema; the
            // rest stay strings since systemd renders timestamps as text.
            if k == "MainPID" {
                if let Ok(n) = v.parse::<i64>() {
                    obj.insert(k.to_string(), json!(n));
                    continue;
                }
            }
            obj.insert(k.to_string(), json!(v));
        }
    }
    Ok(serde_json::Value::Object(obj))
}

/// #1079 — the same verbs against launchd agents. Units are still named by
/// their systemd unit (so every alias above works unchanged) and mapped to
/// the installers' launchd labels; a timer and its service collapse to one
/// job.
mod launchd {
    use anyhow::{Context, Result};
    use serde_json::json;

    use crate::platform::{
        self, gui_domain, launchd_job, launchd_label, plist_path, service_target, LABEL_PREFIX,
    };

    use super::{resolve_alias, ServiceOp};

    pub async fn run(op: ServiceOp, unit: &str, json: bool) -> Result<()> {
        let labels = resolve_labels(unit)?;
        if labels.is_empty() {
            anyhow::bail!(
                "no augmentagent launchd agents matched `--unit {unit}` (try `--unit all` or \
                 one of: daemon, dashboard, updater, digest, tone-refresh, \
                 browser-sidecar, tenant:<name>)"
            );
        }
        if matches!(op, ServiceOp::Status) {
            if json {
                let units: Vec<_> = labels.iter().map(|l| status_json(l)).collect();
                println!("{}", serde_json::to_string_pretty(&json!({ "units": units }))?);
            } else {
                for label in &labels {
                    print_status(label);
                }
            }
            return Ok(());
        }
        let mut failed = Vec::new();
        for label in &labels {
            if let Err(e) = apply(op, label) {
                eprintln!("{label}: {e:#}");
                failed.push(label.as_str());
            }
        }
        if !failed.is_empty() {
            anyhow::bail!("launchctl {} failed for {}", verb(op), failed.join(" "));
        }
        Ok(())
    }

    fn verb(op: ServiceOp) -> &'static str {
        match op {
            ServiceOp::Start => "start",
            ServiceOp::Stop => "stop",
            ServiceOp::Restart => "restart",
            ServiceOp::Status => "status",
            ServiceOp::Enable => "enable",
            ServiceOp::Disable => "disable",
        }
    }

    /// `--unit` → launchd labels, deduplicated in order.
    pub(super) fn resolve_labels(unit: &str) -> Result<Vec<String>> {
        let trimmed = unit.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
            return installed_labels();
        }
        // A label passed verbatim.
        if trimmed.starts_with(LABEL_PREFIX) {
            return Ok(vec![trimmed.to_string()]);
        }
        let mut out: Vec<String> = Vec::new();
        for u in resolve_alias(trimmed) {
            let Some(label) = launchd_label(&u) else {
                anyhow::bail!("`{u}` has no launchd agent on macOS");
            };
            if !out.contains(&label) {
                out.push(label);
            }
        }
        Ok(out)
    }

    /// Every `com.nolanmak.augmentagent*.plist` in `~/Library/LaunchAgents`.
    fn installed_labels() -> Result<Vec<String>> {
        let Some(dir) = platform::launch_agents_dir() else {
            anyhow::bail!("HOME unset — cannot find ~/Library/LaunchAgents");
        };
        let mut labels: Vec<String> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter_map(|n| n.strip_suffix(".plist").map(str::to_string))
                .filter(|n| n.starts_with(LABEL_PREFIX))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        labels.sort();
        Ok(labels)
    }

    fn bootstrap(label: &str) -> Result<()> {
        let plist = plist_path(label).context("HOME unset")?;
        if !plist.exists() {
            anyhow::bail!(
                "not installed: no {} (run the matching `augmentagent install …`)",
                plist.display()
            );
        }
        let plist = plist.to_string_lossy().to_string();
        let (ok, err) = platform::launchctl(&["bootstrap", &gui_domain(), &plist])
            .context("spawning launchctl bootstrap")?;
        if !ok {
            anyhow::bail!("launchctl bootstrap {label}: {err}");
        }
        Ok(())
    }

    fn run_launchctl(args: &[&str]) -> Result<()> {
        let (ok, err) = platform::launchctl(args)
            .with_context(|| format!("spawning launchctl {}", args.join(" ")))?;
        if !ok {
            anyhow::bail!("launchctl {}: {err}", args.join(" "));
        }
        Ok(())
    }

    fn apply(op: ServiceOp, label: &str) -> Result<()> {
        let target = service_target(label);
        let loaded = launchd_job(label).loaded;
        match op {
            // Loading a RunAtLoad agent starts it; an already-loaded one is
            // kicked (a no-op when it is already running).
            ServiceOp::Start => {
                if loaded {
                    run_launchctl(&["kickstart", &target])
                } else {
                    bootstrap(label)
                }
            }
            ServiceOp::Stop => {
                if loaded {
                    run_launchctl(&["bootout", &target])
                } else {
                    Ok(())
                }
            }
            ServiceOp::Restart => {
                if loaded {
                    run_launchctl(&["kickstart", "-k", &target])
                } else {
                    bootstrap(label)
                }
            }
            ServiceOp::Enable => {
                run_launchctl(&["enable", &target])?;
                if loaded {
                    Ok(())
                } else {
                    bootstrap(label)
                }
            }
            ServiceOp::Disable => {
                if loaded {
                    run_launchctl(&["bootout", &target])?;
                }
                run_launchctl(&["disable", &target])
            }
            ServiceOp::Status => unreachable!("status is handled by the caller"),
        }
    }

    fn print_status(label: &str) {
        let job = launchd_job(label);
        let installed = plist_path(label).is_some_and(|p| p.exists());
        let state = if job.loaded {
            job.state.as_str()
        } else if installed {
            "not loaded"
        } else {
            "not installed"
        };
        match job.pid {
            Some(pid) => println!("{label}: {state} (pid {pid})"),
            None => println!("{label}: {state}"),
        }
    }

    /// Same keys as the systemd report so consumers need no branching.
    fn status_json(label: &str) -> serde_json::Value {
        let job = launchd_job(label);
        let installed = plist_path(label).is_some_and(|p| p.exists());
        let active = if job.running() {
            "active"
        } else {
            "inactive"
        };
        let sub = if job.loaded {
            job.state.clone()
        } else {
            "dead".to_string()
        };
        json!({
            "unit": label,
            "ActiveState": active,
            "SubState": sub,
            "LoadState": if job.loaded { "loaded" } else if installed { "not-loaded" } else { "not-found" },
            "MainPID": job.pid.unwrap_or(0),
            "ActiveEnterTimestamp": job.pid.and_then(platform::process_start_unix)
                .map(|t| t.to_string()).unwrap_or_default(),
            "UnitFileState": if installed { "enabled" } else { "not-found" },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_daemon() {
        assert_eq!(resolve_alias("daemon"), vec!["augmentagent.service"]);
        assert_eq!(resolve_alias("main"), vec!["augmentagent.service"]);
        assert_eq!(resolve_alias("augmentagent"), vec!["augmentagent.service"]);
    }

    #[test]
    fn alias_dashboard() {
        assert_eq!(
            resolve_alias("dashboard"),
            vec!["augmentagent-dashboard.service"]
        );
    }

    #[test]
    fn alias_timer_pair() {
        // Schedules expand to (timer, service) so restart/disable hit both.
        assert_eq!(
            resolve_alias("digest"),
            vec![
                "augmentagent-digest.timer",
                "augmentagent-digest.service",
            ]
        );
        assert_eq!(
            resolve_alias("tone-refresh"),
            vec![
                "augmentagent-tone-refresh.timer",
                "augmentagent-tone-refresh.service",
            ]
        );
        assert_eq!(
            resolve_alias("updater"),
            vec![
                "augmentagent-update.timer",
                "augmentagent-update.service",
            ]
        );
    }

    #[test]
    fn alias_tenant() {
        assert_eq!(
            resolve_alias("tenant:acme"),
            vec!["augmentagent-tenant-acme.service"]
        );
        // Empty tenant name → no expansion (caller will bail).
        assert!(resolve_alias("tenant:").is_empty());
    }

    #[test]
    fn alias_passthrough_with_extension() {
        assert_eq!(
            resolve_alias("augmentagent-browser-sidecar.service"),
            vec!["augmentagent-browser-sidecar.service"]
        );
        // Even non-augmentagent units pass through; the user asked for it.
        assert_eq!(resolve_alias("nginx.service"), vec!["nginx.service"]);
    }

    #[test]
    fn alias_unknown_returns_empty() {
        assert!(resolve_alias("totally-bogus").is_empty());
    }

    #[test]
    fn launchd_aliases_collapse_timer_pairs_to_one_label() {
        assert_eq!(
            launchd::resolve_labels("updater").unwrap(),
            vec!["com.nolanmak.augmentagent.updater"]
        );
        assert_eq!(
            launchd::resolve_labels("daemon").unwrap(),
            vec!["com.nolanmak.augmentagent"]
        );
        assert_eq!(
            launchd::resolve_labels("tenant:acme").unwrap(),
            vec!["com.nolanmak.augmentagent.tenant-acme"]
        );
        // A label passes through verbatim.
        assert_eq!(
            launchd::resolve_labels("com.nolanmak.augmentagent.digest").unwrap(),
            vec!["com.nolanmak.augmentagent.digest"]
        );
        // Non-augmentagent units have no macOS job.
        assert!(launchd::resolve_labels("nginx.service").is_err());
    }
}
