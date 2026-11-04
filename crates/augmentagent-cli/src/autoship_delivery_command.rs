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
    ensure!(recipients.len()==1 && snowflake(&recipients[0]["id"])?==owner,"delivery-private-owner-dm-required");
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

async fn apply(workflow:&str,channel:Option<u64>)->Result<Value> {
    let repo=std::env::current_dir()?;let path=crate::autoship_workflow::default_path();
    let store=WorkflowStore::open_read_only(&path)?.context("no durable workflow state")?;
    let request=store.command_request(workflow)?.context("delivery requires an admitted workflow")?;
    drop(store);
    let principal=crate::self_improve::targeted::authorize_command(&repo,&request).await?;
    let destination=discord_destination(channel).await?;
    // Recheck repository authorization after remote validation and before the
    // transaction. A binding never grants execution or merge authority.
    ensure!(crate::self_improve::targeted::authorize_command(&repo,&request).await?==principal,"delivery operator changed");
    let mut store=WorkflowStore::open(&path)?;
    ensure!(store.command_request(workflow)?.is_some_and(|current|current.repository==request.repository && current.target==request.target),"workflow target changed");
    let changed=store.bind_delivery(workflow,&principal,destination,chrono::Utc::now().timestamp_millis())?;
    Ok(json!({"schema_version":1,"accepted":true,"complete":false,"workflow":workflow,
        "reason":if changed {"delivery-bound"} else {"delivery-already-bound"},
        "destination":destination,"delivery":store.progress_delivery(workflow)?,"execution":"not-started-by-delivery-command"}))
}

pub(crate) async fn run(args:&Args)->i32 {
    let Operation::Rebind {workflow,channel,json:json_output,..}=&args.operation;
    let (code,value)=match apply(workflow,*channel).await {
        Ok(value)=>(0,value),Err(error)=>(3,json!({"schema_version":1,"accepted":false,"complete":false,
            "workflow":workflow,"reason":"delivery-rebind-refused","error":format!("{error:#}")})),
    };
    if *json_output {println!("{value}");}else {println!("{}",serde_json::to_string_pretty(&value).unwrap());}
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_actual_bots_private_dm_with_the_configured_owner_can_bind() {
        let bot=json!({"id":"11","bot":true});let dm=json!({"id":"33","type":1,"recipients":[{"id":"22"}]});
        assert_eq!(verified_destination(&bot,&dm,22).unwrap(),Destination {bot:11,owner:22,channel:33});
        for invalid in [json!({"id":"33","type":0,"guild_id":"99","recipients":[{"id":"22"}]}),
            json!({"id":"33","type":3,"recipients":[{"id":"22"},{"id":"44"}]}),
            json!({"id":"33","type":1,"recipients":[{"id":"44"}]}),
            json!({"id":"0","type":1,"recipients":[{"id":"22"}]})] {
            assert!(verified_destination(&bot,&invalid,22).is_err());
        }
        assert!(verified_destination(&json!({"id":"11","bot":false}),&dm,22).is_err());
        assert!(verified_destination(&bot,&dm,11).is_err());
    }
}
