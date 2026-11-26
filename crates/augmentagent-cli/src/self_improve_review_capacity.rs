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

/// #2385: structured reviews by one provider that failed validation on the
/// same head before the alternate native reviewer is tried.
pub(super) const STRUCTURED_FAILURES_BEFORE_FALLBACK: u32 = 2;

/// #2385: after repeated structured-validation failures on one head, prefer
/// the configured native alternate with capacity and fewer failures. None
/// keeps the selected provider. Native session isolation is still proven per
/// call, so a builder brand may serve from a fresh session as elsewhere.
pub(super) fn structured_fallback(
    pool: &[ProviderKind],
    selected: ProviderKind,
    failures: impl Fn(ProviderKind) -> u32,
    status: impl Fn(ProviderKind) -> ReviewerStatus,
) -> Option<ProviderKind> {
    use ProviderKind::{Claude, Codex};
    let failed = failures(selected);
    if failed < STRUCTURED_FAILURES_BEFORE_FALLBACK {
        return None;
    }
    pool.iter()
        .copied()
        .filter(|p| *p != selected && matches!(p, Claude | Codex))
        .filter(|p| failures(*p) < failed)
        .filter(|p| matches!(status(*p), ReviewerStatus::Ready))
        .min_by_key(|p| failures(*p))
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use ProviderKind::{Cerebras, Claude, Codex};

    /// #2385 production shape: AUGMENTAGENT_AUTOPR_REVIEWERS=codex,cerebras,claude,
    /// cerebras unconfigured, codex failing structured validation on PRs
    /// #2372/#2378 every time. After the bound, claude reviews instead.
    #[test]
    fn repeated_structured_failures_fall_back_to_the_alternate_native_reviewer() {
        let pool = [Codex, Cerebras, Claude];
        let ready = |p: ProviderKind| match p {
            Cerebras => ReviewerStatus::NotConfigured("no key".into()),
            _ => ReviewerStatus::Ready,
        };
        let counts = |codex: u32, claude: u32| move |p: ProviderKind| match p { Codex => codex, Claude => claude, _ => 0 };
        assert_eq!(structured_fallback(&pool, Codex, counts(1, 0), ready), None, "one failure is not yet a pattern");
        assert_eq!(structured_fallback(&pool, Codex, counts(2, 0), ready), Some(Claude));
        assert_eq!(structured_fallback(&pool, Codex, counts(4, 2), ready), Some(Claude), "fewer failures wins");
        assert_eq!(structured_fallback(&pool, Codex, counts(2, 3), ready), None, "never trade for a worse record");
        let claude_latched = |p: ProviderKind| match p {
            Claude => ReviewerStatus::Latched(None),
            other => ready(other),
        };
        assert_eq!(structured_fallback(&pool, Codex, counts(5, 0), claude_latched), None, "no capacity, no fallback");
        assert_eq!(structured_fallback(&[Codex, Cerebras], Codex, counts(5, 0), ready), None, "a non-native or absent alternate is not used");
        assert_eq!(structured_fallback(&pool, Claude, counts(0, 2), ready), Some(Codex), "the fallback is symmetric");
    }
}
