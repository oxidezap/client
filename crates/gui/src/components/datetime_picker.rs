//! Generic calendar + time picker widget for [`oxidezap_core::PluginWidget::DateTime`].
//!
//! Draws a localized month grid, hour/minute steppers, and quick presets using
//! theme tokens and rem-based sizing.

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};
use gpui::{
    App, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder as _, rems,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, IconName, Sizable as _};

use crate::theme::Metrics;

/// Current local time derived from the shared `wacore::time` clock.
fn now_local() -> chrono::DateTime<Local> {
    chrono::DateTime::from_timestamp_millis(wacore::time::now_utc().as_millis())
        .unwrap_or_default()
        .with_timezone(&Local)
}

/// Interactive state for a single `PluginWidget::DateTime` control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DateTimeState {
    pub selected_date: NaiveDate,
    pub view_year: i32,
    pub view_month: u32,
    pub hour: u32,
    pub minute: u32,
}

impl DateTimeState {
    /// Parse an RFC 3339 timestamp into local calendar state, or default to
    /// 15 minutes from now when `value` is empty or unparseable.
    pub fn from_rfc3339_or_default(value: &str) -> Self {
        let dt = (!value.trim().is_empty())
            .then(|| {
                chrono::DateTime::parse_from_rfc3339(value.trim())
                    .ok()
                    .map(|parsed| parsed.with_timezone(&Local))
            })
            .flatten()
            .unwrap_or_else(|| now_local() + Duration::minutes(15));
        let selected_date = dt.date_naive();
        Self {
            selected_date,
            view_year: selected_date.year(),
            view_month: selected_date.month(),
            hour: dt.hour(),
            minute: dt.minute(),
        }
    }

    /// Format the currently selected date and time as RFC 3339 with local offset.
    pub fn to_rfc3339(&self) -> Option<String> {
        let time = NaiveTime::from_hms_opt(self.hour, self.minute, 0)?;
        let naive = NaiveDateTime::new(self.selected_date, time);
        let dt = Local.from_local_datetime(&naive).earliest()?;
        Some(dt.to_rfc3339())
    }

    /// Format the selection only if it lies strictly in the future.
    pub fn future_rfc3339(&self) -> Option<String> {
        let time = NaiveTime::from_hms_opt(self.hour, self.minute, 0)?;
        let naive = NaiveDateTime::new(self.selected_date, time);
        let dt = Local.from_local_datetime(&naive).earliest()?;
        (dt > now_local()).then(|| dt.to_rfc3339())
    }

    /// Human-readable summary of the selected date and time.
    pub fn formatted_selection(&self) -> String {
        let Some(time) = NaiveTime::from_hms_opt(self.hour, self.minute, 0) else {
            return String::new();
        };
        let naive = NaiveDateTime::new(self.selected_date, time);
        naive.format("%Y-%m-%d %H:%M").to_string()
    }

    /// Apply a user action to this state.
    pub fn apply(&mut self, update: DateTimeUpdate) {
        match update {
            DateTimeUpdate::PrevMonth => {
                if self.view_month == 1 {
                    self.view_month = 12;
                    self.view_year -= 1;
                } else {
                    self.view_month -= 1;
                }
            }
            DateTimeUpdate::NextMonth => {
                if self.view_month == 12 {
                    self.view_month = 1;
                    self.view_year += 1;
                } else {
                    self.view_month += 1;
                }
            }
            DateTimeUpdate::SelectDate(date) => {
                self.selected_date = date;
                self.view_year = date.year();
                self.view_month = date.month();
            }
            DateTimeUpdate::AdjustHour(delta) => {
                self.hour = (self.hour as i32 + delta).rem_euclid(24) as u32;
            }
            DateTimeUpdate::AdjustMinute(delta) => {
                let total = self.hour as i32 * 60 + self.minute as i32 + delta;
                let wrapped = total.rem_euclid(24 * 60);
                self.hour = (wrapped / 60) as u32;
                self.minute = (wrapped % 60) as u32;
            }
            DateTimeUpdate::PresetMinutes(minutes) => {
                let target = now_local() + Duration::minutes(minutes);
                self.selected_date = target.date_naive();
                self.view_year = self.selected_date.year();
                self.view_month = self.selected_date.month();
                self.hour = target.hour();
                self.minute = target.minute();
            }
            DateTimeUpdate::PresetTomorrowMorning => {
                let tomorrow = now_local().date_naive() + Duration::days(1);
                self.selected_date = tomorrow;
                self.view_year = tomorrow.year();
                self.view_month = tomorrow.month();
                self.hour = 9;
                self.minute = 0;
            }
        }
    }
}

