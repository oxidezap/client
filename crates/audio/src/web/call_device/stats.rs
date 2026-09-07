use std::time::Duration;

pub(super) const REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub callbacks: u64,
    pub underrun_blocks: u64,
    pub underrun_runs: u64,
    pub missing_samples: u64,
    pub late_callbacks: u64,
    pub max_lateness: Duration,
}

impl Counts {
    fn record(&mut self, missing: usize, began: bool, lateness: Duration) {
        self.callbacks = self.callbacks.saturating_add(1);
        self.underrun_blocks = self.underrun_blocks.saturating_add(u64::from(missing > 0));
        self.underrun_runs = self.underrun_runs.saturating_add(u64::from(began));
        self.missing_samples = self.missing_samples.saturating_add(missing as u64);
        self.late_callbacks = self
            .late_callbacks
            .saturating_add(u64::from(!lateness.is_zero()));
        self.max_lateness = self.max_lateness.max(lateness);
    }
}

#[derive(Default)]
pub(super) struct Stats {
    total: Counts,
    window: Counts,
    started: bool,
    in_underrun: bool,
    previous: Option<Duration>,
    stopped: bool,
}

impl Stats {
    pub fn record(&mut self, available: usize, requested: usize, now: Duration, period: Duration) {
        if self.stopped {
            return;
        }
        self.started |= available > 0;
        if !self.started {
            return;
        }
        let missing = requested.saturating_sub(available);
        let began = missing > 0 && !self.in_underrun;
        self.in_underrun = missing > 0;
        // Count only gaps at least one whole block beyond the nominal cadence.
        // This is callback arrival timing, not network delay or audio latency.
        let excess = self
            .previous
            .replace(now)
            .map_or(Duration::ZERO, |previous| {
                now.saturating_sub(previous).saturating_sub(period)
            });
        let lateness = if excess >= period {
            excess
        } else {
            Duration::ZERO
        };
        self.window.record(missing, began, lateness);
        self.total.record(missing, began, lateness);
    }

    pub fn take(&mut self, final_report: bool) -> Option<Counts> {
        if self.stopped {
            return None;
        }
        self.stopped = final_report;
        let counts = if final_report {
            self.total
        } else {
            std::mem::take(&mut self.window)
        };
        (counts.callbacks > 0).then_some(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let mut counts = Counts {
            callbacks: u64::MAX,
            underrun_blocks: u64::MAX,
            underrun_runs: u64::MAX,
            missing_samples: u64::MAX,
            late_callbacks: u64::MAX,
            max_lateness: Duration::MAX,
        };
        let before = counts;
        counts.record(1024, true, Duration::from_secs(1));
        assert_eq!(counts, before);
    }
}
