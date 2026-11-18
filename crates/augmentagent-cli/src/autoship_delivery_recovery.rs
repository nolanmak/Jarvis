//! #1714: `autopr delivery resolve|retry`. Owner-authenticated recovery for
//! progress delivery; never resends an uncertain event without an explicit,
//! single-use `--allow-duplicate --request-id`.
use anyhow::{ensure,Context,Result};
use serde_json::{json,Value};
use crate::autoship_workflow::{WorkflowStore,progress_resolution::{parse_event,RemoteMessage}};

/// Fetch one message by id. Anything short of a readable 200 is "no conclusive
/// evidence" (`None`), never a delivery.
pub(crate) async fn fetch_message(base:&str,token:&str,channel:u64,message:u64)->Result<Option<RemoteMessage>> {
    ensure!(channel>0 && message>0,"Discord identity missing");
    let client=reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(std::time::Duration::from_secs(15)).build()?;
    let authorization=if token.starts_with("Bot ") {token.to_string()} else {format!("Bot {token}")};
    let Ok(response)=client.get(format!("{}/api/v10/channels/{channel}/messages/{message}",base.trim_end_matches('/')))
        .header(reqwest::header::AUTHORIZATION,authorization).send().await else {return Ok(None)};
    if !response.status().is_success() {return Ok(None);}
    let Ok(value)=response.json::<Value>().await else {return Ok(None)};
    let id=|v:&Value|v.as_str().and_then(|s|s.parse::<u64>().ok()).filter(|n|*n>0);
    Ok((|| Some(RemoteMessage{id:id(&value["id"])?,channel:id(&value["channel_id"])?,author:id(&value["author"]["id"])?,
        author_bot:value["author"]["bot"].as_bool().unwrap_or(false),content:value["content"].as_str()?.to_string()}))())
}

async fn principal(workflow:&str)->Result<String> {
    let (repository,_)=super::target(workflow)?;
    crate::self_improve::targeted::authorize_repository(&std::env::current_dir()?,&repository).await
}

pub(crate) async fn resolve(event:&str,message:u64)->Result<Value> {
    let (workflow,sequence)=parse_event(event)?;
    principal(&workflow).await?;
    let path=crate::autoship_workflow::default_path();
    let (bot,channel)=WorkflowStore::open(&path)?.progress_event_destination(&workflow,sequence)?;
    let token=std::env::var("DISCORD_BOT_TOKEN").context("DISCORD_BOT_TOKEN required")?;
    let evidence=fetch_message("https://discord.com",&token,channel,message).await?;
    ensure!(evidence.as_ref().is_none_or(|e|e.author==bot),"evidence was not sent by the bound bot");
    let mut store=WorkflowStore::open(&path)?;
    store.resolve_progress(&workflow,sequence,evidence.as_ref(),chrono::Utc::now().timestamp_millis())?;
    Ok(json!({"schema_version":1,"accepted":true,"event":format!("{workflow}:{sequence}"),"reason":"delivery-resolved",
        "delivery":store.progress_delivery_at(&workflow,chrono::Utc::now().timestamp_millis())?}))
}

pub(crate) async fn retry(workflow:Option<&str>,event:Option<&str>,allow_duplicate:bool,request_id:Option<&str>)->Result<Value> {
    let path=crate::autoship_workflow::default_path();let now=chrono::Utc::now().timestamp_millis();
    match (workflow,event) {
        (Some(workflow),None)=>{
            ensure!(!allow_duplicate,"--allow-duplicate requires --event");
            let workflow=workflow.to_ascii_lowercase();principal(&workflow).await?;
            let summary=WorkflowStore::open(&path)?.retry_progress_now(&workflow,now)?;
            Ok(json!({"schema_version":1,"accepted":true,"workflow":workflow,"reason":"delivery-retry-scheduled",
                "rescheduled":summary.rescheduled,"uncertain_left_for_reconciliation":summary.uncertain}))
        }
        (None,Some(event))=>{
            ensure!(allow_duplicate,"retrying one uncertain event requires --allow-duplicate");
            let request_id=request_id.context("--request-id required")?;
            let (workflow,sequence)=parse_event(event)?;let owner=principal(&workflow).await?;
            let mut store=WorkflowStore::open(&path)?;
            let authorized=store.authorize_duplicate_resend(&workflow,sequence,request_id,&owner,now)?;
            Ok(json!({"schema_version":1,"accepted":true,"event":format!("{workflow}:{sequence}"),
                "reason":if authorized {"duplicate-resend-authorized"} else {"duplicate-resend-already-authorized"},
                "possible_duplicate":true,"resends":store.progress_resends(&workflow)?}))
        }
        _=>anyhow::bail!("exactly one of --workflow or --event is required"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_commands_require_explicit_duplicate_authorization() {
        use clap::Parser;
        for args in [vec!["augmentagent","autopr","delivery","resolve","--event","owner/repo#7:3","--message","55"],
            vec!["augmentagent","autopr","delivery","retry","--workflow","owner/repo#7"],
            vec!["augmentagent","autopr","delivery","retry","--event","owner/repo#7:3","--allow-duplicate","--request-id","req-1","--json"]] {
            assert!(crate::Cli::try_parse_from(args.clone()).is_ok(),"{args:?}");
        }
        for args in [vec!["augmentagent","autopr","delivery","resolve","--event","owner/repo#7:3"],
            vec!["augmentagent","autopr","delivery","retry","--event","owner/repo#7:3","--allow-duplicate"],
            vec!["augmentagent","autopr","delivery","retry","--workflow","owner/repo#7","--event","owner/repo#7:3"],
            vec!["augmentagent","autopr","delivery","retry"]] {
            assert!(crate::Cli::try_parse_from(args.clone()).is_err(),"{args:?}");
        }
    }
    #[tokio::test]
    async fn evidence_fetch_never_fabricates_a_message() {
        let mut server=mockito::Server::new_async().await;
        let body=json!({"id":"77","channel_id":"33","author":{"id":"11","bot":true},"content":"x [autoship:m]"}).to_string();
        for (status,body,expected) in [
            (200,body.clone(),Some(RemoteMessage{id:77,channel:33,author:11,author_bot:true,content:"x [autoship:m]".into()})),
            (404,r#"{"code":10008,"message":"Unknown Message"}"#.to_string(),None),
            (403,r#"{"code":50001,"message":"Missing Access"}"#.to_string(),None),
            (200,"not json".to_string(),None),
        ] {
            let mock=server.mock("GET","/api/v10/channels/33/messages/77").match_header("authorization","Bot t").with_status(status).with_body(body).create_async().await;
            assert_eq!(fetch_message(&server.url(),"t",33,77).await.unwrap(),expected);
            mock.assert_async().await;mock.remove_async().await;
        }
    }
}
