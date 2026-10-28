//! Snapshot create→restore pins — a SEPARATE test binary on purpose.
//!
//! The v8 crate runs its own snapshot tests under a process-wide WRITE
//! lock (snapshot creation is a whole-isolate, exclusive operation — the
//! setup module in v8's test_api.rs makes sequential_test the only shape
//! those tests may use). The unit-test binary keeps the pure-Rust format
//! tests; the V8-touching pins live HERE, where every test in the process
//! is a snapshot test, plus a Mutex to keep them off each other.
//!
//! The money pin is the op roundtrip: `__diting_spawn` is a raw native
//! pointer baked into the blob. V8 stores snapshotted callbacks by INDEX
//! into EXTERNAL_REFERENCES and resolves them at restore — a mismatched
//! table means a call into the wrong address. That call landing cleanly
//! IS the external-references contract, pinned.

use diting_runtime::vm::{create_snapshot, LoopError, Triplet, Vm};
use std::sync::Mutex;

/// Serializes the tests that touch isolate creation (both directions).
static ISOLATE_LOCK: Mutex<()> = Mutex::new(());

/// The full dormant-shell journey: bake once, restore, and every face
/// that was alive at bake time is alive after restore — table state,
/// settle roundtrip, the raw op callback through the external references
/// table, and the terminate→park→revive→replay loop.
#[test]
fn dormant_shell_restores_and_drives() {
    let _g = ISOLATE_LOCK.lock().unwrap();
    let blob = create_snapshot(&[], &[]);
    let blob: &'static [u8] = Box::leak(blob.into());

    let mut vm = Vm::from_snapshot(blob).unwrap();

    // The baked heap came through: faces installed, table empty, the
    // script-context bindings (const/let prelude state) reachable.
    assert_eq!(
        vm.eval_string("typeof __diting_settle").unwrap(),
        "function"
    );
    assert_eq!(
        vm.eval_string("typeof __diting_op_sleep").unwrap(),
        "function"
    );
    assert_eq!(
        vm.eval_string("globalThis.__diting_tableCounts()").unwrap(),
        "0,0",
        "the baked ring starts as empty as a fresh vm's"
    );
    assert_eq!(
        vm.eval_string("RING_SIZE").unwrap(),
        "4096",
        "script-level lexical bindings survived the bake"
    );

    // Settle roundtrip through the restored shell.
    let id: i32 = vm
        .eval_string("globalThis.__diting_defer()")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(id, 0, "the baked id counter restarts where it was baked");
    vm.settle(Triplet {
        promise_id: id,
        ok: true,
        payload: "restored".into(),
    });
    assert!(vm.poll().unwrap());
    assert_eq!(
        vm.eval_string("globalThis.__diting_lastResolved").unwrap(),
        "restored"
    );

    // THE external-references pin: JS calls the restored raw callback,
    // which must resolve to spawn_callback by table index and find the
    // driver slot on THIS isolate.
    vm.eval_string(
        "globalThis.r = null; \
         __diting_op_sleep(2, 'op').then(function (v) { globalThis.r = v; })",
    )
    .unwrap();
    assert_eq!(
        vm.pending_ops(),
        1,
        "restored __diting_spawn reached the driver — the external references table resolved"
    );
    vm.poll().unwrap(); // tick 1 of 2
    assert_eq!(vm.eval_string("globalThis.r").unwrap(), "null");
    vm.poll().unwrap(); // tick 2 of 2: completes, delivers, drains
    assert_eq!(vm.eval_string("globalThis.r").unwrap(), "op");

    // The campaign's closed loop on a restored shell: op completes under
    // terminate → parks → revive → re-delivers → settles.
    vm.eval_string(
        "globalThis.q = null; \
         __diting_op_sleep(1, 'late').then(function (v) { globalThis.q = v; })",
    )
    .unwrap();
    vm.terminate();
    assert!(matches!(vm.poll(), Err(LoopError::Terminated)));
    assert_eq!(
        vm.pending(),
        1,
        "the completion parked on the restored shell"
    );
    vm.revive();
    assert!(vm.poll().unwrap());
    assert_eq!(vm.eval_string("globalThis.q").unwrap(), "late");
}

