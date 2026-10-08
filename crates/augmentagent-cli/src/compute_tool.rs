//! Owner-DM-bound MCP facade. One daemon-owned compute service and deadline
//! survive every orchestration/repair call made during the originating turn.
use anyhow::{Context, Result};
use augmentagent_approval_discord::AuditCtx;
use augmentagent_channel_core::{
    code_mode::{
        compute::{ComputeDispatcher, ComputePolicy, ComputeService, ServiceConfig},
        manifest::manifest_compute,
        runner::run_program_with_options,
    },
    reasoner::ReasonerOpts,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

const FRAME_LIMIT: u64 = 2 * 1024 * 1024;

fn authorized(ctx: &AuditCtx, policy: &ComputePolicy) -> bool {
    policy.enabled && ctx.owner_authorized && ctx.guild_id.is_none() && ctx.channel_id.is_some()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    version: u32,
    grant: String,
    program: String,
}

pub struct ComputeTurn {
    service: Arc<ComputeService>,
    socket: PathBuf,
    grant: String,
    worker: tokio::task::JoinHandle<()>,
    _control: tempfile::TempDir,
    artifacts: Option<tempfile::TempDir>,
    _retention_root: Option<augmentagent_channel_core::code_mode::compute::artifacts::Directory>,
}

fn make_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

impl ComputeTurn {
    async fn start(config: ServiceConfig) -> Result<Self> {
        let control = tempfile::Builder::new().prefix("jc-").tempdir()?;
        make_private(control.path())?;
        let socket = control.path().join("s");
        let listener = UnixListener::bind(&socket)?;
        let service = ComputeService::start(config).await?;
        let grant = uuid::Uuid::new_v4().simple().to_string();
        let task_service = service.clone();
        let task_grant = grant.clone();
        let worker = tokio::spawn(async move {
            loop {
                let Ok(options) = task_service.run_options() else {
                    task_service.cancel();
                    break;
                };
                let accepted = tokio::time::timeout(options.timeout, listener.accept()).await;
                let Ok(Ok((stream, _))) = accepted else {
                    task_service.cancel();
                    break;
                };
                // Socket permissions and the per-turn grant are independent.
                if !stream
                    .peer_cred()
                    .is_ok_and(|cred| cred.uid() == unsafe { libc::geteuid() })
                {
                    continue;
                }
                let _ = handle_connection(stream, &task_grant, &task_service).await;
            }
        });
        Ok(Self {
            service,
            socket,
            grant,
            worker,
            _control: control,
            artifacts: None,
            _retention_root: None,
        })
    }

    fn configure(&self, opts: &mut ReasonerOpts, bin: &Path) -> Result<()> {
        let mut settings: Value = opts
            .settings_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_else(|| json!({}));
        settings["mcpServers"]["compute"] = json!({"command":bin,"args":["compute-tool"], "timeout":3605000,
            "env":{"AUGMENTAGENT_COMPUTE_TOOL_SOCKET":self.socket,"AUGMENTAGENT_COMPUTE_TOOL_GRANT":self.grant}});
        opts.settings_json = Some(settings.to_string());
        opts.allowed_tools.push("mcp__compute__run".into());
        opts.system_prompt.push_str(&format!(
            "\nUse mcp__compute__run with {{program: TypeScript}} for dependency-enabled Python computation. Define and invoke async main(). The orchestration sandbox exposes only this tool surface:\n{}\ndeclare const computeInputs: Readonly<Record<string,string>>;\nAvailable input aliases: {}. Input values are opaque artifact IDs. Python reads explicitly selected files under /inputs and writes declared files under /outputs. Inspect every compute result's ok field. If a program fails, you may repair it by calling this same tool again; the original task deadline and dependency environments remain in effect. Return the computed result from main() and use it in your answer. Never claim a successful computation when the returned ok is false.\n",
            manifest_compute().to_dts(), serde_json::to_string(&self.service.inputs().keys().collect::<Vec<_>>())?));
        Ok(())
    }

    pub async fn finish(mut self) -> Result<Value> {
        self.worker.abort();
        let _ = (&mut self.worker).await;
        let receipt = self.service.finish().await?;
        // Keep only tasks with generated artifacts for the retention worker.
        // Inputs and all files remain in an owner-private directory.
        let generated = receipt["records"].as_array().is_some_and(|records| {
            records.iter().any(|record| {
                record["artifacts"]
                    .as_array()
                    .is_some_and(|files| !files.is_empty())
            })
        });
        if generated {
            if let Some(directory) = self.artifacts.take() {
                let root =
                    augmentagent_channel_core::code_mode::compute::artifacts::Directory::open(
                        self.service.artifact_root(),
                        false,
                    )?;
                let kept: std::collections::BTreeSet<&str> = receipt["records"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|record| record["artifacts"].as_array().into_iter().flatten())
                    .filter_map(|artifact| artifact["id"].as_str())
                    .collect();
                for entry in std::fs::read_dir(root.path())? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if name.len() == 32
                        && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                        && !kept.contains(name)
                    {
                        // unlink removes the selected entry itself; the pinned
                        // directory prevents a parent swap redirecting cleanup.
                        std::fs::remove_file(root.path().join(name))?;
                    }
                }
                let expires = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs()
                    + 86400;
                root.write_report(
                    "retention.json",
                    &json!({"schemaVersion":1,"expiresAt":expires,"receipt":receipt}),
                )?;
                let _ = directory.keep();
            }
        }
        Ok(receipt)
    }
}
impl Drop for ComputeTurn {
    fn drop(&mut self) {
        self.worker.abort();
        self.service.cancel();
    }
}

