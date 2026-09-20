//! Recording mock HTTP server + net-env guard shared by the test
//! binaries that exercise the fetch/session/flow paths. Split from
//! the server root (god-file ratchet).
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// Holds PRIVATE_NET_ENV_LOCK and clears the env var on drop, mirroring
/// diting_browser's NetGuard. The field is the point: holding the guard
/// is what serializes against tests asserting on the unset state.
#[allow(dead_code)] // the guard field is never read; holding it is the effect
pub(crate) struct NetEnvGuard(std::sync::MutexGuard<'static, ()>);

impl Drop for NetEnvGuard {
    fn drop(&mut self) {
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    }
}
pub(crate) fn net_env_guard() -> NetEnvGuard {
    let guard = crate::test_support::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
    NetEnvGuard(guard)
}

/// Local HTTP server on an ephemeral port recording every request line
/// plus body ("POST /submitted q=hello") into the shared log. Routes
/// match on "METHOD path" prefixes; misses 404. Serves 32 requests —
/// enough for a document plus its navigation hops.
pub(crate) fn recording_server(
    routes: &[(&'static str, &'static str)],
) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits: Arc<Mutex<Vec<String>>> = Arc::default();
    let hits2 = hits.clone();
    let routes: Vec<(String, &'static str)> = routes
        .iter()
        .map(|(k, body)| (k.to_string(), *body))
        .collect();
    std::thread::spawn(move || {
        for _ in 0..32 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let head = req.lines().next().unwrap_or("").to_string();
            let body = req
                .split_once("\r\n\r\n")
                .map(|(_, b)| b.to_string())
                .unwrap_or_default();
            hits2.lock().unwrap().push(format!("{} {}", head, body));
            // Route keys are "METHOD path", matched with the query string
            // ignored on BOTH sides — a request's ?access_token=... hits
            // a bare-path route (the API-chain norm), and a route that
            // pins a query (?token=1) still matches its own URL. An
            // unmatched request 404s.
            let method = head.split_whitespace().next().unwrap_or("");
            let path = head
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("");
            let (code, body_out) = match routes.iter().find(|(k, _)| {
                let mut it = k.split_whitespace();
                let rk_method = it.next().unwrap_or("");
                let rk_path = it.next().unwrap_or("").split('?').next().unwrap_or("");
                rk_method == method && rk_path == path
            }) {
                Some((_, b)) => ("200 OK", *b),
                None => ("404 Not Found", ""),
            };
            let resp = format!(
                "HTTP/1.1 {code}\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body_out.len(),
                body_out
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (port, hits)
}
