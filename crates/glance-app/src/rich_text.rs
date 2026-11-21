//! `RichText`: styled, wrapping text that takes part in the window-wide text
//! selection (like `gpui_kit::base::SelectableText`) and has clickable ranges
//! with a pointing-hand cursor, tooltips and an optional hover style (like
//! GPUI's `InteractiveText`).
//!
//! The text is laid out from caller-given [`TextRun`]s, so a range can use its
//! own font (monospace inline code) and background. Clicks, the cursor and
//! tooltips are delegated to an inner `InteractiveText`; selection follows
//! `SelectableText` through the public `gpui_kit::base` selection API.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::base::{
    TextSelection, TextSelectionHandle, TextSelectionRegistration, TextSelectionRun, Theme,
};
use gpui_kit::*;

type ClickFn = Rc<dyn Fn(usize, &mut Window, &mut App)>;
type TooltipFn = Rc<dyn Fn(usize, &mut Window, &mut App) -> Option<AnyView>>;
type HoverStyleFn = Rc<dyn Fn(usize) -> Option<HighlightStyle>>;

/// Rich, selectable text with clickable ranges.
pub struct RichText {
    id: ElementId,
    text: SharedString,
    runs: Option<Vec<TextRun>>,
    masked: Rc<[Range<usize>]>,
    document_order: u64,
    click_ranges: Rc<[Range<usize>]>,
    on_click: Option<ClickFn>,
    tooltip: Option<TooltipFn>,
    hovered_style: Option<HoverStyleFn>,
    /// Built in `request_layout`, once the hovered range is known.
    inner: Option<InteractiveText>,
    layout: Option<TextLayout>,
}

impl RichText {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            runs: None,
            masked: Rc::from(Vec::new()),
            document_order: 0,
            click_ranges: Rc::from(Vec::new()),
            on_click: None,
            tooltip: None,
            hovered_style: None,
            inner: None,
            layout: None,
        }
    }

    /// Styles the text. The runs must cover the whole text, as for
    /// `StyledText::with_runs`; without them the text uses the inherited style.
    pub fn runs(mut self, runs: Vec<TextRun>) -> Self {
        debug_assert_eq!(
            runs.iter().map(|r| r.len).sum::<usize>(),
            self.text.len(),
            "runs must cover the text"
        );
        self.runs = Some(runs);
        self
    }

    /// Places the text in the window's reading order: a drag selects every
    /// selectable element whose order lies between its two ends. Give each
    /// element a distinct, increasing order.
    pub fn document_order(mut self, order: u64) -> Self {
        self.document_order = order;
        self
    }

    /// Byte ranges whose text must never reach the clipboard (hidden
    /// spoilers): copying a selection that touches one yields [`MASK`] in its
    /// place, once per range, whether the selection is partial or whole.
    pub fn masked_ranges(mut self, ranges: Vec<Range<usize>>) -> Self {
        self.masked = Rc::from(ranges);
        self
    }

    /// Makes byte `ranges` of the text clickable. `f` gets the index of the
    /// clicked range. It fires when the press and the release land in the same
    /// range, and not when the press was a drag that selected text.
    pub fn on_click(
        mut self,
        ranges: Vec<Range<usize>>,
        f: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.click_ranges = Rc::from(ranges);
        self.on_click = Some(Rc::new(f));
        self
    }

    /// A tooltip for the clickable range (by index) under the pointer.
    pub fn tooltip(
        mut self,
        f: impl Fn(usize, &mut Window, &mut App) -> Option<AnyView> + 'static,
    ) -> Self {
        self.tooltip = Some(Rc::new(f));
        self
    }

    /// Restyles the clickable range (by index) under the pointer, for example
    /// to underline a hovered link.
    pub fn hovered_range_style(
        mut self,
        f: impl Fn(usize) -> Option<HighlightStyle> + 'static,
    ) -> Self {
        self.hovered_style = Some(Rc::new(f));
        self
    }
}

