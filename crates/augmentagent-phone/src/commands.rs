//! Structured setup commands. Adapters intercept these before logging message
//! bodies, history retrieval, durable inbox payloads, or agent submission.
use crate::PhoneService;
use augmentagent_store::{SurfaceConversationRef, SurfaceOwnerRef};
use serde_json::json;

fn words(text: &str) -> Option<Vec<&str>> {
    let body = text.trim().trim_start_matches(['!', '/']);
    // Only complete, immediate requests are aliases. Conditional requests,
    // quoted text, scheduling, and arbitrary destinations stay with the agent.
    let request = body.trim_end_matches(['!', '.', '?']).trim();
    if ["call me", "please call me", "call me please"]
        .iter()
        .any(|alias| request.eq_ignore_ascii_case(alias))
    {
        return Some(vec!["phone", "call"]);
    }
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
const SETUP_HELP: &str = "In your private chat with the agent, send `phone number set +<country code><number>`, answer the verification call, then send `phone number verify <six-digit code>` in that same private chat. After verification, say ‘call me’.";

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
        Err(crate::Error::Unauthorized) => return Some(format!("Phone owner authorization is required in this chat. {SETUP_HELP}")),
        Err(e) => return Some(e.to_string()),
    };
    let result=match w.as_slice() {
        [_,"call"]=>service.tool(&grant,"call_owner",json!({"request_id":format!("command:{event_id}")})).await.map(|v|format!("Phone call {}: {}.",v["id"].as_str().unwrap_or("unknown"),v["state"].as_str().unwrap_or("unknown"))),
        [_,"status"]=>call_status(service, &grant),
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
    Some(result.unwrap_or_else(|e| match e {
        crate::Error::Store(augmentagent_store::phone::PhoneError::NotConfigured) =>
            format!("No verified phone number is configured. {SETUP_HELP}"),
        _ => e.to_string(),
    }))
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

fn call_status(service: &PhoneService, grant: &crate::Grant) -> Result<String, crate::Error> {
    let Some(call) = service.latest_call(grant)? else {
        return Ok("No phone calls yet.".into());
    };
    let maintenance = service.store.phone_maintenance_status(&call.owner, &call.id)?;
    let mut reply = format!("Phone call {}: {}{}.", call.id, call.state.as_str(),
        if call.stop_requested { " (stop requested)" } else { "" });
    if !call.state.terminal() {
        if call.state == augmentagent_store::phone::CallState::Uncertain && !maintenance.provider_sid_known {
            reply.push_str(" Missing provider confirmation. Ask the operator to investigate before trying another call.");
        } else if call.stop_requested && maintenance.cleanup_attempts >= 3 {
            reply.push_str(" All automatic cleanup attempts have been issued; the last may still be pending. If the call remains active, ask the operator to check and end it with the phone provider.");
        }
    } else if !maintenance.final_usage_recorded && maintenance.usage_attempts >= 3 {
        reply.push_str(" Provider final usage is still missing after all automatic checks were issued. The call allowance remains reserved until usage is confirmed; ask the operator to investigate.");
    }
    Ok(reply)
}
