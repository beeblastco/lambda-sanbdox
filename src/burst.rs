//! Burst meter: the CPU and memory this MicroVM used above its baseline.
//!
//! Lambda bills a MicroVM's baseline (1 vCPU / 2 GiB by default) for every second
//! it runs, and on top of that the vCPU and memory it consumes above the baseline
//! while active. AWS exposes no per-VM usage figure, so the server samples the
//! guest once a second and keeps running totals of that excess. `/exec` returns
//! the totals; the harness bills the growth since the last one it saw.
//!
//! The totals live in process memory, so they ride suspend and resume with the
//! snapshot and start again at zero only on a fresh VM.

use serde::Serialize;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
// A longer gap between samples is a suspend: the VM was frozen, so the gap counts
// as one sample interval rather than billing memory for the whole of it.
const MAX_SAMPLE_GAP: Duration = Duration::from_secs(5);
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Running totals of usage above the baseline since this VM booted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct BurstTotals {
    /// vCPU-seconds above the baseline vCPUs.
    pub vcpu_seconds: f64,
    /// GiB-seconds of memory above the baseline memory.
    pub gb_seconds: f64,
}

/// The baseline the excess is measured from.
#[derive(Debug, Clone, Copy)]
pub struct Baseline {
    pub vcpu: f64,
    pub memory_bytes: f64,
}

/// One guest reading: cumulative CPU time and memory in use right now.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub cpu_usec: u64,
    pub memory_used_bytes: u64,
}

/// Adds each interval's excess to the totals. Pure, so the sampler, `/exec` and
/// the tests drive it the same way.
#[derive(Debug)]
pub struct BurstMeter {
    baseline: Baseline,
    last: Option<(Instant, u64)>,
    totals: BurstTotals,
}

impl BurstMeter {
    pub fn new(baseline: Baseline) -> Self {
        Self {
            baseline,
            last: None,
            totals: BurstTotals::default(),
        }
    }

    /// Record a sample taken at `at`. The first one only sets the starting point.
    /// CPU above the baseline is the CPU time used since the previous sample minus
    /// what the baseline vCPUs could give; memory above it is billed for the
    /// interval at the reading's level.
    pub fn record(&mut self, at: Instant, sample: Sample) {
        let Some((last_at, last_cpu_usec)) = self.last.replace((at, sample.cpu_usec)) else {
            return;
        };
        let mut elapsed = at.saturating_duration_since(last_at);
        if elapsed > MAX_SAMPLE_GAP {
            elapsed = SAMPLE_INTERVAL;
        }
        let seconds = elapsed.as_secs_f64();
        let used = sample.cpu_usec.saturating_sub(last_cpu_usec) as f64 / 1_000_000.0;
        self.totals.vcpu_seconds += (used - seconds * self.baseline.vcpu).max(0.0);
        let extra_bytes = sample.memory_used_bytes as f64 - self.baseline.memory_bytes;
        self.totals.gb_seconds += (extra_bytes / GIB).max(0.0) * seconds;
    }

    pub fn totals(&self) -> BurstTotals {
        self.totals
    }
}

/// The totals up to now, for the `/exec` response. Takes a sample first, so an
/// exec that finished between sampler ticks is counted.
pub fn totals() -> BurstTotals {
    sample_now();

    meter()
        .lock()
        .map(|meter| meter.totals())
        .unwrap_or_default()
}

/// Sample the guest every second for the life of the process. Called once from main.
pub fn spawn_sampler() {
    tokio::spawn(async {
        let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
        loop {
            interval.tick().await;
            sample_now();
        }
    });
}

/// Baseline from `SANDBOX_BASELINE_VCPU` and `SANDBOX_BASELINE_MEMORY_MB`, else the
/// MicroVM default of 1 vCPU / 2 GiB. A value that is not a finite, positive
/// number falls back to the default.
fn baseline_from_env() -> Baseline {
    let vcpu = env_number("SANDBOX_BASELINE_VCPU").unwrap_or(1.0);
    let memory_mb = env_number("SANDBOX_BASELINE_MEMORY_MB").unwrap_or(2048.0);

    Baseline {
        vcpu,
        memory_bytes: memory_mb * 1024.0 * 1024.0,
    }
}

fn env_number(name: &str) -> Option<f64> {
    parse_baseline(&std::env::var(name).ok()?)
}

fn meter() -> &'static Mutex<BurstMeter> {
    static METER: OnceLock<Mutex<BurstMeter>> = OnceLock::new();
    METER.get_or_init(|| Mutex::new(BurstMeter::new(baseline_from_env())))
}

/// A baseline setting as a finite, positive number.
pub fn parse_baseline(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite() && *number > 0.0)
}

