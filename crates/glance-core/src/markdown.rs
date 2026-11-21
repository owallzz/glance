#![allow(dead_code)]
//! Discord-style message markdown: a parser for chat text, a plain-text
//! rendering for previews and notifications, and live style runs for the
//! composer.
//!
//! The dialect follows Discord's (a `simple-markdown` fork):
//!
//! - The whole message is inline: the leftmost opener wins, delimiters never
//!   cross, and every `\n` is a line break.
//! - `*em*` / `_em_`, `**bold**`, `__underline__`, `~~strike~~`, `||spoiler||`,
//!   `` `code` `` and fenced code blocks (which may start and end mid-line and
//!   are lifted out of the paragraph as a block).
//! - At line start: `#`..`###` headings, `-# ` subtext, `> ` and `>>> `
//!   quotes (no nesting) and lists.
//! - `\` escapes any character that is not an ASCII letter, digit or
//!   whitespace.
//! - Bare `http(s)://` URLs (trailing `.,:;"')]` left out, balanced
//!   parentheses kept), `<url>`, masked links `[label](url "title")` (http and
//!   https only; a label that itself contains a URL makes the whole thing
//!   literal) and `glance://join/...` invites.
//!
//! Our own choices where Discord's behaviour is private or unclear:
//! `>>>` directly followed by a newline also starts a quote; lists are `-`,
//! `*` or `N.` plus a space at line start, a deeper indent nests, numbering
//! counts up from the first number, and any other line ends the list;
//! headings and subtext need some non-space text; a code block inside a
//! heading, subtext or link label is shown as inline code; a spoiler inside a
//! spoiler merges into it; nesting deeper than [`MAX_DEPTH`] stays literal;
//! masked-link labels longer than 1 KiB stay literal.
//!
//! Every matcher is a hand-written scanner with memoised searches, so the
//! parser runs in roughly linear time on any input and never panics.

use std::ops::Range;

/// Nested constructs deeper than this keep their content as literal text.
pub const MAX_DEPTH: u8 = 16;
/// Longest masked-link label we look for, in bytes.
const MAX_LABEL: usize = 1024;
/// What a spoiler turns into in [`plain_text`].
pub const SPOILER_MASK: &str = "▮▮▮▮";

// ---------------------------------------------------------------------------
// Public types

/// Inline style flags for one span of rendered text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Inline code: monospace + code background.
    pub code: bool,
    /// Id of the enclosing spoiler (unique within one message, from 0).
    pub spoiler: Option<u32>,
    /// Index into [`Message::links`].
    pub link: Option<u32>,
}

/// A styled byte range of [`RichLine::text`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Web,
    /// A `glance://join/...` ticket, opened in-app.
    GlanceInvite,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub kind: LinkKind,
    pub masked: bool,
    pub title: Option<String>,
}

/// One run of rendered text. A paragraph may contain '\n' (line breaks are
/// kept in the text). Spans cover the text completely, in order, and never
/// overlap. Adjacent spans with equal style are merged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RichLine {
    pub text: String,
    pub spans: Vec<Span>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(RichLine),
    /// Level 1..=3.
    Heading {
        level: u8,
        line: RichLine,
    },
    Subtext(RichLine),
    Quote(Vec<Block>),
    CodeBlock {
        lang: Option<String>,
        code: String,
        spoiler: Option<u32>,
    },
    /// `start` is `Some(n)` for an ordered list.
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    /// A spoiler that contains block content (for example ||```code```||):
    /// the whole group hides together.
    Spoiler {
        id: u32,
        blocks: Vec<Block>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub blocks: Vec<Block>,
    /// [`Style::link`] indexes into this (message-wide).
    pub links: Vec<Link>,
    /// Number of spoiler ids used.
    pub spoilers: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ComposerStyle {
    /// Syntax characters: ** __ ~~ || ` ``` # -# > >>> - 1. [ ]( ) <> and \
    /// escapes; drawn muted.
    pub marker: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Inline code or code-block content (code background fill).
    pub code: bool,
    /// Spoiler background fill.
    pub spoiler: bool,
    /// Link colour.
    pub link: bool,
    /// 0, or 1..=3: bold (the composer can't change font size).
    pub heading: u8,
    /// Muted.
    pub subtext: bool,
    /// Text inside a quote.
    pub quote: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerSpan {
    pub range: Range<usize>,
    pub style: ComposerStyle,
}

// ---------------------------------------------------------------------------
// Entry points

/// Parse a message. Leading and trailing whitespace is ignored.
pub fn parse(src: &str) -> Message {
    let nodes = parse_nodes(src);
    let mut lw = Lowerer::default();
    let blocks = lw.blocks(&nodes, Style::default());
    Message {
        blocks,
        links: lw.links,
        spoilers: lw.spoilers,
    }
}

/// Plain text for notifications and one-line previews: delimiters removed,
/// line breaks kept. Spoiler content is replaced by "▮▮▮▮" (one per spoiler,
/// not per character); masked links become their label. Code-block content
/// is kept, without the fences or the language line.
pub fn plain_text(src: &str) -> String {
    let msg = parse(src);
    let mut out = String::new();
    plain_blocks(&msg.blocks, &mut out);
    out
}

/// Live formatting for the composer: style runs over the RAW source (byte
/// ranges into `src`). They are ordered and non-overlapping but need not
/// cover everything; uncovered text is plain.
pub fn composer_spans(src: &str) -> Vec<ComposerSpan> {
    let nodes = parse_nodes(src);
    let mut out = Vec::new();
    composer_walk(&nodes, ComposerStyle::default(), &mut out);
    finish_composer(out)
}

fn parse_nodes(src: &str) -> Vec<Node> {
    let start = src.len() - src.trim_start().len();
    let text = src.trim();
    let ctx = Ctx {
        depth: 0,
        start_of_line: true,
        in_quote: false,
        in_link: false,
        inline_only: false,
    };
    Parser::new(text, SegMap::at(start), ctx).run()
}

// ---------------------------------------------------------------------------
// Source mapping: nested parses run on "virtual" strings (a slice of the
// parent, or quote lines with their `> ` removed); every node records the
// original byte ranges it came from, for the composer.

type Raw = Vec<Range<usize>>;

#[derive(Clone, Debug)]
struct SegMap {
    /// (virtual start, original start), sorted by virtual start, first at 0.
    segs: Vec<(usize, usize)>,
}

impl SegMap {
    fn at(orig: usize) -> Self {
        SegMap {
            segs: vec![(0, orig)],
        }
    }

    fn index(&self, v: usize) -> usize {
        self.segs
            .partition_point(|&(s, _)| s <= v)
            .saturating_sub(1)
    }

    fn map_into(&self, r: Range<usize>, out: &mut Raw) {
        if r.start >= r.end {
            return;
        }
        let mut k = self.index(r.start);
        let mut v = r.start;
        while v < r.end {
            let (vs, os) = self.segs[k];
            let seg_end = self.segs.get(k + 1).map_or(usize::MAX, |s| s.0);
            let e = r.end.min(seg_end);
            let o = os + (v - vs);
            let range = o..o + (e - v);
            match out.last_mut() {
                Some(last) if last.end == range.start => last.end = range.end,
                _ => out.push(range),
            }
            v = e;
            k += 1;
        }
    }

    fn map(&self, r: Range<usize>) -> Raw {
        let mut out = Vec::new();
        self.map_into(r, &mut out);
        out
    }

    /// The map of a child string made of these parent pieces, in order.
    fn sub(&self, pieces: &[Range<usize>]) -> SegMap {
        let mut segs: Vec<(usize, usize)> = Vec::new();
        let mut cv = 0;
        for p in pieces {
            for r in self.map(p.clone()) {
                match segs.last() {
                    Some(&(lv, lo)) if lo + (cv - lv) == r.start => {}
                    _ => segs.push((cv, r.start)),
                }
                cv += r.len();
            }
        }
        if segs.is_empty() {
            let o = pieces.first().map_or(0, |p| {
                let k = self.index(p.start);
                self.segs[k].1 + (p.start - self.segs[k].0)
            });
            segs.push((0, o));
        }
        SegMap { segs }
    }
}

// ---------------------------------------------------------------------------
// Syntax tree

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Emph {
    Bold,
    Italic,
    Underline,
    Strike,
}

#[derive(Debug)]
enum Node {
    Text {
        text: String,
        raw: Raw,
    },
    Escape {
        text: String,
        marker: Raw,
        raw: Raw,
    },
    Br,
    Emph {
        kind: Emph,
        markers: Raw,
        children: Vec<Node>,
    },
    Spoiler {
        markers: Raw,
        children: Vec<Node>,
    },
    Code {
        code: String,
        markers: Raw,
        raw: Raw,
    },
    CodeBlock {
        lang: Option<String>,
        code: String,
        markers: Raw,
        raw: Raw,
    },
    Link {
        link: Link,
        markers: Raw,
        url_raw: Raw,
        children: Vec<Node>,
    },
    Heading {
        level: u8,
        markers: Raw,
        children: Vec<Node>,
    },
    Subtext {
        markers: Raw,
        children: Vec<Node>,
    },
    Quote {
        markers: Raw,
        children: Vec<Node>,
    },
    List {
        start: Option<u64>,
        items: Vec<(Raw, Vec<Node>)>,
    },
}

// ---------------------------------------------------------------------------
// Parser

#[derive(Clone, Copy, Debug)]
struct Ctx {
    depth: u8,
    /// Whether position 0 of this source counts as the start of a line.
    start_of_line: bool,
    in_quote: bool,
    in_link: bool,
    /// Heading, subtext and link-label content: no line-start rules and no
    /// code blocks.
    inline_only: bool,
}

/// "First position >= from where a fixed predicate holds", remembered so
/// that the left-to-right scan asks each question in amortised O(1).
#[derive(Default)]
struct Cache {
    from: usize,
    ans: Option<usize>,
    set: bool,
}

impl Cache {
    fn find(&mut self, from: usize, end: usize, pred: impl Fn(usize) -> bool) -> Option<usize> {
        if self.set && from >= self.from && self.ans.is_none_or(|a| from <= a) {
            return self.ans;
        }
        let ans = (from..end).find(|&j| pred(j));
        self.from = from;
        self.ans = ans;
        self.set = true;
        ans
    }
}

#[derive(Default)]
struct Caches {
    strong: Cache,
    under: Cache,
    strike: Cache,
    spoiler: Cache,
    fence: Cache,
    rbr: Cache,
    lbr: Cache,
    rpar: Cache,
    title: Cache,
    angle_scheme: Cache,
    angle_rest: Cache,
    url: Cache,
    not_star: Cache,
    not_us: Cache,
}

/// Maximal backtick runs (start, len), and the minimum length of the runs
/// from each index on.
type Runs = (Vec<(usize, usize)>, Vec<usize>);

const UNKNOWN: usize = usize::MAX;
const FAIL: usize = usize::MAX - 1;

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    map: SegMap,
    ctx: Ctx,
    /// esc[p]: the character at p is escaped by the backslash before it.
    esc: Vec<bool>,
    em_star: Vec<usize>,
    em_us: Vec<usize>,
    href_dead: Vec<bool>,
    /// Built on the first inline-code attempt.
    runs: Option<Runs>,
    c: Caches,
}

fn is_lang_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'+' | b'-' | b'.' | b'#')
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn ws_at(s: &str, j: usize) -> bool {
    let b = s.as_bytes()[j];
    if b < 0x80 {
        matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
    } else {
        s.is_char_boundary(j) && s[j..].chars().next().is_some_and(char::is_whitespace)
    }
}

impl<'a> Parser<'a> {
    fn new(s: &'a str, map: SegMap, ctx: Ctx) -> Self {
        Parser {
            s,
            b: s.as_bytes(),
            map,
            ctx,
            esc: Vec::new(),
            em_star: Vec::new(),
            em_us: Vec::new(),
            href_dead: Vec::new(),
            runs: None,
            c: Caches::default(),
        }
    }

    fn n(&self) -> usize {
        self.b.len()
    }

    fn at(&self, p: usize) -> Option<u8> {
        self.b.get(p).copied()
    }

    fn clen(&self, p: usize) -> usize {
        match self.b[p] {
            0x00..=0x7f => 1,
            0xf0..=0xff => 4,
            0xe0..=0xef => 3,
            _ => 2,
        }
    }

    fn ws(&self, p: usize) -> bool {
        p < self.n() && ws_at(self.s, p)
    }

