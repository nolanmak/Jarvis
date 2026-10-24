//! Text-to-speech pipeline for streamed answers (#1547). Tokens pass through
//! [`SpeechTextTransform`] (Markdown, URL and emoji removal) and then
//! [`Chunker`] (sentence boundaries, 120-character cap) before a frame is
//! built. Both stages are streaming-safe: the transform holds back at most
//! [`MAX_HOLD_BACK`] characters while waiting for a closing delimiter, and
//! both produce byte-identical output whether fed whole or one character at a
//! time.
use unicode_general_category::{get_general_category, GeneralCategory};

/// A chunk is closed at the last whitespace once it reaches this many chars,
/// unless it contains no whitespace at all.
pub const MAX_CHUNK_CHARS: usize = 120;
/// Upper bound on characters the transform withholds awaiting a delimiter.
pub const MAX_HOLD_BACK: usize = 32;
/// Spoken in place of a fenced code block.
pub const CODE_OMITTED: &str = "code omitted";
/// Spoken in place of a bare URL.
pub const LINK_PHRASE: &str = "a link";
/// Optional sentence marker the fast layer may emit; always a boundary,
/// never spoken.
pub const SENTENCE_MARKER: char = '•';
/// A period after one of these words never ends a sentence.
pub const ABBREVIATIONS: [&str; 15] = [
    "Mr", "Mrs", "Ms", "Dr", "Prof", "Sr", "Jr", "St", "vs", "etc", "e.g", "i.e", "a.m", "p.m",
    "U.S",
];
const URL_PREFIXES: [&str; 3] = ["http://", "https://", "www."];
const TABLE_ROW_CHARS: &str = "|-: \t";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Text,
    /// One or two backticks held; a third opens a fence.
    Backticks,
    /// Inside a fenced block; everything is dropped until the closing fence.
    Fence,
    /// Held text is a prefix of a URL scheme or `www.`.
    MaybeUrl,
    /// Dropping a URL; held text is trailing punctuation that may belong to
    /// the sentence instead.
    Url,
    /// `!` held; `![` starts an image whose alt text is spoken.
    Bang,
    /// `[` and link text held until `](` or abandonment.
    LinkText,
    /// `[text]` held; `(` confirms a link.
    LinkClose,
    /// `]` held in plain text; `](` is never spoken.
    Bracket,
    /// Dropping a link target until `)`.
    LinkUrl,
    /// `_` after a word character; dropped unless a word character follows.
    Underscore,
    /// Possible list marker or rule at the start of a line.
    LineMarker,
    /// Horizontal rule; remaining dashes are dropped.
    Rule,
    /// Possible table separator row held while it only has `|-: ` chars.
    TableRow,
    /// A long separator row; dropped to the end of the line.
    SeparatorRow,
}

