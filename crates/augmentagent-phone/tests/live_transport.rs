use augmentagent_phone::live_transport::{
    MediaStream, StreamError, StreamInput, MAX_PENDING_BYTES,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

fn connected() -> MediaStream {
    let mut stream = MediaStream::new("AC-test", "CA-test");
    assert!(matches!(
        stream
            .receive(&json!({"event":"connected","protocol":"Call","version":"1.0.0"}).to_string())
            .unwrap(),
        StreamInput::Ignored
    ));
    stream
}
fn start() -> Value {
    json!({"event":"start","sequenceNumber":"1","streamSid":"MZ-test","start":{
        "accountSid":"AC-test","callSid":"CA-test","streamSid":"MZ-test","tracks":["inbound"],
        "mediaFormat":{"encoding":"audio/x-mulaw","sampleRate":8000,"channels":1}}})
}
fn active() -> MediaStream {
    let mut stream = connected();
    assert!(matches!(
        stream.receive(&start().to_string()).unwrap(),
        StreamInput::Started
    ));
    stream
}
#[test]
fn binds_identity_codec_and_single_start_before_audio() {
    for (field, value) in [("accountSid", "AC-other"), ("callSid", "CA-other")] {
        let mut stream = connected();
        let mut event = start();
        event["start"][field] = json!(value);
        assert_eq!(
            stream.receive(&event.to_string()).err(),
            Some(StreamError::Identity)
        );
    }
    let mut stream = connected();
    let mut event = start();
    event["start"]["mediaFormat"]["sampleRate"] = json!(16000);
    assert_eq!(
        stream.receive(&event.to_string()).err(),
        Some(StreamError::Codec)
    );
    let mut stream = active();
    assert!(
        stream.receive(&start().to_string()).is_err(),
        "no second stream start"
    );
    assert_eq!(
        MediaStream::new("AC-test", "CA-test")
            .output(&[255; 160])
            .err(),
        Some(StreamError::State)
    );
}
#[test]
fn ordered_mulaw_audio_round_trips_without_resampling() {
    let mut stream = active();
    let audio = vec![255; 160];
    let event = json!({"event":"media","sequenceNumber":"2","streamSid":"MZ-test",
        "media":{"track":"inbound","chunk":"1","timestamp":"0","payload":STANDARD.encode(&audio)}});
    match stream.receive(&event.to_string()).unwrap() {
        StreamInput::Audio(bytes) => assert_eq!(bytes, audio),
        _ => panic!("audio expected"),
    }
    let frames = stream.output(&audio).unwrap();
    assert_eq!(
        frames[0],
        json!({"event":"media","streamSid":"MZ-test","media":{"payload":STANDARD.encode(&audio)}})
    );
    assert_eq!(frames[1]["event"], "mark");
    assert!(
        stream.receive(&event.to_string()).is_err(),
        "replayed audio rejected"
    );
}
#[test]
fn clear_acknowledgments_do_not_claim_audio_was_played() {
    let mut stream = active();
    let first = stream.output(&[255; 160]).unwrap();
    assert_eq!(
        stream.clear().unwrap(),
        json!({"event":"clear","streamSid":"MZ-test"})
    );
    let mark = |sequence: &str, name: Value| {
        json!({"event":"mark","sequenceNumber":sequence,"streamSid":"MZ-test","mark":{"name":name}})
            .to_string()
    };
    assert!(matches!(
        stream
            .receive(&mark("2", first[1]["mark"]["name"].clone()))
            .unwrap(),
        StreamInput::Cleared { bytes: 160 }
    ));
    let second = stream.output(&[255; 160]).unwrap();
    assert!(matches!(
        stream
            .receive(&mark("3", second[1]["mark"]["name"].clone()))
            .unwrap(),
        StreamInput::Played { bytes: 160 }
    ));
}
#[test]
fn buffers_are_bounded_and_stop_never_means_owner_hangup() {
    let mut stream = active();
    for _ in 0..MAX_PENDING_BYTES / 160 {
        stream.output(&[255; 160]).unwrap();
    }
    assert_eq!(
        stream.output(&[255; 160]).err(),
        Some(StreamError::Capacity)
    );
    stream
        .receive(
            &json!({"event":"stop","sequenceNumber":"2","streamSid":"MZ-test",
        "stop":{"accountSid":"AC-test","callSid":"CA-test"}})
            .to_string(),
        )
        .unwrap();
    assert_eq!(stream.output(&[255; 160]).err(), Some(StreamError::State));
}
#[test]
fn rejects_wrong_stream_and_malformed_audio_without_exposing_payload() {
    let mut stream = active();
    let event = json!({"event":"media","sequenceNumber":"2","streamSid":"MZ-other",
        "media":{"track":"inbound","chunk":"1","timestamp":"0","payload":"secret"}});
    let error = stream.receive(&event.to_string()).err().unwrap();
    assert_eq!(error, StreamError::Identity);
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn malformed_input_poisoning_prevents_recovery_into_an_authorized_stream() {
    let mut stream = active();
    let event = json!({"event":"media","sequenceNumber":"2","streamSid":"MZ-test",
        "media":{"track":"inbound","chunk":"1","timestamp":"0","payload":"!invalid!"}});
    assert_eq!(
        stream.receive(&event.to_string()).err(),
        Some(StreamError::Malformed)
    );
    assert_eq!(stream.output(&[255; 160]).err(), Some(StreamError::State));
    assert_eq!(stream.clear().err(), Some(StreamError::State));
}

#[test]
fn repeated_clear_cannot_accumulate_unbounded_mark_tombstones() {
    let mut stream = active();
    let mut limited = false;
    for _ in 0..1024 {
        match stream.output(&[255]) {
            Ok(_) => {
                stream.clear().unwrap();
            }
            Err(StreamError::Capacity) => {
                limited = true;
                break;
            }
            Err(other) => panic!("unexpected {other}"),
        }
    }
    assert!(
        limited,
        "cleared marks need their own bound independent of pending bytes"
    );
}

#[test]
fn playback_ack_releases_capacity_but_unknown_marks_are_terminal() {
    let mut stream = active();
    let frames = stream.output(&vec![255; MAX_PENDING_BYTES]).unwrap();
    assert_eq!(stream.output(&[255]).err(), Some(StreamError::Capacity));
    let mark = json!({"event":"mark","sequenceNumber":"2","streamSid":"MZ-test",
        "mark":{"name":frames[1]["mark"]["name"]}});
    assert!(matches!(
        stream.receive(&mark.to_string()).unwrap(),
        StreamInput::Played {
            bytes: MAX_PENDING_BYTES
        }
    ));
    assert!(stream.output(&vec![255; MAX_PENDING_BYTES]).is_ok());
    let unknown = json!({"event":"mark","sequenceNumber":"3","streamSid":"MZ-test","mark":{"name":"unknown"}});
    assert_eq!(
        stream.receive(&unknown.to_string()).err(),
        Some(StreamError::Order)
    );
    assert_eq!(stream.output(&[255]).err(), Some(StreamError::State));
}
