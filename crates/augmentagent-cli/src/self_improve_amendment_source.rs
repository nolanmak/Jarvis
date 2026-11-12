//! Immutable source data for correction construction, never witness approval.
use super::*;

#[derive(Debug, Serialize)]
pub(super) struct Evidence {
    original_base: String,
    selection_identity: String,
    coverage: &'static str,
    sources: BTreeMap<String, String>,
}

#[derive(Deserialize)]
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

/// Frozen indexed-source protocol: this only reuses selections produced under
/// the unchanged current scope producer. It imports data, never an old ruling.
fn selection(
    root: &Path, control: &Path, base: &str,
    custody: &[crate::autoship_workflow::NativeScopeCustody], context: Option<&str>,
) -> Result<(String, Selection)> {
    ensure!(matches!(base.len(), 40 | 64) && base.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)), "invalid original source commit");
    scope_authority::require_retained_native_custody(root, custody)?;
    let control = control.canonicalize()?;
    let mut selected = None;
    for observation in custody {
        let reference: scope_authority::NativeReplyReference = serde_json::from_str(&observation.provenance)?;
        ensure!(reference.artifact.parent() == Some(control.as_path()), "amendment source belongs to another workflow");
        let bytes = baseline_witness::private_bytes(&reference.artifact, 64 * 1024)?;
        ensure!(digest(&bytes) == reference.artifact_sha256, "amendment source proof changed during inspection");
        let proof: serde_json::Value = serde_json::from_slice(&bytes)?;
        if proof["head"] != base || proof["purpose"] != "source_selection"
            || context.is_some_and(|expected| proof["scope_context_hash"].as_str() != Some(expected)) { continue; }
        ensure!(proof["scope_policy_hash"].as_str() == Some(scope_authority::witness_policy_hash().as_str()), "original source selection uses an unsupported producer protocol");
        ensure!(selected.is_none(), "ambiguous original source selection");
        let raw = observation.response.trim();
        let raw = raw.strip_prefix("```json\n").and_then(|s| s.strip_suffix("```")).unwrap_or(raw);
        let selection: Selection = serde_json::from_str(raw)?;
        ensure!(!selection.requests.is_empty() && selection.requests.len() <= 16, "invalid original source selection size");
        for request in &selection.requests {
            ensure!(request.start_line > 0 && request.end_line >= request.start_line && request.end_line - request.start_line < 600, "invalid original source range");
            ensure!(request.needle.as_ref().is_none_or(|needle| !needle.is_empty() && needle.len() <= 256), "invalid original source search");
        }
        let identity = digest(&serde_json::to_vec(&("amendment-original-source-selection-v1", base, &observation.input, &observation.response, &reference))?);
        selected = Some((identity, selection));
    }
    selected.context("original source selection unavailable")
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
}
impl<'a> SourceBuffer<'a> {
    fn new(request:&'a ReadRequest,budget:usize)->Self {
        Self {request,budget,full:Some(String::new()),pending:String::new(),previous:Default::default(),selected:BTreeMap::new(),
            line:0,matches:0,through:0,selected_bytes:0,previous_bytes:0}
    }
    fn retain(&mut self,number:usize,line:String)->Result<()> {
        if !self.selected.contains_key(&number) {
            self.selected_bytes=self.selected_bytes.checked_add(line.len()+32).context("source size overflow")?;
            ensure!(self.selected_bytes<=self.budget,"original source windows exceed bounded working set");
            self.selected.insert(number,line);
        }
        Ok(())
    }
    fn line(&mut self,line:String)->Result<()> {
        self.line+=1;
        if let Some(needle)=&self.request.needle {
            if self.matches<5 && line.contains(needle) {
                self.matches+=1;self.through=self.line.saturating_add(99);
                for (number,previous) in self.previous.clone() {self.retain(number,previous)?;}
            }
            if self.line<=self.through {self.retain(self.line,line.clone())?;}
            self.previous_bytes+=line.len();self.previous.push_back((self.line,line));
            if self.previous.len()>50 {self.previous_bytes-=self.previous.pop_front().unwrap().1.len();}
            ensure!(self.previous_bytes<=self.budget,"original source context exceeds bounded working set");
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
    fn finish(mut self)->Result<(Option<String>,String)> {
        if !self.pending.is_empty() {let line=std::mem::take(&mut self.pending);self.line(line)?;}
        let mut excerpt=String::new();
        for (number,line) in self.selected {excerpt.push_str(&format!("{number}: {line}"));if !line.ends_with('\n') {excerpt.push('\n');}}
        Ok((self.full,excerpt))
    }
}

pub(super) async fn read(
    root: &Path, control: &Path, base: &str,
    custody: &[crate::autoship_workflow::NativeScopeCustody], context: Option<&str>,
) -> Result<Evidence> {
    read_bounded(root, control, base, custody, context, 384 * 1024).await
}

pub(super) async fn read_bounded(
    root: &Path, control: &Path, base: &str,
    custody: &[crate::autoship_workflow::NativeScopeCustody], context: Option<&str>, budget: usize,
) -> Result<Evidence> {
    ensure!((1024..=512 * 1024).contains(&budget), "invalid amendment source budget");
    let (identity, selected) = selection(root, control, base, custody, context)?;
    let tree = trusted_git(root, &["--no-replace-objects", "ls-tree", "-r", "--long", "-z", base]).await?;
    ensure!(tree.len() <= 1024 * 1024, "original source table exceeds bound");
    // Preserve the original producer's ordinals, including literal Git paths.
    let entries = tree.split('\0').filter(|entry| !entry.is_empty()).map(|entry| {
        entry.split_once('\t').context("invalid original source table")
    }).collect::<Result<Vec<_>>>()?;
    let entries = entries.into_iter().filter(|(metadata, path)| metadata.split_whitespace().nth(1) == Some("blob")
        && scope_authority::witness_source_allowed(path)).collect::<Vec<_>>();
    let mut full = BTreeMap::new();
    let mut partial = BTreeMap::new();
    let mut complete = true;
    for request in selected.requests {
        let (_, path) = entries.get(request.file_index).context("original source ordinal outside immutable table")?;
        let mut buffer=SourceBuffer::new(&request,budget);
        aligned_review::visit_immutable_source(root,base,path,|chunk|buffer.push(chunk)).await?;
        let (content,excerpt)=buffer.finish()?;
        partial.insert(format!("{path}:{}-{}:{:?}", request.start_line, request.end_line, request.needle), excerpt);
        if let Some(content)=content {full.insert((*path).to_owned(),content);}else{complete=false;}
    }
    // Complete files avoid losing a production call below the original search
    // windows when they fit. Larger selections retain exactly bounded windows;
    // the final authority is explicitly told that omitted bytes are unread.
    let mut evidence = Evidence { original_base: base.into(), selection_identity: identity,
        coverage: "complete selected original files; selection is data only, not qualification or complete repository coverage", sources: full };
    if !complete || serde_json::to_vec(&evidence)?.len() > budget {
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

    async fn fixture() -> (tempfile::TempDir, tempfile::TempDir, String, Vec<crate::autoship_workflow::NativeScopeCustody>) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let control = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(control.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.path().join("behavior.rs"), "pub fn behavior() -> bool { false }\n").unwrap();
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
        assert_eq!(partial,"2: fn first() {}\n3: fn second() {}\n");
        let mut buffer=SourceBuffer::new(&request,1024);
        buffer.push(complete).unwrap();
        for _ in 0..1024 {buffer.push("// unrelated later source\n").unwrap();}
        let (full,partial)=buffer.finish().unwrap();
        assert!(full.is_none(),"large files were retained without a bound");
        assert_eq!(partial,"2: fn first() {}\n3: fn second() {}\n");
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
        let (_,diagnosis)=baseline_witness::retained_semantic_diagnosis_in(&recovery.worktree,&control,&prepared,&custody).unwrap().unwrap();
        let diagnosis:serde_json::Value=serde_json::from_str(&diagnosis).unwrap();
        let evidence=read(&recovery.worktree,&control,prepared.baseline.registration.baseline_head(),&custody,diagnosis["context"].as_str()).await.unwrap();
        let bytes=serde_json::to_vec(&evidence).unwrap();
        assert!(!evidence.sources.is_empty());
        let after=store.snapshot(&id).unwrap().unwrap();
        assert_eq!(after,before,"read-only source reconstruction changed ordinary workflow state");
        println!("Authenticated original immutable source: {} files, {} serialized bytes, coverage={}; evidence_sha256={}; no native calls or workflow writes",evidence.sources.len(),bytes.len(),evidence.coverage,digest(&bytes));
    }
}
