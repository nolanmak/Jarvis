//! Codex launch policy and constrained tool bridge (#1019).
use std::path::{Path, PathBuf};
use crate::reasoner::ReasonerOpts;

fn configured_vm_path(override_path: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(path) = override_path.filter(|p| !p.is_empty()) {
        // A broken explicit override must fail, never silently select another runtime.
        return Some(PathBuf::from(path));
    }
    let path = PathBuf::from(home?).join(".local/share/augmentagent/build-vm/runtime.json");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        _ => Some(path), // unreadable/symlinked configuration is checked by the broker
    }
}

pub(crate) fn build_vm_config_path() -> Option<PathBuf> {
    configured_vm_path(std::env::var_os("AUGMENTAGENT_BUILD_VM_CONFIG"), std::env::var_os("HOME"))
}

pub struct BridgeLaunch {
    pub native_cwd: PathBuf,
    pub config_overrides: Vec<String>,
    pub policy_path: PathBuf,
}

impl BridgeLaunch {
    pub fn prepare(opts: &ReasonerOpts, directory: &Path) -> anyhow::Result<Self> {
        use std::collections::BTreeMap;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use serde_json::json;

        let settings: serde_json::Value = match &opts.settings_json {
            Some(raw) => serde_json::from_str(raw)?,
            None => json!({}),
        };
        let object = settings.as_object().ok_or_else(|| anyhow::anyhow!("unsupported settings shape"))?;
        if object.keys().any(|k| !matches!(k.as_str(), "hooks" | "mcpServers")) {
            anyhow::bail!("unsupported settings: refusing to drop provider policy");
        }
        let web_search = opts.allowed_tools.iter().any(|tool| tool == "WebSearch");
        let web_fetch = opts.allowed_tools.iter().any(|tool| tool == "WebFetch");
        if web_search || web_fetch {
            // Native web exposes search and page retrieval as one capability.
            // It does not traverse the bridge's original PreToolUse hooks.
            anyhow::ensure!(web_search && web_fetch,
                "native web requires both WebSearch and WebFetch; refusing broader tool access");
            if let Some(groups) = settings.pointer("/hooks/PreToolUse").and_then(|value| value.as_array()) {
                for group in groups {
                    if !group.get("hooks").and_then(|value| value.as_array()).is_some_and(|hooks| !hooks.is_empty()) {
                        continue;
                    }
                    let matcher = group.get("matcher").and_then(|value| value.as_str()).unwrap_or(".*");
                    let matcher = regex::Regex::new(&format!("\\A(?:{matcher})\\z"))
                        .map_err(|_| anyhow::anyhow!("unsupported native web guard matcher"))?;
                    anyhow::ensure!(!matcher.is_match("WebSearch") && !matcher.is_match("WebFetch"),
                        "native web cannot enforce the configured web hook; refusing to bypass it");
                }
            }
        }
        let cwd = opts.cwd.as_ref().or(opts.add_dirs.first()).cloned()
            .unwrap_or(std::env::current_dir()?).canonicalize()?;
        let mut roots = opts.add_dirs.iter().map(|p| p.canonicalize())
            .collect::<Result<Vec<_>, _>>()?;
        if !roots.contains(&cwd) { roots.push(cwd.clone()); }
        let writes = opts.allowed_tools.iter().any(|t| matches!(t.as_str(), "Write" | "Edit" | "NotebookEdit"));
        let write_roots = if writes { vec![cwd.clone()] } else { vec![] };
        let mut environment: BTreeMap<String, String> = BTreeMap::new();
        for key in ["HOME", "PATH", "USER", "LOGNAME", "LANG", "TERM", "DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
            if let Ok(value) = std::env::var(key) { environment.insert(key.into(), value); }
        }
        environment.extend(opts.env.iter().cloned());
        let policy = json!({
            "cwd": cwd,
            "read_roots": roots,
            "write_roots": write_roots,
            "allowed_tools": opts.allowed_tools,
            "environment": environment,
            "settings": settings,
            "session_id": opts.session_id,
            "handoff_path": opts.handoff_path,
            // Operator configuration, deliberately not sourced from opts.env.
            "build_vm_config": build_vm_config_path(),
        });
        fn private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
            f.write_all(bytes)?;
            Ok(())
        }
        let directory = directory.canonicalize()?;
        let policy_path = directory.join("tool-policy.json");
        let server_path = directory.join("tool-bridge.py");
        private_file(&policy_path, &serde_json::to_vec(&policy)?)?;
        private_file(&server_path, include_bytes!("../../../scripts/codex-tool-bridge.py"))?;
        private_file(&directory.join("codex-command-sandbox.py"), include_bytes!("../../../scripts/codex-command-sandbox.py"))?;
        private_file(&directory.join("codex-build-vm.py"), include_bytes!("../../../scripts/codex-build-vm.py"))?;
        private_file(&directory.join("provider-supervisor.py"), include_bytes!("../../../scripts/provider-supervisor.py"))?;
        let native_cwd = directory.join("native-workspace");
        std::fs::create_dir(&native_cwd)?;
        // Codex sees no project-local config/AGENTS from the tool workspace.
        // All file access is through the bridge. Keep native reads minimal
        // and writes/network denied, even if a native tool remains visible.
        let mut config_overrides = vec![
            "default_permissions=jarvis_bridge".into(),
            "permissions.jarvis_bridge={filesystem={\":minimal\"=\"read\"},network={enabled=false}}".into(),
            "project_doc_max_bytes=0".into(),
            "web_search=disabled".into(),
        ];
        if web_search && web_fetch {
            config_overrides.push("web_search=live".into());
        }
        for feature in ["shell_tool", "apps", "plugins", "multi_agent", "browser_use",
                        "computer_use", "image_generation", "view_image", "skill_search",
                        "skill_mcp_dependency_install", "shell_snapshot"] {
            config_overrides.push(format!("features.{feature}=false"));
        }
        config_overrides.push(format!(
            "mcp_servers.jarvis={{command=\"python3\",args=[\"-I\",{},{}],required=true,startup_timeout_sec=120,tool_timeout_sec=900,default_tools_approval_mode=\"approve\"}}",
            serde_json::to_string(&server_path)?, serde_json::to_string(&policy_path)?
        ));
        Ok(Self { native_cwd, config_overrides, policy_path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn native_web_cannot_expand_a_fetch_only_profile_or_bypass_matching_hooks() {
        let fixture = tempfile::tempdir().unwrap();
        for tools in [vec!["WebFetch"], vec!["WebSearch"]] {
            let mut opts = crate::reasoner::resume_opts(fixture.path().into());
            opts.allowed_tools = tools.into_iter().map(str::to_string).collect();
            let launch = tempfile::tempdir().unwrap();
            assert!(BridgeLaunch::prepare(&opts, launch.path()).is_err());
        }
        for matcher in ["WebSearch", "WebFetch", ".*", "Read|WebFetch"] {
            let mut opts = crate::reasoner::resume_opts(fixture.path().into());
            opts.allowed_tools = vec!["WebSearch".into(), "WebFetch".into()];
            opts.settings_json = Some(serde_json::json!({"hooks":{"PreToolUse":[{
                "matcher":matcher,"hooks":[{"type":"command","command":"true"}]
            }]}}).to_string());
            let launch = tempfile::tempdir().unwrap();
            assert!(BridgeLaunch::prepare(&opts, launch.path()).is_err(), "web guard was ignored: {matcher}");
        }
        let opts = crate::reasoner::ask_opts(fixture.path().into(), fixture.path().into());
        let launch = tempfile::tempdir().unwrap();
        assert!(BridgeLaunch::prepare(&opts, launch.path()).is_ok(), "file-only query hooks must remain supported");
    }

    #[test]
    fn vm_configuration_uses_durable_default_and_preserves_explicit_overrides() {
        let home = tempfile::tempdir().unwrap();
        let home_arg = Some(home.path().as_os_str().to_owned());
        assert_eq!(configured_vm_path(None, home_arg.clone()), None);
        let default = home.path().join(".local/share/augmentagent/build-vm/runtime.json");
        std::fs::create_dir_all(default.parent().unwrap()).unwrap();
        std::fs::write(&default, "{}").unwrap();
        assert_eq!(configured_vm_path(None, home_arg.clone()), Some(default));
        let explicit = home.path().join("missing-explicit.json");
        assert_eq!(configured_vm_path(Some(explicit.as_os_str().to_owned()), home_arg), Some(explicit));
        assert_eq!(configured_vm_path(None, None), None);
    }

    #[test]
    fn launch_keeps_native_tools_confined_and_private_config_out_of_argv() {
        let temp = tempfile::tempdir().unwrap();
        let wiki = temp.path().join("wiki");
        let transcripts = temp.path().join("transcripts");
        let launch_dir = temp.path().join("launch");
        for dir in [&wiki, &transcripts, &launch_dir] { std::fs::create_dir(dir).unwrap(); }
        let mut opts = crate::reasoner::ask_opts(wiki.clone(), temp.path().into());
        opts.add_dirs.push(transcripts.clone());
        opts.env.push(("SYNTHETIC_TOKEN".into(), "secret-fixture-only".into()));
        opts.env.push(("AUGMENTAGENT_BUILD_VM_CONFIG".into(), "untrusted-profile-override".into()));
        opts.handoff_path = Some(temp.path().join("private-handoff.json"));
        let launch = BridgeLaunch::prepare(&opts, &launch_dir).unwrap();
        assert!(launch.native_cwd.starts_with(&launch_dir));
        assert_ne!(launch.native_cwd, wiki);
        let args = launch.config_overrides.join("\n");
        assert!(args.contains("features.shell_tool=false"));
        assert!(args.contains("features.view_image=false"));
        assert!(args.contains("features.plugins=false"));
        assert!(args.contains("default_permissions=jarvis_bridge"));
        assert!(!args.contains("secret-fixture-only"));
        assert!(!args.contains("private-handoff.json"));
        assert!(!args.contains("danger-full-access"));
        let policy: serde_json::Value = serde_json::from_slice(&std::fs::read(&launch.policy_path).unwrap()).unwrap();
        assert_eq!(policy["handoff_path"], serde_json::json!(opts.handoff_path));
        assert_ne!(policy["build_vm_config"], "untrusted-profile-override");
        for helper in ["codex-build-vm.py", "provider-supervisor.py"] {
            assert_eq!(std::fs::metadata(launch_dir.join(helper)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(policy["write_roots"], serde_json::json!([wiki]));
        assert!(policy["read_roots"].as_array().unwrap().contains(&serde_json::json!(transcripts)));
        assert_eq!(std::fs::metadata(launch.policy_path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn unrecognized_settings_are_not_silently_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let mut opts = crate::reasoner::resume_opts(temp.path().into());
        opts.settings_json = Some(r#"{"futureSecurityPolicy":true}"#.into());
        let error = BridgeLaunch::prepare(&opts, temp.path()).err().unwrap().to_string();
        assert!(error.contains("unsupported settings"), "{error}");
    }

    #[test]
    fn readonly_preset_has_no_writable_roots() {
        let temp = tempfile::tempdir().unwrap();
        let opts = crate::reasoner::triage_opts(Some(temp.path().into()));
        let launch = BridgeLaunch::prepare(&opts, temp.path()).unwrap();
        let policy: serde_json::Value = serde_json::from_slice(&std::fs::read(launch.policy_path).unwrap()).unwrap();
        assert_eq!(policy["write_roots"], serde_json::json!([]));
    }
}
