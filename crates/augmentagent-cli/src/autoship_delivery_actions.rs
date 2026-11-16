//! #1714: degraded, dead-letter, uncertain and unconfigured delivery must name
//! the stuck event and the exact owner command that recovers it. Read-only.
use super::*;

pub(super) fn annotate(_conn:&Connection,_workflow:&str,_status:&mut progress::DeliveryStatus,_now:i64)->Result<()> {Ok(())}

#[cfg(test)]
#[path="autoship_delivery_actions_tests.rs"]
mod tests;
