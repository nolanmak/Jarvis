//! Production fast models for the phone layer (#1546) and its read-only wiki
//! lookup. Keys are loaded per request from the existing private service
//! credentials (keyring, then process env); they never come from chat or tool
//! arguments and never appear in errors or logs.
use augmentagent_phone::fast::{
    FastEvent, FastMessage, FastModel, FastModelError, FastProvider, FastRequest, FastStream,
    ToolCall, ToolChoice,
};
use serde_json::{json, Value};
use std::path::Path;

/// Anthropic model id for the fast layer.
pub const ANTHROPIC_FAST_MODEL: &str = "claude-haiku-5-5";

pub fn anthropic_body(request: &FastRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        match message {
            FastMessage::User(text) => messages.push(json!({"role":"user","content":text})),
            FastMessage::Assistant { text, tool_calls } => {
                let mut content = Vec::new();
                if !text.is_empty() {
                    content.push(json!({"type":"text","text":text}));
                }
                for call in tool_calls {
                    content.push(
                        json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.input}),
                    );
                }
                if content.is_empty() {
                    content.push(json!({"type":"text","text":"…"}));
                }
                messages.push(json!({"role":"assistant","content":content}));
            }
            FastMessage::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let block = json!({"type":"tool_result","tool_use_id":call_id,"content":content,"is_error":is_error});
                // Consecutive results answer one assistant turn together.
                match messages.last_mut() {
                    Some(last)
                        if last["role"] == "user"
                            && last["content"]
                                .as_array()
                                .is_some_and(|c| c.iter().all(|b| b["type"] == "tool_result")) =>
                    {
                        last["content"].as_array_mut().unwrap().push(block)
                    }
                    _ => messages.push(json!({"role":"user","content":[block]})),
                }
            }
        }
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.input_schema}))
        .collect();
    let mut body = json!({
        "model": ANTHROPIC_FAST_MODEL,
        "max_tokens": 400,
        "stream": true,
        "system": request.system,
        "messages": messages,
        "tools": tools,
    });
    if request.tool_choice == ToolChoice::None {
        body["tool_choice"] = json!({"type":"none"});
    }
    body
}

pub fn cerebras_body(request: &FastRequest, model: &str) -> Value {
    let mut messages = vec![json!({"role":"system","content":request.system})];
    for message in &request.messages {
        messages.push(match message {
            FastMessage::User(text) => json!({"role":"user","content":text}),
            FastMessage::Assistant { text, tool_calls } => {
                let mut value = json!({"role":"assistant","content":text});
                if !tool_calls.is_empty() {
                    value["tool_calls"] = tool_calls
                        .iter()
                        .map(|c| json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.input.to_string()}}))
                        .collect();
                }
                value
            }
            FastMessage::ToolResult {
                call_id, content, ..
            } => json!({"role":"tool","tool_call_id":call_id,"content":content}),
        });
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}}))
        .collect();
    let mut body = json!({
        "model": model,
        "max_tokens": 400,
        "stream": true,
        "messages": messages,
        "tools": tools,
    });
    if request.tool_choice == ToolChoice::None {
        body["tool_choice"] = json!("none");
    }
    body
}

/// Complete `data:` payloads from a server-sent event buffer.
fn sse_payloads(buffer: &mut String, chunk: &str) -> Vec<String> {
    buffer.push_str(&chunk.replace("\r\n", "\n"));
    let mut payloads = Vec::new();
    while let Some(end) = buffer.find("\n\n") {
        let event: String = buffer.drain(..end + 2).collect();
        let data: Vec<&str> = event
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim_start)
            .collect();
        if !data.is_empty() {
            payloads.push(data.join("\n"));
        }
    }
    payloads
}

