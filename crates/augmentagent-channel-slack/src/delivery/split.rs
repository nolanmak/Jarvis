//! Split converted mrkdwn into messages that fit Slack's text limit.
//!
//! Budgets are counted in characters (Unicode scalar values). Slack
//! documents the `text` limit in characters; whether it counts UTF-16 units
//! or bytes is unverified, so [`DEFAULT_PART_CHARS`] leaves headroom and even
//! a part made only of 4-byte characters stays far below the documented
//! 40,000-character truncation point (`docs/SLACK-TRANSPORT.md`).
//!
//! Cut preference, within the last half of the budget: after a blank line
//! outside code, after a line outside code, after a line inside a code
//! block, after a space; then the latest cut that does not break an entity
//! (`&amp;`), a `<…>` link or an inline code span; only a single token longer
//! than a whole part is cut blindly. A cut inside a fenced block closes the
//! fence at the end of the part and reopens it at the start of the next, so
//! every part renders on its own. Nothing else is added and nothing is
//! dropped: [`MessagePart::range`]s tile the input exactly.

use std::ops::Range;

/// Slack's documented `chat.postMessage` / `chat.update` `text` limit
/// (characters). **[docs]** 2026-09-29, not verified live.
pub const SLACK_TEXT_LIMIT: usize = 4_000;

/// Characters per part by default: below [`SLACK_TEXT_LIMIT`] with room for
/// the fence markers the splitter may add.
pub const DEFAULT_PART_CHARS: usize = 3_500;

/// Smallest budget honoured; smaller values are raised to this.
pub const MIN_PART_CHARS: usize = 16;

const REOPEN: &str = "```\n";
/// Worst-case characters a part may gain: reopen prefix plus `\n```` suffix.
const FENCE_RESERVE: usize = 4;

/// One message of a split answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagePart {
    /// What is posted: the slice, plus fence markers when cut inside code.
    pub text: String,
    /// Byte range of the input this part carries. Consecutive parts' ranges
    /// are contiguous and cover the whole input.
    pub range: Range<usize>,
    /// `text` starts with a reopened code fence.
    pub reopened_fence: bool,
    /// `text` ends with a fence closing a block continued in the next part.
    pub closed_fence: bool,
}

/// Split `text` into parts of at most `max_chars` characters each (fence
/// markers included). Empty input gives no parts.
pub fn split_message(text: &str, max_chars: usize) -> Vec<MessagePart> {
    if text.is_empty() {
        return Vec::new();
    }
    let max = max_chars.max(MIN_PART_CHARS);
    let layout = Layout::new(text);
    let n = layout.chars.len();
    let mut parts = Vec::new();
    let mut start = 0usize;
    while start < n {
        let reopen = start > 0 && layout.in_code[start];
        let prefix = if reopen { REOPEN.len() } else { 0 };
        if prefix + (n - start) <= max {
            parts.push(layout.part(text, start, n, reopen, false));
            break;
        }
        let limit = start + (max - prefix);
        let mut cut = layout.choose_cut(start, limit);
        if layout.in_code[cut] {
            // The part will gain a closing fence; make room for it.
            cut = layout.choose_cut(start, limit - FENCE_RESERVE);
        }
        parts.push(layout.part(text, start, cut, reopen, layout.in_code[cut]));
        start = cut;
    }
    parts
}

struct Layout {
    /// Characters, and `bounds[k]` = byte offset of char boundary `k`.
    chars: Vec<char>,
    bounds: Vec<usize>,
    /// A cut at boundary `k` falls inside a fenced code block.
    in_code: Vec<bool>,
    /// A cut at boundary `k` would break a token that must stay whole.
    forbidden: Vec<bool>,
}

impl Layout {
    fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let mut bounds: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        bounds.push(text.len());
        let n = chars.len();
        let mut in_code = vec![false; n + 1];
        let mut forbidden = vec![false; n + 1];

