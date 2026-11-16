//! #1714: degraded, dead-letter, uncertain and unconfigured delivery must name
//! the stuck event and the exact owner command that recovers it. Read-only:
//! nothing here sends, retries or resolves.
use super::*;

const MAX_LISTED:usize=16;

pub(super) fn annotate(conn:&Connection,workflow:&str,status:&mut progress::DeliveryStatus,_now:i64)->Result<()> {
    let table:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_progress_delivery')",[],|r|r.get(0))?;
    if table {
        let mut stmt=conn.prepare("SELECT sequence FROM workflow_progress_delivery WHERE workflow=?1 AND state='uncertain' ORDER BY sequence LIMIT ?2")?;
        let rows=stmt.query_map(params![workflow,MAX_LISTED as i64],|r|r.get::<_,i64>(0))?;
        status.uncertain_events=rows.map(|s|s.map(|s|format!("{workflow}:{s}"))).collect::<rusqlite::Result<_>>()?;
    }
    status.recovery=match status.state.as_str() {
        "delivery-uncertain"=>status.uncertain_events.iter().flat_map(|event|[
            format!("autopr delivery resolve --event {event} --message MESSAGE_ID"),
            format!("autopr delivery retry --event {event} --allow-duplicate --request-id REQUEST_ID"),
        ]).collect(),
        "delivery-degraded"|"delivery-dead-letter"=>vec![format!("autopr delivery retry --workflow {workflow}")],
        "delivery-unconfigured"=>vec![format!("autopr delivery rebind --workflow {workflow} --owner-dm")],
        _=>Vec::new(),
    };
    Ok(())
}

#[cfg(test)]
#[path="autoship_delivery_actions_tests.rs"]
mod tests;
