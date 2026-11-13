//! Immutable source data for correction construction, never witness approval.
use super::*;

#[derive(Debug, Serialize)]
pub(super) struct Evidence {
    original_base: String,
    selection_identity: String,
    coverage: &'static str,
    sources: BTreeMap<String, String>,
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    file_index: usize,
    start_line: usize,
    end_line: usize,
    needle: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection { requests: Vec<ReadRequest> }

/// Constructor-only source view. Its private entries are created from Git,
/// never deserialized from model output or treated as witness provenance.
pub(super) struct ConstructorTable {
    base:String,
    identity:String,
    entries:Vec<(String,String)>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConstructorSelection {table_sha256:String,requests:Vec<ReadRequest>}
impl ConstructorTable {
    pub(super) async fn for_episode(root:&Path,episode:&ConstructionEpisode)->Result<Self> {
        Self::build(root,episode.original_base()).await
    }
    async fn build(root:&Path,base:&str)->Result<Self> {
        ensure!(matches!(base.len(),40|64) && base.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),"constructor source requires literal original commit");
        let tree=trusted_git(root,&["--no-replace-objects","ls-tree","-r","--long","-z",base]).await?;
        ensure!(tree.len()<=1024*1024,"constructor source table exceeds bound");
        let mut entries=Vec::new();
        for entry in tree.split('\0').filter(|e|!e.is_empty()) {
            let (metadata,path)=entry.split_once('\t').context("invalid constructor Git table")?;
            let fields=metadata.split_whitespace().collect::<Vec<_>>();
            if fields.get(1)!=Some(&"blob") || !matches!(fields.first().copied(),Some("100644"|"100755"))
                || !scope_authority::witness_source_allowed(path) {continue;}
            ensure!(fields.len()==4 && matches!(fields[2].len(),40|64) && fields[2].bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),"invalid constructor blob identity");
            let size=fields[3].parse::<u64>().context("invalid constructor blob size")?;
            // Column padding and Git display formatting are not source input.
            entries.push((format!("{} blob {} {size}",fields[0],fields[2]),path.to_owned()));
        }
        let identity=digest(&serde_json::to_vec(&("constructor-original-source-table-v1",base,&entries))?);
        Ok(Self {base:base.into(),identity,entries})
    }
    pub(super) fn page(&self,offset:usize,limit:usize)->Result<String> {
        ensure!(offset<=self.entries.len() && (1..=128).contains(&limit),"invalid constructor source page");
        let entries=self.entries.iter().enumerate().skip(offset).take(limit).map(|(index,(metadata,path))|
            serde_json::json!({"file_index":index,"path":path,"git_metadata":metadata})).collect::<Vec<_>>();
        let encoded=serde_json::to_string(&serde_json::json!({"version":1,"original_base":self.base,"table_sha256":self.identity,"offset":offset,"total":self.entries.len(),"entries":entries}))?;
        ensure!(encoded.len()<=32*1024,"constructor source page exceeds bound; request fewer entries");
        Ok(encoded)
    }
    pub(super) async fn read(&self,root:&Path,raw:&str,budget:usize)->Result<Evidence> {
        ensure!((1024..=512*1024).contains(&budget) && raw.len()<=32*1024,"invalid constructor source request budget");
        let selected:ConstructorSelection=serde_json::from_str(raw)?;
        ensure!(selected.table_sha256==self.identity,"constructor source table identity differs");
        ensure!(!selected.requests.is_empty() && selected.requests.len()<=16,"constructor requires 1-16 source requests");
        for request in &selected.requests {
            ensure!(request.file_index<self.entries.len() && request.start_line>0 && request.end_line>=request.start_line
                && request.end_line-request.start_line<600
                && request.needle.as_ref().is_none_or(|needle|!needle.is_empty() && needle.len()<=256),"invalid constructor source selection");
        }
        let identity=digest(&serde_json::to_vec(&("constructor-original-source-data-v1",&self.base,&self.identity,&selected.requests))?);
        let entries=self.entries.iter().map(|(metadata,path)|(metadata.as_str(),path.as_str())).collect::<Vec<_>>();
        render_selection(root,&self.base,&entries,identity,Selection {requests:selected.requests},budget).await
    }
}

// The already-supported frozen data projection in scope_authority. This
// constant admits only that indexed-source protocol, never an old judgment.
const PREDEPLOYMENT_SOURCE_POLICY:&str="bd44467c5c2ddedd493a2c8ce392578669b5ec8570b622403d05d92625f20771";
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceReplay {
    version:u32,current_epoch:String,current_request:String,
    original_epoch:String,original_request:String,original_policy:String,original_choose:String,
    original_input:String,provider:String,
}

fn proof(control:&Path,observation:&crate::autoship_workflow::NativeScopeCustody)->Result<(scope_authority::NativeReplyReference,serde_json::Value)> {
    let reference:scope_authority::NativeReplyReference=serde_json::from_str(&observation.provenance)?;
    ensure!(reference.artifact.parent()==Some(control),"amendment source belongs to another workflow");
    let bytes=baseline_witness::private_bytes(&reference.artifact,64*1024)?;
    ensure!(digest(&bytes)==reference.artifact_sha256,"amendment source proof changed during inspection");
    Ok((reference,serde_json::from_slice(&bytes)?))
}

fn current_request(control:&Path,base:&str,custody:&[crate::autoship_workflow::NativeScopeCustody],context:&str)->Result<Option<String>> {
    let mut selected=None;
    for observation in custody {
        let (reference,actual)=proof(control,observation)?;
        if actual["head"]!=base || actual["purpose"]!="baseline_witness" || actual["scope_context_hash"]!=context {continue;}
        let reply=scope_authority::NativeReply {text:observation.response.clone(),provenance:reference};
        scope_authority::verify_witness_reply(control,base,context,&reply)?;
        ensure!(selected.is_none(),"ambiguous current source-replay witness");
        selected=Some(actual["scope_request_hash"].as_str().context("current witness request missing")?.to_owned());
    }
    Ok(selected)
}

pub(super) fn replay_key(root:&Path,control:&Path,base:&str,custody:&[crate::autoship_workflow::NativeScopeCustody],context:Option<&str>)->Result<Option<String>> {
    scope_authority::require_retained_native_custody(root,custody)?;
    let control=control.canonicalize()?;
    let Some(context)=context else{return Ok(None)};
    Ok(current_request(&control,base,custody,context)?.map(|request|digest(&serde_json::to_vec(&("original-source-data-replay-v1",context,request)).expect("source replay key"))))
}

