//! Module evaluation — the top-level-await completion pattern.
//!
//! Mirrors deno_core 0.411 `modules/map/evaluation.rs`. The law:
//! `module.evaluate()` on an instantiated module returns V8's INTERNAL
//! TLA promise; we attach then2 handlers whose state rides in a
//! `v8::External` (`Box::into_raw`), they clear
//! `pending_mod_evaluation`, wake the host loop, and complete our
//! oneshot; the returned future is the oneshot's receiver. Already
//! `Evaluated` → immediately ready. Under terminate the sender is dropped
//! un-sent → the receiver resolves `Terminated` (upstream's
//! `ExecutionTerminated`).
//!
//! Dropped from upstream: the `main_module_callbacks` notify fan-out (our
//! host has no main-module notify contract) and the
//! `evaluating_top_level` microtask-suppression flag (it guards nested
//! lazy-load checkpoints during evaluation — no lazy loads here; our
//! loop drains microtasks only at explicit checkpoints, same discipline).
//!
//! The oneshot is ours, std-only: upstream reaches for tokio's.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use super::map::ModuleMap;
use super::ModuleId;

/// Why a module evaluation failed. `Threw` carries the exception's
/// message (the host's error face is message-shaped today); `Terminated`
/// is the watchdog interrupt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleEvalError {
    Threw(String),
    Terminated,
}

/// The shared cell behind [`OneshotSender`]/[`OneshotReceiver`].
struct OneshotInner<T> {
    value: Option<T>,
    waker: Option<Waker>,
    /// The sender vanished without sending — the drop-is-a-signal shape
    /// (upstream: a dropped tokio oneshot sender reads as
    /// ExecutionTerminated).
    sender_dropped: bool,
}

pub(crate) struct OneshotSender<T> {
    inner: Rc<RefCell<OneshotInner<T>>>,
}

pub(crate) struct OneshotReceiver<T> {
    inner: Rc<RefCell<OneshotInner<T>>>,
}

pub(crate) fn oneshot<T>() -> (OneshotSender<T>, OneshotReceiver<T>) {
    let inner = Rc::new(RefCell::new(OneshotInner {
        value: None,
        waker: None,
        sender_dropped: false,
    }));
    (
        OneshotSender {
            inner: inner.clone(),
        },
        OneshotReceiver { inner },
    )
}

impl<T> OneshotSender<T> {
    pub(crate) fn send(self, value: T) {
        let mut inner = self.inner.borrow_mut();
        inner.value = Some(value);
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
        // self drops at the end of this fn; Drop sees the value present and
        // leaves the drop-marker unset — that ordering IS the protocol.
    }
}

impl<T> Drop for OneshotSender<T> {
    fn drop(&mut self) {
        // Only a signal when nothing was sent (send() marks by value).
        let mut inner = self.inner.borrow_mut();
        if inner.value.is_none() {
            inner.sender_dropped = true;
            if let Some(waker) = inner.waker.take() {
                waker.wake();
            }
        }
    }
}

impl<T> Future for OneshotReceiver<T> {
    type Output = Result<T, Dropped>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.inner.borrow_mut();
        if let Some(value) = inner.value.take() {
            return Poll::Ready(Ok(value));
        }
        if inner.sender_dropped {
            return Poll::Ready(Err(Dropped));
        }
        inner.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// The sender went away without a verdict — module evaluation under
/// termination, the only shape that lands here.
pub(crate) struct Dropped;

/// The future handed to the host: resolves when the module's evaluation
/// promise settles (or immediately, for the degenerate paths).
pub struct ModEvaluateFuture {
    receiver: OneshotReceiver<Result<(), ModuleEvalError>>,
}

impl Future for ModEvaluateFuture {
    type Output = Result<(), ModuleEvalError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        Pin::new(&mut this.receiver)
            .poll(cx)
            .map(|r| r.unwrap_or(Err(ModuleEvalError::Terminated)))
    }
}

/// The state riding in the handlers' External — recovered by
/// `Box::from_raw` exactly once (the fulfilled/rejected pair each get
/// their own External copy of the same raw pointer; whichever runs first
/// consumes the Box, and both run at most once because a promise settles
/// once — the same lifetime law upstream's ModEvaluate box obeys).
struct EvalState {
    map: Rc<ModuleMap>,
    sender: Option<OneshotSender<Result<(), ModuleEvalError>>>,
}

fn take_state(arg: v8::Local<v8::Value>) -> EvalState {
    let external = v8::Local::<v8::External>::try_from(arg).unwrap();
    // SAFETY: the pointer was created by Box::into_raw in mod_evaluate and
    // is consumed exactly once (a promise settles once).
    *unsafe { Box::from_raw(external.value() as *mut EvalState) }
}

