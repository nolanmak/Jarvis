//! #1294 — Markdown → Slack mrkdwn and long-answer splitting.
//!
//! Pure functions, no network. The conversion table pins every rule the
//! converter applies (and the ones it deliberately does not); the splitting
//! tests check that no character is lost or duplicated, code fences stay
//! balanced in every part and no part exceeds its budget.

use augmentagent_channel_slack::delivery::{
    markdown_to_mrkdwn, split_message, MessagePart, DEFAULT_PART_CHARS, SLACK_TEXT_LIMIT,
};

const WJ: &str = "\u{2060}";

#[test]
fn markdown_conversion_table() {
    let cases: &[(&str, &str, &str)] = &[
        ("bold stars", "**bold** text", "*bold* text"),
        ("bold underscores", "__bold__ text", "*bold* text"),
        ("italic star", "an *italic* word", "an _italic_ word"),
        ("italic underscore", "an _italic_ word", "an _italic_ word"),
        ("strike", "~~gone~~ now", "~gone~ now"),
        (
            "nested bold link",
            "**see [x](https://e.com)**",
            "*see <https://e.com|x>*",
        ),
        (
            "link with query",
            "[docs](https://example.com/a?b=1&c=2)",
            "<https://example.com/a?b=1&amp;c=2|docs>",
        ),
        (
            "link text formatted",
            "[**b**](https://e.com)",
            "<https://e.com|*b*>",
        ),
        (
            "non-http link is not linked",
            "[x](javascript:alert(1))",
            "x (javascript:alert(1))",
        ),
        ("autolink", "<https://example.com>", "<https://example.com>"),
        ("heading", "# Title", "*Title*"),
        ("closed heading", "### Sub ###", "*Sub*"),
        ("heading with code", "## Use `x`", "*Use `x`*"),
        ("bullets", "- one\n- two\n+ three", "• one\n• two\n• three"),
        ("nested bullet", "- a\n  * nested", "• a\n  • nested"),
        ("task list", "- [ ] todo\n- [x] done", "• ☐ todo\n• ☑ done"),
        (
            "ordered list kept",
            "1. first\n2. second",
            "1. first\n2. second",
        ),
        ("blockquote", "> quoted *em*", "> quoted _em_"),
        ("rule", "a\n\n---\n\nb", "a\n\n───\n\nb"),
        ("escaping", "a & b < c > d", "a &amp; b &lt; c &gt; d"),
        (
            "inline code verbatim",
            "`x < y && *z* @here`",
            "`x &lt; y &amp;&amp; *z* @here`",
        ),
        (
            "fenced code verbatim, language dropped",
            "```rust\nfn main() { let _x = a**b; } // <T>\n```",
            "```\nfn main() { let _x = a**b; } // &lt;T&gt;\n```",
        ),
        (
            "tilde fence",
            "~~~\n**not bold**\n~~~",
            "```\n**not bold**\n```",
        ),
        ("unclosed fence is closed", "```\ncode", "```\ncode\n```"),
        (
            "table becomes a code block",
            "| a | b |\n|---|---|\n| 1 | <2> |",
            "```\n| a | b |\n|---|---|\n| 1 | &lt;2&gt; |\n```",
        ),
        (
            "snake_case and arithmetic untouched",
            "snake_case_name and 2*3*4",
            "snake_case_name and 2*3*4",
        ),
        ("unmatched marker", "a ** b", "a ** b"),
        ("backslash escape kept", "\\*x\\*", "\\*x\\*"),
        ("crlf normalised", "a\r\nb", "a\nb"),
        ("unicode passes through", "héllo 日本 🎉", "héllo 日本 🎉"),
    ];
    for (name, input, want) in cases {
        assert_eq!(markdown_to_mrkdwn(input), *want, "case: {name}");
    }
}

#[test]
fn model_text_can_never_ping_anyone() {
    let cases: &[(&str, String)] = &[
        ("@channel look", format!("@{WJ}channel look")),
        ("hey @here!", format!("hey @{WJ}here!")),
        ("(@everyone)", format!("(@{WJ}everyone)")),
        ("@Channel", format!("@{WJ}Channel")),
        ("**@here**", format!("*@{WJ}here*")),
        (
            "[@channel](https://e.com)",
            format!("<https://e.com|@{WJ}channel>"),
        ),
        // Slack's special-mention and user/group syntax is escaped, so it
        // renders as text instead of notifying.
        ("<!channel> hi", "&lt;!channel&gt; hi".into()),
        ("<!here|here>", "&lt;!here|here&gt;".into()),
        ("<!everyone>", "&lt;!everyone&gt;".into()),
        ("<@U00000001> ping", "&lt;@U00000001&gt; ping".into()),
        ("<!subteam^S0001>", "&lt;!subteam^S0001&gt;".into()),
        ("<#C00000001>", "&lt;#C00000001&gt;".into()),
        // A link whose URL is a mention is not a link.
        ("[x](<!channel>)", "x (&lt;!channel&gt;)".into()),
        // Not mentions: an address and a longer word.
        ("mail me@here.com", "mail me@here.com".into()),
        ("@heretic", "@heretic".into()),
    ];
    for (input, want) in cases {
        let got = markdown_to_mrkdwn(input);
        assert_eq!(&got, want, "input: {input}");
        assert!(!got.contains("<!"), "special mention survived: {got}");
        assert!(!got.contains("<@"), "user mention survived: {got}");
    }
}

/// The logical content of the parts, in order, with the fences the splitter
/// added removed.
fn logical(parts: &[MessagePart], source: &str) -> String {
    parts.iter().map(|p| &source[p.range.clone()]).collect()
}