/// One blob, two vms: restores are per-instance and independent — each
/// restarts from the same baked state and runs without seeing the other.
#[test]
fn one_blob_powers_two_independent_vms() {
    let _g = ISOLATE_LOCK.lock().unwrap();
    let blob = create_snapshot(&[], &[]);
    let blob: &'static [u8] = Box::leak(blob.into());
    let mut a = Vm::from_snapshot(blob).unwrap();
    let mut b = Vm::from_snapshot(blob).unwrap();

    // Both restart the baked id counter — same first id, proof each
    // restore got its own heap copy, not a shared one.
    let ida: i32 = a
        .eval_string("globalThis.__diting_defer()")
        .unwrap()
        .parse()
        .unwrap();
    let idb: i32 = b
        .eval_string("globalThis.__diting_defer()")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(ida, 0);
    assert_eq!(idb, 0);

    // Diverge them: settle only in a; b's promise must stay pending.
    a.settle(Triplet {
        promise_id: ida,
        ok: true,
        payload: "mine".into(),
    });
    assert!(a.poll().unwrap());
    b.poll().unwrap(); // drives nothing — b's queue is empty
    assert_eq!(
        a.eval_string("globalThis.__diting_lastResolved").unwrap(),
        "mine"
    );
    assert_eq!(
        b.eval_string("globalThis.__diting_lastResolved").unwrap(),
        "null",
        "b's heap is b's own — a's settle did not leak across restores"
    );
}

/// Format rejection through the restore face (no isolate gets built, so
/// this one may run beside the others).
#[test]
fn from_snapshot_rejects_broken_containers() {
    // Not our format: too short to even hold the tail.
    assert!(matches!(
        Vm::from_snapshot(b"tiny"),
        Err(diting_runtime::snapshot::SnapshotError::TooShort)
    ));
    // Honest container, wrong sidecar generation.
    let bytes = diting_runtime::snapshot::serialize(b"not v8 data", &999u32.to_le_bytes());
    assert!(matches!(
        Vm::from_snapshot(Box::leak(bytes.into_boxed_slice())),
        Err(diting_runtime::snapshot::SnapshotError::BadVersion { found: 999, .. })
    ));
}

/// Embedder scripts bake into the blob (10d-1): what the script list
/// declares — global bindings AND script-lexical state — revives with the
/// shell, and coexists with the prelude faces installed before it.
#[test]
fn embedder_scripts_bake_into_the_blob() {
    let _g = ISOLATE_LOCK.lock().unwrap();
    let blob = create_snapshot(&[
        (
            "shim.js",
            "globalThis.__diting_shim_calls = 0; \
             const LEXICAL = 'lex-val'; \
             globalThis.__diting_read_lexical = function () { return LEXICAL; };",
        ),
        (
            "consumer.js",
            // Runs at bake time AFTER shim.js — top-level order is the
            // list order, same law as diting's SNAPSHOT_SCRIPTS.
            "globalThis.__diting_shim_calls += 1; \
             globalThis.__diting_baked_read = __diting_read_lexical();",
        ),
    ], &[]);
    let blob: &'static [u8] = Box::leak(blob.into());
    let mut vm = Vm::from_snapshot(blob).unwrap();
    assert_eq!(vm.eval_string("globalThis.__diting_shim_calls").unwrap(), "1");
    assert_eq!(vm.eval_string("globalThis.__diting_baked_read").unwrap(), "lex-val");
    assert_eq!(
        vm.eval_string("__diting_read_lexical()").unwrap(),
        "lex-val",
        "script-lexical bindings (const closure state) survive the bake"
    );
    // The prelude faces still live on the same heap — script baking did
    // not displace the shell wiring.
    assert_eq!(vm.eval_string("typeof __diting_settle").unwrap(), "function");
    assert_eq!(vm.eval_string("__diting_tableCounts()").unwrap(), "0,0");
}
