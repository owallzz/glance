//! Renders a parsed chat message (`glance_core::markdown::Message`) the way
//! Discord does: `RichText` for every run of text, plain divs for quote bars,
//! list markers, code boxes and block spoilers.
//!
//! - Parsed messages are cached by text ([`MarkdownCache`]), so the parser
//!   doesn't run every frame.
//! - Every `RichText` gets the next number from a counter threaded through
//!   the render of the whole visible chat list, so a drag selects in reading
//!   order across messages.
//! - Hidden spoilers draw their text in a transparent colour on the spoiler
//!   fill, copy as a mask, and turn every click inside them into a reveal (a
//!   link inside a hidden spoiler isn't clickable). Reveals live in
//!   `GlanceApp::revealed_spoilers` for the session.
//! - Code blocks use `code_highlight`; a grammar's first parse runs on a
//!   worker, and [`watch_highlights`] re-renders when it's done.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

use glance_core::PeerId;
use glance_core::markdown::{self, Block, Link, Message, RichLine, Style};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;

use crate::app::GlanceApp;
use crate::code_highlight;
use crate::rich_text::RichText;
use crate::theme::Palette;

/// Parsed messages kept (oldest dropped first).
const CACHE_CAPACITY: usize = 1024;
/// Discord's body line height (22px rows for 16px text).
const LINE_HEIGHT: f32 = 1.375;

/// Where a message lives, so spoiler reveals don't leak between chats whose
/// message ids overlap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChatScope {
    /// Room chat, by a hash of the room link.
    Room(u64),
    Dm(PeerId),
}

impl ChatScope {
    pub fn room(link: &str) -> Self {
        let mut h = DefaultHasher::new();
        link.hash(&mut h);
        Self::Room(h.finish())
    }
}

/// One revealed spoiler: (chat, message id, spoiler id).
pub type SpoilerKey = (ChatScope, u64, u32);

/// Parsed messages by text.
#[derive(Default)]
pub struct MarkdownCache {
    map: HashMap<u64, (Rc<str>, Rc<Message>)>,
    order: VecDeque<u64>,
}

impl MarkdownCache {
    /// The parsed form of `text`, parsing it on a miss.
    pub fn get(&mut self, text: &str) -> Rc<Message> {
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        let key = h.finish();
        if let Some((src, msg)) = self.map.get(&key)
            && &**src == text
        {
            return msg.clone();
        }
        let msg = Rc::new(markdown::parse(text));
        if self.map.insert(key, (text.into(), msg.clone())).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > CACHE_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        msg
    }
}