/// Frozen indexed-source protocol: this only reuses selections produced under
/// the unchanged current scope producer. It imports data, never an old ruling.
fn selections(
    root: &Path, control: &Path, base: &str,
    custody: &[crate::autoship_workflow::NativeScopeCustody], context: Option<&str>, replay:Option<&str>,
) -> Result<Vec<(String, Selection)>> {
    ensure!(matches!(base.len(), 40 | 64) && base.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)), "invalid original source commit");
    scope_authority::require_retained_native_custody(root, custody)?;
    let control = control.canonicalize()?;
    let replay=replay.map(|raw|->Result<SourceReplay> {
        ensure!(raw.len()<=4096,"source replay exceeds bound");
        let replay:SourceReplay=serde_json::from_str(raw)?;
        ensure!(replay.version==1 && Some(replay.current_epoch.as_str())==context
            && replay.original_policy==PREDEPLOYMENT_SOURCE_POLICY,"source replay belongs to another request or protocol");
        let request=current_request(&control,base,custody,context.context("source replay has no current witness context")?)?
            .context("source replay current witness unavailable")?;
        ensure!(replay.current_request==request,"source replay current request changed");
        Ok(replay)
    }).transpose()?;
    let mut selected = Vec::new();
    let mut inputs=std::collections::BTreeSet::new();
    for observation in custody {
        let (reference,proof)=proof(&control,observation)?;
        if proof["head"] != base || proof["purpose"] != "source_selection" {continue;}
        let direct=context.is_none_or(|expected|proof["scope_context_hash"]==expected)
            && proof["scope_policy_hash"]==scope_authority::witness_policy_hash();
        let projected=replay.as_ref().is_some_and(|replay| observation.input==replay.original_input
            && proof["provider"]==replay.provider && proof["supplied_request_hash"]==replay.original_choose
            && proof["native_input_hash"]==replay.original_input && proof["scope_context_hash"]==replay.original_epoch
            && proof["scope_request_hash"]==replay.original_request && proof["scope_policy_hash"]==replay.original_policy);
        if !direct && !projected {
            ensure!(!context.is_none_or(|expected|proof["scope_context_hash"]==expected),"original source selection uses an unsupported producer protocol");
            continue;
        }
        ensure!(inputs.insert(observation.input.as_str()), "ambiguous original source selection");
        let raw = observation.response.trim();
        let raw = raw.strip_prefix("```json\n").and_then(|s| s.strip_suffix("```")).unwrap_or(raw);
        // Completed rejected attempts remain authenticated custody. Syntax or
        // range failures are not competing accepted source selections.
        let Ok(selection)=serde_json::from_str::<Selection>(raw) else {continue;};
        if selection.requests.is_empty() || selection.requests.len()>16 || selection.requests.iter().any(|request|
            request.start_line==0 || request.end_line<request.start_line || request.end_line-request.start_line>=600
                || request.needle.as_ref().is_some_and(|needle|needle.is_empty() || needle.len()>256)) {continue;}
        let identity = digest(&serde_json::to_vec(&("amendment-original-source-selection-v1", base, &observation.input, &observation.response, &reference,
            if projected {replay.as_ref()}else{None}))?);
        selected.push((identity,selection));
        ensure!(selected.len()<=16,"original source attempts exceed bounded inspection");
    }
    ensure!(!selected.is_empty(),"original source selection unavailable");
    Ok(selected)
}

pub(super) fn authenticate(root:&Path,control:&Path,base:&str,custody:&[crate::autoship_workflow::NativeScopeCustody],context:Option<&str>,replay:Option<&str>)->Result<String> {
    // Free migration inspection authenticates candidate custody without Git.
    // Materialization below resolves producer acceptance before paid admission.
    let candidates=selections(root,control,base,custody,context,replay)?;
    Ok(digest(&serde_json::to_vec(&candidates.iter().map(|(identity,_)|identity).collect::<Vec<_>>())?))
}

/// Replay the original producer's acceptance of numbered excerpts without
/// retaining whole blobs. Overlapping needle windows count separately, as in
/// the original producer; amendment rendering may subsequently merge them.
#[derive(Clone)]
struct SelectionCheck<'a> {
    request:&'a ReadRequest,pending:String,line:usize,bytes:usize,
    previous:std::collections::VecDeque<usize>,ends:Vec<usize>,matches:usize,
}
impl<'a> SelectionCheck<'a> {
    fn new(request:&'a ReadRequest)->Self {
        Self {request,pending:String::new(),line:0,bytes:0,previous:Default::default(),ends:Vec::new(),matches:0}
    }
    fn line(&mut self,raw:&str) {
        self.line+=1;
        let text=raw.lines().next().unwrap_or("");
        let size=self.line.to_string().len()+2+text.len()+1;
        if let Some(needle)=&self.request.needle {
            self.ends.retain(|end|*end>=self.line);
            self.bytes=self.bytes.saturating_add(size.saturating_mul(self.ends.len()));
            if self.matches<5 && text.contains(needle) {
                self.matches+=1;
                self.bytes=self.bytes.saturating_add(self.previous.iter().sum::<usize>()).saturating_add(size);
                self.ends.push(self.line.saturating_add(99));
            }
            self.previous.push_back(size);
            if self.previous.len()>50 {self.previous.pop_front();}
        }else if (self.request.start_line..=self.request.end_line).contains(&self.line) {
            self.bytes=self.bytes.saturating_add(size);
        }
    }
    fn push(&mut self,chunk:&str)->Result<()> {
        for part in chunk.split_inclusive('\n') {
            ensure!(self.pending.len().saturating_add(part.len())<=512*1024,"original source line exceeds bounded selection inspection");
            self.pending.push_str(part);
            if self.pending.ends_with('\n') {let line=std::mem::take(&mut self.pending);self.line(&line);}
        }
        Ok(())
    }
    fn finish(mut self)->usize {
        if !self.pending.is_empty() {let line=std::mem::take(&mut self.pending);self.line(&line);}
        self.bytes
    }
}

