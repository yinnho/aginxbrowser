//! Recursive module graph loading — the parallel fetch state machine.
//!
//! deno_core's `recursive_load.rs` is a futures-util `Stream`
//! (`FuturesUnordered` + `StreamExt`); ours is the same state machine in
//! std shape: a Vec of pinned load futures polled in order, first-ready
//! wins, per-load `visited` set. The states and the two-case recursion law
//! (already-registered → recurse synchronously deeper; not registered →
//! spawn a load future) are upstream's, verbatim in behavior.
//!
//! Dropped from upstream: `prepare_load` (node resolution), lazy/side/synthetic
//! source registries, redirect aliasing (`visited_as_alias`), source-mapped
//! referrer line/column, import phases. A side load may carry the ROOT
//! source inline (`load_side_es_module_from_code` — the page's inline
//! module runs under the document URL while its imports fetch normally).

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::map::{ModuleMap, PreResolvedRequest};
use super::{ModuleError, ModuleId, ModuleLoaderError, ModuleSpecifier};

type ModuleLoadFuture = Pin<Box<dyn Future<Output = Result<ModuleSourceItem, ModuleLoaderError>>>>;

/// What a load future yields: the resolved URL it fetched, and its body.
/// (Upstream's future yields `Option` — `None` marks an already-visited
/// redirect; without aliasing there is no `None` case.)
pub(crate) type ModuleSourceItem = (ModuleSpecifier, String);

#[derive(Debug, PartialEq)]
enum LoadState {
    Init,
    LoadingRoot,
    LoadingImports,
    Done,
}

pub(crate) struct RecursiveLoad {
    id: i32,
    /// Dynamic-import roots never register as the main module (upstream's
    /// `is_dynamic` law — `import.meta.main` is the document's own module).
    is_dynamic: bool,
    root_module_id: Option<ModuleId>,
    resolved_specifier: Option<Result<ModuleSpecifier, ModuleLoaderError>>,
    /// The root module's source, when the embedder supplied it inline
    /// (side loads). Root fetch is skipped; imports fetch normally.
    root_code: Option<String>,
    state: LoadState,
    loader: std::rc::Rc<dyn super::ModuleLoader>,
    map: std::rc::Rc<ModuleMap>,
    pending: Vec<ModuleLoadFuture>,
    visited: HashSet<ModuleSpecifier>,
}

impl RecursiveLoad {
    /// A side load: root resolved through the loader against `"."` (the
    /// embedder's root face — import maps must not remap the page's own
    /// module URL), optionally with inline root code.
    pub(crate) fn new_side(
        map: std::rc::Rc<ModuleMap>,
        specifier: &str,
        root_code: Option<String>,
    ) -> Self {
        let resolved = map.resolve(specifier, ".");
        Self::with_resolved(map, resolved, root_code)
    }

    /// A dynamic import: root resolved against the importing module's URL.
    pub(crate) fn new_dynamic(
        map: std::rc::Rc<ModuleMap>,
        _specifier: &str,
        _referrer: &str,
        resolved: Result<ModuleSpecifier, ModuleLoaderError>,
    ) -> Self {
        let mut load = Self::with_resolved(map, resolved, None);
        load.is_dynamic = true;
        load
    }

    fn with_resolved(
        map: std::rc::Rc<ModuleMap>,
        resolved: Result<ModuleSpecifier, ModuleLoaderError>,
        root_code: Option<String>,
    ) -> Self {
        let loader = map.loader().clone();
        let root_module_id = resolved
            .as_ref()
            .ok()
            .and_then(|spec| map.get_id(spec.as_str()));
        Self {
            id: map.next_load_id(),
            is_dynamic: false,
            root_module_id,
            resolved_specifier: Some(resolved),
            root_code,
            state: LoadState::Init,
            loader,
            map,
            pending: Vec::new(),
            visited: HashSet::new(),
        }
    }

    pub(crate) fn id(&self) -> i32 {
        self.id
    }

    pub(crate) fn root_module_id(&self) -> Option<ModuleId> {
        self.root_module_id
    }

    /// Compile a fetched source into the map (dedup: a name that already
    /// has a module returns its id — concurrent duplicate fetches collapse
    /// into the first registration), then recurse the module's imports:
    /// registered deps recurse synchronously deeper, unregistered ones
    /// spawn load futures. Ensures the whole graph is in the map before
    /// `Done`.
    pub(crate) fn register(
        &mut self,
        scope: &mut v8::PinScope,
        specifier: &ModuleSpecifier,
        code: &str,
    ) -> Result<ModuleId, ModuleError> {
        let is_root = self.state == LoadState::LoadingRoot;
        let module_id = self.map.new_module_from_js_source(
            scope,
            // The load's root IS the main module — for side loads. A
            // dynamic import's root is just an import target.
            is_root && !self.is_dynamic,
            specifier.as_str(),
            code,
        )?;
        if is_root {
            self.root_module_id = Some(module_id);
            self.state = LoadState::LoadingImports;
        }
        self.recurse_imports(module_id);
        if self.pending.is_empty() {
            self.state = LoadState::Done;
        }
        Ok(module_id)
    }

