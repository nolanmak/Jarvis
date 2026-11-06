//! Live identities for controller-created empty literal files. Saved metadata
//! cannot reconstruct these retained handles or authorize native execution.
use super::*;
use std::collections::BTreeMap;
#[derive(Debug,PartialEq,Eq,Serialize)]
struct Identity {device:u64,inode:u64,uid:u32,mode:u32,len:u64,mtime:i64,mtime_nsec:i64,ctime:i64,ctime_nsec:i64}
impl Identity {
    fn of(m:&fs::Metadata)->Self {Self {device:m.dev(),inode:m.ino(),uid:m.uid(),mode:m.mode(),len:m.len(),mtime:m.mtime(),mtime_nsec:m.mtime_nsec(),ctime:m.ctime(),ctime_nsec:m.ctime_nsec()}}
}
#[derive(Debug)]
struct Created {relative:String,file:fs::File,original:Identity}
#[derive(Debug)]
pub(crate) struct ScopeMaterialization {reference:ScopeReference,files:Vec<Created>,directories:BTreeMap<PathBuf,fs::File>}
impl ScopeMaterialization {
    pub(super) fn new(reference:ScopeReference)->Self {Self {reference,files:vec![],directories:BTreeMap::new()}}
    pub(super) fn create(&mut self,relative:&str)->Result<()> {
        ensure!(self.files.len()<256,"scope setup file identity limit exceeded");
        super::relative(relative)?;
        let mut parent=self.reference.worktree.clone();
        self.retain_directory(&parent)?;
        for component in Path::new(relative).parent().unwrap_or(Path::new("")).components() {
            parent.push(component);self.retain_directory(&parent)?;
        }
        let path=self.reference.worktree.join(relative);
        let creator=fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&path)?;
        creator.sync_all()?;self.directories.get(&parent).context("scope setup parent handle missing")?.sync_all()?;
        let original=Identity::of(&creator.metadata()?);
        let file=fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&path)?;
        ensure!(Identity::of(&file.metadata()?)==original,"scope setup file changed while retaining read identity");
        drop(creator);
        self.files.push(Created {relative:relative.into(),file,original});self.validate_current()
    }
    fn retain_directory(&mut self,path:&Path)->Result<()> {
        if !self.directories.contains_key(path) {
            ensure!(self.directories.len()<512,"scope setup directory identity limit exceeded");
            let file=fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
            self.directories.insert(path.into(),file);
        }
        self.validate_directory(path,self.directories.get(path).unwrap())
    }
    fn validate_directory(&self,path:&Path,file:&fs::File)->Result<()> {
        let held=file.metadata()?;let actual=fs::symlink_metadata(path)?;
        ensure!(held.is_dir() && actual.is_dir() && !actual.file_type().is_symlink()
            && held.dev()==actual.dev() && held.ino()==actual.ino()
            && held.uid()==unsafe{libc::geteuid()} && actual.uid()==held.uid(),"scope setup directory identity changed");Ok(())
    }
    pub(crate) fn validate_current(&self)->Result<()> {
        for (path,file) in &self.directories {self.validate_directory(path,file)?;}
        for created in &self.files {
            let held=created.file.metadata()?;let actual=fs::symlink_metadata(self.reference.worktree.join(&created.relative))?;
            ensure!(held.is_file() && actual.is_file() && !actual.file_type().is_symlink() && held.nlink()==1 && actual.nlink()==1
                && held.uid()==unsafe{libc::geteuid()} && held.len()==0 && held.mode()&0o077==0
                && Identity::of(&held)==created.original && Identity::of(&actual)==created.original,
                "scope setup file was replaced, edited or shared");
        }
        Ok(())
    }
    pub(crate) fn reference(&self)->&ScopeReference {&self.reference}
    pub(crate) fn paths(&self)->impl Iterator<Item=&str> {self.files.iter().map(|f|f.relative.as_str())}
    pub(crate) fn handle_count(&self)->usize {self.files.len()+self.directories.len()}
    pub(crate) fn digest(&self)->Result<String> {
        let files=self.files.iter().map(|f|serde_json::json!({"path":f.relative,"identity":f.original})).collect::<Vec<_>>();
        let directories=self.directories.iter().map(|(path,file)| {
            let m=file.metadata()?;Ok(serde_json::json!({"path":path,"device":m.dev(),"inode":m.ino(),"uid":m.uid()}))
        }).collect::<Result<Vec<_>>>()?;
        Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(&serde_json::json!({"version":1,"scope":self.reference,"files":files,"directories":directories}))?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_scope_creation_handles_are_read_only_and_close_on_exec() {
        let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
        super::super::register_at(state.path(),tree.path(),"scope",vec![DeclaredPath {path:"src/new.rs".into(),directory:false}],vec![]).unwrap();
        let grants=super::super::grants(state.path(),tree.path()).unwrap();let held=grants.materialization.unwrap();
        held.validate_current().unwrap();
        for file in held.files.iter().map(|c|&c.file).chain(held.directories.values()) {
            let access=unsafe{libc::fcntl(file.as_raw_fd(),libc::F_GETFL)};
            let descriptor=unsafe{libc::fcntl(file.as_raw_fd(),libc::F_GETFD)};
            assert_eq!(access&libc::O_ACCMODE,libc::O_RDONLY,"setup identity retains writable descriptor");
            assert_ne!(descriptor&libc::FD_CLOEXEC,0,"setup identity can leak into a provider exec");
        }
    }
}
