//! Which Slack files are voice clips, and decoding them for transcription.
//!
//! Decoding shells out to `ffmpeg`, found and bounded exactly like the
//! #1293 document converters (`augmentagent_docs::resolve_tool` with
//! [`ConvertOptions`]): the process `PATH`, then `/opt/homebrew/bin`,
//! `/usr/local/bin`, `/usr/bin`, `/bin`, so it resolves under launchd
//! (which starts jobs with a minimal `PATH`) and systemd alike on Apple
//! Silicon, Intel and Linux. The run is killed at the timeout or when the
//! turn is cancelled (`kill_on_drop`), and a missing binary is an
//! owner-facing error naming the install command per host.
//!
//! Every clip is decoded to 16 kHz mono signed 16-bit WAV, the format the
//! Discord voice sidecar streams to Deepgram and ElevenLabs and the one
//! whisper.cpp reads, with the duration bound applied by `ffmpeg -t`.

use std::path::Path;
use std::time::Duration;

use augmentagent_docs::{resolve_tool, ConvertOptions};

use crate::transport::event::FileRef;

/// Sample rate every clip is decoded to.
pub const STT_SAMPLE_RATE: u32 = 16_000;

/// Whether a clip carries a picture too (the audio track is used).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipMedia {
    Audio,
    Video,
}

impl ClipMedia {
    pub fn as_str(self) -> &'static str {
        match self {
            ClipMedia::Audio => "audio",
            ClipMedia::Video => "video",
        }
    }
}

/// What [`clip_format`] made of an audio/video file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipFormat {
    Supported(ClipMedia),
    /// Audio or video in a format we do not decode; the detail names it.
    Unsupported(String),
}

/// MIME types decoded. Slack records clips as `audio/webm` or `video/mp4`
/// / `video/webm` on desktop and `audio/mp4` (`.m4a`) on mobile (the file
/// object lists `webm`, `m4a`, `mp4`, `mp3`, `wav`, `ogg` as audio
/// filetypes, read 2026-09-29; the exact MIME per client is **unverified**
/// against a live workspace).
const SUPPORTED_MIME: &[(&str, ClipMedia)] = &[
    ("audio/webm", ClipMedia::Audio),
    ("audio/ogg", ClipMedia::Audio),
    ("audio/opus", ClipMedia::Audio),
    ("audio/mp4", ClipMedia::Audio),
    ("audio/m4a", ClipMedia::Audio),
    ("audio/x-m4a", ClipMedia::Audio),
    ("audio/aac", ClipMedia::Audio),
    ("audio/mpeg", ClipMedia::Audio),
    ("audio/mp3", ClipMedia::Audio),
    ("audio/wav", ClipMedia::Audio),
    ("audio/x-wav", ClipMedia::Audio),
    ("audio/wave", ClipMedia::Audio),
    ("audio/vnd.wave", ClipMedia::Audio),
    ("audio/flac", ClipMedia::Audio),
    ("audio/x-flac", ClipMedia::Audio),
    ("video/mp4", ClipMedia::Video),
    ("video/webm", ClipMedia::Video),
    ("video/quicktime", ClipMedia::Video),
];

/// Extensions / Slack `filetype`s decoded when the MIME type is missing or
/// generic.
const SUPPORTED_EXT: &[(&str, ClipMedia)] = &[
    ("webm", ClipMedia::Audio),
    ("ogg", ClipMedia::Audio),
    ("oga", ClipMedia::Audio),
    ("opus", ClipMedia::Audio),
    ("m4a", ClipMedia::Audio),
    ("aac", ClipMedia::Audio),
    ("mp3", ClipMedia::Audio),
    ("wav", ClipMedia::Audio),
    ("flac", ClipMedia::Audio),
    ("mp4", ClipMedia::Video),
    ("mov", ClipMedia::Video),
];

/// Formats named in the owner-facing rejection.
pub const SUPPORTED_SUMMARY: &str = "m4a, mp3, wav, ogg, webm, flac or mp4";

fn media_for_ext(ext: &str) -> Option<ClipMedia> {
    SUPPORTED_EXT
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, m)| *m)
}

