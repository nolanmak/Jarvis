//! CLI delivery administration authenticates GitHub and the actual Discord DM.
use anyhow::{ensure,Context,Result};
use serde_json::{json,Value};
use crate::autoship_workflow::{WorkflowStore,delivery::Destination};

#[derive(clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)] pub operation: Operation,
}
#[derive(clap::Subcommand)]
pub(crate) enum Operation {
    /// Set the private operational destination for CLI and poller work.
    Configure {
        #[arg(long)] repo:String,
        #[arg(long,required_unless_present="owner_dm",conflicts_with="owner_dm")] channel:Option<u64>,
        #[arg(long)] owner_dm:bool,
        #[arg(long)] json:bool,
    },
    /// Route updates to the configured owner's verified private Discord DM.
    Rebind {
        #[arg(long)] workflow:String,
        #[arg(long,required_unless_present="owner_dm",conflicts_with="owner_dm")] channel:Option<u64>,
        /// Find or create the configured owner's private DM with this bot.
        #[arg(long)] owner_dm:bool,
        #[arg(long)] json:bool,
    },
}

fn snowflake(value:&Value)->Result<u64> {
    let id=value.as_str().context("Discord identity is not a snowflake")?.parse::<u64>()?;
    ensure!(id>0,"Discord identity missing");Ok(id)
}
fn verified_destination(bot:&Value,channel:&Value,owner:u64)->Result<Destination> {
    let bot_id=snowflake(&bot["id"])?;
    ensure!(bot["bot"]==true && owner>0 && bot_id!=owner,"Discord bot/owner identity mismatch");
    ensure!(channel["type"]==1 && channel.get("guild_id").is_none_or(Value::is_null),"delivery-private-owner-dm-required");
    let recipients=channel["recipients"].as_array().context("Discord DM recipients missing")?;
    ensure!(recipients.len()==1 && snowflake(&recipients[0]["id"])?==owner
        && recipients[0]["bot"]!=true,"delivery-private-owner-dm-required");
    Ok(Destination {bot:bot_id,owner,channel:snowflake(&channel["id"])?})
}

async fn discord_destination(channel:Option<u64>)->Result<Destination> {
    let token=std::env::var("DISCORD_BOT_TOKEN").context("DISCORD_BOT_TOKEN required")?;
    ensure!(!token.trim().is_empty(),"DISCORD_BOT_TOKEN required");
    let owner=std::env::var("DISCORD_ALLOWED_USER_ID").context("DISCORD_ALLOWED_USER_ID required")?.parse::<u64>()?;
    ensure!(owner>0,"configured Discord owner missing");
    let client=reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20)).build()?;
    let authorization=format!("Bot {token}");
    let bot=client.get("https://discord.com/api/v10/users/@me").header(reqwest::header::AUTHORIZATION,&authorization)
        .send().await?.error_for_status()?.json::<Value>().await?;
    let destination=match channel {
        Some(id)=>{
            ensure!(id>0,"Discord channel missing");
            client.get(format!("https://discord.com/api/v10/channels/{id}"))
        }
        None=>client.post("https://discord.com/api/v10/users/@me/channels").json(&json!({"recipient_id":owner.to_string()})),
    }.header(reqwest::header::AUTHORIZATION,&authorization).send().await?.error_for_status()?.json::<Value>().await?;
    let verified=verified_destination(&bot,&destination,owner)?;
    ensure!(channel.is_none_or(|id|id==verified.channel),"Discord returned a different channel");
    Ok(verified)
}

async fn apply_rebind(workflow:&str,channel:Option<u64>)->Result<Value> {
    let repo=std::env::current_dir()?;let path=crate::autoship_workflow::default_path();
    let (repository,target)=super::target(workflow)?;
    let store=WorkflowStore::open_read_only(&path)?.context("no durable workflow state")?;
    let workflow=if target["kind"]=="pr" {
        store.resolve_pr(&repository,target["number"].as_u64().unwrap())?.unwrap_or(workflow.to_ascii_lowercase())
    }else {workflow.to_ascii_lowercase()};
    ensure!(store.command_request(&workflow)?.is_some() || store.snapshot(&workflow)?.is_some(),"unknown workflow");
    drop(store);
    let principal=crate::self_improve::targeted::authorize_repository(&repo,&repository).await?;
    let destination=discord_destination(channel).await?;
    ensure!(crate::self_improve::targeted::authorize_repository(&repo,&repository).await?==principal,"delivery operator changed");
    let mut store=WorkflowStore::open(&path)?;
    let changed=store.bind_delivery(&workflow,&principal,destination,chrono::Utc::now().timestamp_millis())?;
    Ok(json!({"schema_version":1,"accepted":true,"complete":false,"workflow":workflow,
        "reason":if changed {"delivery-bound"} else {"delivery-already-bound"},
        "destination":destination,"delivery":store.progress_delivery(&workflow)?,"execution":"not-started-by-delivery-command"}))
}

