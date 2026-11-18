use super::*;
use std::sync::Mutex;

fn context()->Context {Context{bot:11,owner:Some(22),author:22,channel:33,message:1,guild:None}}
fn reply()->Reply {Reply{content:"Autoship update [autoship:fixture-marker]".into(),nonce:Some("fixture-marker".into())}}
fn sent(id:u64)->String {serde_json::json!({"id":id.to_string(),"channel_id":"33","author":{"id":"11","username":"fixture","discriminator":"0","avatar":null,"bot":true},"content":"x","timestamp":"2026-10-31T00:00:00+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0}).to_string()}

/// Local-only client. (serenity's ratelimiter path ignores the proxy, so a
/// ratelimited fixture would reach the real API.)
fn transport(server:&mockito::Server)->DiscordTransport {
    DiscordTransport(std::sync::Arc::new(serenity::http::HttpBuilder::new("test-only-token").proxy(server.url()).ratelimiter_disabled(true).build()))
}

#[tokio::test]
async fn rate_limited_progress_send_is_a_single_definite_rejection_with_retry_after() {
    let mut server=mockito::Server::new_async().await;
    let mock=server.mock("POST","/api/v10/channels/33/messages")
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({"nonce":"fixture-marker","enforce_nonce":true})))
        .with_status(429).with_header("content-type","application/json").with_header("retry-after","2.5")
        .with_header("x-ratelimit-scope","user").with_header("x-ratelimit-bucket","fixture")
        .with_body(r#"{"message":"You are being rate limited.","retry_after":2.5,"global":false}"#).expect(1).create_async().await;
    let started=std::time::Instant::now();
    let error=tokio::time::timeout(std::time::Duration::from_secs(10),transport(&server).send_progress(&context(),&reply())).await
        .expect("progress send must not sleep inside the transport").unwrap_err();
    assert_eq!(error.downcast_ref::<SendRejected>(),Some(&SendRejected{status:429,retry_after_ms:Some(2_500)}),"{error:#}");
    assert!(started.elapsed()<std::time::Duration::from_secs(2),"retry belongs to the durable outbox, not an in-process sleep");
    mock.assert_async().await;
}

#[tokio::test]
async fn progress_send_classifies_rejections_receipts_and_ambiguous_failures() {
    let mut server=mockito::Server::new_async().await;
    for (status,body,expected) in [
        (403,r#"{"code":50013,"message":"Missing Permissions"}"#.to_string(),Err(Some(SendRejected{status:403,retry_after_ms:None}))),
        (404,r#"{"code":10003,"message":"Unknown Channel"}"#.to_string(),Err(Some(SendRejected{status:404,retry_after_ms:None}))),
        (500,r#"{"message":"500: Internal Server Error"}"#.to_string(),Err(None)),
        (200,sent(55),Ok(55)),
    ] {
        let mock=server.mock("POST","/api/v10/channels/33/messages").with_status(status).with_header("content-type","application/json").with_body(body).expect(1).create_async().await;
        let result=transport(&server).send_progress(&context(),&reply()).await;
        match (result,expected) {
            (Ok(id),Ok(want))=>assert_eq!(id,want),
            (Err(error),Err(want))=>assert_eq!(error.downcast_ref::<SendRejected>(),want.as_ref(),"HTTP {status}: {error:#}"),
            (other,want)=>panic!("HTTP {status}: {other:?} vs {want:?}"),
        }
        mock.assert_async().await;mock.remove_async().await;
    }
}

struct Outbox {deferred:Mutex<Vec<Option<i64>>>,confirmed:Mutex<Vec<u64>>}
#[async_trait::async_trait]
impl crate::QueryHandler for Outbox {
    async fn answer(&self,_:&crate::AuditCtx,_:&str)->Result<String> {panic!("no chat")}
    fn autoship_progress_pending(&self,_:u64,_:u64)->Result<Vec<ProgressDelivery>> {Ok(vec![ProgressDelivery{context:context(),workflow:"owner/repo/pr/42".into(),sequence:3}])}
    fn autoship_progress_claim(&self,_:&ProgressDelivery)->Result<Option<Reply>> {Ok(Some(reply()))}
    fn autoship_progress_confirm(&self,_:&ProgressDelivery,message:u64)->Result<()> {self.confirmed.lock().unwrap().push(message);Ok(())}
    fn autoship_progress_defer(&self,_:&ProgressDelivery,retry_after_ms:Option<i64>)->Result<()> {self.deferred.lock().unwrap().push(retry_after_ms);Ok(())}
}
struct Wire(Result<u64,SendRejected>,Mutex<usize>);
#[async_trait::async_trait]
impl Transport for Wire {
    async fn verify_owner_dm(&self,_:&Context)->Result<bool> {Ok(true)}
    async fn send(&self,_:&Context,_:&Reply)->Result<u64> {panic!("progress must use send_progress")}
    async fn send_progress(&self,_:&Context,_:&Reply)->Result<u64> {
        *self.1.lock().unwrap()+=1;
        match &self.0 {Ok(id)=>Ok(*id),Err(SendRejected{status:0,..})=>anyhow::bail!("timeout after send"),Err(rejected)=>Err(rejected.clone().into())}
    }
}

#[tokio::test]
async fn drain_parks_definite_rejections_for_retry_and_leaves_ambiguous_sends_uncertain() {
    for (wire,deferred,confirmed) in [
        (Err(SendRejected{status:429,retry_after_ms:Some(2_500)}),vec![Some(2_500)],vec![]),
        (Err(SendRejected{status:403,retry_after_ms:None}),vec![None],vec![]),
        (Err(SendRejected{status:0,retry_after_ms:None}),vec![],vec![]),
        (Ok(55),vec![],vec![55]),
    ] {
        let outbox=Outbox{deferred:Mutex::new(Vec::new()),confirmed:Mutex::new(Vec::new())};
        let wire=Wire(wire,Mutex::new(0));
        drain_queued(&outbox,&wire,11,22).await.unwrap();
        assert_eq!(*wire.1.lock().unwrap(),1,"exactly one send attempt per drain");
        assert_eq!(*outbox.deferred.lock().unwrap(),deferred);assert_eq!(*outbox.confirmed.lock().unwrap(),confirmed);
    }
}
