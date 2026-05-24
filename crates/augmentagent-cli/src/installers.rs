//! `augmentagent install <component>` / `augmentagent uninstall <component>` —
//! thin Rust shims that shell out to the idempotent `scripts/install-*.sh` /
//! `scripts/uninstall-*.sh` family (plus the systemd unit-template copy for
//! the browser sidecar stack).
//!
//! Pattern mirrors `invoice.rs`: env-resolved script dir, `tokio::process::Command`,
//! `Stdio::inherit()` for live stream, anyhow errors with stderr inlined.
//!
//! Linux-only by design (single deploy target).
//!
//! Install logic is NOT reimplemented in Rust — every component delegates to
//! the existing shell installer, which owns argument validation, idempotency,
//! and systemd interactions.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use tokio::process::Command;

/// Where the shell installers live. Override with `AUGMENTAGENT_SCRIPTS_DIR`
/// (daemon cwd is the repo root). Default matches `invoice.rs`'s convention.
fn scripts_dir() -> String {
    std::env::var("AUGMENTAGENT_SCRIPTS_DIR").unwrap_or_else(|_| "scripts".to_string())
}

/// Where the systemd unit templates live (browser-sidecar uses these directly
/// rather than via a shell installer). Override with `AUGMENTAGENT_SYSTEMD_DIR`.
fn systemd_dir() -> String {
    std::env::var("AUGMENTAGENT_SYSTEMD_DIR").unwrap_or_else(|_| "systemd".to_string())
}

/// Per-user systemd unit directory (`$XDG_CONFIG_HOME/systemd/user`, default
/// `~/.config/systemd/user`). Matches what every install-*.sh script targets.
fn user_unit_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("systemd").join("user"));
        }
    }
    let home = std::env::var("HOME").context(
        "HOME unset — cannot resolve ~/.config/systemd/user. Set HOME or XDG_CONFIG_HOME.",
    )?;
    Ok(PathBuf::from(home).join(".config").join("systemd").join("user"))
}

/// Sibling unit files copied as a set for `browser-sidecar` (matches
/// `systemd/README.md`'s install incantation, ~lines 34-47).
const BROWSER_SIDECAR_UNITS: &[&str] = &[
    "augmentagent-xvfb.service",
    "augmentagent-chromium.service",
    "augmentagent-browser-sidecar.service",
];

