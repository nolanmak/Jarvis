//! Immediate owner chat with durable admission and replies. No poll cadence or jitter.
use crate::{api::WaClient, control::chunk_for_whatsapp, owner, types::WaMessage};
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::{
    model_selection::{config_path, SelectionStore, CONVERSATION_SELECTION},
    surface_turn::{run_surface_turn, SurfaceTurnOutcome, SurfaceTurnRequest},
};
use augmentagent_store::{
    Store, SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform, SurfaceTurnRef,
};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

pub struct WhatsappInteractive {
    pub store: Arc<Store>,
    pub phone: String,
    pub wiki_root: PathBuf,
    pub agent: Arc<dyn QueryHandler>,
}

impl WhatsappInteractive {
    /// Reload binding for every message, including replies retried after restart.
    pub fn authorized(&self, message: &WaMessage) -> anyhow::Result<bool> {
        Ok(self
            .store
            .whatsapp_owner_config(&self.phone)?
            .is_some_and(|binding| owner::admits(&binding, message)))
    }

    pub async fn answer(
        &self,
        message: &WaMessage,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Option<String>> {
        if !self.authorized(message)? {
            return Ok(None);
        }
        let chat = message.chat.bare();
        match self
            .store
            .whatsapp_control_reply(&self.phone, &chat, &message.id)?
        {
            Some(Some(reply)) => return Ok(Some(reply)),
            Some(None) => {
                let turn = self.turn_ref(message)?;
                if self.store.surface_turn_state(&turn)?.is_some() {
                    self.store.resolve_surface_turn(
                        &turn,
                        augmentagent_store::SurfaceTurnResolution::Interrupted,
                    )?;
                }
                let reply="Jarvis restarted during this request. It has not been run again because some work may already have happened. Send a new message to continue.";
                self.store
                    .finish_whatsapp_control_turn(&self.phone, &chat, &message.id, reply)?;
                return Ok(Some(reply.into()));
            }
            None => {}
        }
        if !self
            .store
            .claim_whatsapp_control_turn(&self.phone, &chat, &message.id)?
        {
            return Ok(None);
        }
        let reply = if matches!(
            message.text.trim().to_ascii_lowercase().as_str(),
            "reset" | "new"
        ) {
            self.store
                .reset_surface_conversation(self.turn_ref(message)?.conversation())?;
            "Started a new conversation. Earlier work has not been undone.".into()
        } else if message.text.trim().eq_ignore_ascii_case("help") {
            "Send a question to talk to Jarvis. Use cancel to stop a running request, or reset to start a new conversation. This connection currently supports text chat; files and approval cards are not enabled yet.".into()
        } else if message.text.trim().is_empty() {
            "This message has no text. Media input is not enabled on this WhatsApp connection yet."
                .into()
        } else {
            match self.query(message, cancel).await {
                Ok(SurfaceTurnOutcome::Answered(reply)) => reply,
                Ok(SurfaceTurnOutcome::Cancelled) => {
                    "Stopped. This request will not run again; work already completed stays done."
                        .into()
                }
                Err(error) => {
                    tracing::warn!("WhatsApp owner query failed: {error:#}");
                    "The request stopped before an answer was ready. It will not run again automatically. Check Jarvis status before continuing.".into()
                }
            }
        };
        self.store
            .finish_whatsapp_control_turn(&self.phone, &chat, &message.id, &reply)?;
        Ok(Some(reply))
    }

    fn turn_ref(&self, message: &WaMessage) -> anyhow::Result<SurfaceTurnRef> {
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new("whatsapp")?, self.phone.clone())?;
        let conversation = SurfaceConversationRef::new(account, message.chat.bare(), None)?;
        Ok(SurfaceTurnRef::new(
            conversation,
            format!(
                "whatsapp:{}:{}:{}",
                self.phone,
                message.chat.bare(),
                message.id
            ),
        )?)
    }

    async fn query(
        &self,
        message: &WaMessage,
        cancel: &CancellationToken,
    ) -> anyhow::Result<SurfaceTurnOutcome> {
        let turn = self.turn_ref(message)?;
        let conversation = turn.conversation();
        let selected = if self.store.surface_conversation(&conversation)?.is_some() {
            None
        } else {
            SelectionStore::new(config_path()).selected(None)?
        };
        let ctx = AuditCtx {
            session_id: turn.storage_key(),
            guild_id: None,
            http: None,
            channel_id: None,
            owner_authorized: true,
        };
        let cwd = self.wiki_root.to_string_lossy();
        CONVERSATION_SELECTION
            .scope(
                selected,
                run_surface_turn(
                    &self.store,
                    SurfaceTurnRequest {
                        turn: &turn,
                        history: "",
                        current: message.text.trim(),
                        cwd: &cwd,
                    },
                    || Ok(selected),
                    Some(cancel),
                    |prompt| async move { self.agent.answer(&ctx, &prompt).await },
                ),
            )
            .await
    }

