//! Discord-style markdown for chat (contract types).
//!
//! STUB: this file only carries the shared contract so the composer can be
//! built against it. The real parser lands from its own branch and replaces
//! this file at merge. `composer_spans` here only knows `**bold**`.

use std::ops::Range;

/// Inline style flags for one span of rendered text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Inline code: monospace and a code background.
    pub code: bool,
    /// Id of the enclosing spoiler (unique within one message, from 0).
    pub spoiler: Option<u32>,
    /// Index into [`Message::links`].
    pub link: Option<u32>,
}

/// A byte range into [`RichLine::text`] and its style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Web,
    /// A `glance://join/...` ticket, opened in the app.
    GlanceInvite,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub kind: LinkKind,
    pub masked: bool,
    pub title: Option<String>,
}

/// One run of rendered text. Spans cover the text completely, in order, and
/// never overlap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RichLine {
    pub text: String,
    pub spans: Vec<Span>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(RichLine),
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
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Spoiler {
        id: u32,
        blocks: Vec<Block>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub blocks: Vec<Block>,
    pub links: Vec<Link>,
    pub spoilers: u32,
}

/// Stub: the whole message as one plain paragraph.
pub fn parse(src: &str) -> Message {
    let text = src.trim().to_string();
    let spans = if text.is_empty() {
        Vec::new()
    } else {
        vec![Span {
            range: 0..text.len(),
            style: Style::default(),
        }]
    };
    Message {
        blocks: vec![Block::Paragraph(RichLine { text, spans })],
        ..Message::default()
    }
}

/// Stub: the trimmed source.
pub fn plain_text(src: &str) -> String {
    src.trim().to_string()
}

/// Live formatting for the composer: style runs over the raw source, ordered
/// and non-overlapping; uncovered text is plain.
pub fn composer_spans(src: &str) -> Vec<ComposerSpan> {
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(open) = src[from..].find("**").map(|i| from + i) {
        let inner = open + 2;
        let Some(close) = src[inner..].find("**").map(|i| inner + i) else {
            break;
        };
        if close == inner {
            from = inner;
            continue;
        }
        let marker = ComposerStyle {
            marker: true,
            ..ComposerStyle::default()
        };
        let bold = ComposerStyle {
            bold: true,
            ..ComposerStyle::default()
        };
        spans.push(ComposerSpan {
            range: open..inner,
            style: marker,
        });
        spans.push(ComposerSpan {
            range: inner..close,
            style: bold,
        });
        spans.push(ComposerSpan {
            range: close..close + 2,
            style: marker,
        });
        from = close + 2;
    }
    spans
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ComposerStyle {
    /// Syntax characters, drawn muted.
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
