//! One module per vendor. Each exposes `search` and, when the vendor can
//! fetch pages, `fetch`.
use super::{Call, Hit, Page, ProviderError, ProviderId};

pub mod brave;
pub mod brightdata;
pub mod context;
pub mod exa;
pub mod firecrawl;
pub mod serpapi;

pub async fn search(
    id: ProviderId,
    cx: &Call<'_>,
    query: &str,
    count: u8,
) -> Result<Vec<Hit>, ProviderError> {
    match id {
        ProviderId::Brave => brave::search(cx, query, count).await,
        ProviderId::Exa => exa::search(cx, query, count).await,
        ProviderId::Firecrawl => firecrawl::search(cx, query, count).await,
        ProviderId::Context => context::search(cx, query, count).await,
        ProviderId::Serpapi => serpapi::search(cx, query, count).await,
        ProviderId::Brightdata => brightdata::search(cx, query, count).await,
    }
}

pub async fn fetch(
    id: ProviderId,
    cx: &Call<'_>,
    url: &str,
    max_chars: usize,
) -> Result<Page, ProviderError> {
    match id {
        ProviderId::Exa => exa::fetch(cx, url, max_chars).await,
        ProviderId::Firecrawl => firecrawl::fetch(cx, url, max_chars).await,
        ProviderId::Context => context::fetch(cx, url, max_chars).await,
        ProviderId::Brightdata => brightdata::fetch(cx, url, max_chars).await,
        // Search-only vendors are never resolved for fetch.
        ProviderId::Brave | ProviderId::Serpapi => Err(ProviderError::new(
            id,
            None,
            "provider cannot fetch pages",
            cx.keys,
        )),
    }
}

/// String field of a JSON object, empty when absent or not a string.
pub(super) fn text(value: &serde_json::Value, field: &str) -> String {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}
