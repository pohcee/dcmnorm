use std::sync::OnceLock;
use std::time::Instant;

const PERF_ENV_FLAG: &str = "DCMNORM_PERF";

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var(PERF_ENV_FLAG)
            .map(|value| {
                let normalized = value.trim().to_ascii_lowercase();
                matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false)
    })
}

pub struct PerfScope {
    label: &'static str,
    start: Option<Instant>,
}

impl PerfScope {
    pub fn new(label: &'static str) -> Self {
        if enabled() {
            Self {
                label,
                start: Some(Instant::now()),
            }
        } else {
            Self { label, start: None }
        }
    }
}

impl Drop for PerfScope {
    fn drop(&mut self) {
        if let Some(start) = self.start.take() {
            let elapsed = start.elapsed();
            eprintln!(
                "[dcmnorm:perf] {}: {:.3} ms",
                self.label,
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }
}

pub fn scope(label: &'static str) -> PerfScope {
    PerfScope::new(label)
}
