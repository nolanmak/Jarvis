//! The tool preset for a turn in a yolo thread (#2216, spec #2243 5.5).
//!
//! Differences from `ask_opts`, all deliberate:
//! - the working directory is a per-thread scratch workspace, and it is the
//!   only place `Write` and `Edit` reach (the bridge's write root is the
//!   working directory; the wiki is a read-only extra directory);
//! - integration commands are a fixed read-only list, with nothing that
//!   sends, posts, schedules, signs or uploads;
//! - the environment is an allowlist, not a copy of the ask environment;
//! - no wiki scope-guard hook (there are no wiki writes to guard), only the
//!   read-only memory server.
use crate::reasoner::ReasonerOpts;
use crate::tool_audit::{default_audit_log_path, AuditLogger};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The one list of integration commands yolo may run, shared with the bridge
/// test that proves each is read-only.
const READONLY_COMMANDS: &str =
    include_str!("../../../scripts/tests/fixtures/yolo-readonly-commands.json");

/// Read-only memory tools, the same seven `ask_opts` grants.
pub const MEMORY_TOOLS: [&str; 7] = [
    "mcp__memory__search_conversation_history",
    "mcp__memory__read_conversation_thread",
    "mcp__memory__search_messages",
    "mcp__memory__conversation_stats",
    "mcp__memory__search_notes",
    "mcp__memory__memory_search",
    "mcp__memory__memory_recent",
];

/// Environment names a yolo turn may carry. Not here on purpose: the Discord
/// bot token and channel id, every cloud credential, and the iMessage and
/// Apple Notes storage settings that `ask_opts` forwards.
pub const ENV_ALLOWLIST: [&str; 7] = [
    "AUGMENTAGENT_DB",
    "WIKI_ROOT",
    "AUGMENTAGENT_REPO_ROOT",
    "XDG_CONFIG_HOME",
    "AUGMENTAGENT_TRANSCRIPTS_DIR",
    "PATH",
    // The granted `gmail` commands read it.
    "COMPOSIO_API_KEY",
];

fn readonly_commands() -> Vec<(String, Vec<String>)> {
    let parsed: serde_json::Value =
        serde_json::from_str(READONLY_COMMANDS).expect("fixture is valid JSON");
    ["augmentagent", "aa-gh"]
        .into_iter()
        .map(|program| {
            let commands = parsed[program]
                .as_array()
                .expect("fixture lists commands per program")
                .iter()
                .map(|command| command.as_str().expect("command is a string").to_string())
                .collect();
            (program.to_string(), commands)
        })
        .collect()
}

/// `Bash(...)` permission entries for the read-only commands, in the bare
/// and absolute-binary forms `ask_opts` uses.
fn command_tools(repo_root: &Path) -> Vec<String> {
    let binary = repo_root.join("target/release/augmentagent");
    let aa_gh = repo_root.join("scripts/aa-gh");
    let mut tools = Vec::new();
    for (program, commands) in readonly_commands() {
        let absolute = if program == "augmentagent" {
            &binary
        } else {
            &aa_gh
        };
        for command in commands {
            tools.push(format!("Bash({program} {command})"));
            tools.push(format!("Bash({} {command})", absolute.display()));
        }
    }
    tools
}

/// Where one yolo thread keeps its files: under the state directory, which
/// no other preset can read. `None` when there is no state directory.
pub fn workspace_dir(thread_id: u64) -> Option<PathBuf> {
    Some(
        crate::state_dir::state_dir()?
            .join("yolo-workspaces")
            .join(thread_id.to_string()),
    )
}

