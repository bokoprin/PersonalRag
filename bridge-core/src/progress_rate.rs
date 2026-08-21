use std::collections::VecDeque;

const RATE_WINDOW_MS: f64 = 10_000.0;
const ETA_WINDOW_MS: f64 = 30_000.0;
const HISTORY_WINDOW_MS: f64 = 31_000.0;
const MIB: f64 = 1024.0 * 1024.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProgressRateEstimate {
    pub files_per_second: f64,
    pub mib_per_second: f64,
    pub eta_ms: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
struct RateSample {
    elapsed_ms: f64,
    files: usize,
    bytes: u64,
}

#[derive(Debug, Default)]
pub struct ProgressRateTracker {
    samples: VecDeque<RateSample>,
}

impl ProgressRateTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.samples.clear();
    }

    pub fn update(
        &mut self,
        elapsed_ms: f64,
        files: usize,
        bytes: u64,
        remaining_files: Option<usize>,
    ) -> ProgressRateEstimate {
        if !elapsed_ms.is_finite() || elapsed_ms < 0.0 {
            return ProgressRateEstimate::default();
        }
        if self.samples.back().is_some_and(|previous| {
            elapsed_ms < previous.elapsed_ms || files < previous.files || bytes < previous.bytes
        }) {
            self.samples.clear();
        }
        if self.samples.back().is_some_and(|previous| {
            elapsed_ms == previous.elapsed_ms && files == previous.files && bytes == previous.bytes
        }) {
            return self.estimate(remaining_files);
        }
        self.samples.push_back(RateSample {
            elapsed_ms,
            files,
            bytes,
        });
        let cutoff = elapsed_ms - HISTORY_WINDOW_MS;
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|sample| sample.elapsed_ms < cutoff)
        {
            self.samples.pop_front();
        }
        self.estimate(remaining_files)
    }

    fn estimate(&self, remaining_files: Option<usize>) -> ProgressRateEstimate {
        let Some(current) = self.samples.back().copied() else {
            return ProgressRateEstimate::default();
        };
        let short = self.window_rate(current, RATE_WINDOW_MS);
        let eta_rate = self.window_rate(current, ETA_WINDOW_MS).0;
        let eta_ms = remaining_files.and_then(|remaining| {
            (eta_rate > 0.0).then_some((remaining as f64 / eta_rate) * 1_000.0)
        });
        ProgressRateEstimate {
            files_per_second: short.0,
            mib_per_second: short.1 / MIB,
            eta_ms,
        }
    }

    fn window_rate(&self, current: RateSample, window_ms: f64) -> (f64, f64) {
        let cutoff = current.elapsed_ms - window_ms;
        let base = self
            .samples
            .iter()
            .find(|sample| sample.elapsed_ms >= cutoff)
            .copied()
            .or_else(|| self.samples.front().copied());
        let Some(base) = base else {
            return (0.0, 0.0);
        };
        let delta_ms = current.elapsed_ms - base.elapsed_ms;
        if delta_ms <= 0.0 {
            return (0.0, 0.0);
        }
        let seconds = delta_ms / 1_000.0;
        (
            current.files.saturating_sub(base.files) as f64 / seconds,
            current.bytes.saturating_sub(base.bytes) as f64 / seconds,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::ProgressRateTracker;

    #[test]
    fn uses_ten_second_rate_and_thirty_second_eta() {
        let mut tracker = ProgressRateTracker::new();
        tracker.update(0.0, 0, 0, Some(1_000));
        tracker.update(10_000.0, 1_000, 10 * 1024 * 1024, Some(900));
        tracker.update(20_000.0, 1_500, 20 * 1024 * 1024, Some(850));
        let estimate = tracker.update(30_000.0, 2_000, 30 * 1024 * 1024, Some(800));
        assert!((estimate.files_per_second - 50.0).abs() < 0.001);
        assert!((estimate.mib_per_second - 1.0).abs() < 0.001);
        // 30-second rate is 2000/30 = 66.666 files/s, so 800 files ~= 12 seconds.
        assert!((estimate.eta_ms.unwrap() - 12_000.0).abs() < 1.0);
    }

    #[test]
    fn resets_when_counters_restart_for_a_new_phase() {
        let mut tracker = ProgressRateTracker::new();
        tracker.update(0.0, 0, 0, None);
        tracker.update(10_000.0, 1_000, 1_000, None);
        let reset = tracker.update(1_000.0, 10, 10, Some(90));
        assert_eq!(reset.files_per_second, 0.0);
        assert_eq!(reset.eta_ms, None);
    }
}
