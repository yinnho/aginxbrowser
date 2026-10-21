use std::path::PathBuf;

// Snapshot scripts, in load order (table order = execution order). All run
// in one realm inside the snapshot runtime; each script only declares
// bindings at top level and touches earlier scripts' bindings at call time.
const SNAPSHOT_SCRIPTS: &[(&str, &str)] = &[
    // 10d host flip: the Deno shell. The retired deno_core snapshot carried
    // the whole Deno.core object (promise ring, tick, encode/decode); the
    // only face our scripts ever read is `Deno.core.ops` — bootstrap.js
    // captures it at line 110 as a top-level const (`const _OPS =
    // Deno.core.ops`) that every later script shares through the global
    // lexical environment. The runtime installs the ops INTO that same
    // object (Vm::install_ops_under), never onto globalThis — op names as
    // window properties would change the fingerprint face. __diting_init
    // deletes the `Deno` property per page exactly as before (anti-bot
    // law: a Deno prop on window is the canonical deno-runtime marker).
    // Must run FIRST: bootstrap's const capture is the first Deno read.
    // 10d rejection face: bootstrap.js:10819+ registers its window-event
    // handlers through Deno.core.set{Unhandled,Handled}PromiseRejectionHandler
    // at bake time; the PRELUDE's registration functions store the closures
    // into heap globals (baked, restore-free) where Vm::dispatch_rejections
    // reads them live each round.
    ("deno_shell", "globalThis.Deno = { core: { ops: {}, setUnhandledPromiseRejectionHandler: __diting_set_unhandled_rejection_handler, setHandledPromiseRejectionHandler: __diting_set_handled_rejection_handler } };"),
    // Main script: the bootstrap god-file-turned-root.
    ("bootstrap.js", include_str!("js/bootstrap.js")),
    // #252 god-file ratchet split: the fetch family moved out of
    // bootstrap.js wholesale. Its top level only declares bindings; every
    // helper it references (_docBase, __recordFetchTiming, Headers…) is
    // touched at call time, by which point bootstrap.js has run. Must
    // precede xhr.js, whose send() rides __ditingFetchShim.
    ("webrtc.js", include_str!("js/webrtc.js")),
    ("fetch.js", include_str!("js/fetch.js")),
    // Ratchet splits (pure moves): the audio/speechSynthesis faces, the
    // Worker family, media_source, iframe_realm, range_selection,
    // indexeddb and crypto_subtle left bootstrap.js wholesale. All only
    // declare bindings at top level and touch bootstrap helpers at call time.
    ("audio.js", include_str!("js/audio.js")),
    ("worker.js", include_str!("js/worker.js")),
    ("media_source.js", include_str!("js/media_source.js")),
    ("iframe_realm.js", include_str!("js/iframe_realm.js")),
    ("range_selection.js", include_str!("js/range_selection.js")),
    ("indexeddb.js", include_str!("js/indexeddb.js")),
    // Ratchet split: crypto.subtle / CryptoKey left bootstrap.js wholesale.
    // Touches _OPS and __diting_clone_hooks only at call time; basic
    // getRandomValues/randomUUID stay in bootstrap.
    ("crypto_subtle.js", include_str!("js/crypto_subtle.js")),
    // #236 absorption split: the XHR family moved out of bootstrap.js
    // wholesale — consumes the fetch shim closure and the initiator-hint
    // var fetch.js defined, both at class-definition and at send() time.
    ("xhr.js", include_str!("js/xhr.js")),
    // #97 ratchet split: DOMParser wholesale — the XML gates and node
    // classes it names are bootstrap bindings, touched only at
    // parseFromString call time.
    ("domparser.js", include_str!("js/domparser.js")),
    // #98 ratchet split: the Performance family wholesale.
    ("performance.js", include_str!("js/performance.js")),
    // Window-level scalar faces (name/status/origin/closed) — defaults plus
    // live getters, nothing call-time dependent on earlier scripts.
    ("window_faces.js", include_str!("js/window_faces.js")),
    // #291 / #226 splits: message_port and the fetch-settlement registry —
    // both only touch bootstrap-defined bindings, and only at call time.
    ("message_port.js", include_str!("js/message_port.js")),
    ("fetch_probe.js", include_str!("js/fetch_probe.js")),
    // 300015 P2: the chrome-truth window own-prop surface (generated — see
    // the file header). Loads after every behavior module, before the
    // sweeps.
    ("global_surface.js", include_str!("js/global_surface.js")),
    ("media_late.js", include_str!("js/media_late.js")),
    // 300015 P3: Chrome-exact toDataURL/toBlob bytes for blank canvases
    // (generated — see the file header). Wraps bootstrap's toDataURL; must
    // load before the sweep so the wrappers get native-marked with the rest.
    ("canvas_face.js", include_str!("js/canvas_face.js")),
    // LAST (#236 absorption split): the snapshot-time interface sweeps walk
    // globalThis to finish every interface (native marks, non-enumerable
    // globals, enumerable members, WebIDL toStringTag brands) — running last
    // is the point, so every earlier script's globals get swept.
    ("interface_sweep.js", include_str!("js/interface_sweep.js")),
];

fn main() {
    for (name, _) in SNAPSHOT_SCRIPTS {
        println!("cargo:rerun-if-changed=js/{name}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    // Re-embed the commit when HEAD moves (branch switches change .git/HEAD;
    // same-branch commits move .git/refs/heads/<branch> — cargo watches both
    // when present). Release CI builds from a fresh checkout either way.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    // Source revision baked into the binary — /health reports it so a caller
    // can verify doc/tag/binary/source are the same commit without sniffing
    // anything (0.3.0 tmall report P2). "unknown" for git-less builds.
    // AGINXBROWSER_BUILD_COMMIT overrides git: the 86quan deploy rsyncs
    // without .git (by design — the server tree is a build input, not a
    // checkout), so the deploy step passes the commit explicitly and /health
    // stops reporting "unknown".
    let commit = std::env::var("AGINXBROWSER_BUILD_COMMIT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        });
    println!("cargo:rustc-env=AGINXBROWSER_BUILD_COMMIT={commit}");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let snapshot_path = out_dir.join("DITING_SNAPSHOT.bin");

    // 10d: our own snapshot creator (diting_runtime::vm::create_snapshot) —
    // PRELUDE + raw faces + the script list above, in our [blob][sidecar]
    // container. Scripts run origin-less: stack frames surface as
    // "<anonymous>" (300015 brand-leak law; the name rides only panics).
    let output = diting_runtime::vm::create_snapshot(SNAPSHOT_SCRIPTS);

    std::fs::write(&snapshot_path, &output).expect("Failed to write snapshot");
    println!(
        "cargo:rustc-env=AGINXBROWSER_SNAPSHOT_PATH={}",
        snapshot_path.display()
    );
}
