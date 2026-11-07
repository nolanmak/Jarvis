//! Native callback ingress. Proof capture must not retain the workflow mutex.
use super::*;
use std::sync::{Arc,Mutex};
use augmentagent_channel_core::{AuditLogger,native_vm::{PreparedLaunchRecord,ownership::VerifiedNativeObservation}};
pub(super) fn journal<P,R>(store:&Arc<Mutex<WorkflowStore>>,lease:&Lease,requested:Option<&Binding>,capture:impl FnOnce()->Result<P>,persist:impl FnOnce(&mut WorkflowStore,&Lease,&Binding,&P,i64)->Result<R>,clock:&dyn Fn()->i64)->Result<R> {
    let binding={
        let mut locked=store.lock().map_err(|_|anyhow::anyhow!("native callback store poisoned"))?;
        let binding=match requested {Some(binding)=>binding.clone(),None=>locked.snapshot(&lease.workflow)?.context("native callback workflow missing")?.checkpoint.binding};
        locked.validate_dispatch(lease,&binding,clock())?;binding
    };
    // Actual protected file IO occurs with no workflow mutex or transaction.
    let append=capture()?;
    let mut locked=store.lock().map_err(|_|anyhow::anyhow!("native callback store poisoned"))?;
    persist(&mut locked,lease,&binding,&append,clock())
}
pub(crate) fn record_launch(store:&Arc<Mutex<WorkflowStore>>,lease:&Lease,requested:Option<&Binding>,audit:&AuditLogger,prepared:&PreparedLaunchRecord,clock:&dyn Fn()->i64)->Result<native_launch::NativeLaunchRecord> {
    journal(store,lease,requested,||native_launch::verify_launch_append(audit,prepared),WorkflowStore::record_verified_launch_append,clock)
}
pub(crate) fn record_observation(store:&Arc<Mutex<WorkflowStore>>,lease:&Lease,requested:Option<&Binding>,audit:&AuditLogger,fact:&VerifiedNativeObservation,clock:&dyn Fn()->i64)->Result<native_observation::NativeObservation> {
    journal(store,lease,requested,||native_observation::verify_observation_append(audit,fact),WorkflowStore::record_verified_observation_append,clock)
}
pub(crate) fn install(store:&Arc<Mutex<WorkflowStore>>,lease:&Lease,audit:&Arc<AuditLogger>,requested:Arc<dyn Fn()->Result<Option<Binding>>+Send+Sync>)->Result<()> {
    let launch_store=store.clone();let launch_lease=lease.clone();let weak=Arc::downgrade(audit);let launch_requested=requested.clone();
    audit.set_native_launch_observer(Arc::new(move |prepared| {let audit=weak.upgrade().context("native launch controller is gone")?;record_launch(&launch_store,&launch_lease,launch_requested()?.as_ref(),&audit,prepared,&||chrono::Utc::now().timestamp_millis())?;Ok(())}))?;
    let observation_store=store.clone();let observation_lease=lease.clone();let weak=Arc::downgrade(audit);
    audit.set_native_observation_observer(Arc::new(move |fact| {let audit=weak.upgrade().context("native observation controller is gone")?;record_observation(&observation_store,&observation_lease,requested()?.as_ref(),&audit,fact,&||chrono::Utc::now().timestamp_millis())?;Ok(())}))
}
