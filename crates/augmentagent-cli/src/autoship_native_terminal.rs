//! Terminal workflow identity, never a productive lease or native proof.
use super::*;
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalNativeCheckpoint {
    workflow:String,generation:i64,checkpoint_sha256:String,event_sequence:i64,binding:Binding,
}
impl TerminalNativeCheckpoint {
    pub(crate) fn workflow(&self)->&str {&self.workflow}
    pub(crate) fn generation(&self)->i64 {self.generation}
    pub(crate) fn event_sequence(&self)->i64 {self.event_sequence}
    pub(crate) fn binding(&self)->&Binding {&self.binding}
}
fn checked_checkpoint(raw:&str)->Result<Checkpoint> {
    ensure!(raw.len()<=32*1024,TransferReason::HistoryCapacity);
    let checkpoint:Checkpoint=serde_json::from_str(raw)?;
    checkpoint.binding.validate()?;
    ensure!(matches!(checkpoint.stage,Stage::Merged|Stage::Retired)
        && serde_json::to_value(&checkpoint)?==serde_json::from_str::<serde_json::Value>(raw)?,TransferReason::AuthorityConflict);
    Ok(checkpoint)
}
pub(super) fn verify_history(conn:&Connection,terminal:&TerminalNativeCheckpoint)->Result<()> {
    ensure!(terminal.generation>0 && terminal.event_sequence>0,TransferReason::AuthorityConflict);
    let (generation,raw):(i64,String)=conn.query_row("SELECT generation,payload FROM workflow_event WHERE workflow=?1 AND seq=?2 AND kind='checkpoint'",params![terminal.workflow,terminal.event_sequence],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let checkpoint=checked_checkpoint(&raw)?;
    ensure!(generation==terminal.generation && checkpoint.binding==terminal.binding
        && digest(&serde_json::to_value(&checkpoint)?)?==terminal.checkpoint_sha256,TransferReason::AuthorityConflict);
    Ok(())
}
pub(crate) fn capture(conn:&Connection,workflow:&str)->Result<TerminalNativeCheckpoint> {
    let (generation,raw):(i64,String)=conn.query_row("SELECT generation,checkpoint FROM workflow WHERE id=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let checkpoint=checked_checkpoint(&raw)?;
    let sequence:i64=conn.query_row("SELECT seq FROM workflow_event WHERE workflow=?1 AND kind='checkpoint' ORDER BY seq DESC LIMIT 1",[workflow],|r|r.get(0))?;
    let terminal=TerminalNativeCheckpoint {workflow:workflow.into(),generation,checkpoint_sha256:digest(&serde_json::to_value(&checkpoint)?)?,event_sequence:sequence,binding:checkpoint.binding};
    verify_history(conn,&terminal)?;Ok(terminal)
}
impl WorkflowStore {
    pub(crate) fn terminal_native_checkpoint(&mut self,workflow:&str)->Result<TerminalNativeCheckpoint> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let terminal=capture(&tx,workflow)?;tx.commit()?;Ok(terminal)
    }
    pub(crate) fn terminal_native_is_current(&mut self,terminal:&TerminalNativeCheckpoint)->Result<()> {
        ensure!(self.terminal_native_checkpoint(&terminal.workflow)?==*terminal,TransferReason::AuthorityConflict);Ok(())
    }
    pub(crate) fn acknowledge_terminal_native_unstarted(&mut self,terminal:&TerminalNativeCheckpoint,pending:&PendingNativeHandoff,proof:&augmentagent_channel_core::native_vm::ownership::NativeTransferInspection<'_>,now:i64)->Result<()> {
        unstarted::require_absent(pending,proof)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(capture(&tx,&terminal.workflow)?==*terminal
            && pending.request.workflow_id==terminal.workflow
            && terminal.generation>=pending.request.destination.owner.lease_generation,TransferReason::AuthorityConflict);
        unstarted::acknowledge(&tx,&terminal.workflow,terminal.generation,&terminal.binding,Some(terminal),pending,now)?;
        tx.commit()?;Ok(())
    }
}