/// `Some` for anything audio or video (by MIME, Slack clip subtype, Slack
/// filetype or extension); `None` for every other file, which stays with
/// the #1293 document/image path.
pub fn clip_format(f: &FileRef) -> Option<ClipFormat> {
    let mime = f
        .mimetype
        .as_deref()
        .map(|m| {
            m.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|m| !m.is_empty());
    let ext = f
        .name
        .as_deref()
        .and_then(|n| Path::new(n).extension())
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let filetype = f.filetype.as_deref().map(str::to_ascii_lowercase);
    let slack_clip = match f.subtype.as_deref() {
        Some("slack_audio") => Some(ClipMedia::Audio),
        Some("slack_video") => Some(ClipMedia::Video),
        _ => None,
    };
    let is_media_mime = mime
        .as_deref()
        .is_some_and(|m| m.starts_with("audio/") || m.starts_with("video/"));

    if let Some(m) = mime.as_deref() {
        if let Some((_, media)) = SUPPORTED_MIME.iter().find(|(s, _)| *s == m) {
            // A Slack clip recorded with the camera is still a clip; keep
            // the MIME's media kind.
            return Some(ClipFormat::Supported(*media));
        }
        if is_media_mime {
            return Some(ClipFormat::Unsupported(m.to_string()));
        }
    }
    // Missing or generic MIME: Slack's own clip marker, then its filetype,
    // then the extension.
    let by_name = filetype
        .as_deref()
        .and_then(media_for_ext)
        .or_else(|| ext.as_deref().and_then(media_for_ext));
    match (slack_clip, by_name) {
        (Some(clip), _) => Some(ClipFormat::Supported(clip)),
        (None, Some(media)) => Some(ClipFormat::Supported(media)),
        (None, None) => None,
    }
}

/// Error from [`decode_for_stt`], already owner-facing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How to install `ffmpeg` on each supported host.
pub const FFMPEG_INSTALL_HINT: &str =
    "install it (`brew install ffmpeg` on macOS, `apt install ffmpeg` or `dnf install ffmpeg` on Linux)";

/// Where `ffmpeg` resolves with these options, if anywhere.
pub fn resolve_ffmpeg(tools: &ConvertOptions) -> Option<std::path::PathBuf> {
    resolve_tool("ffmpeg", tools)
}

/// Decode `input` (any container ffmpeg reads) to 16 kHz mono 16-bit WAV at
/// `output`, keeping at most `max_duration` plus one second so an
/// over-long clip is detectable without decoding all of it.
pub async fn decode_for_stt(
    input: &Path,
    output: &Path,
    max_duration: Duration,
    tools: &ConvertOptions,
) -> Result<(), DecodeError> {
    let Some(ffmpeg) = resolve_ffmpeg(tools) else {
        let dirs: Vec<String> = tools
            .fallback_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        return Err(DecodeError(format!(
            "ffmpeg is not installed or not on the service PATH (also looked in {}); {FFMPEG_INSTALL_HINT}",
            if dirs.is_empty() {
                "no other directories".to_string()
            } else {
                dirs.join(", ")
            }
        )));
    };
    let limit = format!("{}", max_duration.as_secs() + 1);
    let run = tokio::process::Command::new(&ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args([
            "-vn",
            "-map",
            "0:a:0",
            "-t",
            &limit,
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            "-f",
            "wav",
        ])
        .arg(output)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(tools.timeout, run).await {
        Err(_) => {
            return Err(DecodeError(format!(
                "ffmpeg timed out after {}s and was stopped",
                tools.timeout.as_secs_f32()
            )))
        }
        Ok(Err(e)) => return Err(DecodeError(format!("could not start ffmpeg: {e}"))),
        Ok(Ok(out)) => out,
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // ffmpeg prints the fatal error last.
        let last = stderr
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .unwrap_or("no audio track or unreadable data");
        let short: String = last.chars().take(200).collect();
        return Err(DecodeError(format!("ffmpeg: {short}")));
    }
    Ok(())
}

/// A mono 16-bit PCM WAV.
pub fn wav_from_pcm16(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    wav_header_and(&pcm, sample_rate)
}

/// Wrap raw little-endian mono 16-bit PCM bytes into a WAV.
pub fn wav_from_pcm16_bytes(pcm: &[u8], sample_rate: u32) -> Result<Vec<u8>, String> {
    if !pcm.len().is_multiple_of(2) {
        return Err("the provider returned incomplete 16-bit audio".into());
    }
    if sample_rate == 0 {
        return Err("the provider returned audio without a sample rate".into());
    }
    Ok(wav_header_and(pcm, sample_rate))
}

fn wav_header_and(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let mut v = Vec::with_capacity(44 + pcm.len());
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&sample_rate.to_le_bytes());
    v.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    v.extend_from_slice(pcm);
    v
}

/// `(duration in ms, sample rate)` of a PCM WAV, from its `fmt ` and `data`
/// chunks; `None` for anything else.
pub fn wav_duration_ms(bytes: &[u8]) -> Option<(u64, u32)> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let u16_at = |i: usize| Some(u16::from_le_bytes(bytes.get(i..i + 2)?.try_into().ok()?));
    let u32_at = |i: usize| Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?));
    let mut pos = 12;
    let mut fmt: Option<(u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(pos + 4)? as usize;
        let body = pos + 8;
        if id == b"fmt " {
            fmt = Some((u16_at(body + 2)?, u32_at(body + 4)?, u16_at(body + 14)?));
        } else if id == b"data" {
            let (channels, rate, bits) = fmt?;
            let frame = channels as u64 * (bits as u64 / 8);
            if frame == 0 || rate == 0 {
                return None;
            }
            // ffmpeg writing to a pipe can leave the size unset; use what
            // is actually there.
            let available = (bytes.len() - body) as u64;
            let data = if len == 0 || len as u64 > available {
                available
            } else {
                len as u64
            };
            return Some((data / frame * 1000 / rate as u64, rate));
        }
        pos = body + len + (len & 1);
    }
    None
}

/// `m:ss`.
pub fn format_duration(ms: u64) -> String {
    // Rounded to the nearest second, but a non-empty clip is at least 0:01.
    let secs = if ms == 0 {
        0
    } else {
        ((ms + 500) / 1000).max(1)
    };
    format!("{}:{:02}", secs / 60, secs % 60)
}