/// Re-render `GlanceApp` whenever a code block's highlight finishes on the
/// highlighter's worker (or its syntax set finishes loading). Call once.
pub fn watch_highlights(cx: &mut Context<GlanceApp>) {
    let (tx, rx) = flume::bounded::<()>(1);
    code_highlight::set_on_ready(move || {
        // A full channel already has a re-render queued.
        let _ = tx.try_send(());
    });
    code_highlight::warm_up();
    cx.spawn(async move |this, cx| {
        while rx.recv_async().await.is_ok() {
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Everything a message render needs from the app.
pub struct MessageView<'a> {
    pub scope: ChatScope,
    pub msg_id: u64,
    /// Next `document_order` for selectable text; advanced per element.
    pub order: &'a mut u64,
    pub revealed: &'a HashSet<SpoilerKey>,
}

/// Renders `msg` as a column of blocks.
pub fn render_message(
    msg: &Rc<Message>,
    view: MessageView<'_>,
    cx: &Context<GlanceApp>,
) -> AnyElement {
    let theme = cx.theme();
    let mut r = Renderer {
        p: Palette::current(cx),
        scope: view.scope,
        msg_id: view.msg_id,
        order: view.order,
        revealed: view.revealed,
        app: cx.entity().downgrade(),
        links: msg.links.clone().into(),
        size: theme.font_size,
        ui_font: theme.font_family.clone(),
        mono_font: theme.mono_font_family.clone(),
        n: 0,
        hidden: false,
        block_spoiler: None,
        depth: 0,
    };
    let size = r.size;
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .text_size(size)
        .line_height(relative(LINE_HEIGHT))
        .children(r.blocks(&msg.blocks, true))
        .into_any_element()
}

/// What a click on a range of text does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Link(u32),
    Reveal(u32),
}

struct Renderer<'a> {
    p: Palette,
    scope: ChatScope,
    msg_id: u64,
    order: &'a mut u64,
    revealed: &'a HashSet<SpoilerKey>,
    app: WeakEntity<GlanceApp>,
    links: Rc<[Link]>,
    size: Pixels,
    ui_font: SharedString,
    mono_font: SharedString,
    /// Elements made so far in this message (for element ids).
    n: usize,
    /// Inside a hidden block spoiler: everything is invisible.
    hidden: bool,
    /// The block spoiler we're inside (its tint is on the box, not the runs).
    block_spoiler: Option<u32>,
    /// List nesting depth.
    depth: usize,
}

impl Renderer<'_> {
    fn is_revealed(&self, id: u32) -> bool {
        self.revealed.contains(&(self.scope, self.msg_id, id))
    }

    fn next_id(&mut self) -> ElementId {
        self.n += 1;
        let scope = match self.scope {
            ChatScope::Room(h) => format!("r{h:x}"),
            ChatScope::Dm(p) => format!("d{}", p.short()),
        };
        SharedString::from(format!("md-{scope}-{}-{}", self.msg_id, self.n)).into()
    }

    fn next_order(&mut self) -> u64 {
        *self.order += 1;
        *self.order
    }

    /// A colour that disappears while hidden.
    fn visible(&self, c: Hsla) -> Hsla {
        if self.hidden { c.opacity(0.) } else { c }
    }

    fn blocks(&mut self, blocks: &[Block], top: bool) -> Vec<AnyElement> {
        blocks
            .iter()
            .enumerate()
            .map(|(i, b)| self.block(b, top && i == 0))
            .collect()
    }

    fn block(&mut self, block: &Block, first: bool) -> AnyElement {
        let p = self.p;
        match block {
            Block::Paragraph(line) => self.line(line, p.text_normal, false),
            Block::Heading { level, line } => {
                let (scale, first_top) = match level {
                    1 => (1.5, 8.),
                    2 => (1.25, 8.),
                    _ => (1.0, 4.),
                };
                div()
                    .mt(px(if first { first_top } else { 16. }))
                    .mb(px(8.))
                    .text_size(self.size * scale)
                    .child(self.line(line, p.text_header, true))
                    .into_any_element()
            }
            Block::Subtext(line) => div()
                .text_size(self.size * 0.8125)
                .line_height(relative(1.375))
                .child(self.line(line, p.text_muted, false))
                .into_any_element(),
            Block::Quote(inner) => {
                let bar = self.visible(p.quote_bar);
                div()
                    .flex()
                    .min_w_0()
                    .child(div().w(px(4.)).flex_shrink_0().rounded(px(4.)).bg(bar))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pl(px(12.))
                            .pr(px(8.))
                            .flex()
                            .flex_col()
                            .children(self.blocks(inner, false)),
                    )
                    .into_any_element()
            }
            Block::CodeBlock { lang, code, .. } => self.code_block(lang.as_deref(), code),
            Block::List { start, items } => self.list(*start, items),
            Block::Spoiler { id, blocks } => self.block_spoiler(*id, blocks),
        }
    }

    fn list(&mut self, start: Option<u64>, items: &[Vec<Block>]) -> AnyElement {
        let p = self.p;
        let color = self.visible(p.text_normal);
        // Ordered markers are right-aligned in a column as wide as the
        // longest number (Discord: 0.4em + 0.6em per digit).
        let marker_w = match start {
            Some(n) => {
                let last = n.saturating_add(items.len().saturating_sub(1) as u64);
                let digits = last.to_string().len() as f32;
                self.size * (0.4 + 0.6 * digits) + px(6.)
            }
            None => px(16.),
        };
        let bullet = if self.depth == 0 { "•" } else { "◦" };
        self.depth += 1;
        let rows: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let marker = match start {
                    Some(n) => format!("{}.", n.saturating_add(i as u64)),
                    None => bullet.to_string(),
                };
                div()
                    .flex()
                    .min_w_0()
                    .child(
                        div()
                            .w(marker_w)
                            .flex_shrink_0()
                            .flex()
                            .justify_end()
                            .pr(px(6.))
                            .text_color(color)
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .children(self.blocks(item, false)),
                    )
                    .into_any_element()
            })
            .collect();
        self.depth -= 1;
        div()
            .mt(px(4.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .min_w_0()
            .children(rows)
            .into_any_element()
    }

    fn code_block(&mut self, lang: Option<&str>, code: &str) -> AnyElement {
        let p = self.p;
        let code = code.strip_suffix('\n').unwrap_or(code);
        // An empty block still gets a line.
        let text: SharedString = if code.is_empty() { " " } else { code }.to_string().into();
        let font = font(self.mono_font.clone());
        let plain = |len: usize, color: Hsla| TextRun {
            len,
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let mut runs = Vec::new();
        let highlights = lang.and_then(|l| code_highlight::highlight(l, &text));
        let mut at = 0;
        if let Some(hl) = highlights.filter(|_| !self.hidden) {
            for (range, kind) in hl.iter() {
                if range.start < at || range.end > text.len() {
                    continue;
                }
                if range.start > at {
                    runs.push(plain(range.start - at, p.text_normal));
                }
                runs.push(plain(range.len(), code_highlight::color(*kind, &p)));
                at = range.end;
            }
        }
        if at < text.len() {
            runs.push(plain(text.len() - at, self.visible(p.text_normal)));
        }
        let mut rich = RichText::new(self.next_id(), text.clone())
            .runs(runs)
            .document_order(self.next_order());
        if self.hidden {
            rich = rich.masked_ranges(std::iter::once(0..text.len()).collect());
        }
        div()
            .my(px(4.))
            .p(px(8.))
            .rounded(px(4.))
            .border_1()
            .border_color(self.visible(p.code_border))
            .bg(self.visible(p.code_bg))
            .text_size(self.size * 0.875)
            .line_height(relative(18. / 14.))
            .min_w_0()
            .child(rich)
            .into_any_element()
    }

    fn block_spoiler(&mut self, id: u32, blocks: &[Block]) -> AnyElement {
        let p = self.p;
        let shown = self.is_revealed(id) && !self.hidden;
        let (was_hidden, was_spoiler) = (self.hidden, self.block_spoiler);
        self.hidden = self.hidden || !shown;
        self.block_spoiler = Some(id);
        let inner = self.blocks(blocks, false);
        self.hidden = was_hidden;
        self.block_spoiler = was_spoiler;
        let el = div()
            .id(self.next_id())
            .rounded(px(4.))
            .px(px(4.))
            .min_w_0()
            .flex()
            .flex_col()
            .children(inner);
        if shown {
            return el.bg(p.spoiler_revealed).into_any_element();
        }
        let hover = p.spoiler_hidden_hover;
        let key = (self.scope, self.msg_id, id);
        let app = self.app.clone();
        el.bg(p.spoiler_hidden)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(move |_, _, cx| reveal(&app, key, cx))
            .into_any_element()
    }

    /// One run of inline text.
    fn line(&mut self, line: &RichLine, color: Hsla, bold: bool) -> AnyElement {
        let p = self.p;
        let text: SharedString = line.text.clone().into();
        let mut runs = Vec::with_capacity(line.spans.len());
        // Clickable ranges, merged across spans with the same target.
        let mut ranges: Vec<(Range<usize>, Target)> = Vec::new();
        let mut masked: Vec<Range<usize>> = Vec::new();
        for span in &line.spans {
            let st = span.style;
            let hidden_spoiler = self.hidden
                || st
                    .spoiler
                    .is_some_and(|id| Some(id) != self.block_spoiler && !self.is_revealed(id));
            runs.push(self.run(span.range.len(), &st, color, bold, hidden_spoiler));
            let target = if self.hidden {
                None
            } else if hidden_spoiler {
                st.spoiler.map(Target::Reveal)
            } else {
                st.link.map(Target::Link)
            };
            if let Some(t) = target {
                match ranges.last_mut() {
                    Some((r, last)) if *last == t && r.end == span.range.start => {
                        r.end = span.range.end
                    }
                    _ => ranges.push((span.range.clone(), t)),
                }
            }
            if hidden_spoiler {
                match masked.last_mut() {
                    Some(r) if r.end == span.range.start => r.end = span.range.end,
                    _ => masked.push(span.range.clone()),
                }
            }
        }
        let mut rich = RichText::new(self.next_id(), text)
            .runs(runs)
            .document_order(self.next_order())
            .masked_ranges(masked);
        if !ranges.is_empty() {
            let targets: Rc<[Target]> = ranges.iter().map(|(_, t)| *t).collect();
            let app = self.app.clone();
            let links = self.links.clone();
            let (scope, msg_id) = (self.scope, self.msg_id);
            let on_click = {
                let targets = targets.clone();
                move |ix: usize, window: &mut Window, cx: &mut App| match targets.get(ix) {
                    Some(Target::Reveal(id)) => reveal(&app, (scope, msg_id, *id), cx),
                    Some(Target::Link(l)) => {
                        if let Some(link) = links.get(*l as usize).cloned() {
                            let _ = app.update(cx, |app, cx| app.open_chat_link(&link, window, cx));
                        }
                    }
                    None => {}
                }
            };
            let tip_targets = targets.clone();
            let links = self.links.clone();
            let hover_targets = targets;
            let (link_color, spoiler_hover) = (p.link, p.spoiler_hidden_hover);
            rich = rich
                .on_click(ranges.into_iter().map(|(r, _)| r).collect(), on_click)
                .tooltip(move |ix, window, cx| {
                    let Some(Target::Link(l)) = tip_targets.get(ix) else {
                        return None;
                    };
                    let link = links.get(*l as usize)?;
                    if !link.masked {
                        return None;
                    }
                    let tip = link.title.clone().unwrap_or_else(|| link.url.clone());
                    Some(Tooltip::new(tip).build(window, cx))
                })
                .hovered_range_style(move |ix| match hover_targets.get(ix)? {
                    Target::Link(_) => Some(HighlightStyle {
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            color: Some(link_color),
                            wavy: false,
                        }),
                        ..Default::default()
                    }),
                    Target::Reveal(_) => Some(HighlightStyle {
                        background_color: Some(spoiler_hover),
                        ..Default::default()
                    }),
                });
        }
        rich.into_any_element()
    }

    fn run(&self, len: usize, st: &Style, color: Hsla, bold: bool, hidden: bool) -> TextRun {
        let p = self.p;
        let mut f = font(if st.code {
            self.mono_font.clone()
        } else {
            self.ui_font.clone()
        });
        if st.bold || bold {
            f.weight = FontWeight::BOLD;
        }
        if st.italic {
            f.style = FontStyle::Italic;
        }
        let mut color = if st.link.is_some() { p.link } else { color };
        let mut background = st.code.then_some(p.code_bg);
        if let Some(id) = st.spoiler
            && Some(id) != self.block_spoiler
        {
            background = Some(if hidden {
                p.spoiler_hidden
            } else {
                p.spoiler_revealed
            });
        }
        if hidden {
            color = color.opacity(0.);
            if self.hidden {
                background = None;
            }
        }
        TextRun {
            len,
            font: f,
            color,
            background_color: background,
            underline: st.underline.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(color),
                wavy: false,
            }),
            strikethrough: st.strike.then_some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(color),
            }),
        }
    }
}

