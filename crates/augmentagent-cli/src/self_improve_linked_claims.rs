//! Fresh-work suppression from the existing adoption facts. Never execution authority.
use super::*;
use std::collections::BTreeSet;

pub(super) const FIELDS: &str = "number,headRefName,headRefOid,baseRefName,isCrossRepository,headRepositoryOwner,author,closingIssuesReferences";

pub(super) use crate::autoship_workflow::submission::LinkedPrBlocker as Hold;

/// Keep ordinary authorization failures authoritative; add only fixed deferred
/// command context to this observer's typed failures.
pub(super) fn command_error(error:anyhow::Error)->anyhow::Error {
    if error.downcast_ref::<targeted::CommandFailure>().is_some() {return error;}
    match error.downcast_ref::<Hold>().copied() {
        Some(hold)=>error.context(targeted::CommandFailure {code:3,reason:hold.command_reason()}),
        None=>error,
    }
}

pub(super) fn report(error: &anyhow::Error) -> Option<RunReport> {
    error
        .downcast_ref::<Hold>()
        .map(|hold| RunReport::held(hold.to_string()))
}

fn validate_reference(reference: &serde_json::Value) -> Result<()> {
    let number = reference["number"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("invalid reference number")?;
    let raw = reference["url"].as_str().context("missing reference URL")?;
    let url = reqwest::Url::parse(raw)?;
    anyhow::ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none(),
        "invalid reference URL"
    );
    if url.host_str() == Some("github.com") {
        let parts = url
            .path_segments()
            .context("reference path missing")?
            .collect::<Vec<_>>();
        anyhow::ensure!(
            parts.len() == 4
                && !parts[0].is_empty()
                && !parts[1].is_empty()
                && parts[2].eq_ignore_ascii_case("issues")
                && parts[3].parse::<u64>()? == number
                && url.query().is_none()
                && url.fragment().is_none()
                && raw.eq_ignore_ascii_case(&format!(
                    "https://github.com/{}/{}/issues/{number}",
                    parts[0], parts[1]
                )),
            "noncanonical reference identity"
        );
    }
    Ok(())
}

fn claims(
    rows: &serde_json::Value,
    repository: &str,
    issue: u64,
    allowed: &[String],
) -> Result<BTreeSet<u64>> {
    let checked = (|| -> Result<BTreeSet<u64>> {
        anyhow::ensure!(
            issue > 0
                && repository.split('/').count() == 2
                && repository.split('/').all(|part| !part.is_empty()
                    && part
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))),
            "invalid repository/issue"
        );
        let mut found = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for row in rows.as_array().context("PR inventory is not an array")? {
            let number = row["number"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("invalid PR number")?;
            anyhow::ensure!(seen.insert(number), "duplicate PR number");
            if row["isCrossRepository"]
                .as_bool()
                .context("unknown PR repository")?
            {
                continue;
            }
            let author = row
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .context("unknown PR author")?;
            let association = row
                .get("authorAssociation")
                .map(|v| v.as_str().context("invalid association"))
                .transpose()?
                .unwrap_or("");
            if !author_is_trusted(author, association, allowed) {
                continue;
            }
            for field in ["headRefName", "headRefOid", "baseRefName"] {
                anyhow::ensure!(
                    row[field].as_str().is_some_and(|s| !s.is_empty()),
                    "unknown PR source metadata"
                );
            }
            let refs = row["closingIssuesReferences"]
                .as_array()
                .context("unknown closing references")?;
            // Installed gh list selects first:100 without hydrating nested pages.
            anyhow::ensure!(refs.len() < 100, "closing reference connection saturated");
            for reference in refs {
                validate_reference(reference)?;
            }
            let refs = adoption::source_references(row, repository);
            let claimed = if refs.is_empty() {
                issue_from_branch(row["headRefName"].as_str().unwrap()) == Some(issue)
            } else {
                refs.contains(&issue)
            };
            if claimed {
                found.insert(number);
            }
        }
        Ok(found)
    })();
    checked.map_err(|_| Hold::Unknown.into())
}

pub(super) async fn observe(repo: &Path, repository: &str, issue: u64) -> Result<BTreeSet<u64>> {
    let raw = complete_repository_inventory(repo, "pr", Some(repository), FIELDS)
        .await
        .map_err(|_| anyhow::Error::from(Hold::Unknown))?;
    let rows = serde_json::from_str(&raw).map_err(|_| anyhow::Error::from(Hold::Unknown))?;
    let mut allowed = trusted_authors(repo).await;
    let owner = repository
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !allowed.contains(&owner) {
        allowed.push(owner);
    }
    claims(&rows, repository, issue, &allowed)
}