/// Keep a bounded speculative counter for trailing whitespace. EOF discards it;
/// a later non-whitespace byte commits it. Even millions of trailing blank
/// lines need only two bounded counter states, not a retained whitespace string.
struct TrimmedSelectionCheck<'a> {
    core:SelectionCheck<'a>,tail:Option<SelectionCheck<'a>>,deferred:Option<anyhow::Error>,started:bool,
}
impl<'a> TrimmedSelectionCheck<'a> {
    fn new(request:&'a ReadRequest)->Self {Self {core:SelectionCheck::new(request),tail:None,deferred:None,started:false}}
    fn whitespace(&mut self,text:&str) {
        if text.is_empty() || self.deferred.is_some() {return;}
        let tail=self.tail.get_or_insert_with(||self.core.clone());
        if let Err(error)=tail.push(text) {self.deferred=Some(error);}
    }
    fn push(&mut self,chunk:&str)->Result<()> {
        // Only acceptance reproduces trusted_git's whole-file String::trim.
        // The delivered original source continues to retain exact bytes.
        let chunk=if self.started {chunk}else{chunk.trim_start()};
        if chunk.is_empty() {return Ok(());}
        self.started=true;
        let body=chunk.trim_end();
        if body.is_empty() {self.whitespace(chunk);return Ok(());}
        if let Some(error)=self.deferred.take() {return Err(error);}
        if let Some(tail)=self.tail.take() {self.core=tail;}
        self.core.push(body)?;
        self.whitespace(&chunk[body.len()..]);
        Ok(())
    }
    fn finish(self)->usize {self.core.finish()}
}

async fn accepted_selection(root:&Path,base:&str,entries:&[(&str,&str)],selection:&Selection)->Result<bool> {
    let mut bytes=0usize;
    for request in &selection.requests {
        let Some((_,path))=entries.get(request.file_index) else {return Ok(false);};
        let mut check=TrimmedSelectionCheck::new(request);
        match aligned_review::visit_immutable_source(root,base,path,|chunk|check.push(chunk)).await {
            Ok(())=>{},
            Err(error) if error.downcast_ref::<std::string::FromUtf8Error>().is_some()
                || error.downcast_ref::<std::str::Utf8Error>().is_some()=>return Ok(false),
            Err(error)=>return Err(error),
        }
        bytes=bytes.saturating_add(check.finish());
        if bytes>512*1024 {return Ok(false);}
    }
    Ok(bytes>0)
}

struct SourceBuffer<'a> {
    request: &'a ReadRequest,
    budget: usize,
    full: Option<String>,
    pending: String,
    previous: std::collections::VecDeque<(usize, String)>,
    selected: BTreeMap<usize, String>,
    line: usize,
    matches: usize,
    through: usize,
    selected_bytes: usize,
    previous_bytes: usize,
    partial_overflow: bool,
}
impl<'a> SourceBuffer<'a> {
    fn new(request:&'a ReadRequest,budget:usize)->Self {
        Self {request,budget,full:Some(String::new()),pending:String::new(),previous:Default::default(),selected:BTreeMap::new(),
            line:0,matches:0,through:0,selected_bytes:0,previous_bytes:0,partial_overflow:false}
    }
    fn overflow(&mut self) {
        self.partial_overflow=true;self.previous.clear();self.selected.clear();self.previous_bytes=0;
    }
    fn retain(&mut self,number:usize,line:String)->Result<()> {
        if self.partial_overflow {return Ok(());}
        if !self.selected.contains_key(&number) {
            self.selected_bytes=self.selected_bytes.checked_add(line.len()+number.to_string().len()+2).context("source size overflow")?;
            if self.selected_bytes>self.budget {self.overflow();return Ok(());}
            self.selected.insert(number,line);
        }
        Ok(())
    }
    fn line(&mut self,line:String)->Result<()> {
        self.line+=1;
        if self.partial_overflow {return Ok(());}
        if let Some(needle)=&self.request.needle {
            if self.matches<5 && line.contains(needle) {
                let first=self.line.saturating_sub(50).max(1);
                if self.line>1 && self.previous.front().map(|(number,_)|*number)!=Some(first) {
                    self.overflow();return Ok(());
                }
                self.matches+=1;self.through=self.line.saturating_add(99);
                for (number,previous) in self.previous.clone() {self.retain(number,previous)?;}
            }
            if self.line<=self.through {self.retain(self.line,line.clone())?;}
            self.previous_bytes+=line.len();self.previous.push_back((self.line,line));
            // Unmatched history is only a bounded look-behind, not yet a
            // mandatory window. Reject discarded context only at a real match.
            while self.previous.len()>50 || self.previous_bytes>self.budget {
                self.previous_bytes-=self.previous.pop_front().unwrap().1.len();
            }
        }else if (self.request.start_line..=self.request.end_line).contains(&self.line) {
            self.retain(self.line,line)?;
        }
        Ok(())
    }
    fn push(&mut self,chunk:&str)->Result<()> {
        if let Some(full)=&mut self.full {
            if full.len().saturating_add(chunk.len())<=self.budget {full.push_str(chunk);}else{self.full=None;}
        }
        for part in chunk.split_inclusive('\n') {
            ensure!(self.pending.len().saturating_add(part.len())<=self.budget,"original source line exceeds bounded working set");
            self.pending.push_str(part);
            if self.pending.ends_with('\n') {let line=std::mem::take(&mut self.pending);self.line(line)?;}
        }
        Ok(())
    }
    fn finish(mut self)->Result<(Option<String>,Option<String>)> {
        if !self.pending.is_empty() {let line=std::mem::take(&mut self.pending);self.line(line)?;}
        let mut excerpt=String::new();
        for (number,line) in self.selected {excerpt.push_str(&format!("{number}: {line}"));}
        Ok((self.full,(!self.partial_overflow).then_some(excerpt)))
    }
}

pub(super) async fn read(
    root: &Path, control: &Path, base: &str,
    custody: &[crate::autoship_workflow::NativeScopeCustody], context: Option<&str>,
) -> Result<Evidence> {
    read_bounded(root, control, base, custody, context, None,384 * 1024).await
}

pub(super) async fn read_projected(
    root:&Path,control:&Path,base:&str,custody:&[crate::autoship_workflow::NativeScopeCustody],
    context:Option<&str>,replay:Option<&str>,
)->Result<Evidence> {read_bounded(root,control,base,custody,context,replay,384*1024).await}

pub(super) async fn read_bounded(
    root: &Path, control: &Path, base: &str,
    custody: &[crate::autoship_workflow::NativeScopeCustody], context: Option<&str>, replay:Option<&str>,budget: usize,
) -> Result<Evidence> {
    ensure!((1024..=512 * 1024).contains(&budget), "invalid amendment source budget");
    let candidates=selections(root, control, base, custody, context,replay)?;
    let tree = trusted_git(root, &["--no-replace-objects", "ls-tree", "-r", "--long", "-z", base]).await?;
    ensure!(tree.len() <= 1024 * 1024, "original source table exceeds bound");
    // Preserve the original producer's ordinals, including literal Git paths.
    let entries = tree.split('\0').filter(|entry| !entry.is_empty()).map(|entry| {
        entry.split_once('\t').context("invalid original source table")
    }).collect::<Result<Vec<_>>>()?;
    let entries = entries.into_iter().filter(|(metadata, path)| metadata.split_whitespace().nth(1) == Some("blob")
        && scope_authority::witness_source_allowed(path)).collect::<Vec<_>>();
    let mut accepted=None;
    for (identity,selection) in candidates {
        if !accepted_selection(root,base,&entries,&selection).await? {continue;}
        ensure!(accepted.is_none(),"ambiguous original source selection");
        accepted=Some((identity,selection));
    }
    let (identity,selected)=accepted.context("accepted original source selection unavailable")?;
    render_selection(root,base,&entries,identity,selected,budget).await
}