/// Called by the actual owner query route. No inferred identity or model
/// argument can grant access. Existing query tools and provider choices remain.
pub async fn attach(
    opts: &mut ReasonerOpts,
    ctx: &AuditCtx,
    bin: &Path,
) -> Result<Option<ComputeTurn>> {
    if !ctx.owner_authorized || ctx.guild_id.is_some() || ctx.channel_id.is_none() {
        return Ok(None);
    }
    let policy = ComputePolicy::from_env().map_err(anyhow::Error::msg)?;
    if !authorized(ctx, &policy) {
        return Ok(None);
    }
    let state = augmentagent_channel_core::state_dir::state_dir()
        .context("compute state directory unavailable")?;
    std::fs::create_dir_all(&state)?;
    let retained = state.join("compute-artifacts");
    let root =
        augmentagent_channel_core::code_mode::compute::artifacts::Directory::open(&retained, true)?;
    make_private(&retained)?;
    // The descriptor path pins the parent for creation; use the ordinary task
    // path in the helper, which independently checks every component.
    let temporary = tempfile::Builder::new()
        .prefix("task-")
        .tempdir_in(root.path())?;
    let task_path = retained.join(
        temporary
            .path()
            .file_name()
            .context("task directory missing")?,
    );
    make_private(&task_path)?;
    let config = ServiceConfig::from_env(
        task_path,
        augmentagent_approval_discord::compute_inputs::selected(),
    )?;
    let mut turn = ComputeTurn::start(config).await?;
    turn.artifacts = Some(temporary);
    turn._retention_root = Some(root);
    turn.configure(opts, bin)?;
    Ok(Some(turn))
}

