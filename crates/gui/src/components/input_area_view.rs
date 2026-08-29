//! Isolated input area component with its own Entity and render cycle.
//!
//! This component is designed for performance: when the user types,
//! only this component re-renders, NOT the parent app.

use std::time::Duration;

use wacore::time::Instant;

use gpui::{App, Entity, EventEmitter, Focusable as _, Task, WeakEntity, Window, div, prelude::*};
use gpui_component::{
    ActiveTheme, Disableable as _, IconName, Sizable as _,
    button::{Button, ButtonVariants},
    input::{InputEvent, Textarea, TextareaState},
};

use crate::components::ProductIcon;
use crate::theme::{ActiveProductTheme as _, Metrics};

/// Events emitted by the input area to communicate with the parent app.
#[derive(Clone, Debug)]
pub enum InputAreaEvent {
    /// User wants to send the current message
    SendMessage(String),
    /// User started PTT recording
    StartRecording,
    /// User stopped PTT recording (send the audio)
    StopRecording,
    /// Typing indicator: user started typing
    StartedTyping,
    /// Typing indicator: user stopped typing (timeout)
    StoppedTyping,
    /// User discarded the recording instead of sending it.
    CancelRecording,
    /// User dropped the reply they were composing.
    CancelReply,
}

/// Typing indicator state with debouncing.
#[derive(Default)]
enum TypingState {
    #[default]
    Idle,
    /// Currently typing - stores the instant of the last keystroke
    Composing(Instant),
}

/// Timeout before sending "paused" after typing stops (matches WhatsApp Web)
const TYPING_PAUSED_TIMEOUT: Duration = Duration::from_millis(2500);
/// How often the typing monitor checks for timeout
const TYPING_MONITOR_INTERVAL: Duration = Duration::from_millis(500);

/// The message being replied to, while it is being composed.
#[derive(Clone, Debug)]
pub struct ReplyDraft {
    pub message_id: String,
    pub sender: String,
    pub sender_name: String,
    pub preview: String,
    /// What the message being answered *is*, so the quote sent to the other
    /// side is a photo rather than the word "Photo".
    ///
    /// The preview is a label a human reads; this is what the recipient's
    /// client draws. Dropped here, the kind-aware quote builder in the
    /// session had nothing to be aware of.
    pub kind: Option<oxidezap_core::QuotedKind>,
}

impl From<ReplyDraft> for oxidezap_core::QuotedMessage {
    /// One conversion, so the two send paths cannot disagree about what a
    /// reply quotes. They each built this by hand and both hard-coded the
    /// kind away, which left the kind-aware quote builder in the session
    /// with nothing to work from.
    fn from(draft: ReplyDraft) -> Self {
        Self {
            message_id: draft.message_id,
            sender: draft.sender,
            sender_name: draft.sender_name,
            preview: draft.preview,
            kind: draft.kind,
        }
    }
}

/// How far the field grows before it starts scrolling instead.
///
/// Five lines is the design's number, and it is about where a composer stops
/// being a composer: past that the message wants the whole pane, not a
/// growing strip pushing the conversation off the top.
const COMPOSER_MIN_ROWS: usize = 1;
const COMPOSER_MAX_ROWS: usize = 5;

/// Isolated input area view with its own render cycle.
/// When the user types, only this component re-renders.
pub struct InputAreaView {
    /// The field itself. A textarea rather than a single line: a message is
    /// not always one, and the design asks it to grow with what is typed.
    input: Entity<TextareaState>,
    /// Whether PTT recording is active
    is_recording: bool,
    /// When the current recording started, for the elapsed counter.
    recording_started_at: Option<Instant>,
    /// Most recent input level, 0..=1, for the live meter.
    level: f32,
    /// The message being replied to, if any.
    reply: Option<ReplyDraft>,
    /// The slot height the layout allotted. `None` before the first layout
    /// pass, where the metrics' default is the right answer.
    height: Option<gpui::Pixels>,
    /// Pointer target size for this breakpoint — bigger on touch.
    touch_target: Option<gpui::Pixels>,
    /// Typing indicator state
    typing_state: TypingState,
    /// Task that monitors typing state
    #[allow(dead_code)]
    typing_monitor_task: Option<Task<()>>,
}

impl EventEmitter<InputAreaEvent> for InputAreaView {}

