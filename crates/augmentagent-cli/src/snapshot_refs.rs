//! Conservative references for retention. Unknown process or service state
//! cannot authorize deleting a rollback point.
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct References {
    pub paths: Vec<PathBuf>,
    pub config: String,
    pub reliable: bool,
}
impl References {
    pub fn protects(&self, path: &Path) -> bool {
        !self.reliable
            || path.join(".pin").exists()
            || self.paths.iter().any(|p| p == path || p.starts_with(path))
            || self.config.contains(path.to_string_lossy().as_ref())
    }
}

pub fn collect() -> References {
    use std::os::unix::fs::MetadataExt;
    let mut out = References {
        reliable: true,
        ..Default::default()
    };
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return References::default();
    };
    for entry in procs {
        let Ok(entry) = entry else {
            out.reliable = false;
            continue;
        };
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let p = entry.path();
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(p.join("status")) else {
            if p.exists() {
                out.reliable = false;
            }
            continue;
        };
        if status
            .lines()
            .any(|s| s.starts_with("State:") && (s.contains("Z (") || s.contains("X (")))
        {
            continue;
        }
        match std::fs::read_link(p.join("exe")) {
            Ok(exe) => out.paths.push(PathBuf::from(
                exe.to_string_lossy().trim_end_matches(" (deleted)"),
            )),
            Err(_) if p.exists() => {
                out.reliable = false;
            }
            Err(_) => {}
        }
        // Command arguments include script paths and configured rollback dirs.
        if let Ok(args) = std::fs::read(p.join("cmdline")) {
            out.config.push_str(&String::from_utf8_lossy(&args));
            out.config.push('\n');
        } else if p.exists() {
            out.reliable = false;
        }
        match std::fs::read_dir(p.join("fd")) {
            Ok(fds) => {
                for fd in fds.flatten() {
                    if let Ok(path) = std::fs::read_link(fd.path()) {
                        out.paths.push(path);
                    }
                }
            }
            Err(_) if p.exists() => {
                out.reliable = false;
            }
            Err(_) => {}
        }
    }
    // Include the effective unit and all user unit/drop-in files, including
    // dormant rollback references which do not appear in a running argv.
    let unit = std::process::Command::new("systemctl")
        .args(["--user", "cat", "augmentagent.service"])
        .output();
    match unit {
        Ok(v) if v.status.success() => out.config.push_str(&String::from_utf8_lossy(&v.stdout)),
        _ => out.reliable = false,
    }
    if let Some(home) = std::env::var_os("HOME") {
        read_config_tree(&PathBuf::from(home).join(".config/systemd/user"), &mut out);
    }
    out
}

fn read_config_tree(path: &Path, out: &mut References) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        if path.exists() {
            out.reliable = false;
        }
        return;
    };
    if meta.is_dir() {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(e) => read_config_tree(&e.path(), out),
                        Err(_) => out.reliable = false,
                    }
                }
            }
            Err(_) => out.reliable = false,
        }
    } else if meta.is_file()
        && matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("service" | "conf")
        )
    {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                out.config.push_str(&s);
                out.config.push('\n');
            }
            Err(_) => out.reliable = false,
        }
    }
}
