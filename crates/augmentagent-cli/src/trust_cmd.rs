//! #1833 — `augmentagent trust …`: the owner-run CLI that is the ONLY
//! writer of `trusted-contacts.toml` (D3/NG4), plus confirm/pause controls
//! over the trust state kept in the store (D3/D10).
//!
//! - `add <slug> --tier <t>` reads `people/<slug>.md` `identities:` and asks
//!   y/n per handle; `sync <slug>` asks per difference. Both need a TTY on
//!   stdin and refuse otherwise.
//! - `add <slug> --tier <t> --handle <h>…` is the explicit, scriptable form:
//!   it writes exactly the given canonical handles without prompting.
//! - `list`, `remove`, `confirm`, `pause`, `resume`, `pause-all` and
//!   `resume-all` work non-interactively.
//!
//! Every write lands the contact as pending; nothing is trusted until
//! `trust confirm <slug>` (or the Discord Confirm button, C8).

use std::path::Path;

use anyhow::Result;
use augmentagent_store::Store;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum Op {
    /// Add a trusted contact. Interactive (from the wiki page, needs a TTY)
    /// unless `--handle` is given.
    Add {
        /// Wiki person slug (`people/<slug>.md`).
        slug: String,
        /// `actions` or `full` (required; no default).
        #[arg(long)]
        tier: String,
        /// Canonical handle (`email:<addr>` / `imessage:<E.164|apple-id>`).
        /// Repeatable. Given ⇒ non-interactive: exactly these are written.
        #[arg(long = "handle")]
        handles: Vec<String>,
        /// Calendar invite address; `email:<this>` must be one of the handles.
        #[arg(long)]
        calendar_email: Option<String>,
        /// Registry action for the `actions` tier. Repeatable.
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// Plans per rolling 24 h (default 10).
        #[arg(long)]
        daily_cap: Option<u32>,
        /// Handle for proactive outreach (default: first imessage, else first email).
        #[arg(long)]
        preferred_channel: Option<String>,
    },
    /// Re-read the wiki page's identities and confirm each change (needs a TTY).
    Sync { slug: String },
    /// Show every contact with its state, pauses and config problems.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Remove a contact from the config and forget its trust state.
    Remove { slug: String },
    /// Trust the contact's current entry (new or changed entries are pending).
    Confirm { slug: String },
    /// Stop autonomous actions for one contact.
    Pause { slug: String },
    /// Clear a contact's pause.
    Resume { slug: String },
    /// Live kill switch: stop all autonomous actions now.
    PauseAll,
    /// Clear the live kill switch.
    ResumeAll,
}

pub fn run(store: &Store, op: Op, wiki_dir: Option<&Path>) -> Result<()> {
    let _ = (store, op, wiki_dir);
    todo!()
}
