//! The op-settlement queue — completed async-op triplets on their way to JS
//! settlement, with re-delivery.
//!
//! deno_core's pipeline pops triplets from its FIFO and dispatches them to
//! `__eventLoopTick` in one call; anything interrupted between pop and
//! delivery evaporated (the #226/#240 Rust-side window). `SettleQueue` owns
//! that transition instead: a batch whose delivery fails is parked, not
//! dropped, and re-delivered first on the next round.
//!
//! The queue is single-threaded by design — it lives inside the runtime
//! next to the isolate, exactly like deno_core's `ContextState` fields. No
//! locks, no atomics.

use std::collections::VecDeque;

/// A batch that failed delivery and must go first next round.
struct Parked<T> {
    batch: Vec<T>,
    /// Failed delivery attempts so far (a fresh failure parks with 1).
    attempts: u32,
}

/// A batch is parked this many times without completing before it is dropped
/// as poison: an internal bug must surface in the stats and let the loop
/// move on, never wedge it. Matches the JS-side constant in
/// `crates/diting/js/core_base.js`.
pub const MAX_DELIVERY_ATTEMPTS: u32 = 3;

/// Delivery-loop observability. A dropped batch is a real bug somewhere
/// downstream (the tick's resolver); the counters make it visible without
/// crashing the page.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Triplets successfully handed to the deliver closure.
    pub delivered: u64,
    /// Times a batch was parked after a failed delivery.
    pub parked_batches: u64,
    /// Poison batches dropped after [`MAX_DELIVERY_ATTEMPTS`] failures.
    pub dropped_batches: u64,
    /// Triplets lost with those dropped batches.
    pub dropped_triplets: u64,
}

pub struct SettleQueue<T> {
    ready: VecDeque<T>,
    parked: VecDeque<Parked<T>>,
    stats: Stats,
}

impl<T> Default for SettleQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> SettleQueue<T> {
    pub fn new() -> Self {
        Self {
            ready: VecDeque::new(),
            parked: VecDeque::new(),
            stats: Stats::default(),
        }
    }

    /// A completed triplet enters the queue (completion order = settlement
    /// order).
    pub fn push(&mut self, item: T) {
        self.ready.push_back(item);
    }

