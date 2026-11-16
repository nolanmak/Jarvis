//! Authenticated Discord admission adapter. No message text can supply owner
//! identity, choose another checkout, launch a model, or bypass shared policy.
use crate::autoship_workflow::{WorkflowStore,discord_ingress::{DiscordOrigin,DiscordAdmission},submission::{Request,Target,Provider,Milestone}};
use anyhow::{ensure,Result};
use sha2::{Digest,Sha256};
use std::path::Path;

#[derive(Clone,Copy)]
pub(crate) struct OwnerPolicy {pub bot:u64,pub owner:u64}
pub(crate) struct Context {
    pub origin:DiscordOrigin,
    pub guild:Option<u64>,
    /// Set only after transport metadata proves the DM's sole recipient is
    /// the configured owner. Absence of a guild alone is insufficient.
    pub owner_dm_verified:bool,
}

fn authorize(policy:OwnerPolicy,context:&Context)->Result<()> {
    ensure!(policy.bot>0 && policy.owner>0 && context.origin.bot==policy.bot && context.origin.owner==policy.owner,
        "autoship-discord-owner-required");
    ensure!(context.origin.channel>0 && context.origin.message>0,"autoship-discord-identity-missing");
    // Guild support requires a separately verified private-thread binding. It
    // must not inherit ordinary query-channel admission or a forged mention.
    ensure!(context.guild.is_none() && context.owner_dm_verified,"autoship-private-conversation-required");
    Ok(())
}
fn parse(text:&str)->Result<Request> {
    ensure!(text.len()<=1024,"autoship-command-too-large");
    let words:Vec<_>=text.split_whitespace().collect();
    ensure!(matches!(words.len(),2|4) && words[0]=="!autoship","expected !autoship PR_URL [--provider codex|claude]");
    let url=words[1].strip_prefix('<').and_then(|s|s.strip_suffix('>')).unwrap_or(words[1]);
    let path=url.strip_prefix("https://github.com/").ok_or_else(||anyhow::anyhow!("expected a GitHub HTTPS PR URL"))?;
    let parts:Vec<_>=path.split('/').collect();
    ensure!(parts.len()==4 && parts[2]=="pull","expected an exact GitHub PR URL");
    let provider=if words.len()==4 {
        ensure!(words[2]=="--provider","unknown autoship option");
        Some(match words[3] {"codex"=>Provider::Codex,"claude"=>Provider::Claude,_=>anyhow::bail!("requested builder must be codex or claude")})
    } else {None};
    Request {repository:format!("{}/{}",parts[0],parts[1]),target:Target::Pr(parts[3].parse()?),provider,until:Milestone::Merged}.normalized()
}

use crate::autoship_workflow::discord_controls::{Action,PreparedControl};

/// `!autoship readopt <issue-or-PR URL> --reason TEXT` → (repository, target, reason).
fn parse_readopt(text:&str)->Result<Option<(String,Target,String)>> {
    ensure!(text.len()<=1024,"autoship-command-too-large");
    let words:Vec<_>=text.split_whitespace().collect();
    if words.first()!=Some(&"!autoship") || words.get(1)!=Some(&"readopt") {return Ok(None);}
    ensure!(words.len()>=5 && words[3]=="--reason","expected !autoship readopt <issue-or-PR URL> --reason TEXT");
    let url=words[2].strip_prefix('<').and_then(|s|s.strip_suffix('>')).unwrap_or(words[2]);
    let path=url.strip_prefix("https://github.com/").ok_or_else(||anyhow::anyhow!("expected a GitHub HTTPS issue or PR URL"))?;
    let parts:Vec<_>=path.split('/').collect();
    ensure!(parts.len()==4,"expected an exact GitHub issue or PR URL");
    let number:u64=parts[3].parse()?;ensure!(number>0,"positive target required");
    let target=match parts[2] {"issues"=>Target::Issue(number),"pull"=>Target::Pr(number),_=>anyhow::bail!("expected an issue or PR URL")};
    let repository=crate::autoship_workflow::submission::normalized_repository(&format!("{}/{}",parts[0],parts[1]))?.to_ascii_lowercase();
    Ok(Some((repository,target,words[4..].join(" "))))
}

