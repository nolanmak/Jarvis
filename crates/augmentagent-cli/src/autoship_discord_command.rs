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

fn parse_control(text:&str)->Result<Option<(Action,Option<String>)>> {
    ensure!(text.len()<=1024,"autoship-command-too-large");
    let words:Vec<_>=text.split_whitespace().collect();
    if words.first()!=Some(&"!autoship") {return Ok(None);}
    let action=match words.get(1).copied() {
        Some("status")=>Action::Status,Some("pause")=>Action::Pause,
        Some("resume")=>Action::Resume,Some("use")=>Action::Use,_=>return Ok(None),
    };
    let workflow=if action==Action::Use {
        ensure!(words.len()==3,"expected !autoship use ID");Some(words[2].to_string())
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
    let status=if action==Action::Status && refusal.is_none() {
        let (code,value)=crate::autoship_commands::status(path,&workflow,chrono::Utc::now().timestamp_millis(),0);
        if code==2 || code==4 {refusal=Some("workflow-state-unavailable".into());None}
        else {
            // Only bounded shared status fields, never raw provider or auth errors.
            let field=|key:&str|value[key].as_str().unwrap_or("unknown").chars().take(160).collect::<String>();
            Some(format!("Autoship {workflow}: stage {}; reason {}; provider {}; PR {}. Merge: {}. Deployment: {}. Completion observed: {}.",
                field("stage"),field("reason"),field("provider"),field("pr_url"),
                value["merge"]["state"].as_str().unwrap_or("unknown"),value["deployment"]["state"].as_str().unwrap_or("unknown"),value["complete"]==true))
        }
    } else {None};
    WorkflowStore::open(path)?.commit_discord_control(origin,&digest,&PreparedControl {
        workflow,action,explicit:explicit.is_some(),revision,refusal,status,
    },chrono::Utc::now().timestamp_millis())?;
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
        return Ok(plain("This conversation is bound to Autoship. PR coding follow-ups are not connected yet; this message has not edited the PR. Track the workflow with `autopr status --workflow ID`."));
    }
    match parse_control(text) {
        Ok(Some((action,explicit)))=> {
            if control(repo,path,&origin,text,action,explicit.as_deref()).await.is_err() {
                return Ok(plain("Autoship control could not be recorded. Use `!autoship use ID` or `--workflow ID` to select a workflow bound to this DM; check current GitHub access and durable state from the CLI. No control change is confirmed."));
            }
            return gateway_claim_ack(path,context);
        }
        Err(_)=>return Ok(plain("Use `!autoship status|pause|resume [--workflow ID]` or `!autoship use ID`. Extra options are not supported.")),
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
pub(crate) fn progress_confirm(path:&Path,delivery:&augmentagent_approval_discord::autoship::ProgressDelivery,message:u64)->Result<()> {
    let origin=gateway_origin(&delivery.context)?;
    WorkflowStore::open(path)?.confirm_progress(&origin,&delivery.workflow,delivery.sequence,message,chrono::Utc::now().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discord_control_grammar_requires_exact_selection_and_options() {
        assert_eq!(parse_control("!autoship pause").unwrap(),Some((Action::Pause,None)));
        assert_eq!(parse_control("!autoship use owner/repo#42").unwrap(),Some((Action::Use,Some("owner/repo#42".into()))));
        assert_eq!(parse_control("!autoship resume --workflow owner/repo#42").unwrap(),Some((Action::Resume,Some("owner/repo#42".into()))));
        for text in ["!autoship use","!autoship use a b","!autoship pause --until merged","!autoship resume --workflow", "!autoship status extra"] {assert!(parse_control(text).is_err());}
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
