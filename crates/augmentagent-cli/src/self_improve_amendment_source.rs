//! Immutable source data for correction construction, never witness approval.
use super::*;

#[derive(Debug, Serialize)]
pub(super) struct Evidence {
    original_base: String,
    selection_identity: String,
    sources: BTreeMap<String, String>,
}

pub(super) async fn read(
    _root: &Path, _control: &Path, base: &str,
    _custody: &[crate::autoship_workflow::NativeScopeCustody], _context: Option<&str>,
) -> Result<Evidence> {
    Ok(Evidence { original_base: base.into(), selection_identity: String::new(), sources: BTreeMap::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    async fn fixture() -> (tempfile::TempDir, tempfile::TempDir, String, Vec<crate::autoship_workflow::NativeScopeCustody>) {
        let root = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("behavior.rs"), "pub fn behavior() -> bool { false }\n").unwrap();
        for args in [&["init", "-q"][..], &["add", "."], &["commit", "-qm", "original"]] {
            let out = Command::new("/usr/bin/git").args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
                .args(args).current_dir(root.path()).env_clear().env("PATH", "/usr/bin:/bin").output().await.unwrap();
            assert!(out.status.success());
        }
        let head = actual_head(root.path()).await.unwrap();
        let response = r#"{"requests":[{"file_index":0,"start_line":1,"end_line":1,"needle":null}]}"#;
        let input = digest(b"admitted original source selection");
        let proof = serde_json::to_vec(&serde_json::json!({
            "version":1,"head":head,"provider":"codex","native_session":"source-fixture",
            "supplied_request_hash":digest(b"controller original request"),"response_hash":digest(response.as_bytes()),
            "purpose":"source_selection","scope_context_hash":"a".repeat(64),"native_input_hash":input,
            "scope_request_hash":digest(b"original qualification"),"scope_policy_hash":scope_authority::witness_policy_hash()
        })).unwrap();
        let artifact = control.path().join(format!("primary-scope-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&artifact, &proof).unwrap();
        std::fs::set_permissions(&artifact, std::fs::Permissions::from_mode(0o600)).unwrap();
        let observation = crate::autoship_workflow::NativeScopeCustody {
            input, response:response.into(),
            provenance:serde_json::to_string(&scope_authority::NativeReplyReference {artifact, artifact_sha256:digest(&proof)}).unwrap(),
            provider:crate::autoship_workflow::submission::Provider::Codex,
        };
        (root, control, head, vec![observation])
    }

    #[tokio::test]
    async fn amendment_source_replays_original_git_bytes_without_reading_dirty_candidate() {
        let (root, control, base, custody) = fixture().await;
        std::fs::write(root.path().join("behavior.rs"), "private uncommitted candidate bytes").unwrap();
        let evidence = read(root.path(), control.path(), &base, &custody, Some(&"a".repeat(64))).await.unwrap();
        let supplied = serde_json::to_string(&evidence).unwrap();
        assert!(supplied.contains("pub fn behavior() -> bool { false }"), "amendment lacks original production source");
        assert!(!supplied.contains("private uncommitted"));
        assert_eq!(evidence.original_base, base);
        assert_eq!(evidence.selection_identity.len(), 64);
    }

    #[tokio::test]
    async fn amendment_source_refuses_wrong_context_namespace_or_tampered_native_evidence() {
        let (root, control, base, mut custody) = fixture().await;
        assert!(read(root.path(), control.path(), &base, &custody, Some(&"b".repeat(64))).await.is_err());
        let foreign = tempfile::tempdir().unwrap();
        assert!(read(root.path(), foreign.path(), &base, &custody, Some(&"a".repeat(64))).await.is_err());
        custody[0].response.push(' ');
        assert!(read(root.path(), control.path(), &base, &custody, Some(&"a".repeat(64))).await.is_err());
    }
}
