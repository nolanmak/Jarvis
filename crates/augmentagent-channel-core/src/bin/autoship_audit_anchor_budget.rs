use augmentagent_channel_core::audit_chain::AnchoredAudit;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn audit(root: &Path, name: &str) -> AnchoredAudit {
    let base = root.join(name);
    fs::create_dir_all(base.join("logs")).unwrap();
    fs::create_dir_all(base.join("anchors")).unwrap();
    fs::create_dir_all(base.join("work")).unwrap();
    AnchoredAudit::new(
        base.join("logs/run.jsonl"),
        base.join("anchors/run.json"),
        &[base.join("work")],
    )
    .unwrap()
}

fn evidence_paths(root: &Path, name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = root.join(name);
    (
        base.join("logs/run.jsonl"),
        base.join("anchors/run.json"),
        base.join("anchors"),
    )
}

fn main() {
    let root = std::env::temp_dir().join(format!(
        "autoship-audit-anchor-budget-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let append = audit(&root, "append");
    append.append(json!({"payload":"settled"})).unwrap();
    let (append_log, append_anchor, append_dir) = evidence_paths(&root, "append");
    let append_log_before = fs::read(&append_log).unwrap();
    let append_anchor_before = fs::read(&append_anchor).unwrap();
    let oversized_pending = "\\".repeat(22 * 1024 * 1024);
    let pending_refused = append.append(json!({"payload":oversized_pending})).is_err();
    let pending_preserved = fs::read(&append_log).unwrap() == append_log_before
        && fs::read(&append_anchor).unwrap() == append_anchor_before;
    let temporary_cleaned = fs::read_dir(&append_dir).unwrap().count() == 1;
    let later_append_works = append.append(json!({"payload":"smaller"})).is_ok();

    let seal = audit(&root, "seal");
    seal.append(json!({"payload":"settled"})).unwrap();
    let (seal_log, seal_anchor, _) = evidence_paths(&root, "seal");
    let seal_log_before = fs::read(&seal_log).unwrap();
    let seal_anchor_before = fs::read(&seal_anchor).unwrap();
    let oversized_validation = "v".repeat(64 * 1024 * 1024);
    let seal_refused = seal
        .seal_for_recovery(|_| Ok(json!({"validation":oversized_validation,"metadata":"m"})))
        .is_err();
    let seal_preserved = fs::read(&seal_log).unwrap() == seal_log_before
        && fs::read(&seal_anchor).unwrap() == seal_anchor_before;
    let smaller_seal_works = seal
        .seal_for_recovery(|_| Ok(json!({"smaller":true})))
        .map(|proof| proof["validation"]["smaller"] == json!(true))
        .unwrap_or(false);

    let observed = [
        pending_refused,
        pending_preserved,
        temporary_cleaned,
        later_append_works,
        seal_refused,
        seal_preserved,
        smaller_seal_works,
    ];
    let _ = fs::remove_dir_all(&root);
    println!("{}", serde_json::to_string(&observed).unwrap());
}
