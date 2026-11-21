//! The chat box: a multi-line editor that shows markdown formatting live
//! while you type, like Discord's composer.
//!
//! gpui-kit's only input with custom highlighting is the code-editor kind
//! (`EditorState`), so the composer is one with every code affordance turned
//! off, the UI font instead of the monospace one, and a highlighter that
//! styles the raw source from `glance_core::markdown::composer_spans`.
//!
//! - Enter (and Ctrl/Cmd+Enter, `secondary: true`) emits
//!   `InputEvent::PressEnter { shift: false, .. }` and inserts nothing;
//!   Shift+Enter inserts a newline (`submit_on_enter`). Subscribers send on
//!   `shift: false`.
//! - Text over the send cap stays in the box ([`too_long`]); the views show
//!   [`too_long_note`] until it is short enough.
//! - Tab and Shift+Tab move focus instead of indenting.
//! - The box grows from one visual (wrapped) row up to `max_rows`, then
//!   scrolls. The editor kind has no auto-grow, so `composer_element` sizes it
//!   from the wrapped row count of the last layout.
//! - The composer can't change font size or family per span: headings are
//!   bold, inline code gets the code background, and quotes are unstyled.

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use glance_core::markdown::{ComposerSpan, ComposerStyle, composer_spans};
use gpui_kit::component::input::{
    Editor, EditorState, FoldRange, HighlightStyleResolver, IndentInline, InputEdit,
    InputHighlighter, OutdentInline, Rope,
};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Size};
use gpui_kit::*;

use crate::theme::Palette;

/// Row height relative to the font size (Discord: 22px rows for 16px text).
const LINE_HEIGHT: f32 = 1.375;
/// Space above and below the text inside the pill (Discord: a 44px bar).
const PAD_Y: f32 = 11.;
/// Used for a state `new_composer` didn't make.
const DEFAULT_MAX_ROWS: usize = 8;

/// The palette colours the highlighter paints with.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ComposerColors {
    muted: Hsla,
    link: Hsla,
    code_bg: Hsla,
    spoiler_bg: Hsla,
}

impl ComposerColors {
    fn new(p: &Palette) -> Self {
        Self {
            muted: p.text_muted,
            link: p.link,
            code_bg: p.code_bg,
            spoiler_bg: p.spoiler_hidden,
        }
    }
}

/// What `composer_element` needs to know about each composer.
#[derive(Clone)]
struct ComposerEntry {
    max_rows: usize,
    /// Shared with the highlighter, which has no `cx` in `styles()`; refreshed
    /// on every render so a theme switch repaints without an edit.
    colors: Rc<Cell<ComposerColors>>,
}

#[derive(Default)]
struct Composers(HashMap<EntityId, ComposerEntry>);

impl Global for Composers {}

/// A chat box: multi-line, Enter or Ctrl/Cmd+Enter sends
/// (`InputEvent::PressEnter { shift: false, .. }`), Shift+Enter adds a newline, soft-wrapped, UI
/// font, no line numbers/folding/indent guides/auto-close/search, grows from 1
/// up to `max_rows` visual rows and then scrolls, with live markdown
/// highlighting.
pub fn new_composer(
    placeholder: &str,
    max_rows: usize,
    window: &mut Window,
    cx: &mut App,
) -> Entity<EditorState> {
    let colors = Rc::new(Cell::new(ComposerColors::new(&Palette::current(cx))));
    let factory_colors = colors.clone();
    let placeholder = SharedString::from(placeholder.to_string());
    let state = cx.new(|cx| {
        let mut state = EditorState::new(window, cx)
            .placeholder(placeholder)
            .submit_on_enter(true)
            .line_number(false)
            .folding(false)
            .indent_guides(false)
            .auto_close(false)
            .smart_indent(false)
            .searchable(false)
            .soft_wrap(true)
            .scroll_beyond_last_line(Some(0))
            .cursor_surrounding_lines(Some(0));
        state.set_highlighter_factory(
            Rc::new(move |_language: &str| {
                let highlighter: Box<dyn InputHighlighter> =
                    Box::new(MarkdownHighlighter::new(factory_colors.clone()));
                Some(highlighter)
            }),
            cx,
        );
        state
    });
    let entry = ComposerEntry {
        max_rows: max_rows.max(1),
        colors,
    };
    cx.default_global::<Composers>()
        .0
        .insert(state.entity_id(), entry);
    let id = state.entity_id();
    cx.observe_release(&state, move |_, cx| {
        cx.default_global::<Composers>().0.remove(&id);
    })
    .detach();
    state
}

