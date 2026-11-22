//! Explicit opt-in, paid CLI compatibility tests. Only synthetic sessions.
use augmentagent_channel_core::{ClaudeCliReasoner, Reasoner, ReasonerOpts};
use augmentagent_channel_core::codex::CodexCliReasoner;
use augmentagent_channel_core::native_session::{NativeSession, CURRENT};
use augmentagent_channel_core::providers::{ModelTier, ProviderKind};

async fn exercise(provider: ProviderKind) {
    let directory = tempfile::tempdir().unwrap();
    let session = NativeSession::new(provider).unwrap();
    let mut opts = ReasonerOpts::pinned(ModelTier::Fast, "Answer with the exact requested marker. Do not use tools.");
    opts.cwd = Some(directory.path().to_path_buf());
    opts.allowed_tools.clear();
    let reasoner: Box<dyn Reasoner> = match provider {
        ProviderKind::Claude => Box::new(ClaudeCliReasoner::new()),
        ProviderKind::Codex => Box::new(CodexCliReasoner::openai()),
        _ => unreachable!(),
    };
    let first = CURRENT.scope(session.clone(), reasoner.call(&opts,
        "Reply with exactly SYNTHETIC_NATIVE_VOICE_ALPHA_512.")).await.unwrap();
    assert!(first.contains("SYNTHETIC_NATIVE_VOICE_ALPHA_512"), "first turn: {first}");
    let id = session.id().expect("native session ID after first turn");
    let second = CURRENT.scope(session.clone(), reasoner.call(&opts,
        "Reply with exactly SYNTHETIC_NATIVE_VOICE_BETA_513.")).await.unwrap();
    assert!(second.contains("SYNTHETIC_NATIVE_VOICE_BETA_513"), "second turn: {second}");
    assert_eq!(session.id().as_deref(), Some(id.as_str()));
}

#[tokio::test]
#[ignore = "paid native CLI probe; run explicitly against disposable sessions"]
async fn codex_scoped_adapter_preserves_native_session() {
    exercise(ProviderKind::Codex).await;
}

#[tokio::test]
#[ignore = "paid native CLI probe; run explicitly against disposable sessions"]
async fn claude_scoped_adapter_preserves_native_session() {
    exercise(ProviderKind::Claude).await;
}
