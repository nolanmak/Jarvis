//! Loop options that change what a run reads and where its findings go (#2246):
//! instructions from a wiki file, a re-report window for keyed findings, and
//! one email per new finding to an address the owner has allowlisted.
use super::*;
use augmentagent_store::{LoopFinding, LoopOptions};
use std::path::{Component, Path, PathBuf};

/// Comma-separated owner addresses a loop may email without approval.
/// Unset or empty turns email delivery off.
pub const SELF_SEND_ENV: &str = "AUGMENTAGENT_LOOP_SELF_SEND_ADDRESSES";

/// Sends one loop email. The CLI implements this against the connected mail
/// account; the scheduler decides whether a send is allowed before calling.
#[async_trait]
pub trait LoopMailer: Send + Sync {
    /// Send `body` to `to` from `account` (the only connected account when
    /// `None`) and return the sending address.
    async fn send(
        &self,
        account: Option<&str>,
        to: &str,
        subject: &str,
        body: &str,
    ) -> anyhow::Result<String>;
}

pub fn parse_self_send_allowlist(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|a| a.trim().to_ascii_lowercase())
        .filter(|a| !a.is_empty())
        .collect()
}

pub fn self_send_allowlist() -> Vec<String> {
    parse_self_send_allowlist(&std::env::var(SELF_SEND_ENV).unwrap_or_default())
}

/// Every address a loop may email: the env list plus the owner's own active
/// connected mail accounts (#2256). Connecting an account is an owner action
/// outside the agent's toolbelt, so the agent cannot widen this list.
pub fn own_send_addresses(store: &Store, env_list: &str) -> Vec<String> {
    let mut addresses = parse_self_send_allowlist(env_list);
    for account in store.get_active_gmail_accounts().unwrap_or_default() {
        let email = account.email.trim().to_ascii_lowercase();
        if !email.is_empty() && !addresses.contains(&email) {
            addresses.push(email);
        }
    }
    addresses
}

/// One plain address, exactly as allowlisted. Anything that could name a
/// second recipient (a list, a display name, whitespace) is refused.
pub fn self_send_allowed(address: &str, allowlist: &[String]) -> bool {
    let address = address.trim().to_ascii_lowercase();
    !address.is_empty()
        && address.matches('@').count() == 1
        && !address.contains(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '<' | '>' | '"'))
        && allowlist.iter().any(|allowed| *allowed == address)
}

/// A wiki-relative path with no way out of the wiki.
pub fn validate_prompt_file(path: &str) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("prompt file path is empty".into());
    }
    let ok = Path::new(trimmed)
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if !ok || Path::new(trimmed).is_absolute() {
        return Err(format!("prompt file `{trimmed}` must be a relative path inside the wiki"));
    }
    Ok(())
}

/// Read a loop's instructions file. Symlinks are resolved before the
/// containment check, so a link cannot point the loop outside the wiki.
pub(super) fn read_prompt_file(wiki_root: Option<&Path>, path: &str) -> Result<String, String> {
    validate_prompt_file(path)?;
    let root = wiki_root.ok_or("prompt file needs a wiki directory")?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("wiki directory unavailable: {e}"))?;
    let file: PathBuf = root
        .join(path.trim())
        .canonicalize()
        .map_err(|_| format!("prompt file `{}` not found in the wiki", path.trim()))?;
    if !file.starts_with(&root) {
        return Err(format!("prompt file `{}` resolves outside the wiki", path.trim()));
    }
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("prompt file `{}` unreadable: {e}", path.trim()))?;
    if text.trim().is_empty() {
        return Err(format!("prompt file `{}` is empty", path.trim()));
    }
    Ok(text)
}

/// The instructions for this run: the stored prompt, then the file's text.
pub(super) fn base_prompt(
    l: &UserLoop,
    options: &LoopOptions,
    wiki_root: Option<&Path>,
) -> Result<String, String> {
    match options.prompt_file.as_deref() {
        Some(path) => Ok(format!("{}\n\n{}", l.prompt, read_prompt_file(wiki_root, path)?.trim())),
        None => Ok(l.prompt.clone()),
    }
}

