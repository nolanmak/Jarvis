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
//!
//! ## `selectors` subcommand (#76 — no cookies, no network, no browser)
//!
//! The live-DOM half of #76 (does each selector still resolve against
//! Instagram's UI?) is operator-gated and intentionally not automated. The
//! *registry-shape* half is fully autonomous: a dry-run audit that the
//! layered selector registry is structurally sound (every target has
//! fallbacks, layers are in resilience order, file-input targets only target
//! file inputs, load-bearing terminals keep a text backstop, the
//! reel/carousel/story surfaces are all wired in). Run it in CI / a pre-merge
//! checkbox so a selector edit can't silently rot the registry shape:
//!
//! ```text
//! augmentagent-instagram-validate selectors
//! ```
//!
//! Exit 0 = shape clean; exit 1 = a shape defect (NOT a live-DOM verdict).
//! Backward compatible: invoked with no subcommand (legacy flag form) it
//! still runs the protocol harness.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use augmentagent_channel_instagram::{
    run_validation, selector_registry_report, validate_selector_registry,
    InstagramAuth, ValidateOpts, WebClient,
};

#[derive(Parser)]
#[command(
    name = "augmentagent-instagram-validate",
    about = "Instagram validation: live protocol harness (#17) + offline \
             selector-registry shape audit (#76)"
)]
struct Args {
    /// Offline selector-registry shape audit (#76). No cookies / network /
    /// browser. Subcommand form: `... selectors`. When omitted, the legacy
    /// flag form runs the live protocol harness (back-compat).
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the harvested cookie JSON (the `instagram login` file shape).
    /// Required for the live protocol harness (legacy flag form).
    #[arg(long)]
    cookies: Option<PathBuf>,

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

#[derive(clap::Subcommand)]
enum Command {
    /// Dry-run audit of the layered selector registry (#76). Proves the
    /// registry is *structurally* sound; does NOT check live-DOM resolution
    /// (that is operator-gated and out of scope for an offline tool).
    Selectors,
}

/// The offline selector-registry dry-run. Emits a JSON `{ok, ...}` doc to
/// stdout (CI-greppable) and a human line to stderr; exit 1 on a shape defect.
fn run_selector_audit() -> ExitCode {
    let defects = validate_selector_registry();
    match selector_registry_report() {
        Ok(msg) => {
            eprintln!("{msg}");
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "kind": "selector_registry_shape",
                    "defects": [],
                    "note": "live-DOM resolution is operator-gated and NOT \
                             verified by this dry-run",
                })
            );
            ExitCode::SUCCESS
        }
        Err(report) => {
            eprintln!("{report}");
            let defect_json: Vec<_> = defects
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "target": d.target,
                        "detail": d.detail,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "ok": false,
                    "kind": "selector_registry_shape",
                    "defects": defect_json,
                })
            );
            ExitCode::from(1)
        }
    }
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

    if let Some(Command::Selectors) = args.command {
        // Fully offline; no auth load, no network.
        return run_selector_audit();
    }

    let cookies = match args.cookies {
        Some(p) => p,
        None => {
            eprintln!(
                "no subcommand and no --cookies: pass `selectors` for the \
                 offline registry audit, or --cookies <file> for the live \
                 protocol harness"
            );
            return ExitCode::from(2);
        }
    };

    let auth = match InstagramAuth::load(&cookies) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "failed to load cookies from {}: {e}",
                cookies.display()
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
