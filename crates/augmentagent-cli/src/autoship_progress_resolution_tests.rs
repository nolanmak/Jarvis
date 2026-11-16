use super::*;
use super::super::{delivery::Destination,discord_ingress::DiscordOrigin,submission};

fn origin()->DiscordOrigin {DiscordOrigin{bot:11,owner:22,channel:33,message:1}}
/// One bound workflow with its latest event dispatched and left uncertain.
fn uncertain()->(tempfile::TempDir,WorkflowStore,String,i64,String,String) {
    let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("state.db")).unwrap();
    let request=submission::Request{repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
    let id=store.submit_authorized("cli-request","github:owner",&request,1).unwrap().workflow;
    assert!(store.bind_delivery(&id,"github:owner",Destination{bot:11,owner:22,channel:33},2).unwrap());
    let sequence=store.pending_progress_at(11,22,3).unwrap()[0].2;
    let (nonce,content)=store.claim_progress(&origin(),&id,sequence,4).unwrap().unwrap();
    (root,store,id,sequence,nonce,content)
}
fn remote(content:&str)->RemoteMessage {RemoteMessage{id:77,channel:33,author:11,author_bot:true,content:content.into()}}
fn state(store:&WorkflowStore,id:&str)->String {store.progress_delivery_at(id,5).unwrap().state}

#[test]
fn event_identity_parses_issue_and_pr_workflows() {
    assert_eq!(parse_event("Owner/Repo#7:3").unwrap(),("owner/repo#7".into(),3));
    assert_eq!(parse_event("owner/repo/pr/9:12").unwrap(),("owner/repo/pr/9".into(),12));
    for bad in ["owner/repo#7","owner/repo#7:0","owner/repo#7:x",":4"] {assert!(parse_event(bad).is_err(),"{bad}");}
}

#[test]
fn resolution_requires_conclusive_bound_evidence_and_is_repeatable() {
    let (_root,mut store,id,sequence,nonce,content)=uncertain();
    let refusals=[
        (None,"history unavailable"),
        (Some(remote(&content.replace(&format!("[autoship:{nonce}]"),""))),"missing marker"),
        (Some(RemoteMessage{channel:34,..remote(&content)}),"wrong destination"),
        (Some(RemoteMessage{author:99,..remote(&content)}),"wrong author"),
        (Some(RemoteMessage{author_bot:false,..remote(&content)}),"not a bot message"),
        (Some(remote(&format!("{content} [autoship:{nonce}]"))),"duplicated marker"),
    ];
    for (evidence,case) in refusals {
        assert!(store.resolve_progress(&id,sequence,evidence.as_ref(),5).is_err(),"{case} must not resolve");
        assert_eq!(state(&store,&id),"delivery-uncertain","{case}: stays visibly uncertain");
    }
    store.resolve_progress(&id,sequence,Some(&remote(&content)),6).unwrap();
    assert_eq!(state(&store,&id),"delivery-confirmed");
    store.resolve_progress(&id,sequence,Some(&remote(&content)),7).unwrap();
    assert!(store.resolve_progress(&id,sequence,Some(&RemoteMessage{id:78,..remote(&content)}),8).is_err(),"a different message cannot re-resolve");
}

#[test]
fn ordinary_retry_reschedules_only_definitely_unsent_events() {
    let (_root,mut store,id,sequence,_,_)=uncertain();
    assert_eq!(store.retry_progress_now(&id,10).unwrap(),RetrySummary{rescheduled:0,uncertain:1});
    assert!(store.pending_progress_at(11,22,10+DAY).unwrap().is_empty(),"an uncertain milestone is never resent by ordinary retry");
    // A definite rejection parked for 15 minutes becomes due on owner retry.
    store.resolve_progress(&id,sequence,None,11).unwrap_err();
    let (_root2,mut store2,id2,sequence2,_,_)=uncertain();
    store2.defer_progress(&origin(),&id2,sequence2,Some(15*60_000),10).unwrap();
    assert!(store2.pending_progress_at(11,22,11).unwrap().is_empty());
    assert_eq!(store2.retry_progress_now(&id2,11).unwrap(),RetrySummary{rescheduled:1,uncertain:0});
    assert_eq!(store2.pending_progress_at(11,22,11).unwrap().len(),1);
    let _=sequence;
}
const DAY:i64=86_400_000;

#[test]
fn owner_authorized_duplicate_resend_is_single_use_and_never_confirms_the_original() {
    let (_root,mut store,id,sequence,nonce,_)=uncertain();
    assert!(store.authorize_duplicate_resend(&id,sequence,"",  "github:owner",10).is_err(),"request id required");
    assert!(store.authorize_duplicate_resend(&id,sequence,"req-1","github:owner",10).unwrap());
    let pending=store.pending_progress_at(11,22,10).unwrap();assert_eq!(pending.len(),1,"exactly one new send generation");
    let (fresh,content)=store.claim_progress(&origin(),&id,sequence,11).unwrap().unwrap();
    assert_ne!(fresh,nonce,"the new generation carries its own marker");assert!(content.contains(&format!("[autoship:{fresh}]")));
    assert!(!content.contains(&nonce));
    // Replaying the same request cannot send again.
    assert!(!store.authorize_duplicate_resend(&id,sequence,"req-1","github:owner",12).unwrap());
    assert!(store.pending_progress_at(11,22,12).unwrap().is_empty());
    let resends=store.progress_resends(&id).unwrap();
    assert_eq!(resends,vec![Resend{request_id:"req-1".into(),workflow:id.clone(),sequence,superseded_marker:nonce.clone(),possible_duplicate:true}]);
    // The original attempt's marker can never be used to record delivery.
    let original=RemoteMessage{id:70,channel:33,author:11,author_bot:true,content:format!("x [autoship:{nonce}]")};
    assert!(store.resolve_progress(&id,sequence,Some(&original),13).is_err());
    store.confirm_progress(&origin(),&id,sequence,80,14).unwrap();
    assert!(store.authorize_duplicate_resend(&id,sequence,"req-2","github:owner",15).is_err(),"a confirmed event is not resent");
}

#[test]
fn duplicate_resend_requires_a_dispatched_uncertain_event() {
    let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("state.db")).unwrap();
    let request=submission::Request{repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
    let id=store.submit_authorized("cli-request","github:owner",&request,1).unwrap().workflow;
    assert!(store.bind_delivery(&id,"github:owner",Destination{bot:11,owner:22,channel:33},2).unwrap());
    let sequence=store.pending_progress_at(11,22,3).unwrap()[0].2;
    assert!(store.authorize_duplicate_resend(&id,sequence,"req-1","github:owner",4).is_err(),"never dispatched: ordinary delivery applies");
}
