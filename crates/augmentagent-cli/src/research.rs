//! Daily automated research pipeline (`augmentagent research`).
//!
//! Pulls recent arXiv AI/agent papers and the latest commits from the
//! `leapmodel` repo, compares them against our own agent process via a
//! **swappable LLM driver**, files GitHub issues for the top gaps, and posts a
//! digest to Discord.
//!
//! ## Swappable reasoning driver
//!
//! The pipeline (fetch → dedup → reason → file issues → post) is owned here in
//! Rust. The LLM is invoked as an external command named by `RESEARCH_LLM_CMD`
//! (default `claude -p --output-format json`): we write a prompt to its stdin
//! and read a JSON gap list from its stdout. Any engine that honours that
//! contract — `codex exec`, `ollama run <model>`, a thin OpenAI-compatible shim
//! — drops in with zero changes to the rest of the pipeline. This mirrors the
//! `CLAUDE_CLI` / `AUGMENTAGENT_GH_BIN` env-override convention used elsewhere.
//!
//! Every stage is best-effort: a failed arXiv fetch, leapmodel fetch, or LLM
//! call logs a warning and the run continues with whatever it has, so the daily
//! job never hard-fails the timer.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use augmentagent_store::Store;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::{info, warn};

/// Default look-back if `--since-hours` is somehow zero.
const DEFAULT_SINCE_HOURS: u32 = 24;
/// How many arXiv entries to pull per category page before date/keyword/dedup
/// filtering trims them down. Overridable via `RESEARCH_FETCH_BATCH`.
const DEFAULT_FETCH_BATCH: u32 = 100;
const DEFAULT_MAX_PAPERS: usize = 15;
const DEFAULT_MAX_ISSUES: u32 = 3;
const DEFAULT_CATEGORIES: &str = "cs.AI,cs.MA,cs.CL,cs.LG";
const DEFAULT_KEYWORDS: &str =
    "agent,tool use,multi-agent,rag,planning,reflection,memory,llm,reasoning";
const DEFAULT_GH_REPO: &str = "nolanmak/MyAgentAssistant";
const DEFAULT_LLM_CMD: &str = "claude -p --output-format json";
const LEAPMODEL_REPO: &str = "jupitersoftco/leapmodel";

/// Resolved-from-env configuration for one run.
struct ResearchConfig {
    categories: Vec<String>,
    keywords: Vec<String>,
    max_papers: usize,
    max_issues: u32,
    fetch_batch: u32,
    gh_repo: String,
    llm_cmd: String,
    gh_bin: String,
}