/// Create the workspace, owner-only.
pub fn ensure_workspace(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Options for one yolo turn, before the per-turn web tools are attached.
/// `workspace` must already exist (see [`ensure_workspace`]).
pub fn yolo_opts(wiki_root: PathBuf, repo_root: PathBuf, workspace: PathBuf) -> ReasonerOpts {
    let wiki_root = std::fs::canonicalize(&wiki_root).unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|dir| dir.join(&wiki_root))
            .unwrap_or(wiki_root)
    });
    let workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace);
    let db_path = crate::reasoner::resolve_db_path(&repo_root);
    let binary = repo_root.join("target/release/augmentagent");

    // The memory server only. `ask_opts` pairs it with the wiki scope-guard
    // hook; here file confinement is the bridge's read and write roots.
    let settings_json = serde_json::json!({
        "mcpServers": {
            "memory": {
                "command": repo_root.join("target/release/augmentagent-mcp-memory").to_string_lossy(),
                "args": [],
                "env": { "AUGMENTAGENT_DB": db_path.to_string_lossy() }
            }
        }
    })
    .to_string();

    let mut env: Vec<(String, String)> = vec![
        (
            "AUGMENTAGENT_DB".into(),
            db_path.to_string_lossy().into_owned(),
        ),
        ("WIKI_ROOT".into(), wiki_root.to_string_lossy().into_owned()),
        (
            "AUGMENTAGENT_REPO_ROOT".into(),
            repo_root.to_string_lossy().into_owned(),
        ),
    ];
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        env.push((
            "XDG_CONFIG_HOME".into(),
            config_home.to_string_lossy().into_owned(),
        ));
    }
    let mut add_dirs = vec![wiki_root.clone()];
    if let Some(transcripts) = std::env::var_os("AUGMENTAGENT_TRANSCRIPTS_DIR").map(PathBuf::from) {
        if transcripts.is_dir() {
            env.push((
                "AUGMENTAGENT_TRANSCRIPTS_DIR".into(),
                transcripts.to_string_lossy().into_owned(),
            ));
            add_dirs.push(transcripts);
        }
    }
    // Bare `augmentagent` and `aa-gh` must resolve to this checkout's
    // binaries, as in `ask_opts`.
    if let Some(parent) = binary.parent() {
        let scripts = repo_root.join("scripts");
        let existing = std::env::var("PATH").unwrap_or_default();
        let joined = if existing.is_empty() {
            format!("{}:{}", parent.display(), scripts.display())
        } else {
            format!("{}:{}:{existing}", parent.display(), scripts.display())
        };
        env.push(("PATH".into(), joined));
    }
    if let Some(key) = crate::secret_loader::load_provider_key("COMPOSIO_API_KEY") {
        env.push(("COMPOSIO_API_KEY".into(), key));
    }

    let mut allowed_tools: Vec<String> = ["Read", "Grep", "Glob", "Write", "Edit"]
        .into_iter()
        .map(String::from)
        .collect();
    allowed_tools.extend(MEMORY_TOOLS.into_iter().map(String::from));
    allowed_tools.extend(command_tools(&repo_root));

    ReasonerOpts {
        system_prompt: include_str!("../../../schema/yolo.md").to_string(),
        // Pinned so no launch inherits an interactive default. The tier is
        // ignored for this profile: it has exactly one model.
        model: Some(crate::yolo::model_id()),
        allowed_tools,
        add_dirs,
        permission_mode: "acceptEdits".into(),
        // The working directory is the only write root.
        cwd: Some(workspace),
        env,
        settings_json: Some(settings_json),
        restrict_env: true,
        audit_logger: Some(Arc::new(AuditLogger::new(default_audit_log_path()))),
        audit_notifier: None,
        session_id: None,
        handoff_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{classify, CapabilityClass};

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        let repo = dir.path().join("repo");
        let workspace = dir.path().join("state/yolo-workspaces/1001");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        ensure_workspace(&workspace).unwrap();
        (dir, wiki, repo, workspace)
    }

    #[test]
    fn the_tool_list_is_exactly_files_memory_and_the_read_only_commands() {
        let (_dir, wiki, repo, workspace) = fixture();
        let opts = yolo_opts(wiki, repo.clone(), workspace);
        let mut expected: Vec<String> = ["Read", "Grep", "Glob", "Write", "Edit"]
            .into_iter()
            .map(String::from)
            .collect();
        expected.extend(MEMORY_TOOLS.into_iter().map(String::from));
        expected.extend(command_tools(&repo));
        assert_eq!(opts.allowed_tools, expected);
        // 18 augmentagent commands and 2 aa-gh commands, each in two forms.
        assert_eq!(command_tools(&repo).len(), 40);
        // Nothing that sends, posts, schedules, signs, uploads or runs loops,
        // no native web tools, and no memory write.
        for forbidden in [
            "send",
            "compose",
            " dm",
            "comment",
            "create",
            "rsvp",
            "reschedule",
            "sign",
            "upload",
            "loop",
            "allowlist",
            "context-attach",
            "WebSearch",
            "WebFetch",
            "memory_write",
            "NotebookEdit",
        ] {
            assert!(
                !opts
                    .allowed_tools
                    .iter()
                    .any(|tool| tool.contains(forbidden)),
                "{forbidden} must not be reachable from a yolo turn: {:?}",
                opts.allowed_tools
            );
        }
        assert_eq!(classify(&opts), CapabilityClass::FullAgentic);
    }

    #[test]
    fn writes_are_confined_to_the_workspace_and_the_wiki_is_read_only() {
        let (dir, wiki, repo, workspace) = fixture();
        let opts = yolo_opts(wiki.clone(), repo, workspace.clone());
        assert_eq!(opts.cwd, Some(workspace.canonicalize().unwrap()));
        assert_eq!(opts.add_dirs[0], wiki.canonicalize().unwrap());
        let launch_dir = dir.path().join("launch");
        std::fs::create_dir(&launch_dir).unwrap();
        let launch = crate::codex_tools::BridgeLaunch::prepare(&opts, &launch_dir).unwrap();
        let policy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(launch.policy_path).unwrap()).unwrap();
        // The bridge policy is what actually confines the tools.
        assert_eq!(
            policy["write_roots"],
            serde_json::json!([workspace.canonicalize().unwrap()])
        );
        let reads: Vec<_> = policy["read_roots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|root| root.as_str().unwrap().to_string())
            .collect();
        assert!(
            reads.contains(&wiki.canonicalize().unwrap().to_string_lossy().into_owned()),
            "{reads:?}"
        );
        assert!(
            reads.contains(
                &workspace
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ),
            "{reads:?}"
        );
        assert_eq!(
            policy["allowed_tools"],
            serde_json::json!(opts.allowed_tools),
            "the bridge accepts every tool"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&workspace).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn the_environment_is_an_allowlist_and_settings_hold_only_the_memory_server() {
        let (_dir, wiki, repo, workspace) = fixture();
        let opts = yolo_opts(wiki, repo, workspace);
        for (name, _) in &opts.env {
            assert!(
                ENV_ALLOWLIST.contains(&name.as_str()),
                "{name} is not on the yolo environment allowlist"
            );
        }
        for required in [
            "AUGMENTAGENT_DB",
            "WIKI_ROOT",
            "AUGMENTAGENT_REPO_ROOT",
            "PATH",
        ] {
            assert!(
                opts.env.iter().any(|(name, _)| name == required),
                "{required} missing"
            );
        }
        assert!(opts.restrict_env);
        let settings: serde_json::Value =
            serde_json::from_str(opts.settings_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            settings["mcpServers"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["memory"]
        );
        assert!(
            settings.get("hooks").is_none(),
            "no scope-guard hook: confinement is the bridge roots"
        );
        assert_eq!(
            crate::providers::tier_of(&opts),
            crate::providers::ModelTier::Quality
        );
        assert!(
            opts.system_prompt.contains("yolo thread")
                && !opts.system_prompt.contains("durable-facts")
        );
    }

    #[test]
    fn no_other_preset_can_read_a_yolo_workspace() {
        // Workspaces live under the state directory; the ask preset's roots
        // are the wiki and the transcripts clone, never the state directory.
        let (_dir, wiki, repo, workspace) = fixture();
        let ask = crate::reasoner::ask_opts(wiki, repo);
        for root in ask.add_dirs.iter().chain(ask.cwd.iter()) {
            assert!(
                !workspace.canonicalize().unwrap().starts_with(root),
                "{root:?} covers the workspace"
            );
        }
    }
}
