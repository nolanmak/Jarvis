//! Per-conversation turn serialization shared by text and voice input.
use std::future::Future;
use std::collections::VecDeque;
use std::sync::{Arc, atomic::{AtomicU64, AtomicUsize, Ordering}};

use dashmap::DashMap;
use tokio::sync::{Mutex, OnceCell};

type TurnResult = Result<String, String>;
const MAX_CACHED_TURNS: usize = 128;
const MAX_CACHED_CONVERSATIONS: usize = 1024;

struct Queue {
    writer: Mutex<()>,
    pending: AtomicUsize,
    turns: DashMap<String, Arc<OnceCell<TurnResult>>>,
    finished: Mutex<VecDeque<String>>,
    last_used: AtomicU64,
}

impl Queue {
    fn new() -> Self {
        Self { writer: Mutex::new(()), pending: AtomicUsize::new(0),
            turns: DashMap::new(), finished: Mutex::new(VecDeque::new()),
            last_used: AtomicU64::new(0) }
    }
}

/// Ten waiting turns per text conversation; duplicate IDs share one result.
/// The native-session lease is the second, independent one-writer guard.
pub struct ConversationScheduler {
    queues: DashMap<String, Arc<Queue>>,
    next_use: AtomicU64,
}

impl Default for ConversationScheduler {
    fn default() -> Self { Self::new() }
}

impl ConversationScheduler {
    pub fn new() -> Self {
        Self { queues: DashMap::new(), next_use: AtomicU64::new(1) }
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
        queue.last_used.store(self.next_use.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
        // A cancelled submit can leave an uninitialized cell behind. Keep
        // cells that another caller still holds, and discard orphaned ones.
        queue.turns.retain(|_, cell| cell.get().is_some() || Arc::strong_count(cell) > 1);
        let result = queue.turns.entry(turn_id.to_owned())
            .or_insert_with(|| Arc::new(OnceCell::new())).clone();
        let outcome = result.get_or_init(|| async {
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
        }).await.clone();
        if outcome.as_ref().err().is_some_and(|error| error == "Conversation queue is full (10 pending turns)") {
            queue.turns.remove_if(turn_id, |_, cached| Arc::ptr_eq(cached, &result));
            return outcome;
        }
        {
            let mut finished = queue.finished.lock().await;
            if !finished.iter().any(|id| id == turn_id) {
                finished.push_back(turn_id.to_owned());
                while finished.len() > MAX_CACHED_TURNS {
                    if let Some(old) = finished.pop_front() {
                        queue.turns.remove(&old);
                    }
                }
            }
        }
        self.prune_idle_queues();
        outcome
    }

    fn prune_idle_queues(&self) {
        if self.queues.len() <= MAX_CACHED_CONVERSATIONS { return; }
        let mut oldest: Vec<_> = self.queues.iter().map(|item| (
            item.value().last_used.load(Ordering::SeqCst), item.key().clone()
        )).collect();
        oldest.sort_unstable();
        for (_, key) in oldest {
            if self.queues.len() <= MAX_CACHED_CONVERSATIONS { break; }
            self.queues.remove_if(&key, |_, queue| {
                queue.pending.load(Ordering::SeqCst) == 0 && Arc::strong_count(queue) == 1
            });
        }
    }
}
