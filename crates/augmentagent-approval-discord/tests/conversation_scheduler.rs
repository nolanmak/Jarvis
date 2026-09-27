use augmentagent_approval_discord::conversation::ConversationScheduler;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use tokio::sync::Notify;

#[tokio::test]
async fn text_and_voice_share_one_writer_and_duplicate_turn_runs_once() {
    let scheduler = Arc::new(ConversationScheduler::new());
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for number in 0..10 {
        let scheduler = scheduler.clone();
        let active = active.clone();
        let max_active = max_active.clone();
        let calls = calls.clone();
        handles.push(tokio::spawn(async move {
            scheduler.submit("guild-1:text-1", &format!("turn-{number}"), || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let concurrent = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(concurrent, Ordering::SeqCst);
                tokio::task::yield_now().await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(format!("reply-{number}"))
            }).await.unwrap()
        }));
    }
    let mut replies = Vec::new();
    for handle in handles { replies.push(handle.await.unwrap()); }
    replies.sort();
    assert_eq!(replies.len(), 10);
    assert_eq!(max_active.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 10);
    let duplicate = scheduler.submit("guild-1:text-1", "turn-0", || async {
        panic!("duplicate must not re-run");
        #[allow(unreachable_code)] Ok(String::new())
    }).await.unwrap();
    assert_eq!(duplicate, "reply-0");
}

#[tokio::test]
async fn eleventh_pending_turn_is_rejected_without_silent_loss() {
    let scheduler = Arc::new(ConversationScheduler::new());
    let release = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    let first = {
        let scheduler = scheduler.clone();
        let release = release.clone();
        let started = started.clone();
        tokio::spawn(async move {
            scheduler.submit("guild-1:text-1", "active", || async {
                started.notify_one();
                release.notified().await;
                Ok("done".into())
            }).await
        })
    };
    started.notified().await;
    let mut pending = Vec::new();
    for number in 0..10 {
        let scheduler = scheduler.clone();
        pending.push(tokio::spawn(async move {
            scheduler.submit("guild-1:text-1", &format!("queued-{number}"), || async {
                Ok("done".into())
            }).await
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while scheduler.queue_depth("guild-1:text-1") < 10 {
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(scheduler.submit("guild-1:text-1", "overflow", || async {
        Ok("must not run".into())
    }).await.is_err());
    release.notify_one();
    assert!(first.await.unwrap().is_ok());
    for item in pending { assert!(item.await.unwrap().is_ok()); }
}
