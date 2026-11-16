//! Native tool-result progress for protected workflow controllers. This is not
//! model narration, execution permission, review approval or merge authority.
use super::*;

#[derive(Debug, Clone)]
pub struct CompletedToolStep {
    provider: String,
    semantic_sha256: String,
}
impl CompletedToolStep {
    pub fn provider(&self) -> &str {
        &self.provider
    }
    pub fn semantic_sha256(&self) -> &str {
        &self.semantic_sha256
    }
}
pub type Observer = dyn Fn(&CompletedToolStep) -> anyhow::Result<()> + Send + Sync;

pub(super) fn completed(
    provider: ProviderKind,
    tool: &str,
    args: &serde_json::Value,
    content: &str,
    is_error: bool,
) -> Option<CompletedToolStep> {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    let tool = tool.strip_prefix("mcp__jarvis__").unwrap_or(tool);
    let fields: &[&str] = match tool {
        "Bash" => &["command", "cmd", "cwd", "workdir"],
        "Read" => &["file_path", "path", "offset", "limit"],
        "Write" => &["file_path", "path", "content"],
        "Edit" => &[
            "file_path",
            "path",
            "old_string",
            "new_string",
            "replace_all",
        ],
        "Grep" => &[
            "pattern",
            "path",
            "glob",
            "type",
            "output_mode",
            "context",
            "head_limit",
            "offset",
            "-A",
            "-B",
            "-C",
            "-i",
            "-n",
            "multiline",
        ],
        "Glob" => &["pattern", "path"],
        _ => return None,
    };
    let parsed;
    let args = if let Some(raw) = args.as_str() {
        parsed = serde_json::from_str::<serde_json::Value>(raw).ok()?;
        &parsed
    } else {
        args
    };
    let object = args.as_object()?;
    let parameters: BTreeMap<_, _> = fields
        .iter()
        .filter_map(|key| object.get(*key).map(|value| (*key, value)))
        .collect();
    if parameters.is_empty() || is_claude_refusal(content) {
        return None;
    }
    let (exit, stdout, stderr) = if provider == ProviderKind::Codex && tool == "Bash" {
        let result: serde_json::Value = serde_json::from_str(content).ok()?;
        if result["runner"] != "vm"
            || result["interrupted"] == true
            || result["timed_out"] == true
            || result["output_complete"] == false
        {
            return None;
        }
        let exit = result["exit_code"].as_i64()?;
        if exit < 0 {
            return None;
        }
        (
            exit,
            result["stdout"].as_str()?.to_owned(),
            result["stderr"].as_str()?.to_owned(),
        )
    } else if matches!(provider, ProviderKind::Claude | ProviderKind::Codex) && !is_error {
        // Native SDK ToolEnd success; protected logger ownership/admission is
        // independently required before publishing this observation.
        (0, content.to_owned(), String::new())
    } else {
        return None;
    };
    let semantic_sha256 = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                "native-vm-tool-step-v1",
                tool,
                parameters,
                exit,
                stdout,
                stderr
            ))
            .ok()?
        )
    );
    Some(CompletedToolStep {
        provider: provider.name().into(),
        semantic_sha256,
    })
}