/// #1713 — readopt through L's readoption service. The service is idempotent
/// by request id, so a crash before the reply commit re-runs harmlessly.
async fn readopt(repo:&Path,path:&Path,origin:&DiscordOrigin,text:&str,repository:&str,target:Target,reason:&str)->Result<()> {
    let digest=format!("{:x}",Sha256::digest(text.as_bytes()));
    if let Some(store)=WorkflowStore::open_read_only(path)? {
        if store.replay_discord_control(origin,&digest)?.is_some() {
            crate::self_improve::targeted::authorize_repository(repo,repository).await?;
            return Ok(());
        }
    }
    let (kind,number)=match target {Target::Issue(n)=>("issue",n),Target::Pr(n)=>("pr",n)};
    let label=format!("{repository}#{number}");
    let (refusal,content)=match target {
        Target::Pr(_)=>(Some("pr-readoption-unsupported"),format!("Autoship readopt {label}: pr-readoption-unsupported. PR readoption ships with explicit PR adoption (#1537); no hold was changed.")),
        Target::Issue(issue)=>{
            let request=crate::self_improve::readoption::Request {repository:repository.into(),issue,reason:reason.into(),request_id:origin.request_id()};
            match crate::self_improve::readoption::apply(repo,&request).await {
                Ok(_)=>(None,format!("Autoship readopted {label}: the held issue returns to the pool for fresh scope and review; hold history is preserved and nothing was dispatched.")),
                Err(error)=>{
                    let code=error.downcast_ref::<crate::self_improve::readoption::Refusal>().map_or("workflow-state-unavailable",|r|r.reason());
                    (Some(code),format!("Autoship readopt {label}: {code}. No hold was changed."))
                }
            }
        }
    };
    let target=serde_json::json!({"repository":repository,"kind":kind,"number":number});
    WorkflowStore::open(path)?.commit_discord_readopt(origin,&digest,&target,refusal,&content,chrono::Utc::now().timestamp_millis())?;
    Ok(())
}

fn parse_control(text:&str)->Result<Option<(Action,Option<String>)>> {
    ensure!(text.len()<=1024,"autoship-command-too-large");
    let words:Vec<_>=text.split_whitespace().collect();
    if words.first()!=Some(&"!autoship") {return Ok(None);}
    let action=match words.get(1).copied() {
        Some("status")=>Action::Status,Some("pause")=>Action::Pause,
        Some("resume")=>Action::Resume,Some("use")=>Action::Use,Some("cancel")=>Action::Cancel,_=>return Ok(None),
    };
    let workflow=if action==Action::Use {
        ensure!(words.len()==3,"expected !autoship use ID");Some(words[2].to_string())
    } else if action==Action::Cancel {
        // Terminal: never act on an implicit or concurrently changed selection.
        ensure!(words.len()==4 && words[2]=="--workflow","expected !autoship cancel --workflow ID");
        Some(words[3].to_string())
    } else {
        ensure!(words.len()==2 || (words.len()==4 && words[2]=="--workflow"),"expected !autoship status|pause|resume [--workflow ID]");
        words.get(3).map(|id|id.to_string())
    };
    Ok(Some((action,workflow)))
}

