//! Operator and synthetic-alert entry point; shares state with Discord controls.
use augmentagent_store::{
    owner_alerts::{AlertState, NewOwnerAlert, Urgency},
    Store,
};

#[derive(clap::Subcommand, Debug)]
pub(crate) enum Op {
    /// Override an email, wiki person slug or exact domain; routine mutes escalation.
    Priority {
        sender: String,
        /// Wiki root for resolving person selectors (also accepts global --wiki-dir).
        #[arg(long)]
        wiki_dir: Option<std::path::PathBuf>,
        #[arg(value_parser=["routine","high","critical"], required_unless_present="urgency")]
        level: Option<String>,
        #[arg(long,value_parser=["routine","high","critical"], conflicts_with="level")]
        urgency: Option<String>,
    },
    /// Replay a private JSONL export offline; prints aggregate counts only.
    Calibrate {
        #[arg(long)]
        input: std::path::PathBuf,
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
    /// Record that the alert was seen; preparation reminders continue.
    #[command(visible_alias = "seen")]
    Acknowledge { id: String },
    /// Mark the alert done and stop all further reminders.
    #[command(visible_alias = "done")]
    Resolve { id: String },
    Snooze {
        id: String,
        #[arg(long, default_value_t = 600)]
        seconds: u32,
        #[arg(long)]
        override_deadline: bool,
    },
    /// Print lifecycle/delivery state without message text or destination.
    Status { id: String },
}

pub(crate) async fn run(
    store: &std::sync::Arc<Store>,
    op: &Op,
    wiki: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    match op {
        Op::Calibrate { input } => return calibrate(input),
        Op::Priority {
            sender,
            urgency,
            level,
            wiki_dir,
        } => {
            let priority = match urgency.as_deref().or(level.as_deref()).unwrap_or("high") {
                "critical" => Urgency::Critical,
                "high" => Urgency::High,
                _ => Urgency::Routine,
            };
            let selector = augmentagent_store::owner_alert_gate::canonical_selector(sender)?;
            if let Some(slug) = selector.strip_prefix("person:") {
                let catalog = augmentagent_channel_email::owner_alert_gate::Catalog::load(
                    wiki_dir.as_deref().or(wiki),
                )?;
                let identities = catalog
                    .identities(slug)
                    .filter(|i| !i.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("person must have wiki email identities; pass --wiki-dir")
                    })?;
                store.set_owner_alert_priority_identities(&selector, priority, identities)?;
            } else {
                store.set_owner_alert_priority(&selector, priority)?;
            }
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

#[path = "owner_alert_calibration.rs"]
mod calibration;
pub(crate) fn calibrate(input: &std::path::Path) -> anyhow::Result<()> {
    let report = calibration::replay(std::io::BufReader::new(std::fs::File::open(input)?))?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
pub(crate) fn enqueue_digest(store: &Store, now: i64) -> anyhow::Result<usize> {
    let times = match std::env::var("AUGMENTAGENT_ALERT_HIGH_DIGEST") {
        Ok(v) if v.trim().is_empty() => return Ok(0),
        Ok(v) => v,
        Err(_) if augmentagent_channel_email::owner_alert_gate::enabled() => "09:00,17:00".into(),
        Err(_) => return Ok(0),
    };
    Ok(store.enqueue_owner_alert_digest(
        now,
        &augmentagent_channel_email::owner_alerts::timezone(),
        &times,
    )?)
}
