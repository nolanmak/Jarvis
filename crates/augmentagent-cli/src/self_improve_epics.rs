//! #2179: autoship never selects an epic itself; it works the epic's children.
//! An open epic with no open child issues is flagged once for breakdown.
//! No model calls; only plain GitHub label/comment calls when flagging.
use super::*;

pub(super) const EPIC_LABEL:&str="epic";
pub(super) const BREAKDOWN_LABEL:&str="needs-breakdown";
pub(super) const SKIP_EPIC:&str="skip: epic; autoship works its children";

/// Labeled `epic`, or titled `[EPIC] …`, `EPIC: …`, `Epic: …` (any case).
pub(super) fn is_epic(iss:&serde_json::Value)->bool {
    if has_label(iss,EPIC_LABEL) {return true;}
    let title=iss.get("title").and_then(serde_json::Value::as_str).unwrap_or("").trim_start().to_ascii_lowercase();
    title.starts_with("[epic]") || title.starts_with("epic:")
}

fn number(iss:&serde_json::Value)->Option<u64> {iss.get("number").and_then(serde_json::Value::as_u64)}
fn body(iss:&serde_json::Value)->String {iss.get("body").and_then(serde_json::Value::as_str).unwrap_or("").to_ascii_lowercase()}

/// `#N` occurrences after `prefix` in `text`, where N ends at a non-digit.
fn refs_after(text:&str,prefix:&str)->Vec<u64> {
    text.match_indices(prefix).filter_map(|(at,_)|{
        let digits:String=text[at+prefix.len()..].chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }).collect()
}

/// Open epics (not yet flagged) with zero open non-PR child issues.
pub(super) fn childless_epics(rows:&[serde_json::Value])->Vec<u64> {
    let issues:Vec<&serde_json::Value>=rows.iter().filter(|r|r.get("pull_request").is_none()).collect();
    let open:std::collections::HashSet<u64>=issues.iter().filter_map(|r|number(r)).collect();
    issues.iter().filter(|r|is_epic(r) && !has_label(r,BREAKDOWN_LABEL)).filter_map(|epic|{
        let n=number(epic)?;
        let part_of=issues.iter().any(|r|number(r)!=Some(n) && refs_after(&body(r),"part of #").contains(&n));
        let own=body(epic);
        let listed=["- [ ] #","- [x] #"].iter().flat_map(|p|refs_after(&own,p)).any(|m|m!=n && open.contains(&m));
        (!part_of && !listed).then_some(n)
    }).collect()
}

pub(super) fn breakdown_comment(epic:u64)->String {
    format!("This epic has no open child issues, so autoship can't work it. Break it into scoped child issues (`Part of #{epic}`) with testable acceptance criteria.")
}

/// Best effort: never fails selection.
pub(super) async fn flag_childless_epics(repo_root:&Path,rows:&[serde_json::Value]) {
    let epics=childless_epics(rows);
    if epics.is_empty() {return;}
    let gh=gh_bin();
    ensure_label(repo_root,BREAKDOWN_LABEL,"d93f0b","Epic needs scoped child issues before autoship can work it").await;
    for epic in epics {
        // Label first: the label is the idempotency marker later ticks read.
        let issue=epic.to_string();
        match run(&gh,&["issue","edit",&issue,"--add-label",BREAKDOWN_LABEL],repo_root).await {
            Ok((true,_,_))=>{},
            other=>{warn!(issue=epic,ok=other.as_ref().is_ok_and(|r|r.0),"epic breakdown label failed; retry next tick");continue;}
        }
        match run(&gh,&["issue","comment",&issue,"--body",&breakdown_comment(epic)],repo_root).await {
            Ok((true,_,_))=>info!(issue=epic,"epic has no open child issues; flagged {BREAKDOWN_LABEL}"),
            _=>{
                // Without the comment, retract the label so the next tick retries both.
                warn!(issue=epic,"epic breakdown comment failed; retracting label for retry");
                let _=run(&gh,&["issue","edit",&issue,"--remove-label",BREAKDOWN_LABEL],repo_root).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(number:u64,title:&str,body:&str,labels:&[&str])->serde_json::Value {
        serde_json::json!({"number":number,"title":title,"body":body,"labels":labels.iter().map(|l|serde_json::json!({"name":l})).collect::<Vec<_>>()})
    }
    #[test]
    fn epics_are_recognised_by_label_or_title_prefix_only() {
        assert!(is_epic(&row(1,"Ship things","",&["epic"])));
        for title in ["[EPIC] x","EPIC: x","Epic: x","[epic] x","epic: x","  Epic: x"] {assert!(is_epic(&row(1,title,"",&[])),"{title}");}
        for title in ["Fix the epic rounding bug","Epicurean menu","An EPIC: not a prefix","Epic x"] {assert!(!is_epic(&row(1,title,"",&[])),"{title}");}
    }
    #[test]
    fn childless_epics_count_only_open_non_pr_children() {
        let epic=row(42,"Epic: widgets","Tracking.",&["epic"]);
        assert_eq!(childless_epics(&[epic.clone(),row(41,"Fix","Part of #42",&[])]),Vec::<u64>::new(),"Part of child");
        assert_eq!(childless_epics(&[epic.clone(),row(41,"Fix","part of #42.",&[])]),Vec::<u64>::new(),"case-insensitive");
        assert_eq!(childless_epics(&[epic.clone(),row(41,"Fix","Part of #420",&[])]),vec![42],"#420 is not #42");
        let mut pr=row(41,"PR","Part of #42",&[]);pr["pull_request"]=serde_json::json!({});
        assert_eq!(childless_epics(&[epic.clone(),pr]),vec![42],"a PR is not a child issue");
        let listed=row(42,"Epic: widgets","- [ ] #41\n- [ ] #39",&["epic"]);
        assert_eq!(childless_epics(&[listed.clone(),row(41,"Fix","",&[])]),Vec::<u64>::new(),"task-list child");
        assert_eq!(childless_epics(&[listed]),vec![42],"task-list link to a closed (absent) issue");
        assert_eq!(childless_epics(&[row(42,"Epic: w","",&["epic","needs-breakdown"])]),Vec::<u64>::new(),"already flagged");
        assert_eq!(childless_epics(&[row(7,"Plain","",&[])]),Vec::<u64>::new());
        let nested=row(43,"[EPIC] parent","",&[]);
        assert_eq!(childless_epics(&[nested,row(42,"Epic: child epic","Part of #43",&["epic"])]),vec![42],"a child epic is a child; itself still childless");
    }
}
