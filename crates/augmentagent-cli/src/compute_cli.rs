//! Provider-free compute acceptance entrypoint. This runs before dotenv,
//! database setup, provider credentials, and channel initialization.
use anyhow::{Context, Result};
use augmentagent_channel_core::code_mode::{
    compute::{
        artifacts::{self, Artifact, Directory},
        ComputeDispatcher, ComputeService, ServiceConfig,
    },
    manifest::manifest_compute,
    runner::run_program_with_options,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, io::Read, path::PathBuf};

#[derive(Debug, clap::Args)]
pub struct ComputeRunArgs {
    #[arg(long)]
    pub program: PathBuf,
    /// JSON object mapping input aliases to explicit local files.
    #[arg(long, default_value = "{}")]
    pub inputs: String,
    #[arg(long)]
    pub output_dir: PathBuf,
    #[arg(long)]
    pub report: PathBuf,
}

struct Prepared {
    // Drop task storage before its descriptor-pinned parent.
    _task: tempfile::TempDir,
    program: String,
    destination: Directory,
    report_parent: Directory,
    report_name: String,
    config: ServiceConfig,
}

fn prepare(args: &ComputeRunArgs) -> Result<Prepared> {
    let mut bytes = Vec::new();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let program_file = options.open(&args.program)?;
    anyhow::ensure!(
        program_file.metadata()?.is_file(),
        "program must be a regular file"
    );
    program_file.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 256 * 1024,
        "TypeScript program exceeds 256 KiB"
    );
    let program = String::from_utf8(bytes).context("program must be UTF-8")?;
    anyhow::ensure!(args.inputs.len() <= 64 * 1024, "input mapping is too large");
    let input_files: BTreeMap<String, PathBuf> =
        serde_json::from_str(&args.inputs).context("--inputs must be a JSON alias-to-file map")?;
    anyhow::ensure!(
        input_files.len() <= 32 && input_files.keys().all(|name| artifacts::filename(name)),
        "invalid input mapping"
    );
    let input_files = input_files
        .into_iter()
        .map(|(name, path)| Ok((name, std::path::absolute(path)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let report = std::path::absolute(&args.report)?;
    let report_name = report
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| artifacts::filename(name))
        .context("report needs a flat UTF-8 filename")?
        .to_owned();
    let report_parent = Directory::open(report.parent().context("report parent missing")?, false)?;
    anyhow::ensure!(
        std::fs::symlink_metadata(report_parent.path().join(&report_name))
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "report destination must not exist"
    );
    let destination = Directory::open(&args.output_dir, true)?;
    destination.require_empty()?;
    let task = tempfile::Builder::new()
        .prefix("compute-audit-")
        .tempdir_in(report_parent.path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(task.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    // The helper has its own descriptor table, so give it the ordinary path.
    let task_path = report
        .parent()
        .unwrap()
        .join(task.path().file_name().unwrap());
    let config = ServiceConfig::from_env(task_path, input_files)?;
    Ok(Prepared {
        program,
        destination,
        report_parent,
        report_name,
        config,
        _task: task,
    })
}

/// Return the documented exit code after all task-owned processes are closed.
pub async fn run(args: &ComputeRunArgs) -> i32 {
    let prepared = match prepare(args) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("compute-run configuration: {error}");
            return 2;
        }
    };
    let mut report = execute(&prepared).await;
    let retain_audit = report["cleanup"]["cleanupVerified"] == true;
    if retain_audit {
        // Generated files have already been exported. Keep only private audit
        // files, never raw input snapshots or duplicate output capabilities.
        let cleanup = (|| -> Result<()> {
            for entry in std::fs::read_dir(prepared._task.path())? {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_str().is_some_and(|name| {
                    name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit())
                }) {
                    std::fs::remove_file(entry.path())?;
                }
            }
            Ok(())
        })();
        if cleanup.is_err() {
            eprintln!("compute-run audit cleanup failed");
            return 1;
        }
        report["auditDirectory"] =
            json!(prepared._task.path().file_name().unwrap().to_str().unwrap());
    }
    let success = report["ok"] == true;
    if let Err(error) = prepared
        .report_parent
        .write_report(&prepared.report_name, &report)
    {
        eprintln!("compute-run report: {error}");
        return 1;
    }
    if retain_audit {
        let _ = prepared._task.keep();
    }
    println!("{}", json!({"ok":success,"report":args.report}));
    if success {
        0
    } else {
        1
    }
}

async fn execute(prepared: &Prepared) -> Value {
    let service = match ComputeService::start(prepared.config.clone()).await {
        Ok(service) => service,
        Err(_) => {
            return json!({"schemaVersion":1,"ok":false,"final":null,"records":[],"artifacts":[],
            "error":{"code":"sandbox_unavailable","message":"Compute service could not start."},
            "cleanup":{"cleanupVerified":false}})
        }
    };
    let dispatcher = ComputeDispatcher::new(service.clone());
    let options = match service.run_options() {
        Ok(options) => options,
        Err(_) => {
            service.cancel();
            let cleanup = service.finish().await.ok();
            return json!({"schemaVersion":1,"ok":false,"final":null,"records":[],"artifacts":[],
                "error":{"code":"timeout","message":"Compute task deadline expired."},"cleanup":cleanup});
        }
    };
    let manifest = manifest_compute();
    let future = run_program_with_options(&prepared.program, &manifest, &dispatcher, &options);
    let outcome = tokio::select! {
        outcome = future => outcome,
        _ = shutdown_signal() => {
            service.cancel();
            Err(augmentagent_channel_core::code_mode::RunnerError::Protocol("Compute task cancelled.".into()))
        }
    };
    let cleanup = service.finish().await;
    let mut error = if outcome
        .as_ref()
        .map_or(true, |value| value.dispatch_failures > 0)
        || dispatcher.has_failed()
    {
        Some(
            json!({"code":outcome.as_ref().err().map(|error| error.public_code()).unwrap_or("execution_failed"),"message":"Program or compute call failed."}),
        )
    } else {
        None
    };
    let receipt = match cleanup {
        Ok(receipt) => receipt,
        Err(_) => {
            error = Some(
                json!({"code":"cleanup_unverified","message":"Compute cleanup could not be verified."}),
            );
            json!({"cleanupVerified":false,"records":[]})
        }
    };
    let records = receipt.get("records").cloned().unwrap_or_else(|| json!([]));
    let parsed = records
        .as_array()
        .context("invalid execution records")
        .and_then(|records| {
            let mut artifacts = Vec::new();
            for record in records {
                artifacts.extend(serde_json::from_value::<Vec<Artifact>>(
                    record["artifacts"].clone(),
                )?);
            }
            Ok(artifacts)
        });
    let mut exported = Vec::new();
    if error.is_none() {
        match parsed.and_then(|entries| {
            artifacts::export(service.artifact_root(), &prepared.destination, &entries)?;
            Ok(entries)
        }) {
            Ok(entries) => exported = entries,
            Err(_) => {
                error = Some(
                    json!({"code":"output_denied","message":"Artifact export failed verification or destination checks."}),
                )
            }
        }
    }
    json!({"schemaVersion":1,"ok":error.is_none(),"final":outcome.ok().map(|value| value.final_value),
        "records":records,"artifacts":exported,"error":error,
        "cleanup":{"cleanupVerified":receipt["cleanupVerified"],"cancelled":receipt["cancelled"]}})
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM registration");
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
