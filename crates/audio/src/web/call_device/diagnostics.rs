use std::cell::RefCell;
use std::future::Future;

use log::{debug, error, warn};
use wasm_bindgen::JsCast as _;

use super::stats::{REPORT_INTERVAL, Stats};

#[derive(Default)]
pub(super) struct Diagnostics {
    pub stats: Option<Stats>,
    write_error: Option<wasm_bindgen::JsValue>,
    write_error_seen: bool,
}

impl Diagnostics {
    pub fn new(enabled: bool) -> Self {
        Self {
            stats: enabled.then(Stats::default),
            ..Self::default()
        }
    }

    pub fn write_failed(&mut self, error: wasm_bindgen::JsValue) {
        if !self.write_error_seen {
            self.write_error_seen = true;
            self.write_error = Some(error);
        }
    }

    pub fn report(&mut self, final_report: bool) {
        if let Some(error) = self.write_error.take() {
            error!(
                "call playout could not write to the speaker: {}",
                describe(&error)
            );
        }
        if let Some(counts) = self
            .stats
            .as_mut()
            .and_then(|stats| stats.take(final_report))
        {
            let scope = if final_report {
                "final total"
            } else {
                "interval"
            };
            if counts.underrun_blocks > 0 || counts.late_callbacks > 0 {
                warn!(
                    "call playout {scope}: {} callbacks, {} starved blocks in {} runs, \
                     {} missing samples, {} late callbacks, max callback lateness {:.2} ms",
                    counts.callbacks,
                    counts.underrun_blocks,
                    counts.underrun_runs,
                    counts.missing_samples,
                    counts.late_callbacks,
                    counts.max_lateness.as_secs_f64() * 1000.0,
                );
            } else {
                debug!(
                    "call playout {scope}: {} callbacks, no starvation or late callbacks",
                    counts.callbacks
                );
            }
        }
    }
}

pub(super) async fn report_until<T>(
    ending: impl Future<Output = T>,
    diagnostics: &RefCell<Diagnostics>,
) -> T {
    // Dropping the race cancels the platform wait before the graph is released.
    futures_lite::future::or(ending, async {
        loop {
            oxidezap_platform::sleep(REPORT_INTERVAL).await;
            diagnostics.borrow_mut().report(false);
        }
    })
    .await
}

pub(super) fn describe(value: &wasm_bindgen::JsValue) -> String {
    value
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| value.as_string())
        .unwrap_or_else(|| format!("{value:?}"))
}
