use augmentagent_ccat::{
    evaluate, now_epoch, redact, DecisionOutcome, DecisionProvider, ReceiptSigner,
    SeaCatDecisionProvider, PUBLIC_GIT_PUSH_POLICY,
};
use std::{env, fs, fs::OpenOptions, io::Write, path::Path, process::ExitCode, time::Duration};

const MAX_INPUT_BYTES: u64 = 512 * 1024;

fn usage() -> ! {
    eprintln!("usage: augmentagent-ccat <public-git-push|receipt-create|receipt-verify> ...");
    std::process::exit(64);
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        usage();
    };
    if command == "receipt-create" {
        return receipt_create(args.collect());
    }
    if command == "receipt-verify" {
        return receipt_verify(args.collect());
    }
    if command != "public-git-push" {
        usage();
    }
    let Some(path) = args.next() else {
        usage();
    };
    if args.next().is_some() {
        usage();
    }
    if env::var("AUGMENTAGENT_CCAT_ENABLED").as_deref() != Ok("true") {
        eprintln!("CCat is disabled; set AUGMENTAGENT_CCAT_ENABLED=true to enable this gate.");
        return ExitCode::from(12);
    }
    let metadata = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_INPUT_BYTES => metadata,
        Ok(_) => {
            eprintln!("CCat input is not a regular file within the configured size limit.");
            return ExitCode::from(12);
        }
        Err(_) => {
            eprintln!("CCat input is unavailable.");
            return ExitCode::from(12);
        }
    };
    let _ = metadata;
    let input = match fs::read_to_string(&path) {
        Ok(input) => input,
        Err(_) => {
            eprintln!("CCat input is not valid UTF-8.");
            return ExitCode::from(12);
        }
    };
    let payload = redact(&input);
    let provider = match SeaCatDecisionProvider::from_environment() {
        Ok(provider) => provider,
        Err(_) => {
            eprintln!("CCat is not configured.");
            return ExitCode::from(12);
        }
    };
    let decision = evaluate(
        &PUBLIC_GIT_PUSH_POLICY,
        payload.sha256,
        provider
            .decide(&payload.text, &PUBLIC_GIT_PUSH_POLICY)
            .await,
    );
    // This is intentionally the entire operator-facing receipt: no source
    // material, prompt, provider answer text, or credential crosses stdout.
    println!(
        "policy={} version={} outcome={:?} payload_sha256={} provider={} model={}",
        decision.policy_id,
        decision.policy_version,
        decision.outcome,
        decision.payload_sha256,
        decision.provider.unwrap_or_else(|| "none".into()),
        decision.model.unwrap_or_else(|| "none".into())
    );
    match decision.outcome {
        DecisionOutcome::Allow => ExitCode::SUCCESS,
        DecisionOutcome::Review => ExitCode::from(10),
        DecisionOutcome::Block => ExitCode::from(11),
        DecisionOutcome::Unavailable => ExitCode::from(12),
    }
}

fn receipt_create(args: Vec<String>) -> ExitCode {
    if args.len() != 4 {
        usage();
    }
    let signer = match ReceiptSigner::from_environment() {
        Ok(signer) => signer,
        Err(_) => {
            eprintln!("CCat approval signing key is not configured.");
            return ExitCode::from(12);
        }
    };
    let receipt = match signer.issue(
        args[0].clone(),
        args[1].clone(),
        args[2].clone(),
        now_epoch().unwrap_or(0),
        Duration::from_secs(300),
    ) {
        Ok(receipt) => receipt,
        Err(error) => {
            eprintln!("CCat receipt could not be created: {error}");
            return ExitCode::from(12);
        }
    };
    let serialized = match serde_json::to_vec(&receipt) {
        Ok(value) => value,
        Err(_) => return ExitCode::from(12),
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(Path::new(&args[3])).and_then(|mut file| {
        file.write_all(&serialized)?;
        file.write_all(b"\n")
    }) {
        Ok(()) => {
            println!("receipt_id={}", receipt.receipt_id);
            ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("CCat receipt could not be written to a new private file.");
            ExitCode::from(12)
        }
    }
}

fn receipt_verify(args: Vec<String>) -> ExitCode {
    if args.len() != 5 {
        usage();
    }
    let signer = match ReceiptSigner::from_environment() {
        Ok(signer) => signer,
        Err(_) => return ExitCode::from(12),
    };
    match signer.verify_and_consume(
        Path::new(&args[0]),
        &args[1],
        &args[2],
        &args[3],
        Path::new(&args[4]),
        now_epoch().unwrap_or(0),
    ) {
        Ok(path) => {
            println!(
                "receipt_consumed={}",
                path.file_name().and_then(|v| v.to_str()).unwrap_or("used")
            );
            ExitCode::SUCCESS
        }
        Err(_) => ExitCode::from(12),
    }
}
