//! Twilio HTTP transport and request-signature verification.

use crate::{DialRequest, Error, ProviderError, Telephony};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::time::Duration;

pub struct TwilioClient {
    http: reqwest::Client,
    account: String,
    token: String,
    from: String,
    base: String,
}
impl TwilioClient {
    pub fn new(account: &str, token: &str, from: &str) -> Result<Self, Error> {
        if account.len() != 34
            || !account.starts_with("AC")
            || !account[2..].bytes().all(|b| b.is_ascii_hexdigit())
            || token.is_empty()
        {
            return Err(Error::NotConfigured);
        }
        Self::with_base(account, token, from, "https://api.twilio.com")
    }
    // Test endpoint injection stays private: production cannot forward credentials
    // to model-provided URLs or follow a redirect to another origin.
    fn with_base(account: &str, token: &str, from: &str, base: &str) -> Result<Self, Error> {
        augmentagent_store::phone::validate_e164(from).map_err(|_| Error::NotConfigured)?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::NotConfigured)?;
        Ok(Self {
            http,
            account: account.into(),
            token: token.into(),
            from: from.into(),
            base: base.into(),
        })
    }
    fn calls_url(&self) -> String {
        format!("{}/2010-04-01/Accounts/{}/Calls", self.base, self.account)
    }
}
#[async_trait]
impl Telephony for TwilioClient {
    async fn create(&self, request: DialRequest) -> Result<String, ProviderError> {
        let duration = request.max_duration_seconds.to_string();
        let form = vec![
            ("To", request.destination.expose()),
            ("From", &self.from),
            ("Twiml", &request.twiml),
            ("StatusCallback", &request.status_callback),
            ("StatusCallbackMethod", "POST"),
            ("StatusCallbackEvent", "initiated"),
            ("StatusCallbackEvent", "ringing"),
            ("StatusCallbackEvent", "answered"),
            ("StatusCallbackEvent", "completed"),
            ("Timeout", "30"),
            ("TimeLimit", &duration),
            ("Record", "false"),
        ];
        let response = self
            .http
            .post(format!("{}.json", self.calls_url()))
            .basic_auth(&self.account, Some(&self.token))
            .form(&form)
            .send()
            .await
            .map_err(|_| ProviderError::Uncertain)?;
        if !response.status().is_success() {
            return Err(
                if response.status().is_client_error() && response.status().as_u16() != 408 {
                    ProviderError::Rejected
                } else {
                    ProviderError::Uncertain
                },
            );
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| ProviderError::Uncertain)?;
        let sid = body["sid"]
            .as_str()
            .filter(|s| valid_sid(s))
            .ok_or(ProviderError::Uncertain)?;
        Ok(sid.into())
    }
    async fn hangup(&self, sid: &str) -> Result<(), ProviderError> {
        self.stop_call(sid, true).await
    }
    async fn hangup_known_state(&self, sid: &str, answered: bool) -> Result<(), ProviderError> {
        self.stop_call(sid, answered).await
    }
}
impl TwilioClient {
    async fn stop_call(&self, sid: &str, answered: bool) -> Result<(), ProviderError> {
        if !valid_sid(sid) {
            return Err(ProviderError::Rejected);
        }
        let url = format!("{}/{}.json", self.calls_url(), sid);
        let statuses = if answered {
            ["completed", "canceled"]
        } else {
            ["canceled", "completed"]
        };
        for status in statuses {
            // Send the mutation first. An unavailable/eventually-consistent GET
            // must not hold up a duration deadline or explicit stop request.
            let response = self
                .http
                .post(&url)
                .basic_auth(&self.account, Some(&self.token))
                .timeout(Duration::from_secs(5))
                .form(&[("Status", status)])
                .send()
                .await
                .map_err(|_| ProviderError::Uncertain)?;
            if response.status().is_success() {
                return Ok(());
            }
            if response.status() != reqwest::StatusCode::BAD_REQUEST {
                return Err(ProviderError::Uncertain);
            }
            let body: serde_json::Value = response
                .json()
                .await
                .map_err(|_| ProviderError::Uncertain)?;
            if body["code"].as_u64() != Some(21220) {
                return Err(ProviderError::Uncertain);
            }
            // Only a definite invalid-state rejection permits trying the other
            // terminal status. No retry on timeouts, redirects or server errors.
        }
        // Both mutations were rejected as invalid state. Already-ended calls
        // are idempotent success only with positive terminal-state evidence.
        let response = self
            .http
            .get(&url)
            .basic_auth(&self.account, Some(&self.token))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| ProviderError::Uncertain)?;
        if !response.status().is_success() {
            return Err(ProviderError::Uncertain);
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| ProviderError::Uncertain)?;
        if body["sid"].as_str() == Some(sid)
            && matches!(
                body["status"].as_str(),
                Some("completed" | "canceled" | "failed" | "busy" | "no-answer")
            )
        {
            Ok(())
        } else {
            Err(ProviderError::Uncertain)
        }
    }
}

