//! Read the pinned original environment without checking out or running code.
use super::*;

fn git_bytes(root:&Path,args:&[&str])->Result<Vec<u8>> {
    let out=std::process::Command::new("/usr/bin/git").args(["--no-replace-objects","-c","core.hooksPath=/dev/null"])
        .args(args).current_dir(root).env_clear().env("PATH","/usr/bin:/bin").stdin(Stdio::null()).output()?;
    ensure!(out.status.success(),"pinned original compiler inputs are unavailable");
    ensure!(out.stdout.len()<=32*1024*1024,"original compiler input inventory exceeds inspection bound");
    Ok(out.stdout)
}
fn excluded(path:&str)->bool {
    path.split('/').any(|part|[".git","target","node_modules",".venv",".self-improve-worktrees"].contains(&part))
}
fn compiler_control(path:&str)->bool {
    let path=Path::new(path);
    path.file_name().is_some_and(|name|name=="Cargo.toml") || path.parent().and_then(|parent|parent.file_name()).is_some_and(|name|name==".cargo")
        && path.file_name().is_some_and(|name|name=="config" || name=="config.toml")
}
fn root_control(path:&str)->bool {["Cargo.lock","rust-toolchain","rust-toolchain.toml"].contains(&path)}

pub(super) fn original_environment(root:&Path,base:&str,files:&BTreeMap<String,String>)->Result<String> {
    ensure!(matches!(base.len(),40|64) && base.bytes().all(|byte|byte.is_ascii_hexdigit()),"original environment requires an immutable commit");
    let inventory=git_bytes(root,&["ls-tree","-rztl","--full-tree",base])?;
    let mut entries=BTreeMap::new();
    for (count,record) in inventory.split(|byte|*byte==0).filter(|record|!record.is_empty()).enumerate() {
        ensure!(count<100000,"original compiler input scan exceeds limit");
        let record=std::str::from_utf8(record)?;
        let (header,path)=record.split_once('\t').context("original tree entry has no path")?;
        let header=header.split_whitespace().collect::<Vec<_>>();ensure!(header.len()==4,"original tree metadata changed");
        ensure!(Path::new(path).components().all(|part|matches!(part,std::path::Component::Normal(_))),"original tree path is not relative");
        entries.insert(path.to_owned(),(header[0].to_owned(),header[1].to_owned(),header[2].to_owned(),header[3].to_owned()));
    }
    let mut contents=BTreeMap::new();
    for (path,(mode,kind,oid,size)) in &entries {
        if excluded(path) || !(compiler_control(path) || root_control(path)) {continue;}
        if !root_control(path) && ["tree","commit"].contains(&kind.as_str()) {continue;}
        ensure!(kind=="blob" && ["100644","100755"].contains(&mode.as_str()),"original compiler control is not a regular file");
        ensure!(size.parse::<usize>()?<=16*1024*1024,"original compiler control exceeds inspection bound");
        contents.insert(path.clone(),git_bytes(root,&["cat-file","blob",oid])?);
    }
    for (path,body) in files {
        ensure!(!path.is_empty() && Path::new(path).components().all(|part|matches!(part,std::path::Component::Normal(_))),"compiler overlay path is not relative");
        if excluded(path) || !(compiler_control(path) || root_control(path)) {continue;}
        let mut parent=Path::new(path).parent();
        while let Some(directory)=parent {
            if let Some((mode,kind,_,_))=entries.get(&directory.to_string_lossy().to_string()) {
                ensure!(["tree","commit"].contains(&kind.as_str()) && mode!="120000","compiler overlay crosses a non-directory");
            }
            parent=directory.parent();
        }
        if let Some((mode,kind,_,_))=entries.get(path) {ensure!(kind=="blob" && mode!="120000","compiler overlay replaces a non-file");}
        contents.insert(path.clone(),body.as_bytes().to_vec());
    }
    let controls=contents.iter().filter(|(path,_)|compiler_control(path)).map(|(path,bytes)|(path.clone(),amendment::digest(bytes))).collect::<Vec<_>>();
    environment_id_from_inputs(&controls,Some(verifier_checkout::EXPORTER),|name|Ok(contents.get(name).cloned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(root:&Path,args:&[&str])->String {
        let out=std::process::Command::new("/usr/bin/git").args(["-c","core.hooksPath=/dev/null"])
            .args(args).current_dir(root).env_clear().env("PATH","/usr/bin:/bin").output().unwrap();
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));String::from_utf8(out.stdout).unwrap().trim().into()
    }

    #[test]
    fn original_environment_matches_installed_controls_and_ignores_candidate_edits() {
        let root=tempfile::tempdir_in("/tmp").unwrap();let root=root.path();
        git(root,&["init","-q"]);
        for path in [".cargo","crates/a/.cargo","target/ignored","node_modules/ignored"] {std::fs::create_dir_all(root.join(path)).unwrap();}
        let original:BTreeMap<String,String>=BTreeMap::from([
            ("Cargo.toml".into(),"[workspace]\nmembers = []\n".into()),
            (".cargo/config.toml".into(),"[build]\njobs=1\n".into()),
            ("crates/a/Cargo.toml".into(),"[package]\nname=\"a\"\nversion=\"0.1.0\"\n".into()),
            ("crates/a/.cargo/config".into(),"[net]\noffline=true\n".into()),
            ("rust-toolchain.toml".into(),"[toolchain]\nchannel=\"stable\"\n".into()),
            ("target/ignored/Cargo.toml".into(),"ignored target\n".into()),
            ("node_modules/ignored/Cargo.toml".into(),"ignored node module\n".into()),
        ]);
        for (path,body) in &original {std::fs::write(root.join(path),body).unwrap();}
        std::fs::write(root.join("Cargo.lock"),b"raw lock bytes\xff\x00\n").unwrap();
        git(root,&["add","."]);git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","original controls"]);
        let base=git(root,&["rev-parse","HEAD"]);let before=environment_id(root).unwrap();
        assert_eq!(original_environment(root,&base,&BTreeMap::new()).unwrap(),before);
        let overlay=BTreeMap::from([(".cargo/config.toml".into(),"[build]\njobs=2\n".into()),("rust-toolchain".into(),"stable\n".into())]);
        for (path,body) in &overlay {std::fs::write(root.join(path),body).unwrap();}
        let installed=environment_id(root).unwrap();assert_ne!(installed,before);
        assert_eq!(original_environment(root,&base,&overlay).unwrap(),installed,"proposal compiler overlays were omitted");
        std::fs::write(root.join("Cargo.toml"),"candidate compiler edits\n").unwrap();
        std::fs::write(root.join("Cargo.lock"),"candidate lock\n").unwrap();
        assert_ne!(environment_id(root).unwrap(),installed);
        let status=git(root,&["status","--porcelain"]);
        assert_eq!(original_environment(root,&base,&overlay).unwrap(),installed,"candidate controls replaced pinned original inputs");
        assert_eq!(git(root,&["status","--porcelain"]),status,"inspection changed the candidate");
    }

    #[test]
    fn original_environment_ignores_local_git_replacements() {
        let root=tempfile::tempdir_in("/tmp").unwrap();let root=root.path();git(root,&["init","-q"]);
        std::fs::write(root.join("Cargo.lock"),"original lock\n").unwrap();git(root,&["add","."]);
        git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","original"]);
        let base=git(root,&["rev-parse","HEAD"]);let expected=environment_id(root).unwrap();
        std::fs::write(root.join("Cargo.lock"),"replacement lock\n").unwrap();git(root,&["add","."]);
        git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","replacement"]);
        let replacement=git(root,&["rev-parse","HEAD"]);git(root,&["replace",&base,&replacement]);
        assert_ne!(environment_id(root).unwrap(),expected);
        assert_eq!(original_environment(root,&base,&BTreeMap::new()).unwrap(),expected,"local replacement changed frozen original controls");
    }

    #[cfg(unix)]
    #[test]
    fn original_environment_rejects_symlink_controls_and_overlay_parents() {
        for direct in [false,true] {
            let root=tempfile::tempdir_in("/tmp").unwrap();let root=root.path();git(root,&["init","-q"]);
            std::fs::create_dir(root.join("actual")).unwrap();
            std::fs::write(root.join("actual/config.toml"),"[build]\njobs=1\n").unwrap();
            std::os::unix::fs::symlink(if direct {"actual/config.toml"}else{"actual"},root.join(if direct {"Cargo.toml"}else{".cargo"})).unwrap();
            git(root,&["add","."]);git(root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","symlink inputs"]);
            let base=git(root,&["rev-parse","HEAD"]);
            let files=if direct {BTreeMap::new()}else{BTreeMap::from([(".cargo/config.toml".into(),"[build]\njobs=2\n".into())])};
            assert!(original_environment(root,&base,&files).is_err(),"symlink compiler control admitted: direct={direct}");
            assert!(original_environment(root,"HEAD",&files).is_err(),"mutable source ref admitted");
        }
    }
}
