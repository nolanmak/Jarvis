//! Restricted, unreleased supervisor inspection for the actual bootstrap.
use super::*;
use std::sync::{Arc, atomic::AtomicBool};

/// Owns an actual supervisor behind its release barrier, the original launch,
/// and its permanently restricted controller. Saved identity is descriptive;
/// this object grants no model/body/input permission or native cleanup proof.
/// Field order keeps the controller alive while the group tears down children.
pub struct ClosedNativeInspection {
    group: crate::process_tree::ProcessGroup,
    _child: tokio::process::Child,
    bootstrap: ClosedNativeBootstrap,
    prepared: PreparedLaunchRecord,
    identity: serde_json::Value,
}

impl ClosedNativeBootstrap {
    /// Consume one original preparation to inspect its unreleased supervisor.
    /// Cancellation/failure tears down descendants and retains native intent;
    /// it does not invent retirement or permission to reconstruct the attempt.
    pub async fn inspect_resources(mut self) -> Result<ClosedNativeInspection> {
        self._controller.native_preflight_current()?;
        let prepared=self.prepared_record()?;
        self._controller.verify_native_preflight_reference(&prepared)?;
        let (child,group)=self.launch.spawn_admitted(Arc::new(AtomicBool::new(true)),None)?;
        // Build the owning object before the first await so cancellation keeps
        // the launch/controller alive through the original group's teardown.
        let mut result=ClosedNativeInspection {group,_child:child,bootstrap:self,
            prepared,identity:serde_json::Value::Null};
        result.identity=result.group.inspect_native_preflight(&result.prepared,&result.bootstrap._controller).await?;
        result.validate()?;
        Ok(result)
    }
}

impl ClosedNativeInspection {
    /// Descriptive identity of the original prepared attempt.
    pub fn prepared_record(&self)->&PreparedLaunchRecord {&self.prepared}
    /// Last inspected identity, not serialized permission to launch or replay.
    pub fn resource_identity(&self)->&serde_json::Value {&self.identity}
    /// Recheck original authority, startup correlation, kernel limits and the
    /// closed release barrier without dispatching, charging or submitting input.
    pub fn validate(&self)->Result<()> {
        self.bootstrap._controller.native_preflight_current()?;
        ensure!(self.bootstrap.prepared_record()?==self.prepared,"closed inspection original launch changed");
        let identity=self.group.validate_closed_native_resources(&self.prepared,&self.bootstrap._controller)?;
        ensure!(identity==self.identity,"closed inspection original resource identity changed");
        self.bootstrap._controller.native_preflight_current()
    }
}
