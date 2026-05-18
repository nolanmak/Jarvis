//! Typed Google Drive ops over the Composio v3 REST client. Composio wraps
//! Google's `changes.list`; responses nest under `data` / `data.response_data`
//! so we extract tolerantly with `find_array` / `find_string_field`.

use serde_json::{json, Value};

use crate::composio::{find_array, find_string_field, ComposioClient, ComposioError};

#[derive(Debug, Clone)]
pub struct DriveChange {
    pub file_id: String,
    pub name: String,
    pub mime_type: String,
    pub modified_time: String,
    pub web_view_link: String,
    pub removed: bool,
}

#[derive(Debug, Default)]
pub struct DriveChangesPage {
    pub changes: Vec<DriveChange>,
    pub next_page_token: Option<String>,
    pub new_start_page_token: Option<String>,
}

fn s(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

/// One-time baseline cursor so the first poll doesn't replay all history.
pub async fn get_start_page_token(
    client: &ComposioClient,
    entity_id: &str,
) -> Result<String, ComposioError> {
    let v = client
        .execute(
            "GOOGLEDRIVE_GET_CHANGES_START_PAGE_TOKEN",
            entity_id,
            json!({}),
        )
        .await?;
    find_string_field(&v, &["startPageToken", "start_page_token"]).ok_or_else(|| {
        ComposioError::Decode(format!(
            "no startPageToken in response: {}",
            serde_json::to_string(&v).unwrap_or_default()
        ))
    })
}

/// One page of the changes feed for `page_token`.
pub async fn list_changes(
    client: &ComposioClient,
    entity_id: &str,
    page_token: &str,
) -> Result<DriveChangesPage, ComposioError> {
    let v = client
        .execute(
            "GOOGLEDRIVE_LIST_CHANGES",
            entity_id,
            json!({ "pageToken": page_token, "includeRemoved": true }),
        )
        .await?;

    let mut page = DriveChangesPage {
        next_page_token: find_string_field(&v, &["nextPageToken", "next_page_token"]),
        new_start_page_token: find_string_field(
            &v,
            &["newStartPageToken", "new_start_page_token"],
        ),
        ..Default::default()
    };

    if let Some(arr) = find_array(&v, &["changes"]) {
        for c in arr {
            let removed = c.get("removed").and_then(Value::as_bool).unwrap_or(false);
            let file = c.get("file").cloned().unwrap_or(Value::Null);
            page.changes.push(DriveChange {
                file_id: s(c, &["fileId", "file_id"]),
                name: s(&file, &["name"]),
                mime_type: s(&file, &["mimeType", "mime_type"]),
                modified_time: s(&file, &["modifiedTime", "modified_time"]),
                web_view_link: s(&file, &["webViewLink", "web_view_link"]),
                removed,
            });
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_changes_and_tokens_under_nesting() {
        let v = json!({"data": {"response_data": {
            "newStartPageToken": "1005",
            "changes": [
                {"fileId": "F1", "removed": false,
                 "file": {"name": "Q3 plan", "mimeType": "application/vnd.google-apps.document",
                          "modifiedTime": "2026-05-18T12:00:00Z",
                          "webViewLink": "https://drive.google.com/file/F1"}},
                {"fileId": "F2", "removed": true}
            ]
        }}});
        // emulate list_changes parsing on a raw value
        let changes = find_array(&v, &["changes"]).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(
            find_string_field(&v, &["newStartPageToken"]),
            Some("1005".to_string())
        );
        let f0 = &changes[0];
        assert_eq!(s(f0, &["fileId"]), "F1");
        assert!(!f0.get("removed").unwrap().as_bool().unwrap());
    }
}
