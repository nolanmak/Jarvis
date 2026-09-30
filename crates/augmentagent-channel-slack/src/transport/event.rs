//! Typed view of Socket Mode frames.
//!
//! Slack sends three families of frame over the socket: `hello` after the
//! handshake, `disconnect` before it closes a link, and envelopes
//! (`events_api`, `interactive`, `slash_commands`) that must be acknowledged
//! by `envelope_id`. Everything here is tolerant: unknown envelope kinds and
//! unknown event types become [`SlackEvent::Unknown`] with the raw JSON kept,
//! so the socket loop can still acknowledge them and the consumer can decide
//! what to do. Field shapes follow the Socket Mode and Events API reference
//! pages read on 2026-09-29 (see `docs/SLACK-TRANSPORT.md`); none of them has
//! been confirmed against a live workspace yet.

use serde_json::Value;
use thiserror::Error;

/// A parsed inbound frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Envelope {
    Hello(Hello),
    Disconnect(Disconnect),
    /// Anything with an `envelope_id`, which therefore needs an ack.
    /// Boxed: it carries the raw payload and dwarfs the other variants.
    Event(Box<EventEnvelope>),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hello {
    pub app_id: Option<String>,
    pub num_connections: Option<u32>,
    pub approximate_connection_time_secs: Option<u64>,
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnect {
    pub reason: DisconnectReason,
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Slack is about to refresh this link; reconnect soon.
    Warning,
    /// Slack wants the client to open a fresh connection now.
    RefreshRequested,
    /// The app's Socket Mode link was disabled; reconnecting will not help.
    LinkDisabled,
    Other(String),
}

impl DisconnectReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Warning => "warning",
            Self::RefreshRequested => "refresh_requested",
            Self::LinkDisabled => "link_disabled",
            Self::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeKind {
    EventsApi,
    Interactive,
    SlashCommands,
    Other(String),
}

/// Outer fields of an `events_api` payload (`event_callback`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventsApiMeta {
    pub team_id: Option<String>,
    pub api_app_id: Option<String>,
    pub event_id: Option<String>,
    pub event_time: Option<u64>,
    pub event_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventEnvelope {
    pub envelope_id: String,
    pub kind: EnvelopeKind,
    pub accepts_response_payload: bool,
    pub retry_attempt: Option<u32>,
    pub retry_reason: Option<String>,
    pub events_api: Option<EventsApiMeta>,
    pub event: SlackEvent,
    /// The untouched `payload` object.
    pub payload: Value,
}

impl EventEnvelope {
    /// An identifier that is expected to stay the same when Slack redelivers
    /// the same event. `event_id` for Events API callbacks (documented as
    /// globally unique), the `trigger_id` for interactions and slash
    /// commands, and the envelope id as a last resort.
    pub fn stable_id(&self) -> String {
        if let Some(id) = self.events_api.as_ref().and_then(|m| m.event_id.as_deref()) {
            return id.to_string();
        }
        if let Some(t) = self.payload.get("trigger_id").and_then(Value::as_str) {
            return format!("trigger:{t}");
        }
        format!("envelope:{}", self.envelope_id)
    }

    /// `true` when Slack marked this as a redelivery.
    pub fn is_redelivery(&self) -> bool {
        self.retry_attempt.unwrap_or(0) > 0
    }
}

/// The typed event inside an envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum SlackEvent {
    /// A top-level channel/DM message (no `thread_ts`, or `thread_ts == ts`).
    Message(MessageEvent),
    /// A reply inside a thread (`thread_ts != ts`).
    ThreadReply(MessageEvent),
    /// `message` with subtype `message_changed`.
    MessageEdited(MessageEdited),
    /// `message` with subtype `message_deleted`.
    MessageDeleted(MessageDeleted),
    /// `app_mention`.
    AppMention(MessageEvent),
    /// `interactive` envelopes: block actions, view submissions, shortcuts.
    Interaction(Interaction),
    /// `slash_commands` envelopes.
    SlashCommand(SlashCommand),
    /// `file_*` events.
    File(FileEvent),
    /// `app_home_opened`.
    AppHome(AppHomeEvent),
    /// Anything the transport does not model yet. Kept so the consumer can
    /// log or persist it; never a reason to drop the connection.
    Unknown { kind: String, raw: Value },
}

