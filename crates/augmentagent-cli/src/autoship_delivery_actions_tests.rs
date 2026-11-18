use super::*;
use super::super::{delivery::Destination,discord_ingress::DiscordOrigin,submission};

const MINUTE:i64=60_000;
fn origin()->DiscordOrigin {DiscordOrigin{bot:11,owner:22,channel:33,message:1}}
fn workflow(store:&mut WorkflowStore,pr:u64,bind:bool)->(String,i64) {
    let request=submission::Request{repository:"owner/repo".into(),target:submission::Target::Pr(pr),provider:None,until:submission::Milestone::Merged};
    let id=store.submit_authorized(&format!("cli-request-{pr}"),"github:owner",&request,1).unwrap().workflow;
    if bind {assert!(store.bind_delivery(&id,"github:owner",Destination{bot:11,owner:22,channel:33},2).unwrap());}
    let sequence=store.progress_events(&id).unwrap().last().unwrap().sequence;
    (id,sequence)
}

#[test]
fn uncertain_delivery_names_the_event_and_both_recovery_commands() {
    let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("state.db")).unwrap();
    let (id,sequence)=workflow(&mut store,42,true);
    store.claim_progress(&origin(),&id,sequence,3).unwrap().unwrap();
    let status=store.progress_delivery_at(&id,4).unwrap();
    assert_eq!(status.state,"delivery-uncertain");
    let event=format!("{id}:{sequence}");
    assert_eq!(status.uncertain_events,vec![event.clone()],"the owner needs the event id to resolve it");
    assert_eq!(status.recovery,vec![
        format!("autopr delivery resolve --event {event} --message MESSAGE_ID"),
        format!("autopr delivery retry --event {event} --allow-duplicate --request-id REQUEST_ID"),
    ]);
    // Serialized status carries it for the CLI/JSON surface.
    let json=serde_json::to_value(&status).unwrap();
    assert_eq!(json["uncertain_events"][0],event.as_str());
}

#[test]
fn degraded_and_dead_letter_delivery_offer_the_safe_workflow_retry() {
    let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("state.db")).unwrap();
    let (id,sequence)=workflow(&mut store,42,true);
    store.claim_progress(&origin(),&id,sequence,3).unwrap().unwrap();
    store.defer_progress(&origin(),&id,sequence,Some(60*MINUTE),4).unwrap();
    let degraded=store.progress_delivery_at(&id,1+16*MINUTE).unwrap();
    assert_eq!(degraded.state,"delivery-degraded");
    assert_eq!(degraded.recovery,vec![format!("autopr delivery retry --workflow {id}")]);
    assert!(degraded.uncertain_events.is_empty(),"a rejected send is not uncertain");
    let dead=store.progress_delivery_at(&id,1+25*60*MINUTE).unwrap();
    assert_eq!(dead.state,"delivery-dead-letter");
    assert_eq!(dead.recovery,vec![format!("autopr delivery retry --workflow {id}")],"a dead letter keeps its retry action");
}

#[test]
fn unconfigured_delivery_points_to_a_private_owner_destination_and_settled_delivery_needs_nothing() {
    let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("state.db")).unwrap();
    let (id,_)=workflow(&mut store,42,false);
    let status=store.progress_delivery_at(&id,3).unwrap();
    assert_eq!(status.state,"delivery-unconfigured");
    assert_eq!(status.recovery,vec![format!("autopr delivery rebind --workflow {id} --owner-dm")]);
    let (bound,sequence)=workflow(&mut store,43,true);
    store.claim_progress(&origin(),&bound,sequence,3).unwrap().unwrap();
    store.confirm_progress(&origin(),&bound,sequence,55,4).unwrap();
    let confirmed=store.progress_delivery_at(&bound,5).unwrap();
    assert_eq!(confirmed.state,"delivery-confirmed");assert!(confirmed.recovery.is_empty() && confirmed.uncertain_events.is_empty());
    // A fresh pending send inside the 15-minute window needs no owner action.
    let (fresh,_)=workflow(&mut store,44,true);
    let pending=store.progress_delivery_at(&fresh,3).unwrap();
    assert_eq!(pending.state,"delivery-pending");assert!(pending.recovery.is_empty());
}
