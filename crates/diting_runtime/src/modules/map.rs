//! The module map — module identity, compile-time import pre-resolution,
//! and V8's instantiation callbacks.
//!
//! Mirrors deno_core 0.411 `modules/map.rs` + `module_map_data.rs`, with
//! the weight we don't carry cut away (each cut noted): no module types
//! beyond JavaScript (no JSON/text/bytes/wasm synthetic modules), no
//! redirect aliasing (`module_url_found` — our loader keys under the
//! specified URL), no code cache, no source-map extraction, no lazy/side
//! module registries, no import-graph instrumentation, no snapshot
//! sidecar, no import attributes (parsed and ignored — the spec-level
//! `with { type: "json" }` gate would reject as a loader error long before
//! here, same as a browser refusing the MIME), no import phases
//! (`import.source`/`import.defer`).
//!
//! The instantiate-time resolve callback answers from the COMPILE-TIME
//! pre-resolved request table ([`ModuleInfo::requests`]) — the loader is
//! not consulted during instantiation. Upstream's
//! `LoadingInternalModulesGuard`/ext-scheme validation goes with the ext
//! machinery; upstream's transient `*const Self` slot annex (multi-realm
//! isolates sharing a map per realm) collapses into our permanent
//! `Rc<ModuleMap>` isolate slot — one context per Vm, so the map IS the
//! isolate's map, and the annex's unsound-looking raw pointer disappears.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::task::Waker;

use super::load::RecursiveLoad;
use super::{ModuleId, ModuleLoadId, ModuleLoader, ModuleLoaderError};

/// One pre-resolved import edge, captured at compile time.
#[derive(Debug, Clone)]
pub(crate) struct PreResolvedRequest {
    /// The specifier EXACTLY as written in the source (the lookup key V8
    /// hands back at instantiation).
    pub(crate) specifier_key: String,
    /// The URL that specifier resolved to through the loader.
    pub(crate) resolved: url::Url,
}

/// Bookkeeping for one compiled module.
#[derive(Debug, Clone)]
pub(crate) struct ModuleInfo {
    pub(crate) name: String,
    pub(crate) requests: Vec<PreResolvedRequest>,
}

struct ModuleMapData {
    /// The compiled modules, index = ModuleId.
    handles: Vec<v8::Global<v8::Module>>,
    /// Inverted index: module handle → its id.
    handles_inverted: HashMap<v8::Global<v8::Module>, usize>,
    info: Vec<ModuleInfo>,
    /// Registered name → id. Upstream keeps a SymbolicModule alias chain
    /// here for redirects; without aliasing a plain map is the truth.
    by_name: HashMap<String, ModuleId>,
    main_module_id: Option<ModuleId>,
}

impl ModuleMapData {
    fn create_module_info(
        &mut self,
        name: String,
        handle: v8::Global<v8::Module>,
        main: bool,
        requests: Vec<PreResolvedRequest>,
    ) -> ModuleId {
        let id = self.handles.len();
        self.handles_inverted.insert(handle.clone(), id);
        self.handles.push(handle);
        self.info.push(ModuleInfo {
            name: name.clone(),
            requests,
        });
        self.by_name.insert(name, id);
        if main {
            self.main_module_id = Some(id);
        }
        id
    }
}

/// A dynamic import whose module is evaluating under top-level await:
/// the promise here is V8's INTERNAL evaluation promise (not the user's
/// import promise) — settled by [`super::eval`]'s wake handlers.
pub(crate) struct DynImportEvaluation {
    pub(crate) load_id: ModuleLoadId,
    pub(crate) module_id: ModuleId,
    pub(crate) promise: v8::Global<v8::Promise>,
}

