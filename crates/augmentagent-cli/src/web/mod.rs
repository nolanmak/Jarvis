//! Web search and page fetch for models that cannot run native web tools
//! (#2200, spec #2242). Six vendors; configured credentials decide which are
//! usable. This module is the shared core: ids, availability, pin
//! resolution, failover, deadlines and error redaction. `tool` holds the MCP
//! server and the per-turn attachment.
#![allow(dead_code)] // wired into turns by #2202

pub mod pins;
pub mod command;
pub mod providers;
pub mod tool;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tool_tests;
#[cfg(test)]
mod command_tests;
#[cfg(test)]
mod provider_tests;

use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProviderId {
    Brave,
    Exa,
    Firecrawl,
    Context,
    Serpapi,
    Brightdata,
}

impl ProviderId {
    pub const ALL: [ProviderId; 6] = [
        ProviderId::Brave,
        ProviderId::Exa,
        ProviderId::Firecrawl,
        ProviderId::Context,
        ProviderId::Serpapi,
        ProviderId::Brightdata,
    ];

    pub fn id(self) -> &'static str {
        match self {
            ProviderId::Brave => "brave",
            ProviderId::Exa => "exa",
            ProviderId::Firecrawl => "firecrawl",
            ProviderId::Context => "context",
            ProviderId::Serpapi => "serpapi",
            ProviderId::Brightdata => "brightdata",
        }
    }

    /// Ids are trimmed and case-insensitive wherever an owner or model types one.
    pub fn parse(text: &str) -> Option<Self> {
        let wanted = text.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|id| id.id() == wanted)
    }

    /// Vendor API origin. Clients take the base as an argument so tests can
    /// point them at a mock server; it is never read from the environment.
    pub fn default_base(self) -> &'static str {
        match self {
            ProviderId::Brave => "https://api.search.brave.com",
            ProviderId::Exa => "https://api.exa.ai",
            ProviderId::Firecrawl => "https://api.firecrawl.dev",
            ProviderId::Context => "https://api.context.dev",
            ProviderId::Serpapi => "https://serpapi.com",
            ProviderId::Brightdata => "https://api.brightdata.com",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Search,
    Fetch,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Search => "search",
            Kind::Fetch => "fetch",
        }
    }
}

/// First available provider wins when nothing is pinned.
pub const SEARCH_ORDER: [ProviderId; 6] = [
    ProviderId::Brave,
    ProviderId::Exa,
    ProviderId::Firecrawl,
    ProviderId::Context,
    ProviderId::Serpapi,
    ProviderId::Brightdata,
];
pub const FETCH_ORDER: [ProviderId; 4] = [
    ProviderId::Firecrawl,
    ProviderId::Exa,
    ProviderId::Context,
    ProviderId::Brightdata,
];

fn order(kind: Kind) -> &'static [ProviderId] {
    match kind {
        Kind::Search => &SEARCH_ORDER,
        Kind::Fetch => &FETCH_ORDER,
    }
}

pub const SERPAPI_KEY: &str = "SERPAPI_KEY";
const SERPAPI_KEY_ALIAS: &str = "SERPAPI_API_KEY";
pub const BRIGHTDATA_API_KEY: &str = "BRIGHTDATA_API_KEY";
pub const BRIGHTDATA_SERP_ZONE: &str = "BRIGHTDATA_SERP_ZONE";
pub const BRIGHTDATA_UNLOCKER_ZONE: &str = "BRIGHTDATA_UNLOCKER_ZONE";

/// Canonical credential names a provider needs for one kind; `None` when the
/// vendor has no such product.
fn required(id: ProviderId, kind: Kind) -> Option<&'static [&'static str]> {
    match (id, kind) {
        (ProviderId::Brave, Kind::Search) => Some(&["BRAVE_API_KEY"]),
        (ProviderId::Brave, Kind::Fetch) => None,
        (ProviderId::Exa, _) => Some(&["EXA_API_KEY"]),
        (ProviderId::Firecrawl, _) => Some(&["FIRECRAWL_API_KEY"]),
        (ProviderId::Context, _) => Some(&["CONTEXT_DEV_API_KEY"]),
        (ProviderId::Serpapi, Kind::Search) => Some(&[SERPAPI_KEY]),
        (ProviderId::Serpapi, Kind::Fetch) => None,
        (ProviderId::Brightdata, Kind::Search) => Some(&[BRIGHTDATA_API_KEY, BRIGHTDATA_SERP_ZONE]),
        (ProviderId::Brightdata, Kind::Fetch) => {
            Some(&[BRIGHTDATA_API_KEY, BRIGHTDATA_UNLOCKER_ZONE])
        }
    }
}