impl ResearchConfig {
    fn from_env(max_issues_flag: Option<u32>) -> Self {
        let categories = csv_env("RESEARCH_ARXIV_CATEGORIES", DEFAULT_CATEGORIES);
        let keywords = csv_env("RESEARCH_KEYWORDS", DEFAULT_KEYWORDS)
            .into_iter()
            .map(|k| k.to_lowercase())
            .collect();
        let max_papers = std::env::var("RESEARCH_MAX_PAPERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_PAPERS);
        // flag > env > default
        let max_issues = max_issues_flag
            .or_else(|| {
                std::env::var("RESEARCH_MAX_ISSUES")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(DEFAULT_MAX_ISSUES);
        let fetch_batch = std::env::var("RESEARCH_FETCH_BATCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_FETCH_BATCH);
        let gh_repo =
            std::env::var("RESEARCH_GH_REPO").unwrap_or_else(|_| DEFAULT_GH_REPO.to_string());
        let llm_cmd =
            std::env::var("RESEARCH_LLM_CMD").unwrap_or_else(|_| DEFAULT_LLM_CMD.to_string());
        let gh_bin = std::env::var("AUGMENTAGENT_GH_BIN").unwrap_or_else(|_| "gh".to_string());
        Self {
            categories,
            keywords,
            max_papers,
            max_issues,
            fetch_batch,
            gh_repo,
            llm_cmd,
            gh_bin,
        }
    }
}

fn csv_env(key: &str, default: &str) -> Vec<String> {
    let raw = std::env::var(key).unwrap_or_else(|_| default.to_string());
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[derive(Default, Clone)]
struct Paper {
    /// Normalized arXiv id (e.g. `2401.12345`, version stripped) — dedup key.
    id: String,
    /// Canonical abs URL for the digest/issue.
    url: String,
    title: String,
    summary: String,
    published: String,
}

/// One gap the LLM driver surfaced.
#[derive(Debug, Deserialize)]
struct Gap {
    title: String,
    body: String,
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    source: String,
}

#[derive(Debug, Deserialize)]
struct GapWrapper {
    #[serde(default)]
    gaps: Vec<Gap>,
}

/// Latest leapmodel commit (subset of the `gh api .../commits` shape).
#[derive(Debug, Deserialize)]
struct CommitEnvelope {
    sha: String,
    commit: CommitMeta,
}
#[derive(Debug, Deserialize)]
struct CommitMeta {
    message: String,
    author: CommitAuthor,
}
#[derive(Debug, Deserialize)]
struct CommitAuthor {
    #[serde(default)]
    date: String,
}

pub(crate) async fn run_research(
    store: Arc<Store>,
    since_hours: u32,
    post_discord: bool,
    dry_run: bool,
    max_issues: Option<u32>,
) -> Result<()> {
    let cfg = ResearchConfig::from_env(max_issues);
    let since_hours = if since_hours == 0 {
        DEFAULT_SINCE_HOURS
    } else {
        since_hours
    };
    let cutoff = Utc::now() - chrono::Duration::hours(since_hours as i64);
    info!(
        since_hours,
        dry_run,
        post_discord,
        categories = %cfg.categories.join(","),
        max_papers = cfg.max_papers,
        max_issues = cfg.max_issues,
        llm_cmd = %cfg.llm_cmd,
        "research: starting daily run"
    );

    // --- A/B. arXiv: fetch, date-filter, keyword-filter, rank, dedup. -------
    let papers = match fetch_arxiv(&cfg, cutoff).await {
        Ok(p) => p,
        Err(e) => {
            warn!("research: arXiv fetch failed, continuing without papers: {e:#}");
            Vec::new()
        }
    };
    let mut fresh: Vec<Paper> = Vec::new();
    for p in papers {
        if p.id.is_empty() {
            continue;
        }
        match store.research_seen(&p.id) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => warn!("research: dedup lookup failed for {}: {e:#}", p.id),
        }
        fresh.push(p);
        if fresh.len() >= cfg.max_papers {
            break;
        }
    }
    // Mark the surfaced set as seen so a re-run never re-issues them. Done for
    // both dry and live runs (a dry run is how we'd preview the set).
    for p in &fresh {
        if let Err(e) = store.mark_research_seen(&p.id) {
            warn!("research: failed to mark {} seen: {e:#}", p.id);
        }
    }
    info!(
        fresh_papers = fresh.len(),
        "research: arXiv papers selected after filter+dedup"
    );

    // --- C. leapmodel latest commits via gh. --------------------------------
    let commits = match fetch_leapmodel(&cfg, cutoff).await {
        Ok(c) => c,
        Err(e) => {
            warn!("research: leapmodel fetch failed, continuing without it: {e:#}");
            Vec::new()
        }
    };
    info!(leapmodel_commits = commits.len(), "research: leapmodel fetched");

    if fresh.is_empty() && commits.is_empty() {
        let msg = "🔬 **Daily research** — nothing new on arXiv or leapmodel in the window.";
        println!("{msg}");
        if post_discord {
            crate::post_digest_to_discord(msg).await?;
        }
        return Ok(());
    }

    // --- D. Reason via the swappable LLM driver. ----------------------------
    let our_process = load_our_process();
    let prompt = build_prompt(&fresh, &commits, &our_process);
    let gaps = match run_llm_driver(&cfg.llm_cmd, &prompt).await {
        Ok(g) => g,
        Err(e) => {
            warn!("research: LLM driver failed, posting findings without gaps: {e:#}");
            Vec::new()
        }
    };
    info!(gaps = gaps.len(), "research: LLM returned gaps");

    // Rank gaps by confidence, highest first.
    let mut ranked = gaps;
    ranked.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // --- E. File issues for the top gaps (skipped in dry-run). --------------
    let cap = cfg.max_issues as usize;
    let mut created: Vec<(u64, String)> = Vec::new();
    let mut also_noted: Vec<String> = Vec::new();
    for (i, gap) in ranked.iter().enumerate() {
        if i < cap {
            if dry_run {
                info!(
                    title = %gap.title,
                    confidence = gap.confidence,
                    "research: [dry-run] would file issue"
                );
                also_noted.push(format!("(dry-run) {}", gap.title));
            } else {
                match create_issue(&cfg, gap).await {
                    Ok(num) => {
                        info!(issue = num, title = %gap.title, "research: filed issue");
                        created.push((num, gap.title.clone()));
                    }
                    Err(e) => {
                        warn!("research: issue create failed for {:?}: {e:#}", gap.title);
                        also_noted.push(gap.title.clone());
                    }
                }
            }
        } else {
            also_noted.push(gap.title.clone());
        }
    }

    // --- F. Build + emit the digest. ----------------------------------------
    let digest = build_digest(&cfg, &fresh, &commits, &created, &also_noted, dry_run);
    println!("{digest}");
    if post_discord {
        crate::post_digest_to_discord(&digest)
            .await
            .context("posting research digest to Discord")?;
        info!("research: digest posted to Discord");
    }

    Ok(())
}

/// Fetch recent arXiv entries across the configured categories, keep those
/// submitted after `cutoff` whose title/abstract match a keyword.
async fn fetch_arxiv(cfg: &ResearchConfig, cutoff: DateTime<Utc>) -> Result<Vec<Paper>> {
    if cfg.categories.is_empty() {
        return Ok(Vec::new());
    }
    let search_query = cfg
        .categories
        .iter()
        .map(|c| format!("cat:{c}"))
        .collect::<Vec<_>>()
        .join(" OR ");

    let client = reqwest::Client::builder()
        .user_agent("augmentagent-research/1.0")
        .timeout(Duration::from_secs(30))
        .build()
        .context("building arXiv http client")?;

    let resp = client
        .get("http://export.arxiv.org/api/query")
        .query(&[
            ("search_query", search_query.as_str()),
            ("sortBy", "submittedDate"),
            ("sortOrder", "descending"),
            ("start", "0"),
            ("max_results", &cfg.fetch_batch.to_string()),
        ])
        .send()
        .await
        .context("arXiv request")?;
    if !resp.status().is_success() {
        anyhow::bail!("arXiv returned HTTP {}", resp.status());
    }
    let xml = resp.text().await.context("reading arXiv body")?;

    let mut out = Vec::new();
    for p in parse_arxiv_atom(&xml) {
        // Date filter: skip anything older than the window.
        if let Ok(published) = DateTime::parse_from_rfc3339(p.published.trim()) {
            if published.with_timezone(&Utc) < cutoff {
                continue;
            }
        }
        // Keyword filter (empty keyword list ⇒ keep everything).
        if !cfg.keywords.is_empty() {
            let hay = format!("{} {}", p.title, p.summary).to_lowercase();
            if !cfg.keywords.iter().any(|k| hay.contains(k)) {
                continue;
            }
        }
        out.push(p);
    }
    Ok(out)
}

/// Minimal Atom parser for the arXiv API response — extracts id/title/summary/
/// published per `<entry>`. Tolerant: a malformed tail just stops parsing.
fn parse_arxiv_atom(xml: &str) -> Vec<Paper> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut papers = Vec::new();
    let mut buf = Vec::new();
    let mut in_entry = false;
    let mut cur_tag = String::new();
    let mut cur = Paper::default();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "entry" {
                    in_entry = true;
                    cur = Paper::default();
                }
                cur_tag = tag;
            }
            Ok(Event::Text(e)) => {
                if in_entry {
                    let txt = e.xml_content().unwrap_or_default().to_string();
                    match cur_tag.as_str() {
                        "id" => cur.id.push_str(&txt),
                        "title" => cur.title.push_str(&txt),
                        "summary" => cur.summary.push_str(&txt),
                        "published" => cur.published.push_str(&txt),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "entry" {
                    in_entry = false;
                    // The raw <id> is an abs URL; keep it and derive the
                    // version-stripped dedup key.
                    cur.url = cur.id.trim().to_string();
                    cur.id = normalize_arxiv_id(&cur.url);
                    cur.title = collapse_ws(&cur.title);
                    cur.summary = collapse_ws(&cur.summary);
                    papers.push(std::mem::take(&mut cur));
                }
                cur_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    papers
}

/// `http://arxiv.org/abs/2401.12345v2` → `2401.12345` (version stripped so
/// v1/v2 of the same paper dedup to one row).
fn normalize_arxiv_id(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or(url);
    match tail.rfind('v') {
        Some(i) if tail[i + 1..].chars().all(|c| c.is_ascii_digit()) && i > 0 => {
            tail[..i].to_string()
        }
        _ => tail.to_string(),
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Fetch leapmodel commits since `cutoff` by shelling out to the already
/// authenticated `gh` CLI — one auth mechanism for the whole pipeline.
async fn fetch_leapmodel(
    cfg: &ResearchConfig,
    cutoff: DateTime<Utc>,
) -> Result<Vec<CommitEnvelope>> {
    let since = cutoff.to_rfc3339();
    let endpoint = format!("repos/{LEAPMODEL_REPO}/commits?since={since}&per_page=30");
    let out = tokio::process::Command::new(&cfg.gh_bin)
        .arg("api")
        .arg(&endpoint)
        .output()
        .await
        .context("spawning gh api for leapmodel")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("gh api {endpoint} failed: {stderr}");
    }
    let commits: Vec<CommitEnvelope> =
        serde_json::from_slice(&out.stdout).context("parsing gh commits JSON")?;
    Ok(commits)
}

/// Read a concise snapshot of "our agent process" from repo docs to ground the
/// gap analysis. Best-effort: missing files are skipped.
fn load_our_process() -> String {
    let mut parts = Vec::new();
    if let Some(readme) = read_truncated("README.md", 8000) {
        parts.push(format!("# README\n{readme}"));
    }
    // schema/*.md are the agent's actual prompts — strong gap-analysis signal.
    if let Ok(entries) = std::fs::read_dir("schema") {
        let mut files: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
            .collect();
        files.sort();
        for f in files.iter().take(6) {
            if let Some(body) = f.to_str().and_then(|p| read_truncated(p, 2500)) {
                parts.push(format!("# {}\n{}", f.display(), body));
            }
        }
    }
    parts.join("\n\n---\n\n")
}

fn read_truncated(path: &str, max: usize) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    if s.len() > max {
        Some(format!("{}…[truncated]", &s[..max]))
    } else {
        Some(s)
    }
}

fn build_prompt(papers: &[Paper], commits: &[CommitEnvelope], our_process: &str) -> String {
    let mut p = String::new();
    p.push_str(
        "You are a research analyst for an autonomous personal-assistant agent \
         (email/Discord triage → draft → human approval, with a wiki memory and \
         self-improvement loop). Below are (1) our agent process, (2) recent \
         arXiv papers, and (3) recent commits from the `leapmodel` ML research \
         repo.\n\n\
         Identify concrete GAPS: techniques, ideas, or findings from the papers \
         or leapmodel work that we could adopt to improve our agent's reasoning, \
         reliability, memory, or iteration loop. Be specific and actionable — \
         each gap should be filable as a GitHub issue. Prefer a few high-value \
         gaps over many shallow ones. For leapmodel, also consider whether its \
         training-dynamics findings transfer to our agent's iteration/convergence \
         behaviour (low transfer is fine — only flag real ones).\n\n\
         Respond with ONLY a JSON object, no prose, no markdown fences:\n\
         {\"gaps\":[{\"title\":\"...\",\"body\":\"markdown issue body with a \
         'Source:' line and a 'Why it matters for our agent:' section\",\
         \"confidence\":0.0-1.0,\"source\":\"arxiv:<id> or leapmodel:<sha>\"}]}\n\n",
    );
    p.push_str("=== OUR AGENT PROCESS ===\n");
    p.push_str(our_process);
    p.push_str("\n\n=== RECENT ARXIV PAPERS ===\n");
    if papers.is_empty() {
        p.push_str("(none in window)\n");
    }
    for paper in papers {
        p.push_str(&format!(
            "- [{}] {}\n  {}\n  abstract: {}\n",
            paper.id,
            paper.title,
            paper.url,
            truncate(&paper.summary, 1200)
        ));
    }
    p.push_str("\n=== RECENT LEAPMODEL COMMITS ===\n");
    if commits.is_empty() {
        p.push_str("(none in window)\n");
    }
    for c in commits {
        let short = &c.sha[..c.sha.len().min(7)];
        p.push_str(&format!(
            "- [{}] {} ({})\n",
            short,
            truncate(&c.commit.message.replace('\n', " "), 500),
            c.commit.author.date
        ));
    }
    p
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max])
    } else {
        s.to_string()
    }
}

/// Spawn the configured LLM command, pipe `prompt` to stdin, parse gaps from
/// stdout. Engine-agnostic: handles a raw `{"gaps":[…]}` reply or a
/// `claude -p --output-format json` envelope `{"result":"…"}`.
async fn run_llm_driver(cmd: &str, prompt: &str) -> Result<Vec<Gap>> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let mut parts = cmd.split_whitespace();
    let program = parts.next().context("RESEARCH_LLM_CMD is empty")?;
    let args: Vec<&str> = parts.collect();

    let mut child = tokio::process::Command::new(program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning RESEARCH_LLM_CMD ({program})"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("writing prompt to LLM stdin")?;
        stdin.shutdown().await.ok();
    }

    let out = child
        .wait_with_output()
        .await
        .context("waiting on LLM driver")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("LLM driver exited non-zero: {stderr}");
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    parse_llm_gaps(&stdout)
}

/// Tolerant extraction of the gap list from whatever the driver printed.
fn parse_llm_gaps(raw: &str) -> Result<Vec<Gap>> {
    // 1. Maybe the driver printed our wrapper directly.
    if let Ok(w) = serde_json::from_str::<GapWrapper>(raw) {
        if !w.gaps.is_empty() {
            return Ok(w.gaps);
        }
    }
    // 2. Maybe it's a claude `--output-format json` envelope: unwrap `.result`.
    let inner = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => v
            .get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| raw.to_string()),
        Err(_) => raw.to_string(),
    };
    // 3. Slice the first {...} object out of the (possibly fenced/prosey) text.
    let candidate = slice_json_object(&inner).unwrap_or(inner.as_str());
    let w: GapWrapper = serde_json::from_str(candidate)
        .with_context(|| format!("parsing gaps JSON from driver output: {}", truncate(raw, 400)))?;
    Ok(w.gaps)
}

