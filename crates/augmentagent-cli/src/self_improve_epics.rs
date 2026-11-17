//! #2179: autoship never selects an epic itself; it works the epic's children.
//! An open epic with no open child issues is flagged once for breakdown.
//! No model calls; only plain GitHub label/comment calls when flagging.
use super::*;

pub(super) const EPIC_LABEL:&str="epic";
pub(super) const BREAKDOWN_LABEL:&str="needs-breakdown";
pub(super) const SKIP_EPIC:&str="skip: epic; autoship works its children";

/// Labeled `epic`, or titled `[EPIC] …`, `EPIC: …`, `Epic: …` (any case).
pub(super) fn is_epic(_iss:&serde_json::Value)->bool {false}

/// Open epics (not yet flagged) with zero open non-PR child issues.
pub(super) fn childless_epics(_rows:&[serde_json::Value])->Vec<u64> {Vec::new()}

pub(super) fn breakdown_comment(epic:u64)->String {
    format!("This epic has no open child issues, so autoship can't work it. Break it into scoped child issues (`Part of #{epic}`) with testable acceptance criteria.")
}

/// Best effort: never fails selection.
pub(super) async fn flag_childless_epics(_repo_root:&Path,_rows:&[serde_json::Value]) {}

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
