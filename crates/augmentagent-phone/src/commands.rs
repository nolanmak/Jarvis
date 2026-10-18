//! Structured setup commands. Adapters intercept these before logging message
//! bodies, history retrieval, durable inbox payloads, or agent submission.
use crate::PhoneService;
use augmentagent_store::{SurfaceConversationRef, SurfaceOwnerRef};
use serde_json::json;

fn words(text: &str) -> Option<Vec<&str>> {
    let body = text.trim().trim_start_matches(['!', '/']);
    let words: Vec<_> = body.split_whitespace().collect();
    words
        .first()
        .is_some_and(|w| w.eq_ignore_ascii_case("phone"))
        .then_some(words)
}
pub fn sets_number(text: &str) -> bool {
    words(text).is_some_and(|w| matches!(w.as_slice(), [_, "number", "set", _]))
}
pub fn recognizes(text: &str) -> bool {
    words(text).is_some()
}
pub fn sensitive(text: &str) -> bool {
    words(text).is_some_and(|w| w.get(1).is_some_and(|w| w.eq_ignore_ascii_case("number")))
}
pub const HELP:&str="Phone: `phone call`, `phone status`, `phone stop`. In your private chat: `phone number set +<country code><number>`, `phone number verify <six-digit code>`, `phone number status`, `phone number remove`.";

pub async fn execute(
    service: &PhoneService,
    owner: &SurfaceOwnerRef,
    conversation: &SurfaceConversationRef,
    event_id: &str,
    text: &str,
) -> Option<String> {
    let w = words(text)?;
    let grant = match service.authorize(owner, conversation) {
        Ok(g) => g,
        Err(e) => return Some(e.to_string()),
    };
    let result=match w.as_slice() {
        [_,"call"]=>service.tool(&grant,"call_owner",json!({"request_id":format!("command:{event_id}")})).await.map(|v|format!("Phone call {}: {}.",v["id"].as_str().unwrap_or("unknown"),v["state"].as_str().unwrap_or("unknown"))),
        [_,"status"]=>service.latest_call(&grant).map(|c|match c {Some(c)=>format!("Phone call {}: {}{}.",c.id,c.state.as_str(),if c.stop_requested {" (stop requested)"}else{""}),None=>"No phone calls yet.".into()}),
        [_,"stop"]=>match service.latest_call(&grant) {
            Ok(Some(call))=>service.tool(&grant,"phone_hangup",json!({"call_id":call.id})).await.map(|_|"Phone hangup requested.".into()),
            Ok(None)=>Ok("No phone calls to stop.".into()),Err(e)=>Err(e),
        },
        [_,"number","set",number]=>service.set_number(&grant,&format!("setup:{event_id}"),number).await.map(|_|"Verification call requested. Enter the six-digit code with `phone number verify <code>` here. Your previous verified number stays active until verification succeeds.".into()),
        [_,"number","verify",code]=>service.verify_number(&grant,code).map(|ok|if ok {"Phone number verified. You can now say ‘call me’."}else{"Code is incorrect, expired or already used. Check the code or start number setup again."}.into()),
        [_,"number","status"]=>service.number_status(&grant).map(|v|format!("Phone number: {}{}.",v["masked"].as_str().unwrap_or("not configured"),if v["pending"]==true {"; verification pending"}else{""})),
        [_,"number","remove"]=>service.remove_number(&grant).await.map(|_|"Phone number removed; pending or active calls have been stopped or marked for cleanup.".into()),
        _=>return Some(HELP.into()),
    };
    Some(result.unwrap_or_else(|e| e.to_string()))
}

/// Detect private commands in provider payloads too (edited messages and block
/// text can carry the original code). Used before durable ingestion, never to
/// grant authority or execute instructions from nested content.
pub fn contains_sensitive_text(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => sensitive(text),
        serde_json::Value::Array(items) => items.iter().any(contains_sensitive_text),
        serde_json::Value::Object(fields) => fields.values().any(contains_sensitive_text),
        _ => false,
    }
}
