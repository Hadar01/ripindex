//! Resource politeness: nobody keeps a tool that makes their laptop audible.
//! A [`Governor`] is meant to be consulted before each unit of background
//! work — one shard's write, one merge group — and answers "how long to
//! idle before doing more, or (on battery / under memory pressure) whether
//! to pause entirely."
//!
//! Three independent, separately testable pieces:
//! - [`TokenBucket`]: a byte-rate cap on background IO, so a large merge
//!   doesn't saturate disk bandwidth.
//! - [`DutyCycle`]: a CPU fraction cap, achieved by idling in proportion to
//!   work done — a hard cap on indexer/merge CPU with no wall-clock windowing
//!   to get wrong.
//! - [`PowerSource`]: battery and memory-pressure signals, behind a trait so
//!   the pause-on-signal *policy* in [`Governor::throttle`] is testable with
//!   a fake even though real detection is inherently OS-specific.
//!
//! [`RealPowerSource`] (M4) does real, OS-native detection: `GetSystemPowerStatus`
//! (on-battery) and `GlobalMemoryStatusEx`'s `dwMemoryLoad` (memory pressure)
//! on Windows; `/sys/class/power_supply/*/status` and `/proc/meminfo` on
//! Linux. Both are best-effort — any read failure or an unexpected shape
//! falls back to "no signal" (never spuriously throttles a plugged-in,
//! comfortably-provisioned machine because a sysfs file was missing) rather
//! than erroring. The daemon's per-root actor consults a `Governor` — built
//! from `RealPowerSource` — before each merge-prepare group
//! (`governor.throttle(bytes)`) and after each group's CPU work
//! (`governor.after_cpu_work(spent_ms)`); see `daemon::actor`.
//!
//! The Unix path compiles under `cfg(unix)` but, like this project's other
//! from-day-one Unix-specific code, is unexercised on this (Windows)
//! development sandbox.

use std::sync::Mutex;

use super::watcher::{Clock, RealClock};

/// A byte-rate limiter. Pure arithmetic over a virtual clock (`now_ms`
/// supplied by the caller, not read internally), so it's exercised with no
/// real waiting in tests.
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    rate_per_ms: f64,
    last_ms: u64,
}

impl TokenBucket {
    /// `rate_bytes_per_sec` also sets the burst capacity (one second's worth).
    pub fn new(rate_bytes_per_sec: u64, now_ms: u64) -> Self {
        let capacity = rate_bytes_per_sec.max(1) as f64;
        Self { capacity, tokens: capacity, rate_per_ms: capacity / 1000.0, last_ms: now_ms }
    }

    fn refill(&mut self, now_ms: u64) {
        let dt = now_ms.saturating_sub(self.last_ms) as f64;
        self.tokens = (self.tokens + dt * self.rate_per_ms).min(self.capacity);
        self.last_ms = now_ms;
    }

    /// Spend `bytes`. If the bucket doesn't have enough, spends what it has
    /// and returns the number of milliseconds the caller should wait (at the
    /// configured rate) before the rest would be available; `0` means the
    /// full amount was granted immediately.
    pub fn spend(&mut self, now_ms: u64, bytes: u64) -> u64 {
        self.refill(now_ms);
        let bytes = bytes as f64;
        if self.tokens >= bytes {
            self.tokens -= bytes;
            return 0;
        }
        let deficit = bytes - self.tokens;
        self.tokens = 0.0;
        (deficit / self.rate_per_ms).ceil() as u64
    }
}

/// A CPU fraction cap: after doing `spent_ms` of work, idle in the same
/// proportion needed to hold the target fraction — no windowing, so there's
/// no window-boundary case to get wrong, just a running average.
#[derive(Debug, Clone, Copy)]
pub struct DutyCycle {
    fraction: f64,
}

impl DutyCycle {
    /// `fraction` is clamped to `[0.01, 1.0]` — never fully starve, never
    /// need to divide by zero.
    pub fn new(fraction: f64) -> Self {
        Self { fraction: fraction.clamp(0.01, 1.0) }
    }

    /// Milliseconds to idle after `spent_ms` of work to hold the duty cycle.
    pub fn idle_after(&self, spent_ms: u64) -> u64 {
        ((spent_ms as f64) * (1.0 - self.fraction) / self.fraction).round() as u64
    }
}