/// Parse `MemTotal - MemAvailable` from a `/proc/meminfo` body, in bytes. The
/// same "used" the CloudWatch agent's `mem_used_percent` reports: reclaimable
/// page cache is not counted.
pub fn parse_meminfo_used_bytes(contents: &str) -> Option<u64> {
    let field = |name: &str| -> Option<u64> {
        let line = contents.lines().find(|line| line.starts_with(name))?;
        let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        Some(kib * 1024)
    };

    Some(field("MemTotal:")?.saturating_sub(field("MemAvailable:")?))
}

/// Parse the busy CPU time of the aggregate `cpu` line of `/proc/stat`, in
/// microseconds. Busy is user, nice, system, irq and softirq. Steal is time the
/// hypervisor withheld, and guest time is already inside user and nice.
pub fn parse_proc_stat_busy_usec(contents: &str) -> Option<u64> {
    let line = contents.lines().find(|line| line.starts_with("cpu "))?;
    let ticks: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse::<u64>().ok())
        .collect();
    let busy: u64 = [0, 1, 2, 5, 6]
        .iter()
        .map(|index| ticks.get(*index).copied().unwrap_or(0))
        .sum();
    // SAFETY: sysconf takes no pointers; it returns -1 for an unknown name.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = if hz > 0 { hz as u64 } else { 100 };

    Some(busy * 1_000_000 / hz)
}

// Record one sample now. The sampler and `/exec` share the meter, so they share
// one previous reading and never bill an interval twice.
fn sample_now() {
    let Some(sample) = read_sample() else {
        return;
    };
    if let Ok(mut meter) = meter().lock() {
        meter.record(Instant::now(), sample);
    }
}

// The whole VM's CPU and memory. `/proc` covers every process in the guest, the
// server and anything an agent left running in the background included.
fn read_sample() -> Option<Sample> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;

    Some(Sample {
        cpu_usec: parse_proc_stat_busy_usec(&stat)?,
        memory_used_bytes: parse_meminfo_used_bytes(&meminfo)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASELINE: Baseline = Baseline {
        vcpu: 1.0,
        memory_bytes: 2.0 * GIB,
    };

    fn sample(cpu_seconds: f64, memory_gib: f64) -> Sample {
        Sample {
            cpu_usec: (cpu_seconds * 1_000_000.0) as u64,
            memory_used_bytes: (memory_gib * GIB) as u64,
        }
    }

    fn after(start: Instant, seconds: u64) -> Instant {
        start + Duration::from_secs(seconds)
    }

    #[test]
    fn bills_nothing_within_the_baseline() {
        let start = Instant::now();
        let mut meter = BurstMeter::new(BASELINE);
        meter.record(start, sample(0.0, 1.0));
        meter.record(after(start, 1), sample(0.8, 1.5));

        assert_eq!(meter.totals(), BurstTotals::default());
    }

    #[test]
    fn bills_cpu_and_memory_above_the_baseline() {
        let start = Instant::now();
        let mut meter = BurstMeter::new(BASELINE);
        meter.record(start, sample(0.0, 2.0));
        // Three vCPUs busy for a second at 3 GiB: 2 extra vCPU-s, 1 extra GiB-s.
        meter.record(after(start, 1), sample(3.0, 3.0));

        assert!((meter.totals().vcpu_seconds - 2.0).abs() < 1e-9);
        assert!((meter.totals().gb_seconds - 1.0).abs() < 1e-9);
    }

    #[test]
    fn counts_a_suspend_gap_as_one_interval() {
        let start = Instant::now();
        let mut meter = BurstMeter::new(BASELINE);
        meter.record(start, sample(0.0, 4.0));
        meter.record(after(start, 3600), sample(0.0, 4.0));

        assert!((meter.totals().gb_seconds - 2.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_baselines_that_are_not_finite_and_positive() {
        assert_eq!(parse_baseline("2048"), Some(2048.0));
        for value in ["-1", "0", "NaN", "inf", "two"] {
            assert_eq!(parse_baseline(value), None, "{value}");
        }
    }

    #[test]
    fn parses_meminfo_used_without_page_cache() {
        let body = "MemTotal:        8192000 kB\nMemFree:  100 kB\nMemAvailable:    6144000 kB\n";

        assert_eq!(parse_meminfo_used_bytes(body), Some(2_048_000 * 1024));
    }

    #[test]
    fn parses_busy_time_from_proc_stat() {
        // user nice system idle iowait irq softirq steal guest guest_nice
        let body = "cpu  100 0 50 1000 10 5 5 70 40 0\ncpu0 100 0 50 1000 10 5 5 70 40 0\n";
        // SAFETY: as in parse_proc_stat_busy_usec.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;

        assert_eq!(parse_proc_stat_busy_usec(body), Some(160 * 1_000_000 / hz));
    }
}
