use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CcatError {
    #[error("CCat is not configured")]
    NotConfigured,
    #[error("CCat provider request failed: {0}")]
    Provider(String),
    #[error("CCat provider returned an invalid response: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionOutcome {
    Allow,
    Review,
    Block,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub outcome: DecisionOutcome,
    pub policy_id: String,
    pub policy_version: u32,
    pub payload_sha256: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub selected_probabilities: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionKind {
    YesNo,
    Category,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub id: &'static str,
    pub kind: QuestionKind,
    pub text: &'static str,
    pub options: &'static [(&'static str, &'static str)],
    pub block_when: &'static str,
    pub block_threshold: f64,
    pub review_when: &'static str,
    pub review_threshold: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub id: &'static str,
    pub version: u32,
    pub questions: &'static [Question],
    pub fail_closed: bool,
}

const PUSH_QUESTIONS: &[Question] = &[
    Question {
        id: "contains_private_or_proprietary_context",
        kind: QuestionKind::YesNo,
        text: "Does this redacted Git change disclose private or proprietary context that must not be public?",
        options: &[],
        block_when: "yes",
        block_threshold: 0.90,
        review_when: "yes",
        review_threshold: 0.50,
    },
    Question {
        id: "publication_decision",
        kind: QuestionKind::Category,
        text: "What publication action is appropriate for this redacted Git change?",
        options: &[
            ("allow", "Safe to publish after deterministic checks."),
            ("review", "Needs an operator review before publication."),
            ("block", "Must not be published."),
        ],
        block_when: "block",
        block_threshold: 0.80,
        review_when: "review",
        review_threshold: 0.50,
    },
];

pub const PUBLIC_GIT_PUSH_POLICY: Policy = Policy {
    id: "public_git_push",
    version: 1,
    questions: PUSH_QUESTIONS,
    fail_closed: true,
};

const EXTERNAL_MESSAGE_QUESTIONS: &[Question] = &[
    Question {
        id: "contains_private_context",
        kind: QuestionKind::YesNo,
        text: "Does this redacted outbound message disclose private context that must not be sent to its recipient?",
        options: &[],
        block_when: "yes",
        block_threshold: 0.90,
        review_when: "yes",
        review_threshold: 0.50,
    },
    Question {
        id: "send_decision",
        kind: QuestionKind::Category,
        text: "What action is appropriate for this redacted outbound message?",
        options: &[
            (
                "allow",
                "Safe to send after deterministic authorization checks.",
            ),
            ("review", "Needs an operator review before sending."),
            ("block", "Must not be sent."),
        ],
        block_when: "block",
        block_threshold: 0.80,
        review_when: "review",
        review_threshold: 0.50,
    },
];

/// Existing approval-gated sends can use this policy without widening their
/// authority: `Allow` retains the human approval requirement.
pub const EXTERNAL_MESSAGE_SEND_POLICY: Policy = Policy {
    id: "external_message_send",
    version: 1,
    questions: EXTERNAL_MESSAGE_QUESTIONS,
    fail_closed: false,
};

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub kind: QuestionKind,
    pub answer: String,
    pub probabilities: BTreeMap<String, f64>,
}

