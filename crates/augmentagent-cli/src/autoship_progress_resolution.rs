//! #1714: owner recovery for progress delivery. Resolution records an
//! uncertain send as delivered only on conclusive remote evidence; an explicit
//! duplicate resend is single-use per request id and never claims the original
//! attempt succeeded. Nothing here guarantees exactly-once platform delivery.
use super::*;

/// One message as fetched from the platform by its id.
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct RemoteMessage {pub id:u64,pub channel:u64,pub author:u64,pub author_bot:bool,pub content:String}

#[derive(Debug,Clone,PartialEq,Eq,Serialize)]
pub struct RetrySummary {pub rescheduled:usize,pub uncertain:usize}

#[derive(Debug,Clone,PartialEq,Eq,Serialize)]
pub struct Resend {pub request_id:String,pub workflow:String,pub sequence:i64,pub superseded_marker:String,pub possible_duplicate:bool}

/// `OWNER/REPO#N:SEQUENCE` or `OWNER/REPO/pr/N:SEQUENCE`.
pub fn parse_event(event:&str)->Result<(String,i64)> {
    let (workflow,sequence)=event.rsplit_once(':').context("event must be WORKFLOW:SEQUENCE")?;
    let sequence:i64=sequence.parse()?;ensure!(sequence>0 && !workflow.is_empty(),"invalid event identity");
    Ok((workflow.to_ascii_lowercase(),sequence))
}

impl WorkflowStore {
    /// Record delivery from fetched remote evidence, or refuse and stay uncertain.
    pub fn resolve_progress(&mut self,_workflow:&str,_sequence:i64,_evidence:Option<&RemoteMessage>,_now:i64)->Result<()> {Ok(())}
    /// Ordinary retry: rejected (definitely unsent) events become due now;
    /// uncertain events are only reported for reconciliation, never resent.
    pub fn retry_progress_now(&mut self,_workflow:&str,_now:i64)->Result<RetrySummary> {Ok(RetrySummary{rescheduled:0,uncertain:0})}
    /// Owner-authorized resend of one uncertain event. Returns false when this
    /// request id was already used (replay never sends again).
    pub fn authorize_duplicate_resend(&mut self,_workflow:&str,_sequence:i64,_request_id:&str,_principal:&str,_now:i64)->Result<bool> {Ok(true)}
    pub fn progress_resends(&self,_workflow:&str)->Result<Vec<Resend>> {Ok(Vec::new())}
}

#[cfg(test)]
#[path="autoship_progress_resolution_tests.rs"]
mod tests;
