//! SeaCat transport. The provider is deliberately small: policy evaluation
//! remains local and rejects any response outside the configured contract.

use crate::policy::{Answer, CcatError, DecisionProvider, Policy, ProviderDecision, QuestionKind};
use async_trait::async_trait;
use augmentagent_auth::{Auth, AuthError};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, env, time::Duration};

const API_KEY_NAME: &str = "SEACAT_API_KEY";
const SECRET_SERVICE_API_KEY: &str = "api-key";
const DEFAULT_ENDPOINT: &str = "https://seacat.dev";

#[derive(Clone)]
pub struct SeaCatDecisionProvider {
    client: Client,
    endpoint: String,
    api_key: String,
}

impl SeaCatDecisionProvider {
    /// Build the production provider. The keyring is preferred to the
    /// owner-only environment fallback used on keyring-less hosts.
    pub fn from_environment() -> Result<Self, CcatError> {
        let key = match Auth::get(SECRET_SERVICE_API_KEY, API_KEY_NAME) {
            Ok(value) => String::from_utf8(value).map_err(|_| CcatError::NotConfigured)?,
            Err(AuthError::NotFound { .. }) | Err(AuthError::Keyring(_)) => {
                env::var(API_KEY_NAME).map_err(|_| CcatError::NotConfigured)?
            }
        };
        if key.trim().is_empty() {
            return Err(CcatError::NotConfigured);
        }
        Self::new(DEFAULT_ENDPOINT, key)
    }

    pub fn new(endpoint: &str, api_key: String) -> Result<Self, CcatError> {
        let endpoint = endpoint.trim_end_matches('/').to_string();
        if endpoint.is_empty() {
            return Err(CcatError::NotConfigured);
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(180))
            .redirect(reqwest::redirect::Policy::limited(8))
            .build()
            .map_err(|e| CcatError::Provider(e.to_string()))?;
        Ok(Self {
            client,
            endpoint,
            api_key,
        })
    }

    fn body(state: &str, policy: &Policy) -> Value {
        let mut questions = Map::new();
        for question in policy.questions {
            let value = match question.kind {
                QuestionKind::YesNo => json!({ "type": "yes_no", "text": question.text }),
                QuestionKind::Category => {
                    let options: Map<String, Value> = question
                        .options
                        .iter()
                        .map(|(name, description)| {
                            (
                                (*name).to_string(),
                                Value::String((*description).to_string()),
                            )
                        })
                        .collect();
                    json!({ "type": "category", "text": question.text, "options": options })
                }
            };
            questions.insert(question.id.into(), value);
        }
        json!({ "state": state, "questions": questions })
    }

    async fn request(&self, body: &Value) -> Result<SeaCatResponse, CcatError> {
        // A failed POST may have completed remotely, so retry only responses
        // SeaCat documents as transient and never retry validation failures.
        for attempt in 0..3 {
            let response = self
                .client
                .post(format!("{}/v1/decide", self.endpoint))
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json()
                        .await
                        .map_err(|e| CcatError::InvalidResponse(e.to_string()));
                }
                Ok(response)
                    if matches!(response.status(), StatusCode::TOO_MANY_REQUESTS)
                        || response.status().is_server_error() =>
                {
                    if attempt == 2 {
                        return Err(CcatError::Provider(format!(
                            "transient HTTP {}",
                            response.status()
                        )));
                    }
                    let delay = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(1 << attempt);
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
                Ok(response) => {
                    return Err(CcatError::Provider(format!("HTTP {}", response.status())));
                }
                Err(error) => {
                    if attempt == 2 {
                        return Err(CcatError::Provider(error.to_string()));
                    }
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
            }
        }
        Err(CcatError::Provider("retry loop exhausted".into()))
    }
}

#[derive(Deserialize)]
struct SeaCatResponse {
    model: String,
    answers: BTreeMap<String, SeaCatAnswer>,
}
#[derive(Deserialize)]
struct SeaCatAnswer {
    #[serde(rename = "type")]
    kind: String,
    answer: String,
    probabilities: BTreeMap<String, f64>,
}

#[async_trait]
impl DecisionProvider for SeaCatDecisionProvider {
    async fn decide(&self, state: &str, policy: &Policy) -> Result<ProviderDecision, CcatError> {
        let response = self.request(&Self::body(state, policy)).await?;
        let answers = response
            .answers
            .into_iter()
            .map(|(id, value)| {
                let kind = match value.kind.as_str() {
                    "yes_no" => QuestionKind::YesNo,
                    "category" => QuestionKind::Category,
                    _ => return Err(CcatError::InvalidResponse("unknown answer type".into())),
                };
                Ok((
                    id,
                    Answer {
                        kind,
                        answer: value.answer,
                        probabilities: value.probabilities,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok(ProviderDecision {
            provider: "seacat".into(),
            model: response.model,
            answers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DecisionProvider, evaluate, policy::PUBLIC_GIT_PUSH_POLICY};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    fn body() -> Value {
        json!({ "model": "seacat-1", "answers": {
            "contains_private_or_proprietary_context": { "type": "yes_no", "answer": "no", "probabilities": { "yes": 0.01, "no": 0.99 } },
            "publication_decision": { "type": "category", "answer": "allow", "probabilities": { "allow": 0.98, "review": 0.01, "block": 0.01 } }
        }})
    }
    #[tokio::test]
    async fn sends_only_a_typed_decision_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/decide"))
            .and(header("authorization", "Bearer test-key"))
            .and(body_json(json!({
                "state": "redacted state",
                "questions": {
                    "contains_private_or_proprietary_context": {
                        "type": "yes_no",
                        "text": "Does this redacted Git change disclose private or proprietary context that must not be public?"
                    },
                    "publication_decision": {
                        "type": "category",
                        "text": "What publication action is appropriate for this redacted Git change?",
                        "options": {
                            "allow": "Safe to publish after deterministic checks.",
                            "review": "Needs an operator review before publication.",
                            "block": "Must not be published."
                        }
                    }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(body()))
            .expect(1)
            .mount(&server)
            .await;
        let provider = SeaCatDecisionProvider::new(&server.uri(), "test-key".into()).unwrap();
        let decision = provider
            .decide("redacted state", &PUBLIC_GIT_PUSH_POLICY)
            .await
            .unwrap();
        assert_eq!(decision.model, "seacat-1");
        assert_eq!(decision.answers.len(), 2);
    }
    #[tokio::test]
    async fn malformed_json_is_not_a_decision() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let provider = SeaCatDecisionProvider::new(&server.uri(), "test-key".into()).unwrap();
        assert!(matches!(
            provider.decide("state", &PUBLIC_GIT_PUSH_POLICY).await,
            Err(CcatError::InvalidResponse(_))
        ));
    }

    #[tokio::test]
    async fn unexpected_answer_type_is_not_a_decision() {
        let server = MockServer::start().await;
        let mut response = body();
        response["answers"]["publication_decision"]["type"] = json!("yes_no");
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;
        let provider = SeaCatDecisionProvider::new(&server.uri(), "test-key".into()).unwrap();
        let result = provider.decide("state", &PUBLIC_GIT_PUSH_POLICY).await;
        assert_eq!(
            evaluate(&PUBLIC_GIT_PUSH_POLICY, "synthetic".into(), result).outcome,
            crate::policy::DecisionOutcome::Unavailable
        );
    }
}
