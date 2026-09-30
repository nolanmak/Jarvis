//! Markdown (as models write it) → Slack `mrkdwn`.
//!
//! Rules, from Slack's formatting reference (read 2026-09-29, see
//! `docs/SLACK-TRANSPORT.md`):
//!
//! - `&`, `<` and `>` are sent as `&amp;`, `&lt;`, `&gt;` everywhere, code
//!   included, so model text can never form a `<…>` control sequence. That
//!   alone neutralises `<!channel>`, `<!here>`, `<!everyone>`, `<@U…>`,
//!   `<#C…>` and `<!subteam^…>`: they render as text and notify nobody.
//!   The only `<…>` this module emits are links it built itself from an
//!   `http(s)`/`mailto` URL with no `<`, `>`, `|` or whitespace in it.
//! - Plain `@channel` / `@here` / `@everyone` do not notify (documented), but
//!   a word joiner (U+2060) is still inserted after the `@` so they cannot
//!   be linked by `link_names` or a future parser change. The delivery
//!   planner also sends `link_names: false`.
//! - Code (fenced and inline) is kept verbatim apart from that escaping,
//!   which Slack renders back to the original characters. Fence language
//!   tags are dropped (Slack would print them as the first code line).
//! - `**b**`/`__b__` → `*b*`, `*i*`/`_i_` → `_i_`, `~~s~~` → `~s~`,
//!   `[t](url)` → `<url|t>`, headings → a bold line, `-`/`*`/`+` bullets →
//!   `•`, task boxes → `☐`/`☑`, rules → `───`, pipe tables → a code block
//!   (Slack has no tables). Ordered lists and `>` quotes pass through.
//!
//! Not handled (left as text): indented code blocks, setext headings, HTML,
//! reference-style links, images (shown as a link to the image URL).

const WORD_JOINER: char = '\u{2060}';
const SPECIAL_MENTIONS: [&str; 3] = ["channel", "here", "everyone"];

/// Convert model Markdown to Slack mrkdwn. Deterministic: the same input
/// always gives the same output, which the delivery planner relies on for
/// idempotent re-planning after a restart.
pub fn markdown_to_mrkdwn(input: &str) -> String {
    let text = input.replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some((fence_char, run)) = fence_open(line) {
            out.push("```".into());
            i += 1;
            while i < lines.len() && !fence_close(lines[i], fence_char, run) {
                out.push(escape(lines[i]));
                i += 1;
            }
            out.push("```".into());
            i += 1;
            continue;
        }
        if is_table_start(&lines, i) {
            out.push("```".into());
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                out.push(escape(lines[i]));
                i += 1;
            }
            out.push("```".into());
            continue;
        }
        out.push(convert_line(line));
        i += 1;
    }
    out.join("\n")
}

/// `&`, `<`, `>` → entities. Nothing else is touched.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        push_escaped(&mut out, ch);
    }
    out
}

fn push_escaped(out: &mut String, ch: char) {
    match ch {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        _ => out.push(ch),
    }
}

fn strip_indent(line: &str, max: usize) -> Option<&str> {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    (spaces <= max).then(|| &line[spaces..])
}

fn fence_open(line: &str) -> Option<(char, usize)> {
    let rest = strip_indent(line, 3)?;
    let ch = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = rest.chars().take_while(|c| *c == ch).count();
    if run < 3 {
        return None;
    }
    // A backtick fence's info string may not contain a backtick.
    if ch == '`' && rest[run..].contains('`') {
        return None;
    }
    Some((ch, run))
}

fn fence_close(line: &str, ch: char, run: usize) -> bool {
    let Some(rest) = strip_indent(line, 3) else {
        return false;
    };
    let n = rest.chars().take_while(|c| *c == ch).count();
    n >= run && rest[n * ch.len_utf8()..].trim().is_empty()
}

fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    t.contains('-')
        && t.contains('|')
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

fn is_table_start(lines: &[&str], i: usize) -> bool {
    lines[i].trim_start().starts_with('|')
        && lines
            .get(i + 1)
            .is_some_and(|next| is_table_separator(next))
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    let Some(first) = t.chars().next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_')
        && t.chars().filter(|c| *c == first).count() >= 3
        && t.chars().all(|c| c == first || c == ' ')
}

