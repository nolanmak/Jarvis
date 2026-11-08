//! Descriptive launch references, removable only with their qualified retired
//! runtime under the caller's original StateLock and exclusive session lock.
use super::*;

pub(super) struct LinkedLaunch {
    path:PathBuf,device:u64,inode:u64,spec_sha256:String,
}
fn private_directory(info:&std::fs::Metadata)->bool {
    info.is_dir() && !info.file_type().is_symlink()
        && info.uid()==unsafe {libc::geteuid()} && info.mode()&0o077==0
}
/// Unknown/partial/foreign entries are retained. A missing session is not a
/// retirement proof, so its launch reference is never consumed by pruning.
pub(super) fn collect(provider:&Path)->Result<BTreeMap<PathBuf,Vec<LinkedLaunch>>> {
    let mut linked=BTreeMap::<PathBuf,Vec<LinkedLaunch>>::new();
    for (index,entry) in std::fs::read_dir(provider)?.enumerate() {
        ensure!(index<8192,ownership::TransferReason::StateCapacity);
        let path=entry?.path();
        let Some(name)=path.file_name().and_then(|n|n.to_str()) else {continue;};
        if !name.starts_with("launch-") {continue;}
        let info=path.symlink_metadata()?;
        if !private_directory(&info) {continue;}
        let Ok(bytes)=private(&path.join("launch.json")) else {continue;};
        let Ok(spec)=serde_json::from_slice::<serde_json::Value>(&bytes) else {continue;};
        if spec["version"]!=1 || spec["provider"].as_str()!=provider.file_name().and_then(|n|n.to_str()) {continue;}
        let Some(session)=spec["session_directory"].as_str().map(PathBuf::from) else {continue;};
        if session.parent()!=Some(provider) || !session.file_name().and_then(|n|n.to_str()).is_some_and(|n|
            n.starts_with("session-") && n.len()>8 && n.len()<=256
                && n.bytes().all(|c|c.is_ascii_alphanumeric() || c==b'-')) {continue;}
        linked.entry(session).or_default().push(LinkedLaunch {path,device:info.dev(),inode:info.ino(),spec_sha256:digest(&bytes)});
    }
    Ok(linked)
}
/// Caller already verified this exact runtime's retirement and owns its lock.
/// Recheck captured directory/spec identity before removing each reference;
/// a changed entry refuses before the runtime itself can be removed.
pub(super) fn remove(linked:Vec<LinkedLaunch>)->Result<()> {
    for launch in linked {
        let info=launch.path.symlink_metadata()?;
        ensure!(private_directory(&info) && info.dev()==launch.device && info.ino()==launch.inode
            && digest(private(&launch.path.join("launch.json"))?)==launch.spec_sha256,
            "retired runtime launch reference changed before pruning");
        std::fs::remove_dir_all(&launch.path)?;
    }
    Ok(())
}
