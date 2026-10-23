//! Acquire façade: browser factory, stateless handlers, search orchestration.
//!
//! Split by **job** (not line count):
//! - [`browser`] — jar / factory / cookie normalize / V8 host thread
//! - [`fetch`] — do_fetch / do_click / do_eval + challenge ride-out
//! - [`outputs`] — do_screenshot / do_video / do_pdf (`screenshot` feature)
//! - [`search`] — do_search + cache + engine health
//!
//! Callers keep using `crate::server::…`; this module re-exports the public API.

mod browser;
mod fetch;
#[cfg(feature = "screenshot")]
mod outputs;
mod search;

// Façade re-exports — names exist for other modules / future callers even when
// this file itself does not mention them again.
#[allow(unused_imports)]
pub use browser::{
    build_browser, build_browser_for_account, build_browser_with_jar, persist_shared_cookies,
};
pub(crate) use browser::{
    cookie_list_from_json, inject_cookies, normalize_cookie_entry, run_on_local_runtime,
    v8_stack_size,
};
#[cfg(test)]
pub(crate) use browser::{cookie_store_path, shared_cookie_jar_for_tests};

pub use fetch::{do_click, do_eval, do_fetch};
pub(crate) use fetch::maybe_bypass_challenge;
#[cfg(test)]
pub(crate) use fetch::{media_from_eval, parse_layout_facts, resource_facts};
#[cfg(feature = "screenshot")]
pub use outputs::{do_pdf, do_screenshot, do_video};

pub use search::{do_search, SearchError};
pub(crate) use search::search_engine_health;
#[cfg(test)]
pub(crate) use search::do_search_with_registry;

/// Shared test plumbing for handler-level tests that need a real local
/// origin: an env guard that opens the private-network door under the same
/// process-wide lock diting_net uses, and a request-recording HTTP server.
#[cfg(test)]
pub(crate) mod test_util;

#[cfg(test)]
mod tests;