/// Reply-contract additions for a loop that dedupes by key or emails
/// findings. Empty for every other loop, which keeps their prompt unchanged.
pub(super) fn prompt_extra(store: &Store, l: &UserLoop, options: &LoopOptions, now: i64) -> String {
    if options.rereport_after_secs.is_none() && options.email_to.is_none() {
        return String::new();
    }
    let mut out = String::from(
        " Each finding may also carry \"key\": a short stable identifier for the one subject it is about (for a person, their wiki page name or primary handle), reused exactly on every run, and \"body\": the complete text to deliver for that finding. Report one finding per subject.",
    );
    let since = options
        .rereport_after_secs
        .map(|secs| now.saturating_sub(secs.saturating_mul(1000)));
    let keys = store.loop_covered_keys(&l.id, since, 400).unwrap_or_default();
    if !keys.is_empty() {
        let mut list = String::new();
        for key in keys {
            if list.len() + key.len() > 8 * 1024 {
                break;
            }
            if !list.is_empty() {
                list.push_str(", ");
            }
            list.push_str(&key);
        }
        out.push_str(" Keys already reported and not due again (do not report these): ");
        out.push_str(&list);
        out.push('.');
    }
    out
}

/// Findings not yet reported, or keyed ones whose re-report window has
/// passed. Repeats of one finding inside a single report count once.
pub(super) fn fresh(
    store: &Store,
    l: &UserLoop,
    options: &LoopOptions,
    findings: &[LoopFinding],
    now: i64,
) -> Vec<LoopFinding> {
    let mut seen = std::collections::HashSet::new();
    findings
        .iter()
        .filter(|f| seen.insert(f.fingerprint.clone()))
        .filter(|f| {
            match store.loop_finding_reported_at(&l.id, &f.fingerprint).unwrap_or(None) {
                None => true,
                Some(at) => match (f.key.is_some(), options.rereport_after_secs) {
                    (true, Some(secs)) => now.saturating_sub(at) >= secs.saturating_mul(1000),
                    _ => false,
                },
            }
        })
        .cloned()
        .collect()
}

pub(super) fn email_subject(finding: &LoopFinding) -> String {
    let line = finding.finding.lines().next().unwrap_or_default().trim();
    truncate(line, 150)
}

pub(super) fn email_body(l: &UserLoop, finding: &LoopFinding, token: &str) -> String {
    let mut body = finding.body.clone().unwrap_or_else(|| finding.finding.clone());
    body.push_str("\n\nEvidence:\n");
    for evidence in &finding.evidence {
        body.push_str("- ");
        body.push_str(evidence);
        body.push('\n');
    }
    body.push('\n');
    body.push_str(&augmentagent_store::loop_delivery_footer(&l.id, token));
    body.push('\n');
    body
}

/// Why email delivery stopped. A refusal is a configuration problem and
/// fails the run; a send failure keeps the answer and retries delivery.
pub(super) enum EmailError {
    Refused(String),
    Send(String),
}

/// Email each fresh finding, recording it as covered straight after its send
/// so a retry never sends it again. Returns the subjects sent.
#[allow(clippy::too_many_arguments)]
pub(super) async fn email_findings(
    store: &Store,
    mailer: Option<&Arc<dyn LoopMailer>>,
    allowlist: &[String],
    l: &UserLoop,
    options: &LoopOptions,
    to: &str,
    request_id: &str,
    fresh: &[LoopFinding],
) -> Result<Vec<String>, EmailError> {
    if !self_send_allowed(to, allowlist) {
        return Err(EmailError::Refused(format!(
            "email delivery refused: `{to}` is neither a connected mail account nor in {SELF_SEND_ENV}"
        )));
    }
    let Some(mailer) = mailer else {
        return Err(EmailError::Refused(
            "email delivery refused: this daemon has no mail account configured".into(),
        ));
    };
    let mut sent = Vec::new();
    for finding in fresh {
        let token = uuid::Uuid::new_v4().to_string();
        let subject = email_subject(finding);
        let body = email_body(l, finding, &token);
        let from = mailer
            .send(options.email_account.as_deref(), to, &subject, &body)
            .await
            .map_err(|e| EmailError::Send(format!("email delivery failed: {e:#}")))?;
        if let Err(e) = store.record_loop_delivery(&token, &l.id, &from, to, &subject) {
            warn!(loop_id = %l.id, "loop delivery record failed: {e:#}");
        }
        store
            .record_loop_finding(&l.id, request_id, finding)
            .map_err(|e| EmailError::Send(format!("coverage record failed after send: {e:#}")))?;
        sent.push(subject);
    }
    Ok(sent)
}

/// What the loop's channel sees after an email delivery: a count and the
/// subjects, never the bodies.
pub(super) fn receipt(to: &str, subjects: &[String]) -> String {
    let mut out = format!(
        "Emailed {} new finding{} to {to}:",
        subjects.len(),
        if subjects.len() == 1 { "" } else { "s" }
    );
    for subject in subjects.iter().take(20) {
        out.push_str("\n- ");
        out.push_str(&truncate(subject, 100));
    }
    if subjects.len() > 20 {
        out.push_str(&format!("\n- and {} more", subjects.len() - 20));
    }
    out
}
