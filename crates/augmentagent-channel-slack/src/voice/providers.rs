//! #1297 — the Rust text-to-speech adapter for spoken Slack replies.
//!
//! The Discord voice sidecar (`sidecars/discord-voice/src/tts.ts`) streams
//! speech into a live call. A Slack reply is a finished file, so this is the
//! request/response form of the same two vendors with the same choices:
//!
//! | Vendor     | Request                                                               | Output         |
//! |------------|-----------------------------------------------------------------------|----------------|
//! | Deepgram   | `POST /v1/speak?model=aura-2-thalia-en&encoding=linear16&sample_rate=24000&container=none`, `Authorization: Token …`, text in ≤ 1800-char chunks (the sidecar's split) | 24 kHz PCM |
//! | ElevenLabs | `POST /v1/text-to-speech/<voice>/stream?output_format=pcm_24000`, `xi-api-key`, model `eleven_flash_v2_5` | 24 kHz PCM |
//!
//! Configuration uses the sidecar's names ([`tts_from_env`]):
//! `DEEPGRAM_API_KEY`, `ELEVENLABS_API_KEY`, `ELEVENLABS_VOICE_ID`, and the
//! vendor from `AUGMENTAGENT_SLACK_TTS_PROVIDER`, else the sidecar's
//! `AUGMENTAGENT_DISCORD_TTS_PROVIDER`, else Deepgram. The other vendor is
//! the credit fallback when its key (and, for ElevenLabs, a voice) is set,
//! and it is used only on confirmed credit exhaustion (`402`, or ElevenLabs
//! `quota_exceeded`) — [`super::TtsStack`], the sidecar's rule. Errors carry
//! the HTTP status as `code` and an owner-safe message: never a key or a
//! URL.

use std::time::Duration;

use async_trait::async_trait;

use super::speech::{
    AudioFormat, SpeechError, SpeechOperation, SynthesizedAudio, TextToSpeech, TtsStack,
    MAX_SPOKEN_BYTES,
};

/// Slack's own vendor choice; wins over the sidecar's.
pub const SLACK_TTS_PROVIDER_ENV: &str = "AUGMENTAGENT_SLACK_TTS_PROVIDER";
/// The Discord voice sidecar's vendor choice, shared by Slack.
pub const DISCORD_TTS_PROVIDER_ENV: &str = "AUGMENTAGENT_DISCORD_TTS_PROVIDER";
pub const DEEPGRAM_KEY_ENV: &str = "DEEPGRAM_API_KEY";
pub const ELEVENLABS_KEY_ENV: &str = "ELEVENLABS_API_KEY";
pub const ELEVENLABS_VOICE_ENV: &str = "ELEVENLABS_VOICE_ID";

/// The sidecar's provider deadline.
pub const PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
/// The sidecar's Deepgram text chunk.
pub const DEEPGRAM_CHUNK_CHARS: usize = 1800;
/// The sidecar's output: mono 16-bit PCM at this rate.
pub const TTS_SAMPLE_RATE: u32 = 24_000;

const DEEPGRAM_BASE: &str = "https://api.deepgram.com";
const ELEVENLABS_BASE: &str = "https://api.elevenlabs.io";
const DEEPGRAM_MODEL: &str = "aura-2-thalia-en";
const ELEVENLABS_MODEL: &str = "eleven_flash_v2_5";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechVendor {
    Deepgram,
    ElevenLabs,
}

impl SpeechVendor {
    pub fn as_str(self) -> &'static str {
        match self {
            SpeechVendor::Deepgram => "deepgram",
            SpeechVendor::ElevenLabs => "elevenlabs",
        }
    }

    fn other(self) -> Self {
        match self {
            SpeechVendor::Deepgram => SpeechVendor::ElevenLabs,
            SpeechVendor::ElevenLabs => SpeechVendor::Deepgram,
        }
    }
}

/// One vendor's text-to-speech over HTTP.
pub struct HttpTts {
    vendor: SpeechVendor,
    key: String,
    voice: Option<String>,
    endpoint: Option<String>,
    timeout: Duration,
}

