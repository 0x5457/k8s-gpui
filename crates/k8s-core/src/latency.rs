//! Selects transport timeouts from measured RTT and packet loss.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

/// Connection timeout for TCP and TLS handshakes.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Write timeout for normal requests.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Read timeout for non-watch requests. Leave the client read timeout unset.
pub const NON_WATCH_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The delay before the nth consecutive retry: 1s, 2s, 4s, 8s, 16s, then 30s.
///
/// One ladder for the whole crate. Three callers used to each carry their own
/// copy of it - the metrics sampler, the watch reconnector and the connection
/// machine - with the same 1s base, the same 30s ceiling and the same cap on the
/// exponent, differing only in whether they incremented their counter before or
/// after reading it. Doubling to 16s and then holding is the point: long enough
/// that a cluster which is restarting is not hammered, short enough that a
/// transient blip is over before a reader has looked away.
pub(crate) fn backoff(failures: u32) -> Duration {
    const BASE: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(30);
    if failures == 0 {
        return Duration::ZERO;
    }
    BASE.saturating_mul(1u32 << failures.saturating_sub(1).min(5))
        .min(MAX)
}

/// Budget for building one cluster during registry load.
///
/// A credential plugin runs as a child process with no timeout of its own, so a plugin that
/// waits for input would otherwise keep the registry — and the window that waits for it —
/// loading forever. The budget is per context, so one broken plugin costs one context and not
/// every context behind it.
pub const REGISTRY_LOAD_DEADLINE: Duration = NON_WATCH_READ_TIMEOUT;

/// Backstop for loading every context in one kubeconfig.
///
/// Contexts load one at a time, so a file whose contexts all hang would otherwise cost one
/// budget each. This caps the total; contexts still waiting when it expires are reported as
/// timed out without being attempted.
pub const REGISTRY_TOTAL_DEADLINE: Duration = Duration::from_secs(4 * 30);

pub(crate) async fn with_read_timeout<T, E>(
    duration: Duration,
    operation: impl Future<Output = Result<T, E>>,
    timeout_error: E,
) -> Result<T, E> {
    match tokio::time::timeout(duration, operation).await {
        Ok(result) => result,
        Err(_) => Err(timeout_error),
    }
}

/// Watch timeout for the local tier, in seconds.
pub const WATCH_TIMEOUT_LOCAL_SECS: u32 = 60;

/// Watch timeout for the high-latency tier, in seconds.
pub const WATCH_TIMEOUT_HIGH_LATENCY_SECS: u32 = 300;

/// RTT threshold for the high-latency tier.
pub const HIGH_LATENCY_RTT: Duration = Duration::from_millis(150);

/// Packet-loss threshold for the high-latency tier.
pub const HIGH_LATENCY_LOSS_RATE: f64 = 0.005;

/// Interval between RTT probes.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Number of recent probes used for packet loss.
pub const PROBE_WINDOW: usize = 32;

/// Successful probes behind the published round-trip time.
///
/// The tier, the watch timeout it selects and the one latency number the app ever
/// shows all read this value, so it has to survive a single bad minute. One probe
/// is a sample and not a rate: a credential that was not ready yet, one retry, or
/// one packet that took a different path is enough to call a healthy link
/// cross-region, and a tier chosen that way costs a 300 second watch timeout and
/// streaming lists until enough fast probes push it back out of the window.
pub const RTT_SMOOTHING_SAMPLES: usize = 5;

/// Maximum concurrent background requests for one cluster.
pub const BACKGROUND_CONCURRENCY: usize = 4;

/// Transport tier selected from measured RTT and packet loss.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LatencyTier {
    #[default]
    Local,
    HighLatency,
}

impl LatencyTier {
    pub fn watch_timeout_secs(self) -> u32 {
        match self {
            Self::Local => WATCH_TIMEOUT_LOCAL_SECS,
            Self::HighLatency => WATCH_TIMEOUT_HIGH_LATENCY_SECS,
        }
    }

    /// High latency uses WatchList streaming and falls back to ListWatch after failure.
    pub fn uses_streaming_lists(self) -> bool {
        matches!(self, Self::HighLatency)
    }
}

/// Select a tier from RTT and packet loss. Values must exceed the threshold.
pub fn classify(rtt: Option<Duration>, loss_rate: f64) -> LatencyTier {
    if rtt.is_some_and(|rtt| rtt > HIGH_LATENCY_RTT) || loss_rate > HIGH_LATENCY_LOSS_RATE {
        LatencyTier::HighLatency
    } else {
        LatencyTier::Local
    }
}

/// Snapshot of the latest probe results.
#[derive(Clone, Debug, PartialEq)]
pub struct Latency {
    /// RTT from the recent successful probes: their median, so one slow probe
    /// cannot move it. `None` before the first success.
    pub rtt: Option<Duration>,
    /// Packet loss over the recent probe window.
    pub loss_rate: f64,
    /// Total number of probes.
    pub probes: u64,
    /// Total number of failed probes.
    pub failures: u64,
}