        let mut open = false;
        let mut line_start = 0usize;
        while line_start < n {
            let line_end = chars[line_start..]
                .iter()
                .position(|c| *c == '\n')
                .map_or(n, |p| line_start + p);
            let line: String = chars[line_start..line_end].iter().collect();
            let is_fence = line.trim_start().starts_with("```");
            for slot in &mut in_code[line_start..=line_end] {
                *slot = open;
            }
            if is_fence {
                // Never cut a fence line itself.
                for slot in &mut forbidden[line_start + 1..=line_end] {
                    *slot = true;
                }
                open = !open;
            } else if !open {
                mark_spans(&chars[line_start..line_end], line_start, &mut forbidden);
            }
            line_start = line_end + 1;
        }
        // After the final newline the state is whatever the last line left.
        if n > 0 && chars[n - 1] == '\n' {
            in_code[n] = open;
        }
        mark_entities(&chars, &mut forbidden);
        Self {
            chars,
            bounds,
            in_code,
            forbidden,
        }
    }

    fn part(&self, text: &str, start: usize, end: usize, reopen: bool, close: bool) -> MessagePart {
        let range = self.bounds[start]..self.bounds[end];
        let body = &text[range.clone()];
        let mut out = String::with_capacity(body.len() + 8);
        if reopen {
            out.push_str(REOPEN);
        }
        out.push_str(body);
        if close {
            if !body.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```");
        }
        MessagePart {
            text: out,
            range,
            reopened_fence: reopen,
            closed_fence: close,
        }
    }

    fn after(&self, k: usize, ch: char) -> bool {
        k > 0 && self.chars[k - 1] == ch
    }

    fn choose_cut(&self, start: usize, limit: usize) -> usize {
        let min_fill = start + (limit - start).div_ceil(2);
        let allowed = |k: usize| k > start && !self.forbidden[k];
        let prefs: [&dyn Fn(usize) -> bool; 4] = [
            &|k| self.after(k, '\n') && self.after(k - 1, '\n') && !self.in_code[k],
            &|k| self.after(k, '\n') && !self.in_code[k],
            &|k| self.after(k, '\n'),
            &|k| self.after(k, ' ') || self.after(k, '\t'),
        ];
        for pref in prefs {
            if let Some(k) = (min_fill..=limit).rev().find(|&k| allowed(k) && pref(k)) {
                return k;
            }
        }
        (start + 1..=limit)
            .rev()
            .find(|&k| allowed(k))
            .unwrap_or(limit)
    }
}

/// `<…>` links and `` `…` `` spans on one line outside code.
fn mark_spans(line: &[char], offset: usize, forbidden: &mut [bool]) {
    let mut i = 0;
    while i < line.len() {
        let close = match line[i] {
            '<' => line[i + 1..]
                .iter()
                .position(|c| *c == '>' || *c == '<')
                .filter(|&p| line[i + 1 + p] == '>'),
            '`' => line[i + 1..].iter().position(|c| *c == '`'),
            _ => None,
        };
        match close {
            Some(p) => {
                let end = i + 1 + p; // index of the closing char
                for slot in &mut forbidden[offset + i + 1..=offset + end] {
                    *slot = true;
                }
                i = end + 1;
            }
            None => i += 1,
        }
    }
}

/// `&name;` / `&#123;` entities anywhere, code included.
fn mark_entities(chars: &[char], forbidden: &mut [bool]) {
    for i in 0..chars.len() {
        if chars[i] != '&' {
            continue;
        }
        let body = chars[i + 1..]
            .iter()
            .take(9)
            .position(|c| !(c.is_ascii_alphanumeric() || *c == '#'));
        if let Some(p) = body.filter(|&p| p >= 2 && chars[i + 1 + p] == ';') {
            for slot in &mut forbidden[i + 1..=i + 1 + p] {
                *slot = true;
            }
        }
    }
}
