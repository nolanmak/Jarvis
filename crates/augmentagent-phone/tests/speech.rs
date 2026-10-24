//! Offline tests for the TTS text pipeline: Markdown/URL/emoji sanitizer,
//! sentence-boundary chunker and the SpeechSender streaming API (#1547).
use augmentagent_phone::{
    ingress::{SpeechEvent, SpeechSender},
    relay::{RelayInput, RelaySession, RelayTurn},
    speech::{Chunker, SpeechStream, SpeechTextTransform, MAX_CHUNK_CHARS, MAX_HOLD_BACK},
};
use proptest::prelude::*;
use std::sync::Arc;
use unicode_general_category::{get_general_category, GeneralCategory};

const ABBREVIATIONS: [&str; 15] = [
    "Mr", "Mrs", "Ms", "Dr", "Prof", "Sr", "Jr", "St", "vs", "etc", "e.g", "i.e", "a.m", "p.m",
    "U.S",
];

// Step 6 / AC8: exact plain-text output for Markdown inputs.
const SANITIZE_CASES: [(&str, &str); 25] = [
    ("# Heading\nBody text.", "Heading\nBody text."),
    ("## Second level heading", "Second level heading"),
    ("Plain sentence with no markup.", "Plain sentence with no markup."),
    ("This is **bold** and this is *italic*.", "This is bold and this is italic."),
    ("Use __strong__ and _emphasis_ here.", "Use strong and emphasis here."),
    ("Keep snake_case_names intact.", "Keep snake_case_names intact."),
    ("- first\n- second\n- third", "first\nsecond\nthird"),
    ("* alpha\n  * nested beta\n    * deeper gamma", "alpha\nnested beta\ndeeper gamma"),
    ("1. one\n2. two\n10. ten", "one\ntwo\nten"),
    ("Run `cargo test` now.", "Run cargo test now."),
    ("Example:\n```rust\nfn main() {}\n```\nDone.", "Example:\ncode omitted\nDone."),
    ("```\nonly code\n```", "code omitted"),
    ("See [the docs](https://example.com/docs) for details.", "See the docs for details."),
    ("Visit https://example.com/path?q=1 today.", "Visit a link today."),
    ("Go to www.example.com.", "Go to a link."),
    ("Great job 🎉 everyone 👍🏽!", "Great job everyone !"),
    ("Temperature is 72°F ✅", "Temperature is 72F"),
    ("| Name | Qty |\n|------|-----|\n| Apple | 3 |", "Name Qty\nApple 3"),
    ("> quoted line\nnormal line", "quoted line\nnormal line"),
    ("Multiple    spaces\n\n\nand blank lines.", "Multiple spaces\nand blank lines."),
    ("A line ending in a hard break  \nnext line.", "A line ending in a hard break\nnext line."),
    ("Mixed: **bold with `code`** and [link](http://x.io).", "Mixed: bold with code and link."),
    ("Issue #1547 is tracked.", "Issue 1547 is tracked."),
    ("---\nAfter a rule.", "After a rule."),
    ("![diagram](https://img.example/a.png) shows it.", "diagram shows it."),
];

#[test]
fn ac08_markdown_inputs_sanitize_to_exact_plain_text() {
    for (input, expected) in SANITIZE_CASES {
        assert_eq!(SpeechTextTransform::apply(input), expected, "input: {input:?}");
    }
}

fn forbidden_symbol(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::OtherSymbol | GeneralCategory::ModifierSymbol
    )
}

fn assert_no_forbidden(output: &str) {
    for needle in ["*", "#", "`", "http://", "https://", "]("] {
        assert!(!output.contains(needle), "{needle:?} in {output:?}");
    }
    assert!(
        !output.chars().any(forbidden_symbol),
        "So/Sk symbol in {output:?}"
    );
}

// Step 7 / AC8: no forbidden characters or sequences for arbitrary input.
proptest! {
    #![proptest_config(ProptestConfig { cases: 512, failure_persistence: None, .. ProptestConfig::default() })]
    #[test]
    fn ac08_sanitized_output_never_contains_markup_or_urls(input in "\\PC{0,200}") {
        assert_no_forbidden(&SpeechTextTransform::apply(&input));
    }
    #[test]
    fn ac08_markdown_shaped_input_never_contains_markup_or_urls(
        input in "([A-Za-z0-9 .!?\\n#*`_\\-|>\\[\\]()~:/]|https?://|www\\.|```|\\]\\(|😀|🎉)*"
    ) {
        assert_no_forbidden(&SpeechTextTransform::apply(&input));
    }
}

