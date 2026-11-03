//! The calendar side effects the gate and Undo use, behind a trait so the
//! gate is testable without Google. The daemon uses [`GoogleCalendarOps`]
//! over the direct Google client (C3/C4).

use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_channel_calendar::{
    connected_google_calendar_accounts, CandidateSet, CandidateWindow, EventRef, GoogleCalendar,
    GuestOutcome,
};
use augmentagent_store::Store;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;

#[async_trait]
pub trait CalendarOps: Send + Sync {
    /// D14 lookup over every connected owner calendar. Any account error
    /// is `Err` (never a partial list).
    async fn candidates(&self, text: &str) -> Result<CandidateSet, String>;
    async fn add_guest(&self, event: &EventRef, email: &str) -> Result<GuestOutcome, String>;
    async fn remove_guest(&self, event: &EventRef, email: &str) -> Result<GuestOutcome, String>;
}

/// [`CalendarOps`] over the direct Google client.
pub struct GoogleCalendarOps {
    pub google: GoogleCalendar,
    pub store: Arc<Store>,
    pub tz: Tz,
    /// Fixed "now" for the window parse (tests); `None` = the clock.
    pub now: Option<DateTime<Utc>>,
}

#[async_trait]
impl CalendarOps for GoogleCalendarOps {
    async fn candidates(&self, text: &str) -> Result<CandidateSet, String> {
        let accounts = connected_google_calendar_accounts(&self.store).map_err(|e| e.to_string())?;
        let window = CandidateWindow::parse(text, self.now.unwrap_or_else(Utc::now), self.tz);
        self.google
            .find_candidate_events(&accounts, &window, text)
            .await
            .map_err(|e| e.to_string())
    }

    async fn add_guest(&self, event: &EventRef, email: &str) -> Result<GuestOutcome, String> {
        self.google
            .add_guest(&event.account, &event.calendar_id, &event.event_id, email)
            .await
            .map_err(|e| e.to_string())
    }

    async fn remove_guest(&self, event: &EventRef, email: &str) -> Result<GuestOutcome, String> {
        self.google
            .remove_guest(&event.account, &event.calendar_id, &event.event_id, email)
            .await
            .map_err(|e| e.to_string())
    }
}
