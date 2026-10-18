//! Private, revocable capabilities for native CLI MCP clients. The Unix socket
//! and token are injected by the daemon, never accepted as model arguments.
use crate::{ingress::SpeechSender, Error, Grant, PhoneService};
use augmentagent_store::{SurfaceConversationRef, SurfaceOwnerRef};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};
struct Entry {
    grant: Grant,
    speech: Option<SpeechSender>,
}
pub struct ToolServer {
    service: Arc<PhoneService>,
    socket: PathBuf,
    _directory: tempfile::TempDir,
    grants: Mutex<HashMap<String, Arc<Entry>>>,
    abort: Mutex<Option<tokio::task::AbortHandle>>,
}
pub struct ToolGrant {
    token: String,
    server: Weak<ToolServer>,
    has_speech: bool,
}
impl ToolGrant {
    pub fn token(&self) -> &str {
        &self.token
    }
    pub fn has_speech(&self) -> bool {
        self.has_speech
    }
}
impl Drop for ToolGrant {
    fn drop(&mut self) {
        if let Some(server) = self.server.upgrade() {
            server
                .grants
                .lock()
                .expect("phone grants poisoned")
                .remove(&self.token);
        }
    }
}
impl Drop for ToolServer {
    fn drop(&mut self) {
        if let Some(abort) = self.abort.get_mut().expect("phone task poisoned").take() {
            abort.abort();
        }
    }
}
impl ToolServer {
    pub async fn start(service: Arc<PhoneService>) -> std::io::Result<Arc<Self>> {
        let directory = tempfile::Builder::new()
            .prefix("jarvis-phone-tools-")
            .tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let socket = directory.path().join("control.sock");
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let server = Arc::new(Self {
            service,
            socket,
            _directory: directory,
            grants: Mutex::new(HashMap::new()),
            abort: Mutex::new(None),
        });
        let weak = Arc::downgrade(&server);
        let slots = Arc::new(Semaphore::new(32));
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let Some(server) = weak.upgrade() else { break };
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ =
                        tokio::time::timeout(Duration::from_secs(45), server.handle(socket)).await;
                });
            }
        });
        *server.abort.lock().expect("phone task poisoned") = Some(task.abort_handle());
        Ok(server)
    }
    /// Trusted chat adapter entry point; command text never enters the agent.
    pub async fn command(
        &self,
        owner: &SurfaceOwnerRef,
        conversation: &SurfaceConversationRef,
        event_id: &str,
        text: &str,
    ) -> Option<String> {
        crate::commands::execute(&self.service, owner, conversation, event_id, text).await
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }
    pub fn grant(
        self: &Arc<Self>,
        owner: &SurfaceOwnerRef,
        conversation: &SurfaceConversationRef,
        speech: Option<SpeechSender>,
    ) -> Result<ToolGrant, Error> {
        let grant = self.service.authorize(owner, conversation)?;
        let token = uuid::Uuid::new_v4().to_string();
        let has_speech = speech.is_some();
        self.grants
            .lock()
            .expect("phone grants poisoned")
            .insert(token.clone(), Arc::new(Entry { grant, speech }));
        Ok(ToolGrant {
            token,
            server: Arc::downgrade(self),
            has_speech,
        })
    }
    async fn handle(&self, socket: UnixStream) {
        let (read, mut write) = socket.into_split();
        let mut bytes = Vec::new();
        let result = match BufReader::new(read)
            .take(32_769)
            .read_until(b'\n', &mut bytes)
            .await
        {
            Ok(_) if bytes.len() <= 32_768 && bytes.last() == Some(&b'\n') => {
                self.dispatch(&bytes).await
            }
            _ => Err(Error::InvalidArguments),
        };
        let response = match result {
            Ok(value) => json!({"version":1,"ok":true,"result":value}),
            Err(e) => json!({"version":1,"ok":false,"error":e.code()}),
        };
        let _ = write.write_all(format!("{response}\n").as_bytes()).await;
    }
    async fn dispatch(&self, bytes: &[u8]) -> Result<Value, Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            version: u8,
            grant: String,
            method: String,
            arguments: Value,
        }
        let request: Request =
            serde_json::from_slice(bytes).map_err(|_| Error::InvalidArguments)?;
        if request.version != 1 {
            return Err(Error::InvalidArguments);
        }
        let entry = self
            .grants
            .lock()
            .expect("phone grants poisoned")
            .get(&request.grant)
            .cloned()
            .ok_or(Error::Unauthorized)?;
        self.service.check(&entry.grant, false)?;
        match request.method.as_str() {
            "phone_number_status" => {
                empty(&request.arguments)?;
                self.service.number_status(&entry.grant)
            }
            "phone_number_remove" => {
                empty(&request.arguments)?;
                self.service.remove_number(&entry.grant).await?;
                Ok(json!({"removed":true}))
            }
            "phone_number_setup" => {
                empty(&request.arguments)?;
                self.service.check(&entry.grant, true)?;
                Ok(
                    json!({"instructions":"Send `phone number set +<country code><number>` in this private chat. Enter the spoken code using `phone number verify <code>`. Keep numbers and verification codes out of agent prompts."}),
                )
            }
            "speak" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Speak {
                    utterance_id: String,
                    text: String,
                }
                let args: Speak = serde_json::from_value(request.arguments)
                    .map_err(|_| Error::InvalidArguments)?;
                let speech = entry.speech.as_ref().ok_or(Error::Unauthorized)?;
                speech
                    .speak(&args.utterance_id, &args.text, args.utterance_id == "final")
                    .await
                    .map_err(|_| Error::InvalidArguments)?;
                Ok(json!({"state":"queued","utterance_id":args.utterance_id}))
            }
            name => {
                self.service
                    .tool(&entry.grant, name, request.arguments)
                    .await
            }
        }
    }
}
fn empty(value: &Value) -> Result<(), Error> {
    if value.as_object().is_some_and(|o| o.is_empty()) {
        Ok(())
    } else {
        Err(Error::InvalidArguments)
    }
}
pub fn schemas(speech: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({"name":"call_owner","description":"Call the owner's verified phone when they request a phone conversation. Reuse request_id only to retry the same request; never redial an uncertain call.","inputSchema":{"type":"object","properties":{"request_id":{"type":"string","maxLength":512}},"required":["request_id"],"additionalProperties":false}}),
        json!({"name":"phone_status","description":"Read an owner call's status.","inputSchema":{"type":"object","properties":{"call_id":{"type":"string"}},"required":["call_id"],"additionalProperties":false}}),
        json!({"name":"phone_hangup","description":"Stop an owner call.","inputSchema":{"type":"object","properties":{"call_id":{"type":"string"}},"required":["call_id"],"additionalProperties":false}}),
    ];
    for (name,description) in [("phone_number_setup","Show private phone-number setup instructions. Number and code entry use private commands outside the model."),("phone_number_status","Read masked phone configuration in the private owner chat."),("phone_number_remove","Remove the owner's phone destination and stop their calls.")] {
        tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":{},"additionalProperties":false}}));
    }
    if speech {
        tools.push(json!({"name":"speak","description":"Speak user-facing text during this phone turn. Reuse an utterance_id only for identical text. Use final only for the final spoken answer; the ordinary final answer will then not be played twice. Receipt means queued, not heard.","inputSchema":{"type":"object","properties":{"utterance_id":{"type":"string","maxLength":64},"text":{"type":"string","maxLength":12000}},"required":["utterance_id","text"],"additionalProperties":false}}));
    }
    tools
}