    /// Stable chunk keys survive a daemon crash between send acknowledgement and
    /// marking the input processed. The sidecar refuses ambiguous resends.
    pub async fn deliver(
        &self,
        client: &WaClient,
        message: &WaMessage,
        reply: &str,
    ) -> anyhow::Result<()> {
        if !self.authorized(message)? {
            anyhow::bail!("WhatsApp owner binding changed before delivery");
        }
        if !crate::control_enabled()
            || !self
                .store
                .is_whatsapp_outbound_allowed(&message.chat.bare())?
        {
            anyhow::bail!("WhatsApp outbound control is disabled");
        }
        for (index, chunk) in chunk_for_whatsapp(reply).iter().enumerate() {
            let key = format!("reply:{}:{}:{index}", message.chat.bare(), message.id);
            client
                .send_text_once(&message.chat.bare(), chunk, &key)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Agent(AtomicUsize);
    #[async_trait::async_trait]
    impl QueryHandler for Agent {
        async fn answer(&self, ctx: &AuditCtx, prompt: &str) -> anyhow::Result<String> {
            assert!(ctx.owner_authorized);
            assert!(ctx.session_id.contains("whatsapp"));
            assert!(ctx.http.is_none());
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(format!("answer: {prompt}"))
        }
    }
    fn message() -> WaMessage {
        serde_json::from_value(
            serde_json::json!({"id":"m1","chat":"15550000000@s.whatsapp.net",
            "sender":"15550000000:9@s.whatsapp.net","timestamp":1,"text":"hello",
            "from_me":true,"origin_verified":true,"account_jid":"15550000000:3@s.whatsapp.net"}),
        )
        .unwrap()
    }
    fn setup() -> (tempfile::TempDir, WhatsappInteractive, Arc<Agent>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("db")).unwrap());
        store
            .upsert_whatsapp_device(
                "15550000000",
                "15550000000:3@s.whatsapp.net",
                "15550000000@s.whatsapp.net",
            )
            .unwrap();
        store
            .set_whatsapp_owner_config(&augmentagent_store::WhatsappOwnerConfig {
                phone: "15550000000".into(),
                owner_jid: "15550000000@s.whatsapp.net".into(),
                control_chat_jid: "15550000000@s.whatsapp.net".into(),
                mode: "self_chat".into(),
            })
            .unwrap();
        let agent = Arc::new(Agent(AtomicUsize::new(0)));
        let surface = WhatsappInteractive {
            store,
            phone: "15550000000".into(),
            wiki_root: dir.path().into(),
            agent: agent.clone(),
        };
        (dir, surface, agent)
    }
    #[tokio::test]
    async fn owner_query_records_answer_before_delivery_and_replay_never_calls_agent_twice() {
        let (_dir, surface, agent) = setup();
        let message = message();
        let cancel = CancellationToken::new();
        let first = surface.answer(&message, &cancel).await.unwrap();
        assert_eq!(first, Some("answer: hello".into()));
        assert_eq!(surface.answer(&message, &cancel).await.unwrap(), first);
        assert_eq!(agent.0.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn interrupted_claim_is_not_submitted_again() {
        let (_dir, surface, agent) = setup();
        let message = message();
        surface
            .store
            .claim_whatsapp_control_turn(&surface.phone, &message.chat.bare(), &message.id)
            .unwrap();
        let answer = surface
            .answer(&message, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert!(answer.contains("not been run again"));
        assert_eq!(agent.0.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn another_account_or_agent_echo_never_reaches_the_agent() {
        let (_dir, surface, agent) = setup();
        let mut message = message();
        message.metadata.agent_generated = true;
        assert_eq!(
            surface
                .answer(&message, &CancellationToken::new())
                .await
                .unwrap(),
            None
        );
        message.metadata.agent_generated = false;
        message.metadata.account_jid = "15551111111@s.whatsapp.net".into();
        assert_eq!(
            surface
                .answer(&message, &CancellationToken::new())
                .await
                .unwrap(),
            None
        );
        assert_eq!(agent.0.load(Ordering::SeqCst), 0);
    }
}