async fn handle_connection(
    stream: UnixStream,
    grant: &str,
    service: &Arc<ComputeService>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        (&mut read)
            .take(FRAME_LIMIT + 1)
            .read_until(b'\n', &mut bytes),
    )
    .await??;
    anyhow::ensure!(
        bytes.len() <= FRAME_LIMIT as usize && bytes.last() == Some(&b'\n'),
        "invalid compute control frame"
    );
    let request = serde_json::from_slice::<ControlRequest>(&bytes);
    let reply = match request {
        Ok(request)
            if request.version == 1
                && request.grant == grant
                && request.program.len() <= 256 * 1024 =>
        {
            let dispatcher = ComputeDispatcher::new(service.clone());
            let options = service.run_options()?;
            let manifest = manifest_compute();
            let future =
                run_program_with_options(&request.program, &manifest, &dispatcher, &options);
            let mut extra = [0u8; 1];
            let outcome = tokio::select! {
                result = future => result,
                _ = read.read(&mut extra) => { service.cancel(); return Ok(()); }
            };
            match outcome {
                Ok(value) => {
                    json!({"version":1,"ok":!dispatcher.has_failed() && value.dispatch_failures == 0,"final":value.final_value,
                    "trace":value.trace})
                }
                Err(error) => {
                    json!({"version":1,"ok":false,"error":{"code":error.public_code(),"message":"Compute orchestration failed. Repair the program within the remaining task budget."}})
                }
            }
        }
        _ => {
            json!({"version":1,"ok":false,"error":{"code":"permission_denied","message":"Invalid or expired compute turn capability."}})
        }
    };
    let mut bytes = serde_json::to_vec(&reply)?;
    anyhow::ensure!(
        bytes.len() < FRAME_LIMIT as usize,
        "compute reply exceeds limit"
    );
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), write.write_all(&bytes)).await??;
    Ok(())
}

fn call_control(arguments: &Value) -> Result<Value> {
    use std::io::{BufRead, Read, Write};
    let object = arguments
        .as_object()
        .context("run requires a program object")?;
    anyhow::ensure!(object.len() == 1, "run accepts only program");
    let program = object
        .get("program")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= 256 * 1024)
        .context("program is missing or too large")?;
    let socket = std::env::var_os("AUGMENTAGENT_COMPUTE_TOOL_SOCKET")
        .context("compute socket unavailable")?;
    let grant =
        std::env::var("AUGMENTAGENT_COMPUTE_TOOL_GRANT").context("compute grant unavailable")?;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(3605)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    writeln!(
        stream,
        "{}",
        json!({"version":1,"grant":grant,"program":program})
    )?;
    let mut bytes = Vec::new();
    std::io::BufReader::new(stream)
        .take(FRAME_LIMIT + 1)
        .read_until(b'\n', &mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= FRAME_LIMIT as usize && bytes.last() == Some(&b'\n'),
        "compute reply missing or oversized"
    );
    let reply: Value = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(reply["version"] == 1, "invalid compute reply");
    Ok(reply)
}

fn dispatch(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let result = match request["method"].as_str().unwrap_or("") {
        "initialize" => {
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"jarvis-compute","version":"1.0"}})
        }
        "ping" => json!({}),
        "tools/list" => {
            json!({"tools":[{"name":"run","description":"Run TypeScript orchestration using tools.compute.run in this authorized owner task. Repair calls share the original deadline and prepared environments.",
            "inputSchema":{"type":"object","properties":{"program":{"type":"string","maxLength":262144}},"required":["program"],"additionalProperties":false}}]})
        }
        "tools/call" => {
            let outcome = if request["params"]["name"] == "run" {
                call_control(&request["params"]["arguments"])
            } else {
                Err(anyhow::anyhow!("unknown compute tool"))
            };
            match outcome {
                Ok(value) => {
                    json!({"isError":value["ok"] != true,"content":[{"type":"text","text":value.to_string()}]})
                }
                Err(_) => {
                    json!({"isError":true,"content":[{"type":"text","text":"Compute task capability is unavailable or the request is invalid."}]})
                }
            }
        }
        _ => {
            return json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
        }
    };
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

