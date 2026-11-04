//! #1833 C5 exit tests: the strict planner parser and the D8 templates.
//! (`preset_is_pinned` lives beside the preset in channel-core.)

use augmentagent_autonomy::planner::{
    event_fields, parse_plan, template_text, PlanOutput, PlanReject,
};
use augmentagent_autonomy::registry::{ADD_GUEST, SEND_DETAILS};
use augmentagent_channel_calendar::Candidate;
use chrono::{DateTime, Utc};

fn known(id: &str) -> bool {
    matches!(id, "cand_1" | "cand_2")
}

#[test]
fn valid_plans_parse() {
    assert_eq!(
        parse_plan(r#"{"plan":"none"}"#, &known),
        Ok(PlanOutput::None)
    );
    assert_eq!(
        parse_plan(
            r#" {"plan":"calendar","candidate":"cand_2","actions":["calendar.add_guest"]} "#,
            &known
        ),
        Ok(PlanOutput::Calendar {
            candidate: "cand_2".into(),
            action: ADD_GUEST
        })
    );
    assert_eq!(
        parse_plan(
            r#"{"plan":"calendar","candidate":"cand_1","actions":["calendar.send_details"]}"#,
            &known
        ),
        Ok(PlanOutput::Calendar {
            candidate: "cand_1".into(),
            action: SEND_DETAILS
        })
    );
    // add_guest implies send_details: both listed is add_guest.
    assert_eq!(
        parse_plan(
            r#"{"plan":"calendar","candidate":"cand_1","actions":["calendar.send_details","calendar.add_guest"]}"#,
            &known
        ),
        Ok(PlanOutput::Calendar {
            candidate: "cand_1".into(),
            action: ADD_GUEST
        })
    );
}

#[test]
fn unknown_candidate_id_is_no_plan() {
    for id in ["cand_3", "cand_0", "ev-sec-1", "", "CAND_1"] {
        let raw =
            format!(r#"{{"plan":"calendar","candidate":"{id}","actions":["calendar.add_guest"]}}"#);
        assert_eq!(
            parse_plan(&raw, &known),
            Err(PlanReject::UnknownCandidate(id.into())),
            "{id}"
        );
    }
}

#[test]
fn extra_fields_or_prose_is_no_plan() {
    let bad = [
        r#"Sure! {"plan":"none"}"#,
        "```json\n{\"plan\":\"none\"}\n```",
        r#"{"plan":"none"} done"#,
        r#"{"plan":"none","why":"small talk"}"#,
        r#"{"plan":"calendar","candidate":"cand_1","actions":["calendar.add_guest"],"note":"hi"}"#,
        r#"{"plan":"calendar","candidate":"cand_1","actions":["calendar.add_guest"],"message":"Added you!"}"#,
        r#"{"plan":"calendar","candidate":"cand_1","actions":[]}"#,
        r#"{"plan":"calendar","candidate":"cand_1","actions":["reply"]}"#,
        r#"{"plan":"calendar","candidate":"cand_1","actions":["calendar.delete_event"]}"#,
        r#"{"plan":"calendar","candidate":"cand_1"}"#,
        r#"{"plan":"email"}"#,
        r#"[{"plan":"none"}]"#,
        r#"{"plan":"none"}{"plan":"none"}"#,
        "",
        "none",
    ];
    for raw in bad {
        assert!(parse_plan(raw, &known).is_err(), "accepted: {raw:?}");
    }
}

#[test]
fn recipient_field_rejected() {
    for key in ["recipient", "to", "target", "email", "phone"] {
        let raw = format!(
            r#"{{"plan":"calendar","candidate":"cand_1","actions":["calendar.add_guest"],"{key}":"mallory@example.test"}}"#
        );
        assert_eq!(
            parse_plan(&raw, &known),
            Err(PlanReject::RecipientField(key.into())),
            "{key}"
        );
    }
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

#[test]
fn templates_render_from_event_fields_only() {
    let c = Candidate {
        id: "cand_1".into(),
        account: "owner.work@example.test".into(),
        is_organizer: true,
        start: utc("2026-11-03T18:00:00Z"),
        end: utc("2026-11-03T18:30:00Z"),
        all_day: false,
        title: "Design review".into(),
        join_link: Some("https://meet.example.test/abc-defg-hij".into()),
        attendees: vec![
            "secret-colleague@example.test".into(),
            "owner.work@example.test".into(),
        ],
    };
    let fields = event_fields(&c, chrono_tz::America::New_York).expect("has a join link");
    assert_eq!(fields.title, "Design review");
    assert_eq!(fields.local_time, "Tue 3 Nov, 1:00 PM");
    assert_eq!(fields.link, "https://meet.example.test/abc-defg-hij");

    let add = template_text(ADD_GUEST, &fields, "jane@work.example.test");
    assert_eq!(
        add,
        "Added you to Design review (Tue 3 Nov, 1:00 PM) — invite sent to jane@work.example.test. Join: https://meet.example.test/abc-defg-hij" // pii-ok synthetic
    );
    let details = template_text(SEND_DETAILS, &fields, "jane@work.example.test");
    assert_eq!(
        details,
        "Design review — Tue 3 Nov, 1:00 PM. Join: https://meet.example.test/abc-defg-hij"
    );
    for text in [&add, &details] {
        assert!(
            !text.contains("secret-colleague"),
            "attendees never reach the text"
        );
        assert!(
            !text.contains("owner.work@"),
            "the owner's account never reaches the text"
        );
    }

    // No join link: nothing to send, so no template (the gate holds it).
    let mut no_link = c.clone();
    no_link.join_link = None;
    assert!(event_fields(&no_link, chrono_tz::America::New_York).is_none());

    // All-day events have no clock time.
    let mut all_day = c;
    all_day.all_day = true;
    all_day.start = utc("2026-11-03T05:00:00Z");
    assert_eq!(
        event_fields(&all_day, chrono_tz::America::New_York)
            .unwrap()
            .local_time,
        "Tue 3 Nov (all day)"
    );
}
