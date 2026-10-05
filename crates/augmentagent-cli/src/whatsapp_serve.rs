//! Immediate WhatsApp owner conversation loop over the durable sidecar journal.
use anyhow::{bail, Result};
use augmentagent_channel_whatsapp::{
    api::WaClient, channel::replay_to_store, interactive::WhatsappInteractive, WaEvent, WaMessage,
};
use augmentagent_store::{
    surface_health::SurfaceListenerHealth, Store, SurfacePlatform, WhatsappDevice,
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn report(store: &Store, state: &str, phone: Option<&str>, dry_run: bool) {
    let now = now_ms();
    let _ = store.put_surface_listener_health(&SurfaceListenerHealth {
        platform: SurfacePlatform::new("whatsapp").expect("constant"),
        state: state.into(),
        detail: None,
        recovery: None,
        workspaces: phone.into_iter().map(str::to_string).collect(),
        dry_run,
        last_event_at_ms: None,
        last_send_at_ms: None,
        state_since_ms: now,
        heartbeat_at_ms: now,
        pid: std::process::id(),
    });
}

pub fn spawn(
    surface: Arc<WhatsappInteractive>,
    dry_run: bool,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let store = &surface.store;
        if dry_run || !augmentagent_channel_whatsapp::control_enabled() {
            report(store, "disabled", None, dry_run);
            return Ok(());
        }
        let devices = store.list_active_whatsapp_devices()?;
        let [device] = devices.as_slice() else {
            report(store, "not_configured", None, dry_run);
            return Ok(());
        };
        if store.whatsapp_owner_config(&device.phone)?.is_none() {
            report(store, "not_configured", Some(&device.phone), false);
            return Ok(());
        }
        // A single sidecar socket owns one paired device. Refuse competing
        // daemons before recovering interrupted turns or opening that socket.
        let state = augmentagent_channel_core::state_dir::state_dir()
            .ok_or_else(|| anyhow::anyhow!("state directory unavailable"))?;
        std::fs::create_dir_all(&state)?;
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(state.join("whatsapp-interactive.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("WhatsApp listener already running");
        }
        while !shutdown.is_cancelled() {
            if !store
                .get_whatsapp_device_by_phone(&device.phone)?
                .is_some_and(|d| d.active && d.device_jid == device.device_jid)
                || store.whatsapp_owner_config(&device.phone)?.is_none()
            {
                break;
            }
            report(store, "connecting", Some(&device.phone), false);
            match crate::whatsapp_cmd::pairing_client(Duration::from_secs(10)).await {
                Ok((client, events, _owned)) => {
                    if let Err(error) =
                        connected(Arc::clone(&surface), device, &client, events, &shutdown).await
                    {
                        tracing::warn!("WhatsApp listener reconnecting: {error:#}");
                    }
                }
                Err(error) => tracing::warn!("WhatsApp sidecar unavailable: {error:#}"),
            }
            report(store, "reconnecting", Some(&device.phone), false);
            tokio::select! {_=shutdown.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(5))=>{}}
        }
        report(store, "stopped", Some(&device.phone), false);
        Ok(())
    })
}

struct Active {
    message: WaMessage,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<Option<String>>>,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

async fn connected(
    surface: Arc<WhatsappInteractive>,
    device: &WhatsappDevice,
    client: &WaClient,
    mut events: tokio::sync::mpsc::Receiver<WaEvent>,
    shutdown: &CancellationToken,
) -> Result<()> {
    let live = client.status().await?;
    if live["device_jid"].as_str() != Some(&device.device_jid) {
        bail!("sidecar device does not match configured account");
    }
    let mut active: Option<Active> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _=shutdown.cancelled()=>return Ok(()),
            event=events.recv()=>match event {
                None=>bail!("sidecar socket closed"),
                Some(WaEvent::Disconnected)=>report(&surface.store,"reconnecting",Some(&device.phone),false),
                Some(WaEvent::LoggedOut{..})=>{surface.store.mark_whatsapp_device_logged_out(&device.phone)?;bail!("device logged out");},
                _=>{},
            },
            _=tick.tick()=>{
                let status=client.status().await?;
                if status["device_jid"].as_str()!=Some(&device.device_jid) {
                    bail!("sidecar device no longer matches the configured account");
                }
                if status["connected"]!=true {
                    report(&surface.store,"reconnecting",Some(&device.phone),false);
                    continue;
                }
                replay_to_store(client,&surface.store,&device.phone).await?;
                report(&surface.store,"connected",Some(&device.phone),false);
                if active.as_ref().is_some_and(|a|a.task.is_finished()) {
                    let mut done=active.take().expect("checked");
                    if let Some(reply)=(&mut done.task).await?? {
                        surface.deliver(client,&done.message,&reply).await?;
                    }
                    surface.store.mark_whatsapp_inbound_processed(&device.phone,&done.message.chat.bare(),&done.message.id)?;
                }
                let Some(owner)=surface.store.whatsapp_owner_config(&device.phone)? else {bail!("owner was unbound");};
                for event in surface.store.pending_whatsapp_control(&device.phone,&owner.control_chat_jid)? {
                    let message:WaMessage=serde_json::from_str(&event.payload_json)?;
                    if active.as_ref().is_some_and(|a|a.message.id==message.id) {continue;}
                    if !surface.authorized(&message)? {
                        surface.store.mark_whatsapp_inbound_processed(&device.phone,&event.chat_jid,&event.message_id)?;continue;
                    }
                    if message.text.trim().eq_ignore_ascii_case("cancel") {
                        if let Some(current)=&active {current.cancel.cancel();}
                        // Cancellation is harmless to repeat, and its response
                        // uses the same durable send key after a reconnect.
                        surface.deliver(client,&message,"Cancellation requested. Any active request will stop.").await?;
                        surface.store.mark_whatsapp_inbound_processed(&device.phone,&event.chat_jid,&event.message_id)?;continue;
                    }
                    if active.is_some(){continue;}
                    let cancel=shutdown.child_token();
                    let task_surface=Arc::clone(&surface);let task_message=message.clone();let task_cancel=cancel.clone();
                    let task=tokio::spawn(async move{task_surface.answer(&task_message,&task_cancel).await});
                    active=Some(Active{message,cancel,task});
                }
            }
        }
    }
}
