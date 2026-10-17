//! The module machinery — ES module compilation, graph loading, dynamic
//! `import()`, and top-level-await evaluation, owned.
//!
//! Batch 10b of the claim campaign. The law is deno_core 0.411's
//! (`modules/map.rs`, `recursive_load.rs`, `map/dynamic.rs`,
//! `map/evaluation.rs`, `runtime/bindings.rs` — studied line by line, helpers
//! copied verbatim where marked); the shape is ours: std-only futures
//! plumbing (no tokio, no futures crate), one context per isolate, no ext
//! machinery. The known dropped weight, each with its reason, is noted at
//! the drop sites; the plan doc holds the full table.
//!
//! The load-bearing design is upstream's and stays: imports are
//! PRE-RESOLVED at compile time. `new_module_from_js_source` compiles the
//! source, walks `get_module_requests()`, and resolves every specifier
//! eagerly into [`ModuleInfo::requests`]; the recursive loader then walks
//! those resolved URLs to fetch the graph, and V8's instantiate-time
//! resolve callback answers from the same table in O(1) — the loader is
//! never called during instantiation (that ordering is what makes redirect
//! aliasing and import maps work at all).

use std::future::Future;
use std::pin::Pin;
use url::Url;

/// Resolved module specifier. Same alias as deno_core
/// (module_specifier.rs: `pub type ModuleSpecifier = Url`) — a URL IS the
/// specifier; 10a made the flip type-identical and this crate keeps it.
pub type ModuleSpecifier = Url;

/// A module's numeric identity. Index into the module map's handle/info
/// vecs; stable for the isolate's lifetime.
pub type ModuleId = usize;

/// A dynamic-import load's identity (one counter, mirroring upstream's
/// `next_load_id`).
pub type ModuleLoadId = i32;

/// The error face loaders speak. deno_core 0.411 aliases this to
/// deno_error's `JsErrorBox`; our embedder only ever constructs
/// `ModuleLoaderError::generic(msg)` and our machinery only ever renders a
/// message (string) or a Chrome-shaped TypeError — so the class machinery
/// would be dead weight. Our own 30-line type; deliberate divergence, the
/// message strings stay identical.
#[derive(Debug, Clone)]
pub struct ModuleLoaderError(String);

impl ModuleLoaderError {
    pub fn generic(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModuleLoaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModuleLoaderError {}

/// A loaded module's body. deno's `ModuleSource` also carries
/// `module_url_found` (redirect aliasing), a module type, and a code-cache
/// slot; our HTTP loader keys everything under the SPECIFIED url (reqwest
/// follows redirects transparently and the module runs under the URL the
/// page asked for — the same posture the current diting loader has), only
/// ever produces JavaScript, and caches nothing. One field, no lies.
#[derive(Debug, Clone)]
pub struct ModuleSource {
    pub code: String,
}

/// What a loader's `load` hands back: bytes now, or a future that produces
/// them. Kept shape-compatible with deno_core's enum so the embedder's
/// loader flip stays mechanical.
pub enum ModuleLoadResponse {
    Sync(Result<ModuleSource, ModuleLoaderError>),
    Async(Pin<Box<dyn Future<Output = Result<ModuleSource, ModuleLoaderError>> + 'static>>),
}

/// The embedder's module face. Two calls, both sync to enter:
/// `resolve` maps a specifier + referrer to a URL (import-map and base-url
/// territory — runs at compile time for every import edge), `load` fetches
/// a resolved URL's body.
///
/// Dropped from deno's face, deliberately: `prepare_load` (node resolution
/// machinery — our loader implements none), import attributes (parsed and
/// ignored — see the compile path), `get_host_defined_options` (ext/node
/// tagging), code-cache hooks, `finish_load` bookkeeping, and the
/// `resolve_with_scope` variant (the scope was only used to build class-y
/// JS errors, which our error face doesn't have).
pub trait ModuleLoader {
    /// Resolve `specifier` as imported from `referrer`.
    ///
    /// The referrer `"."` denotes the root module passed to the side-load
    /// face (a page's `<script type=module src>` resolved against the
    /// document base — import maps must not remap the root).
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<ModuleSpecifier, ModuleLoaderError>;

    /// Fetch a resolved module's source. `maybe_referrer` is the importing
    /// module's URL when one is known (graph loads pass it; root loads
    /// don't) — for diagnostics; policy gates key off the specifier.
    fn load(&self, specifier: &ModuleSpecifier, maybe_referrer: Option<&str>)
    -> ModuleLoadResponse;
}

/// Why a module op failed. `Exception` carries a live V8 exception
/// (compile error, instantiation error, evaluation throw) that must be
/// re-thrown/rejected VERBATIM; `Loader` is a message-shaped failure
/// (resolution, fetch) that renders as a TypeError on the dynamic-import
/// path — Chrome's face for both resolution and network failures.
/// `Terminated` is the watchdog interrupt (no exception object exists).
pub(crate) enum ModuleError {
    Exception(v8::Global<v8::Value>),
    Loader(ModuleLoaderError),
    Terminated,
}

/// Resolves a module using this algorithm:
/// <https://html.spec.whatwg.org/multipage/webappapis.html#resolve-a-module-specifier>
///
/// Verbatim from deno_core 0.411 `module_specifier.rs` (MIT, Copyright
/// 2018-2026 the Deno authors) minus the error enum's field shapes — the
/// branches and their order are the algorithm, and the algorithm is the
/// compatibility.
pub fn resolve_import(specifier: &str, base: &str) -> Result<ModuleSpecifier, url::ParseError> {
    match Url::parse(specifier) {
        // 1. Apply the URL parser to specifier.
        //    If the result is not failure, return the result.
        Ok(url) => Ok(url),

        // 2. If specifier does not start with the character U+002F SOLIDUS (/),
        //    the two-character sequence U+002E FULL STOP, U+002F SOLIDUS (./),
        //    or the three-character sequence U+002E FULL STOP, U+002E FULL STOP,
        //    U+002F SOLIDUS (../), return failure.
        Err(url::ParseError::RelativeUrlWithoutBase)
            if !(specifier.starts_with('/')
                || specifier.starts_with("./")
                || specifier.starts_with("../")) =>
        {
            Err(url::ParseError::RelativeUrlWithoutBase)
        }

        // 3. Return the result of applying the URL parser to specifier with
        //    base URL as the base URL.
        Err(url::ParseError::RelativeUrlWithoutBase) => {
            let base = Url::parse(base)?;
            base.join(specifier)
        }

        // If parsing the specifier as a URL failed for a different reason
        // than it being relative, always return the original error. We don't
        // want to return `ImportPrefixMissing` or `InvalidBaseUrl` if the
        // real problem lies somewhere else.
        Err(err) => Err(err),
    }
}

pub(crate) mod dynamic;
pub(crate) mod eval;
pub(crate) mod load;
pub(crate) mod map;

pub use dynamic::import_module_dynamically;
pub use eval::ModuleEvalError;
pub use map::ModuleMap;
