//! Real Git/controller fixtures, synthetic aliases, and no network access.
use super::*;
use sha2::{Digest, Sha256};

const OLD: &str = "https://github.com/fixture-owner/Before.git";
const NEW: &str = "https://github.com/Fixture-Owner/CURRENT";

fn git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

async fn fixture(test: &str, case: &str) {
    const CHILD: &str = "AUTOSHIP_ORIGIN_FIXTURE";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let case = std::env::var("AUTOSHIP_ORIGIN_CASE").unwrap();
        if case.starts_with("publish-") {
            let remote=root.parent().unwrap().join("remote.git");
            git(&root,&["init","--bare","-q",remote.to_str().unwrap()]);
            git(&root,&["remote","set-url","--push","origin",remote.to_str().unwrap()]);
            let before=git(&root,&["--git-dir",remote.to_str().unwrap(),"for-each-ref"]);
            if case=="publish-no-alias" {
                assert!(publish_smoke::probe(&root,"fixture-owner/current").await.is_err());
                assert_eq!(git(&root,&["--git-dir",remote.to_str().unwrap(),"for-each-ref"]),before);
                return;
            }
            publish_smoke::probe(&root,"fixture-owner/current").await.unwrap();
            assert_eq!(git(&root,&["--git-dir",remote.to_str().unwrap(),"for-each-ref"]),before,"smoke published a ref");
            let marker=root.parent().unwrap().join("pre-push-observed");
            augmentagent_test_support::write_executable(&root.join(".git/hooks/pre-push"),
                &format!("#!/bin/sh\nprintf observed > '{}'\nexit 1\n",marker.display())).unwrap();
            assert!(publish_smoke::probe(&root,"fixture-owner/current").await.is_err(),"original push admission was bypassed");
            assert!(marker.exists(),"real pre-push admission was not exercised");
            assert_eq!(git(&root,&["--git-dir",remote.to_str().unwrap(),"for-each-ref"]),before);
            git(&root,&["remote","set-url","origin","https://github.com/fixture-owner/unrelated.git"]);
            assert!(publish_smoke::probe(&root,"fixture-owner/current").await.is_err(),"foreign alias accepted");
            return;
        }
        if case == "push" {
            let resolved = push_destination::Resolved::resolve(&root, "fixture-owner/current")
                .await
                .unwrap();
            let hook = resolved.hook(&root, root.parent().unwrap()).await.unwrap();
            let script =
                Path::new(hook.config().strip_prefix("core.hooksPath=").unwrap()).join("pre-push");
            assert!(std::process::Command::new("/bin/sh")
                .arg(&script)
                .args(["origin", OLD])
                .status()
                .unwrap()
                .success());
            assert!(
                !std::process::Command::new("/bin/sh")
                    .arg(&script)
                    .args(["origin", NEW])
                    .status()
                    .unwrap()
                    .success(),
                "alias policy weakened exact transport fencing"
            );
            assert!(
                push_destination::Resolved::resolve(&root, "fixture-owner/current")
                    .await
                    .is_ok(),
                "declared renamed repository was refused by push admission"
            );
            git(&root, &["remote", "set-url", "origin", NEW]);
            assert!(
                push_destination::Resolved::resolve(&root, "fixture-owner/current")
                    .await
                    .is_ok()
            );
            git(
                &root,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "https://github.com/fixture-owner/unrelated.git",
                ],
            );
            assert!(
                push_destination::Resolved::resolve(&root, "fixture-owner/current")
                    .await
                    .is_err()
            );
            return;
        }
        let controller = workspace::prepare(&root, "build").await.unwrap();
        let checkout = controller.root.clone();
        drop(controller);
        let receipt = root.join(".git/augmentagent-controllers/build.json");
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
        // Real v1 shape: origin bytes were hashed, without a canonical name.
        legacy["version"] = 1.into();
        legacy["origin_sha256"] = format!("{:x}", Sha256::digest(OLD.as_bytes())).into();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("previous_origin_sha256");
        if case == "unrecognized" {
            legacy["origin_sha256"] = format!(
                "{:x}",
                Sha256::digest(b"https://github.com/fixture-owner/unlisted.git")
            )
            .into();
        }
        if case == "physical" {
            legacy["inode"] = (legacy["inode"].as_u64().unwrap() + 1).into();
        }
        if case == "late-alias" {
            legacy["version"] = 2.into();
            legacy["origin_sha256"] =
                format!("{:x}", Sha256::digest(b"github:fixture-owner/before")).into();
            legacy["previous_origin_sha256"] =
                format!("{:x}", Sha256::digest(OLD.as_bytes())).into();
        }
        std::fs::write(&receipt, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let pointer = std::fs::read(checkout.join(".git")).unwrap();
        git(&root, &["remote", "set-url", "origin", NEW]);
        if matches!(case.as_str(), "dirty" | "unrecognized" | "physical") {
            if case == "dirty" {
                std::fs::write(checkout.join("code"), "synthetic unfinished work").unwrap();
            }
            let before = std::fs::read(&receipt).unwrap();
            assert!(
                workspace::prepare(&root, "build").await.is_err(),
                "unproven or dirty controller migrated"
            );
            assert_eq!(std::fs::read(&receipt).unwrap(), before);
            assert_eq!(std::fs::read(checkout.join(".git")).unwrap(), pointer);
            return;
        }
        let resumed = workspace::prepare(&root, "build").await;
        assert!(
            resumed.is_ok(),
            "same repository rename invalidated legacy ownership: {}",
            resumed.err().map(|e| format!("{e:#}")).unwrap_or_default()
        );
        drop(resumed);
        let migrated: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
        assert_eq!(migrated["version"], 2);
        if case == "late-alias" {
            assert_eq!(
                migrated["previous_identity_sha256"],
                legacy["origin_sha256"]
            );
            assert_eq!(
                migrated["previous_origin_sha256"],
                legacy["previous_origin_sha256"]
            );
        } else {
            assert_eq!(migrated["previous_origin_sha256"], legacy["origin_sha256"]);
        }
        assert_eq!(std::fs::read(checkout.join(".git")).unwrap(), pointer);
        // Stable after migration, including case, transport and .git spelling.
        for url in [
            "git@github.com:FIXTURE-OWNER/current.git",
            "https://github.com/fixture-owner/current.GIT",
        ] {
            git(&root, &["remote", "set-url", "origin", url]);
            drop(workspace::prepare(&root, "build").await.unwrap());
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(&receipt).unwrap())
                    .unwrap(),
                migrated
            );
        }
        git(
            &root,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/fixture-owner/unrelated.git",
            ],
        );
        let before = std::fs::read(&receipt).unwrap();
        assert!(
            workspace::prepare(&root, "build").await.is_err(),
            "foreign repository adopted the owned controller"
        );
        assert_eq!(std::fs::read(&receipt).unwrap(), before);
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("code"), "synthetic baseline\n").unwrap();
    git(&repo, &["add", "code"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ],
    );
    git(&repo, &["remote", "add", "origin", OLD]);
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    // Execute all real Git operations; only fetch uses the local fixture repo.
    augmentagent_test_support::write_executable(
        &bin.join("git"),
        r#"#!/usr/bin/python3
import os,sys
args=sys.argv[1:]
if 'fetch' in args:
    args[args.index('origin')]=os.environ['AUTOSHIP_ORIGIN_FIXTURE']
os.execv('/usr/bin/git',['git',*args])
"#,
    )
    .unwrap();
    let out = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", &root)
        .env(
            "AUGMENTAGENT_REPOSITORY_ALIASES",
            if case=="publish-no-alias" {"{}".into()}else{serde_json::json!({"fixture-owner/current":[OLD]}).to_string()},
        )
        .env("AUTOSHIP_ORIGIN_CASE", case)
        .env(CHILD, &repo)
        .current_dir(&repo)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn renamed_origin_push_uses_declared_canonical_repository_offline() {
    fixture("self_improve::repository_identity_tests::renamed_origin_push_uses_declared_canonical_repository_offline", "push").await;
}

