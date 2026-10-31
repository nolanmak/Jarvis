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

#[cfg(test)]
mod tests {
    use super::*;
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
