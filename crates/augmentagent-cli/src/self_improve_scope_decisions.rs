//! Owner-authorized conservative defaults. These are scoped implementation
//! constraints, never evidence that an external capability is available.
use super::*;

pub(super) const POLICY: &str = "safe-scope-defaults-v1";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Effect {
    Spend,
    SecurityPrivacy,
    Credentials,
    IrreversibleProduction,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Choice {
    pub decision: String,
    pub effects: Vec<Effect>,
    pub reversible: bool,
    pub default_off: bool,
    pub new_account_dependency: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum Alternative {
    Classified(Choice),
    Legacy(String),
}
impl From<&str> for Alternative {
    fn from(s: &str) -> Self {
        Self::Legacy(s.into())
    }
}
impl From<String> for Alternative {
    fn from(s: String) -> Self {
        Self::Legacy(s)
    }
}
impl std::fmt::Display for Alternative {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.text())
    }
}
impl Alternative {
    pub(super) fn text(&self) -> &str {
        match self {
            Self::Legacy(s) => s,
            Self::Classified(c) => &c.decision,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Assumption {
    pub slot: String,
    pub question: String,
    pub decision: String,
    pub constraints: String,
    /// Owner-only effects that every alternative carries (#2289). The
    /// flagged-off parts still build; only activation waits for the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<String>,
}
pub(super) const CONSTRAINTS:&str="Reversible local implementation only. Optional external backend remains disabled by default. No spend, new account dependency, credentials, security/privacy exposure, live external qualification, or irreversible production change is authorized.";

impl Effect {
    fn name(&self) -> &'static str {
        match self {
            Self::Spend => "spend",
            Self::SecurityPrivacy => "security_privacy",
            Self::Credentials => "credentials",
            Self::IrreversibleProduction => "irreversible_production",
        }
    }
}
/// Conservative reading of an unclassified (legacy) alternative. Any hint of
/// an owner-only effect counts as that effect: misreading a safe option as
/// sensitive only escalates, misreading a sensitive one as safe never happens
/// for these phrases.
fn infer(text: &str) -> Choice {
    let t = text.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| t.contains(w));
    let mut effects = vec![];
    if any(&["paid", "purchase", "buy ", "billing", "subscription", "spend", "pricing", "invoice", "cost"]) {
        effects.push(Effect::Spend);
    }
    if any(&["credential", "api key", "api-key", "secret", "token", "password", "oauth", "private key"]) {
        effects.push(Effect::Credentials);
    }
    if any(&["pii", "personal data", "privacy", "expose", "publicly", "disable auth", "bypass auth", "permission"]) {
        effects.push(Effect::SecurityPrivacy);
    }
    if any(&["delete", "drop ", "destroy", "purge", "irreversible", "force push", "force-push", "wipe"]) {
        effects.push(Effect::IrreversibleProduction);
    }
    Choice {
        decision: text.into(),
        effects,
        reversible: !any(&["irreversible", "permanent", "one-way"]),
        default_off: any(&["disabled", "default off", "default-off", "off by default", "behind a flag", "behind a config", "opt-in", "opt in", "existing", "local", "no-op", "dry run", "dry-run"]),
        new_account_dependency: any(&["new account", "sign up", "signup", "register an account", "third-party account"]),
    }
}
/// Pick the safe default (#2289). Routine options rank by reversible, then
/// default-off, then no new account; ties keep the model's order. When EVERY
/// option carries an owner-only effect, the assumption escalates to the
/// owner but still builds the non-blocked parts behind a default-off flag.
/// `strict` only shapes validator feedback for re-prompting; it never parks.
pub(super) fn choose(
    slot: &str,
    question: &str,
    options: &[Alternative],
    strict: bool,
) -> Result<Assumption> {
    let mut choices = Vec::with_capacity(options.len());
    for option in options {
        let choice = match option {
            Alternative::Legacy(_) if strict => bail!(
                "external_decision alternatives need structured decision/effects/reversible/default_off/new_account_dependency; include a conservative default-off option unless EVERY alternative requires spend, security_privacy, credentials or irreversible_production"
            ),
            Alternative::Legacy(text) => infer(text),
            Alternative::Classified(choice) => {
                ensure!(choice.effects.len() <= 4, "alternative effects exceed bound");
                if strict && choice.effects.is_empty() {
                    ensure!(
                        choice.reversible && choice.default_off && !choice.new_account_dependency,
                        "a routine alternative must be reversible, default_off=true, and new_account_dependency=false; do not escalate routine implementation choices"
                    );
                }
                choice.clone()
            }
        };
        choices.push(choice);
    }
    ensure!(!choices.is_empty(), "external decision has no alternatives");
    let rank = |c: &Choice| (c.reversible, c.default_off, !c.new_account_dependency);
    let mut routine: Option<&Choice> = None;
    for choice in choices.iter().filter(|c| c.effects.is_empty()) {
        if routine.is_none_or(|best| rank(choice) > rank(best)) {
            routine = Some(choice);
        }
    }
    if let Some(choice) = routine {
        return Ok(Assumption {
            slot: slot.into(),
            question: question.into(),
            decision: choice.decision.clone(),
            constraints: CONSTRAINTS.into(),
            escalation: None,
        });
    }
    let effects = choices
        .iter()
        .flat_map(|c| c.effects.iter().map(Effect::name))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Assumption {
        slot: slot.into(),
        question: question.into(),
        decision: format!(
            "Build the non-blocked parts behind a default-off flag; activate none of these until the owner decides: {}",
            choices.iter().map(|c| c.decision.as_str()).collect::<Vec<_>>().join(" | ")
        ),
        constraints: CONSTRAINTS.into(),
        escalation: Some(effects),
    })
}