    /// Triplets not yet delivered — fresh plus parked. The loop's
    /// has-more-work signal.
    pub fn pending(&self) -> usize {
        self.ready.len() + self.parked.iter().map(|p| p.batch.len()).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.pending() == 0
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Attempt one delivery round.
    ///
    /// Parked batches go first (oldest first), then one fresh batch of up
    /// to `cap` items. `deliver` receives the whole batch slice; an `Err`
    /// parks the batch — its ENTIRE content, even if the tick had already
    /// resolved a prefix before failing: the JS-side Missing-absorb layer
    /// (js/core_base.js) folds re-delivered prefixes into no-ops — and
    /// propagates the error so the loop sees terminates/exceptions. A batch
    /// reaching [`MAX_DELIVERY_ATTEMPTS`] failed attempts is dropped
    /// (counted in [`Stats`]), and its error still propagates.
    ///
    /// `Ok(true)` = at least one batch was delivered this round;
    /// `Ok(false)` = nothing was ready.
    pub fn deliver<E>(
        &mut self,
        cap: usize,
        mut deliver: impl FnMut(&[T]) -> Result<(), E>,
    ) -> Result<bool, E> {
        let mut delivered_any = false;

        // 1) Parked batches, oldest first — settlement order is completion
        //    order, and everything parked is older than anything fresh.
        while let Some(front) = self.parked.front_mut() {
            let batch = std::mem::take(&mut front.batch);
            match deliver(&batch) {
                Ok(()) => {
                    self.parked.pop_front();
                    self.stats.delivered += batch.len() as u64;
                    delivered_any = true;
                }
                Err(e) => {
                    if front.attempts + 1 >= MAX_DELIVERY_ATTEMPTS {
                        // Poison: drop, count, and still surface the error —
                        // the loop must see it, but never spin on it.
                        self.parked.pop_front();
                        self.stats.dropped_batches += 1;
                        self.stats.dropped_triplets += batch.len() as u64;
                    } else {
                        front.attempts += 1;
                        front.batch = batch;
                    }
                    return Err(e);
                }
            }
        }

        // 2) One fresh batch, capped.
        if self.ready.is_empty() {
            return Ok(delivered_any);
        }
        let n = self.ready.len().min(cap);
        let batch: Vec<T> = self.ready.drain(..n).collect();
        match deliver(&batch) {
            Ok(()) => {
                self.stats.delivered += n as u64;
                Ok(true)
            }
            Err(e) => {
                self.parked.push_back(Parked { batch, attempts: 1 });
                self.stats.parked_batches += 1;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collecting deliver closure: records every batch it sees, fails while
    /// the "terminated" flag is up.
    struct Recorder {
        batches: Vec<Vec<u32>>,
        fail: bool,
        failures: usize,
    }

    impl Recorder {
        fn deliver(&mut self, batch: &[u32]) -> Result<(), &'static str> {
            self.batches.push(batch.to_vec());
            if self.fail {
                self.failures += 1;
                Err("terminated")
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn fifo_order_and_cap() {
        let mut q = SettleQueue::new();
        for i in 0..5u32 {
            q.push(i);
        }
        let mut r = Recorder {
            batches: vec![],
            fail: false,
            failures: 0,
        };
        let delivered = q.deliver(3, |b| r.deliver(b)).unwrap();
        assert!(delivered);
        let delivered = q.deliver(3, |b| r.deliver(b)).unwrap();
        assert!(delivered);
        assert_eq!(r.batches, vec![vec![0, 1, 2], vec![3, 4]]);
        assert_eq!(q.pending(), 0);
        assert!(
            !q.deliver(3, |b| r.deliver(b)).unwrap(),
            "empty rounds are Ok(false)"
        );
    }

    #[test]
    fn failed_delivery_parks_and_redelivers_first() {
        let mut q = SettleQueue::new();
        // First round: terminate eats the batch mid-flight.
        for i in 0..3u32 {
            q.push(i);
        }
        let mut r = Recorder {
            batches: vec![],
            fail: true,
            failures: 0,
        };
        assert!(q.deliver(10, |b| r.deliver(b)).is_err());
        assert_eq!(q.pending(), 3, "parked batch still counts as pending");

        // Meanwhile new completions arrive.
        q.push(10);
        q.push(11);

        // Post-cancel round: parked batch must go FIRST, then the fresh
        // batch in the same round — settlement order is completion order.
        r.fail = false;
        assert!(q.deliver(10, |b| r.deliver(b)).unwrap());
        assert_eq!(
            r.batches,
            vec![vec![0, 1, 2], vec![0, 1, 2], vec![10, 11]],
            "failed attempt then parked-first re-delivery, fresh after"
        );
        assert_eq!(q.pending(), 0);
        assert_eq!(q.stats().delivered, 5);
        assert_eq!(q.stats().parked_batches, 1);
    }

    #[test]
    fn terminate_shape_recovers_on_third_attempt() {
        // Two failures, success on the third attempt: must NOT be dropped
        // (drop happens on the third FAILURE).
        let mut q = SettleQueue::new();
        q.push(7u32);
        let mut r = Recorder {
            batches: vec![],
            fail: true,
            failures: 0,
        };
        assert!(q.deliver(10, |b| r.deliver(b)).is_err());
        assert!(q.deliver(10, |b| r.deliver(b)).is_err());
        assert_eq!(r.failures, 2);
        r.fail = false;
        assert!(q.deliver(10, |b| r.deliver(b)).unwrap());
        assert_eq!(q.pending(), 0);
        assert_eq!(
            q.stats().dropped_batches,
            0,
            "recovered batches are never dropped"
        );
        assert_eq!(q.stats().delivered, 1);
    }

    #[test]
    fn poison_batch_drops_after_max_attempts() {
        let mut q = SettleQueue::new();
        q.push(1u32);
        q.push(2);
        let mut r = Recorder {
            batches: vec![],
            fail: true,
            failures: 0,
        };
        assert!(q.deliver(10, |b| r.deliver(b)).is_err());
        assert!(q.deliver(10, |b| r.deliver(b)).is_err());
        assert!(q.deliver(10, |b| r.deliver(b)).is_err());
        assert_eq!(r.failures, 3);
        assert_eq!(
            q.pending(),
            0,
            "the poison batch is gone after the third failure"
        );
        let s = q.stats();
        assert_eq!(s.dropped_batches, 1);
        assert_eq!(s.dropped_triplets, 2);
        assert_eq!(s.delivered, 0);

        // The loop moves on: later work still flows.
        q.push(9);
        r.fail = false;
        assert!(q.deliver(10, |b| r.deliver(b)).unwrap());
        assert_eq!(q.stats().delivered, 1);
    }

    #[test]
    fn multiple_parked_batches_keep_order() {
        let mut q = SettleQueue::new();
        let mut r = Recorder {
            batches: vec![],
            fail: true,
            failures: 0,
        };

        // Round 1: everything fails — [1] parks.
        q.push(1u32);
        assert!(q.deliver(10, |b| r.deliver(b)).is_err());

        // Round 2: parked [1] succeeds, then the fresh batch fails — cap 1
        // so only [2] parks; [3] stays ready.
        q.push(2u32);
        q.push(3);
        let mut calls = 0;
        assert!(q
            .deliver(1, |b| {
                calls += 1;
                r.batches.push(b.to_vec());
                if calls == 2 {
                    Err("tick threw")
                } else {
                    Ok(())
                }
            })
            .is_err());

        // Round 3: parked [2] must deliver before fresh [3].
        r.fail = false;
        assert!(q.deliver(10, |b| r.deliver(b)).unwrap());
        let tail: Vec<Vec<u32>> = r.batches[3..].to_vec();
        assert_eq!(tail, vec![vec![2], vec![3]], "parked before fresh");
        let s = q.stats();
        assert_eq!(s.delivered, 3, "[1] round 2, [2]+[3] round 3");
        assert_eq!(s.parked_batches, 2);
    }
}
