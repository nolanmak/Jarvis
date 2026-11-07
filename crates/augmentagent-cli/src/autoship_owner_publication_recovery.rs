//! Authenticated metadata claim history. No model, body or native release permit.
use super::discord_ingress::DiscordOrigin;
use super::*;

#[path = "autoship_owner_recovery_blocked.rs"]
mod blocked;
#[path="autoship_owner_recovery_native_intent.rs"]
mod native_intent;
pub(crate) use native_intent::RecoveryNativeIntentPersistence;
pub(crate) use blocked::RecoveryBlockedState;
#[path="autoship_owner_successor_ready.rs"]
mod successor_ready;
pub(crate) use successor_ready::ReadySettlement;

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
    original_identity(conn, workflow, request)
}
// Independent immutable identity layer. Publication completion reference readers
// must not re-enter revision/publication/body completion validation here.
fn original_identity(conn: &Connection, workflow: &str, request: &str) -> Result<Original> {
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
    let mut claims = journal(conn, workflow)?;
    if !claims.is_empty() {
        owner_revision::validate_history(conn, workflow)?;
        owner_dispatch::validate_history(conn, workflow)?;
    }
    Ok(claims.pop())
}
/// Immutable bounded claim/event/original-intent joins only. This layer grants
/// no current authority and never calls higher-level completion validators.
fn journal(conn: &Connection, workflow: &str) -> Result<Vec<(Claim, RecordRef)>> {
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_publication_recovery_claimed'",[workflow],|r|r.get(0))?;
    if !table_exists(conn)? {
        ensure!(events == 0, "recovery history index missing");
        return Ok(Vec::new());
    }
    let mut query=conn.prepare("SELECT generation,sequence,record_sha256,substr(record,1,32769),length(CAST(record AS BLOB)) FROM workflow_owner_publication_recovery WHERE workflow=?1 ORDER BY generation LIMIT 1025")?;
    let mut claims: Vec<(Claim, RecordRef)> = Vec::new();
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
                && claim.predecessor.lease_until <= claim.claimed_at
                && !matches!(claim.checkpoint.stage, Stage::Merged | Stage::Retired)
                && claim.owner.len() <= 256
                && !claim.owner.is_empty()
                && claim.owner.bytes().all(|b| b.is_ascii_graphic())
                && claim.github_principal.starts_with("github:"),
            "recovery claim shape changed"
        );
        ensure!(
            claim.original == original_identity(conn, workflow, &claim.original.request_id)?,
            "recovery original lineage changed"
        );
        match claims.last() {
            None => ensure!(
                claim.previous.is_none()
                    && claim.predecessor.generation == claim.original.activated_generation,
                "recovery has no original predecessor"
            ),
            Some((last, reference)) => ensure!(
                claim.original == last.original
                    && claim.previous.as_ref() == Some(reference)
                    && reference.sequence < sequence
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
        claims.push((
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
    let expected=match claims.last() {
        Some((claim,reference)) if successor_ready::for_claim(conn,claim,reference)?.is_none()=>Some(claim.generation),
        _=>None,
    };
    let ready_events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_successor_ready'",[workflow],|r|r.get(0))?;
    let mut ready_count=0;
    for (claim,reference) in &claims {if successor_ready::for_claim(conn,claim,reference)?.is_some() {ready_count+=1;}}
    let indexed_ready:i64=if conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_successor_ready')",[],|r|r.get::<_,bool>(0))? {
        let (count,bytes):(i64,i64)=conn.query_row("SELECT count(*),coalesce(sum(length(CAST(record AS BLOB))),0) FROM workflow_owner_successor_ready WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(count<=1024 && bytes<=128*1024*1024,"owner readiness history exceeds aggregate bound");count
    }else {0};
    ensure!(ready_count==ready_events && indexed_ready==ready_count,"owner readiness history has no original recovery claim");
    ensure!(pending==expected,"recovery pending pointer changed");
    Ok(claims)
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
    if turns.len()==1 && turns[0].1=="recovery_ready" {
        let (claim,reference)=previous.as_ref().context("owner readiness has no recovery claim")?;
        ensure!(claim.original.request_id==turns[0].0 && successor_ready::for_claim(conn,claim,reference)?.is_some(),"owner ready turn lost its immutable bridge");
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
        if blocked::blocked_state(&tx, &current, now)?.is_some() { return Ok(None); }
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
        require_current_claim(&self.conn, lease, now)?;
        Ok(())
    }
}

// No native I/O: callers may use this under the original native locks.
fn require_current_claim(conn: &Connection, lease: &RecoveryLease, now: i64) -> Result<()> {
    let recorded =
        history(conn, &lease.claim.original.workflow)?.context("recovery claim missing")?;
    ensure!(recorded.1 == lease.reference, "recovery claim superseded");
    let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow WHERE id=?1 AND owner=?2 AND generation=?3 AND lease_until>?4 AND json_extract(checkpoint,'$.stage') NOT IN ('merged','retired'))",params![lease.claim.original.workflow,lease.claim.owner,lease.claim.generation,now],|r|r.get(0))?;
    ensure!(valid, "recovery metadata lease expired or terminal");
    Ok(())
}
/// Bounded authenticated metadata fence for a logger that never escapes its
/// native metadata visitor. Capturing the same Arc retains, rather than remints,
/// the claim; the fresh grant deadline remains enforced at every callback.
pub(crate) fn native_metadata_fence(
    store: std::sync::Arc<std::sync::Mutex<WorkflowStore>>,
    lease: std::sync::Arc<RecoveryLease>,
    authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
    now: i64,
) -> Result<std::sync::Arc<dyn Fn() -> Result<()> + Send + Sync>> {
    store
        .lock()
        .map_err(|_| anyhow::anyhow!("recovery metadata store poisoned"))?
        .validate_owner_publication_recovery(&lease, authorization, now)?;
    let expires = authorization.expires_at();
    Ok(std::sync::Arc::new(move || {
        let now = chrono::Utc::now().timestamp_millis();
        ensure!(
            now <= expires,
            "recovery native metadata authorization expired"
        );
        let locked = store
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery metadata store poisoned"))?;
        require_current_claim(&locked.conn, &lease, now)
    }))
}
/// Borrowed transaction context minted only after fresh authorization and the
/// current opaque claim/original intent checks. Never execution/native proof.
pub(super) struct RecoveryNativeContext<'a> {
    conn: &'a Connection,
    workflow: &'a str,
    binding: Binding,
    namespace: augmentagent_channel_core::native_vm::ownership::Authority,
}
impl RecoveryNativeContext<'_> {
    pub(super) fn conn(&self) -> &Connection {
        self.conn
    }
    pub(super) fn workflow(&self) -> &str {
        self.workflow
    }
    pub(super) fn binding(&self) -> &Binding {
        &self.binding
    }
    pub(super) fn namespace(&self) -> &augmentagent_channel_core::native_vm::ownership::Authority {
        &self.namespace
    }
}
impl WorkflowStore {
    pub(crate) fn owner_publication_recovery_native_facts(
        &mut self,
        lease: &RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        now: i64,
    ) -> Result<super::owner_publication::OwnerPublicationNativeFacts> {
        self.validate_owner_publication_recovery(lease, authorization, now)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_current_claim(&tx, lease, now)?;
        let result = recovery_native_facts_tx(&tx, lease, authorization, now)?;
        tx.commit()?;
        Ok(result)
    }
}