    /// Upstream's `register_and_recurse_inner`: BFS over the module's
    /// (and, synchronously, its already-registered dependents') requests.
    fn recurse_imports(&mut self, root_id: ModuleId) {
        let mut queue = VecDeque::new();
        queue.push_back(root_id);
        while let Some(module_id) = queue.pop_front() {
            // The registered module's canonical name is the referrer its
            // imports already resolved against at compile time; here we
            // only need the resolved URLs to fetch.
            let requests: Vec<PreResolvedRequest> =
                self.map.get_requests(module_id).unwrap_or_default();
            for request in requests {
                if self.visited.contains(&request.resolved) {
                    continue;
                }
                self.visited.insert(request.resolved.clone());
                match self.map.get_id(request.resolved.as_str()) {
                    Some(dep_id) => queue.push_back(dep_id),
                    None => {
                        let loader = self.loader.clone();
                        let resolved = request.resolved.clone();
                        let fut = async move {
                            match loader.load(&resolved, None) {
                                super::ModuleLoadResponse::Sync(result) => result,
                                super::ModuleLoadResponse::Async(fut) => fut.await,
                            }
                            .map(|source| (resolved.clone(), source.code))
                        };
                        self.pending.push(Box::pin(fut));
                    }
                }
            }
        }
    }

    /// One stream step, hand-rolled: yields the next fetched source, the
    /// terminal `None`, or a load failure. Mirrors upstream's
    /// `Stream::poll_next` transition table exactly (including the
    /// re-poll after a root dedup and after a visited-redirect future).
    pub(crate) fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<ModuleSourceItem, ModuleLoaderError>>> {
        match self.state {
            LoadState::Init => {
                let resolved = self
                    .resolved_specifier
                    .take()
                    .expect("root specifier is resolved before polling");
                let spec = match resolved {
                    Ok(s) => s,
                    Err(e) => return Poll::Ready(Some(Err(e))),
                };
                self.visited.insert(spec.clone());
                if let Some(code) = self.root_code.take() {
                    // Inline root: yield it straight to the driver for
                    // registration — no fetch.
                    self.state = LoadState::LoadingRoot;
                    return Poll::Ready(Some(Ok((spec, code))));
                }
                if let Some(id) = self.map.get_id(spec.as_str()) {
                    // Root already registered (an earlier load of the same
                    // graph): recurse its imports and keep going.
                    self.root_module_id = Some(id);
                    self.state = LoadState::LoadingImports;
                    self.recurse_imports(id);
                    if self.pending.is_empty() {
                        self.state = LoadState::Done;
                    }
                    return self.poll_next(cx);
                }
                let loader = self.loader.clone();
                let root = spec.clone();
                let fut = async move {
                    match loader.load(&root, None) {
                        super::ModuleLoadResponse::Sync(result) => result,
                        super::ModuleLoadResponse::Async(fut) => fut.await,
                    }
                    .map(|source| (root.clone(), source.code))
                };
                self.pending.push(Box::pin(fut));
                self.state = LoadState::LoadingRoot;
                self.poll_next(cx)
            }
            LoadState::LoadingRoot | LoadState::LoadingImports => {
                let mut i = 0;
                while i < self.pending.len() {
                    if let Poll::Ready(item) = self.pending[i].as_mut().poll(cx) {
                        // The future is spent (it yielded Ready) — remove
                        // and drop it before yielding.
                        drop(self.pending.remove(i));
                        match item {
                            Ok(source) => return Poll::Ready(Some(Ok(source))),
                            Err(e) => return Poll::Ready(Some(Err(e))),
                        }
                    } else {
                        i += 1;
                    }
                }
                // Upstream's FuturesUnordered law, easy to fumble in a
                // hand-rolled loop: futures still in flight (all Pending)
                // means the STREAM is Pending — Ready(None) is exclusively
                // the empty-stream signal (Done). Conflating the two turns
                // a gated load into a rootless Done.
                if self.pending.is_empty() {
                    self.state = LoadState::Done;
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            }
            LoadState::Done => Poll::Ready(None),
        }
    }
}
