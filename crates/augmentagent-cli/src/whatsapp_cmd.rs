//! Operator commands for the live WhatsApp sidecar.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use augmentagent_channel_whatsapp::api::{default_socket_path, WaClient, WaError};
use augmentagent_channel_whatsapp::{control_enabled, PLATFORM};
use augmentagent_channel_whatsapp::{WaEvent, WhatsappAuth};
use augmentagent_store::{Store, SubscriptionMode, WhatsappOwnerConfig};
use tokio::sync::mpsc;

struct OwnedSidecar(Child);

impl Drop for OwnedSidecar {
    fn drop(&mut self) {
        // The pairing subprocess is temporary. The daemon's own sidecar,
        // if already running, is never owned or stopped by this command.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let _ = self.0.wait();
    }
}

fn normalize_phone(value: &str) -> Result<String> {
    let phone = value.trim().strip_prefix('+').unwrap_or(value.trim());
    if !(7..=15).contains(&phone.len()) || !phone.bytes().all(|b| b.is_ascii_digit()) {
        bail!("phone must be 7–15 E.164 digits, optionally prefixed with +");
    }
    Ok(phone.to_string())
}

fn device_phone(device_jid: &str) -> Option<&str> {
    let (user, server) = device_jid.split_once('@')?;
    if server != "s.whatsapp.net" {
        return None;
    }
    let phone = user.split(':').next()?;
    if phone.bytes().all(|b| b.is_ascii_digit()) {
        Some(phone)
    } else {
        None
    }
}

fn sidecar_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("AUGMENTAGENT_WA_SIDECAR_BIN") {
        return PathBuf::from(path);
    }
    let checkout = PathBuf::from("sidecars/wa-sidecar/wa-sidecar");
    if checkout.is_file() {
        checkout
    } else {
        PathBuf::from("augmentagent-wa-sidecar")
    }
}

