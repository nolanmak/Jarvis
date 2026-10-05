//! Prominent owner alerts bypass the serial approval carousel.
use crate::ApprovalBroker;
use augmentagent_store::{
    alert_schedule::AlertNotice,
    owner_alerts::{AlertState, Urgency},
    Store,
};
use serenity::all::{
    ButtonStyle, CreateActionRow, CreateAllowedMentions, CreateButton, CreateEmbed, CreateMessage,
    Nonce, UserId,
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

fn clipped(text: &str, limit: usize) -> String {
    text.chars()
        .take(limit)
        .collect::<String>()
        .replace('@', "＠")
}

pub fn message(
    notice: &AlertNotice,
    owner: UserId,
    now: i64,
    text_status: Option<&str>,
) -> CreateMessage {
    let a = &notice.alert;
    let heading = if a.urgency == Urgency::Critical {
        "🚨 CRITICAL — action required"
    } else {
        "⚠️ Action required"
    };
    let subject = notice
        .details
        .as_ref()
        .map(|d| d.subject.as_str())
        .unwrap_or("Owner alert");
    let due = a
        .due_at_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|due| {
            let zone: chrono_tz::Tz = a.timezone.parse().unwrap_or(chrono_tz::UTC);
            format!(
                "{} · <t:{}:R>",
                due.with_timezone(&zone).format("%a %b %d, %H:%M %Z"),
                due.timestamp()
            )
        })
        .unwrap_or_else(|| "Unknown — confirm the deadline".into());
    let kind = notice
        .details
        .as_ref()
        .map(|d| d.deadline_kind.as_str())
        .unwrap_or("unknown");
    let mut description = format!(
        "**From:** {}\n**Action:** {}\n**Due:** {}\n**Why now:** {}\n[Open source]({})",
        clipped(&a.sender, 120),
        clipped(&a.action, 400),
        due,
        clipped(&a.reason, 400),
        a.source_url
    );
    if kind == "inferred_preparation" {
        description.push_str(
            "\nDeadline is an inferred preparation cutoff, not an explicit sender deadline.",
        );
    }
    if let Some(url) = notice
        .details
        .as_ref()
        .and_then(|d| d.meeting_url.as_deref())
        .filter(|url| url.starts_with("https://calendar.google.com/"))
    {
        description.push_str(&format!("\n[Open meeting]({url})"));
    }
    if a.state == AlertState::Acknowledged {
        description.push_str("\nSeen, still unresolved — preparation reminder.");
    }
    if let Some(status) = text_status.filter(|s| matches!(*s, "failed" | "unknown" | "expired")) {
        description.push_str(&format!(
            "\n**Text delivery: {status}.** Discord reminders remain active."
        ));
    }
    let _ = now;
    CreateMessage::new()
        .content(format!("<@{}> {heading}", owner.get()))
        .allowed_mentions(
            CreateAllowedMentions::new()
                .all_users(false)
                .all_roles(false)
                .everyone(false)
                .users([owner]),
        )
        .embed(
            CreateEmbed::new()
                .title(clipped(subject, 180))
                .description(description)
                .colour(if a.urgency == Urgency::Critical {
                    0xe53935
                } else {
                    0xffa000
                })
                .footer(serenity::all::CreateEmbedFooter::new(format!(
                    "Alert {} · notice {}",
                    a.id, notice.id
                ))),
        )
        .components(vec![CreateActionRow::Buttons(vec![
            CreateButton::new(format!("oa:ack:{}", a.id))
                .label("Acknowledge")
                .style(ButtonStyle::Primary),
            CreateButton::new(format!("oa:snooze:{}", a.id))
                .label("Snooze 10 min")
                .style(ButtonStyle::Secondary),
            CreateButton::new(format!("oa:resolve:{}", a.id))
                .label("Resolved")
                .style(ButtonStyle::Success),
        ])])
        .nonce(Nonce::String(format!("owner-alert-{}", notice.id)))
        .enforce_nonce(true)
}

