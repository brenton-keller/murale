use std::time::Instant;

// --- Frame timing instrumentation ---

pub(crate) struct FrameStats {
    intervals_ms: Vec<f64>,
    last_frame: Option<Instant>,
    last_report: Instant,
}

impl FrameStats {
    pub(crate) fn new() -> Self {
        Self {
            intervals_ms: Vec::with_capacity(2048),
            last_frame: None,
            last_report: Instant::now(),
        }
    }

    pub(crate) fn record(&mut self, now: Instant) {
        if let Some(prev) = self.last_frame {
            let dt = now.duration_since(prev).as_secs_f64() * 1000.0;
            self.intervals_ms.push(dt);
        }
        self.last_frame = Some(now);
    }

    pub(crate) fn should_report(&self) -> bool {
        self.last_report.elapsed().as_secs() >= 5 && self.intervals_ms.len() >= 2
    }

    pub(crate) fn take_stats(&mut self) -> (f64, f64, f64, f64) {
        let n = self.intervals_ms.len() as f64;
        let sum: f64 = self.intervals_ms.iter().sum();
        let avg = sum / n;
        let min = self.intervals_ms.iter().cloned().reduce(f64::min).unwrap();
        let max = self.intervals_ms.iter().cloned().reduce(f64::max).unwrap();
        let variance: f64 = self
            .intervals_ms
            .iter()
            .map(|dt| (dt - avg).powi(2))
            .sum::<f64>()
            / n;
        let jitter = variance.sqrt();

        self.intervals_ms.clear();
        self.last_report = Instant::now();

        (avg, min, max, jitter)
    }
}