impl SlackEvent {
    pub fn kind_name(&self) -> &str {
        match self {
            Self::Message(_) => "message",
            Self::ThreadReply(_) => "thread_reply",
            Self::MessageEdited(_) => "message_changed",
            Self::MessageDeleted(_) => "message_deleted",
            Self::AppMention(_) => "app_mention",
            Self::Interaction(_) => "interaction",
            Self::SlashCommand(_) => "slash_command",
            Self::File(f) => &f.kind,
            Self::AppHome(_) => "app_home_opened",
            Self::Unknown { kind, .. } => kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageEvent {
    pub channel: String,
    pub channel_type: Option<String>,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub team: Option<String>,
    pub subtype: Option<String>,
    pub files: Vec<FileRef>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub id: String,
    pub name: Option<String>,
    pub mimetype: Option<String>,
    pub url_private: Option<String>,
    pub size: Option<u64>,
    // #1293 — what the inbound attachment pipeline needs (file object
    // fields per https://docs.slack.dev/reference/objects/file-object,
    // read 2026-09-29).
    pub title: Option<String>,
    pub filetype: Option<String>,
    pub url_private_download: Option<String>,
    /// `hosted`, `external`, `snippet`, `post`; `tombstone` /
    /// `hidden_by_limit` for deleted or plan-limited files (the last two
    /// are from Slack SDK sources, **unverified** in the fetched docs).
    pub mode: Option<String>,
    /// `check_file_info` for Slack Connect files whose details need a
    /// `files.info` call first.
    pub file_access: Option<String>,
    pub is_external: bool,
    // #1297 — audio/video clip fields (file object docs, read 2026-09-29):
    // `subtype` is `slack_audio` / `slack_video` for clips recorded in
    // Slack; `duration_ms` is the media length Slack declares.
    pub subtype: Option<String>,
    pub media_display_type: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageEdited {
    pub channel: String,
    /// `ts` of the message that was edited (from `message.ts`).
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: Option<String>,
    pub text: Option<String>,
    pub previous_text: Option<String>,
    /// `ts` of the edit event itself.
    pub event_ts: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageDeleted {
    pub channel: String,
    pub deleted_ts: String,
    pub event_ts: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionKind {
    BlockActions,
    ViewSubmission,
    ViewClosed,
    Shortcut,
    MessageAction,
    BlockSuggestion,
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockAction {
    pub action_id: String,
    pub block_id: Option<String>,
    pub action_type: Option<String>,
    pub value: Option<String>,
    pub selected_option: Option<Value>,
    pub action_ts: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Interaction {
    pub kind: InteractionKind,
    pub trigger_id: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub channel_id: Option<String>,
    pub message_ts: Option<String>,
    pub response_url: Option<String>,
    pub callback_id: Option<String>,
    pub actions: Vec<BlockAction>,
    pub view: Option<Value>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommand {
    pub command: String,
    pub text: String,
    pub user_id: String,
    pub team_id: Option<String>,
    pub channel_id: Option<String>,
    pub trigger_id: Option<String>,
    pub response_url: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEvent {
    /// `file_shared`, `file_created`, `file_change`, `file_deleted`, ...
    pub kind: String,
    pub file_id: Option<String>,
    pub user_id: Option<String>,
    pub channel_id: Option<String>,
    pub event_ts: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppHomeEvent {
    pub user_id: String,
    pub channel: Option<String>,
    pub tab: Option<String>,
    pub view: Option<Value>,
    pub raw: Value,
}

#[derive(Debug, Error)]
pub enum EnvelopeParseError {
    #[error("frame is not valid JSON: {0}")]
    Json(String),
    #[error("frame is not a JSON object")]
    NotAnObject,
    #[error("frame has no `type`")]
    MissingType,
    #[error("`{kind}` frame has no `envelope_id`, so it cannot be acknowledged")]
    MissingEnvelopeId { kind: String },
}

/// Parse one text frame from the socket.
pub fn parse_envelope(text: &str) -> Result<Envelope, EnvelopeParseError> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| EnvelopeParseError::Json(e.to_string()))?;
    parse_envelope_value(value)
}

pub fn parse_envelope_value(value: Value) -> Result<Envelope, EnvelopeParseError> {
    let obj = value.as_object().ok_or(EnvelopeParseError::NotAnObject)?;
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or(EnvelopeParseError::MissingType)?
        .to_string();
    match kind.as_str() {
        "hello" => Ok(Envelope::Hello(Hello {
            app_id: str_at(&value, &["connection_info", "app_id"]),
            num_connections: value
                .get("num_connections")
                .and_then(Value::as_u64)
                .map(|n| n as u32),
            approximate_connection_time_secs: value
                .pointer("/debug_info/approximate_connection_time")
                .and_then(Value::as_u64),
            host: str_at(&value, &["debug_info", "host"]),
        })),
        "disconnect" => {
            let reason = match obj.get("reason").and_then(Value::as_str).unwrap_or("") {
                "warning" => DisconnectReason::Warning,
                "refresh_requested" => DisconnectReason::RefreshRequested,
                "link_disabled" => DisconnectReason::LinkDisabled,
                other => DisconnectReason::Other(other.to_string()),
            };
            Ok(Envelope::Disconnect(Disconnect {
                reason,
                host: str_at(&value, &["debug_info", "host"]),
            }))
        }
        _ => {
            let envelope_id = obj
                .get("envelope_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| EnvelopeParseError::MissingEnvelopeId { kind: kind.clone() })?
                .to_string();
            let payload = obj.get("payload").cloned().unwrap_or(Value::Null);
            let envelope_kind = match kind.as_str() {
                "events_api" => EnvelopeKind::EventsApi,
                "interactive" => EnvelopeKind::Interactive,
                "slash_commands" => EnvelopeKind::SlashCommands,
                other => EnvelopeKind::Other(other.to_string()),
            };
            let (events_api, event) = match envelope_kind {
                EnvelopeKind::EventsApi => {
                    let meta = EventsApiMeta {
                        team_id: str_at(&payload, &["team_id"]),
                        api_app_id: str_at(&payload, &["api_app_id"]),
                        event_id: str_at(&payload, &["event_id"]),
                        event_time: payload.get("event_time").and_then(Value::as_u64),
                        event_context: str_at(&payload, &["event_context"]),
                    };
                    let inner = payload.get("event").cloned().unwrap_or(Value::Null);
                    (Some(meta), parse_events_api_event(inner))
                }
                EnvelopeKind::Interactive => (None, parse_interaction(&payload)),
                EnvelopeKind::SlashCommands => (None, parse_slash_command(&payload)),
                EnvelopeKind::Other(ref k) => (
                    None,
                    SlackEvent::Unknown {
                        kind: k.clone(),
                        raw: payload.clone(),
                    },
                ),
            };
            Ok(Envelope::Event(Box::new(EventEnvelope {
                envelope_id,
                kind: envelope_kind,
                accepts_response_payload: obj
                    .get("accepts_response_payload")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                retry_attempt: obj
                    .get("retry_attempt")
                    .and_then(Value::as_u64)
                    .map(|n| n as u32),
                retry_reason: obj
                    .get("retry_reason")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                events_api,
                event,
                payload,
            })))
        }
    }
}

fn str_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut cur = value;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::to_string)
}

fn parse_events_api_event(inner: Value) -> SlackEvent {
    let kind = str_at(&inner, &["type"]).unwrap_or_default();
    match kind.as_str() {
        "message" => parse_message(inner),
        "app_mention" => SlackEvent::AppMention(message_fields(&inner)),
        "app_home_opened" => SlackEvent::AppHome(AppHomeEvent {
            user_id: str_at(&inner, &["user"]).unwrap_or_default(),
            channel: str_at(&inner, &["channel"]),
            tab: str_at(&inner, &["tab"]),
            view: inner.get("view").cloned(),
            raw: inner,
        }),
        k if k.starts_with("file_") => SlackEvent::File(FileEvent {
            kind: k.to_string(),
            file_id: str_at(&inner, &["file_id"]).or_else(|| str_at(&inner, &["file", "id"])),
            user_id: str_at(&inner, &["user_id"]).or_else(|| str_at(&inner, &["user"])),
            channel_id: str_at(&inner, &["channel_id"]),
            event_ts: str_at(&inner, &["event_ts"]),
            raw: inner,
        }),
        "" => SlackEvent::Unknown {
            kind: "<missing>".into(),
            raw: inner,
        },
        other => SlackEvent::Unknown {
            kind: other.to_string(),
            raw: inner,
        },
    }
}

fn parse_message(inner: Value) -> SlackEvent {
    let subtype = str_at(&inner, &["subtype"]);
    let channel = str_at(&inner, &["channel"]).unwrap_or_default();
    match subtype.as_deref() {
        Some("message_changed") => {
            let msg = inner.get("message").cloned().unwrap_or(Value::Null);
            let prev = inner
                .get("previous_message")
                .cloned()
                .unwrap_or(Value::Null);
            SlackEvent::MessageEdited(MessageEdited {
                channel,
                ts: str_at(&msg, &["ts"]).unwrap_or_default(),
                thread_ts: str_at(&msg, &["thread_ts"]),
                user: str_at(&msg, &["user"]),
                text: str_at(&msg, &["text"]),
                previous_text: str_at(&prev, &["text"]),
                event_ts: str_at(&inner, &["event_ts"]).or_else(|| str_at(&inner, &["ts"])),
                raw: inner,
            })
        }
        Some("message_deleted") => SlackEvent::MessageDeleted(MessageDeleted {
            channel,
            deleted_ts: str_at(&inner, &["deleted_ts"]).unwrap_or_default(),
            event_ts: str_at(&inner, &["event_ts"]).or_else(|| str_at(&inner, &["ts"])),
            raw: inner,
        }),
        _ => {
            let m = message_fields(&inner);
            match (&m.thread_ts, &m.ts) {
                (Some(t), ts) if t != ts => SlackEvent::ThreadReply(m),
                _ => SlackEvent::Message(m),
            }
        }
    }
}

/// The `files` of a message object (an event's `event`, or a message from
/// `conversations.history`/`replies`). Entries without an `id` are dropped.
pub fn file_refs(message: &Value) -> Vec<FileRef> {
    message
        .get("files")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    Some(FileRef {
                        id: str_at(f, &["id"])?,
                        name: str_at(f, &["name"]),
                        mimetype: str_at(f, &["mimetype"]),
                        url_private: str_at(f, &["url_private"]),
                        size: f.get("size").and_then(Value::as_u64),
                        title: str_at(f, &["title"]),
                        filetype: str_at(f, &["filetype"]),
                        url_private_download: str_at(f, &["url_private_download"]),
                        mode: str_at(f, &["mode"]),
                        file_access: str_at(f, &["file_access"]),
                        is_external: f
                            .get("is_external")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        subtype: str_at(f, &["subtype"]),
                        media_display_type: str_at(f, &["media_display_type"]),
                        duration_ms: f.get("duration_ms").and_then(Value::as_u64),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn message_fields(inner: &Value) -> MessageEvent {
    let files = file_refs(inner);
    MessageEvent {
        channel: str_at(inner, &["channel"]).unwrap_or_default(),
        channel_type: str_at(inner, &["channel_type"]),
        user: str_at(inner, &["user"]),
        bot_id: str_at(inner, &["bot_id"]),
        text: str_at(inner, &["text"]).unwrap_or_default(),
        ts: str_at(inner, &["ts"]).unwrap_or_default(),
        thread_ts: str_at(inner, &["thread_ts"]),
        team: str_at(inner, &["team"]),
        subtype: str_at(inner, &["subtype"]),
        files,
        raw: inner.clone(),
    }
}

fn parse_interaction(payload: &Value) -> SlackEvent {
    let kind = match str_at(payload, &["type"]).unwrap_or_default().as_str() {
        "block_actions" => InteractionKind::BlockActions,
        "view_submission" => InteractionKind::ViewSubmission,
        "view_closed" => InteractionKind::ViewClosed,
        "shortcut" => InteractionKind::Shortcut,
        "message_action" => InteractionKind::MessageAction,
        "block_suggestion" => InteractionKind::BlockSuggestion,
        other => InteractionKind::Other(other.to_string()),
    };
    let actions = payload
        .get("actions")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|a| BlockAction {
                    action_id: str_at(a, &["action_id"]).unwrap_or_default(),
                    block_id: str_at(a, &["block_id"]),
                    action_type: str_at(a, &["type"]),
                    value: str_at(a, &["value"]),
                    selected_option: a.get("selected_option").cloned(),
                    action_ts: str_at(a, &["action_ts"]),
                })
                .collect()
        })
        .unwrap_or_default();
    SlackEvent::Interaction(Interaction {
        kind,
        trigger_id: str_at(payload, &["trigger_id"]),
        user_id: str_at(payload, &["user", "id"]),
        team_id: str_at(payload, &["team", "id"]),
        channel_id: str_at(payload, &["channel", "id"]),
        message_ts: str_at(payload, &["message", "ts"])
            .or_else(|| str_at(payload, &["message_ts"])),
        response_url: str_at(payload, &["response_url"]),
        callback_id: str_at(payload, &["callback_id"])
            .or_else(|| str_at(payload, &["view", "callback_id"])),
        actions,
        view: payload.get("view").cloned(),
        raw: payload.clone(),
    })
}

fn parse_slash_command(payload: &Value) -> SlackEvent {
    SlackEvent::SlashCommand(SlashCommand {
        command: str_at(payload, &["command"]).unwrap_or_default(),
        text: str_at(payload, &["text"]).unwrap_or_default(),
        user_id: str_at(payload, &["user_id"]).unwrap_or_default(),
        team_id: str_at(payload, &["team_id"]),
        channel_id: str_at(payload, &["channel_id"]),
        trigger_id: str_at(payload, &["trigger_id"]),
        response_url: str_at(payload, &["response_url"]),
        raw: payload.clone(),
    })
}
