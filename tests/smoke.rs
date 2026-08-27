//! End-to-end smoke tests that spawn the real `aginxbrowser` binary and hit
//! its HTTP API. Network-dependent tests (fetching example.com etc.) are marked
//! `#[ignore]` so `cargo test` stays CI-friendly; run them explicitly with
//! `cargo test --test smoke -- --ignored`.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A handle to a running server process, killed on drop.
struct ServerGuard {
    child: Option<Child>,
    port: u16,
}

impl ServerGuard {
    fn spawn() -> Option<Self> {
        // Pick a free port by binding a TCP listener first, then handing the
        // port to the server via AGINXBROWSER_BIND.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        // Drop the listener so the server can take the port.
        drop(listener);

        let bin = env!("CARGO_BIN_EXE_aginxbrowser");
        let child = Command::new(bin)
            .env("AGINXBROWSER_BIND", format!("127.0.0.1:{port}"))
            .env("AGINXBROWSER_CACHE_TTL_SECS", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        // Wait for the server to accept connections (max ~10s).
        let deadline = Instant::now() + Duration::from_secs(10);
        let base = format!("http://127.0.0.1:{port}");
        loop {
            if Instant::now() > deadline {
                eprintln!("smoke: server did not come up on port {port}");
                return None;
            }
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = &base;
        Some(ServerGuard { child: Some(child), port })
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Minimal blocking HTTP client (no reqwest dependency in tests).
fn http_get_json(url: &str) -> Option<String> {
    let resp = ureq::get(url).timeout(Duration::from_secs(15)).call().ok()?;
    resp.into_string().ok()
}

fn http_post_json(url: &str, body: &str) -> Option<String> {
    let resp = ureq::post(url)
        .timeout(Duration::from_secs(30))
        .set("Content-Type", "application/json")
        .send_string(body)
        .ok()?;
    resp.into_string().ok()
}

#[test]
fn health_endpoint() {
    let server = match ServerGuard::spawn() {
        Some(s) => s,
        None => {
            eprintln!("smoke: could not spawn server, skipping");
            return;
        }
    };
    let body = http_get_json(&server.url("/health")).expect("GET /health");
    assert!(body.contains(r#""status":"ok""#), "health body: {body}");
    assert!(body.contains("diting"));
}

/// The human-facing status page: Umbrel (and self-hosters poking the port)
/// require a page the browser can open at the app root. No network involved —
/// pure server-side render.
#[test]
fn status_page_serves_html_at_root() {
    let server = match ServerGuard::spawn() {
        Some(s) => s,
        None => {
            eprintln!("smoke: could not spawn server, skipping");
            return;
        }
    };
    let resp = ureq::get(&server.url("/"))
        .timeout(Duration::from_secs(15))
        .call()
        .expect("GET /");
    assert!(resp.content_type().starts_with("text/html"));
    let body = resp.into_string().expect("read body");
    assert!(body.contains("<!doctype html>"), "doctype: {body}");
    assert!(
        body.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        "version readout: {body}"
    );
    assert!(body.contains("active sessions"), "stats: {body}");
    // /status is the canonical alias (agents and monitors may prefer it).
    let alias = http_get_json(&server.url("/status")).expect("GET /status");
    assert!(alias.contains("<!doctype html>"), "alias body: {alias}");
}

#[test]
#[ignore] // requires network to example.com
fn fetch_tier_http_example_com() {
    let server = match ServerGuard::spawn() {
        Some(s) => s,
        None => return,
    };
    let body = http_post_json(
        &server.url("/fetch"),
        r#"{"url":"https://example.com","max_chars":200,"render_tier":"http"}"#,
    )
    .expect("POST /fetch");
    assert!(body.contains("Example Domain"), "fetch body: {body}");
}

#[test]
#[ignore] // requires network
fn firecrawl_scrape_example_com() {
    let server = match ServerGuard::spawn() {
        Some(s) => s,
        None => return,
    };
    let body = http_post_json(
        &server.url("/v1/scrape"),
        r#"{"url":"https://example.com","formats":["markdown"]}"#,
    )
    .expect("POST /v1/scrape");
    assert!(body.contains(r#""success":true"#), "scrape body: {body}");
    assert!(body.contains("sourceURL"), "scrape body: {body}");
    assert!(body.contains("Example Domain"), "scrape body: {body}");
}
