//! Exa: search and page contents.
use super::text;
use crate::web::{send, Call, Hit, Page, ProviderError, ProviderId};
use serde_json::json;

const ID: ProviderId = ProviderId::Exa;

fn key<'a>(cx: &'a Call<'_>) -> &'a str {
    cx.keys.get("EXA_API_KEY").unwrap_or_default()
}

pub async fn search(cx: &Call<'_>, query: &str, count: u8) -> Result<Vec<Hit>, ProviderError> {
    // Without `contents.highlights` Exa returns no snippet at all. Its
    // moderation is off by default, so there is nothing to switch for an
    // unfiltered turn.
    let request = cx
        .http
        .post(format!("{}/search", cx.base_url))
        .header("x-api-key", key(cx))
        .json(&json!({"query": query, "numResults": count, "contents": {"highlights": true}}));
    let body = send(cx, ID, request).await?.json(ID, cx.keys)?;
    let rows = body.get("results").and_then(serde_json::Value::as_array);
    Ok(rows
        .map(|rows| {
            rows.iter()
                .map(|row| Hit {
                    title: text(row, "title"),
                    url: text(row, "url"),
                    snippet: row
                        .pointer("/highlights/0")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    date: row
                        .get("publishedDate")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                })
                .filter(|hit| !hit.url.is_empty())
                .collect()
        })
        .unwrap_or_default())
}

pub async fn fetch(cx: &Call<'_>, url: &str, max_chars: usize) -> Result<Page, ProviderError> {
    // One more character than asked for, so the caller can tell the page
    // was longer and say it was cut.
    let request = cx
        .http
        .post(format!("{}/contents", cx.base_url))
        .header("x-api-key", key(cx))
        .json(&json!({"urls": [url], "text": {"maxCharacters": max_chars + 1}}));
    let reply = send(cx, ID, request).await?;
    let body = reply.json(ID, cx.keys)?;
    // HTTP 200 can still carry a failed URL.
    if body
        .pointer("/statuses/0/status")
        .and_then(serde_json::Value::as_str)
        == Some("error")
    {
        let tag = body
            .pointer("/statuses/0/error/tag")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("fetch failed");
        return Err(ProviderError::new(
            ID,
            Some(reply.status),
            &format!("page not fetched: {tag}"),
            cx.keys,
        ));
    }
    match body
        .pointer("/results/0/text")
        .and_then(serde_json::Value::as_str)
    {
        Some(text) => Ok(Page {
            final_url: None,
            text: text.to_string(),
        }),
        None => Err(ProviderError::new(
            ID,
            Some(reply.status),
            "no page text in the reply",
            cx.keys,
        )),
    }
}
