//! GPUI's selectable-text bridge for formatted message bubbles.
//!
//! `SelectableText` handles a plain string, but it cannot carry WhatsApp's
//! inline styles or clickable link ranges. This element keeps one `StyledText`
//! layout and registers that same visible text with GPUI Kit's existing
//! window selection model.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, BorderStyle, Bounds, Corners, CursorStyle, Edges, Element, ElementId, Global,
    GlobalElementId, Half, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    SharedString, StyledText, Window, transparent_black,
};
use gpui_base::{
    TextSelection, TextSelectionHandle, TextSelectionProjection, TextSelectionRegistration,
    TextSelectionRun,
};
use gpui_component::ActiveTheme as _;

struct RetainedSelection {
    handle: TextSelectionHandle,
    text: SharedString,
    pressed_link: Rc<Cell<Option<LinkPress>>>,
    projection: Rc<RefCell<TextSelectionProjection>>,
}

struct RetainedParticipant {
    handle: TextSelectionHandle,
    text: SharedString,
    document_order: u64,
    _selection_subscription: gpui::Subscription,
}

pub(super) struct RichTextState {
    handle: TextSelectionHandle,
    pressed_link: Rc<Cell<Option<LinkPress>>>,
    projection: Rc<RefCell<TextSelectionProjection>>,
}

#[derive(Clone, Copy)]
struct LinkPress {
    link_ix: usize,
    origin: Point<Pixels>,
    dragged: bool,
}

/// Holds participants by message identity until their selection ends.
#[derive(Default)]
struct RichTextSelectionRegistry(HashMap<(gpui::WindowId, Arc<str>), RetainedParticipant>);

impl Global for RichTextSelectionRegistry {}

fn new_retained_selection(text: &SharedString, cx: &mut App) -> RetainedSelection {
    retained_selection(TextSelectionHandle::new(text.to_string(), cx), text)
}

fn retained_selection(handle: TextSelectionHandle, text: &SharedString) -> RetainedSelection {
    RetainedSelection {
        handle,
        text: text.clone(),
        pressed_link: Rc::new(Cell::new(None)),
        projection: Rc::new(RefCell::new(TextSelectionProjection::default())),
    }
}

/// Active message identities and their retained text and timeline order.
pub(crate) fn active_selection_message_snapshots(
    window_id: gpui::WindowId,
    cx: &App,
) -> Vec<(String, SharedString, u64)> {
    if !cx.has_global::<RichTextSelectionRegistry>() {
        return Vec::new();
    }
    cx.global::<RichTextSelectionRegistry>()
        .0
        .iter()
        .filter(|((id, _), participant)| {
            *id == window_id && participant.handle.snapshot(cx).is_some()
        })
        .map(|((_, message_id), participant)| {
            (
                message_id.to_string(),
                participant.text.clone(),
                participant.document_order,
            )
        })
        .collect()
}

fn track_selection_handle(
    window_id: gpui::WindowId,
    message_id: &Arc<str>,
    handle: &TextSelectionHandle,
    text: &SharedString,
    document_order: u64,
    cx: &mut App,
) {
    let key = (window_id, Arc::clone(message_id));
    if handle.snapshot(cx).is_some() {
        if !cx.has_global::<RichTextSelectionRegistry>() {
            cx.set_global(RichTextSelectionRegistry::default());
        }
        let already_retained = cx
            .global::<RichTextSelectionRegistry>()
            .0
            .get(&key)
            .is_some_and(|participant| {
                participant.handle.entity_id() == handle.entity_id() && participant.text == *text
            });
        if already_retained {
            if let Some(participant) = cx.global_mut::<RichTextSelectionRegistry>().0.get_mut(&key)
            {
                participant.document_order = document_order;
            }
        } else {
            let registry_key = key.clone();
            let subscription = handle.subscribe(
                move |event, cx| {
                    if matches!(
                        event,
                        gpui_base::TextSelectionEvent::Cleared
                            | gpui_base::TextSelectionEvent::SelectionChanged(None)
                    ) {
                        let registry_key = registry_key.clone();
                        cx.defer(move |cx| {
                            let inactive = cx.has_global::<RichTextSelectionRegistry>()
                                && cx
                                    .global::<RichTextSelectionRegistry>()
                                    .0
                                    .get(&registry_key)
                                    .is_some_and(|participant| {
                                        participant.handle.snapshot(cx).is_none()
                                    });
                            if inactive {
                                cx.global_mut::<RichTextSelectionRegistry>()
                                    .0
                                    .remove(&registry_key);
                            }
                        });
                    }
                },
                cx,
            );
            cx.global_mut::<RichTextSelectionRegistry>().0.insert(
                key,
                RetainedParticipant {
                    handle: handle.clone(),
                    text: text.clone(),
                    document_order,
                    _selection_subscription: subscription,
                },
            );
        }
    } else if cx.has_global::<RichTextSelectionRegistry>() {
        cx.global_mut::<RichTextSelectionRegistry>().0.remove(&key);
    }
}