impl InputAreaView {
    /// Create a new input area view
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Enter sends and Shift+Enter breaks the line, which is what
        // `submit_on_enter` means here; the field grows from one line to five
        // and scrolls past that rather than pushing the timeline off screen.
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(COMPOSER_MIN_ROWS, COMPOSER_MAX_ROWS)
                .submit_on_enter(true)
                .placeholder("Type a message")
        });

        // Subscribe to input events (for Enter key to send, etc.)
        cx.subscribe_in(&input, window, Self::handle_input_event)
            .detach();

        Self {
            input,
            is_recording: false,
            recording_started_at: None,
            level: 0.0,
            reply: None,
            height: None,
            touch_target: None,
            typing_state: TypingState::default(),
            typing_monitor_task: None,
        }
    }

    /// Handle input events (Enter, Change, etc.)
    fn handle_input_event(
        &mut self,
        _input: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => {
                self.submit_input(window, cx);
            }
            InputEvent::Change => {
                // Handle typing indicator - minimal work, no notify
                self.on_keystroke(cx);
            }
            _ => {}
        }
    }

    /// Handle a keystroke - updates typing state
    /// Does NOT call cx.notify() to avoid triggering parent re-renders
    fn on_keystroke(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();

        match self.typing_state {
            TypingState::Idle => {
                // First keystroke - start typing
                self.typing_state = TypingState::Composing(now);
                // Emit event to parent (they handle their own notification)
                cx.emit(InputAreaEvent::StartedTyping);
                self.start_typing_monitor(cx);
            }
            TypingState::Composing(_) => {
                // Already typing - just update the timestamp (O(1), no allocations)
                // NO notification needed - just internal state update
                self.typing_state = TypingState::Composing(now);
            }
        }
    }

    /// Start the typing monitor task
    fn start_typing_monitor(&mut self, cx: &mut Context<Self>) {
        self.typing_monitor_task = None;

        self.typing_monitor_task = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            loop {
                crate::platform::sleep(TYPING_MONITOR_INTERVAL).await;

                let should_stop = entity
                    .update(cx, |view, cx| {
                        let TypingState::Composing(last_keystroke) = view.typing_state else {
                            return true;
                        };
                        if last_keystroke.elapsed() >= TYPING_PAUSED_TIMEOUT {
                            view.stop_typing_internal(cx);
                            return true;
                        }
                        false
                    })
                    .unwrap_or(true);

                if should_stop {
                    break;
                }
            }
        }));
    }

    fn stop_typing_internal(&mut self, cx: &mut Context<Self>) {
        if matches!(self.typing_state, TypingState::Composing(_)) {
            cx.emit(InputAreaEvent::StoppedTyping);
        }
        self.typing_state = TypingState::Idle;
        self.typing_monitor_task = None;
    }

    /// Reset typing state without emitting StoppedTyping (called by parent on
    /// chat switch, which routes the paused presence itself); otherwise the
    /// state machine would stay Composing and swallow the first keystroke's
    /// StartedTyping in the newly selected chat.
    /// The composer's focus target.
    ///
    /// Exposed as a handle rather than a `focus()` helper because focusing
    /// needs `&mut App`, which the caller already holds; returning the handle
    /// keeps this borrow read-only and lets the caller decide when to move
    /// focus.
    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    pub fn reset_typing(&mut self) {
        self.typing_state = TypingState::Idle;
        self.typing_monitor_task = None;
    }

    /// Swap the composed text on chat switch: install the target chat's draft
    /// and hand the outgoing chat's draft back to the parent, so unsent text
    /// never rides along to a different recipient.
    pub fn swap_text(
        &mut self,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        let old = self.input.read(cx).text().to_string();
        self.input.update(cx, |state, cx| {
            state.set_value(new_text, window, cx);
        });
        old
    }

    /// Read, trim-check, clear and emit the composed message (Enter and the
    /// send button share this path).
    fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.stop_typing_internal(cx);
        cx.emit(InputAreaEvent::SendMessage(text));
    }

    /// Toggle PTT recording
    fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        if self.is_recording {
            cx.emit(InputAreaEvent::StopRecording);
        } else {
            cx.emit(InputAreaEvent::StartRecording);
        }
    }

    /// Abandon the recording without sending it.
    fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        self.recording_started_at = None;
        self.level = 0.0;
        cx.emit(InputAreaEvent::CancelRecording);
    }

    /// Tell the composer the layout's slot height, so it stops guessing.
    pub fn set_layout(
        &mut self,
        height: gpui::Pixels,
        touch_target: gpui::Pixels,
        cx: &mut Context<Self>,
    ) {
        if self.height != Some(height) || self.touch_target != Some(touch_target) {
            self.height = Some(height);
            self.touch_target = Some(touch_target);
            cx.notify();
        }
    }

    pub fn set_recording(&mut self, is_recording: bool, cx: &mut Context<Self>) {
        if self.is_recording == is_recording {
            return;
        }
        self.is_recording = is_recording;
        self.recording_started_at = is_recording.then(Instant::now);
        if !is_recording {
            self.level = 0.0;
        }
        cx.notify();
    }

    /// The live input level, 0..=1.
    pub fn set_level(&mut self, level: f32, cx: &mut Context<Self>) {
        self.level = level.clamp(0.0, 1.0);
        cx.notify();
    }

    pub fn set_reply(&mut self, reply: Option<ReplyDraft>, cx: &mut Context<Self>) {
        self.reply = reply;
        cx.notify();
    }

    pub fn reply(&self) -> Option<&ReplyDraft> {
        self.reply.as_ref()
    }

    fn clear_reply(&mut self, cx: &mut Context<Self>) {
        self.reply = None;
        cx.emit(InputAreaEvent::CancelReply);
        cx.notify();
    }

    /// `m:ss` since recording started.
    fn recording_elapsed(&self) -> String {
        let secs = self
            .recording_started_at
            .map(|start| start.elapsed().as_secs())
            .unwrap_or(0);
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

impl Render for InputAreaView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_recording = self.is_recording;
        let entity = cx.entity().clone();
        // The composer draws itself into the slot the layout gave it. Owning
        // a height constant of its own is what used to clip it by 6px on
        // mobile, where the layout said 56 and this said 62.
        let metrics = cx.product().metrics;
        let height = self.height.unwrap_or_else(|| metrics.composer_height());
        let control = self.touch_target.unwrap_or_else(|| metrics.icon_button());

        div()
            // A floor, not a ceiling: the layout's slot is what an empty
            // composer occupies, and the field grows the strip from there.
            .min_h(height)
            .flex_shrink_0()
            .flex()
            .flex_col()
            .justify_center()
            .py(metrics.space_sm())
            .px(metrics.space_xl())
            .bg(cx.theme().background)
            .border_t_1()
            .border_color(cx.theme().border)
            .children(
                self.reply
                    .clone()
                    .map(|reply| render_reply_bar(reply, entity.clone(), metrics, cx)),
            )
            .child(if is_recording {
                self.render_recording(control, metrics, cx)
                    .into_any_element()
            } else {
                self.render_composer(control, metrics, cx)
                    .into_any_element()
            })
    }
}

