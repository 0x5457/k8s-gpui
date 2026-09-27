//! Frame-time samples and percentile statistics. The window prints every 5 seconds.
#[derive(Clone, Copy, Default)]
pub struct Summary {
    pub count: usize,
    pub mean: f32,
    pub p50: f32,
    pub p95: f32,
    pub p99: f32,
    pub max: f32,
}

impl Summary {
    pub fn fps(&self) -> f32 {
        if self.mean > 0.0 {
            1000.0 / self.mean
        } else {
            0.0
        }
    }
}

#[derive(Default)]
pub struct FrameStats {
    samples: Vec<f32>,
    sum: f64,
}

impl FrameStats {
    pub fn record(&mut self, milliseconds: f32) {
        self.samples.push(milliseconds);
        self.sum += f64::from(milliseconds);
    }

    pub fn reset(&mut self) {
        self.samples.clear();
        self.sum = 0.0;
    }

    pub fn summary(&self) -> Summary {
        let count = self.samples.len();
        if count == 0 {
            return Summary::default();
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Summary {
            count,
            mean: (self.sum / count as f64) as f32,
            p50: percentile(&sorted, 0.50),
            p95: percentile(&sorted, 0.95),
            p99: percentile(&sorted, 0.99),
            max: sorted.last().copied().unwrap_or_default(),
        }
    }
}

fn percentile(sorted: &[f32], fraction: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f32 * fraction).ceil() as usize).saturating_sub(1);
    sorted.get(index).copied().unwrap_or_default()
}
