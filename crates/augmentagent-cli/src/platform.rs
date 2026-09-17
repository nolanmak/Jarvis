//! #1079 — host service-manager detection and the launchd backend.
//!
//! The daemon ships as systemd user units on Linux and as launchd agents on
//! macOS. Every CLI surface that manages or probes those jobs (`service`,
//! `status`, `logs`, `install browser-sidecar`, `browser start|stop|status`,
//! the daemon-running pre-flights) asks this module which manager the host
//! has, and names jobs by their systemd unit name everywhere else. On macOS
//! that name is mapped to the launchd label the `scripts/install-*.sh`
//! installers already write (`com.nolanmak.augmentagent…`).
//!
//! On Linux every caller keeps its existing `systemctl`/`journalctl` path
//! unchanged; nothing here runs unless the manager is [`ServiceManager::Launchd`].

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Which service manager runs the augmentagent jobs on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceManager {
    Systemd,
    Launchd,
}

/// Env override, mainly for tests and odd hosts: `systemd` or `launchd`.
pub const MANAGER_ENV: &str = "AUGMENTAGENT_SERVICE_MANAGER";

impl ServiceManager {
    /// The manager for this host: [`MANAGER_ENV`] when set to a known value,
    /// else launchd on macOS and systemd everywhere else.
    pub fn detect() -> Self {
        Self::from_env_value(std::env::var(MANAGER_ENV).ok().as_deref())
            .unwrap_or(if cfg!(target_os = "macos") {
                ServiceManager::Launchd
            } else {
                ServiceManager::Systemd
            })
    }

    fn from_env_value(v: Option<&str>) -> Option<Self> {
        match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("systemd") => Some(ServiceManager::Systemd),
            Some("launchd") => Some(ServiceManager::Launchd),
            _ => None,
        }
    }

    pub fn is_launchd(self) -> bool {
        self == ServiceManager::Launchd
    }
}

/// Prefix every installer's launchd label shares.
pub const LABEL_PREFIX: &str = "com.nolanmak.augmentagent";

/// Map a systemd unit name to the launchd label the installers use for the
/// same job. A `.timer` and its `.service` map to one label (launchd folds
/// the schedule into the job). `None` for units with no macOS counterpart
/// (Xvfb: macOS has a real display) or that are not ours.
pub fn launchd_label(unit: &str) -> Option<String> {
    let base = unit
        .strip_suffix(".service")
        .or_else(|| unit.strip_suffix(".timer"))
        .unwrap_or(unit);
    match base {
        "augmentagent" => Some(LABEL_PREFIX.to_string()),
        "augmentagent-dashboard" => Some(format!("{LABEL_PREFIX}-dashboard")),
        "augmentagent-update" => Some(format!("{LABEL_PREFIX}.updater")),
        "augmentagent-xvfb" => None,
        other => other
            .strip_prefix("augmentagent-")
            .filter(|rest| !rest.is_empty())
            .map(|rest| format!("{LABEL_PREFIX}.{rest}")),
    }
}

/// Is `unit` a scheduled job? A loaded launchd job for a timer sits idle
/// between runs, so "loaded" is what systemd would call an active timer.
pub fn is_timer(unit: &str) -> bool {
    unit.ends_with(".timer")
}

/// `~/Library/LaunchAgents`.
pub fn launch_agents_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/LaunchAgents"))
}

/// `~/Library/LaunchAgents/<label>.plist`.
pub fn plist_path(label: &str) -> Option<PathBuf> {
    launch_agents_dir().map(|d| d.join(format!("{label}.plist")))
}

/// launchd's per-user GUI domain, `gui/<uid>`.
pub fn gui_domain() -> String {
    // SAFETY: getuid(2) cannot fail and has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// `gui/<uid>/<label>`.
pub fn service_target(label: &str) -> String {
    format!("{}/{label}", gui_domain())
}

/// What `launchctl print gui/<uid>/<label>` says about one job.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaunchdJob {
    /// launchd knows the job (it is bootstrapped into the domain).
    pub loaded: bool,
    /// `state = …` (`running`, `waiting`, `not running`, …).
    pub state: String,
    pub pid: Option<i64>,
    pub last_exit_code: Option<i64>,
}

impl LaunchdJob {
    pub fn running(&self) -> bool {
        self.loaded && self.state == "running"
    }
}