pub fn serve() -> Result<()> {
    use std::io::{BufRead, Read, Write};
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = std::io::stdout().lock();
    loop {
        let mut bytes = Vec::new();
        (&mut input)
            .take(FRAME_LIMIT + 1)
            .read_until(b'\n', &mut bytes)?;
        if bytes.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            bytes.len() <= FRAME_LIMIT as usize && bytes.last() == Some(&b'\n'),
            "oversized MCP request"
        );
        let request: Value = serde_json::from_slice(&bytes)?;
        if request.get("id").is_none() {
            continue;
        }
        writeln!(output, "{}", dispatch(&request))?;
        output.flush()?;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    async fn control(turn: &ComputeTurn, grant: &str, program: &str) -> Value {
        let mut stream = UnixStream::connect(&turn.socket).await.unwrap();
        let message = json!({"version":1,"grant":grant,"program":program}).to_string() + "\n";
        stream.write_all(message.as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            BufReader::new(stream).read_until(b'\n', &mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn test_config(root: &Path) -> ServiceConfig {
        let artifact_root = root.join("artifacts");
        std::fs::create_dir(&artifact_root).unwrap();
        make_private(&artifact_root).unwrap();
        ServiceConfig {
            policy: ComputePolicy {
                enabled: false,
                call_timeout: Duration::from_secs(5),
                task_timeout: Duration::from_secs(10),
            },
            runtime: root.join("missing-runtime"),
            scratch_root: root.join("scratch"),
            pip_runtime: None,
            artifact_root,
            input_files: BTreeMap::new(),
            scratch_limits: None,
        }
    }

    #[tokio::test]
    async fn capability_denial_and_program_repair_share_original_task_budget() {
        let root = tempfile::tempdir().unwrap();
        let turn = ComputeTurn::start(test_config(root.path())).await.unwrap();
        let before = turn.service.run_options().unwrap().timeout;
        let denied = control(&turn, "wrong-turn", "42").await;
        assert_eq!(denied["error"]["code"], "permission_denied");
        let failed = control(&turn, &turn.grant, "throw new Error('SOURCE_CANARY')").await;
        assert_eq!(failed["ok"], false);
        assert!(!failed.to_string().contains("SOURCE_CANARY"));
        let repaired = control(
            &turn,
            &turn.grant,
            "async function main(){await new Promise(r=>setTimeout(r,100));return 60;} main();",
        )
        .await;
        assert_eq!(repaired["ok"], true);
        assert_eq!(repaired["final"], 60);
        assert!(turn.service.run_options().unwrap().timeout < before);
        assert_eq!(turn.finish().await.unwrap()["cleanupVerified"], true);
    }

    #[tokio::test]
    async fn configured_facade_exposes_no_runtime_paths_and_preserves_other_tools() {
        let root = tempfile::tempdir().unwrap();
        let turn = ComputeTurn::start(test_config(root.path())).await.unwrap();
        let mut opts = augmentagent_channel_core::reasoner::resume_opts(root.path().to_owned());
        let previous = opts.allowed_tools.clone();
        turn.configure(&mut opts, Path::new("/fixture/augmentagent"))
            .unwrap();
        for tool in previous {
            assert!(opts.allowed_tools.contains(&tool));
        }
        assert!(opts.allowed_tools.contains(&"mcp__compute__run".into()));
        assert!(opts.system_prompt.contains("computeInputs"));
        assert!(opts.system_prompt.contains("dependencyLock"));
        let settings: Value = serde_json::from_str(opts.settings_json.as_ref().unwrap()).unwrap();
        assert_eq!(settings["mcpServers"]["compute"]["timeout"], 3605000);
        let schema = dispatch(&json!({"id":1,"method":"tools/list"}));
        assert_eq!(
            schema["result"]["tools"][0]["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        turn.finish().await.unwrap();
    }

    #[test]
    fn authorization_requires_enabled_explicit_owner_and_discord_dm() {
        let policy = ComputePolicy::from_lookup(|key| {
            (key == "AUGMENTAGENT_COMPUTE_ENABLED").then(|| "true".into())
        })
        .unwrap();
        let mut ctx = AuditCtx::empty();
        assert!(!authorized(&ctx, &policy));
        ctx.owner_authorized = true;
        assert!(!authorized(&ctx, &policy));
        ctx.channel_id = Some(serenity::model::id::ChannelId::new(123));
        assert!(authorized(&ctx, &policy));
        ctx.guild_id = Some(456);
        assert!(!authorized(&ctx, &policy));
        ctx.guild_id = None;
        ctx.owner_authorized = false;
        assert!(!authorized(&ctx, &policy));
        ctx.owner_authorized = true;
        let disabled = ComputePolicy::from_lookup(|_| None).unwrap();
        assert!(!authorized(&ctx, &disabled));
    }
    #[tokio::test]
    #[ignore = "requires provisioned VM and public PyPI; scripted reasoner, no provider credentials or sends"]
    async fn real_owner_query_repairs_computation_in_the_same_environment() {
        use augmentagent_approval_discord::QueryHandler;
        use augmentagent_channel_core::{providers::ProviderKind, FallbackReasoner, Reasoner};
        if std::env::var_os("JARVIS_COMPUTE_OWNER_FIXTURE_ROOT").is_none() {
            let root = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["compute_tool::tests::real_owner_query_repairs_computation_in_the_same_environment", "--exact", "--ignored", "--test-threads=1", "--nocapture"])
                .env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", root.path()).env("XDG_STATE_HOME", root.path().join("state"))
                .env("JARVIS_COMPUTE_OWNER_FIXTURE_ROOT", root.path())
                .env("AUGMENTAGENT_COMPUTE_ENABLED", "true")
                .env("AUGMENTAGENT_BUILD_VM_CONFIG", std::env::var_os("JARVIS_TEST_VM_CONFIG").expect("VM config required"))
                .env("AUGMENTAGENT_BUILD_SCRATCH_DIR", std::env::var_os("JARVIS_TEST_COMPUTE_SCRATCH").expect("scratch required"))
                .env("AUGMENTAGENT_COMPUTE_PIP_RUNTIME", std::env::var_os("JARVIS_TEST_COMPUTE_PIP").expect("pinned installer required"))
                .output().unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        struct Scripted;
        #[async_trait::async_trait]
        impl Reasoner for Scripted {
            async fn call(&self, opts: &ReasonerOpts, _: &str) -> Result<String> {
                assert!(opts.allowed_tools.contains(&"mcp__compute__run".into()));
                assert!(opts.system_prompt.contains("dependencyLock"));
                let settings: Value = serde_json::from_str(opts.settings_json.as_ref().unwrap())?;
                let server = &settings["mcpServers"]["compute"];
                let socket = server["env"]["AUGMENTAGENT_COMPUTE_TOOL_SOCKET"]
                    .as_str()
                    .unwrap();
                let grant = server["env"]["AUGMENTAGENT_COMPUTE_TOOL_GRANT"]
                    .as_str()
                    .unwrap();
                async fn attempt(socket: &str, grant: &str, code: &str) -> Result<Value> {
                    let program = format!("async function main(){{ return await tools.compute.run({{runtime:'python',dependencies:['openpyxl==3.1.5'],inputs:[{{artifactId:computeInputs.sheet,name:'numbers.xlsx'}}],outputs:['summary.json'],code:{}}}); }} main();", serde_json::to_string(code)?);
                    let mut stream = UnixStream::connect(socket).await?;
                    stream
                        .write_all(
                            (json!({"version":1,"grant":grant,"program":program}).to_string()
                                + "\n")
                                .as_bytes(),
                        )
                        .await?;
                    let mut bytes = Vec::new();
                    tokio::time::timeout(
                        Duration::from_secs(120),
                        BufReader::new(stream).read_until(b'\n', &mut bytes),
                    )
                    .await??;
                    Ok(serde_json::from_slice(&bytes)?)
                }
                let failed = attempt(
                    socket,
                    grant,
                    "import openpyxl\nraise ValueError('repair fixture')",
                )
                .await?;
                assert_eq!(failed["ok"], false);
                let fixed = attempt(socket, grant, "import json\nfrom openpyxl import load_workbook\nbook=load_workbook('/inputs/numbers.xlsx',read_only=True,data_only=True)\nvalues=[row[0] for row in book.active.iter_rows(values_only=True)]\nsummary={'count':len(values),'total':sum(values)}\nprint(json.dumps(summary))\nopen('/outputs/summary.json','w').write(json.dumps(summary))").await?;
                assert_eq!(fixed["ok"], true, "{fixed}");
                assert_eq!(fixed["final"]["runner"], "vm");
                assert_eq!(fixed["final"]["environmentReused"], true);
                assert_eq!(
                    fixed["final"]["dependencyLock"],
                    failed["final"]["dependencyLock"]
                );
                let summary: Value =
                    serde_json::from_str(fixed["final"]["stdout"].as_str().unwrap())?;
                assert_eq!(summary, json!({"count":3,"total":60}));
                Ok(format!(
                    "The spreadsheet contains {} rows totaling {}.",
                    summary["count"], summary["total"]
                ))
            }
        }
        let root = PathBuf::from(std::env::var_os("JARVIS_COMPUTE_OWNER_FIXTURE_ROOT").unwrap());
        let wiki = root.join("wiki");
        std::fs::create_dir(&wiki).unwrap();
        let sheet = root.join("numbers.xlsx");
        std::fs::write(
            &sheet,
            include_bytes!("../../../scripts/tests/fixtures/code-mode-compute/numbers.xlsx"),
        )
        .unwrap();
        let reasoner = Arc::new(FallbackReasoner::for_tests(
            vec![(
                ProviderKind::Claude,
                Arc::new(Scripted) as Arc<dyn Reasoner>,
            )],
            augmentagent_channel_core::cooldown::CooldownLatch::at(root.join("cooldowns.json")),
        ));
        let handler = crate::WikiQuerier {
            reasoner,
            wiki_root: wiki,
            repo_root: root.clone(),
            conversation_store: None,
            conversation_scheduler: Arc::new(
                augmentagent_approval_discord::conversation::ConversationScheduler::new(),
            ),
            voice_enabled: false,
            voice_tools: std::sync::OnceLock::new(),
            final_spoken_turns: dashmap::DashMap::new(),
        };
        let mut ctx = AuditCtx::empty();
        ctx.owner_authorized = true;
        ctx.channel_id = Some(serenity::model::id::ChannelId::new(123));
        ctx.session_id = "compute-owner-fixture".into();
        let inputs = BTreeMap::from([("sheet".into(), sheet)]);
        let answer = augmentagent_approval_discord::compute_inputs::SELECTED
            .scope(
                inputs,
                handler.answer_turn(&ctx, "", "Please total the spreadsheet."),
            )
            .await
            .unwrap();
        assert_eq!(answer, "The spreadsheet contains 3 rows totaling 60.");
        let tasks: Vec<_> = std::fs::read_dir(root.join("state/augmentagent/compute-artifacts"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            std::fs::read_dir(&tasks[0]).unwrap().count(),
            2,
            "retain only generated output and its receipt, not raw input snapshots"
        );
        let retained: Value =
            serde_json::from_slice(&std::fs::read(tasks[0].join("retention.json")).unwrap())
                .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(retained["expiresAt"].as_u64().unwrap() >= now + 86390);
        let records = retained["receipt"]["records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["taskId"], records[1]["taskId"]);
        let artifact = &records[1]["artifacts"][0];
        let exported: Value = serde_json::from_slice(
            &std::fs::read(tasks[0].join(artifact["id"].as_str().unwrap())).unwrap(),
        )
        .unwrap();
        assert_eq!(exported, json!({"count":3,"total":60}));
    }
}
