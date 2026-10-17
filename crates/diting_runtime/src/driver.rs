//! The op driver — async op futures pumping completions into the settle
//! queue.
//!
//! Batch 3 of the crate's roadmap. deno_core drives ops through a
//! FuturesUnordered pumped by tokio; this is the same law in the shape we
//! need: every loop pass polls every in-flight op future once, completions
//! enter the [`SettleQueue`] in completion order, and the settle pipeline
//! takes it from there. The loop cadence belongs to the embedder (the
//! diting runtime pumps us per command / per pump tick; a real reactor
//! will wake it through [`OpDriver::take_wake`]).
//!
//! Scope verdict (2026-10-17 recon): diting uses NONE of deno_core's other
//! loop phases — no setImmediate anywhere in the tree, no close callbacks;
//! page timers are op_sleep-driven, which is phase 2 like every other op.
//! So this driver implements phase 2 only. Immediates/close faces get
//! built when a real consumer exists, not before.

use crate::settle::SettleQueue;
use crate::vm::Triplet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

/// A future woke us since the last check — a reactor-driven embedder uses
/// this to decide "poll again now" vs "sleep until the reactor says so".
/// Poll-driven embedders (our current loop) can ignore it: they re-drive
/// every pass anyway.
pub(crate) struct WakeFlag(AtomicBool);

impl WakeFlag {
    pub(crate) fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    pub(crate) fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn waker(self: &Arc<Self>) -> Waker {
        Waker::from(self.clone())
    }
}

impl Wake for WakeFlag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

/// In-flight async ops. The isolate slot owns this (behind Rc<RefCell>):
/// V8 function callbacks reach it with no closure capture — a raw
/// `extern "C"` pointer has nowhere to put one (deno's ops do the same
/// trick through their OpCtx external).
pub struct OpDriver {
    futures: Vec<Pin<Box<dyn Future<Output = Triplet>>>>,
    woken: Arc<WakeFlag>,
}

impl Default for OpDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl OpDriver {
    pub fn new() -> Self {
        Self {
            futures: Vec::new(),
            woken: Arc::new(WakeFlag(AtomicBool::new(false))),
        }
    }

    /// An async op starts. Its eventual output is exactly one settle
    /// triplet: the promise id it must settle, the outcome, the payload.
    pub fn spawn(&mut self, fut: impl Future<Output = Triplet> + 'static) {
        self.futures.push(Box::pin(fut));
    }

    /// Ops in flight (no completion yet).
    pub fn pending_ops(&self) -> usize {
        self.futures.len()
    }

    /// True if any future called wake since the last take. Advisory only —
    /// a poll-driven loop never needs it.
    pub fn take_wake(&mut self) -> bool {
        self.woken.0.swap(false, Ordering::AcqRel)
    }

    /// One drive pass: poll every in-flight future once; completions enter
    /// the queue in completion order (swap_remove pulls the tail into the
    /// vacated slot, which this same pass then polls — each future is
    /// polled exactly once). Returns how many completed.
    ///
    /// Called once per loop pass, before delivery. Rust-side work is
    /// untouched by terminate (the interrupt only bites JS), which is
    /// exactly why an op completing while the page is terminated parks in
    /// the queue and survives to the revive.
    pub fn drive(&mut self, queue: &mut SettleQueue<Triplet>) -> usize {
        let waker = Waker::from(self.woken.clone());
        self.drive_with(queue, &waker)
    }

    /// The reactor face (10d): futures receive the CALLER's waker, so a
    /// tokio timer or network read that the op body is parked on wakes the
    /// host's event-loop task directly — the parked loop needs no polling
    /// cadence of its own. The advisory flag stays untouched here; a
    /// reactor host never reads it.
    pub fn drive_with(&mut self, queue: &mut SettleQueue<Triplet>, waker: &Waker) -> usize {
        let mut cx = Context::from_waker(waker);
        let mut completed = 0;
        let mut i = 0;
        while i < self.futures.len() {
            if let Poll::Ready(triplet) = self.futures[i].as_mut().poll(&mut cx) {
                queue.push(triplet);
                // The spent future is dropped here, not awaited further.
                drop(self.futures.swap_remove(i));
                completed += 1;
            } else {
                i += 1;
            }
        }
        completed
    }
}

/// Reference op: completes after N drive passes, settling its promise with
/// the payload it was spawned with. The deterministic stand-in for
/// op_sleep while the real op table still lives in deno_core.
pub struct AfterTicks {
    pub ticks: u32,
    pub triplet: Triplet,
}

impl Future for AfterTicks {
    type Output = Triplet;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Triplet> {
        let this = self.get_mut();
        if this.ticks == 0 {
            return Poll::Ready(std::mem::take(&mut this.triplet));
        }
        this.ticks -= 1;
        if this.ticks == 0 {
            Poll::Ready(std::mem::take(&mut this.triplet))
        } else {
            // Future etiquette: a future that returns Pending must have
            // registered a wake, or a strict executor may never poll it
            // again. Our driver re-polls unconditionally, but keep the
            // contract so real futures behave here too.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_gate_completion_and_wake_registers() {
        let mut d = OpDriver::new();
        let mut q = SettleQueue::new();
        d.spawn(AfterTicks {
            ticks: 2,
            triplet: Triplet {
                promise_id: 1,
                ok: true,
                payload: "x".into(),
            },
        });

        assert_eq!(d.drive(&mut q), 0, "first pass: one tick short");
        assert_eq!(q.pending(), 0);
        assert!(d.take_wake(), "Pending registered a wake");
        assert!(!d.take_wake(), "the wake is consumed by the take");

        assert_eq!(
            d.drive(&mut q),
            1,
            "second pass completes (ticks=N → Nth pass)"
        );
        assert_eq!(q.pending(), 1);
        assert_eq!(
            q.stats().delivered,
            0,
            "queued only — delivery is the queue's job"
        );
        assert_eq!(d.pending_ops(), 0);
    }

    #[test]
    fn completions_keep_completion_order_not_spawn_order() {
        let mut d = OpDriver::new();
        let mut q = SettleQueue::new();
        d.spawn(AfterTicks {
            ticks: 3,
            triplet: Triplet {
                promise_id: 20,
                ok: true,
                payload: "slow".into(),
            },
        });
        d.spawn(AfterTicks {
            ticks: 1,
            triplet: Triplet {
                promise_id: 10,
                ok: true,
                payload: "fast".into(),
            },
        });

        d.drive(&mut q);
        assert_eq!(q.pending(), 1, "only the fast one completed");
        d.drive(&mut q);
        d.drive(&mut q);
        assert_eq!(q.pending(), 2);

        let mut seen = Vec::new();
        q.deliver(16, |batch| {
            seen.extend(batch.iter().map(|t| t.promise_id));
            Ok::<(), ()>(())
        })
        .unwrap();
        assert_eq!(seen, vec![10, 20], "completion order wins over spawn order");
    }
}
