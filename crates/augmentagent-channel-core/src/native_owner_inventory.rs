//! Derive an immutable manifest of the exact completed native export bytes.
use anyhow::Result;
use std::path::Path;
use serde_json::{Value,json};
pub(super) fn export_manifest(_root:&Path)->Result<Value> {Ok(json!({"version":1,"entries":[]}))}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt,symlink};
    fn fixture()->tempfile::TempDir {let root=tempfile::tempdir().unwrap();std::fs::set_permissions(root.path(),std::fs::Permissions::from_mode(0o700)).unwrap();root}
    fn file(path:&Path,bytes:&[u8]) {std::fs::write(path,bytes).unwrap();std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o600)).unwrap();}
    #[test]
    fn native_export_manifest_hashes_exact_bytes_and_includes_empty_directories() {
        use sha2::{Digest,Sha256};
        let root=fixture();let directory=root.path().join("empty");std::fs::create_dir(&directory).unwrap();std::fs::set_permissions(&directory,std::fs::Permissions::from_mode(0o700)).unwrap();
        file(&root.path().join("é\\literal"),b"original\0bytes");
        assert_eq!(export_manifest(root.path()).unwrap(),json!({"version":1,"entries":[{"kind":"directory","path":"empty"},{"kind":"file","path":"é\\literal","bytes":14,"sha256":format!("{:x}",Sha256::digest(b"original\0bytes"))}]}));
        file(&root.path().join("é\\literal"),b"changed");assert_eq!(export_manifest(root.path()).unwrap()["entries"][1]["bytes"],7);
    }
    #[test]
    fn native_export_manifest_refuses_symlinks_special_files_and_unprivate_content() {
        for case in ["symlink","fifo","public","hardlink","non-utf8"] {
            let root=fixture();let outside=fixture();let path=root.path().join("file");file(&outside.path().join("private"),b"outside");
            match case {
                "symlink"=>symlink(outside.path().join("private"),&path).unwrap(),
                "fifo"=>{use std::os::unix::ffi::OsStrExt;let name=std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0)},
                "public"=>{file(&path,b"data");std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o644)).unwrap()},
                "hardlink"=>std::fs::hard_link(outside.path().join("private"),&path).unwrap(),
                "non-utf8"=>{use std::os::unix::ffi::OsStringExt;file(&root.path().join(std::ffi::OsString::from_vec(vec![0xff])),b"data")},
                _=>unreachable!(),
            }
            assert!(export_manifest(root.path()).is_err(),"accepted {case}");
        }
    }
    #[test]
    fn native_export_manifest_refuses_byte_file_and_directory_overflow_without_truncating() {
        for case in ["bytes","files","directories"] {
            let root=fixture();
            match case {
                "bytes"=>{file(&root.path().join("big"),b"");std::fs::OpenOptions::new().write(true).open(root.path().join("big")).unwrap().set_len(64*1024*1024+1).unwrap()},
                "files"=>for index in 0..4097 {file(&root.path().join(format!("file-{index}")),b"")},
                "directories"=>for index in 0..2049 {let path=root.path().join(format!("dir-{index}"));std::fs::create_dir(&path).unwrap();std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap()},
                _=>unreachable!(),
            }
            assert!(export_manifest(root.path()).is_err(),"accepted {case} overflow");
        }
    }
}