pub(super) fn assumptions(assessment: &Assessment, strict: bool) -> Result<Vec<Assumption>> {
    let mut result = vec![];
    for (slot, value) in &assessment.slots {
        if let Slot::ExternalDecision {
            question,
            alternatives,
            ..
        } = value
        {
            result.push(choose(slot, question, alternatives, strict)?);
        }
    }
    Ok(result)
}
pub(super) fn require_classified(assessment: &Assessment) -> Result<()> {
    assumptions(assessment, true).map(|_| ())
}

/// Persist intent before publishing, and reconcile an uncertain comment by its
/// exact stable marker/body before retrying. A failed public write cannot let
/// an assumption silently proceed without the visible owner-facing record.
pub(super) async fn record(
    root: &Path,
    source: AcceptanceSource,
    workflow: &workflow::WorkflowGuard,
    assessment: &Assessment,
) -> Result<Vec<Assumption>> {
    let values = assumptions(assessment, false)?;
    for value in &values {
        let key = owner_checkout_recovery::scope_key(
            workflow.id(),
            amendment::digest(&serde_json::to_vec(&(
                POLICY,
                workflow.id(),
                &workflow.binding().contract,
                value,
            ))?),
        )?;
        let body=serde_json::json!({"version":1,"kind":"assumed_decision","policy":POLICY,"assumption":value}).to_string();
        workflow.record_scope_stage_response(&key, &body)?;
        let sent = amendment::digest(format!("assumed-decision-comment:{key}").as_bytes());
        if workflow.recorded_scope_stage_response(&sent)?.is_some() {
            continue;
        }
        // Routine bodies stay byte-identical so exact-body reconciliation of
        // comments posted before #2289 never duplicates them.
        let comment = match &value.escalation {
            None => format!(
                "Assumed decision\n\n{}: {}\n\n{}\n\n<!-- jarvis-assumed-decision:{key} -->",
                value.slot, value.decision, CONSTRAINTS
            ),
            Some(effects) => format!(
                "Assumed decision\n\n{}: {}\n\nOwner decision needed: every alternative involves {effects}. Autoship keeps building the non-blocked parts behind a default-off flag and activates none of these alternatives until the owner decides.\n\n{}\n\n<!-- jarvis-assumed-decision:{key} -->",
                value.slot, value.decision, CONSTRAINTS
            ),
        };
        augmentagent_channel_core::public_report::validate("", &comment)?;
        let number = match source {
            AcceptanceSource::Issue(n) | AcceptanceSource::PullRequest(n) => n,
        };
        let repository = workflow.repository();
        let mut found = false;
        for page in 1..=10 {
            workflow.validate_before_side_effect()?;
            let (ok, raw, error) = run(
                &gh_bin(),
                &[
                    "api",
                    &format!(
                        "repos/{repository}/issues/{number}/comments?per_page=100&page={page}"
                    ),
                ],
                root,
            )
            .await?;
            ensure!(
                ok,
                "assumed decision comment reconciliation unavailable: {}",
                truncate(&error, 200)
            );
            let comments: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
            if comments
                .iter()
                .any(|row| row["body"].as_str() == Some(comment.as_str()))
            {
                found = true;
                break;
            }
            if comments.len() < 100 {
                break;
            }
            ensure!(
                page < 10,
                "assumed decision comment scan exceeds bound; preserve for reconciliation"
            );
        }
        if !found {
            workflow.validate_before_side_effect()?;
            let (ok, _, error) = run(
                &gh_bin(),
                &[
                    "issue",
                    "comment",
                    &number.to_string(),
                    "--repo",
                    repository,
                    "--body",
                    &comment,
                ],
                root,
            )
            .await?;
            ensure!(
                ok,
                "assumed decision comment publication unavailable: {}",
                truncate(&error, 200)
            );
        }
        workflow.record_scope_stage_response(&sent,&serde_json::json!({"version":1,"kind":"assumed_decision_comment_recorded","assumption":key}).to_string())?;
    }
    Ok(values)
}

#[cfg(test)]
#[path = "self_improve_scope_decisions_tests.rs"]
mod tests;
