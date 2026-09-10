use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

use deno_core::error::ModuleLoaderError;
use deno_core::ModuleLoadResponse;
use deno_core::ModuleLoader;
use deno_core::ModuleLoadOptions;
use deno_core::ModuleLoadReferrer;
use deno_core::ModuleSource;
use deno_core::ModuleSourceCode;
use deno_core::ModuleSpecifier;

use crate::diting_js::import_map::ImportMap;

pub struct DitingModuleLoader {
    pub base_url: String,
    /// Proxy URL threaded through to every dynamic ES-module fetch (#139).
    /// `None` keeps the pre-#139 direct-connection behaviour for callers
    /// that haven't been updated.
    pub proxy_url: Option<String>,
    /// Document import maps, shared with JsState and the runtime so parser
    /// scripts, dynamically inserted maps and this loader all see one map.
    pub import_map: Rc<RefCell<ImportMap>>,
    /// Blob bodies mirrored from JS `URL.createObjectURL` (blob URL → bytes +
    /// MIME), shared with JsState. The JS-side `__blobObjs` table is
    /// unreachable from here, so createObjectURL mirrors each entry in via
    /// `op_blob_register` and `import("blob:…")` resolves through this map.
    pub blob_store: Rc<RefCell<HashMap<String, (Vec<u8>, String)>>>,
}

impl DitingModuleLoader {
    #[cfg_attr(not(test), allow(dead_code))] // direct-connection shorthand; runtime.rs uses with_proxy
    pub fn new(base_url: &str) -> Self {
        Self::with_proxy(base_url, None)
    }

    pub fn with_proxy(base_url: &str, proxy_url: Option<String>) -> Self {
        let import_map = Rc::new(RefCell::new(ImportMap::default()));
        let blob_store = Rc::new(RefCell::new(HashMap::new()));
        Self::with_proxy_and_import_map(base_url, proxy_url, import_map, blob_store)
    }

    pub fn with_proxy_and_import_map(
        base_url: &str,
        proxy_url: Option<String>,
        import_map: Rc<RefCell<ImportMap>>,
        blob_store: Rc<RefCell<HashMap<String, (Vec<u8>, String)>>>,
    ) -> Self {
        DitingModuleLoader {
            base_url: base_url.to_string(),
            proxy_url,
            import_map,
            blob_store,
        }
    }
}

fn io_err(msg: String) -> ModuleLoaderError {
    // 0.411 aliases ModuleLoaderError = deno_error 0.7 JsErrorBox (no From<io::Error>).
    ModuleLoaderError::generic(msg)
}

/// The MIME essences HTML calls "JavaScript MIME type". Chromium enforces
/// this allowlist for module scripts (`import()`, `<script type=module>`);
/// an empty essence reads as text/plain and is refused the same way.
/// Classic scripts have no such gate (their network path never checked a
/// content-type either).
fn module_mime_allowed(mime: &str) -> bool {
    matches!(
        mime.trim().to_ascii_lowercase().as_str(),
        "text/javascript" | "application/javascript" | "text/ecmascript" | "application/ecmascript"
    )
}

/// WHATWG percent-decoding with browser leniency: an invalid `%xx` escape
/// passes through literally instead of failing the whole URL.
fn percent_decode(payload: &str) -> Vec<u8> {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = payload.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Decode a data: URL payload flagged `;base64`. Matches the Chromium shape
/// the classic-script decoder already implements: ASCII whitespace stripped,
/// stray `=` before the tail rejected, missing padding tolerated, and a
/// length ≡ 1 (mod 4) is a hard error rather than garbage execution.
fn decode_base64_payload(payload: &str) -> Result<Vec<u8>, ModuleLoaderError> {
    let encoded: String = payload
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let stripped = encoded.trim_end_matches('=');
    if encoded.len() - stripped.len() > 2
        || stripped.len() % 4 == 1
        || !stripped
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return Err(io_err("Invalid base64 payload in module data: URL".to_string()));
    }
    let mut padded = stripped.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    BASE64
        .decode(padded.as_bytes())
        .map_err(|e| io_err(format!("Invalid base64 payload in module data: URL: {}", e)))
}

