//! Exact native attribution for staged verifier adjudication. Preparing a
//! strategy intent and actually admitting its model call are separate facts.
use super::*;

pub(crate) const PREFIX:&str="verifier-adjudication-native-v2:";
fn sha(value:&str)->bool {value.len()==64 && value.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))}
fn hash(bytes:&[u8])->String {format!("{:x}",Sha256::digest(bytes))}

#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DispatchIntent {
    pub version:u32,pub workflow:String,pub episode:String,pub input:String,pub epoch:String,
    pub stage_sha256:String,pub provider:String,pub round:u32,pub request_sha256:String,
    #[serde(default,skip_serializing_if="Option::is_none")] pub model:Option<String>,
}
impl DispatchIntent {
    pub(crate) fn validate(&self,workflow:&str)->Result<()> {
        ensure!(self.workflow==workflow && matches!(self.version,1|2) && self.round<2
            && sha(&self.episode) && sha(&self.input) && sha(&self.stage_sha256) && sha(&self.request_sha256)
            && !self.epoch.is_empty() && self.epoch.len()<=1024 && matches!(self.provider.as_str(),"codex"|"claude"),"invalid staged authority dispatch intent");
        ensure!(if self.version==1 {self.model.is_none()}else{self.model.as_ref().is_some_and(|model|!model.is_empty() && model.len()<=512 && model.bytes().all(|b|b.is_ascii_graphic()))},"authority intent model is not frozen");
        Ok(())
    }
    pub(crate) fn dispatch_key(&self)->String {hash(&serde_json::to_vec(&("staged-authority-dispatch-v1",&self.input,self.round)).unwrap())}
    pub(crate) fn native_purpose(&self)->Result<String> {
        self.validate(&self.workflow)?;ensure!(self.version==2,"legacy authority intent has no exact native admission domain");
        Ok(format!("{PREFIX}{}:{}",self.input,self.round))
    }
}
pub(crate) fn parse(purpose:&str)->Result<(&str,u32)> {
    let value=purpose.strip_prefix(PREFIX).context("not an adjudication native request")?;
    let (input,round)=value.split_once(':').context("adjudication native request lacks round")?;
    ensure!(sha(input) && matches!(round,"0"|"1"),"invalid adjudication native request identity");
    Ok((input,round.parse()?))
}
pub(crate) fn response_keys(purpose:&str)->Result<(String,String)> {
    let (input,round)=parse(purpose)?;
    let raw=hash(&serde_json::to_vec(&("staged-authority-native-response-v2",purpose))?);
    let proof=hash(&serde_json::to_vec(&("staged-authority-completion-v1",input,round))?);
    Ok((raw,proof))
}

/// Actual admission must consume the exact already-reserved controller intent,
/// not arbitrary reviewer text or another provider/model's setup attempt.
pub(super) fn require_prepared(tx:&Connection,workflow:&str,purpose:&str,provider:submission::Provider)->Result<()> {
    let (input,round)=parse(purpose)?;
    let key=hash(&serde_json::to_vec(&("staged-authority-dispatch-v1",input,round))?);
    let raw=read_scope_response(tx,workflow,&key)?.context("authority native request lacks prepared intent")?;
    let intent:DispatchIntent=serde_json::from_str(&raw)?;intent.validate(workflow)?;
    ensure!(intent.native_purpose()?==purpose && intent.dispatch_key()==key
        && intent.provider==match provider {submission::Provider::Codex=>"codex",submission::Provider::Claude=>"claude"},"authority native request changed its prepared ownership");
    for (purpose,epoch,limit) in [("verifier-amend-input",intent.input.as_str(),2u32),("verifier-amend-total",intent.epoch.as_str(),4u32)] {
        let count:Option<u32>=tx.query_row("SELECT attempts FROM bounded_attempt WHERE workflow=?1 AND purpose=?2 AND binding=?3",params![workflow,purpose,format!("epoch:{epoch}")],|r|r.get(0)).optional()?;
        ensure!(count.is_some_and(|count|count>0 && count<=limit),"authority native request lost its original strategy reservation");
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct OutcomeUnresolved;
impl std::fmt::Display for OutcomeUnresolved {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("original verifier authority native outcome requires evidence recovery")}
}
impl std::error::Error for OutcomeUnresolved {}

/// Legacy prepared intents predate exact native attribution. A missing outcome
/// is unknown, never proof of zero admission. Return their original proof for
/// private-byte validation by the observer before any new model role runs.
pub(super) fn legacy_custody(conn:&Connection,workflow:&str)->Result<Vec<NativeScopeCustody>> {
    let mut records=conn.prepare("SELECT key,response FROM scope_stage_response WHERE workflow=?1 ORDER BY key")?;
    let mut retained=Vec::new();
    for row in records.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))? {
        let (key,raw)=row?;
        let Ok(intent)=serde_json::from_str::<DispatchIntent>(&raw) else {continue};
        if intent.version!=1 || intent.dispatch_key()!=key {continue;}
        intent.validate(workflow)?;
        let proof_key=hash(&serde_json::to_vec(&("staged-authority-completion-v1",&intent.input,intent.round))?);
        let proof=read_scope_response(conn,workflow,&proof_key)?.ok_or(OutcomeUnresolved)?;
        let value:serde_json::Value=serde_json::from_str(&proof)?;
        ensure!(value["version"]==1 && value["attempt_sha256"].as_str()==Some(hash(raw.as_bytes()).as_str())
            && value["provider"].as_str()==Some(intent.provider.as_str()) && value.get("purpose").is_none(),OutcomeUnresolved);
        retained.push(NativeScopeCustody {input:key,response:raw,provenance:proof,
            provider:if intent.provider=="codex" {submission::Provider::Codex}else{submission::Provider::Claude}});
    }
    Ok(retained)
}
