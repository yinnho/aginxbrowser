//! HTTP route handlers, split by surface (ARCHITECTURE.md P2 batch 2).
//! The crate root keeps the axum Router wiring, the shared AppError face,
//! and the process-level handlers (status/health/doctor/mcp).
pub(crate) mod acquisition;
pub(crate) mod sessions;
#[cfg(feature = "screenshot")]
pub(crate) mod outputs;
