//! Phone-only HTTP/WSS listener, private MCP broker and continuous cleanup.
//! TLS termination remains the operator's reverse proxy; no general tool route
//! is exposed on this listener. Dropping the runtime initiates bounded cleanup.
use crate::CleanupCall;
use crate::{
    ingress::ConversationAgent, tools::ToolServer, webhook::WebhookAuth, Error, PhoneService,
};
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

/// Fits inside the daemon's shared stop budget, including task teardown.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(18);
const CLEANUP_BUDGET: Duration = Duration::from_secs(16);

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Service(#[from] Error),
    #[error("phone listener or local tool socket failed")]
    Io(#[from] std::io::Error),
    #[error("phone runtime task did not finish cleanly")]
    Task,
}
pub struct PhoneRuntime {
    service: Arc<PhoneService>,
    tools: Arc<ToolServer>,
    stop: CancellationToken,
    tasks: Vec<JoinHandle<Result<(), RuntimeError>>>,
}
impl PhoneRuntime {
    pub async fn start(
        service: Arc<PhoneService>,
        auth: Arc<WebhookAuth>,
        agent: Arc<dyn ConversationAgent>,
        listener: TcpListener,
    ) -> Result<Self, RuntimeError> {
        service.enabled()?;
        let tools = ToolServer::start(service.clone()).await?;
        // Synchronous reconciliation marks stale calls stopped before callers
        // get access to the new runtime and can request another call.
        let initial = service.cleanup_due(true)?;
        let stop = CancellationToken::new();
        let router = crate::ingress::router(service.clone(), auth, agent);
        let listener_stop = stop.clone();
        let failed_stop = stop.clone();
        let http = tokio::spawn(async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(listener_stop.cancelled_owned())
                .await;
            failed_stop.cancel();
            result.map_err(RuntimeError::Io)
        });
        let watchdog_service = service.clone();
        let watchdog_stop = stop.clone();
        let failed_stop = stop.clone();
        let watchdog = tokio::spawn(async move {
            let result = watchdog(watchdog_service, initial, watchdog_stop).await;
            failed_stop.cancel();
            result
        });
        Ok(Self {
            service,
            tools,
            stop,
            tasks: vec![http, watchdog],
        })
    }
    pub fn tools(&self) -> &Arc<ToolServer> {
        &self.tools
    }
    pub fn is_running(&self) -> bool {
        !self.stop.is_cancelled() && self.tasks.iter().all(|task| !task.is_finished())
    }
    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        self.service.stopping.store(true, Ordering::SeqCst);
        self.stop.cancel();
        let mut failed = false;
        let deadline = tokio::time::Instant::now() + SHUTDOWN_BUDGET;
        for mut task in self.tasks.drain(..) {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(_) => failed = true,
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    failed = true;
                }
            }
        }
        if failed {
            Err(RuntimeError::Task)
        } else {
            Ok(())
        }
    }
}
impl Drop for PhoneRuntime {
    fn drop(&mut self) {
        self.service.stopping.store(true, Ordering::SeqCst);
        self.stop.cancel();
    }
}
/// Persist stop intent before reading provider configuration. Missing credentials
/// must not leave calls eligible to resume on a later startup.
pub fn mark_cleanup_pending(store: &augmentagent_store::Store) -> Result<bool, RuntimeError> {
    let calls = store.phone_active_calls().map_err(Error::from)?;
    for call in &calls {
        store
            .phone_request_stop(&call.owner, &call.id)
            .map_err(Error::from)?;
        if call.provider_sid.is_none() {
            store.phone_mark_uncertain(&call.id).map_err(Error::from)?;
        }
    }
    Ok(!calls.is_empty())
}

/// Retire persisted calls when normal phone startup is disabled or unavailable.
/// No listener, agent adapter, owner grant or tool socket is constructed.
pub async fn cleanup_without_runtime(
    store: Arc<augmentagent_store::Store>,
    provider: Arc<dyn crate::Telephony>,
) -> Result<(), RuntimeError> {
    if !mark_cleanup_pending(&store)? {
        return Ok(());
    }
    let service = Arc::new(PhoneService::new(
        store,
        provider,
        Arc::new(crate::SystemClock),
        crate::PhoneConfig {
            enabled: false,
            public_base: String::new(),
            verification_key: Vec::new(),
            links: Vec::new(),
            voice: crate::voice::VoiceConfig::default(),
            owner_first_name: None,
            debug_events: false,
            barge_in: crate::relay::BargeIn::default(),
            fast: None,
        },
    ));
    let initial = service.cleanup_due(true)?;
    let stop = CancellationToken::new();
    stop.cancel();
    watchdog(service, initial, stop).await
}

fn spawn_cleanup(
    service: &Arc<PhoneService>,
    calls: Vec<CleanupCall>,
    jobs: &mut JoinSet<Result<(), Error>>,
) {
    for call in calls {
        let service = service.clone();
        jobs.spawn(async move { service.cleanup_call(call).await });
    }
}
async fn watchdog(
    service: Arc<PhoneService>,
    initial: Vec<CleanupCall>,
    stop: CancellationToken,
) -> Result<(), RuntimeError> {
    let mut jobs = JoinSet::new();
    spawn_cleanup(&service, initial, &mut jobs);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown_at = None;
    let mut failure = false;
    loop {
        tokio::select! {
            _=stop.cancelled(),if shutdown_at.is_none()=>{
                service.stopping.store(true,Ordering::SeqCst);
                shutdown_at=Some(tokio::time::Instant::now());
            },
            Some(result)=jobs.join_next(),if !jobs.is_empty()=>{
                match result {Ok(outcome)=>{if outcome.is_err(){failure=true;}},Err(_)=>{failure=true;stop.cancel();}}
            },
            _=tick.tick()=>{
                match service.cleanup_due(false){Ok(due)=>spawn_cleanup(&service,due,&mut jobs),Err(_)=>{failure=true;stop.cancel();}}
                if let Some(at)=shutdown_at {
                    let active=service.store.phone_active_calls().map_err(Error::from)?;
                    if active.is_empty() && jobs.is_empty(){break;}
                    if at.elapsed()>=CLEANUP_BUDGET{
                        // Durable pending cleanup remains observable and will
                        // reconcile on restart; do not claim successful hangup.
                        failure=true;break;
                    }
                }
            },
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    if failure {
        Err(RuntimeError::Task)
    } else {
        Ok(())
    }
}