fn convert_line(line: &str) -> String {
    if line.trim().is_empty() {
        return line.to_string();
    }
    if is_rule(line) {
        return "───".into();
    }
    if let Some(rest) = strip_indent(line, 3) {
        let hashes = rest.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) {
            let after = &rest[hashes..];
            if after.is_empty() || after.starts_with(' ') || after.starts_with('\t') {
                let mut title = after.trim();
                let closed = title.trim_end_matches('#');
                if closed.is_empty() || closed.ends_with(' ') || closed.ends_with('\t') {
                    title = closed.trim_end();
                }
                return if title.is_empty() {
                    String::new()
                } else {
                    format!("*{}*", inline(title))
                };
            }
        }
    }
    let indent_len = line.len() - line.trim_start().len();
    let (indent, body) = line.split_at(indent_len);
    if let Some(quoted) = body.strip_prefix('>') {
        let quoted = quoted.strip_prefix(' ').unwrap_or(quoted);
        return format!("{indent}> {}", convert_line(quoted));
    }
    let mut chars = body.chars();
    if let (Some('-' | '*' | '+'), Some(' ' | '\t')) = (chars.next(), chars.next()) {
        let item = body[2..].trim_start();
        let (box_mark, item) = if let Some(rest) = item.strip_prefix("[ ] ") {
            ("☐ ", rest)
        } else if let Some(rest) = item
            .strip_prefix("[x] ")
            .or_else(|| item.strip_prefix("[X] "))
        {
            ("☑ ", rest)
        } else {
            ("", item)
        };
        return format!("{indent}• {box_mark}{}", inline(item));
    }
    inline(line)
}

fn run_len(c: &[char], i: usize, ch: char) -> usize {
    c[i..].iter().take_while(|x| **x == ch).count()
}

fn is_word(c: Option<&char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric())
}

fn is_space(c: Option<&char>) -> bool {
    c.is_none_or(|c| c.is_whitespace())
}

/// Start of a closing backtick run of exactly `run` backticks at or after `from`.
fn backtick_close(c: &[char], from: usize, run: usize) -> Option<usize> {
    let mut j = from;
    while j < c.len() {
        if c[j] == '`' {
            let n = run_len(c, j, '`');
            if n == run {
                return Some(j);
            }
            j += n;
        } else {
            j += 1;
        }
    }
    None
}

/// `[text](url)` starting at `i`: (text, raw url, index after `)`).
fn parse_link(c: &[char], i: usize) -> Option<(String, String, usize)> {
    let mut depth = 0usize;
    let mut j = i;
    let text_end = loop {
        match c.get(j)? {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            _ => {}
        }
        j += 1;
    };
    if c.get(text_end + 1) != Some(&'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut k = text_end + 1;
    let url_end = loop {
        match c.get(k)? {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break k;
                }
            }
            _ => {}
        }
        k += 1;
    };
    let text: String = c[i + 1..text_end].iter().collect();
    let url: String = c[text_end + 2..url_end].iter().collect();
    Some((text, url.trim().to_string(), url_end + 1))
}

fn linkable(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:"))
        && !url
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '|'))
}

/// `<https://…>` starting at `i`: (url, index after `>`).
fn parse_autolink(c: &[char], i: usize) -> Option<(String, usize)> {
    let end = c[i + 1..].iter().position(|x| *x == '>')? + i + 1;
    let url: String = c[i + 1..end].iter().collect();
    linkable(&url).then_some((url, end + 1))
}

/// Closing `**`/`__` for a strong run opened at `from`: index of the pair
/// that closes it (the last two of a longer run, so `***x***` nests).
fn strong_close(c: &[char], from: usize, ch: char) -> Option<usize> {
    let mut j = from;
    while j + 1 < c.len() {
        if c[j] == ch {
            let n = run_len(c, j, ch);
            let k = j + n - 2;
            if n >= 2
                && !is_space(c.get(j.wrapping_sub(1)))
                && j > from
                && (ch != '_' || !is_word(c.get(k + 2)))
            {
                return Some(k);
            }
            j += n;
        } else {
            j += 1;
        }
    }
    None
}

