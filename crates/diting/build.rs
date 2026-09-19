use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=js/bootstrap.js");
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

    let output = deno_core::snapshot::create_snapshot(
        deno_core::snapshot::CreateSnapshotOptions {
            cargo_manifest_dir: env!("CARGO_MANIFEST_DIR"),
            startup_snapshot: None,
            skip_op_registration: true,
            extensions: vec![],
            extension_transpiler: None,
            with_runtime_cb: Some(Box::new(move |runtime| {
                runtime
                    .execute_script("<diting:bootstrap>", bootstrap_js.to_string())
                    .expect("bootstrap.js should not fail during snapshot creation");
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