/// Parse the top-level `key = value` lines of `launchctl print` output. Only
/// the job's own block (one tab of indent) is read, so nested dictionaries
/// such as `environment` cannot shadow `state` or `pid`.
pub fn parse_launchctl_print(text: &str) -> LaunchdJob {
    let mut job = LaunchdJob {
        loaded: true,
        ..Default::default()
    };
    for line in text.lines() {
        let Some(body) = line.strip_prefix('\t') else {
            continue;
        };
        if body.starts_with('\t') {
            continue;
        }
        let Some((k, v)) = body.split_once(" = ") else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "state" => job.state = v.to_string(),
            "pid" => job.pid = v.parse().ok(),
            "last exit code" => job.last_exit_code = v.parse().ok(),
            _ => {}
        }
    }
    job
}

/// Query launchd for `label`. A job launchd does not know reads as not
/// loaded; so does a host without `launchctl`.
pub fn launchd_job(label: &str) -> LaunchdJob {
    let out = Command::new("launchctl")
        .arg("print")
        .arg(service_target(label))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => parse_launchctl_print(&String::from_utf8_lossy(&o.stdout)),
        _ => LaunchdJob::default(),
    }
}

/// Is `unit` up? `Some(true)` running (or, for a timer, armed), `Some(false)`
/// known and down, `None` when the manager could not be asked.
pub fn unit_is_active(unit: &str) -> Option<bool> {
    match ServiceManager::detect() {
        ServiceManager::Systemd => {
            let out = Command::new("systemctl")
                .args(["--user", "is-active", unit])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            match String::from_utf8_lossy(&out.stdout).trim() {
                "active" | "reloading" | "activating" | "deactivating" | "refreshing" => Some(true),
                "inactive" | "failed" => Some(false),
                _ => None,
            }
        }
        ServiceManager::Launchd => {
            let label = launchd_label(unit)?;
            let job = launchd_job(&label);
            Some(if is_timer(unit) { job.loaded } else { job.running() })
        }
    }
}

/// Start time of `pid` as a unix timestamp, from `ps -o lstart=`. `None` when
/// the process is gone or the output cannot be parsed.
pub fn process_start_unix(pid: i64) -> Option<i64> {
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_lstart(String::from_utf8_lossy(&out.stdout).trim())
}

/// `ps -o lstart=` prints local time as `Wed Sep 16 20:37:08 2026`.
fn parse_lstart(s: &str) -> Option<i64> {
    use chrono::TimeZone;
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let naive = chrono::NaiveDateTime::parse_from_str(&collapsed, "%a %b %d %H:%M:%S %Y").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.timestamp())
}

/// Is a process with this id still running? Linux keeps its `/proc` check;
/// elsewhere `kill(pid, 0)` answers the same question (EPERM still means the
/// process exists, just not ours).
pub fn pid_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        return std::path::Path::new(&format!("/proc/{pid}")).exists();
    }
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs only the existence/permission check.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The command that opens a URL in the desktop browser.
pub fn open_url_program() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Can this session open a browser window? Linux needs an X/Wayland display;
/// a macOS login session always has one.
pub fn has_desktop() -> bool {
    if cfg!(target_os = "macos") {
        return true;
    }
    !std::env::var("DISPLAY").unwrap_or_default().is_empty()
}

/// Shell hints for stopping/starting the daemon, for error messages.
pub fn daemon_stop_hint() -> &'static str {
    match ServiceManager::detect() {
        ServiceManager::Systemd => "systemctl --user stop augmentagent.service",
        ServiceManager::Launchd => "augmentagent service --unit daemon stop",
    }
}

pub fn daemon_start_hint() -> &'static str {
    match ServiceManager::detect() {
        ServiceManager::Systemd => "systemctl --user start augmentagent.service",
        ServiceManager::Launchd => "augmentagent service --unit daemon start",
    }
}

pub fn daemon_restart_hint() -> &'static str {
    match ServiceManager::detect() {
        ServiceManager::Systemd => "systemctl --user restart augmentagent.service",
        ServiceManager::Launchd => "augmentagent service --unit daemon restart",
    }
}

/// How to install a missing tool on this host, for doctor hints.
pub fn package_install_hint(apt_package: &'static str, brew_formula: &'static str) -> String {
    if cfg!(target_os = "macos") {
        format!("brew install {brew_formula}")
    } else {
        format!("apt-get install -y {apt_package}")
    }
}

