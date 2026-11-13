// Untrusted observer PROTOTYPE, not a complete correction or approval.
// Intended only for existing isolated/pinned verifier staging at original Git.
// This covers an inbound iMessage card path, not every original #2014 criterion.
use std::collections::VecDeque;
use std::sync::{Arc,Mutex};
use augmentagent_channel_core::{Reasoner,reasoner::ReasonerOpts};
use augmentagent_approval_discord::{ApprovalBroker,ApprovalError,approval_message,strip_assumes_for_send};
use augmentagent_channel_imessage::{Conversation,MessageEntry,PollDelta,ImessageReplier,ImessageReplyConfig,synthetic_imessage_email};
use augmentagent_store::{Store,Email};

struct DraftInput(Mutex<VecDeque<String>>);
#[async_trait::async_trait]
impl Reasoner for DraftInput {
    async fn call(&self,opts:&ReasonerOpts,_request:&str)->anyhow::Result<String> {
        // Script the provider boundary, not the production card/guard logic.
        if opts.system_prompt.starts_with("You select evidence for a messaging draft") {
            return Ok(r#"{"action":"ready","facts":[],"gaps":[]}"#.into());
        }
        self.0.lock().unwrap().pop_front().ok_or_else(||anyhow::anyhow!("unexpected provider request"))
    }
}
#[derive(Default)]
struct Capture {cards:Mutex<Vec<(String,String,serde_json::Value)>>,flags:Mutex<Vec<String>>}
#[async_trait::async_trait]
impl ApprovalBroker for Capture {
    async fn post_approval(&self,id:&str,email:&Email,draft:&str)->Result<(),ApprovalError> {
        // Render the actual production approval surface, then preserve data.
        let rendered=serde_json::to_value(approval_message(id,email,draft,0)).unwrap();
        self.cards.lock().unwrap().push((id.into(),draft.into(),rendered));
        Ok(())
    }
    async fn post_flag_notice(&self,_email:&Email,reason:&str)->Result<(),ApprovalError> {self.flags.lock().unwrap().push(reason.into());Ok(())}
}
#[tokio::main]
async fn main()->anyhow::Result<()> {
    let _root=std::env::current_dir()?;
    // Synthetic recipient only; no production store, credentials or network.
    let phone="+15555550100"; // pii-ok synthetic
    let directory=std::env::temp_dir().join(format!("autoship-observe-imessage-{}",std::process::id()));
    std::fs::create_dir(&directory)?;
    let store=Arc::new(Store::open(&directory.join("fixture.db"))?);
    store.allow_imessage_inbound(phone)?;
    let conversation=Conversation {identifier:phone.into(),dir:"fixture".into(),title:"Example Contact".into(),
        participants:vec![phone.into()],service:"iMessage".into(),..Default::default()};
    let entries=vec![(3,MessageEntry {timestamp:"2026-01-01T12:00:00Z".into(),sender:phone.into(),
        body:"Hello, could you send the proposal? Thursday still works for me.".into(),attachments:vec![]})];
    for (index,entry) in &entries {store.upsert_email(&synthetic_imessage_email(&conversation,*index,entry))?;}
    let delta=PollDelta {conversation,new_entries:entries,first_run:false};
    let capture=Arc::new(Capture::default());
    let replier=ImessageReplier {store:store.clone(),approvals:capture.clone(),
        reasoner:Arc::new(DraftInput(Mutex::new(VecDeque::from([
            r#"{"decision":"reply","reason":"direct question"}"#.to_owned(),
            "hello, sending the proposal tonight. thursday works for me.".to_owned()])))),
        config:ImessageReplyConfig::default()};
    let stats=replier.handle_deltas(&[delta]).await;
    let mut observations=Vec::new();
    for (id,draft,card) in capture.cards.lock().unwrap().iter() {
        let action=store.get_action_with_email(id)?.ok_or_else(||anyhow::anyhow!("posted card lacks action"))?;
        let fields=card.get("embeds").and_then(|v|v.get(0)).and_then(|v|v.get("fields")).cloned();
        observations.push(serde_json::json!({"status":action.action.status,"draft":draft,
            "rendered_fields":fields,"text_for_send":strip_assumes_for_send(draft)}));
    }
    println!("{}",serde_json::json!({"cards":stats.cards,"errors":stats.errors,"observations":observations,"flags":*capture.flags.lock().unwrap()}));
    drop(replier);drop(store);std::fs::remove_dir_all(directory)?;
    Ok(())
}