async fn pairing_client(
    timeout: Duration,
) -> Result<(WaClient, mpsc::Receiver<WaEvent>, Option<OwnedSidecar>)> {
    let path = default_socket_path();
    let (events, receiver) = mpsc::channel(32);
    match WaClient::connect(&path, events.clone()).await {
        Ok(client) => return Ok((client, receiver, None)),
        Err(WaError::NotConnected { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    let binary = sidecar_binary();
    let child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start WhatsApp sidecar {}", binary.display()))?;
    let mut owned = OwnedSidecar(child);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match WaClient::connect(&path, events.clone()).await {
            Ok(client) => return Ok((client, receiver, Some(owned))),
            Err(WaError::NotConnected { .. }) if tokio::time::Instant::now() < deadline => {
                if let Some(status) = owned.0.try_wait().context("watch WhatsApp sidecar startup")? {
                    bail!("WhatsApp sidecar exited before its socket was ready: {status}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn persist_pairing(
    store: &Store,
    phone: &str,
    device_jid: &str,
    user_jid: &str,
    owner: &str,
    mode: &str,
) -> Result<()> {
    if device_phone(device_jid) != Some(phone) || user_jid != format!("{phone}@s.whatsapp.net") {
        bail!("paired WhatsApp device does not match requested phone {phone}");
    }
    let auth = WhatsappAuth {
        phone: phone.to_string(),
        device_jid: device_jid.to_string(),
        user_jid: user_jid.to_string(),
        paired_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    auth.save_configured()
        .context("save WhatsApp device index")?;
    WhatsappAuth::load_with_file_fallback(phone).context("verify WhatsApp device index")?;
    store.upsert_whatsapp_device(phone, device_jid, user_jid)?;
    store.set_whatsapp_owner_config(&WhatsappOwnerConfig {
        phone: phone.to_string(),
        owner_jid: owner.to_string(),
        control_chat_jid: owner.to_string(),
        mode: mode.to_string(),
    })?;
    Ok(())
}

async fn login(
    store: &Store,
    phone: &str,
    timeout_secs: u64,
    self_chat: bool,
    owner_jid: Option<String>,
) -> Result<()> {
    let phone = normalize_phone(phone)?;
    if timeout_secs == 0 {
        bail!("--timeout-secs must be greater than zero");
    }
    let self_jid = format!("{phone}@s.whatsapp.net");
    let (owner, mode) = if self_chat {
        (self_jid.clone(), "self_chat")
    } else {
        let owner = owner_jid.context("--owner-jid is required for a dedicated account")?;
        validate_chat_jid(&owner)?;
        if !owner.ends_with("@s.whatsapp.net") || owner == self_jid {
            bail!("a dedicated account requires a different personal owner JID");
        }
        (owner, "dedicated")
    };
    let devices = store.list_active_whatsapp_devices()?;
    if devices.iter().any(|device| device.phone != phone) {
        bail!("another WhatsApp phone is already indexed; unlink it before pairing a different account");
    }
    let timeout = Duration::from_secs(timeout_secs);
    let deadline = tokio::time::Instant::now() + timeout;
    let (client, mut events, _owned) = pairing_client(timeout).await?;
    let status = tokio::time::timeout_at(deadline, client.status())
        .await.context("WhatsApp pairing timed out while checking status")??;
    if status["paired"].as_bool() == Some(true) {
        let device_jid = status["device_jid"]
            .as_str()
            .context("paired sidecar omitted device_jid")?;
        persist_pairing(store, &phone, device_jid, &self_jid, &owner, mode)?;
        println!("WhatsApp device {device_jid} already paired and indexed");
        return Ok(());
    }
    tokio::time::timeout_at(deadline, client.start_pairing())
        .await.context("WhatsApp pairing timed out while starting QR")?
        .context("start WhatsApp QR pairing")?;
    println!("On your phone, open WhatsApp → Linked devices → Link a device.");
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("WhatsApp pairing timed out");
        }
        let event = tokio::time::timeout(remaining, events.recv())
            .await
            .context("WhatsApp pairing timed out")?
            .context("WhatsApp sidecar closed during pairing")?;
        match event {
            WaEvent::Qr { code } => {
                let qr =
                    qrcode::QrCode::new(code.as_bytes()).context("render WhatsApp pairing QR")?;
                let image = qr.render::<qrcode::render::unicode::Dense1x2>().build();
                println!("Scan this QR code (refreshes automatically):\n\x1b[30;47m{image}\x1b[0m");
            }
            WaEvent::PairSuccess {
                device_jid,
                user_jid,
            } => {
                persist_pairing(store, &phone, &device_jid, &user_jid, &owner, mode)?;
                println!("WhatsApp device {device_jid} paired and indexed");
                return Ok(());
            }
            WaEvent::LoggedOut { reason } => bail!("WhatsApp logged out during pairing: {reason}"),
            _ => {}
        }
    }
}

async fn unlink(store: &Store, phone: &str) -> Result<()> {
    let phone = normalize_phone(phone)?;
    let indexed = store.get_whatsapp_device_by_phone(&phone)?;
    let client = match client().await {
        Ok(client) => Some(client),
        Err(error)
            if indexed.is_none()
                && error
                    .downcast_ref::<WaError>()
                    .is_some_and(|e| matches!(e, WaError::NotConnected { .. })) =>
        {
            None
        }
        Err(error) => {
            return Err(error.context("sidecar must be running to unlink an indexed device"))
        }
    };
    if let Some(client) = client {
        let status = client.status().await?;
        if status["paired"].as_bool() == Some(true) {
            let actual = status["device_jid"]
                .as_str()
                .context("paired sidecar omitted device_jid")?;
            if device_phone(actual) != Some(phone.as_str()) {
                bail!("sidecar is paired to a different phone");
            }
            if let Some(device) = &indexed {
                if device.device_jid != actual {
                    bail!("sidecar device changed since the local index was saved");
                }
            }
            client
                .logout(actual)
                .await
                .context("unlink WhatsApp device")?;
        }
    }
    WhatsappAuth::delete_configured(&phone).context("remove WhatsApp device index")?;
    store.delete_whatsapp_device(&phone)?;
    println!("WhatsApp phone {phone} unlinked");
    Ok(())
}

use crate::WhatsappOp;

fn validate_chat_jid(value: &str) -> Result<()> {
    let (local, server) = value
        .split_once('@')
        .context("chat JID must be <id>@s.whatsapp.net or <id>@g.us")?;
    if local.is_empty()
        || value.matches('@').count() != 1
        || !matches!(server, "s.whatsapp.net" | "g.us")
        || !local
            .bytes()
            .all(|b| b.is_ascii_digit() || (server == "g.us" && b == b'-'))
    {
        bail!("invalid WhatsApp chat JID: {value}");
    }
    Ok(())
}

async fn client() -> Result<WaClient> {
    let (events, mut receiver) = mpsc::channel(32);
    let client = WaClient::connect(default_socket_path(), events)
        .await
        .context("connect to WhatsApp sidecar")?;
    // The CLI is not an inbound consumer. Drain its event copy so QR and
    // message bursts never block responses while a status/list call is live.
    tokio::spawn(async move { while receiver.recv().await.is_some() {} });
    Ok(client)
}

pub async fn run(op: WhatsappOp, store: Arc<Store>) -> Result<()> {
    match op {
        WhatsappOp::Status { json } => {
            let devices = store.list_active_whatsapp_devices()?;
            let owner = match devices.as_slice() {
                [device] => store.whatsapp_owner_config(&device.phone)?,
                _ => None,
            };
            let (sidecar_running, live) = match client().await {
                Ok(client) => (true, client.status().await.context("sidecar status")?),
                Err(error)
                    if error
                        .downcast_ref::<WaError>()
                        .is_some_and(|e| matches!(e, WaError::NotConnected { .. })) =>
                {
                    (
                        false,
                        serde_json::json!({"paired": false, "connected": false, "device_jid": ""}),
                    )
                }
                Err(error) => return Err(error),
            };
            let status = serde_json::json!({
                "sidecar_running": sidecar_running,
                "paired": live["paired"].as_bool().unwrap_or(false),
                "connected": live["connected"].as_bool().unwrap_or(false),
                "device_jid": live["device_jid"].as_str().unwrap_or(""),
                "indexed_devices": devices.len(),
                "configured": owner.is_some(),
                "owner_jid": owner.as_ref().map(|config| config.owner_jid.as_str()),
                "control_chat_jid": owner.as_ref().map(|config| config.control_chat_jid.as_str()),
                "account_mode": owner.as_ref().map(|config| config.mode.as_str()),
                "enabled": control_enabled(),
                "actively_listening": false,
            });
            if json {
                println!("{}", status);
            } else {
                println!("WhatsApp: {status}");
            }
        }
        WhatsappOp::Devices { json } => {
            let devices = store.list_active_whatsapp_devices()?;
            if json {
                println!("{}", serde_json::to_string(&devices)?);
            } else {
                println!("{} active WhatsApp device(s)", devices.len());
                for device in devices {
                    println!(
                        "  {}  {}  {}",
                        device.phone, device.device_jid, device.session_status
                    );
                }
            }
        }
        WhatsappOp::ListChats { limit, json } => {
            if limit == 0 {
                bail!("--limit must be greater than zero");
            }
            let chats = client().await?.list_chats(limit).await?;
            if json {
                println!("{}", serde_json::to_string(&chats)?);
            } else {
                println!("{} known WhatsApp chat(s)", chats.len());
                for chat in chats {
                    println!("  {}", serde_json::to_string(&chat)?);
                }
            }
        }
        WhatsappOp::Subscribe {
            chat_jid,
            mode,
            name,
        } => {
            validate_chat_jid(&chat_jid)?;
            if chat_jid.ends_with("@g.us") {
                bail!("group subscriptions are not supported by the current WhatsApp channel");
            }
            let mode = SubscriptionMode::parse(&mode).context("invalid subscription mode")?;
            let devices = store.list_active_whatsapp_devices()?;
            let [device] = devices.as_slice() else {
                bail!("exactly one paired WhatsApp device is required before subscribing");
            };
            let live = client().await?.status().await?;
            if live["paired"].as_bool() != Some(true)
                || live["device_jid"].as_str() != Some(device.device_jid.as_str())
            {
                bail!("WhatsApp sidecar is not paired to the indexed device; run `augmentagent whatsapp login` to reconcile it");
            }
            let sub = store.upsert_subscription(
                PLATFORM,
                &chat_jid,
                name.as_deref().unwrap_or(&chat_jid),
                mode,
                Some(&device.phone),
            )?;
            println!("{}", serde_json::to_string(&sub)?);
        }
        WhatsappOp::Subscriptions { json } => {
            let subs = store.list_active_subscriptions(PLATFORM)?;
            if json {
                println!("{}", serde_json::to_string(&subs)?);
            } else {
                println!("{} active WhatsApp subscription(s)", subs.len());
                for sub in subs {
                    println!("  {}  {}  {}", sub.id, sub.channel_id, sub.mode.as_str());
                }
            }
        }
        WhatsappOp::Unsubscribe { id } => {
            let sub = store
                .get_subscription(&id)?
                .context("subscription not found")?;
            if sub.platform != PLATFORM {
                bail!("subscription belongs to {}", sub.platform);
            }
            store.delete_subscription(&id)?;
            println!("subscription {id} deactivated");
        }
        WhatsappOp::AllowInbound { chat_jid } => {
            validate_chat_jid(&chat_jid)?;
            store.allow_whatsapp_inbound(&chat_jid)?;
            println!("inbound allowed for {chat_jid}");
        }
        WhatsappOp::DenyInbound { chat_jid } => {
            validate_chat_jid(&chat_jid)?;
            store.deny_whatsapp_inbound(&chat_jid)?;
            println!("inbound denied for {chat_jid}");
        }
        WhatsappOp::AllowOutbound { chat_jid } => {
            validate_chat_jid(&chat_jid)?;
            store.allow_whatsapp_outbound(&chat_jid)?;
            println!("outbound allowed for {chat_jid}");
        }
        WhatsappOp::DenyOutbound { chat_jid } => {
            validate_chat_jid(&chat_jid)?;
            store.deny_whatsapp_outbound(&chat_jid)?;
            println!("outbound denied for {chat_jid}");
        }
        WhatsappOp::Login {
            phone,
            timeout_secs,
            self_chat,
            owner_jid,
        } => tokio::select! {
            result = login(&store, &phone, timeout_secs, self_chat, owner_jid) => result?,
            _ = tokio::signal::ctrl_c() => bail!("WhatsApp pairing cancelled"),
        },
        WhatsappOp::Unlink { phone } => unlink(&store, &phone).await?,
        WhatsappOp::PollOnce { .. } => {
            bail!("WhatsApp poll-once is not wired yet; see issue #1231")
        }
    }
    Ok(())
}
