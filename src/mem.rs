use std::sync::OnceLock;
use std::time::Instant;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

pub(crate) fn init() {
    PROCESS_START.get_or_init(Instant::now);
}

pub(crate) fn elapsed_secs() -> f64 {
    PROCESS_START
        .get()
        .map_or(0.0, |t| t.elapsed().as_secs_f64())
}

pub(crate) fn log_rss(tag: &str) {
    let (rss, swap) = read_rss();
    let total = rss + swap;
    let elapsed = elapsed_secs();
    tracing::info!(
        "[mem] {tag} rss={rss:.1}MB swap={swap:.1}MB total={total:.1}MB elapsed={elapsed:.0}s"
    );
}

pub(crate) fn log_extra(tag: &str) {
    let threads = count_threads();
    let fds = count_fds();
    let elapsed = elapsed_secs();
    tracing::info!("[mem-extra] {tag} threads={threads} fds={fds} elapsed={elapsed:.0}s");
}

fn count_threads() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, |d| d.count())
}

fn count_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, |d| d.count())
}

pub(crate) fn read_rss_raw() -> (f64, f64) {
    read_rss()
}

fn read_rss() -> (f64, f64) {
    let contents = match std::fs::read_to_string("/proc/self/status") {
        Ok(c) => c,
        Err(_) => return (0.0, 0.0),
    };
    let mut rss_kb = 0u64;
    let mut swap_kb = 0u64;
    for line in contents.lines() {
        if line.starts_with("VmRSS:") {
            rss_kb = line
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        } else if line.starts_with("VmSwap:") {
            swap_kb = line
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    (rss_kb as f64 / 1024.0, swap_kb as f64 / 1024.0)
}
