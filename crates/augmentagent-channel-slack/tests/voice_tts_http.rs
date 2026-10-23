//! #1297 — the Rust text-to-speech adapter for spoken Slack replies: the
//! same vendors, requests, voice and 24 kHz PCM output as the Discord voice
//! sidecar (`sidecars/discord-voice/src/tts.ts`), the same credential names
//! and the same vendor-fallback rule (switch only on confirmed credit
//! exhaustion), over request/response HTTP.
//!
//! Every vendor is a local `mockito` server (or a silent TCP listener for
//! the timeout); keys are synthetic and never reach a real provider.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use augmentagent_channel_slack::voice::providers::{
    tts_from_env, HttpTts, SpeechVendor, DEEPGRAM_KEY_ENV, DISCORD_TTS_PROVIDER_ENV,
    ELEVENLABS_KEY_ENV, ELEVENLABS_VOICE_ENV, SLACK_TTS_PROVIDER_ENV,
};
use augmentagent_channel_slack::voice::speech::{AudioFormat, TextToSpeech};
use augmentagent_channel_slack::voice::TtsStack;
use mockito::Matcher;

const DG_KEY: &str = "dg-test-key";
const EL_KEY: &str = "el-test-key";
const VOICE: &str = "voice-test";

fn pcm(ms: usize) -> Vec<u8> {
    // 24 kHz mono 16-bit: 48 bytes per millisecond.
    (0..ms * 48).map(|i| (i % 251) as u8).collect()
}

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| map.get(k).cloned()
}

async fn deepgram_mock(
    server: &mut mockito::ServerGuard,
    status: usize,
    body: Vec<u8>,
) -> mockito::Mock {
    server
        .mock("POST", "/v1/speak")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("model".into(), "aura-2-thalia-en".into()),
            Matcher::UrlEncoded("encoding".into(), "linear16".into()),
            Matcher::UrlEncoded("sample_rate".into(), "24000".into()),
            Matcher::UrlEncoded("container".into(), "none".into()),
        ]))
        .match_header("authorization", format!("Token {DG_KEY}").as_str())
        .match_header("content-type", "application/json")
        .with_status(status)
        .with_body(body)
        .create_async()
        .await
}

async fn elevenlabs_mock(
    server: &mut mockito::ServerGuard,
    status: usize,
    body: Vec<u8>,
) -> mockito::Mock {
    server
        .mock(
            "POST",
            format!("/v1/text-to-speech/{VOICE}/stream").as_str(),
        )
        .match_query(Matcher::UrlEncoded(
            "output_format".into(),
            "pcm_24000".into(),
        ))
        .match_header("xi-api-key", EL_KEY)
        .match_body(Matcher::PartialJson(
            serde_json::json!({"model_id": "eleven_flash_v2_5"}),
        ))
        .with_status(status)
        .with_body(body)
        .create_async()
        .await
}

#[tokio::test]
async fn deepgram_returns_24khz_pcm_like_the_sidecar() {
    let mut server = mockito::Server::new_async().await;
    let mock = deepgram_mock(&mut server, 200, pcm(250)).await;
    let tts = HttpTts::deepgram(DG_KEY).with_endpoint(server.url());

    let audio = tts.synthesize("Two meetings tomorrow.").await.unwrap();

    mock.assert_async().await;
    assert_eq!(tts.provider(), "deepgram");
    assert_eq!(
        audio.format,
        AudioFormat::Pcm16 {
            sample_rate: 24_000
        }
    );
    assert_eq!(audio.bytes, pcm(250));
    // Playable once wrapped (what the reply pipeline uploads).
    let wav = audio.to_file_bytes().unwrap();
    assert_eq!(&wav[..4], b"RIFF");
}

#[tokio::test]
async fn deepgram_long_text_is_sent_in_sidecar_sized_chunks_and_joined() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/speak")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(pcm(10))
        .expect(3)
        .create_async()
        .await;
    let tts = HttpTts::deepgram(DG_KEY).with_endpoint(server.url());
    let text = "word ".repeat(800); // 4000 chars: three 1800-char requests

    let audio = tts.synthesize(&text).await.unwrap();

    mock.assert_async().await;
    assert_eq!(audio.bytes.len(), 3 * pcm(10).len());
}

