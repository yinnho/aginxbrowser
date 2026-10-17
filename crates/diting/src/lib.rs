//! Diting (谛听) — the browser engine under AginxBrowser.
//!
//! One responsibility: load and run a page — network → DOM → CSS → layout →
//! paint, with a built-in V8 runtime — and expose it through the `Page`-level
//! API. No product concerns live here (ARCHITECTURE.md §2, rule R2).

pub mod diting_browser;
pub mod diting_dom;
pub mod diting_js;
pub mod diting_net;
// Engine runtime knobs (AGINXBROWSER_* env), owned by the engine since the
// workspace split; the product re-exports them from `config`.
pub mod env_knobs;
// Cascade layer absorbed from upstream obscura-render (read-only slice,
// not yet wired to the product pipeline — see docs/engine/render.md).
pub mod diting_css;
// Taffy fork-delta classification tests (obscura's vendored taffy vs the
// stock 0.13.0 our blitz pipeline pins) — docs/engine/render.md §11.
#[cfg(feature = "screenshot")]
pub mod diting_layout;
// Bundled CJK font supply for /screenshot determinism (batch 3c) —
// docs/engine/render.md §18.
#[cfg(feature = "screenshot")]
pub mod diting_fonts;

/// Source revision the engine was built from (build.rs git rev-parse;
/// AGINXBROWSER_BUILD_COMMIT overrides for the .git-less 86quan tree).
/// Lives HERE because our build script's `rustc-env` only reaches this
/// crate's compilation — dependent crates' `option_env!` never see it
/// (the local /health "unknown" face). The .git/HEAD rerun pins keep it
/// fresh per commit.
pub const BUILD_COMMIT: &str = env!("AGINXBROWSER_BUILD_COMMIT");
