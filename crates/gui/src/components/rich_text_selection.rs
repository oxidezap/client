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

/// Keeps active participants reachable when virtualization drops their row.
#[derive(Default)]
struct RichTextSelectionRegistry(HashMap<(gpui::WindowId, String), TextSelectionHandle>);

impl Global for RichTextSelectionRegistry {}

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
        cx.global_mut::<RichTextSelectionRegistry>()
            .0
            .insert(key, handle.clone());
    } else if cx.has_global::<RichTextSelectionRegistry>() {
        cx.global_mut::<RichTextSelectionRegistry>().0.remove(&key);
    }
}

/// Clears a window's selection if it includes a message whose text element is
/// about to disappear from the virtual timeline.
pub(crate) fn clear_if_selected_message(
    message_id: &str,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let key = (window.window_handle().window_id(), message_id.to_owned());
    let selected = cx.has_global::<RichTextSelectionRegistry>()
        && cx
            .global::<RichTextSelectionRegistry>()
            .0
            .get(&key)
            .is_some_and(|handle| handle.snapshot(cx).is_some());
    if selected {
        TextSelection::clear(window, cx);
        forget_window_selection_registry(key.0, cx);
    }
    selected
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
        range: Range<usize>,
        color: gpui::Hsla,
        window: &mut Window,
    ) {
        let (Some(start), Some(end)) = (
            layout.position_for_index(range.start),
            layout.position_for_index(range.end),
        ) else {
            return;
        };
        for bounds in selection_quad_bounds(start, end, layout.bounds(), layout.line_height()) {
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
                            forget_window_selection_registry(
                                window.window_handle().window_id(),
                                cx,
                            );
                        }
                        RetainedSelection {
                            handle: TextSelectionHandle::new(self.text.to_string(), cx),
                            text: self.text.clone(),
                            pressed_link: Rc::new(Cell::new(None)),
                            projection: Rc::new(RefCell::new(TextSelectionProjection::default())),
                        }
                    }
                    None => RetainedSelection {
                        handle: TextSelectionHandle::new(self.text.to_string(), cx),
                        text: self.text.clone(),
                        pressed_link: Rc::new(Cell::new(None)),
                        projection: Rc::new(RefCell::new(TextSelectionProjection::default())),
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
            window.window_handle().window_id(),
            &self.selection_key,
            &handle.handle,
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
            Self::paint_selection(&layout, range, selection_color, window);
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
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, _cx| {
            if !phase.bubble() || event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            if let Some(mut press) = move_state.get()
                && event.position != press.origin
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
                return;
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
    use gpui::{Bounds, point, px, size};

    use super::selection_quad_bounds;

    #[test]
    fn selection_highlight_spans_wrapped_middle_lines() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(100.)));
        let quads = selection_quad_bounds(
            point(px(40.), px(20.)),
            point(px(30.), px(80.)),
            bounds,
            px(20.),
        );

        assert_eq!(
            quads,
            vec![
                Bounds::from_corners(point(px(40.), px(20.)), point(px(110.), px(40.))),
                Bounds::from_corners(point(px(10.), px(40.)), point(px(110.), px(80.))),
                Bounds::from_corners(point(px(10.), px(80.)), point(px(30.), px(100.))),
            ]
        );
    }
}