/// Releases retained participant handles after a window-level clear.
pub(crate) fn forget_window_selection_registry(window_id: gpui::WindowId, cx: &mut App) {
    if cx.has_global::<RichTextSelectionRegistry>() {
        cx.global_mut::<RichTextSelectionRegistry>()
            .0
            .retain(|(id, _), _| *id != window_id);
    }
}

/// One formatted message text run participating in GPUI's window selection.
pub(super) struct SelectableRichText {
    id: ElementId,
    text: SharedString,
    styled_text: StyledText,
    links: Vec<Range<usize>>,
    link_targets: Arc<[SharedString]>,
    selection_key: Arc<str>,
    document_order: u64,
}

impl SelectableRichText {
    pub(super) fn new(
        id: impl Into<ElementId>,
        text: SharedString,
        styled_text: StyledText,
        links: Vec<Range<usize>>,
        link_targets: Arc<[SharedString]>,
        selection_key: impl Into<String>,
        document_order: u64,
    ) -> Self {
        Self {
            id: id.into(),
            text,
            styled_text,
            links,
            link_targets,
            selection_key: Arc::from(selection_key.into()),
            document_order,
        }
    }

    fn paint_selection(
        layout: &gpui::TextLayout,
        text: &str,
        range: Range<usize>,
        color: gpui::Hsla,
        window: &mut Window,
    ) {
        for bounds in selection_quad_bounds(text, range, layout) {
            window.paint_quad(PaintQuad {
                bounds,
                background: color.into(),
                corner_radii: Corners::default(),
                border_widths: Edges::default(),
                border_color: transparent_black(),
                border_style: BorderStyle::default(),
            });
        }
    }
}