fn reveal(app: &WeakEntity<GlanceApp>, key: SpoilerKey, cx: &mut App) {
    let _ = app.update(cx, |app, cx| {
        app.revealed_spoilers.insert(key);
        cx.notify();
    });
}

/// The host of an http(s) URL, lowercased, without user info or port.
/// `None` for any other scheme.
pub fn web_domain(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    // Browsers (WHATWG URL rules) treat `\` like `/` in http(s) URLs, so it
    // ends the authority too: `https://evil.com\@trusted.com` opens evil.com.
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = if let Some(v6) = host.strip_prefix('[') {
        // [::1]:8080
        format!("[{}]", v6.split(']').next().unwrap_or_default())
    } else {
        host.split(':').next().unwrap_or_default().to_string()
    };
    let host = host.trim_end_matches('.').to_string();
    (!host.is_empty()).then_some(host)
}

impl GlanceApp {
    /// A link in a chat message was clicked.
    pub fn open_chat_link(&mut self, link: &Link, window: &mut Window, cx: &mut Context<Self>) {
        match link.kind {
            markdown::LinkKind::GlanceInvite => {
                // Like a link handed over by the OS: switches rooms if needed.
                self.join_link(&link.url, cx);
                self.screen = crate::app::Screen::Room;
            }
            markdown::LinkKind::Web => {
                // The parser only makes http(s) links; check again anyway.
                let Some(domain) = web_domain(&link.url) else {
                    return;
                };
                if self.settings.trusted_domains.contains(&domain) {
                    cx.open_url(&link.url);
                } else {
                    self.leaving = Some(crate::app::LeavingLink {
                        url: link.url.clone(),
                        domain,
                        trust: false,
                    });
                    // Escape closes the dialog.
                    self.focus.focus(window, cx);
                }
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{MarkdownCache, web_domain};

    #[test]
    fn web_domains() {
        assert_eq!(
            web_domain("https://Example.com/a?b").as_deref(),
            Some("example.com")
        );
        assert_eq!(web_domain("http://a.io:8080").as_deref(), Some("a.io"));
        assert_eq!(
            web_domain("https://trusted.com@evil.com/x").as_deref(),
            Some("evil.com")
        );
        assert_eq!(
            web_domain(r"https://evil.com\@trusted.com").as_deref(),
            Some("evil.com")
        );
        assert_eq!(
            web_domain(r"https://evil.com\\@trusted.com/x").as_deref(),
            Some("evil.com")
        );
        assert_eq!(web_domain("https://[::1]:80/").as_deref(), Some("[::1]"));
        assert_eq!(web_domain("HTTPS://x.org.").as_deref(), Some("x.org"));
        assert_eq!(web_domain("glance://join/abc"), None);
        assert_eq!(web_domain("javascript:alert(1)"), None);
        assert_eq!(web_domain("https://"), None);
    }

    #[test]
    fn cache_reuses_parses_and_stays_bounded() {
        let mut cache = MarkdownCache::default();
        let a = cache.get("**hi**");
        let b = cache.get("**hi**");
        assert!(std::rc::Rc::ptr_eq(&a, &b));
        for i in 0..(super::CACHE_CAPACITY + 10) {
            cache.get(&format!("message {i}"));
        }
        assert!(cache.map.len() <= super::CACHE_CAPACITY);
        assert_eq!(cache.map.len(), cache.order.len());
    }
}
