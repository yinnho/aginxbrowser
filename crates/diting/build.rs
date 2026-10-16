use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=js/bootstrap.js");
    println!("cargo:rerun-if-changed=js/fetch.js");
    println!("cargo:rerun-if-changed=js/audio.js");
    println!("cargo:rerun-if-changed=js/worker.js");
    println!("cargo:rerun-if-changed=js/range_selection.js");
    println!("cargo:rerun-if-changed=js/indexeddb.js");
    println!("cargo:rerun-if-changed=js/xhr.js");
    println!("cargo:rerun-if-changed=js/domparser.js");
    println!("cargo:rerun-if-changed=js/performance.js");
    println!("cargo:rerun-if-changed=js/window_faces.js");
    println!("cargo:rerun-if-changed=js/global_surface.js");
    println!("cargo:rerun-if-changed=js/canvas_face.js");
    println!("cargo:rerun-if-changed=js/interface_sweep.js");
    println!("cargo:rerun-if-changed=js/message_port.js");
    println!("cargo:rerun-if-changed=js/fetch_probe.js");
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

    let bootstrap_js = include_str!("js/bootstrap.js");
    // Second script, same realm (#252 god-file ratchet split): the fetch
    // family moved out of bootstrap.js wholesale. Its top level only
    // declares bindings (var/const/function + the globalThis.fetch
    // assignment) — every helper it references (_docBase, __recordFetchTiming,
    // Headers…) is touched at call time, by which point bootstrap.js has
    // run. It must precede xhr.js, whose send() rides __ditingFetchShim.
    let fetch_js = include_str!("js/fetch.js");
    // Ratchet split (pure move): the Web Audio / speechSynthesis faces and the
    // Worker family left bootstrap.js wholesale. Both only declare bindings at
    // top level and touch bootstrap helpers at call time.
    let audio_js = include_str!("js/audio.js");
    let worker_js = include_str!("js/worker.js");
    let range_selection_js = include_str!("js/range_selection.js");
    let indexeddb_js = include_str!("js/indexeddb.js");
    // Third script, same realm, runs after bootstrap.js: message_port.js
    // only consumes bindings the first script defined, so plain sequential
    // execute_script calls are equivalent to one concatenated script.
    let message_port_js = include_str!("js/message_port.js");
    // Fourth script, same rule: the #226 fetch-settlement registry only
    // touches bindings bootstrap.js defined, and only at call time.
    let fetch_probe_js = include_str!("js/fetch_probe.js");
    // Fifth script, same rule (#236 absorption split): the XHR family moved
    // out of bootstrap.js wholesale — it consumes the fetch shim closure and
    // the initiator-hint var fetch.js defined, both at class-definition
    // and at send() time.
    let xhr_js = include_str!("js/xhr.js");
    // Sixth script, same rule (#97 ratchet split): DOMParser moved out of
    // bootstrap.js wholesale — the XML gates and node classes it names are
    // bootstrap bindings, touched only at parseFromString call time.
    let domparser_js = include_str!("js/domparser.js");
    // Seventh script (#98 ratchet split): the Performance family moved out
    // of bootstrap.js wholesale — bootstrap helpers are shared realm
    // bindings, touched only at call time.
    let performance_js = include_str!("js/performance.js");
    // Window-level scalar faces (name/status/origin/closed) — defaults plus
    // live getters, nothing call-time dependent on earlier scripts.
    let window_faces_js = include_str!("js/window_faces.js");
    // 300015 P2: the chrome-truth window own-prop surface (generated — see
    // the file header). Loads after every behavior module, before the sweeps.
    let global_surface_js = include_str!("js/global_surface.js");
    // 300015 P3: Chrome-exact toDataURL/toBlob bytes for blank canvases
    // (generated — see the file header). Wraps bootstrap's toDataURL (the
    // non-blank path and the honest blank floor); must load before
    // interface_sweep.js so the wrappers get native-marked with the rest.
    let canvas_face_js = include_str!("js/canvas_face.js");
    // LAST script (#236 absorption split): the snapshot-time interface sweeps
    // walk globalThis to finish every interface (native marks, non-enumerable
    // globals, enumerable members, WebIDL toStringTag brands) — running them
    // last is the point, so xhr.js's globals get swept too (they used to be
    // swept because XHR lived mid-bootstrap; the split would have silently
    // dropped the brand and the zone.js enumerable face).
    let interface_sweep_js = include_str!("js/interface_sweep.js");

    let output = deno_core::snapshot::create_snapshot(
        deno_core::snapshot::CreateSnapshotOptions {
            cargo_manifest_dir: env!("CARGO_MANIFEST_DIR"),
            startup_snapshot: None,
            skip_op_registration: true,
            extensions: vec![],
            extension_transpiler: None,
            with_runtime_cb: Some(Box::new(move |runtime| {
                // Stack frames from these scripts are visible to page JS
                // (Error.stack / catch). Do NOT use "<diting:…>" names —
                // they leak the engine brand (300015 env probes). Chrome
                // shows "<anonymous>" for eval-like builtins; match that.

                runtime
                    .execute_script("<anonymous>", bootstrap_js.to_string())
                    .expect("bootstrap.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", fetch_js.to_string())
                    .expect("fetch.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", audio_js.to_string())
                    .expect("audio.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", worker_js.to_string())
                    .expect("worker.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", range_selection_js.to_string())
                    .expect("range_selection.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", indexeddb_js.to_string())
                    .expect("indexeddb.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", xhr_js.to_string())
                    .expect("xhr.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", domparser_js.to_string())
                    .expect("domparser.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", performance_js.to_string())
                    .expect("performance.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", window_faces_js.to_string())
                    .expect("window_faces.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", message_port_js.to_string())
                    .expect("message_port.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", fetch_probe_js.to_string())
                    .expect("fetch_probe.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", global_surface_js.to_string())
                    .expect("global_surface.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", canvas_face_js.to_string())
                    .expect("canvas_face.js should not fail during snapshot creation");
                runtime
                    .execute_script("<anonymous>", interface_sweep_js.to_string())
                    .expect("interface_sweep.js should not fail during snapshot creation");
            })),
        },
        None,
    )
    .expect("Failed to create V8 snapshot");

    std::fs::write(&snapshot_path, &*output.output).expect("Failed to write snapshot");
    println!(
        "cargo:rustc-env=AGINXBROWSER_SNAPSHOT_PATH={}",
        snapshot_path.display()
    );

    for file in &output.files_loaded_during_snapshot {
        println!("cargo:rerun-if-changed={}", file.display());
    }
}