fn selection_quad_bounds(
    text: &str,
    range: Range<usize>,
    layout: &gpui::TextLayout,
) -> Vec<Bounds<Pixels>> {
    let bounds = layout.bounds();
    let line_height = layout.line_height();
    let mut fragments = Vec::new();
    let mut line_start_ix = 0;
    let mut line_top = bounds.top();

    for line in layout.line_layouts() {
        let runs = line.runs();
        let wrap_boundaries = line.wrap_boundaries();
        let mut source_boundaries: Vec<usize> = runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|glyph| line_start_ix + glyph.index))
            .collect();
        source_boundaries.sort_unstable();
        source_boundaries.dedup();
        let cluster_ends: HashMap<usize, usize> = source_boundaries
            .windows(2)
            .map(|pair| (pair[0], pair[1]))
            .collect();
        // Preserve the first shaped caret position per source boundary; all
        // positioned glyph extrema remain represented by `segments`.
        let mut shaped_x_by_index = HashMap::new();
        for run in runs {
            for glyph in &run.glyphs {
                shaped_x_by_index
                    .entry(line_start_ix + glyph.index)
                    .or_insert(glyph.position.x);
            }
        }
        let mut segments = vec![Vec::new(); wrap_boundaries.len() + 1];
        let mut segment_ix = 0;
        let mut next_boundary_ix = 0;
        for (run_ix, run) in runs.iter().enumerate() {
            for (glyph_ix, glyph) in run.glyphs.iter().enumerate() {
                if wrap_boundaries
                    .get(next_boundary_ix)
                    .is_some_and(|boundary| {
                        boundary.run_ix == run_ix && boundary.glyph_ix == glyph_ix
                    })
                {
                    segment_ix += 1;
                    next_boundary_ix += 1;
                }
                segments[segment_ix].push((
                    line_start_ix + glyph.index,
                    glyph.position.x,
                    run.font_id,
                ));
            }
        }

        for (segment_ix, glyphs) in segments.iter().enumerate() {
            let Some((_, first_x, _)) = glyphs.first() else {
                continue;
            };
            let mut visual_left = *first_x;
            let mut visual_right = *first_x;
            for (_, x, _) in glyphs.iter().skip(1) {
                if *x < visual_left {
                    visual_left = *x;
                }
                if *x > visual_right {
                    visual_right = *x;
                }
            }

            let segment_top = line_top + line_height * segment_ix as f32;
            let trailing_source_ix = glyphs
                .iter()
                .filter(|(_, x, _)| *x == visual_right)
                .map(|(source_ix, _, _)| *source_ix)
                .max();
            let trailing_advance = trailing_source_ix
                .and_then(|source_ix| {
                    let start_x = shaped_x_by_index.get(&source_ix)?;
                    let cluster_end_ix = cluster_ends
                        .get(&source_ix)
                        .copied()
                        .unwrap_or(line_start_ix + line.len());
                    let end_x = shaped_x_by_index
                        .get(&cluster_end_ix)
                        .copied()
                        .unwrap_or(line.unwrapped_layout.width);
                    Some(trailing_cluster_extension(visual_right, *start_x, end_x))
                })
                .unwrap_or(line_height.half());
            let visual_glyphs: Vec<_> = glyphs
                .iter()
                .map(|(source_ix, x, _)| {
                    (
                        *source_ix,
                        *x - visual_left,
                        if *x == visual_right && Some(*source_ix) == trailing_source_ix {
                            trailing_advance
                        } else {
                            Pixels::ZERO
                        },
                    )
                })
                .collect();

            let segment_width = visual_right - visual_left + trailing_advance;
            if segment_width <= Pixels::ZERO {
                continue;
            }
            let segment_bounds = Bounds::from_corners(
                Point::new(bounds.left(), segment_top),
                Point::new(bounds.left() + segment_width, segment_top + line_height),
            );
            fragments.extend(selection_glyph_bounds(
                &visual_glyphs,
                &range,
                segment_bounds,
            ));
        }

        line_top += line.size(line_height).height;
        line_start_ix += line.len() + 1;
    }

    for (offset, character) in text[range.clone()].char_indices() {
        if character != '\n' {
            continue;
        }
        let start_ix = range.start + offset;
        let Some(start) = layout.position_for_index(start_ix) else {
            continue;
        };
        let line_start_ix = text[..start_ix]
            .rfind('\n')
            .map_or(0, |previous_newline| previous_newline + 1);
        let Some(line_start) = layout.position_for_index(line_start_ix) else {
            continue;
        };
        if let Some(fragment) = hard_break_selection_bounds(line_start, start, bounds, line_height)
        {
            fragments.push(fragment);
        }
    }

    merge_selection_fragments(fragments)
}

fn selection_glyph_bounds(
    glyphs: &[(usize, Pixels, Pixels)],
    range: &Range<usize>,
    segment_bounds: Bounds<Pixels>,
) -> Vec<Bounds<Pixels>> {
    let mut visual_glyphs = glyphs.to_vec();
    visual_glyphs.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut right_edges = vec![Pixels::ZERO; visual_glyphs.len()];
    let mut group_start = 0;
    while group_start < visual_glyphs.len() {
        let x = visual_glyphs[group_start].1;
        let mut group_end = group_start + 1;
        let mut group_advance = visual_glyphs[group_start].2;
        while group_end < visual_glyphs.len() && visual_glyphs[group_end].1 == x {
            if visual_glyphs[group_end].2 > group_advance {
                group_advance = visual_glyphs[group_end].2;
            }
            group_end += 1;
        }
        let right = if group_end < visual_glyphs.len() {
            visual_glyphs[group_end].1
        } else {
            x + group_advance
        };
        right_edges[group_start..group_end].fill(right);
        group_start = group_end;
    }

    let mut fragments = Vec::new();
    for ((source_ix, x, _), right) in visual_glyphs.iter().zip(right_edges) {
        if range.contains(source_ix) && right > *x {
            fragments.push(Bounds::from_corners(
                Point::new(segment_bounds.left() + *x, segment_bounds.top()),
                Point::new(segment_bounds.left() + right, segment_bounds.bottom()),
            ));
        }
    }
    fragments
}