/// Battery and memory-pressure signals. Behind a trait so the pause policy
/// is testable without real hardware; see the module's scope note.
pub trait PowerSource: Send + Sync {
    fn on_battery(&self) -> bool;
    fn memory_pressure(&self) -> bool;
}

/// Real, OS-native battery/memory-pressure detection. See the module docs.
pub struct RealPowerSource;

/// Above this `dwMemoryLoad` / computed-used-fraction percentage, treat the
/// system as under memory pressure. Conservative: background indexing work
/// is the kind of thing a user wants paused well before the system is
/// actually thrashing.
const MEMORY_PRESSURE_PERCENT: u32 = 90;

impl PowerSource for RealPowerSource {
    #[cfg(windows)]
    fn on_battery(&self) -> bool {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        // SAFETY: `status` is a valid, correctly-sized out-pointer for the
        // duration of this call.
        let ok = unsafe { GetSystemPowerStatus(&mut status) };
        // `ACLineStatus`: 0 = offline (on battery), 1 = online, 255 = unknown.
        ok != 0 && status.ACLineStatus == 0
    }

    #[cfg(unix)]
    fn on_battery(&self) -> bool {
        // No single, universal API; go by what the kernel exposes under
        // sysfs. Any battery reporting "Discharging" means we're on battery;
        // a machine with no battery directories at all (most servers,
        // desktops) is never considered "on battery."
        let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else { return false };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_battery = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("BAT"));
            if !is_battery {
                continue;
            }
            if let Ok(status) = std::fs::read_to_string(path.join("status")) {
                if status.trim().eq_ignore_ascii_case("discharging") {
                    return true;
                }
            }
        }
        false
    }

    #[cfg(not(any(windows, unix)))]
    fn on_battery(&self) -> bool {
        false
    }

    #[cfg(windows)]
    fn memory_pressure(&self) -> bool {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: `status.dwLength` is set to the struct's real size as the
        // API requires, and `status` is a valid out-pointer for the call.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        ok != 0 && status.dwMemoryLoad >= MEMORY_PRESSURE_PERCENT
    }

    #[cfg(unix)]
    fn memory_pressure(&self) -> bool {
        let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else { return false };
        let mut total_kb: Option<u64> = None;
        let mut available_kb: Option<u64> = None;
        for line in meminfo.lines() {
            let parse = |prefix: &str| -> Option<u64> { line.strip_prefix(prefix)?.split_whitespace().next()?.parse().ok() };
            if let Some(v) = parse("MemTotal:") {
                total_kb = Some(v);
            } else if let Some(v) = parse("MemAvailable:") {
                available_kb = Some(v);
            }
        }
        match (total_kb, available_kb) {
            (Some(total), Some(available)) if total > 0 => {
                let used_percent = ((total - available.min(total)) * 100) / total;
                used_percent as u32 >= MEMORY_PRESSURE_PERCENT
            }
            _ => false,
        }
    }

    #[cfg(not(any(windows, unix)))]
    fn memory_pressure(&self) -> bool {
        false
    }
}

/// How long [`Governor::throttle`] backs off when a [`PowerSource`] signal
/// is active, before checking again.
pub const POWER_BACKOFF_MS: u64 = 30_000;

pub struct Governor {
    bucket: Mutex<TokenBucket>,
    duty: DutyCycle,
    power: Box<dyn PowerSource>,
    clock: Box<dyn Clock + Send + Sync>,
}

impl Governor {
    pub fn new(bytes_per_sec: u64, cpu_fraction: f64) -> Self {
        let clock = RealClock::new();
        let bucket = TokenBucket::new(bytes_per_sec, clock.now_ms());
        Self { bucket: Mutex::new(bucket), duty: DutyCycle::new(cpu_fraction), power: Box::new(RealPowerSource), clock: Box::new(clock) }
    }

    #[cfg(test)]
    fn with_parts(bucket: TokenBucket, duty: DutyCycle, power: Box<dyn PowerSource>, clock: Box<dyn Clock + Send + Sync>) -> Self {
        Self { bucket: Mutex::new(bucket), duty, power, clock }
    }

    /// Before doing `bytes` of background IO: how long to wait first. A
    /// battery or memory-pressure signal takes priority over the byte
    /// budget and backs off for [`POWER_BACKOFF_MS`] regardless of `bytes`.
    pub fn throttle(&self, bytes: u64) -> u64 {
        if self.power.on_battery() || self.power.memory_pressure() {
            return POWER_BACKOFF_MS;
        }
        let now = self.clock.now_ms();
        self.bucket.lock().unwrap().spend(now, bytes)
    }