impl InputAreaView {
    /// The ordinary state: attach, field, emoji, and either record or send.
    fn render_composer(
        &self,
        control: gpui::Pixels,
        metrics: Metrics,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity().clone();
        let record_entity = entity.clone();
        // Asked of the rope rather than of a copy of it. `text()` hands back
        // the document itself; `to_string` copied all of it, on every
        // keystroke, to answer whether the send button or the microphone
        // belongs here.
        let has_text = self
            .input
            .read(cx)
            .text()
            .chars()
            .any(|c| !c.is_whitespace());

        div()
            .flex()
            .items_center()
            .gap(metrics.space_md())
            .child(
                // Drawn, disabled, and saying so. The slot is part of what a
                // composer *is* and should not appear the day sending files
                // lands; a control that looks live and does nothing is worse
                // than one that admits it.
                Button::new("attach")
                    .icon(ProductIcon::Paperclip)
                    .ghost()
                    .disabled(true)
                    .tooltip("Attaching files is not available yet")
                    .w(control)
                    .h(control),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Textarea::new(&self.input).w_full()),
            )
            .child(
                Button::new("emoji")
                    .icon(ProductIcon::Smile)
                    .ghost()
                    .disabled(true)
                    .tooltip("The emoji picker is not available yet")
                    .w(control)
                    .h(control),
            )
            // Record or send, never both: which one is available follows
            // whether there is anything to send, the way every messaging
            // client behaves.
            .child(if has_text {
                Button::new("send")
                    .icon(IconName::ArrowRight)
                    .primary()
                    .tooltip("Send")
                    .w(control)
                    .h(control)
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |view, cx| view.submit_input(window, cx));
                    })
                    .into_any_element()
            } else {
                // Drawn disabled where nothing can come of pressing it, with
                // the reason in the tooltip. A control that looks live and
                // does nothing is the worse answer: the browser has no Opus
                // encoder, and that is knowable before the microphone is ever
                // asked for.
                let can_record = oxidezap_audio::CAN_RECORD;
                Button::new("ptt")
                    .icon(ProductIcon::Mic)
                    .ghost()
                    .disabled(!can_record)
                    .tooltip(if can_record {
                        "Hold to record a voice message"
                    } else {
                        "Voice messages cannot be recorded in the browser"
                    })
                    .w(control)
                    .h(control)
                    .when(can_record, |button| {
                        button.on_click(move |_, _window, cx| {
                            record_entity.update(cx, |view, cx| view.toggle_recording(cx));
                        })
                    })
                    .into_any_element()
            })
    }

    /// Recording: elapsed time, a live level, and the two ways out.
    ///
    /// The old UI turned the microphone button red and said nothing else — no
    /// duration, no level, and no way to abandon a recording without sending
    /// it.
    fn render_recording(
        &self,
        control: gpui::Pixels,
        metrics: Metrics,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity().clone();
        let cancel_entity = entity.clone();
        let product = cx.product();

        div()
            .flex()
            .items_center()
            .gap(metrics.space_lg())
            .child(
                Button::new("cancel-recording")
                    .icon(ProductIcon::Trash)
                    .ghost()
                    .tooltip("Discard recording")
                    .w(control)
                    .h(control)
                    .on_click(move |_, _window, cx| {
                        cancel_entity.update(cx, |view, cx| view.cancel_recording(cx));
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(metrics.space_md())
                    .child(
                        div()
                            .size(metrics.space_md())
                            .rounded_full()
                            .bg(cx.theme().danger),
                    )
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(metrics.text_meta())
                            .text_color(cx.theme().foreground)
                            .child(self.recording_elapsed()),
                    ),
            )
            .child(render_level(self.level, metrics, cx))
            .child(
                div()
                    .text_size(metrics.text_small())
                    .text_color(product.hsla(product.palette.subtle_foreground))
                    .child("Recording"),
            )
            .child(
                Button::new("send-recording")
                    .icon(IconName::ArrowRight)
                    .primary()
                    .tooltip("Send voice message")
                    .w(control)
                    .h(control)
                    .on_click(move |_, _window, cx| {
                        entity.update(cx, |view, cx| view.toggle_recording(cx));
                    }),
            )
    }
}

