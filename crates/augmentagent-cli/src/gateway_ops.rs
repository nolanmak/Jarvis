//! Owner gateway controls; normal startup and timers never enable routing.
use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Subcommand)]
pub enum GatewayOp {
    Status {
        #[arg(long)]
        json: bool,
    },
    Off {
        #[arg(long)]
        json: bool,
    },
    On {
        #[arg(long, default_value="auto", value_parser=["auto","claude","codex"])]
        mode: String,
    },
    Update {
        #[arg(long)]
        to: Option<String>,
    },
    Rollback,
}
impl GatewayOp {
    fn request(&self) -> Value {
        match self {
            Self::Status { .. } => json!({"operation":"status"}),
            Self::Off { .. } => json!({"operation":"off"}),
            Self::On { mode } => json!({"operation":"on","mode":mode}),
            Self::Update { to } => json!({"operation":"update","to":to}),
            Self::Rollback => json!({"operation":"rollback"}),
        }
    }
}

const DRIVER: &str = r#"
import json,os,sys,fcntl
from pathlib import Path
payload=json.load(sys.stdin)
request=payload['request']
namespace={'__name__':'gateway_payload','__file__':str(Path(payload['repository'])/'scripts/lib/omniroute_install.py')}
try:
 exec(compile(payload['installer'],'<packaged installer>','exec'),namespace)
 exec(compile(payload['operations'],'<packaged gateway operations>','exec'),namespace)
 config=Path(os.environ.get('XDG_CONFIG_HOME',Path.home()/'.config'))/'augmentagent'
 data=Path(os.environ.get('XDG_DATA_HOME',Path.home()/'.local/share'))/'augmentagent/omniroute'
 pinfile=Path(payload['repository'])/'scripts/omniroute.pin'
 pin=json.loads(pinfile.read_text() if pinfile.is_file() else payload['compiled_pin'])
 installer=namespace['Installer'](config,data,pin)
 if not installer.router.exists():
  result={'ok':request['operation']=='off','mode':'direct','reason':'gateway not configured'}
 else:
  descriptor=os.open(config/'omniroute-install.lock',os.O_RDONLY|os.O_NOFOLLOW)
  try:
   fcntl.flock(descriptor,(fcntl.LOCK_SH if request['operation']=='status' else fcntl.LOCK_EX)|fcntl.LOCK_NB)
   ops=namespace['Operations'](installer,payload['selftest_binary'])
   ops.pin_available=pinfile.is_file()
   result=ops.execute(request['operation'],mode=request.get('mode','auto'),to=request.get('to'))
  finally:os.close(descriptor)
except Exception:
 result={'ok':False,'error':'gateway operation unavailable; inspect private configuration, keyring and maintenance lock'}
 if request['operation'] in ('update','rollback'):
  result['alert']='Gateway maintenance failed. Verify direct routing before enabling the gateway.'
print(json.dumps(result))
sys.exit(0 if result.get('ok') else 1)
"#;

pub fn repository() -> PathBuf {
    std::env::var_os("AUGMENTAGENT_REPO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

async fn execute(repository: &Path, operation: &GatewayOp) -> Result<Value> {
    // Validate URL/host policy through the real router parser before any request.
    // Missing config remains an ordinary optional/off state in the helper.
    if !matches!(operation, GatewayOp::Off { .. }) {
        augmentagent_channel_core::model_router::load()?;
    }
    let payload = json!({"repository":repository,"selftest_binary":std::env::current_exe()?,
        "request":operation.request(),
        "installer":include_str!("../../../scripts/lib/omniroute_install.py"),
        "operations":include_str!("../../../scripts/lib/omniroute_ops.py"),
        "compiled_pin":include_str!("../../../scripts/omniroute.pin")});
    let mut child = tokio::process::Command::new("python3")
        .args(["-I", "-B", "-c", DRIVER])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .context("gateway helper stdin unavailable")?;
    input.write_all(&serde_json::to_vec(&payload)?).await?;
    drop(input);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(1800),
        child.wait_with_output(),
    )
    .await??;
    anyhow::ensure!(
        output.stdout.len() <= 1024 * 1024,
        "gateway result exceeded its bound"
    );
    let value: Value =
        serde_json::from_slice(&output.stdout).context("gateway result unavailable")?;
    anyhow::ensure!(value["ok"].is_boolean(), "gateway result invalid");
    Ok(value)
}

pub async fn run(repository: &Path, operation: &GatewayOp) -> Result<i32> {
    let report = match execute(repository, operation).await {
        Ok(report) => report,
        Err(_) => {
            let mut report = json!({"ok":false,"error":"gateway operation unavailable; inspect configuration and prerequisites"});
            if matches!(operation, GatewayOp::Update { .. } | GatewayOp::Rollback) {
                report["alert"]=json!("Gateway maintenance failed. Verify direct routing before enabling the gateway.");
            }
            report
        }
    };
    if let Some(alert) = report["alert"].as_str() {
        let _ = crate::notify::notify_owner(crate::notify::Notice::new(
            "gateway_ops",
            alert.to_owned(),
        ))
        .await;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(i32::from(report["ok"] != true))
}

/// Existing Discord owner/model interception calls this before history/inference.
/// Every gateway-prefixed command is consumed; enabling stays CLI-only.
pub fn discord_operation(text: &str) -> Option<Result<GatewayOp, &'static str>> {
    let mut words = text.split_whitespace();
    if !matches!(words.next(), Some("/gateway" | "gateway")) {
        return None;
    }
    Some(match words.collect::<Vec<_>>().as_slice(){
        []|["status"]=>Ok(GatewayOp::Status{json:false}),
        ["off"]=>Ok(GatewayOp::Off{json:false}),
        _=>Err("Discord gateway controls are status/off only. Use the owner CLI for on, update or rollback."),
    })
}

pub async fn discord(repository: &Path, operation: Result<GatewayOp, &'static str>) -> String {
    match operation {
        Err(reason) => reason.into(),
        Ok(operation) => match execute(repository, &operation).await {
            Ok(value) => format!(
                "Gateway: {}",
                serde_json::to_string(&value).unwrap_or_default()
            ),
            Err(_) => "Gateway unavailable; inspect the local configuration.".into(),
        },
    }
}

pub async fn routed_selftest<F>(profile: Option<&str>, future: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    use augmentagent_channel_core::{model_router, providers::ProviderKind};
    let kind = ProviderKind::parse(profile.unwrap_or("claude"))
        .context("unsupported routed probe provider")?;
    anyhow::ensure!(
        matches!(kind, ProviderKind::Claude | ProviderKind::Codex),
        "routed probe requires Claude or Codex"
    );
    let config = model_router::load()?.context("routed probe requires a gateway configuration")?;
    anyhow::ensure!(
        config.enabled()
            && config.allows(kind)
            && config
                .model(kind, augmentagent_channel_core::ModelTier::Quality)
                .is_some(),
        "routed probe requires an enabled matching gateway route"
    );
    augmentagent_channel_core::gateway_probe::scope(future).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discord_consumes_gateway_controls_and_keeps_enablement_cli_only() {
        assert!(matches!(
            discord_operation("/gateway off"),
            Some(Ok(GatewayOp::Off { .. }))
        ));
        assert!(matches!(
            discord_operation("gateway status"),
            Some(Ok(GatewayOp::Status { .. }))
        ));
        for command in [
            "/gateway on",
            "gateway update",
            "/gateway rollback",
            "/gateway off extra",
        ] {
            assert!(
                matches!(discord_operation(command), Some(Err(_))),
                "{command}"
            );
        }
        assert!(discord_operation("please turn on gateway").is_none());
    }
}