#[async_trait]
pub trait DecisionProvider: Send + Sync {
    async fn decide(&self, state: &str, policy: &Policy) -> Result<ProviderDecision, CcatError>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderDecision {
    pub provider: String,
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
}

/// Evaluate responses strictly. Any missing, extra, malformed, or unexpected
/// option means the caller cannot safely treat the response as authorization.
pub fn evaluate(
    policy: &Policy,
    payload_sha256: String,
    result: Result<ProviderDecision, CcatError>,
) -> Decision {
    let unavailable = |provider: Option<String>, model: Option<String>| Decision {
        outcome: DecisionOutcome::Unavailable,
        policy_id: policy.id.into(),
        policy_version: policy.version,
        payload_sha256: payload_sha256.clone(),
        provider,
        model,
        selected_probabilities: BTreeMap::new(),
    };
    let response = match result {
        Ok(value) => value,
        Err(_) => return unavailable(None, None),
    };
    if response.answers.len() != policy.questions.len() {
        return unavailable(Some(response.provider), Some(response.model));
    }
    let mut selected = BTreeMap::new();
    let mut outcome = DecisionOutcome::Allow;
    for question in policy.questions {
        let Some(answer) = response.answers.get(question.id) else {
            return unavailable(Some(response.provider), Some(response.model));
        };
        if answer.kind != question.kind {
            return unavailable(Some(response.provider), Some(response.model));
        }
        let expected: Vec<&str> = match question.kind {
            QuestionKind::YesNo => vec!["yes", "no"],
            QuestionKind::Category => question.options.iter().map(|(name, _)| *name).collect(),
        };
        if !expected.iter().any(|name| *name == answer.answer)
            || answer.probabilities.len() != expected.len()
            || expected
                .iter()
                .any(|name| !answer.probabilities.contains_key(*name))
            || answer
                .probabilities
                .values()
                .any(|p| !p.is_finite() || *p < 0.0 || *p > 1.0)
            || (answer.probabilities.values().sum::<f64>() - 1.0).abs() > 0.01
        {
            return unavailable(Some(response.provider), Some(response.model));
        }
        let probability = answer.probabilities[&answer.answer];
        selected.insert(question.id.to_string(), probability);
        if answer.answer == question.block_when && probability >= question.block_threshold {
            outcome = DecisionOutcome::Block;
        } else if outcome != DecisionOutcome::Block
            && answer.answer == question.review_when
            && probability >= question.review_threshold
        {
            outcome = DecisionOutcome::Review;
        }
    }
    Decision {
        outcome,
        policy_id: policy.id.into(),
        policy_version: policy.version,
        payload_sha256,
        provider: Some(response.provider),
        model: Some(response.model),
        selected_probabilities: selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(answer: &str, probability: f64) -> ProviderDecision {
        let mut answers = BTreeMap::new();
        answers.insert(
            "contains_private_or_proprietary_context".into(),
            Answer {
                kind: QuestionKind::YesNo,
                answer: answer.into(),
                probabilities: BTreeMap::from([
                    ("yes".into(), probability),
                    ("no".into(), 1.0 - probability),
                ]),
            },
        );
        answers.insert(
            "publication_decision".into(),
            Answer {
                kind: QuestionKind::Category,
                answer: "allow".into(),
                probabilities: BTreeMap::from([
                    ("allow".into(), 1.0),
                    ("review".into(), 0.0),
                    ("block".into(), 0.0),
                ]),
            },
        );
        ProviderDecision {
            provider: "test".into(),
            model: "test-1".into(),
            answers,
        }
    }
    #[test]
    fn high_confidence_sensitive_content_blocks() {
        assert_eq!(
            evaluate(
                &PUBLIC_GIT_PUSH_POLICY,
                "x".into(),
                Ok(response("yes", 0.91))
            )
            .outcome,
            DecisionOutcome::Block
        );
    }
    #[test]
    fn uncertain_sensitive_content_requires_review() {
        assert_eq!(
            evaluate(
                &PUBLIC_GIT_PUSH_POLICY,
                "x".into(),
                Ok(response("yes", 0.6))
            )
            .outcome,
            DecisionOutcome::Review
        );
    }
    #[test]
    fn provider_failure_is_unavailable() {
        assert_eq!(
            evaluate(
                &PUBLIC_GIT_PUSH_POLICY,
                "x".into(),
                Err(CcatError::Provider("nope".into()))
            )
            .outcome,
            DecisionOutcome::Unavailable
        );
    }
    #[test]
    fn missing_question_is_unavailable() {
        let mut response = response("no", 1.0);
        response.answers.remove("publication_decision");
        assert_eq!(
            evaluate(&PUBLIC_GIT_PUSH_POLICY, "x".into(), Ok(response)).outcome,
            DecisionOutcome::Unavailable
        );
    }
    #[test]
    fn unknown_answer_is_unavailable() {
        let mut response = response("no", 1.0);
        response
            .answers
            .get_mut("contains_private_or_proprietary_context")
            .unwrap()
            .answer = "maybe".into();
        assert_eq!(
            evaluate(&PUBLIC_GIT_PUSH_POLICY, "x".into(), Ok(response)).outcome,
            DecisionOutcome::Unavailable
        );
    }
    #[test]
    fn category_block_overrides_a_safe_yes_no_answer() {
        let mut response = response("no", 1.0);
        response
            .answers
            .get_mut("publication_decision")
            .unwrap()
            .answer = "block".into();
        response
            .answers
            .get_mut("publication_decision")
            .unwrap()
            .probabilities = BTreeMap::from([
            ("allow".into(), 0.05),
            ("review".into(), 0.05),
            ("block".into(), 0.90),
        ]);
        assert_eq!(
            evaluate(&PUBLIC_GIT_PUSH_POLICY, "x".into(), Ok(response)).outcome,
            DecisionOutcome::Block
        );
    }
}