/// User interaction on a [`DateTimeState`] picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateTimeUpdate {
    PrevMonth,
    NextMonth,
    SelectDate(NaiveDate),
    AdjustHour(i32),
    AdjustMinute(i32),
    PresetMinutes(i64),
    PresetTomorrowMorning,
}

/// Days to display in a 6x7 calendar grid starting on Monday.
pub fn month_grid_days(year: i32, month: u32) -> Vec<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid first of month");
    let offset = first.weekday().num_days_from_monday() as i64;
    let start = first - Duration::days(offset);
    (0..42).map(|i| start + Duration::days(i)).collect()
}

fn month_label(month: u32) -> &'static str {
    match month {
        1 => crate::tr!(January),
        2 => crate::tr!(February),
        3 => crate::tr!(March),
        4 => crate::tr!(April),
        5 => crate::tr!(May),
        6 => crate::tr!(June),
        7 => crate::tr!(July),
        8 => crate::tr!(August),
        9 => crate::tr!(September),
        10 => crate::tr!(October),
        11 => crate::tr!(November),
        12 => crate::tr!(December),
        _ => "",
    }
}

/// Render a calendar + time picker + quick-preset control.
pub fn render_datetime_picker<F>(
    id_prefix: &str,
    label: &str,
    state: &DateTimeState,
    live: bool,
    on_update: F,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<F>
where
    F: Fn(DateTimeUpdate, &mut gpui::Window, &mut App) + Clone + 'static,
{
    let theme = cx.theme();
    let today = now_local().date_naive();
    let days = month_grid_days(state.view_year, state.view_month);
    let month_title = format!("{} {}", month_label(state.view_month), state.view_year);
    let time_label = if label.is_empty() {
        crate::tr!(SendTime).to_string()
    } else {
        label.to_string()
    };
    let is_future = state.future_rfc3339().is_some();

    let prev_cb = on_update.clone();
    let next_cb = on_update.clone();
    let h_minus_cb = on_update.clone();
    let h_plus_cb = on_update.clone();
    let m_minus_cb = on_update.clone();
    let m_plus_cb = on_update.clone();
    let p15_cb = on_update.clone();
    let p60_cb = on_update.clone();
    let tomorrow_cb = on_update.clone();

    let weekdays = [
        crate::tr!(Mon),
        crate::tr!(Tue),
        crate::tr!(Wed),
        crate::tr!(Thu),
        crate::tr!(Fri),
        crate::tr!(Sat),
        crate::tr!(Sun),
    ];

    div()
        .flex()
        .flex_col()
        .gap(metrics.space_sm())
        // Calendar month header
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    Button::new(SharedString::from(format!("{id_prefix}/cal-prev")))
                        .icon(IconName::ChevronLeft)
                        .ghost()
                        .small()
                        .disabled(!live)
                        .when(live, |b| {
                            b.cursor_pointer().on_click(move |_, window, cx| {
                                prev_cb(DateTimeUpdate::PrevMonth, window, cx);
                            })
                        }),
                )
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(month_title),
                )
                .child(
                    Button::new(SharedString::from(format!("{id_prefix}/cal-next")))
                        .icon(IconName::ChevronRight)
                        .ghost()
                        .small()
                        .disabled(!live)
                        .when(live, |b| {
                            b.cursor_pointer().on_click(move |_, window, cx| {
                                next_cb(DateTimeUpdate::NextMonth, window, cx);
                            })
                        }),
                ),
        )
        // Weekday header row
        .child(
            div()
                .grid()
                .grid_cols(7)
                .gap(rems(0.1))
                .children(weekdays.into_iter().map(|day_name| {
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(rems(1.3))
                        .text_size(metrics.text_micro())
                        .text_color(theme.muted_foreground)
                        .child(day_name)
                })),
        )
        // 6x7 day grid
        .child(
            div()
                .grid()
                .grid_cols(7)
                .gap(rems(0.1))
                .children(days.into_iter().map(|day| {
                    let in_month = day.month() == state.view_month && day.year() == state.view_year;
                    let is_selected = day == state.selected_date;
                    let is_past = day < today;
                    let disabled = !live || is_past;
                    let day_cb = on_update.clone();
                    Button::new(SharedString::from(format!("{id_prefix}/day-{day}")))
                        .label(day.day().to_string())
                        .small()
                        .disabled(disabled)
                        .when(is_selected, |b| b.primary())
                        .when(!is_selected, |b| b.ghost())
                        .when(!in_month && !is_selected, |b| {
                            b.text_color(theme.muted_foreground)
                        })
                        .when(!disabled, |b| {
                            b.cursor_pointer().on_click(move |_, window, cx| {
                                day_cb(DateTimeUpdate::SelectDate(day), window, cx);
                            })
                        })
                })),
        )
        // Time steppers row
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .p(metrics.space_sm())
                .rounded(rems(0.4))
                .bg(theme.secondary)
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(time_label),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(metrics.space_xxs())
                        .child(
                            Button::new(SharedString::from(format!("{id_prefix}/h-minus")))
                                .label("-")
                                .ghost()
                                .small()
                                .disabled(!live)
                                .when(live, |b| {
                                    b.cursor_pointer().on_click(move |_, window, cx| {
                                        h_minus_cb(DateTimeUpdate::AdjustHour(-1), window, cx);
                                    })
                                }),
                        )
                        .child(
                            div()
                                .w(rems(1.8))
                                .text_center()
                                .text_size(metrics.text_small())
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child(format!("{:02}", state.hour)),
                        )
                        .child(
                            Button::new(SharedString::from(format!("{id_prefix}/h-plus")))
                                .label("+")
                                .ghost()
                                .small()
                                .disabled(!live)
                                .when(live, |b| {
                                    b.cursor_pointer().on_click(move |_, window, cx| {
                                        h_plus_cb(DateTimeUpdate::AdjustHour(1), window, cx);
                                    })
                                }),
                        )
                        .child(
                            div()
                                .text_size(metrics.text_small())
                                .text_color(theme.muted_foreground)
                                .child(":"),
                        )
                        .child(
                            Button::new(SharedString::from(format!("{id_prefix}/m-minus")))
                                .label("-")
                                .ghost()
                                .small()
                                .disabled(!live)
                                .when(live, |b| {
                                    b.cursor_pointer().on_click(move |_, window, cx| {
                                        m_minus_cb(DateTimeUpdate::AdjustMinute(-5), window, cx);
                                    })
                                }),
                        )
                        .child(
                            div()
                                .w(rems(1.8))
                                .text_center()
                                .text_size(metrics.text_small())
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child(format!("{:02}", state.minute)),
                        )
                        .child(
                            Button::new(SharedString::from(format!("{id_prefix}/m-plus")))
                                .label("+")
                                .ghost()
                                .small()
                                .disabled(!live)
                                .when(live, |b| {
                                    b.cursor_pointer().on_click(move |_, window, cx| {
                                        m_plus_cb(DateTimeUpdate::AdjustMinute(5), window, cx);
                                    })
                                }),
                        ),
                ),
        )
        // Quick presets row
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(metrics.space_xs())
                .child(
                    Button::new(SharedString::from(format!("{id_prefix}/preset-15m")))
                        .label(crate::tr!(Preset15m))
                        .ghost()
                        .small()
                        .disabled(!live)
                        .when(live, |b| {
                            b.cursor_pointer().on_click(move |_, window, cx| {
                                p15_cb(DateTimeUpdate::PresetMinutes(15), window, cx);
                            })
                        }),
                )
                .child(
                    Button::new(SharedString::from(format!("{id_prefix}/preset-1h")))
                        .label(crate::tr!(Preset1h))
                        .ghost()
                        .small()
                        .disabled(!live)
                        .when(live, |b| {
                            b.cursor_pointer().on_click(move |_, window, cx| {
                                p60_cb(DateTimeUpdate::PresetMinutes(60), window, cx);
                            })
                        }),
                )
                .child(
                    Button::new(SharedString::from(format!("{id_prefix}/preset-tomorrow")))
                        .label(crate::tr!(Tomorrow0900))
                        .ghost()
                        .small()
                        .disabled(!live)
                        .when(live, |b| {
                            b.cursor_pointer().on_click(move |_, window, cx| {
                                tomorrow_cb(DateTimeUpdate::PresetTomorrowMorning, window, cx);
                            })
                        }),
                ),
        )
        // Selected summary / validation hint
        .child(
            div()
                .text_size(metrics.text_micro())
                .text_color(if is_future {
                    theme.muted_foreground
                } else {
                    theme.danger
                })
                .child(if is_future {
                    state.formatted_selection()
                } else {
                    crate::tr!(SelectFutureDateTime).to_string()
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_grid_has_42_days_starting_on_monday() {
        let days = month_grid_days(2026, 11);
        assert_eq!(days.len(), 42);
        assert_eq!(days[0].weekday(), chrono::Weekday::Mon);
    }

    #[test]
    fn datetime_state_parses_and_roundtrips_rfc3339() {
        let state = DateTimeState::from_rfc3339_or_default("2030-05-15T14:30:00+00:00");
        assert!(state.to_rfc3339().is_some());
        assert!(state.future_rfc3339().is_some());
    }
}