pub(super) fn recovery_native_facts_tx(
    tx: &Transaction<'_>,
    lease: &RecoveryLease,
    authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
    now: i64,
) -> Result<super::owner_publication::OwnerPublicationNativeFacts> {
    require_authorized_claim(tx, lease, authorization, now)?;
    let original = &lease.claim.original;
    // Use the shared bounded intent reader; the namespace independently follows
    // only the validated publication/native lineage.
    let (record,_)=native_intent::original_record(tx,original)?;
    let namespace=native_intent::frontier(tx,original)?;
    let context = RecoveryNativeContext {
        conn: &tx,
        workflow: &original.workflow,
        binding: record.recovery_binding().clone(),
        namespace,
    };
    let facts = super::native_handoff::read_recovery_metadata_facts(&context)?;
    require_current_claim(&tx, lease, now)?;
    Ok(super::owner_publication::OwnerPublicationNativeFacts::from_recovery_metadata(facts))
}

/// Transaction-borrowed permission to read the original protected preparation.
/// It grants no activation, source, scope mutation or body permission.
pub(crate) struct RecoveryPreparationRead<'tx, 'claim> {
    conn: &'tx Connection,
    lease: &'claim RecoveryLease,
    namespace: augmentagent_channel_core::native_vm::ownership::Authority,
    original_binding: augmentagent_channel_core::native_vm::ownership::TransferBinding,
}
impl<'claim> RecoveryPreparationRead<'_, 'claim> {
    pub(crate) fn recovery_claim(&self) -> &'claim RecoveryLease {
        self.lease
    }
    pub(crate) fn original_record(
        &self,
        proof: &augmentagent_channel_core::native_vm::ownership::MetadataNativeOwnerProof<'_>,
    ) -> Result<crate::self_improve::OwnerPublicationRecord> {
        ensure!(
            proof.namespace() == &self.namespace && proof.binding() == &self.original_binding,
            "recovery preparation native proof changed"
        );
        let original = &self.lease.claim.original;
        let (raw,len):(String,i64)=self.conn.query_row("SELECT substr(record,1,4227073),length(CAST(record AS BLOB)) FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",params![original.workflow,original.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            (1..=4 * 1024 * 1024 + 32 * 1024).contains(&len) && raw.len() as i64 == len,
            "recovery preparation intent exceeds bound"
        );
        super::owner_publication::require(self.conn, &original.request_id, &original.intent, &raw)?;
        let record: crate::self_improve::OwnerPublicationRecord = serde_json::from_str(&raw)?;
        ensure!(
            serde_json::to_string(&record)? == raw,
            "recovery preparation intent encoding changed"
        );
        record.validate_history(self.conn)?;
        ensure!(native_intent::frontier(self.conn,original)?==self.namespace,"recovery preparation native frontier changed");
        if self.namespace.owner.lease_generation!=original.activated_generation {
            record.validate_completed_canonical_files(&native_intent::completed(self.conn,original)?)?;
        }
        let (context, generation, preparation, _) = record.recovery_identity()?;
        ensure!(
            context == original.input_context
                && generation == original.activated_generation
                && preparation == original.preparation,
            "recovery original preparation context changed"
        );
        Ok(record)
    }
}
impl WorkflowStore {
    /// Original native locks are already held by `proof`. All validation and
    /// bounded protected reads run in this short SQL transaction; no native I/O
    /// or provider/GitHub call occurs while it is held.
    pub(crate) fn with_owner_recovery_preparation<'claim, T>(
        &mut self,
        lease: &'claim RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        proof: &augmentagent_channel_core::native_vm::ownership::MetadataNativeOwnerProof<'_>,
        now: i64,
        read: impl FnOnce(&RecoveryPreparationRead<'_, 'claim>) -> Result<T>,
    ) -> Result<T> {
        let facts = self.owner_publication_recovery_native_facts(lease, authorization, now)?;
        ensure!(
            facts.namespace() == proof.namespace() && facts.original_binding() == proof.binding(),
            "recovery preparation requires exact original native proof"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_current_claim(&tx, lease, now)?;
        let access = RecoveryPreparationRead {
            conn: &tx,
            lease,
            namespace: proof.namespace().clone(),
            original_binding: proof.binding().clone(),
        };
        access.original_record(proof)?;
        let result = read(&access)?;
        let current = chrono::Utc::now().timestamp_millis();
        authorization.require(authorization.plan(), current)?;
        require_current_claim(&tx, lease, current)?;
        access.original_record(proof)?;
        tx.commit()?;
        Ok(result)
    }
}