/// Incremental parser for Anthropic Messages server-sent events.
#[derive(Default)]
pub struct AnthropicSse {
    buffer: String,
    complete: bool,
    /// Open tool_use blocks by content index: (id, name, partial JSON).
    tools: std::collections::HashMap<u64, (String, String, String)>,
}
impl AnthropicSse {
    pub fn push(&mut self, chunk: &str) -> Result<Vec<FastEvent>, FastModelError> {
        if self.buffer.len().saturating_add(chunk.len()) > 1_048_576 {
            return Err(FastModelError("stream_limit"));
        }
        let mut events = Vec::new();
        for payload in sse_payloads(&mut self.buffer, chunk) {
            let value: Value =
                serde_json::from_str(&payload).map_err(|_| FastModelError("malformed_stream"))?;
            let index = value["index"].as_u64().unwrap_or(0);
            match value["type"].as_str() {
                Some("error") => return Err(FastModelError("provider_error")),
                Some("message_stop") => self.complete = true,
                Some("content_block_start") if value["content_block"]["type"] == "tool_use" => {
                    let block = &value["content_block"];
                    self.tools.insert(
                        index,
                        (
                            block["id"].as_str().unwrap_or_default().into(),
                            block["name"].as_str().unwrap_or_default().into(),
                            String::new(),
                        ),
                    );
                }
                Some("content_block_delta") => match value["delta"]["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) =
                            value["delta"]["text"].as_str().filter(|t| !t.is_empty())
                        {
                            events.push(FastEvent::Token(text.into()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(tool) = self.tools.get_mut(&index) {
                            tool.2
                                .push_str(value["delta"]["partial_json"].as_str().unwrap_or(""));
                        }
                    }
                    _ => {}
                },
                Some("content_block_stop") => {
                    if let Some((id, name, input)) = self.tools.remove(&index) {
                        let input = if input.trim().is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(&input)
                                .map_err(|_| FastModelError("malformed_tool_input"))?
                        };
                        events.push(FastEvent::ToolCall(ToolCall { id, name, input }));
                    }
                }
                _ => {}
            }
        }
        if self.tools.len() > 32
            || self
                .tools
                .values()
                .map(|(id, name, args)| id.len() + name.len() + args.len())
                .sum::<usize>()
                > 1_048_576
        {
            return Err(FastModelError("stream_limit"));
        }
        Ok(events)
    }
}

/// Incremental parser for OpenAI-compatible chat completion chunks.
#[derive(Default)]
pub struct OpenAiSse {
    buffer: String,
    complete: bool,
    /// Tool calls being assembled, by index: (id, name, arguments).
    tools: std::collections::BTreeMap<u64, (String, String, String)>,
}
impl OpenAiSse {
    pub fn push(&mut self, chunk: &str) -> Result<Vec<FastEvent>, FastModelError> {
        if self.buffer.len().saturating_add(chunk.len()) > 1_048_576 {
            return Err(FastModelError("stream_limit"));
        }
        let mut events = Vec::new();
        for payload in sse_payloads(&mut self.buffer, chunk) {
            if payload.trim() == "[DONE]" {
                self.complete = true;
                events.extend(self.flush()?);
                continue;
            }
            let value: Value =
                serde_json::from_str(&payload).map_err(|_| FastModelError("malformed_stream"))?;
            if value.get("error").is_some() {
                return Err(FastModelError("provider_error"));
            }
            let choice = &value["choices"][0];
            if let Some(text) = choice["delta"]["content"]
                .as_str()
                .filter(|t| !t.is_empty())
            {
                events.push(FastEvent::Token(text.into()));
            }
            for call in choice["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let entry = self
                    .tools
                    .entry(call["index"].as_u64().unwrap_or(0))
                    .or_default();
                if let Some(id) = call["id"].as_str() {
                    entry.0 = id.into();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    entry.1.push_str(name);
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    entry.2.push_str(args);
                }
            }
            if choice["finish_reason"].is_string() {
                events.extend(self.flush()?);
            }
        }
        if self.tools.len() > 32
            || self
                .tools
                .values()
                .map(|(id, name, args)| id.len() + name.len() + args.len())
                .sum::<usize>()
                > 1_048_576
        {
            return Err(FastModelError("stream_limit"));
        }
        Ok(events)
    }
    fn flush(&mut self) -> Result<Vec<FastEvent>, FastModelError> {
        std::mem::take(&mut self.tools)
            .into_values()
            .map(|(id, name, args)| {
                let input = if args.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&args)
                        .map_err(|_| FastModelError("malformed_tool_input"))?
                };
                Ok(FastEvent::ToolCall(ToolCall { id, name, input }))
            })
            .collect()
    }
}

