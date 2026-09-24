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
    GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId,
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
    _selection_subscription: gpui::Subscription,
}

pub(super) struct RichTextState {
    handle: TextSelectionHandle,
    pressed_link: Rc<Cell<Option<LinkPress>>>,
    projection: Rc<RefCell<TextSelectionProjection>>,
}

#[derive(Clone, Copy)]
struct LinkPress {
    index: usize,
    origin: Point<Pixels>,
    dragged: bool,
}

/// Holds participants by message identity until their selection ends.
#[derive(Default)]
struct RichTextSelectionRegistry(HashMap<(gpui::WindowId, String), RetainedParticipant>);

impl Global for RichTextSelectionRegistry {}

fn new_retained_selection(text: &SharedString, cx: &mut App) -> RetainedSelection {
    RetainedSelection {
        handle: TextSelectionHandle::new(text.to_string(), cx),
        text: text.clone(),
        pressed_link: Rc::new(Cell::new(None)),
        projection: Rc::new(RefCell::new(TextSelectionProjection::default())),
    }
}

/// Active message identities in a window, used to limit timeline invalidation.
pub(crate) fn active_selection_message_ids(window_id: gpui::WindowId, cx: &App) -> Vec<String> {
    if !cx.has_global::<RichTextSelectionRegistry>() {
        return Vec::new();
    }
    cx.global::<RichTextSelectionRegistry>()
        .0
        .iter()
        .filter(|((id, _), participant)| {
            *id == window_id && participant.handle.snapshot(cx).is_some()
        })
        .map(|((_, message_id), _)| message_id.clone())
        .collect()
}

fn track_selection_handle(
    window_id: gpui::WindowId,
    message_id: &str,
    handle: &TextSelectionHandle,
    cx: &mut App,
) {
    let key = (window_id, message_id.to_owned());
    if handle.snapshot(cx).is_some() {
        if !cx.has_global::<RichTextSelectionRegistry>() {
            cx.set_global(RichTextSelectionRegistry::default());
        }
        let already_retained = cx
            .global::<RichTextSelectionRegistry>()
            .0
            .contains_key(&key);
        if !already_retained {
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
    selection_key: String,
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
            selection_key: selection_key.into(),
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
    for (offset, character) in text[range.clone()].char_indices() {
        let start_ix = range.start + offset;
        let end_ix = start_ix + character.len_utf8();
        let (Some(start), Some(end)) = (
            layout.position_for_index(start_ix),
            layout.position_for_index(end_ix),
        ) else {
            continue;
        };
        if start.y == end.y {
            let (left, right) = if start.x <= end.x {
                (start.x, end.x)
            } else {
                (end.x, start.x)
            };
            if left < right {
                fragments.push(Bounds::from_corners(
                    Point::new(left, start.y),
                    Point::new(right, start.y + line_height),
                ));
            }
        } else {
            // A hard line break has no glyph on the next line. Extend its
            // selection to the visual end of the line, which is the nearer
            // edge for both LTR and RTL paragraphs.
            let line_edge = if start.x - bounds.left() > bounds.right() - start.x {
                bounds.right()
            } else {
                bounds.left()
            };
            let (left, right) = if start.x <= line_edge {
                (start.x, line_edge)
            } else {
                (line_edge, start.x)
            };
            if left < right {
                fragments.push(Bounds::from_corners(
                    Point::new(left, start.y),
                    Point::new(right, start.y + line_height),
                ));
            }
        }
    }

    merge_selection_fragments(fragments)
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
                    None => new_retained_selection(&self.text, cx),
                };
                let state = RichTextState {
                    handle: retained.handle.clone(),
                    pressed_link: Rc::clone(&retained.pressed_link),
                    projection: Rc::clone(&retained.projection),
                };
                (state, retained)
            },
        );
        track_selection_handle(window_id, &self.selection_key, &handle.handle, cx);
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
        if let Ok(index) = layout.index_for_position(mouse_position)
            && links.iter().any(|range| range.contains(&index))
        {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        let targets = self.link_targets.clone();
        let down_state = Rc::clone(&handle.pressed_link);
        let mouse_down_state = Rc::clone(&down_state);
        let down_layout = layout.clone();
        let down_hitbox = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _cx| {
            if phase.bubble() && event.button == MouseButton::Left {
                let press = if down_hitbox.is_hovered(window) {
                    down_layout
                        .index_for_position(event.position)
                        .ok()
                        .map(|index| LinkPress {
                            index,
                            origin: event.position,
                            dragged: false,
                        })
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
            let Ok(index) = up_layout.index_for_position(event.position) else {
                return;
            };
            if TextSelection::has_selection(window, cx) {
                // A click-sized pointer wiggle can create a transient local
                // selection. It should not suppress link activation or leave
                // a tiny selection behind after the click.
                TextSelection::clear(window, cx);
            }
            let Some(link_ix) = links
                .iter()
                .position(|range| range.contains(&press.index) && range.contains(&index))
            else {
                return;
            };
            TextSelection::end(window, cx);
            cx.stop_propagation();
            cx.open_url(&targets[link_ix]);
        });
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px};

    use super::merge_selection_fragments;

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
}