#[tokio::test]
async fn renamed_origin_controller_migrates_legacy_receipt_without_adopting_foreign_repo() {
    fixture("self_improve::repository_identity_tests::renamed_origin_controller_migrates_legacy_receipt_without_adopting_foreign_repo", "controller").await;
}

#[tokio::test]
async fn renamed_origin_migration_preserves_unproven_and_dirty_receipts() {
    if std::env::var_os("AUTOSHIP_ORIGIN_FIXTURE").is_some() {
        fixture("self_improve::repository_identity_tests::renamed_origin_migration_preserves_unproven_and_dirty_receipts","child").await;
        return;
    }
    for case in ["dirty", "unrecognized", "physical"] {
        fixture("self_improve::repository_identity_tests::renamed_origin_migration_preserves_unproven_and_dirty_receipts",case).await;
    }
}

#[tokio::test]
async fn renamed_origin_alias_added_after_initial_v2_migration_is_safe() {
    fixture("self_improve::repository_identity_tests::renamed_origin_alias_added_after_initial_v2_migration_is_safe","late-alias").await;
}

#[tokio::test]
async fn renamed_origin_publish_smoke_checks_real_push_admission_without_updating_refs() {
    const TEST:&str="self_improve::repository_identity_tests::renamed_origin_publish_smoke_checks_real_push_admission_without_updating_refs";
    if std::env::var_os("AUTOSHIP_ORIGIN_FIXTURE").is_some() {fixture(TEST,"child").await;return;}
    for case in ["publish-smoke","publish-no-alias"] {fixture(TEST,case).await;}
}
