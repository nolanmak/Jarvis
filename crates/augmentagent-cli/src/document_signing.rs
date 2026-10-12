use super::*;
use anyhow::bail;
use augmentagent_docs::signing::{self, Placement, Proposal, Receipt};

pub async fn propose(
    store: Arc<Store>,
    input: &Path,
    output: &Path,
    signature: &str,
    placement: Placement,
) -> Result<()> {
    let root = std::env::var_os("WIKI_ROOT")
        .map(PathBuf::from)
        .context("WIKI_ROOT is required")?
        .canonicalize()?;
    let proposal = signing::propose(
        &root,
        &signing::state_root()?,
        input,
        output,
        signature,
        placement,
    )
    .await?;
    let human = format!("Apply signature '{}' to {} ({} pages).\nPage {}; x={}, y={}, width={} PDF points from bottom-left; date: {}.\nOutput: {}\nDocument SHA-256: {}\nSignature SHA-256: {}\nReview the attached unsigned PDF. Approve signs this exact revision only; it does not send the document.",
        proposal.signature, proposal.source, proposal.pages, proposal.placement.page, proposal.placement.x, proposal.placement.y, proposal.placement.width,
        proposal.placement.date.as_deref().unwrap_or("none"), proposal.output, proposal.document_sha256, proposal.signature_sha256);
    let token = std::env::var("DISCORD_BOT_TOKEN")
        .context("DISCORD_BOT_TOKEN required to post signing approval")?;
    let cid: u64 = std::env::var("DISCORD_CHANNEL_ID")
        .context("DISCORD_CHANNEL_ID required")?
        .parse()?;
    let email = augmentagent_store::Email {
        message_id: format!("doc-sign:{}", uuid::Uuid::new_v4()),
        thread_id: None,
        from: "Owner signature".into(),
        to: String::new(),
        cc: String::new(),
        attachments: vec![],
        subject: format!("Sign PDF: {}", proposal.source),
        body: serde_json::to_string(&proposal)?,
        date: chrono::Utc::now().to_rfc3339(),
        account_entity_id: None,
        platform: "document-signing".into(),
        kind: "sign_pdf".into(),
    };
    store.upsert_email(&email)?;
    let id = store.log_action(
        &email.message_id,
        None,
        &email.from,
        &email.subject,
        Some(&email.body),
        Some(&human),
        ActionStatus::Pending,
    )?;
    let unsigned = signing::review_bytes(&root, &proposal)?;
    let attachment = serenity::all::CreateAttachment::bytes(unsigned, "unsigned-review.pdf");
    let card = approval_message(&id, &email, &human, 0).add_file(attachment);
    serenity::all::ChannelId::new(cid)
        .send_message(&serenity::http::Http::new(&token), card)
        .await
        .context("post signing approval card")?;
    println!("Signature approval posted: {id}\n{human}");
    Ok(())
}

