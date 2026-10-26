//! Required-workflow native transport runs in a no-NIC guest. Ordinary sessions
//! retain their existing transport; there is no required-workflow host fallback.
use std::{collections::BTreeMap, io::Write, os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use tokio::process::Command;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    version: u32,
    runtime_config: PathBuf,
    state_root: PathBuf,
    memory_mb: u32,
    providers: BTreeMap<String, Provider>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Provider {
    gateway: serde_json::Value,
    auth_file: Option<PathBuf>,
    auth_relative: String,
    #[serde(default)]
    environment: Vec<String>,
}

pub(crate) fn required(logger: Option<&std::sync::Arc<crate::tool_audit::AuditLogger>>) -> bool {
    logger.is_some_and(|logger| logger.run_resources_required())
}
fn private(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0 && metadata.nlink() == 1 && metadata.len() <= 2*1024*1024,
        "native VM policy is not an owner-private regular file");
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(2*1024*1024+1).read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn digest(value: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub(crate) struct Launch {
    pub command: Command,
    aliases: PathBuf,
    session: PathBuf,
    _directory: tempfile::TempDir,
}
impl Launch {
    pub fn prepare(provider: &str, opts: &crate::reasoner::ReasonerOpts, original: &Command,
        bridge: Option<&crate::codex_tools::BridgeLaunch>, native: Option<&crate::native_session::Launch>) -> Result<Self> {
        let policy_path = std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY")
            .context("required native VM policy unavailable")?;
        let policy: Policy = serde_json::from_slice(&private(Path::new(&policy_path))?)?;
        ensure!(policy.version == 1 && (512..=4096).contains(&policy.memory_mb), "invalid native VM policy");
        let profile = policy.providers.get(provider).context("required native provider profile unavailable")?;
        let root = policy.state_root.canonicalize()?;
        let metadata = root.metadata()?;
        ensure!(metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "native VM state root is not private");
        for worker in opts.cwd.iter().chain(opts.add_dirs.iter()) {
            let worker = worker.canonicalize()?;
            ensure!(!root.starts_with(&worker), "worker root overlaps native VM controller state");
        }
        let provider_root = root.join(provider);
        std::fs::create_dir_all(&provider_root)?;
        std::fs::set_permissions(&provider_root, std::fs::Permissions::from_mode(0o700))?;
        let aliases = provider_root.join("aliases");
        std::fs::create_dir_all(&aliases)?;
        std::fs::set_permissions(&aliases, std::fs::Permissions::from_mode(0o700))?;
        let resume = matches!(native, Some(crate::native_session::Launch::Resume { .. }));
        let session = match native {
            Some(crate::native_session::Launch::Resume { id }) => {
                let name: String = serde_json::from_slice(&private(&aliases.join(digest(id)))?)?;
                ensure!(name.starts_with("session-") && name.bytes().all(|c|c.is_ascii_alphanumeric() || c==b'-'), "invalid native runtime alias");
                let session = provider_root.join(name);
                private(&session.join("complete.json")).context("native runtime completion unavailable")?;
                session
            },
            _ => {
                let path = provider_root.join(format!("session-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir(&path)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
                path
            }
        };
        let directory = tempfile::Builder::new().prefix("launch-").tempdir_in(&provider_root)?;
        let helpers: &[(&str, &[u8])] = &[
            ("native-provider-vm.py", include_bytes!("../../../scripts/native-provider-vm.py")),
            ("native-provider-vm-guest.py", include_bytes!("../../../scripts/native-provider-vm-guest.py")),
            ("native-provider-mcp-relay.py", include_bytes!("../../../scripts/native-provider-mcp-relay.py")),
            ("native-provider-gateway.py", include_bytes!("../../../scripts/native-provider-gateway.py")),
            ("codex-build-vm.py", include_bytes!("../../../scripts/codex-build-vm.py")),
        ];
        for (name, contents) in helpers { write(&directory.path().join(name), contents)?; }
        let source = original.as_std();
        let program = PathBuf::from(source.get_program());
        let executable = if program.is_absolute() { program.canonicalize()? } else {
            let search = source.get_envs().find(|(key,_)|*key=="PATH").and_then(|(_,value)|value)
                .map(std::ffi::OsString::from).unwrap_or_else(|| "/usr/bin:/bin".into());
            std::env::split_paths(&search).map(|path|path.join(&program)).find(|path|path.is_file())
                .context("native executable unavailable")?.canonicalize()?
        };
        let mut assets = Vec::new();
        let mut arguments: Vec<String> = source.get_args().map(|arg|arg.to_str().context("non-UTF8 native argument").map(str::to_owned)).collect::<Result<_>>()?;
        let mut index = 0;
        while index < arguments.len() {
            if arguments[index].starts_with("mcp_servers.jarvis=") {
                arguments[index] = "mcp_servers.jarvis={command=\"/usr/bin/python3\",args=[\"-I\",\"/native/mcp-relay.py\"],required=true,startup_timeout_sec=120,tool_timeout_sec=900,default_tools_approval_mode=\"approve\"}".into();
            } else if let Some(raw) = arguments[index].strip_prefix("model_instructions_file=") {
                let path: String = serde_json::from_str(raw)?;
                let guest = format!("/native/asset-{}",assets.len());
                assets.push(serde_json::json!({"source":path,"name":guest.trim_start_matches("/native/")}));
                arguments[index] = format!("model_instructions_file={}",serde_json::to_string(&guest)?);
            } else if matches!(arguments[index].as_str(), "-C" | "--mcp-config" | "-i") && index+1 < arguments.len() {
                let option = arguments[index].clone();
                index += 1;
                if option == "-C" { arguments[index] = "/workspace".into(); }
                else if option == "--mcp-config" && bridge.is_some() {
                    arguments[index] = r#"{"mcpServers":{"jarvis":{"command":"/usr/bin/python3","args":["-I","/native/mcp-relay.py"]}}}"#.into();
                } else if option == "-i" {
                    let guest = format!("/native/asset-{}",assets.len());
                    assets.push(serde_json::json!({"source":arguments[index],"name":guest.trim_start_matches("/native/")}));
                    arguments[index] = guest;
                }
            }
            index += 1;
        }
        let mut environment = BTreeMap::new();
        for (name,value) in source.get_envs() {
            let Some(value) = value else { continue };
            let name = name.to_str().context("invalid native environment key")?;
            if profile.environment.iter().any(|allowed|allowed==name) || matches!(name,"LANG"|"TERM") {
                environment.insert(name.to_owned(), value.to_str().context("invalid native environment value")?.to_owned());
            }
        }
        let mcp_argv = bridge.map(|bridge| vec!["/usr/bin/python3".to_string(), "-I".into(),
            bridge.native_cwd.parent().expect("bridge launch directory").join("tool-bridge.py").to_string_lossy().into_owned(),
            bridge.policy_path.to_string_lossy().into_owned()]);
        let spec = serde_json::json!({"version":1,"runtime_config":policy.runtime_config,"memory_mb":policy.memory_mb,
            "session_directory":session,"resume":resume,"executable":executable,"argv":arguments,
            "environment":environment,"auth_file":profile.auth_file,"auth_relative":profile.auth_relative,
            "gateway":profile.gateway,"mcp_argv":mcp_argv,"assets":assets});
        let spec_path = directory.path().join("launch.json");
        write(&spec_path,&serde_json::to_vec(&spec)?)?;
        let mut command = Command::new("/usr/bin/python3");
        command.arg("-I").arg(directory.path().join("native-provider-vm.py")).arg(spec_path).env_clear()
            .env("PATH","/usr/bin:/bin");
        let result = Self { command, aliases, session, _directory:directory };
        if let Some(crate::native_session::Launch::Create { requested_id:Some(id) }) = native { result.observe_session(id)?; }
        Ok(result)
    }
    pub fn observe_session(&self, id: &str) -> Result<()> {
        ensure!(!id.is_empty() && id.len() <= 256, "invalid observed native session");
        let alias = self.aliases.join(digest(id));
        let name = self.session.file_name().context("missing session directory")?.to_string_lossy();
        let value = serde_json::to_vec(name.as_ref())?;
        if alias.exists() { ensure!(private(&alias)? == value, "native session alias collision"); }
        else { write(&alias,&value)?; }
        Ok(())
    }
}
