//! Interactive browser sessions for agent-style browsing.
//!
//! Each session lives on its own OS thread with a persistent Browser + Page.
//! Commands are dispatched via channels, results returned via oneshot.
//! Sessions auto-expire after 8 minutes of inactivity.
//!
//! Split (ARCHITECTURE.md P2): [`manager`] owns the actor and the
//! per-session thread, [`commands`] is the command protocol, [`state`]
//! holds handles/errors/snapshot backends, [`interact`] the page
//! interaction executors, [`record`] recording/replay. The re-exports
//! below are exactly the externally consumed face (`main.rs`, `mcp.rs`,
//! `flow.rs`); the response types stay reachable inside the module via
//! their defining submodules. `interact` is `pub(crate)` (not re-exported)
//! because the CDP Input face imports its mouse-event JS builders downward
//! — the R1-allowed direction (issue #47).

mod commands;
pub(crate) mod interact;
mod manager;
mod record;
mod state;
#[cfg(test)]
mod tests;

pub use commands::{ScrollDirection, SessionCommand};
pub use manager::{SessionManager, SESSIONS};
pub use record::replay_bash;
pub use state::{ConsoleFilter, SessionError};