/// Owner identity must be checked by the transport before invoking this function.
pub fn control(store: &Store, custom_id: &str, now: i64) -> anyhow::Result<String> {
    let Some(rest) = custom_id.strip_prefix("oa:") else {
        anyhow::bail!("not an owner alert control")
    };
    let Some((verb, id)) = rest.split_once(':') else {
        anyhow::bail!("invalid owner alert control")
    };
    let alert = store
        .owner_alert(id)?
        .ok_or_else(|| anyhow::anyhow!("This alert no longer exists"))?;
    if alert.state == AlertState::Resolved {
        return Ok("Already resolved. No further reminders are scheduled.".into());
    }
    match verb {
        "ack" => {
            store.set_owner_alert_state(id, AlertState::Acknowledged, now)?;
            store.reconcile_owner_alert_texts(now)?;
            Ok("Acknowledged as seen. Preparation reminders continue until resolved; no reply was sent.".into())
        }
        "resolve" => {
            store.set_owner_alert_state(id, AlertState::Resolved, now)?;
            store.reconcile_owner_alert_texts(now)?;
            Ok("Resolved. Future reminders and queued text escalation are cancelled.".into())
        }
        "snooze" => {
            store.snooze_owner_alert(id, now + 600_000, false, now)?;
            Ok("Snoozed for 10 minutes. This does not mark the task complete.".into())
        }
        _ => anyhow::bail!("unknown owner alert control"),
    }
}

pub async fn tick(store: &Store, broker: &dyn ApprovalBroker, now: i64) -> anyhow::Result<usize> {
    let started = std::time::Instant::now();
    let mut count = 0;
    for _ in 0..20 {
        let Some(notice) =
            store.claim_owner_alert_notice(now + started.elapsed().as_millis() as i64)?
        else {
            break;
        };
        match broker.post_owner_alert(&notice).await {
            Ok(url) => {
                store.complete_owner_alert_notice(
                    notice.id,
                    Some(&url),
                    None,
                    now + started.elapsed().as_millis() as i64,
                )?;
                count += 1;
            }
            Err(error) => {
                store.complete_owner_alert_notice(
                    notice.id,
                    None,
                    Some(&error.to_string()),
                    now + started.elapsed().as_millis() as i64,
                )?;
                tracing::warn!(alert_id=%notice.alert.id,"owner alert Discord delivery failed: {error}");
            }
        }
    }
    store.enqueue_owner_alert_texts(now + started.elapsed().as_millis() as i64)?;
    Ok(count)
}