/// Escape text for a plist `<string>`.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A long-running launchd agent, rendered to plist XML. Mirrors the shape
/// `scripts/install-autostart.sh` writes: run at load, restart on crash.
pub struct AgentPlist<'a> {
    pub label: &'a str,
    pub program_arguments: &'a [String],
    pub working_directory: Option<&'a str>,
    pub environment: &'a [(&'a str, String)],
    pub stdout_path: &'a str,
    pub stderr_path: &'a str,
}

impl AgentPlist<'_> {
    pub fn render(&self) -> String {
        let mut s = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n",
        );
        s.push_str(&format!(
            "    <key>Label</key>\n    <string>{}</string>\n",
            xml_escape(self.label)
        ));
        if let Some(wd) = self.working_directory {
            s.push_str(&format!(
                "    <key>WorkingDirectory</key>\n    <string>{}</string>\n",
                xml_escape(wd)
            ));
        }
        s.push_str("    <key>ProgramArguments</key>\n    <array>\n");
        for a in self.program_arguments {
            s.push_str(&format!("        <string>{}</string>\n", xml_escape(a)));
        }
        s.push_str("    </array>\n");
        if !self.environment.is_empty() {
            s.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
            for (k, v) in self.environment {
                s.push_str(&format!(
                    "        <key>{}</key>\n        <string>{}</string>\n",
                    xml_escape(k),
                    xml_escape(v)
                ));
            }
            s.push_str("    </dict>\n");
        }
        s.push_str(
            "    <key>RunAtLoad</key>\n    <true/>\n\
             \x20   <key>KeepAlive</key>\n    <dict>\n\
             \x20       <key>SuccessfulExit</key>\n        <false/>\n\
             \x20       <key>Crashed</key>\n        <true/>\n\
             \x20   </dict>\n\
             \x20   <key>ThrottleInterval</key>\n    <integer>10</integer>\n",
        );
        s.push_str(&format!(
            "    <key>StandardOutPath</key>\n    <string>{}</string>\n",
            xml_escape(self.stdout_path)
        ));
        s.push_str(&format!(
            "    <key>StandardErrorPath</key>\n    <string>{}</string>\n",
            xml_escape(self.stderr_path)
        ));
        s.push_str("</dict>\n</plist>\n");
        s
    }
}

