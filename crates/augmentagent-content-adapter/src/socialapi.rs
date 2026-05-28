//! SocialAPI.ai cross-post fan-out (#241).
//!
//! One source draft → per-account adapted variants → ONE approval → on
//! approve, N scheduled-post rows (one per connected SocialAPI.ai account).
//!
//! This is the *outbound* counterpart to [`crate::adapter::fan_out`]. Where
//! `fan_out` takes a fixed list of [`Platform`]s, here the targets are
//! *connected SocialAPI.ai accounts* — each carries its own account id plus a
//! sub-platform string (`"instagram"`, `"x"`, `"linkedin"`, …). We map each
//! sub-platform onto the adapter's text-shape [`Platform`] so the existing
//! per-platform prompts / char limits / media specs are reused verbatim.
//!
//! ## Why dedupe the model calls
//!
//! Two Instagram brands behind one SocialAPI.ai key both want the *same*
//! Instagram-shaped caption — the adaptation depends on the platform, not on
//! which account ultimately posts it. So we run [`fan_out`] once per *distinct*
//! [`Platform`] across the target set and clone the resulting
//! [`PlatformVariant`] onto every account on that platform. N accounts on K
//! distinct platforms ⇒ K reasoner calls, not N.
//!
//! Accounts whose sub-platform the text adapter can't render
//! ([`Platform::from_socialapi_sub`] → `None`) are still returned — with a
//! verbatim source-body variant — so nothing is silently dropped before
//! approval.

use std::collections::BTreeSet;
use std::sync::Arc;

use augmentagent_channel_core::reasoner::Reasoner;

use crate::adapter::fan_out;
use crate::types::{Platform, PlatformVariant, SourceDraft};

/// One connected SocialAPI.ai account selected as a cross-post destination.
/// Mirrors the fields the CLI pulls from `store.list_socialapi_accounts()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocialTarget {
    /// SocialAPI.ai account id — goes into `scheduled_posts.socialapi_account_id`.
    pub account_id: String,
    /// Sub-platform string off the connected account (`"instagram"`, `"x"`,
    /// `"linkedin"`, …). Encoded into the row's `platform` as
    /// `"socialapi:<sub>"` by the caller.
    pub sub_platform: String,
    /// Human label for the approval card (display name / handle). Optional.
    pub label: Option<String>,
}