/// Return the substring from the first `{` to its matching `}` (brace-depth
/// aware), so markdown fences or surrounding prose don't defeat the parse.
fn slice_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0i32;
    for (i, c) in s[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// File one issue via `gh issue create --repo <repo>`. Returns the issue
/// number parsed from the URL gh prints on success.
async fn create_issue(cfg: &ResearchConfig, gap: &Gap) -> Result<u64> {
    let body = format!(
        "{}\n\n---\n_Source: {}_\n_Auto-filed by the daily `augmentagent research` pipeline._",
        gap.body, gap.source
    );
    let out = tokio::process::Command::new(&cfg.gh_bin)
        .arg("issue")
        .arg("create")
        .arg("--repo")
        .arg(&cfg.gh_repo)
        .arg("--title")
        .arg(&gap.title)
        .arg("--body")
        .arg(&body)
        .output()
        .await
        .context("spawning gh issue create")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("gh issue create failed: {stderr}");
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let url = stdout.trim();
    url.rsplit('/')
        .find_map(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow::anyhow!("gh stdout has no issue number: {url:?}"))
}

fn build_digest(
    cfg: &ResearchConfig,
    papers: &[Paper],
    commits: &[CommitEnvelope],
    created: &[(u64, String)],
    also_noted: &[String],
    dry_run: bool,
) -> String {
    let mut d = String::new();
    d.push_str("🔬 **Daily research digest**\n\n");

    if !created.is_empty() {
        d.push_str("**Issues filed:**\n");
        for (num, title) in created {
            d.push_str(&format!(
                "• #{num} — {title}\n  https://github.com/{}/issues/{num}\n",
                cfg.gh_repo
            ));
        }
        d.push('\n');
    } else if dry_run {
        d.push_str("_(dry-run — no issues filed)_\n\n");
    }

    if !also_noted.is_empty() {
        d.push_str("**Also noted (no issue):**\n");
        for t in also_noted {
            d.push_str(&format!("• {t}\n"));
        }
        d.push('\n');
    }

    if !papers.is_empty() {
        d.push_str(&format!("**arXiv ({} new):**\n", papers.len()));
        for p in papers {
            d.push_str(&format!("• {} — {}\n", p.title, p.url));
        }
        d.push('\n');
    }

    if !commits.is_empty() {
        d.push_str(&format!("**leapmodel ({} commits):**\n", commits.len()));
        for c in commits {
            let short = &c.sha[..c.sha.len().min(7)];
            let line = c.commit.message.lines().next().unwrap_or("");
            d.push_str(&format!("• `{short}` {}\n", truncate(line, 140)));
        }
    }

    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_version() {
        assert_eq!(normalize_arxiv_id("http://arxiv.org/abs/2401.12345v2"), "2401.12345");
        assert_eq!(normalize_arxiv_id("http://arxiv.org/abs/2401.12345"), "2401.12345");
    }

    #[test]
    fn parse_atom_extracts_entries() {
        let xml = r#"<?xml version="1.0"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <id>query-id</id>
          <entry>
            <id>http://arxiv.org/abs/2401.00001v1</id>
            <title>An LLM Agent for Tool Use</title>
            <summary>We present a multi-agent system.</summary>
            <published>2026-06-23T10:00:00Z</published>
          </entry>
        </feed>"#;
        let papers = parse_arxiv_atom(xml);
        assert_eq!(papers.len(), 1);
        assert_eq!(papers[0].id, "2401.00001");
        assert_eq!(papers[0].title, "An LLM Agent for Tool Use");
    }

    #[test]
    fn parse_gaps_raw_wrapper() {
        let raw = r#"{"gaps":[{"title":"Add reflection","body":"x","confidence":0.8,"source":"arxiv:1"}]}"#;
        let gaps = parse_llm_gaps(raw).unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].title, "Add reflection");
    }

    #[test]
    fn parse_gaps_claude_envelope_with_fences() {
        let raw = r#"{"type":"result","result":"```json\n{\"gaps\":[{\"title\":\"T\",\"body\":\"b\",\"confidence\":0.5,\"source\":\"s\"}]}\n```"}"#;
        let gaps = parse_llm_gaps(raw).unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].title, "T");
    }

    #[test]
    fn slice_json_handles_prose() {
        let s = "here you go:\n```json\n{\"gaps\":[]}\n```\nbye";
        assert_eq!(slice_json_object(s), Some("{\"gaps\":[]}"));
    }
}