    fn skip_ws(&self, mut p: usize) -> usize {
        while self.ws(p) {
            p += self.clen(p);
        }
        p
    }

    fn skip_spaces(&self, mut p: usize) -> usize {
        while self.at(p) == Some(b' ') {
            p += 1;
        }
        p
    }

    fn line_end(&self, p: usize) -> usize {
        self.b[p..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(self.n(), |k| p + k)
    }

    fn line_start(&self, i: usize) -> bool {
        if i == 0 {
            self.ctx.start_of_line
        } else {
            self.b[i - 1] == b'\n'
        }
    }

    fn raw(&self, r: Range<usize>) -> Raw {
        self.map.map(r)
    }

    fn raws(&self, rs: &[Range<usize>]) -> Raw {
        let mut out = Vec::new();
        for r in rs {
            self.map.map_into(r.clone(), &mut out);
        }
        out
    }

    fn text(&self, r: Range<usize>) -> Node {
        Node::Text {
            text: self.s[r.clone()].to_string(),
            raw: self.raw(r),
        }
    }

    fn inner(&self, i: usize) -> Ctx {
        Ctx {
            depth: self.ctx.depth.saturating_add(1),
            start_of_line: self.line_start(i),
            ..self.ctx
        }
    }

    fn child1(&self, range: Range<usize>, ctx: Ctx) -> Vec<Node> {
        self.child(std::slice::from_ref(&range), ctx)
    }

    fn child(&self, pieces: &[Range<usize>], ctx: Ctx) -> Vec<Node> {
        let map = self.map.sub(pieces);
        if let [r] = pieces {
            Parser::new(&self.s[r.clone()], map, ctx).run()
        } else {
            let text: String = pieces.iter().map(|r| &self.s[r.clone()]).collect();
            Parser::new(&text, map, ctx).run()
        }
    }

    fn ensure_esc(&mut self) {
        if !self.esc.is_empty() || self.b.is_empty() {
            return;
        }
        let mut esc = vec![false; self.n() + 1];
        let mut p = 0;
        while p < self.n() {
            if self.b[p] == b'\\' && !esc[p] && p + 1 < self.n() {
                esc[p + 1] = true;
            }
            p += self.clen(p);
        }
        self.esc = esc;
    }

    fn run(&mut self) -> Vec<Node> {
        let n = self.n();
        if n == 0 {
            return Vec::new();
        }
        if self.ctx.depth > MAX_DEPTH {
            return vec![self.text(0..n)];
        }
        let mut out = Vec::new();
        let mut text_start: Option<usize> = None;
        let mut i = 0;
        while i < n {
            if let Some((node, end)) = self.rule(i) {
                if let Some(t) = text_start.take() {
                    out.push(self.text(t..i));
                }
                out.push(node);
                debug_assert!(end > i);
                i = end.max(i + 1);
            } else {
                text_start.get_or_insert(i);
                i += self.clen(i);
            }
        }
        if let Some(t) = text_start {
            out.push(self.text(t..n));
        }
        out
    }

    fn rule(&mut self, i: usize) -> Option<(Node, usize)> {
        let c = self.b[i];
        let block_ok = !self.ctx.inline_only && self.line_start(i);
        if c == b'|'
            && let Some(r) = self.spoiler(i)
        {
            return Some(r);
        }
        if block_ok
            && c == b'-'
            && let Some(r) = self.subtext(i)
        {
            return Some(r);
        }
        if block_ok
            && c == b'#'
            && let Some(r) = self.heading(i)
        {
            return Some(r);
        }
        if c == b'`'
            && !self.ctx.inline_only
            && let Some(r) = self.code_block(i)
        {
            return Some(r);
        }
        if block_ok
            && !self.ctx.in_quote
            && matches!(c, b' ' | b'>')
            && let Some(r) = self.quote(i)
        {
            return Some(r);
        }
        if block_ok
            && (matches!(c, b' ' | b'-' | b'*') || c.is_ascii_digit())
            && let Some(r) = self.list(i)
        {
            return Some(r);
        }
        if c == b'\n' {
            return Some((Node::Br, i + 1));
        }
        if c == b'\\'
            && let Some(r) = self.escape(i)
        {
            return Some(r);
        }
        if !self.ctx.in_link {
            let r = match c {
                b'<' => self.angle_link(i),
                b'h' => self.url(i),
                b'g' => self.invite(i),
                b'[' => self.masked_link(i),
                _ => None,
            };
            if r.is_some() {
                return r;
            }
        }
        match c {
            b'*' | b'_' => self.emphasis(i),
            b'~' => self.strike(i),
            b'`' => self.inline_code(i),
            _ => None,
        }
    }

    // -- spoiler: ^\|\|([\s\S]+?)\|\|

    fn spoiler(&mut self, i: usize) -> Option<(Node, usize)> {
        if self.at(i + 1) != Some(b'|') || i + 2 >= self.n() {
            return None;
        }
        let from = i + 2 + self.clen(i + 2);
        let b = self.b;
        let j = self.c.spoiler.find(from, b.len(), |j| {
            b[j] == b'|' && b.get(j + 1) == Some(&b'|')
        })?;
        let node = Node::Spoiler {
            markers: self.raws(&[i..i + 2, j..j + 2]),
            children: self.child1(i + 2..j, self.inner(i)),
        };
        Some((node, j + 2))
    }

    // -- heading ^(#{1,3}) +([^\n]+?)(\n|$) and subtext ^-# +([^\n]+?)(\n|$)

    /// After the `#`s: one or more spaces, then the rest of the line, which
    /// must hold some non-space text. Returns (trimmed content, end).
    fn line_content(&self, p: usize) -> Option<(Range<usize>, usize)> {
        if self.at(p) != Some(b' ') {
            return None;
        }
        let le = self.line_end(p);
        let line = &self.s[p..le];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let start = p + (line.len() - line.trim_start().len());
        let end = if le < self.n() { le + 1 } else { le };
        Some((start..start + trimmed.len(), end))
    }

    fn heading(&mut self, i: usize) -> Option<(Node, usize)> {
        let hashes = self.b[i..].iter().take_while(|&&c| c == b'#').count();
        if hashes > 3 {
            return None;
        }
        let (content, end) = self.line_content(i + hashes)?;
        let node = Node::Heading {
            level: hashes as u8,
            markers: self.raw(i..content.start),
            children: self.child1(content, self.line_ctx()),
        };
        Some((node, end))
    }

    fn subtext(&mut self, i: usize) -> Option<(Node, usize)> {
        if self.at(i + 1) != Some(b'#') {
            return None;
        }
        let (content, end) = self.line_content(i + 2)?;
        let node = Node::Subtext {
            markers: self.raw(i..content.start),
            children: self.child1(content, self.line_ctx()),
        };
        Some((node, end))
    }

    fn line_ctx(&self) -> Ctx {
        Ctx {
            depth: self.ctx.depth.saturating_add(1),
            start_of_line: false,
            inline_only: true,
            ..self.ctx
        }
    }

    // -- code block: ^```(([a-z0-9_+\-.#]+?)\n+)?\n*([^]+?)\n*```  (case-insensitive)

    fn fence_from(&mut self, from: usize) -> Option<usize> {
        let b = self.b;
        self.c
            .fence
            .find(from, b.len(), |j| b[j..].starts_with(b"```"))
    }

    fn count_newlines(&self, p: usize) -> usize {
        self.b[p.min(self.n())..]
            .iter()
            .take_while(|&&c| c == b'\n')
            .count()
    }

    /// Content starting at `c` (at least one character), ending before the
    /// first fence after it, without the newlines before that fence.
    fn code_content(&mut self, c: usize) -> Option<(Range<usize>, usize)> {
        if c >= self.n() {
            return None;
        }
        let min_end = c + self.clen(c);
        let f = self.fence_from(min_end)?;
        let mut e = f;
        while e > min_end && self.b[e - 1] == b'\n' {
            e -= 1;
        }
        Some((c..e, f))
    }

    fn code_block(&mut self, i: usize) -> Option<(Node, usize)> {
        if !self.b[i..].starts_with(b"```") {
            return None;
        }
        let bs = i + 3;
        let mut found: Option<(Option<Range<usize>>, Range<usize>, usize)> = None;
        // With a language line.
        let mut p = bs;
        while p < self.n() && is_lang_byte(self.b[p]) {
            p += 1;
        }
        if p > bs && self.at(p) == Some(b'\n') {
            let r = self.count_newlines(p);
            if let Some((content, f)) = self.code_content(p + r) {
                found = Some((Some(bs..p), content, f));
            } else if r >= 2 && self.b[p + r..].starts_with(b"```") {
                found = Some((Some(bs..p), p + r - 1..p + r, p + r));
            }
        }
        // Without.
        if found.is_none() {
            let q = bs + self.count_newlines(bs);
            if let Some((content, f)) = self.code_content(q) {
                found = Some((None, content, f));
            } else if q > bs && self.b[q..].starts_with(b"```") {
                found = Some((None, q - 1..q, q));
            }
        }
        let (lang, content, f) = found?;
        let open_end = lang.as_ref().map_or(bs, |l| l.end + 1);
        let node = Node::CodeBlock {
            lang: lang.map(|l| self.s[l].to_ascii_lowercase()),
            code: self.s[content.clone()].to_string(),
            markers: self.raws(&[i..open_end, f..f + 3]),
            raw: self.raw(content),
        };
        Some((node, f + 3))
    }

    // -- block quote: ^( *>>> ([\s\S]*))|^( *> [^\n]*(\n *> [^\n]*)*\n?)

    fn quote(&mut self, i: usize) -> Option<(Node, usize)> {
        let n = self.n();
        let p = self.skip_spaces(i);
        let ctx = Ctx {
            depth: self.ctx.depth.saturating_add(1),
            start_of_line: true,
            in_quote: true,
            ..self.ctx
        };
        if self.b[p..].starts_with(b">>>") && matches!(self.at(p + 3), Some(b' ' | b'\n')) {
            let node = Node::Quote {
                markers: self.raw(i..p + 3 + usize::from(self.b[p + 3] == b' ')),
                children: self.child1(p + 4..n, ctx),
            };
            return Some((node, n));
        }
        let mut pieces: Vec<Range<usize>> = Vec::new();
        let mut markers: Vec<Range<usize>> = Vec::new();
        let mut ls = i;
        let end = loop {
            let q = self.skip_spaces(ls);
            if !(self.at(q) == Some(b'>') && self.at(q + 1) == Some(b' ')) {
                // Only reachable on the first line: continuations are checked
                // before moving on.
                return None;
            }
            markers.push(ls..q + 2);
            let le = self.line_end(q + 2);
            pieces.push(q + 2..le);
            if le >= n {
                break n;
            }
            let next = le + 1;
            let nq = self.skip_spaces(next);
            if self.at(nq) == Some(b'>') && self.at(nq + 1) == Some(b' ') {
                pieces.last_mut().unwrap().end = le + 1;
                ls = next;
            } else {
                break le + 1;
            }
        };
        let node = Node::Quote {
            markers: self.raws(&markers),
            children: self.child(&pieces, ctx),
        };
        Some((node, end))
    }

    // -- lists (our rules): ( *)([-*]|\d{1,9}\.) +text at line start

    fn bullet(&self, ls: usize) -> Option<Bullet> {
        let p = self.skip_spaces(ls);
        let c = self.at(p)?;
        let (q, ordered) = if c == b'-' || c == b'*' {
            (p + 1, None)
        } else if c.is_ascii_digit() {
            let digits = self.b[p..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count();
            if digits > 9 || self.at(p + digits) != Some(b'.') {
                return None;
            }
            let num: u64 = self.s[p..p + digits].parse().ok()?;
            (p + digits + 1, Some(num))
        } else {
            return None;
        };
        if self.at(q) != Some(b' ') {
            return None;
        }
        let cs = self.skip_spaces(q);
        let le = self.line_end(cs);
        if self.s[cs..le].trim().is_empty() {
            return None;
        }
        Some(Bullet {
            indent: p - ls,
            ordered,
            marker: p..cs,
            content: cs..le,
            next: if le < self.n() { le + 1 } else { le },
        })
    }

    fn list(&mut self, i: usize) -> Option<(Node, usize)> {
        let first = self.bullet(i)?;
        let base = first.indent;
        let ordered = first.ordered.is_some();
        let item_ctx = Ctx {
            depth: self.ctx.depth.saturating_add(1),
            start_of_line: true,
            ..self.ctx
        };
        let mut items = Vec::new();
        let mut ls = i;
        let n = self.n();
        while ls < n {
            let Some(bl) = self.bullet(ls) else { break };
            if bl.indent > base || bl.ordered.is_some() != ordered {
                break;
            }
            let mut children = self.child1(bl.content.clone(), item_ctx);
            let sub_start = bl.next;
            let mut next = sub_start;
            while next < n {
                match self.bullet(next) {
                    Some(c) if c.indent > base => next = c.next,
                    _ => break,
                }
            }
            if next > sub_start {
                children.extend(self.child1(sub_start..next, item_ctx));
            }
            items.push((self.raw(bl.marker), children));
            ls = next;
        }
        Some((
            Node::List {
                start: first.ordered,
                items,
            },
            ls,
        ))
    }

    // -- escape: ^\\([^0-9A-Za-z\s])

    fn escape(&mut self, i: usize) -> Option<(Node, usize)> {
        let p = i + 1;
        if p >= self.n() {
            return None;
        }
        let c = self.b[p];
        if c.is_ascii_alphanumeric() || self.ws(p) {
            return None;
        }
        let end = p + self.clen(p);
        let node = Node::Escape {
            text: self.s[p..end].to_string(),
            marker: self.raw(i..p),
            raw: self.raw(p..end),
        };
        Some((node, end))
    }

    // -- links

    fn link_node(&self, url: String, kind: LinkKind, r: Range<usize>, markers: Raw) -> Node {
        Node::Link {
            link: Link {
                url,
                kind,
                masked: false,
                title: None,
            },
            markers,
            url_raw: Vec::new(),
            children: vec![self.text(r)],
        }
    }

    /// `<url>`: ^<([^: >]+:\/[^ >]+)>, kept only for http(s) and invites.
    fn angle_link(&mut self, i: usize) -> Option<(Node, usize)> {
        let (s, n) = (self.s, self.n());
        let colon = self.c.angle_scheme.find(i + 1, n, |j| {
            matches!(s.as_bytes()[j], b':' | b'>' | b' ') || ws_at(s, j)
        })?;
        if colon == i + 1 || self.b[colon] != b':' || self.at(colon + 1) != Some(b'/') {
            return None;
        }
        let close = self.c.angle_rest.find(colon + 2, n, |j| {
            matches!(s.as_bytes()[j], b'>' | b' ') || ws_at(s, j)
        })?;
        if close == colon + 2 || self.b[close] != b'>' {
            return None;
        }
        let url = &self.s[i + 1..close];
        let kind = if is_http(url) {
            LinkKind::Web
        } else if is_invite(url) {
            LinkKind::GlanceInvite
        } else {
            return None;
        };
        let node = self.link_node(
            url.to_string(),
            kind,
            i + 1..close,
            self.raws(&[i..i + 1, close..close + 1]),
        );
        Some((node, close + 1))
    }

    /// Bare URL: ^(https?:\/\/[^\s<]+[^<.,:;"')\]\s]), keeping a trailing `)`
    /// when the parentheses balance.
    fn url(&mut self, i: usize) -> Option<(Node, usize)> {
        let rest = &self.b[i..];
        let p = if rest.starts_with(b"https://") {
            i + 8
        } else if rest.starts_with(b"http://") {
            i + 7
        } else {
            return None;
        };
        let (s, n) = (self.s, self.n());
        let run_end = self
            .c
            .url
            .find(p, n, |j| s.as_bytes()[j] == b'<' || ws_at(s, j))
            .unwrap_or(n);
        let (mut open, mut close) = (0usize, 0usize);
        for &c in &self.b[i..run_end] {
            match c {
                b'(' => open += 1,
                b')' => close += 1,
                _ => {}
            }
        }
        let mut e = run_end;
        while e > p {
            match self.b[e - 1] {
                b'.' | b',' | b':' | b';' | b'"' | b'\'' | b']' => e -= 1,
                b')' if open < close => {
                    close -= 1;
                    e -= 1;
                }
                _ => break,
            }
        }
        if self.s[p..e].chars().count() < 2 {
            return None;
        }
        let node = self.link_node(self.s[i..e].to_string(), LinkKind::Web, i..e, Vec::new());
        Some((node, e))
    }

    /// `glance://join/<ticket>`.
    fn invite(&mut self, i: usize) -> Option<(Node, usize)> {
        const PREFIX: &[u8] = b"glance://join/";
        if !self.b[i..].starts_with(PREFIX) {
            return None;
        }
        let p = i + PREFIX.len();
        let len = self.b[p..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        if len == 0 {
            return None;
        }
        let e = p + len;
        let node = self.link_node(
            self.s[i..e].to_string(),
            LinkKind::GlanceInvite,
            i..e,
            Vec::new(),
        );
        Some((node, e))
    }

    fn next_byte(cache: &mut Cache, b: &[u8], from: usize, c: u8) -> Option<usize> {
        cache.find(from, b.len(), |j| b[j] == c)
    }

    /// Masked link: ^\[(INSIDE)\]\( \s*<?(href)>?(?:\s+['"](title)['"])?\s* \)
    /// with INSIDE = (?:\[[^\]]*\]|[^\[\]]|\](?=[^\[]*\]))*
    fn masked_link(&mut self, i: usize) -> Option<(Node, usize)> {
        let n = self.n();
        let b = self.b;
        let limit = (i + 1 + MAX_LABEL).min(n);
        let mut cands = Vec::new();
        let mut p = i + 1;
        while p < limit {
            match b[p] {
                b'[' => match Self::next_byte(&mut self.c.rbr, b, p + 1, b']') {
                    Some(k) => p = k + 1,
                    None => break,
                },
                b']' => {
                    if b.get(p + 1) == Some(&b'(') {
                        cands.push(p);
                    }
                    let nb = Self::next_byte(&mut self.c.rbr, b, p + 1, b']');
                    let no = Self::next_byte(&mut self.c.lbr, b, p + 1, b'[');
                    match nb {
                        Some(k) if no.is_none_or(|o| k < o) => p += 1,
                        _ => break,
                    }
                }
                _ => p += self.clen(p),
            }
        }
        for &close in cands.iter().rev() {
            let Some(h) = self.href(close + 2) else {
                continue;
            };
            let label = i + 1..close;
            let url = unescape_url(&self.s[h.url.clone()]);
            let label_text = &self.s[label.clone()];
            let literal = !is_http(&url) || label_text.trim().is_empty() || has_url(label_text);
            if literal {
                return Some((self.text(i..h.end), h.end));
            }
            let ctx = Ctx {
                depth: self.ctx.depth.saturating_add(1),
                start_of_line: false,
                in_link: true,
                inline_only: true,
                ..self.ctx
            };
            let node = Node::Link {
                link: Link {
                    url,
                    kind: LinkKind::Web,
                    masked: true,
                    title: h.title.map(|t| self.s[t].to_string()),
                },
                markers: self.raws(&[i..i + 1, close..h.url.start, h.url.end..h.end]),
                url_raw: self.raw(h.url.clone()),
                children: self.child1(label, ctx),
            };
            return Some((node, h.end));
        }
        None
    }

    fn href(&mut self, p: usize) -> Option<Href> {
        let q = self.skip_ws(p);
        if self.at(q) == Some(b'<')
            && let Some(h) = self.href_scan(q + 1)
        {
            return Some(h);
        }
        self.href_scan(q)
    }

    /// Non-greedy `(?:\([^)]*\)|[^\s\\]|\\.)*?` followed by the rest.
    fn href_scan(&mut self, h0: usize) -> Option<Href> {
        let n = self.n();
        if self.href_dead.is_empty() {
            self.href_dead = vec![false; n + 1];
        }
        let mut visited = Vec::new();
        let mut h = h0;
        loop {
            if h > n || self.href_dead[h] {
                break;
            }
            if let Some((title, end)) = self.href_rest(h) {
                return Some(Href {
                    url: h0..h,
                    title,
                    end,
                });
            }
            visited.push(h);
            if h >= n {
                break;
            }
            match self.b[h] {
                b'(' => {
                    let b = self.b;
                    match Self::next_byte(&mut self.c.rpar, b, h + 1, b')') {
                        Some(k) => h = k + 1,
                        None => h += 1,
                    }
                }
                b'\\' => {
                    if h + 1 < n && !matches!(self.b[h + 1], b'\n' | b'\r') {
                        h += 1 + self.clen(h + 1);
                    } else {
                        break;
                    }
                }
                _ if self.ws(h) => break,
                _ => h += self.clen(h),
            }
        }
        for v in visited {
            self.href_dead[v] = true;
        }
        None
    }

    /// `>?(?:\s+['"]([\s\S]*?)['"])?\s*\)` at h.
    fn href_rest(&mut self, h: usize) -> Option<(Option<Range<usize>>, usize)> {
        let gs = [h + 1, h];
        let skip = usize::from(self.at(h) != Some(b'>'));
        for &g in &gs[skip..] {
            let w = self.skip_ws(g);
            if w > g && matches!(self.at(w), Some(b'"' | b'\'')) {
                let (s, n) = (self.s, self.n());
                let close = self.c.title.find(w + 1, n, |k| {
                    let b = s.as_bytes();
                    if !matches!(b[k], b'"' | b'\'') {
                        return false;
                    }
                    let mut e = k + 1;
                    while e < n && ws_at(s, e) {
                        e += 1;
                    }
                    b.get(e) == Some(&b')')
                });
                if let Some(k) = close {
                    let e = self.skip_ws(k + 1);
                    return Some((Some(w + 1..k), e + 1));
                }
            }
            if self.at(w) == Some(b')') {
                return Some((None, w + 1));
            }
        }
        None
    }

    // -- em / strong / underline, ranked by match length (em, then strong,
    // then underline on ties)

    fn emphasis(&mut self, i: usize) -> Option<(Node, usize)> {
        let (em, double, kind) = if self.b[i] == b'*' {
            (self.em_star(i), self.strong(i), Emph::Bold)
        } else {
            (self.em_us(i), self.under(i), Emph::Underline)
        };
        // `****` and the like: content made only of the delimiter stays
        // literal.
        let delim = self.b[i];
        let em = em.filter(|&e| self.has_other(i + 1, e - 1, delim));
        let double = double.filter(|&e| self.has_other(i + 2, e - 2, delim));
        let (kind, d, end) = match (em, double) {
            (Some(e), Some(s)) if s > e => (kind, 2, s),
            (Some(e), _) => (Emph::Italic, 1, e),
            (None, Some(s)) => (kind, 2, s),
            (None, None) => return None,
        };
        let node = Node::Emph {
            kind,
            markers: self.raws(&[i..i + d, end - d..end]),
            children: self.child1(i + d..end - d, self.inner(i)),
        };
        Some((node, end))
    }

    /// Whether `from..to` holds a byte other than `delim`.
    fn has_other(&mut self, from: usize, to: usize, delim: u8) -> bool {
        let b = self.b;
        let cache = if delim == b'*' {
            &mut self.c.not_star
        } else {
            &mut self.c.not_us
        };
        cache
            .find(from, b.len(), |j| b[j] != delim)
            .is_some_and(|j| j < to)
    }

    /// One content unit of `*em*` at p:
    /// \*\* | \\[\s\S] | \s+(?:\\[\s\S]|[^\s\*\\]|\*\*) | [^\s\*\\]
    fn star_unit(&self, p: usize) -> Option<usize> {
        let n = self.n();
        if p >= n {
            return None;
        }
        let q = if self.ws(p) { self.skip_ws(p) } else { p };
        if q >= n {
            return None;
        }
        match self.b[q] {
            b'*' => (self.at(q + 1) == Some(b'*')).then_some(q + 2),
            b'\\' => (q + 1 < n).then(|| q + 1 + self.clen(q + 1)),
            _ => Some(q + self.clen(q)),
        }
    }

    /// One content unit of `_em_` at p: __ | \\[\s\S] | [^\\_]
    fn us_unit(&self, p: usize) -> Option<usize> {
        let n = self.n();
        if p >= n {
            return None;
        }
        match self.b[p] {
            b'_' => (self.at(p + 1) == Some(b'_')).then_some(p + 2),
            b'\\' => (p + 1 < n).then(|| p + 1 + self.clen(p + 1)),
            _ => Some(p + self.clen(p)),
        }
    }

    /// ^\*(?=\S)(unit+?)\*(?!\*)
    fn em_star(&mut self, i: usize) -> Option<usize> {
        let p = i + 1;
        if p >= self.n() || self.ws(p) {
            return None;
        }
        let p = self.star_unit(p)?;
        if self.em_star.is_empty() {
            self.em_star = vec![UNKNOWN; self.n() + 1];
        }
        let mut visited = Vec::new();
        let mut p = p;
        let res = loop {
            match self.em_star[p] {
                UNKNOWN => {}
                FAIL => break None,
                e => break Some(e),
            }
            visited.push(p);
            if self.at(p) == Some(b'*') && self.at(p + 1) != Some(b'*') {
                break Some(p + 1);
            }
            match self.star_unit(p) {
                Some(q) => p = q,
                None => break None,
            }
        };
        let v = res.unwrap_or(FAIL);
        for k in visited {
            self.em_star[k] = v;
        }
        res
    }

    /// ^\b_(unit+?)_\b — only the closing boundary matters.
    fn em_us(&mut self, i: usize) -> Option<usize> {
        let p = self.us_unit(i + 1)?;
        if self.em_us.is_empty() {
            self.em_us = vec![UNKNOWN; self.n() + 1];
        }
        let mut visited = Vec::new();
        let mut p = p;
        let res = loop {
            match self.em_us[p] {
                UNKNOWN => {}
                FAIL => break None,
                e => break Some(e),
            }
            visited.push(p);
            if self.at(p) == Some(b'_') && !self.at(p + 1).is_some_and(is_word_byte) {
                break Some(p + 1);
            }
            match self.us_unit(p) {
                Some(q) => p = q,
                None => break None,
            }
        };
        let v = res.unwrap_or(FAIL);
        for k in visited {
            self.em_us[k] = v;
        }
        res
    }

    /// `**x**` / `__x__`: content (?:\\[\s\S]|[^\\])+? then the doubled
    /// delimiter not followed by a third.
    fn double(&mut self, i: usize, d: u8) -> Option<usize> {
        if self.at(i + 1) != Some(d) {
            return None;
        }
        let cs = i + 2;
        if cs >= self.n() {
            return None;
        }
        let from = if self.b[cs] == b'\\' {
            if cs + 1 >= self.n() {
                return None;
            }
            cs + 1 + self.clen(cs + 1)
        } else {
            cs + self.clen(cs)
        };
        self.ensure_esc();
        let (b, esc) = (self.b, &self.esc);
        let cache = if d == b'*' {
            &mut self.c.strong
        } else {
            &mut self.c.under
        };
        let j = cache.find(from, b.len(), |j| {
            b[j] == d && b.get(j + 1) == Some(&d) && b.get(j + 2) != Some(&d) && !esc[j]
        })?;
        Some(j + 2)
    }

    fn strong(&mut self, i: usize) -> Option<usize> {
        self.double(i, b'*')
    }

    fn under(&mut self, i: usize) -> Option<usize> {
        self.double(i, b'_')
    }

    // -- strikethrough: ^~~([\s\S]+?)~~(?!_)

    fn strike(&mut self, i: usize) -> Option<(Node, usize)> {
        if self.at(i + 1) != Some(b'~') || i + 2 >= self.n() {
            return None;
        }
        let from = i + 2 + self.clen(i + 2);
        let b = self.b;
        let j = self.c.strike.find(from, b.len(), |j| {
            b[j] == b'~' && b.get(j + 1) == Some(&b'~') && b.get(j + 2) != Some(&b'_')
        })?;
        let node = Node::Emph {
            kind: Emph::Strike,
            markers: self.raws(&[i..i + 2, j..j + 2]),
            children: self.child1(i + 2..j, self.inner(i)),
        };
        Some((node, j + 2))
    }

    // -- inline code: ^(`+)([\s\S]*?[^`])\1(?!`)

    fn inline_code(&mut self, i: usize) -> Option<(Node, usize)> {
        if self.runs.is_none() {
            let mut runs = Vec::new();
            let mut p = 0;
            while p < self.n() {
                if self.b[p] == b'`' {
                    let len = self.b[p..].iter().take_while(|&&c| c == b'`').count();
                    runs.push((p, len));
                    p += len;
                } else {
                    p += 1;
                }
            }
            let mut min = vec![usize::MAX; runs.len() + 1];
            for k in (0..runs.len()).rev() {
                min[k] = min[k + 1].min(runs[k].1);
            }
            self.runs = Some((runs, min));
        }
        let (runs, min) = self.runs.as_ref().unwrap();
        // The run holding i (i may sit inside one after a failed attempt).
        let k = runs.partition_point(|r| r.0 <= i) - 1;
        let open = runs[k].0 + runs[k].1 - i;
        let after = runs.partition_point(|r| r.0 < i + open);
        if min[after] > open {
            return None;
        }
        // The longest opener (greedy `+ backtracking) that has a closing run
        // of exactly its length; the first such run.
        let mut best: Option<(usize, usize)> = None;
        for &(start, len) in &runs[after..] {
            if len <= open && best.is_none_or(|(_, l)| len > l) {
                best = Some((start, len));
                if len == open {
                    break;
                }
            }
        }
        let (close, m) = best?;
        let content = i + m..close;
        let node = Node::Code {
            code: strip_code_spaces(&self.s[content.clone()]).to_string(),
            markers: self.raws(&[i..i + m, close..close + m]),
            raw: self.raw(content),
        };
        Some((node, close + m))
    }
}

struct Bullet {
    indent: usize,
    ordered: Option<u64>,
    marker: Range<usize>,
    content: Range<usize>,
    /// Start of the next line (or the end).
    next: usize,
}

struct Href {
    url: Range<usize>,
    title: Option<Range<usize>>,
    end: usize,
}

/// `/^ (?= *`)|(` *) $/g` → "$1": one space just inside the backticks is
/// dropped when it sits next to a backtick.
fn strip_code_spaces(s: &str) -> &str {
    let mut a = 0;
    let mut z = s.len();
    if s.starts_with(' ') && s[1..].trim_start_matches(' ').starts_with('`') {
        a = 1;
    }
    if z > a && s.ends_with(' ') && s[..z - 1].trim_end_matches(' ').ends_with('`') {
        z -= 1;
    }
    if a <= z { &s[a..z] } else { "" }
}

fn unescape_url(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&n) = chars.peek()
            && !n.is_ascii_alphanumeric()
            && !n.is_whitespace()
        {
            out.push(n);
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// An http or https URL with something after the scheme.
fn is_http(url: &str) -> bool {
    let rest = if starts_with_ci(url, "https://") {
        &url[8..]
    } else if starts_with_ci(url, "http://") {
        &url[7..]
    } else {
        return false;
    };
    !rest.is_empty() && !rest.chars().any(char::is_whitespace)
}

fn is_invite(url: &str) -> bool {
    url.strip_prefix("glance://join/")
        .is_some_and(|t| !t.is_empty() && t.bytes().all(|c| c.is_ascii_alphanumeric()))
}

/// A masked-link label that itself looks like a link.
fn has_url(label: &str) -> bool {
    let lower = label.to_ascii_lowercase();
    lower.contains("http://") || lower.contains("https://") || lower.contains("glance://")
}

// ---------------------------------------------------------------------------
// Lowering the tree to blocks

#[derive(Default)]
struct Lowerer {
    links: Vec<Link>,
    spoilers: u32,
}

/// Whether a block sits inside a line (code blocks, spoilers around them),
/// so the newline after it still belongs to the following text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Start,
    Inline,
    Line,
}

enum Piece<'n> {
    Text(&'n str, Style),
    Block(Block, Edge),
}

fn contains_block(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::CodeBlock { .. }
        | Node::Heading { .. }
        | Node::Subtext { .. }
        | Node::Quote { .. }
        | Node::List { .. } => true,
        Node::Emph { children, .. }
        | Node::Spoiler { children, .. }
        | Node::Link { children, .. } => contains_block(children),
        _ => false,
    })
}

impl Lowerer {
    fn pieces<'n>(&mut self, nodes: &'n [Node], st: Style, out: &mut Vec<Piece<'n>>) {
        for node in nodes {
            match node {
                Node::Text { text, .. } | Node::Escape { text, .. } => {
                    out.push(Piece::Text(text, st));
                }
                Node::Br => out.push(Piece::Text("\n", st)),
                Node::Emph { kind, children, .. } => {
                    let mut s = st;
                    match kind {
                        Emph::Bold => s.bold = true,
                        Emph::Italic => s.italic = true,
                        Emph::Underline => s.underline = true,
                        Emph::Strike => s.strike = true,
                    }
                    self.pieces(children, s, out);
                }
                Node::Spoiler { children, .. } => {
                    if st.spoiler.is_some() {
                        self.pieces(children, st, out);
                        continue;
                    }
                    let id = self.spoilers;
                    self.spoilers += 1;
                    let s = Style {
                        spoiler: Some(id),
                        ..st
                    };
                    if contains_block(children) {
                        let blocks = self.blocks(children, s);
                        out.push(Piece::Block(Block::Spoiler { id, blocks }, Edge::Inline));
                    } else {
                        self.pieces(children, s, out);
                    }
                }
                Node::Code { code, .. } => out.push(Piece::Text(code, Style { code: true, ..st })),
                Node::CodeBlock { lang, code, .. } => out.push(Piece::Block(
                    Block::CodeBlock {
                        lang: lang.clone(),
                        code: code.clone(),
                        spoiler: st.spoiler,
                    },
                    Edge::Inline,
                )),
                Node::Link { link, children, .. } => {
                    let idx = self.links.len() as u32;
                    self.links.push(link.clone());
                    self.pieces(
                        children,
                        Style {
                            link: Some(idx),
                            ..st
                        },
                        out,
                    );
                }
                Node::Heading {
                    level, children, ..
                } => {
                    let line = self.line(children, st);
                    out.push(Piece::Block(
                        Block::Heading {
                            level: *level,
                            line,
                        },
                        Edge::Line,
                    ));
                }
                Node::Subtext { children, .. } => {
                    let line = self.line(children, st);
                    out.push(Piece::Block(Block::Subtext(line), Edge::Line));
                }
                Node::Quote { children, .. } => {
                    let blocks = self.blocks(children, st);
                    out.push(Piece::Block(Block::Quote(blocks), Edge::Line));
                }
                Node::List { start, items } => {
                    let items = items
                        .iter()
                        .map(|(_, children)| self.blocks(children, st))
                        .collect();
                    out.push(Piece::Block(
                        Block::List {
                            start: *start,
                            items,
                        },
                        Edge::Line,
                    ));
                }
            }
        }
    }

