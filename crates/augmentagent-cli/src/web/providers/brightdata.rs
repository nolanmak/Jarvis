//! Bright Data: Google results through a SERP zone, page fetch through a Web
//! Unlocker zone. Both use one endpoint; the zone decides the product.
//! Unfiltered search (`safe=off`) is not requested yet: the vendor's docs do
//! not confirm Google parameters pass through (#2211).
use super::text;
use crate::web::{
    send, Call, Hit, Page, ProviderError, ProviderId, BRIGHTDATA_API_KEY, BRIGHTDATA_SERP_ZONE,
    BRIGHTDATA_UNLOCKER_ZONE,
};
use serde_json::json;

const ID: ProviderId = ProviderId::Brightdata;

async fn request(
    cx: &Call<'_>,
    body: serde_json::Value,
) -> Result<crate::web::VendorResponse, ProviderError> {
    let key = cx.keys.get(BRIGHTDATA_API_KEY).unwrap_or_default();
    let request = cx
        .http
        .post(format!("{}/request", cx.base_url))
        .bearer_auth(key)
        .json(&body);
    let reply = send(cx, ID, request).await?;
    // The vendor reports unblock failures in headers, on any status.
    let header = |name: &str| {
        reply
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    if let Some(error) = header("x-brd-error") {
        let detail = header("x-brd-err-msg").unwrap_or_default();
        return Err(ProviderError::new(
            ID,
            Some(reply.status),
            &format!("{error} {detail}"),
            cx.keys,
        ));
    }
    Ok(reply)
}

pub async fn search(cx: &Call<'_>, query: &str, _count: u8) -> Result<Vec<Hit>, ProviderError> {
    let zone = cx.keys.get(BRIGHTDATA_SERP_ZONE).unwrap_or_default();
    let target = reqwest::Url::parse_with_params(
        "https://www.google.com/search",
        [("q", query), ("brd_json", "1")],
    )
    .map_err(|_| ProviderError::new(ID, None, "could not build the search URL", cx.keys))?;
    let reply = request(
        cx,
        json!({"zone": zone, "url": target.as_str(), "format": "raw"}),
    )
    .await?;
    let body = reply.json(ID, cx.keys)?;
    let rows = body.get("organic").and_then(serde_json::Value::as_array);
    Ok(rows
        .map(|rows| {
            rows.iter()
                .map(|row| Hit {
                    title: text(row, "title"),
                    url: text(row, "link"),
                    snippet: text(row, "description"),
                    date: None,
                })
                .filter(|hit| !hit.url.is_empty())
                .collect()
        })
        .unwrap_or_default())
}

pub async fn fetch(cx: &Call<'_>, url: &str, _max_chars: usize) -> Result<Page, ProviderError> {
    let zone = cx.keys.get(BRIGHTDATA_UNLOCKER_ZONE).unwrap_or_default();
    let reply = request(
        cx,
        json!({"zone": zone, "url": url, "format": "raw", "data_format": "markdown"}),
    )
    .await?;
    // With `format: raw` the response body is the markdown itself.
    Ok(Page {
        final_url: None,
        text: reply.text(),
    })
}
