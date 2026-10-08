//! Provider-free compute acceptance entrypoint. This runs before dotenv,
//! database setup, provider credentials, and channel initialization.
use anyhow::{Context, Result};
use augmentagent_channel_core::code_mode::{
    compute::{
        artifacts::{self, Artifact, Directory},
        retention, ComputeDispatcher, ComputeService, ServiceConfig,
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
    storage: Option<retention::Lease>,
    _disabled: Option<tempfile::TempDir>,
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
    let mut config = ServiceConfig::from_env(PathBuf::new(), input_files)?;
    let (storage, disabled) = if config.policy.enabled {
        let lease = retention::Lease::reserve(
            &config.scratch_root.join("compute-artifacts"),
            "cli",
            retention::now()?,
            config.scratch_limits.context("missing scratch limits")?,
        )?;
        config.artifact_root = lease.path.clone();
        (Some(lease), None)
    } else {
        // Disabled computation cannot import selected data. Keep only bounded,
        // non-sensitive refusal metadata; no provisioned VM storage is needed.
        config.input_files.clear();
        let temporary = tempfile::Builder::new()
            .prefix("compute-disabled-")
            .tempdir()?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
        config.artifact_root = temporary.path().to_owned();
        (None, Some(temporary))
    };
    Ok(Prepared {
        program,
        destination,
        report_parent,
        report_name,
        config,
        storage,
        _disabled: disabled,
    })
}

/// Return the documented exit code after all task-owned processes are closed.
pub async fn run(args: &ComputeRunArgs) -> i32 {
    // Register synchronously before preparing storage or spawning a helper.
    // Signals arriving during initialization remain queued for execute().
    let mut shutdown = match ShutdownSignals::register() {
        Ok(shutdown) => shutdown,
        Err(error) => {
            eprintln!("compute-run signal registration: {error}");
            return 1;
        }
    };
    let mut prepared = match prepare(args) {
        Ok(prepared) => prepared,
        Err(error) if error.downcast_ref::<retention::AdmissionDenied>().is_some() => {
            let written = (|| -> Result<()> {
                let path = std::path::absolute(&args.report)?;
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("invalid report name")?;
                let parent =
                    Directory::open(path.parent().context("report parent missing")?, false)?;
                parent.write_report(name, &json!({"schemaVersion":1,"ok":false,"final":null,"records":[],"artifacts":[],
                    "error":{"code":"resource_limit","message":"Compute storage capacity or admission limit exceeded."},
                    "cleanup":{"cleanupVerified":true}}))
            })();
            if written.is_err() {
                eprintln!("compute-run admission report could not be written");
            }
            return 1;
        }
        Err(error) => {
            eprintln!("compute-run configuration: {error}");
            return 2;
        }
    };
    let mut report = execute(&prepared, &mut shutdown).await;
    let retain_audit = report["cleanup"]["cleanupVerified"] == true;
    let audit = if retain_audit {
        let exported = (|| -> Result<tempfile::TempDir> {
            let (temporary, directory) =
                prepared.report_parent.private_tempdir("compute-audit-")?;
            if report["startupFailure"] == true {
                // The helper may have been stopped before creating its journal.
                // Record the host's verified pre-workload cancellation privately.
                directory.write_report("audit.json", &json!({"schemaVersion":1,
                    "phase":"initialization", "error":report["error"],
                    "cleanupVerified":true,"closed":true,
                    "cancelled":report["cleanup"]["cancelled"],"records":[]}))?;
            } else {
                artifacts::export_audit(&prepared.config.artifact_root, &directory)?;
            }
            if let Some(lease) = prepared.storage.take() {
                lease.discard()?;
            }
            Ok(temporary)
        })();
        match exported {
            Ok(temporary) => {
                report["auditDirectory"] =
                    json!(temporary.path().file_name().unwrap().to_str().unwrap());
                Some(temporary)
            }
            Err(_) => {
                report["ok"] = json!(false);
                report["error"] = json!({"code":"cleanup_unverified","message":"Compute audit export or storage cleanup failed."});
                report["cleanup"]["cleanupVerified"] = json!(false);
                None
            }
        }
    } else {
        None
    };
    let success = report["ok"] == true;
    if let Err(error) = prepared
        .report_parent
        .write_report(&prepared.report_name, &report)
    {
        eprintln!("compute-run report: {error}");
        return 1;
    }
    if let Some(audit) = audit {
        let _ = audit.keep();
    }
    println!("{}", json!({"ok":success,"report":args.report}));
    if success {
        0
    } else {
        1
    }
}

async fn execute(prepared: &Prepared, shutdown: &mut ShutdownSignals) -> Value {
    let service = match ComputeService::start_cancellable(prepared.config.clone(), shutdown.recv()).await {
        Ok(service) => service,
        Err(error) => {
            let failure = error.downcast_ref::<augmentagent_channel_core::code_mode::compute::service::StartupFailure>();
            let code = failure.map_or("sandbox_unavailable", |failure| failure.code);
            let verified = failure.is_some_and(|failure| failure.cleanup_verified);
            return json!({"schemaVersion":1,"ok":false,"final":null,"records":[],"artifacts":[],
            "startupFailure":true,
            "error":{"code":code,"message":"Compute service initialization did not complete."},
            "cleanup":{"cleanupVerified":verified,"cancelled":code == "cancelled"}})
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
    let mut cancelled = false;
    let outcome = tokio::select! {
        biased;
        _ = shutdown.recv() => {
            cancelled = true;
            service.cancel();
            Err(augmentagent_channel_core::code_mode::RunnerError::Protocol("Compute task cancelled.".into()))
        }
        outcome = future => outcome,
    };
    let cleanup = service.finish().await;
    let mut error = if outcome
        .as_ref()
        .map_or(true, |value| value.dispatch_failures > 0)
        || dispatcher.has_failed()
    {
        Some(
            json!({"code":if cancelled { "cancelled" } else { outcome.as_ref().err().map(|error| error.public_code()).unwrap_or("execution_failed") },"message":"Program or compute call failed."}),
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

struct ShutdownSignals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn register() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            term: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
        })
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        tokio::select! { _ = self.term.recv() => {}, _ = self.interrupt.recv() => {} }
        #[cfg(not(unix))]
        { let _ = tokio::signal::ctrl_c().await; }
    }
}