/// What a masked range copies as.
pub const MASK: &str = glance_core::markdown::SPOILER_MASK;

/// The text and copy text the copy callback reads.
struct CopySource {
    text: SharedString,
    copy_text: SharedString,
    masked: Rc<[Range<usize>]>,
}

/// Kept between frames for one element.
struct Retained {
    handle: TextSelectionHandle,
    copy: Rc<RefCell<CopySource>>,
    /// The selected range as last painted.
    projected: Rc<RefCell<Option<Range<usize>>>>,
    /// The clickable range under the pointer.
    hovered: Rc<Cell<Option<usize>>>,
    // A drag changes the selection without a frame of its own.
    _refresh: Subscription,
}

pub struct Frame {
    handle: TextSelectionHandle,
    projected: Rc<RefCell<Option<Range<usize>>>>,
    hovered: Rc<Cell<Option<usize>>>,
}

/// What copying yields for a selected `range` of the element.
fn copy_for(source: &CopySource, range: Option<Range<usize>>) -> String {
    match range {
        Some(r) if !(r.start == 0 && r.end >= source.text.len()) => {
            masked_slice(&source.text, r, &source.masked)
        }
        _ => source.copy_text.to_string(),
    }
}

/// `text[range]` with every masked range it touches replaced by [`MASK`].
fn masked_slice(text: &str, range: Range<usize>, masked: &[Range<usize>]) -> String {
    let mut out = String::new();
    let mut at = range.start;
    let mut hidden: Vec<&Range<usize>> = masked
        .iter()
        .filter(|m| m.start < range.end && m.end > range.start && !m.is_empty())
        .collect();
    hidden.sort_by_key(|m| m.start);
    for m in hidden {
        if m.start > at {
            out.push_str(text.get(at..m.start).unwrap_or_default());
        }
        if m.end > at {
            out.push_str(MASK);
            at = m.end;
        }
    }
    if at < range.end {
        out.push_str(text.get(at..range.end).unwrap_or_default());
    }
    out
}

/// Applies `style` to the bytes of `range` in `runs`, splitting runs at the
/// range's ends.
fn highlight_runs(runs: Vec<TextRun>, range: Range<usize>, style: &HighlightStyle) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut start = 0;
    for run in runs {
        let end = start + run.len;
        let lo = range.start.clamp(start, end);
        let hi = range.end.clamp(start, end);
        if lo >= hi {
            out.push(run);
        } else {
            for (a, b, styled) in [(start, lo, false), (lo, hi, true), (hi, end, false)] {
                if a < b {
                    let mut piece = run.clone();
                    piece.len = b - a;
                    if styled {
                        apply_style(&mut piece, style);
                    }
                    out.push(piece);
                }
            }
        }
        start = end;
    }
    out
}

fn apply_style(run: &mut TextRun, style: &HighlightStyle) {
    if let Some(color) = style.color {
        run.color = color;
    }
    if let Some(weight) = style.font_weight {
        run.font.weight = weight;
    }
    if let Some(font_style) = style.font_style {
        run.font.style = font_style;
    }
    if let Some(bg) = style.background_color {
        run.background_color = Some(bg);
    }
    if let Some(underline) = style.underline {
        run.underline = Some(underline);
    }
    if let Some(strike) = style.strikethrough {
        run.strikethrough = Some(strike);
    }
    if let Some(fade) = style.fade_out {
        run.color.fade_out(fade);
    }
}

fn range_at(ranges: &[Range<usize>], ix: usize) -> Option<usize> {
    ranges.iter().position(|r| r.contains(&ix))
}