pub async fn run(store: Arc<Store>, broker: Arc<dyn ApprovalBroker>, shutdown: CancellationToken) {
    let mut timer = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _=shutdown.cancelled()=>return,
            _=timer.tick()=>{
                if let Err(error)=tick(&store,broker.as_ref(),chrono::Utc::now().timestamp_millis()).await {
                    tracing::warn!("owner alert scheduler failed: {error:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notice_mentions_only_the_owner_and_has_distinct_controls() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path().join("test.db")).unwrap();
        store
            .insert_owner_alert(
                &augmentagent_store::owner_alerts::NewOwnerAlert {
                    id: "sample",
                    sender: "@everyone",
                    action: "Review briefing",
                    reason: "Meeting starts soon",
                    urgency: Urgency::Critical,
                    source_url: "https://example.test/1",
                    due_at_ms: None,
                    timezone: "UTC",
                    meeting_id: None,
                    expires_at_ms: 10000,
                    text_after_ms: 1,
                },
                1,
            )
            .unwrap();
        let notice = store.claim_owner_alert_notice(2).unwrap().unwrap();
        let json =
            serde_json::to_value(message(&notice, UserId::new(123), 2, Some("unknown"))).unwrap();
        assert!(json["content"]
            .as_str()
            .unwrap()
            .contains("<@123> 🚨 CRITICAL"));
        assert_eq!(
            json["allowed_mentions"]["users"],
            serde_json::json!(["123"])
        );
        assert_eq!(json["allowed_mentions"]["parse"], serde_json::json!([]));
        assert!(json["embeds"][0]["description"]
            .as_str()
            .unwrap()
            .contains("Unknown — confirm"));
        assert_eq!(
            json["components"][0]["components"][0]["custom_id"],
            "oa:ack:sample"
        );
        control(&store, "oa:ack:sample", 3).unwrap();
        assert_eq!(
            store.owner_alert("sample").unwrap().unwrap().state,
            AlertState::Acknowledged
        );
    }
    struct AlertBroker {
        fail: bool,
        posts: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ApprovalBroker for AlertBroker {
        async fn post_approval(
            &self,
            _: &str,
            _: &augmentagent_store::Email,
            _: &str,
        ) -> Result<(), crate::ApprovalError> {
            panic!("alert must bypass approval carousel")
        }
        async fn post_flag_notice(
            &self,
            _: &augmentagent_store::Email,
            _: &str,
        ) -> Result<(), crate::ApprovalError> {
            panic!("alert must bypass digest")
        }
        async fn post_owner_alert(&self, _: &AlertNotice) -> Result<String, crate::ApprovalError> {
            self.posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                Err(crate::ApprovalError::NotReady)
            } else {
                Ok("https://discord.com/channels/1/2/3".into())
            }
        }
    }
    #[tokio::test]
    async fn direct_scheduler_retries_failure_and_controls_persist_without_approval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let now = 10_000_000;
        let unrelated = augmentagent_store::Email {
            message_id: "ordinary".into(),
            thread_id: Some("ordinary-thread".into()),
            from: "colleague@example.test".into(),
            to: String::new(),
            cc: String::new(),
            subject: "Lunch".into(),
            body: "Lunch tomorrow?".into(),
            date: String::new(),
            attachments: vec![],
            account_entity_id: Some("account".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        store.upsert_email(&unrelated).unwrap();
        let action = store
            .log_action(
                &unrelated.message_id,
                unrelated.thread_id.as_deref(),
                &unrelated.from,
                &unrelated.subject,
                Some(&unrelated.body),
                Some("Sure"),
                augmentagent_store::ActionStatus::Pending,
            )
            .unwrap();
        store
            .record_nudge(&action, chrono::Utc::now().timestamp_millis() + 21_600_000)
            .unwrap();
        assert!(store.find_active_nudge().unwrap().is_some());
        store
            .insert_owner_alert(
                &augmentagent_store::owner_alerts::NewOwnerAlert {
                    id: "test",
                    sender: "Colleague",
                    action: "Prepare recommendation",
                    reason: "Meeting soon",
                    source_url: "https://example.test/1",
                    urgency: Urgency::High,
                    due_at_ms: Some(now + 3_600_000),
                    timezone: "America/New_York",
                    meeting_id: None,
                    expires_at_ms: now + 3_600_000,
                    text_after_ms: now + 600_000,
                },
                now,
            )
            .unwrap();
        let failed = AlertBroker {
            fail: true,
            posts: 0.into(),
        };
        assert_eq!(tick(&store, &failed, now).await.unwrap(), 0);
        assert_eq!(
            store.owner_alerts_due_for_text(now + 1000).unwrap().len(),
            1
        );
        store
            .with_conn(|c| {
                c.execute(
                    "UPDATE owner_alerts SET urgency='critical' WHERE id='test'",
                    [],
                )
            })
            .unwrap();
        let good = AlertBroker {
            fail: false,
            posts: 0.into(),
        };
        assert_eq!(tick(&store, &good, now + 61_000).await.unwrap(), 1);
        control(&store, "oa:snooze:test", now + 61_001).unwrap();
        drop(store);
        let store = Store::open(path).unwrap();
        assert_eq!(tick(&store, &good, now + 650_000).await.unwrap(), 0);
        assert_eq!(tick(&store, &good, now + 661_002).await.unwrap(), 1);
        control(&store, "oa:ack:test", now + 661_003).unwrap();
        assert!(store
            .owner_alerts_due_for_text(now + 661_004)
            .unwrap()
            .is_empty());
        assert!(control(&store, "oa:snooze:test", now + 3_500_000).is_err());
        control(&store, "oa:resolve:test", now + 700_000).unwrap();
        assert_eq!(tick(&store, &good, now + 800_000).await.unwrap(), 0);
        assert!(control(&store, "oa:ack:test", now + 800_001)
            .unwrap()
            .contains("Already resolved"));
    }
}