impl std::fmt::Debug for HttpTts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("HttpTts")
            .field("vendor", &self.vendor)
            .field("voice", &self.voice)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl HttpTts {
    pub fn deepgram(key: &str) -> Self {
        Self {
            vendor: SpeechVendor::Deepgram,
            key: key.trim().to_string(),
            voice: None,
            endpoint: None,
            timeout: PROVIDER_TIMEOUT,
        }
    }

    pub fn elevenlabs(key: &str, voice: &str) -> Self {
        Self {
            vendor: SpeechVendor::ElevenLabs,
            key: key.trim().to_string(),
            voice: Some(voice.trim().to_string()),
            endpoint: None,
            timeout: PROVIDER_TIMEOUT,
        }
    }

    /// Base URL instead of the vendor's (local mock servers only).
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Some(url.into().trim_end_matches('/').to_string());
        self
    }

    /// Per-request deadline (default [`PROVIDER_TIMEOUT`]).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn error(&self, code: impl Into<String>, message: impl Into<String>) -> SpeechError {
        SpeechError::new(self.vendor.as_str(), SpeechOperation::Tts, code, message)
    }

    fn base(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(match self.vendor {
            SpeechVendor::Deepgram => DEEPGRAM_BASE,
            SpeechVendor::ElevenLabs => ELEVENLABS_BASE,
        })
    }

    fn client(&self) -> Result<reqwest::Client, SpeechError> {
        reqwest::Client::builder()
            .timeout(self.timeout)
            .connect_timeout(self.timeout.min(Duration::from_secs(10)))
            .build()
            .map_err(|_| self.error("client", "could not start an HTTP client"))
    }

    /// Send one request and read its body (bounded), mapping failures to
    /// owner-safe errors.
    async fn fetch(
        &self,
        request: reqwest::RequestBuilder,
        budget: usize,
    ) -> Result<Vec<u8>, SpeechError> {
        let transport = |e: reqwest::Error| {
            if e.is_timeout() {
                self.error(
                    "timeout",
                    format!("no answer within {}s", self.timeout.as_secs_f32()),
                )
            } else {
                self.error("network", "could not reach the provider")
            }
        };
        let mut response = request.send().await.map_err(transport)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.bytes().await.unwrap_or_default();
            let code = if quota_exceeded(&body) {
                "quota_exceeded".to_string()
            } else {
                status.as_u16().to_string()
            };
            return Err(self.error(
                code,
                format!("text-to-speech request failed (HTTP {})", status.as_u16()),
            ));
        }
        let mut out = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            if out.len() + chunk.len() > budget {
                return Err(self.error(
                    "too_large",
                    format!("the provider returned more than {MAX_SPOKEN_BYTES} bytes of audio"),
                ));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

/// ElevenLabs reports exhausted credit as `quota_exceeded` in its JSON
/// error (`{"detail": {"status": "quota_exceeded", …}}`).
fn quota_exceeded(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let status =
        |v: &serde_json::Value| v.get("status").and_then(|s| s.as_str()) == Some("quota_exceeded");
    status(&v) || v.get("detail").is_some_and(status)
}

/// The sidecar's split: consecutive runs of at most `max` characters.
fn chunks(text: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(max).map(|c| c.iter().collect()).collect()
}

#[async_trait]
impl TextToSpeech for HttpTts {
    fn provider(&self) -> &str {
        self.vendor.as_str()
    }

    async fn synthesize(&self, text: &str) -> Result<SynthesizedAudio, SpeechError> {
        if self.key.is_empty() {
            return Err(self.error(
                super::speech::NOT_CONFIGURED,
                format!("the {} key is missing", self.vendor.as_str()),
            ));
        }
        if text.trim().is_empty() {
            return Err(self.error("no_text", "nothing to read aloud"));
        }
        let client = self.client()?;
        let mut audio = Vec::new();
        match self.vendor {
            SpeechVendor::Deepgram => {
                let url = format!("{}/v1/speak", self.base());
                for chunk in chunks(text, DEEPGRAM_CHUNK_CHARS) {
                    let request = client
                        .post(&url)
                        .query(&[
                            ("model", DEEPGRAM_MODEL),
                            ("encoding", "linear16"),
                            ("sample_rate", "24000"),
                            ("container", "none"),
                        ])
                        .header("Authorization", format!("Token {}", self.key))
                        .header("Content-Type", "application/json")
                        .body(serde_json::json!({ "text": chunk }).to_string());
                    let part = self
                        .fetch(request, MAX_SPOKEN_BYTES.saturating_sub(audio.len()))
                        .await?;
                    audio.extend_from_slice(&part);
                }
            }
            SpeechVendor::ElevenLabs => {
                let voice = self.voice.as_deref().unwrap_or_default();
                if voice.is_empty() {
                    return Err(self.error(
                        super::speech::NOT_CONFIGURED,
                        "the ElevenLabs voice ID is missing",
                    ));
                }
                let url = format!(
                    "{}/v1/text-to-speech/{}/stream",
                    self.base(),
                    urlencode(voice)
                );
                let request = client
                    .post(&url)
                    .query(&[("output_format", "pcm_24000")])
                    .header("xi-api-key", &self.key)
                    .header("Content-Type", "application/json")
                    .body(
                        serde_json::json!({ "text": text, "model_id": ELEVENLABS_MODEL })
                            .to_string(),
                    );
                audio = self.fetch(request, MAX_SPOKEN_BYTES).await?;
            }
        }
        if audio.is_empty() {
            return Err(self.error("no_audio", "the provider returned no audio"));
        }
        Ok(SynthesizedAudio {
            bytes: audio,
            format: AudioFormat::Pcm16 {
                sample_rate: TTS_SAMPLE_RATE,
            },
        })
    }
}

/// Percent-encode a path segment (the sidecar's `encodeURIComponent`).
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn vendor_named(value: &str) -> Result<Option<SpeechVendor>, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "deepgram" => Ok(Some(SpeechVendor::Deepgram)),
        "elevenlabs" => Ok(Some(SpeechVendor::ElevenLabs)),
        "off" | "none" | "0" => Ok(None),
        other => Err(format!(
            "text-to-speech provider `{other}` is not supported: use deepgram or elevenlabs"
        )),
    }
}