/// Flags every install variant accepts. Flattened into each `Component`
/// variant so the user can write `install dashboard --rebuild --json`
/// (clap-style: parent flags must precede the subcommand otherwise).
#[derive(Args, Clone, Debug, Default)]
pub struct InstallFlags {
    /// Run `cargo build --release -p augmentagent-cli && npm run build`
    /// before invoking the install script. No-op for uninstall.
    #[arg(long, default_value_t = false)]
    pub rebuild: bool,
    /// Suppress live stream; emit one JSON summary at end:
    /// `{component, action, succeeded, stdout_tail, stderr_tail}`.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Flags every uninstall variant accepts (subset of `InstallFlags` —
/// `--rebuild` doesn't apply when tearing down).
#[derive(Args, Clone, Debug, Default)]
pub struct UninstallFlags {
    /// Suppress live stream; emit one JSON summary at end:
    /// `{component, action, succeeded, stdout_tail, stderr_tail}`.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// One component the `/setup` skill can install. Each variant maps 1:1 to a
/// `scripts/install-<name>.sh` (or the systemd unit-file copy for
/// `BrowserSidecar`).
///
/// Each variant carries its own `InstallFlags` (rebuild + json) so the flags
/// can be passed positionally after the component name, matching the issue's
/// acceptance criteria (`install dashboard --rebuild`, `install dashboard --json`).
#[derive(Subcommand, Clone, Debug)]
pub enum InstallComponent {
    /// systemd user unit for the Rust daemon (`scripts/install-autostart.sh`).
    Autostart {
        #[command(flatten)]
        flags: InstallFlags,
    },
    /// 5-minute auto-update timer (`scripts/install-autoupdate.sh`).
    Autoupdate {
        #[command(flatten)]
        flags: InstallFlags,
    },
    /// Dashboard systemd unit on `$DASHBOARD_PORT`
    /// (`scripts/install-dashboard.sh`). NOTE: no uninstall script exists
    /// upstream; `uninstall dashboard` surfaces that as an explicit error.
    Dashboard {
        #[command(flatten)]
        flags: InstallFlags,
    },
    /// Daily digest timer (`scripts/install-digest.sh`). Honours
    /// `AUGMENTAGENT_DIGEST_HOUR` / `AUGMENTAGENT_DIGEST_MINUTE` from the
    /// caller's environment.
    Digest {
        #[command(flatten)]
        flags: InstallFlags,
    },
    /// Multi-tenant daemon (`scripts/install-tenant.sh <name>`). The slug
    /// must be lowercase `[a-z0-9-]`; the underlying script enforces this.
    Tenant {
        /// Tenant slug (lowercase `[a-z0-9-]`, no leading/trailing `-`).
        #[arg(long)]
        name: String,
        #[command(flatten)]
        flags: InstallFlags,
    },
    /// Xvfb + headed Chromium + Python browser sidecar stack. Copies the
    /// three `.service` files from `systemd/` to `~/.config/systemd/user/`,
    /// reloads, and enables them.
    BrowserSidecar {
        #[command(flatten)]
        flags: InstallFlags,
    },
}

/// One component the `/setup` skill can uninstall. Mirrors `InstallComponent`
/// but carries `UninstallFlags` (no `--rebuild`).
#[derive(Subcommand, Clone, Debug)]
pub enum UninstallComponent {
    /// systemd user unit for the Rust daemon (`scripts/uninstall-autostart.sh`).
    Autostart {
        #[command(flatten)]
        flags: UninstallFlags,
    },
    /// 5-minute auto-update timer (`scripts/uninstall-autoupdate.sh`).
    Autoupdate {
        #[command(flatten)]
        flags: UninstallFlags,
    },
    /// Dashboard. NO upstream `uninstall-dashboard.sh` exists — this errors
    /// out with manual-removal instructions rather than silently no-opping.
    Dashboard {
        #[command(flatten)]
        flags: UninstallFlags,
    },
    /// Daily digest timer (`scripts/uninstall-digest.sh`).
    Digest {
        #[command(flatten)]
        flags: UninstallFlags,
    },
    /// Multi-tenant daemon (`scripts/uninstall-tenant.sh <name>`).
    Tenant {
        /// Tenant slug (lowercase `[a-z0-9-]`, no leading/trailing `-`).
        #[arg(long)]
        name: String,
        #[command(flatten)]
        flags: UninstallFlags,
    },
    /// Xvfb + headed Chromium + Python browser sidecar stack. Disables +
    /// stops the three units, removes the copied unit files, reloads.
    BrowserSidecar {
        #[command(flatten)]
        flags: UninstallFlags,
    },
}

impl InstallComponent {
    fn flags(&self) -> &InstallFlags {
        match self {
            InstallComponent::Autostart { flags }
            | InstallComponent::Autoupdate { flags }
            | InstallComponent::Dashboard { flags }
            | InstallComponent::Digest { flags }
            | InstallComponent::Tenant { flags, .. }
            | InstallComponent::BrowserSidecar { flags } => flags,
        }
    }
    fn label(&self) -> String {
        match self {
            InstallComponent::Autostart { .. } => "autostart".into(),
            InstallComponent::Autoupdate { .. } => "autoupdate".into(),
            InstallComponent::Dashboard { .. } => "dashboard".into(),
            InstallComponent::Digest { .. } => "digest".into(),
            InstallComponent::Tenant { name, .. } => format!("tenant:{name}"),
            InstallComponent::BrowserSidecar { .. } => "browser-sidecar".into(),
        }
    }
}

impl UninstallComponent {
    fn flags(&self) -> &UninstallFlags {
        match self {
            UninstallComponent::Autostart { flags }
            | UninstallComponent::Autoupdate { flags }
            | UninstallComponent::Dashboard { flags }
            | UninstallComponent::Digest { flags }
            | UninstallComponent::Tenant { flags, .. }
            | UninstallComponent::BrowserSidecar { flags } => flags,
        }
    }
    fn label(&self) -> String {
        match self {
            UninstallComponent::Autostart { .. } => "autostart".into(),
            UninstallComponent::Autoupdate { .. } => "autoupdate".into(),
            UninstallComponent::Dashboard { .. } => "dashboard".into(),
            UninstallComponent::Digest { .. } => "digest".into(),
            UninstallComponent::Tenant { name, .. } => format!("tenant:{name}"),
            UninstallComponent::BrowserSidecar { .. } => "browser-sidecar".into(),
        }
    }
}

/// Captured output from a child process (only populated in `--json` mode).
#[derive(Default)]
struct Captured {
    stdout: String,
    stderr: String,
}

/// Tail the last `max` chars of `s`, prefixing with `…` on truncation. Used
/// to keep the JSON summary small while preserving the actionable tail.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let cut = s.len().saturating_sub(max);
    let mut start = cut;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

/// JSON summary emitted to stdout when `--json` is set. Field shape matches
/// the issue spec verbatim.
fn emit_summary(component_label: &str, action: &str, succeeded: bool, cap: &Captured) {
    let summary = serde_json::json!({
        "component": component_label,
        "action": action,
        "succeeded": succeeded,
        "stdout_tail": tail(&cap.stdout, 4096),
        "stderr_tail": tail(&cap.stderr, 4096),
    });
    println!("{summary}");
}

/// Spawn `cmd` either streaming live (Stdio::inherit) or capturing both
/// streams for the JSON summary. Returns whether the child exited 0 and
/// the captured output (empty strings when streaming).
async fn run_child(mut cmd: Command, json: bool, cap: &mut Captured) -> Result<bool> {
    cmd.stdin(Stdio::null());
    if json {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().await.context("spawning child process")?;
        cap.stdout.push_str(&String::from_utf8_lossy(&out.stdout));
        cap.stderr.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok(out.status.success())
    } else {
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        let mut child = cmd.spawn().context("spawning child process")?;
        let status = child.wait().await.context("waiting on child process")?;
        Ok(status.success())
    }
}

/// `cargo build --release -p augmentagent-cli && npm run build`. Invoked
/// before the install script when `--rebuild` is set.
async fn rebuild(json: bool, cap: &mut Captured) -> Result<()> {
    let mut cargo = Command::new("cargo");
    cargo
        .arg("build")
        .arg("--release")
        .arg("-p")
        .arg("augmentagent-cli");
    if !run_child(cargo, json, cap).await? {
        anyhow::bail!(
            "rebuild failed: `cargo build --release -p augmentagent-cli` exited non-zero"
        );
    }
    let mut npm = Command::new("npm");
    npm.arg("run").arg("build");
    if !run_child(npm, json, cap).await? {
        anyhow::bail!("rebuild failed: `npm run build` exited non-zero");
    }
    Ok(())
}

/// Run a `scripts/<script>` invocation with optional extra args. The script
/// must already be executable (the repo's `install-git-hooks.sh` does that).
async fn run_script(
    script: &str,
    extra_args: &[&str],
    json: bool,
    cap: &mut Captured,
) -> Result<bool> {
    let dir = scripts_dir();
    let path = format!("{dir}/{script}");
    let mut cmd = Command::new(&path);
    for a in extra_args {
        cmd.arg(a);
    }
    run_child(cmd, json, cap)
        .await
        .with_context(|| format!("running {path}"))
}

/// Copy the three browser-sidecar unit files into `~/.config/systemd/user/`,
/// reload systemd, then enable + start them. Mirrors `systemd/README.md`.
async fn install_browser_sidecar(json: bool, cap: &mut Captured) -> Result<bool> {
    let dest = user_unit_dir()?;
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    let src_dir = systemd_dir();
    for unit in BROWSER_SIDECAR_UNITS {
        let src = PathBuf::from(&src_dir).join(unit);
        let dst = dest.join(unit);
        std::fs::copy(&src, &dst).with_context(|| {
            format!("copying {} → {}", src.display(), dst.display())
        })?;
        if !json {
            eprintln!("[install browser-sidecar] copied {} → {}", src.display(), dst.display());
        } else {
            cap.stdout
                .push_str(&format!("copied {} → {}\n", src.display(), dst.display()));
        }
    }
    let mut reload = Command::new("systemctl");
    reload.arg("--user").arg("daemon-reload");
    if !run_child(reload, json, cap).await? {
        return Ok(false);
    }
    let mut enable = Command::new("systemctl");
    enable.arg("--user").arg("enable").arg("--now");
    for unit in BROWSER_SIDECAR_UNITS {
        enable.arg(unit);
    }
    run_child(enable, json, cap).await
}

/// Inverse of `install_browser_sidecar`: disable + stop, remove unit files,
/// reload. Safe to re-run on a system that was never installed (systemctl
/// no-ops on missing units; we tolerate copy-removal failures).
async fn uninstall_browser_sidecar(json: bool, cap: &mut Captured) -> Result<bool> {
    let mut disable = Command::new("systemctl");
    disable.arg("--user").arg("disable").arg("--now");
    for unit in BROWSER_SIDECAR_UNITS {
        disable.arg(unit);
    }
    // Disable may fail if units don't exist — that's fine, keep going.
    let _ = run_child(disable, json, cap).await?;

    let dest = user_unit_dir()?;
    for unit in BROWSER_SIDECAR_UNITS {
        let path = dest.join(unit);
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                if json {
                    cap.stderr
                        .push_str(&format!("failed to remove {}: {e}\n", path.display()));
                } else {
                    eprintln!(
                        "[uninstall browser-sidecar] failed to remove {}: {e}",
                        path.display()
                    );
                }
            } else if !json {
                eprintln!("[uninstall browser-sidecar] removed {}", path.display());
            } else {
                cap.stdout
                    .push_str(&format!("removed {}\n", path.display()));
            }
        }
    }
    let mut reload = Command::new("systemctl");
    reload.arg("--user").arg("daemon-reload");
    run_child(reload, json, cap).await
}

