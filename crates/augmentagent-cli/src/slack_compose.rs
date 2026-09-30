//! #1290 — `augmentagent slack compose`: propose a new Slack message to a
//! named person or subscribed channel.
//!
//! This is the agent's (and the owner's shell's) way to start a Slack
//! message. It never sends: it resolves the recipient through the wiki
//! identity layer, stores a pending approval with its destination, and the
//! daemon's approval queue posts the card (Slack and/or Discord, per
//! `AUGMENTAGENT_APPROVAL_SURFACES`). Only an Approve sends, as the owner's
//! own Slack account. An ambiguous or unknown recipient is an error and
//! nothing is stored; `--dry-run` resolves and stores nothing.

use std::path::Path;

use augmentagent_channel_slack::contact::compose::{self, ComposeOutcome};
use augmentagent_store::Store;

#[derive(clap::Args, Debug, Clone)]
pub(crate) struct ComposeArgs {
    /// A person (full or first name, wiki page, or Slack user id) or a
    /// subscribed channel (`#name` or its id).
    #[arg(long)]
    pub to: String,
    /// The message, in Markdown (converted to Slack formatting on send).
    #[arg(long)]
    pub text: String,
    /// Composio Slack workspace to send through; needed when several are
    /// connected.
    #[arg(long)]
    pub team_id: Option<String>,
    /// Resolve the recipient and print what would be carded; store nothing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

pub(crate) fn run(
    store: &Store,
    wiki_root: Option<&Path>,
    to: &str,
    text: &str,
    team_id: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<String> {
    let team = compose::compose_workspace(store, team_id).map_err(anyhow::Error::msg)?;
    if let Some(wanted) = team_id {
        anyhow::ensure!(
            wanted == team,
            "Slack workspace `{wanted}` is not connected through Composio"
        );
    }
    match compose::compose(store, wiki_root, &team, to, text, dry_run) {
        ComposeOutcome::Preview { recipient } => Ok(format!(
            "dry run: would card a new Slack message to {} in workspace {team}; nothing stored \
             or sent",
            recipient.label()
        )),
        ComposeOutcome::Card {
            action_id,
            recipient,
            ..
        } => Ok(format!(
            "pending approval: new Slack message to {} (ref {}). Nothing is sent until the \
             owner approves the card.",
            recipient.label(),
            &action_id[..8.min(action_id.len())]
        )),
        ComposeOutcome::Ambiguous { query, candidates } => anyhow::bail!(
            "“{query}” matches more than one person; nothing stored. Use one of: {}",
            candidates.join("; ")
        ),
        ComposeOutcome::Unknown { reason, .. } => {
            anyhow::bail!("not composed: {reason}; nothing stored")
        }
    }
}
