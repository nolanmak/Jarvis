//! The `/yolo` owner text command (#2219, spec #2243 section 5.4). Pure
//! decision and wording; the Discord handler creates the thread.
use augmentagent_approval_discord::owner_text::{OwnerCommandCtx, OwnerCommandOutcome};
use augmentagent_channel_core::yolo::NotReady;
use augmentagent_store::yolo_threads::ThreadMark;

pub const USAGE: &str = "Usage: /yolo | /yolo status";
pub const ALWAYS_YOLO: &str =
    "This thread is always a yolo thread. Continue in the parent channel for normal mode.";
pub const NEEDS_QUERY_CHANNEL: &str = "Yolo mode needs a query channel.";

/// What the command needs to know about the host, gathered by the caller.
pub struct Facts {
    pub readiness: Result<(), NotReady>,
    pub model: String,
    pub zdr_requested: bool,
    /// Sandboxed workspace shell commands are part of this build.
    pub shell: bool,
    /// Web tools are switched on for yolo turns, and which kinds are usable.
    pub web: Option<(bool, bool)>,
    /// One line about filtered results for the provider that would serve
    /// search now, when there is something to say.
    pub web_caveat: Option<String>,
}

fn capabilities(facts: &Facts) -> String {
    let zdr = if facts.zdr_requested {
        "ZDR requested"
    } else {
        "ZDR not requested"
    };
    let shell = if facts.shell {
        "shell: available"
    } else {
        "shell: not available"
    };
    let web = match facts.web {
        None => "web: off".to_string(),
        Some((false, false)) => "web: no provider configured".to_string(),
        Some((search, fetch)) => format!(
            "web: search {}, fetch {}",
            if search { "available" } else { "unavailable" },
            if fetch { "available" } else { "unavailable" }
        ),
    };
    let mut line = format!("Model: {}. {zdr}. {shell}. {web}.", facts.model);
    if let Some(caveat) = &facts.web_caveat {
        line.push(' ');
        line.push_str(caveat);
    }
    line
}

fn status(in_yolo_thread: bool, facts: &Facts) -> String {
    let readiness = match facts.readiness {
        Ok(()) => "ready".to_string(),
        Err(cause) => format!("not available: {}", cause.sentence()),
    };
    let place = if in_yolo_thread {
        "This is a yolo thread."
    } else {
        "This is not a yolo thread. Send /yolo in the query channel to open one."
    };
    format!("{place} Yolo is {readiness}\n{}", capabilities(facts))
}

fn intro(facts: &Facts) -> String {
    format!(
        "Everything in this thread is answered by the unfiltered model, and nothing here is carried into other conversations. It can read your data but cannot send or change anything. To go back, use the parent channel.\n{}",
        capabilities(facts)
    )
}

