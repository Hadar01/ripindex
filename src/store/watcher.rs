//! The file watcher: a hint, never a source of truth (see the `state` module
//! docs for why — queue overflows, coalesced directory-level events, renames
//! covering whole subtrees, and no ordering guarantee between an event and
//! the filesystem state when it's handled). Every hint, including an
//! overflow, just schedules a reconcile; `update_index`'s own staged
//! comparison against the last commit is what actually decides what changed.
//!
//! [`run`] is pure scheduling logic — debounce a burst into one trigger,
//! force a trigger at least every `periodic_reconcile_ms` regardless of what
//! the watcher reported — decoupled from both the real event source
//! ([`NotifySource`], backed by the `notify` crate) and real time (a
//! [`Clock`]), so the scheduling behaviour is deterministically testable:
//! [`test_support::FakeClock`] and [`test_support::ScriptedSource`] replay a
//! scripted sequence of hints, drops, and delays with no real waiting whatsoever.
//!
//! **Scope note.** [`on_trigger`](run) reconciles the *whole* root on every
//! trigger rather than scoping to the subtree an event named. Correct
//! (reconcile converges regardless of scope, and is what's tested
//! exhaustively in `store::update`) but not the cheapest possible response to
//! a one-file edit in a huge tree; per-subtree scoping is the natural
//! follow-up, bounded in the meantime by how large a "whole tree" stat pass
//! actually costs (see the reconcile-cost bench).

use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvOutcome {
    /// A real filesystem event arrived (and any others already queued were drained).
    Hint,
    /// The watcher reported a problem (e.g. a kernel event queue overflow).
    /// Treated exactly like a `Hint` — a reason to reconcile, nothing more.
    Overflow,
    /// No event within the requested window.
    Timeout,
    /// The source will never produce another event.
    Closed,
}

/// Where hints come from. `recv` blocks (in a real source) for at most
/// `timeout_ms`.
pub trait EventSource {
    fn recv(&mut self, timeout_ms: u64) -> RecvOutcome;
}

/// A source of "how much virtual time has passed", decoupled from real time
/// so scheduling logic can be tested without waiting.
pub trait Clock {
    fn now_ms(&self) -> u64;
}

pub struct RealClock(Instant);

impl RealClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis() as u64
    }
}

/// A `notify`-backed source. Any event — a real change or an error the
/// watcher reports (queue overflow, a dropped watch) — becomes a hint;
/// anything else already queued is drained first, so a burst of editor
/// writes reaches [`run`] as a single `Hint`.
pub struct NotifySource {
    rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    _watcher: notify::RecommendedWatcher,
}

impl NotifySource {
    pub fn watch(root: &Path) -> notify::Result<Self> {
        use notify::Watcher;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher.watch(root, notify::RecursiveMode::Recursive)?;
        Ok(Self { rx, _watcher: watcher })
    }
}

impl EventSource for NotifySource {
    fn recv(&mut self, timeout_ms: u64) -> RecvOutcome {
        use std::sync::mpsc::RecvTimeoutError;
        match self.rx.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(Ok(_event)) => {
                while self.rx.try_recv().is_ok() {} // drain the rest of this burst
                RecvOutcome::Hint
            }
            Ok(Err(e)) => {
                log::warn!("watcher: {e}; reconciling the whole root");
                RecvOutcome::Overflow
            }
            Err(RecvTimeoutError::Timeout) => RecvOutcome::Timeout,
            Err(RecvTimeoutError::Disconnected) => RecvOutcome::Closed,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WatchConfig {
    /// Coalesce hints arriving within this quiet window into one trigger.
    /// Default 500 ms.
    pub debounce_ms: u64,
    /// Force a trigger at least this often even with no hints at all — the
    /// bound on how long a watcher that silently drops or misreads an event
    /// can leave the index wrong. Default 10 minutes.
    pub periodic_reconcile_ms: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self { debounce_ms: 500, periodic_reconcile_ms: 10 * 60 * 1000 }
    }
}

/// Drive the schedule until `should_stop()` returns true or the source
/// closes. Reconciles once immediately, then on every debounced burst of
/// hints and at least every `periodic_reconcile_ms`. `on_trigger` performs
/// the actual reconcile (in production, `update_index`) and runs at most
/// once per debounced batch.
pub fn run(
    source: &mut dyn EventSource,
    clock: &dyn Clock,
    cfg: &WatchConfig,
    mut should_stop: impl FnMut() -> bool,
    mut on_trigger: impl FnMut(),
) {
    on_trigger();
    let mut last_trigger = clock.now_ms();
    loop {
        if should_stop() {
            return;
        }
        let elapsed = clock.now_ms().saturating_sub(last_trigger);
        let wait = cfg.periodic_reconcile_ms.saturating_sub(elapsed);
        match source.recv(wait) {
            RecvOutcome::Closed => return,
            RecvOutcome::Timeout => {
                on_trigger();
                last_trigger = clock.now_ms();
            }
            RecvOutcome::Hint | RecvOutcome::Overflow => {
                // Absorb further hints until a quiet window of `debounce_ms`,
                // so a burst (an editor's write-temp-then-rename, a `git
                // checkout` touching hundreds of files) becomes one trigger.
                // Empty body on purpose: the work *is* draining the source until
                // it goes quiet (Timeout) or ends (Closed).
                while matches!(source.recv(cfg.debounce_ms), RecvOutcome::Hint | RecvOutcome::Overflow) {}
                on_trigger();
                last_trigger = clock.now_ms();
            }
        }
    }
}

/// Deterministic test doubles: a manually-advanced clock and a source driven
/// by a fixed script, so [`run`]'s scheduling can be exercised with no real
/// waiting and no real filesystem watch.
pub mod test_support {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    pub struct FakeClock(AtomicU64);

