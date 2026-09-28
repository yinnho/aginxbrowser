//! Panic persistence (aginxos REQ #414, device respawn #19): the process
//! died mid-search with zero trace anywhere — no OOM in dmesg, no panic on
//! stderr in the service log. This hook appends the panic payload, thread,
//! location and a forced backtrace to `{data}/crash.log` synchronously
//! before the default reporting runs, so a respawned process can read how
//! its predecessor died.
//!
//! SIGKILL deaths (OOM killer, `kill -9`) are outside a process's own
//! reach — exit-code/signal capture belongs to the supervising daemon and
//! is tracked on that side, not here.

use std::io::Write as _;

/// Install the crash-log hook. Writes go to `{app_data_dir}/crash.log`
/// (the credential-bearing state home — 0600 at creation on unix). When
/// no data dir resolves (stripped container without HOME/XDG), the default
/// hook stays; a missing log is better than a crash log next to the binary.
pub fn install() {
    let Some(dir) = crate::config::app_data_dir() else {
        return;
    };
    install_at(&dir);
}

/// [`install`] against an explicit directory (tests use a temp dir; the
/// hook itself stays process-global either way).
pub(crate) fn install_at(dir: &std::path::Path) {
    let log = dir.join("crash.log");
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let entry = format_entry(
            crate::now_secs(),
            info.payload(),
            info.location(),
            &std::thread::current(),
        );
        append_entry(&log, &entry);
        default_hook(info);
    }));
}

/// One crash record. `PanicHookInfo` cannot be constructed outside a real
/// panic, so the hook adapter extracts the pieces and the formatting stays
/// testable.
fn format_entry(
    now: u64,
    payload: &(dyn std::any::Any + Send),
    location: Option<&std::panic::Location<'_>>,
    thread: &std::thread::Thread,
) -> String {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string());
    let at = location
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown>".to_string());
    format!(
        "[{now}] aginxbrowser {} pid {} panic in thread {:?}\n  msg: {msg}\n  at: {at}\n  backtrace:\n{}\n",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        thread.name().unwrap_or("<unnamed>"),
        std::backtrace::Backtrace::force_capture()
    )
}

/// Synchronous append — the panic path must not depend on an async runtime
/// or a worker that may itself be wedged. Failures are silent by design:
/// the default hook still reports to stderr, which is the floor we found
/// missing on the device only because the process vanished faster than
/// stderr drained.
fn append_entry(path: &std::path::Path, entry: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[cfg(unix)]
    let mut options = {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut o = std::fs::OpenOptions::new();
        o.mode(0o600);
        o
    };
    #[cfg(not(unix))]
    let mut options = std::fs::OpenOptions::new();
    if let Ok(mut f) = options.create(true).append(true).open(path) {
        let _ = f.write_all(entry.as_bytes());
        let _ = f.write_all(b"\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_entry_carries_payload_version_and_thread() {
        let payload = "boom: ladder retracted".to_string();
        let thread = std::thread::current();
        let entry = format_entry(1_761_000_000, &payload, None, &thread);
        assert!(entry.contains("[1761000000]"));
        assert!(entry.contains(env!("CARGO_PKG_VERSION")));
        assert!(entry.contains("boom: ladder retracted"));
        assert!(entry.contains("at: <unknown>"));
        assert!(entry.contains("backtrace:"));
    }

    #[test]
    fn format_entry_takes_str_payloads_too() {
        let payload: &str = "static boom";
        let thread = std::thread::current();
        // &&str coerces to &(dyn Any + Send) — `str` itself is !Sized, so
        // the reference &str is the Any referent, exactly the shape a
        // `panic!("literal")` payload arrives in.
        let entry = format_entry(0, &payload, None, &thread);
        assert!(entry.contains("static boom"));
    }

    #[test]
    fn append_entry_writes_the_record() {
        let dir = std::env::temp_dir().join(format!("agx_crash_{}", std::process::id()));
        let path = dir.join("crash.log");
        append_entry(&path, "first");
        append_entry(&path, "second");
        let body = std::fs::read_to_string(&path).expect("crash log readable");
        assert!(body.contains("first"));
        assert!(body.contains("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // End-to-end: install_at swaps the process hook (chained onto the
    // previous one, so later panics still report normally), a real panic
    // fires it, and the record lands on disk.
    #[test]
    fn installed_hook_persists_a_real_panic() {
        let dir = std::env::temp_dir().join(format!("agx_crash_e2e_{}", std::process::id()));
        install_at(&dir);
        let marker = format!("crash-test-marker-{}", std::process::id());
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            panic!("{marker}");
        }))
        .is_err();
        assert!(caught);
        let body = std::fs::read_to_string(dir.join("crash.log")).expect("crash log written");
        assert!(body.contains(&format!(
            "crash-test-marker-{}",
            std::process::id()
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
