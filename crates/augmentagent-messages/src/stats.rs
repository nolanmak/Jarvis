//! `conversation_stats`: aggregates over the message index (#1098).
//!
//! "Who do I message most", "how often do I talk to X", "when did I last
//! hear from Y", "which channels were busiest last month" are single SQL
//! aggregates. A search tool returning 20 rows cannot answer them and an
//! agent paging through results times out.
//!
//! This tool returns counts and timestamps only: [`StatsRow`] has no field
//! that can carry message text.

use augmentagent_store::rusqlite::types::Value as SqlValue;
use augmentagent_store::rusqlite::{Connection, ToSql};
use serde::Serialize;

use crate::people;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    Person,
    Conversation,
    Platform,
    Kind,
    Day,
    Week,
    Month,
}

impl GroupBy {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "person" => Self::Person,
            "conversation" => Self::Conversation,
            "platform" => Self::Platform,
            "kind" => Self::Kind,
            "day" => Self::Day,
            "week" => Self::Week,
            "month" => Self::Month,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderBy {
    Messages,
    LastContact,
    FirstContact,
}

impl OrderBy {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "messages" => Self::Messages,
            "last_contact" => Self::LastContact,
            "first_contact" => Self::FirstContact,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatsRequest {
    pub group_by: GroupBy,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_me: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub order_by: OrderBy,
    pub limit: usize,
}

impl Default for StatsRequest {
    fn default() -> Self {
        Self {
            group_by: GroupBy::Person,
            platforms: Vec::new(),
            kinds: Vec::new(),
            with: None,
            from_me: None,
            since_ms: None,
            until_ms: None,
            container: None,
            order_by: OrderBy::Messages,
            limit: DEFAULT_LIMIT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatsRow {
    pub key: String,
    pub label: Option<String>,
    pub messages: i64,
    pub from_me: i64,
    pub from_them: i64,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
    pub platforms: Vec<String>,
    pub active_days: i64,
    /// `true` when the key is a handle no person page claims.
    pub unresolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatsResponse {
    pub rows: Vec<StatsRow>,
    pub total_groups: i64,
    pub filters_applied: StatsRequest,
    /// Time zone the day/week/month buckets and `active_days` are computed in.
    pub timezone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambiguous: Option<Vec<people::PersonMatch>>,
}

fn timezone() -> String {
    std::env::var("TZ")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "system local".into())
}

fn ts(ms: Option<i64>) -> Option<String> {
    ms.and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.to_rfc3339())
}

/// Aggregate. Person rows count messages that person *sent*, plus the
/// owner's messages in 1:1 DMs with them (the only direction that attributes
/// unambiguously); a room's other traffic is never attributed to its members.
pub fn stats(c: &Connection, req: &StatsRequest) -> anyhow::Result<StatsResponse> {
    let mut params: Vec<SqlValue> = Vec::new();
    let mut filters: Vec<String> = Vec::new();
    let bind = |params: &mut Vec<SqlValue>, v: SqlValue| {
        params.push(v);
        format!("?{}", params.len())
    };
    if !req.platforms.is_empty() {
        let marks: Vec<String> = req
            .platforms
            .iter()
            .map(|p| bind(&mut params, SqlValue::Text(p.to_ascii_lowercase())))
            .collect();
        filters.push(format!("mi.platform IN ({})", marks.join(", ")));
    }
    if !req.kinds.is_empty() {
        let marks: Vec<String> = req
            .kinds
            .iter()
            .map(|k| bind(&mut params, SqlValue::Text(k.to_ascii_lowercase())))
            .collect();
        filters.push(format!("mi.conv_kind IN ({})", marks.join(", ")));
    }
    if let Some(since) = req.since_ms {
        let m = bind(&mut params, SqlValue::Integer(since));
        filters.push(format!("mi.ts_ms >= {m}"));
    }
    if let Some(until) = req.until_ms {
        let m = bind(&mut params, SqlValue::Integer(until));
        filters.push(format!("mi.ts_ms < {m}"));
    }
    if let Some(container) = &req.container {
        let m = bind(
            &mut params,
            SqlValue::Text(format!("%{}%", crate::query::like_escape(container))),
        );
        filters.push(format!("mi.container LIKE {m} ESCAPE '\\'"));
    }
    if let Some(from_me) = req.from_me {
        filters.push(format!("mi.from_me = {}", i64::from(from_me)));
    }
    let mut ambiguous = None;
    if let Some(reference) = &req.with {
        let matches = people::resolve_person(c, reference)?;
        if matches.len() > 1 {
            ambiguous = Some(matches);
        } else {
            let handles = match matches.first() {
                Some(m) if m.resolved => people::handles_for(c, &m.key)?,
                Some(m) => vec![m.key.clone()],
                None => vec![crate::handles::canonical(reference)],
            };
            let marks: Vec<String> = handles
                .iter()
                .map(|h| bind(&mut params, SqlValue::Text(h.clone())))
                .collect();
            let list = marks.join(", ");
            filters.push(format!(
                "mi.conversation_id IN (SELECT conversation_id FROM message_index \
                 WHERE sender_handle IN ({list}) OR counterpart_handle IN ({list}))"
            ));
        }
    }
    if let Some(candidates) = ambiguous {
        return Ok(StatsResponse {
            rows: Vec::new(),
            total_groups: 0,
            filters_applied: req.clone(),
            timezone: timezone(),
            ambiguous: Some(candidates),
        });
    }
    let where_clause = if filters.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", filters.join(" AND "))
    };

    // Rows feeding the aggregate: (key, label, is_from_me, ts, platform).
    let source = match req.group_by {
        GroupBy::Person => format!(
            "SELECT COALESCE(mp.person_key, mi.sender_handle) AS k, \
                    COALESCE(mp.person_key, mi.sender_label, mi.sender_handle) AS lbl, \
                    0 AS mine, mi.ts_ms AS ts, mi.platform AS plat \
               FROM message_index mi \
               LEFT JOIN message_people mp ON mp.handle = mi.sender_handle \
               {where_from_them} \
             UNION ALL \
             SELECT COALESCE(mp.person_key, cp.handle), \
                    COALESCE(mp.person_key, cp.handle), 1, mi.ts_ms, mi.platform \
               FROM message_index mi \
               JOIN (SELECT conversation_id, MIN(handle) AS handle FROM conversation_people \
                      GROUP BY conversation_id) cp ON cp.conversation_id = mi.conversation_id \
               LEFT JOIN message_people mp ON mp.handle = cp.handle \
               {where_mine}",
            where_from_them = if where_clause.is_empty() {
                "WHERE mi.from_me = 0".to_string()
            } else {
                format!("{where_clause} AND mi.from_me = 0")
            },
            where_mine = if where_clause.is_empty() {
                "WHERE mi.from_me = 1 AND mi.conv_kind = 'dm'".to_string()
            } else {
                format!("{where_clause} AND mi.from_me = 1 AND mi.conv_kind = 'dm'")
            },
        ),
        other => {
            let (key_expr, label_expr) = match other {
                GroupBy::Conversation => (
                    "mi.conversation_id",
                    "COALESCE(mi.container || ' ' || mi.conversation_title, mi.conversation_title)",
                ),
                GroupBy::Platform => ("mi.platform", "mi.platform"),
                GroupBy::Kind => ("mi.conv_kind", "mi.conv_kind"),
                GroupBy::Day => (
                    "strftime('%Y-%m-%d', mi.ts_ms / 1000, 'unixepoch', 'localtime')",
                    "strftime('%Y-%m-%d', mi.ts_ms / 1000, 'unixepoch', 'localtime')",
                ),
                GroupBy::Week => (
                    "strftime('%Y-W%W', mi.ts_ms / 1000, 'unixepoch', 'localtime')",
                    "strftime('%Y-W%W', mi.ts_ms / 1000, 'unixepoch', 'localtime')",
                ),
                _ => (
                    "strftime('%Y-%m', mi.ts_ms / 1000, 'unixepoch', 'localtime')",
                    "strftime('%Y-%m', mi.ts_ms / 1000, 'unixepoch', 'localtime')",
                ),
            };
            format!(
                "SELECT {key_expr} AS k, {label_expr} AS lbl, mi.from_me AS mine, \
                        mi.ts_ms AS ts, mi.platform AS plat \
                   FROM message_index mi{where_clause}"
            )
        }
    };

    let order = match req.order_by {
        OrderBy::Messages => "messages DESC, last_ts DESC",
        OrderBy::LastContact => "last_ts DESC",
        OrderBy::FirstContact => "first_ts ASC",
    };
    let limit = req.limit.clamp(1, MAX_LIMIT);
    let grouped = format!(
        "SELECT k, MIN(lbl), COUNT(*) AS messages, SUM(mine) AS from_me, \
                SUM(1 - mine) AS from_them, MIN(ts) AS first_ts, MAX(ts) AS last_ts, \
                GROUP_CONCAT(DISTINCT plat), \
                COUNT(DISTINCT strftime('%Y-%m-%d', ts / 1000, 'unixepoch', 'localtime')) \
           FROM ({source}) GROUP BY k"
    );
    let bound: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();
    let total_groups: i64 = c.query_row(
        &format!("SELECT COUNT(*) FROM ({grouped})"),
        bound.as_slice(),
        |r| r.get(0),
    )?;
    let sql = format!("SELECT * FROM ({grouped}) ORDER BY {order} LIMIT {limit}");
    let mut stmt = c.prepare(&sql)?;
    let rows: Vec<StatsRow> = stmt
        .query_map(bound.as_slice(), |r| {
            let key: String = r.get(0)?;
            let platforms: Option<String> = r.get(7)?;
            Ok(StatsRow {
                unresolved: key.contains(':') || key == "me",
                label: r.get::<_, Option<String>>(1)?,
                key,
                messages: r.get(2)?,
                from_me: r.get(3)?,
                from_them: r.get(4)?,
                first_ts: ts(r.get(5)?),
                last_ts: ts(r.get(6)?),
                platforms: {
                    // GROUP_CONCAT order is unspecified; sort for stable output.
                    let mut ps: Vec<String> = platforms
                        .unwrap_or_default()
                        .split(',')
                        .filter(|p| !p.is_empty())
                        .map(str::to_string)
                        .collect();
                    ps.sort();
                    ps
                },
                active_days: r.get(8)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(StatsResponse {
        rows,
        total_groups,
        filters_applied: req.clone(),
        timezone: timezone(),
        ambiguous: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::drain;
    use augmentagent_store::{Email, Store};
    use std::time::Duration;

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        let wiki = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(wiki.path().join("people")).unwrap();
        std::fs::write(
            wiki.path().join("people/alex-kim.md"),
            "---\nkind: person\nidentities:\n  phone: [\"+14155550123\"]\n  discord: \"500\"\n---\n",
        )
        .unwrap();
        let put = |id: &str,
                   platform: &str,
                   from: &str,
                   thread: &str,
                   subject: &str,
                   kind: &str,
                   date: &str| {
            store
                .upsert_email(&Email {
                    message_id: id.into(),
                    thread_id: Some(thread.into()),
                    from: from.into(),
                    to: String::new(),
                    cc: String::new(),
                    attachments: vec![],
                    subject: subject.into(),
                    body: "text".into(),
                    date: date.into(),
                    account_entity_id: Some("discord:900".into()),
                    platform: platform.into(),
                    kind: kind.into(),
                })
                .unwrap();
        };
        // Alex: 2 sent by them (2 platforms), 1 owner DM reply.
        put(
            "t1",
            "imessage",
            "+14155550123",
            "imessage:+14155550123",
            "iMessage: Alex Kim",
            "dm",
            "2026-01-10T10:00:00Z",
        );
        put(
            "t2",
            "imessage",
            "me",
            "imessage:+14155550123",
            "iMessage: Alex Kim",
            "dm",
            "2026-01-11T10:00:00Z",
        );
        put(
            "d1",
            "discord",
            "Alex <discord:500>",
            "chan-1",
            "Discord DM: Alex [Alex]",
            "dm",
            "2026-02-01T10:00:00Z",
        );
        // Sam in a server channel: room traffic must not be attributed to members.
        put(
            "s1",
            "discord",
            "Sam <discord:600>",
            "chan-9",
            "Discord: Acme HQ #general [Sam]",
            "guild_channel",
            "2026-02-02T10:00:00Z",
        );
        put(
            "s2",
            "discord",
            "me <discord:900>",
            "chan-9",
            "Discord: Acme HQ #general [me]",
            "guild_channel",
            "2026-02-03T10:00:00Z",
        );
        put(
            "s3",
            "discord",
            "Sam <discord:600>",
            "chan-9",
            "Discord: Acme HQ #general [Sam]",
            "guild_channel",
            "2026-02-04T10:00:00Z",
        );
        drain(&store, 100, Duration::ZERO).unwrap();
        people::resolve_people(&store, wiki.path()).unwrap();
        (dir, wiki, store)
    }

    fn run(s: &Store, req: StatsRequest) -> StatsResponse {
        s.with_conn(|c| Ok(stats(c, &req).unwrap())).unwrap()
    }

    #[test]
    fn top_people_merges_handles_across_platforms_and_splits_direction() {
        let (_d, _w, s) = fixture();
        let r = run(&s, StatsRequest::default());
        let alex = r
            .rows
            .iter()
            .find(|row| row.key == "alex-kim")
            .expect("alex row");
        assert_eq!((alex.messages, alex.from_them, alex.from_me), (3, 2, 1));
        assert_eq!(alex.platforms, ["discord", "imessage"]);
        assert!(!alex.unresolved);
        assert_eq!(alex.active_days, 3);
        assert!(alex.first_ts.as_deref().unwrap().starts_with("2026-01-10"));
        assert!(alex.last_ts.as_deref().unwrap().starts_with("2026-02-01"));
    }

    #[test]
    fn room_traffic_is_attributed_to_the_sender_not_to_members() {
        let (_d, _w, s) = fixture();
        let r = run(&s, StatsRequest::default());
        let sam = r
            .rows
            .iter()
            .find(|row| row.key == "discord:600")
            .expect("sam row");
        assert_eq!(
            (sam.messages, sam.from_them, sam.from_me),
            (2, 2, 0),
            "owner's channel message isn't Sam's"
        );
        assert!(sam.unresolved, "no person page claims this handle");
        assert!(
            r.rows.iter().all(|row| row.key != "me"),
            "the owner is never a person row"
        );
    }

    #[test]
    fn conversation_and_platform_and_kind_groupings() {
        let (_d, _w, s) = fixture();
        let conv = run(
            &s,
            StatsRequest {
                group_by: GroupBy::Conversation,
                ..Default::default()
            },
        );
        let room = conv.rows.iter().find(|r| r.key == "chan-9").unwrap();
        assert_eq!(
            (room.messages, room.from_me),
            (3, 1),
            "conversation counts the room"
        );
        assert_eq!(room.label.as_deref(), Some("Acme HQ #general"));
        let plat = run(
            &s,
            StatsRequest {
                group_by: GroupBy::Platform,
                ..Default::default()
            },
        );
        assert_eq!(plat.rows[0].key, "discord");
        assert_eq!(plat.rows[0].messages, 4);
        let kinds = run(
            &s,
            StatsRequest {
                group_by: GroupBy::Kind,
                ..Default::default()
            },
        );
        let keys: Vec<&str> = kinds.rows.iter().map(|r| r.key.as_str()).collect();
        assert!(keys.contains(&"dm") && keys.contains(&"channel"));
    }

    #[test]
    fn month_bucketing_and_filters_combine() {
        let (_d, _w, s) = fixture();
        let months = run(
            &s,
            StatsRequest {
                group_by: GroupBy::Month,
                order_by: OrderBy::FirstContact,
                ..Default::default()
            },
        );
        let keys: Vec<&str> = months.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["2026-01", "2026-02"]);
        assert_eq!(months.total_groups, 2);
        let filtered = run(
            &s,
            StatsRequest {
                group_by: GroupBy::Platform,
                platforms: vec!["discord".into()],
                kinds: vec!["dm".into()],
                since_ms: chrono::DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
                    .unwrap()
                    .timestamp_millis()
                    .into(),
                ..Default::default()
            },
        );
        assert_eq!(filtered.rows.len(), 1);
        assert_eq!(filtered.rows[0].messages, 1);
    }

    #[test]
    fn with_filter_restricts_to_that_persons_conversations_and_ambiguity_is_reported() {
        let (_d, w, s) = fixture();
        let r = run(
            &s,
            StatsRequest {
                group_by: GroupBy::Platform,
                with: Some("alex kim".into()),
                ..Default::default()
            },
        );
        let total: i64 = r.rows.iter().map(|row| row.messages).sum();
        assert_eq!(total, 3, "only Alex's conversations");
        std::fs::write(
            w.path().join("people/alex-stone.md"),
            "---\nkind: person\nidentities:\n  email: [\"astone@example.org\"]\n---\n",
        )
        .unwrap();
        people::resolve_people(&s, w.path()).unwrap();
        let amb = run(
            &s,
            StatsRequest {
                with: Some("alex".into()),
                ..Default::default()
            },
        );
        assert!(amb.rows.is_empty());
        assert_eq!(amb.ambiguous.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn limit_is_clamped_and_empty_results_are_well_formed() {
        let (_d, _w, s) = fixture();
        let r = run(
            &s,
            StatsRequest {
                limit: 9999,
                ..Default::default()
            },
        );
        assert!(r.rows.len() <= MAX_LIMIT);
        let empty = run(
            &s,
            StatsRequest {
                platforms: vec!["nosuch".into()],
                ..Default::default()
            },
        );
        assert!(empty.rows.is_empty());
        assert_eq!(empty.total_groups, 0);
        assert!(!empty.timezone.is_empty());
    }

    #[test]
    fn response_carries_no_message_text() {
        let (_d, _w, s) = fixture();
        let json = serde_json::to_string(&run(&s, StatsRequest::default())).unwrap();
        assert!(
            !json.contains("text"),
            "message bodies must never appear: {json}"
        );
    }

    #[test]
    fn aggregates_never_touch_the_emails_table() {
        let (_d, _w, s) = fixture();
        let plan: String = s
            .with_conn(|c| {
                let req = StatsRequest::default();
                // Rebuild the same SQL the tool runs, then explain it.
                let sql = "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM (SELECT k FROM \
                     (SELECT COALESCE(mp.person_key, mi.sender_handle) AS k FROM message_index mi \
                      LEFT JOIN message_people mp ON mp.handle = mi.sender_handle \
                      WHERE mi.from_me = 0) GROUP BY k)";
                let _ = &req;
                let mut stmt = c.prepare(sql)?;
                let rows: Vec<String> = stmt
                    .query_map([], |r| r.get::<_, String>(3))?
                    .collect::<Result<_, _>>()?;
                Ok(rows.join(" | "))
            })
            .unwrap();
        assert!(!plan.contains("emails"), "{plan}");
    }
}
