//! Env-knob test guards for the product crate's single test binary.
//!
//! A bin crate compiles every `#[cfg(test)]` module into ONE test target,
//! so one lock here serializes every test that flips a process env knob
//! across download/server/screenshot. The engine (crates/diting) carries
//! its own guards inside diting_net for its own tests — `cfg(test)` can't
//! cross crates, and the knobs are read live from env at call time
//! (`allow_file_access()` ORs the env var in), so a product-side set under
//! lock is behaviorally identical to the engine's flag+env guard
//! (1f7486c pattern, split-lore noted in ARCHITECTURE.md §4).

/// Serializes product tests that read or mutate
/// `AGINXBROWSER_ALLOW_PRIVATE_NETWORK` — process env is shared across
/// parallel test threads.
pub(crate) static PRIVATE_NET_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Set `AGINXBROWSER_ALLOW_FILE_ACCESS` for the duration of a test: the
/// gate is off by default and the env half is read live, so the drop guard
/// removing the var restores the closed posture even on panic (tests run
/// without `--allow-file-access`, so the in-process flag half stays unset).
pub(crate) fn file_access_guard(enabled: bool) -> impl Drop {
    let guard = FILE_ACCESS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if enabled {
        std::env::set_var("AGINXBROWSER_ALLOW_FILE_ACCESS", "1");
    } else {
        std::env::remove_var("AGINXBROWSER_ALLOW_FILE_ACCESS");
    }
    FileAccessGuard(Some(guard))
}

static FILE_ACCESS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds the env lock for the test's duration; dropping releases both.
struct FileAccessGuard(Option<std::sync::MutexGuard<'static, ()>>);

impl Drop for FileAccessGuard {
    fn drop(&mut self) {
        std::env::remove_var("AGINXBROWSER_ALLOW_FILE_ACCESS");
        drop(self.0.take());
    }
}

static WORKFLOW_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Points `AGINXBROWSER_WORKFLOW_DIR` at a scratch directory for the test's
/// duration (flow tests: on-disk overrides and additions against the baked
/// built-ins). Dropping removes the var, restoring cwd-relative resolution,
/// even on panic.
pub(crate) fn workflow_dir_env_guard() -> WorkflowDirGuard {
    let guard = WORKFLOW_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Held without a value until the test sets its own; the drop guard only
    // has to undo whatever the test leaves behind.
    std::env::remove_var("AGINXBROWSER_WORKFLOW_DIR");
    WorkflowDirGuard(Some(guard))
}

pub(crate) struct WorkflowDirGuard(Option<std::sync::MutexGuard<'static, ()>>);

impl Drop for WorkflowDirGuard {
    fn drop(&mut self) {
        std::env::remove_var("AGINXBROWSER_WORKFLOW_DIR");
        drop(self.0.take());
    }
}