    fn blocks(&mut self, nodes: &[Node], st: Style) -> Vec<Block> {
        let mut pieces = Vec::new();
        self.pieces(nodes, st, &mut pieces);
        group(pieces)
    }

    fn line(&mut self, nodes: &[Node], st: Style) -> RichLine {
        let mut pieces = Vec::new();
        self.pieces(nodes, st, &mut pieces);
        let mut line = RichLine::default();
        for p in pieces {
            if let Piece::Text(t, s) = p {
                line.push(t, s);
            }
        }
        line
    }
}

fn group(pieces: Vec<Piece<'_>>) -> Vec<Block> {
    let mut out = Vec::new();
    let mut cur = RichLine::default();
    let mut prev = Edge::Start;
    for p in pieces {
        match p {
            Piece::Text(t, s) => cur.push(t, s),
            Piece::Block(b, edge) => {
                flush(&mut out, std::mem::take(&mut cur), prev, true);
                out.push(b);
                prev = edge;
            }
        }
    }
    flush(&mut out, cur, prev, false);
    out
}

/// Text between blocks: the newline right after an in-line block and the one
/// right before any block only end those lines.
fn flush(out: &mut Vec<Block>, mut line: RichLine, prev: Edge, block_next: bool) {
    if prev == Edge::Inline {
        line.trim_leading_newline();
    }
    if line.text.is_empty() {
        return;
    }
    let only_spaces = !line.text.contains('\n') && line.text.trim().is_empty();
    if only_spaces && (prev != Edge::Start || block_next) {
        return;
    }
    if block_next {
        line.trim_trailing_newline();
    }
    out.push(Block::Paragraph(line));
}