fn emphasis_close(c: &[char], from: usize, ch: char) -> Option<usize> {
    let mut j = from;
    while j < c.len() {
        if c[j] == ch {
            let n = run_len(c, j, ch);
            if n == 1 && j > from && !is_space(c.get(j - 1)) && !is_word(c.get(j + 1)) {
                return Some(j);
            }
            j += n;
        } else {
            j += 1;
        }
    }
    None
}

fn tilde_close(c: &[char], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 1 < c.len() {
        if c[j] == '~' && c[j + 1] == '~' && j > from && !is_space(c.get(j - 1)) {
            return Some(j);
        }
        j += 1;
    }
    None
}

fn inline(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        let prev = i.checked_sub(1).and_then(|p| c.get(p));
        match ch {
            '\\' if c.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                push_escaped(&mut out, '\\');
                push_escaped(&mut out, c[i + 1]);
                i += 2;
            }
            '`' => {
                let run = run_len(&c, i, '`');
                match backtick_close(&c, i + run, run) {
                    Some(end) => {
                        for x in &c[i..end + run] {
                            push_escaped(&mut out, *x);
                        }
                        i = end + run;
                    }
                    None => {
                        out.extend(std::iter::repeat_n('`', run));
                        i += run;
                    }
                }
            }
            '[' => match parse_link(&c, i) {
                Some((text, url, next)) => {
                    if linkable(&url) {
                        out.push('<');
                        out.push_str(&escape(&url));
                        if !text.trim().is_empty() {
                            out.push('|');
                            out.push_str(&inline(&text));
                        }
                        out.push('>');
                    } else {
                        out.push_str(&inline(&text));
                        out.push_str(" (");
                        out.push_str(&escape(&url));
                        out.push(')');
                    }
                    i = next;
                }
                None => {
                    out.push('[');
                    i += 1;
                }
            },
            '<' => match parse_autolink(&c, i) {
                Some((url, next)) => {
                    out.push('<');
                    out.push_str(&escape(&url));
                    out.push('>');
                    i = next;
                }
                None => {
                    out.push_str("&lt;");
                    i += 1;
                }
            },
            '*' | '_' => {
                let run = run_len(&c, i, ch);
                let opens = !is_space(c.get(i + run)) && (ch == '*' || !is_word(prev));
                if run >= 2 && opens {
                    if let Some(k) = strong_close(&c, i + 2, ch) {
                        let inner: String = c[i + 2..k].iter().collect();
                        out.push('*');
                        out.push_str(&inline(&inner));
                        out.push('*');
                        i = k + 2;
                        continue;
                    }
                }
                if run == 1 && opens && !is_word(prev) {
                    if let Some(k) = emphasis_close(&c, i + 1, ch) {
                        let inner: String = c[i + 1..k].iter().collect();
                        out.push('_');
                        out.push_str(&inline(&inner));
                        out.push('_');
                        i = k + 1;
                        continue;
                    }
                }
                out.extend(std::iter::repeat_n(ch, run));
                i += run;
            }
            '~' if c.get(i + 1) == Some(&'~') && !is_space(c.get(i + 2)) => {
                match tilde_close(&c, i + 2) {
                    Some(k) => {
                        let inner: String = c[i + 2..k].iter().collect();
                        out.push('~');
                        out.push_str(&inline(&inner));
                        out.push('~');
                        i = k + 2;
                    }
                    None => {
                        out.push_str("~~");
                        i += 2;
                    }
                }
            }
            '@' if !is_word(prev) => {
                let word: String = c[i + 1..]
                    .iter()
                    .take_while(|x| x.is_alphanumeric())
                    .collect();
                out.push('@');
                if SPECIAL_MENTIONS
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(&word))
                {
                    out.push(WORD_JOINER);
                }
                i += 1;
            }
            _ => {
                push_escaped(&mut out, ch);
                i += 1;
            }
        }
    }
    out
}
