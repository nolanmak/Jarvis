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

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use augmentagent_autonomy::writer::{read_entries, render, write_contacts};
use augmentagent_autonomy::{
    config_path, load_config, normalize_handle, registry, ContactEntry, Handle, LoadReport,
    TrustState,
};
use augmentagent_store::Store;
use augmentagent_wiki::{IdentityIndex, WikiLayout};
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
    let path = config_path();
    let now = chrono::Utc::now().timestamp_millis();
    match op {
        Op::Add { slug, tier, handles, calendar_email, allow, daily_cap, preferred_channel } => {
            let interactive = handles.is_empty();
            if interactive {
                require_tty("add")?;
            }
            let entries = read_entries(&path)?;
            if entries.iter().any(|e| e.wiki == slug) {
                bail!(
                    "`{slug}` is already in {}; use `trust sync {slug}` or `trust remove {slug}` first",
                    path.display()
                );
            }
            let (handles, calendar_email) = if interactive {
                let wiki = wiki_dir.context("interactive `trust add` reads the wiki page: pass --wiki-dir")?;
                let candidates = wiki_handles(wiki, &slug)?;
                if candidates.is_empty() {
                    bail!("people/{slug}.md lists no email or iMessage identities; pass --handle explicitly");
                }
                println!("Handles from people/{slug}.md:");
                for h in &candidates {
                    println!("  {h}");
                }
                let mut chosen = Vec::new();
                for h in candidates {
                    if ask(&format!("Trust {h} as {slug}?"))? {
                        chosen.push(h.canonical());
                    }
                }
                if chosen.is_empty() {
                    bail!("no handles accepted; nothing written");
                }
                let calendar_email = match calendar_email {
                    Some(c) => Some(c),
                    None => choose_calendar_email(&chosen)?,
                };
                (chosen, calendar_email)
            } else {
                for h in &handles {
                    Handle::parse_canonical(h).map_err(|e| {
                        anyhow!("{e}; handles must be canonical (`email:<lowercased address>` or `imessage:<E.164 | lowercased Apple ID>`)")
                    })?;
                }
                (handles, calendar_email)
            };
            let entry = ContactEntry {
                wiki: slug.clone(),
                tier: Some(tier),
                allowed_actions: allow,
                handles,
                calendar_email,
                preferred_channel,
                daily_cap,
            };
            let mut next = entries;
            next.push(entry);
            save(&path, &next, &slug)?;
            println!(
                "Added `{slug}` to {}. It is pending: nothing is trusted until `augmentagent trust confirm {slug}`.",
                path.display()
            );
            Ok(())
        }
        Op::Sync { slug } => {
            require_tty("sync")?;
            let wiki = wiki_dir.context("`trust sync` reads the wiki page: pass --wiki-dir")?;
            let mut entries = read_entries(&path)?;
            let idx = entries
                .iter()
                .position(|e| e.wiki == slug)
                .ok_or_else(|| anyhow!("`{slug}` is not in {}", path.display()))?;
            let from_wiki: Vec<String> = wiki_handles(wiki, &slug)?.iter().map(Handle::canonical).collect();
            let current = entries[idx].handles.clone();
            let additions: Vec<&String> = from_wiki.iter().filter(|h| !current.contains(h)).collect();
            let removals: Vec<&String> = current.iter().filter(|h| !from_wiki.contains(h)).collect();
            if additions.is_empty() && removals.is_empty() {
                println!("`{slug}` already matches people/{slug}.md; nothing to change.");
                return Ok(());
            }
            let mut handles = current.clone();
            for h in additions {
                if ask(&format!("Add {h} to {slug}?"))? {
                    handles.push(h.clone());
                }
            }
            for h in removals {
                if ask(&format!("Remove {h} from {slug} (no longer on the wiki page)?"))? {
                    handles.retain(|x| x != h);
                }
            }
            if handles == current {
                println!("No changes accepted; nothing written.");
                return Ok(());
            }
            entries[idx].handles = handles;
            save(&path, &entries, &slug)?;
            println!("Updated `{slug}`. It is pending until `augmentagent trust confirm {slug}`.");
            Ok(())
        }
        Op::List { json } => list(store, &load_config(&path), json),
        Op::Remove { slug } => {
            let mut entries = read_entries(&path)?;
            let before = entries.len();
            entries.retain(|e| e.wiki != slug);
            if entries.len() == before {
                bail!("`{slug}` is not in {}", path.display());
            }
            write_contacts(&path, &entries)?;
            store.forget_trust(&slug)?;
            println!("Removed `{slug}` from {} and forgot its confirmation.", path.display());
            Ok(())
        }
        Op::Confirm { slug } => {
            let report = load_config(&path);
            if let Some(fatal) = &report.fatal {
                bail!("{} cannot be loaded: {fatal}", path.display());
            }
            if let Some(p) = report.problems.iter().find(|p| p.slug.as_deref() == Some(slug.as_str())) {
                bail!("{p}; fix the entry before confirming");
            }
            let changed = augmentagent_autonomy::confirm_contact(store, &report, &slug, now)?;
            if changed {
                println!("Confirmed `{slug}`: it is now an active trusted contact.");
            } else {
                println!("`{slug}` was already confirmed.");
            }
            Ok(())
        }
        Op::Pause { slug } => {
            if load_config(&path).contact(&slug).is_none() {
                eprintln!("note: `{slug}` is not a loaded contact; pausing it anyway");
            }
            if store.pause_contact(&slug, now)? {
                println!("Paused `{slug}`.");
            } else {
                println!("`{slug}` was already paused.");
            }
            Ok(())
        }
        Op::Resume { slug } => {
            if store.resume_contact(&slug)? {
                println!("Resumed `{slug}`.");
            } else {
                println!("`{slug}` was not paused.");
            }
            Ok(())
        }
        Op::PauseAll => {
            store.set_autonomy_global_pause(true, now)?;
            println!("Autonomy paused for everyone (takes effect on the next decision).");
            Ok(())
        }
        Op::ResumeAll => {
            store.set_autonomy_global_pause(false, now)?;
            println!("Global autonomy pause cleared.");
            Ok(())
        }
    }
}

