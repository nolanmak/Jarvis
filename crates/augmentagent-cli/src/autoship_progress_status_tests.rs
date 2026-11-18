use super::*;
use super::super::{delivery::Destination,discord_ingress::DiscordOrigin,submission};

const SECOND:i64=1_000;
fn origin()->DiscordOrigin {DiscordOrigin{bot:11,owner:22,channel:33,message:1}}

struct Fixture {path:std::path::PathBuf,_root:tempfile::TempDir,id:String,lease:Lease}
fn fixture()->(WorkflowStore,Fixture) {
    let root=tempfile::tempdir().unwrap();let path=root.path().join("state.db");
    let mut store=WorkflowStore::open(&path).unwrap();
    let request=submission::Request{repository:"owner/repo".into(),target:submission::Target::Issue(1),provider:None,until:submission::Milestone::Merged};
    let id=store.submit_authorized("cli-request","github:owner",&request,1).unwrap().workflow;
    assert!(store.bind_delivery(&id,"github:owner",Destination{bot:11,owner:22,channel:33},2).unwrap());
    assert_eq!(store.next_command_job("owner/repo",3).unwrap().as_deref(),Some(id.as_str()));
    store.register(&id,"fix/one",Binding{head:"head-a".into(),contract:"contract-a".into(),evidence_hash:"dependency-a".into()},4).unwrap();
    let lease=store.claim(&id,"worker",5,DAY).unwrap().unwrap();
    (store,Fixture{path,_root:root,id,lease})
}
const DAY:i64=86_400_000;
fn stage(store:&mut WorkflowStore,f:&Fixture,stage:Stage,now:i64) {
    let mut cp=store.snapshot(&f.id).unwrap().unwrap().checkpoint;cp.stage=stage;store.checkpoint(&f.lease,&cp,now).unwrap();
}
/// Deliver whatever is due at `now` through the production claim/confirm path.
/// Returns (sequence, kind, edit target, message id) for each send.
fn deliver(store:&mut WorkflowStore,f:&Fixture,now:i64,next_id:&mut u64)->Vec<(i64,String,Option<u64>,u64)> {
    let mut sent=Vec::new();
    for (o,w,sequence) in store.pending_progress_at(11,22,now).unwrap() {
        assert_eq!(w,f.id);
        let Some(_)=store.claim_progress(&o,&w,sequence,now).unwrap() else {continue};
        let target=store.progress_edit_target(&o,&w,sequence).unwrap();
        let message=target.unwrap_or_else(||{*next_id+=1;*next_id});
        store.confirm_progress(&o,&w,sequence,message,now).unwrap();
        let kind=store.progress_events(&w).unwrap().into_iter().find(|e|e.sequence==sequence).unwrap().kind;
        sent.push((sequence,kind,target,message));
    }
    sent
}

#[test]
fn routine_updates_edit_one_status_message_throttled_while_milestones_append_immediately() {
    let (mut store,f)=fixture();let mut ids=100;
    let t=100*SECOND;
    let first=deliver(&mut store,&f,t,&mut ids);
    assert_eq!(first.len(),1);assert_eq!(first[0].2,None,"the first update is a new message");
    stage(&mut store,&f,Stage::Build,t+SECOND);
    let build=deliver(&mut store,&f,t+SECOND,&mut ids);
    assert_eq!(build.len(),1);assert_eq!(build[0].1,"stage");
    assert_eq!(build[0].2,None,"no status message yet: routine update creates it");
    let status=build[0].3;
    // Two routine updates inside the 30 s window: nothing is sent yet.
    stage(&mut store,&f,Stage::Verify,t+2*SECOND);
    stage(&mut store,&f,Stage::Review,t+3*SECOND);
    assert!(deliver(&mut store,&f,t+SECOND+29*SECOND,&mut ids).is_empty(),"routine edits are throttled to one per 30 s");
    drop(store);let mut store=WorkflowStore::open(&f.path).unwrap();
    let edit=deliver(&mut store,&f,t+SECOND+ROUTINE_EDIT_INTERVAL_MS,&mut ids);
    assert_eq!(edit.len(),1,"only the latest routine state is published");
    assert_eq!(edit[0].1,"stage");assert_eq!(edit[0].2,Some(status),"routine update edits the status message");
    assert_eq!(edit[0].0,store.progress_events(&f.id).unwrap().last().unwrap().sequence);
    // A blocker is published immediately as a new message, even inside the window.
    let now=t+SECOND+ROUTINE_EDIT_INTERVAL_MS+SECOND;
    stage(&mut store,&f,Stage::Held,now);
    let blocked=deliver(&mut store,&f,now,&mut ids);
    assert_eq!(blocked.len(),1);assert_eq!(blocked[0].1,"blocked");
    assert_eq!(blocked[0].2,None,"blockers append; they never overwrite the status message");
    assert_ne!(blocked[0].3,status);
}

#[test]
fn an_edit_must_confirm_against_its_status_message() {
    let (mut store,f)=fixture();let mut ids=100;let t=100*SECOND;
    deliver(&mut store,&f,t,&mut ids);
    stage(&mut store,&f,Stage::Build,t+SECOND);
    let status=deliver(&mut store,&f,t+SECOND,&mut ids)[0].3;
    stage(&mut store,&f,Stage::Verify,t+2*SECOND);
    let now=t+SECOND+ROUTINE_EDIT_INTERVAL_MS;
    let (o,w,sequence)=store.pending_progress_at(11,22,now).unwrap().pop().unwrap();
    store.claim_progress(&o,&w,sequence,now).unwrap().unwrap();
    assert_eq!(store.progress_edit_target(&o,&w,sequence).unwrap(),Some(status));
    assert!(store.confirm_progress(&o,&w,sequence,status+7,now).is_err(),"an edit receipt for another message is not delivery");
    store.confirm_progress(&o,&w,sequence,status,now).unwrap();
}

#[test]
fn deleted_status_message_is_replaced_by_a_new_message_in_the_same_destination() {
    let (mut store,f)=fixture();let mut ids=100;let t=100*SECOND;
    deliver(&mut store,&f,t,&mut ids);
    stage(&mut store,&f,Stage::Build,t+SECOND);
    let status=deliver(&mut store,&f,t+SECOND,&mut ids)[0].3;
    stage(&mut store,&f,Stage::Verify,t+2*SECOND);
    let now=t+SECOND+ROUTINE_EDIT_INTERVAL_MS;
    let (o,w,sequence)=store.pending_progress_at(11,22,now).unwrap().pop().unwrap();
    store.claim_progress(&o,&w,sequence,now).unwrap().unwrap();
    assert_eq!(store.progress_edit_target(&o,&w,sequence).unwrap(),Some(status));
    // Discord: 404 Unknown Message on edit.
    store.forget_status_message(&o,&w,sequence).unwrap();
    store.defer_progress(&o,&w,sequence,None,now).unwrap();
    let later=now+super::super::progress_retry::MAX_BACKOFF_MS;
    let (o,w,again)=store.pending_progress_at(11,22,later).unwrap().pop().unwrap();assert_eq!(again,sequence);
    store.claim_progress(&o,&w,sequence,later).unwrap().unwrap();
    assert_eq!(store.progress_edit_target(&o,&w,sequence).unwrap(),None,"retry sends a new status message");
    store.confirm_progress(&o,&w,sequence,555,later).unwrap();
    stage(&mut store,&f,Stage::Review,later+SECOND);
    let edit=deliver(&mut store,&f,later+ROUTINE_EDIT_INTERVAL_MS,&mut ids);
    assert_eq!(edit[0].2,Some(555),"the replacement becomes the status message");
}
