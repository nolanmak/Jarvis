//! #2042 — `calendar create-event --repeat …`: turn the CLI flags into the
//! RRULE line, IANA zone and card wording the draft carries. The rules
//! themselves live in `augmentagent_channel_calendar::repeat`.

use anyhow::{bail, Result};
use augmentagent_channel_calendar::repeat::{self, Repeat};
use chrono::{DateTime, FixedOffset, Offset};
use chrono_tz::Tz;

#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct RepeatArgs {
    /// Make it a recurring series: daily | weekdays | weekly | biweekly | monthly
    #[arg(long)]
    pub repeat: Option<String>,
    /// Total occurrences including the first (2-365; default: no end)
    #[arg(long)]
    pub repeat_count: Option<u32>,
    /// Last date an occurrence may fall on, YYYY-MM-DD (inclusive)
    #[arg(long)]
    pub repeat_until: Option<String>,
    /// IANA time zone for the series, e.g. America/New_York (default: the
    /// owner's zone)
    #[arg(long)]
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedRepeat {
    pub rule: String,
    pub time_zone: String,
    pub words: String,
}

/// `Ok(None)` for a one-off event.
pub(crate) fn plan(
    args: &RepeatArgs,
    start: DateTime<FixedOffset>,
    default_zone: Tz,
) -> Result<Option<PlannedRepeat>> {
    let Some(name) = args
        .repeat
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    else {
        if args.repeat_count.is_some() || args.repeat_until.is_some() || args.time_zone.is_some() {
            bail!("--repeat-count, --repeat-until and --time-zone only apply with --repeat");
        }
        return Ok(None);
    };
    let Some(preset) = Repeat::parse(name) else {
        bail!(
            "--repeat must be one of daily | weekdays | weekly | biweekly | monthly (got {name:?})"
        );
    };
    let end = repeat::parse_end(args.repeat_count, args.repeat_until.as_deref())
        .map_err(anyhow::Error::msg)?;
    let zone = match args
        .time_zone
        .as_deref()
        .map(str::trim)
        .filter(|z| !z.is_empty())
    {
        Some(z) => z.parse::<Tz>().map_err(|_| {
            anyhow::anyhow!("--time-zone {z:?} is not an IANA zone name (e.g. America/New_York)")
        })?,
        None => default_zone,
    };
    // A zone whose offset at the start differs from --start's offset is
    // almost always a mistake (the series would land at another local hour).
    let zone_offset = start.with_timezone(&zone).offset().fix();
    if zone_offset != *start.offset() {
        bail!(
            "--start offset {} does not match {zone}'s offset {} at that time; \
             fix --start or pass the right --time-zone",
            start.offset(),
            zone_offset
        );
    }
    let rule = repeat::build(preset, end, start, zone).map_err(anyhow::Error::msg)?;
    Ok(Some(PlannedRepeat {
        rule,
        time_zone: zone.name().to_string(),
        words: repeat::describe(preset, end, start, zone),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NY: Tz = chrono_tz::America::New_York;

    fn start() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-11-17T10:00:00-05:00").unwrap()
    }

    fn args(
        repeat: Option<&str>,
        count: Option<u32>,
        until: Option<&str>,
        tz: Option<&str>,
    ) -> RepeatArgs {
        RepeatArgs {
            repeat: repeat.map(Into::into),
            repeat_count: count,
            repeat_until: until.map(Into::into),
            time_zone: tz.map(Into::into),
        }
    }

    #[test]
    fn no_repeat_is_a_one_off() {
        assert_eq!(
            plan(&args(None, None, None, None), start(), NY).unwrap(),
            None
        );
    }

    #[test]
    fn weekly_with_a_count_uses_the_owner_zone() {
        let p = plan(&args(Some("weekly"), Some(10), None, None), start(), NY)
            .unwrap()
            .unwrap();
        assert_eq!(p.rule, "RRULE:FREQ=WEEKLY;COUNT=10");
        assert_eq!(p.time_zone, "America/New_York");
        assert_eq!(p.words, "weekly on Tue, 10 times");
    }

    #[test]
    fn an_explicit_zone_must_be_real_and_match_the_start_offset() {
        let e = plan(
            &args(Some("daily"), None, None, Some("Mars/Olympus")),
            start(),
            NY,
        )
        .unwrap_err();
        assert!(e.to_string().contains("IANA"), "{e}");
        let e = plan(
            &args(Some("daily"), None, None, Some("America/Los_Angeles")),
            start(),
            NY,
        )
        .unwrap_err();
        assert!(e.to_string().contains("offset"), "{e}");
        let ok = plan(
            &args(Some("daily"), None, None, Some("America/Toronto")),
            start(),
            NY,
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok.time_zone, "America/Toronto");
    }

    #[test]
    fn create_event_parses_the_repeat_flags() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([
            "augmentagent",
            "calendar",
            "create-event",
            "--summary",
            "1:1",
            "--start",
            "2026-11-17T10:00:00-05:00",
            "--repeat",
            "weekly",
            "--repeat-count",
            "10",
            "--time-zone",
            "America/New_York",
        ])
        .unwrap();
        match cli.cmd {
            crate::Cmd::Calendar {
                op: crate::CalendarOp::CreateEvent { repeat, .. },
            } => {
                assert_eq!(repeat.repeat.as_deref(), Some("weekly"));
                assert_eq!(repeat.repeat_count, Some(10));
                assert_eq!(repeat.time_zone.as_deref(), Some("America/New_York"));
            }
            _ => panic!("wrong command"),
        }
    }

    /// Owner-authorized live check on ONE owned Google-direct account
    /// (`JARVIS_CALENDAR_QA_ACCOUNT=<email>`): approve a guest-less weekly x3
    /// draft through the real approver, see exactly 3 occurrences at the same
    /// local time, then delete the series.
    #[tokio::test]
    #[ignore = "owner-authorized live calendar write; needs JARVIS_CALENDAR_QA_ACCOUNT and the Google OAuth env"]
    async fn live_recurring_create_round_trip() -> Result<()> {
        use crate::{
            test_support, ActionStatus, ApprovalActionHandler, ApprovalActionOutcome, Store,
        };
        use augmentagent_channel_calendar::{
            CalendarApi, ComposioCalendarClient, EventDraft, GoogleCalendar,
        };
        use chrono::{TimeZone, Timelike, Utc};
        use std::sync::Arc;
        let _ = dotenvy::dotenv();
        let google = || -> Result<GoogleCalendar> {
            let tokens = augmentagent_channel_email::google::GoogleGmail::from_env()?;
            Ok(GoogleCalendar::new(Arc::new(tokens)))
        };
        let client = || -> Result<ComposioCalendarClient> {
            Ok(
                ComposioCalendarClient::new("unused-for-google-direct".into())
                    .with_google(google()?),
            )
        };
        let (api, approver_client, cleaner) = (client()?, client()?, google()?);
        augmentagent_channel_core::state_dir::isolate_for_tests();
        let email = std::env::var("JARVIS_CALENDAR_QA_ACCOUNT")?;
        let entity = format!("google:{email}");
        let dir = tempfile::tempdir()?;
        let store = Arc::new(Store::open(dir.path().join("t.db"))?);

        // 07:15 owner-local, 4 days out (quiet hour, nothing real there).
        let zone = augmentagent_approval_discord::timeparse::owner_zone();
        let day = (Utc::now() + chrono::Duration::days(4))
            .with_timezone(&zone)
            .date_naive();
        let start = zone
            .from_local_datetime(&day.and_hms_opt(7, 15, 0).unwrap())
            .earliest()
            .unwrap()
            .fixed_offset();
        let planned = plan(&args(Some("weekly"), Some(3), None, None), start, zone)?.unwrap();
        println!(
            "plan: {} {} ({})",
            planned.rule, planned.words, planned.time_zone
        );
        let summary = format!(
            "[TEST #2042] {}",
            &uuid::Uuid::new_v4().simple().to_string()[..10]
        );
        let draft = EventDraft {
            summary: summary.clone(),
            start_datetime: start.to_rfc3339(),
            duration_minutes: 15,
            attendees: vec![],
            description: Some(
                "Owner-authorized recurrence QA. Temporary; deleted after verification.".into(),
            ),
            create_meeting_room: false,
            recurrence: vec![planned.rule.clone()],
            time_zone: Some(planned.time_zone.clone()),
        };
        let body = serde_json::to_string(&draft)?;
        let mail = augmentagent_store::Email {
            message_id: format!("gcal-create:{}", uuid::Uuid::new_v4()),
            thread_id: None,
            from: email.clone(),
            to: String::new(),
            cc: String::new(),
            attachments: Vec::new(),
            subject: summary.clone(),
            body: body.clone(),
            date: start.to_rfc3339(),
            account_entity_id: Some(entity.clone()),
            platform: "gcal".into(),
            kind: "create_event".into(),
        };
        store.upsert_email(&mail)?;
        let action = store.log_action(
            &mail.message_id,
            None,
            &mail.from,
            &mail.subject,
            Some(&body),
            Some("recurrence QA"),
            ActionStatus::Pending,
        )?;
        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar = Arc::new(approver_client);
        let ApprovalActionOutcome::CalendarCreated { event_id, .. } =
            approver.approve(&action).await
        else {
            anyhow::bail!("create failed; search the calendar for {summary} before retrying");
        };
        println!("created series {event_id} ({summary})");

        let verify: Result<()> = async {
            let from = start.with_timezone(&Utc) - chrono::Duration::hours(1);
            let to = from + chrono::Duration::days(28);
            let hits: Vec<_> = api
                .list_events(&entity, "primary", from, to)
                .await?
                .into_iter()
                .filter(|e| e.summary.as_deref() == Some(summary.as_str()))
                .collect();
            let locals: Vec<String> = hits
                .iter()
                .filter_map(|e| e.start.as_ref()?.to_utc())
                .map(|t| {
                    t.with_timezone(&zone)
                        .format("%a %Y-%m-%d %H:%M %Z")
                        .to_string()
                })
                .collect();
            println!("occurrences: {locals:?}");
            anyhow::ensure!(
                hits.len() == 3,
                "expected 3 occurrences, got {}",
                hits.len()
            );
            anyhow::ensure!(
                hits.iter()
                    .all(|e| e.recurring_event_id.as_deref() == Some(event_id.as_str())),
                "occurrences are not instances of the created series"
            );
            anyhow::ensure!(
                hits.iter()
                    .filter_map(|e| e.start.as_ref()?.to_utc())
                    .all(|t| t.with_timezone(&zone).hour() == 7
                        && t.with_timezone(&zone).minute() == 15),
                "an occurrence drifted off 07:15 local"
            );
            Ok(())
        }
        .await;
        let cleanup = cleaner
            .call_no_content(
                &email,
                "DELETE",
                &format!("calendars/primary/events/{event_id}"),
            )
            .await;
        println!("cleanup (whole series): {cleanup:?}");
        cleanup?;
        verify
    }

    // AC2 — end flags without --repeat, and unknown presets, are errors.
    #[test]
    fn end_flags_need_repeat_and_presets_are_checked() {
        for a in [
            args(None, Some(3), None, None),
            args(None, None, Some("2027-01-01"), None),
            args(None, None, None, Some("America/New_York")),
        ] {
            let e = plan(&a, start(), NY).unwrap_err();
            assert!(e.to_string().contains("--repeat"), "{e}");
        }
        let e = plan(&args(Some("yearly"), None, None, None), start(), NY).unwrap_err();
        assert!(
            e.to_string()
                .contains("daily | weekdays | weekly | biweekly | monthly"),
            "{e}"
        );
        assert!(plan(&args(Some("weekly"), Some(1), None, None), start(), NY).is_err());
    }
}
