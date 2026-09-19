//! CDP (Chrome DevTools Protocol) bridge — claimed from upstream obscura-cdp
//! and adapted to the diting engine. Exposes the engine over the wire
//! protocol Playwright (`connectOverCDP`), Puppeteer (`connect`) and
//! browser-use speak, so off-the-shelf automation clients can drive diting.
//!
//! Layout mirrors upstream: `types` (wire shapes), `util`, `cookie_params`,
//! `dispatch` (routing + per-connection state), `domains/*` handlers, and an
//! axum-mounted HTTP/WS surface (`http`) replacing upstream's hand-rolled
//! TCP accept loop.

pub mod cookie_params;
pub mod dispatch;
pub mod domains;
pub mod http;
pub mod types;
pub(crate) mod util;
