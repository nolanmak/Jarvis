//! Provider conformance at real channel extraction/composition boundaries.
use augmentagent_channel_core::{reasoner::{ClaudeCliReasoner, Reasoner, ReasonerOpts}, codex::CodexCliReasoner};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

struct Observed {
    inner: Arc<dyn Reasoner>,
    successes: AtomicUsize,
}
#[async_trait::async_trait]
impl Reasoner for Observed {
    async fn call(&self, opts: &ReasonerOpts, user: &str) -> anyhow::Result<String> {
        let result = self.inner.call(opts, user).await?;
        assert!(!result.trim().is_empty(), "empty output cannot count as conformance");
        self.successes.fetch_add(1, Ordering::SeqCst);
        Ok(result)
    }
}

async fn contracts(provider: Arc<dyn Reasoner>) {
    augmentagent_channel_core::state_dir::isolate_for_tests(); // #1048
    let observed = Arc::new(Observed { inner: provider, successes: AtomicUsize::new(0) });
    let signature = augmentagent_channel_email::sigextract::SignatureExtractor::new(observed.as_ref())
        .extract("Fixture Author\nTest Engineer\nExample Organization\nhttps://example.com").await.unwrap();
    assert!(signature.title.as_deref().or(signature.role.as_deref()).is_some_and(|value| value.contains("Engineer")), "{signature:?}");
    assert_eq!(signature.company.as_deref(), Some("Example Organization"));
    assert!(signature.phones.is_empty());
    assert!(signature.address.is_none());

    let memo = augmentagent_channel_voice::extract::extract(&observed,
        "I will finish the synthetic parser by Friday. My teammate Fixture Colleague will review it.").await;
    assert!(!memo.title.is_empty());
    assert!(memo.people.iter().any(|person| person.contains("Fixture Colleague")), "{memo:?}");
    assert!(memo.commitments.iter().any(|commitment| commitment.to_lowercase().contains("parser")), "{memo:?}");

    use augmentagent_channel_journal::compose::{compose_opts, compose_user_message, parse_composed_entry};
    let journal = observed.call(&compose_opts(), &compose_user_message(
        "Assistant: Would a beach holiday help?\nUser: I finished the synthetic parser today. I feel relieved.")).await.unwrap();
    assert!(journal.starts_with("TITLE:"), "{journal}");
    let (title, body) = parse_composed_entry(&journal);
    assert!(title.is_some_and(|title| title.split_whitespace().count() <= 8));
    assert!(body.starts_with("<p>") && body.contains("parser") && body.to_lowercase().contains("relieved"), "{body}");
    assert!(!body.to_lowercase().contains("beach"), "must not journal the assistant's suggestion");

    use augmentagent_content_adapter::{fan_out, Platform, SourceDraft};
    let source = SourceDraft::new("I built a synthetic parser in Rust. It reads test fixtures and reports invalid input. Small regression tests helped me preserve working cases while adding new ones. I kept the examples synthetic and documented the expected behavior. Next I will improve the error messages and add clearer examples for the team. The parser is an internal learning project, not a released product.");
    let platforms = [Platform::Twitter, Platform::Linkedin, Platform::Instagram];
    let before = observed.successes.load(Ordering::SeqCst);
    let variants = fan_out(&observed, &source, &platforms).await;
    assert_eq!(observed.successes.load(Ordering::SeqCst) - before, 3,
        "a raw-source fallback cannot stand in for successful adaptation");
    assert_eq!(variants.len(), platforms.len());
    for (variant, expected) in variants.iter().zip(platforms) {
        assert_eq!(variant.platform, expected);
        assert!(!variant.posts.is_empty() && variant.posts.iter().all(|post| !post.is_empty()));
        assert!(!variant.over_limit, "{variant:?}");
        assert!(variant.posts.join(" ").to_lowercase().contains("parser"), "{variant:?}");
    }
}

#[tokio::test]
#[ignore = "requires Codex login; synthetic signature, voice, journal and social adaptation"]
async fn live_codex_channel_formats() { contracts(Arc::new(CodexCliReasoner::openai())).await; }

#[tokio::test]
#[ignore = "requires Claude login; same synthetic channel formats as Codex"]
async fn live_claude_channel_formats() { contracts(Arc::new(ClaudeCliReasoner::new())).await; }