    impl FakeClock {
        pub fn new() -> Self {
            Self(AtomicU64::new(0))
        }

        pub fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// A source that replays `(virtual_ms_to_advance, outcome)` pairs; once
    /// exhausted it reports `Timeout` forever, advancing the clock by
    /// whatever `run` asked to wait — i.e. "no more real events, only the
    /// periodic fallback keeps firing," the overflow/dropped-event scenario.
    pub struct ScriptedSource<'a> {
        clock: &'a FakeClock,
        script: VecDeque<(u64, RecvOutcome)>,
    }

    impl<'a> ScriptedSource<'a> {
        pub fn new(clock: &'a FakeClock, script: Vec<(u64, RecvOutcome)>) -> Self {
            Self { clock, script: script.into() }
        }
    }

    impl EventSource for ScriptedSource<'_> {
        fn recv(&mut self, timeout_ms: u64) -> RecvOutcome {
            match self.script.pop_front() {
                Some((advance, outcome)) => {
                    self.clock.advance(advance);
                    outcome
                }
                None => {
                    self.clock.advance(timeout_ms);
                    RecvOutcome::Timeout
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::cell::Cell;

    fn run_script(script: Vec<(u64, RecvOutcome)>, cfg: WatchConfig, stop_after: u32) -> u32 {
        let clock = FakeClock::new();
        let mut source = ScriptedSource::new(&clock, script);
        let count = Cell::new(0u32);
        run(&mut source, &clock, &cfg, || count.get() >= stop_after, || count.set(count.get() + 1));
        count.get()
    }

    #[test]
    fn always_reconciles_once_at_startup_even_with_no_events() {
        let n = run_script(vec![], WatchConfig { periodic_reconcile_ms: 1_000_000, ..Default::default() }, 1);
        assert_eq!(n, 1);
    }

    #[test]
    fn a_burst_of_hints_becomes_one_trigger() {
        // Five hints arriving 10ms apart, well inside the 500ms debounce.
        let script = vec![
            (0, RecvOutcome::Hint),
            (10, RecvOutcome::Hint),
            (10, RecvOutcome::Hint),
            (10, RecvOutcome::Hint),
            (10, RecvOutcome::Hint),
        ];
        // Startup trigger (1) + one more after the burst settles = 2, even
        // though 5 hints arrived.
        let n = run_script(script, WatchConfig::default(), 2);
        assert_eq!(n, 2);
    }

    #[test]
    fn overflow_is_treated_exactly_like_a_hint() {
        let n = run_script(vec![(0, RecvOutcome::Overflow)], WatchConfig::default(), 2);
        assert_eq!(n, 2);
    }

    #[test]
    fn widely_spaced_hints_are_separate_triggers() {
        // Two hints, a full second apart — well outside the 500ms debounce.
        let script = vec![(0, RecvOutcome::Hint), (1_000, RecvOutcome::Hint)];
        let n = run_script(script, WatchConfig::default(), 3);
        assert_eq!(n, 3); // startup + one per hint
    }

    /// The overflow-recovery / dropped-event property: even if the watcher
    /// *never* reports a real event again (queue overflow that drops
    /// everything, or a watch silently dying), the periodic fallback still
    /// fires and the index still converges eventually.
    #[test]
    fn no_events_at_all_still_reconciles_periodically() {
        let cfg = WatchConfig { debounce_ms: 500, periodic_reconcile_ms: 60_000 };
        // Nothing in the script: every recv() times out, and `run` counts
        // elapsed virtual time toward the periodic deadline.
        let n = run_script(vec![], cfg, 4); // startup + 3 periodic fires
        assert_eq!(n, 4);
    }

    #[test]
    fn stops_promptly_when_asked() {
        let clock = FakeClock::new();
        let mut source = ScriptedSource::new(&clock, vec![]);
        let count = Cell::new(0u32);
        run(&mut source, &clock, &WatchConfig::default(), || true, || count.set(count.get() + 1));
        assert_eq!(count.get(), 1); // only the startup trigger; stop is checked before the next wait
    }

    #[test]
    fn closed_source_ends_the_run() {
        let clock = FakeClock::new();
        let mut source = ScriptedSource::new(&clock, vec![(0, RecvOutcome::Closed)]);
        let count = Cell::new(0u32);
        run(&mut source, &clock, &WatchConfig::default(), || false, || count.set(count.get() + 1));
        assert_eq!(count.get(), 1); // startup only; Closed ends the loop before triggering again
    }
}
