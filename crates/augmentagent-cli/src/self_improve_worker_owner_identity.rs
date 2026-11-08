//! Fresh configured Discord bot identity for worker metadata admission. No
//! message send, saved-origin self-authorization, or productive execution.
use super::*;

use reqwest::header::{HeaderValue,AUTHORIZATION};
use sha2::{Digest,Sha256};
use std::sync::Arc;
use std::time::Duration;
const BOT_ENDPOINT:&str="https://discord.com/api/v10/users/@me";
const MAX_RESPONSE:usize=8192;
const MAX_AGE:i64=30_000;

#[derive(Clone)]
struct ConfiguredOwner {owner:u64,authorization:HeaderValue}
impl ConfiguredOwner {
    fn parse(owner:&str,token:&str)->Result<Self> {
        let owner=owner.parse::<u64>().ok().filter(|id|*id>0).context("configured Discord owner unavailable")?;
        anyhow::ensure!(!token.is_empty() && token.len()<=4096,"configured Discord credentials unavailable");
        let mut authorization=HeaderValue::from_str(&format!("Bot {token}"))
            .map_err(|_|anyhow::anyhow!("configured Discord credentials invalid"))?;
        authorization.set_sensitive(true);
        Ok(Self {owner,authorization})
    }
    fn fingerprint(&self)->String {
        let mut digest=Sha256::new();digest.update(self.owner.to_be_bytes());digest.update(self.authorization.as_bytes());
        format!("{:x}",digest.finalize())
    }
}
type ReadConfig=Arc<dyn Fn()->Result<ConfiguredOwner>+Send+Sync>;
fn environment()->Result<ConfiguredOwner> {
    // Discard VarError contents: a non-Unicode credential must never reach logs.
    let owner=std::env::var("DISCORD_ALLOWED_USER_ID").map_err(|_|anyhow::anyhow!("configured Discord owner unavailable"))?;
    let token=std::env::var("DISCORD_BOT_TOKEN").map_err(|_|anyhow::anyhow!("configured Discord credentials unavailable"))?;
    ConfiguredOwner::parse(&owner,&token)
}
fn identity_client(https_only:bool)->Result<reqwest::Client> {
    let builder=reqwest::Client::builder().https_only(https_only).redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(10));
    // The non-HTTPS variant is private and used only by loopback fixtures.
    let builder=if https_only {builder}else{builder.no_proxy()};
    builder.build().map_err(|_|anyhow::anyhow!("configured bot verification client unavailable"))
}
struct Snapshot {policy:crate::autoship_discord_command::OwnerPolicy,fingerprint:String,verified_at:i64,read:ReadConfig}
impl Snapshot {
    fn check(&self)->Result<crate::autoship_discord_command::OwnerPolicy> {
        let now=chrono::Utc::now().timestamp_millis();
        anyhow::ensure!(now>=self.verified_at && now<=self.verified_at.saturating_add(MAX_AGE),"configured owner verification expired");
        anyhow::ensure!((self.read)()?.fingerprint()==self.fingerprint,"configured owner identity changed");
        Ok(self.policy)
    }
}
/// Retains one actual transport observation and its deadline/configuration
/// fence. No Clone/Deserialize; production retains no token or response body.
pub(super) struct VerifiedOwnerPolicy {snapshot:Arc<Snapshot>}
impl VerifiedOwnerPolicy {
    pub(super) fn current(&self)->Result<crate::autoship_discord_command::OwnerPolicy> {self.snapshot.check()}
    pub(super) fn expires_at(&self)->i64 {self.snapshot.verified_at.saturating_add(MAX_AGE)}
    pub(super) fn fence(&self)->Arc<dyn Fn()->Result<()>+Send+Sync> {
        let snapshot=self.snapshot.clone();Arc::new(move ||snapshot.check().map(|_|()))
    }
}
/// Production source is fixed HTTPS Discord and the worker's current runtime
/// configuration, never saved request identity or a caller-selected endpoint.
pub(super) async fn configured()->Result<VerifiedOwnerPolicy> {
    verify_with(&identity_client(true)?,BOT_ENDPOINT,Arc::new(environment)).await
}
async fn verify_with(client:&reqwest::Client,endpoint:&str,read:ReadConfig)->Result<VerifiedOwnerPolicy> {
    let configured=read()?;
    let mut response=client.get(endpoint).header(AUTHORIZATION,configured.authorization.clone())
        .header("Cache-Control","no-store").send().await.map_err(|_|anyhow::anyhow!("configured bot verification transport unavailable"))?;
    anyhow::ensure!(response.status()==reqwest::StatusCode::OK,"configured bot verification returned HTTP {}",response.status().as_u16());
    anyhow::ensure!(response.content_length().is_none_or(|length|length<=MAX_RESPONSE as u64),"configured bot response exceeds budget");
    let mut bytes=Vec::new();
    while let Some(chunk)=response.chunk().await.map_err(|_|anyhow::anyhow!("configured bot response unavailable"))? {
        anyhow::ensure!(bytes.len().saturating_add(chunk.len())<=MAX_RESPONSE,"configured bot response exceeds budget");bytes.extend_from_slice(&chunk);
    }
    #[derive(serde::Deserialize)]
    struct Bot {id:String,bot:bool}
    let bot:Bot=serde_json::from_slice(&bytes).map_err(|_|anyhow::anyhow!("configured bot response invalid"))?;
    let id=bot.id.parse::<u64>().ok().filter(|id|*id>0).context("configured bot identity invalid")?;
    anyhow::ensure!(bot.bot && bot.id==id.to_string(),"configured bot identity invalid");
    let proof=VerifiedOwnerPolicy {snapshot:Arc::new(Snapshot {
        policy:crate::autoship_discord_command::OwnerPolicy {bot:id,owner:configured.owner},
        fingerprint:configured.fingerprint(),verified_at:chrono::Utc::now().timestamp_millis(),read,
    })};
    proof.current()?;Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{Read,Write},net::TcpListener,sync::{Arc,Mutex},thread,time::{Duration,Instant}};
    struct Server {url:String,request:Arc<Mutex<String>>,worker:Option<thread::JoinHandle<()>>}
    impl Server {
        fn new(status:u16,body:&str)->Self {
            let listener=TcpListener::bind("127.0.0.1:0").unwrap();let address=listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();let request=Arc::new(Mutex::new(String::new()));let saved=request.clone();
            let body=body.to_owned();
            let worker=thread::spawn(move || {
                let until=Instant::now()+Duration::from_secs(3);
                let mut stream=loop {match listener.accept() {Ok((stream,_))=>break stream,Err(error) if error.kind()==std::io::ErrorKind::WouldBlock && Instant::now()<until=>thread::sleep(Duration::from_millis(1)),Err(error)=>panic!("fixture connection unavailable: {error}")}};
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut bytes=Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {let mut chunk=[0;1024];let count=stream.read(&mut chunk).unwrap();assert!(count>0 && bytes.len()+count<=16384);bytes.extend_from_slice(&chunk[..count]);}
                *saved.lock().unwrap()=String::from_utf8(bytes).unwrap();
                let location=if status==302 {"Location: http://127.0.0.1:9/never\r\n"}else{""};
                let response=format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\n{location}Connection: close\r\n\r\n{body}",body.len());
                let _=stream.write_all(response.as_bytes());
            });
            Self {url:format!("http://{address}/users/@me"),request,worker:Some(worker)}
        }
    }
    impl Drop for Server {fn drop(&mut self) {if let Some(worker)=self.worker.take() {let result=worker.join();if !thread::panicking(){result.unwrap();}}}}
    fn configuration()->(ReadConfig,Arc<Mutex<ConfiguredOwner>>) {
        let state=Arc::new(Mutex::new(ConfiguredOwner::parse("22","fixture-token").unwrap()));let read=state.clone();
        (Arc::new(move ||Ok(read.lock().unwrap().clone())),state)
    }
    #[tokio::test]
    async fn verifies_actual_transport_and_sensitive_credentials() {
        let server=Server::new(200,r#"{"id":"11","bot":true,"extra":"ignored"}"#);let (read,state)=configuration();
        let proof=verify_with(&identity_client(false).unwrap(),&server.url,read).await.unwrap();
        let policy=proof.current().unwrap();assert_eq!(policy.bot,11);assert_eq!(policy.owner,22);
        assert!(!format!("{:?}",state.lock().unwrap().authorization).contains("fixture-token"));
        let request=server.request.lock().unwrap();assert!(request.starts_with("GET /users/@me HTTP/1.1\r\n"));assert!(request.to_lowercase().contains("authorization: bot fixture-token\r\n"));
    }
    #[tokio::test]
    async fn revocation_reaches_an_already_captured_fence() {
        let server=Server::new(200,r#"{"id":"11","bot":true}"#);let (read,state)=configuration();
        let proof=verify_with(&identity_client(false).unwrap(),&server.url,read).await.unwrap();let fence=proof.fence();fence().unwrap();
        *state.lock().unwrap()=ConfiguredOwner::parse("33","fixture-token").unwrap();assert!(proof.current().is_err());assert!(fence().is_err());
        *state.lock().unwrap()=ConfiguredOwner::parse("22","other-token").unwrap();assert!(fence().is_err());
    }
    #[tokio::test]
    async fn configuration_change_during_lookup_cannot_publish_a_verifier() {
        let server=Server::new(200,r#"{"id":"11","bot":true}"#);
        let calls=std::sync::atomic::AtomicUsize::new(0);
        let read:ReadConfig=Arc::new(move || {
            let owner=if calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst)==0 {"22"}else{"33"};
            ConfiguredOwner::parse(owner,"fixture-token")
        });
        assert!(verify_with(&identity_client(false).unwrap(),&server.url,read).await.is_err());
    }
    #[tokio::test]
    async fn expiry_is_not_reminted_by_capturing_a_fence() {
        let server=Server::new(200,r#"{"id":"11","bot":true}"#);let (read,_)=configuration();
        let mut proof=verify_with(&identity_client(false).unwrap(),&server.url,read).await.unwrap();
        Arc::get_mut(&mut proof.snapshot).unwrap().verified_at=chrono::Utc::now().timestamp_millis()-30_001;
        assert!(proof.current().is_err());assert!((proof.fence())().is_err());assert!(proof.expires_at()<chrono::Utc::now().timestamp_millis());
    }
    #[tokio::test]
    async fn refuses_invalid_identities_without_echoing_response_content() {
        for body in [r#"{"id":"11","bot":false}"#,r#"{"id":"0","bot":true}"#,r#"{"id":"0011","bot":true}"#,r#"{"id":"11"}"#,"fixture-token"] {
            let server=Server::new(200,body);let (read,_)=configuration();
            let error=verify_with(&identity_client(false).unwrap(),&server.url,read).await.err().expect("invalid bot identity accepted");assert!(!error.to_string().contains("fixture-token"));
        }
    }
    #[tokio::test]
    async fn refuses_http_errors_and_redirects_without_sending_to_another_endpoint() {
        for status in [401,302] {
            let server=Server::new(status,"fixture-token");let (read,_)=configuration();
            let error=verify_with(&identity_client(false).unwrap(),&server.url,read).await.err().unwrap().to_string();assert!(error.contains(&status.to_string()),"{error}");assert!(!error.contains("fixture-token"));
        }
    }
    #[tokio::test]
    async fn bounds_identity_response_before_parsing() {
        let server=Server::new(200,&" ".repeat(8193));let (read,_)=configuration();
        let error=verify_with(&identity_client(false).unwrap(),&server.url,read).await.err().unwrap().to_string();assert!(error.contains("budget"),"{error}");
    }
    #[test]
    fn rejects_missing_or_invalid_configuration_without_exposing_credentials() {
        for (owner,token) in [("0","fixture-token"),("bad","fixture-token"),("22",""),("22","fixture-token\r\nInjected: true")] {
            let error=ConfiguredOwner::parse(owner,token).err().expect("invalid configuration accepted").to_string();assert!(!error.contains("fixture-token"));
        }
    }
}
