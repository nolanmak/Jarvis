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

// #1045 — single-file Read exceptions outside the read roots.
//
// The query preset's scope guard (`scripts/aa-wiki-scope-guard.sh`, #127)
// lets Claude `Read`, and nothing else, two kinds of inbound attachment temp
// file. This is the one definition of those exceptions. The bridge policy is
// built from it at launch, and the guard's regexes are pinned to it by
// `scope_guard_carve_outs_mirror_the_read_allowance_definition`, so the same
// attachment paths are readable under Claude and Codex. The guard keeps its
// own copy on purpose: it is the only check on the Claude path and a second,
// independent check inside the bridge, so it must not take its policy from an
// environment value.

/// Hook script whose Read carve-outs these definitions describe.
pub const SCOPE_GUARD_SCRIPT: &str = "aa-wiki-scope-guard.sh";
/// Discord attachments (augmentagent-approval-discord, #441/#939) are written
/// to `/tmp/aa-{img,txt,doc}-<msg_id>-<idx>.<ext>`.
pub const DISCORD_ATTACHMENT_DIR: &str = "/tmp";
pub const DISCORD_ATTACHMENT_NAME: &str = r"aa-(txt|img|doc)-[0-9]+-[0-9]+\.[a-zA-Z0-9]+";
/// `imessage fetch-attachment` (#888) saves into the ask session's own
/// directory, minted by `ask_opts` under this root and named by this variable.
pub const IMESSAGE_SESSION_DIR_ENV: &str = "AUGMENTAGENT_IMESSAGE_TMP_DIR";
pub const IMESSAGE_ATTACHMENT_ROOT: &str = "/tmp/aa-imsg";
/// One path segment: the session directory name and the CLI-sanitized file name.
pub const PORTABLE_NAME: &str = r"[A-Za-z0-9._-]+";

/// Read, and only Read, of a file directly inside `directory` whose whole name
/// matches `name_pattern`. The bridge opens the directory without following
/// symlinks and requires a private regular file owned by the daemon's user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadAllowance {
    pub directory: PathBuf,
    pub name_pattern: &'static str,
}

impl ReadAllowance {
    fn policy(&self) -> serde_json::Value {
        serde_json::json!({"tools": ["Read"], "directory": self.directory, "name_pattern": self.name_pattern})
    }
}

fn full_match(pattern: &str, value: &str) -> bool {
    regex::Regex::new(&format!(r"\A(?:{pattern})\z")).is_ok_and(|expression| expression.is_match(value))
}

/// The session directory the guard accepts: exactly one portable segment
/// below [`IMESSAGE_ATTACHMENT_ROOT`]. `.` and `..` fit the segment pattern but
/// name no child; the guard can never match them because it resolves the
/// requested path first, so they grant nothing here either.
pub fn imessage_session_dir(value: &str) -> Option<PathBuf> {
    let name = value.strip_prefix(IMESSAGE_ATTACHMENT_ROOT)?.strip_prefix('/')?;
    (full_match(PORTABLE_NAME, name) && name != "." && name != "..").then(|| PathBuf::from(value))
}

/// Whether `opts` runs the scope guard on Read: Claude grants the carve-outs
/// exactly there, and the bridge runs the same hook before every Read.
fn runs_scope_guard_on_read(opts: &ReasonerOpts) -> bool {
    let Some(settings) = opts.settings_json.as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok()) else { return false };
    let groups = settings.pointer("/hooks/PreToolUse").and_then(|value| value.as_array());
    groups.into_iter().flatten().any(|group| {
        let matcher = group.get("matcher").and_then(|value| value.as_str()).unwrap_or(".*");
        full_match(matcher, "Read") && group.get("hooks").and_then(|value| value.as_array())
            .into_iter().flatten().any(|hook| {
                hook.get("type").and_then(|value| value.as_str()) == Some("command")
                    && hook.get("command").and_then(|value| value.as_str())
                        .and_then(|command| Path::new(command).file_name())
                        .is_some_and(|name| name == SCOPE_GUARD_SCRIPT)
            })
    })
}

