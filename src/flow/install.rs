//! flow_install — the ecosystem's consumption side (#143).
//!
//! Flows are shareable assets (cookies/storage stripped at export); DupHub
//! hosts them as templates, and this module pulls one down onto the local
//! workflow directory so the existing load chain (`available_workflows` /
//! `resolve_flow_doc`) picks it up with zero changes: on-disk beats
//! baked-in, a same-named built-in is overridden — install inherits both
//! rules by landing in the same place a dropped directory would.
//!
//! The wire is the dup protocol's templates scope (aginx-carrier
//! `crates/dup/src/remote.rs`), deliberately content-agnostic:
//!
//! - `GET {base}/api/templates/{name}/dup/manifest` →
//!   `{files: {relative path → sha256}, hash}`
//! - `GET {base}/api/templates/{name}/dup/file/{path}` → raw bytes,
//!   anonymous
//!
//! Uploading stays out of the engine (the `dup` CLI owns credentials);
//! this side only ever reads. `base` comes from `AGINXBROWSER_DUPHUB_URL`
//! (default the public hub), never from a request parameter — an
//! install must not turn into an arbitrary-URL fetch.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// A flow package is text plus templates. Caps keep a hostile or broken
/// remote from turning "install a flow" into "stream 2 GiB onto the disk":
/// 64 files, 4 MiB a file, 8 MiB total.
const MAX_FILES: usize = 64;
const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// The manifest shape the dup remote serves.
#[derive(serde::Deserialize)]
struct DupManifest {
    files: BTreeMap<String, String>,
    #[serde(default)]
    hash: String,
}

/// The public hub. Private hubs set `AGINXBROWSER_DUPHUB_URL`.
const DEFAULT_DUPHUB: &str = "https://duphub.com";

