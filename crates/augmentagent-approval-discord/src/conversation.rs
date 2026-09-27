//! Per-conversation turn serialization shared by text and voice input.
use std::future::Future;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

use dashmap::DashMap;
use tokio::sync::{Mutex, OnceCell};

type TurnResult = Result<String, String>;

struct Queue {
    writer: Mutex<()>,
    pending: AtomicUsize,
    turns: DashMap<String, Arc<OnceCell<TurnResult>>>,
}

impl Queue {
    fn new() -> Self {
        Self { writer: Mutex::new(()), pending: AtomicUsize::new(0), turns: DashMap::new() }
    }
}

/// Ten waiting turns per text conversation; duplicate IDs share one result.
/// The native-session lease is the second, independent one-writer guard.
pub struct ConversationScheduler {
    queues: DashMap<String, Arc<Queue>>,
}

impl Default for ConversationScheduler {
    fn default() -> Self { Self::new() }
}

impl ConversationScheduler {
    pub fn new() -> Self {
        Self { queues: DashMap::new() }
    }

    pub fn queue_depth(&self, conversation: &str) -> usize {
        self.queues.get(conversation)
            .map(|queue| queue.pending.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    pub async fn submit<F, Fut>(
        &self,
        conversation: &str,
        turn_id: &str,
        work: F,
    ) -> TurnResult
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<String>>,
    {
        if conversation.trim().is_empty() || turn_id.trim().is_empty() {
            return Err("Conversation and turn IDs are required".into());
        }
        let queue = self.queues.entry(conversation.to_owned())
            .or_insert_with(|| Arc::new(Queue::new())).clone();
        let result = queue.turns.entry(turn_id.to_owned())
            .or_insert_with(|| Arc::new(OnceCell::new())).clone();
        result.get_or_init(|| async {
            let admitted = queue.pending.fetch_update(
                Ordering::SeqCst,
                Ordering::SeqCst,
                |count| (count < 10).then_some(count + 1),
            );
            if admitted.is_err() {
                return Err("Conversation queue is full (10 pending turns)".into());
            }
            let _writer = queue.writer.lock().await;
            queue.pending.fetch_sub(1, Ordering::SeqCst);
            work().await.map_err(|error| error.to_string())
        }).await.clone()
    }
}
