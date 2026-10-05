//! Operator and synthetic-alert entry point; shares state with Discord controls.
use augmentagent_store::{
    owner_alerts::{AlertState, NewOwnerAlert, Urgency},
    Store,
};

#[derive(clap::Subcommand, Debug)]
pub(crate) enum Op {
    /// Override priority for an exact sender; routine mutes proactive escalation.
    Priority {
        sender: String,
        #[arg(long,value_parser=["routine","high","critical"])]
        urgency: String,
    },
    /// Persist the reminder schedule across restarts.
    Policy {
        #[arg(long, default_value_t = 600)]
        followup_seconds: u32,
        #[arg(long, default_value_t = 3600)]
        prepare_first_seconds: u32,
        #[arg(long, default_value_t = 900)]
        prepare_last_seconds: u32,
        #[arg(long, default_value_t = 6)]
        max_notices: u32,
    },
    /// Run one Discord alert evaluation, useful for end-to-end acceptance.
    Dispatch,
    /// Explicitly opt the verified owner destination into proactive texts.
    Configure {
        #[arg(long)]
        destination: Option<String>,
        #[arg(long)]
        enabled: bool,
    },
    /// Create an actionable alert (also supports synthetic end-to-end tests).
    Create {
        id: String,
        #[arg(long)]
        sender: String,
        #[arg(long)]
        action: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        source_url: String,
        #[arg(long, value_parser=["routine","high","critical"])]
        urgency: String,
        /// RFC3339 deadline, including its UTC offset. Omit when unknown.
        #[arg(long)]
        due: Option<String>,
        #[arg(long, default_value = "UTC")]
        timezone: String,
        #[arg(long)]
        meeting_id: Option<String>,
        #[arg(long, default_value_t = 600)]
        escalation_seconds: u32,
        #[arg(long, default_value_t = 3600)]
        useful_seconds: u32,
    },
    Acknowledge {
        id: String,
    },
    Resolve {
        id: String,
    },
    Snooze {
        id: String,
        #[arg(long, default_value_t = 600)]
        seconds: u32,
        #[arg(long)]
        override_deadline: bool,
    },
    /// Print lifecycle/delivery state without message text or destination.
    Status {
        id: String,
    },
}

pub(crate) async fn run(store: &std::sync::Arc<Store>, op: &Op) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    match op {
        Op::Priority { sender, urgency } => {
            let priority = match urgency.as_str() {
                "critical" => Urgency::Critical,
                "high" => Urgency::High,
                _ => Urgency::Routine,
            };
            store.set_owner_alert_priority(sender, priority)?;
            println!("Sender priority saved");
        }
        Op::Policy {
            followup_seconds,
            prepare_first_seconds,
            prepare_last_seconds,
            max_notices,
        } => {
            store.configure_owner_alert_schedule(
                i64::from(*followup_seconds) * 1000,
                i64::from(*prepare_first_seconds) * 1000,
                i64::from(*prepare_last_seconds) * 1000,
                i64::from(*max_notices),
            )?;
            println!("Reminder policy saved");
        }
        Op::Dispatch => {
            let token = std::env::var("DISCORD_BOT_TOKEN")?;
            let channel = std::env::var("DISCORD_CHANNEL_ID")?.parse::<u64>()?;
            let owner = std::env::var("DISCORD_ALLOWED_USER_ID")?.parse::<u64>()?;
            let broker = augmentagent_approval_discord::DiscordApprovalBroker::post_only(
                &token,
                channel,
                owner,
                std::sync::Arc::clone(store),
            );
            let count =
                augmentagent_approval_discord::owner_alerts::tick(store, &broker, now).await?;
            println!("{}", serde_json::json!({"posted":count}));
        }
        Op::Configure {
            destination,
            enabled,
        } => {
            store.configure_owner_alert_texts(destination.as_deref(), *enabled)?;
            println!(
                "Owner texts {}",
                if *enabled { "enabled" } else { "disabled" }
            );
        }
        Op::Create {
            id,
            sender,
            action,
            reason,
            source_url,
            urgency,
            due,
            timezone,
            meeting_id,
            escalation_seconds,
            useful_seconds,
        } => {
            anyhow::ensure!(*useful_seconds > 0, "useful-seconds must be positive");
            let due = due
                .as_deref()
                .map(chrono::DateTime::parse_from_rfc3339)
                .transpose()?
                .map(|d| d.timestamp_millis());
            let urgency = match urgency.as_str() {
                "critical" => Urgency::Critical,
                "high" => Urgency::High,
                _ => Urgency::Routine,
            };
            let inserted = store.insert_owner_alert(
                &NewOwnerAlert {
                    id,
                    sender,
                    action,
                    reason,
                    source_url,
                    urgency,
                    due_at_ms: due,
                    timezone,
                    meeting_id: meeting_id.as_deref(),
                    expires_at_ms: due.unwrap_or(now + i64::from(*useful_seconds) * 1000),
                    text_after_ms: now + i64::from(*escalation_seconds) * 1000,
                },
                now,
            )?;
            store.enqueue_owner_alert_texts(now)?;
            println!("{}", serde_json::json!({"id":id,"created":inserted}));
        }
        Op::Acknowledge { id } | Op::Resolve { id } => {
            let state = if matches!(op, Op::Resolve { .. }) {
                AlertState::Resolved
            } else {
                AlertState::Acknowledged
            };
            let changed = store.set_owner_alert_state(id, state, now)?;
            println!("{}", serde_json::json!({"id":id,"changed":changed}));
        }
        Op::Snooze {
            id,
            seconds,
            override_deadline,
        } => {
            let changed = store.snooze_owner_alert(
                id,
                now + i64::from(*seconds) * 1000,
                *override_deadline,
                now,
            )?;
            println!("{}", serde_json::json!({"id":id,"changed":changed}));
        }
        Op::Status { id } => {
            let alert = store
                .owner_alert(id)?
                .ok_or_else(|| anyhow::anyhow!("unknown alert"))?;
            println!(
                "{}",
                serde_json::json!({"id":id,"state":alert.state.as_str(),
                "urgency":alert.urgency.as_str(),"text":store.owner_alert_text_status(id)?,
                "created_at_ms":alert.created_at_ms,"updated_at_ms":alert.updated_at_ms})
            );
        }
    }
    Ok(())
}