/// The isolate's module state. One per Vm, stored in an isolate slot as
/// `Rc<ModuleMap>` — every V8 callback (instantiate resolve, import.meta,
/// dynamic import) recovers it from there with no closure capture.
pub struct ModuleMap {
    loader: Rc<dyn ModuleLoader>,
    data: RefCell<ModuleMapData>,
    next_load_id: Cell<i32>,
    /// Dynamic imports that have not settled: load id → the user's
    /// promise resolver.
    pub(crate) dynamic_import_map: RefCell<HashMap<ModuleLoadId, v8::Global<v8::PromiseResolver>>>,
    /// In-flight dynamic-import graph loads.
    pub(crate) pending_dynamic_loads: RefCell<Vec<RecursiveLoad>>,
    /// Dynamic imports whose module evaluation promise is pending (TLA).
    pub(crate) pending_dyn_evaluations: RefCell<Vec<DynImportEvaluation>>,
    /// Resolvers waiting on a module whose TLA evaluation is still pending
    /// (duplicate `import()` of an evaluating module).
    pub(crate) pending_tla_waiters:
        RefCell<HashMap<ModuleId, Vec<v8::Global<v8::PromiseResolver>>>>,
    /// A static (non-dynamic) module evaluation is in flight — its then2
    /// handlers clear this. The host's loop condition reads it.
    pub(crate) pending_mod_evaluation: Cell<bool>,
    /// The host loop's waker, registered on every module-progress poll.
    /// Module-side completions (TLA handlers, evaluation wakeups) wake it
    /// so the loop re-polls.
    pub(crate) waker: RefCell<Option<Waker>>,
}

impl ModuleMap {
    pub fn new(loader: Rc<dyn ModuleLoader>) -> Self {
        Self {
            loader,
            data: RefCell::new(ModuleMapData {
                handles: Vec::new(),
                handles_inverted: HashMap::new(),
                info: Vec::new(),
                by_name: HashMap::new(),
                main_module_id: None,
            }),
            next_load_id: Cell::new(0),
            dynamic_import_map: RefCell::new(HashMap::new()),
            pending_dynamic_loads: RefCell::new(Vec::new()),
            pending_dyn_evaluations: RefCell::new(Vec::new()),
            pending_tla_waiters: RefCell::new(HashMap::new()),
            pending_mod_evaluation: Cell::new(false),
            waker: RefCell::new(None),
        }
    }

    pub(crate) fn loader(&self) -> &Rc<dyn ModuleLoader> {
        &self.loader
    }

    /// Resolve a specifier through the loader (import-map and base-url
    /// territory). The public face for graph loading; instantiation never
    /// calls this (pre-resolved table).
    pub(crate) fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<url::Url, ModuleLoaderError> {
        self.loader.resolve(specifier, referrer)
    }

    pub(crate) fn next_load_id(&self) -> ModuleLoadId {
        let id = self.next_load_id.get();
        self.next_load_id.set(id + 1);
        id
    }

    /// Module id by registered name (the map is the graph's registry).
    pub(crate) fn get_id(&self, name: &str) -> Option<ModuleId> {
        self.data.borrow().by_name.get(name).copied()
    }

    pub(crate) fn get_handle(&self, id: ModuleId) -> Option<v8::Global<v8::Module>> {
        self.data.borrow().handles.get(id).cloned()
    }

    pub(crate) fn get_name_by_id(&self, id: ModuleId) -> Option<String> {
        self.data.borrow().info.get(id).map(|i| i.name.clone())
    }

    /// A module's pre-resolved import edges (the recursive loader's walk
    /// list).
    pub(crate) fn get_requests(&self, id: ModuleId) -> Option<Vec<PreResolvedRequest>> {
        self.data.borrow().info.get(id).map(|i| i.requests.clone())
    }

    pub(crate) fn get_name_by_module(&self, module: &v8::Global<v8::Module>) -> Option<String> {
        let data = self.data.borrow();
        data.handles_inverted
            .get(module)
            .and_then(|id| data.info.get(*id).map(|i| i.name.clone()))
    }