fn check_invariants(source: &str, max: usize, parts: &[MessagePart]) {
    assert_eq!(logical(parts, source), source, "content lost or duplicated");
    let mut expect_start = 0;
    for (i, p) in parts.iter().enumerate() {
        assert_eq!(p.range.start, expect_start, "part {i} is not contiguous");
        expect_start = p.range.end;
        assert!(!p.range.is_empty(), "part {i} is empty");
        let n = p.text.chars().count();
        assert!(n <= max, "part {i} has {n} chars > {max}");
        let prefix = if p.reopened_fence { "```\n" } else { "" };
        let body = &source[p.range.clone()];
        let suffix = if !p.closed_fence {
            ""
        } else if body.ends_with('\n') {
            "```"
        } else {
            "\n```"
        };
        assert_eq!(p.text, format!("{prefix}{body}{suffix}"), "part {i} text");
        let fences = p
            .text
            .lines()
            .filter(|l| l.trim_start().starts_with("```"))
            .count();
        assert_eq!(fences % 2, 0, "part {i} has an unbalanced code fence");
    }
}

#[test]
fn documented_limits_are_the_ones_used() {
    assert_eq!(SLACK_TEXT_LIMIT, 4_000);
    assert!(DEFAULT_PART_CHARS < SLACK_TEXT_LIMIT);
}

#[test]
fn short_and_empty_answers() {
    assert!(split_message("", 100).is_empty());
    let parts = split_message("hello", 100);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].text, "hello");
    check_invariants("hello", 100, &parts);
}

#[test]
fn long_answer_splits_at_paragraph_boundaries() {
    let para = "word ".repeat(40); // 200 chars
    let source: String = (0..30)
        .map(|i| format!("{i:02} {para}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let parts = split_message(&source, 1_000);
    assert!(parts.len() > 5);
    check_invariants(&source, 1_000, &parts);
    for p in &parts[..parts.len() - 1] {
        assert!(
            source[..p.range.end].ends_with("\n\n"),
            "cut not at a paragraph boundary: {:?}",
            &source[p.range.end.saturating_sub(10)..p.range.end]
        );
    }
}

#[test]
fn lines_are_preferred_over_words_when_there_are_no_paragraphs() {
    let source: String = (0..100).map(|i| format!("line {i} of text\n")).collect();
    let parts = split_message(&source, 200);
    check_invariants(&source, 200, &parts);
    for p in &parts[..parts.len() - 1] {
        assert!(source[..p.range.end].ends_with('\n'));
    }
}

#[test]
fn code_block_longer_than_a_part_is_closed_and_reopened() {
    let code: String = (0..200).map(|i| format!("let x{i} = {i} * 2;\n")).collect();
    let source = format!("Intro paragraph.\n\n```\n{code}```\n\nOutro.");
    let parts = split_message(&source, 500);
    assert!(parts.len() > 3);
    check_invariants(&source, 500, &parts);
    assert!(parts.iter().any(|p| p.reopened_fence));
    assert!(parts.iter().any(|p| p.closed_fence));
    // Every code line reaches Slack whole, inside a code block.
    for i in 0..200 {
        let line = format!("let x{i} = {i} * 2;");
        assert_eq!(
            parts.iter().filter(|p| p.text.contains(&line)).count(),
            1,
            "{line}"
        );
    }
}

#[test]
fn a_word_longer_than_a_part_is_hard_cut_without_loss() {
    let source = "x".repeat(3_000);
    let parts = split_message(&source, 1_000);
    assert_eq!(parts.len(), 3);
    check_invariants(&source, 1_000, &parts);
}

#[test]
fn budget_is_counted_in_characters_and_cuts_on_char_boundaries() {
    let source = "日本語🎉".repeat(500);
    let parts = split_message(&source, 300);
    check_invariants(&source, 300, &parts);
}

#[test]
fn entities_links_and_inline_code_are_never_cut() {
    let source = "&amp;".repeat(400);
    let parts = split_message(&source, 103);
    check_invariants(&source, 103, &parts);
    for p in &parts {
        assert_eq!(p.text.replace("&amp;", ""), "", "entity cut: {}", p.text);
    }
    let link = "<https://example.com/some/long/path|a link>";
    let source = format!("{}{}", "a".repeat(90), link.repeat(10));
    let parts = split_message(&source, 100);
    check_invariants(&source, 100, &parts);
    for p in &parts {
        assert_eq!(
            p.text.matches('<').count(),
            p.text.matches('>').count(),
            "link cut: {}",
            p.text
        );
    }
    let source = format!("{}`inline code span`{}", "b".repeat(95), "c".repeat(50));
    let parts = split_message(&source, 100);
    check_invariants(&source, 100, &parts);
    assert!(parts.iter().any(|p| p.text.contains("`inline code span`")));
}

/// Small deterministic generator so the property test needs no new crate.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() as usize) % items.len()]
    }
}

#[test]
fn property_parts_always_reassemble_to_the_input() {
    let atoms = [
        "word ",
        "longerword ",
        "é",
        "日本",
        "🎉",
        "\n",
        "\n\n",
        "&amp;",
        "&lt;",
        "`code`",
        "<https://e.com|link>",
        "*bold* ",
        "\n```\n",
        "    indented\n",
        "x",
        " ",
    ];
    let mut rng = Lcg(0x1294);
    for case in 0..400 {
        let len = 1 + (rng.next() % 400) as usize;
        let mut source = String::new();
        for _ in 0..len {
            source.push_str(rng.pick(&atoms));
        }
        // The splitter runs on converted text, whose fences the converter
        // always balances.
        let converted = markdown_to_mrkdwn(&source);
        let max = 40 + (rng.next() % 460) as usize;
        let parts = split_message(&converted, max);
        assert_eq!(parts.is_empty(), converted.is_empty(), "case {case}");
        check_invariants(&converted, max, &parts);
    }
}
