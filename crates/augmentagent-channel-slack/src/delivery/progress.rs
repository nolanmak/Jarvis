//! Throttled turn progress: one status message, edited with `chat.update`
//! at most once per [`ProgressConfig::min_interval`].
//!
//! [`ProgressMessage::set`] never blocks and never calls Slack itself; it
//! replaces the pending text. A per-message task sends the newest text as
//! soon as the interval allows, so a burst of updates becomes one edit and
//! the latest always wins. Identical text is not re-sent. A rate-limited
//! edit waits `max(interval, Retry-After)` and retries the newest text; after
//! [`ProgressConfig::max_consecutive_failures`] failures that text is dropped
//! (progress is best effort; the answer itself goes through the outbox).
//!
//! `chat.update` is **[docs]** Tier 3 ("50+ per minute", per method per
//! workspace), so the default interval of 3 s per message leaves room for
//! several concurrent turns. Progress edits are not written to the outbox:
//! a lost edit is superseded by the next one.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::{markdown_to_mrkdwn, SLACK_TEXT_LIMIT};
use crate::transport::web::{PostMessage, SlackWebApi, UpdateMessage, WebApiError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressConfig {
    /// At most one edit per message per interval.
    pub min_interval: Duration,
    /// Give up on a text after this many failed edits in a row.
    pub max_consecutive_failures: u32,
}

impl Default for ProgressConfig {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(3),
            max_consecutive_failures: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressReport {
    pub sent: usize,
    pub failed: usize,
}

/// A status message being kept up to date.
pub struct ProgressMessage {
    channel: String,
    ts: String,
    tx: watch::Sender<Option<String>>,
    task: JoinHandle<ProgressReport>,
}

/// Model Markdown → mrkdwn, kept within one message (status lines are
/// short; a runaway one is cut with an ellipsis).
fn status_text(markdown: &str) -> String {
    let text = markdown_to_mrkdwn(markdown);
    if text.chars().count() < SLACK_TEXT_LIMIT {
        return text;
    }
    let mut cut: String = text.chars().take(SLACK_TEXT_LIMIT - 1).collect();
    // Do not leave half an entity behind.
    if let Some(amp) = cut.rfind('&') {
        if !cut[amp..].contains(';') {
            cut.truncate(amp);
        }
    }
    cut.push('…');
    cut
}

impl ProgressMessage {
    /// Edit an existing message `ts` in `channel`.
    pub fn start(
        api: Arc<dyn SlackWebApi>,
        channel: &str,
        ts: &str,
        config: ProgressConfig,
        cancel: CancellationToken,
    ) -> Self {
        Self::spawn(api, channel, ts, config, cancel, None)
    }

    /// Post the status message (in `thread_ts` when given), then keep it
    /// updated. The post itself is not throttled.
    pub async fn post(
        api: Arc<dyn SlackWebApi>,
        channel: &str,
        thread_ts: Option<&str>,
        initial_markdown: &str,
        config: ProgressConfig,
        cancel: CancellationToken,
    ) -> Result<Self, WebApiError> {
        let text = status_text(initial_markdown);
        let posted = api
            .post_message(PostMessage {
                channel: channel.to_string(),
                text: text.clone(),
                thread_ts: thread_ts.map(str::to_string),
                link_names: Some(false),
                ..PostMessage::default()
            })
            .await?;
        Ok(Self::spawn(
            api,
            &posted.channel,
            &posted.ts,
            config,
            cancel,
            Some(text),
        ))
    }

    fn spawn(
        api: Arc<dyn SlackWebApi>,
        channel: &str,
        ts: &str,
        config: ProgressConfig,
        cancel: CancellationToken,
        shown: Option<String>,
    ) -> Self {
        let (tx, rx) = watch::channel(None);
        let task = tokio::spawn(run(
            api,
            channel.to_string(),
            ts.to_string(),
            config,
            rx,
            cancel,
            shown,
        ));
        Self {
            channel: channel.to_string(),
            ts: ts.to_string(),
            tx,
            task,
        }
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn message_ts(&self) -> &str {
        &self.ts
    }

    /// Replace the pending status text (Markdown). Returns immediately.
    pub fn set(&self, markdown: impl AsRef<str>) {
        self.tx.send_replace(Some(status_text(markdown.as_ref())));
    }

    /// Send `final_markdown` (still respecting the interval), flush, and
    /// stop. With `None`, only a pending edit is flushed.
    pub async fn finish(self, final_markdown: Option<String>) -> ProgressReport {
        if let Some(text) = final_markdown {
            self.set(text);
        }
        drop(self.tx);
        self.task.await.unwrap_or_default()
    }
}

async fn run(
    api: Arc<dyn SlackWebApi>,
    channel: String,
    ts: String,
    config: ProgressConfig,
    mut rx: watch::Receiver<Option<String>>,
    cancel: CancellationToken,
    mut shown: Option<String>,
) -> ProgressReport {
    let mut report = ProgressReport::default();
    let mut next_allowed = Instant::now();
    let mut pending: Option<String> = None;
    let mut failures = 0u32;
    loop {
        if pending.is_none() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    pending = rx.borrow_and_update().clone();
                }
            }
            if pending.is_none() || pending == shown {
                pending = None;
                continue;
            }
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep_until(next_allowed) => {}
        }
        // Whatever was set while waiting supersedes the pending text.
        if let Some(latest) = rx.borrow_and_update().clone() {
            pending = Some(latest);
        }
        let Some(text) = pending.clone() else {
            continue;
        };
        if shown.as_ref() == Some(&text) {
            pending = None;
            continue;
        }
        let request = UpdateMessage {
            channel: channel.clone(),
            ts: ts.clone(),
            text: text.clone(),
            blocks: None,
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            r = api.update_message(request) => r,
        };
        let now = Instant::now();
        match result {
            Ok(_) => {
                report.sent += 1;
                shown = Some(text);
                pending = None;
                failures = 0;
                next_allowed = now + config.min_interval;
            }
            Err(WebApiError::Cancelled) => break,
            Err(err) => {
                report.failed += 1;
                failures += 1;
                let wait = match &err {
                    WebApiError::RateLimited { retry_after } => {
                        (*retry_after).max(config.min_interval)
                    }
                    _ => config.min_interval,
                };
                next_allowed = now + wait;
                warn!(error = %err, "slack progress edit failed");
                if failures >= config.max_consecutive_failures.max(1) {
                    pending = None;
                    failures = 0;
                }
            }
        }
    }
    report
}
