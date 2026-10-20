use super::*;
use anyhow::bail;
use augmentagent_docs::signing::{self, PlacementRequest, Proposal, Receipt};

pub async fn propose(
    store: Arc<Store>,
    input: &Path,
    output: &Path,
    signature: &str,
    request: PlacementRequest,
) -> Result<()> {
    let root = std::env::var_os("WIKI_ROOT")
        .map(PathBuf::from)
        .context("WIKI_ROOT is required")?
        .canonicalize()?;
    let signing::Proposed { proposal, preview_png } = signing::propose_with_preview(
        &root,
        &signing::state_root()?,
        input,
        output,
        signature,
        request,
    )
    .await?;
    let human = card_text(&proposal);
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
    let card = approval_message(&id, &email, &human, 0).add_files(card_files(unsigned, preview_png));
    serenity::all::ChannelId::new(cid)
        .send_message(&serenity::http::Http::new(&token), card)
        .await
        .context("post signing approval card")?;
    println!("Signature approval posted: {id}\n{human}");
    Ok(())
}

/// What the owner reads before approving: where the ink lands (and on which
/// detected line), plus the hashes binding the preview to the published bytes.
fn card_text(p: &Proposal) -> String {
    let pl = &p.placement;
    let placed = match (&pl.ink, &pl.anchor) {
        (Some(ink), Some(a)) => format!(
            "on line {} (top y={}, x {}–{}{}): ink bottom y={}, {:.1} pt above the line; ink x={}, width={}",
            a.line, a.top, a.x0, a.x1,
            a.text.as_deref().map(|t| format!(", above \"{t}\"")).unwrap_or_default(),
            ink.y, ink.y - a.top, ink.x, ink.width),
        (Some(ink), None) => format!("ink bottom-left x={}, y={}, width={}", ink.x, ink.y, ink.width),
        _ => format!("image box x={}, y={}, width={}", pl.x, pl.y, pl.width),
    };
    let mut text = format!(
        "Apply signature '{}' to {} ({} pages).\nPage {}, {placed} (PDF points from bottom-left); date: {}.\nOutput: {}\nDocument SHA-256: {}\nSignature SHA-256: {}",
        p.signature, p.source, p.pages, pl.page, pl.date.as_deref().unwrap_or("none"), p.output, p.document_sha256, p.signature_sha256);
    if let (Some(preview), Some(out)) = (&p.preview_sha256, &p.expected_output_sha256) {
        text.push_str(&format!("\nPreview SHA-256: {preview}\nSigned output SHA-256: {out}"));
    }
    text.push_str("\nThe preview image is the signed page exactly as approval will publish it. Approve signs this exact revision only; it does not send the document.");
    text
}

fn card_files(unsigned: Vec<u8>, preview_png: Vec<u8>) -> Vec<serenity::all::CreateAttachment> {
    vec![
        serenity::all::CreateAttachment::bytes(preview_png, "signed-page-preview.png"),
        serenity::all::CreateAttachment::bytes(unsigned, "unsigned-review.pdf"),
    ]
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
            PlacementRequest::ink(1, 72., 72., 144., None),
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
        // #1507 AC11 — the failure is recorded, never a silent no-op.
        let row = a.store.get_action_with_email(&id).unwrap().unwrap().action;
        assert_eq!(row.status, "error");
        assert!(row.error_message.as_deref().is_some_and(|m| m.contains("changed")), "{:?}", row.error_message);
    }
    /// #1510 AC5 — the card attaches the signed-page preview and states where
    /// the ink lands relative to the detected line, with the binding hashes.
    #[test]
    fn card_shows_the_line_the_preview_and_its_hashes() {
        let mut p: Proposal = serde_json::from_value(serde_json::json!({
            "source": "letter.pdf", "output": "letter-signed.pdf", "document_sha256": "d", "signature": "default",
            "signature_sha256": "s", "pages": 4, "expected_output_sha256": "o", "preview_sha256": "v",
            "placement": {"page": 4, "x": 70.1, "y": 707.1, "width": 187.0, "date": null,
                "ink": {"x": 72.0, "y": 709.98, "width": 180.0, "height": 20.4},
                "anchor": {"line": 1, "x0": 60.0, "x1": 552.0, "top": 708.48, "text": "Jane Signer"}}
        })).unwrap();
        let text = card_text(&p);
        for want in ["Page 4, on line 1 (top y=708.48", "above \"Jane Signer\"", "ink bottom y=709.98, 1.5 pt above the line",
                     "Preview SHA-256: v", "Signed output SHA-256: o", "The preview image is the signed page"] {
            assert!(text.contains(want), "missing {want:?} in {text}");
        }
        // Proposals made before #1510 still describe what they will draw.
        p.placement.ink = None;
        p.placement.anchor = None;
        p.preview_sha256 = None;
        assert!(card_text(&p).contains("image box x=70.1, y=707.1, width=187"), "{}", card_text(&p));
        let files = card_files(b"%PDF-".to_vec(), b"\x89PNG".to_vec());
        let names: Vec<_> = files.iter().map(|f| f.filename.as_str()).collect();
        assert_eq!(names, ["signed-page-preview.png", "unsigned-review.pdf"]);
        assert_eq!(files[0].data, b"\x89PNG");
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