/// Public entry point — `augmentagent install <component> [--rebuild] [--json]`.
pub async fn run_install(component: InstallComponent) -> Result<()> {
    let label = component.label();
    let flags = component.flags().clone();
    let mut cap = Captured::default();
    if flags.rebuild {
        if let Err(e) = rebuild(flags.json, &mut cap).await {
            if flags.json {
                emit_summary(&label, "install", false, &cap);
            }
            return Err(e);
        }
    }
    let ok = match &component {
        InstallComponent::Autostart { .. } => {
            run_script("install-autostart.sh", &[], flags.json, &mut cap).await?
        }
        InstallComponent::Autoupdate { .. } => {
            run_script("install-autoupdate.sh", &[], flags.json, &mut cap).await?
        }
        InstallComponent::Dashboard { .. } => {
            let script_ok =
                run_script("install-dashboard.sh", &[], flags.json, &mut cap).await?;
            // #33 — idempotently initialise the sqlite db so the very next
            // `augmentagent status --json` doesn't fail on a fresh box. Opening
            // the store creates the file (if missing) and runs migrations; on
            // a re-run it's a near no-op. Failure here MUST NOT fail the
            // install — the dashboard itself opens the db on first start, so
            // we log + continue.
            //
            // #45: Rust now owns the base schema (no longer waits for Node to
            // CREATE the tables Rust ALTERs), so the "no such table" race PR
            // #39 worked around is gone. The guard is retained because other
            // errors are still possible (disk full, permission denied, lock
            // contention).
            let db_path = std::env::var("AUGMENTAGENT_DB")
                .unwrap_or_else(|_| "./data.db".to_string());
            match augmentagent_store::Store::open(&db_path) {
                Ok(_) => {
                    tracing::info!(target: "augmentagent_cli::installers", "sqlite db ready at {db_path}");
                    if flags.json {
                        cap.stdout
                            .push_str(&format!("sqlite db ready at {db_path}\n"));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "augmentagent_cli::installers",
                        "could not init sqlite db at {db_path}: {e}. Dashboard will init on first run."
                    );
                    if flags.json {
                        cap.stderr.push_str(&format!(
                            "could not init sqlite db at {db_path}: {e}. Dashboard will init on first run.\n"
                        ));
                    }
                }
            }
            script_ok
        }
        InstallComponent::Digest { .. } => {
            run_script("install-digest.sh", &[], flags.json, &mut cap).await?
        }
        InstallComponent::Tenant { name, .. } => {
            run_script("install-tenant.sh", &[name.as_str()], flags.json, &mut cap).await?
        }
        InstallComponent::BrowserSidecar { .. } => {
            install_browser_sidecar(flags.json, &mut cap).await?
        }
    };
    if flags.json {
        emit_summary(&label, "install", ok, &cap);
        if !ok {
            anyhow::bail!("install {label} failed (see stderr_tail in JSON summary)");
        }
        Ok(())
    } else if ok {
        Ok(())
    } else {
        anyhow::bail!("install {label} exited non-zero");
    }
}

