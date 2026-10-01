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
//! below are exactly the externally consumed face (`main.rs`,
//! `flow.rs`); the response types stay reachable inside the module via
//! their defining submodules.

mod commands;
mod interact;
mod manager;
pub(crate) mod preload_recipes;
mod record;
#[cfg(feature = "screenshot")]
mod screenshot;
mod state;
#[cfg(test)]
mod tests;

pub use commands::{ScrollDirection, SessionCommand};
pub use manager::{SessionManager, SESSIONS};
pub(crate) use manager::{send_command, send_screenshot};
pub use record::replay_bash;
pub use state::{ConsoleFilter, SessionError};