enum Parser {
    Anthropic(AnthropicSse),
    OpenAi(OpenAiSse),
}
impl Parser {
    fn finish(&self) -> Result<(), FastModelError> {
        let complete = match self {
            Self::Anthropic(p) => p.complete && p.tools.is_empty() && p.buffer.trim().is_empty(),
            Self::OpenAi(p) => p.complete && p.tools.is_empty() && p.buffer.trim().is_empty(),
        };
        if complete {
            Ok(())
        } else {
            Err(FastModelError("truncated_stream"))
        }
    }
    fn push(&mut self, chunk: &str) -> Result<Vec<FastEvent>, FastModelError> {
        match self {
            Self::Anthropic(p) => p.push(chunk),
            Self::OpenAi(p) => p.push(chunk),
        }
    }
}

/// Streaming HTTP client for the configured fast provider.
pub struct HttpFastModel {
    provider: FastProvider,
    client: reqwest::Client,
}
impl HttpFastModel {
    pub fn new(provider: FastProvider) -> Self {
        Self {
            provider,
            client: reqwest::Client::new(),
        }
    }
}
#[async_trait::async_trait]
impl FastModel for HttpFastModel {
    async fn stream(&self, request: FastRequest) -> Result<FastStream, FastModelError> {
        let (builder, parser) = match self.provider {
            FastProvider::Anthropic => {
                let key = crate::secret_loader::load_provider_key("ANTHROPIC_API_KEY")
                    .ok_or(FastModelError("missing_credentials"))?;
                (
                    self.client
                        .post("https://api.anthropic.com/v1/messages")
                        .header("x-api-key", key)
                        .header("anthropic-version", "2023-06-01")
                        .json(&anthropic_body(&request)),
                    Parser::Anthropic(AnthropicSse::default()),
                )
            }
            FastProvider::Cerebras => {
                let key = crate::secret_loader::load_provider_key("CEREBRAS_API_KEY")
                    .ok_or(FastModelError("missing_credentials"))?;
                let model = crate::providers::model_for(
                    crate::providers::ProviderKind::Cerebras,
                    crate::providers::ModelTier::Fast,
                );
                (
                    self.client
                        .post(format!(
                            "{}/chat/completions",
                            crate::cerebras::cerebras_base_url()
                        ))
                        .bearer_auth(key)
                        .json(&cerebras_body(&request, &model)),
                    Parser::OpenAi(OpenAiSse::default()),
                )
            }
        };
        // Only status classes and fixed codes leave this function: provider
        // bodies may echo request text.
        let response = builder
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| FastModelError("transport"))?;
        if !response.status().is_success() {
            return Err(FastModelError(if response.status().is_client_error() {
                "http_4xx"
            } else {
                "http_5xx"
            }));
        }
        Ok(response_stream(response, parser))
    }
}

// Shared by production HTTP and local wire fixtures: a clean TCP EOF is not
// proof that the provider completed a model turn.
fn response_stream(mut response: reqwest::Response, mut parser: Parser) -> FastStream {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        let mut pending = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => {
                    let result = if pending.is_empty() {
                        parser.finish()
                    } else {
                        Err(FastModelError("truncated_stream"))
                    };
                    if let Err(error) = result {
                        let _ = tx.send(Err(error)).await;
                    }
                    return;
                }
                Err(_) => {
                    let _ = tx.send(Err(FastModelError("transport"))).await;
                    return;
                }
            };
            pending.extend_from_slice(&chunk);
            let valid = match std::str::from_utf8(&pending) {
                Ok(_) => pending.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => {
                    let _ = tx.send(Err(FastModelError("malformed_stream"))).await;
                    return;
                }
            };
            let text = String::from_utf8(pending.drain(..valid).collect())
                .expect("validated UTF-8 prefix");
            match parser.push(&text) {
                Ok(events) => {
                    for event in events {
                        if tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            }
        }
    });
    Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }))
}