impl RichLine {
    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        let end = self.text.len();
        if let Some(last) = self.spans.last_mut()
            && last.style == style
            && last.range.end == start
        {
            last.range.end = end;
            return;
        }
        self.spans.push(Span {
            range: start..end,
            style,
        });
    }

    fn trim_leading_newline(&mut self) {
        if !self.text.starts_with('\n') {
            return;
        }
        self.text.remove(0);
        for sp in &mut self.spans {
            sp.range = sp.range.start.saturating_sub(1)..sp.range.end - 1;
        }
        self.normalize();
    }

    fn trim_trailing_newline(&mut self) {
        if !self.text.ends_with('\n') {
            return;
        }
        self.text.pop();
        let len = self.text.len();
        for sp in &mut self.spans {
            sp.range = sp.range.start.min(len)..sp.range.end.min(len);
        }
        self.normalize();
    }

    fn normalize(&mut self) {
        let spans = std::mem::take(&mut self.spans);
        for sp in spans {
            if sp.range.is_empty() {
                continue;
            }
            match self.spans.last_mut() {
                Some(last) if last.style == sp.style && last.range.end == sp.range.start => {
                    last.range.end = sp.range.end;
                }
                _ => self.spans.push(sp),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Plain text

fn plain_blocks(blocks: &[Block], out: &mut String) {
    for (k, block) in blocks.iter().enumerate() {
        if k > 0 {
            out.push('\n');
        }
        match block {
            Block::Paragraph(l) | Block::Heading { line: l, .. } | Block::Subtext(l) => {
                plain_line(l, out)
            }
            Block::Quote(bs) => plain_blocks(bs, out),
            Block::CodeBlock { code, spoiler, .. } => match spoiler {
                Some(_) => out.push_str(SPOILER_MASK),
                None => out.push_str(code),
            },
            Block::List { start, items } => {
                for (j, item) in items.iter().enumerate() {
                    if j > 0 {
                        out.push('\n');
                    }
                    match start {
                        Some(n) => out.push_str(&format!("{}. ", n.saturating_add(j as u64))),
                        None => out.push_str("• "),
                    }
                    plain_blocks(item, out);
                }
            }
            Block::Spoiler { .. } => out.push_str(SPOILER_MASK),
        }
    }
}

fn plain_line(line: &RichLine, out: &mut String) {
    let mut last = None;
    for sp in &line.spans {
        match sp.style.spoiler {
            Some(id) if last == Some(id) => {}
            Some(_) => out.push_str(SPOILER_MASK),
            None => out.push_str(&line.text[sp.range.clone()]),
        }
        last = sp.style.spoiler;
    }
}

// ---------------------------------------------------------------------------
// Composer

fn push_raw(out: &mut Vec<ComposerSpan>, raw: &Raw, style: ComposerStyle) {
    if style == ComposerStyle::default() {
        return;
    }
    for r in raw {
        if !r.is_empty() {
            out.push(ComposerSpan {
                range: r.clone(),
                style,
            });
        }
    }
}

fn composer_walk(nodes: &[Node], st: ComposerStyle, out: &mut Vec<ComposerSpan>) {
    let marker = ComposerStyle { marker: true, ..st };
    for node in nodes {
        match node {
            Node::Text { raw, .. } => push_raw(out, raw, st),
            Node::Escape { marker: m, raw, .. } => {
                push_raw(out, m, marker);
                push_raw(out, raw, st);
            }
            Node::Br => {}
            Node::Emph {
                kind,
                markers,
                children,
            } => {
                push_raw(out, markers, marker);
                let mut s = st;
                match kind {
                    Emph::Bold => s.bold = true,
                    Emph::Italic => s.italic = true,
                    Emph::Underline => s.underline = true,
                    Emph::Strike => s.strike = true,
                }
                composer_walk(children, s, out);
            }
            Node::Spoiler { markers, children } => {
                push_raw(out, markers, marker);
                composer_walk(
                    children,
                    ComposerStyle {
                        spoiler: true,
                        ..st
                    },
                    out,
                );
            }
            Node::Code { markers, raw, .. } | Node::CodeBlock { markers, raw, .. } => {
                push_raw(out, markers, marker);
                push_raw(out, raw, ComposerStyle { code: true, ..st });
            }
            Node::Link {
                markers,
                url_raw,
                children,
                ..
            } => {
                let link = ComposerStyle { link: true, ..st };
                push_raw(out, markers, marker);
                push_raw(out, url_raw, link);
                composer_walk(children, link, out);
            }
            Node::Heading {
                level,
                markers,
                children,
            } => {
                push_raw(out, markers, marker);
                composer_walk(
                    children,
                    ComposerStyle {
                        heading: *level,
                        ..st
                    },
                    out,
                );
            }
            Node::Subtext { markers, children } => {
                push_raw(out, markers, marker);
                composer_walk(
                    children,
                    ComposerStyle {
                        subtext: true,
                        ..st
                    },
                    out,
                );
            }
            Node::Quote { markers, children } => {
                let q = ComposerStyle { quote: true, ..st };
                push_raw(out, markers, ComposerStyle { marker: true, ..q });
                composer_walk(children, q, out);
            }
            Node::List { items, .. } => {
                for (m, children) in items {
                    push_raw(out, m, marker);
                    composer_walk(children, st, out);
                }
            }
        }
    }
}

fn finish_composer(mut spans: Vec<ComposerSpan>) -> Vec<ComposerSpan> {
    spans.sort_by_key(|s| s.range.start);
    let mut out: Vec<ComposerSpan> = Vec::with_capacity(spans.len());
    for mut sp in spans {
        if let Some(last) = out.last() {
            // Never expected, but keep the output well-formed.
            if sp.range.start < last.range.end {
                sp.range.start = last.range.end;
            }
        }
        if sp.range.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.style == sp.style && last.range.end == sp.range.start => {
                last.range.end = sp.range.end;
            }
            _ => out.push(sp),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // -- helpers

    fn st(flags: &str) -> Style {
        let mut s = Style::default();
        for c in flags.chars() {
            match c {
                'b' => s.bold = true,
                'i' => s.italic = true,
                'u' => s.underline = true,
                's' => s.strike = true,
                'c' => s.code = true,
                _ => panic!("bad flag {c}"),
            }
        }
        s
    }

    fn linked(flags: &str, link: u32) -> Style {
        Style {
            link: Some(link),
            ..st(flags)
        }
    }

    fn hidden(flags: &str, id: u32) -> Style {
        Style {
            spoiler: Some(id),
            ..st(flags)
        }
    }

    fn line_runs(l: &RichLine) -> Vec<(String, Style)> {
        l.spans
            .iter()
            .map(|sp| (l.text[sp.range.clone()].to_string(), sp.style))
            .collect()
    }

    /// The message must be one paragraph; its runs.
    fn runs(src: &str) -> Vec<(String, Style)> {
        let m = parse(src);
        match m.blocks.as_slice() {
            [Block::Paragraph(l)] => line_runs(l),
            other => panic!("{src:?}: expected one paragraph, got {other:#?}"),
        }
    }

    fn r(text: &str, style: Style) -> (String, Style) {
        (text.to_string(), style)
    }

    fn p(text: &str) -> (String, Style) {
        r(text, Style::default())
    }

    fn literal(src: &str) {
        assert_eq!(runs(src), vec![p(src.trim())], "{src:?}");
        assert!(parse(src).links.is_empty(), "{src:?}");
    }

    fn rich(parts: &[(&str, Style)]) -> RichLine {
        let mut l = RichLine::default();
        for (t, s) in parts {
            l.push(t, *s);
        }
        l
    }

    fn para(text: &str) -> Block {
        Block::Paragraph(rich(&[(text, Style::default())]))
    }

    fn code_block(lang: Option<&str>, code: &str) -> Block {
        Block::CodeBlock {
            lang: lang.map(str::to_string),
            code: code.to_string(),
            spoiler: None,
        }
    }

    fn heading(level: u8, text: &str) -> Block {
        Block::Heading {
            level,
            line: rich(&[(text, Style::default())]),
        }
    }

    fn bullets(items: &[&str]) -> Block {
        Block::List {
            start: None,
            items: items.iter().map(|t| vec![para(t)]).collect(),
        }
    }

    fn blocks(src: &str) -> Vec<Block> {
        parse(src).blocks
    }

    fn cs(flags: &str) -> ComposerStyle {
        let mut s = ComposerStyle::default();
        for c in flags.chars() {
            match c {
                'm' => s.marker = true,
                'b' => s.bold = true,
                'i' => s.italic = true,
                'u' => s.underline = true,
                's' => s.strike = true,
                'c' => s.code = true,
                'p' => s.spoiler = true,
                'l' => s.link = true,
                't' => s.subtext = true,
                'q' => s.quote = true,
                '1' => s.heading = 1,
                '2' => s.heading = 2,
                '3' => s.heading = 3,
                _ => panic!("bad flag {c}"),
            }
        }
        s
    }

    fn comp(src: &str) -> Vec<(Range<usize>, ComposerStyle)> {
        composer_spans(src)
            .into_iter()
            .map(|s| (s.range, s.style))
            .collect()
    }

    // -- spec §7 test vectors

    #[test]
    fn spec_emphasis_vectors() {
        assert_eq!(runs("*a*"), vec![r("a", st("i"))]);
        assert_eq!(runs("_a_"), vec![r("a", st("i"))]);
        assert_eq!(runs("**a**"), vec![r("a", st("b"))]);
        assert_eq!(runs("***a***"), vec![r("a", st("bi"))]);
        assert_eq!(runs("__a__"), vec![r("a", st("u"))]);
        assert_eq!(runs("___a___"), vec![r("a", st("iu"))]);
        assert_eq!(runs("__**a**__"), vec![r("a", st("bu"))]);
        literal("snake_case_word");
        assert_eq!(runs("a*b*c"), vec![p("a"), r("b", st("i")), p("c")]);
        literal("2 * 3 * 4");
        assert_eq!(runs("~~ a ~~"), vec![r(" a ", st("s"))]);
        literal("~a~");
        // Discord's INLINE_CODE_ESCAPE_BACKTICKS_R only drops a space that
        // sits next to a backtick, so these spaces stay.
        assert_eq!(runs("`` a`b ``"), vec![r(" a`b ", st("c"))]);
    }

    #[test]
    fn spec_block_vectors() {
        assert_eq!(blocks("```js\nx\n```"), vec![code_block(Some("js"), "x")]);
        assert_eq!(blocks("```js x```"), vec![code_block(None, "js x")]);
        assert_eq!(blocks("# Hi"), vec![heading(1, "Hi")]);
        literal("#Hi");
        literal("#### Hi");
        literal("x # Hi");
        assert_eq!(
            blocks("-# small"),
            vec![Block::Subtext(rich(&[("small", Style::default())]))]
        );
        assert_eq!(blocks("> q"), vec![Block::Quote(vec![para("q")])]);
        literal(">q");
        assert_eq!(blocks("> > q"), vec![Block::Quote(vec![para("> q")])]);
        assert_eq!(
            blocks("a\n>>> b\nc"),
            vec![para("a"), Block::Quote(vec![para("b\nc")])]
        );
        assert_eq!(
            blocks("- a\n - b\n- c"),
            vec![Block::List {
                start: None,
                items: vec![vec![para("a"), bullets(&["b"])], vec![para("c")]],
            }]
        );
        assert_eq!(
            blocks("3. a\n7. b"),
            vec![Block::List {
                start: Some(3),
                items: vec![vec![para("a")], vec![para("b")]],
            }]
        );
    }

    #[test]
    fn spec_link_and_escape_vectors() {
        for src in ["[t](https://x.io)", "[t](<https://x.io>)"] {
            let m = parse(src);
            assert_eq!(
                m.links,
                vec![Link {
                    url: "https://x.io".into(),
                    kind: LinkKind::Web,
                    masked: true,
                    title: None,
                }],
                "{src}"
            );
            assert_eq!(runs(src), vec![r("t", linked("", 0))]);
        }
        let m = parse("<https://x.io>");
        assert_eq!(m.links[0].url, "https://x.io");
        assert!(!m.links[0].masked);
        assert_eq!(
            runs("<https://x.io>"),
            vec![r("https://x.io", linked("", 0))]
        );
        assert_eq!(
            runs("https://x.io."),
            vec![r("https://x.io", linked("", 0)), p(".")]
        );
        literal("\\a");
        assert_eq!(runs("\\*a\\*"), vec![p("*a*")]);
        assert_eq!(
            runs("||a `b|| c`"),
            vec![r("a `b", hidden("", 0)), p(" c`")]
        );
    }

    // -- §2.4 combinations and other emphasis rules

    #[test]
    fn emphasis_combinations() {
        assert_eq!(runs("__*a*__"), vec![r("a", st("iu"))]);
        assert_eq!(runs("__***a***__"), vec![r("a", st("biu"))]);
        literal("****");
        literal("____");
        literal("******");
        // By the regex the italic can't close before `**`, so this is
        // literal (the spec guessed "italic a, then *").
        literal("*a**");
        assert_eq!(
            runs("*a **b** c*"),
            vec![r("a ", st("i")), r("b", st("bi")), r(" c", st("i"))]
        );
        assert_eq!(runs("** a **"), vec![r(" a ", st("b"))]);
        assert_eq!(runs("a__b__c"), vec![p("a"), r("b", st("u")), p("c")]);
        assert_eq!(runs("_hello world_"), vec![r("hello world", st("i"))]);
        assert_eq!(
            runs("foo_bar_ baz"),
            vec![p("foo"), r("bar", st("i")), p(" baz")]
        );
        literal("_a_b");
        assert_eq!(runs("x _a_ y"), vec![p("x "), r("a", st("i")), p(" y")]);
        literal("*a *");
        assert_eq!(runs("*a\nb*"), vec![r("a\nb", st("i"))]);
        assert_eq!(runs("**a\nb**"), vec![r("a\nb", st("b"))]);
        assert_eq!(runs("~~a\nb~~"), vec![r("a\nb", st("s"))]);
        literal("~~a~~_");
        assert_eq!(runs("**__~~||x||~~__**"), vec![r("x", hidden("bus", 0))]);
        // Italic inside italic is flattened.
        assert_eq!(runs("*a _b_ c*"), vec![r("a b c", st("i"))]);
        // An escaped delimiter doesn't close.
        assert_eq!(runs("**a\\**"), vec![p("*"), r("a*", st("i"))]);
        assert_eq!(runs("*a\\*b*"), vec![r("a*b", st("i"))]);
        // Mid-word and unicode.
        assert_eq!(runs("é*ü*é"), vec![p("é"), r("ü", st("i")), p("é")]);
    }

    #[test]
    fn inline_code_rules() {
        assert_eq!(runs("`a`"), vec![r("a", st("c"))]);
        assert_eq!(runs("`**a**`"), vec![r("**a**", st("c"))]);
        assert_eq!(runs("`||a||`"), vec![r("||a||", st("c"))]);
        assert_eq!(runs("`\\*`"), vec![r("\\*", st("c"))]);
        assert_eq!(runs("` `` `"), vec![r("``", st("c"))]);
        assert_eq!(runs("`  a  `"), vec![r("  a  ", st("c"))]);
        assert_eq!(runs("`a\nb`"), vec![r("a\nb", st("c"))]);
        assert_eq!(runs("x `a` y"), vec![p("x "), r("a", st("c")), p(" y")]);
        literal("``");
        literal("`a");
        // An unmatched longer run backtracks to a shorter opener.
        assert_eq!(runs("``a`"), vec![r("`a", st("c"))]);
        // Code wins over a spoiler that opens inside it.
        assert_eq!(runs("`a ||b` c||"), vec![r("a ||b", st("c")), p(" c||")]);
    }

    #[test]
    fn escapes() {
        assert_eq!(runs("\\\\"), vec![p("\\")]);
        assert_eq!(runs("\\_a\\_"), vec![p("_a_")]);
        assert_eq!(runs("\\`a\\`"), vec![p("`a`")]);
        assert_eq!(runs("\\|\\|a\\|\\|"), vec![p("||a||")]);
        assert_eq!(runs("\\~\\~a\\~\\~"), vec![p("~~a~~")]);
        assert_eq!(runs("\\[a](https://x.io)"), {
            let mut v = vec![p("[a](")];
            v.push(r("https://x.io", linked("", 0)));
            v.push(p(")"));
            v
        });
        assert_eq!(runs("\\😀"), vec![p("😀")]);
        literal("\\ a");
        literal("\\1");
        literal("a\\");
        assert_eq!(blocks("\\# x"), vec![para("# x")]);
        assert_eq!(blocks("\\-# x"), vec![para("-# x")]);
        assert_eq!(blocks("\\- item"), vec![para("- item")]);
        assert_eq!(blocks("1\\. item"), vec![para("1. item")]);
        assert_eq!(blocks("\\> quote"), vec![para("> quote")]);
        assert_eq!(blocks("\\```x```"), vec![para("```x```")]);
        // Escapes do nothing inside code.
        assert_eq!(blocks("```\\*```"), vec![code_block(None, "\\*")]);
    }

    // -- links

    #[test]
    fn bare_urls() {
        let l0 = linked("", 0);
        assert_eq!(
            runs("see https://x.io/a, ok"),
            vec![p("see "), r("https://x.io/a", l0), p(", ok")]
        );
        assert_eq!(
            runs("(https://x.io)"),
            vec![p("("), r("https://x.io", l0), p(")")]
        );
        let wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(runs(wiki), vec![r(wiki, l0)]);
        assert_eq!(
            runs("https://x.io/a)."),
            vec![r("https://x.io/a", l0), p(").")]
        );
        for trail in [".", ",", ":", ";", "\"", "'", "]", ")", ".)", "..."] {
            let src = format!("https://x.io/p{trail}");
            assert_eq!(runs(&src), vec![r("https://x.io/p", l0), p(trail)], "{src}");
        }
        assert_eq!(runs("http://ab"), vec![r("http://ab", l0)]);
        literal("http://a");
        literal("ftp://x.io");
        literal("HTTPS://x.io");
        literal("mailto:a@b.c");
        // The URL stops at `<` and whitespace.
        assert_eq!(runs("https://x.io<b"), vec![r("https://x.io", l0), p("<b")]);
        // Like Discord's text rule, a URL glued to a word still links.
        assert_eq!(runs("xhttps://a.io"), vec![p("x"), r("https://a.io", l0)]);
        assert_eq!(
            runs("**https://x.io**"),
            vec![r("https://x.io", linked("b", 0))]
        );
        let m = parse("a https://x.io b https://y.io");
        assert_eq!(m.links.len(), 2);
        assert_eq!(m.links[1].url, "https://y.io");
        assert!(m.links.iter().all(|l| !l.masked && l.kind == LinkKind::Web));
        assert_eq!(
            runs("a https://x.io b https://y.io"),
            vec![
                p("a "),
                r("https://x.io", l0),
                p(" b "),
                r("https://y.io", linked("", 1))
            ]
        );
    }

    #[test]
    fn angle_links() {
        // Not an angle link, but the bare URL inside still links.
        assert_eq!(parse("<https://x.io/a b>").links[0].url, "https://x.io/a");
        literal("<steam://run/1>");
        literal("<https:x>");
        literal("<>");
        literal("<a>");
        let m = parse("<glance://join/ABC234>");
        assert_eq!(m.links[0].kind, LinkKind::GlanceInvite);
        assert_eq!(m.links[0].url, "glance://join/ABC234");
        assert_eq!(parse("<HTTP://X.IO>").links[0].url, "HTTP://X.IO");
    }

    #[test]
    fn masked_links() {
        let l0 = linked("", 0);
        let m = parse("[t](https://x.io \"Title\")");
        assert_eq!(m.links[0].title.as_deref(), Some("Title"));
        assert_eq!(m.links[0].url, "https://x.io");
        assert!(m.links[0].masked);
        let m = parse("[t](https://x.io 'Single')");
        assert_eq!(m.links[0].title.as_deref(), Some("Single"));
        assert_eq!(parse("[t]( https://x.io )").links[0].url, "https://x.io");
        assert_eq!(
            parse("[w](https://en.wikipedia.org/wiki/Rust_(lang))").links[0].url,
            "https://en.wikipedia.org/wiki/Rust_(lang)"
        );
        assert_eq!(parse("[a](HTTPS://X.IO)").links[0].url, "HTTPS://X.IO");
        assert_eq!(
            parse("[a](https://x.io/\\_y)").links[0].url,
            "https://x.io/_y"
        );
        // Formatting inside the label.
        assert_eq!(
            runs("[**b** c](https://x.io)"),
            vec![r("b", linked("b", 0)), r(" c", l0)]
        );
        // Not an image.
        assert_eq!(runs("![a](https://x.io)"), vec![p("!"), r("a", l0)]);
        // Brackets inside the label.
        assert_eq!(runs("[a [b] c](https://x.io)"), vec![r("a [b] c", l0)]);
        // Two links.
        assert_eq!(
            runs("[a](https://x.io) and [b](https://y.io)"),
            vec![r("a", l0), p(" and "), r("b", linked("", 1))]
        );
        // Literal: other schemes, a URL in the label, invites, empty labels.
        for src in [
            "[t](javascript:alert(1))",
            "[t](ftp://x.io)",
            "[t](file:///etc/passwd)",
            "[t](/relative)",
            "[t]()",
            "[https://a.io](https://b.io)",
            "[see HTTP://a.io](https://b.io)",
            "[join](glance://join/ABC)",
            "[](https://x.io)",
            "[ ](https://x.io)",
            "[a <https://b.io>](https://x.io)",
        ] {
            literal(src);
        }
        // Unfinished or malformed.
        assert_eq!(
            runs("[t] (https://x.io)"),
            vec![p("[t] ("), r("https://x.io", l0), p(")")]
        );
        literal("[t]");
        literal("[t](a b)");
        assert_eq!(
            runs("[t](https://x.io"),
            vec![p("[t]("), r("https://x.io", l0)]
        );
    }

    #[test]
    fn glance_invites() {
        let m = parse("join glance://join/ABCDEF234 now");
        assert_eq!(
            m.links,
            vec![Link {
                url: "glance://join/ABCDEF234".into(),
                kind: LinkKind::GlanceInvite,
                masked: false,
                title: None,
            }]
        );
        assert_eq!(
            runs("join glance://join/ABCDEF234 now"),
            vec![
                p("join "),
                r("glance://join/ABCDEF234", linked("", 0)),
                p(" now")
            ]
        );
        literal("glance://join/");
        literal("glance://other/ABC");
        literal("glance://join/!");
    }

    // -- code blocks

    #[test]
    fn code_block_language_rules() {
        assert_eq!(blocks("```py code```"), vec![code_block(None, "py code")]);
        assert_eq!(blocks("```py\n```"), vec![code_block(None, "py")]);
        assert_eq!(
            blocks("```py \ncode```"),
            vec![code_block(None, "py \ncode")]
        );
        assert_eq!(blocks("```c++ x\ny```"), vec![code_block(None, "c++ x\ny")]);
        assert_eq!(blocks("```c++\nx```"), vec![code_block(Some("c++"), "x")]);
        assert_eq!(blocks("```C#\nx```"), vec![code_block(Some("c#"), "x")]);
        assert_eq!(
            blocks("```objective-c\nx```"),
            vec![code_block(Some("objective-c"), "x")]
        );
        assert_eq!(blocks("```x.y\nz```"), vec![code_block(Some("x.y"), "z")]);
        assert_eq!(blocks("```\n\nfoo\n\n```"), vec![code_block(None, "foo")]);
        assert_eq!(
            blocks("```rs\n\n\nfn x() {}\n```"),
            vec![code_block(Some("rs"), "fn x() {}")]
        );
        assert_eq!(
            blocks("```\n  indented\n```"),
            vec![code_block(None, "  indented")]
        );
        assert_eq!(blocks("```a```"), vec![code_block(None, "a")]);
        assert_eq!(
            blocks("```**a** ||b||```"),
            vec![code_block(None, "**a** ||b||")]
        );
        assert_eq!(blocks("```a\nb\nc```"), vec![code_block(Some("a"), "b\nc")]);
        assert_eq!(blocks("```py\n\n```"), vec![code_block(Some("py"), "\n")]);
        // The first fence closes; fences don't nest.
        assert_eq!(
            blocks("```a``` b ```c```"),
            vec![code_block(None, "a"), para(" b "), code_block(None, "c")]
        );
        assert_eq!(blocks("````a````"), vec![code_block(None, "`a"), para("`")]);
    }

    #[test]
    fn code_block_position_and_unclosed() {
        assert_eq!(
            blocks("before ```x``` after"),
            vec![para("before "), code_block(None, "x"), para(" after")]
        );
        assert_eq!(
            blocks("a\n```\nx\n```\nb"),
            vec![para("a"), code_block(None, "x"), para("b")]
        );
        assert_eq!(
            blocks("a\n\n```x```\n\nb"),
            vec![para("a\n"), code_block(None, "x"), para("\nb")]
        );
        assert_eq!(
            blocks("```a```\n```b```"),
            vec![code_block(None, "a"), code_block(None, "b")]
        );
        assert_eq!(
            blocks("```a```\n\n```b```"),
            vec![code_block(None, "a"), para(""), code_block(None, "b")]
        );
        assert_eq!(
            blocks("```a``` ```b```"),
            vec![code_block(None, "a"), code_block(None, "b")]
        );
        assert_eq!(blocks("```rust\nfn"), vec![para("```rust\nfn")]);
        literal("``````");
        literal("```");
        assert_eq!(
            blocks("**a ```x``` b**"),
            vec![
                Block::Paragraph(rich(&[("a ", st("b"))])),
                code_block(None, "x"),
                Block::Paragraph(rich(&[(" b", st("b"))])),
            ]
        );
    }

    // -- quotes

    #[test]
    fn quotes() {
        assert_eq!(blocks("> a\n> b"), vec![Block::Quote(vec![para("a\nb")])]);
        assert_eq!(
            blocks("> a\nb"),
            vec![Block::Quote(vec![para("a")]), para("b")]
        );
        assert_eq!(
            blocks("> a\n\n> b"),
            vec![
                Block::Quote(vec![para("a")]),
                para(""),
                Block::Quote(vec![para("b")])
            ]
        );
        assert_eq!(
            blocks(">>> a\n> b\nc"),
            vec![Block::Quote(vec![para("a\n> b\nc")])]
        );
        assert_eq!(blocks(">>>\na\nb"), vec![Block::Quote(vec![para("a\nb")])]);
        literal(">>>a");
        literal(">> a");
        literal("a > b");
        assert_eq!(
            blocks("x\n  > indented"),
            vec![para("x"), Block::Quote(vec![para("indented")])]
        );
        // `>` alone doesn't continue a quote.
        assert_eq!(
            blocks("> a\n>\n> b"),
            vec![
                Block::Quote(vec![para("a")]),
                para(">"),
                Block::Quote(vec![para("b")])
            ]
        );
        assert_eq!(
            blocks("> # Title\n> text"),
            vec![Block::Quote(vec![heading(1, "Title"), para("text")])]
        );
        assert_eq!(
            blocks("> -# small"),
            vec![Block::Quote(vec![Block::Subtext(rich(&[(
                "small",
                Style::default()
            )]))])]
        );
        assert_eq!(
            blocks("> - a\n> - b"),
            vec![Block::Quote(vec![bullets(&["a", "b"])])]
        );
        assert_eq!(
            blocks("> ```js\n> x\n> ```"),
            vec![Block::Quote(vec![code_block(Some("js"), "x")])]
        );
        assert_eq!(
            blocks(">>> # T\n- a\n```x```"),
            vec![Block::Quote(vec![
                heading(1, "T"),
                bullets(&["a"]),
                code_block(None, "x"),
            ])]
        );
        // No nesting inside `>>>` either.
        assert_eq!(
            blocks(">>> a\n>>> b"),
            vec![Block::Quote(vec![para("a\n>>> b")])]
        );
        assert_eq!(
            blocks("> **a\n> b**"),
            vec![Block::Quote(vec![Block::Paragraph(rich(&[(
                "a\nb",
                st("b")
            )]))])]
        );
    }

    // -- lists

    #[test]
    fn lists() {
        assert_eq!(blocks("- a\n- b"), vec![bullets(&["a", "b"])]);
        assert_eq!(blocks("* a\n* b"), vec![bullets(&["a", "b"])]);
        assert_eq!(blocks("- a\n* b"), vec![bullets(&["a", "b"])]);
        // A `*` list item wins over italic.
        assert_eq!(blocks("* a*"), vec![bullets(&["a*"])]);
        assert_eq!(runs("*a*"), vec![r("a", st("i"))]);
        assert_eq!(
            blocks("1. a\n2. b\n2. c"),
            vec![Block::List {
                start: Some(1),
                items: vec![vec![para("a")], vec![para("b")], vec![para("c")]],
            }]
        );
        literal("-item");
        literal("1.item");
        literal("1) a");
        literal("+ a");
        literal("x - a");
        literal("1234567890. too long");
        assert_eq!(blocks("- a\nb"), vec![bullets(&["a"]), para("b")]);
        assert_eq!(blocks("x\n- a"), vec![para("x"), bullets(&["a"])]);
        assert_eq!(
            blocks("- a\n\n- b"),
            vec![bullets(&["a"]), para(""), bullets(&["b"])]
        );
        // A change of kind starts a new list.
        assert_eq!(
            blocks("- a\n1. b"),
            vec![
                bullets(&["a"]),
                Block::List {
                    start: Some(1),
                    items: vec![vec![para("b")]],
                }
            ]
        );
        // Ordered inside unordered, three levels.
        assert_eq!(
            blocks("- a\n  1. b\n  2. c\n    - d\n- e"),
            vec![Block::List {
                start: None,
                items: vec![
                    vec![
                        para("a"),
                        Block::List {
                            start: Some(1),
                            items: vec![vec![para("b")], vec![para("c"), bullets(&["d"])]],
                        }
                    ],
                    vec![para("e")],
                ],
            }]
        );
        assert_eq!(
            blocks("- # big\n- **b**"),
            vec![Block::List {
                start: None,
                items: vec![
                    vec![heading(1, "big")],
                    vec![Block::Paragraph(rich(&[("b", st("b"))]))]
                ],
            }]
        );
        assert_eq!(
            blocks("- a ```x```"),
            vec![Block::List {
                start: None,
                items: vec![vec![para("a "), code_block(None, "x")]],
            }]
        );
        assert_eq!(
            blocks("2024. was a year"),
            vec![Block::List {
                start: Some(2024),
                items: vec![vec![para("was a year")]],
            }]
        );
        assert_eq!(
            blocks("- - x"),
            vec![Block::List {
                start: None,
                items: vec![vec![bullets(&["x"])]],
            }]
        );
    }

    // -- headings and subtext

    #[test]
    fn headings_and_subtext() {
        assert_eq!(blocks("## h2"), vec![heading(2, "h2")]);
        assert_eq!(blocks("### h3"), vec![heading(3, "h3")]);
        assert_eq!(blocks("#   spaced  "), vec![heading(1, "spaced")]);
        assert_eq!(blocks("# a #"), vec![heading(1, "a #")]);
        assert_eq!(blocks("# # a"), vec![heading(1, "# a")]);
        assert_eq!(
            blocks("# **b** c"),
            vec![Block::Heading {
                level: 1,
                line: rich(&[("b", st("b")), (" c", Style::default())]),
            }]
        );
        literal("#");
        literal("# ");
        literal("##");
        literal("#\tx");
        assert_eq!(blocks("a\n  # x"), vec![para("a\n  # x")]);
        assert_eq!(blocks("a\n#  \nb"), vec![para("a\n#  \nb")]);
        assert_eq!(blocks("# h\ntext"), vec![heading(1, "h"), para("text")]);
        assert_eq!(blocks("# h\n\ntext"), vec![heading(1, "h"), para("\ntext")]);
        assert_eq!(blocks("a\n# h"), vec![para("a"), heading(1, "h")]);
        assert_eq!(blocks("# a\n## b"), vec![heading(1, "a"), heading(2, "b")]);
        // A code block inside a heading is shown as inline code.
        assert_eq!(
            blocks("# a ```x``` b"),
            vec![Block::Heading {
                level: 1,
                line: rich(&[
                    ("a ", Style::default()),
                    ("x", st("c")),
                    (" b", Style::default())
                ]),
            }]
        );
        assert_eq!(
            blocks("-# small *it*"),
            vec![Block::Subtext(rich(&[
                ("small ", Style::default()),
                ("it", st("i"))
            ]))]
        );
        literal("-#x");
        literal("-## x");
        assert_eq!(
            blocks("-# a\n-# b"),
            vec![
                Block::Subtext(rich(&[("a", Style::default())])),
                Block::Subtext(rich(&[("b", Style::default())]))
            ]
        );
        // A heading inside a spoiler at line start.
        assert_eq!(
            blocks("||# x||"),
            vec![Block::Spoiler {
                id: 0,
                blocks: vec![Block::Heading {
                    level: 1,
                    line: rich(&[("x", hidden("", 0))]),
                }],
            }]
        );
    }

    // -- spoilers

    #[test]
    fn spoilers() {
        let m = parse("||a|| ||b||");
        assert_eq!(m.spoilers, 2);
        assert_eq!(
            runs("||a|| ||b||"),
            vec![r("a", hidden("", 0)), p(" "), r("b", hidden("", 1))]
        );
        assert_eq!(
            runs("||a ||b|| c||"),
            vec![r("a ", hidden("", 0)), p("b"), r(" c", hidden("", 1))]
        );
        literal("||||");
        literal("||a");
        assert_eq!(runs("|||||"), vec![r("|", hidden("", 0))]);
        assert_eq!(runs("||a\nb||"), vec![r("a\nb", hidden("", 0))]);
        assert_eq!(
            runs("||**b** `c`||"),
            vec![
                r("b", hidden("b", 0)),
                r(" ", hidden("", 0)),
                r("c", hidden("c", 0))
            ]
        );
        // A spoiler around a code block hides the whole group.
        let m = parse("||```rust\nx```||");
        assert_eq!(
            m.blocks,
            vec![Block::Spoiler {
                id: 0,
                blocks: vec![Block::CodeBlock {
                    lang: Some("rust".into()),
                    code: "x".into(),
                    spoiler: Some(0),
                }],
            }]
        );
        assert_eq!(m.spoilers, 1);
        let m = parse("see ||```x``` and **b**|| end ||c||");
        assert_eq!(
            m.blocks,
            vec![
                para("see "),
                Block::Spoiler {
                    id: 0,
                    blocks: vec![
                        Block::CodeBlock {
                            lang: None,
                            code: "x".into(),
                            spoiler: Some(0),
                        },
                        Block::Paragraph(rich(&[(" and ", hidden("", 0)), ("b", hidden("b", 0))])),
                    ],
                },
                Block::Paragraph(rich(&[(" end ", Style::default()), ("c", hidden("", 1))])),
            ]
        );
        assert_eq!(m.spoilers, 2);
        // A spoiler inside a block spoiler merges into it.
        let m = parse("||```x``` ||y|| ||");
        assert_eq!(m.spoilers, 2);
        // A quote inside a spoiler.
        assert_eq!(
            blocks("||> q||"),
            vec![Block::Spoiler {
                id: 0,
                blocks: vec![Block::Quote(vec![Block::Paragraph(rich(&[(
                    "q",
                    hidden("", 0)
                )]))])],
            }]
        );
        // Links inside spoilers keep both.
        assert_eq!(
            runs("||https://x.io||"),
            vec![r(
                "https://x.io",
                Style {
                    link: Some(0),
                    ..hidden("", 0)
                }
            )]
        );
    }

    #[test]
    fn whitespace_and_breaks() {
        assert_eq!(parse(""), Message::default());
        assert_eq!(parse("  \n\t "), Message::default());
        assert_eq!(blocks("  hi  "), vec![para("hi")]);
        assert_eq!(blocks("a\n\n\nb"), vec![para("a\n\n\nb")]);
        assert_eq!(blocks("a  b"), vec![para("a  b")]);
        // Discord tokens are literal.
        literal("<@123> <#456> <:e:789> <t:1:R> @everyone");
    }

    // -- plain text

    #[test]
    fn plain_text_cases() {
        let cases = [
            ("**bold** _it_ __u__ ~~s~~", "bold it u s"),
            ("||secret|| text", "▮▮▮▮ text"),
            ("||a|| and ||b||", "▮▮▮▮ and ▮▮▮▮"),
            ("||a\nb|| c", "▮▮▮▮ c"),
            ("||**a** b||", "▮▮▮▮"),
            ("||```x```||", "▮▮▮▮"),
            ("x ||```y``` z|| w", "x \n▮▮▮▮\n w"),
            ("[label](https://x.io)", "label"),
            ("[**label**](https://x.io \"t\")", "label"),
            ("<https://x.io>", "https://x.io"),
            ("go https://x.io.", "go https://x.io."),
            ("```rust\nfn x()\n```", "fn x()"),
            ("a\n```x```\nb", "a\nx\nb"),
            ("a ```x``` b", "a \nx\n b"),
            ("`co` de", "co de"),
            ("> quote\nafter", "quote\nafter"),
            (">>> a\nb", "a\nb"),
            ("# Title\nbody", "Title\nbody"),
            ("-# small", "small"),
            ("- a\n- b", "• a\n• b"),
            ("3. a\n9. b", "3. a\n4. b"),
            ("  hi  ", "hi"),
            ("\\*x\\*", "*x*"),
            ("*a**", "*a**"),
            ("", ""),
            ("line one\nline two", "line one\nline two"),
        ];
        for (src, want) in cases {
            assert_eq!(plain_text(src), want, "{src:?}");
        }
    }

    // -- composer

    #[test]
    fn composer_basic() {
        assert_eq!(
            comp("**bold**"),
            vec![(0..2, cs("m")), (2..6, cs("b")), (6..8, cs("m"))]
        );
        assert_eq!(
            comp("a *i* b"),
            vec![(2..3, cs("m")), (3..4, cs("i")), (4..5, cs("m"))]
        );
        assert_eq!(
            comp("__u__ ~~s~~"),
            vec![
                (0..2, cs("m")),
                (2..3, cs("u")),
                (3..5, cs("m")),
                (6..8, cs("m")),
                (8..9, cs("s")),
                (9..11, cs("m"))
            ]
        );
        assert_eq!(
            comp("`x`"),
            vec![(0..1, cs("m")), (1..2, cs("c")), (2..3, cs("m"))]
        );
        assert_eq!(
            comp("||s||"),
            vec![(0..2, cs("m")), (2..3, cs("p")), (3..5, cs("m"))]
        );
        assert_eq!(comp("# Head"), vec![(0..2, cs("m")), (2..6, cs("1"))]);
        assert_eq!(comp("### H"), vec![(0..4, cs("m")), (4..5, cs("3"))]);
        assert_eq!(comp("-# s"), vec![(0..3, cs("m")), (3..4, cs("t"))]);
        assert_eq!(comp("> q"), vec![(0..2, cs("mq")), (2..3, cs("q"))]);
        assert_eq!(
            comp("> a\n> b"),
            vec![
                (0..2, cs("mq")),
                (2..3, cs("q")),
                (4..6, cs("mq")),
                (6..7, cs("q"))
            ]
        );
        assert_eq!(comp(">>> a"), vec![(0..4, cs("mq")), (4..5, cs("q"))]);
        assert_eq!(comp("- a\n12. b"), vec![(0..2, cs("m")), (4..8, cs("m"))]);
        assert_eq!(comp("\\*"), vec![(0..1, cs("m"))]);
        assert_eq!(
            comp("[a](https://x.io)"),
            vec![
                (0..1, cs("m")),
                (1..2, cs("l")),
                (2..4, cs("m")),
                (4..16, cs("l")),
                (16..17, cs("m"))
            ]
        );
        assert_eq!(comp("https://x.io"), vec![(0..12, cs("l"))]);
        assert_eq!(
            comp("<https://x.io>"),
            vec![(0..1, cs("m")), (1..13, cs("l")), (13..14, cs("m"))]
        );
        assert_eq!(
            comp("```js\nx\n```"),
            vec![(0..6, cs("m")), (6..7, cs("c")), (8..11, cs("m"))]
        );
        // Offsets survive the trimmed leading whitespace.
        assert_eq!(
            comp("  **b**  "),
            vec![(2..4, cs("m")), (4..5, cs("b")), (5..7, cs("m"))]
        );
        // Unicode.
        assert_eq!(
            comp("é **ü**"),
            vec![(3..5, cs("m")), (5..7, cs("b")), (7..9, cs("m"))]
        );
        // Nested styles carry the outer flags.
        assert_eq!(
            comp("**a *b***"),
            vec![
                (0..2, cs("m")),
                (2..4, cs("b")),
                (4..5, cs("bm")),
                (5..6, cs("bi")),
                (6..7, cs("bm")),
                (7..9, cs("m"))
            ]
        );
        // Inside quotes the ranges skip the `> ` of each line.
        assert_eq!(
            comp("> **a\n> b**"),
            vec![
                (0..4, cs("mq")),
                (4..5, cs("bq")),
                (6..8, cs("mq")),
                (8..9, cs("bq")),
                (9..11, cs("mq"))
            ]
        );
    }

    #[test]
    fn composer_typing_in_progress() {
        for src in [
            "**bol",
            "*it",
            "__u",
            "~~s",
            "||sp",
            "`co",
            "```rs\nfn",
            "[a](https://x",
            "[a]",
            "#",
            "# ",
            "-#",
            ">",
            "-",
            "1.",
            "\\",
            "<https://x.io",
            "~",
            "|",
            "**a *b",
        ] {
            let spans = comp(src);
            assert!(
                spans.iter().all(|(_, s)| !s.bold
                    && !s.italic
                    && !s.underline
                    && !s.strike
                    && !s.code
                    && !s.spoiler
                    && s.heading == 0),
                "{src:?}: {spans:?}"
            );
        }
        assert!(comp("**bol").is_empty());
        assert!(comp("`co").is_empty());
        // A rejected masked link is plain.
        assert!(comp("[t](ftp://x.io)").is_empty());
    }

    // -- invariants over many generated strings

    fn check_line(l: &RichLine, m: &Message) {
        let mut pos = 0;
        for (k, sp) in l.spans.iter().enumerate() {
            assert_eq!(sp.range.start, pos, "gap or overlap in {l:?}");
            assert!(sp.range.end > sp.range.start, "empty span in {l:?}");
            assert!(l.text.is_char_boundary(sp.range.start));
            assert!(l.text.is_char_boundary(sp.range.end));
            if k > 0 {
                assert_ne!(l.spans[k - 1].style, sp.style, "unmerged spans in {l:?}");
            }
            if let Some(i) = sp.style.link {
                assert!((i as usize) < m.links.len());
            }
            if let Some(i) = sp.style.spoiler {
                assert!(i < m.spoilers);
            }
            pos = sp.range.end;
        }
        assert_eq!(pos, l.text.len(), "spans don't cover {l:?}");
    }

    fn check_blocks(blocks: &[Block], m: &Message) {
        for b in blocks {
            match b {
                Block::Paragraph(l) | Block::Subtext(l) => check_line(l, m),
                Block::Heading { level, line } => {
                    assert!((1..=3).contains(level));
                    assert!(!line.text.is_empty());
                    check_line(line, m);
                }
                Block::Quote(bs) => check_blocks(bs, m),
                Block::CodeBlock { spoiler, code, .. } => {
                    assert!(!code.is_empty());
                    if let Some(i) = spoiler {
                        assert!(*i < m.spoilers);
                    }
                }
                Block::List { items, .. } => {
                    assert!(!items.is_empty());
                    for it in items {
                        check_blocks(it, m);
                    }
                }
                Block::Spoiler { id, blocks } => {
                    assert!(*id < m.spoilers);
                    check_blocks(blocks, m);
                }
            }
        }
    }

    fn check_all(src: &str) {
        let m = parse(src);
        check_blocks(&m.blocks, &m);
        for l in &m.links {
            match l.kind {
                LinkKind::Web => assert!(is_http(&l.url), "{src:?}: {l:?}"),
                LinkKind::GlanceInvite => {
                    assert!(is_invite(&l.url), "{src:?}: {l:?}");
                    assert!(!l.masked);
                }
            }
        }
        let plain = plain_text(src);
        assert!(plain.len() <= src.len() * 4 + 16);
        let spans = composer_spans(src);
        let mut end = 0;
        for sp in &spans {
            assert!(sp.range.start >= end, "{src:?}: overlap {spans:?}");
            assert!(sp.range.end > sp.range.start, "{src:?}: empty {spans:?}");
            assert!(sp.range.end <= src.len());
            assert!(src.is_char_boundary(sp.range.start), "{src:?}: {spans:?}");
            assert!(src.is_char_boundary(sp.range.end), "{src:?}: {spans:?}");
            assert_ne!(sp.style, ComposerStyle::default());
            end = sp.range.end;
        }
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    const PIECES: &[&str] = &[
        "*",
        "**",
        "***",
        "_",
        "__",
        "~~",
        "~",
        "||",
        "|",
        "`",
        "``",
        "```",
        "\\",
        "[",
        "]",
        "(",
        ")",
        "](",
        "<",
        ">",
        "#",
        "# ",
        "## ",
        "-# ",
        "> ",
        ">>> ",
        ">>>",
        "- ",
        "* ",
        "1. ",
        "  ",
        " ",
        "\n",
        "\n\n",
        "a",
        "b",
        "word",
        "é",
        "😀",
        "ü",
        "\u{a0}",
        "\t",
        "https://x.io",
        "http://a.b/c(d)",
        "glance://join/AB2",
        "js\n",
        " \"t\"",
        "'",
        "\"",
        ".",
        ",",
        "!",
        "1",
        "\u{2028}",
        "\r\n",
    ];

    fn generate(rng: &mut Lcg, max_pieces: u64) -> String {
        let count = rng.next() % (max_pieces + 1);
        (0..count)
            .map(|_| PIECES[(rng.next() % PIECES.len() as u64) as usize])
            .collect()
    }

    #[test]
    fn generated_strings_keep_invariants() {
        let mut rng = Lcg(0x5eed_1234_abcd_0042);
        for _ in 0..6000 {
            check_all(&generate(&mut rng, 30));
        }
        for _ in 0..200 {
            check_all(&generate(&mut rng, 400));
        }
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let cases: Vec<String> = vec![
            "||".repeat(300) + "a" + &"||".repeat(300),
            "*_".repeat(300) + "a" + &"_*".repeat(300),
            "**__".repeat(200) + "a" + &"__**".repeat(200),
            "~~".repeat(300) + "a" + &"~~".repeat(300),
            "[".repeat(300) + "a" + &"](https://x.io)".repeat(300),
            (0..200)
                .map(|k| format!("{}- x\n", " ".repeat(k)))
                .collect(),
            (0..200)
                .map(|k| format!("{}- ", " ".repeat(k)))
                .collect::<String>()
                + "x",
            "- ".repeat(500) + "x",
            "> ".repeat(500) + "x",
            "||> ".repeat(100) + "x",
            "||**".repeat(100) + "x" + &"**||".repeat(100),
        ];
        for src in &cases {
            check_all(src);
        }
        // Past the cap the content stays literal.
        let src = "||".repeat(40) + "a" + &"||".repeat(40);
        let plain = parse(&src);
        assert!(!plain.blocks.is_empty());
    }

    #[test]
    fn long_inputs_parse_fast() {
        let n = 10_000;
        let mut inputs: Vec<String> = [
            "*", "|", "_", "`", "~", "[", "]", "(", "<", ">", "\\", "#", "-", " ", "\n", ".", ":",
        ]
        .iter()
        .map(|c| c.repeat(n))
        .collect();
        for pat in [
            "*a",
            "_a",
            "a_",
            "**a",
            "__a",
            "~~a",
            "||a",
            "`a",
            "``a",
            "[a",
            "[a]",
            "[a](",
            "[a](b",
            "](",
            "<a",
            "<a:/",
            "http://",
            "https://x.io/(",
            "> a\n",
            "- a\n",
            "1. ",
            "# ",
            "-# ",
            "```",
            "````a",
            "\\*",
            "* ",
            "*a ",
            "_a ",
            "a*b",
            "[[",
            "]]",
            "|a",
            "~a",
            "!",
            "[a](https://x.io \"",
            " 'x",
            "😀*",
            "**é",
            "\n> ",
            "# ``",
        ] {
            inputs.push(pat.repeat(n / pat.len()));
        }
        inputs.push("[".repeat(n / 2) + &"]".repeat(n / 2));
        inputs.push("(".repeat(n / 2) + &")".repeat(n / 2));
        inputs.push("*".repeat(n / 2) + "a" + &" *".repeat(n / 4));
        inputs.push("``".to_string() + &"a`".repeat(n / 2));
        inputs.push((1..140).map(|k| "`".repeat(k) + "x").collect());
        inputs.push("# ".to_string() + &"`".repeat(n));
        inputs.push("[".to_string() + &"`".repeat(n));
        let mut rng = Lcg(7);
        inputs.push(generate(&mut rng, 20_000));
        for src in &inputs {
            let t = Instant::now();
            let m = parse(src);
            let _ = plain_text(src);
            let _ = composer_spans(src);
            let took = t.elapsed();
            let head: String = src.chars().take(12).collect();
            assert!(
                took < Duration::from_secs(2),
                "{head:?}... ({} bytes) took {took:?}",
                src.len()
            );
            check_blocks(&m.blocks, &m);
        }
    }
}
