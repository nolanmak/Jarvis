//! Tone-mirroring eval harness (#73 §7).
//!
//! Picks a hold-out of recent `tone_examples` rows, prints per-row BLEU-4
//! and cosine-BOW scores against a reference draft (passed in via stdin or
//! a CSV of pairs), and emits a mean cosine number plus a CSV trail at
//! `~/.local/state/augmentagent/tone-eval-history.csv` for week-over-week
//! tracking.
//!
//! Two modes (mirroring the spec's pre-flight calibration loop):
//!
//! - `--mode pairs` reads candidate↔reference pairs from a TSV file
//!   (`tab-separated, no header: candidate <TAB> reference`). Use this
//!   when you already have draft outputs (e.g. from a one-off Opus run
//!   with the tone block on/off) and just want the metric.
//! - `--mode self-similarity` (default) sanity-checks the corpus by
//!   scoring each held-out row against the mean recent neighbor — useful
//!   for picking a calibration baseline.

use std::path::PathBuf;

use anyhow::{Context, Result};
use augmentagent_store::{Store, ToneExample};
use augmentagent_tools::scoring::{bleu4, cosine_bow};
use clap::{Parser, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "tone-eval", about = "AugmentAgent tone-mirror eval harness (#73)")]
struct Args {
    /// Path to data.db. Defaults to AUGMENTAGENT_DB or ./data.db.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Mode: pairs (TSV from --pairs-file) or self-similarity baseline.
    #[arg(long, value_enum, default_value_t = Mode::SelfSimilarity)]
    mode: Mode,
    /// TSV of `candidate <TAB> reference` pairs (mode=pairs).
    #[arg(long)]
    pairs_file: Option<PathBuf>,
    /// How many hold-out rows to score.
    #[arg(long, default_value_t = 20)]
    holdout: i64,
    /// Account entity_id to scope the corpus to. Required for self-similarity.
    #[arg(long)]
    account: Option<String>,
    /// Pass threshold on mean cosine (#73 §7 default 0.72).
    #[arg(long, default_value_t = 0.72)]
    pass_threshold: f64,
    /// Append the run summary to this CSV. Default
    /// `~/.local/state/augmentagent/tone-eval-history.csv`.
    #[arg(long)]
    history_csv: Option<PathBuf>,
}

#[derive(Clone, Debug, ValueEnum)]
enum Mode {
    Pairs,
    SelfSimilarity,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let db_path = args
        .db
        .clone()
        .or_else(|| std::env::var("AUGMENTAGENT_DB").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("data.db"));

    let store = Store::open(&db_path).context("open store")?;

    let pairs: Vec<(String, String)> = match args.mode {
        Mode::Pairs => load_pairs_file(args.pairs_file.as_deref().context(
            "--pairs-file is required when --mode pairs",
        )?)?,
        Mode::SelfSimilarity => {
            let account = args
                .account
                .as_deref()
                .context("--account is required for self-similarity mode")?;
            let examples = store
                .recent_tone_examples("global", "*", Some(account), args.holdout)
                .context("recent_tone_examples")?;
            self_similarity_pairs(&examples)
        }
    };

    if pairs.is_empty() {
        eprintln!("(no pairs to score; bail.)");
        std::process::exit(2);
    }

    let mut bleu_scores = Vec::with_capacity(pairs.len());
    let mut cos_scores = Vec::with_capacity(pairs.len());
    println!("# bleu4\tcosine\tcandidate_chars\treference_chars");
    for (cand, refr) in &pairs {
        let b = bleu4(cand, refr);
        let c = cosine_bow(cand, refr);
        bleu_scores.push(b);
        cos_scores.push(c);
        println!(
            "{:.3}\t{:.3}\t{}\t{}",
            b,
            c,
            cand.chars().count(),
            refr.chars().count()
        );
    }
    let mean_bleu = mean(&bleu_scores);
    let mean_cos = mean(&cos_scores);
    println!("# n={n} mean_bleu={mean_bleu:.3} mean_cosine={mean_cos:.3}", n = pairs.len());

    if let Err(e) = append_history_csv(args.history_csv.as_deref(), pairs.len(), mean_bleu, mean_cos)
    {
        eprintln!("(warn) failed to append history csv: {e:#}");
    }

    if mean_cos < args.pass_threshold {
        eprintln!(
            "FAIL: mean cosine {mean_cos:.3} < threshold {threshold:.3}",
            threshold = args.pass_threshold
        );
        std::process::exit(1);
    }
    println!("OK: mean cosine {mean_cos:.3} >= {threshold:.3}", threshold = args.pass_threshold);
    Ok(())
}

fn load_pairs_file(path: &std::path::Path) -> Result<Vec<(String, String)>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let mut pairs = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut split = line.splitn(2, '\t');
        let cand = split.next().unwrap_or_default().to_string();
        let refr = split.next().with_context(|| {
            format!("line {} missing TAB separator", i + 1)
        })?;
        pairs.push((cand, refr.to_string()));
    }
    Ok(pairs)
}

/// Pair each example with the next-most-recent one. Gives a baseline
/// self-similarity number ("how alike are this person's recent emails to
/// each other?") that the actual draft-vs-actual scores are calibrated
/// against. Per #73 §7: a tone-block-on draft must beat this floor.
fn self_similarity_pairs(examples: &[ToneExample]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for w in examples.windows(2) {
        out.push((w[0].body.clone(), w[1].body.clone()));
    }
    out
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn append_history_csv(
    explicit_path: Option<&std::path::Path>,
    n: usize,
    mean_bleu: f64,
    mean_cos: f64,
) -> Result<()> {
    let path = match explicit_path {
        Some(p) => p.to_path_buf(),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/state/augmentagent/tone-eval-history.csv")
        }
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let exists = path.exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {} for append", path.display()))?;
    use std::io::Write;
    if !exists {
        writeln!(f, "ts_iso,n,mean_bleu,mean_cosine")?;
    }
    let now = chrono_now_iso();
    writeln!(f, "{now},{n},{mean_bleu:.4},{mean_cos:.4}")?;
    Ok(())
}

fn chrono_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Avoid pulling in chrono just for one ISO timestamp — simple
    // YYYY-MM-DDTHH:MM:SSZ via Unix-epoch secs.
    let days = now / 86_400;
    let secs_in_day = now % 86_400;
    let h = secs_in_day / 3600;
    let m = (secs_in_day % 3600) / 60;
    let s = secs_in_day % 60;
    let (y, mo, d) = days_to_ymd(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert a Unix-epoch days count to a Gregorian (Y, M, D) tuple. Pure
/// arithmetic, no external deps — tone-eval is a one-shot CLI that
/// doesn't justify the chrono pull-in.
fn days_to_ymd(mut days: i64) -> (i32, u32, u32) {
    days += 719_468; // shift so day 0 = 0000-03-01
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