/// Read-only keyword lookup over the wiki's Markdown files: up to three
/// matching paragraphs, best first. Stops at `budget`.
pub fn wiki_lookup(root: &Path, query: &str, budget: std::time::Duration) -> String {
    let deadline = std::time::Instant::now() + budget;
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() > 2)
        .map(str::to_lowercase)
        .collect();
    if terms.is_empty() {
        return "No matching notes.".into();
    }
    let mut hits: Vec<(usize, String)> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut files = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if std::time::Instant::now() >= deadline || files >= 5_000 {
                break;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() || entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if kind.is_dir() {
                if depth < 8 {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            files += 1;
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for paragraph in text.split("\n\n") {
                let lower = paragraph.to_lowercase();
                let score = terms.iter().filter(|t| lower.contains(t.as_str())).count();
                if score > 0 {
                    let title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("note");
                    let snippet: String = paragraph.trim().chars().take(400).collect();
                    hits.push((score, format!("{title}: {snippet}")));
                }
            }
        }
    }
    if hits.is_empty() {
        return "No matching notes.".into();
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.into_iter()
        .take(3)
        .map(|(_, hit)| hit)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod phone_fast_tests {
    use super::*;
    use augmentagent_phone::fast::{tool_specs, TOOL_NAMES};

    fn request() -> FastRequest {
        FastRequest {
            system: "persona".into(),
            messages: vec![
                FastMessage::User("draft it".into()),
                FastMessage::Assistant {
                    text: "On it.".into(),
                    tool_calls: vec![ToolCall {
                        id: "t1".into(),
                        name: "dispatch_task".into(),
                        input: json!({"description":"draft"}),
                    }],
                },
                FastMessage::ToolResult {
                    call_id: "t1".into(),
                    content: "{\"task_id\":\"x\"}".into(),
                    is_error: false,
                },
            ],
            tools: tool_specs(),
            tool_choice: ToolChoice::Auto,
        }
    }
    fn names(events: &[FastEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                FastEvent::Token(t) => format!("token:{t}"),
                FastEvent::ToolCall(c) => format!("tool:{}:{}:{}", c.id, c.name, c.input),
            })
            .collect()
    }

    #[test]
    fn phone_fast_anthropic_body_streams_haiku_with_exactly_three_tools() {
        let body = anthropic_body(&request());
        assert_eq!(body["model"], "claude-haiku-5-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "persona");
        let tools: Vec<_> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(tools, ["dispatch_task", "lookup_fast", "end_call"]);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "t1");
    }

    // #1555: speculative requests list the tools but forbid calling them;
    // normal requests leave the provider default untouched.
    #[test]
    fn phone_fast_speculative_bodies_set_tool_choice_none() {
        assert!(anthropic_body(&request()).get("tool_choice").is_none());
        assert!(cerebras_body(&request(), "m").get("tool_choice").is_none());
        let speculative = FastRequest {
            tool_choice: ToolChoice::None,
            ..request()
        };
        let anthropic = anthropic_body(&speculative);
        assert_eq!(anthropic["tool_choice"], json!({"type":"none"}));
        assert_eq!(
            anthropic["tools"].as_array().unwrap().len(),
            TOOL_NAMES.len()
        );
        let cerebras = cerebras_body(&speculative, "m");
        assert_eq!(cerebras["tool_choice"], "none");
        assert_eq!(
            cerebras["tools"].as_array().unwrap().len(),
            TOOL_NAMES.len()
        );
    }

    #[test]
    fn phone_fast_cerebras_body_uses_openai_function_tools() {
        let body = cerebras_body(&request(), "fast-model");
        assert_eq!(body["model"], "fast-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        let tools: Vec<_> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(tools, ["dispatch_task", "lookup_fast", "end_call"]);
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "dispatch_task"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "t1");
    }

    #[test]
    fn phone_fast_anthropic_sse_yields_tokens_and_tool_calls_across_chunk_splits() {
        let wire = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi there. \"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"lookup_fast\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\":\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"calendar\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut parser = AnthropicSse::default();
        let mut events = Vec::new();
        for piece in wire.as_bytes().chunks(7) {
            events.extend(parser.push(std::str::from_utf8(piece).unwrap()).unwrap());
        }
        assert_eq!(
            names(&events),
            [
                "token:Hi there. ",
                "tool:tu_1:lookup_fast:{\"query\":\"calendar\"}"
            ]
        );
        let mut parser = AnthropicSse::default();
        assert_eq!(
            parser.push("event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"secret detail\"}}\n\n").err(),
            Some(FastModelError("provider_error"))
        );
    }

    #[test]
    fn phone_fast_openai_sse_yields_tokens_and_assembled_tool_calls() {
        let wire = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Sure. \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"dispatch_task\",\"arguments\":\"{\\\"descr\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"iption\\\":\\\"x\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut parser = OpenAiSse::default();
        let mut events = Vec::new();
        for piece in wire.as_bytes().chunks(5) {
            events.extend(parser.push(std::str::from_utf8(piece).unwrap()).unwrap());
        }
        assert_eq!(
            names(&events),
            [
                "token:Sure. ",
                "tool:c1:dispatch_task:{\"description\":\"x\"}"
            ]
        );
    }

    #[tokio::test]
    async fn phone_fast_http_eof_after_partial_text_reports_truncation() {
        use futures::StreamExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (tail, expected) in [
            (b"".as_slice(), Some("truncated_stream")),
            (b"data: [DONE]\n\n".as_slice(), None),
            (b"\xf0\x9f".as_slice(), Some("truncated_stream")),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let mut body =
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n".to_vec();
            body.extend_from_slice(tail);
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).await.unwrap();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            });
            let response = reqwest::get(format!("http://{address}")).await.unwrap();
            let events: Vec<_> = response_stream(response, Parser::OpenAi(OpenAiSse::default()))
                .collect()
                .await;
            assert!(matches!(&events[0], Ok(FastEvent::Token(text)) if text == "Hello"));
            assert_eq!(
                events.last().and_then(|e| e.as_ref().err()).map(|e| e.0),
                expected
            );
            server.await.unwrap();
        }
    }

    #[test]
    fn phone_fast_eof_requires_provider_terminal_event_and_complete_frames() {
        for mut parser in [
            Parser::Anthropic(AnthropicSse::default()),
            Parser::OpenAi(OpenAiSse::default()),
        ] {
            assert_eq!(parser.finish(), Err(FastModelError("truncated_stream")));
            parser.push("data: {").unwrap();
            assert_eq!(parser.finish(), Err(FastModelError("truncated_stream")));
        }
        let mut a = Parser::Anthropic(AnthropicSse::default());
        a.push("data: {\"type\":\"message_stop\"}\n\n").unwrap();
        assert_eq!(a.finish(), Ok(()));
        let mut o = Parser::OpenAi(OpenAiSse::default());
        o.push("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
            .unwrap();
        assert_eq!(o.finish(), Err(FastModelError("truncated_stream")));
        o.push("data: [DONE]\n\n").unwrap();
        assert_eq!(o.finish(), Ok(()));
        o.push("data: {").unwrap();
        assert_eq!(o.finish(), Err(FastModelError("truncated_stream")));
    }

    #[test]
    fn phone_fast_unfinished_tool_input_never_finishes_successfully() {
        let mut a = Parser::Anthropic(AnthropicSse::default());
        a.push(concat!(
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"lookup_fast\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )).unwrap();
        assert_eq!(a.finish(), Err(FastModelError("truncated_stream")));
        let mut o = Parser::OpenAi(OpenAiSse::default());
        o.push("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t\",\"function\":{\"name\":\"lookup_fast\",\"arguments\":\"{\"}}]}}]}\n\n").unwrap();
        assert_eq!(o.finish(), Err(FastModelError("truncated_stream")));
        assert_eq!(
            o.push("data: [DONE]\n\n").err(),
            Some(FastModelError("malformed_tool_input"))
        );
    }

    #[test]
    fn phone_fast_wiki_lookup_reads_matching_markdown_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("calendar.md"),
            "# Calendar\n\nTomorrow: dentist at 9am.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pets.md"),
            "# Pets\n\nThe cat is called Miso.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("secret.txt"), "dentist tomorrow token").unwrap();
        let found = wiki_lookup(
            dir.path(),
            "dentist tomorrow",
            std::time::Duration::from_millis(500),
        );
        assert!(found.contains("dentist at 9am"), "{found}");
        assert!(!found.contains("Miso"));
        assert!(!found.contains("token"));
        assert_eq!(
            wiki_lookup(
                dir.path(),
                "nonexistent zebra",
                std::time::Duration::from_millis(500)
            ),
            "No matching notes."
        );
    }
}
