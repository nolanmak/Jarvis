//! Authenticated metadata claim history. No model, body or native release permit.
use super::discord_ingress::DiscordOrigin;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordRef {
    sequence: i64,
    sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Original {
    workflow: String,
    request_id: String,
    intent: super::owner_publication::Reference,
    input_context: String,
    activated_generation: i64,
    preparation: OwnerPreparationBinding,
    origin: DiscordOrigin,
    command: submission::Request,
    body_dispatch: Option<RecordRef>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Predecessor {
    owner: Option<String>,
    generation: i64,
    lease_until: i64,
}
/// Read-only claim preview. It is neither authentication nor native authority.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RecoveryPlan {
    original: Original,
    predecessor: Predecessor,
    previous: Option<RecordRef>,
}
impl RecoveryPlan {
    pub(crate) fn origin(&self) -> &DiscordOrigin {
        &self.original.origin
    }
    pub(crate) fn command(&self) -> &submission::Request {
        &self.original.command
    }
    pub(crate) fn same_original(&self, other: &Self) -> bool {
        self.original == other.original
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    version: u32,
    original: Original,
    predecessor: Predecessor,
    previous: Option<RecordRef>,
    owner: String,
    generation: i64,
    claimed_at: i64,
    lease_until: i64,
    github_principal: String,
    control: owner_control::OwnerControl,
    checkpoint: Checkpoint,
}
/// Nonserializable current metadata claim. Saved records cannot construct it.
/// Its generation never becomes productive by clearing a pause or applying input.
pub(crate) struct RecoveryLease {
    claim: Claim,
    reference: RecordRef,
}
impl RecoveryLease {
    pub(crate) fn generation(&self) -> i64 {
        self.claim.generation
    }
    pub(crate) fn request_id(&self) -> &str {
        &self.claim.original.request_id
    }
}
fn digest(raw: &str) -> String {
    format!("{:x}", Sha256::digest(raw.as_bytes()))
}
fn original(conn: &Connection, workflow: &str, request: &str) -> Result<Original> {
    owner_revision::validate_history(conn, workflow)?;
    owner_dispatch::validate_history(conn, workflow)?;
    let (raw,len):(String,i64)=conn.query_row("SELECT substr(record,1,4227073),length(CAST(record AS BLOB)) FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(
        (1..=4 * 1024 * 1024 + 32 * 1024).contains(&len) && raw.len() as i64 == len,
        "recovery intent exceeds bound"
    );
    let record: crate::self_improve::OwnerPublicationRecord = serde_json::from_str(&raw)?;
    ensure!(
        record.workflow() == workflow
            && record.request() == request
            && serde_json::to_string(&record)? == raw,
        "recovery original intent changed"
    );
    record.validate_history(conn)?;
    let intent = super::owner_publication::reference(&raw);
    super::owner_publication::require(conn, request, &intent, &raw)?;
    let (input_context, activated_generation, preparation, body_sha) =
        record.recovery_identity()?;
    let message = owner_inbox::message_evidence(conn, workflow, request)?;
    ensure!(
        matches!(message.state.as_str(), "pending" | "applied")
            && digest(&message.body) == body_sha,
        "recovery original owner input changed"
    );
    super::discord_ingress::bound_workflow(conn, &message.origin, Some(workflow))?;
    let (repository, target, until, provider): (String, String, String, String) = conn.query_row(
        "SELECT repository,target,until_milestone,provider FROM command_job WHERE workflow=?1",
        [workflow],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let command = submission::Request {
        repository,
        target: serde_json::from_str(&target)?,
        until: serde_json::from_str(&until)?,
        provider: serde_json::from_str(&provider)?,
    }
    .normalized()?;
    ensure!(
        command.repository == record.repository(),
        "recovery repository changed"
    );
    let body:Option<(i64,String,i64)>=conn.query_row("SELECT sequence,substr(record,1,16385),length(CAST(record AS BLOB)) FROM workflow_owner_body_dispatch WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let body_dispatch = body
        .map(|(sequence, raw, len)| {
            ensure!(
                (1..=16384).contains(&len) && raw.len() as i64 == len,
                "recovery body record exceeds bound"
            );
            Ok::<_, anyhow::Error>(RecordRef {
                sequence,
                sha256: digest(&raw),
            })
        })
        .transpose()?;
    Ok(Original {
        workflow: workflow.into(),
        request_id: request.into(),
        intent,
        input_context,
        activated_generation,
        preparation,
        origin: message.origin,
        command,
        body_dispatch,
    })
}
fn table_exists(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_publication_recovery')",[],|r|r.get(0))?)
}
fn history(conn: &Connection, workflow: &str) -> Result<Option<(Claim, RecordRef)>> {
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_publication_recovery_claimed'",[workflow],|r|r.get(0))?;
    if !table_exists(conn)? {
        ensure!(events == 0, "recovery history index missing");
        return Ok(None);
    }
    let mut query=conn.prepare("SELECT generation,sequence,record_sha256,substr(record,1,32769),length(CAST(record AS BLOB)) FROM workflow_owner_publication_recovery WHERE workflow=?1 ORDER BY generation LIMIT 1025")?;
    let mut previous: Option<(Claim, RecordRef)> = None;
    let mut count = 0usize;
    for row in query.query_map([workflow], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })? {
        let (generation, sequence, sha, raw, len) = row?;
        count += 1;
        ensure!(
            count <= 1024
                && (1..=32768).contains(&len)
                && raw.len() as i64 == len
                && digest(&raw) == sha,
            "recovery history exceeds bound or changed"
        );
        let claim: Claim = serde_json::from_str(&raw)?;
        ensure!(
            serde_json::to_string(&claim)? == raw
                && claim.version == 1
                && claim.original.workflow == workflow
                && claim.generation == generation
                && claim.predecessor.generation.checked_add(1) == Some(generation)
                && claim.lease_until > claim.claimed_at
                && claim.owner.len() <= 256
                && !claim.owner.is_empty()
                && claim.owner.bytes().all(|b| b.is_ascii_graphic())
                && claim.github_principal.starts_with("github:"),
            "recovery claim shape changed"
        );
        ensure!(
            claim.original == original(conn, workflow, &claim.original.request_id)?,
            "recovery original lineage changed"
        );
        match &previous {
            None => ensure!(
                claim.previous.is_none()
                    && claim.predecessor.generation == claim.original.activated_generation,
                "recovery has no original predecessor"
            ),
            Some((last, reference)) => ensure!(
                claim.original == last.original
                    && claim.previous.as_ref() == Some(reference)
                    && claim.predecessor.generation == last.generation
                    && claim
                        .predecessor
                        .owner
                        .as_ref()
                        .is_none_or(|owner| owner == &last.owner),
                "recovery predecessor lineage changed"
            ),
        }
        let (kind, event_generation, payload, at): (String, i64, String, i64) = conn.query_row(
            "SELECT kind,generation,payload,at_ms FROM workflow_event WHERE workflow=?1 AND seq=?2",
            params![workflow, sequence],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        ensure!(
            kind == "owner_publication_recovery_claimed"
                && event_generation == generation
                && at == claim.claimed_at
                && serde_json::from_str::<serde_json::Value>(&payload)?
                    == serde_json::json!({"version":1,"request_id":claim.original.request_id,"record_sha256":sha}),
            "recovery claim event changed"
        );
        previous = Some((
            claim,
            RecordRef {
                sequence,
                sha256: sha,
            },
        ));
    }
    ensure!(count as i64 == events, "recovery claim history incomplete");
    let pending: Option<i64> = conn
        .query_row(
            "SELECT generation FROM workflow_owner_publication_recovery_pending WHERE workflow=?1",
            [workflow],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        pending == previous.as_ref().map(|(claim, _)| claim.generation),
        "recovery pending pointer changed"
    );
    Ok(previous)
}
/// Ordinary dispatch never consumes a metadata recovery generation, including
/// after input application or damage to the mutable pending pointer.
pub(super) fn require_no_recovery_dispatch(conn: &Connection, workflow: &str) -> Result<()> {
    ensure!(
        history(conn, workflow)?.is_none(),
        "owner-publication-recovery-metadata-only"
    );
    Ok(())
}
fn plan(conn: &Connection, workflow: &str) -> Result<Option<RecoveryPlan>> {
    let previous = history(conn, workflow)?;
    let row: Option<(String, Option<String>, i64, i64)> = conn
        .query_row(
            "SELECT checkpoint,owner,generation,lease_until FROM workflow WHERE id=?1",
            [workflow],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((raw, owner, generation, lease_until)) = row else {
        return Ok(None);
    };
    let checkpoint: Checkpoint = serde_json::from_str(&raw)?;
    if matches!(checkpoint.stage, Stage::Merged | Stage::Retired) {
        return Ok(None);
    }
    let turns = {
        let mut query=conn.prepare("SELECT t.request_id,t.state FROM workflow_owner_turn t JOIN workflow_owner_publication_intent p ON p.request_id=t.request_id WHERE t.workflow=?1 AND t.state NOT IN ('completed','setup_refused') LIMIT 2")?;
        let rows = query
            .query_map([workflow], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    if turns.is_empty() {
        return Ok(None);
    }
    ensure!(
        turns.len() == 1 && matches!(turns[0].1.as_str(), "claimed" | "uncertain"),
        "recovery original turn is unknown or ambiguous"
    );
    let original = original(conn, workflow, &turns[0].0)?;
    owner_inbox::require_pending(conn, workflow, &original.request_id)?;
    match &previous {
        None => ensure!(
            generation == original.activated_generation,
            "recovery current generation has no original lineage"
        ),
        Some((claim, _)) => ensure!(
            claim.original == original
                && claim.generation == generation
                && owner.as_ref().is_none_or(|owner| owner == &claim.owner),
            "recovery current predecessor changed"
        ),
    }
    Ok(Some(RecoveryPlan {
        original,
        predecessor: Predecessor {
            owner,
            generation,
            lease_until,
        },
        previous: previous.map(|(_, reference)| reference),
    }))
}
impl WorkflowStore {
    pub(crate) fn owner_publication_recovery_plan(
        &self,
        workflow: &str,
    ) -> Result<Option<RecoveryPlan>> {
        plan(&self.conn, workflow)
    }
    /// A live predecessor defers. Expiry permits metadata claiming only; it is
    /// never native teardown, body nonrelease or another model admission.
    pub(crate) fn claim_owner_publication_recovery(
        &mut self,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        owner: &str,
        now: i64,
        ttl: i64,
    ) -> Result<Option<RecoveryLease>> {
        ensure!(
            !owner.is_empty()
                && owner.len() <= 256
                && owner.bytes().all(|b| b.is_ascii_graphic())
                && ttl > 0,
            "invalid recovery lease"
        );
        let until = now.checked_add(ttl).context("recovery lease overflow")?;
        let expected = authorization.plan();
        authorization.require(expected, now)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(current) = plan(&tx, &expected.original.workflow)? else {
            return Ok(None);
        };
        if current != *expected || current.predecessor.lease_until > now {
            return Ok(None);
        }
        authorization.require(&current, now)?;
        let generation = current
            .predecessor
            .generation
            .checked_add(1)
            .context("recovery generation overflow")?;
        let checkpoint: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&current.original.workflow],
            |r| r.get(0),
        )?;
        let claim = Claim {
            version: 1,
            original: current.original,
            predecessor: current.predecessor,
            previous: current.previous,
            owner: owner.into(),
            generation,
            claimed_at: now,
            lease_until: until,
            github_principal: authorization.principal().into(),
            control: owner_control::read(&tx, &expected.original.workflow)?
                .context("recovery owner control missing")?,
            checkpoint: serde_json::from_str(&checkpoint)?,
        };
        let raw = serde_json::to_string(&claim)?;
        ensure!(raw.len() <= 32768, "recovery claim exceeds bound");
        let sha = digest(&raw);
        let changed=tx.execute("UPDATE workflow SET owner=?2,generation=?3,lease_until=?4,heartbeat_at=?5 WHERE id=?1 AND generation=?6 AND owner IS ?7 AND lease_until=?8 AND lease_until<=?5",
            params![claim.original.workflow,owner,generation,until,now,claim.predecessor.generation,claim.predecessor.owner,claim.predecessor.lease_until])?;
        if changed != 1 {
            return Ok(None);
        }
        event(&tx,&claim.original.workflow,generation,"owner_publication_recovery_claimed",&serde_json::json!({"version":1,"request_id":claim.original.request_id,"record_sha256":sha}).to_string(),now)?;
        let sequence: i64 = tx.query_row(
            "SELECT max(seq) FROM workflow_event WHERE workflow=?1",
            [&claim.original.workflow],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO workflow_owner_publication_recovery VALUES(?1,?2,?3,?4,?5)",
            params![claim.original.workflow, generation, sequence, sha, raw],
        )?;
        tx.execute("INSERT INTO workflow_owner_publication_recovery_pending VALUES(?1,?2) ON CONFLICT(workflow) DO UPDATE SET generation=excluded.generation",params![claim.original.workflow,generation])?;
        let reference = RecordRef {
            sequence,
            sha256: sha,
        };
        let recorded = history(&tx, &claim.original.workflow)?
            .context("recovery claim missing after append")?;
        ensure!(recorded.1 == reference, "recovery append changed");
        tx.commit()?;
        Ok(Some(RecoveryLease { claim, reference }))
    }
    pub(crate) fn validate_owner_publication_recovery(
        &self,
        lease: &RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        now: i64,
    ) -> Result<()> {
        authorization.require(authorization.plan(), now)?;
        ensure!(
            authorization.plan().original == lease.claim.original,
            "recovery authorization differs from original claim"
        );
        let recorded = history(&self.conn, &lease.claim.original.workflow)?
            .context("recovery claim missing")?;
        ensure!(recorded.1 == lease.reference, "recovery claim superseded");
        let valid:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow WHERE id=?1 AND owner=?2 AND generation=?3 AND lease_until>?4 AND json_extract(checkpoint,'$.stage') NOT IN ('merged','retired'))",params![lease.claim.original.workflow,lease.claim.owner,lease.claim.generation,now],|r|r.get(0))?;
        ensure!(valid, "recovery metadata lease expired or terminal");
        Ok(())
    }
}
