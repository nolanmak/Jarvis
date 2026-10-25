//! `augmentagent google …` — connect Gmail mailboxes through the operator's
//! own Google OAuth client instead of Composio (#1573).
//!
//! The dashboard drives these from its `/oauth/google/*` routes so the code
//! exchange, token storage and account registration live in one place (the
//! Rust side, which is also what polls the mailbox). They work from a shell
//! too, for headless setups and for diagnosing a connection.

use std::io::Read;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;

use augmentagent_channel_email::google::{email_of, entity_id_for, GoogleGmail, OAuthApp};
use augmentagent_store::Store;

#[derive(Subcommand, Debug, Clone)]
pub enum GoogleOp {
    /// Print the Google consent URL that connects one mailbox. The dashboard
    /// redirects the browser to it; `state` is echoed back on the callback.
    AuthUrl {
        /// Must match a redirect URI registered on the OAuth client exactly.
        #[arg(long)]
        redirect_uri: String,
        /// Opaque CSRF token the caller verifies on the callback.
        #[arg(long)]
        state: String,
    },
    /// Finish a consent. Reads the authorization code from STDIN (never
    /// argv), exchanges it, stores the refresh token under the mailbox
    /// address and registers the mailbox for polling. Any other active row
    /// for the same address — a Composio connection to the same inbox — is
    /// deactivated so one transport polls it.
    Connect {
        /// The same redirect URI the consent was started with.
        #[arg(long)]
        redirect_uri: String,
        #[arg(long, default_value_t = false, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Revoke a mailbox's grant at Google, forget the token and stop polling
    /// the account.
    Disconnect {
        /// Mailbox address or `google:<address>` entity id.
        #[arg(long)]
        account: String,
    },
    /// List Google-connected mailboxes and whether each grant is usable.
    Accounts {
        #[arg(long, default_value_t = false, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Live check of one mailbox: mint an access token and read the profile.
    Check {
        /// Mailbox address or `google:<address>` entity id.
        #[arg(long)]
        account: String,
    },
}

pub async fn run(store: Arc<Store>, op: GoogleOp) -> Result<()> {
    match op {
        GoogleOp::AuthUrl {
            redirect_uri,
            state,
        } => {
            let app = OAuthApp::from_env()?;
            println!("{}", app.auth_url(&redirect_uri, &state));
            Ok(())
        }
        GoogleOp::Connect { redirect_uri, json } => connect(store, &redirect_uri, json).await,
        GoogleOp::Disconnect { account } => disconnect(store, &account).await,
        GoogleOp::Accounts { json } => accounts(store, json),
        GoogleOp::Check { account } => check(&account).await,
    }
}

fn mailbox_of(selector: &str) -> String {
    email_of(selector)
        .unwrap_or(selector)
        .trim()
        .to_ascii_lowercase()
}

async fn connect(store: Arc<Store>, redirect_uri: &str, json: bool) -> Result<()> {
    let mut code = String::new();
    std::io::stdin()
        .read_to_string(&mut code)
        .context("read the authorization code from stdin")?;
    let code = code.trim();
    anyhow::ensure!(!code.is_empty(), "no authorization code on stdin");

    let google = GoogleGmail::from_env()?;
    let token = google
        .connect(code, redirect_uri)
        .await
        .context("exchange the Google authorization code")?;
    let entity_id = store
        .upsert_google_gmail_account(&token.email)
        .context("register the mailbox")?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "email": token.email,
                "entity_id": entity_id,
                "scopes": token.scopes,
            })
        );
    } else {
        println!("connected {} as {entity_id}", token.email);
    }
    Ok(())
}

async fn disconnect(store: Arc<Store>, account: &str) -> Result<()> {
    let email = mailbox_of(account);
    let entity_id = entity_id_for(&email);
    // Stop polling first: a tick that raced the revoke would otherwise log a
    // revoked-grant error for a mailbox the operator just removed.
    let was_active = store.deactivate_gmail_account(&entity_id)?;
    let revoked = match GoogleGmail::from_env() {
        Ok(google) => google.disconnect(&email).await?,
        // No client configured any more: nothing to revoke with, but the
        // local grant still goes.
        Err(e) => {
            tracing::warn!("google client unavailable during disconnect: {e}");
            false
        }
    };
    println!(
        "{email}: polling {}; token {}",
        if was_active { "stopped" } else { "was not active" },
        if revoked { "revoked at Google and removed" } else { "removed locally" }
    );
    Ok(())
}

fn accounts(store: Arc<Store>, json: bool) -> Result<()> {
    let google = GoogleGmail::from_env().ok();
    let rows: Vec<serde_json::Value> = store
        .get_active_gmail_accounts()?
        .into_iter()
        .filter(|a| email_of(&a.entity_id).is_some())
        .map(|a| {
            let email = email_of(&a.entity_id).unwrap_or_default().to_string();
            let grant = match &google {
                None => "client-not-configured",
                Some(g) => match g.token(&email) {
                    Ok(t) if t.is_revoked() => "revoked",
                    Ok(_) => "stored",
                    Err(_) => "missing",
                },
            };
            serde_json::json!({
                "email": email,
                "entity_id": a.entity_id,
                "grant": grant,
            })
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("(no Google-connected mailboxes)");
        return Ok(());
    }
    for r in rows {
        println!("{}\t{}\t{}", r["email"].as_str().unwrap_or(""), r["entity_id"].as_str().unwrap_or(""), r["grant"].as_str().unwrap_or(""));
    }
    Ok(())
}

async fn check(account: &str) -> Result<()> {
    let email = mailbox_of(account);
    let google = GoogleGmail::from_env()?;
    let token = google.token(&email)?;
    if token.is_revoked() {
        anyhow::bail!(
            "{email}: grant revoked ({}); reconnect it from the dashboard",
            token.revoke_reason.as_deref().unwrap_or("invalid_grant")
        );
    }
    let profile = google.profile_email(&email).await?;
    println!("{email}: ok (Google reports {profile}; scopes: {})", token.scopes.join(" "));
    Ok(())
}
