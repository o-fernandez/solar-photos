//! Opt-in stage timing. Set `SOLAR_PROFILE=1` to print an aggregate breakdown of
//! where the background pipeline spends its time — decode vs. detect vs. embed vs.
//! the extra preview encode. Off by default and effectively free (one relaxed
//! atomic load per stage), so it can stay compiled in.
//!
//! The point is to answer "would the GPU help?" with numbers: if `decode` dwarfs
//! `detect`+`embed`, the bottleneck is CPU-bound image work the GPU can't touch.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Copy)]
pub enum Stage {
    Decode = 0,
    Detect = 1,
    Embed = 2,
    Preview = 3,
}

const N: usize = 4;
const NAMES: [&str; N] = ["decode", "detect", "embed", "preview"];

static ENABLED: AtomicBool = AtomicBool::new(false);
static COUNT: [AtomicU64; N] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static NANOS: [AtomicU64; N] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Read the env once at startup. Call from setup.
pub fn init() {
    let on = std::env::var("SOLAR_PROFILE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    ENABLED.store(on, Ordering::Relaxed);
    if on {
        eprintln!("[prof] SOLAR_PROFILE on — timings print every 200 decodes");
    }
}

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Record one sample. No-op (and cheap) when profiling is off. A summary line is
/// printed every 200 decodes so a long sweep reports progress without spamming.
pub fn record(stage: Stage, d: Duration) {
    if !enabled() {
        return;
    }
    let i = stage as usize;
    COUNT[i].fetch_add(1, Ordering::Relaxed);
    NANOS[i].fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    if matches!(stage, Stage::Decode) && COUNT[i].load(Ordering::Relaxed) % 200 == 0 {
        dump();
    }
}

fn dump() {
    let mut line = String::from("[prof]");
    for i in 0..N {
        let c = COUNT[i].load(Ordering::Relaxed);
        if c == 0 {
            continue;
        }
        let ns = NANOS[i].load(Ordering::Relaxed) as f64;
        let avg_ms = ns / c as f64 / 1e6;
        let tot_s = ns / 1e9;
        line.push_str(&format!(" {}: n={c} avg={avg_ms:.1}ms tot={tot_s:.1}s", NAMES[i]));
    }
    eprintln!("{line}");
}
