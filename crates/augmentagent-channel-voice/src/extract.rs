//! Structure a raw transcript into a small JSON record via a Haiku call.
//!
//! One retry on parse failure, then a raw-text fallback so a flaky model
//! never drops a memo on the floor — the transcript still reaches the wiki,
//! just unstructured.

use std::sync::Arc;

use augmentagent_channel_core::reasoner::{ReasonerOpts, Reasoner};
use serde::{Deserialize, Serialize};
use tracing::warn;

const SYSTEM_PROMPT: &str = include_str!("../prompts/extract.md");

/// Structured form of a voice memo. Every field is optional/defaulted so the
/// raw fallback (all-empty but `summary` = transcript) is always valid.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoRecord {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub people: Vec<String>,
    #[serde(default)]
    pub commitments: Vec<String>,
    #[serde(default)]
    pub topics: Vec<String>,
}

impl MemoRecord {
    /// Fallback record when structuring fails: keep the transcript verbatim
    /// as the summary so nothing is lost.
    fn raw(transcript: &str) -> Self {
        Self {
            title: "Voice memo".into(),
            summary: transcript.trim().to_string(),
            ..Default::default()
        }
    }
}

fn opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: SYSTEM_PROMPT.to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
        settings_json: None,
    }
}

fn parse_blob(s: &str) -> Option<MemoRecord> {
    // Tolerate fences / prose around the object.
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<MemoRecord>(&s[start..=end]).ok()
}

/// Extract a [`MemoRecord`] from a transcript. Never errors — worst case is
/// the raw fallback.
pub async fn extract<R: Reasoner + ?Sized>(
    reasoner: &Arc<R>,
    transcript: &str,
) -> MemoRecord {
    if transcript.trim().is_empty() {
        return MemoRecord::default();
    }
    let opts = opts();
    let user = format!("<transcript>\n{}\n</transcript>", transcript.trim());

    for attempt in 0..2 {
        match reasoner.call(&opts, &user).await {
            Ok(reply) => {
                if let Some(rec) = parse_blob(&reply) {
                    return rec;
                }
                warn!(attempt, "voice extract: unparseable model reply");
            }
            Err(e) => warn!(attempt, "voice extract call failed: {e:#}"),
        }
    }
    warn!("voice extract: falling back to raw transcript");
    MemoRecord::raw(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct ScriptedReasoner {
        replies: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(
            &self,
            _opts: &ReasonerOpts,
            _user: &str,
        ) -> anyhow::Result<String> {
            let mut g = self.replies.lock().unwrap();
            if g.is_empty() {
                anyhow::bail!("no scripted reply");
            }
            Ok(g.remove(0))
        }
    }

    fn reasoner(replies: Vec<&str>) -> Arc<ScriptedReasoner> {
        Arc::new(ScriptedReasoner {
            replies: Mutex::new(replies.into_iter().map(String::from).collect()),
        })
    }

    #[tokio::test]
    async fn parses_clean_json() {
        let r = reasoner(vec![
            r#"{"title":"Call Sam","summary":"Need to call Sam re deck","people":["Sam"],"commitments":["call Sam"],"topics":["deck"]}"#,
        ]);
        let rec = extract(&r, "remember to call sam about the deck").await;
        assert_eq!(rec.title, "Call Sam");
        assert_eq!(rec.people, vec!["Sam"]);
        assert_eq!(rec.commitments, vec!["call Sam"]);
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let r = reasoner(vec![
            "not json at all",
            r#"prose {"title":"X","summary":"y"} trailing"#,
        ]);
        let rec = extract(&r, "blah").await;
        assert_eq!(rec.title, "X");
        assert_eq!(rec.summary, "y");
    }

    #[tokio::test]
    async fn raw_fallback_keeps_transcript() {
        let r = reasoner(vec!["garbage", "still garbage"]);
        let rec = extract(&r, "  the actual words  ").await;
        assert_eq!(rec.summary, "the actual words");
        assert_eq!(rec.title, "Voice memo");
    }

    #[tokio::test]
    async fn empty_transcript_is_empty_record() {
        let r = reasoner(vec![]);
        let rec = extract(&r, "   ").await;
        assert_eq!(rec, MemoRecord::default());
    }
}
