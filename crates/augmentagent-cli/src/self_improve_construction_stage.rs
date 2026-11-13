//! Connect construction intent to the existing original executor and witness.
//! This service is not yet enrolled by the held-work selector or adjudicator.
use super::*;

fn authenticate(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement)->Result<()> {
    episode.require_current(root,workflow)?;
    ensure!(measurement.episode==episode.key(),initial_repair::Hold::EvidenceUnknown);
    let saved:Measurement=read(workflow,&intent_key(episode.key(),measurement.slot))?.context(initial_repair::Hold::EvidenceUnknown)?;
    ensure!(saved.identity()?==measurement.identity()? && read::<Rejection>(workflow,&rejection_key(episode.key(),measurement.slot))?.is_none(),initial_repair::Hold::EvidenceUnknown);
    let turn:Turn=read(workflow,&turn_key(episode.key(),measurement.round))?.context(initial_repair::Hold::EvidenceUnknown)?;
    ensure!(turn.version==1 && turn.episode==episode.key() && turn.round==measurement.round
        && turn.input.identity()?==measurement.input_sha256,initial_repair::Hold::EvidenceUnknown);
    let reply=constructor_native::retained(root,workflow,episode,&turn.input)?;
    ensure!(digest(reply.text.as_bytes())==measurement.response_sha256
        && reply.provider==measurement.provider && reply.native_session==measurement.native_session,initial_repair::Hold::EvidenceUnknown);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unowned_constructor_text_cannot_start_original_staging() {
        let (root,control,old,guard,episode)=super::super::tests::fixture().await;
        let measurement=next_with(root.path(),&guard,super::super::tests::request(&episode,&old,"test-model"),|_| {
            std::future::ready(Ok(constructor_native::Reply {text:super::super::tests::proposal(&old),provider:"codex".into(),native_session:"unowned-model-string".into()}))
        }).await.unwrap();
        let before=std::fs::read_dir(control.path()).unwrap().count();
        let error=stage(root.path(),&guard,&episode,&measurement).await.err().unwrap();
        assert_eq!(error.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown));
        assert_eq!(std::fs::read_dir(control.path()).unwrap().count(),before,"unowned output created a staging checkout");
        assert!(guard.verifier_evidence_page("original_verifier_plan",0).unwrap().is_empty());
        guard.finish().unwrap();
    }
}

async fn stage(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement)->Result<()> {
    let _=(root,workflow,episode,measurement);
    // RED: untrusted model strings currently reach the staging boundary.
    Ok(())
}
