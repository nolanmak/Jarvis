//! Persistent OS-owner CLI conversation served by the daemon.
use crate::{
    local_phone_transport::{Handler, Listener, Request},
    phone_runtime::Endpoint,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::phone_agent::{ConversationIo, PhoneAgent};
use augmentagent_phone::{Clock, SystemClock};
use augmentagent_store::{
    phone::PhoneOrigin, Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef,
    SurfacePlatform,
};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

fn origin() -> Result<PhoneOrigin> {
    let id = format!("uid:{}", unsafe { libc::geteuid() });
    let account = SurfaceAccountRef::new(SurfacePlatform::new("cli")?, &id)?;
    Ok(PhoneOrigin {
        owner: SurfaceOwnerRef::new(account.clone(), &id)?,
        conversation: SurfaceConversationRef::new(account, "local", None)?,
    })
}
struct LocalIo {
    store: Arc<Store>,
}
#[async_trait]
impl ConversationIo for LocalIo {
    async fn history(&self, origin: &PhoneOrigin) -> Result<String> {
        Ok(self.store.local_chat_history(&origin.conversation)?)
    }
    async fn mirror(&self, origin: &PhoneOrigin, key: &str, text: &str) -> Result<()> {
        self.store
            .append_local_chat(&origin.conversation, key, text)?;
        Ok(())
    }
    fn audit_context(&self, _: &PhoneOrigin, turn_id: &str) -> AuditCtx {
        let mut ctx = AuditCtx::empty();
        ctx.owner_authorized = true;
        ctx.session_id = turn_id.into();
        ctx
    }
}
// Transcript writes happen inside the shared native queue, so a phone turn
// cannot overtake the text turn's durable assistant reply.
struct TextQuery<'a> {
    inner: &'a dyn QueryHandler,
    io: &'a LocalIo,
    origin: &'a PhoneOrigin,
    current: &'a str,
}
#[async_trait]
impl QueryHandler for TextQuery<'_> {
    async fn answer(&self, ctx: &AuditCtx, prompt: &str) -> Result<String> {
        self.io
            .mirror(
                self.origin,
                &format!("{}:owner", ctx.session_id),
                &format!("You: {}", self.current),
            )
            .await?;
        let answer = self.inner.answer(ctx, prompt).await?;
        self.io
            .mirror(
                self.origin,
                &format!("{}:agent", ctx.session_id),
                &format!("Jarvis: {answer}"),
            )
            .await?;
        Ok(answer)
    }
}

struct LocalHandler {
    endpoint: Arc<Endpoint>,
    store: Arc<Store>,
    origin: PhoneOrigin,
    query: Arc<dyn QueryHandler>,
    wiki: PathBuf,
    io: Arc<LocalIo>,
}
#[async_trait]
impl Handler for LocalHandler {
    async fn handle(&self, request: Request) -> Result<String> {
        self.endpoint
            .service
            .authorize(&self.origin.owner, &self.origin.conversation)?;
        if request.text.trim() == "phone history" {
            return self.io.history(&self.origin).await;
        }
        if augmentagent_phone::commands::recognizes(&request.text) {
            // Consume before side effects. Number/code commands never enter the
            // native prompt or transcript, including on failure or redelivery.
            if !self.store.consume_private_surface_command(
                &self.origin.conversation,
                &request.request_id,
                SystemClock.now_ms(),
            )? {
                return Ok("Command already received. Check phone status before requesting another action.".into());
            }
            return self
                .endpoint
                .tools
                .command(
                    &self.origin.owner,
                    &self.origin.conversation,
                    &request.request_id,
                    &request.text,
                )
                .await
                .context("unrecognized phone command");
        }
        let turn_id = format!("local:{}", request.request_id);
        let audit = self.io.audit_context(&self.origin, &turn_id);
        let history = self.io.history(&self.origin).await?;
        let query = TextQuery {
            inner: self.query.as_ref(),
            io: &self.io,
            origin: &self.origin,
            current: &request.text,
        };
        let answer = augmentagent_phone::context::scope(
            self.origin.clone(),
            None,
            augmentagent_approval_discord::interaction::owner_history(
                true,
                crate::phone_runtime::native_text_turn(
                    &self.store,
                    &self.endpoint.tools,
                    &query,
                    &audit,
                    &self.origin,
                    &self.wiki,
                    &history,
                    &request.text,
                ),
            ),
        )
        .await?;
        Ok(answer)
    }
}