/// Build a module source from a `data:` URL. Mirrors the JS-side
/// fetch()/classic-script decoders (b12405d, f841205) with the one module-
/// specific addition: the MIME essence must be a JavaScript type.
fn data_module_source(
    module_specifier: &ModuleSpecifier,
    url: &str,
) -> Result<ModuleSource, ModuleLoaderError> {
    let comma = url
        .find(',')
        .ok_or_else(|| io_err(format!("Invalid data: module URL (no comma): {}", url)))?;
    // The scheme is `data:`, so index 5 is the first meta byte.
    let meta = &url[5..comma];
    let after = &url[comma + 1..];
    // A fragment is URL syntax, not payload: strip it before any decoding
    // (a %23 escape inside the payload decodes to a literal '#').
    let payload = match after.find('#') {
        Some(frag) => &after[..frag],
        None => after,
    };
    let mime = meta.split(';').next().unwrap_or("");
    let base64 = meta.split(';').any(|p| p.eq_ignore_ascii_case("base64"));
    if !module_mime_allowed(mime) {
        return Err(io_err(format!(
            "Refused to execute module from {}: MIME type ({}) is not a JavaScript MIME type",
            url,
            if mime.is_empty() {
                "text/plain"
            } else {
                mime
            }
        )));
    }
    let bytes = if base64 {
        decode_base64_payload(payload)?
    } else {
        percent_decode(payload)
    };
    // Same cap as the streaming HTTP path: the page controls module URLs,
    // so a data: URL concatenated to gigabytes must reject, not OOM.
    let limit = crate::diting_js::ops::fetch_body_byte_limit();
    if bytes.len() > limit {
        return Err(io_err(format!(
            "Module {} body exceeds limit of {} bytes",
            url, limit
        )));
    }
    let code = String::from_utf8_lossy(&bytes).into_owned();
    Ok(ModuleSource::new(
        deno_core::ModuleType::JavaScript,
        ModuleSourceCode::String(code.into()),
        module_specifier,
        None,
    ))
}

/// Resolve a `blob:` module from the Rust-side mirror of
/// `URL.createObjectURL`. Same MIME gate as data: (Chrome refuses blob
/// modules whose type is not a JavaScript MIME), and a revoked or unknown
/// URL is a catchable rejection.
fn blob_module_source(
    module_specifier: &ModuleSpecifier,
    url: &str,
    blob_store: &RefCell<HashMap<String, (Vec<u8>, String)>>,
) -> Result<ModuleSource, ModuleLoaderError> {
    let entry = blob_store.borrow().get(url).cloned();
    let (bytes, mime) = entry.ok_or_else(|| {
        io_err(format!(
            "Failed to fetch module {}: blob URL not found (revoked or never created)",
            url
        ))
    })?;
    if !module_mime_allowed(&mime) {
        return Err(io_err(format!(
            "Refused to execute module from {}: MIME type ({}) is not a JavaScript MIME type",
            url,
            if mime.is_empty() {
                "text/plain"
            } else {
                &mime
            }
        )));
    }
    let code = String::from_utf8_lossy(&bytes).into_owned();
    Ok(ModuleSource::new(
        deno_core::ModuleType::JavaScript,
        ModuleSourceCode::String(code.into()),
        module_specifier,
        None,
    ))
}

