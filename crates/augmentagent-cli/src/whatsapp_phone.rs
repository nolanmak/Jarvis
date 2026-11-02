//! Installed owner WhatsApp self-chat, sharing the native query handler and
//! phone bridge. Status is read over an authenticated same-user local socket.
use anyhow::{ensure, Result};
use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalActionHandler, QueryHandler};
use augmentagent_channel_core::phone_agent::PhoneAgent;
use augmentagent_channel_whatsapp::{
    phone_delivery::{self, History, WhatsappPhoneIo, WhatsappTransport},
    Jid, WaClient, WaEvent, WhatsappControlConfig, WhatsappControlSurface,
};
use augmentagent_phone::{Clock, SystemClock};
use augmentagent_store::{
    phone::PhoneOrigin, Store, SurfaceAccountRef, SurfaceOwnerRef, SurfacePlatform,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, Notify},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

struct ArchiveHistory(Arc<Store>);
#[async_trait]
impl History for ArchiveHistory {
    async fn transcript(&self, origin: &PhoneOrigin) -> Result<String> {
        // Provider fetch_history is unavailable. Only use this owner's bounded
        // imported archive; never claim a live provider history fetch succeeded.
        let mut rows = self.0.with_conn(|conn| {
            let mut stmt=conn.prepare("SELECT substr(fromEmail,1,256), substr(COALESCE(body,''),1,8192) FROM emails WHERE platform='whatsapp' AND accountEntityId='whatsapp-history' AND threadId=?1 ORDER BY COALESCE(julianday(receivedAt),2440587.5+firstSeenAt/86400000.0) DESC, firstSeenAt DESC, messageId DESC LIMIT 40")?;
            let rows=stmt.query_map([format!("whatsapp-history:{}",origin.conversation.conversation_id())],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.reverse();
        let mut history = String::new();
        for (sender, body) in rows {
            if body.lines().any(augmentagent_phone::commands::sensitive) {
                continue;
            }
            if history.len() + sender.len() + body.len() + 4 > 64 * 1024 {
                continue;
            }
            history.push_str(&format!("{sender}: {body}\n"));
        }
        Ok(history)
    }
}
fn owner_from_status(status: &serde_json::Value) -> Result<(Jid, SurfaceOwnerRef)> {
    ensure!(
        status["paired"] == true && status["connected"] == true,
        "WhatsApp sidecar is not connected and paired"
    );
    let linked = Jid::new(status["device_jid"].as_str().unwrap_or_default());
    ensure!(
        linked.is_personal()
            && !linked.user().is_empty()
            && linked.user().bytes().all(|c| c.is_ascii_digit()),
        "WhatsApp linked identity unavailable"
    );
    let account = SurfaceAccountRef::new(
        SurfacePlatform::new("whatsapp")?,
        format!("device:{}", linked.user()),
    )?;
    let owner = SurfaceOwnerRef::new(account, linked.bare())?;
    Ok((linked, owner))
}
fn stop_account(store: &Store, account: &SurfaceAccountRef) -> Result<()> {
    for call in store.phone_active_calls()? {
        if store.phone_origin(&call.id)?.owner.account() == account {
            store.phone_request_stop(&call.owner, &call.id)?;
        }
    }
    Ok(())
}

struct AccountCleanup {
    store: Arc<Store>,
    account: SurfaceAccountRef,
}
impl Drop for AccountCleanup {
    fn drop(&mut self) {
        if stop_account(&self.store, &self.account).is_err() {
            tracing::warn!("WhatsApp call cleanup needs attention");
        }
    }
}

/// Keep a transient sidecar disconnect from permanently disabling the surface.
/// Reconnect only restores transport, never revoked owner bindings or calls.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    endpoint: Arc<crate::phone_runtime::Endpoint>,
    store: Arc<Store>,
    query: Arc<dyn QueryHandler>,
    approvals: Option<Arc<dyn ApprovalActionHandler>>,
    wiki: PathBuf,
    socket: PathBuf,
    shutdown: CancellationToken,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let shutdown = shutdown.child_token();
        let _cancel = shutdown.clone().drop_guard();
        loop {
            let installation = tokio::select! {
                biased;
                _=shutdown.cancelled()=>return Ok(()),
                result=install(endpoint.clone(),store.clone(),query.clone(),approvals.clone(),wiki.clone(),socket.clone(),shutdown.clone())=>result,
            };
            match installation {
                Ok(worker)=>{
                    if !matches!(worker.await,Ok(Ok(()))) {
                        tracing::warn!("WhatsApp phone connection stopped; retrying local sidecar");
                    }
                },
                Err(_)=>tracing::warn!("WhatsApp phone unavailable: check local sidecar, explicit owner link and outbound self-chat allowlist"),
            }
            tokio::select! {
                biased;
                _=shutdown.cancelled()=>return Ok(()),
                _=tokio::time::sleep(Duration::from_secs(5))=>{},
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn install(
    endpoint: Arc<crate::phone_runtime::Endpoint>,
    store: Arc<Store>,
    query: Arc<dyn QueryHandler>,
    approvals: Option<Arc<dyn ApprovalActionHandler>>,
    wiki: PathBuf,
    socket: PathBuf,
    shutdown: CancellationToken,
) -> Result<JoinHandle<Result<()>>> {
    ensure!(
        augmentagent_channel_whatsapp::control_enabled(),
        "WhatsApp controls are disabled"
    );
    let (events_tx, mut events) = mpsc::channel(128);
    let client = WaClient::connect(socket, events_tx).await?;
    let status = tokio::time::timeout(Duration::from_secs(10), client.status()).await??;
    let (linked, owner) = owner_from_status(&status)?;
    ensure!(
        endpoint.service.linked_surface_owners().contains(&owner),
        "WhatsApp owner needs an explicit phone link"
    );
    ensure!(
        store.is_whatsapp_outbound_allowed(&linked.bare())?,
        "WhatsApp self-chat is not allowed for outbound control"
    );
    let account = owner.account().clone();
    let control = Arc::new(
        WhatsappControlSurface::new(
            client.clone(),
            store.clone(),
            WhatsappControlConfig {
                control_chat_jid: linked.bare(),
            },
            approvals,
            Some(query.clone()),
        )
        .with_phone_tools(linked.clone(), endpoint.tools.clone())?,
    );
    let wake = Arc::new(Notify::new());
    let io = Arc::new(WhatsappPhoneIo::new(
        store.clone(),
        endpoint.tools.clone(),
        linked.clone(),
        Arc::new(ArchiveHistory(store.clone())),
        wake.clone(),
    )?);
    let bridge = Arc::new(PhoneAgent::new(
        store.clone(),
        endpoint.service.clone(),
        query,
        io,
        wiki,
        Arc::new(|chat| {
            augmentagent_channel_core::model_selection::SelectionStore::new(
                augmentagent_channel_core::model_selection::config_path(),
            )
            .selected(Some(chat.conversation_id()))
        }),
    ).with_attention_provider(Arc::new(crate::phone_attention::PhoneAttention::new(store.clone()))));
    let transport = Arc::new(WhatsappTransport::new(
        client,
        store.clone(),
        endpoint.tools.clone(),
        linked.clone(),
    )?);
    let registration = endpoint.router.register("whatsapp", bridge)?;
    store.recover_surface_delivery_for(&account, SystemClock.now_ms())?;
    Ok(tokio::spawn(async move {
        let _registration = registration;
        let _cleanup = AccountCleanup {
            store: store.clone(),
            account: account.clone(),
        };
        let local_shutdown = shutdown.child_token();
        let _cancel = local_shutdown.clone().drop_guard();
        let mut delivery = JoinSet::new();
        delivery.spawn(phone_delivery::run_delivery(
            store.clone(),
            account.clone(),
            transport,
            wake,
            local_shutdown,
        ));
        let mut ordinary = JoinSet::new();
        let mut commands = JoinSet::new();
        let mut stops = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _=shutdown.cancelled()=>break,
                _=delivery.join_next()=>break,
                event=events.recv()=>match event {
                    None | Some(WaEvent::Disconnected)=>break,
                    Some(WaEvent::LoggedOut{..})=>{store.unbind_surface_owner(&account)?;break;},
                    Some(WaEvent::PairSuccess{device_jid,user_jid})=>{
                        if Jid::new(device_jid).bare()!=linked.bare() || Jid::new(user_jid).bare()!=linked.bare() {
                            store.unbind_surface_owner(&account)?;break;
                        }
                    },
                    Some(WaEvent::ReceivedMessage{message})=>{
                        if !message.is_verified_self_command(&linked) {continue;}
                        let words:Vec<_>=message.text.trim().trim_start_matches(['!','/']).split_whitespace().collect();
                        let stop=words.len()==2 && words[0].eq_ignore_ascii_case("phone") && words[1]=="stop";
                        let lane=if stop {&mut stops} else if augmentagent_phone::commands::recognizes(&message.text) {&mut commands} else {&mut ordinary};
                        let limit=if stop {2} else {16};
                        if lane.len()>=limit {tracing::warn!("WhatsApp control capacity reached; message not executed");continue;}
                        let handler=control.clone();
                        lane.spawn(async move {if handler.handle_control_message(&message).await.is_err(){tracing::warn!("WhatsApp control failed; check status before retrying");}});
                    },
                    _=>{},
                },
                _=ordinary.join_next(), if !ordinary.is_empty()=>{},
                _=commands.join_next(), if !commands.is_empty()=>{},
                _=stops.join_next(), if !stops.is_empty()=>{},
            }
        }
        ordinary.abort_all();
        commands.abort_all();
        stops.abort_all();
        // Persist cleanup before losing the adapter. The shared phone watchdog
        // retries provider hangup independently of WhatsApp connectivity.
        stop_account(&store, &account)?;
        Ok(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_approval_discord::{AuditCtx, QueryHandler};
    use augmentagent_phone::{
        router::AgentRouter, tools::ToolServer, DialRequest, OwnerLink, PhoneConfig, PhoneService,
        ProviderError, SystemClock, Telephony,
    };
    use augmentagent_store::{
        owner::ControlConversationKind, Store, SurfaceAccountRef, SurfaceConversationRef,
        SurfaceOwnerRef, SurfacePlatform,
    };
    use serde_json::{json, Value};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        sync::{mpsc, Notify},
    };
    use tokio_util::sync::CancellationToken;
    #[derive(Default)]
    struct Provider {
        starts: AtomicUsize,
        stops: AtomicUsize,
    }
    #[async_trait]
    impl Telephony for Provider {
        async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok("CA-fixture".into())
        }
        async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    struct Query(Arc<Notify>);
    #[async_trait]
    impl QueryHandler for Query {
        async fn answer(&self, _: &AuditCtx, _: &str) -> anyhow::Result<String> {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    async fn eventually(check: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn installed_sidecar_stops_during_native_turn_and_revokes_logout() {
        if std::env::var_os("WA_INSTALL_TEST_CHILD").is_none() {
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","whatsapp_phone::tests::installed_sidecar_stops_during_native_turn_and_revokes_logout","--nocapture"]).env("WA_INSTALL_TEST_CHILD","1").env("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED","1").output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let account = SurfaceAccountRef::new(
            SurfacePlatform::new("whatsapp").unwrap(),
            "device:12025550111",
        )
        .unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "12025550111@s.whatsapp.net").unwrap();
        let chat = SurfaceConversationRef::new(account.clone(), owner.sender_id(), None).unwrap();
        store.bind_surface_owner(&owner, 0).unwrap();
        store
            .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
            .unwrap();
        store.allow_whatsapp_outbound(owner.sender_id()).unwrap();
        store
            .phone_import_destination("owner", "+12025550123", 0)
            .unwrap();
        let provider = Arc::new(Provider::default());
        let service = Arc::new(PhoneService::new(
            store.clone(),
            provider.clone(),
            Arc::new(SystemClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "fixture-secret-at-least-32-bytes",
                vec![OwnerLink {
                    surface_owner: owner.clone(),
                    owner_id: "owner".into(),
                }],
            )
            .unwrap(),
        ));
        let tools = ToolServer::start(service.clone()).await.unwrap();
        let endpoint = Arc::new(crate::phone_runtime::Endpoint {
            service,
            tools,
            router: AgentRouter::new(store.clone()),
        });
        let socket = dir.path().join("wa.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (input, mut events) = mpsc::channel::<Value>(16);
        let connections = Arc::new(AtomicUsize::new(0));
        let statuses = connections.clone();
        let wrong_identity = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let sidecar_identity = wrong_identity.clone();
        let peer = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                loop {
                    tokio::select! {
                        line=lines.next_line()=>{ let Some(line)=line.unwrap() else {break}; let request:Value=serde_json::from_str(&line).unwrap();
                            let result=match request["op"].as_str().unwrap() { "status"=>{statuses.fetch_add(1,Ordering::SeqCst);json!({"paired":true,"connected":true,"device_jid":if sidecar_identity.load(Ordering::SeqCst) {"12025550112:3@s.whatsapp.net"}else{"12025550111:3@s.whatsapp.net"}})}, "send_text"=>json!({"message_id":"reply"}), other=>panic!("unexpected op {other}") };
                            w.write_all(format!("{}\n",json!({"version":1,"request_id":request["request_id"],"ok":true,"result":result})).as_bytes()).await.unwrap();
                        }, event=events.recv()=>{let Some(event)=event else{return};w.write_all(format!("{event}\n").as_bytes()).await.unwrap();if event["event"]=="disconnected" {break;}}
                    }
                }
            }
        });
        let entered = Arc::new(Notify::new());
        let shutdown = CancellationToken::new();
        assert!(
            install(
                endpoint.clone(),
                store.clone(),
                Arc::new(Query(entered.clone())),
                None,
                dir.path().into(),
                socket.clone(),
                shutdown.clone()
            )
            .await
            .is_err(),
            "unlinked sidecar identity was accepted"
        );
        assert_eq!(provider.starts.load(Ordering::SeqCst), 0);
        wrong_identity.store(false, Ordering::SeqCst);
        let worker = spawn(
            endpoint,
            store.clone(),
            Arc::new(Query(entered.clone())),
            None,
            dir.path().into(),
            socket,
            shutdown.clone(),
        );
        eventually(|| connections.load(Ordering::SeqCst) >= 2).await;
        let message = |id: &str, text: &str| json!({"version":1,"event":"received-message","id":id,"chat":"12025550111@s.whatsapp.net","sender":"12025550111:2@s.whatsapp.net","text":text,"timestamp":1,"from_me":true,"agent_outbound":false});
        input.send(message("call", "phone call")).await.unwrap();
        eventually(|| provider.starts.load(Ordering::SeqCst) == 1).await;
        input
            .send(message("text", "long running request"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        input.send(message("stop", "phone stop")).await.unwrap();
        eventually(|| provider.stops.load(Ordering::SeqCst) > 0).await;
        input
            .send(json!({"version":1,"event":"disconnected"}))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while connections.load(Ordering::SeqCst) < 3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            provider.starts.load(Ordering::SeqCst),
            1,
            "reconnect redialed"
        );
        assert!(store.surface_owner_binding(&account).unwrap().is_some());
        input
            .send(json!({"version":1,"event":"logged-out","reason":"fixture"}))
            .await
            .unwrap();
        eventually(|| store.surface_owner_binding(&account).unwrap().is_none()).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(store.surface_owner_binding(&account).unwrap().is_none());
        peer.abort();
        shutdown.cancel();
    }
    #[test]
    fn sidecar_status_requires_connected_personal_numeric_identity() {
        for status in [
            json!({}),
            json!({"paired":false,"connected":true,"device_jid":"12025550111:3@s.whatsapp.net"}),
            json!({"paired":true,"connected":false,"device_jid":"12025550111:3@s.whatsapp.net"}),
            json!({"paired":true,"connected":true,"device_jid":"group@g.us"}),
            json!({"paired":true,"connected":true,"device_jid":"unknown@s.whatsapp.net"}),
        ] {
            assert!(owner_from_status(&status).is_err());
        }
        let (linked, owner) = owner_from_status(
            &json!({"paired":true,"connected":true,"device_jid":"12025550111:3@s.whatsapp.net"}),
        )
        .unwrap();
        assert_eq!(linked.bare(), owner.sender_id());
        assert_eq!(owner.account().account_id(), "device:12025550111");
    }
    #[tokio::test]
    async fn archive_history_is_bounded_private_and_scoped_to_self_chat() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let account = SurfaceAccountRef::new(
            SurfacePlatform::new("whatsapp").unwrap(),
            "device:12025550111",
        )
        .unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "12025550111@s.whatsapp.net").unwrap();
        let chat = SurfaceConversationRef::new(account, owner.sender_id(), None).unwrap();
        for (index, body, thread) in [
            (0, "first safe message", "12025550111@s.whatsapp.net"),
            (
                1,
                "phone number verify 654321",
                "12025550111@s.whatsapp.net",
            ),
            (
                2,
                "/phone number set +12025550123",
                "12025550111@s.whatsapp.net",
            ),
            (3, "foreign conversation", "12025550112@s.whatsapp.net"),
            (4, "last safe message", "12025550111@s.whatsapp.net"),
        ] {
            store
                .upsert_email_backfill(
                    &augmentagent_store::Email {
                        message_id: format!("history-{index}"),
                        thread_id: Some(format!("whatsapp-history:{thread}")),
                        from: "owner".into(),
                        to: String::new(),
                        cc: String::new(),
                        attachments: vec![],
                        subject: String::new(),
                        body: body.into(),
                        date: String::new(),
                        account_entity_id: Some("whatsapp-history".into()),
                        platform: "whatsapp".into(),
                        kind: "dm".into(),
                    },
                    index,
                )
                .unwrap();
        }
        let origin = PhoneOrigin {
            owner,
            conversation: chat,
        };
        let history = ArchiveHistory(store.clone());
        assert_eq!(
            history.transcript(&origin).await.unwrap(),
            "owner: first safe message\nowner: last safe message\n"
        );
        // Import time can differ from source chronology. ISO timestamps with
        // different offsets must order by the actual instant, not their text.
        for (id, date, body) in [
            ("date-a", "2020-01-01T10:00:00+02:00", "earlier source"),
            ("date-b", "2020-01-01T09:00:00Z", "later source"),
        ] {
            store
                .upsert_email_backfill(
                    &augmentagent_store::Email {
                        message_id: id.into(),
                        thread_id: Some(format!("whatsapp-history:{}", origin.owner.sender_id())),
                        from: "owner".into(),
                        to: String::new(),
                        cc: String::new(),
                        attachments: vec![],
                        subject: String::new(),
                        body: body.into(),
                        date: date.into(),
                        account_entity_id: Some("whatsapp-history".into()),
                        platform: "whatsapp".into(),
                        kind: "dm".into(),
                    },
                    10,
                )
                .unwrap();
        }
        let ordered = history.transcript(&origin).await.unwrap();
        assert!(ordered.find("earlier source").unwrap() < ordered.find("later source").unwrap());
        for i in 5..55 {
            store
                .upsert_email_backfill(
                    &augmentagent_store::Email {
                        message_id: format!("large-{i}"),
                        thread_id: Some(format!("whatsapp-history:{}", origin.owner.sender_id())),
                        from: "owner".into(),
                        to: String::new(),
                        cc: String::new(),
                        attachments: vec![],
                        subject: String::new(),
                        body: "🦀".repeat(10000),
                        date: String::new(),
                        account_entity_id: Some("whatsapp-history".into()),
                        platform: "whatsapp".into(),
                        kind: "dm".into(),
                    },
                    i,
                )
                .unwrap();
        }
        let bounded = history.transcript(&origin).await.unwrap();
        assert!(!bounded.is_empty() && bounded.len() <= 64 * 1024);
        assert!(!bounded.contains("first safe message"));
    }
}