    /// Compile a module's source and register it, PRE-RESOLVING every
    /// import edge. The heart of the design: `get_module_requests()` hands
    /// us the parsed specifiers at compile time; each resolves through the
    /// loader NOW, and the resolved URL is what the recursive loader
    /// fetches and what the instantiate callback answers with.
    ///
    /// A name that already has a module returns its id unchanged (the
    /// in-flight dedup law — the second fetch of a concurrent import
    /// collapses into the first's registration).
    pub(crate) fn new_module_from_js_source(
        &self,
        scope: &mut v8::PinScope,
        main: bool,
        name: &str,
        code: &str,
    ) -> Result<ModuleId, super::ModuleError> {
        // Dedup is UNCONDITIONAL, main included. Upstream errors on a
        // second main load (MainModuleAlreadyExists — an embedder-bug
        // tripwire in a load-once world); our side-load face is
        // idempotent instead: the same name re-claims the same module
        // and re-points main_module_id, never double-registers. Source
        // drift on re-load is the caller's contract — upstream's non-main
        // dedup doesn't compare source either.
        if let Some(id) = self.get_id(name) {
            if main {
                self.data.borrow_mut().main_module_id = Some(id);
            }
            return Ok(id);
        }

        let name_str = v8::String::new(scope, name).expect("module name fits");
        let source_str = v8::String::new(scope, code).expect("module source fits");
        // is_module = true: the origin is what makes script_compiler treat
        // this as ESM (imports legal, strict mode implied).
        let origin = v8::ScriptOrigin::new(
            scope,
            name_str.into(),
            0,
            0,
            false,
            0,
            None,
            false,
            false,
            true,
            None,
        );

        v8::tc_scope!(let tc, scope);
        let mut source = v8::script_compiler::Source::new(source_str, Some(&origin));
        let maybe_module = v8::script_compiler::compile_module(tc, &mut source);
        if tc.has_caught() {
            assert!(maybe_module.is_none());
            if tc.has_terminated() || tc.is_execution_terminating() {
                // The watchdog interrupt has no exception object — the
                // compile itself was killed mid-flight.
                return Err(super::ModuleError::Terminated);
            }
            let exception = tc.exception().unwrap();
            return Err(super::ModuleError::Exception(v8::Global::new(
                tc, exception,
            )));
        }
        let module = maybe_module.expect("compile succeeded with no module");

        // Pre-resolve every import edge. NOTE: import attributes are
        // available on each ModuleRequest and deliberately ignored — a
        // `with { type: ... }` import rides the same JavaScript path (our
        // loader MIME-gates at fetch, which is where a browser rejects a
        // wrong type anyway).
        let requests_fixed = module.get_module_requests();
        let mut requests = Vec::with_capacity(requests_fixed.length() as usize);
        for i in 0..requests_fixed.length() {
            let module_request =
                v8::Local::<v8::ModuleRequest>::try_from(requests_fixed.get(tc, i).unwrap())
                    .unwrap();
            let specifier_key = module_request.get_specifier().to_rust_string_lossy(tc);
            let resolved = match self.loader.resolve(&specifier_key, name) {
                Ok(u) => u,
                Err(e) => {
                    // Leave no pending exception behind for a resolution
                    // failure (there is none — resolve is pure Rust), and
                    // surface as a message-shaped error.
                    return Err(super::ModuleError::Loader(e));
                }
            };
            requests.push(PreResolvedRequest {
                specifier_key,
                resolved,
            });
        }

        let handle = v8::Global::new(tc, module);
        let id =
            self.data
                .borrow_mut()
                .create_module_info(name.to_string(), handle, main, requests);
        Ok(id)
    }

    /// Instantiate a registered module. The V8-facing resolve callback
    /// answers every edge from the pre-resolved table; a module that is
    /// already instantiated (or beyond) is a no-op — the dedup law again,
    /// here for the instantiate phase.
    pub(crate) fn instantiate_module(
        &self,
        scope: &mut v8::PinScope,
        id: ModuleId,
    ) -> Result<(), super::ModuleError> {
        v8::tc_scope!(let tc, scope);

        let module = self
            .get_handle(id)
            .map(|handle| v8::Local::new(tc, handle))
            .expect("module id is registered");

        if module.get_status() == v8::ModuleStatus::Errored {
            return Err(super::ModuleError::Exception(v8::Global::new(
                tc,
                module.get_exception(),
            )));
        }
        if module.get_status() != v8::ModuleStatus::Uninstantiated {
            return Ok(());
        }

        let instantiate_result = module.instantiate_module(tc, Self::module_resolve_callback);
        if instantiate_result.is_none() {
            if tc.has_terminated() || tc.is_execution_terminating() {
                return Err(super::ModuleError::Terminated);
            }
            let exception = tc.exception().unwrap();
            return Err(super::ModuleError::Exception(v8::Global::new(
                tc, exception,
            )));
        }
        Ok(())
    }