async fn control(repo:&Path,path:&Path,origin:&DiscordOrigin,text:&str,action:Action,explicit:Option<&str>)->Result<()> {
    let digest=format!("{:x}",Sha256::digest(text.as_bytes()));
    let store=WorkflowStore::open_read_only(path)?.ok_or_else(||anyhow::anyhow!("workflow-not-found"))?;
    // Replays resolve their immutable target before consulting today's selection.
    let replay=store.replay_discord_control(origin,&digest)?;
    let workflow=if let Some(reply)=&replay {
        store.discord_workflow(origin,Some(&reply.workflow))?.unwrap()
    } else {store.discord_workflow(origin,explicit)?.ok_or_else(||anyhow::anyhow!("workflow-not-found"))?};
    let request=store.command_request(&workflow)?.ok_or_else(||anyhow::anyhow!("workflow-not-found"))?;
    let revision=store.owner_control(&workflow)?.map_or(0,|c|c.revision);
    drop(store);
    let authorized=crate::self_improve::targeted::authorize_command(repo,&request).await;
    if replay.is_some() {authorized?;return Ok(());}
    let mut refusal=authorized.err().map(|_|"target-authorization-failed".to_string());
    if refusal.is_none() && action==Action::Resume && crate::self_improve::targeted::command_readiness(repo,&request).await.is_err() {
        refusal=Some("execution-readiness-blocked".into());
    }
    // #1713 — cancel runs T's idempotent fenced retirement before the reply is
    // committed; a crash in between re-runs it harmlessly on redelivery.
    let mut canceled=None;
    if action==Action::Cancel && refusal.is_none() {
        match crate::self_improve::targeted::cancel_workflow(repo,path,&workflow,&request.repository).await {
            Ok(())=>canceled=Some(format!("Autoship {workflow} canceled: stage retired, reason owner_canceled. Native teardown is unverified.")),
            Err(error)=>refusal=Some(error.downcast_ref::<crate::self_improve::targeted::CommandFailure>()
                .map_or("workflow-state-unavailable",|failure|failure.reason).into()),
        }
    }
    let status=if action==Action::Cancel {canceled} else if action==Action::Status && refusal.is_none() {
        // The configured daily cap, as every other status caller uses. Cap 0
        // is not a valid allowance and failed every registered workflow.
        let cap=std::env::var("AUGMENTAGENT_AUTOPR_DAILY_CAP").ok().and_then(|v|v.parse::<u32>().ok()).unwrap_or(5).max(1);
        let (code,value)=crate::autoship_commands::status(path,&workflow,chrono::Utc::now().timestamp_millis(),cap);
        if code==2 || code==4 {refusal=Some("workflow-state-unavailable".into());None}
        else {
            // Only bounded shared status fields, never raw provider or auth errors.
            let field=|key:&str|value[key].as_str().unwrap_or("unknown").chars().take(160).collect::<String>();
            let resource = crate::autoship_workflow::resource_refusal::render_status(&value)
                .map(|text| format!(" {text}.")).unwrap_or_default();
            Some(format!("Autoship {workflow}: stage {}; reason {}; provider {}; PR {}. Merge: {}. Deployment: {}. Completion observed: {}.{resource}",
                field("stage"),field("reason"),field("provider"),field("pr_url"),
                value["merge"]["state"].as_str().unwrap_or("unknown"),value["deployment"]["state"].as_str().unwrap_or("unknown"),value["complete"]==true))
        }
    } else {None};
    WorkflowStore::open(path)?.commit_discord_control(origin,&digest,&PreparedControl {
        workflow,action,explicit:explicit.is_some(),revision,refusal,status,
    },chrono::Utc::now().timestamp_millis())?;
    Ok(())
}

async fn owner_followup(repo:&Path,path:&Path,origin:&DiscordOrigin,text:&str)->Result<()> {
    ensure!(!text.trim().is_empty() && text.len()<=16*1024,"owner message exceeds bound");
    let digest=format!("{:x}",Sha256::digest(text.as_bytes()));
    let store=WorkflowStore::open_read_only(path)?.ok_or_else(||anyhow::anyhow!("workflow-not-found"))?;
    let replay=store.replay_discord_control(origin,&digest)?;
    let workflow=if let Some(reply)=&replay {store.discord_workflow(origin,Some(&reply.workflow))?.unwrap()}
        else {store.discord_workflow(origin,None)?.ok_or_else(||anyhow::anyhow!("workflow-selection-required"))?};
    let request=store.command_request(&workflow)?.ok_or_else(||anyhow::anyhow!("workflow-not-found"))?;drop(store);
    let authorization=crate::self_improve::targeted::authorize_command(repo,&request).await;
    if replay.is_some() {authorization?;return Ok(());}
    WorkflowStore::open(path)?.enqueue_owner_message(origin,&workflow,text,authorization.err().map(|_|"target-authorization-failed"),chrono::Utc::now().timestamp_millis())?;
    Ok(())
}