impl ReplyApprover {
    pub(crate) async fn approve_document_signature(
        &self,
        id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let state = match signing::state_root() {
            Ok(state) => state,
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: e.to_string(),
                }
            }
        };
        self.approve_document_signature_at(id, action, &state).await
    }

    async fn approve_document_signature_at(
        &self,
        id: &str,
        action: augmentagent_store::ActionWithEmail,
        state: &Path,
    ) -> ApprovalActionOutcome {
        let result: Result<ApprovalActionOutcome> = async {
            let root = self.wiki_root.as_deref().context("wiki root unavailable for signing")?;
            if let Some(saved) = self.store.document_signature(id)? {
                let receipt: Receipt = serde_json::from_str(&saved)?;
                signing::verify_receipt(root, &receipt)?;
                return Ok(ApprovalActionOutcome::DocumentSigned { receipt: format!("{}\nSHA-256: {}", receipt.attach, receipt.output_sha256), already_existed: true, output: Some((root.join(&receipt.proposal.output), receipt.output_sha256.clone())) });
            }
            if action.action.status != "pending" { bail!("signing action is {}; propose a new reviewed revision", action.action.status); }
            let proposal: Proposal = serde_json::from_str(&action.email.body).context("invalid signing proposal")?;
            let receipt = signing::execute(root, state, &proposal).await?;
            let json = serde_json::to_string(&receipt)?;
            // Publication is an effect: persistence failures are unconfirmed, never retryable.
            self.store.record_document_signature(id, &json).context("signed output exists but audit receipt is unconfirmed; inspect output before retrying")?;
            self.store.update_action_status(id, ActionStatus::Sent, None, None).context("signed output exists but action status is unconfirmed; inspect receipt")?;
            Ok(ApprovalActionOutcome::DocumentSigned { receipt: format!("{}\nSHA-256: {}", receipt.attach, receipt.output_sha256), already_existed: false, output: Some((root.join(&receipt.proposal.output), receipt.output_sha256.clone())) })
        }.await;
        match result {
            Ok(outcome) => outcome,
            Err(e) => {
                let message = format!("{e:#}");
                let _ =
                    self.store
                        .update_action_status(id, ActionStatus::Error, None, Some(&message));
                ApprovalActionOutcome::Failed { message }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn fixture() -> (tempfile::TempDir, ReplyApprover, String, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("wiki");
        let state = temp.path().join("assets");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("agreement.md"),
            "# QA only\n\nSynthetic services agreement.\n",
        )
        .unwrap();
        augmentagent_docs::pdf::render_pdf(&root, "agreement.md".as_ref(), None)
            .await
            .unwrap();
        signing::register_at(&state, &std::fs::read(root.join("agreement.pdf")).unwrap()).unwrap();
        let png = temp.path().join("test.png");
        std::fs::write(
            &png,
            include_bytes!("../../augmentagent-docs/tests/fixtures/signature-test.png"),
        )
        .unwrap();
        signing::install(&state, "default", &png).unwrap();
        let p = signing::propose(
            &root,
            &state,
            "agreement.pdf".as_ref(),
            "signed.pdf".as_ref(),
            "default",
            Placement {
                page: 1,
                x: 72.,
                y: 72.,
                width: 144.,
                date: None,
            },
        )
        .await
        .unwrap();
        let store = Arc::new(Store::open(temp.path().join("qa.db")).unwrap());
        let email = augmentagent_store::Email {
            message_id: "sign:test".into(),
            thread_id: None,
            from: "Owner".into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: "Sign QA agreement".into(),
            body: serde_json::to_string(&p).unwrap(),
            date: "2026-10-12".into(),
            account_entity_id: None,
            platform: "document-signing".into(),
            kind: "sign_pdf".into(),
        };
        store.upsert_email(&email).unwrap();
        let id = store
            .log_action(
                &email.message_id,
                None,
                &email.from,
                &email.subject,
                Some(&email.body),
                Some("Apply default signature to this exact QA document"),
                ActionStatus::Pending,
            )
            .unwrap();
        let mut approver = test_support::approver_with_store(store);
        approver.wiki_root = Some(root);
        (temp, approver, id, state)
    }
    async fn approve(a: &ReplyApprover, id: &str, state: &Path) -> ApprovalActionOutcome {
        let action = a.store.get_action_with_email(id).unwrap().unwrap();
        a.audited(
            id,
            "approve",
            a.approve_document_signature_at(id, action, state),
        )
        .await
    }
    #[tokio::test]
    async fn approval_signs_once_and_records_immutable_receipt_and_next_turn_context() {
        let (_temp, a, id, state) = fixture().await;
        assert!(!a.wiki_root.as_ref().unwrap().join("signed.pdf").exists());
        let (one, two) = tokio::join!(approve(&a, &id, &state), approve(&a, &id, &state));
        assert_eq!(
            [&one, &two]
                .iter()
                .filter(|r| matches!(
                    r,
                    ApprovalActionOutcome::DocumentSigned {
                        already_existed: false,
                        ..
                    }
                ))
                .count(),
            1
        );
        let receipt = a.store.document_signature(&id).unwrap().unwrap();
        let parsed: Receipt = serde_json::from_str(&receipt).unwrap();
        assert_eq!(parsed.proposal.signature, "default");
        assert!(!parsed.proposal.document_sha256.is_empty());
        assert!(a.store.record_document_signature(&id, &receipt).is_err());
        assert!(a
            .store
            .with_conn(|c| c.execute("UPDATE document_signatures SET receipt='changed'", []))
            .is_err());
        assert!(matches!(
            approve(&a, &id, &state).await,
            ApprovalActionOutcome::DocumentSigned {
                already_existed: true,
                ..
            }
        ));
        let context = crate::approval_context::context(&a.store).unwrap();
        assert!(context.contains("completed") && context.contains("ATTACH: signed.pdf"));
        assert_eq!(
            a.store.approval_history(None, 20, Some(&id)).unwrap().len(),
            1
        );
        // Routine action retention must not erase or block the immutable signing audit.
        a.store
            .with_conn(|c| c.execute("DELETE FROM actions WHERE id=?1", [&id]))
            .unwrap();
        assert_eq!(
            a.store.document_signature(&id).unwrap().as_deref(),
            Some(receipt.as_str())
        );
    }
    #[tokio::test]
    async fn changed_document_and_unsupported_controls_never_sign() {
        let (_temp, a, id, state) = fixture().await;
        assert!(matches!(
            a.schedule(&id, 0).await,
            ApprovalActionOutcome::Failed { .. }
        ));
        assert!(matches!(
            a.revise(&id, "move it").await,
            ApprovalActionOutcome::Failed { .. }
        ));
        std::fs::write(
            a.wiki_root.as_ref().unwrap().join("agreement.pdf"),
            b"modified",
        )
        .unwrap();
        assert!(matches!(
            approve(&a, &id, &state).await,
            ApprovalActionOutcome::Failed { .. }
        ));
        assert!(!a.wiki_root.as_ref().unwrap().join("signed.pdf").exists());
        assert!(a.store.document_signature(&id).unwrap().is_none());
    }
    #[tokio::test]
    async fn audit_failure_after_publication_is_unconfirmed_and_never_replayed() {
        let (_temp, a, id, state) = fixture().await;
        a.store.with_conn(|c|c.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON document_signatures BEGIN SELECT RAISE(FAIL,'injected'); END;")).unwrap();
        let result = approve(&a, &id, &state).await;
        assert!(
            matches!(result,ApprovalActionOutcome::Failed { ref message } if message.contains("unconfirmed"))
        );
        assert!(a.wiki_root.as_ref().unwrap().join("signed.pdf").exists());
        assert_eq!(
            a.store.approval_history(None, 20, Some(&id)).unwrap()[0].outcome,
            "unconfirmed"
        );
        assert!(matches!(
            approve(&a, &id, &state).await,
            ApprovalActionOutcome::Failed { .. }
        ));
    }
}