fn trailing_cluster_extension(
    visual_right: Pixels,
    source_start: Pixels,
    source_end: Pixels,
) -> Pixels {
    let caret_right = if source_end > source_start {
        source_end
    } else {
        source_start
    };
    if caret_right > visual_right {
        caret_right - visual_right
    } else {
        Pixels::ZERO
    }
}

fn link_at_position(
    layout: &gpui::TextLayout,
    text: &str,
    links: &[Range<usize>],
    position: Point<Pixels>,
) -> Option<usize> {
    let index = match layout.index_for_position(position) {
        Ok(index) | Err(index) => index,
    };
    link_index_at_position(index, links, |range| {
        selection_quad_bounds(text, range.clone(), layout)
            .iter()
            .any(|bounds| {
                position.x >= bounds.left()
                    && position.x <= bounds.right()
                    && position.y >= bounds.top()
                    && position.y <= bounds.bottom()
            })
    })
}

fn link_index_at_position(
    index: usize,
    links: &[Range<usize>],
    end_range_contains_pointer: impl Fn(&Range<usize>) -> bool,
) -> Option<usize> {
    if let Some(link_ix) = links.iter().position(|range| {
        range.end == index && !range.is_empty() && end_range_contains_pointer(range)
    }) {
        return Some(link_ix);
    }
    links.iter().position(|range| range.contains(&index))
}

