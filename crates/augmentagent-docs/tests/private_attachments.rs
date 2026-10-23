use augmentagent_docs::inbound::write_private_attachment;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

#[tokio::test]
async fn attachment_replacement_is_private_and_does_not_follow_existing_links() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::write(&outside, b"original").unwrap();
    let target = dir.path().join("attachment");
    for hard_link in [false, true] {
        if hard_link {
            std::fs::hard_link(&outside, &target).unwrap();
        } else {
            symlink(&outside, &target).unwrap();
        }
        write_private_attachment(&target, b"private bytes").await.unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"original");
        assert_eq!(std::fs::read(&target).unwrap(), b"private bytes");
        let metadata = std::fs::symlink_metadata(&target).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        std::fs::remove_file(&target).unwrap();
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn failed_attachment_publication_leaves_no_partial_file_or_tempfile() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("directory");
    std::fs::create_dir(&target).unwrap();
    assert!(write_private_attachment(&target, b"private bytes").await.is_err());
    assert!(target.is_dir());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