#[tokio::test]
async fn elevenlabs_streams_pcm_with_the_sidecar_model_and_voice() {
    let mut server = mockito::Server::new_async().await;
    let mock = elevenlabs_mock(&mut server, 200, pcm(100)).await;
    let tts = HttpTts::elevenlabs(EL_KEY, VOICE).with_endpoint(server.url());

    let audio = tts.synthesize("Hello there.").await.unwrap();

    mock.assert_async().await;
    assert_eq!(tts.provider(), "elevenlabs");
    assert_eq!(audio.bytes, pcm(100));
}

#[tokio::test]
async fn credit_exhaustion_402_switches_to_the_other_vendor() {
    let mut server = mockito::Server::new_async().await;
    let dg = deepgram_mock(
        &mut server,
        402,
        b"{\"err_code\":\"ASR_PAYMENT_REQUIRED\"}".to_vec(),
    )
    .await;
    let el = elevenlabs_mock(&mut server, 200, pcm(50)).await;
    let stack = TtsStack::new(Arc::new(
        HttpTts::deepgram(DG_KEY).with_endpoint(server.url()),
    ))
    .with_alternate(Arc::new(
        HttpTts::elevenlabs(EL_KEY, VOICE).with_endpoint(server.url()),
    ));

    let (audio, used) = stack.synthesize("Hello.").await.unwrap();

    dg.assert_async().await;
    el.assert_async().await;
    assert_eq!(used.provider, "elevenlabs");
    assert_eq!(used.switched_from.as_deref(), Some("deepgram"));
    assert_eq!(audio.bytes, pcm(50));
}

#[tokio::test]
async fn elevenlabs_quota_exceeded_is_exhaustion_too() {
    let mut server = mockito::Server::new_async().await;
    let body = serde_json::json!({"detail": {"status": "quota_exceeded", "message": "no credits"}});
    let _el = elevenlabs_mock(&mut server, 401, body.to_string().into_bytes()).await;
    let tts = HttpTts::elevenlabs(EL_KEY, VOICE).with_endpoint(server.url());

    let err = tts.synthesize("Hello.").await.unwrap_err();

    assert_eq!(err.code, "quota_exceeded");
    assert!(err.exhausted());
}

#[tokio::test]
async fn a_server_error_is_reported_and_never_switches_vendor() {
    let mut server = mockito::Server::new_async().await;
    let dg = deepgram_mock(&mut server, 503, b"upstream down".to_vec()).await;
    let el = elevenlabs_mock(&mut server, 200, pcm(50)).await.expect(0);
    let stack = TtsStack::new(Arc::new(
        HttpTts::deepgram(DG_KEY).with_endpoint(server.url()),
    ))
    .with_alternate(Arc::new(
        HttpTts::elevenlabs(EL_KEY, VOICE).with_endpoint(server.url()),
    ));

    let failure = stack.synthesize("Hello.").await.unwrap_err();

    dg.assert_async().await;
    el.assert_async().await;
    assert_eq!(failure.error.provider, "deepgram");
    assert_eq!(failure.error.code, "503");
    assert_eq!(failure.switched_from, None);
    assert_eq!(failure.owner_summary(), "deepgram failed (HTTP 503)");
    // Owner-safe: no key, no URL.
    let shown = format!("{} {}", failure.error, failure.owner_summary());
    assert!(
        !shown.contains(DG_KEY) && !shown.contains("127.0.0.1"),
        "{shown}"
    );
}

#[tokio::test]
async fn a_provider_that_never_answers_times_out() {
    // Accepts the connection and never replies.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hold = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    let tts = HttpTts::deepgram(DG_KEY)
        .with_endpoint(format!("http://{addr}"))
        .with_timeout(Duration::from_millis(300));

    let started = std::time::Instant::now();
    let err = tts.synthesize("Hello.").await.unwrap_err();

    assert_eq!(err.code, "timeout");
    assert!(!err.exhausted());
    assert!(started.elapsed() < Duration::from_secs(5));
    hold.abort();
}

#[tokio::test]
async fn empty_audio_from_the_provider_is_an_error() {
    let mut server = mockito::Server::new_async().await;
    let _dg = deepgram_mock(&mut server, 200, Vec::new()).await;
    let tts = HttpTts::deepgram(DG_KEY).with_endpoint(server.url());
    let err = tts.synthesize("Hello.").await.unwrap_err();
    assert_eq!(err.code, "no_audio");
}

// ---------------------------------------------------------------------------
// Daemon-level selection from the sidecar's environment names
// ---------------------------------------------------------------------------