pub(crate) async fn admit(repo:&Path,path:&Path,policy:OwnerPolicy,context:&Context,text:&str)->Result<DiscordAdmission> {
    authorize(policy,context)?;
    let request=parse(text)?;
    // The configured Discord owner and the GitHub checkout grant are separate
    // authorities. Both must pass, including on a replay after target closure.
    crate::self_improve::targeted::authorize_command(repo,&request).await?;
    let replay=WorkflowStore::open_read_only(path)?.map(|store|store.replay_authorized(
        &context.origin.request_id(),&context.origin.principal(),&request)).transpose()?.flatten();
    if replay.is_none() {crate::self_improve::targeted::validate_command(repo,&request).await?;}
    let digest=format!("{:x}",Sha256::digest(text.as_bytes()));
    WorkflowStore::open(path)?.submit_discord_authorized(&context.origin,&request,&digest,chrono::Utc::now().timestamp_millis())
}

type GatewayContext = augmentagent_approval_discord::autoship::Context;
type GatewayReply = augmentagent_approval_discord::autoship::Reply;

fn gateway_origin(context:&GatewayContext)->Result<DiscordOrigin> {
    let origin=DiscordOrigin {bot:context.bot,owner:context.author,channel:context.channel,message:context.message};
    authorize(OwnerPolicy {bot:context.bot,owner:context.owner.unwrap_or(0)},
        &Context {origin:origin.clone(),guild:context.guild,owner_dm_verified:true})?;
    Ok(origin)
}

pub(crate) fn gateway_bound(path:&Path,context:&GatewayContext)->Result<bool> {
    let origin=gateway_origin(context)?;
    let Some(store)=WorkflowStore::open_read_only(path)? else {return Ok(false);};
    match store.discord_workflow(&origin,None) {
        Ok(workflow)=>Ok(workflow.is_some()),
        Err(error) if error.to_string()=="workflow-selection-required"=>Ok(true),
        Err(error)=>Err(error),
    }
}