    /// V8's instantiate-time resolve hook: (context, specifier,
    /// import_attributes, referrer) → the referrer's pre-resolved module
    /// for that specifier. Answers from the compile-time table — O(1), no
    /// loader call. A miss is a broken graph invariant, thrown as
    /// TypeError (upstream's face for the same condition).
    fn module_resolve_callback<'s>(
        context: v8::Local<'s, v8::Context>,
        specifier: v8::Local<'s, v8::String>,
        _import_attributes: v8::Local<'s, v8::FixedArray>,
        referrer: v8::Local<'s, v8::Module>,
    ) -> Option<v8::Local<'s, v8::Module>> {
        // SAFETY: CallbackScope can be safely constructed from Local<Context>
        v8::callback_scope!(unsafe scope, context);
        let module_map = module_map_from(scope);

        let referrer_global = v8::Global::new(scope, referrer);
        let specifier_str = specifier.to_rust_string_lossy(scope);

        let referrer_name = module_map
            .as_ref()
            .and_then(|m| m.get_name_by_module(&referrer_global));

        // The pre-resolved table answers directly; the registry lookup rides
        // the same Option chain. A miss anywhere is a broken graph invariant
        // (compile pre-resolves every edge the loader accepted).
        let module = module_map.as_ref().and_then(|module_map| {
            let data = module_map.data.borrow();
            let info = data
                .handles_inverted
                .get(&referrer_global)
                .and_then(|id| data.info.get(*id))?;
            let request = info
                .requests
                .iter()
                .find(|r| r.specifier_key == specifier_str)?;
            let id = module_map.get_id(request.resolved.as_str())?;
            module_map.get_handle(id)
        });

        let Some(module) = module else {
            let msg = v8::String::new(
                scope,
                &format!(
                    r#"Cannot resolve module "{specifier_str}" from "{}""#,
                    referrer_name.as_deref().unwrap_or("<unknown>")
                ),
            )
            .unwrap();
            scope.throw_exception(v8::Exception::type_error(scope, msg));
            return None;
        };
        Some(v8::Local::new(scope, module))
    }

    /// Wake the host loop (module-side completion: TLA handler ran,
    /// evaluation promise settled).
    pub(crate) fn wake(&self) {
        if let Some(waker) = self.waker.borrow_mut().take() {
            waker.wake();
        }
    }

    pub fn has_pending_dynamic_imports(&self) -> bool {
        !self.pending_dynamic_loads.borrow().is_empty()
            || !self.pending_dyn_evaluations.borrow().is_empty()
            || !self.pending_tla_waiters.borrow().is_empty()
            || !self.dynamic_import_map.borrow().is_empty()
    }

    pub fn has_pending_module_evaluation(&self) -> bool {
        self.pending_mod_evaluation.get()
    }
}

/// The isolate slot type: callbacks recover the map with no closure
/// capture, same trick as the op driver's slot.
pub(crate) type ModuleSlot = Rc<ModuleMap>;

pub(crate) fn module_map_from(scope: &mut v8::PinCallbackScope<'_, '_>) -> Option<Rc<ModuleMap>> {
    scope.get_slot::<ModuleSlot>().cloned()
}

/// The PinScope flavor — trait-shape callbacks (Function builders, the
/// dynamic-import host hook) and Vm-side code read the slot this way.
pub(crate) fn module_map_from_scope(scope: &v8::PinScope) -> Option<Rc<ModuleMap>> {
    scope.get_slot::<ModuleSlot>().cloned()
}

/// `import.meta` initialization: url + main, the browser's face. Upstream
/// adds a wasm-instantiation hook (no wasm face here) and node:vm
/// delegation (no vm here). Called once per module, on first access.
pub extern "C" fn initialize_import_meta_object_callback(
    context: v8::Local<v8::Context>,
    module: v8::Local<v8::Module>,
    meta: v8::Local<v8::Object>,
) {
    // SAFETY: CallbackScope can be safely constructed from Local<Context>
    v8::callback_scope!(unsafe scope, context);
    let Some(module_map) = module_map_from(scope) else {
        return;
    };
    let module_global = v8::Global::new(scope, module);
    let Some(name) = module_map.get_name_by_module(&module_global) else {
        return;
    };
    let main = module_map
        .data
        .borrow()
        .main_module_id
        .map(|id| module_map.get_name_by_id(id) == Some(name.clone()))
        .unwrap_or(false);

    let url_key = v8::String::new(scope, "url").unwrap();
    let url_val = v8::String::new(scope, &name).unwrap();
    meta.create_data_property(scope, url_key.into(), url_val.into());

    let main_key = v8::String::new(scope, "main").unwrap();
    let main_val = v8::Boolean::new(scope, main);
    meta.create_data_property(scope, main_key.into(), main_val.into());
}