#[test]
fn the_sidecar_env_names_select_the_vendor_and_its_alternate() {
    assert_eq!(DEEPGRAM_KEY_ENV, "DEEPGRAM_API_KEY");
    assert_eq!(ELEVENLABS_KEY_ENV, "ELEVENLABS_API_KEY");
    assert_eq!(ELEVENLABS_VOICE_ENV, "ELEVENLABS_VOICE_ID");
    assert_eq!(
        DISCORD_TTS_PROVIDER_ENV,
        "AUGMENTAGENT_DISCORD_TTS_PROVIDER"
    );

    // Default: Deepgram, like the sidecar.
    let s = tts_from_env(env(&[(DEEPGRAM_KEY_ENV, DG_KEY)]), None).unwrap();
    assert_eq!(s.primary_provider(), "deepgram");
    assert_eq!(s.alternate_provider(), None);

    // Both keys (and a voice): the other vendor is the credit fallback.
    let both = [
        (DEEPGRAM_KEY_ENV, DG_KEY),
        (ELEVENLABS_KEY_ENV, EL_KEY),
        (ELEVENLABS_VOICE_ENV, VOICE),
    ];
    let s = tts_from_env(env(&both), None).unwrap();
    assert_eq!(s.alternate_provider(), Some("elevenlabs"));

    // The Discord sidecar's selection applies to Slack too ...
    let mut pairs = both.to_vec();
    pairs.push((DISCORD_TTS_PROVIDER_ENV, "elevenlabs"));
    let s = tts_from_env(env(&pairs), None).unwrap();
    assert_eq!(s.primary_provider(), "elevenlabs");
    assert_eq!(s.alternate_provider(), Some("deepgram"));

    // ... unless Slack has its own.
    pairs.push((SLACK_TTS_PROVIDER_ENV, "deepgram"));
    let s = tts_from_env(env(&pairs), None).unwrap();
    assert_eq!(s.primary_provider(), "deepgram");

    // ElevenLabs without a voice is no alternate (sidecar rule).
    let s = tts_from_env(
        env(&[(DEEPGRAM_KEY_ENV, DG_KEY), (ELEVENLABS_KEY_ENV, EL_KEY)]),
        None,
    )
    .unwrap();
    assert_eq!(s.alternate_provider(), None);
}

#[test]
fn missing_keys_or_a_bad_choice_say_what_to_set() {
    let why = tts_from_env(env(&[]), None).unwrap_err();
    assert!(why.contains("DEEPGRAM_API_KEY"), "{why}");

    let why = tts_from_env(
        env(&[
            (SLACK_TTS_PROVIDER_ENV, "elevenlabs"),
            (ELEVENLABS_KEY_ENV, EL_KEY),
        ]),
        None,
    )
    .unwrap_err();
    assert!(why.contains("ELEVENLABS_VOICE_ID"), "{why}");

    let why = tts_from_env(env(&[(SLACK_TTS_PROVIDER_ENV, "polly")]), None).unwrap_err();
    assert!(why.contains("deepgram or elevenlabs"), "{why}");

    let why = tts_from_env(
        env(&[(SLACK_TTS_PROVIDER_ENV, "off"), (DEEPGRAM_KEY_ENV, DG_KEY)]),
        None,
    )
    .unwrap_err();
    assert!(why.contains("turned off"), "{why}");
    // Never the key itself.
    assert!(!why.contains(DG_KEY));
}

#[tokio::test]
async fn the_env_stack_uses_the_endpoint_override_for_both_vendors() {
    let mut server = mockito::Server::new_async().await;
    let dg = deepgram_mock(&mut server, 402, Vec::new()).await;
    let el = elevenlabs_mock(&mut server, 200, pcm(20)).await;
    let both = [
        (DEEPGRAM_KEY_ENV, DG_KEY),
        (ELEVENLABS_KEY_ENV, EL_KEY),
        (ELEVENLABS_VOICE_ENV, VOICE),
    ];
    let url = server.url();
    let stack = tts_from_env(env(&both), Some(url.as_str())).unwrap();
    let (_, used) = stack.synthesize("Hi.").await.unwrap();
    dg.assert_async().await;
    el.assert_async().await;
    assert_eq!(used.switched_from.as_deref(), Some("deepgram"));
    assert_eq!(SpeechVendor::ElevenLabs.as_str(), "elevenlabs");
}