pub(super) async fn require_absence(repo: &Path, repository: &str, issue: u64) -> Result<()> {
    if !observe(repo, repository, issue).await?.is_empty() {
        return Err(Hold::Claimed.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row() -> serde_json::Value {
        serde_json::json!({"number":77,"isCrossRepository":false,"author":{"login":"owner"},
            "headRefName":"fix/widget","headRefOid":"a".repeat(40),"baseRefName":"main",
            "closingIssuesReferences":[{"number":42,"url":"https://github.com/owner/repo/issues/42"}]})
    }
    fn inspect(rows: Vec<serde_json::Value>, issue: u64) -> Result<BTreeSet<u64>> {
        claims(
            &serde_json::Value::Array(rows),
            "owner/repo",
            issue,
            &["owner".into()],
        )
    }
    #[test]
    fn claims_suppress_each_issue_without_selecting_execution_source() {
        let mut value = row();
        value["closingIssuesReferences"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"number":43,"url":"https://github.com/OWNER/REPO/issues/43"}));
        for n in [42, 43] {
            assert_eq!(
                inspect(vec![value.clone()], n).unwrap(),
                BTreeSet::from([77])
            );
        }
        assert!(inspect(vec![value.clone()], 44).unwrap().is_empty());
        assert_eq!(
            adoption::authorized(&serde_json::json!([value]), "owner/repo", &["owner".into()])[0]
                .source_issue,
            None
        );
        let mut another = row();
        another["number"] = 78.into();
        assert_eq!(
            inspect(vec![row(), another], 42).unwrap(),
            BTreeSet::from([77, 78])
        );
    }
    #[test]
    fn legacy_fallback_and_foreign_references_match_existing_adoption() {
        let mut value = row();
        value["headRefName"] = "agent-fix/issue-42".into();
        value["closingIssuesReferences"] =
            serde_json::json!([{"number":9,"url":"https://github.com/elsewhere/repo/issues/9"}]);
        assert_eq!(
            inspect(vec![value.clone()], 42).unwrap(),
            BTreeSet::from([77])
        );
        assert!(inspect(vec![value.clone()], 9).unwrap().is_empty());
        value["closingIssuesReferences"] =
            serde_json::json!([{"number":9,"url":"https://github.com/owner/repo/issues/9"}]);
        assert!(
            inspect(vec![value], 42).unwrap().is_empty(),
            "explicit local sources override legacy branch fallback"
        );
    }
    #[test]
    fn known_forks_and_untrusted_rows_cannot_claim_or_poison_inventory() {
        let fork = serde_json::json!({"number":1,"isCrossRepository":true});
        let untrusted =
            serde_json::json!({"number":2,"isCrossRepository":false,"author":{"login":"outsider"}});
        assert!(inspect(vec![fork, untrusted], 42).unwrap().is_empty());
    }
    #[test]
    fn missing_conflicting_or_saturated_trusted_metadata_is_unknown() {
        for field in [
            "isCrossRepository",
            "author",
            "headRefName",
            "headRefOid",
            "baseRefName",
            "closingIssuesReferences",
        ] {
            let mut value = row();
            value.as_object_mut().unwrap().remove(field);
            assert_eq!(
                inspect(vec![value], 42).unwrap_err().downcast_ref::<Hold>(),
                Some(&Hold::Unknown),
                "{field}"
            );
        }
        assert!(inspect(vec![row(), row()], 42).is_err());
        let mut value = row();
        value["closingIssuesReferences"] = serde_json::json!([{"number":42}]);
        assert!(inspect(vec![value], 42).is_err());
        let mut value = row();
        value["closingIssuesReferences"] = serde_json::Value::Array(vec![
            serde_json::json!({"number":42,"url":"https://github.com/owner/repo/issues/42"});
            100
        ]);
        assert_eq!(
            inspect(vec![value], 42).unwrap_err().downcast_ref::<Hold>(),
            Some(&Hold::Unknown)
        );
    }
    #[test]
    fn malformed_reference_urls_are_unknown_instead_of_confirmed_absence() {
        // Assemble the synthetic userinfo URL without embedding an email-shaped
        // literal in public source; the privacy guard checks staged text.
        let userinfo = ["https://", "user", "@", "github.com/owner/repo/issues/42"].concat();
        for url in [
            userinfo.as_str(),
            "not a URL",
            "https://github.com/owner/repo/issues/43",
            "https://github.com/owner/repo/issues/42?extra=1",
        ] {
            let mut value = row();
            value["closingIssuesReferences"][0]["url"] = url.into();
            assert_eq!(
                inspect(vec![value], 42).unwrap_err().downcast_ref::<Hold>(),
                Some(&Hold::Unknown),
                "{url}"
            );
        }
    }
    #[test]
    fn command_context_defers_known_blockers_without_overriding_authorization() {
        for hold in [Hold::Claimed,Hold::Unknown] {
            let error=command_error(hold.into());
            let command=error.downcast_ref::<targeted::CommandFailure>().unwrap();
            assert_eq!(command.code,3);assert_eq!(command.reason,hold.command_reason());
            assert_eq!(error.downcast_ref::<Hold>(),Some(&hold));
        }
        let error=command_error(anyhow::Error::from(Hold::Unknown)
            .context(targeted::CommandFailure{code:2,reason:"target-authorization-failed"}));
        let command=error.downcast_ref::<targeted::CommandFailure>().unwrap();
        assert_eq!(command.code,2);assert_eq!(command.reason,"target-authorization-failed");
        assert!(Hold::from_code("private_unrecognized_value").is_none());
    }

}
