// Diting claim of the op-settlement tick face — plan step 2
// (docs/diting_runtime认领计划_20261017.md), plus re-delivery v1.
//
// deno_core's __eventLoopTick resolves a batch of (promiseId, isOk, res)
// triplets in one bare loop with two failure shapes: one unknown promise id
// throws (absorbed here per-triplet), and an interrupted loop — terminate
// landing mid-batch, or an internal resolver throw — evaporates every
// remaining triplet (the #226/#240 delivery shape). Our tick wraps the
// original: each batch is PARKED before delivery and unparked only after
// every triplet was attempted; every later tick drains parked batches
// first, so an interrupted batch's tail re-delivers instead of vanishing.
//
// Why slice-invocation instead of calling the ring resolver directly: the
// executing deno_core state exposes no __infra global (probe 2026-10-17:
// the crate's package-root 00_infra.js source and the compiled-in snapshot
// state disagree — only Deno.core is reachable). The original tick's
// closure is the one door into the promise ring, so we keep it as the
// resolver and claim the delivery loop around it. The ring itself stays
// deno's until the diting_runtime crate swap. The Rust-side half of the
// wound (pop-under-terminate: triplets popped but the tick never invoked)
// also waits for the crate — this file closes only the JS-side half.
//
// Install ladder: the executing core object is sealed read-only, so the
// tick can only be replaced by re-pointing the Deno.core PATH — deno_core's
// capture (jsruntime.rs store_js_callbacks) and bootstrap.js both read
// globalThis.Deno.core.* by path, never by object identity, so a shell Deno
// whose .core is a descriptor copy of the original (tick swapped) is
// indistinguishable to every consumer; internal deno closures keep their
// direct references to the original, untouched.
//
// Loaded FIRST, before bootstrap.js: the snapshot serializes globals after
// all scripts run, and every production JsRuntime captures
// Deno.core.__eventLoopTick inside its constructor — before __diting_init
// deletes the Deno global — so what gets captured is ours.
"use strict";

(() => {
  const core = globalThis.Deno && globalThis.Deno.core;
  const original = core && core.__eventLoopTick;
  if (typeof original !== "function") {
    // No silent bail: if the tick face ever moves, the build must fail
    // instead of quietly leaving deno's fragile delivery loop live (the
    // feature-gating trap).
    throw new Error("core_base: Deno.core.__eventLoopTick absent at snapshot time");
  }

  // Build-time derivation of the missing-promise throw shape, so the absorb
  // filter below is never guesswork: the shipped-source shape is a bare
  // string "Missing promise @ N", but the executing snapshot state is what
  // matters. A shape we can't recognize fails the build; the observed shape
  // is recorded on the claim marker for the runtime pin test.
  let missingThrow = "none";
  try {
    original(2 ** 30, true, null);
  } catch (e) {
    if (typeof e === "string" && e.startsWith("Missing promise")) {
      missingThrow = "string";
    } else if (
      e && typeof e === "object" &&
      typeof e.message === "string" && e.message.startsWith("Missing promise")
    ) {
      missingThrow = "error-msg";
    } else {
      throw new Error(
        "core_base: unknown missing-promise throw shape (" + String(e).slice(0, 60) + ") — re-derive the absorb filter",
      );
    }
  }

  function isMissingThrow(e) {
    return (typeof e === "string" && e.startsWith("Missing promise")) ||
      (e && typeof e === "object" &&
        typeof e.message === "string" && e.message.startsWith("Missing promise"));
  }

  // ---- re-delivery v1 ------------------------------------------------
  // A parked batch { t: flat triplets } stays parked until every triplet
  // was attempted. Missing ids absorb (already-settled or stale-realm
  // promises — a navigation rebuilds the realm and stale entries
  // self-clean). A batch that parks MAX_ATTEMPTS times without completing
  // is dropped and rethrows: an internal resolver bug must surface, never
  // spin. Terminate lands as an uncatchable unwind — no finally runs, the
  // entry simply stays parked; the next tick drains it.
  const replay = [];
  const MAX_ATTEMPTS = 3;

  function deliverTriplets(list) {
    for (let i = 0; i + 2 < list.length; i += 3) {
      try {
        original(list[i], list[i + 1], list[i + 2]);
      } catch (e) {
        if (!isMissingThrow(e)) {
          // Keep the current triplet in the tail: the resolver may have
          // settled the promise before throwing, and re-resolving an
          // already-settled id absorbs as Missing — harmless either way.
          list.splice(0, i);
          throw e;
        }
      }
    }
    list.length = 0;
  }

  function drainReplay() {
    let attemptsLeft = MAX_ATTEMPTS;
    while (replay.length) {
      const entry = replay[0];
      try {
        deliverTriplets(entry.t);
      } catch (e) {
        if (--attemptsLeft > 0) continue;
        replay.shift();
        throw e;
      }
      replay.shift();
    }
  }

  const claimed = function __eventLoopTick() {
    // Settlement order is oldest first: drain parked batches, then deliver.
    drainReplay();

    const t = Array.prototype.slice.call(arguments);
    replay.push({ t: t });
    try {
      deliverTriplets(t);
      replay.pop();
    } catch (e) {
      // Tail stays parked at the top of the stack; the error surfaces
      // exactly like deno's would.
      throw e;
    }
  };

  // Install: prefer mutating the core object in place; fall back to the
  // shell swap when the object is frozen (the observed case).
  const d = Object.getOwnPropertyDescriptor(core, "__eventLoopTick");
  let install;
  if (d && d.configurable) {
    Object.defineProperty(core, "__eventLoopTick", {
      value: claimed, writable: true, configurable: true, enumerable: d.enumerable,
    });
    install = "defineProperty";
  } else if (d && d.writable) {
    core.__eventLoopTick = claimed;
    install = "assign";
  } else {
    const deno = globalThis.Deno;
    const shell = {};
    for (const k of Object.getOwnPropertyNames(deno)) {
      if (k === "core") continue;
      Object.defineProperty(shell, k, Object.getOwnPropertyDescriptor(deno, k));
    }
    const coreCopy = {};
    for (const k of Object.getOwnPropertyNames(core)) {
      if (k === "__eventLoopTick") continue;
      Object.defineProperty(coreCopy, k, Object.getOwnPropertyDescriptor(core, k));
    }
    Object.defineProperty(coreCopy, "__eventLoopTick", {
      value: claimed, writable: true, configurable: true, enumerable: true,
    });
    shell.core = coreCopy;
    Object.defineProperty(globalThis, "Deno", {
      value: shell, writable: true, configurable: true, enumerable: true,
    });
    install = "shell";
  }

  // Claim marker: enumeration-hidden by the bootstrap wrappers (underscore +
  // diting patterns), never deleted; runtime tests read it to pin that the
  // overlay actually landed in the snapshot. The __-prefixed handles exist
  // for the re-delivery pin test only.
  globalThis.__diting_core_base = {
    claimed: true,
    missingThrow: missingThrow,
    install: install,
    __replayLength: function () { return replay.length; },
    __forceReplay: function () {
      const t = [];
      for (let i = 0; i < arguments.length; i++) t.push(arguments[i]);
      replay.push({ t: t });
    },
  };

  // Verify through the PATH (in the shell branch, the local `core` binding
  // still points at the untouched original).
  if (globalThis.Deno.core.__eventLoopTick !== claimed) {
    throw new Error("core_base: tick install reported " + install + " but the path still resolves elsewhere");
  }
})();
