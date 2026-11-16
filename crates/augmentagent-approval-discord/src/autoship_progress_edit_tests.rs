use super::*;
use std::sync::Mutex;

fn context()->Context {Context{bot:11,owner:Some(22),author:22,channel:33,message:1,guild:None}}
fn reply()->Reply {Reply{content:"Autoship update [autoship:fixture-marker]".into(),nonce:Some("fixture-marker".into())}}
fn transport(server:&mockito::Server)->DiscordTransport {
    DiscordTransport(std::sync::Arc::new(serenity::http::HttpBuilder::new("test-only-token").proxy(server.url()).ratelimiter_disabled(true).build()))
}
fn edited(id:u64,channel:u64,author:u64)->String {serde_json::json!({"id":id.to_string(),"channel_id":channel.to_string(),"author":{"id":author.to_string(),"bot":true},"content":"x"}).to_string()}

#[tokio::test]
async fn edit_progress_patches_the_status_message_and_classifies_rejections() {
    let mut server=mockito::Server::new_async().await;
    let cases:Vec<(usize,String,Vec<(&str,&str)>,Result<(),Option<SendRejected>>)>=vec![
        (200,edited(102,33,11),vec![],Ok(())),
        (200,edited(103,33,11),vec![],Err(None)),
        (404,r#"{"code":10008,"message":"Unknown Message"}"#.into(),vec![],Err(Some(SendRejected{status:404,retry_after_ms:None}))),
        (429,r#"{"message":"rate limited","retry_after":1.25,"global":false}"#.into(),vec![("retry-after","1.25")],Err(Some(SendRejected{status:429,retry_after_ms:Some(1_250)}))),
        (502,"bad gateway".into(),vec![],Err(None)),
    ];
    for (status,body,headers,expected) in cases {
        let mut mock=server.mock("PATCH","/api/v10/channels/33/messages/102")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"content":reply().content})))
            .with_status(status).with_header("content-type","application/json").with_body(body).expect(1);
        for (k,v) in headers {mock=mock.with_header(k,v);}
        let mock=mock.create_async().await;
        let result=transport(&server).edit_progress(&context(),102,&reply()).await;
        match (result,expected) {
            (Ok(()),Ok(()))=>{},
            (Err(error),Err(want))=>assert_eq!(error.downcast_ref::<SendRejected>(),want.as_ref(),"HTTP {status}: {error:#}"),
            (other,want)=>panic!("HTTP {status}: {other:?} vs {want:?}"),
        }
        mock.assert_async().await;mock.remove_async().await;
    }
}

#[derive(Default)]
struct Outbox {target:Option<u64>,deferred:Mutex<Vec<Option<i64>>>,confirmed:Mutex<Vec<u64>>,lost:Mutex<usize>}
#[async_trait::async_trait]
impl crate::QueryHandler for Outbox {
    async fn answer(&self,_:&crate::AuditCtx,_:&str)->Result<String> {panic!("no chat")}
    fn autoship_progress_pending(&self,_:u64,_:u64)->Result<Vec<ProgressDelivery>> {Ok(vec![ProgressDelivery{context:context(),workflow:"owner/repo#1".into(),sequence:4}])}
    fn autoship_progress_claim(&self,_:&ProgressDelivery)->Result<Option<Reply>> {Ok(Some(reply()))}
    fn autoship_progress_edit_target(&self,_:&ProgressDelivery)->Result<Option<u64>> {Ok(self.target)}
    fn autoship_progress_confirm(&self,_:&ProgressDelivery,message:u64)->Result<()> {self.confirmed.lock().unwrap().push(message);Ok(())}
    fn autoship_progress_defer(&self,_:&ProgressDelivery,retry_after_ms:Option<i64>)->Result<()> {self.deferred.lock().unwrap().push(retry_after_ms);Ok(())}
    fn autoship_progress_status_lost(&self,_:&ProgressDelivery)->Result<()> {*self.lost.lock().unwrap()+=1;Ok(())}
}
/// status 0 = ambiguous failure (timeout after the request was sent).
struct Wire {edit:Result<(),SendRejected>,edits:Mutex<Vec<u64>>,sends:Mutex<usize>}
#[async_trait::async_trait]
impl Transport for Wire {
    async fn verify_owner_dm(&self,_:&Context)->Result<bool> {Ok(true)}
    async fn send(&self,_:&Context,_:&Reply)->Result<u64> {panic!("progress uses send_progress")}
    async fn send_progress(&self,_:&Context,_:&Reply)->Result<u64> {*self.sends.lock().unwrap()+=1;Ok(900)}
    async fn edit_progress(&self,_:&Context,message:u64,_:&Reply)->Result<()> {
        self.edits.lock().unwrap().push(message);
        match &self.edit {Ok(())=>Ok(()),Err(SendRejected{status:0,..})=>anyhow::bail!("timeout after edit"),Err(r)=>Err(r.clone().into())}
    }
}

#[tokio::test]
async fn drain_edits_the_status_message_and_safely_retries_failed_edits() {
    for (edit,confirmed,deferred,lost) in [
        (Ok(()),vec![102],vec![],0),
        // An edit is idempotent: an ambiguous failure is retried, never left uncertain.
        (Err(SendRejected{status:0,retry_after_ms:None}),vec![],vec![None],0),
        (Err(SendRejected{status:429,retry_after_ms:Some(1_250)}),vec![],vec![Some(1_250)],0),
        // Deleted status message: forget it and retry as a new message.
        (Err(SendRejected{status:404,retry_after_ms:None}),vec![],vec![None],1),
    ] {
        let outbox=Outbox{target:Some(102),..Default::default()};
        let wire=Wire{edit,edits:Mutex::new(Vec::new()),sends:Mutex::new(0)};
        drain_queued(&outbox,&wire,11,22).await.unwrap();
        assert_eq!(*wire.edits.lock().unwrap(),vec![102]);assert_eq!(*wire.sends.lock().unwrap(),0,"an edit never also sends");
        assert_eq!(*outbox.confirmed.lock().unwrap(),confirmed);assert_eq!(*outbox.deferred.lock().unwrap(),deferred);
        assert_eq!(*outbox.lost.lock().unwrap(),lost);
    }
    let outbox=Outbox::default();let wire=Wire{edit:Ok(()),edits:Mutex::new(Vec::new()),sends:Mutex::new(0)};
    drain_queued(&outbox,&wire,11,22).await.unwrap();
    assert!(wire.edits.lock().unwrap().is_empty());assert_eq!(*wire.sends.lock().unwrap(),1);assert_eq!(*outbox.confirmed.lock().unwrap(),vec![900]);
}
