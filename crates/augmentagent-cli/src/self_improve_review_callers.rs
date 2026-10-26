//! Bounded caller source delivery for broad lexical matches. Exact Git blob
//! identities bind the inventory; every byte is delivered in review groups.
//! A manifest is never represented as source read by a reviewer.
use super::*;
use tokio::io::AsyncReadExt;

const CHUNK: usize = 32 * 1024;
const BATCH_FILES: usize = 256;
const BATCH_BYTES: usize = 8 * 1024 * 1024;
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct Blob {
    path: String,
    oid: String,
    bytes: usize,
}
#[derive(Clone)]
pub(super) struct Plan {
    pub manifest: String,
    blobs: Vec<Blob>,
}
impl Plan {
    pub async fn collect(root: &Path, head: &str, diff: &str) -> Result<Self> {
        let (symbols, paths)=caller_paths(root,head,diff).await?;
        Self::from_paths(root,head,paths,symbols,"All lexical matches for changed and removed Rust item names; not a semantic call graph").await
    }
    pub async fn named(root:&Path,head:&str,path:&str)->Result<Self> {
        Self::from_paths(root,head,BTreeSet::from([path.into()]),BTreeSet::new(),"Explicit immutable source requested by final authority").await
    }
    async fn from_paths(root:&Path,head:&str,paths:BTreeSet<String>,symbols:BTreeSet<String>,lookup:&str)->Result<Self> {
        let mut blobs=Vec::new();
        let mut manifest_bytes=0usize;
        for path in paths {
            let raw=git(root,&["--literal-pathspecs","ls-tree","-z",head,"--",&path]).await?;
            let rows:Vec<_>=raw.split('\0').filter(|s|!s.is_empty()).collect();
            anyhow::ensure!(rows.len()==1,"caller evidence path is not one immutable source file");
            let (meta,name)=rows[0].split_once('\t').context("caller metadata missing path")?;
            let fields:Vec<_>=meta.split_whitespace().collect();
            anyhow::ensure!(name==path && fields.len()==3 && matches!(fields[0],"100644"|"100755") && fields[1]=="blob", "caller evidence must be a regular tracked source file");
            let oid=fields[2].to_owned();
            anyhow::ensure!(matches!(oid.len(),40|64) && oid.bytes().all(|b|b.is_ascii_hexdigit()),"invalid caller blob identity");
            let bytes=git(root,&["cat-file","-s",&oid]).await?.trim().parse()?;
            manifest_bytes=manifest_bytes.checked_add(path.len()+oid.len()+64).context("caller inventory overflow")?;
            anyhow::ensure!(manifest_bytes<=BATCH_BYTES,"caller metadata exceeds bounded inventory; no incomplete inventory accepted");
            blobs.push(Blob {path,oid,bytes});
        }
        let manifest=serde_json::to_string(&serde_json::json!({
            "lookup":lookup,
            "symbols":symbols,"source_manifest_only":blobs,
            "delivery":"Every listed immutable blob is separately streamed through every review role; this manifest is not source coverage."
        }))?;
        Ok(Self {manifest,blobs})
    }
    pub fn review_context(&self) -> String {
        serde_json::json!({"caller_manifest_sha256":aligned::digest(self.manifest.as_bytes()),"files":self.blobs.len(),
            "delivery":"The exact binding contains the complete immutable caller inventory. Every caller source byte is reviewed in separate bounded groups for every role. This metadata is not source coverage; inspect this assigned group and name precise missing dependencies."}).to_string()
    }
    pub fn stream<'a>(&'a self, root: &'a Path) -> Stream<'a> {
        Stream { root, blobs:&self.blobs, next:0, active:None }
    }
}
struct Active {
    child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    blob: Blob,
    offset: usize,
    remaining: Vec<u8>,
}
pub(super) struct Stream<'a> {
    root: &'a Path,
    blobs: &'a [Blob],
    next: usize,
    active: Option<Active>,
}
impl Stream<'_> {
    pub async fn next_batch(&mut self) -> Result<Option<BTreeMap<String,String>>> {
        let mut result=BTreeMap::new();
        let mut bytes=0usize;
        while result.len()<BATCH_FILES && bytes+CHUNK+4<=BATCH_BYTES {
            if self.active.is_none() {
                let Some(blob)=self.blobs.get(self.next) else { break; };
                self.next+=1;
                let mut child=Command::new("/usr/bin/git").args(["--no-replace-objects","cat-file","blob",&blob.oid])
                    .current_dir(self.root).env_clear().env("PATH","/usr/bin:/bin")
                    .env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
                    .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
                let stdout=child.stdout.take().context("caller pipe unavailable")?;
                self.active=Some(Active {child,stdout,blob:blob.clone(),offset:0,remaining:Vec::new()});
            }
            let active=self.active.as_mut().unwrap();
            let mut buffer=vec![0;CHUNK];
            // Pipe packet sizes are nondeterministic. Fill a fixed byte window
            // so restart reconstructs identical group/prompt cache identities.
            let mut size=0;
            while size<CHUNK {
                let count=active.stdout.read(&mut buffer[size..]).await?;
                if count==0 { break; }
                size+=count;
            }
            active.remaining.extend_from_slice(&buffer[..size]);
            anyhow::ensure!(!active.remaining.contains(&0),"caller evidence is binary");
            let valid=match std::str::from_utf8(&active.remaining) {
                Ok(value)=>value.len(),
                Err(error) if error.error_len().is_none() && size>0=>error.valid_up_to(),
                Err(_)=>anyhow::bail!("caller evidence is not strict UTF-8"),
            };
            if valid>0 || (size==0 && active.blob.bytes==0) {
                let body=String::from_utf8(active.remaining[..valid].to_vec())?;
                let end=active.offset.checked_add(valid).context("caller source size overflow")?;
                anyhow::ensure!(end<=active.blob.bytes,"immutable caller blob grew");
                let key=format!("caller:{}#bytes-{}-{end}",serde_json::to_string(&active.blob.path)?,active.offset);
                result.insert(key,body);
                active.offset=end;
                active.remaining.drain(..valid);
                bytes+=valid;
            }
            if size==0 {
                anyhow::ensure!(active.offset==active.blob.bytes && active.remaining.is_empty(),"immutable caller blob incomplete");
                anyhow::ensure!(active.child.wait().await?.success(),"immutable caller blob read failed");
                self.active=None;
            }
        }
        if result.is_empty() { Ok(None) } else { Ok(Some(result)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git_sync(root:&Path,args:&[&str])->String {
        let output=std::process::Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
    fn repository()->tempfile::TempDir {
        let root=tempfile::tempdir().unwrap();
        git_sync(root.path(),&["init","-q"]);
        git_sync(root.path(),&["config","user.name","Synthetic Fixture"]);
        git_sync(root.path(),&["config","user.email","fixture@example.invalid"]);
        root
    }
    async fn delivered(plan:&Plan,root:&Path)->BTreeMap<String,String> {
        let mut stream=plan.stream(root);let mut all=BTreeMap::new();
        while let Some(batch)=stream.next_batch().await.unwrap() {
            assert!(batch.len()<=BATCH_FILES);
            assert!(batch.values().map(String::len).sum::<usize>()<=BATCH_BYTES);
            for (key,value) in batch { assert!(all.insert(key,value).is_none()); }
        }
        all
    }
    #[tokio::test]
    async fn broad_caller_inventory_streams_every_file_without_candidate_mutation() {
        let root=repository();
        for index in 0..257 { std::fs::write(root.path().join(format!("caller-{index}.rs")),format!("fn fixture_{index}() {{ shared_api(); }}\n")).unwrap(); }
        git_sync(root.path(),&["add","."]);git_sync(root.path(),&["commit","-qm","fixture"]);
        let head=git_sync(root.path(),&["rev-parse","HEAD"]);
        let diff="+pub fn shared_api() {}";
        assert!(complete_caller_evidence(root.path(),&head,diff).await.unwrap_err().is::<EvidenceBudget>());
        let plan=Plan::collect(root.path(),&head,diff).await.unwrap();
        let all=delivered(&plan,root.path()).await;
        assert_eq!(all.len(),257);
        for index in 0..257 { assert!(all.values().any(|body|body==&format!("fn fixture_{index}() {{ shared_api(); }}\n"))); }
        assert_eq!(all,delivered(&plan,root.path()).await,"restart reconstructs exact receipt identities");
        assert_eq!(git_sync(root.path(),&["rev-parse","HEAD"]),head);
        assert!(git_sync(root.path(),&["status","--porcelain"]).is_empty());
    }
    #[tokio::test]
    async fn one_large_unicode_caller_is_lossless_and_bounded() {
        let root=repository();
        let body=format!("fn caller() {{ shared_api(); }}\n{}","αβγ\n".repeat(1_300_000));
        assert!(body.len()>BATCH_BYTES);
        std::fs::write(root.path().join("caller.rs"),&body).unwrap();
        git_sync(root.path(),&["add","."]);git_sync(root.path(),&["commit","-qm","fixture"]);
        let head=git_sync(root.path(),&["rev-parse","HEAD"]);
        let plan=Plan::collect(root.path(),&head,"+pub fn shared_api() {}").await.unwrap();
        let mut stream=plan.stream(root.path());let mut actual=String::new();let mut batches=0;
        while let Some(batch)=stream.next_batch().await.unwrap() {
            batches+=1;
            assert!(batch.values().map(String::len).sum::<usize>()<=BATCH_BYTES);
            let mut pieces:Vec<_>=batch.into_iter().collect();
            pieces.sort_by_key(|(key,_)|key.rsplit_once("#bytes-").unwrap().1.split('-').next().unwrap().parse::<usize>().unwrap());
            for (_,piece) in pieces { actual.push_str(&piece); }
        }
        assert!(batches>1);assert_eq!(actual,body);
        // The evidence source is immutable HEAD, even if working bytes change.
        std::fs::write(root.path().join("caller.rs"),"mutated").unwrap();
        assert_eq!(delivered(&plan,root.path()).await.values().map(String::len).sum::<usize>(),body.len());
    }
    #[tokio::test]
    async fn invalid_utf8_is_not_marked_complete() {
        let root=repository();
        std::fs::write(root.path().join("caller.rs"),b"shared_api \xff").unwrap();
        git_sync(root.path(),&["add","."]);git_sync(root.path(),&["commit","-qm","fixture"]);
        let head=git_sync(root.path(),&["rev-parse","HEAD"]);
        let plan=Plan::collect(root.path(),&head,"+pub fn shared_api() {}").await.unwrap();
        assert!(plan.stream(root.path()).next_batch().await.is_err());
    }
}
