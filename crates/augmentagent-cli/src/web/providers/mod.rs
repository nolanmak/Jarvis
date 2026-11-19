//! One module per vendor. Each exposes `search` and, when the vendor can
//! fetch pages, `fetch`. Adding a provider is one module plus one arm in
//! each dispatcher here and in `ProviderId::implemented`.
use super::{Call, Hit, Page, ProviderError, ProviderId};

pub mod brave;

fn not_implemented(id: ProviderId, cx: &Call<'_>) -> ProviderError {
    ProviderError::new(id, None, "provider is not implemented", cx.keys)
}

pub async fn search(
    id: ProviderId,
    cx: &Call<'_>,
    query: &str,
    count: u8,
) -> Result<Vec<Hit>, ProviderError> {
    match id {
        ProviderId::Brave => brave::search(cx, query, count).await,
        other => Err(not_implemented(other, cx)),
    }
}

pub async fn fetch(
    id: ProviderId,
    cx: &Call<'_>,
    _url: &str,
    _max_chars: usize,
) -> Result<Page, ProviderError> {
    Err(not_implemented(id, cx))
}

/// String field of a JSON object, empty when absent or not a string.
pub(super) fn text(value: &serde_json::Value, field: &str) -> String {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}
