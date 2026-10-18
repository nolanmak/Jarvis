//! Transport-authenticated context for the current agent turn. Nothing here is
//! deserialized from a tool request or from ingested text.
use crate::ingress::SpeechSender;
use augmentagent_store::phone::PhoneOrigin;
#[derive(Clone)]
pub struct TurnContext {
    pub origin: PhoneOrigin,
    pub speech: Option<SpeechSender>,
}
tokio::task_local! {pub static CURRENT:TurnContext;}
pub fn current() -> Option<TurnContext> {
    CURRENT.try_with(Clone::clone).ok()
}
pub async fn scope<F: std::future::Future>(
    origin: PhoneOrigin,
    speech: Option<SpeechSender>,
    future: F,
) -> F::Output {
    CURRENT.scope(TurnContext { origin, speech }, future).await
}
