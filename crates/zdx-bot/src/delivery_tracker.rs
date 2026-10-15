//! Reply deliveries in flight, shared by the reply path and the Mini App
//! server in one bot process.
//!
//! The Mini App polls a thread only while something is live. A reply's
//! pending delivery record can sit for minutes (pacing, flood waits, several
//! files), and a stored `pending` record alone cannot tell "still running"
//! from "the process stopped mid-send". This set answers that exactly for the
//! current process, so the server can keep clients polling until the outcome
//! lands, and a record left behind by a crash never looks active.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct State {
    /// Thread id → number of deliveries running for it.
    running: HashMap<String, usize>,
    /// Deliveries finished in this process, any thread. Lets a reader detect
    /// a delivery that started and finished while it was reading. Kept under
    /// the same lock as `running`, so "no longer running" and "finished" are
    /// always published together.
    finished: u64,
}

#[derive(Clone, Default)]
pub(crate) struct ActiveDeliveries(Arc<Mutex<State>>);

impl ActiveDeliveries {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().expect("active deliveries lock poisoned")
    }

    /// Marks a delivery for `thread_id` as running until the guard drops.
    pub(crate) fn start(&self, thread_id: &str) -> DeliveryGuard {
        *self
            .state()
            .running
            .entry(thread_id.to_string())
            .or_default() += 1;
        DeliveryGuard {
            set: self.clone(),
            thread_id: thread_id.to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self, thread_id: &str) -> bool {
        self.state().running.contains_key(thread_id)
    }

    /// Runs `load` (a read of the thread's events) and reports whether a
    /// client should keep polling: a delivery for the thread is running after
    /// the read, or some delivery finished during it. The second case covers
    /// a delivery whose pending record the read saw but whose outcome landed
    /// just after. Completions of other threads can also set it, which only
    /// costs extra polls.
    pub(crate) fn observe<T>(&self, thread_id: &str, load: impl FnOnce() -> T) -> (T, bool) {
        let finished_before = self.state().finished;
        let loaded = load();
        let state = self.state();
        let delivering = state.running.contains_key(thread_id) || state.finished != finished_before;
        drop(state);
        (loaded, delivering)
    }
}

/// Keeps one delivery marked as running; dropping it clears the mark.
pub(crate) struct DeliveryGuard {
    set: ActiveDeliveries,
    thread_id: String,
}

impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        let mut state = self.set.state();
        if let Some(count) = state.running.get_mut(&self.thread_id) {
            *count -= 1;
            if *count == 0 {
                state.running.remove(&self.thread_id);
            }
        }
        state.finished += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::ActiveDeliveries;

    #[test]
    fn overlapping_deliveries_stay_active_until_the_last_ends() {
        let set = ActiveDeliveries::default();
        assert!(!set.is_active("t"));
        let first = set.start("t");
        let second = set.start("t");
        drop(first);
        assert!(set.is_active("t"));
        assert!(!set.is_active("other"));
        drop(second);
        assert!(!set.is_active("t"));
    }

    #[test]
    fn observe_reports_deliveries_that_run_or_finish_during_the_read() {
        let set = ActiveDeliveries::default();
        assert!(!set.observe("t", || ()).1, "idle");

        // Started before the read, finished during it: the read may hold the
        // pending record without the outcome.
        let running = set.start("t");
        assert!(set.observe("t", || drop(running)).1);

        // Started and finished entirely during the read.
        assert!(set.observe("t", || drop(set.start("t"))).1);

        // Started during the read and still running.
        let (guard, delivering) = set.observe("t", || set.start("t"));
        assert!(delivering);
        drop(guard);
        assert!(!set.observe("t", || ()).1, "idle again");
    }

    /// The invariant the Mini App relies on, under real concurrency: if a
    /// read sees a delivery's pending record but not its outcome, `observe`
    /// reports it as delivering. The writer mirrors `send_final_response`:
    /// start, write pending, write outcome, drop the guard.
    #[test]
    fn a_read_holding_only_the_pending_record_always_reports_delivering() {
        use std::sync::atomic::{AtomicU8, Ordering};
        use std::sync::{Arc, Barrier};

        const PENDING: u8 = 1;
        const OUTCOME: u8 = 2;
        for _ in 0..2_000 {
            let set = ActiveDeliveries::default();
            let log = Arc::new(AtomicU8::new(0));
            let barrier = Arc::new(Barrier::new(2));
            let writer = {
                let (set, log, barrier) = (set.clone(), Arc::clone(&log), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    let guard = set.start("t");
                    log.store(PENDING, Ordering::SeqCst);
                    barrier.wait();
                    log.store(OUTCOME, Ordering::SeqCst);
                    drop(guard);
                })
            };
            barrier.wait();
            for _ in 0..20 {
                let (seen, delivering) = set.observe("t", || log.load(Ordering::SeqCst));
                if seen == PENDING {
                    assert!(delivering, "pending seen but not delivering");
                }
            }
            writer.join().expect("writer");
        }
    }
}
