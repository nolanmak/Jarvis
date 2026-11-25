//! #2246 — sends a loop's finding emails from a connected mail account.
//!
//! The scheduler has already checked the recipient against the owner's
//! allowlist; this only resolves the sending account and sends.
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use augmentagent_approval_discord::LoopMailer;
use augmentagent_channel_email::gmail::{ComposioClient, GmailApi};
use augmentagent_store::Store;

pub struct GmailLoopMailer {
    pub store: Arc<Store>,
}

#[async_trait]
impl LoopMailer for GmailLoopMailer {
    async fn send(
        &self,
        account: Option<&str>,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<String> {
        let (entity_id, email) =
            crate::resolve_gmail_entity_id(&self.store, account.map(str::to_string))?;
        let api_key =
            std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
        let gmail = ComposioClient::new(api_key);
        let draft_id = gmail
            .create_draft_with_attachment(&entity_id, to, subject, body, None, &[], &[], &[])
            .await
            .context("create_draft (loop delivery) failed")?;
        let sent_id = gmail
            .send_draft(&entity_id, &draft_id)
            .await
            .context("send_draft (loop delivery) failed")?;
        crate::record_self_send(&self.store, sent_id.as_deref(), None, Some(&entity_id), None);
        Ok(email)
    }
}