fn valid_sid(s: &str) -> bool {
    s.len() == 34 && s.starts_with("CA") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Validate form callbacks against the configured external URL, not untrusted
/// Forwarded/Host headers. All form fields participate, including unknown ones.
/// HTTPS voice ingress uses the standard port; no URL normalization is performed.
pub fn validate_signature(
    token: &str,
    public_url: &str,
    fields: &[(String, String)],
    signature: &str,
) -> bool {
    let Ok(signature) = STANDARD.decode(signature) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(token.as_bytes()) else {
        return false;
    };
    mac.update(public_url.as_bytes());
    let mut sorted = fields.to_vec();
    sorted.sort();
    sorted.dedup();
    for (key, value) in sorted {
        mac.update(key.as_bytes());
        mac.update(value.as_bytes());
    }
    mac.verify_slice(&signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DialRequest, ProviderError, Telephony};
    use augmentagent_store::Store;
    use mockito::Matcher;

    fn request() -> DialRequest {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("db")).unwrap();
        s.phone_import_destination("owner", "+12025550123", 0)
            .unwrap();
        let c = s
            .phone_reserve_call(augmentagent_store::phone::ReserveCall {
                owner: "owner",
                request_id: "r",
                conversation: "c",
                now_ms: 1,
            })
            .unwrap();
        DialRequest {
            destination: s.phone_dial_destination(&c.call.id).unwrap(),
            twiml: "<Response><Hangup/></Response>".into(),
            status_callback: "https://phone.example.test/phone/status/id".into(),
            max_duration_seconds: 900,
        }
    }
    #[tokio::test]
    async fn create_sends_bounded_call_no_recording_and_callbacks_without_retry() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/2010-04-01/Accounts/ACtest/Calls.json")
            .match_header("authorization", Matcher::Regex("^Basic ".into()))
            .match_body(Matcher::AllOf(vec![
                Matcher::UrlEncoded("To".into(), "+12025550123".into()),
                Matcher::UrlEncoded("From".into(), "+12025550456".into()),
                Matcher::UrlEncoded("TimeLimit".into(), "900".into()),
                Matcher::UrlEncoded("Record".into(), "false".into()),
                Matcher::UrlEncoded("Timeout".into(), "30".into()),
                Matcher::UrlEncoded("StatusCallbackEvent".into(), "completed".into()),
            ]))
            .with_status(201)
            .with_body(r#"{"sid":"CA01234567890123456789012345678901"}"#)
            .expect(1)
            .create_async()
            .await;
        let client =
            TwilioClient::with_base("ACtest", "test-token", "+12025550456", &server.url()).unwrap();
        assert_eq!(
            client.create(request()).await.unwrap(),
            "CA01234567890123456789012345678901"
        );
        mock.assert_async().await;
    }
    #[tokio::test]
    async fn server_failure_and_malformed_success_are_uncertain_not_retryable() {
        for (status, body) in [(500, "failure"), (201, "{}")] {
            let mut server = mockito::Server::new_async().await;
            let mock = server
                .mock("POST", "/2010-04-01/Accounts/ACtest/Calls.json")
                .with_status(status)
                .with_body(body)
                .expect(1)
                .create_async()
                .await;
            let client =
                TwilioClient::with_base("ACtest", "test-token", "+12025550456", &server.url())
                    .unwrap();
            assert!(matches!(
                client.create(request()).await,
                Err(ProviderError::Uncertain)
            ));
            mock.assert_async().await;
        }
    }
    #[tokio::test]
    async fn rejected_credentials_are_terminal_without_echoing_provider_body() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/2010-04-01/Accounts/ACtest/Calls.json")
            .with_status(401)
            .with_body("sensitive provider text")
            .expect(1)
            .create_async()
            .await;
        let client =
            TwilioClient::with_base("ACtest", "test-token", "+12025550456", &server.url()).unwrap();
        let error = client.create(request()).await.unwrap_err();
        assert!(matches!(error, ProviderError::Rejected));
        assert!(!error.to_string().contains("sensitive"));
        mock.assert_async().await;
    }
    #[tokio::test]
    async fn hangup_posts_immediately_without_waiting_for_status_read() {
        let mut server = mockito::Server::new_async().await;
        let path = "/2010-04-01/Accounts/ACtest/Calls/CA01234567890123456789012345678901.json";
        let read = server
            .mock("GET", path)
            .with_status(503)
            .expect(0)
            .create_async()
            .await;
        let stop = server
            .mock("POST", path)
            .match_body(Matcher::UrlEncoded("Status".into(), "completed".into()))
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;
        let client =
            TwilioClient::with_base("ACtest", "test-token", "+12025550456", &server.url()).unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            client.hangup("CA01234567890123456789012345678901"),
        )
        .await
        .unwrap()
        .unwrap();
        stop.assert_async().await;
        read.assert_async().await;
    }

    #[tokio::test]
    async fn cancel_race_tries_completion_only_after_definite_invalid_state() {
        let mut server = mockito::Server::new_async().await;
        let path = "/2010-04-01/Accounts/ACtest/Calls/CA01234567890123456789012345678901.json";
        let cancel = server
            .mock("POST", path)
            .match_body(Matcher::UrlEncoded("Status".into(), "canceled".into()))
            .with_status(400)
            .with_body(r#"{"code":21220}"#)
            .expect(1)
            .create_async()
            .await;
        let complete = server
            .mock("POST", path)
            .match_body(Matcher::UrlEncoded("Status".into(), "completed".into()))
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;
        let read = server.mock("GET", path).expect(0).create_async().await;
        let client =
            TwilioClient::with_base("ACtest", "test-token", "+12025550456", &server.url()).unwrap();
        client
            .hangup_known_state("CA01234567890123456789012345678901", false)
            .await
            .unwrap();
        cancel.assert_async().await;
        complete.assert_async().await;
        read.assert_async().await;
    }
    #[tokio::test]
    async fn hangup_unknown_outcomes_never_trigger_a_second_mutation() {
        for (status, body) in [
            (500, "failure"),
            (401, "private rejection"),
            (302, "redirect"),
            (400, r#"{"code":20003}"#),
            (400, "malformed"),
        ] {
            let mut server = mockito::Server::new_async().await;
            let path = "/2010-04-01/Accounts/ACtest/Calls/CA01234567890123456789012345678901.json";
            let stop = server
                .mock("POST", path)
                .with_status(status)
                .with_body(body)
                .expect(1)
                .create_async()
                .await;
            let read = server.mock("GET", path).expect(0).create_async().await;
            let client =
                TwilioClient::with_base("ACtest", "test-token", "+12025550456", &server.url())
                    .unwrap();
            assert!(matches!(
                client.hangup("CA01234567890123456789012345678901").await,
                Err(ProviderError::Uncertain)
            ));
            stop.assert_async().await;
            read.assert_async().await;
        }
    }
    #[tokio::test]
    async fn already_ended_hangup_requires_matching_terminal_provider_evidence() {
        for (sid, status, success) in [
            ("CA01234567890123456789012345678901", "completed", true),
            ("CA01234567890123456789012345678901", "canceled", true),
            ("CA01234567890123456789012345678901", "in-progress", false),
            ("CA99999999999999999999999999999999", "completed", false),
        ] {
            let mut server = mockito::Server::new_async().await;
            let path = "/2010-04-01/Accounts/ACtest/Calls/CA01234567890123456789012345678901.json";
            let stop = server
                .mock("POST", path)
                .with_status(400)
                .with_body(r#"{"code":21220}"#)
                .expect(2)
                .create_async()
                .await;
            let read = server
                .mock("GET", path)
                .with_status(200)
                .with_body(serde_json::json!({"sid":sid,"status":status}).to_string())
                .expect(1)
                .create_async()
                .await;
            let client =
                TwilioClient::with_base("ACtest", "test-token", "+12025550456", &server.url())
                    .unwrap();
            assert_eq!(
                client
                    .hangup("CA01234567890123456789012345678901")
                    .await
                    .is_ok(),
                success
            );
            stop.assert_async().await;
            read.assert_async().await;
        }
    }

    #[test]
    fn signature_requires_exact_public_url_all_parameters_and_authentic_signature() {
        let fields = vec![
            ("CallSid".into(), "CA1234567890ABCDE".into()),
            ("Caller".into(), "+14158675310".into()),
            ("Digits".into(), "1234".into()),
            ("From".into(), "+14158675310".into()),
            ("To".into(), "+18005551212".into()),
        ];
        // Published Twilio security example (token 12345).
        let url = "https://example.com/myapp.php?foo=1&bar=2";
        let signature = "L/OH5YylLD5NRKLltdqwSvS0BnU=";
        assert!(validate_signature("12345", url, &fields, signature));
        assert!(!validate_signature("wrong", url, &fields, signature));
        assert!(!validate_signature(
            "12345",
            "http://localhost/myapp.php?foo=1&bar=2",
            &fields,
            signature
        ));
        let mut altered = fields.clone();
        altered.push(("NewField".into(), "value".into()));
        assert!(!validate_signature("12345", url, &altered, signature));
        assert!(!validate_signature("12345", url, &fields, "invalid-base64"));
    }
}
