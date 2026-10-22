use augmentagent_channel_slack::{
    catch_up::history_record,
    ingest::subscribed_event,
    surface::SlackWorkspace,
    transport::{
        event::{parse_envelope_value, Envelope},
        web::SlackHistoryMessage,
    },
};
use serde_json::json;

#[test]
fn catch_up_and_live_ingestion_exclude_private_phone_inputs_including_edits() {
    let workspace = SlackWorkspace::new("T0000001", None).unwrap();
    for text in [
        "phone number set +12025550973",
        "/PHONE NUMBER verify 654321",
    ] {
        let message = SlackHistoryMessage {
            ts: "1700000100.000001".into(),
            thread_ts: None,
            text: Some(text.into()),
            user: Some("U0000001".into()),
            bot_id: None,
            metadata: None,
            raw: json!({"type":"message","text":text}),
        };
        assert!(history_record(&workspace, "C0000001", &message).is_none());
        for event in [
            json!({"type":"message","channel":"C0000001","ts":message.ts,"text":text,"user":"U0000001"}),
            json!({"type":"message","subtype":"message_changed","channel":"C0000001","message":{"ts":message.ts,"text":text,"user":"U0000001"}}),
        ] {
            let frame = json!({"type":"events_api","envelope_id":"E-example","payload":{"team_id":"T0000001","event_id":"Ev-example","event":event}});
            let Envelope::Event(envelope) = parse_envelope_value(frame).unwrap() else {
                panic!()
            };
            assert!(subscribed_event(&envelope).is_none());
        }
    }
}
