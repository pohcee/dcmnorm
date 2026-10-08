//! One threading policy for every JPEG 2000 codec call in dcmnorm.
//!
//! Both OpenJPEG and OpenHTJ2K can split a single frame's decode/encode across threads, which is
//! what makes a single large image (a 4096x3328 mammogram) fast - production OpenJPEG used to run
//! strictly single-threaded. But dcmnorm also decodes multi-frame objects frame-parallel on rayon
//! (`decode_pixel_data_parallel_frames`), and nesting a full per-frame thread pool inside that
//! would oversubscribe the machine, so a call made from inside a rayon worker gets one thread.

use std::sync::OnceLock;

/// Overrides the per-call codec thread count (a positive integer). `1` disables codec-level
/// threading entirely.
pub const THREADS_ENV: &str = "DCMNORM_JPEG2000_THREADS";

/// Upper bound on the default thread count: past this, per-frame speedups flatten out (see
/// docs/jpeg2000-codec-evaluation.md) while the cost to concurrent requests keeps growing.
pub const DEFAULT_MAX_THREADS: u32 = 8;

/// Threads one codec call may use: `DCMNORM_JPEG2000_THREADS` if set, otherwise
/// min(available parallelism, 8).
pub fn configured_threads() -> u32 {
    static CONFIGURED: OnceLock<u32> = OnceLock::new();
    *CONFIGURED.get_or_init(|| {
        std::env::var(THREADS_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|&threads| threads > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get() as u32)
                    .unwrap_or(1)
                    .clamp(1, DEFAULT_MAX_THREADS)
            })
    })
}

/// Threads for a call that's about to create its own per-call thread pool (OpenJPEG): one when
/// already running on a rayon worker, otherwise [`configured_threads`].
pub fn per_call_threads() -> u32 {
    if rayon::current_thread_index().is_some() {
        1
    } else {
        configured_threads()
    }
}