/// Descriptive durable join, never a lease or native/write capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationRecoveryReference {
    version: u32,
    generation: i64,
    sequence: i64,
    record_sha256: String,
}
impl PublicationRecoveryReference {
    pub(crate) fn generation(&self) -> i64 {
        self.generation
    }
    pub(crate) fn sequence(&self) -> i64 {
        self.sequence
    }
}
impl RecoveryLease {
    pub(super) fn publication_reference(&self) -> PublicationRecoveryReference {
        PublicationRecoveryReference {
            version: 1,
            generation: self.claim.generation,
            sequence: self.reference.sequence,
            record_sha256: self.reference.sha256.clone(),
        }
    }
}
/// Validate a saved completion reference without the recursive current-authority
/// reader. The caller's full revision/publication validation remains required.
pub(crate) fn validate_publication_reference(
    conn: &Connection,
    workflow: &str,
    request: &str,
    raw_intent: &str,
    reference: &PublicationRecoveryReference,
    at: i64,
) -> Result<()> {
    ensure!(
        reference.version == 1 && reference.sequence > 0 && reference.record_sha256.len() == 64,
        "recovery publication reference shape changed"
    );
    let claims = journal(conn, workflow)?;
    let (claim, record) = claims
        .iter()
        .find(|(claim, record)| {
            claim.generation == reference.generation && record.sequence == reference.sequence
        })
        .context("recovery publication claim missing")?;
    ensure!(
        record.sha256 == reference.record_sha256
            && claim.original.workflow == workflow
            && claim.original.request_id == request
            && claim.original.intent == super::owner_publication::reference(raw_intent)
            && claim.claimed_at <= at,
        "recovery publication claim differs from original intent"
    );
    Ok(())
}
pub(super) fn require_authorized_claim(
    conn: &Connection,
    lease: &RecoveryLease,
    authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
    now: i64,
) -> Result<()> {
    authorization.require(authorization.plan(), now)?;
    ensure!(
        authorization.plan().original == lease.claim.original,
        "recovery authorization differs from original claim"
    );
    require_current_claim(conn, lease, now)
}

