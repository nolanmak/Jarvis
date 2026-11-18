use super::*;
use super::super::{delivery::Destination,discord_ingress::DiscordOrigin,submission};

fn setup(path:&Path)->(WorkflowStore,String,i64) {
    let mut store=WorkflowStore::open(path).unwrap();
    let request=submission::Request{repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
    let id=store.submit_authorized("cli-request","github:owner",&request,1).unwrap().workflow;
    assert!(store.bind_delivery(&id,"github:owner",Destination{bot:11,owner:22,channel:33},2).unwrap());
    let pending=store.pending_progress_at(11,22,3).unwrap();assert_eq!(pending.len(),1);
    (store,id,pending[0].2)
}
fn origin()->DiscordOrigin {DiscordOrigin{bot:11,owner:22,channel:33,message:1}}

#[test]
fn rate_limited_send_retries_after_retry_after_with_the_same_marker_across_restart() {
    let root=tempfile::tempdir().unwrap();let path=root.path().join("state.db");
    let (mut store,id,sequence)=setup(&path);
    let (nonce,content)=store.claim_progress(&origin(),&id,sequence,10).unwrap().unwrap();
    // Discord answered 429: the message was not created.
    assert_eq!(store.defer_progress(&origin(),&id,sequence,Some(2_500),11).unwrap(),2_511);
    assert_eq!(store.progress_delivery_at(&id,12).unwrap().state,"delivery-pending","a definite rejection is not an uncertain send");
    assert!(store.pending_progress_at(11,22,2_510).unwrap().is_empty(),"retry-after must be honoured");
    assert!(store.claim_progress(&origin(),&id,sequence,2_510).unwrap().is_none());
    drop(store);let mut store=WorkflowStore::open(&path).unwrap();
    let pending=store.pending_progress_at(11,22,2_511).unwrap();
    assert_eq!(pending.len(),1,"deferred event survives restart and becomes due");assert_eq!(pending[0].2,sequence);
    let again=store.claim_progress(&origin(),&id,sequence,2_511).unwrap().unwrap();
    assert_eq!(again,(nonce,content),"retry reuses the durable marker and content");
    assert!(store.claim_progress(&origin(),&id,sequence,2_512).unwrap().is_none(),"in-flight retry is single-use");
    store.confirm_progress(&origin(),&id,sequence,77,2_513).unwrap();
    assert_eq!(store.progress_delivery_at(&id,2_514).unwrap().state,"delivery-confirmed");
}

#[test]
fn rejection_without_retry_after_backs_off_exponentially_capped_at_fifteen_minutes() {
    let root=tempfile::tempdir().unwrap();let (mut store,id,sequence)=setup(&root.path().join("state.db"));
    let mut now=10;let mut delays=Vec::new();
    for _ in 0..8 {
        store.claim_progress(&origin(),&id,sequence,now).unwrap().unwrap();
        let due=store.defer_progress(&origin(),&id,sequence,None,now).unwrap();
        delays.push(due-now);now=due;
    }
    assert_eq!(delays,vec![30_000,60_000,120_000,240_000,480_000,900_000,900_000,900_000]);
}

#[test]
fn only_a_dispatched_send_can_be_deferred_and_uncertain_sends_are_never_resent() {
    let root=tempfile::tempdir().unwrap();let (mut store,id,sequence)=setup(&root.path().join("state.db"));
    assert!(store.defer_progress(&origin(),&id,sequence,Some(1),5).is_err(),"nothing dispatched");
    store.claim_progress(&origin(),&id,sequence,10).unwrap().unwrap();
    // Timeout after send: stays uncertain, never re-claimed however long we wait.
    assert!(store.pending_progress_at(11,22,10+DAY_MS).unwrap().is_empty());
    assert!(store.claim_progress(&origin(),&id,sequence,10+DAY_MS).unwrap().is_none());
    store.confirm_progress(&origin(),&id,sequence,77,11).unwrap();
    assert!(store.defer_progress(&origin(),&id,sequence,Some(1),12).is_err(),"a confirmed send cannot be reopened");
    let other=DiscordOrigin{channel:34,..origin()};
    assert!(store.defer_progress(&other,&id,sequence,Some(1),12).is_err(),"no cross-destination deferral");
}
const DAY_MS:i64=86_400_000;