/// Public entry point — `augmentagent uninstall <component> [--json]`.
pub async fn run_uninstall(component: UninstallComponent) -> Result<()> {
    let label = component.label();
    let json = component.flags().json;
    let mut cap = Captured::default();
    let ok = match &component {
        UninstallComponent::Autostart { .. } => {
            run_script("uninstall-autostart.sh", &[], json, &mut cap).await?
        }
        UninstallComponent::Autoupdate { .. } => {
            run_script("uninstall-autoupdate.sh", &[], json, &mut cap).await?
        }
        UninstallComponent::Dashboard { .. } => {
            // No uninstall-dashboard.sh exists upstream. Surface the gap
            // explicitly rather than silently no-op or scope-creep a new
            // script in this change.
            let msg = "no `scripts/uninstall-dashboard.sh` exists — \
                       remove the dashboard unit manually \
                       (`systemctl --user disable --now augmentagent-dashboard.service && \
                       rm ~/.config/systemd/user/augmentagent-dashboard.service && \
                       systemctl --user daemon-reload`)";
            if json {
                cap.stderr.push_str(msg);
                emit_summary(&label, "uninstall", false, &cap);
            }
            anyhow::bail!(msg);
        }
        UninstallComponent::Digest { .. } => {
            run_script("uninstall-digest.sh", &[], json, &mut cap).await?
        }
        UninstallComponent::Tenant { name, .. } => {
            run_script("uninstall-tenant.sh", &[name.as_str()], json, &mut cap).await?
        }
        UninstallComponent::BrowserSidecar { .. } => {
            uninstall_browser_sidecar(json, &mut cap).await?
        }
    };
    if json {
        emit_summary(&label, "uninstall", ok, &cap);
        if !ok {
            anyhow::bail!("uninstall {label} failed (see stderr_tail in JSON summary)");
        }
        Ok(())
    } else if ok {
        Ok(())
    } else {
        anyhow::bail!("uninstall {label} exited non-zero");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_short_strings_intact() {
        assert_eq!(tail("hello", 10), "hello");
        assert_eq!(tail("", 10), "");
    }

    #[test]
    fn tail_truncates_long_strings_with_marker() {
        let s = "a".repeat(100);
        let t = tail(&s, 10);
        assert!(t.starts_with('…'));
        assert_eq!(t.chars().filter(|&c| c == 'a').count(), 10);
    }

    #[test]
    fn install_labels_round_trip() {
        let f = InstallFlags::default();
        assert_eq!(
            InstallComponent::Autostart { flags: f.clone() }.label(),
            "autostart"
        );
        assert_eq!(
            InstallComponent::Dashboard { flags: f.clone() }.label(),
            "dashboard"
        );
        assert_eq!(
            InstallComponent::BrowserSidecar { flags: f.clone() }.label(),
            "browser-sidecar"
        );
        assert_eq!(
            InstallComponent::Tenant {
                name: "code-coffee".into(),
                flags: f,
            }
            .label(),
            "tenant:code-coffee"
        );
    }

    /// #33 + #45 — the dashboard branch of `run_install` opens the store
    /// after the shell script returns to guarantee `$AUGMENTAGENT_DB` exists
    /// + migrated before the very next `augmentagent status --json`. Locks
    /// in three behaviours the dashboard branch relies on:
    ///
    /// 1. On a truly empty file, `Store::open` now SUCCEEDS — as of #45 the
    ///    Rust crate owns the schema and creates the 9 base tables itself
    ///    before ALTERing them, so it no longer needs the Node dashboard to
    ///    have run first. The install branch's warn-on-error guard still
    ///    wraps the call (other errors are still possible — disk full,
    ///    permission denied, etc.) but the "no such table" race window from
    ///    PR #39 is gone.
    /// 2. On a pre-seeded (legacy / Node-bootstrapped) db, `Store::open`
    ///    succeeds (its CREATE TABLE IF NOT EXISTS statements are no-ops
    ///    against the existing tables).
    /// 3. Re-opens of a migrated db remain safe (steady-state autostart).
    #[test]
    fn store_open_dashboard_bootstrap_is_safe_to_retry() {
        let tmp = std::env::temp_dir().join(format!(
            "augmentagent-issue-33-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);

        // (1) Empty file: must now succeed (Rust owns the schema as of #45).
        augmentagent_store::Store::open(&tmp)
            .expect("Store::open on an empty db should succeed after #45");

        // (2) Pre-seed a Node-style bootstrap (actions with the real columns
        // Node creates, including `messageId` so the matching index can be
        // built), then call Store::open. Mirrors the concurrent-startup
        // scenario where the dashboard initialised the DB before the Rust
        // daemon did.
        let _ = std::fs::remove_file(&tmp);
        {
            let c = rusqlite::Connection::open(&tmp).expect("open seed conn");
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS actions (\
                     id TEXT PRIMARY KEY,\
                     messageId TEXT NOT NULL,\
                     threadId TEXT,\
                     fromEmail TEXT NOT NULL,\
                     subject TEXT NOT NULL,\
                     originalBody TEXT,\
                     draftBody TEXT,\
                     status TEXT NOT NULL DEFAULT 'pending',\
                     errorMessage TEXT,\
                     createdAt INTEGER NOT NULL,\
                     updatedAt INTEGER NOT NULL\
                 ); \
                 CREATE TABLE IF NOT EXISTS emails (\
                     messageId TEXT PRIMARY KEY,\
                     accountEntityId TEXT,\
                     firstSeenAt INTEGER NOT NULL DEFAULT 0,\
                     triageResult TEXT,\
                     platform TEXT NOT NULL DEFAULT 'gmail'\
                 ); \
                 CREATE TABLE IF NOT EXISTS gmail_accounts (\
                     id TEXT PRIMARY KEY,\
                     active INTEGER DEFAULT 1\
                 );",
            )
            .expect("seed legacy schema");
        }
        augmentagent_store::Store::open(&tmp)
            .expect("first open on seeded db should succeed");
        // (3) Re-open must remain safe.
        augmentagent_store::Store::open(&tmp)
            .expect("re-open on migrated db should be a no-op");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn uninstall_labels_round_trip() {
        let f = UninstallFlags::default();
        assert_eq!(
            UninstallComponent::Digest { flags: f.clone() }.label(),
            "digest"
        );
        assert_eq!(
            UninstallComponent::Tenant {
                name: "alpha".into(),
                flags: f,
            }
            .label(),
            "tenant:alpha"
        );
    }
}
