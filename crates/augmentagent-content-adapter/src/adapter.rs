//! Parallel fan-out: one `SourceDraft` → `Vec<PlatformVariant>`, one Reasoner
//! call per target platform, all in flight at once.

use std::sync::Arc;

use augmentagent_channel_core::reasoner::{social_adapter_opts, Reasoner};
use tracing::warn;

use crate::media::{media_for, split_alt};
use crate::prompts::{system_prompt, user_message};
use crate::types::{Platform, PlatformVariant, SourceDraft};

/// Parse a platform reply into the post list. X may return a JSON array
/// (thread); everything else is a single post. We tolerate a stray code
/// fence and fall back to "treat the whole reply as one post".
fn parse_posts(platform: Platform, reply: &str) -> Vec<String> {
    let trimmed = reply.trim();
    if platform == Platform::Twitter {
        // Try a JSON array of strings first (thread).
        let candidate = strip_fence(trimmed);
        if candidate.starts_with('[') {
            if let Ok(v) = serde_json::from_str::<Vec<String>>(&candidate) {
                let cleaned: Vec<String> = v
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !cleaned.is_empty() {
                    return cleaned;
                }
            }
        }
    }
    vec![trimmed.to_string()]
}

fn strip_fence(s: &str) -> String {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // Drop an optional language tag line, then the trailing fence.
        let body = rest.splitn(2, '\n').nth(1).unwrap_or(rest);
        return body.trim_end_matches("```").trim().to_string();
    }
    t.to_string()
}

/// Adapt one platform. Never panics — a failed/empty model call yields a
/// variant carrying the source body verbatim so the user still gets
/// *something* to approve rather than a silent drop.
async fn adapt_one<R: Reasoner + ?Sized>(
    reasoner: &Arc<R>,
    platform: Platform,
    src: &SourceDraft,
) -> PlatformVariant {
    let opts = social_adapter_opts(system_prompt(platform));
    let user = user_message(platform, src);
    let reply = match reasoner.call(&opts, &user).await {
        Ok(r) if !r.trim().is_empty() => r,
        Ok(_) => {
            warn!(platform = platform.as_str(), "adapter: empty reply; using source");
            src.body.clone()
        }
        Err(e) => {
            warn!(platform = platform.as_str(), "adapter call failed: {e:#}; using source");
            src.body.clone()
        }
    };
    let (body, alt) = split_alt(&reply);
    let posts = parse_posts(platform, &body);
    let media = media_for(platform, src, alt.as_deref());
    PlatformVariant::new(platform, posts, media)
}

/// Fan `src` out across `platforms`, concurrently. Order of the returned
/// vec matches `platforms`.
pub async fn fan_out<R: Reasoner + ?Sized>(
    reasoner: &Arc<R>,
    src: &SourceDraft,
    platforms: &[Platform],
) -> Vec<PlatformVariant> {
    let futs = platforms
        .iter()
        .map(|p| adapt_one(reasoner, *p, src))
        .collect::<Vec<_>>();
    futures::future::join_all(futs).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_channel_core::reasoner::ReasonerOpts;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Returns a canned reply keyed by which platform section the system
    /// prompt contains.
    struct PlatReasoner {
        by_marker: HashMap<&'static str, String>,
        calls: Mutex<usize>,
    }
    #[async_trait]
    impl Reasoner for PlatReasoner {
        async fn call(
            &self,
            opts: &ReasonerOpts,
            _u: &str,
        ) -> anyhow::Result<String> {
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

    #[tokio::test]
    async fn fans_out_all_three_platforms_in_one_call() {
        let r = reasoner(&[
            ("Platform: X / Twitter", "punchy tweet"),
            ("Platform: LinkedIn", "a thoughtful linkedin post"),
            ("Platform: Instagram", "a vibey caption #ship"),
        ]);
        let src = SourceDraft::new("we shipped the thing");
        let out = fan_out(
            &r,
            &src,
            &[Platform::Twitter, Platform::Linkedin, Platform::Instagram],
        )
        .await;
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].platform, Platform::Twitter);
        assert_eq!(out[0].posts, vec!["punchy tweet"]);
        assert_eq!(out[1].posts, vec!["a thoughtful linkedin post"]);
        assert_eq!(*r.calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn twitter_thread_array_is_parsed() {
        let r = reasoner(&[(
            "Platform: X / Twitter",
            r#"["first beat","second beat","third beat"]"#,
        )]);
        let out = fan_out(&r, &SourceDraft::new("long idea"), &[Platform::Twitter]).await;
        assert!(out[0].is_thread());
        assert_eq!(out[0].posts.len(), 3);
        assert_eq!(out[0].posts[1], "second beat");
    }

    #[tokio::test]
    async fn failed_call_falls_back_to_source_body() {
        // No canned reply for instagram ⇒ call errors ⇒ source verbatim.
        let r = reasoner(&[("Platform: X / Twitter", "tweet")]);
        let out = fan_out(
            &r,
            &SourceDraft::new("the original"),
            &[Platform::Instagram],
        )
        .await;
        assert_eq!(out[0].posts, vec!["the original"]);
    }

    #[tokio::test]
    async fn alt_line_lifts_into_media_spec() {
        let r = reasoner(&[(
            "Platform: Instagram",
            "caption text\nALT: a photo of the launch",
        )]);
        let src = SourceDraft::new("launch").with_media_intent("launch photo");
        let out = fan_out(&r, &src, &[Platform::Instagram]).await;
        assert_eq!(out[0].posts, vec!["caption text"]);
        assert_eq!(
            out[0].media.as_ref().unwrap().alt_text,
            "a photo of the launch"
        );
    }
}
