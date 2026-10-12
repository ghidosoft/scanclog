//! Scan duration estimates, for progress reporting.
//!
//! eSCL reports no progress, and scanners send baseline JPEGs without
//! restart markers, so the received bytes cannot be mapped to scanned rows.
//! Scan time, however, is dominated by the mechanics: a fixed start-up cost
//! plus a speed per inch of paper that depends on the source and resolution.
//! ScanClog learns that speed from completed scans.

use crate::escl::{InputSource, UNITS_PER_INCH};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Time spent before the scan head moves (warm-up, calibration).
const OVERHEAD: f32 = 3.0;
/// Weight of the newest measurement in the running average.
const LEARNING_RATE: f32 = 0.5;

/// Measured speed for one source and resolution.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScanSpeed {
    pub source: InputSource,
    pub resolution: u32,
    pub seconds_per_inch: f32,
}

/// Learned scan speeds, stored in the configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScanTiming {
    speeds: Vec<ScanSpeed>,
}

impl ScanTiming {
    /// Expected duration of a scan of `height` (in 1/300 in).
    pub fn estimate(&self, source: InputSource, resolution: u32, height: u32) -> Duration {
        let inches = height as f32 / UNITS_PER_INCH as f32;
        let speed = self
            .speed(source, resolution)
            .unwrap_or_else(|| default_speed(resolution));
        Duration::from_secs_f32(OVERHEAD + inches * speed)
    }

    /// Records how long a scan of `height` (in 1/300 in) took.
    pub fn record(&mut self, source: InputSource, resolution: u32, height: u32, took: Duration) {
        let inches = height as f32 / UNITS_PER_INCH as f32;
        if inches < 0.5 {
            // Too short to tell the speed from the overhead.
            return;
        }
        let measured = ((took.as_secs_f32() - OVERHEAD) / inches).max(0.05);
        match self
            .speeds
            .iter_mut()
            .find(|s| s.source == source && s.resolution == resolution)
        {
            Some(entry) => {
                entry.seconds_per_inch += LEARNING_RATE * (measured - entry.seconds_per_inch);
            }
            None => self.speeds.push(ScanSpeed {
                source,
                resolution,
                seconds_per_inch: measured,
            }),
        }
    }

    fn speed(&self, source: InputSource, resolution: u32) -> Option<f32> {
        self.speeds
            .iter()
            .find(|s| s.source == source && s.resolution == resolution)
            .map(|s| s.seconds_per_inch)
            .filter(|s| s.is_finite() && *s > 0.0)
    }
}

/// A guess for scanners not measured yet, based on a typical inkjet
/// multifunction device.
fn default_speed(resolution: u32) -> f32 {
    match resolution {
        0..=100 => 0.7,
        101..=200 => 1.2,
        201..=300 => 1.5,
        301..=600 => 1.7,
        _ => 4.0,
    }
}

/// Fraction of an estimated duration that has elapsed, kept below 1 so a
/// slow scan never looks finished.
pub fn progress(elapsed: Duration, expected: Duration) -> f64 {
    if expected.is_zero() {
        return 0.0;
    }
    (elapsed.as_secs_f64() / expected.as_secs_f64()).min(0.97)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A4: u32 = 3508;

    #[test]
    fn estimates_from_defaults_then_learns() {
        let mut timing = ScanTiming::default();
        let first = timing.estimate(InputSource::Platen, 300, A4);
        assert!((first.as_secs_f32() - (3.0 + 11.693 * 1.5)).abs() < 0.1);

        timing.record(InputSource::Platen, 300, A4, Duration::from_secs(30));
        let learned = timing.estimate(InputSource::Platen, 300, A4);
        assert!(
            (learned.as_secs_f32() - 30.0).abs() < 0.1,
            "first measurement is taken as is"
        );

        timing.record(InputSource::Platen, 300, A4, Duration::from_secs(20));
        let averaged = timing.estimate(InputSource::Platen, 300, A4).as_secs_f32();
        assert!((averaged - 25.0).abs() < 0.1, "{averaged}");

        // Other sources and resolutions are unaffected.
        assert_eq!(timing.estimate(InputSource::Feeder, 300, A4), first);
    }

    #[test]
    fn ignores_tiny_regions() {
        let mut timing = ScanTiming::default();
        timing.record(InputSource::Platen, 300, 100, Duration::from_secs(60));
        assert_eq!(timing, ScanTiming::default());
    }

    #[test]
    fn progress_never_completes() {
        assert_eq!(
            progress(Duration::from_secs(5), Duration::from_secs(10)),
            0.5
        );
        assert!(progress(Duration::from_secs(50), Duration::from_secs(10)) < 1.0);
        assert_eq!(progress(Duration::from_secs(1), Duration::ZERO), 0.0);
    }

    #[test]
    fn serializes_as_a_list() {
        let mut timing = ScanTiming::default();
        timing.record(InputSource::Feeder, 150, 4200, Duration::from_secs(23));
        let text = toml::to_string(&Wrapper {
            timing: timing.clone(),
        })
        .unwrap();
        assert!(text.contains("seconds_per_inch"));
        let back: Wrapper = toml::from_str(&text).unwrap();
        assert_eq!(back.timing, timing);
    }

    #[derive(Serialize, Deserialize)]
    struct Wrapper {
        timing: ScanTiming,
    }
}