/// A saved completion must occupy the claim's actual event interval, not merely
/// name a real claim from the same original context. This is descriptive history
/// validation; it cannot authorize a new mutation or revive an expired lease.
pub(crate) fn validate_publication_event_position(
    conn: &Connection,
    workflow: &str,
    request: &str,
    reference: &PublicationRecoveryReference,
    sequence: i64,
) -> Result<()> {
    let claims = journal(conn, workflow)?;
    let index = claims
        .iter()
        .position(|(claim, row)| {
            claim.original.request_id == request
                && claim.generation == reference.generation
                && row.sequence == reference.sequence
                && row.sha256 == reference.record_sha256
        })
        .context("publication claim event interval missing")?;
    let next = claims
        .iter()
        .skip(index + 1)
        .find(|(claim, _)| claim.original.request_id == request)
        .map(|(_, row)| row.sequence);
    ensure!(
        reference.sequence < sequence && next.is_none_or(|next| sequence < next),
        "owner publication completion is outside recovery claim event window"
    );
    Ok(())
}
pub(crate) fn validate_live_publication_event_position(
    conn: &Connection,
    workflow: &str,
    request: &str,
    sequence: i64,
) -> Result<()> {
    let first = journal(conn, workflow)?
        .into_iter()
        .find(|(claim, _)| claim.original.request_id == request)
        .map(|(_, row)| row.sequence);
    ensure!(
        first.is_none_or(|first| sequence < first),
        "live owner publication completion follows a recovery claim fence"
    );
    Ok(())
}