/// The Read exceptions `opts` is entitled to: none unless it allows Read under
/// the scope guard; then Discord attachments, plus this session's iMessage
/// attachment directory when one was minted (last assignment wins, as in the
/// spawned environment).
pub fn read_allowances(opts: &ReasonerOpts) -> Vec<ReadAllowance> {
    if !opts.allowed_tools.iter().any(|tool| tool == "Read") || !runs_scope_guard_on_read(opts) {
        return Vec::new();
    }
    let mut allowances = vec![ReadAllowance {
        directory: PathBuf::from(DISCORD_ATTACHMENT_DIR),
        name_pattern: DISCORD_ATTACHMENT_NAME,
    }];
    let session = opts.env.iter().rev().find(|(key, _)| key == IMESSAGE_SESSION_DIR_ENV)
        .and_then(|(_, value)| imessage_session_dir(value));
    if let Some(directory) = session {
        allowances.push(ReadAllowance { directory, name_pattern: PORTABLE_NAME });
    }
    allowances
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
            // Pattern-scoped single files, never directories to walk (#1045).
            "read_allowances": read_allowances(opts).iter().map(ReadAllowance::policy).collect::<Vec<_>>(),
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
        private_file(&directory.join("build-dependency-proxy.py"), include_bytes!("../../../scripts/build-dependency-proxy.py"))?;
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
        for helper in ["codex-build-vm.py", "build-dependency-proxy.py", "provider-supervisor.py"] {
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

    /// The guard's carve-out regexes are rendered from the definition, gate on
    /// Read alone, and no other path regex hides in the guard (#1045 C2).
    #[test]
    fn scope_guard_carve_outs_mirror_the_read_allowance_definition() {
        use std::collections::{BTreeMap, BTreeSet};
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/aa-wiki-scope-guard.sh");
        assert!(path.ends_with(SCOPE_GUARD_SCRIPT));
        let script = std::fs::read_to_string(path).unwrap();
        // Directories are inserted literally, so they must mean the same as regex text everywhere.
        for directory in [DISCORD_ATTACHMENT_DIR, IMESSAGE_ATTACHMENT_ROOT] {
            assert!(directory.chars().all(|c| c.is_ascii_alphanumeric() || "/_-".contains(c)), "{directory}");
        }
        // Logical statements: comment lines dropped, `\` continuations joined.
        let code = script.lines().filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>().join("\n").replace("\\\n", " ");
        let mut found = BTreeMap::new();
        for statement in code.lines() {
            for operand in statement.split("=~").skip(1) {
                found.insert(operand.split_whitespace().next().unwrap().to_string(), statement.to_string());
            }
        }
        let discord = format!("^{DISCORD_ATTACHMENT_DIR}/{DISCORD_ATTACHMENT_NAME}$");
        let session_dir = format!("^{IMESSAGE_ATTACHMENT_ROOT}/{PORTABLE_NAME}$");
        let session_file = format!("^\"${IMESSAGE_SESSION_DIR_ENV}\"/{PORTABLE_NAME}$");
        // The transcript clone's tool gate is the guard's only other regex.
        let transcript_tools = "^(Read|Glob|Grep)$".to_string();
        assert_eq!(found.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([discord.clone(), session_dir.clone(), session_file.clone(), transcript_tools]),
            "scope guard regexes drifted from the read allowance definition");
        for carve_out in [&discord, &session_dir, &session_file] {
            let statement = &found[carve_out];
            assert!(statement.contains(r#""$TOOL" == "Read""#), "not Read-gated: {statement}");
            for tool in ["Glob", "Grep", "Write", "Edit"] {
                assert!(!statement.contains(tool), "{tool} in a Read carve-out: {statement}");
            }
        }
        assert_eq!(found[&session_dir], found[&session_file], "session file regex must require a validated session dir");
    }

    /// The same names are admitted by the real guard (bash ERE, in the daemon's
    /// UTF-8 locale) and the definition (Rust regex); the bridge's Python dialect
    /// is exercised by the capability inventory's paired provider test. The
    /// guard strips a trailing newline from the resolved path (command
    /// substitution), so only interior control characters are probed.
    #[test]
    fn scope_guard_and_definition_agree_on_attachment_names() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        if Command::new("jq").arg("--version").output().is_err() {
            return;
        }
        let guard = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/aa-wiki-scope-guard.sh");
        let wiki = tempfile::tempdir().unwrap();
        let session = "/tmp/aa-imsg/1045-17";
        let guard_allows = |path: &str| -> bool {
            let mut command = Command::new("bash");
            command.arg(guard).env_clear().env("WIKI_ROOT", wiki.path()).env(IMESSAGE_SESSION_DIR_ENV, session);
            // The daemon's locale: bash bracket ranges are collation-dependent.
            command.env("PATH", std::env::var_os("PATH").unwrap()).env("LANG", "en_US.UTF-8");
            let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(serde_json::json!({"tool_name": "Read",
                "tool_input": {"file_path": path}}).to_string().as_bytes()).unwrap();
            let output = child.wait_with_output().unwrap();
            output.status.success() && !String::from_utf8_lossy(&output.stdout).contains("\"block\"")
        };
        for name in ["aa-txt-1045-0.md", "aa-img-1045-12.PNG", "aa-doc-1045-3.txt", "aa-txt-..", "aa-txt-1045-0.",
                     "aa-txt-1045-0", "aa-txt-1045-0.md.bak", "aa-TXT-1045-0.md", "aa-pdf-1045-0.md",
                     "aa-txt--1045-0.md", "aa-txt-1045-0.m d", "aa-txt-1045-0.m\u{e9}", "aa-txt-\u{661}-0.md",
                     "xaa-txt-1045-0.md", "aa-txt-1045-0\n.md", "aa-txt-1045-0.md/x", "aa-\u{3c4}xt-1045-0.md"] {
            let expected = full_match(DISCORD_ATTACHMENT_NAME, name);
            assert_eq!(guard_allows(&format!("{DISCORD_ATTACHMENT_DIR}/{name}")), expected, "{name:?}");
        }
        assert!(full_match(DISCORD_ATTACHMENT_NAME, "aa-txt-1045-0.md"));
        for name in ["9-IMG_001-3fa2b1c0.jpeg", "note.txt", ".", "..", "a b", "a/b", "\u{e9}", "\u{df}", "", "a\nb"] {
            let expected = full_match(PORTABLE_NAME, name) && name != "." && name != "..";
            assert_eq!(guard_allows(&format!("{session}/{name}")), expected, "{name:?}");
        }
    }

    #[test]
    fn read_allowance_patterns_are_portable_single_segment_names() {
        // Literals, bracket ranges, groups, alternation, `+` and `\.`: the same in
        // POSIX ERE (the guard), Rust regex and Python `re` (the bridge).
        let portable = regex::Regex::new(r"\A(?:[A-Za-z0-9_-]|\\\.|\[[A-Za-z0-9._-]+\]|[()|+])+\z").unwrap();
        for pattern in [DISCORD_ATTACHMENT_NAME, PORTABLE_NAME] {
            assert!(portable.is_match(pattern), "{pattern}");
            for name in ["", "/", "a/b", "aa-txt-1-0.md/", "/aa-txt-1-0.md", "a\nb", "a\0b"] {
                assert!(!full_match(pattern, name), "{pattern} admits {name:?}");
            }
        }
    }

    #[test]
    fn imessage_session_dir_is_exactly_one_portable_child_of_the_root() {
        assert_eq!(imessage_session_dir("/tmp/aa-imsg/4242-17"), Some(PathBuf::from("/tmp/aa-imsg/4242-17")));
        for value in ["", "/tmp/aa-imsg", "/tmp/aa-imsg/", "/tmp/aa-imsg/.", "/tmp/aa-imsg/..", "/tmp/aa-imsg/a/b",
                      "/tmp/aa-imsg/a b", "/tmp/aa-imsgX/a", "/tmp/aa-imsg//a", "tmp/aa-imsg/a", "/tmp/aa-imsg/a\n",
                      "/tmp/aa-imsg/a/", "/tmp/aa-imsg/../etc"] {
            assert_eq!(imessage_session_dir(value), None, "{value:?}");
        }
    }

    #[test]
    fn read_allowances_follow_the_scope_guard_on_read() {
        let temp = tempfile::tempdir().unwrap();
        let discord = ReadAllowance { directory: PathBuf::from("/tmp"), name_pattern: DISCORD_ATTACHMENT_NAME };
        let query = crate::reasoner::ask_opts(temp.path().into(), temp.path().into());
        assert!(runs_scope_guard_on_read(&query));
        let mut opts = query.clone();
        opts.env.retain(|(key, _)| key != IMESSAGE_SESSION_DIR_ENV);
        assert_eq!(read_allowances(&opts), vec![discord.clone()]);
        opts.env.push((IMESSAGE_SESSION_DIR_ENV.into(), "/tmp/aa-imsg/1-1".into()));
        opts.env.push((IMESSAGE_SESSION_DIR_ENV.into(), "/tmp/aa-imsg/2-2".into()));
        assert_eq!(read_allowances(&opts), vec![discord.clone(),
            ReadAllowance { directory: PathBuf::from("/tmp/aa-imsg/2-2"), name_pattern: PORTABLE_NAME }]);
        opts.env.push((IMESSAGE_SESSION_DIR_ENV.into(), "/tmp/aa-imsg/..".into()));
        assert_eq!(read_allowances(&opts), vec![discord.clone()], "an invalid last value grants no session dir");

        let mut no_read = opts.clone();
        no_read.allowed_tools.retain(|tool| tool != "Read");
        assert!(read_allowances(&no_read).is_empty());
        let hooks = |matcher: &str, command: &str| Some(serde_json::json!({"hooks": {"PreToolUse": [{
            "matcher": matcher, "hooks": [{"type": "command", "command": command}]}]}}).to_string());
        for (matcher, command) in [("Write|Edit", "/repo/scripts/aa-wiki-scope-guard.sh"),
                                   ("Read", "/repo/scripts/other-guard.sh"),
                                   ("Read", "/repo/scripts/aa-wiki-scope-guard.sh.bak")] {
            let mut other = opts.clone();
            other.settings_json = hooks(matcher, command);
            assert!(read_allowances(&other).is_empty(), "{matcher} {command}");
        }
        let mut guarded = opts.clone();
        guarded.settings_json = hooks("Read|Grep", "/repo/scripts/aa-wiki-scope-guard.sh");
        assert_eq!(read_allowances(&guarded), vec![discord]);
        let mut unguarded = opts;
        unguarded.settings_json = None;
        assert!(read_allowances(&unguarded).is_empty());
        for opts in [crate::reasoner::triage_opts(Some(temp.path().into())), crate::reasoner::resume_opts(temp.path().into())] {
            assert!(read_allowances(&opts).is_empty());
        }
    }
}
