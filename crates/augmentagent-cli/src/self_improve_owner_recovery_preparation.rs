//! Reopen original private acceptance under fresh recovery/native read authority.
//! No conversion into an original activation producer or coding permission.
use super::*;
use crate::autoship_workflow::owner_publication_recovery::RecoveryPreparationRead;
use augmentagent_channel_core::native_vm::ownership::MetadataNativeOwnerProof;

/// The original native proof remains borrowed through this recovered metadata
/// preparation. Its underlying producer never escapes into activation APIs.
pub(crate) struct RecoveredOwnerPreparation<'claim, 'a, 'native> {
    record: OwnerPublicationRecord,
    producer: PreparedOwnerAcceptance,
    claim: &'claim crate::autoship_workflow::owner_publication_recovery::RecoveryLease,
    _native: &'a MetadataNativeOwnerProof<'native>,
}
impl RecoveredOwnerPreparation<'_, '_, '_> {
    fn draft(&self) -> OwnerPublicationDraft<'_> {
        OwnerPublicationDraft {
            record: self.record.clone(),
            producer: &self.producer,
        }
    }
    pub(crate) fn validate_native_current(&self) -> Result<()> {
        self._native.validate_current()
    }
    pub(crate) fn native_namespace(
        &self,
    ) -> &augmentagent_channel_core::native_vm::ownership::Authority {
        self._native.namespace()
    }
    pub(crate) fn native_binding(
        &self,
    ) -> &augmentagent_channel_core::native_vm::ownership::TransferBinding {
        self._native.binding()
    }
    pub(crate) fn publication_record(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<(OwnerPublicationRecord, String)> {
        let (raw,len):(String,i64)=conn.query_row("SELECT substr(record,1,4227073),length(CAST(record AS BLOB)) FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",rusqlite::params![self.record.workflow(),self.record.request()],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            (1..=MAX_INTENT as i64).contains(&len)
                && raw.len() as i64 == len
                && raw == serde_json::to_string(&self.record)?,
            "recovered publication original record changed"
        );
        self.record.validate_history(conn)?;
        Ok((self.record.clone(), raw))
    }
    pub(crate) fn with_scope_transition<T>(
        &self,
        visit: impl FnOnce(
            &augmentagent_channel_core::autoship_write_scope::ScopeTransition,
        ) -> Result<T>,
    ) -> Result<T> {
        self.validate_native_current()?;
        self.draft().with_scope_transition(visit)
    }
    pub(crate) fn commit_canonical(
        &self,
        access: &crate::autoship_workflow::OwnerPublicationCommitMetadata<'_>,
        transition: &augmentagent_channel_core::autoship_write_scope::ScopeTransition,
        facts_sha256: &str,
        existing: Option<&OwnerPublicationCompletion>,
        now: i64,
    ) -> Result<OwnerPublicationCompletion> {
        self.draft().commit_canonical(
            access,
            self._native,
            transition,
            facts_sha256,
            existing,
            now,
        )
    }
    /// Pins this borrowed metadata preparation to the same opaque recovery
    /// claim. Fresh store/authorization checks remain independently required.
    pub(crate) fn require_recovery_claim(
        &self,
        claim: &crate::autoship_workflow::owner_publication_recovery::RecoveryLease,
    ) -> Result<()> {
        ensure!(
            std::ptr::eq(self.claim, claim),
            "recovered preparation belongs to another recovery claim"
        );
        Ok(())
    }
    pub(crate) fn request_id(&self) -> &str {
        self.record.request()
    }
    pub(crate) fn original_generation(&self) -> i64 {
        self.record.input.generation
    }
    pub(crate) fn activated_generation(&self) -> i64 {
        self.record.activated_generation
    }
    pub(crate) fn context_sha256(&self) -> &str {
        self.record.preparation.context_sha256()
    }
    pub(crate) fn contract_hash(&self) -> &str {
        self.producer
            .prepared()
            .baseline
            .registration
            .contract_hash()
    }
}
pub(crate) fn reopen<'claim, 'a, 'native>(
    access: &RecoveryPreparationRead<'_, 'claim>,
    proof: &'a MetadataNativeOwnerProof<'native>,
    candidate: &Path,
) -> Result<RecoveredOwnerPreparation<'claim, 'a, 'native>> {
    let record = access.original_record(proof)?;
    record.candidate.verify(candidate)?;
    let raw = bytes(&record.preparation_file, 4 * 1024 * 1024)?;
    let stored: Stored = serde_json::from_slice(&raw)?;
    ensure!(
        stored.version == 3
            && serde_json::to_vec(&stored)? == raw
            && stored.input == record.input
            && stored.candidate == record.candidate
            && stored.canonical == record.canonical,
        "recovery original preparation envelope changed"
    );
    let previous = record.previous_prepared()?;
    stored.input.validate_prepared(
        &stored.prepared,
        previous.as_ref().map(|old| &old.baseline.registration),
    )?;
    let producer = PreparedOwnerAcceptance {
        input: stored.input,
        candidate: stored.candidate,
        canonical: stored.canonical,
        prepared: stored.prepared,
        path: record.preparation_file.clone(),
    };
    ensure!(
        producer.activation_binding()? == record.preparation,
        "recovery original preparation artifact changed"
    );
    record
        .canonical
        .publication_is_next(&record.next_manifest)?;
    let evidence = bytes(
        &record.canonical.control.join(&record.next_evidence.file),
        4 * 1024 * 1024,
    )?;
    ensure!(
        evidence.len() == record.next_evidence.bytes
            && amendment::digest(&evidence) == record.next_evidence.sha256
            && evidence == serde_json::to_vec(producer.prepared())?,
        "recovery original next evidence changed"
    );
    // All saved values remain descriptive; only the actual read authority and
    // still-held original native proof permit returning this metadata role.
    access.original_record(proof)?;
    Ok(RecoveredOwnerPreparation {
        record,
        producer,
        claim: access.recovery_claim(),
        _native: proof,
    })
}