pub(super) fn require_transfer_context(
    conn: &Connection,
    lease: &RecoveryLease,
    authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
    pending: &PendingNativeHandoff,
    now: i64,
) -> Result<()> {
    require_authorized_claim(conn, lease, authorization, now)?;
    let original = &lease.claim.original;
    let (raw,len):(String,i64)=conn.query_row("SELECT substr(record,1,4227073),length(CAST(record AS BLOB)) FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",params![original.workflow,original.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(
        (1..=4 * 1024 * 1024 + 32 * 1024).contains(&len) && raw.len() as i64 == len,
        "owner recovery transfer intent exceeds bound"
    );
    super::owner_publication::require(conn, &original.request_id, &original.intent, &raw)?;
    let record: crate::self_improve::OwnerPublicationRecord = serde_json::from_str(&raw)?;
    if pending.request().destination.owner.lease_generation==original.activated_generation {
        record.require_recovery_transfer(pending)?;
        ensure!(lease.generation()>original.activated_generation,"owner recovery transfer has no successor claim");
    }else {
        native_intent::require_forward(conn,original,pending)?;
        ensure!(lease.generation()>=pending.request().destination.owner.lease_generation,"recovery forward transfer is newer than current claim");
    }
    Ok(())
}
impl WorkflowStore {
    pub(crate) fn owner_recovery_pending_transfer(
        &mut self,
        lease: &RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        now: i64,
    ) -> Result<Option<PendingNativeHandoff>> {
        self.validate_owner_publication_recovery(lease, authorization, now)?;
        let pending = match self.resolve_native_handoff_persistence(
            &lease.claim.original.workflow,
            None,
            Err(anyhow::anyhow!(
                augmentagent_channel_core::native_vm::ownership::TransferReason::PersistRetry
            )),
        ) {
            native_pending::NativePreparationPersistence::Durable(
                native_pending::HandoffPreparation::Prepared(p)
                | native_pending::HandoffPreparation::ReconcileExisting(p),
            ) => Some(p),
            native_pending::NativePreparationPersistence::NotCommitted { .. } => None,
            native_pending::NativePreparationPersistence::Unknown { reason, .. } => {
                anyhow::bail!(reason)
            }
        };
        if let Some(pending) = &pending {
            require_transfer_context(&self.conn, lease, authorization, pending, now)?;
        }
        Ok(pending)
    }
    pub(crate) fn validate_owner_recovery_transfer(
        &self,
        lease: &RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        pending: &PendingNativeHandoff,
        now: i64,
    ) -> Result<()> {
        require_transfer_context(&self.conn, lease, authorization, pending, now)
    }
    pub(crate) fn owner_recovery_transfer_replay_is_current(
        &mut self,
        lease: &RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        pending: &PendingNativeHandoff,
        now: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        require_transfer_context(&tx, lease, authorization, pending, now)?;
        native_pending::require_clear_abort_barriers(&tx, &pending.request().workflow_id)?;
        let row:(String,i64,u64,String)=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[&pending.request().workflow_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(
            native_pending::decode(&tx, &pending.request().workflow_id, row)? == *pending,
            "owner recovery transfer pending identity changed"
        );
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",[&pending.request().workflow_id],|r|r.get(0))?;
        ensure!(
            !blocked,
            "owner recovery transfer has unsettled remote operation"
        );
        ensure!(!tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2)",params![pending.request().workflow_id,pending.request().destination.owner.lease_generation],|r|r.get::<_,bool>(0))?,"owner recovery transfer destination is already registered");
        for source in &pending.request().source_owners {
            ensure!(tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3 AND retirement_reason IS NULL AND retired_at IS NULL)",params![pending.request().workflow_id,source.owner.lease_generation,source.audit_path],|r|r.get::<_,bool>(0))?,"owner recovery transfer source is no longer active");
        }
        tx.commit()?;
        Ok(())
    }
}

/// Transaction-scoped actual recovery admission, never an execution/native
/// capability. Fields are private so a saved descriptor cannot construct it.
pub(super) struct RecoveryNativeSnapshotContext<'tx,'db,'claim> {
    tx:&'tx Transaction<'db>, lease:&'claim RecoveryLease,
    facts:&'tx super::native_handoff::NativeHandoffFacts,
}
impl<'db> RecoveryNativeSnapshotContext<'_,'db,'_> {
    pub(super) fn transaction(&self)->&Transaction<'db> {self.tx}
    pub(super) fn generation(&self)->i64 {self.lease.generation()}
    pub(super) fn facts(&self)->&super::native_handoff::NativeHandoffFacts {self.facts}
}
impl WorkflowStore {
    /// Core already holds the original preparation scan. No native IO, model
    /// admission, replacement lease or transfer request is created here.
    pub(crate) fn capture_owner_recovery_native_snapshot(&mut self, lease:&RecoveryLease,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        basis:&super::owner_publication::OwnerPublicationNativeFacts,
        scan:&augmentagent_channel_core::native_vm::ownership::PreparationScan<'_>,now:i64)->Result<super::native_handoff::NativeHandoffSnapshot> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let snapshot=capture_recovery_snapshot_tx(&tx,lease,authorization,basis,scan,now)?;
        let current=chrono::Utc::now().timestamp_millis();
        require_authorized_claim(&tx,lease,authorization,current)?;
        let checked=recovery_native_facts_tx(&tx,lease,authorization,current)?;
        ensure!(checked.fingerprint()?==basis.fingerprint()?,"recovery native snapshot facts changed before commit");
        tx.commit()?;Ok(snapshot)
    }
}

fn capture_recovery_snapshot_tx(tx:&Transaction<'_>,lease:&RecoveryLease,
    authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
    basis:&super::owner_publication::OwnerPublicationNativeFacts,
    scan:&augmentagent_channel_core::native_vm::ownership::PreparationScan<'_>,now:i64)->Result<super::native_handoff::NativeHandoffSnapshot> {
        let fresh=recovery_native_facts_tx(tx,lease,authorization,now)?;
        ensure!(fresh.fingerprint()?==basis.fingerprint()?,"recovery native snapshot basis changed");
        ensure!(scan.binding()==fresh.original_binding() && scan.source_owners()==fresh.snapshot_facts().source_owners
            && scan.destination().owner.workflow_id==fresh.snapshot_facts().workflow_id
            && scan.destination().owner.lease_generation==lease.generation(),"recovery native snapshot scan scope differs from original claim");
        let rows=super::native_handoff::selected_rows(tx,fresh.snapshot_facts(),scan.inventory(),scan.retired())?;
        let context=RecoveryNativeSnapshotContext {tx:tx,lease,facts:fresh.snapshot_facts()};
        let snapshot=super::native_handoff::capture_recovery_selected(&context,rows,now)?;
    Ok(snapshot)
}

pub(super) fn require_ordinary_native_transfer(conn:&Connection,pending:&PendingNativeHandoff)->Result<()> {native_intent::require_ordinary(conn,pending)}
pub(super) fn require_terminal_native_transfer(conn:&Connection,pending:&PendingNativeHandoff)->Result<()> {native_intent::require_terminal(conn,pending)}