const STREAM_SAMPLE: &str = "# Status update\n\nThe **deploy** finished at 3 p.m. today; see [the release notes for this build](https://example.com/release/42) or https://example.com/dash 🎉.\n\n- item one\n- item `two`\n\n```sh\ncargo test -p augmentagent-phone\n```\n\n| Suite | Count |\n|------|------:|\n| relay | 8 |\n\n[a link text that is deliberately much longer than the hold-back allows](https://x.y) __done__ ✅ 👍🏽\n> quoted\nhttp://trailing.example/path. The End.";

// Step 8 / AC9: one char per paused-clock tick equals the whole-string
// transform, and the hold-back is bounded on every tick.
#[tokio::test(start_paused = true)]
async fn ac09_streaming_transform_matches_whole_string_within_hold_back() {
    let whole = SpeechTextTransform::apply(STREAM_SAMPLE);
    let mut transform = SpeechTextTransform::new();
    let mut streamed = String::new();
    let mut max_held = 0;
    for c in STREAM_SAMPLE.chars() {
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        streamed.push_str(&transform.push(&c.to_string()));
        let held = transform.held();
        assert!(held <= MAX_HOLD_BACK, "hold-back {held} after {streamed:?}");
        max_held = max_held.max(held);
    }
    streamed.push_str(&transform.flush());
    assert_eq!(transform.held(), 0);
    assert_eq!(streamed, whole);
    assert!(max_held > 8, "sample must exercise the hold-back ({max_held})");
    assert_no_forbidden(&whole);
    assert!(whole.contains("code omitted"));
    assert!(whole.contains("a link"));
    assert!(whole.contains("the release notes for this build"));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, .. ProptestConfig::default() })]
    #[test]
    fn ac09_random_split_points_do_not_change_sanitized_output(
        input in "([A-Za-z0-9 .!?\\n#*`_\\-|>\\[\\]()~:/]|https?://|www\\.|```|\\]\\(|😀)*",
        cuts in proptest::collection::vec(0usize..200, 0..6)
    ) {
        let whole = SpeechTextTransform::apply(&input);
        let chars: Vec<char> = input.chars().collect();
        let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(chars.len())).collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut transform = SpeechTextTransform::new();
        let mut streamed = String::new();
        let mut start = 0;
        for cut in cuts.into_iter().chain([chars.len()]) {
            let piece: String = chars[start..cut].iter().collect();
            streamed.push_str(&transform.push(&piece));
            prop_assert!(transform.held() <= MAX_HOLD_BACK);
            start = cut;
        }
        streamed.push_str(&transform.flush());
        prop_assert_eq!(streamed, whole);
    }
}

// Step 9 / AC10: exact split positions.
const BOUNDARY_CASES: [(&str, &[&str]); 20] = [
    ("Dr. Smith is here.", &["Dr. Smith is here."]),
    ("It costs 3.50 today.", &["It costs 3.50 today."]),
    ("Visit example.com. Then call.", &["Visit example.com. ", "Then call."]),
    ("She said \"Dr. S\" and left.", &["She said \"Dr. S\" and left."]),
    ("Done.• Next.", &["Done. ", "Next."]),
    ("First sentence. Second sentence.", &["First sentence. ", "Second sentence."]),
    ("Really? Yes! Great.", &["Really? ", "Yes! ", "Great."]),
    ("Wait... what", &["Wait... what"]),
    ("Line one\nLine two", &["Line one\n", "Line two"]),
    ("Mr. and Mrs. Jones arrived. They sat.", &["Mr. and Mrs. Jones arrived. ", "They sat."]),
    ("Call at 5 p.m. Tomorrow works.", &["Call at 5 p.m. Tomorrow works."]),
    ("The U.S. Army. It marched.", &["The U.S. Army. ", "It marched."]),
    ("e.g. Apples are red. I.e. Fruit.", &["e.g. Apples are red. ", "I.e. Fruit."]),
    ("Version 2.0 shipped; 3.0 is next.", &["Version 2.0 shipped; ", "3.0 is next."]),
    ("He said \"Stop.\" Then he left.", &["He said \"Stop.\" ", "Then he left."]),
    ("One.•Two.•Three.", &["One.", "Two.", "Three."]),
    ("Finished.  \n\nNext paragraph.", &["Finished.  \n\n", "Next paragraph."]),
    ("Email a.b@example.com? Reply soon.", &["Email a.b@example.com? ", "Reply soon."]),
    ("Prof. Jones vs. Dr. Lee; St. Mary etc. Fine.", &["Prof. Jones vs. Dr. Lee; ", "St. Mary etc. Fine."]),
    ("Pause… Then go. and stop", &["Pause… ", "Then go. and stop"]),
];

