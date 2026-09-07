#[path = "../stats.rs"]
mod stats;

use stats::{Counts, REPORT_INTERVAL, Stats};
use std::time::Duration;

const PERIOD: Duration = Duration::from_millis(20);

#[test]
fn startup_silence_is_not_starvation_or_a_late_callback() {
    let mut stats = Stats::default();
    stats.record(0, 1024, Duration::ZERO, PERIOD);
    stats.record(0, 1024, Duration::from_secs(30), PERIOD);
    assert_eq!(stats.take(false), None);
    stats.record(8640, 1024, Duration::from_secs(60), PERIOD);
    assert_eq!(
        stats.take(true),
        Some(Counts {
            callbacks: 1,
            ..Counts::default()
        })
    );
}

#[test]
fn partial_blocks_and_runs_survive_report_boundaries() {
    let mut stats = Stats::default();
    stats.record(1000, 1024, Duration::ZERO, PERIOD);
    stats.record(0, 1024, PERIOD, PERIOD);
    let first = stats.take(false).unwrap();
    assert_eq!(
        (
            first.underrun_blocks,
            first.underrun_runs,
            first.missing_samples
        ),
        (2, 1, 1048)
    );
    stats.record(0, 1024, PERIOD * 2, PERIOD);
    let continuing = stats.take(false).unwrap();
    assert_eq!(
        (continuing.underrun_blocks, continuing.underrun_runs),
        (1, 0)
    );
    stats.record(1024, 1024, PERIOD * 3, PERIOD);
    stats.record(1023, 1024, PERIOD * 4, PERIOD);
    let total = stats.take(true).unwrap();
    assert_eq!(
        (
            total.callbacks,
            total.underrun_blocks,
            total.underrun_runs,
            total.missing_samples
        ),
        (5, 4, 2, 2073)
    );
    assert_eq!(stats.take(true), None);
    assert_eq!(stats.take(false), None);
    stats.record(0, 1024, PERIOD * 5, PERIOD);
    assert_eq!(stats.take(false), None);
}

#[test]
fn callback_gaps_use_actual_period_and_do_not_drift() {
    for rate in [16_000, 44_100, 48_000, 96_000] {
        let period = Duration::from_secs_f64(1024.0 / f64::from(rate));
        let mut stats = Stats::default();
        stats.record(1024, 1024, Duration::ZERO, period);
        stats.record(1024, 1024, period, period);
        stats.record(1024, 1024, period * 3, period);
        stats.record(1024, 1024, period * 4, period);
        stats.record(1024, 1024, period * 8, period);
        let total = stats.take(true).unwrap();
        assert_eq!(total.late_callbacks, 2);
        assert_eq!(total.max_lateness, period * 3);
        assert_eq!(total.underrun_blocks, 0);
    }
}

#[test]
fn alternating_starvation_has_no_callback_reports_and_bounded_summaries() {
    let mut stats = Stats::default();
    let mut reports = 0;
    let mut next_report = REPORT_INTERVAL;
    for callback in 0..1000 {
        let now = PERIOD * callback;
        stats.record(if callback % 2 == 0 { 1024 } else { 0 }, 1024, now, PERIOD);
        if now >= next_report {
            reports += usize::from(stats.take(false).is_some());
            next_report = now + REPORT_INTERVAL;
        }
    }
    let total = stats.take(true).unwrap();
    reports += 1;
    assert_eq!(reports, 4);
    assert_eq!(
        (
            total.callbacks,
            total.underrun_blocks,
            total.underrun_runs,
            total.missing_samples
        ),
        (1000, 500, 500, 512_000)
    );
}

#[test]
fn delayed_reporting_takes_one_aggregate_without_catchup() {
    let mut stats = Stats::default();
    stats.record(1024, 1024, Duration::ZERO, PERIOD);
    for callback in 1..10_000 {
        stats.record(0, 1024, PERIOD * callback, PERIOD);
    }
    let counts = stats.take(false).unwrap();
    assert_eq!(counts.underrun_blocks, 9999);
    assert_eq!(counts.underrun_runs, 1);
    assert_eq!(stats.take(false), None);
}

#[test]
fn legacy_alternating_starvation_warns_every_run() {
    let mut blocks = 0u32;
    let mut in_underrun = false;
    let mut warnings = 0;
    for callback in 0..1000 {
        if callback % 2 == 0 {
            blocks += 1;
            let began = !std::mem::replace(&mut in_underrun, true);
            warnings += usize::from(began || blocks.is_power_of_two());
        } else {
            in_underrun = false;
        }
    }
    assert_eq!(warnings, 500);
}