/// Streaming Markdown/URL/emoji sanitizer. Feed text with [`push`] and
/// collect ready output; call [`flush`] once at the end of the answer.
///
/// [`push`]: SpeechTextTransform::push
/// [`flush`]: SpeechTextTransform::flush
#[derive(Debug)]
pub struct SpeechTextTransform {
    out: String,
    held: String,
    state: State,
    ticks: u8,
    line_start: bool,
    url_at_token_start: bool,
    /// Collapsed whitespace awaiting the next word; `Some(true)` contains a
    /// newline, which the chunker treats as a boundary.
    pending_ws: Option<bool>,
    started: bool,
    last_out: Option<char>,
}
impl Default for SpeechTextTransform {
    fn default() -> Self {
        Self::new()
    }
}
impl SpeechTextTransform {
    pub fn new() -> Self {
        Self {
            out: String::new(),
            held: String::new(),
            state: State::Text,
            ticks: 0,
            line_start: true,
            url_at_token_start: false,
            pending_ws: None,
            started: false,
            last_out: None,
        }
    }
    /// Whole-string transform: identical to streaming the text in any pieces.
    pub fn apply(text: &str) -> String {
        let mut transform = Self::new();
        let mut out = transform.push(text);
        out.push_str(&transform.flush());
        out
    }
    /// Hand over the collapsed whitespace that is waiting for the next word,
    /// so a stream can close a finished sentence without the next token.
    pub fn take_pending_whitespace(&mut self) -> Option<char> {
        self.pending_ws
            .take()
            .map(|newline| if newline { '\n' } else { ' ' })
    }
    /// Characters consumed but not yet decided (never above [`MAX_HOLD_BACK`]).
    pub fn held(&self) -> usize {
        self.held.chars().count() + usize::from(self.pending_ws.is_some())
    }
    pub fn push(&mut self, text: &str) -> String {
        for c in text.chars() {
            self.step(c);
        }
        std::mem::take(&mut self.out)
    }
    /// Resolves any held text at the end of the answer. Trailing whitespace is
    /// dropped; nothing follows it.
    pub fn flush(&mut self) -> String {
        self.settle();
        self.pending_ws = None;
        self.line_start = true;
        std::mem::take(&mut self.out)
    }
    /// Resolve a held state the way end-of-input would, keeping pending
    /// whitespace. Abandoned link text re-enters the normal rules so no
    /// markup inside it is spoken.
    fn settle(&mut self) {
        loop {
            match self.state {
                State::Text => {
                    debug_assert!(self.held.is_empty());
                    return;
                }
                State::Url => {
                    let trailing = std::mem::take(&mut self.held);
                    self.state = State::Text;
                    self.emit_link_phrase(&trailing);
                }
                State::Fence
                | State::LinkUrl
                | State::Rule
                | State::SeparatorRow
                | State::Backticks
                | State::TableRow
                | State::Underscore => {
                    self.held.clear();
                    self.state = State::Text;
                }
                State::MaybeUrl | State::LineMarker => {
                    let held = std::mem::take(&mut self.held);
                    self.state = State::Text;
                    self.emit_str(&held);
                }
                State::Bang | State::LinkText | State::LinkClose | State::Bracket => {
                    self.abandon(None);
                }
            }
        }
    }
    fn at_token_start(&self) -> bool {
        self.pending_ws.is_some()
            || !self.started
            || !self.last_out.is_some_and(char::is_alphanumeric)
    }
    fn emit(&mut self, c: char) {
        if c.is_whitespace() {
            if self.started {
                let newline = matches!(c, '\n' | '\r');
                self.pending_ws = Some(self.pending_ws.unwrap_or(false) || newline);
            }
            return;
        }
        if dropped_symbol(c) {
            return;
        }
        // Dropped markup between `]` and `(` must not reassemble a link
        // delimiter in the spoken text.
        if c == '(' && self.last_out == Some(']') && self.pending_ws.is_none() {
            return;
        }
        if let Some(newline) = self.pending_ws.take() {
            self.out.push(if newline { '\n' } else { ' ' });
        }
        self.out.push(c);
        self.started = true;
        self.last_out = Some(c);
    }
    fn emit_str(&mut self, text: &str) {
        for c in text.chars() {
            self.emit(c);
        }
    }
    fn emit_link_phrase(&mut self, trailing: &str) {
        self.emit(' ');
        self.emit_str(LINK_PHRASE);
        self.emit_str(trailing);
    }
    /// Held link text was not a link after all: speak its first character
    /// literally and run the rest through the normal rules again.
    fn abandon(&mut self, next: Option<char>) {
        let held = std::mem::take(&mut self.held);
        self.state = State::Text;
        let mut chars = held.chars();
        if let Some(first) = chars.next() {
            self.emit(first);
        }
        for c in chars {
            self.step(c);
        }
        if let Some(c) = next {
            self.step(c);
        }
    }
    fn step(&mut self, c: char) {
        match self.state {
            State::Fence => {
                if c == '`' {
                    self.ticks += 1;
                    if self.ticks == 3 {
                        self.ticks = 0;
                        self.state = State::Text;
                    }
                } else {
                    self.ticks = 0;
                }
            }
            State::Backticks => {
                if c == '`' {
                    self.held.push(c);
                    if self.held.len() == 3 {
                        self.held.clear();
                        self.state = State::Fence;
                        self.ticks = 0;
                        self.emit(' ');
                        self.emit_str(CODE_OMITTED);
                        self.emit(' ');
                    }
                } else {
                    // Inline code: the delimiters go, the content is spoken.
                    self.held.clear();
                    self.state = State::Text;
                    self.step(c);
                }
            }
            State::MaybeUrl => {
                let mut candidate = self.held.to_ascii_lowercase();
                candidate.push(c.to_ascii_lowercase());
                let www_allowed = self.url_at_token_start;
                if URL_PREFIXES
                    .iter()
                    .filter(|prefix| **prefix != "www." || www_allowed)
                    .any(|p| p.starts_with(candidate.as_str()))
                {
                    self.held.push(c);
                    if URL_PREFIXES.contains(&candidate.as_str()) {
                        self.held.clear();
                        self.state = State::Url;
                    }
                } else {
                    let held = std::mem::take(&mut self.held);
                    self.state = State::Text;
                    self.emit_str(&held);
                    self.step(c);
                }
            }
            State::Url => {
                if c.is_whitespace() {
                    let trailing = std::mem::take(&mut self.held);
                    self.state = State::Text;
                    self.emit_link_phrase(&trailing);
                    self.step(c);
                } else if matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | '"' | '\'') {
                    self.held.push(c);
                    if self.held.len() > 8 {
                        self.held.clear();
                    }
                } else {
                    self.held.clear();
                }
            }
            State::Bang => {
                self.held.clear();
                self.state = State::Text;
                if c == '[' {
                    self.step(c);
                } else {
                    self.emit('!');
                    self.step(c);
                }
            }
            State::LinkText => {
                if c == '\n' {
                    self.abandon(Some(c));
                } else if c == ']' {
                    self.held.push(c);
                    self.state = State::LinkClose;
                } else {
                    self.held.push(c);
                    if self.held() > MAX_HOLD_BACK {
                        self.abandon(None);
                    }
                }
            }
            State::LinkClose => {
                if c == '(' {
                    let held = std::mem::take(&mut self.held);
                    let inner = &held[1..held.len() - 1];
                    self.state = State::Text;
                    for ch in inner.chars() {
                        self.step(ch);
                    }
                    self.settle();
                    self.state = State::LinkUrl;
                } else {
                    self.abandon(Some(c));
                }
            }
            State::Bracket => {
                self.held.clear();
                self.state = State::Text;
                if c == '(' {
                    self.state = State::LinkUrl;
                } else {
                    self.emit(']');
                    self.step(c);
                }
            }
            State::LinkUrl => {
                if c == ')' {
                    self.state = State::Text;
                } else if c == '\n' {
                    self.state = State::Text;
                    self.step(c);
                }
            }
            State::Underscore => {
                self.held.clear();
                self.state = State::Text;
                if c.is_alphanumeric() {
                    self.emit('_');
                }
                self.step(c);
            }
            State::LineMarker => self.line_marker(c),
            State::Rule => {
                if c != '-' {
                    self.state = State::Text;
                    self.step(c);
                }
            }
            State::TableRow => {
                if c == '\n' {
                    self.held.clear();
                    self.state = State::Text;
                    self.step(c);
                } else if TABLE_ROW_CHARS.contains(c) {
                    self.held.push(c);
                    if self.held() > MAX_HOLD_BACK {
                        self.held.clear();
                        self.state = State::SeparatorRow;
                    }
                } else {
                    let held = std::mem::take(&mut self.held);
                    self.state = State::Text;
                    self.line_start = false;
                    for ch in held.chars() {
                        self.emit(if ch == '|' { ' ' } else { ch });
                    }
                    self.step(c);
                }
            }
            State::SeparatorRow => {
                if c == '\n' {
                    self.state = State::Text;
                    self.step(c);
                }
            }
            State::Text => self.text(c),
        }
    }
    fn line_marker(&mut self, c: char) {
        let held = self.held.as_str();
        let dashes = held.bytes().all(|b| b == b'-');
        let digits = held.bytes().all(|b| b.is_ascii_digit());
        let numbered = held.len() > 1
            && held.ends_with(['.', ')'])
            && held[..held.len() - 1].bytes().all(|b| b.is_ascii_digit());
        if c.is_whitespace() && (held == "-" || held == "+" || numbered) {
            self.held.clear();
            self.state = State::Text;
            self.line_start = false;
            self.step(c);
        } else if c == '-' && dashes {
            self.held.push(c);
            if self.held.len() == 3 {
                self.held.clear();
                self.state = State::Rule;
            }
        } else if digits && ((c.is_ascii_digit() && held.len() < 3) || matches!(c, '.' | ')')) {
            self.held.push(c);
        } else {
            let held = std::mem::take(&mut self.held);
            self.state = State::Text;
            self.line_start = false;
            self.emit_str(&held);
            self.step(c);
        }
    }
    fn text(&mut self, c: char) {
        if c == '`' {
            self.held.push(c);
            self.state = State::Backticks;
            return;
        }
        if c == '\n' {
            self.line_start = true;
            self.emit(c);
            return;
        }
        if c.is_whitespace() {
            self.emit(c);
            return;
        }
        if self.line_start {
            match c {
                '#' | '>' => return,
                '-' | '+' | '0'..='9' => {
                    self.held.push(c);
                    self.state = State::LineMarker;
                    return;
                }
                '|' => {
                    self.held.push(c);
                    self.state = State::TableRow;
                    return;
                }
                _ => self.line_start = false,
            }
        }
        match c {
            '#' | '*' | '~' => {}
            '|' => self.emit(' '),
            '!' => {
                self.held.push(c);
                self.state = State::Bang;
            }
            '[' => {
                self.held.push(c);
                self.state = State::LinkText;
            }
            ']' => {
                self.held.push(c);
                self.state = State::Bracket;
            }
            '_' => {
                if !self.at_token_start() {
                    self.held.push(c);
                    self.state = State::Underscore;
                }
            }
            'h' | 'H' => {
                self.url_at_token_start = self.at_token_start();
                self.held.push(c);
                self.state = State::MaybeUrl;
            }
            'w' | 'W' if self.at_token_start() => {
                self.url_at_token_start = true;
                self.held.push(c);
                self.state = State::MaybeUrl;
            }
            _ => self.emit(c),
        }
    }
}

