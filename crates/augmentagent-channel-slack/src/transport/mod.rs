//! #1283 — real-time Slack transport: a first-party app over Socket Mode plus
//! a typed Web API client. Decision record: `docs/SLACK-TRANSPORT.md`.
//!
//! `augmentagent slack app` (#1284, [`crate::app`]) uses the Web API client
//! and the connector to verify tokens; nothing here is wired into `serve`
//! yet (#1287). The Composio-backed ingestion in `api.rs`/`channel.rs` is
//! untouched.

pub mod backoff;
pub mod event;
pub mod socket;
pub mod token;
pub mod web;

pub use event::{Envelope, EventEnvelope, SlackEvent};
pub use socket::{
    Ack, ConnectionState, SlackConnector, SlackDelivery, SlackEventSink, SocketConnector,
    SocketModeClient, SocketModeConfig, SocketModeMetrics,
};
pub use token::{AppLevelToken, BotToken};
pub use web::{AuthTest, HttpSlackWebApi, RecordingSlackWebApi, SlackWebApi, WebApiConfig, WebApiError};