pub fn spawn(
    endpoint: Arc<Endpoint>,
    store: Arc<Store>,
    query: Arc<dyn QueryHandler>,
    wiki: PathBuf,
    socket: PathBuf,
    stop: CancellationToken,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let origin = origin()?;
    anyhow::ensure!(
        endpoint
            .service
            .linked_surface_owners()
            .contains(&origin.owner),
        "explicit local OS-owner phone link required"
    );
    // Explicit operator configuration plus a same-UID private listener is the
    // authority source. Previously revoked bindings are never restored here.
    store.initialize_private_surface_owner(
        &origin.owner,
        &origin.conversation,
        SystemClock.now_ms(),
    )?;
    endpoint
        .service
        .authorize(&origin.owner, &origin.conversation)?;
    let listener = Listener::bind(&socket)?;
    let io = Arc::new(LocalIo {
        store: store.clone(),
    });
    let bridge = Arc::new(PhoneAgent::new(
        store.clone(),
        endpoint.service.clone(),
        query.clone(),
        io.clone(),
        wiki.clone(),
        Arc::new(|_| Ok(None)),
    ));
    let registration = endpoint.router.register("cli", bridge)?;
    let handler = Arc::new(LocalHandler {
        endpoint,
        store,
        origin,
        query,
        wiki,
        io,
    });
    Ok(tokio::spawn(async move {
        let _registration = registration;
        if listener.serve(handler, stop.clone()).await.is_err() {
            tracing::warn!("local conversation listener stopped; existing phone adapter remains until daemon shutdown");
            // A failed optional CLI listener must not stop the main daemon or
            // discard the adapter needed by an already accepted phone call.
            stop.cancelled().await;
        }
        Ok(())
    }))
}
pub async fn ask(text: String) -> Result<String> {
    let socket = std::env::var_os("AUGMENTAGENT_PHONE_LOCAL_SOCKET")
        .context("AUGMENTAGENT_PHONE_LOCAL_SOCKET is required for --daemon")?;
    let request = Request {
        version: 1,
        request_id: uuid::Uuid::new_v4().to_string(),
        text,
    };
    let response =
        crate::local_phone_transport::request(std::path::Path::new(&socket), &request).await?;
    anyhow::ensure!(response.ok, "{}", response.text);
    Ok(response.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_channel_core::{
        providers::ProviderKind, surface_conformance::RecordingNativeProvider, Reasoner,
    };
    use augmentagent_phone::{
        router::AgentRouter, tools::ToolServer, DialRequest, OwnerLink, PhoneConfig, PhoneService,
        ProviderError, Telephony,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Default)]
    struct Provider(AtomicUsize);
    #[async_trait]
    impl Telephony for Provider {
        async fn create(&self, _: DialRequest) -> std::result::Result<String, ProviderError> {
            let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(format!("CA-synthetic-local-{n}"))
        }
        async fn hangup(&self, _: &str) -> std::result::Result<(), ProviderError> {
            Ok(())
        }
    }
    struct Query {
        native: Arc<RecordingNativeProvider>,
    }
    #[async_trait]
    impl QueryHandler for Query {
        async fn answer(&self, ctx: &AuditCtx, text: &str) -> Result<String> {
            assert!(ctx.owner_authorized);
            assert!(augmentagent_phone::context::current().is_some());
            let mut opts = augmentagent_channel_core::reasoner::triage_opts(None);
            opts.session_id = Some(ctx.session_id.clone());
            self.native.call(&opts, text).await
        }
    }
    #[tokio::test]
    async fn private_daemon_commands_and_text_persist_without_number_history_or_redial() {
        // Unix socket names must fit even when managed build TMPDIR is long.
        let dir = tempfile::Builder::new().prefix("phone-")
            .tempdir_in(std::fs::canonicalize("/tmp").unwrap()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let origin = origin().unwrap();
        let provider = Arc::new(Provider::default());
        let service = Arc::new(PhoneService::new(
            store.clone(),
            provider.clone(),
            Arc::new(SystemClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "local-test-verification-key-32bytes",
                vec![OwnerLink {
                    surface_owner: origin.owner.clone(),
                    owner_id: "owner".into(),
                }],
            )
            .unwrap(),
        ));
        let tools = ToolServer::start(service.clone()).await.unwrap();
        let endpoint = Arc::new(Endpoint {
            service,
            tools,
            router: AgentRouter::new(store.clone()),
        });
        let native = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
        let query = Arc::new(Query {
            native: native.clone(),
        });
        let socket = dir.path().join("chat.sock");
        let stop = CancellationToken::new();
        let task = spawn(
            endpoint.clone(),
            store.clone(),
            query.clone(),
            dir.path().into(),
            socket.clone(),
            stop.clone(),
        )
        .unwrap();
        let send = |text: &str| Request {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            text: text.into(),
        };
        for text in ["first question", "second question"] {
            let response = crate::local_phone_transport::request(&socket, &send(text))
                .await
                .unwrap();
            assert!(response.ok, "{}", response.text);
        }
        let binding = store
            .surface_conversation(&origin.conversation)
            .unwrap()
            .unwrap();
        let setup = send("phone number set +12025550123");
        assert!(
            crate::local_phone_transport::request(&socket, &setup)
                .await
                .unwrap()
                .ok
        );
        assert!(
            crate::local_phone_transport::request(&socket, &setup)
                .await
                .unwrap()
                .ok
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            native.calls().len(),
            2,
            "number commands must bypass the model"
        );
        let history = crate::local_phone_transport::request(&socket, &send("phone history"))
            .await
            .unwrap();
        assert!(history.ok);
        assert!(history.text.contains("first question"));
        assert!(!history.text.contains("12025550123"));
        // Continue this exact local conversation through the registered phone
        // adapter, using signed loopback WebSocket ingress and fake telephony.
        use augmentagent_store::phone::CallState;
        use base64::Engine;
        use futures::{SinkExt, StreamExt};
        use hmac::{Hmac, Mac};
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let runtime = augmentagent_phone::runtime::PhoneRuntime::start(
            endpoint.service.clone(),
            Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
                "AC-local",
                "synthetic-callback-key",
            )),
            endpoint.router.clone(),
            listener,
        )
        .await
        .unwrap();
        store
            .phone_import_destination("owner", "+12025550123", SystemClock.now_ms())
            .unwrap();
        assert!(
            crate::local_phone_transport::request(&socket, &send("phone call"))
                .await
                .unwrap()
                .ok
        );
        let call = store.phone_latest_call("owner").unwrap().unwrap();
        assert!(!call.verification);
        let sid = call.provider_sid.as_deref().unwrap();
        store
            .phone_provider_event(
                &call.id,
                sid,
                CallState::AwaitingAcceptance,
                SystemClock.now_ms(),
            )
            .unwrap();
        store
            .phone_accept(&call.id, sid, SystemClock.now_ms())
            .unwrap();
        let mut request = format!("ws://{address}/phone/relay/{}", call.id)
            .into_client_request()
            .unwrap();
        let mut mac = Hmac::<sha1::Sha1>::new_from_slice(b"synthetic-callback-key").unwrap();
        mac.update(format!("wss://phone.example.test/phone/relay/{}", call.id).as_bytes());
        request.headers_mut().insert(
            "x-twilio-signature",
            base64::engine::general_purpose::STANDARD
                .encode(mac.finalize().into_bytes())
                .parse()
                .unwrap(),
        );
        let (mut relay, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        relay.send(Message::Text(serde_json::json!({"type":"setup","accountSid":"AC-local","callSid":sid,"sessionId":"VX-local"}).to_string())).await.unwrap();
        for n in 0..3 {
            relay.send(Message::Text(serde_json::json!({"type":"prompt","voicePrompt":format!("local phone turn {n}"),"last":true}).to_string())).await.unwrap();
            let output = tokio::time::timeout(std::time::Duration::from_secs(3), relay.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(output.is_text());
        }
        assert_eq!(native.calls().len(), 5);
        let transcript = crate::local_phone_transport::request(&socket, &send("phone history"))
            .await
            .unwrap();
        assert!(transcript.text.contains("local phone turn 2"));
        relay.close(None).await.unwrap();
        runtime.shutdown().await.unwrap();
        assert_eq!(provider.0.load(Ordering::SeqCst), 2);
        stop.cancel();
        task.await.unwrap().unwrap();
        let stop = CancellationToken::new();
        let task = spawn(
            endpoint.clone(),
            store.clone(),
            query,
            dir.path().into(),
            socket.clone(),
            stop.clone(),
        )
        .unwrap();
        assert!(
            crate::local_phone_transport::request(&socket, &send("third question"))
                .await
                .unwrap()
                .ok
        );
        assert_eq!(
            store
                .surface_conversation(&origin.conversation)
                .unwrap()
                .unwrap()
                .native_session_id,
            binding.native_session_id
        );
        stop.cancel();
        task.await.unwrap().unwrap();
        store.unbind_surface_owner(origin.owner.account()).unwrap();
        assert!(
            spawn(
                endpoint,
                store,
                Arc::new(Query { native }),
                dir.path().into(),
                socket,
                CancellationToken::new()
            )
            .is_err(),
            "restart cannot restore revoked authority"
        );
    }
}
