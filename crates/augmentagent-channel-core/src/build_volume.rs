//! One build-volume root; existing component overrides retain precedence.
use std::path::{Path, PathBuf};
pub const ENV: &str = "AUGMENTAGENT_BUILD_VOLUME";
fn setting(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    }
}
pub fn configured() -> Option<PathBuf> {
    setting(ENV).map(absolute)
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct Paths {
    pub gate: PathBuf,
    pub sessions: PathBuf,
    pub verifiers: PathBuf,
    pub scratch: PathBuf,
}
pub fn paths() -> Paths {
    for_volume(configured().as_deref())
}
pub fn for_volume(volume: Option<&Path>) -> Paths {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let gate = setting("AUGMENTAGENT_GATE_TARGET_DIR").unwrap_or_else(|| {
        volume
            .map(|v| v.join("augmentagent-gate-target"))
            .unwrap_or_else(|| home.join(".cache/augmentagent-gate-target"))
    });
    let sessions = setting("AUGMENTAGENT_BUILD_PARENT").unwrap_or_else(|| {
        volume
            .map(|v| v.join("augmentagent-session-targets"))
            .unwrap_or_else(|| {
                use std::os::unix::ffi::OsStrExt;
                let legacy = PathBuf::from("/mnt/build");
                let writable = std::ffi::CString::new(legacy.as_os_str().as_bytes())
                    .is_ok_and(|p| unsafe { libc::access(p.as_ptr(), libc::W_OK) } == 0);
                if legacy.is_dir() && writable {
                    legacy.join("augmentagent-session-targets")
                } else {
                    setting("XDG_CACHE_HOME")
                        .filter(|p| p.is_absolute())
                        .unwrap_or_else(|| home.join(".cache"))
                        .join("augmentagent-session-targets")
                }
            })
    });
    let verifiers = volume
        .map(|v| v.join("augmentagent-verifier-targets"))
        .unwrap_or_else(|| {
            absolute(gate.clone())
                .parent()
                .unwrap_or(Path::new("/"))
                .to_path_buf()
        });
    let scratch = setting(crate::build_scratch::SCRATCH_ENV).unwrap_or_else(|| {
        volume
            .map(|v| v.join("codex-vm"))
            .unwrap_or_else(|| PathBuf::from(crate::build_scratch::DEFAULT_SCRATCH_DIR))
    });
    Paths {
        gate: absolute(gate),
        sessions: absolute(sessions),
        verifiers: absolute(verifiers),
        scratch: absolute(scratch),
    }
}
pub fn verifier_parent(builder: &Path) -> PathBuf {
    configured()
        .map(|v| v.join("augmentagent-verifier-targets"))
        .unwrap_or_else(|| builder.parent().unwrap_or(Path::new("/")).to_path_buf())
}
pub fn watched_paths() -> Vec<(String, PathBuf)> {
    let paths = paths();
    [
        ("gate output", paths.gate),
        ("session output parent", paths.sessions),
        ("verifier output parent", paths.verifiers),
        ("VM build scratch", paths.scratch),
    ]
    .into_iter()
    .map(|(label, path)| (format!("{label} ({})", path.display()), path))
    .collect()
}
