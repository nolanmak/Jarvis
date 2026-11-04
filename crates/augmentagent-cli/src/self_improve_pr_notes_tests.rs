//! Recovery notes must never replace owner requirements or closing references.
use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn optional_recovery_note_preserves_pr_source_and_issue_link() {
    const CHILD:&str="AUTOSHIP_PR_NOTE_FIXTURE";
    const ORIGINAL:&str="Resolve bare given names consistently.\n\nFixes #1668\n";
    if let Some(root)=std::env::var_os(CHILD) {
        let root=PathBuf::from(root);
        assert!(write_optional_pr_note(&root,"1669","Auto-resume: storage busy; retry admission").await);
        assert_eq!(std::fs::read_to_string(root.join("body")).unwrap(),ORIGINAL,
            "a machine-written recovery note must not become the PR's canonical task");
        let notes=std::fs::read_to_string(root.join("comments")).unwrap();
        assert!(notes.contains("storage busy; retry admission"));
        return;
    }
    let dir=tempfile::tempdir().unwrap();let root=dir.path();
    assert!(std::process::Command::new("git").args(["init","-q"]).current_dir(root).status().unwrap().success());
    std::fs::write(root.join("body"),ORIGINAL).unwrap();
    let gh=root.join("gh.py");
    std::fs::write(&gh,r#"#!/usr/bin/env python3
import os,pathlib,sys
root=pathlib.Path(os.environ['AUTOSHIP_PR_NOTE_FIXTURE']);args=sys.argv[1:]
assert args[:1]==['pr'] and args[2]=='1669'
body=args[args.index('--body')+1]
if args[1]=='edit':(root/'body').write_text(body)
elif args[1]=='comment':
 with (root/'comments').open('a') as f:f.write(body+'\n')
else:raise Exception('unexpected PR operation')
"#).unwrap();
    std::fs::set_permissions(&gh,std::fs::Permissions::from_mode(0o700)).unwrap();
    let result=std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact","self_improve::pr_notes_tests::optional_recovery_note_preserves_pr_source_and_issue_link","--nocapture"])
        .env(CHILD,root).env("GH_BIN",gh).output().unwrap();
    assert!(result.status.success(),"{}\n{}",String::from_utf8_lossy(&result.stdout),String::from_utf8_lossy(&result.stderr));
}
