use augmentagent_phone::{live_context::LiveContext, live_session::LiveInput};
fn fragment(id: &str, user: bool, text: &str, start: f64, end: f64) -> LiveInput {
    LiveInput::Transcript {
        event_id: id.into(),
        user,
        text: text.into(),
        start_ms: start,
        end_ms: end,
    }
}
#[test]
fn task_contains_bounded_role_context_and_never_future_speech() {
    let mut context = LiveContext::default();
    context
        .push(&fragment("one", true, "Check Tuesday.", 0., 100.))
        .unwrap();
    context
        .push(&fragment("two", false, "Your calendar?", 100., 150.))
        .unwrap();
    context
        .push(&fragment("three", true, "Yes, calendar.", 150., 200.))
        .unwrap();
    context
        .push(&fragment(
            "future",
            true,
            "Order something else.",
            300.,
            400.,
        ))
        .unwrap();
    let task = context.request(250.).unwrap().unwrap();
    assert!(task.contains("Check Tuesday."));
    assert!(task.contains("Your calendar?"));
    assert!(task.contains("Yes, calendar."));
    assert!(!task.contains("Order something else."));
    assert!(task.contains("\"user\""));
    assert!(task.contains("\"assistant\""));
    assert!(task.len() <= 12000);
}
#[test]
fn missing_partial_or_already_consumed_request_needs_clarification() {
    let mut context = LiveContext::default();
    assert!(context.request(0.).unwrap().is_none());
    context
        .push(&fragment("one", true, "Check calendar", 100., 200.))
        .unwrap();
    assert!(context.request(150.).unwrap().is_none());
    context
        .push(&fragment(
            "two",
            true,
            "Check Wednesday calendar",
            250.,
            350.,
        ))
        .unwrap();
    assert!(context.request(400.).unwrap().is_some());
    assert!(context.request(450.).unwrap().is_none());
}
#[test]
fn duplicate_transcript_does_not_duplicate_text() {
    let mut context = LiveContext::default();
    let input = fragment("one", true, "Check calendar", 0., 100.);
    context.push(&input).unwrap();
    context.push(&input).unwrap();
    assert_eq!(
        context
            .request(150.)
            .unwrap()
            .unwrap()
            .matches("Check calendar")
            .count(),
        1
    );
    assert!(context
        .push(&fragment("one", true, "Delete calendar", 0., 100.))
        .is_err());
}
#[test]
fn lost_context_requires_repeat_instead_of_dispatching_a_truncated_request() {
    let mut context = LiveContext::default();
    for i in 0..200 {
        context
            .push(&fragment(
                &format!("evt-{i}"),
                true,
                &"x".repeat(100),
                i as f64 * 100.,
                i as f64 * 100. + 50.,
            ))
            .unwrap();
    }
    assert!(context.request(20000.).unwrap().is_none());
    context
        .push(&fragment("repeat", true, "Check calendar", 21000., 21100.))
        .unwrap();
    assert!(context.request(21200.).unwrap().is_some());
}
