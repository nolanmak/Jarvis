//! S activity writes from the existing protected workflow controller.
use super::*;

impl WorkflowGuard {
    pub(super) fn install_native_tool_progress(&self) -> Result<()> {
        // Merge/recovery-only controllers have no native model owner and must
        // not manufacture a model-progress authority.
        let Some(owner) = self.audit.native_runtime_owner()? else {
            return Ok(());
        };
        anyhow::ensure!(
            owner.workflow_id == self.lease.workflow
                && owner.lease_generation == self.lease.generation,
            "native progress owner does not match workflow lease"
        );
        let store = self.store.clone();
        let lease = self.lease.clone();
        self.audit
            .set_native_tool_progress_observer(Arc::new(move |step| {
                store
                    .lock()
                    .map_err(|_| anyhow::anyhow!("native workflow progress lock poisoned"))?
                    .record_native_tool_progress(
                        &lease,
                        step.provider(),
                        step.semantic_sha256(),
                        millis(),
                    )?;
                Ok(())
            }))
    }

    pub(in crate::self_improve) fn record_guarded_progress(
        &self,
        admission: &str,
        semantic_sha256: &str,
    ) -> Result<()> {
        self.healthy_lease()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow progress lock poisoned"))?
            .record_guarded_verifier_progress(
                &self.lease,
                &self.binding,
                admission,
                semantic_sha256,
                millis(),
            )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn protected_native_tool_updates_its_workflow_after_model_admission() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let repo = root.path().join("repo");
        let state = root.path().join("state");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&state).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Fixture",
        ]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let mut guard = WorkflowGuard::acquire_fixture(
            &state.join("workflow.db"),
            Start {
                repo_root: &repo,
                worktree: &repo,
                repository: "owner/repo",
                source_issue: Some(7),
                pr: None,
                branch: "fixture",
                contract: "fixture",
                environment: "fixture",
            },
        )
        .await
        .unwrap()
        .unwrap();
        guard
            .checkpoint(Stage::Scope, "scope_started", "scope", 0, &[])
            .await
            .unwrap();
        // Scripted provider admission at the real store boundary; no inference claim.
        guard
            .store
            .lock()
            .unwrap()
            .charge_admitted_model(
                &guard.lease,
                "scripted-native-call",
                millis(),
                None,
                None,
                Some(durable::submission::Provider::Codex),
                false,
            )
            .unwrap();
        let audit = guard.audit_logger();
        let record = augmentagent_channel_core::tool_audit::build_audit_record(
            augmentagent_channel_core::ProviderKind::Codex,
            "time".into(),
            "session".into(),
            "Bash".into(),
            serde_json::json!({"command":"cargo test -p fixture"}),
            &serde_json::json!({"runner":"vm","exit_code":0,"stdout":"passed","stderr":""})
                .to_string(),
            false,
        );
        audit.record_checked(&record).await.unwrap();
        audit.drain_checked().await.unwrap();
        let snapshot = guard
            .store
            .lock()
            .unwrap()
            .activity_status(guard.id())
            .unwrap()
            .unwrap();
        assert!(
            snapshot.last_meaningful_progress_at > 0,
            "workflow guard did not install native tool progress observer"
        );
        assert_eq!(snapshot.reason_code, "native_tool_completed");
        assert_eq!(
            snapshot.inactivity_deadline_at,
            Some(snapshot.last_meaningful_progress_at + 900_000)
        );
        let usage = guard
            .store
            .lock()
            .unwrap()
            .snapshot(guard.id())
            .unwrap()
            .unwrap();
        audit.record_checked(&record).await.unwrap();
        audit.drain_checked().await.unwrap();
        assert_eq!(
            guard
                .store
                .lock()
                .unwrap()
                .activity_status(guard.id())
                .unwrap()
                .unwrap(),
            snapshot
        );
        let after = guard
            .store
            .lock()
            .unwrap()
            .snapshot(guard.id())
            .unwrap()
            .unwrap();
        assert_eq!(after.model_calls, usage.model_calls);
        assert_eq!(after.tokens, usage.tokens);
        guard.finish().unwrap();
    }
}