/// Present credentials under their canonical names. Values never appear in
/// `Debug` output.
#[derive(Clone, Default)]
pub struct Keys(BTreeMap<&'static str, String>);

impl Keys {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.0.iter().map(|(name, value)| (*name, value.as_str()))
    }
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Support {
    Ok,
    /// Credential names that are not present.
    Missing(Vec<&'static str>),
    Unsupported,
}

#[derive(Clone, Debug)]
pub struct Availability(BTreeMap<(ProviderId, Kind), Support>);

impl Availability {
    pub fn support(&self, id: ProviderId, kind: Kind) -> &Support {
        &self.0[&(id, kind)]
    }

    pub fn is_available(&self, id: ProviderId, kind: Kind) -> bool {
        *self.support(id, kind) == Support::Ok
    }

    #[cfg(test)]
    pub fn for_tests(usable: &[(ProviderId, Kind)]) -> Self {
        let mut map = BTreeMap::new();
        for id in ProviderId::ALL {
            for kind in [Kind::Search, Kind::Fetch] {
                let support = if usable.contains(&(id, kind)) {
                    Support::Ok
                } else {
                    Support::Unsupported
                };
                map.insert((id, kind), support);
            }
        }
        Self(map)
    }

    /// Providers usable for `kind`, in preference order.
    pub fn available(&self, kind: Kind) -> Vec<ProviderId> {
        order(kind)
            .iter()
            .copied()
            .filter(|id| self.is_available(*id, kind))
            .collect()
    }
}

/// A credential is present when it is non-empty after trimming and holds no
/// control characters. The trimmed value is what gets used.
fn present(raw: Option<String>) -> Option<String> {
    let value = raw?.trim().to_string();
    (!value.is_empty() && !value.chars().any(char::is_control)).then_some(value)
}

/// The one availability rule, shared by the daemon (lookup = credential
/// store then environment), the tool process (lookup = its environment),
/// `/search` and `doctor`.
pub fn availability(lookup: &dyn Fn(&str) -> Option<String>) -> (Availability, Keys) {
    let mut keys = BTreeMap::new();
    let names: std::collections::BTreeSet<&'static str> = ProviderId::ALL
        .into_iter()
        .flat_map(|id| [Kind::Search, Kind::Fetch].map(|kind| required(id, kind)))
        .flatten()
        .flatten()
        .copied()
        .collect();
    for name in names {
        let mut value = present(lookup(name));
        if value.is_none() && name == SERPAPI_KEY {
            value = present(lookup(SERPAPI_KEY_ALIAS));
        }
        if let Some(value) = value {
            keys.insert(name, value);
        }
    }
    let mut map = BTreeMap::new();
    for id in ProviderId::ALL {
        for kind in [Kind::Search, Kind::Fetch] {
            let support = match required(id, kind) {
                None => Support::Unsupported,
                Some(names) => {
                    let missing: Vec<_> = names
                        .iter()
                        .copied()
                        .filter(|name| !keys.contains_key(name))
                        .collect();
                    if missing.is_empty() {
                        Support::Ok
                    } else {
                        Support::Missing(missing)
                    }
                }
            };
            map.insert((id, kind), support);
        }
    }
    (Availability(map), Keys(keys))
}

/// What a provider client needs for one request.
pub struct Call<'a> {
    pub http: &'a reqwest::Client,
    pub base_url: &'a str,
    pub keys: &'a Keys,
    /// True only in a yolo turn: ask for unfiltered results where the
    /// vendor has a switch.
    pub unfiltered: bool,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub date: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub final_url: Option<String>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// HTTP 400 or 422: our request is wrong, another vendor will not help.
    BadRequest,
    /// Everything else: timeout, connection failure, any other status, a
    /// malformed body, or a vendor-reported failure inside a 2xx.
    ProviderFault,
}

#[derive(Clone, Debug)]
pub struct ProviderError {
    pub provider: ProviderId,
    pub class: ErrorClass,
    pub status: Option<u16>,
    pub message: String,
}

const MESSAGE_LIMIT: usize = 200;
const REDACTED: &str = "[redacted]";

/// Percent-encoded form of a value as it would appear in a query string.
fn query_encoded(value: &str) -> Option<String> {
    let url = reqwest::Url::parse_with_params("http://x/", [("k", value)]).ok()?;
    url.query()
        .and_then(|query| query.strip_prefix("k="))
        .map(str::to_string)
}

/// Remove every credential value (raw and percent-encoded), then truncate.
/// Order matters: truncating first could split a key so it no longer matches.
fn redact(raw: &str, keys: &Keys) -> String {
    let mut text = raw.to_string();
    for (_, value) in keys.iter() {
        text = text.replace(value, REDACTED);
        if let Some(encoded) = query_encoded(value) {
            text = text.replace(&encoded, REDACTED);
        }
    }
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    text.chars().take(MESSAGE_LIMIT).collect()
}

impl ProviderError {
    /// The single constructor for vendor-reported failures. `raw` is vendor
    /// text; it must never be a request URL.
    pub fn new(provider: ProviderId, status: Option<u16>, raw: &str, keys: &Keys) -> Self {
        let class = match status {
            Some(400 | 422) => ErrorClass::BadRequest,
            _ => ErrorClass::ProviderFault,
        };
        Self {
            provider,
            class,
            status,
            message: redact(raw, keys),
        }
    }

    /// Transport failures. `reqwest` errors carry the request URL, which for
    /// SerpApi holds the key, so it is stripped before formatting.
    pub fn transport(provider: ProviderId, error: reqwest::Error, keys: &Keys) -> Self {
        let kind = if error.is_timeout() {
            "timed out"
        } else if error.is_connect() {
            "connection failed"
        } else {
            "request failed"
        };
        let detail = error.without_url().to_string();
        Self::new(provider, None, &format!("{kind}: {detail}"), keys)
    }

    fn summary(&self) -> String {
        match self.status {
            Some(status) => format!("{} (HTTP {status}: {})", self.provider.id(), self.message),
            None => format!("{} ({})", self.provider.id(), self.message),
        }
    }
}

pub const BODY_LIMIT: usize = 5 * 1024 * 1024;

#[derive(Debug)]
pub struct VendorResponse {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
}

impl VendorResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(
        &self,
        provider: ProviderId,
        keys: &Keys,
    ) -> Result<serde_json::Value, ProviderError> {
        serde_json::from_slice(&self.body).map_err(|_| {
            ProviderError::new(
                provider,
                Some(self.status),
                "response was not valid JSON",
                keys,
            )
        })
    }
}