async fn apply_configure(repository:&str,channel:Option<u64>)->Result<Value> {
    let repository=crate::autoship_workflow::submission::normalized_repository(repository)?;
    let repo=std::env::current_dir()?;
    let principal=crate::self_improve::targeted::authorize_repository(&repo,&repository).await?;
    let destination=discord_destination(channel).await?;
    ensure!(crate::self_improve::targeted::authorize_repository(&repo,&repository).await?==principal,"delivery operator changed");
    let changed=WorkflowStore::open(&crate::autoship_workflow::default_path())?.configure_delivery(&repository,&principal,destination,chrono::Utc::now().timestamp_millis())?;
    Ok(json!({"schema_version":1,"accepted":true,"complete":false,"repository":repository,
        "reason":if changed {"delivery-default-configured"} else {"delivery-default-unchanged"},
        "destination":destination,"execution":"not-started-by-delivery-command"}))
}

pub(crate) async fn run(args:&Args)->i32 {
    let (result,json_output)=match &args.operation {
        Operation::Rebind {workflow,channel,json,..}=>(apply_rebind(workflow,*channel).await,*json),
        Operation::Configure {repo,channel,json,..}=>(apply_configure(repo,*channel).await,*json),
    };
    let (code,value)=match result {
        Ok(value)=>(0,value),Err(error)=>(3,json!({"schema_version":1,"accepted":false,"complete":false,
            "reason":"delivery-change-refused","error":format!("{error:#}")})),
    };
    if json_output {println!("{value}");}else {println!("{}",serde_json::to_string_pretty(&value).unwrap());}
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delivery_commands_require_one_explicit_private_destination() {
        use clap::Parser;
        for args in [vec!["augmentagent","autopr","delivery","configure","--repo","owner/repo","--owner-dm","--json"],
            vec!["augmentagent","autopr","delivery","rebind","--workflow","owner/repo#7","--channel","33","--json"]] {
            assert!(crate::Cli::try_parse_from(args).is_ok());
        }
        for args in [vec!["augmentagent","autopr","delivery","configure","--repo","owner/repo"],
            vec!["augmentagent","autopr","delivery","configure","--repo","owner/repo","--channel","33","--owner-dm"],
            vec!["augmentagent","autopr","delivery","rebind","--owner-dm"]] {
            assert!(crate::Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn only_the_actual_bots_private_dm_with_the_configured_owner_can_bind() {
        let bot=json!({"id":"11","bot":true});let dm=json!({"id":"33","type":1,"recipients":[{"id":"22"}]});
        assert_eq!(verified_destination(&bot,&dm,22).unwrap(),Destination {bot:11,owner:22,channel:33});
        for invalid in [json!({"id":"33","type":0,"guild_id":"99","recipients":[{"id":"22"}]}),
            json!({"id":"33","type":3,"recipients":[{"id":"22"},{"id":"44"}]}),
            json!({"id":"33","type":1,"recipients":[{"id":"22","bot":true}]}),
            json!({"id":"33","type":1,"recipients":[{"id":"22"},{"id":"44"}]}),
            json!({"id":"33","type":1,"recipients":[{"id":"44"}]}),
            json!({"id":"0","type":1,"recipients":[{"id":"22"}]})] {
            assert!(verified_destination(&bot,&invalid,22).is_err(),"invalid private destination accepted: {invalid}");
        }
        assert!(verified_destination(&json!({"id":"11","bot":false}),&dm,22).is_err());
        assert!(verified_destination(&bot,&dm,11).is_err());
    }
}
