//! Routes accepted phone turns by their durably recorded originating surface.
//! Registration is an in-process transport capability, never a model argument.
use crate::{
    ingress::{AgentError, ConversationAgent, SpeechSender},
    speculation::Speculative,
    Error,
};
use async_trait::async_trait;
use augmentagent_store::{phone::PhoneCall, Store, SurfacePlatform};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock, Weak},
};
use tokio_util::sync::CancellationToken;

pub struct AgentRouter {
    store: Arc<Store>,
    adapters: RwLock<HashMap<String, Arc<dyn ConversationAgent>>>,
}
pub struct Registration {
    router: Weak<AgentRouter>,
    platform: String,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(router) = self.router.upgrade() {
            router
                .adapters
                .write()
                .expect("phone adapters poisoned")
                .remove(&self.platform);
        }
    }
}
impl AgentRouter {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            store,
            adapters: RwLock::new(HashMap::new()),
        })
    }
    pub fn register(
        self: &Arc<Self>,
        platform: &str,
        agent: Arc<dyn ConversationAgent>,
    ) -> Result<Registration, Error> {
        SurfacePlatform::new(platform).map_err(|_| Error::InvalidArguments)?;
        let mut adapters = self.adapters.write().expect("phone adapters poisoned");
        if adapters.contains_key(platform) {
            return Err(Error::InvalidArguments);
        }
        adapters.insert(platform.into(), agent);
        Ok(Registration {
            router: Arc::downgrade(self),
            platform: platform.into(),
        })
    }
    fn adapter(&self, call: &PhoneCall) -> Result<Arc<dyn ConversationAgent>, AgentError> {
        let origin = self.store.phone_origin(&call.id).map_err(|_| AgentError)?;
        if call.conversation != origin.conversation.storage_key() {
            return Err(AgentError);
        }
        self.adapters
            .read()
            .map_err(|_| AgentError)?
            .get(origin.owner.account().platform().as_str())
            .cloned()
            .ok_or(AgentError)
    }
}
#[async_trait]
impl ConversationAgent for AgentRouter {
    async fn call_ended(&self, call: &PhoneCall, key: &str, text: &str) -> Result<(), AgentError> {
        self.adapter(call)?.call_ended(call, key, text).await
    }
    async fn mirror_transcript(&self, call: &PhoneCall, key: &str, text: &str) -> Result<(), AgentError> {
        self.adapter(call)?.mirror_transcript(call, key, text).await
    }
    async fn summarize_live_result(&self, call: &PhoneCall, result: &str) -> Result<String, AgentError> {
        self.adapter(call)?.summarize_live_result(call, result).await
    }

    async fn resume_task(&self, call: &PhoneCall, task_id: &str) -> Result<(), AgentError> {
        self.adapter(call)?.resume_task(call, task_id).await
    }

    async fn answer(
        &self,
        call: &PhoneCall,
        turn_id: &str,
        text: &str,
        speech: SpeechSender,
        cancel: CancellationToken,
    ) -> Result<String, AgentError> {
        self.adapter(call)?
            .answer(call, turn_id, text, speech, cancel)
            .await
    }
    async fn interrupted(&self, call: &PhoneCall, heard: &str) -> Result<(), AgentError> {
        self.adapter(call)?.interrupted(call, heard).await
    }
    fn speculate(&self, call: &PhoneCall, text: &str) -> Option<Speculative> {
        self.adapter(call).ok()?.speculate(call, text)
    }
    async fn answer_speculated(
        &self,
        call: &PhoneCall,
        turn_id: &str,
        text: &str,
        speech: SpeechSender,
        cancel: CancellationToken,
        speculation: Speculative,
    ) -> Result<String, AgentError> {
        self.adapter(call)?
            .answer_speculated(call, turn_id, text, speech, cancel, speculation)
            .await
    }
    async fn summarize_call(
        &self,
        call: &PhoneCall,
        request: &crate::memory::SummaryRequest,
    ) -> Result<crate::memory::SummaryDraft, AgentError> {
        self.adapter(call)?.summarize_call(call, request).await
    }
    async fn notify_owner(
        &self,
        origin: &augmentagent_store::phone::PhoneOrigin,
        key: &str,
        text: &str,
    ) -> Result<(), AgentError> {
        if origin.owner.account() != origin.conversation.account() {
            return Err(AgentError);
        }
        let adapter = self
            .adapters
            .read()
            .map_err(|_| AgentError)?
            .get(origin.owner.account().platform().as_str())
            .cloned()
            .ok_or(AgentError)?;
        adapter.notify_owner(origin, key, text).await
    }
}