/// D3: `add`/`sync` prompt per handle and must not run unattended.
fn require_tty(verb: &str) -> Result<()> {
    if std::io::stdin().is_terminal() {
        return Ok(());
    }
    bail!(
        "`trust {verb}` asks you to confirm each handle and needs an interactive terminal (stdin is not a TTY). \
         To script setup, use `trust add <slug> --tier <tier> --handle <canonical handle>...`."
    )
}

fn ask(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn choose_calendar_email(handles: &[String]) -> Result<Option<String>> {
    for h in handles {
        if let Some(addr) = h.strip_prefix("email:") {
            if ask(&format!("Send calendar invites to {addr}?"))? {
                return Ok(Some(addr.to_string()));
            }
        }
    }
    Ok(None)
}

/// The page's email and iMessage/phone identities as canonical handles, in
/// page order, deduplicated. Values that don't normalise are reported and
/// skipped. The identity index only *suggests*; it never grants trust.
fn wiki_handles(wiki: &Path, slug: &str) -> Result<Vec<Handle>> {
    let index = IdentityIndex::build(&WikiLayout::new(wiki)).context("read wiki people/")?;
    let page = index
        .pages()
        .iter()
        .find(|p| p.slug == slug)
        .ok_or_else(|| anyhow!("no people/{slug}.md in {}", wiki.display()))?;
    let ids = &page.identities;
    let raw = ids
        .email
        .iter()
        .map(|v| ("email", v))
        .chain(ids.imessage.iter().map(|v| ("imessage", v)))
        .chain(ids.phone.iter().map(|v| ("imessage", v)));
    let mut out: Vec<Handle> = Vec::new();
    for (platform, value) in raw {
        match normalize_handle(platform, value) {
            Ok(h) if !out.contains(&h) => out.push(h),
            Ok(_) => {}
            Err(e) => eprintln!("skipping {platform} identity: {e}"),
        }
    }
    Ok(out)
}

/// Validate the file `entries` would produce and write it only when the
/// changed contact loads cleanly (no per-contact problem, no duplicate).
fn save(path: &Path, entries: &[ContactEntry], slug: &str) -> Result<()> {
    let text = render(entries)?;
    let report = LoadReport::from_text(path, &text);
    if let Some(fatal) = report.fatal {
        bail!("refusing to write an unloadable config: {fatal}");
    }
    if let Some(p) = report.problems.iter().find(|p| p.slug.as_deref() == Some(slug)) {
        bail!("refusing to write: {p}");
    }
    write_contacts(path, entries)?;
    Ok(())
}

fn list(store: &Store, report: &LoadReport, json: bool) -> Result<()> {
    let global_pause = store.autonomy_global_pause()?;
    let mut rows = Vec::new();
    for c in &report.contacts {
        // Read-only: never records state or the file hash (the daemon's
        // reconcile does), so listing can't mask a change.
        let state = store.trust_state(&c.slug, &c.entry_hash)?.unwrap_or(TrustState::Pending);
        rows.push(serde_json::json!({
            "slug": c.slug,
            "tier": c.tier.as_str(),
            "state": state.as_str(),
            "paused": store.contact_paused(&c.slug)?,
            "handles": c.handles.iter().map(Handle::canonical).collect::<Vec<_>>(),
            "allowed_actions": c.allowed_actions,
            "calendar_email": c.calendar_email,
            "preferred_channel": c.preferred_handle.canonical(),
            "daily_cap": c.daily_cap,
        }));
    }
    let problems: Vec<String> = report.problems.iter().map(ToString::to_string).collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": report.path.display().to_string(),
                "global_pause": global_pause,
                "contacts": rows,
                "problems": problems,
                "fatal": report.fatal,
                "registry": registry::LISTABLE_ACTIONS,
            }))?
        );
        return Ok(());
    }
    println!("trusted contacts: {}", report.path.display());
    if global_pause {
        println!("GLOBAL PAUSE is on (`augmentagent trust resume-all` to clear)");
    }
    if let Some(fatal) = &report.fatal {
        println!("NOT LOADED: {fatal}");
    }
    if rows.is_empty() {
        println!("(no contacts)");
    }
    for r in &rows {
        println!(
            "{}\t{}\t{}{}\t{}",
            r["slug"].as_str().unwrap_or_default(),
            r["tier"].as_str().unwrap_or_default(),
            r["state"].as_str().unwrap_or_default(),
            if r["paused"].as_bool() == Some(true) { " (paused)" } else { "" },
            r["handles"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_default(),
        );
    }
    for p in &problems {
        println!("DISABLED: {p}");
    }
    Ok(())
}
