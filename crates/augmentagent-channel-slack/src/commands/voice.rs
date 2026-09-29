//! `voice` on Slack (#1297): the spoken-reply switch for a conversation.
//!
//! `voice on` / `voice off` store the conversation's reply mode
//! ([`crate::voice::reply::set_reply_mode`]); every answer reads it
//! ([`crate::voice::reply::reply_mode_for`]: own choice, then a thread's
//! channel or DM, then text), so it applies to the next answer and survives
//! a restart. `voice` / `voice status` shows the mode and the daemon's
//! speech providers. Live voice (Discord's `/voice` in a call) is the #1298
//! blocker and is named, never faked.

use augmentagent_approval_discord::owner_commands::LIVE_VOICE_BLOCKER;
use augmentagent_store::Store;

use super::{usage_of, CommandContext, SlackCommandDeps};
use crate::voice::reply::{reply_mode_source, set_reply_mode, ReplyMode};

/// Is `args` a `voice` subcommand? Plain text reaches `voice` only with one
/// of these (a sentence that starts with "voice" goes to the agent).
pub(super) fn is_subcommand(args: &str) -> bool {
    matches!(
        args.trim().to_ascii_lowercase().as_str(),
        "on" | "off" | "status"
    )
}

fn providers(deps: &SlackCommandDeps) -> (String, String) {
    match &deps.voice {
        Some(r) => (
            match &r.stt {
                Ok(p) => format!("transcribed with {p}"),
                Err(why) => format!("not transcribed on this host: {why}"),
            },
            match &r.tts {
                Ok(p) => p.clone(),
                Err(why) => format!("not available: {why}"),
            },
        ),
        None => (
            "not set up on this daemon".into(),
            "not available: not set up on this daemon".into(),
        ),
    }
}

pub(super) fn command(
    store: &Store,
    deps: &SlackCommandDeps,
    cx: &CommandContext<'_>,
    args: &str,
) -> String {
    let conversation = cx.conversation;
    match args.trim().to_ascii_lowercase().as_str() {
        "" | "status" => {
            let (mode, inherited) = match reply_mode_source(store, conversation) {
                Ok(m) => m,
                Err(e) => return format!("Voice status unavailable: {e}"),
            };
            let (clips, tts) = providers(deps);
            format!(
                "Spoken replies: {}{} (`voice on` / `voice off`)\nText-to-speech: {tts}\nVoice \
                 clips: {clips}\nLive voice: {LIVE_VOICE_BLOCKER}",
                if mode == ReplyMode::Spoken {
                    "on"
                } else {
                    "off"
                },
                if inherited {
                    " (from this thread's channel)"
                } else {
                    ""
                },
            )
        }
        "on" => {
            if let Err(e) = set_reply_mode(store, conversation, ReplyMode::Spoken, cx.now_ms) {
                return format!("Spoken replies unchanged: {e}");
            }
            let mut reply = "Spoken replies are on for this conversation: each answer comes as \
                             an audio file plus the full text. `voice off` goes back to text."
                .to_string();
            if let Some(Err(why)) = deps.voice.as_ref().map(|r| &r.tts) {
                reply.push_str(&format!(
                    "\nNo text-to-speech provider is ready on this daemon ({why}), so answers \
                     stay text with a note until one is."
                ));
            }
            reply
        }
        "off" => match set_reply_mode(store, conversation, ReplyMode::Text, cx.now_ms) {
            Ok(()) => "Spoken replies are off for this conversation: answers are text only.".into(),
            Err(e) => format!("Spoken replies unchanged: {e}"),
        },
        _ => usage_of("voice"),
    }
}
