//! Dynamic `import()` — the host callback, graph loads, and settlement.
//!
//! Mirrors deno_core 0.411 `runtime/bindings.rs`
//! (`host_import_module_dynamically_callback`) and `modules/map/dynamic.rs`
//! (load, drain, evaluate, settle). The law: the host callback CREATES the
//! resolver + promise and returns the promise; the graph load runs as a
//! driven-from-the-event-loop future (never a spawned task); when the
//! stream ends the root is instantiated and evaluated; the evaluation
//! promise (TLA) is tapped by a wake handler and settled by polling its
//! state each pass; the user's promise resolves with the module namespace.
//!
//! Dropped from upstream, deliberately: continuation-preserved embedder
//! data (`cped` — node:vm machinery), the `.catch` re-throw wrapper that
//! re-stacks resolution errors at the import site (our rejections are
//! TypeError-shaped messages, Chrome's face — stack rewriting went with
//! it), import-attribute validation callbacks, import phases
//! (`import.source`/`import.defer`), and the lazy/synthetic fast paths.
//! TLA waiters STAY: a duplicate `import()` of a still-evaluating module
//! must resolve on completion, not hang.
//!
//! Re-entrancy law (upstream's, in caps, at `dynamic_import_resolve`): no
//! `ModuleMap` borrows are held while a resolver settles — settling can
//! run user microtasks, which can `import()` again, which re-enters the
//! map. Every drain below takes its vec out, merges survivors back, and
//! never holds a borrow across a resolve/reject/checkpoint.

use std::rc::Rc;
use std::task::{Context, Poll};

use super::load::RecursiveLoad;
use super::map::{DynImportEvaluation, ModuleMap, module_map_from_scope};
use super::{ModuleError, ModuleEvalError, ModuleId, ModuleLoaderError};

/// The host callback V8 invokes for `import(specifier)`.
///
/// SAFETY: V8 calls this under the API-callback contract (valid
/// scope/context, on the isolate's thread). No panics cross the boundary —
/// a missing module-map slot (modules never installed) yields a rejected
/// TypeError, not a crash.
pub fn import_module_dynamically<'s, 'i>(
    scope: &mut v8::PinScope<'s, 'i>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let Some(map) = module_map_from_scope(scope) else {
        let resolver = v8::PromiseResolver::new(scope)?;
        let promise = resolver.get_promise(scope);
        let msg = v8::String::new(scope, "Dynamic import is not available").unwrap();
        resolver
            .reject(scope, v8::Exception::type_error(scope, msg))
            .unwrap();
        return Some(promise);
    };

    let specifier_str = specifier.to_rust_string_lossy(scope);
    let referrer = resource_name.to_rust_string_lossy(scope);

    let resolver = v8::PromiseResolver::new(scope)?;
    let promise = resolver.get_promise(scope);

    let resolved = map.loader().resolve(&specifier_str, &referrer);
    let resolved_spec = match resolved {
        Ok(spec) => spec,
        Err(e) => {
            // Chrome's face: a bad specifier is a TypeError at the import
            // site, catchable by user code.
            let msg = v8::String::new(scope, e.message()).unwrap();
            resolver
                .reject(scope, v8::Exception::type_error(scope, msg))
                .unwrap();
            return Some(promise);
        }
    };

    // Fast path: the module is already loaded and fully evaluated — settle
    // synchronously, no async machinery. A module still under TLA
    // evaluation parks the resolver as a waiter instead.
    if let Some(id) = map.get_id(resolved_spec.as_str()) {
        let module = map
            .get_handle(id)
            .map(|h| v8::Local::new(scope, h))
            .expect("registered id has a handle");
        if module.get_status() == v8::ModuleStatus::Evaluated {
            let has_pending_tla = map
                .pending_dyn_evaluations
                .borrow()
                .iter()
                .any(|pending| pending.module_id == id);
            if has_pending_tla {
                map.pending_tla_waiters
                    .borrow_mut()
                    .entry(id)
                    .or_default()
                    .push(v8::Global::new(scope, resolver));
                return Some(promise);
            }
            let namespace = module.get_module_namespace();
            resolver.resolve(scope, namespace).unwrap();
            return Some(promise);
        }
    }

    let load =
        RecursiveLoad::new_dynamic(map.clone(), &specifier_str, &referrer, Ok(resolved_spec));
    map.dynamic_import_map
        .borrow_mut()
        .insert(load.id(), v8::Global::new(scope, resolver));
    map.pending_dynamic_loads.borrow_mut().push(load);
    Some(promise)
}