/// The length of what Enter would send (trimmed), when that is over `max`
/// bytes: such a message stays in the composer instead of being cut.
pub fn too_long(state: &EditorState, max: usize) -> Option<usize> {
    // Cheap (no copy) while the whole text fits.
    if state.text().len() <= max {
        return None;
    }
    let n = state.text().to_string().trim().len();
    (n > max).then_some(n)
}

/// Discord's "too long" note for a composer, while its text is over `max`.
pub fn too_long_note(state: &Entity<EditorState>, max: usize, cx: &App) -> Option<Div> {
    let n = too_long(state.read(cx), max)?;
    Some(
        div()
            .text_xs()
            .text_color(Palette::current(cx).red)
            .child(format!("Your message is too long ({n}/{max})")),
    )
}

/// The editor, styled to sit inside the existing input pill (no background or
/// border of its own), sized to its content.
pub fn composer_element(state: &Entity<EditorState>, cx: &App) -> impl IntoElement {
    build(state, None, cx)
}

/// [`composer_element`] with a paste hook, as `Input::on_paste`: return true
/// to consume the paste (files, images), false to paste the text as usual.
pub fn composer_element_with_paste(
    state: &Entity<EditorState>,
    on_paste: impl Fn(&ClipboardItem, &mut Window, &mut App) -> bool + 'static,
    cx: &App,
) -> impl IntoElement {
    build(state, Some(Rc::new(on_paste)), cx)
}

type PasteHandler = Rc<dyn Fn(&ClipboardItem, &mut Window, &mut App) -> bool>;