/// The daemon's text-to-speech from the sidecar's environment names (see
/// the module docs). `Err` is why spoken replies are unavailable, naming
/// what to set (never a key). `endpoint` replaces both vendors' base URL
/// (local mock servers only).
pub fn tts_from_env(
    lookup: impl Fn(&str) -> Option<String>,
    endpoint: Option<&str>,
) -> Result<TtsStack, String> {
    let get = |name: &str| {
        lookup(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let (choice, source) = match get(SLACK_TTS_PROVIDER_ENV) {
        Some(v) => (v, SLACK_TTS_PROVIDER_ENV),
        None => (
            get(DISCORD_TTS_PROVIDER_ENV).unwrap_or_default(),
            DISCORD_TTS_PROVIDER_ENV,
        ),
    };
    let Some(primary) = vendor_named(&choice).map_err(|e| format!("{e} ({source})"))? else {
        return Err(format!("text-to-speech is turned off by {source}"));
    };
    let build = |vendor: SpeechVendor| -> Result<HttpTts, String> {
        let tts = match vendor {
            SpeechVendor::Deepgram => {
                let key = get(DEEPGRAM_KEY_ENV)
                    .ok_or_else(|| format!("{DEEPGRAM_KEY_ENV} is not set"))?;
                HttpTts::deepgram(&key)
            }
            SpeechVendor::ElevenLabs => {
                let key = get(ELEVENLABS_KEY_ENV)
                    .ok_or_else(|| format!("{ELEVENLABS_KEY_ENV} is not set"))?;
                let voice = get(ELEVENLABS_VOICE_ENV)
                    .ok_or_else(|| format!("{ELEVENLABS_VOICE_ENV} is not set"))?;
                HttpTts::elevenlabs(&key, &voice)
            }
        };
        Ok(match endpoint {
            Some(url) => tts.with_endpoint(url),
            None => tts,
        })
    };
    let first = build(primary).map_err(|why| {
        format!(
            "{} text-to-speech is not configured: {why} (set it in the daemon's environment)",
            primary.as_str()
        )
    })?;
    let mut stack = TtsStack::new(std::sync::Arc::new(first));
    if let Ok(alternate) = build(primary.other()) {
        stack = stack.with_alternate(std::sync::Arc::new(alternate));
    }
    Ok(stack)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_follow_the_sidecar_split() {
        assert_eq!(chunks("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(chunks("é".repeat(5).as_str(), 2).len(), 3);
        assert!(chunks("", 4).is_empty());
    }

    #[test]
    fn quota_exceeded_is_read_from_the_error_body() {
        assert!(quota_exceeded(br#"{"detail":{"status":"quota_exceeded"}}"#));
        assert!(quota_exceeded(br#"{"status":"quota_exceeded"}"#));
        assert!(!quota_exceeded(
            br#"{"detail":{"status":"invalid_api_key"}}"#
        ));
        assert!(!quota_exceeded(b"not json"));
    }

    #[test]
    fn debug_never_shows_the_key() {
        let shown = format!("{:?}", HttpTts::deepgram("secret-value"));
        assert!(!shown.contains("secret-value"), "{shown}");
    }
}
