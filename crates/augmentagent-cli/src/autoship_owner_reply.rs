//! Private per-request agent replies share the gateway's durable delivery loop.
//! A timeout is unknown delivery, never permission to resend the model body.
use super::discord_ingress::DiscordOrigin;
use super::*;

#[derive(Clone, Debug)]
pub(crate) struct Delivery {
    pub origin: DiscordOrigin,
    pub workflow: String,
    pub request_id: String,
    pub part: u32,
}
pub(crate) struct Uncertain {
    pub delivery: Delivery,
    pub nonce: String,
    pub content: String,
}
fn chunks(response: &str) -> Result<Vec<String>> {
    ensure!(
        response.len() <= 1024 * 1024,
        "owner reply exceeds response bound"
    );
    let mut result = Vec::new();
    let mut piece = String::new();
    let mut units = 0;
    for ch in response.chars() {
        if units + ch.len_utf16() > 1400 {
            result.push(std::mem::take(&mut piece));
            units = 0;
        }
        piece.push(ch);
        units += ch.len_utf16();
    }
    if !piece.is_empty() || result.is_empty() {
        result.push(piece);
    }
    ensure!(result.len() <= 1024, "too many owner reply parts");
    Ok(result)
}
fn render(
    origin: &DiscordOrigin,
    workflow: &str,
    request: &str,
    response: &str,
) -> Result<Vec<(String, String)>> {
    let pieces = chunks(response)?;
    let total = pieces.len();
    let response_sha = format!("{:x}", Sha256::digest(response.as_bytes()));
    pieces.into_iter().enumerate().map(|(part,piece)| {
        let nonce=format!("{:x}",Sha256::digest(serde_json::to_vec(&("owner-reply-v1",workflow,request,origin.bot,origin.owner,origin.channel,part,response_sha.as_str()))?))[..24].to_string();
        let content=format!("Autoship agent reply · {workflow}\nRequest: {request} · part {}/{}\nModel response; tests and shipping require recorded evidence.\n\n{piece}\n[autoship:{nonce}]",part+1,total);
        ensure!(content.encode_utf16().count()<=1900,"owner reply message exceeds Discord bound");Ok((nonce,content))
    }).collect()
}
pub(super) fn enqueue(
    tx: &Transaction<'_>,
    workflow: &str,
    request: &str,
    response: &str,
) -> Result<()> {
    let message = super::owner_inbox::message_evidence(tx, workflow, request)?;
    ensure!(
        message.state == "applied",
        "owner reply requires applied input"
    );
    super::discord_ingress::bound_workflow(tx, &message.origin, Some(workflow))?;
    let parts = render(&message.origin, workflow, request, response)?;
    let total = parts.len();
    for (part, (nonce, content)) in parts.into_iter().enumerate() {
        tx.execute("INSERT INTO workflow_owner_reply_delivery VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'queued',NULL,NULL,NULL)",params![request,workflow,part as i64,total as i64,message.origin.bot.to_string(),message.origin.owner.to_string(),message.origin.channel.to_string(),nonce,content])?;
    }
    Ok(())
}
fn exists(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_reply_delivery')",[],|r|r.get(0))?)
}
fn origin(conn: &Connection, delivery: &Delivery) -> Result<()> {
    delivery.origin.validate()?;
    let actual =
        super::owner_inbox::message_evidence(conn, &delivery.workflow, &delivery.request_id)?;
    ensure!(
        actual.state == "applied" && actual.origin == delivery.origin,
        "owner reply destination differs from authenticated request"
    );
    super::discord_ingress::bound_workflow(conn, &actual.origin, Some(&delivery.workflow))?;
    Ok(())
}
fn validate(
    conn: &Connection,
    workflow: &str,
    request: &str,
) -> Result<Vec<(String, String, String)>> {
    let (origin, response, _) = super::owner_dispatch::completed_reply(conn, workflow, request)?;
    validate_persisted_parts(conn, workflow, request, &origin, &response)
}
pub(super) fn validate_persisted_parts(
    conn: &Connection,
    workflow: &str,
    request: &str,
    origin: &DiscordOrigin,
    response: &str,
) -> Result<Vec<(String, String, String)>> {
    let parts = render(origin, workflow, request, response)?;
    let mut query=conn.prepare("SELECT workflow,part,total,bot,owner,channel,nonce,content,state,dispatched_at,message,confirmed_at FROM workflow_owner_reply_delivery WHERE request_id=?1 ORDER BY part LIMIT 1025")?;
    let mut rows = query.query([request])?;
    let mut result = Vec::new();
    let mut unfinished = false;
    let mut unknown = 0;
    while let Some(row) = rows.next()? {
        let part: i64 = row.get(1)?;
        let total: i64 = row.get(2)?;
        let index = result.len();
        ensure!(
            index < parts.len() && part == index as i64 && total == parts.len() as i64,
            "owner reply part history changed"
        );
        let identity: (String, String, String, String, String, String) = (
            row.get(0)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
        );
        ensure!(
            identity
                == (
                    workflow.into(),
                    origin.bot.to_string(),
                    origin.owner.to_string(),
                    origin.channel.to_string(),
                    parts[index].0.clone(),
                    parts[index].1.clone()
                ),
            "owner reply persisted content or destination changed"
        );
        let state: String = row.get(8)?;
        let dispatched: Option<i64> = row.get(9)?;
        let message: Option<String> = row.get(10)?;
        let confirmed: Option<i64> = row.get(11)?;
        match state.as_str() {
            "queued" => {
                ensure!(
                    dispatched.is_none() && message.is_none() && confirmed.is_none(),
                    "queued owner reply has a dispatch receipt"
                );
                unfinished = true;
            }
            "uncertain" => {
                ensure!(
                    !unfinished
                        && dispatched.is_some_and(|at| at >= 0)
                        && message.is_none()
                        && confirmed.is_none(),
                    "owner reply uncertainty is out of order"
                );
                unfinished = true;
                unknown += 1;
            }
            "confirmed" => {
                ensure!(
                    !unfinished
                        && dispatched.is_some_and(|at| at >= 0)
                        && message
                            .as_deref()
                            .and_then(|s| s.parse::<u64>().ok())
                            .is_some_and(|id| id > 0)
                        && confirmed.zip(dispatched).is_some_and(|(a, b)| a >= b),
                    "owner reply confirmation is out of order"
                );
            }
            _ => bail!("unknown owner reply delivery state"),
        }
        result.push((parts[index].0.clone(), parts[index].1.clone(), state));
    }
    ensure!(
        result.len() == parts.len() && unknown <= 1,
        "owner reply part history incomplete"
    );
    let state: String = conn.query_row(
        "SELECT state FROM workflow_owner_body_result WHERE request_id=?1 AND workflow=?2",
        params![request, workflow],
        |r| r.get(0),
    )?;
    ensure!(
        state
            == if unknown > 0 {
                "uncertain"
            } else if unfinished {
                "queued"
            } else {
                "confirmed"
            },
        "owner reply result disposition changed"
    );
    Ok(result)
}
fn make_delivery(
    conn: &Connection,
    workflow: String,
    request_id: String,
    part: i64,
    bot: u64,
    owner: u64,
) -> Result<Delivery> {
    let message = super::owner_inbox::message_evidence(conn, &workflow, &request_id)?;
    ensure!(
        message.origin.bot == bot && message.origin.owner == owner,
        "owner reply identity differs from gateway"
    );
    let result = Delivery {
        origin: message.origin,
        workflow,
        request_id,
        part: u32::try_from(part)?,
    };
    origin(conn, &result)?;
    Ok(result)
}
impl WorkflowStore {
    pub(crate) fn pending_owner_replies(&self, bot: u64, owner: u64) -> Result<Vec<Delivery>> {
        ensure!(
            bot > 0 && owner > 0,
            "owner reply recovery identity missing"
        );
        if !exists(&self.conn)? {
            return Ok(Vec::new());
        }
        let mut query=self.conn.prepare("SELECT workflow,request_id,part FROM (SELECT d.workflow,d.request_id,d.part,r.sequence,row_number() OVER (PARTITION BY d.workflow ORDER BY r.sequence,d.part) AS rank FROM workflow_owner_reply_delivery d JOIN workflow_owner_body_result r ON r.request_id=d.request_id JOIN discord_workflow_binding b ON b.workflow=d.workflow AND b.bot=d.bot AND b.owner=d.owner AND b.channel=d.channel WHERE d.bot=?1 AND d.owner=?2 AND d.state='queued' AND r.state='queued' AND NOT EXISTS(SELECT 1 FROM workflow_owner_body_result previous WHERE previous.workflow=d.workflow AND previous.sequence<r.sequence AND previous.state!='confirmed')) WHERE rank<=8 ORDER BY rank,sequence LIMIT 128")?;
        let rows = query.query_map(params![bot.to_string(), owner.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (workflow, request, part) = row?;
            match make_delivery(&self.conn, workflow, request, part, bot, owner) {
                Ok(delivery) => result.push(delivery),
                Err(_) => tracing::warn!(
                    "private owner reply route validation failed; queued data retained"
                ),
            }
        }
        Ok(result)
    }
    pub(crate) fn uncertain_owner_replies(&self, bot: u64, owner: u64) -> Result<Vec<Uncertain>> {
        ensure!(
            bot > 0 && owner > 0,
            "owner reply recovery identity missing"
        );
        if !exists(&self.conn)? {
            return Ok(Vec::new());
        }
        let mut query=self.conn.prepare("SELECT d.workflow,d.request_id,d.part FROM workflow_owner_reply_delivery d JOIN workflow_owner_body_result r ON r.request_id=d.request_id JOIN discord_workflow_binding b ON b.workflow=d.workflow AND b.bot=d.bot AND b.owner=d.owner AND b.channel=d.channel WHERE d.bot=?1 AND d.owner=?2 AND d.state='uncertain' ORDER BY r.sequence,d.part LIMIT 128")?;
        let rows = query.query_map(params![bot.to_string(), owner.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (workflow, request, part) = row?;
            let attempt = (|| -> Result<Uncertain> {
                let delivery = make_delivery(&self.conn, workflow, request, part, bot, owner)?;
                let parts = validate(&self.conn, &delivery.workflow, &delivery.request_id)?;
                let (nonce, content, state) = parts
                    .get(delivery.part as usize)
                    .context("uncertain owner reply part missing")?;
                ensure!(state == "uncertain", "owner reply is no longer uncertain");
                Ok(Uncertain {
                    delivery,
                    nonce: nonce.clone(),
                    content: content.clone(),
                })
            })();
            match attempt {Ok(reply)=>result.push(reply),Err(_)=>tracing::warn!("private owner reply validation failed; uncertainty retained while other deliveries continue")}
        }
        Ok(result)
    }
    pub(crate) fn claim_owner_reply(
        &mut self,
        delivery: &Delivery,
        now: i64,
    ) -> Result<Option<(String, String)>> {
        ensure!(now >= 0, "invalid owner reply dispatch time");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        origin(&tx, delivery)?;
        let parts = validate(&tx, &delivery.workflow, &delivery.request_id)?;
        let index = delivery.part as usize;
        let Some((nonce, content, state)) = parts.get(index) else {
            bail!("owner reply part missing");
        };
        if state != "queued" || parts[..index].iter().any(|p| p.2 != "confirmed") {
            return Ok(None);
        }
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_body_result earlier JOIN workflow_owner_body_result current ON current.request_id=?1 WHERE earlier.workflow=current.workflow AND earlier.sequence<current.sequence AND earlier.state!='confirmed')",[&delivery.request_id],|r|r.get(0))?;
        if blocked {
            return Ok(None);
        }
        tx.execute("UPDATE workflow_owner_reply_delivery SET state='uncertain',dispatched_at=?3 WHERE request_id=?1 AND part=?2 AND state='queued'",params![delivery.request_id,delivery.part,now])?;
        tx.execute(
            "UPDATE workflow_owner_body_result SET state='uncertain' WHERE request_id=?1",
            [&delivery.request_id],
        )?;
        tx.commit()?;
        Ok(Some((nonce.clone(), content.clone())))
    }
    pub(crate) fn confirm_owner_reply(
        &mut self,
        delivery: &Delivery,
        message: u64,
        now: i64,
    ) -> Result<()> {
        ensure!(
            message > 0 && now >= 0,
            "owner reply receipt identity missing"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        origin(&tx, delivery)?;
        let parts = validate(&tx, &delivery.workflow, &delivery.request_id)?;
        let (_, _, state) = parts
            .get(delivery.part as usize)
            .context("owner reply part missing")?;
        let (previous,dispatched):(Option<String>,Option<i64>)=tx.query_row("SELECT message,dispatched_at FROM workflow_owner_reply_delivery WHERE request_id=?1 AND part=?2",params![delivery.request_id,delivery.part],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if state == "confirmed" {
            ensure!(
                previous == Some(message.to_string()),
                "owner reply receipt conflict"
            );
            return Ok(());
        }
        ensure!(
            state == "uncertain" && dispatched.is_some_and(|at| now >= at),
            "owner reply was not dispatched or receipt time predates dispatch"
        );
        tx.execute("UPDATE workflow_owner_reply_delivery SET state='confirmed',message=?3,confirmed_at=?4 WHERE request_id=?1 AND part=?2",params![delivery.request_id,delivery.part,message.to_string(),now])?;
        tx.execute("UPDATE workflow_owner_body_result SET state=CASE WHEN EXISTS(SELECT 1 FROM workflow_owner_reply_delivery WHERE request_id=?1 AND state!='confirmed') THEN 'queued' ELSE 'confirmed' END WHERE request_id=?1",[&delivery.request_id])?;
        tx.commit()?;
        Ok(())
    }
}
/// Private replies cannot be moved to a configurable progress destination.
/// Finish/reconcile their original sends before replacing that inbound route.
pub(super) fn require_settled(conn: &Connection, workflow: &str) -> Result<()> {
    if !exists(conn)? {
        return Ok(());
    }
    let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_body_result WHERE workflow=?1 AND state!='confirmed')",[workflow],|r|r.get(0))?;
    ensure!(!pending, "owner-reply-delivery-pending");
    Ok(())
}