fn selection_quad_bounds(
    start: Point<Pixels>,
    end: Point<Pixels>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Vec<Bounds<Pixels>> {
    if start.y == end.y {
        return vec![Bounds::from_corners(
            start,
            Point::new(end.x, end.y + line_height),
        )];
    }
    let mut quads = vec![Bounds::from_corners(
        start,
        Point::new(bounds.right(), start.y + line_height),
    )];
    if end.y > start.y + line_height {
        quads.push(Bounds::from_corners(
            Point::new(bounds.left(), start.y + line_height),
            Point::new(bounds.right(), end.y),
        ));
    }
    quads.push(Bounds::from_corners(
        Point::new(bounds.left(), end.y),
        Point::new(end.x, end.y + line_height),
    ));
    quads
}

fn paint_selection(layout: &TextLayout, range: Range<usize>, color: Hsla, window: &mut Window) {
    let (Some(start), Some(end)) = (
        layout.position_for_index(range.start),
        layout.position_for_index(range.end),
    ) else {
        return;
    };
    for bounds in selection_quad_bounds(start, end, layout.bounds(), layout.line_height()) {
        window.paint_quad(fill(bounds, color));
    }
}

impl IntoElement for RichText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RichText {
    type RequestLayoutState = Frame;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Frame) {
        let copy_text: SharedString = if self.masked.is_empty() {
            self.text.clone()
        } else {
            masked_slice(&self.text, 0..self.text.len(), &self.masked).into()
        };
        let masked = self.masked.clone();
        let frame = window.with_element_state(
            global_id.expect("RichText has an element id"),
            |retained: Option<Retained>, window| {
                let retained = retained.unwrap_or_else(|| {
                    let handle = TextSelectionHandle::new(copy_text.to_string(), cx);
                    let copy = Rc::new(RefCell::new(CopySource {
                        text: self.text.clone(),
                        copy_text: copy_text.clone(),
                        masked: masked.clone(),
                    }));
                    let projected = Rc::new(RefCell::new(None));
                    handle.copy_with(
                        {
                            let copy = copy.clone();
                            let projected = projected.clone();
                            move |_| copy_for(&copy.borrow(), projected.borrow().clone())
                        },
                        cx,
                    );
                    let refresh = handle.refresh_window_on_change(window, cx);
                    Retained {
                        handle,
                        copy,
                        projected,
                        hovered: Rc::new(Cell::new(None)),
                        _refresh: refresh,
                    }
                });
                {
                    let mut copy = retained.copy.borrow_mut();
                    copy.masked = masked;
                    if copy.text != self.text || copy.copy_text != copy_text {
                        copy.text = self.text.clone();
                        copy.copy_text = copy_text.clone();
                        retained
                            .handle
                            .set_fallback_copy_text(copy_text.to_string(), cx);
                    }
                }
                let frame = Frame {
                    handle: retained.handle.clone(),
                    projected: retained.projected.clone(),
                    hovered: retained.hovered.clone(),
                };
                (frame, retained)
            },
        );

        let mut runs = self
            .runs
            .take()
            .unwrap_or_else(|| vec![window.text_style().to_run(self.text.len())]);
        if let (Some(style_fn), Some(ix)) = (&self.hovered_style, frame.hovered.get())
            && let Some(range) = self.click_ranges.get(ix)
            && let Some(style) = style_fn(ix)
        {
            runs = highlight_runs(runs, range.clone(), &style);
        }
        let styled = StyledText::new(self.text.clone()).with_runs(runs);
        self.layout = Some(styled.layout().clone());

        let mut inner = InteractiveText::new(self.id.clone(), styled);
        if let Some(on_click) = self.on_click.clone() {
            inner = inner.on_click(self.click_ranges.to_vec(), move |ix, window, cx| {
                // Releasing a drag that selected text across a link only selects.
                if !TextSelection::has_selection(window, cx) {
                    on_click(ix, window, cx);
                }
            });
        }
        if let Some(tooltip) = self.tooltip.clone() {
            let ranges = self.click_ranges.clone();
            inner = inner.tooltip(move |ix, window, cx| {
                range_at(&ranges, ix).and_then(|range_ix| tooltip(range_ix, window, cx))
            });
        }
        let (layout_id, ()) = inner.request_layout(global_id, inspector_id, window, cx);
        self.inner = Some(inner);
        (layout_id, frame)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        let inner = self.inner.as_mut().expect("laid out");
        let hitbox = inner.prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        let registration = TextSelectionRegistration::new(hitbox.clone(), bounds)
            .with_document_order(self.document_order)
            .with_text_bounds(vec![bounds])
            .with_rendered_element(&frame.handle, window, cx);
        frame.handle.register(registration, window, cx);
        hitbox
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        frame: &mut Frame,
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.layout.clone().expect("laid out");
        let projection = frame.handle.update_runs(
            &[
                TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)
                    .with_document_order(self.document_order),
            ],
            cx,
        );
        let selected = projection.ranges().first().cloned().flatten();
        if *frame.projected.borrow() != selected {
            *frame.projected.borrow_mut() = selected.clone();
            window.refresh();
        }
        if let Some(range) = selected.filter(|r| !r.is_empty()) {
            let color = Theme::global(cx).tokens.colors.selection;
            paint_selection(&layout, range, color, window);
        }

        if self.hovered_style.is_some() && !self.click_ranges.is_empty() {
            let hovered = frame.hovered.clone();
            let ranges = self.click_ranges.clone();
            let hitbox = hitbox.clone();
            let layout = layout.clone();
            let view = window.current_view();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let now = if hitbox.is_hovered(window) {
                    layout
                        .index_for_position(event.position)
                        .ok()
                        .and_then(|ix| range_at(&ranges, ix))
                } else {
                    None
                };
                if hovered.get() != now {
                    hovered.set(now);
                    cx.notify(view);
                    window.refresh();
                }
            });
        }

        let inner = self.inner.as_mut().expect("laid out");
        inner.paint(global_id, inspector_id, bounds, &mut (), hitbox, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{FontWeight, HighlightStyle, TextRun, font, red};

    use super::{CopySource, MASK, copy_for, highlight_runs};

    fn run(len: usize) -> TextRun {
        TextRun {
            len,
            font: font("Helvetica"),
            color: red(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    #[test]
    fn hover_style_splits_runs_at_the_range() {
        let bold = HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        let runs = highlight_runs(vec![run(4), run(6)], 2..7, &bold);
        let lens: Vec<_> = runs.iter().map(|r| r.len).collect();
        assert_eq!(lens, vec![2, 2, 3, 3]);
        let weights: Vec<_> = runs.iter().map(|r| r.font.weight).collect();
        assert_eq!(
            weights,
            vec![
                FontWeight::NORMAL,
                FontWeight::BOLD,
                FontWeight::BOLD,
                FontWeight::NORMAL
            ]
        );
    }

    #[test]
    fn copying_the_whole_element_uses_the_copy_text() {
        let src = CopySource {
            text: "see link".into(),
            copy_text: "see [link](https://a.b)".into(),
            masked: Vec::new().into(),
        };
        assert_eq!(copy_for(&src, Some(0..8)), "see [link](https://a.b)");
        assert_eq!(copy_for(&src, None), "see [link](https://a.b)");
        assert_eq!(copy_for(&src, Some(4..8)), "link");
    }

    #[test]
    fn partial_copies_mask_hidden_ranges() {
        // "a secret b secret c" with both "secret"s hidden.
        let src = CopySource {
            text: "a secret b secret c".into(),
            copy_text: "masked".into(),
            masked: vec![2..8, 11..17].into(),
        };
        assert_eq!(copy_for(&src, Some(0..4)), format!("a {MASK}"));
        assert_eq!(copy_for(&src, Some(4..12)), format!("{MASK} b {MASK}"));
        assert_eq!(copy_for(&src, Some(17..19)), " c");
        assert_eq!(copy_for(&src, Some(9..10)), "b");
        assert_eq!(copy_for(&src, None), "masked");
    }
}