/// Only the verified gateway boundary calls this adapter. It deliberately does
/// not install an ordinary wiki conversation as a workflow coding session.
pub(crate) async fn gateway_command(repo:&Path,path:&Path,context:&GatewayContext,text:&str)->Result<Option<GatewayReply>> {
    let origin=gateway_origin(context)?;
    let plain=|content:&str|Some(GatewayReply {content:content.into(),nonce:None});
    if !augmentagent_approval_discord::autoship::recognizes(text) {
        ensure!(gateway_bound(path,context)?,"autoship conversation is not bound");
        // #1713 — several workflows bound and none selected: ask, never guess.
        if let Some(store)=WorkflowStore::open_read_only(path)? {
            if store.discord_workflow(&origin,None).is_err_and(|e|e.to_string()=="workflow-selection-required") {
                let bound=store.discord_bound_workflows(&origin)?;
                return Ok(plain(&format!("Several autoship workflows are bound to this DM: {}. Select one with `!autoship use ID` (or add `--workflow ID`). This message was not queued and changed nothing.",
                    bound.iter().take(10).map(|id|format!("`{id}`")).collect::<Vec<_>>().join(", "))));
            }
        }
        return Ok(plain("Autoship coding sessions are unavailable in this build. This message was not queued and did not pause the workflow. Use `!autoship status`, `pause`, or `resume` to control the PR."));
    }
    match parse_readopt(text) {
        Ok(Some((repository,target,reason)))=>{
            if readopt(repo,path,&origin,text,&repository,target,&reason).await.is_err() {
                return Ok(plain("Autoship readopt could not be recorded. Check the message is the original request (a request ID cannot be reused for another target) and current GitHub access. No hold change is confirmed."));
            }
            return gateway_claim_ack(path,context);
        }
        Err(_)=>return Ok(plain("Autoship command not recognized. Use `!autoship readopt <issue URL> --reason TEXT`.")),
        Ok(None)=>{},
    }
    match parse_control(text) {
        Ok(Some((action,explicit)))=> {
            if control(repo,path,&origin,text,action,explicit.as_deref()).await.is_err() {
                return Ok(plain("Autoship control could not be recorded. Use `!autoship use ID` or `--workflow ID` to select a workflow bound to this DM; check current GitHub access and durable state from the CLI. No control change is confirmed."));
            }
            return gateway_claim_ack(path,context);
        }
        Err(_)=>return Ok(plain("Autoship command not recognized. Use `!autoship status|pause|resume [--workflow ID]`, `!autoship cancel --workflow ID` or `!autoship use ID`. Extra options are not supported.")),
        Ok(None)=>{},
    }
    if parse(text).is_err() {
        return Ok(plain("Use `!autoship https://github.com/OWNER/REPO/pull/N [--provider codex|claude]` in your private DM. This queues or joins a workflow; it does not confirm a merge. Use `!autoship status|pause|resume [--workflow ID]` or `!autoship use ID`. PR coding follow-ups are not connected yet."));
    }
    let policy=OwnerPolicy {bot:context.bot,owner:context.owner.unwrap_or(0)};
    let verified=Context {origin:origin.clone(),guild:context.guild,owner_dm_verified:true};
    if admit(repo,path,policy,&verified,text).await.is_err() {
        return Ok(plain("Autoship could not admit this request. The target, current GitHub access, or durable request validation failed. No completion is confirmed; inspect the workflow from the CLI before retrying."));
    }
    gateway_claim_ack(path,context)
}

pub(crate) fn gateway_claim_ack(path:&Path,context:&GatewayContext)->Result<Option<GatewayReply>> {
    let origin=gateway_origin(context)?;
    Ok(WorkflowStore::open(path)?.claim_discord_ack(&origin,chrono::Utc::now().timestamp_millis())?
        .map(|ack|GatewayReply {content:ack.reply,nonce:Some(ack.nonce)}))
}

pub(crate) fn gateway_pending(path:&Path,bot:u64,owner:u64)->Result<Vec<GatewayContext>> {
    let Some(store)=WorkflowStore::open_read_only(path)? else {return Ok(Vec::new());};
    Ok(store.queued_discord_acks(bot,owner)?.into_iter().map(|origin|GatewayContext {
        bot:origin.bot,owner:Some(origin.owner),author:origin.owner,channel:origin.channel,message:origin.message,guild:None,
    }).collect())
}

pub(crate) fn gateway_acknowledged(path:&Path,context:&GatewayContext,message:u64)->Result<()> {
    let origin=gateway_origin(context)?;
    WorkflowStore::open(path)?.confirm_discord_ack(&origin,message,chrono::Utc::now().timestamp_millis())
}