#[test]
fn ac10_boundary_table_splits_at_expected_positions() {
    for (input, expected) in BOUNDARY_CASES {
        assert_eq!(Chunker::chunk(input), expected, "input: {input:?}");
    }
}

#[test]
fn ac03_cap_splits_at_the_last_whitespace_or_not_at_all() {
    let words = "word ".repeat(30);
    assert_eq!(
        Chunker::chunk(&words),
        ["word ".repeat(24).as_str(), "word ".repeat(6).as_str()]
    );
    let unbroken = "a".repeat(130);
    assert_eq!(Chunker::chunk(&unbroken), [unbroken.as_str()]);
    let mixed = format!("{unbroken} tail");
    assert_eq!(Chunker::chunk(&mixed), [format!("{unbroken} ").as_str(), "tail"]);
}

// A chunk may not keep a sentence boundary inside it: `[.!?…;]`, optional
// closing quotes, whitespace, then an uppercase letter or digit, unless the
// preceding word is an abbreviation.
fn internal_boundary(chunk: &str) -> bool {
    let chars: Vec<char> = chunk.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !matches!(c, '.' | '!' | '?' | '…' | ';') {
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && matches!(chars[j], '"' | '\'' | '”' | '’' | ')') {
            j += 1;
        }
        if j >= chars.len() || !chars[j].is_whitespace() {
            continue;
        }
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if j >= chars.len() || !(chars[j].is_uppercase() || chars[j].is_numeric()) {
            continue;
        }
        let word: String = chars[..i]
            .iter()
            .rev()
            .take_while(|c| !c.is_whitespace())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let word = word.trim_start_matches(|c: char| !c.is_alphanumeric());
        if c == '.' && ABBREVIATIONS.iter().any(|a| a.eq_ignore_ascii_case(word)) {
            continue;
        }
        return true;
    }
    false
}

// Step 2 / AC2 + AC3: byte-for-byte concatenation, cap, punctuation ends chunks.
proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, .. ProptestConfig::default() })]
    #[test]
    fn ac02_chunks_concatenate_to_the_input_byte_for_byte(input in "[^•]{0,400}") {
        let chunks = Chunker::chunk(&input);
        prop_assert_eq!(chunks.concat(), input);
        prop_assert!(chunks.iter().all(|c| !c.is_empty()));
    }
    #[test]
    fn ac03_chunks_respect_cap_and_end_at_sentence_punctuation(
        input in "([A-Za-z]{1,12}|[0-9]{1,3}|[ ]{1,3}|\\n|Dr\\.|e\\.g\\.|[.!?…;]|[.][0-9]|\"|’)*"
    ) {
        let chunks = Chunker::chunk(&input);
        prop_assert_eq!(chunks.concat(), input.clone());
        for (i, chunk) in chunks.iter().enumerate() {
            let count = chunk.chars().count();
            if count > MAX_CHUNK_CHARS {
                // Only an unbroken token may exceed the cap; it carries at
                // most the whitespace that ended it.
                prop_assert!(!chunk.trim_end().chars().any(char::is_whitespace), "{chunk:?}");
                prop_assert!(chunk.trim_end().chars().count() + 1 >= count, "{chunk:?}");
            }
            if i + 1 < chunks.len() {
                prop_assert!(chunk.ends_with(char::is_whitespace), "{chunk:?}");
            }
            prop_assert!(!internal_boundary(chunk), "{chunk:?}");
        }
    }
    #[test]
    fn ac10_bullet_always_splits_and_is_never_emitted(
        input in "([A-Za-z]{1,8}|[ ]|[.!?]|•){0,80}"
    ) {
        let chunks = Chunker::chunk(&input);
        prop_assert!(chunks.iter().all(|c| !c.contains('•')));
        let stripped = input.replace('•', "");
        prop_assert_eq!(chunks.concat(), stripped.clone());
        // Byte offset in the bullet-free text -> index of the chunk holding it.
        let mut owner = Vec::with_capacity(stripped.len());
        for (index, chunk) in chunks.iter().enumerate() {
            owner.extend(std::iter::repeat_n(index, chunk.len()));
        }
        // The last word before a bullet and the first word after it never
        // share a chunk.
        let mut offset = 0;
        let segments: Vec<&str> = input.split('•').collect();
        for pair in segments.windows(2) {
            offset += pair[0].len();
            let before = pair[0].trim_end();
            let after_ws = pair[1].len() - pair[1].trim_start().len();
            if !before.is_empty() && after_ws < pair[1].len() {
                prop_assert_ne!(owner[offset - 1 - (pair[0].len() - before.len())],
                    owner[offset + after_ws], "{:?} -> {:?}", input, chunks);
            }
        }
    }
}

