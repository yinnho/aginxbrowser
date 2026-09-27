pub mod module_loader;
pub mod runtime;
pub mod ops;
pub mod ws;
mod import_map;
pub mod markdown;
mod write_stream;

#[cfg_attr(not(test), allow(unused_imports))] // runtime tests are the sole consumer
pub use markdown::HTML_TO_MARKDOWN_JS;

/// The V8 version the kernel actually executes scripts on (#102) — the
/// engine truth behind the UA string a site sees. Safe to call before any
/// isolate exists (a static string in the binary).
pub fn v8_version() -> &'static str {
    deno_core::v8::V8::get_version()
}