async fn render_selection(root:&Path,base:&str,entries:&[(&str,&str)],identity:String,selected:Selection,budget:usize)->Result<Evidence> {
    let mut full = BTreeMap::new();
    let mut partial = BTreeMap::new();
    let mut complete = true;
    let mut partial_available=true;
    for request in selected.requests {
        let (_, path) = entries.get(request.file_index).context("original source ordinal outside immutable table")?;
        let mut buffer=SourceBuffer::new(&request,budget);
        aligned_review::visit_immutable_source(root,base,path,|chunk|buffer.push(chunk)).await?;
        let (content,excerpt)=buffer.finish()?;
        if let Some(excerpt)=excerpt {partial.insert(format!("{path}:{}-{}:{:?}", request.start_line, request.end_line, request.needle), excerpt);}else{partial_available=false;}
        if let Some(content)=content {full.insert((*path).to_owned(),content);}else{complete=false;}
    }
    // Complete files avoid losing a production call below the original search
    // windows when they fit. Larger selections retain exactly bounded windows;
    // the final authority is explicitly told that omitted bytes are unread.
    let mut evidence = Evidence { original_base: base.into(), selection_identity: identity,
        coverage: "complete selected original files; selection is data only, not qualification or complete repository coverage", sources: full };
    if !complete || serde_json::to_vec(&evidence)?.len() > budget {
        ensure!(partial_available,"original source windows exceed bounded working set");
        evidence.sources = partial;
        evidence.coverage = "partial original source excerpts; omitted bytes and empty searches are unread, never proof of absence or qualification";
    }
    ensure!(evidence.sources.values().any(|content| !content.trim().is_empty()), "original source selection yielded no evidence");
    ensure!(serde_json::to_vec(&evidence)?.len() <= budget, "original source excerpts exceed amendment budget");
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    async fn commit_constructor_source_fixture(root:&Path) {
        for args in [vec!["add","-A"],vec!["commit","-qm","constructor source fixture"]] {
            let out=Command::new("/usr/bin/git").args(["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null"])
                .args(args).current_dir(root).env_clear().env("PATH","/usr/bin:/bin").output().await.unwrap();
            assert!(out.status.success());
        }
    }
    fn constructor_query(page:&serde_json::Value,path:&str,start:usize,end:usize)->String {
        let file=page["entries"].as_array().unwrap().iter().find(|e|e["path"]==path).unwrap();
        serde_json::json!({"table_sha256":page["table_sha256"],"requests":[{"file_index":file["file_index"],"start_line":start,"end_line":end,"needle":null}]}).to_string()
    }
    #[tokio::test]
    async fn constructor_sources_read_original_git_without_borrowing_witness_custody() {
        let (root,_control,base,_unused_witness)=fixture().await;
        let table=ConstructorTable::build(root.path(),&base).await.unwrap();
        let page:serde_json::Value=serde_json::from_str(&table.page(0,128).unwrap()).unwrap();
        std::fs::write(root.path().join("behavior.rs"),"candidate replacement must not be read").unwrap();
        let evidence=table.read(root.path(),&constructor_query(&page,"behavior.rs",1,1),4096).await.unwrap();
        assert_eq!(evidence.sources["behavior.rs"],"pub fn behavior() -> bool { false }\n");
        assert!(evidence.coverage.starts_with("complete selected original files"));
        assert_eq!(evidence.original_base,base);
        assert!(ConstructorTable::build(root.path(),"HEAD").await.is_err());
    }
    #[tokio::test]
    async fn constructor_sources_reject_foreign_tables_paths_ranges_and_non_text() {
        let (root,_control,base,_)=fixture().await;
        let table=ConstructorTable::build(root.path(),&base).await.unwrap();
        let page:serde_json::Value=serde_json::from_str(&table.page(0,128).unwrap()).unwrap();
        let valid:serde_json::Value=serde_json::from_str(&constructor_query(&page,"behavior.rs",1,1)).unwrap();
        for mutation in ["table","ordinal","range","needle","path","extra"] {
            let mut query=valid.clone();
            match mutation {
                "table"=>query["table_sha256"]="f".repeat(64).into(),
                "ordinal"=>query["requests"][0]["file_index"]=999999.into(),
                "range"=>query["requests"][0]["start_line"]=0.into(),
                "needle"=>query["requests"][0]["needle"]="".into(),
                "path"=>query["requests"][0]["path"]="/etc/passwd".into(),
                "extra"=>query["untrusted_path"]="behavior.rs".into(),_=>unreachable!(),
            }
            assert!(table.read(root.path(),&query.to_string(),4096).await.is_err(),"accepted {mutation}");
        }
        assert!(table.read(root.path(),&constructor_query(&page,"zzz-invalid.bin",1,1),4096).await.is_err());
        assert!(table.page(0,0).is_err());assert!(table.page(0,129).is_err());assert!(table.page(999999,1).is_err());
    }
    #[tokio::test]
    async fn constructor_sources_exclude_credentials_and_symlinks_and_bind_paged_ordinals() {
        let (root,_control,base,_)=fixture().await;
        std::fs::write(root.path().join(".env"),"SYNTHETIC_CREDENTIAL=fixture").unwrap();
        std::fs::write(root.path().join("secret.key"),"synthetic fixture").unwrap();
        std::os::unix::fs::symlink(".env",root.path().join("alias.rs")).unwrap();
        commit_constructor_source_fixture(root.path()).await;
        let next=actual_head(root.path()).await.unwrap();
        let table=ConstructorTable::build(root.path(),&next).await.unwrap();
        let page:serde_json::Value=serde_json::from_str(&table.page(0,128).unwrap()).unwrap();
        for name in [".env","secret.key","alias.rs"] {
            assert!(!page["entries"].as_array().unwrap().iter().any(|e|e["path"]==name));
        }
        let first:serde_json::Value=serde_json::from_str(&table.page(0,1).unwrap()).unwrap();
        let second:serde_json::Value=serde_json::from_str(&table.page(1,1).unwrap()).unwrap();
        assert_eq!(first["table_sha256"],second["table_sha256"]);
        assert_ne!(first["entries"][0]["file_index"],second["entries"][0]["file_index"]);
        let old=ConstructorTable::build(root.path(),&base).await.unwrap();
        assert!(old.read(root.path(),&constructor_query(&page,"behavior.rs",1,1),4096).await.is_err());
    }
    #[tokio::test]
    async fn constructor_sources_bound_serialized_evidence_without_claiming_unread_bytes() {
        let (root,_control,_base,_)=fixture().await;
        std::fs::write(root.path().join("large.rs"),"abcdefghij".repeat(20)+"\n"+&"unread-tail\n".repeat(1000)).unwrap();
        commit_constructor_source_fixture(root.path()).await;
        let table=ConstructorTable::build(root.path(),&actual_head(root.path()).await.unwrap()).await.unwrap();
        let page:serde_json::Value=serde_json::from_str(&table.page(0,128).unwrap()).unwrap();
        let evidence=table.read(root.path(),&constructor_query(&page,"large.rs",1,1),1024).await.unwrap();
        assert!(evidence.coverage.starts_with("partial original source excerpts"));
        assert!(serde_json::to_vec(&evidence).unwrap().len()<=1024);
        assert!(!serde_json::to_string(&evidence).unwrap().contains("unread-tail"));
        assert!(table.read(root.path(),&constructor_query(&page,"large.rs",1,599),1024).await.is_err(),"oversized required windows were silently truncated");
    }

    async fn fixture() -> (tempfile::TempDir, tempfile::TempDir, String, Vec<crate::autoship_workflow::NativeScopeCustody>) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let control = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(control.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.path().join("behavior.rs"), "pub fn behavior() -> bool { false }\n").unwrap();
        std::fs::write(root.path().join("zzz-invalid.bin"),[0xff]).unwrap();
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

    #[test]
    fn amendment_source_selection_check_matches_original_numbered_windows() {
        for (source,start,end,needle) in [
            ("first\r\nneedle\r\nlast  ".to_owned(),1,3,None),
            ((0..250).map(|n|if n==55 || n==60 {"needle\n"}else{"ordinary\n"}).collect::<String>(),1,200,Some("needle")),
            ("first\nlast\r".to_owned(),999,1000,None),
            ("  \n\nfirst\n\n  \n".to_owned(),2,3,None),
            ("\n\n".to_owned(),1,2,None),
        ] {
            let request=ReadRequest {file_index:0,start_line:start,end_line:end,needle:needle.map(str::to_owned)};
            let lines=source.trim().lines().collect::<Vec<_>>();
            let ranges=if let Some(needle)=needle {
                lines.iter().enumerate().filter(|(_,line)|line.contains(needle)).take(5)
                    .map(|(index,_)|(index.saturating_sub(50),(index+100).min(lines.len()))).collect::<Vec<_>>()
            }else {vec![(start-1,end.min(lines.len()))]};
            let expected=ranges.into_iter().map(|(start,end)|lines.iter().enumerate().take(end).skip(start)
                .map(|(index,line)|format!("{}: {line}\n",index+1).len()).sum::<usize>()).sum::<usize>();
            let mut check=TrimmedSelectionCheck::new(&request);
            for byte in source.as_bytes().chunks(3) {check.push(std::str::from_utf8(byte).unwrap()).unwrap();}
            assert_eq!(check.finish(),expected,"producer acceptance changed at CRLF, EOF, empty range or overlapping windows");
        }
    }

    #[test]
    fn amendment_source_selection_discards_unbounded_trailing_whitespace_metadata() {
        let request=ReadRequest {file_index:0,start_line:2,end_line:2,needle:None};
        let mut check=TrimmedSelectionCheck::new(&request);
        check.push("first").unwrap();
        for _ in 0..1024 {check.push(&"\n".repeat(1024)).unwrap();}
        assert_eq!(check.finish(),0,"trim-discarded EOF whitespace became accepted original evidence");
        let request=ReadRequest {file_index:0,start_line:1,end_line:1,needle:None};
        let mut check=TrimmedSelectionCheck::new(&request);
        check.push("first").unwrap();
        for _ in 0..1024 {check.push(&"\n".repeat(1024)).unwrap();}
        check.push("later").unwrap();
        assert_eq!(check.finish(),"1: first\n".len(),"interior short blank lines exceeded bounded counters");
    }

    #[tokio::test]
    async fn amendment_source_uses_correction_after_rejected_selection() {
        for rejected_response in [
            r#"{"requests":[{"file_index":0,"start_line":999,"end_line":1000,"needle":null}]}"#,
            r#"{"requests":[{"file_index":1,"start_line":1,"end_line":1,"needle":null}]}"#,
            r#"{"requests":[{"file_index":99,"start_line":1,"end_line":1,"needle":null}]}"#,
            "not valid JSON",
        ] {
        let (root,control,base,mut custody)=fixture().await;
        let mut rejected=crate::autoship_workflow::NativeScopeCustody {
            input:digest(b"rejected first source selection"),response:rejected_response.into(),
            provenance:custody[0].provenance.clone(),provider:custody[0].provider,
        };
        let (_,mut original)=proof(control.path(),&custody[0]).unwrap();
        original["native_input_hash"]=rejected.input.clone().into();
        original["response_hash"]=digest(rejected.response.as_bytes()).into();
        let bytes=serde_json::to_vec(&original).unwrap();
        let artifact=control.path().join(format!("primary-scope-{}.json",uuid::Uuid::new_v4()));
        std::fs::write(&artifact,&bytes).unwrap();
        std::fs::set_permissions(&artifact,std::fs::Permissions::from_mode(0o600)).unwrap();
        rejected.provenance=serde_json::to_string(&scope_authority::NativeReplyReference {artifact,artifact_sha256:digest(&bytes)}).unwrap();
        custody.insert(0,rejected);
        let evidence=read(root.path(),control.path(),&base,&custody,Some(&"a".repeat(64))).await
            .expect("rejected empty-range response must not compete with accepted correction");
        assert_eq!(evidence.sources["behavior.rs"],"pub fn behavior() -> bool { false }\n");
        }
    }

    #[tokio::test]
    async fn amendment_source_replays_original_git_bytes_without_reading_dirty_candidate() {
        let (root, control, base, custody) = fixture().await;
        std::fs::write(root.path().join("behavior.rs"), "private uncommitted candidate bytes").unwrap();
        let evidence = read(root.path(), control.path(), &base, &custody, Some(&"a".repeat(64))).await.unwrap();
        let supplied = serde_json::to_string(&evidence).unwrap();
        assert!(supplied.contains("pub fn behavior() -> bool { false }"), "amendment lacks original production source");
        assert!(!supplied.contains("private uncommitted"));
        assert_eq!(evidence.sources["behavior.rs"], "pub fn behavior() -> bool { false }\n", "source bytes were trimmed");
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

    #[test]
    fn amendment_source_stream_keeps_exact_bytes_and_bounded_partial_context() {
        let request=ReadRequest {file_index:0,start_line:2,end_line:3,needle:None};
        let mut buffer=SourceBuffer::new(&request,1024);
        let complete="  // header\nfn first() {}\nfn second() {}\n  ";
        for chunk in complete.as_bytes().chunks(3) {buffer.push(std::str::from_utf8(chunk).unwrap()).unwrap();}
        let (full,partial)=buffer.finish().unwrap();
        assert_eq!(full.as_deref(),Some(complete));
        assert_eq!(partial.as_deref(),Some("2: fn first() {}\n3: fn second() {}\n"));
        let mut buffer=SourceBuffer::new(&request,1024);
        buffer.push(complete).unwrap();
        for _ in 0..1024 {buffer.push("// unrelated later source\n").unwrap();}
        let (full,partial)=buffer.finish().unwrap();
        assert!(full.is_none(),"large files were retained without a bound");
        assert_eq!(partial.as_deref(),Some("2: fn first() {}\n3: fn second() {}\n"));
    }

    #[test]
    fn amendment_source_complete_file_wins_over_fallback_window_overhead() {
        let request=ReadRequest {file_index:0,start_line:1,end_line:600,needle:None};
        let content="abc\n".repeat(600);
        let evidence=Evidence {original_base:"a".repeat(40),selection_identity:"b".repeat(64),coverage:"complete",sources:BTreeMap::from([("behavior.rs".into(),content.clone())])};
        assert!(serde_json::to_vec(&evidence).unwrap().len()<4096,"complete file fixture must fit the real serialized budget");
        let mut buffer=SourceBuffer::new(&request,4096);
        buffer.push(&content).expect("fallback bookkeeping rejected a complete file that fits");
        assert_eq!(buffer.finish().unwrap().0.as_deref(),Some(content.as_str()));
    }

    #[test]
    fn amendment_source_excerpts_preserve_crlf_and_absent_final_newline() {
        let request=ReadRequest {file_index:0,start_line:1,end_line:3,needle:None};
        let mut buffer=SourceBuffer::new(&request,4096);
        buffer.push("first\r\nsecond\r\nfinal  ").unwrap();
        assert_eq!(buffer.finish().unwrap().1.as_deref(),Some("1: first\r\n2: second\r\n3: final  "),"excerpt invented a source terminator");
    }

    #[test]
    fn amendment_source_unmatched_history_does_not_poison_later_valid_windows() {
        let request=ReadRequest {file_index:0,start_line:1,end_line:600,needle:Some("needle_here".into())};
        let long=format!("{}\n","x".repeat(256));
        let mut buffer=SourceBuffer::new(&request,4096);
        for _ in 0..100 {buffer.push(&long).unwrap();}
        for _ in 0..60 {buffer.push("short\n").unwrap();}
        buffer.push("needle_here\n").unwrap();
        for _ in 0..99 {buffer.push("tail\n").unwrap();}
        let (full,partial)=buffer.finish().unwrap();
        assert!(full.is_none());
        let partial=partial.expect("irrelevant unmatched history poisoned a later complete required window");
        assert!(partial.starts_with("111: short\n"));assert!(partial.contains("161: needle_here\n"));assert!(partial.ends_with("260: tail\n"));
        let mut missing=SourceBuffer::new(&request,4096);
        for _ in 0..100 {missing.push(&long).unwrap();}
        missing.push("needle_here\n").unwrap();
        assert!(missing.finish().unwrap().1.is_none(),"a real required preceding window exceeding budget was silently dropped");
    }

    #[tokio::test]
    async fn amendment_authority_does_not_spend_a_call_on_unstaged_source_only_dispute() {
        let (root,control,base,custody)=fixture().await;
        let evidence=read(root.path(),control.path(),&base,&custody,Some(&"a".repeat(64))).await.unwrap();
        let request=super::construction_request("Original proposal is disputed; no replacement has been constructed or measured.",&evidence).unwrap();
        let expected="c".repeat(64);let calls=std::sync::atomic::AtomicUsize::new(0);
        let result=super::authority_response(root.path(),&base,&expected,&request,|prompt| {
            assert!(prompt.contains("pub fn behavior() -> bool { false }"),"fixture lost authenticated original source");
            calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            // A valid complete refusal is not malformed output and must remain
            // authoritative. The missing work is construction before this call.
            let raw=serde_json::json!({"approved":false,"previous_contract_hash":expected,
                "approve_environment_transition":false,"reason":"No complete measured replacement was supplied for independent adjudication","proposal":null}).to_string();
            async move {Ok((raw,"scripted-independent-authority".into()))}
        },||panic!("a valid completed refusal cannot justify a formatting retry")).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst),0,
            "source-only dispute spent independent authority before a concrete staged correction existed");
        assert!(result.is_err(),"unstaged source-only dispute reached an authority decision");
    }

    #[tokio::test]
    async fn amendment_source_and_construction_task_reach_actual_authority_call() {
        let (root,control,base,custody)=fixture().await;
        let evidence=read(root.path(),control.path(),&base,&custody,Some(&"a".repeat(64))).await.unwrap();
        let request=super::construction_request("Original proposal is disputed; retain criteria.",&evidence).unwrap();
        let expected="c".repeat(64);let calls=std::sync::atomic::AtomicUsize::new(0);
        let (decision,_)=super::authority_response(root.path(),&base,&expected,&request,|prompt| {
            calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            assert!(prompt.contains("Construct a complete executable correction now"),"authority is asked only to classify an unchanged defective proposal");
            assert!(prompt.contains("pub fn behavior() -> bool { false }"),"actual authority call lacks immutable production source");
            assert!(prompt.contains(&base));
            let raw=serde_json::json!({"approved":false,"previous_contract_hash":expected,"approve_environment_transition":false,"reason":"Independent authority legitimately declines the correction","proposal":null}).to_string();
            async move {Ok((raw,"scripted-independent-authority".into()))}
        },||panic!("valid completed refusal cannot spend a correction call")).await.unwrap();
        assert!(!decision.approved);assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst),1);
        assert!(super::construction_request(&"x".repeat(512*1024),&evidence).is_err(),"oversized complete request passed construction preflight");
    }

    #[tokio::test]
    async fn amendment_source_positive_witness_projects_history_into_actual_request() {
        let (root,control,prepared)=baseline_witness::prepared_fixture();
        std::fs::set_permissions(control.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        baseline_witness::persist_scripted(root.path(),control.path(),&prepared,true).unwrap();
        baseline_witness::assert_current(root.path(),control.path(),&prepared).unwrap();
        let witness=baseline_witness::scripted_native_custody(control.path(),&prepared).unwrap();
        let (_,current)=proof(control.path(),&witness).unwrap();
        let base=prepared.baseline.registration.baseline_head();
        let source_raw=r#"{"requests":[{"file_index":0,"start_line":1,"end_line":1,"needle":null}]}"#;
        let source_input=digest(b"historical source observation");
        let old=serde_json::json!({"version":1,"head":base,"provider":"codex","native_session":"historical-source-fixture",
            "supplied_request_hash":digest(b"historical source question"),"response_hash":digest(source_raw.as_bytes()),"purpose":"source_selection",
            "scope_context_hash":"a".repeat(64),"native_input_hash":source_input,"scope_request_hash":digest(b"historical baseline question"),
            "scope_policy_hash":PREDEPLOYMENT_SOURCE_POLICY});
        let bytes=serde_json::to_vec(&old).unwrap();let artifact=control.path().join(format!("primary-scope-{}.json",uuid::Uuid::new_v4()));
        std::fs::write(&artifact,&bytes).unwrap();std::fs::set_permissions(&artifact,std::fs::Permissions::from_mode(0o600)).unwrap();
        let source=crate::autoship_workflow::NativeScopeCustody {input:source_input,response:source_raw.into(),
            provenance:serde_json::to_string(&scope_authority::NativeReplyReference {artifact,artifact_sha256:digest(&bytes)}).unwrap(),provider:crate::autoship_workflow::submission::Provider::Codex};
        let custody=vec![source,witness];
        assert!(baseline_witness::retained_semantic_diagnosis_in(root.path(),control.path(),&prepared,&custody).unwrap().is_none());
        assert!(baseline_witness::retained_semantic_judgment_in(root.path(),control.path(),&prepared,&custody).unwrap().is_none(),"positive data reopened legacy Unknown recovery");
        let replay=serde_json::json!({"version":1,"current_epoch":current["scope_context_hash"],"current_request":current["scope_request_hash"],
            "original_epoch":old["scope_context_hash"],"original_request":old["scope_request_hash"],"original_policy":old["scope_policy_hash"],
            "original_choose":old["supplied_request_hash"],"original_input":custody[0].input,"provider":"codex"}).to_string();
        let key=digest(&serde_json::to_vec(&("original-source-data-replay-v1",&current["scope_context_hash"],&current["scope_request_hash"])).unwrap());
        let reads=std::cell::Cell::new(0);
        let (request,_)=super::sourced_authority_request(root.path(),control.path(),&prepared,&custody,None,
            "Failure: verifier environment changed; re-register baseline",|requested| {
                assert_eq!(requested,key,"positive context chose another recorded mapping");reads.set(reads.get()+1);Ok(Some(replay.clone()))
            }).await.expect("qualified positive context cannot deliver historical source to amendment authority");
        assert_eq!(reads.get(),1);
        let expected=prepared.baseline.registration.contract_hash();let calls=std::cell::Cell::new(0);
        let (decision,_)=super::authority_response(root.path(),base,expected,&request,|actual| {
            calls.set(calls.get()+1);
            assert!(actual.contains("READY = False"));assert!(actual.contains("Construct a complete executable correction now"));
            let raw=serde_json::json!({"approved":false,"previous_contract_hash":expected,"approve_environment_transition":false,
                "reason":"Independent authority legitimately refuses this environment correction","proposal":null}).to_string();
            async move {Ok((raw,"scripted-independent-review".into()))}
        },||panic!("valid refusal cannot consume another correction call")).await.unwrap();
        assert!(!decision.approved);assert_eq!(calls.get(),1);
        baseline_witness::assert_current(root.path(),control.path(),&prepared).unwrap();
    }

    #[tokio::test]
    async fn amendment_source_refuses_duplicate_selection_and_unsupported_producer() {
        let (root,control,base,mut custody)=fixture().await;
        let duplicate=crate::autoship_workflow::NativeScopeCustody {input:custody[0].input.clone(),response:custody[0].response.clone(),provenance:custody[0].provenance.clone(),provider:custody[0].provider};
        custody.push(duplicate);
        assert!(read(root.path(),control.path(),&base,&custody,Some(&"a".repeat(64))).await.unwrap_err().to_string().contains("ambiguous"));
        custody.pop();
        let mut reference:scope_authority::NativeReplyReference=serde_json::from_str(&custody[0].provenance).unwrap();
        let mut proof:serde_json::Value=serde_json::from_slice(&std::fs::read(&reference.artifact).unwrap()).unwrap();
        proof["scope_policy_hash"]="b".repeat(64).into();
        let bytes=serde_json::to_vec(&proof).unwrap();std::fs::write(&reference.artifact,&bytes).unwrap();reference.artifact_sha256=digest(&bytes);
        custody[0].provenance=serde_json::to_string(&reference).unwrap();
        assert!(read(root.path(),control.path(),&base,&custody,Some(&"a".repeat(64))).await.unwrap_err().to_string().contains("unsupported producer"));
    }

    #[tokio::test]
    async fn amendment_source_uses_only_exact_recorded_historical_projection() {
        let (root,control,base,mut custody)=fixture().await;
        let mut old_ref:scope_authority::NativeReplyReference=serde_json::from_str(&custody[0].provenance).unwrap();
        let mut old:serde_json::Value=serde_json::from_slice(&std::fs::read(&old_ref.artifact).unwrap()).unwrap();
        old["scope_policy_hash"]="bd44467c5c2ddedd493a2c8ce392578669b5ec8570b622403d05d92625f20771".into();
        let bytes=serde_json::to_vec(&old).unwrap();std::fs::write(&old_ref.artifact,&bytes).unwrap();old_ref.artifact_sha256=digest(&bytes);
        custody[0].provenance=serde_json::to_string(&old_ref).unwrap();
        let current="b".repeat(64);let current_request=digest(b"current original qualification");
        let reply=scope_authority::scripted_witness_reply(control.path(),&base,&current,"current negative diagnosis").unwrap();
        let mut proof:serde_json::Value=serde_json::from_slice(&std::fs::read(&reply.provenance.artifact).unwrap()).unwrap();
        proof["scope_request_hash"]=current_request.clone().into();
        let bytes=serde_json::to_vec(&proof).unwrap();std::fs::write(&reply.provenance.artifact,&bytes).unwrap();
        let reference=scope_authority::NativeReplyReference {artifact:reply.provenance.artifact,artifact_sha256:digest(&bytes)};
        custody.push(crate::autoship_workflow::NativeScopeCustody {input:proof["native_input_hash"].as_str().unwrap().into(),response:reply.text,provenance:serde_json::to_string(&reference).unwrap(),provider:crate::autoship_workflow::submission::Provider::Codex});
        let replay=serde_json::json!({"version":1,"current_epoch":current,"current_request":current_request,
            "original_epoch":old["scope_context_hash"],"original_request":old["scope_request_hash"],"original_policy":old["scope_policy_hash"],
            "original_choose":old["supplied_request_hash"],"original_input":custody[0].input,"provider":"codex"});
        let evidence=read_projected(root.path(),control.path(),&base,&custody,Some(&current),Some(&replay.to_string())).await.expect("existing authenticated source projection cannot supply amendment evidence");
        assert_eq!(evidence.sources["behavior.rs"],"pub fn behavior() -> bool { false }\n");
        assert!(read_projected(root.path(),control.path(),&base,&custody,Some(&current),None).await.is_err(),"old source policy accepted without its recorded projection");
        for field in ["current_epoch","current_request","original_epoch","original_request","original_policy","original_choose","original_input","provider"] {
            let mut changed=replay.clone();changed[field]="f".repeat(64).into();
            assert!(read_projected(root.path(),control.path(),&base,&custody,Some(&current),Some(&changed.to_string())).await.is_err(),"changed replay field {field} accepted");
        }
    }

    #[tokio::test]
    #[ignore = "explicit read-only original-source compatibility; no model calls or workflow mutation"]
    async fn production_amendment_source_compatibility() {
        let state=PathBuf::from(std::env::var_os("AUTOSHIP_RECOVERY_STATE").expect("explicit read-only state"));
        let id=std::env::var("AUTOSHIP_RECOVERY_WORKFLOW").expect("explicit ordinary workflow");
        let store=crate::autoship_workflow::WorkflowStore::open_read_only(&state).unwrap().unwrap();
        let before=store.snapshot(&id).unwrap().unwrap();
        let recovery=store.recovery_context(&id,&before.checkpoint.binding).unwrap().unwrap();
        let control=control_directories(&recovery.repository,&id).unwrap().workflow_acceptance;
        let custody=store.inspect_baseline_native_custody(&id).unwrap();
        let number=id.strip_prefix(&format!("{}#",recovery.repository)).unwrap().parse::<u64>().unwrap();
        let source=AcceptanceSource::Issue(number);
        let (prepared,_)=super::super::retained_semantic_source(&recovery.worktree,&control,source,&before.checkpoint.binding,&custody,&store.inspect_verifier_evidence(&id)).unwrap().unwrap();
        let enrolled=super::constructor_enrollment(&recovery.worktree,&control,source).unwrap();
        assert_eq!(enrolled.baseline.registration.contract_hash(),prepared.baseline.registration.contract_hash(),"constructor enrollment differs from retained ordinary source");
        assert_eq!(enrolled.baseline.registration.baseline_head(),prepared.baseline.registration.baseline_head());
        let (negative_identity,diagnosis_raw)=baseline_witness::retained_semantic_diagnosis_in(&recovery.worktree,&control,&prepared,&custody).unwrap().unwrap();
        let diagnosis:serde_json::Value=serde_json::from_str(&diagnosis_raw).unwrap();
        let base=prepared.baseline.registration.baseline_head();let context=diagnosis["context"].as_str();
        let key=replay_key(&recovery.worktree,&control,base,&custody,context).unwrap();
        let replay=key.map(|key|store.inspect_verifier_evidence(&id).scope_response(&key).unwrap()).flatten();
        let evidence=read_projected(&recovery.worktree,&control,base,&custody,context,replay.as_deref()).await.unwrap();
        assert_eq!(id,"nolanmak/jarvis#2014","historical rollout proof is explicitly scoped to the retained ordinary issue");
        assert_eq!(evidence.sources.len(),8);assert!(evidence.coverage.starts_with("complete selected original files"));
        let request=crate::autoship_workflow::submission::Request {repository:recovery.repository.clone(),target:crate::autoship_workflow::submission::Target::Issue(number),provider:None,until:crate::autoship_workflow::submission::Milestone::Merged};
        let targeted::Resolved::Issue(issue)=targeted::resolve_for_recovery(&recovery.worktree,&request).await.unwrap() else {panic!("original issue unavailable")};
        let current_request=if let Some(pr)=recovery.source_pr {
            let request=crate::autoship_workflow::submission::Request {target:crate::autoship_workflow::submission::Target::Pr(pr),..request};
            let targeted::Resolved::Pr(pr)=targeted::resolve_for_recovery(&recovery.worktree,&request).await.unwrap() else {panic!("original source PR unavailable")};
            format!("{}\n{}\n{}",issue.title,issue.body,pr.body)
        }else{serde_json::to_string(&(issue.number,&issue.title,&issue.body)).unwrap()};
        assert_eq!(current_request,recovery.source_request,"protected original owner request changed");
        let main=workflow::remote_main_head(&recovery.worktree,&recovery.repository).await.unwrap();
        let head=actual_head(&recovery.worktree).await.unwrap();
        let (basis,diff)=super::super::contribution_diff(&recovery.worktree,base,&main,&head).await.unwrap();
        let upstream=trusted_git(&recovery.worktree,&["--no-replace-objects","diff","--name-only","--no-ext-diff","--no-textconv",base,&basis,"--"]).await.unwrap();
        // Production Git metadata and source remain read-only. Measure current
        // engine fingerprints in a disposable shared-object clone, never by
        // adding temporary worktrees to the deployed repository or state root.
        let scratch=tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(scratch.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        let clone=scratch.path().join("repo");
        let cloned=Command::new("/usr/bin/git").args(["--no-replace-objects","-c","core.hooksPath=/dev/null","clone","--shared","--no-checkout","--"])
            .arg(&recovery.worktree).arg(&clone).env_clear().env("PATH","/usr/bin:/bin").env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
            .output().await.unwrap();assert!(cloned.status.success(),"isolated source clone failed");
        trusted_git(&clone,&["checkout","--detach",&head]).await.unwrap();
        let mut environments=super::super::environment_evidence_against_main(&clone,scratch.path(),&prepared,&main).await.unwrap();
        environments.current_candidate=environment_id(&recovery.worktree).unwrap();
        let failure=if prepared.proposal.contract.verifiers.iter().any(|verifier|!verifier.command.controller_asserted()) {
            "legacy in-process verifier requires independent behavioral-oracle migration".to_owned()
        }else{BaselineNotRed {kind:Some(initial_repair::FailureKind::NotReproduced),detail:"baseline_witness_invalid: authenticated retained measurement is unknown; exact independent diagnosis retained privately".into()}.to_string()};
        let owner=super::super::authority_request_context(&issue.title,&recovery.source_request,Some((&negative_identity,&diagnosis_raw))).unwrap();
        let prompt=super::super::authority_base_request(&prepared,super::super::AuthorityFacts {source,request_context:&owner,failure:&failure,canonical_main:&main,
            scope_base:&basis,head:&head,diff:&diff,upstream_paths:&upstream,environments:&environments,adjudication:None}).unwrap();
        let evidence=read_bounded(&recovery.worktree,&control,base,&custody,context,replay.as_deref(),super::super::source_budget(&prompt).unwrap()).await.unwrap();
        assert_eq!(evidence.sources.len(),8);assert!(evidence.coverage.starts_with("complete selected original files"));
        let bytes=serde_json::to_vec(&evidence).unwrap();
        let constructed=super::super::construction_request(&prompt,&evidence).unwrap();
        let actual_request=super::super::initial_authority_request(&constructed).unwrap();
        let total=actual_request.len()+super::super::AUTHORITY_SYSTEM.len();
        assert!(total+super::super::CORRECTION_RESERVE<=super::super::REQUEST_LIMIT);
        assert_eq!(actual_head(&recovery.worktree).await.unwrap(),head,"production candidate moved during read-only preflight");
        let after=store.snapshot(&id).unwrap().unwrap();
        assert_eq!(after,before,"read-only source reconstruction changed ordinary workflow state");
        println!("Authenticated original immutable source: {} files, {} serialized bytes, coverage={}; evidence_sha256={}; no native calls or workflow writes",evidence.sources.len(),bytes.len(),evidence.coverage,digest(&bytes));
        println!("Complete historical authority preflight: request_bytes={}, system_bytes={}, total_bytes={}, reserved_correction_bytes={}, limit={}; request_sha256={}",actual_request.len(),super::super::AUTHORITY_SYSTEM.len(),total,super::super::CORRECTION_RESERVE,super::super::REQUEST_LIMIT,digest(actual_request.as_bytes()));
    }
}