impl Latency {
    pub fn tier(&self) -> LatencyTier {
        classify(self.rtt, self.loss_rate)
    }
}

impl Default for Latency {
    fn default() -> Self {
        Self {
            rtt: None,
            loss_rate: 0.0,
            probes: 0,
            failures: 0,
        }
    }
}

/// Sliding-window packet-loss tracker. The probe task updates it.
#[derive(Debug, Default)]
pub struct LatencyTracker {
    window: VecDeque<bool>,
    rtt_samples: VecDeque<Duration>,
    probes: u64,
    failures: u64,
}

impl LatencyTracker {
    /// `None` records a failed probe.
    pub fn record(&mut self, sample: Option<Duration>) {
        self.probes = self.probes.saturating_add(1);
        if let Some(rtt) = sample {
            if self.rtt_samples.len() >= RTT_SMOOTHING_SAMPLES {
                self.rtt_samples.pop_front();
            }
            self.rtt_samples.push_back(rtt);
        } else {
            self.failures = self.failures.saturating_add(1);
        }
        if self.window.len() >= PROBE_WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(sample.is_some());
    }

    pub fn snapshot(&self) -> Latency {
        let loss_rate = if self.window.is_empty() {
            0.0
        } else {
            let lost = self.window.iter().filter(|ok| !**ok).count();
            lost as f64 / self.window.len() as f64
        };
        Latency {
            rtt: median(&self.rtt_samples),
            loss_rate,
            probes: self.probes,
            failures: self.failures,
        }
    }
}

/// The middle of the window, taking the later of two middles so an even window
/// answers with the slower half rather than the faster one.
fn median(samples: &VecDeque<Duration>) -> Option<Duration> {
    let mut sorted: Vec<Duration> = samples.iter().copied().collect();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn millis(rtt: u64) -> Option<Duration> {
        Some(Duration::from_millis(rtt))
    }

    #[test]
    fn rtt_boundary_decides_tier() {
        assert_eq!(classify(millis(149), 0.0), LatencyTier::Local);
        assert_eq!(classify(millis(150), 0.0), LatencyTier::Local);
        assert_eq!(classify(millis(151), 0.0), LatencyTier::HighLatency);
    }

    #[test]
    fn loss_boundary_decides_tier() {
        assert_eq!(classify(millis(10), 0.0049), LatencyTier::Local);
        assert_eq!(classify(millis(10), 0.005), LatencyTier::Local);
        assert_eq!(classify(millis(10), 0.0051), LatencyTier::HighLatency);
        assert_eq!(classify(None, 0.0), LatencyTier::Local);
    }

    #[test]
    fn timeout_and_streaming_follow_tier() {
        assert_eq!(LatencyTier::Local.watch_timeout_secs(), 60);
        assert_eq!(LatencyTier::HighLatency.watch_timeout_secs(), 300);
        assert!(!LatencyTier::Local.uses_streaming_lists());
        assert!(LatencyTier::HighLatency.uses_streaming_lists());
    }

    #[derive(Debug, PartialEq, Eq)]
    enum TestError {
        Operation,
        Timeout,
    }

    #[tokio::test]
    async fn read_timeout_maps_to_typed_error() {
        let pending = std::future::pending::<Result<(), TestError>>();
        assert_eq!(
            with_read_timeout(Duration::ZERO, pending, TestError::Timeout).await,
            Err(TestError::Timeout)
        );
        assert_eq!(
            with_read_timeout(
                Duration::from_secs(1),
                async { Err::<(), TestError>(TestError::Operation) },
                TestError::Timeout,
            )
            .await,
            Err(TestError::Operation)
        );
    }

    #[test]
    fn tracker_publishes_a_median_rtt_and_a_rolling_loss_window() {
        let mut tracker = LatencyTracker::default();
        tracker.record(millis(20));
        assert_eq!(tracker.snapshot().tier(), LatencyTier::Local);

        tracker.record(None);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.loss_rate, 0.5);
        assert_eq!(snapshot.tier(), LatencyTier::HighLatency);
        assert_eq!(
            snapshot.rtt,
            millis(20),
            "a failed sample keeps the last successful RTT"
        );
        assert_eq!((snapshot.probes, snapshot.failures), (2, 1));

        for _ in 0..PROBE_WINDOW {
            tracker.record(millis(10));
        }
        assert_eq!(
            tracker.snapshot().loss_rate,
            0.0,
            "a failed sample leaves the window"
        );

        // One slow probe is a slow minute, not a slow link: the tier decides a
        // 300 second watch timeout and streaming lists, so a single outlier must
        // not be able to choose it.
        tracker.record(millis(400));
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.rtt, millis(10));
        assert_eq!(snapshot.tier(), LatencyTier::Local);

        // A link that is slow every time is slow.
        tracker.record(millis(400));
        tracker.record(millis(400));
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.rtt, millis(400));
        assert_eq!(snapshot.tier(), LatencyTier::HighLatency);
    }
}
