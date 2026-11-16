//! Availability is distinct from writer provenance. Fresh-session admission
//! may reuse a provider brand; authority still proves exact native isolation.
use super::*;
use augmentagent_channel_core::ProviderKind;

pub(super) fn available(
    candidates: Vec<ProviderKind>,
    status: impl Fn(ProviderKind) -> ReviewerStatus,
) -> std::result::Result<ProviderKind, ReviewUnavailable> {
    let mut latched = Vec::new();
    let mut missing = Vec::new();
    for candidate in &candidates {
        match status(*candidate) {
            ReviewerStatus::Ready => return Ok(*candidate),
            ReviewerStatus::Latched(until) => latched.push((candidate.name().to_string(), until)),
            ReviewerStatus::NotConfigured(why) => {
                missing.push(format!("{}: {why}", candidate.name()))
            }
        }
    }
    if latched.is_empty() {
        Err(ReviewUnavailable::NoCapacity {
            reviewers: candidates,
            detail: missing.join("; "),
        })
    } else {
        Err(ReviewUnavailable::Latched { until: latched })
    }
}

pub(super) fn isolated(
    pool: &[ProviderKind],
    authors: std::result::Result<Option<Vec<ProviderKind>>, String>,
    status: impl Fn(ProviderKind) -> ReviewerStatus,
) -> std::result::Result<ProviderKind, ReviewUnavailable> {
    use ProviderKind::{Claude, Codex};
    let authors = match authors {
        Ok(Some(authors)) if !authors.is_empty() => authors,
        Ok(_) => return Err(ReviewUnavailable::ProvenanceUnknown),
        Err(error) => return Err(ReviewUnavailable::ProvenanceUnverifiable { error }),
    };
    let native: Vec<_> = pool
        .iter()
        .copied()
        .filter(|p| matches!(p, Claude | Codex))
        .collect();
    // Prefer a different builder family when it has capacity. If none does,
    // choose admitted isolated capacity without erasing any writer history.
    if let Ok(provider) = select_reviewer(&native, Ok(Some(authors)), &status) {
        return Ok(provider);
    }
    let mut preferred = Vec::new();
    if native.contains(&Codex) {
        preferred.push(Codex);
    }
    preferred.extend(native.into_iter().filter(|p| *p != Codex));
    available(preferred, status)
}
