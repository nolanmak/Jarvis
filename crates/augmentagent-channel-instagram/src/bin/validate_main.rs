//! `augmentagent-instagram-validate` — operator entrypoint for the #17
//! validation harness.
//!
//! `docs/instagram-protocol.md` was reconstructed from public knowledge and
//! is marked **REQUIRES LIVE OPERATOR VALIDATION**. This is the command that
//! discharges that requirement: an operator with a live logged-in IG session
//! harvests cookies (`scripts/instagram-harvest.sh`), then runs:
//!
//! ```text
//! augmentagent-instagram-validate --cookies instagram-auth.json \
//!   --feed-user <numeric id of a close contact>
//! ```
//!
//! Reads are exercised unconditionally; the DM-send and comment probes are
//! **dry-run by default** (request built, never POSTed) and only fire a fixed
//! recognizable marker when `--exercise-writes` + the relevant target id are
//! both supplied. Exit code is non-zero iff a probe detected protocol
//! **drift** (a soft-block / challenge during validation is reported but does
//! not fail the run — the detector working is itself a pass).
//!
//! JSON report → stdout (CI / runbook checkbox); human table → stderr.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use augmentagent_channel_instagram::{
    run_validation, InstagramAuth, ValidateOpts, WebClient,
};

#[derive(Parser)]
#[command(
    name = "augmentagent-instagram-validate",
    about = "Live operator validation of docs/instagram-protocol.md (#17)"
)]
struct Args {
    /// Path to the harvested cookie JSON (the `instagram login` file shape).
    #[arg(long)]
    cookies: PathBuf,

    /// Numeric user id of a contact to exercise the feed-by-user read.
    /// Without it the feed probe is skipped (not failed).
    #[arg(long)]
    feed_user: Option<String>,

    /// Existing 1:1 thread id for the send-dm probe. Only used when
    /// `--exercise-writes` is also passed.
    #[arg(long)]
    thread: Option<String>,

    /// Media id for the comment probe. Only used with `--exercise-writes`.
    #[arg(long)]
    media: Option<String>,

    /// Actually POST a fixed marker on the send-dm / comment probes. OFF by
    /// default — the harness is read-biased and side-effect-free unless this
    /// is explicitly set. Even then, the text is a fixed recognizable marker,
    /// never an LLM draft.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    exercise_writes: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let auth = match InstagramAuth::load(&args.cookies) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "failed to load cookies from {}: {e}",
                args.cookies.display()
            );
            return ExitCode::from(2);
        }
    };

    let api = WebClient::new(auth.clone());
    let opts = ValidateOpts {
        feed_user: args.feed_user,
        thread_id: args.thread,
        media_id: args.media,
        exercise_writes: args.exercise_writes,
    };
    let now_ms = chrono::Utc::now().timestamp_millis();

    let report = run_validation(&auth, &api, &opts, now_ms).await;

    // Human table → stderr; machine JSON → stdout (so `... | jq` works and a
    // runbook step can assert on it without scraping the table).
    eprintln!("{}", report.render_table());
    match serde_json::to_string_pretty(&report) {
        Ok(j) => println!("{j}"),
        Err(e) => eprintln!("(failed to serialize JSON report: {e})"),
    }

    if report.passed() {
        ExitCode::SUCCESS
    } else {
        // Drift detected — the reconstructed protocol doc is wrong somewhere;
        // re-capture before trusting the channel.
        ExitCode::from(1)
    }
}