fn duphub_url() -> String {
    std::env::var("AGINXBROWSER_DUPHUB_URL")
        .unwrap_or_else(|_| DEFAULT_DUPHUB.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// A package-internal path must be relative, ordinary components only —
/// no leading `/`, no `..`, no backslashes, no trailing separator. `..`
/// and friends die here, before any path is ever joined onto the
/// workflow directory.
fn valid_pkg_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.ends_with('/')
        && !p.contains('\\')
        && Path::new(p)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Install a flow package from the configured hub. See [`install_from`].
pub async fn install(name: &str) -> Result<Value, String> {
    install_from(&duphub_url(), name, &super::workflow_dir()).await
}

/// Pull `name` from `base` and land it under `dir/<name>/`. Every failure
/// leaves the filesystem untouched: files stage in `.installing-<name>`
/// and only a fully verified, parseable package swaps into place.
pub(crate) async fn install_from(
    base: &str,
    name: &str,
    dir: &Path,
) -> Result<Value, String> {
    if !super::is_workflow_name(name) {
        return Err(format!(
            "invalid flow name {name:?} (lowercase/digits/dashes only)"
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    // Manifest first — it is the contract everything else verifies against.
    let resp = client
        .get(format!("{base}/api/templates/{name}/dup/manifest"))
        .send()
        .await
        .map_err(|e| format!("manifest fetch failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let note = if status.as_u16() == 404 {
            format!(" — no template named {name:?} on {base}")
        } else {
            String::new()
        };
        return Err(format!(
            "manifest fetch failed ({status}){note}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let manifest: DupManifest = resp
        .json()
        .await
        .map_err(|e| format!("manifest is not the dup shape: {e}"))?;

    if manifest.files.is_empty() {
        return Err("remote manifest lists no files".into());
    }
    if !manifest.files.contains_key("flow.json") {
        return Err("package has no flow.json — not a flow package".into());
    }
    if manifest.files.len() > MAX_FILES {
        return Err(format!(
            "package has {} files (cap {MAX_FILES})",
            manifest.files.len()
        ));
    }
    for path in manifest.files.keys() {
        if !valid_pkg_path(path) {
            return Err(format!(
                "manifest path {path:?} escapes the package directory"
            ));
        }
    }

    // Fetch and verify: every file's bytes must hash to the manifest's
    // promise — a package that fails anywhere is not installed at all.
    let mut fetched: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total: usize = 0;
    for (path, want) in &manifest.files {
        let resp = client
            .get(format!("{base}/api/templates/{name}/dup/file/{path}"))
            .send()
            .await
            .map_err(|e| format!("file {path:?} fetch failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("file {path:?} fetch failed ({})", resp.status()));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("file {path:?} read failed: {e}"))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!("file {path:?} is {} bytes (cap {MAX_FILE_BYTES})", bytes.len()));
        }
        total += bytes.len();
        if total > MAX_TOTAL_BYTES {
            return Err(format!("package exceeds {MAX_TOTAL_BYTES} bytes total"));
        }
        let got = sha256_hex(&bytes);
        if !got.eq_ignore_ascii_case(want) {
            return Err(format!(
                "integrity check failed for {path:?}: manifest says {want}, fetched bytes hash {got}"
            ));
        }
        fetched.push((path.clone(), bytes.to_vec()));
    }

    // The flow document must be a flow document before anything is
    // written — a package that would install unrunnable is refused.
    let flow_text = &fetched.iter().find(|(p, _)| p == "flow.json").unwrap().1;
    let doc: Value = serde_json::from_slice(flow_text)
        .map_err(|e| format!("flow.json is not valid JSON: {e}"))?;
    let steps = doc
        .get("steps")
        .and_then(|s| s.as_array())
        .ok_or("flow.json has no steps array")?;
    // Third-party eval disclosure: how much script this package carries.
    let eval_steps = steps
        .iter()
        .filter(|s| s.get("args").and_then(|a| a.get("script")).is_some())
        .count();

    // Stage everything, then swap. `.installing-` starts with a dot, so it
    // can never surface as a workflow (`is_workflow_name` rejects it) even
    // if a crash leaves it behind.
    std::fs::create_dir_all(dir).map_err(|e| format!("workflow dir {}: {e}", dir.display()))?;
    let staging = dir.join(format!(".installing-{name}"));
    let target = dir.join(name);
    let _ = std::fs::remove_dir_all(&staging);
    for (path, bytes) in &fetched {
        let dest = staging.join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("staging {path:?}: {e}"))?;
        }
        std::fs::write(&dest, bytes).map_err(|e| format!("staging {path:?}: {e}"))?;
    }
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(|e| format!("replacing previous install: {e}"))?;
    }
    std::fs::rename(&staging, &target).map_err(|e| format!("landing: {e}"))?;

    Ok(json!({
        "name": name,
        "source": base,
        "manifest_hash": manifest.hash,
        "files": manifest.files.keys().cloned().collect::<Vec<_>>(),
        "bytes": total,
        "steps": steps.len(),
        "eval_steps": eval_steps,
        "installed_at": target.display().to_string(),
        // On-disk beats baked-in: an install over a built-in name overrides
        // it (existing workflow semantics) — say so instead of implying
        // both exist.
        "overrides_builtin": super::BUILTIN_FLOWS.iter().any(|(n, _)| *n == name),
        "available_now": target.join("flow.json").is_file(),
        "disclosure": "third-party flow: its scripts execute with this engine's privileges \
             inside session pages on flow_run — review the steps before running",
    }))
}

/// The templates-market listing envelope (aginx-carrier `clone` crate
/// `search_templates` consumes the same shape).
#[derive(serde::Deserialize)]
struct TemplatesEnvelope {
    #[serde(default)]
    templates: Vec<TemplatesItem>,
    #[serde(default)]
    total: Option<u64>,
}

#[derive(serde::Deserialize)]
struct TemplatesItem {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    latest_version: String,
    #[serde(default)]
    download_count: i64,
    /// The market's asset kind (REQ aginx-hub-asset-kind §1): `agx-flow`
    /// for packages whose tree root carries a flow.json, `clone` for
    /// avatars. Absent on a hub that predates the protocol.
    #[serde(default)]
    kind: Option<String>,
}

/// List flow packages available on the configured hub — flow discovery
/// (REQ aginx-hub-asset-kind §4: flow_install takes a name; this is how
/// the agent learns the names). Queries the templates market with
/// `kind=agx-flow`, then re-filters client-side so a hub that ignores the
/// param can't sneak clones into the flow list. A listing with no `kind`
/// fields at all predates the protocol — the receipt says so instead of
/// presenting clones as installable flows.
pub async fn search_hub(query: &str) -> Result<Value, String> {
    search_hub_from(&duphub_url(), query).await
}

pub(crate) async fn search_hub_from(base: &str, query: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut url = url::Url::parse(&format!("{base}/api/templates"))
        .map_err(|e| format!("hub url: {e}"))?;
    url.query_pairs_mut()
        .append_pair("kind", "agx-flow")
        .append_pair("limit", "50");
    let q = query.trim();
    if !q.is_empty() {
        url.query_pairs_mut().append_pair("q", q);
    }

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("hub listing failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "hub listing failed ({status}): {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let listing: TemplatesEnvelope = resp
        .json()
        .await
        .map_err(|e| format!("hub listing is not the templates shape: {e}"))?;

    if !listing.templates.is_empty()
        && !listing.templates.iter().any(|t| t.kind.is_some())
    {
        // Old hub: the kind filter is silently ignored and every template
        // (avatars included) comes back. Naming them as flows would just
        // produce "package has no flow.json" on install. An empty listing
        // is NOT this case — "no flows" is a valid answer from any hub.
        return Ok(json!({
            "hub": base,
            "kind_filter": "unsupported",
            "templates_seen": listing.templates.len(),
            "flows": [],
            "note": "hub listing predates the kind protocol — cannot tell flows \
                 from clones; upgrade the hub (templates.kind) to enable discovery",
        }));
    }

    let flows: Vec<Value> = listing
        .templates
        .iter()
        .filter(|t| t.kind.as_deref() == Some("agx-flow"))
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description.chars().take(200).collect::<String>(),
                "version": t.latest_version,
                "downloads": t.download_count,
            })
        })
        .collect();
    Ok(json!({
        "hub": base,
        "query": q,
        "total": listing.total.unwrap_or(flows.len() as u64),
        "flows": flows,
        "install": "flow_install(<name>)",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback dup remote: routes the two protocol endpoints over a raw
    /// TcpListener (no framework), serving `files` with true sha256s in the
    /// manifest. `tamper` swaps one path's served bytes without updating
    /// the manifest — the integrity-failure fixture.
    async fn mock_duphub(
        files: BTreeMap<String, Vec<u8>>,
        tamper: Option<(&'static str, Vec<u8>)>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let files = files.clone();
                let tamper = tamper.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    let (status, ctype, body): (&str, &str, Vec<u8>) = if path
                        .ends_with("/dup/manifest")
                    {
                        let manifest = json!({
                            "files": files
                                .iter()
                                .map(|(p, b)| (p.clone(), sha256_hex(b)))
                                .collect::<BTreeMap<String, String>>(),
                            "hash": "test-state-id",
                        });
                        ("200 OK", "application/json", manifest.to_string().into_bytes())
                    } else if let Some(rest) = path.split("/dup/file/").nth(1) {
                        // Un-percent-encode is unnecessary for the fixture
                        // paths (plain segments).
                        match (files.get(rest), tamper.as_ref().and_then(|(t, b)| {
                            if *t == rest { Some(b.clone()) } else { None }
                        })) {
                            (_, Some(b)) => ("200 OK", "application/octet-stream", b),
                            (Some(b), None) => ("200 OK", "application/octet-stream", b.clone()),
                            _ => (
                                "404 Not Found",
                                "text/plain",
                                b"no such file".to_vec(),
                            ),
                        }
                    } else {
                        ("404 Not Found", "text/plain", b"no such route".to_vec())
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn flow_json(steps: usize) -> Vec<u8> {
        let doc = json!({
            "create": {"url": "https://example.com"},
            "steps": (0..steps)
                .map(|i| json!({"op": "eval", "args": {"script": format!("1 + {i}")}}))
                .collect::<Vec<_>>(),
        });
        serde_json::to_vec(&doc).unwrap()
    }

    fn scratch_dir(tag: &str) -> PathBuf2 {
        let d = std::env::temp_dir().join(format!("agx-flow-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    // PathBuf alias so the helper reads cleanly without importing at top.
    type PathBuf2 = std::path::PathBuf;

    #[tokio::test(flavor = "current_thread")]
    async fn happy_path_lands_and_the_load_chain_sees_it() {
        let mut files = BTreeMap::new();
        files.insert("flow.json".to_string(), flow_json(3));
        files.insert("templates/body.html".to_string(), b"<p>hi</p>".to_vec());
        let base = mock_duphub(files, None).await;
        let dir = scratch_dir("happy");

        let receipt = install_from(&base, "third-party-flow", &dir).await.unwrap();
        assert_eq!(receipt["name"], "third-party-flow");
        assert_eq!(receipt["steps"], 3);
        assert_eq!(receipt["eval_steps"], 3);
        // bytes = the sum of what was verified and written.
        let flow_len = std::fs::read(dir.join("third-party-flow/flow.json")).unwrap().len();
        assert_eq!(receipt["bytes"], (flow_len + 9) as u64); // + b"<p>hi</p>"
        assert_eq!(receipt["files"].as_array().map(Vec::len), Some(2));
        assert_eq!(receipt["available_now"], true);
        assert_eq!(receipt["overrides_builtin"], false);
        // The auxiliary file rode along into its subdirectory.
        let landed = std::fs::read(dir.join("third-party-flow/templates/body.html")).unwrap();
        assert_eq!(landed, b"<p>hi</p>");
        // The load chain's own gate: the dir qualifies as a workflow.
        assert!(dir.join("third-party-flow/flow.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tampered_file_refuses_to_install() {
        let mut files = BTreeMap::new();
        files.insert("flow.json".to_string(), flow_json(1));
        let base = mock_duphub(
            files,
            Some(("flow.json", flow_json(99))), // served bytes ≠ manifest hash
        )
        .await;
        let dir = scratch_dir("tamper");

        let err = install_from(&base, "evil", &dir).await.unwrap_err();
        assert!(err.contains("integrity check failed"), "{err}");
        assert!(!dir.join("evil").exists(), "nothing may land on failure");
        assert!(!dir.join(".installing-evil").exists(), "staging must be gone or never complete");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn traversal_paths_are_rejected_before_any_fetch() {
        let mut files = BTreeMap::new();
        files.insert("flow.json".to_string(), flow_json(1));
        files.insert("../escape.txt".to_string(), b"x".to_vec());
        let base = mock_duphub(files, None).await;
        let dir = scratch_dir("traversal");

        let err = install_from(&base, "escape-artist", &dir).await.unwrap_err();
        assert!(err.contains("escapes the package directory"), "{err}");
        assert!(!dir.join("escape-artist").exists());
        assert!(!dir.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn package_without_flow_json_is_not_a_flow() {
        let mut files = BTreeMap::new();
        files.insert("README.md".to_string(), b"not a flow".to_vec());
        let base = mock_duphub(files, None).await;
        let dir = scratch_dir("noflow");

        let err = install_from(&base, "readme-only", &dir).await.unwrap_err();
        assert!(err.contains("no flow.json"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broken_flow_json_is_rejected_before_landing() {
        let mut files = BTreeMap::new();
        files.insert("flow.json".to_string(), b"{\"steps\": 42}".to_vec()); // not an array
        let base = mock_duphub(files, None).await;
        let dir = scratch_dir("badjson");

        let err = install_from(&base, "broken", &dir).await.unwrap_err();
        assert!(err.contains("steps array"), "{err}");
        assert!(!dir.join("broken").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_name_never_reaches_the_network() {
        let base = "http://127.0.0.1:1".to_string(); // nothing listening
        let dir = scratch_dir("badname");
        for name in ["../evil", "UPPER", "with space", "a/b", ""] {
            let err = install_from(&base, name, &dir).await.unwrap_err();
            assert!(err.contains("invalid flow name"), "{name}: {err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reinstall_replaces_the_previous_install() {
        let mut files = BTreeMap::new();
        files.insert("flow.json".to_string(), flow_json(1));
        let base = mock_duphub(files.clone(), None).await;
        let dir = scratch_dir("reinstall");

        install_from(&base, "twice", &dir).await.unwrap();
        // Second manifest drops flow.json for a different shape entirely —
        // the swap replaces, not merges.
        let mut v2 = BTreeMap::new();
        v2.insert("flow.json".to_string(), flow_json(5));
        let base2 = mock_duphub(v2, None).await;
        let receipt = install_from(&base2, "twice", &dir).await.unwrap();
        assert_eq!(receipt["steps"], 5);
        let text =
            std::fs::read_to_string(dir.join("twice/flow.json")).unwrap();
        assert!(text.contains("1 + 4"), "second install's bytes must be the ones on disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- flow discovery (REQ aginx-hub-asset-kind §4) ----

    /// A loopback templates market: serves `listing` at `/api/templates`
    /// and records every request target (query string included) so tests
    /// can pin what the discovery face actually asked for.
    async fn mock_hub_market(
        listing: String,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_srv = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let listing = listing.clone();
                let seen = seen_srv.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    seen.lock().unwrap().push(path.clone());
                    let (status, ctype, body): (&str, &str, Vec<u8>) = if path
                        .starts_with("/api/templates")
                    {
                        ("200 OK", "application/json", listing.into_bytes())
                    } else {
                        ("404 Not Found", "text/plain", b"no such route".to_vec())
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn flow_search_lists_only_flow_kind() {
        let listing = json!({
            "templates": [
                {"name": "xhs-post", "description": "post to xhs", "latest_version": "1.2.0",
                 "download_count": 41, "kind": "agx-flow"},
                {"name": "qr-login", "description": "", "latest_version": "0.1.0",
                 "download_count": 3, "kind": "agx-flow"},
                {"name": "chennan-avatar", "description": "a clone, not a flow",
                 "latest_version": "2.0", "download_count": 99, "kind": "clone"},
            ],
            "total": 3,
        })
        .to_string();
        let (base, seen) = mock_hub_market(listing).await;

        let receipt = search_hub_from(&base, "").await.unwrap();
        let names: Vec<&str> = receipt["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["xhs-post", "qr-login"], "clones must not surface as flows");
        assert_eq!(receipt["total"], 3);
        assert_eq!(receipt["kind_filter"], json!(null), "supported hub: no degrade note");
        // The ask carried the kind filter even though the client re-filters.
        assert!(seen.lock().unwrap()[0].contains("kind=agx-flow"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn flow_search_passes_the_query_through() {
        let (base, seen) = mock_hub_market(json!({"templates": [], "total": 0}).to_string()).await;

        let receipt = search_hub_from(&base, "  xhs  ").await.unwrap();
        assert_eq!(receipt["flows"].as_array().map(Vec::len), Some(0));
        assert_eq!(receipt["query"], "xhs", "trimmed query in the receipt");
        assert!(
            seen.lock().unwrap()[0].contains("q=xhs"),
            "query must ride to the hub: {}",
            seen.lock().unwrap()[0]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn flow_search_degrades_on_a_kindless_hub() {
        // A hub predating the kind protocol ignores the filter param and
        // returns every template — clones included. The receipt must say
        // "unsupported" and name nothing, not present clones as flows.
        let listing = json!({
            "templates": [
                {"name": "some-avatar", "description": "clone", "latest_version": "1",
                 "download_count": 7},
                {"name": "another", "description": "clone", "latest_version": "1",
                 "download_count": 8},
            ],
            "total": 2,
        })
        .to_string();
        let (base, _seen) = mock_hub_market(listing).await;

        let receipt = search_hub_from(&base, "").await.unwrap();
        assert_eq!(receipt["kind_filter"], "unsupported");
        assert_eq!(receipt["templates_seen"], 2);
        assert_eq!(receipt["flows"].as_array().map(Vec::len), Some(0));
        assert!(receipt["note"].as_str().unwrap().contains("upgrade the hub"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn flow_search_reports_an_unreachable_hub() {
        let err = search_hub_from("http://127.0.0.1:1", "").await.unwrap_err();
        assert!(err.contains("hub listing failed"), "{err}");
    }
}
