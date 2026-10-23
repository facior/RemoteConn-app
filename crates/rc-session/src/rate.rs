//! Adaptive bitrate from the viewer's network statistics (AIMD).
//!
//! Loss or a growing round-trip time (queues filling up) cut the bitrate by
//! 30 %; a few seconds of clean network raise it by 15 % up to the preset cap.

use std::time::{Duration, Instant};

use rc_proto::{quality_settings::Preset, NetworkStats};

const MIN_KBPS: u32 = 500;
/// Multipliers in percent.
const DECREASE_PCT: u64 = 70;
const INCREASE_PCT: u64 = 115;
const DECREASE_GAP: Duration = Duration::from_secs(1);
const INCREASE_AFTER: Duration = Duration::from_secs(3);
/// Loss above 2 % counts as congestion.
const LOSS_LIMIT_PERMILLE: u32 = 20;

pub fn preset_cap_kbps(preset: Preset) -> u32 {
    match preset {
        Preset::Quality => 20_000,
        Preset::Speed => 2_500,
        Preset::Balanced | Preset::Unspecified => 8_000,
    }
}

pub struct RateController {
    cap_kbps: u32,
    current_kbps: u32,
    baseline_rtt: Option<u32>,
    last_change: Option<Instant>,
    clean_since: Option<Instant>,
}

impl RateController {
    pub fn new(cap_kbps: u32, start_kbps: u32) -> Self {
        Self {
            cap_kbps,
            current_kbps: start_kbps.clamp(MIN_KBPS, cap_kbps.max(MIN_KBPS)),
            baseline_rtt: None,
            last_change: None,
            clean_since: None,
        }
    }

    pub fn current_kbps(&self) -> u32 {
        self.current_kbps
    }

    /// New cap from a quality preset; returns the bitrate if it changed.
    pub fn set_cap(&mut self, cap_kbps: u32) -> Option<u32> {
        self.cap_kbps = cap_kbps.max(MIN_KBPS);
        let before = self.current_kbps;
        // Start a new preset at its cap (capped presets go down at once).
        self.current_kbps = self.cap_kbps;
        self.clean_since = None;
        (self.current_kbps != before).then_some(self.current_kbps)
    }

    /// Feeds one stats report; returns the new bitrate if it changed.
    pub fn on_stats(&mut self, stats: &NetworkStats, now: Instant) -> Option<u32> {
        let baseline = self
            .baseline_rtt
            .map_or(stats.rtt_ms, |b| b.min(stats.rtt_ms));
        self.baseline_rtt = Some(baseline);
        let rtt_limit = baseline + baseline.max(50);
        let congested = stats.loss_permille > LOSS_LIMIT_PERMILLE || stats.rtt_ms > rtt_limit;
        let since_change = self.last_change.map(|t| now.duration_since(t));

        if congested {
            self.clean_since = None;
            if since_change.is_some_and(|d| d < DECREASE_GAP) {
                return None;
            }
            let next = scale(self.current_kbps, DECREASE_PCT).max(MIN_KBPS);
            return self.change(next, now);
        }
        let clean_since = *self.clean_since.get_or_insert(now);
        if now.duration_since(clean_since) >= INCREASE_AFTER
            && since_change.is_none_or(|d| d >= INCREASE_AFTER)
            && self.current_kbps < self.cap_kbps
        {
            let next = scale(self.current_kbps, INCREASE_PCT).min(self.cap_kbps);
            return self.change(next, now);
        }
        None
    }

    fn change(&mut self, next: u32, now: Instant) -> Option<u32> {
        if next == self.current_kbps {
            return None;
        }
        self.current_kbps = next;
        self.last_change = Some(now);
        Some(next)
    }
}

fn scale(kbps: u32, pct: u64) -> u32 {
    (kbps as u64 * pct / 100) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(rtt_ms: u32, loss_permille: u32) -> NetworkStats {
        NetworkStats {
            rtt_ms,
            loss_permille,
            ..Default::default()
        }
    }

    #[test]
    fn backs_off_on_loss_and_rtt_growth() {
        let t0 = Instant::now();
        let mut rc = RateController::new(8000, 8000);
        assert_eq!(rc.on_stats(&stats(20, 0), t0), None);
        assert_eq!(
            rc.on_stats(&stats(20, 50), t0 + Duration::from_secs(1)),
            Some(5600)
        );
        // Not twice within a second.
        assert_eq!(
            rc.on_stats(&stats(20, 50), t0 + Duration::from_millis(1500)),
            None
        );
        // RTT far above the 20 ms baseline also counts as congestion.
        assert_eq!(
            rc.on_stats(&stats(200, 0), t0 + Duration::from_secs(3)),
            Some(3920)
        );
    }

    #[test]
    fn recovers_slowly_up_to_cap() {
        let t0 = Instant::now();
        let mut rc = RateController::new(8000, 4000);
        let mut t = t0;
        let mut rates = vec![];
        for _ in 0..40 {
            t += Duration::from_secs(1);
            if let Some(r) = rc.on_stats(&stats(20, 0), t) {
                rates.push(r);
            }
        }
        assert_eq!(rates.first(), Some(&4600));
        assert_eq!(rc.current_kbps(), 8000);
        assert!(rates.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn never_below_minimum_and_cap_applies() {
        let t0 = Instant::now();
        let mut rc = RateController::new(8000, 600);
        assert_eq!(rc.on_stats(&stats(20, 500), t0), Some(MIN_KBPS));
        assert_eq!(
            rc.on_stats(&stats(20, 500), t0 + Duration::from_secs(5)),
            None
        );
        assert_eq!(rc.set_cap(preset_cap_kbps(Preset::Speed)), Some(2500));
        assert_eq!(rc.set_cap(preset_cap_kbps(Preset::Speed)), None);
    }
}