impl ModuleLoader for DitingModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: deno_core::ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        // deno_core represents the root passed to load_side_es_module with a
        // synthetic "." referrer. A browser resolves <script type=module src>
        // as a resource URL before it starts a graph; the document import map
        // must not remap that root URL.
        if referrer == "." {
            return deno_core::resolve_import(specifier, &self.base_url)
                .map_err(|error| io_err(error.to_string()));
        }

        let base = if referrer.is_empty() || referrer.starts_with('<') || referrer == "about:blank"
        {
            &self.base_url
        } else {
            referrer
        };

        let base = ModuleSpecifier::parse(base)
            .map_err(|e| io_err(format!("Invalid module referrer {}: {}", base, e)))?;
        self.import_map
            .try_borrow_mut()
            .map_err(|_| io_err("Import map is already borrowed".to_string()))?
            .resolve(specifier, &base)
            .map_err(io_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let url = module_specifier.to_string();
        // data:/blob: resolve locally — no HTTP client can fetch either
        // scheme. fetch()/XHR and classic scripts already handle both on the
        // JS side (b12405d, f841205); the module path (import() and
        // `<script type=module>`) used to die in validate_fetch_url/reqwest.
        // Both are synchronous: no network, and the blob mirror is a
        // same-thread Rc (load() runs on the JS thread).
        match module_specifier.scheme() {
            "data" => {
                return ModuleLoadResponse::Sync(data_module_source(module_specifier, &url));
            }
            "blob" => {
                return ModuleLoadResponse::Sync(blob_module_source(
                    module_specifier,
                    &url,
                    &self.blob_store,
                ));
            }
            _ => {}
        }
        // Capture the loader's proxy here so the async closure below owns a
        // plain Option<String> rather than borrowing &self across an `await`.
        let proxy_url = self.proxy_url.clone();

        ModuleLoadResponse::Async(Pin::from(Box::new(async move {
            // Same page-reachable policy as fetch()/XHR (obscura #849):
            // dynamic import() must not reach file:// or private/internal
            // hosts just because it took the module-loader path.
            let specifier = ModuleSpecifier::parse(&url)
                .map_err(|e| io_err(format!("Invalid module URL {}: {}", url, e)))?;
            if let Err(reason) =
                crate::diting_js::ops::validate_fetch_url(&specifier)
            {
                return Err(io_err(format!(
                    "Failed to fetch module {}: {}",
                    url, reason
                )));
            }

            // Reuse the process-wide cached client (same one op_fetch_url
            // uses). Modern SPAs dynamic-import 20-50 chunks per page; the
            // old code built a fresh reqwest::Client per import, each with
            // its own empty connection pool, no reuse, fresh TLS init for
            // every chunk. The cache means the first import on a given
            // proxy pays the build cost once and every chunk after reuses
            // the same warm pool.
            let client = crate::diting_js::ops::cached_request_client(proxy_url.as_deref())
                .map_err(io_err)?;

            tracing::debug!(
                "Loading ES module: {} (proxy: {})",
                url,
                proxy_url.as_deref().unwrap_or("direct")
            );

            let mut resp = client
                .get(&url)
                .header("Accept", "application/javascript, text/javascript, */*")
                .send()
                .await
                .map_err(|e| io_err(format!("Failed to fetch module {}: {}", url, e)))?;

            if !resp.status().is_success() {
                return Err(io_err(format!(
                    "Module {} returned HTTP {}",
                    url,
                    resp.status()
                )));
            }

            // Same body cap as fetch()/XHR (obscura #849): the page controls
            // module URLs, so a server streaming gigabytes into a dynamic
            // import() must fail as a catchable rejection instead of OOMing
            // the process. Content-Length is checked before reading; a lying
            // or absent header still runs into the per-chunk check while
            // streaming.
            let body_limit = crate::diting_js::ops::fetch_body_byte_limit();
            if let Some(len) = resp
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<usize>().ok())
            {
                if len > body_limit {
                    return Err(io_err(format!(
                        "Module {} response body too large: content-length {} exceeds limit {} bytes",
                        url, len, body_limit
                    )));
                }
            }
            let mut code_bytes: Vec<u8> = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| io_err(format!("Failed to read module body {}: {}", url, e)))?
            {
                code_bytes.extend_from_slice(&chunk);
                if code_bytes.len() > body_limit {
                    return Err(io_err(format!(
                        "Module {} response body exceeded limit of {} bytes",
                        url, body_limit
                    )));
                }
            }
            // Module scripts decode as UTF-8 per spec (unlike classic scripts,
            // which honour charset attributes), so lossy conversion here
            // matches what a browser would run.
            let code = String::from_utf8_lossy(&code_bytes).into_owned();

            // Debug knob (AGINXBROWSER_MODULE_TRACE=1): bracket every fetched
            // module with ENTER/EXIT console marks. A watchdog that terminates
            // a synchronous spin inside module-graph evaluation leaves no JS
            // stack behind (terminate_execution discards it), but the console
            // log survives — the last MODULE_ENTER without a matching
            // MODULE_EXIT names the exact chunk that owns the spin. Statements
            // around the body are legal ESM (imports are hoisted); a module
            // with top-level await just reports its EXIT late, which still
            // localizes the spin. Strictly an engine-debugging aid.
            let code = if std::env::var("AGINXBROWSER_MODULE_TRACE").is_ok() {
                format!(
                    "console.log(\"MODULE_ENTER:{url}\");\n{code}\nconsole.log(\"MODULE_EXIT:{url}\");"
                )
            } else {
                code
            };

            Ok(ModuleSource::new(
                deno_core::ModuleType::JavaScript,
                ModuleSourceCode::String(code.into()),
                &specifier,
                None,
            ))
        })))
    }
}
