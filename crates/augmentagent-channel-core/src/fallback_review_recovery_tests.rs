//! Exercise actual fallback dispatch and its protected provenance ledger.
use super::*;
struct Writer {
    root: std::path::PathBuf,
    provider: ProviderKind,
    fail: bool,
}
#[async_trait::async_trait]
impl Reasoner for Writer {
    async fn call(&self, _: &ReasonerOpts, _: &str) -> anyhow::Result<String> {
        std::fs::write(
            self.root.join(format!("{}.txt", self.provider.name())),
            "preserved writer edit",
        )?;
        if self.fail {
            return Err(ReasonerError::Timeout {
                provider: self.provider.name().into(),
                secs: 1,
            }
            .into());
        }
        Ok("finished by Claude".into())
    }
}
#[tokio::test]
async fn review_recovery_codex_to_claude_switch_retains_both_histories_after_restart() {
    const NAME:&str="fallback::review_recovery_tests::review_recovery_codex_to_claude_switch_retains_both_histories_after_restart";
    if std::env::var_os("REVIEW_SWITCH_CHILD").is_none() {
        let home = tempfile::tempdir().unwrap();
        let out = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", home.path().join("state"))
            .env("REVIEW_SWITCH_CHILD", "1")
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let output = std::process::Command::new("/usr/bin/git")
        .args(["init", "-q"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut reasoner = FallbackReasoner::for_tests(
        vec![
            (
                ProviderKind::Codex,
                Arc::new(Writer {
                    root: root.clone(),
                    provider: ProviderKind::Codex,
                    fail: true,
                }),
            ),
            (
                ProviderKind::Claude,
                Arc::new(Writer {
                    root: root.clone(),
                    provider: ProviderKind::Claude,
                    fail: false,
                }),
            ),
        ],
        CooldownLatch::at(root.join("cooldowns.json")),
    );
    reasoner.handoff_root = Some(root.join("private/handoffs"));
    reasoner
        .track_review_history(&root, "candidate", false)
        .unwrap();
    let mut opts = ReasonerOpts::pinned(crate::providers::ModelTier::Quality, "fixture");
    opts.allowed_tools = vec!["Write".into()];
    reasoner
        .call(&opts, "preserve both builders' edits")
        .await
        .unwrap();
    let authors = reasoner.review_authors().unwrap().unwrap();
    assert_eq!(authors.len(), 2);
    assert!(authors.contains(&ProviderKind::Codex) && authors.contains(&ProviderKind::Claude));
    assert!(root.join("codex.txt").exists() && root.join("claude.txt").exists());
    drop(reasoner);
    let mut restored =
        FallbackReasoner::for_tests(vec![], CooldownLatch::at(root.join("cooldowns.json")));
    restored.handoff_root = Some(root.join("private/handoffs"));
    restored
        .track_review_history(&root, "candidate", true)
        .unwrap();
    assert_eq!(restored.review_authors().unwrap().unwrap(), authors);
    // Native session isolation must override both writer context histories.
    use crate::native_session::{
        current_for, fresh_review_scope, FreshBuilderSessions, Launch, NativeSession,
        ObservedConversation, CURRENT,
    };
    let builders = FreshBuilderSessions::from_observed(&[
        ObservedConversation {
            provider: ProviderKind::Codex,
            id: "codex-writer".into(),
            uncertain: false,
        },
        ObservedConversation {
            provider: ProviderKind::Claude,
            id: "claude-writer".into(),
            uncertain: false,
        },
    ])
    .unwrap();
    let inherited =
        NativeSession::from_id(ProviderKind::Claude, Some("shared-chat-writer".into())).unwrap();
    CURRENT
        .scope(
            inherited,
            builders.scope(async {
                let (_, id) = fresh_review_scope(ProviderKind::Codex, async {
                    let session = current_for(ProviderKind::Codex)?.unwrap();
                    let mut lease = session.begin(ProviderKind::Codex)?;
                    assert!(
                        matches!(lease.launch(), Launch::Create { .. }),
                        "review resumed shared writer context"
                    );
                    lease.observe("fresh-independent-codex")?;
                    lease.finish()?;
                    Ok(())
                })
                .await
                .unwrap();
                assert_eq!(id, "fresh-independent-codex");
            }),
        )
        .await;
    let observed = builders.observed();
    assert_eq!(observed.len(), 2);
    assert!(
        observed.iter().any(|s| s.id == "codex-writer")
            && observed.iter().any(|s| s.id == "claude-writer")
    );
}