/// Is `label` marked disabled in the GUI domain (`launchctl disable`)?
pub fn launchd_disabled(label: &str) -> bool {
    Command::new("launchctl")
        .args(["print-disabled", &gui_domain()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| parse_print_disabled(&String::from_utf8_lossy(&o.stdout), label))
        .unwrap_or(false)
}

/// `launchctl print-disabled` lines read `"label" => disabled|enabled`
/// (older macOS: `=> true|false`, where true means disabled).
fn parse_print_disabled(text: &str, label: &str) -> bool {
    let quoted = format!("\"{label}\"");
    text.lines()
        .filter_map(|l| l.trim().split_once(" => "))
        .find(|(k, _)| *k == quoted)
        .is_some_and(|(_, v)| matches!(v.trim(), "disabled" | "true"))
}

/// Run `launchctl <args>`, returning whether it exited 0 and its stderr.
pub fn launchctl(args: &[&str]) -> std::io::Result<(bool, String)> {
    let out = Command::new("launchctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    Ok((out.status.success(), String::from_utf8_lossy(&out.stderr).trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_parses_known_values_only() {
        assert_eq!(ServiceManager::from_env_value(Some("launchd")), Some(ServiceManager::Launchd));
        assert_eq!(ServiceManager::from_env_value(Some(" SYSTEMD ")), Some(ServiceManager::Systemd));
        assert_eq!(ServiceManager::from_env_value(Some("runit")), None);
        assert_eq!(ServiceManager::from_env_value(None), None);
    }

    #[test]
    fn labels_match_the_shell_installers() {
        // scripts/install-autostart.sh, install-dashboard.sh, install-autoupdate.sh
        assert_eq!(launchd_label("augmentagent.service").as_deref(), Some("com.nolanmak.augmentagent"));
        assert_eq!(
            launchd_label("augmentagent-dashboard.service").as_deref(),
            Some("com.nolanmak.augmentagent-dashboard")
        );
        assert_eq!(
            launchd_label("augmentagent-update.timer").as_deref(),
            Some("com.nolanmak.augmentagent.updater")
        );
        assert_eq!(
            launchd_label("augmentagent-update.service").as_deref(),
            Some("com.nolanmak.augmentagent.updater")
        );
        // install-digest.sh / install-calendar.sh / install-research.sh / install-wix-sync.sh
        for (unit, label) in [
            ("augmentagent-digest.timer", "com.nolanmak.augmentagent.digest"),
            ("augmentagent-calendar.timer", "com.nolanmak.augmentagent.calendar"),
            ("augmentagent-research.timer", "com.nolanmak.augmentagent.research"),
            ("augmentagent-wix-sync.timer", "com.nolanmak.augmentagent.wix-sync"),
            ("augmentagent-browser-sidecar.service", "com.nolanmak.augmentagent.browser-sidecar"),
            ("augmentagent-tenant-acme.service", "com.nolanmak.augmentagent.tenant-acme"),
        ] {
            assert_eq!(launchd_label(unit).as_deref(), Some(label), "{unit}");
        }
    }

    #[test]
    fn units_without_a_macos_job_have_no_label() {
        assert_eq!(launchd_label("augmentagent-xvfb.service"), None);
        assert_eq!(launchd_label("nginx.service"), None);
        assert_eq!(launchd_label("augmentagent-"), None);
    }

    #[test]
    fn parses_a_running_job() {
        let text = "gui/501/com.nolanmak.augmentagent = {\n\
                    \tactive count = 1\n\
                    \tpath = /Users/x/Library/LaunchAgents/com.nolanmak.augmentagent.plist\n\
                    \tstate = running\n\
                    \n\
                    \tenvironment = {\n\
                    \t\tstate = decoy\n\
                    \t\tpid = 1\n\
                    \t}\n\
                    \tpid = 4242\n\
                    \tlast exit code = (never exited)\n\
                    }\n";
        let job = parse_launchctl_print(text);
        assert!(job.loaded);
        assert!(job.running());
        assert_eq!(job.pid, Some(4242));
        assert_eq!(job.last_exit_code, None);
    }

    #[test]
    fn parses_an_idle_scheduled_job() {
        let text = "gui/501/com.nolanmak.augmentagent.updater = {\n\
                    \tstate = not running\n\
                    \tlast exit code = 0\n\
                    }\n";
        let job = parse_launchctl_print(text);
        assert!(job.loaded);
        assert!(!job.running());
        assert_eq!(job.pid, None);
        assert_eq!(job.last_exit_code, Some(0));
    }

    #[test]
    fn reads_the_disabled_bit() {
        let text = "disabled services = {\n\
                    \t\"com.nolanmak.augmentagent\" => disabled\n\
                    \t\"com.nolanmak.augmentagent.updater\" => enabled\n\
                    \t\"com.nolanmak.augmentagent.digest\" => true\n\
                    }\n";
        assert!(parse_print_disabled(text, "com.nolanmak.augmentagent"));
        assert!(!parse_print_disabled(text, "com.nolanmak.augmentagent.updater"));
        assert!(parse_print_disabled(text, "com.nolanmak.augmentagent.digest"));
        // A prefix of another label must not match.
        assert!(!parse_print_disabled(text, "com.nolanmak.augmentagent.dig"));
        assert!(!parse_print_disabled(text, "com.nolanmak.augmentagent-dashboard"));
    }

    #[test]
    fn unknown_job_is_not_loaded() {
        assert!(!LaunchdJob::default().loaded);
        assert!(!LaunchdJob::default().running());
    }

    #[test]
    fn parses_ps_lstart() {
        let ts = parse_lstart("Wed Sep 16 20:37:08 2026").expect("parses");
        assert!(ts > 1_780_000_000, "{ts}");
        // ps pads single-digit days with a space.
        assert!(parse_lstart("Thu Sep  3 09:05:01 2026").is_some());
        assert!(parse_lstart("").is_none());
    }

    #[test]
    fn current_process_is_alive() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(u32::MAX - 1));
    }

    #[test]
    fn plist_renders_escaped_and_complete() {
        let args = vec!["/bin/echo".to_string(), "a&b".to_string()];
        let env = [("PATH", "/usr/bin:/bin".to_string())];
        let p = AgentPlist {
            label: "com.nolanmak.augmentagent.test",
            program_arguments: &args,
            working_directory: Some("/tmp/x"),
            environment: &env,
            stdout_path: "/tmp/out.log",
            stderr_path: "/tmp/err.log",
        }
        .render();
        assert!(p.contains("<string>com.nolanmak.augmentagent.test</string>"));
        assert!(p.contains("<string>a&amp;b</string>"));
        assert!(p.contains("<key>WorkingDirectory</key>"));
        assert!(p.contains("<key>StandardErrorPath</key>\n    <string>/tmp/err.log</string>"));
        assert!(p.trim_end().ends_with("</plist>"));
    }
}