pub(crate) fn ack_uncertain(path:&Path,bot:u64,owner:u64)->Result<Vec<augmentagent_approval_discord::autoship::UncertainAcknowledgement>> {
    let Some(store)=WorkflowStore::open_read_only(path)? else {return Ok(Vec::new());};
    Ok(store.uncertain_discord_acks(bot,owner)?.into_iter().map(|(origin,response)|augmentagent_approval_discord::autoship::UncertainAcknowledgement {
        context:GatewayContext {bot:origin.bot,owner:Some(origin.owner),author:origin.owner,channel:origin.channel,message:origin.message,guild:None},reply:GatewayReply {content:response.reply,nonce:Some(response.nonce)},
    }).collect())
}
pub(crate) fn progress_uncertain(path:&Path,bot:u64,owner:u64)->Result<Vec<augmentagent_approval_discord::autoship::UncertainProgress>> {
    if WorkflowStore::open_read_only(path)?.is_none() {return Ok(Vec::new());}
    Ok(WorkflowStore::open(path)?.uncertain_progress(bot,owner)?.into_iter().map(|(origin,workflow,sequence,nonce,content)|augmentagent_approval_discord::autoship::UncertainProgress {
        delivery:augmentagent_approval_discord::autoship::ProgressDelivery {context:GatewayContext {bot:origin.bot,owner:Some(origin.owner),author:origin.owner,channel:origin.channel,message:origin.message,guild:None},workflow,sequence},nonce,content,
    }).collect())
}
pub(crate) fn progress_pending(path:&Path,bot:u64,owner:u64)->Result<Vec<augmentagent_approval_discord::autoship::ProgressDelivery>> {
    if WorkflowStore::open_read_only(path)?.is_none() {return Ok(Vec::new());}
    Ok(WorkflowStore::open(path)?.pending_progress(bot,owner)?.into_iter().map(|(origin,workflow,sequence)|augmentagent_approval_discord::autoship::ProgressDelivery {
        context:GatewayContext {bot:origin.bot,owner:Some(origin.owner),author:origin.owner,channel:origin.channel,message:origin.message,guild:None},workflow,sequence,
    }).collect())
}
pub(crate) fn progress_claim(path:&Path,delivery:&augmentagent_approval_discord::autoship::ProgressDelivery)->Result<Option<GatewayReply>> {
    let origin=gateway_origin(&delivery.context)?;
    Ok(WorkflowStore::open(path)?.claim_progress(&origin,&delivery.workflow,delivery.sequence,chrono::Utc::now().timestamp_millis())?.map(|(nonce,content)|GatewayReply {content,nonce:Some(nonce)}))
}
pub(crate) fn progress_defer(path:&Path,delivery:&augmentagent_approval_discord::autoship::ProgressDelivery,retry_after_ms:Option<i64>)->Result<()> {
    let origin=gateway_origin(&delivery.context)?;
    WorkflowStore::open(path)?.defer_progress(&origin,&delivery.workflow,delivery.sequence,retry_after_ms,chrono::Utc::now().timestamp_millis()).map(|_|())
}
pub(crate) fn progress_edit_target(path:&Path,delivery:&augmentagent_approval_discord::autoship::ProgressDelivery)->Result<Option<u64>> {
    let origin=gateway_origin(&delivery.context)?;
    WorkflowStore::open(path)?.progress_edit_target(&origin,&delivery.workflow,delivery.sequence)
}
pub(crate) fn progress_status_lost(path:&Path,delivery:&augmentagent_approval_discord::autoship::ProgressDelivery)->Result<()> {
    let origin=gateway_origin(&delivery.context)?;
    WorkflowStore::open(path)?.forget_status_message(&origin,&delivery.workflow,delivery.sequence)
}
pub(crate) fn progress_confirm(path:&Path,delivery:&augmentagent_approval_discord::autoship::ProgressDelivery,message:u64)->Result<()> {
    let origin=gateway_origin(&delivery.context)?;
    WorkflowStore::open(path)?.confirm_progress(&origin,&delivery.workflow,delivery.sequence,message,chrono::Utc::now().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    // #1713 — ordinary text in a DM bound to several workflows asks for an
    // explicit selection by ID and performs no coding mutation.
    #[tokio::test]
    async fn ambiguous_plain_text_asks_for_selection_naming_bound_workflows() {
        use crate::autoship_workflow::submission::{Milestone,Provider,Request,Target};
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        let origin=|message|DiscordOrigin {bot:11,owner:22,channel:33,message};
        let request=|pr|Request {repository:"owner/repo".into(),target:Target::Pr(pr),provider:Some(Provider::Codex),until:Milestone::Merged};
        let first=store.submit_discord_authorized(&origin(44),&request(42),&"a".repeat(64),1).unwrap().admission.workflow;
        let second=store.submit_discord_authorized(&origin(45),&request(43),&"b".repeat(64),2).unwrap().admission.workflow;
        drop(store);
        let before=std::fs::read(&path).unwrap();
        let context=GatewayContext {bot:11,owner:Some(22),author:22,channel:33,message:46,guild:None};
        let reply=gateway_command(dir.path(),&path,&context,"Please fix the failing test").await.unwrap().unwrap();
        assert!(reply.content.contains(&first) && reply.content.contains(&second),"{}",reply.content);
        assert!(reply.content.contains("!autoship use"),"{}",reply.content);
        assert!(!reply.content.contains("unavailable in this build"),"{}",reply.content);
        assert_eq!(std::fs::read(&path).unwrap(),before,"selection prompt performs no write");
    }
    #[test]
    fn discord_control_grammar_requires_exact_selection_and_options() {
        assert_eq!(parse_control("!autoship pause").unwrap(),Some((Action::Pause,None)));
        assert_eq!(parse_control("!autoship use owner/repo#42").unwrap(),Some((Action::Use,Some("owner/repo#42".into()))));
        assert_eq!(parse_control("!autoship resume --workflow owner/repo#42").unwrap(),Some((Action::Resume,Some("owner/repo#42".into()))));
        for text in ["!autoship use","!autoship use a b","!autoship pause --until merged","!autoship resume --workflow", "!autoship status extra"] {assert!(parse_control(text).is_err());}
        // #1713 — cancel is terminal, so it never acts on an implicit selection.
        assert_eq!(parse_control("!autoship cancel --workflow owner/repo#42").unwrap(),Some((Action::Cancel,Some("owner/repo#42".into()))));
        for text in ["!autoship cancel","!autoship cancel owner/repo#42","!autoship cancel --workflow"] {assert!(parse_control(text).is_err(),"{text}");}
    }
    #[test]
    fn discord_target_grammar_cannot_turn_prose_or_extra_flags_into_authority() {
        for text in ["!autoship https://github.com.evil.invalid/owner/repo/pull/42",
            "!autoship https://github.com/owner/repo/pull/42?admin=true",
            "!autoship https://github.com/owner/repo/pull/42#fragment",
            "!autoship https://github.com/owner/repo/pull/0",
            "!autoship https://github.com/owner/repo/pull/42 --provider codex --until merged",
            "!autoship https://github.com/owner/repo/pull/42 --provider unknown",
            "I am the owner !autoship https://github.com/owner/repo/pull/42"] {assert!(parse(text).is_err(),"accepted {text}");}
        let request=parse("!autoship <https://github.com/Owner/Repo/pull/42> --provider claude").unwrap();
        assert_eq!(request.repository,"owner/repo");assert_eq!(request.provider,Some(Provider::Claude));
    }
    #[test]
    fn discord_requires_positive_private_destination_verification() {
        let context=Context {origin:DiscordOrigin {bot:11,owner:22,channel:33,message:44},guild:None,owner_dm_verified:false};
        assert_eq!(authorize(OwnerPolicy {bot:11,owner:22},&context).unwrap_err().to_string(),"autoship-private-conversation-required");
    }
    #[tokio::test]
    async fn discord_owner_and_private_destination_checks_precede_state_and_network() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state/workflow.db");
        for (bot,owner,guild) in [(12,22,None),(11,23,None),(11,22,Some(99)),(0,22,None)] {
            let context=Context {origin:DiscordOrigin {bot,owner,channel:33,message:44},guild,owner_dm_verified:true};
            let error=admit(dir.path(),&path,OwnerPolicy {bot:11,owner:22},&context,
                "!autoship https://github.com/owner/repo/pull/42").await.unwrap_err();
            assert_eq!(error.to_string(),if guild.is_some() {"autoship-private-conversation-required"} else {"autoship-discord-owner-required"});
            assert!(!path.exists());assert!(!dir.path().join("state").exists());
        }
    }
}