    /// After `spent_ms` of CPU work: how long to idle to hold the duty cycle.
    pub fn after_cpu_work(&self, spent_ms: u64) -> u64 {
        self.duty.idle_after(spent_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_grants_up_to_capacity_immediately() {
        let mut b = TokenBucket::new(1000, 0); // 1000 B/s, burst 1000 B
        assert_eq!(b.spend(0, 500), 0);
        assert_eq!(b.spend(0, 500), 0); // exactly the burst
        assert!(b.spend(0, 1) > 0); // over budget now
    }

    #[test]
    fn token_bucket_refills_over_virtual_time() {
        let mut b = TokenBucket::new(1000, 0);
        b.spend(0, 1000); // drain the bucket
        assert_eq!(b.spend(500, 500), 0, "half a second at 1000 B/s refills 500 B");
        assert!(b.spend(500, 1) > 0, "and no more than that");
    }

    #[test]
    fn token_bucket_reports_a_sensible_wait() {
        let mut b = TokenBucket::new(1000, 0);
        b.spend(0, 1000);
        let wait = b.spend(0, 500); // need 500 B at 1 B/ms
        assert_eq!(wait, 500);
    }

    #[test]
    fn token_bucket_never_exceeds_capacity() {
        let mut b = TokenBucket::new(1000, 0);
        assert_eq!(b.spend(1_000_000, 1000), 0); // huge idle gap, still capped at burst
        assert!(b.spend(1_000_000, 1) > 0);
    }

    #[test]
    fn duty_cycle_half_means_equal_idle_and_work() {
        let d = DutyCycle::new(0.5);
        assert_eq!(d.idle_after(100), 100);
    }

    #[test]
    fn duty_cycle_full_never_idles() {
        let d = DutyCycle::new(1.0);
        assert_eq!(d.idle_after(1000), 0);
    }

    #[test]
    fn duty_cycle_small_fraction_idles_much_longer_than_it_works() {
        let d = DutyCycle::new(0.1);
        assert_eq!(d.idle_after(100), 900); // 10% duty: 9x idle per unit of work
    }

    #[test]
    fn duty_cycle_clamps_to_a_sane_range() {
        assert_eq!(DutyCycle::new(0.0).idle_after(100), DutyCycle::new(0.01).idle_after(100));
        assert_eq!(DutyCycle::new(5.0).idle_after(100), 0);
    }

    struct FakePower {
        battery: bool,
        pressure: bool,
    }
    impl PowerSource for FakePower {
        fn on_battery(&self) -> bool {
            self.battery
        }
        fn memory_pressure(&self) -> bool {
            self.pressure
        }
    }

    #[test]
    fn power_signal_overrides_the_byte_budget() {
        use super::super::watcher::test_support::FakeClock;
        let clock = FakeClock::new();
        let g = Governor::with_parts(
            TokenBucket::new(1_000_000_000, 0), // effectively unlimited
            DutyCycle::new(1.0),
            Box::new(FakePower { battery: true, pressure: false }),
            Box::new(clock),
        );
        assert_eq!(g.throttle(1), POWER_BACKOFF_MS);

        let clock2 = super::super::watcher::test_support::FakeClock::new();
        let g2 = Governor::with_parts(
            TokenBucket::new(1_000_000_000, 0),
            DutyCycle::new(1.0),
            Box::new(FakePower { battery: false, pressure: true }),
            Box::new(clock2),
        );
        assert_eq!(g2.throttle(1), POWER_BACKOFF_MS);
    }

    #[test]
    fn no_power_signal_falls_through_to_the_byte_budget() {
        let clock = super::super::watcher::test_support::FakeClock::new();
        let g = Governor::with_parts(
            TokenBucket::new(100, 0),
            DutyCycle::new(1.0),
            Box::new(FakePower { battery: false, pressure: false }),
            Box::new(clock),
        );
        assert_eq!(g.throttle(100), 0); // within the burst
        assert!(g.throttle(1) > 0); // now over budget
    }

    /// Not a claim about what this machine's actual power/memory state is —
    /// just that the real, OS-native detection never panics and produces a
    /// plain `bool` on whatever this build happens to run on, sandbox or not.
    #[test]
    fn real_power_source_never_panics() {
        let p = RealPowerSource;
        let _: bool = p.on_battery();
        let _: bool = p.memory_pressure();
    }
}