/// The live input level while recording.
fn render_level(level: f32, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    const BARS: usize = 16;
    let full = metrics.waveform_height();
    let level = level.clamp(0.0, 1.0);

    div()
        .flex_1()
        .h(full)
        .flex()
        .items_center()
        .gap(metrics.waveform_bar_gap())
        .children((0..BARS).map(move |ix| {
            // A fixed envelope scaled by the live level, so the bars move with
            // the voice without flickering randomly between frames.
            let position = (ix as f32 + 0.5) / BARS as f32;
            let envelope = 0.35 + 0.65 * (position * std::f32::consts::PI).sin();
            div()
                .w(metrics.waveform_bar_width())
                .h((full * envelope * level).max(metrics.bar_thin()))
                .rounded_full()
                .bg(cx.theme().primary)
        }))
}

/// The message being replied to, above the field, with a way to drop it.
fn render_reply_bar(
    reply: ReplyDraft,
    entity: Entity<InputAreaView>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let hue = cx.product().speaker(&reply.sender);

    div()
        .flex()
        .items_center()
        .gap(metrics.space_md())
        .pb(metrics.space_md())
        .child(
            div()
                .w(metrics.selection_bar_width())
                .h(metrics.avatar_inline())
                .rounded_full()
                .flex_shrink_0()
                .bg(hue),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(hue)
                        .child(format!("Replying to {}", reply.sender_name)),
                )
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().muted_foreground)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(reply.preview),
                ),
        )
        .child(
            Button::new("cancel-reply")
                .icon(IconName::Close)
                .ghost()
                .xsmall()
                .tooltip("Cancel reply")
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |view, cx| view.clear_reply(cx));
                }),
        )
}
