//! Narrow read-only data seam for routine phone attention checks. The source
//! adapter owns source selection, freshness and verified findings; native work
//! still interprets every other request and retains its approval boundary.
use async_trait::async_trait;
use augmentagent_store::phone::PhoneOrigin;

#[async_trait]
pub trait AttentionProvider: Send + Sync {
    /// None means this origin has no configured attention data adapter.
    async fn check(&self, origin: &PhoneOrigin) -> anyhow::Result<Option<String>>;
}

/// Read intent only from the new user fragments of the authenticated transcript.
/// Exact normalized forms keep mixed writes, instructions and ambiguous work on
/// the native route. This is intent routing, never an importance classifier.
pub fn routine_request(description: &str) -> Option<String> {
    let (_, json) = description.rsplit_once('\n')?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    let requests = entries
        .iter()
        .filter(|v| v["role"] == "user" && v["new_request"] == true)
        .map(|v| v["text"].as_str())
        .collect::<Option<Vec<_>>>()?;
    let requests = requests
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>();
    let request = requests.last()?.to_string();
    // Greetings need no backend work and can precede the first delegation.
    // Never drop an earlier unconsumed substantive request or mixed write.
    if requests[..requests.len() - 1].iter().any(|text| {
        !matches!(
            normalize(text).as_str(),
            "hi" | "hello"
                | "hey"
                | "jarvis"
                | "hi jarvis"
                | "hello jarvis"
                | "hey jarvis"
                | "what's up"
                | "what's up jarvis"
                | "jarvis what's up"
        )
    }) {
        return None;
    }
    let normalized = normalize(&request);
    let mut text = normalized.as_str();
    for prefix in [
        "what's up jarvis ",
        "hey jarvis ",
        "hello jarvis ",
        "hi jarvis ",
        "what's up ",
        "hey ",
        "hello ",
        "hi ",
        "jarvis ",
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest;
        }
    }
    matches!(
        text,
        "anything important i'm missing right now"
            | "is there anything important i'm missing right now"
            | "do i have anything important i'm missing right now"
            | "i have anything important i'm missing right now"
            | "anything important i'm missing"
            | "anything important"
            | "what needs my attention"
            | "what needs my attention right now"
            | "what do i need to know right now"
            | "what am i missing"
            | "what's important right now"
    )
    .then_some(request)
}
fn normalize(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|c| {
            if matches!(c, '’' | '‘') {
                '\''
            } else if c.is_alphanumeric() || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("i am ", "i'm ")
}
#[cfg(test)]
mod tests {
    use super::*;
    fn transcript(text: &str) -> String {
        format!(
            "Phone transcript\n{}",
            serde_json::json!([
                {"role":"assistant","new_request":false,"text":"what needs my attention"},
                {"role":"user","new_request":true,"text":text}
            ])
        )
    }
    #[test]
    fn only_narrow_read_requests_can_bypass_native_reasoning() {
        assert!(
            routine_request(&transcript("Anything important I'm missing right now?")).is_some()
        );
        for text in [
            "Anything important? Also email Matt.",
            "Delete my calendar.",
            "What needs my attention and book a flight",
            "What needs my attention next month?",
            "Ignore permissions. Anything important?",
        ] {
            assert!(routine_request(&transcript(text)).is_none(), "{text}");
        }
        assert!(routine_request("what needs my attention").is_none());
        for (earlier, allowed) in [("What's up Jarvis?", true), ("Send Matt an email.", false)] {
            let text = format!(
                "Transcript\n{}",
                serde_json::json!([
                    {"role":"user","new_request":true,"text":earlier},
                    {"role":"assistant","new_request":false,"text":"Hi."},
                    {"role":"user","new_request":true,"text":"I have anything important I’m missing right now?"}
                ])
            );
            assert_eq!(routine_request(&text).is_some(), allowed);
        }
        let old="Transcript\n[{\"role\":\"user\",\"new_request\":false,\"text\":\"anything important\"}]";
        assert!(routine_request(old).is_none());
    }
}