fn normalized(text: &str) -> String {
    text.replace('•', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// AC11: sanitize-then-chunk loses or duplicates no words.
proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, .. ProptestConfig::default() })]
    #[test]
    fn ac11_sanitized_then_chunked_matches_sanitized_whole_text(
        input in "([A-Za-z0-9]{1,10}|[ ]{1,2}|\\n|[.!?•#*`_|>]|\\[[a-z]{1,5}\\]\\(https://x\\.y\\)|https://ex\\.am/[a-z]{1,4}|```\\n[a-z ]{0,10}\\n```|🎉)*",
        cuts in proptest::collection::vec(0usize..300, 0..5)
    ) {
        let whole = SpeechTextTransform::apply(&input);
        let chars: Vec<char> = input.chars().collect();
        let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(chars.len())).collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut stream = SpeechStream::new();
        let mut chunks = Vec::new();
        let mut start = 0;
        for cut in cuts.into_iter().chain([chars.len()]) {
            let piece: String = chars[start..cut].iter().collect();
            chunks.extend(stream.push(&piece));
            start = cut;
        }
        chunks.extend(stream.flush());
        prop_assert!(chunks.iter().all(|c| !c.is_empty() && !c.contains('•')));
        prop_assert_eq!(normalized(&chunks.concat()), normalized(&whole));
    }
}

fn relay_turn(prompt: &str) -> RelayTurn {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session
        .receive(r#"{"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test"}"#)
        .unwrap();
    let RelayInput::Turn(turn) = session
        .receive(&serde_json::json!({"type":"prompt","voicePrompt":prompt,"last":true}).to_string())
        .unwrap()
    else {
        panic!()
    };
    turn
}

// SpeechSender streams sanitized chunks and closes with last:true; clones
// share one pipeline so the MCP speak path and the agent agree on state.
#[tokio::test]
async fn speech_sender_streams_sanitized_chunks_then_a_last_frame() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let sender = SpeechSender::new(tx, Arc::new(relay_turn("question")));
    let agent_side = sender.clone();
    agent_side.speak_partial("**First** sentence. Sec").await.unwrap();
    agent_side.speak_partial("ond one? Visit https://ex.am/ple no").await.unwrap();
    agent_side.speak_partial("w. Third").await.unwrap();
    sender.finish().await.unwrap();
    sender.finish().await.unwrap();
    assert!(sender.speak_partial("after finish").await.is_err());
    drop(sender);
    drop(agent_side);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let tokens: Vec<(String, bool)> = events
        .iter()
        .map(|event| match event {
            SpeechEvent::Token { text, last, .. } => (text.clone(), *last),
            other => panic!("unexpected event {other:?}"),
        })
        .collect();
    assert_eq!(
        tokens,
        [
            ("First sentence. ".to_string(), false),
            ("Second one? ".into(), false),
            ("Visit a link now. ".into(), false),
            ("Third".into(), true),
        ]
    );
    assert!(events.iter().all(|e| matches!(e, SpeechEvent::Token { turn, .. } if turn.text == "question")));
}

// A sentence finished by a token (punctuation plus whitespace) is sent at
// once; the pipeline never waits for the next word to learn its case.
#[test]
fn stream_closes_a_finished_sentence_at_the_end_of_a_token() {
    let mut stream = SpeechStream::new();
    assert_eq!(stream.push("First sentence. "), ["First sentence. "]);
    assert!(stream.push("Price is 3.").is_empty(), "a decimal may follow");
    assert!(stream.push("50").is_empty());
    assert_eq!(stream.push(" and done.\n"), ["Price is 3.50 and done.\n"]);
    assert!(stream.push("Dr. ").is_empty(), "abbreviations never close");
    assert_eq!(stream.flush(), ["Dr."]);
}

#[tokio::test]
async fn speech_sender_finish_without_text_sends_an_empty_last_frame() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let sender = SpeechSender::new(tx, Arc::new(relay_turn("question")));
    sender.finish().await.unwrap();
    drop(sender);
    let event = rx.recv().await.unwrap();
    assert!(matches!(event, SpeechEvent::Token { ref text, last: true, .. } if text.is_empty()));
    assert!(rx.recv().await.is_none());
}