impl SocialTarget {
    pub fn new(account_id: impl Into<String>, sub_platform: impl Into<String>) -> Self {
        Self {
            account_id: account_id.into(),
            sub_platform: sub_platform.into(),
            label: None,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The text-shape platform this account maps onto, if any.
    pub fn platform(&self) -> Option<Platform> {
        Platform::from_socialapi_sub(&self.sub_platform)
    }
}

/// One target paired with the variant it will post. `variant` is the
/// adapted, approval-ready content; `target` carries the destination
/// account + sub-platform the caller turns into a scheduled-post row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetVariant {
    pub target: SocialTarget,
    pub variant: PlatformVariant,
}

/// Fan `src` out across a set of connected SocialAPI.ai `targets`.
///
/// Runs [`fan_out`] once per *distinct* renderable [`Platform`] in the target
/// set (concurrently), then clones each platform's variant onto every account
/// on that platform. Targets whose sub-platform can't be mapped get a
/// verbatim source-body variant rendered for [`Platform::Instagram`]'s shape
/// as a neutral fallback (no model call, never silently dropped).
///
/// The returned vec is in `targets` order so the caller's per-account row
/// enqueue is deterministic.
pub async fn fan_out_socialapi<R: Reasoner + ?Sized>(
    reasoner: &Arc<R>,
    src: &SourceDraft,
    targets: &[SocialTarget],
) -> Vec<TargetVariant> {
    // Distinct renderable platforms, in a stable order.
    let platforms: Vec<Platform> = {
        let mut set: BTreeSet<&'static str> = BTreeSet::new();
        let mut ordered: Vec<Platform> = Vec::new();
        for t in targets {
            if let Some(p) = t.platform() {
                if set.insert(p.as_str()) {
                    ordered.push(p);
                }
            }
        }
        ordered
    };

    // One model call per distinct platform; index back by platform string.
    let variants = fan_out(reasoner, src, &platforms).await;

    let mut out: Vec<TargetVariant> = Vec::with_capacity(targets.len());
    for t in targets {
        let variant = match t.platform() {
            Some(p) => variants
                .iter()
                .find(|v| v.platform == p)
                .cloned()
                // Defensive: a platform we collected above must be present,
                // but never panic on a fan-out shape mismatch.
                .unwrap_or_else(|| {
                    PlatformVariant::new(p, vec![src.body.trim().to_string()], None)
                }),
            // Unmappable sub-platform: verbatim source body. Instagram's
            // shape is the most permissive default (long char limit,
            // single post) and is only used for limit-flagging on the card.
            None => PlatformVariant::new(
                Platform::Instagram,
                vec![src.body.trim().to_string()],
                None,
            ),
        };
        out.push(TargetVariant {
            target: t.clone(),
            variant,
        });
    }
    out
}

/// One combined approval-card body for the whole cross-post family. Each
/// account gets its own labelled section reusing [`crate::variant_card`], so
/// the operator sees every per-account variant but approves the family with a
/// single decision (the caller posts this as ONE card).
pub fn family_card(items: &[TargetVariant]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "**Cross-post fan-out — {} account(s)**\n",
        items.len()
    ));
    for (i, it) in items.iter().enumerate() {
        let label = it
            .target
            .label
            .clone()
            .unwrap_or_else(|| it.target.account_id.clone());
        out.push_str(&format!(
            "\n__{}/{} — {} ({})__\n",
            i + 1,
            items.len(),
            label,
            it.target.sub_platform
        ));
        out.push_str(&crate::preview::variant_card(&it.variant));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_channel_core::reasoner::ReasonerOpts;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct PlatReasoner {
        by_marker: HashMap<&'static str, String>,
        calls: Mutex<usize>,
    }
    #[async_trait]
    impl Reasoner for PlatReasoner {
        async fn call(&self, opts: &ReasonerOpts, _u: &str) -> anyhow::Result<String> {
            *self.calls.lock().unwrap() += 1;
            for (marker, reply) in &self.by_marker {
                if opts.system_prompt.contains(*marker) {
                    return Ok(reply.clone());
                }
            }
            anyhow::bail!("no canned reply")
        }
    }
    fn reasoner(map: &[(&'static str, &str)]) -> Arc<PlatReasoner> {
        Arc::new(PlatReasoner {
            by_marker: map.iter().map(|(k, v)| (*k, v.to_string())).collect(),
            calls: Mutex::new(0),
        })
    }

    #[test]
    fn sub_platform_maps_via_alias_table() {
        assert_eq!(
            SocialTarget::new("a", "x").platform(),
            Some(Platform::Twitter)
        );
        assert_eq!(
            SocialTarget::new("a", "Instagram").platform(),
            Some(Platform::Instagram)
        );
        assert_eq!(SocialTarget::new("a", "tiktok").platform(), None);
    }

    #[tokio::test]
    async fn one_model_call_per_distinct_platform_not_per_account() {
        // Two instagram accounts + one X account ⇒ 2 distinct platforms ⇒
        // 2 reasoner calls, but 3 returned target-variants.
        let r = reasoner(&[
            ("Platform: Instagram", "a vibey caption"),
            ("Platform: X / Twitter", "punchy tweet"),
        ]);
        let targets = vec![
            SocialTarget::new("ig_1", "instagram"),
            SocialTarget::new("ig_2", "instagram"),
            SocialTarget::new("x_1", "x"),
        ];
        let out = fan_out_socialapi(&r, &SourceDraft::new("we shipped"), &targets).await;
        assert_eq!(out.len(), 3);
        assert_eq!(*r.calls.lock().unwrap(), 2);
        // Both IG accounts share the same caption.
        assert_eq!(out[0].variant.posts, vec!["a vibey caption"]);
        assert_eq!(out[1].variant.posts, vec!["a vibey caption"]);
        assert_eq!(out[0].variant.platform, Platform::Instagram);
        // X account got the tweet shape.
        assert_eq!(out[2].variant.posts, vec!["punchy tweet"]);
        assert_eq!(out[2].variant.platform, Platform::Twitter);
        // Account ids and order preserved.
        assert_eq!(out[0].target.account_id, "ig_1");
        assert_eq!(out[2].target.account_id, "x_1");
    }

    #[tokio::test]
    async fn unmappable_sub_platform_falls_back_to_source_body() {
        let r = reasoner(&[("Platform: X / Twitter", "tweet")]);
        let targets = vec![SocialTarget::new("tt_1", "tiktok")];
        let out = fan_out_socialapi(&r, &SourceDraft::new("the original"), &targets).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].variant.posts, vec!["the original"]);
        // No platform was renderable ⇒ no model call.
        assert_eq!(*r.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn family_card_has_one_section_per_account() {
        let r = reasoner(&[
            ("Platform: Instagram", "cap"),
            ("Platform: LinkedIn", "a post"),
        ]);
        let targets = vec![
            SocialTarget::new("ig_1", "instagram").with_label("Brand IG"),
            SocialTarget::new("li_1", "linkedin").with_label("Brand LI"),
        ];
        let out = fan_out_socialapi(&r, &SourceDraft::new("news"), &targets).await;
        let card = family_card(&out);
        assert!(card.contains("Cross-post fan-out — 2 account(s)"));
        assert!(card.contains("1/2 — Brand IG (instagram)"));
        assert!(card.contains("2/2 — Brand LI (linkedin)"));
        assert!(card.contains("cap"));
        assert!(card.contains("a post"));
    }
}
