//! Opt-in session diagnostics. RSS is process-wide and Linux-specific.
use std::fs;

pub fn enabled() -> bool {
    std::env::var_os("FJERN_STATS").is_some()
}

pub fn rss_mib() -> Option<f64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(kib as f64 / 1024.0)
}