/// Emoji and other symbols (Unicode So/Sk) plus the invisible glue that only
/// exists to compose them.
fn dropped_symbol(c: char) -> bool {
    matches!(c, '\u{FE0E}' | '\u{FE0F}' | '\u{200D}' | '\u{20E3}')
        || matches!(
            get_general_category(c),
            GeneralCategory::OtherSymbol | GeneralCategory::ModifierSymbol
        )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    /// Sentence punctuation seen; whitespace (after optional closing quotes)
    /// makes it a candidate boundary.
    AwaitWhitespace,
    /// Whitespace is accumulating; the next word decides. `conditional`
    /// boundaries need an uppercase letter or digit, forced ones (newline,
    /// marker) always split.
    Decide { conditional: bool },
}

/// Splits sanitized text into speakable chunks. Chunks are never trimmed, so
/// concatenating them reproduces the input (minus [`SENTENCE_MARKER`]).
#[derive(Debug)]
pub struct Chunker {
    current: String,
    count: usize,
    pending: Pending,
}
impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
}
impl Chunker {
    pub fn new() -> Self {
        Self {
            current: String::new(),
            count: 0,
            pending: Pending::None,
        }
    }
    /// Whole-string chunking: identical to streaming the text in any pieces.
    pub fn chunk(text: &str) -> Vec<String> {
        let mut chunker = Self::new();
        let mut chunks = chunker.push(text);
        chunks.extend(chunker.flush());
        chunks
    }
    pub fn push(&mut self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for c in text.chars() {
            self.step(c, &mut out);
        }
        out
    }
    pub fn flush(&mut self) -> Option<String> {
        self.pending = Pending::None;
        self.take()
    }
    /// Sentence punctuation was seen and whitespace is still outstanding.
    pub fn awaiting_whitespace(&self) -> bool {
        self.pending == Pending::AwaitWhitespace
    }
    /// Close the current chunk now if punctuation and whitespace are already
    /// in, without waiting for the next word. The end of a streamed token is
    /// treated as end of input so a finished sentence reaches TTS at once.
    pub fn flush_sentence(&mut self) -> Option<String> {
        if !matches!(self.pending, Pending::Decide { .. }) {
            return None;
        }
        self.pending = Pending::None;
        self.take()
    }
    fn take(&mut self) -> Option<String> {
        self.count = 0;
        (!self.current.is_empty()).then(|| std::mem::take(&mut self.current))
    }
    fn close(&mut self, out: &mut Vec<String>) {
        out.extend(self.take());
    }
    fn step(&mut self, c: char, out: &mut Vec<String>) {
        match self.pending {
            Pending::Decide { conditional } if !c.is_whitespace() => {
                if !conditional || c.is_uppercase() || c.is_numeric() {
                    self.close(out);
                }
                self.pending = Pending::None;
            }
            Pending::AwaitWhitespace => {
                if c.is_whitespace() {
                    self.pending = Pending::Decide { conditional: true };
                } else if !matches!(c, '"' | '\'' | '”' | '’' | ')') {
                    self.pending = Pending::None;
                }
            }
            _ => {}
        }
        if c == SENTENCE_MARKER {
            self.pending = Pending::Decide { conditional: false };
            return;
        }
        if self.count >= MAX_CHUNK_CHARS {
            if let Some((index, ws)) = self.current.char_indices().rev().find(|(_, c)| c.is_whitespace()) {
                let rest = self.current.split_off(index + ws.len_utf8());
                self.close(out);
                self.count = rest.chars().count();
                self.current = rest;
            } else if c.is_whitespace() {
                // An unbroken token longer than the cap keeps its trailing
                // whitespace so the next chunk starts on a word.
                self.current.push(c);
                self.close(out);
                return;
            }
        }
        self.current.push(c);
        self.count += 1;
        if c == '\n' {
            self.pending = Pending::Decide { conditional: false };
        } else if matches!(c, '!' | '?' | '…' | ';') || (c == '.' && !self.after_abbreviation()) {
            self.pending = Pending::AwaitWhitespace;
        }
    }
    /// The word before the period just pushed is a listed abbreviation
    /// (quotes and brackets before the word are ignored, case is not).
    fn after_abbreviation(&self) -> bool {
        let body = &self.current[..self.current.len() - 1];
        let word = body
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or("")
            .trim_start_matches(|c: char| !c.is_alphanumeric());
        ABBREVIATIONS.iter().any(|a| a.eq_ignore_ascii_case(word))
    }
}

/// Sanitize, then chunk. One instance per spoken turn.
#[derive(Debug, Default)]
pub struct SpeechStream {
    transform: SpeechTextTransform,
    chunker: Chunker,
}
impl SpeechStream {
    pub fn new() -> Self {
        Self::default()
    }
    /// Chunks that are ready after this token. A sentence completed by this
    /// token (punctuation plus whitespace) is closed immediately.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        let clean = self.transform.push(text);
        let mut chunks = self.chunker.push(&clean);
        if self.chunker.awaiting_whitespace() {
            if let Some(ws) = self.transform.take_pending_whitespace() {
                chunks.extend(self.chunker.push(&ws.to_string()));
            }
        }
        chunks.extend(self.chunker.flush_sentence());
        chunks
    }
    pub fn flush(&mut self) -> Vec<String> {
        let clean = self.transform.flush();
        let mut chunks = self.chunker.push(&clean);
        chunks.extend(self.chunker.flush());
        chunks
    }
}