fn build(state: &Entity<EditorState>, on_paste: Option<PasteHandler>, cx: &App) -> AnyElement {
    let p = Palette::current(cx);
    let theme = cx.theme();
    let font_family = theme.font_family.clone();
    let font_size = theme.font_size;

    let entry = cx
        .try_global::<Composers>()
        .and_then(|c| c.0.get(&state.entity_id()).cloned());
    let max_rows = entry.as_ref().map_or(DEFAULT_MAX_ROWS, |e| e.max_rows);
    if let Some(entry) = &entry {
        entry.colors.set(ComposerColors::new(&p));
    }

    let editor = state.read(cx);
    // The last layout's row height; before the first one, what it will be
    // (gpui rounds line heights to whole pixels).
    let line_height = editor
        .line_height()
        .unwrap_or_else(|| (font_size * LINE_HEIGHT).round());
    let rows = visual_rows(editor.display_map().wrap_row_count(), max_rows);
    // The code editor is at least two rows tall (its built-in `rows`, which
    // only a Textarea can change). For one row the frame is shorter than
    // that, and the editor's root clips the empty second row.
    let height = line_height * rows as f32;

    let mut editor = Editor::new(state)
        .with_size(Size::XSmall)
        .appearance(false)
        .bordered(false)
        .font_family(font_family)
        .text_size(font_size)
        .line_height(relative(LINE_HEIGHT))
        .text_color(p.text_normal)
        .h(height);
    if let Some(handler) = on_paste {
        editor = editor.on_paste(move |item, window, cx| handler(item, window, cx));
    }

    // The rows come from the layout, which runs after this render: when an
    // edit or a resize changes the wrapped row count, ask for another frame.
    let watched = state.clone();
    let watcher = canvas(
        move |_, _, cx| {
            let now = visual_rows(watched.read(cx).display_map().wrap_row_count(), max_rows);
            if now != rows {
                watched.update(cx, |_, cx| cx.notify());
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_0();

    div()
        .w_full()
        .min_w_0()
        .py(px(PAD_Y))
        .relative()
        // Tab moves focus, as in Discord, instead of indenting.
        .capture_action(|_: &IndentInline, window, cx| {
            window.focus_next(cx);
            cx.stop_propagation();
        })
        .capture_action(|_: &OutdentInline, window, cx| {
            window.focus_prev(cx);
            cx.stop_propagation();
        })
        .child(editor)
        .child(watcher)
        .into_any_element()
}

/// How many rows the box shows for `wrapped` rows of text.
fn visual_rows(wrapped: usize, max_rows: usize) -> usize {
    wrapped.clamp(1, max_rows.max(1))
}

/// Styles the raw source with `composer_spans`, recomputed on every edit
/// (over at most the first [`HIGHLIGHT_CAP`] bytes).
struct MarkdownHighlighter {
    spans: Vec<ComposerSpan>,
    colors: Rc<Cell<ComposerColors>>,
}

impl MarkdownHighlighter {
    fn new(colors: Rc<Cell<ComposerColors>>) -> Self {
        Self {
            spans: Vec::new(),
            colors,
        }
    }
}

/// The composer highlights at most this much of its text; the rest shows
/// plain. Highlighting is linear but runs on every keystroke, and a pasted
/// megabyte would cost ~100 ms each time. Four times the send cap, so
/// anything sendable (and a fair bit more) is styled.
const HIGHLIGHT_CAP: usize = 4 * glance_proto::MAX_CHAT_LEN;

/// The start of the text, up to [`HIGHLIGHT_CAP`] bytes on a char boundary.
fn highlight_source<'a>(chunks: impl Iterator<Item = &'a str>) -> String {
    let mut src = String::new();
    for chunk in chunks {
        src.push_str(chunk);
        if src.len() >= HIGHLIGHT_CAP {
            break;
        }
    }
    glance_proto::truncate_utf8(&mut src, HIGHLIGHT_CAP);
    src
}

impl InputHighlighter for MarkdownHighlighter {
    fn language(&self) -> SharedString {
        "glance-markdown".into()
    }

    fn update(
        &mut self,
        _edit: Option<InputEdit>,
        text: &Rope,
        _folding: bool,
        _window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        self.colors.set(ComposerColors::new(&Palette::current(cx)));
        self.spans = composer_spans(&highlight_source(text.chunks()));
    }

    fn update_batch(
        &mut self,
        edits: &[(InputEdit, Rope)],
        folding: bool,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        // Only the final text matters: the spans are recomputed in full.
        if let Some((edit, text)) = edits.last() {
            self.update(Some(*edit), text, folding, window, cx);
        }
    }

    fn styles(
        &self,
        range: &Range<usize>,
        _resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        style_runs(&self.spans, range, &self.colors.get())
    }

    fn fold_ranges(&self, _text: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

/// How one composer style looks.
fn highlight_style(style: &ComposerStyle, c: &ComposerColors) -> HighlightStyle {
    let color = if style.marker || style.subtext {
        Some(c.muted)
    } else if style.link {
        Some(c.link)
    } else {
        None
    };
    let background_color = if style.code {
        Some(c.code_bg)
    } else if style.spoiler {
        Some(c.spoiler_bg)
    } else {
        None
    };
    HighlightStyle {
        color,
        font_weight: (style.bold || style.heading > 0).then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        background_color,
        underline: style.underline.then(|| UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        }),
        strikethrough: style.strike.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: None,
        }),
        fade_out: None,
    }
}

/// Ordered, non-overlapping runs that cover `range` exactly, as
/// `InputHighlighter::styles` must return: the spans clipped to the range,
/// with the gaps between them filled with the default style.
fn style_runs(
    spans: &[ComposerSpan],
    range: &Range<usize>,
    colors: &ComposerColors,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if range.is_empty() {
        return vec![(range.clone(), HighlightStyle::default())];
    }
    let mut runs = Vec::new();
    let mut at = range.start;
    // Spans are ordered, so skip the ones that end before the range.
    let first = spans.partition_point(|s| s.range.end <= range.start);
    for span in &spans[first..] {
        if span.range.start >= range.end {
            break;
        }
        // Clamp to the range, and to what earlier runs already covered.
        let start = span.range.start.max(at);
        let end = span.range.end.min(range.end);
        if start >= end {
            continue;
        }
        if start > at {
            runs.push((at..start, HighlightStyle::default()));
        }
        runs.push((start..end, highlight_style(&span.style, colors)));
        at = end;
    }
    if at < range.end {
        runs.push((at..range.end, HighlightStyle::default()));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::{
        ComposerColors, HIGHLIGHT_CAP, highlight_source, highlight_style, style_runs, visual_rows,
    };

    #[test]
    fn highlighting_stops_at_the_cap() {
        let short = ["**a**", " b"];
        assert_eq!(highlight_source(short.into_iter()), "**a** b");
        let big = "é".repeat(HIGHLIGHT_CAP);
        let src = highlight_source([big.as_str(), "tail"].into_iter());
        assert!(src.len() <= HIGHLIGHT_CAP && src.len() > HIGHLIGHT_CAP - 2);
        assert!(src.chars().all(|c| c == 'é'));
    }
    use glance_core::markdown::{ComposerSpan, ComposerStyle, composer_spans};
    use gpui_kit::{FontStyle, FontWeight, HighlightStyle, Hsla, hsla};
    use std::ops::Range;

    fn colors() -> ComposerColors {
        ComposerColors {
            muted: hsla(0., 0., 0.5, 1.),
            link: hsla(0.55, 1., 0.5, 1.),
            code_bg: hsla(0., 0., 0.2, 1.),
            spoiler_bg: hsla(0., 0., 0.1, 1.),
        }
    }

    fn span(range: Range<usize>, style: ComposerStyle) -> ComposerSpan {
        ComposerSpan { range, style }
    }

    fn marker() -> ComposerStyle {
        ComposerStyle {
            marker: true,
            ..Default::default()
        }
    }

    fn bold() -> ComposerStyle {
        ComposerStyle {
            bold: true,
            ..Default::default()
        }
    }

    /// The runs are in order, touch each other, and cover the range exactly.
    fn assert_covers(runs: &[(Range<usize>, HighlightStyle)], range: &Range<usize>) {
        assert!(!runs.is_empty());
        assert_eq!(runs[0].0.start, range.start);
        assert_eq!(runs.last().unwrap().0.end, range.end);
        for pair in runs.windows(2) {
            assert_eq!(pair[0].0.end, pair[1].0.start, "{runs:?}");
        }
        for (r, _) in runs {
            assert!(r.start < r.end || range.is_empty(), "{runs:?}");
        }
    }

    #[test]
    fn no_spans_is_one_plain_run() {
        let runs = style_runs(&[], &(0..5), &colors());
        assert_eq!(runs, vec![(0..5, HighlightStyle::default())]);
    }

    #[test]
    fn gaps_are_filled_with_the_default_style() {
        // "a **b** c"
        let spans = composer_spans("a **b** c");
        let c = colors();
        let runs = style_runs(&spans, &(0..9), &c);
        assert_covers(&runs, &(0..9));
        let muted = highlight_style(&marker(), &c);
        let strong = highlight_style(&bold(), &c);
        assert_eq!(
            runs,
            vec![
                (0..2, HighlightStyle::default()),
                (2..4, muted),
                (4..5, strong),
                (5..7, muted),
                (7..9, HighlightStyle::default()),
            ]
        );
    }

    #[test]
    fn spans_are_clipped_to_the_requested_range() {
        // A second line requested on its own: "x\n**bold**\n" asks for 2..11.
        let spans = composer_spans("x\n**bold**\n");
        let c = colors();
        let runs = style_runs(&spans, &(4..7), &c);
        assert_eq!(runs, vec![(4..7, highlight_style(&bold(), &c))]);

        let runs = style_runs(&spans, &(3..11), &c);
        assert_covers(&runs, &(3..11));
        assert_eq!(runs[0], (3..4, highlight_style(&marker(), &c)));
        assert_eq!(runs.last().unwrap().0, 10..11);
        assert_eq!(runs.last().unwrap().1, HighlightStyle::default());
    }

    #[test]
    fn overlapping_or_unordered_input_still_gives_a_cover() {
        let spans = vec![
            span(0..4, bold()),
            span(2..6, marker()),
            span(9..20, bold()),
        ];
        let runs = style_runs(&spans, &(1..12), &colors());
        assert_covers(&runs, &(1..12));
        assert_eq!(runs[0].0, 1..4);
        assert_eq!(runs[1].0, 4..6);
        assert_eq!(runs[2], (6..9, HighlightStyle::default()));
        assert_eq!(runs[3].0, 9..12);
    }

    #[test]
    fn an_empty_range_gets_one_empty_run() {
        let runs = style_runs(&[span(0..3, bold())], &(2..2), &colors());
        assert_eq!(runs, vec![(2..2, HighlightStyle::default())]);
    }

    #[test]
    fn styles_map_to_highlights() {
        let c = colors();
        let style = |f: fn(&mut ComposerStyle)| {
            let mut s = ComposerStyle::default();
            f(&mut s);
            highlight_style(&s, &c)
        };

        assert_eq!(style(|_| {}), HighlightStyle::default());
        assert_eq!(style(|s| s.marker = true).color, Some(c.muted));
        assert_eq!(style(|s| s.subtext = true).color, Some(c.muted));
        assert_eq!(style(|s| s.link = true).color, Some(c.link));
        // A link's brackets are markers, and markers stay muted.
        assert_eq!(
            style(|s| {
                s.link = true;
                s.marker = true
            })
            .color,
            Some(c.muted)
        );
        assert_eq!(style(|s| s.bold = true).font_weight, Some(FontWeight::BOLD));
        assert_eq!(style(|s| s.heading = 2).font_weight, Some(FontWeight::BOLD));
        assert_eq!(
            style(|s| s.italic = true).font_style,
            Some(FontStyle::Italic)
        );
        assert!(style(|s| s.underline = true).underline.is_some());
        assert!(style(|s| s.strike = true).strikethrough.is_some());
        assert_eq!(style(|s| s.code = true).background_color, Some(c.code_bg));
        assert_eq!(
            style(|s| s.spoiler = true).background_color,
            Some(c.spoiler_bg)
        );
        // Code inside a spoiler keeps the code fill.
        assert_eq!(
            style(|s| {
                s.code = true;
                s.spoiler = true
            })
            .background_color,
            Some(c.code_bg)
        );
        // Spoiler text stays readable: no colour change, only a fill.
        assert_eq!(style(|s| s.spoiler = true).color, None::<Hsla>);
        // Quotes are unstyled (the composer can't draw the bar).
        assert_eq!(style(|s| s.quote = true), HighlightStyle::default());
    }

    #[test]
    fn rows_grow_from_one_to_the_cap() {
        assert_eq!(visual_rows(0, 8), 1);
        assert_eq!(visual_rows(1, 8), 1);
        assert_eq!(visual_rows(5, 8), 5);
        assert_eq!(visual_rows(30, 8), 8);
        assert_eq!(visual_rows(3, 0), 1);
    }
}