impl AuditLogger {
    pub fn set_native_tool_progress_observer(&self, observer: Arc<Observer>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.is_protected()
                && self.run_resources_required()
                && self.native_runtime_owner()?.is_some(),
            "native progress requires a protected owned resource scope"
        );
        *self
            .native_progress_observer
            .lock()
            .map_err(|_| anyhow::anyhow!("native progress observer lock poisoned"))? =
            Some(observer);
        Ok(())
    }

    pub(super) fn publish_native_progress(
        &self,
        step: Option<&CompletedToolStep>,
    ) -> anyhow::Result<()> {
        let Some(step) = step else { return Ok(()) };
        let observer = self
            .native_progress_observer
            .lock()
            .map_err(|_| anyhow::anyhow!("native progress observer lock poisoned"))?
            .clone();
        let Some(observer) = observer else {
            return Ok(());
        };
        anyhow::ensure!(
            self.is_protected()
                && self.run_resources_required()
                && self.native_runtime_owner()?.is_some(),
            "native progress lost its protected owner"
        );
        observer(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn fixture() -> (tempfile::TempDir, Arc<AuditLogger>) {
        let root = tempfile::tempdir().unwrap();
        for dir in ["logs", "anchors"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }
        let logger = Arc::new(
            AuditLogger::protected(
                root.path().join("logs/tool.jsonl"),
                root.path().join("anchors/tool.json"),
                &[],
            )
            .unwrap(),
        );
        logger.require_run_resources().unwrap();
        logger
            .set_native_runtime_owner(crate::native_vm::RuntimeOwner {
                workflow_id: "owner/repo#7".into(),
                lease_generation: 3,
            })
            .unwrap();
        (root, logger)
    }
    fn record(provider: ProviderKind, description: &str, body: &str) -> AuditRecord {
        build_audit_record(
            provider,
            "2026-11-15T00:00:00Z".into(),
            "session".into(),
            "Bash".into(),
            serde_json::json!({"command":"cargo test -p fixture","description":description,"timeout":120000}),
            body,
            false,
        )
    }
    fn vm(stdout: &str, elapsed: u64) -> String {
        serde_json::json!({"runner":"vm","exit_code":0,"stdout":stdout,"stderr":"","elapsed_ms":elapsed}).to_string()
    }

    #[tokio::test]
    async fn native_completed_result_is_published_after_its_protected_audit_row() {
        let (_root, logger) = fixture();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink = observed.clone();
        let path = logger.path().to_owned();
        logger
            .set_native_tool_progress_observer(Arc::new(move |step| {
                let audit = std::fs::read_to_string(&path)?;
                assert!(
                    audit.contains("cargo test -p fixture"),
                    "progress preceded durable tool evidence"
                );
                sink.lock().unwrap().push((
                    step.provider().to_owned(),
                    step.semantic_sha256().to_owned(),
                ));
                Ok(())
            }))
            .unwrap();
        logger
            .record_checked(&record(
                ProviderKind::Codex,
                "run tests",
                &vm("tests passed", 1),
            ))
            .await
            .unwrap();
        logger.drain_checked().await.unwrap();
        let rows = observed.lock().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "completed native command did not publish progress"
        );
        assert_eq!(rows[0].0, "codex");
        assert_eq!(rows[0].1.len(), 64);
    }

    #[tokio::test]
    async fn semantic_digest_ignores_descriptions_elapsed_time_and_session_metadata() {
        let (_root, logger) = fixture();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink = observed.clone();
        logger
            .set_native_tool_progress_observer(Arc::new(move |step| {
                sink.lock().unwrap().push(step.semantic_sha256().to_owned());
                Ok(())
            }))
            .unwrap();
        logger
            .record_checked(&record(
                ProviderKind::Codex,
                "first description",
                &vm("same output", 1),
            ))
            .await
            .unwrap();
        let mut next = record(
            ProviderKind::Codex,
            "changed description",
            &vm("same output", 999),
        );
        next.session_id = "another session".into();
        next.ts = "2026-11-16T00:00:00Z".into();
        logger.record_checked(&next).await.unwrap();
        logger
            .record_checked(&record(
                ProviderKind::Codex,
                "third",
                &vm("new compiler evidence", 1000),
            ))
            .await
            .unwrap();
        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 3);
        assert_eq!(
            observed[0], observed[1],
            "metadata changed semantic progress identity"
        );
        assert_ne!(observed[1], observed[2]);
    }

    #[tokio::test]
    async fn serialized_or_controller_narration_cannot_manufacture_completion() {
        let (_root, logger) = fixture();
        let count = Arc::new(Mutex::new(0));
        let sink = count.clone();
        logger
            .set_native_tool_progress_observer(Arc::new(move |_| {
                *sink.lock().unwrap() += 1;
                Ok(())
            }))
            .unwrap();
        let real = record(ProviderKind::Codex, "tests", &vm("ok", 1));
        let mut value = serde_json::to_value(&real).unwrap();
        value["native_step"] =
            serde_json::json!({"provider":"codex","semantic_sha256":"a".repeat(64)});
        let forged: AuditRecord = serde_json::from_value(value).unwrap();
        logger.record_checked(&forged).await.unwrap();
        logger.record_controller(serde_json::json!({"actor":"controller","kind":"native_step","semantic_sha256":"b".repeat(64)})).unwrap();
        assert_eq!(*count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn failed_audit_never_publishes_tool_progress() {
        let (_root, logger) = fixture();
        let count = Arc::new(Mutex::new(0));
        let sink = count.clone();
        logger
            .set_native_tool_progress_observer(Arc::new(move |_| {
                *sink.lock().unwrap() += 1;
                Ok(())
            }))
            .unwrap();
        // Immutable audit hash must reject a rewritten committed row.
        std::fs::write(logger.path(), b"tampered\n").unwrap();
        assert!(logger
            .record_checked(&record(ProviderKind::Codex, "tests", &vm("ok", 1)))
            .await
            .is_err());
        assert_eq!(*count.lock().unwrap(), 0);
    }

    #[test]
    fn unprotected_or_unowned_logger_cannot_enable_progress() {
        let root = tempfile::tempdir().unwrap();
        let plain = AuditLogger::new(root.path().join("tool.jsonl"));
        assert!(plain
            .set_native_tool_progress_observer(Arc::new(|_| Ok(())))
            .is_err());
        for dir in ["logs", "anchors"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }
        let protected = AuditLogger::protected(
            root.path().join("logs/tool.jsonl"),
            root.path().join("anchors/tool.json"),
            &[],
        )
        .unwrap();
        assert!(protected
            .set_native_tool_progress_observer(Arc::new(|_| Ok(())))
            .is_err());
    }

    #[tokio::test]
    async fn actual_codex_completion_adapter_preserves_native_progress() {
        let (root, logger) = fixture();
        let observed = Arc::new(Mutex::new(0));
        let sink = observed.clone();
        logger
            .set_native_tool_progress_observer(Arc::new(move |_| {
                *sink.lock().unwrap() += 1;
                Ok(())
            }))
            .unwrap();
        let mut opts = crate::reasoner::resume_opts(root.path().to_owned());
        opts.audit_logger = Some(logger.clone());
        crate::codex::record_tool_item(&opts,&serde_json::json!({"type":"mcp_tool_call","server":"jarvis","tool":"Bash",
            "arguments":{"command":"cargo test -p fixture"},"status":"completed","result":{"content":[{"type":"text","text":vm("passed",1)}]}})).await;
        logger.drain_checked().await.unwrap();
        assert_eq!(
            *observed.lock().unwrap(),
            1,
            "native Codex completion bypassed progress publication"
        );
    }
    #[tokio::test]
    async fn refused_host_or_interrupted_results_do_not_publish_progress() {
        let (_root, logger) = fixture();
        let count = Arc::new(Mutex::new(0));
        let sink = count.clone();
        logger
            .set_native_tool_progress_observer(Arc::new(move |_| {
                *sink.lock().unwrap() += 1;
                Ok(())
            }))
            .unwrap();
        for content in [
            serde_json::json!({"runner":"host","exit_code":0,"stdout":"ok","stderr":""}).to_string(),
            serde_json::json!({"runner":"none","exit_code":null,"stdout":"","stderr":"denied"}).to_string(),
            serde_json::json!({"runner":"vm","exit_code":124,"stdout":"partial","stderr":"","interrupted":true}).to_string(),
            serde_json::json!({"runner":"vm","exit_code":0,"stdout":"partial","stderr":"","output_complete":false}).to_string(),
            "the model says it ran tests".into(),
        ] {
            logger.record_checked(&record(ProviderKind::Codex,"refused",&content)).await.unwrap();
        }
        let refusal = build_audit_record(
            ProviderKind::Claude,
            "time".into(),
            "session".into(),
            "Bash".into(),
            serde_json::json!({"command":"cargo test"}),
            "Permission to use Bash was denied",
            true,
        );
        logger.record_checked(&refusal).await.unwrap();
        assert_eq!(*count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn protected_claude_native_tool_result_uses_the_same_progress_boundary() {
        let (_root, logger) = fixture();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        logger
            .set_native_tool_progress_observer(Arc::new(move |step| {
                sink.lock().unwrap().push(step.provider().to_owned());
                Ok(())
            }))
            .unwrap();
        logger
            .record_checked(&record(ProviderKind::Claude, "run tests", "tests passed\n"))
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["claude"],
            "completed native Claude tool did not publish progress"
        );
    }
    #[tokio::test]
    async fn failed_progress_publication_retains_audit_and_fails_closed() {
        let (_root, logger) = fixture();
        logger
            .set_native_tool_progress_observer(Arc::new(|_| {
                Err(anyhow::anyhow!("progress store unavailable"))
            }))
            .unwrap();
        assert!(logger
            .record_checked(&record(ProviderKind::Codex, "tests", &vm("ok", 1)))
            .await
            .is_err());
        assert!(std::fs::read_to_string(logger.path())
            .unwrap()
            .contains("cargo test -p fixture"));
        assert!(
            logger.drain_checked().await.is_err(),
            "lost progress publication was reported healthy"
        );
    }
}
