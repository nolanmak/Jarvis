//! Live read-only smoke test against the real SocialAPI.ai API (#543).
//!
//! Ignored by default — it needs a real key and network. Run explicitly:
//!
//! ```sh
//! SOCIALAPI_API_KEY=... cargo test -p augmentagent-channel-socialapi \
//!     --test live_api -- --ignored --nocapture
//! ```
//!
//! Exists because every original response model in this crate was written
//! before anyone held a real key, compiled clean, passed its wiremock tests,
//! and then failed to decode a single live response. Wiremock can only prove
//! we parse what we *believe* the API returns; this test proves it against
//! what the API *actually* returns. Reads only — it never posts, replies,
//! or spends an interaction credit, and the DM-source pass writes its seen
//! ledger to a throwaway temp store.

use std::sync::Arc;

use augmentagent_channel_socialapi::{SocialApiAuth, SocialApiClient, SocialApiDmSource};
use augmentagent_store::Store;

#[tokio::test]
#[ignore = "hits the live SocialAPI.ai API; needs SOCIALAPI_API_KEY"]
async fn live_read_paths_decode() {
    let auth = SocialApiAuth::load().expect("SOCIALAPI_API_KEY must be set for this test");
    let client = SocialApiClient::new(auth);

    // Accounts: envelope + name/username fields.
    let accounts = client.list_accounts().await.expect("list_accounts");
    println!("accounts: {}", accounts.len());
    assert!(!accounts.is_empty(), "expected at least one connected account");
    for a in &accounts {
        println!("  {} {} name={:?} username={:?} status={}", a.id, a.platform, a.name, a.username, a.status);
        assert!(!a.id.is_empty());
        assert!(!a.platform.is_empty());
    }

    // Comment inbox is two-level (#543): posts first, then per-post comments.
    let posts = client.list_inbox_posts(None).await.expect("list_inbox_posts");
    println!("inbox posts: {}", posts.len());
    let mut comments_seen = 0usize;
    for p in posts.iter().take(5) {
        let comments = client
            .list_comments(&p.id, Some(&p.account_id))
            .await
            .expect("list_comments");
        for c in &comments {
            println!(
                "  comment on {}: {} by {} (is_owner={})",
                p.id,
                c.text.chars().take(30).collect::<String>(),
                c.author_display(),
                c.is_owner
            );
            assert!(!c.platform_id.is_empty());
        }
        comments_seen += comments.len();
    }
    println!("comments across first 5 posts: {comments_seen}");

    // Conversations + per-thread messages (the two-endpoint flow).
    let convs = client.list_conversations(None).await.expect("list_conversations");
    println!("conversations: {}", convs.len());
    if let Some(conv) = convs.first() {
        assert!(!conv.id.is_empty());
        let msgs = client.list_messages(&conv.id).await.expect("list_messages");
        println!(
            "messages in {} (with {}): {}",
            conv.id,
            conv.participant_name,
            msgs.len()
        );
        // Direction must be provider-stated on real messages — it's the
        // ownership signal the DM poller now relies on.
        assert!(
            msgs.iter().all(|m| m.is_incoming() || m.is_outgoing()),
            "every live message should state a direction"
        );
    }

    // Full DM source pass against a throwaway store: exercises envelope
    // decode, the messages fan-out, tail/horizon selection, and the seen
    // ledger — everything short of triage/draft (no LLM, no sends).
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(Store::open(tmp.path()).unwrap());
    let source = SocialApiDmSource::new(Arc::new(client), store, 25);
    use augmentagent_channel_core::trigger::InboundSource;
    let items = source.fetch_new().await.expect("dm source fetch_new");
    println!("dm work items (fresh unanswered, within horizon): {}", items.len());
    for it in &items {
        println!("  {} {}", it.external_id, it.payload["with"].as_str().unwrap_or("?"));
    }
}