/// `Some` for every message whose first token is `/yolo`, valid or not.
pub fn run(
    text: &str,
    cx: &OwnerCommandCtx,
    mark: ThreadMark,
    facts: &Facts,
) -> Option<OwnerCommandOutcome> {
    let mut words = text.split_whitespace();
    if words.next() != Some("/yolo") {
        return None;
    }
    let args: Vec<&str> = words.collect();
    let in_yolo_thread = mark == ThreadMark::Yes;
    let reply = match args.as_slice() {
        ["status"] => status(in_yolo_thread, facts),
        ["off"] if in_yolo_thread => ALWAYS_YOLO.to_string(),
        ["off"] => "This is not a yolo thread.".to_string(),
        [] | ["on"] if in_yolo_thread => status(true, facts),
        [] | ["on"] if cx.is_query_channel => {
            return Some(match facts.readiness {
                Ok(()) => OwnerCommandOutcome::OpenYoloThread {
                    intro: intro(facts),
                },
                Err(cause) => OwnerCommandOutcome::Reply(format!(
                    "Yolo is not available: {}",
                    cause.sentence()
                )),
            });
        }
        [] | ["on"] => match cx.query_channel_id {
            Some(channel) => {
                format!("Send /yolo in <#{channel}>; it opens a separate thread so yolo history stays apart.")
            }
            None => NEEDS_QUERY_CHANNEL.to_string(),
        },
        _ => USAGE.to_string(),
    };
    Some(OwnerCommandOutcome::Reply(reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx(is_query_channel: bool, is_dm: bool) -> OwnerCommandCtx {
        OwnerCommandCtx {
            guild_id: (!is_dm).then_some(5),
            channel_id: 42,
            message_id: 9,
            is_dm,
            is_query_channel,
            query_channel_id: Some(77),
        }
    }

    fn facts(readiness: Result<(), NotReady>) -> Facts {
        Facts {
            readiness,
            model: "glm-5.3-uncensored".into(),
            zdr_requested: true,
            shell: false,
            web: Some((true, false)),
            web_caveat: None,
        }
    }

    fn reply(outcome: Option<OwnerCommandOutcome>) -> String {
        match outcome {
            Some(OwnerCommandOutcome::Reply(text)) => text,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn only_the_slash_form_is_a_command_and_every_slash_form_is_consumed() {
        let ready = facts(Ok(()));
        for ordinary in ["yolo", "let's /yolo", "/yolox", "you only live once", ""] {
            assert_eq!(
                run(ordinary, &cx(true, false), ThreadMark::No, &ready),
                None,
                "{ordinary}"
            );
        }
        for invalid in ["/yolo now", "/yolo status extra", "/yolo off please"] {
            assert_eq!(
                reply(run(invalid, &cx(true, false), ThreadMark::No, &ready)),
                USAGE,
                "{invalid}"
            );
        }
    }

    #[test]
    fn in_the_query_channel_a_ready_yolo_opens_a_thread() {
        let outcome = run("/yolo", &cx(true, false), ThreadMark::No, &facts(Ok(()))).unwrap();
        let OwnerCommandOutcome::OpenYoloThread { intro } = outcome else {
            panic!("expected a thread")
        };
        assert!(intro.contains("unfiltered model"), "{intro}");
        assert!(intro.contains("Model: glm-5.3-uncensored. ZDR requested. shell: not available. web: search available, fetch unavailable."), "{intro}");
        // `/yolo on` is the same command.
        assert!(matches!(
            run("/yolo on", &cx(true, false), ThreadMark::No, &facts(Ok(()))),
            Some(OwnerCommandOutcome::OpenYoloThread { .. })
        ));
    }

    #[test]
    fn a_yolo_that_is_not_ready_names_the_cause_and_opens_nothing() {
        for cause in [
            NotReady::Disabled,
            NotReady::MissingKey,
            NotReady::InvalidEndpoint,
        ] {
            let text = reply(run(
                "/yolo",
                &cx(true, false),
                ThreadMark::No,
                &facts(Err(cause)),
            ));
            assert_eq!(text, format!("Yolo is not available: {}", cause.sentence()));
        }
    }

    #[test]
    fn outside_the_query_channel_the_owner_is_pointed_at_it() {
        let ready = facts(Ok(()));
        // A DM, and a thread that is not a yolo thread.
        for context in [cx(false, true), cx(false, false)] {
            let text = reply(run("/yolo", &context, ThreadMark::No, &ready));
            assert_eq!(
                text,
                "Send /yolo in <#77>; it opens a separate thread so yolo history stays apart."
            );
        }
        let mut no_channel = cx(false, true);
        no_channel.query_channel_id = None;
        assert_eq!(
            reply(run("/yolo", &no_channel, ThreadMark::No, &ready)),
            NEEDS_QUERY_CHANNEL
        );
    }

    #[test]
    fn a_yolo_thread_stays_one_and_reports_status() {
        let ready = facts(Ok(()));
        let thread = cx(false, false);
        assert_eq!(
            reply(run("/yolo off", &thread, ThreadMark::Yes, &ready)),
            ALWAYS_YOLO
        );
        // `/yolo` inside a yolo thread never opens another thread.
        for command in ["/yolo", "/yolo on", "/yolo status"] {
            let text = reply(run(command, &thread, ThreadMark::Yes, &ready));
            assert!(
                text.starts_with("This is a yolo thread. Yolo is ready"),
                "{command}: {text}"
            );
        }
        assert_eq!(
            reply(run("/yolo off", &thread, ThreadMark::No, &ready)),
            "This is not a yolo thread."
        );
        let text = reply(run(
            "/yolo status",
            &cx(true, false),
            ThreadMark::No,
            &facts(Err(NotReady::Disabled)),
        ));
        assert!(text.starts_with("This is not a yolo thread."), "{text}");
        assert!(text.contains("not available: yolo is disabled"), "{text}");
    }

    #[test]
    fn status_states_what_is_and_is_not_requested_or_available() {
        let mut detail = facts(Ok(()));
        detail.zdr_requested = false;
        detail.shell = true;
        detail.web = None;
        detail.web_caveat = Some("context may filter results.".into());
        let text = reply(run(
            "/yolo status",
            &cx(false, false),
            ThreadMark::Yes,
            &detail,
        ));
        assert!(
            text.contains(
                "ZDR not requested. shell: available. web: off. context may filter results."
            ),
            "{text}"
        );
        detail.web = Some((false, false));
        let text = reply(run(
            "/yolo status",
            &cx(false, false),
            ThreadMark::Yes,
            &detail,
        ));
        assert!(text.contains("web: no provider configured."), "{text}");
    }
}