impl ModuleMap {
    /// One module-progress pass over dynamic imports: drain ready load
    /// steps, settle finished evaluations. Loops while settling freed
    /// dependent imports (upstream's `has_evaluated` loop — an import that
    /// only becomes resolvable after another import's evaluation settles
    /// must not wait a whole loop turn).
    pub(crate) fn poll_progress(
        self: &Rc<Self>,
        cx: &mut Context<'_>,
        scope: &mut v8::PinScope,
    ) -> Result<(), ModuleEvalError> {
        // Registered unconditionally, like upstream: module-side wakeups
        // (TLA handlers) need the loop to re-poll.
        *self.waker.borrow_mut() = Some(cx.waker().clone());

        let mut has_evaluated = true;
        while has_evaluated {
            has_evaluated = false;
            self.drain_dynamic_loads(cx, scope)?;
            if self.evaluate_dyn_imports(scope) {
                has_evaluated = true;
            }
        }
        Ok(())
    }

    /// Drive every in-flight dynamic-import load one step. Load futures
    /// are polled right here (the loop's poll — never a spawned task).
    fn drain_dynamic_loads(
        self: &Rc<Self>,
        cx: &mut Context<'_>,
        scope: &mut v8::PinScope,
    ) -> Result<(), ModuleEvalError> {
        if self.pending_dynamic_loads.borrow().is_empty() {
            return Ok(());
        }
        let mut loads = std::mem::take(&mut *self.pending_dynamic_loads.borrow_mut());
        // Survivors (and loads spawned re-entrantly during this drain,
        // which land in the live vec) merge back afterwards.
        let mut keep: Vec<RecursiveLoad> = Vec::new();
        // A terminate interrupt kills the load being processed (its stream
        // errored — same as upstream's errored load, not retried) but must
        // NOT vaporize the rest of the in-flight graphs: the #226 law,
        // module edition. Break out, re-queue the survivors, then surface.
        let terminated = 'terminated: {
            // NOT `loads.drain(..)`: a full-range Drain dropped on early
            // exit deletes the un-yielded remainder (std law, easy to
            // miss — probed). Popping keeps the rest owned by `loads`.
            while !loads.is_empty() {
                let mut load = loads.remove(0);
                match load.poll_next(cx) {
                    Poll::Pending => keep.push(load),
                    Poll::Ready(Some(Ok((url, code)))) => {
                        v8::tc_scope!(let tc, scope);
                        match load.register(tc, &url, &code) {
                            Ok(_) => keep.push(load),
                            Err(ModuleError::Exception(exception)) => {
                                let exception = v8::Local::new(tc, exception);
                                self.dynamic_import_reject(tc, load.id(), exception);
                            }
                            Err(ModuleError::Loader(e)) => {
                                self.dynamic_import_reject_loader(tc, load.id(), &e);
                            }
                            Err(ModuleError::Terminated) => {
                                // The dead load's user promise can never
                                // settle — drop its bookkeeping so the
                                // host loop's pending count doesn't pump
                                // forever (the promise hangs; the interrupt
                                // is reported through the host error face).
                                self.dynamic_import_map.borrow_mut().remove(&load.id());
                                break 'terminated true;
                            }
                        }
                    }
                    Poll::Ready(Some(Err(e))) => {
                        v8::tc_scope!(let tc, scope);
                        self.dynamic_import_reject_loader(tc, load.id(), &e);
                    }
                    Poll::Ready(None) => {
                        // Graph complete: instantiate, then evaluate.
                        let root = load.root_module_id().expect("root registered before Done");
                        v8::tc_scope!(let tc, scope);
                        if let Err(e) = self.instantiate_module(tc, root) {
                            match e {
                                ModuleError::Exception(exception) => {
                                    let exception = v8::Local::new(tc, exception);
                                    self.dynamic_import_reject(tc, load.id(), exception);
                                }
                                ModuleError::Terminated | ModuleError::Loader(_) => {
                                    self.dynamic_import_map.borrow_mut().remove(&load.id());
                                    break 'terminated true;
                                }
                            }
                            continue;
                        }
                        let state_resolver =
                            self.dynamic_import_map.borrow().get(&load.id()).cloned();
                        if let Some(resolver) = state_resolver {
                            if self
                                .dynamic_import_module_evaluate(tc, root, load.id(), resolver)
                                .is_err()
                            {
                                // Only Terminated lands here — same law as
                                // above: re-queue the survivors, surface.
                                break 'terminated true;
                            }
                        }
                    }
                }
            }
            false
        };
        {
            let mut pending = self.pending_dynamic_loads.borrow_mut();
            pending.extend(keep);
            pending.extend(loads);
        }
        if terminated {
            return Err(ModuleEvalError::Terminated);
        }
        Ok(())
    }

    /// Evaluate a dynamically imported module. The TLA promise gets a
    /// wake handler (so the loop re-polls) and lands in the evaluation
    /// list, settled by [`Self::evaluate_dyn_imports`].
    fn dynamic_import_module_evaluate(
        self: &Rc<Self>,
        scope: &mut v8::PinScope,
        id: ModuleId,
        load_id: super::ModuleLoadId,
        _resolver: v8::Global<v8::PromiseResolver>,
    ) -> Result<(), ModuleEvalError> {
        let module = self
            .get_handle(id)
            .map(|h| v8::Local::new(scope, h))
            .expect("registered id has a handle");
        if !matches!(
            module.get_status(),
            v8::ModuleStatus::Instantiated | v8::ModuleStatus::Evaluated
        ) {
            return Ok(());
        }

        v8::tc_scope!(let tc, scope);
        let maybe_value = module.evaluate(tc);
        if let Some(value) = maybe_value {
            let promise = v8::Local::<v8::Promise>::try_from(value)
                .expect("module evaluation result is a promise under TLA");

            fn wake_module(
                scope: &mut v8::PinScope<'_, '_>,
                _args: v8::FunctionCallbackArguments<'_>,
                _rv: v8::ReturnValue,
            ) {
                if let Some(map) = module_map_from_scope(scope) {
                    map.wake();
                }
            }
            let wake = v8::FunctionBuilder::<v8::Function>::new(wake_module).build(tc);
            if let Some(wake) = wake {
                promise.then2(tc, wake, wake);
            }
            self.pending_dyn_evaluations
                .borrow_mut()
                .push(DynImportEvaluation {
                    load_id,
                    module_id: id,
                    promise: v8::Global::new(tc, promise),
                });
        } else if tc.has_terminated() || tc.is_execution_terminating() {
            return Err(ModuleEvalError::Terminated);
        } else {
            // Sync failure outside the TLA promise: the module is Errored
            // and V8's own import machinery rejects the user promise with
            // the module's exception — nothing to do here (upstream's
            // assert arm).
        }
        Ok(())
    }

    /// Settle dynamic imports whose evaluation promise completed. Returns
    /// whether anything settled (the poll_progress loop's fuel).
    fn evaluate_dyn_imports(self: &Rc<Self>, scope: &mut v8::PinScope) -> bool {
        if self.pending_dyn_evaluations.borrow().is_empty() {
            return false;
        }
        let pending = std::mem::take(&mut *self.pending_dyn_evaluations.borrow_mut());
        let mut still = Vec::new();
        let mut resolved_any = false;
        for eval in pending {
            let promise = eval.promise.open(scope);
            match promise.state() {
                v8::PromiseState::Pending => still.push(eval),
                v8::PromiseState::Fulfilled => {
                    resolved_any = true;
                    self.dynamic_import_resolve(scope, eval.load_id, eval.module_id);
                    self.resolve_tla_waiters(scope, eval.module_id);
                }
                v8::PromiseState::Rejected => {
                    resolved_any = true;
                    let exception = v8::Global::new(scope, promise.result(scope));
                    self.dynamic_import_reject(
                        scope,
                        eval.load_id,
                        v8::Local::new(scope, exception.clone()),
                    );
                    self.reject_tla_waiters(scope, eval.module_id, exception);
                }
            }
        }
        self.pending_dyn_evaluations.borrow_mut().extend(still);
        resolved_any
    }

    /// Resolve a finished dynamic import with the module namespace.
    fn dynamic_import_resolve(
        self: &Rc<Self>,
        scope: &mut v8::PinScope,
        load_id: super::ModuleLoadId,
        module_id: ModuleId,
    ) {
        let Some(resolver) = self.dynamic_import_map.borrow_mut().remove(&load_id) else {
            return;
        };
        let resolver = resolver.open(scope);
        let Some(module) = self.get_handle(module_id).map(|h| v8::Local::new(scope, h)) else {
            return;
        };
        // No map borrows held across the resolve: settling can run user
        // microtasks that import() again (upstream's re-entrancy law).
        let namespace = module.get_module_namespace();
        resolver.resolve(scope, namespace).unwrap();
        scope.perform_microtask_checkpoint();
    }

    /// Reject a dynamic import with a live exception (verbatim — compile
    /// and evaluation errors keep their JS identity).
    fn dynamic_import_reject(
        self: &Rc<Self>,
        scope: &mut v8::PinScope,
        load_id: super::ModuleLoadId,
        exception: v8::Local<v8::Value>,
    ) {
        let Some(resolver) = self.dynamic_import_map.borrow_mut().remove(&load_id) else {
            return;
        };
        let resolver = resolver.open(scope);
        resolver.reject(scope, exception).unwrap();
        scope.perform_microtask_checkpoint();
    }

    /// Reject with a message-shaped failure as a TypeError — Chrome's
    /// face for resolution and network failures of `import()`.
    fn dynamic_import_reject_loader(
        self: &Rc<Self>,
        scope: &mut v8::PinScope,
        load_id: super::ModuleLoadId,
        error: &ModuleLoaderError,
    ) {
        let msg = v8::String::new(scope, error.message()).unwrap();
        let exception = v8::Exception::type_error(scope, msg);
        self.dynamic_import_reject(scope, load_id, exception);
    }

    /// Duplicate imports waiting on a module's TLA: resolve them all.
    fn resolve_tla_waiters(self: &Rc<Self>, scope: &mut v8::PinScope, module_id: ModuleId) {
        let waiters = self.pending_tla_waiters.borrow_mut().remove(&module_id);
        let Some(waiters) = waiters else { return };
        let Some(module) = self.get_handle(module_id).map(|h| v8::Local::new(scope, h)) else {
            return;
        };
        let namespace = module.get_module_namespace();
        for resolver in waiters {
            resolver.open(scope).resolve(scope, namespace).unwrap();
        }
        scope.perform_microtask_checkpoint();
    }

    fn reject_tla_waiters(
        self: &Rc<Self>,
        scope: &mut v8::PinScope,
        module_id: ModuleId,
        exception: v8::Global<v8::Value>,
    ) {
        let waiters = self.pending_tla_waiters.borrow_mut().remove(&module_id);
        let Some(waiters) = waiters else { return };
        let exception = v8::Local::new(scope, exception);
        for resolver in waiters {
            resolver.open(scope).reject(scope, exception).unwrap();
        }
        scope.perform_microtask_checkpoint();
    }
}