fn hard_break_selection_bounds(
    line_start: Point<Pixels>,
    line_end: Point<Pixels>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Option<Bounds<Pixels>> {
    let trailing_edge = if line_end.x >= line_start.x {
        bounds.right()
    } else {
        bounds.left()
    };
    let (left, right) = if line_end.x <= trailing_edge {
        (line_end.x, trailing_edge)
    } else {
        (trailing_edge, line_end.x)
    };
    (left < right).then(|| {
        Bounds::from_corners(
            Point::new(left, line_end.y),
            Point::new(right, line_end.y + line_height),
        )
    })
}

fn merge_selection_fragments(mut fragments: Vec<Bounds<Pixels>>) -> Vec<Bounds<Pixels>> {
    fragments.sort_by(|left, right| {
        left.origin
            .y
            .partial_cmp(&right.origin.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                left.origin
                    .x
                    .partial_cmp(&right.origin.x)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    let mut merged: Vec<Bounds<Pixels>> = Vec::with_capacity(fragments.len());
    for fragment in fragments {
        if let Some(previous) = merged.last_mut()
            && previous.origin.y == fragment.origin.y
            && fragment.origin.x <= previous.right()
        {
            if fragment.right() > previous.right() {
                previous.size.width = fragment.right() - previous.origin.x;
            }
        } else {
            merged.push(fragment);
        }
    }
    merged
}

impl IntoElement for SelectableRichText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableRichText {
    type RequestLayoutState = RichTextState;
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
    ) -> (LayoutId, Self::RequestLayoutState) {
        let window_id = window.window_handle().window_id();
        let registry_key = (window_id, Arc::clone(&self.selection_key));
        let registry_participant = cx
            .has_global::<RichTextSelectionRegistry>()
            .then(|| {
                cx.global::<RichTextSelectionRegistry>()
                    .0
                    .get(&registry_key)
                    .map(|participant| (participant.handle.clone(), participant.text.clone()))
            })
            .flatten();
        let registry_handle = match registry_participant {
            Some((handle, text)) if text == self.text && handle.snapshot(cx).is_some() => {
                Some(handle)
            }
            Some((handle, text)) if text != self.text && handle.snapshot(cx).is_some() => {
                // The bubble changed while virtualized, so its retained
                // participant must not keep exporting the previous text.
                TextSelection::clear(window, cx);
                forget_window_selection_registry(window_id, cx);
                None
            }
            _ => None,
        };
        let handle = window.with_element_state(
            global_id.expect("SelectableRichText must have a stable element id"),
            |retained: Option<RetainedSelection>, window| {
                let retained = match retained {
                    Some(retained) if retained.text == self.text => retained,
                    Some(retained) => {
                        // Only this participant's selection became stale;
                        // edits to another bubble must not clear it.
                        if retained.handle.snapshot(cx).is_some() {
                            TextSelection::clear(window, cx);
                            forget_window_selection_registry(window_id, cx);
                        }
                        new_retained_selection(&self.text, cx)
                    }
                    None => match registry_handle.clone() {
                        Some(handle) => retained_selection(handle, &self.text),
                        None => new_retained_selection(&self.text, cx),
                    },
                };
                let state = RichTextState {
                    handle: retained.handle.clone(),
                    pressed_link: Rc::clone(&retained.pressed_link),
                    projection: Rc::clone(&retained.projection),
                };
                (state, retained)
            },
        );
        track_selection_handle(
            window_id,
            &self.selection_key,
            &handle.handle,
            &self.text,
            self.document_order,
            cx,
        );
        let (layout_id, ()) = self
            .styled_text
            .request_layout(global_id, inspector_id, window, cx);
        (layout_id, handle)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled_text
            .prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        handle.handle.register(
            TextSelectionRegistration::new(hitbox.clone(), bounds)
                .with_document_order(self.document_order)
                .with_text_bounds(vec![bounds]),
            window,
            cx,
        );
        hitbox
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled_text.layout().clone();
        let projection = handle.handle.update_runs(
            &[
                TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)
                    .with_document_order(self.document_order),
            ],
            cx,
        );
        {
            let mut previous_projection = handle.projection.borrow_mut();
            if *previous_projection != projection {
                *previous_projection = projection.clone();
                window.refresh();
            }
        }
        let selection_color = cx.theme().selection;
        for range in projection.ranges().iter().flatten().cloned() {
            Self::paint_selection(&layout, &self.text, range, selection_color, window);
        }
        self.styled_text.paint(
            global_id,
            inspector_id,
            bounds,
            &mut (),
            &mut (),
            window,
            cx,
        );

        if self.links.is_empty() {
            return;
        }
        let links = self.links.clone();
        let mouse_position = window.mouse_position();
        if link_at_position(&layout, &self.text, &links, mouse_position).is_some() {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        let targets = self.link_targets.clone();
        let link_text = self.text.clone();
        let down_state = Rc::clone(&handle.pressed_link);
        let mouse_down_state = Rc::clone(&down_state);
        let down_layout = layout.clone();
        let down_hitbox = hitbox.clone();
        let down_links = links.clone();
        let down_text = link_text.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _cx| {
            if phase.bubble() && event.button == MouseButton::Left {
                let press = if down_hitbox.is_hovered(window) {
                    link_at_position(&down_layout, &down_text, &down_links, event.position).map(
                        |link_ix| LinkPress {
                            link_ix,
                            origin: event.position,
                            dragged: false,
                        },
                    )
                } else {
                    None
                };
                mouse_down_state.set(press);
                if press.is_some() {
                    window.refresh();
                }
            }
        });
        let move_state = Rc::clone(&down_state);
        let drag_slop = window.rem_size() * 0.2;
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, _cx| {
            if !phase.bubble() || event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            if let Some(mut press) = move_state.get()
                && ((event.position.x - press.origin.x).abs() > drag_slop
                    || (event.position.y - press.origin.y).abs() > drag_slop)
            {
                press.dragged = true;
                move_state.set(Some(press));
            }
        });
        let up_layout = layout;
        let up_text = link_text;
        let up_hitbox = hitbox.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if !phase.bubble() || event.button != MouseButton::Left {
                return;
            }
            let Some(press) = down_state.replace(None) else {
                return;
            };
            if press.dragged || !up_hitbox.is_hovered(window) {
                return;
            }
            let Some(link_ix) = link_at_position(&up_layout, &up_text, &links, event.position)
            else {
                return;
            };
            if link_ix != press.link_ix {
                return;
            }
            if TextSelection::has_selection(window, cx) {
                // A click-sized pointer wiggle can create a transient local
                // selection. It should not suppress link activation or leave
                // a tiny selection behind after the click.
                TextSelection::clear(window, cx);
            }
            TextSelection::end(window, cx);
            cx.stop_propagation();
            cx.open_url(&targets[link_ix]);
        });
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px};

    use super::{
        hard_break_selection_bounds, link_index_at_position, merge_selection_fragments,
        selection_glyph_bounds, trailing_cluster_extension,
    };

    #[test]
    fn bidi_selection_fragments_merge_in_visual_order_without_bridging_runs() {
        let fragment = |left, top, right, bottom| {
            Bounds::from_corners(point(px(left), px(top)), point(px(right), px(bottom)))
        };
        let quads = merge_selection_fragments(vec![
            fragment(80., 20., 100., 40.),
            fragment(30., 20., 45., 40.),
            fragment(10., 20., 30., 40.),
            fragment(10., 40., 30., 60.),
        ]);

        assert_eq!(
            quads,
            vec![
                fragment(10., 20., 45., 40.),
                fragment(80., 20., 100., 40.),
                fragment(10., 40., 30., 60.),
            ]
        );
    }

    #[test]
    fn mixed_direction_selection_uses_glyph_clusters_at_run_boundaries() {
        let glyphs = [
            (0, px(0.), px(0.)),
            (1, px(10.), px(0.)),
            (2, px(20.), px(0.)),
            (3, px(30.), px(0.)),
            (4, px(60.), px(10.)),
            (6, px(50.), px(0.)),
            (8, px(40.), px(0.)),
        ];
        let line = Bounds::from_corners(point(px(0.), px(20.)), point(px(70.), px(40.)));
        let fragments = merge_selection_fragments(selection_glyph_bounds(&glyphs, &(3..6), line));

        assert_eq!(
            fragments,
            vec![
                Bounds::from_corners(point(px(30.), px(20.)), point(px(40.), px(40.))),
                Bounds::from_corners(point(px(60.), px(20.)), point(px(70.), px(40.))),
            ]
        );
    }

    #[test]
    fn trailing_cluster_extension_keeps_all_positioned_glyphs() {
        assert_eq!(trailing_cluster_extension(px(10.), px(0.), px(15.)), px(5.));
    }

    #[test]
    fn exclusive_link_end_requires_a_hit_on_the_final_glyph() {
        let links = [0..4, 4..8];
        assert_eq!(link_index_at_position(4, &links, |_| false), Some(1));
        assert_eq!(link_index_at_position(8, &links, |_| false), None);
        assert_eq!(link_index_at_position(8, &links, |_| true), Some(1));
    }

    #[test]
    fn short_ltr_newline_highlight_extends_to_the_trailing_edge() {
        let bounds = Bounds::from_corners(point(px(0.), px(0.)), point(px(100.), px(20.)));
        assert_eq!(
            hard_break_selection_bounds(
                point(px(0.), px(0.)),
                point(px(10.), px(0.)),
                bounds,
                px(20.)
            ),
            Some(Bounds::from_corners(
                point(px(10.), px(0.)),
                point(px(100.), px(20.)),
            ))
        );
    }

    #[test]
    fn equal_position_glyphs_share_a_precomputed_visual_edge() {
        let glyphs = [
            (3, px(0.), px(0.)),
            (4, px(0.), px(10.)),
            (5, px(0.), px(0.)),
            (6, px(10.), px(0.)),
        ];
        let line = Bounds::from_corners(point(px(0.), px(20.)), point(px(20.), px(40.)));
        let fragments = merge_selection_fragments(selection_glyph_bounds(&glyphs, &(3..6), line));

        assert_eq!(
            fragments,
            vec![Bounds::from_corners(
                point(px(0.), px(20.)),
                point(px(10.), px(40.)),
            )]
        );
    }
}