fn on_fulfilled(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue,
) {
    let mut state = take_state(args.data());
    state.map.pending_mod_evaluation.set(false);
    state.map.wake();
    if let Some(sender) = state.sender.take() {
        sender.send(Ok(()));
    }
    let _ = scope;
}

fn on_rejected(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue,
) {
    let mut state = take_state(args.data());
    state.map.pending_mod_evaluation.set(false);
    state.map.wake();
    // Upstream sends Ok(()) here: the module's Errored status IS the
    // error record, and the re-thrown exception below surfaces through
    // the unhandled-rejection face. Both halves copied verbatim in
    // behavior — diting's host reads the same shapes today.
    if let Some(sender) = state.sender.take() {
        sender.send(Ok(()));
    }
    scope.throw_exception(args.get(0));
}

impl ModuleMap {
    /// Evaluate an instantiated module. TLA-aware: the returned future
    /// completes when the evaluation promise settles; a sync-degenerating
    /// module completes within the first poll.
    pub(crate) fn mod_evaluate(
        self: &Rc<Self>,
        scope: &mut v8::PinScope,
        id: ModuleId,
    ) -> ModEvaluateFuture {
        v8::tc_scope!(let tc, scope);

        let module = self
            .get_handle(id)
            .map(|handle| v8::Local::new(tc, handle))
            .expect("module id is registered");

        if module.get_status() == v8::ModuleStatus::Evaluated {
            let (sender, receiver) = oneshot();
            sender.send(Ok(()));
            return ModEvaluateFuture { receiver };
        }
        assert_eq!(
            module.get_status(),
            v8::ModuleStatus::Instantiated,
            "Module not instantiated: {} ({})",
            self.get_name_by_id(id).unwrap_or_default(),
            id,
        );

        let (sender, receiver) = oneshot();

        let Some(value) = module.evaluate(tc) else {
            if tc.has_terminated() || tc.is_execution_terminating() {
                // Sender drops un-sent → receiver resolves Terminated.
            } else {
                // Synchronous failure outside the TLA promise (linking
                // machinery). Surface the pending exception's message.
                let msg = match tc.exception() {
                    Some(e) => value_to_message(tc, e),
                    None => "module evaluation failed".into(),
                };
                sender.send(Err(ModuleEvalError::Threw(msg)));
            }
            return ModEvaluateFuture { receiver };
        };

        self.pending_mod_evaluation.set(true);

        let promise = v8::Local::<v8::Promise>::try_from(value)
            .expect("module evaluation result is a promise under TLA");

        let state = v8::External::new(
            tc,
            Box::into_raw(Box::new(EvalState {
                map: self.clone(),
                sender: Some(sender),
            })) as _,
        );

        let on_fulfilled_fn = v8::FunctionBuilder::<v8::Function>::new(on_fulfilled)
            .data(state.into())
            .build(tc);
        let on_rejected_fn = v8::FunctionBuilder::<v8::Function>::new(on_rejected)
            .data(state.into())
            .build(tc);

        if on_fulfilled_fn.is_none()
            || on_rejected_fn.is_none()
            || promise
                .then2(tc, on_fulfilled_fn.unwrap(), on_rejected_fn.unwrap())
                .is_none()
        {
            // Attaching failed (shutdown-shape stack pressure). Consume the
            // state box back and synthesize the verdict from the promise
            // itself, upstream's fallback ladder verbatim.
            let state = take_state(state.into());
            self.pending_mod_evaluation.set(false);
            if let Some(sender) = state.sender {
                match promise.state() {
                    v8::PromiseState::Fulfilled => sender.send(Ok(())),
                    v8::PromiseState::Rejected => {
                        let result = promise.result(tc);
                        let msg = value_to_message(tc, result);
                        sender.send(Err(ModuleEvalError::Threw(msg)));
                    }
                    v8::PromiseState::Pending => {
                        // Nothing to report and no way to be woken — the
                        // sender drop is the terminated signal.
                    }
                }
            }
        }

        // The module-evaluation checkpoint: promise/microtask jobs queued
        // during top-level evaluation run here, and V8's internal TLA
        // resume machinery for async graphs advances on these checkpoints.
        tc.perform_microtask_checkpoint();

        ModEvaluateFuture { receiver }
    }
}

/// A V8 value → its message string. `String(v)` coercion covers Error
/// objects (message) and raw rejections alike.
fn value_to_message(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> String {
    let string = value
        .to_string(scope)
        .unwrap_or_else(|| v8::String::new(scope, "<unprintable exception>").unwrap());
    string.to_rust_string_lossy(scope)
}
