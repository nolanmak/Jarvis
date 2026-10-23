//! Trusted owner self-chat phone controls. Never submit command text to models.
use crate::{Jid, WaMessage};
use augmentagent_phone::{tools::ToolServer, Clock, SystemClock};
use augmentagent_store::{
    phone::PhoneOrigin, Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef,
    SurfacePlatform,
};
use std::sync::Arc;

pub(crate) struct PhoneControl {
    store: Arc<Store>,
    tools: Arc<ToolServer>,
    linked_owner: Jid,
    origin: PhoneOrigin,
}
impl PhoneControl {
    /// `linked_owner` must come from the authenticated sidecar status, never
    /// command text or a message's claimed sender. Stored links are still checked
    /// on every command; this constructor does not create authority.
    pub fn new(
        store: Arc<Store>,
        tools: Arc<ToolServer>,
        linked_owner: Jid,
        chat: &str,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            linked_owner.is_personal()
                && !linked_owner.user().is_empty()
                && linked_owner.user().bytes().all(|c| c.is_ascii_digit())
                && linked_owner.bare() == chat,
            "phone controls require the linked owner's self-chat"
        );
        let account = SurfaceAccountRef::new(
            SurfacePlatform::new("whatsapp")?,
            format!("device:{}", linked_owner.user()),
        )?;
        let origin = PhoneOrigin {
            owner: SurfaceOwnerRef::new(account.clone(), linked_owner.bare())?,
            conversation: SurfaceConversationRef::new(account, linked_owner.bare(), None)?,
        };
        Ok(Self {
            store,
            tools,
            linked_owner,
            origin,
        })
    }
    pub fn authorized_origin(&self, message: &WaMessage) -> Option<(PhoneOrigin, i64)> {
        if !message.is_verified_self_command(&self.linked_owner) {
            return None;
        }
        let _grant = self
            .tools
            .grant(&self.origin.owner, &self.origin.conversation, None)
            .ok()?;
        let binding = self
            .store
            .surface_owner_binding(self.origin.owner.account())
            .ok()??;
        Some((self.origin.clone(), binding.revision))
    }

    pub async fn execute(&self, message: &WaMessage) -> anyhow::Result<Option<String>> {
        if !message.is_verified_self_command(&self.linked_owner) {
            return Ok(None);
        }
        if augmentagent_phone::commands::sets_number(&message.text) {
            if let Err(error) = self
                .tools
                .initialize_whatsapp_private_owner(&self.origin.owner, &self.origin.conversation)
            {
                return Ok(Some(error.to_string()));
            }
        }
        // Do not let a rejected sender consume a legitimate provider event ID.
        if self
            .tools
            .grant(&self.origin.owner, &self.origin.conversation, None)
            .is_err()
        {
            return Ok(Some(
                "Owner authorization required for phone commands.".into(),
            ));
        }
        let event_id = message.stable_id();
        if !self.store.consume_private_surface_command(
            &self.origin.conversation,
            &event_id,
            SystemClock.now_ms(),
        )? {
            return Ok(Some("Phone command already received. Check `phone status` or `phone number status` before sending a new command.".into()));
        }
        // The service rechecks authority after the durable receipt and before
        // side effects. An interrupted/uncertain command is never retried here.
        Ok(self
            .tools
            .command(
                &self.origin.owner,
                &self.origin.conversation,
                &event_id,
                &message.text,
            )
            .await)
    }
}