/// Send one vendor request with the attempt timeout and the body cap. A
/// non-2xx status becomes a `ProviderError` carrying the vendor's text.
pub async fn send(
    cx: &Call<'_>,
    provider: ProviderId,
    request: reqwest::RequestBuilder,
) -> Result<VendorResponse, ProviderError> {
    let mut response = request
        .timeout(cx.timeout)
        .send()
        .await
        .map_err(|error| ProviderError::transport(provider, error, cx.keys))?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ProviderError::transport(provider, error, cx.keys))?
    {
        if body.len() + chunk.len() > BODY_LIMIT {
            return Err(ProviderError::new(
                provider,
                Some(status),
                "response body exceeded the size limit",
                cx.keys,
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let reply = VendorResponse {
        status,
        headers,
        body,
    };
    if !(200..300).contains(&status) {
        return Err(ProviderError::new(
            provider,
            Some(status),
            &reply.text(),
            cx.keys,
        ));
    }
    Ok(reply)
}

/// Pins as resolved from the pin file for one conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinView {
    pub conversation_search: Option<ProviderId>,
    pub conversation_fetch: Option<ProviderId>,
    pub default_search: Option<ProviderId>,
    pub default_fetch: Option<ProviderId>,
}

impl PinView {
    fn conversation_pin(&self, kind: Kind) -> Option<ProviderId> {
        match kind {
            Kind::Search => self.conversation_search,
            Kind::Fetch => self.conversation_fetch,
        }
    }

    fn default_pin(&self, kind: Kind) -> Option<ProviderId> {
        match kind {
            Kind::Search => self.default_search,
            Kind::Fetch => self.default_fetch,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Providers to try, in order. One entry when pinned.
    pub providers: Vec<ProviderId>,
    pub pinned: bool,
    /// One line per stored pin that was skipped because its provider is not
    /// available for this kind.
    pub notes: Vec<String>,
}

/// Conversation pin, else default pin, else the preference order.
pub fn resolve(kind: Kind, pins: &PinView, availability: &Availability) -> Plan {
    let mut notes = Vec::new();
    for (scope, pin) in [
        ("conversation", pins.conversation_pin(kind)),
        ("default", pins.default_pin(kind)),
    ] {
        let Some(id) = pin else { continue };
        if availability.is_available(id, kind) {
            return Plan {
                providers: vec![id],
                pinned: true,
                notes,
            };
        }
        notes.push(format!(
            "note: {scope} pin {} is not available for {}; skipped",
            id.id(),
            kind.name()
        ));
    }
    Plan {
        providers: availability.available(kind),
        pinned: false,
        notes,
    }
}

/// Per provider attempt. One attempt per provider per call.
pub fn attempt_timeout(id: ProviderId, kind: Kind) -> Duration {
    match (kind, id) {
        (Kind::Search, _) => Duration::from_secs(10),
        (Kind::Fetch, ProviderId::Brightdata) => Duration::from_secs(90),
        (Kind::Fetch, _) => Duration::from_secs(45),
    }
}

/// Whole tool call, below the bridge's 120 s MCP ceiling.
pub const CALL_BUDGET: Duration = Duration::from_secs(100);

#[derive(Clone, Debug)]
pub struct Served<T> {
    pub provider: ProviderId,
    pub value: T,
    pub notes: Vec<String>,
}

/// A failed tool call, already redacted and safe to show the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallFailure(pub String);

pub struct Runner {
    pub http: reqwest::Client,
    pub keys: Keys,
    pub availability: Availability,
    pub unfiltered: bool,
    /// Test seam: overrides `ProviderId::default_base`.
    pub bases: BTreeMap<ProviderId, String>,
    pub budget: Duration,
}

impl Runner {
    pub fn new(lookup: &dyn Fn(&str) -> Option<String>, unfiltered: bool) -> Self {
        let (availability, keys) = availability(lookup);
        Self {
            http: reqwest::Client::new(),
            keys,
            availability,
            unfiltered,
            bases: BTreeMap::new(),
            budget: CALL_BUDGET,
        }
    }

    fn call(&self, id: ProviderId, kind: Kind) -> (String, Duration) {
        let base = self
            .bases
            .get(&id)
            .cloned()
            .unwrap_or_else(|| id.default_base().to_string());
        (base, attempt_timeout(id, kind))
    }

    pub async fn search(
        &self,
        pins: &PinView,
        query: &str,
        count: u8,
    ) -> Result<Served<Vec<Hit>>, CallFailure> {
        let count = count.clamp(1, 10);
        let mut served = self
            .run(Kind::Search, pins, |id, base, timeout| async move {
                let cx = Call {
                    http: &self.http,
                    base_url: &base,
                    keys: &self.keys,
                    unfiltered: self.unfiltered,
                    timeout,
                };
                providers::search(id, &cx, query, count).await
            })
            .await?;
        served.value.truncate(usize::from(count));
        Ok(served)
    }

    pub async fn fetch(
        &self,
        pins: &PinView,
        url: &str,
        max_chars: usize,
    ) -> Result<Served<Page>, CallFailure> {
        self.run(Kind::Fetch, pins, |id, base, timeout| async move {
            let cx = Call {
                http: &self.http,
                base_url: &base,
                keys: &self.keys,
                unfiltered: self.unfiltered,
                timeout,
            };
            providers::fetch(id, &cx, url, max_chars).await
        })
        .await
    }

    async fn run<T, F, Fut>(
        &self,
        kind: Kind,
        pins: &PinView,
        attempt: F,
    ) -> Result<Served<T>, CallFailure>
    where
        F: Fn(ProviderId, String, Duration) -> Fut,
        Fut: std::future::Future<Output = Result<T, ProviderError>>,
    {
        let plan = resolve(kind, pins, &self.availability);
        if plan.providers.is_empty() {
            return Err(CallFailure(format!(
                "no web provider is available for {}",
                kind.name()
            )));
        }
        let started = Instant::now();
        let mut failures: Vec<ProviderError> = Vec::new();
        for (index, id) in plan.providers.iter().copied().enumerate() {
            let (base, timeout) = self.call(id, kind);
            // Failover starts the next provider only when the remaining
            // budget covers that provider's whole attempt.
            if index > 0 && self.budget.saturating_sub(started.elapsed()) < timeout {
                break;
            }
            let outcome = match tokio::time::timeout(timeout, attempt(id, base, timeout)).await {
                Ok(outcome) => outcome,
                Err(_) => Err(ProviderError::new(id, None, "timed out", &self.keys)),
            };
            match outcome {
                Ok(value) => {
                    return Ok(Served {
                        provider: id,
                        value,
                        notes: plan.notes,
                    })
                }
                Err(error) if plan.pinned => {
                    let others: Vec<_> = self
                        .availability
                        .available(kind)
                        .into_iter()
                        .filter(|other| *other != id)
                        .map(ProviderId::id)
                        .collect();
                    let others = if others.is_empty() {
                        "none".to_string()
                    } else {
                        others.join(", ")
                    };
                    return Err(CallFailure(format!(
                        "{} failed: {}. Pinned provider, no failover. Other available providers: {others}.",
                        kind.name(),
                        error.summary()
                    )));
                }
                Err(error) if error.class == ErrorClass::BadRequest => {
                    return Err(CallFailure(format!(
                        "{} rejected the request: {}",
                        kind.name(),
                        error.summary()
                    )));
                }
                Err(error) => failures.push(error),
            }
        }
        let listed: Vec<_> = failures.iter().map(ProviderError::summary).collect();
        Err(CallFailure(format!(
            "{} failed on every provider tried: {}",
            kind.name(),
            listed.join("; ")
        )))
    }
}
