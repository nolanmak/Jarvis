#![allow(dead_code)]
use augmentagent_channel_linkedin::LinkedInAuth;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use wiremock::{Request, ResponseTemplate};
pub const OWNER: &str = "urn:li:fsd_profile:OWNER";
pub fn auth() -> LinkedInAuth {
    LinkedInAuth {
        member_urn: OWNER.into(),
        cookies: BTreeMap::from([
            ("li_at".into(), "SYNTHETIC".into()),
            ("JSESSIONID".into(), "\"ajax:test\"".into()),
        ]),
        user_agent: "test".into(),
        harvested_at_ms: 0,
    }
}
pub fn group(i: usize) -> bool {
    [2, 22, 42].contains(&i)
}
pub fn conversation(i: usize) -> Value {
    let mut participants = vec![
        json!({"hostIdentityUrn":OWNER}),
        json!({"hostIdentityUrn":format!("person:{i}")}),
    ];
    if group(i) {
        participants.push(json!({"hostIdentityUrn":"introducer-1","participantType":{"member":{"firstName":{"text":"Introducer"}}}}));
    }
    json!({"entityUrn":format!("thread:{i}"),"title":format!("Topic {i}"),"groupChat":group(i),"lastActivityAt":90000-i,"conversationParticipants":participants,
        "messages":{"elements":[{"backendUrn":format!("message:{i}:{}",if group(i){8}else{2}),"actor":{"hostIdentityUrn":OWNER},"body":{"text":"Owner replied"}}]}})
}
pub fn body(i: usize, j: usize) -> String {
    let mut text = format!("bodymarker thread {i} message {j}");
    if i == 2 && j == 0 {
        text.push_str(&"🙂世界\n".repeat(2200));
    }
    text
}
pub fn respond(request: &Request) -> ResponseTemplate {
    assert_eq!(
        request.method.as_str(),
        "GET",
        "archive must never issue writes"
    );
    let q = request.url.query_pairs().collect::<BTreeMap<_, _>>();
    if request.url.path().ends_with("graphql") {
        let variables = &q["variables"];
        let start = if variables.contains("nextCursor:mailbox%3A20")
            || variables.contains("nextCursor:mailbox:20")
        {
            20
        } else if variables.contains("nextCursor:mailbox%3A40")
            || variables.contains("nextCursor:mailbox:40")
        {
            40
        } else {
            0
        };
        let end = (start + 20).min(45);
        ResponseTemplate::new(200).set_body_json(json!({"data":{"messengerConversationsByCategoryQuery":{
   "elements":(start..end).map(conversation).collect::<Vec<_>>(),"metadata":{"nextCursor":(end<45).then(||format!("mailbox:{end}"))}
  }}}))
    } else {
        let i = q["conversationUrn"]
            .strip_prefix("thread:")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let start = q
            .get("nextCursor")
            .map(|c| {
                c.strip_prefix("history:")
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
            .unwrap_or(0);
        let count = if group(i) { 9 } else { 3 };
        let end = (start + 4).min(count);
        let messages=(start..end).map(|j|json!({"backendUrn":format!("message:{i}:{j}"),"senderUrn":if j%3==2{OWNER.to_string()}else{format!("person:{i}")},"deliveredAt":1000+j/2,
   "body":{"text":body(i,j)}})).collect::<Vec<_>>();
        ResponseTemplate::new(200).set_body_json(json!({"elements":messages,"metadata":{"nextCursor":(end<count).then(||format!("history:{end}"))}}))
    }
}
