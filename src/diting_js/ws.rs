//! Page-facing WebSocket transport (obscura #925 family). The old bootstrap
//! stub fired `open` from a microtask and dropped every `send`, so challenge
//! protocols that negotiate over WebSocket (DDoS-Guard) could never complete.
//! This module owns the real sockets: a small registry inside `JsState` (the
//! !Send op land) plus one tokio IO task per socket (the Send land) that
//! speaks the wire protocol through `tokio_tungstenite::client_async` over a
//! TCP/TLS connection we dial ourselves — same SSRF posture as fetch
//! (scheme gate at open, per-resolved-address `is_forbidden_ip` at connect).

use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use deno_core::op2;
use deno_core::OpState;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::ops::SharedState;

/// Sockets a single page realm may hold. A page that leaks sockets (or
/// `close()`s during CONNECTING, where the IO task lingers up to the connect
/// timeout) hits this instead of growing without bound.
const WS_SOCKET_CAP: usize = 8;
/// Inbound messages buffered per socket before the IO task gives up. The JS
/// pump drains via `op_ws_next_message`; a page that never pumps must not
/// turn the channel into unbounded memory.
const WS_INBOUND_CAP: usize = 512;
const WS_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// JS → wire. Close carries the requested code/reason so the IO task can
/// still send a proper close frame if the handshake lands late.
pub(crate) enum WsOutbound {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
}

/// Wire → JS, delivered in FIFO order through the inbound channel.
pub(crate) enum WsInbound {
    Open,
    Text(String),
    Binary(Vec<u8>),
    Error(String),
    Close(u16, String),
}

impl WsInbound {
    /// The JSON the `op_ws_next_message` op returns; the bootstrap pump
    /// parses it and dispatches the matching DOM event.
    pub(crate) fn to_json(&self) -> String {
        match self {
            WsInbound::Open => "{\"kind\":\"open\"}".to_string(),
            WsInbound::Text(t) => {
                let mut obj = serde_json::Map::new();
                obj.insert("kind".into(), "message".into());
                obj.insert("binary".into(), false.into());
                obj.insert("data".into(), t.clone().into());
                serde_json::Value::Object(obj).to_string()
            }
            WsInbound::Binary(b) => {
                let mut obj = serde_json::Map::new();
                obj.insert("kind".into(), "message".into());
                obj.insert("binary".into(), true.into());
                obj.insert("data".into(), b64_encode(b).into());
                serde_json::Value::Object(obj).to_string()
            }
            WsInbound::Error(e) => {
                let mut obj = serde_json::Map::new();
                obj.insert("kind".into(), "error".into());
                obj.insert("message".into(), e.clone().into());
                serde_json::Value::Object(obj).to_string()
            }
            WsInbound::Close(code, reason) => {
                let mut obj = serde_json::Map::new();
                obj.insert("kind".into(), "close".into());
                obj.insert("code".into(), (*code as u64).into());
                obj.insert("reason".into(), reason.clone().into());
                serde_json::Value::Object(obj).to_string()
            }
        }
    }
}

pub(crate) struct WsSocket {
    pub url: String,
    pub outbound_tx: mpsc::UnboundedSender<WsOutbound>,
    /// The !Send half lives in the registry (op land); the IO task holds the
    /// paired sender. `poll_recv` inside the async op wakes the isolate
    /// through tokio's registered waker — no manual waker bridge.
    pub inbound: Rc<RefCell<mpsc::UnboundedReceiver<WsInbound>>>,
    /// Messages currently buffered in `inbound` (unbounded channels have no
    /// len); the IO task refuses to deliver past WS_INBOUND_CAP so a socket
    /// nobody pumps can't grow the channel without bound.
    pub inbound_count: Arc<AtomicUsize>,
}

#[derive(Default)]
pub(crate) struct WsRegistry {
    pub sockets: HashMap<u32, WsSocket>,
    pub next_id: u32,
}

/// `ws://host:port/path` → `http(s)://…` — the shape `validate_fetch_url` and
/// the cookie jar want (the jar's Secure check tests `scheme == "https"`, and
/// per W3C wss is the secure variant).
pub(crate) fn translate_ws_url(u: &url::Url) -> Option<url::Url> {
    let translated = match u.scheme() {
        "ws" => format!("http://{}", &u.as_str()[5..]),
        "wss" => format!("https://{}", &u.as_str()[6..]),
        _ => return None,
    };
    url::Url::parse(&translated).ok()
}

/// The per-socket tokio task: dial (DNS → SSRF check → TCP/SOCKS → TLS),
/// handshake, then shuttle messages until either side closes.
struct WsConnectJob {
    host: String,
    port: u16,
    tls: bool,
    allow_private: bool,
    proxy_url: Option<String>,
    request: http::Request<()>,
}

enum WsStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for WsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            WsStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            WsStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for WsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            WsStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            WsStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            WsStream::Plain(s) => Pin::new(s).poll_flush(cx),
            WsStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            WsStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            WsStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

fn deliver(
    tx: &mpsc::UnboundedSender<WsInbound>,
    count: &AtomicUsize,
    ev: WsInbound,
) -> bool {
    if count.load(Ordering::Relaxed) >= WS_INBOUND_CAP {
        return false;
    }
    if tx.send(ev).is_ok() {
        count.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

impl WsConnectJob {
    async fn dial(&self) -> Result<WsStream, String> {
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .map_err(|e| format!("DNS resolution failed for '{}': {e}", self.host))?
            .collect();
        if addrs.is_empty() {
            return Err(format!("DNS resolution returned no addresses for '{}'", self.host));
        }
        let allow = self.allow_private || crate::diting_net::client::env_allows_private_network();
        let target = if allow {
            addrs[0]
        } else {
            match addrs.iter().find(|sa| !crate::diting_net::client::is_forbidden_ip(sa.ip())) {
                Some(sa) => *sa,
                None => {
                    return Err(format!(
                        "SSRF blocked: '{}' resolves only to forbidden addresses",
                        self.host
                    ))
                }
            }
        };

        // A configured proxy terminates TCP at the proxy hop; direct
        // otherwise. socks5/socks5h ride tokio_socks, http proxies become
        // CONNECT tunnels — tungstenite sees a plain stream either way.
        if let Some(proxy) = &self.proxy_url {
            let lower = proxy.to_ascii_lowercase();
            if lower.starts_with("socks5://") || lower.starts_with("socks5h://") {
                let stream = tokio_socks::tcp::Socks5Stream::connect(
                    proxy.as_str(),
                    (self.host.as_str(), self.port),
                )
                .await
                .map_err(|e| format!("SOCKS proxy connect failed: {e}"))?;
                return if self.tls {
                    Ok(WsStream::Tls(Box::new(
                        tls_upgrade(stream.into_inner(), &self.host).await?,
                    )))
                } else {
                    Ok(WsStream::Plain(stream.into_inner()))
                };
            }
            if lower.starts_with("http://") {
                return self.dial_via_http_proxy(proxy).await;
            }
            if lower.starts_with("https://") {
                return Err("https:// proxies are not supported for WebSocket yet".into());
            }
        }

        let tcp = TcpStream::connect(target)
            .await
            .map_err(|e| format!("TCP connect to {target} failed: {e}"))?;
        let _ = tcp.set_nodelay(true);
        if self.tls {
            Ok(WsStream::Tls(Box::new(tls_upgrade(tcp, &self.host).await?)))
        } else {
            Ok(WsStream::Plain(tcp))
        }
    }

    async fn dial_via_http_proxy(&self, proxy: &str) -> Result<WsStream, String> {
        let authority = match proxy.split_once("://") {
            Some((_, rest)) => rest,
            None => proxy,
        };
        let tcp = TcpStream::connect(authority)
            .await
            .map_err(|e| format!("TCP connect to proxy {authority} failed: {e}"))?;
        let _ = tcp.set_nodelay(true);
        let hostport = format!("{}:{}", self.host, self.port);
        let req = format!(
            "CONNECT {hp} HTTP/1.1\r\nHost: {hp}\r\n\r\n",
            hp = hostport
        );
        let (mut rd, mut wr) = tokio::io::split(tcp);
        wr.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
        wr.flush().await.map_err(|e| e.to_string())?;
        let mut head = Vec::new();
        // Read until the header terminator; a CONNECT reply is tiny.
        let mut byte = [0u8; 1];
        loop {
            let n = rd.read(&mut byte).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("proxy closed the CONNECT tunnel".into());
            }
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
            if head.len() > 16 * 1024 {
                return Err("proxy CONNECT reply too large".into());
            }
        }
        let status = head.split(|b| *b == b' ').nth(1).map(|s| s.to_vec()).unwrap_or_default();
        if status != b"200" {
            return Err(format!("proxy CONNECT refused (status {})", String::from_utf8_lossy(&status)));
        }
        let tcp = rd.unsplit(wr);
        if self.tls {
            Ok(WsStream::Tls(Box::new(tls_upgrade(tcp, &self.host).await?)))
        } else {
            Ok(WsStream::Plain(tcp))
        }
    }

    async fn run(
        self,
        inbound_tx: mpsc::UnboundedSender<WsInbound>,
        inbound_count: Arc<AtomicUsize>,
        mut outbound_rx: mpsc::UnboundedReceiver<WsOutbound>,
    ) {
        let connect = async {
            let stream = self.dial().await?;
            tokio_tungstenite::client_async(self.request.clone(), stream)
                .await
                .map(|(ws, _)| ws)
                .map_err(|e| format!("WebSocket handshake failed: {e}"))
        };
        let mut ws = match tokio::time::timeout(WS_CONNECT_TIMEOUT, connect).await {
            Ok(Ok(ws)) => {
                if !deliver(&inbound_tx, &inbound_count, WsInbound::Open) {
                    return;
                }
                ws
            }
            Ok(Err(e)) => {
                let _ = inbound_tx.send(WsInbound::Error(e));
                let _ = inbound_tx.send(WsInbound::Close(1006, String::new()));
                return;
            }
            Err(_) => {
                let _ = inbound_tx.send(WsInbound::Error("WebSocket connection timed out".into()));
                let _ = inbound_tx.send(WsInbound::Close(1006, String::new()));
                return;
            }
        };

        let mut close_sent = false;
        loop {
            tokio::select! {
                out = outbound_rx.recv(), if !close_sent => {
                    let msg = match out {
                        Some(WsOutbound::Text(t)) => tokio_tungstenite::tungstenite::Message::Text(t),
                        Some(WsOutbound::Binary(b)) => tokio_tungstenite::tungstenite::Message::Binary(b),
                        Some(WsOutbound::Close(code, reason)) => {
                            let _ = ws.send(tokio_tungstenite::tungstenite::Message::Close(Some(
                                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                    code: code.into(),
                                    reason: reason.into(),
                                },
                            )))
                            .await;
                            let _ = ws.flush().await;
                            // Linger for the peer's close reply so the JS close
                            // event carries the negotiated code instead of a
                            // synthetic 1006.
                            close_sent = true;
                            continue;
                        }
                        // Registry gone (realm torn down): close politely.
                        None => {
                            let _ = ws.send(tokio_tungstenite::tungstenite::Message::Close(None)).await;
                            let _ = ws.flush().await;
                            return;
                        }
                    };
                    if ws.send(msg).await.is_err() {
                        return;
                    }
                }
                read = ws.next() => {
                    let ev = match read {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => WsInbound::Text(t),
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(b))) => WsInbound::Binary(b),
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(p))) => {
                            let _ = ws.send(tokio_tungstenite::tungstenite::Message::Pong(p)).await;
                            continue;
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_))) | Some(Ok(tokio_tungstenite::tungstenite::Message::Frame(_))) => continue,
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(cf))) => {
                            let (code, reason) = cf
                                .map(|f| (u16::from(f.code), f.reason.to_string()))
                                .unwrap_or((1005, String::new()));
                            WsInbound::Close(code, reason)
                        }
                        // Transport death or EOF without a close frame: Chrome
                        // reports an error event, then close 1006.
                        Some(Err(e)) => {
                            let _ = deliver(&inbound_tx, &inbound_count, WsInbound::Error(e.to_string()));
                            WsInbound::Close(1006, String::new())
                        }
                        None => WsInbound::Close(1006, String::new()),
                    };
                    let closing = matches!(ev, WsInbound::Close(..));
                    if !deliver(&inbound_tx, &inbound_count, ev) {
                        return;
                    }
                    if closing {
                        return;
                    }
                }
            }
        }
    }
}

async fn tls_upgrade(tcp: TcpStream, host: &str) -> Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("invalid TLS server name '{host}': {e}"))?;
    connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| format!("TLS handshake with '{host}' failed: {e}"))
}

// ---------------------------------------------------------------------------
// Ops. `op_ws_open`/`op_ws_next_message` are async(deferred) like op_sleep /
// op_fetch_url; the two sync ops only touch channels.
// ---------------------------------------------------------------------------

fn b64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn ws_error_json(message: &str) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("error".into(), message.into());
    serde_json::Value::Object(obj).to_string()
}

/// Returns Err(the JSON error payload) when the open must fail. JS surfaces
/// every failure as error + close(1006) events — the constructor itself only
/// throws for malformed URLs, which bootstrap validates first.
#[op2(async(deferred), fast)]
#[string]
pub(crate) async fn op_ws_open(
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
    #[string] protocols_json: String,
) -> String {
    if tokio::runtime::Handle::try_current().is_err() {
        return ws_error_json("no async runtime (bare test isolate)");
    }
    let parsed = match url::Url::parse(&url) {
        Ok(u) => u,
        Err(_) => return ws_error_json(&format!("invalid URL '{url}'")),
    };
    let tls = parsed.scheme() == "wss";
    if parsed.scheme() != "ws" && !tls {
        return ws_error_json(&format!("unsupported scheme '{}'", parsed.scheme()));
    }
    let host = match parsed.host_str() {
        Some(h) => h.to_string(),
        None => return ws_error_json(&format!("URL '{url}' has no host")),
    };
    let port = parsed.port_or_known_default().unwrap_or(if tls { 443 } else { 80 });
    if host.is_empty() {
        return ws_error_json(&format!("URL '{url}' has an empty host"));
    }

    // Same scheme gate as fetch — the translated http(s) form is what the
    // validator (and the cookie jar) understand.
    if let Some(translated) = translate_ws_url(&parsed) {
        if let Err(e) = super::ops::validate_fetch_url(&translated) {
            return ws_error_json(&e);
        }
    }

    // setBlockedURLs parity with fetch: a page pattern-blocked from a URL
    // shouldn't reach it over a side channel either.
    {
        let gs_rc = state.borrow().borrow::<SharedState>().clone();
        let gs = gs_rc.borrow();
        if let Some(pattern) = gs
            .blocked_urls
            .iter()
            .find(|p| *p == "*" || url.contains(p.as_str()) || super::ops::glob_match(p, &url))
            .cloned()
        {
            return ws_error_json(&format!(
                "blocked by Network.setBlockedURLs pattern: {pattern}"
            ));
        }
    }

    let protocols: Vec<String> = serde_json::from_str::<Vec<String>>(&protocols_json).unwrap_or_default();

    let (http_client, cookie_header, origin) = {
        let gs_rc = state.borrow().borrow::<SharedState>().clone();
        let gs = gs_rc.borrow();
        if gs.ws_registry.sockets.len() >= WS_SOCKET_CAP {
            return ws_error_json("too many open WebSocket connections");
        }
        (
            gs.http_client.clone(),
            translated_cookie_header(&parsed, &gs.cookie_jar),
            origin_of(&gs.url),
        )
    };
    // The client's UA/header state lives in tokio RwLocks; await them outside
    // any JsState borrow.
    let (ua, accept_language, extra_headers, allow_private, proxy_url) = match &http_client {
        Some(c) => (
            c.user_agent.read().await.clone(),
            c.accept_language.read().await.clone(),
            c.extra_headers
                .read()
                .await
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            c.allow_private_network,
            c.proxy_url().map(|s| s.to_string()),
        ),
        None => (String::new(), String::new(), Vec::new(), false, None),
    };

        let mut builder = http::Request::builder()
            .method("GET")
            .uri(parsed.as_str())
            .header("Host", http_host(&parsed))
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade");
        let key = generate_ws_key();
        builder = builder
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", key);
        if !origin.is_empty() {
            builder = builder.header("Origin", origin);
        }
        if !ua.is_empty() {
            builder = builder.header("User-Agent", ua);
        }
        if !accept_language.is_empty() {
            builder = builder.header("Accept-Language", accept_language);
        }
        if !cookie_header.is_empty() {
            builder = builder.header("Cookie", cookie_header.clone());
        }
        if !protocols.is_empty() {
            builder = builder.header("Sec-WebSocket-Protocol", protocols.join(", "));
        }
        for (k, v) in extra_headers {
            // Reserved headers stay ours (tungstenite validates the upgrade
            // shape); everything the context layered on goes through.
            let lower = k.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "host" | "upgrade" | "connection" | "sec-websocket-key" | "sec-websocket-version"
                    | "sec-websocket-protocol" | "origin" | "cookie" | "user-agent"
            ) {
                continue;
            }
            builder = builder.header(k.as_str(), v.as_str());
        }
        let request = match builder.body(()) {
            Ok(r) => r,
            Err(e) => return ws_error_json(&format!("handshake request invalid: {e}")),
        };
    let job = WsConnectJob {
        host,
        port,
        tls,
        allow_private,
        proxy_url,
        request,
    };

    let id = {
        let gs_rc = state.borrow().borrow::<SharedState>().clone();
        let mut gs = gs_rc.borrow_mut();
        gs.ws_registry.next_id += 1;
        let id = gs.ws_registry.next_id;
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let inbound_count = Arc::new(AtomicUsize::new(0));
        gs.ws_registry.sockets.insert(
            id,
            WsSocket {
                url: url.clone(),
                outbound_tx: out_tx,
                inbound: Rc::new(RefCell::new(in_rx)),
                inbound_count: inbound_count.clone(),
            },
        );
        tokio::spawn(job.run(in_tx, inbound_count, out_rx));
        id
    };
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), (id as u64).into());
    serde_json::Value::Object(obj).to_string()
}

fn translated_cookie_header(ws_url: &url::Url, jar: &Option<Arc<crate::diting_net::cookies::CookieJar>>) -> String {
    let Some(jar) = jar else { return String::new() };
    match translate_ws_url(ws_url) {
        Some(secure_form) => jar.get_cookie_header(&secure_form),
        None => String::new(),
    }
}

fn origin_of(document_url: &str) -> String {
    match url::Url::parse(document_url) {
        // The url crate normalizes default ports away at parse time, so a
        // Some(port) here is always non-default — Origin serialization wants
        // exactly that.
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}", u.scheme()),
            (Some(host), None) => format!("{}://{host}", u.scheme()),
            _ => String::new(),
        },
        Err(_) => String::new(),
    }
}

fn http_host(u: &url::Url) -> String {
    match u.port_or_known_default() {
        Some(port) => format!("{}:{}", u.host_str().unwrap_or_default(), port),
        None => u.host_str().unwrap_or_default().to_string(),
    }
}

/// 16 random bytes, base64 — the RFC 6455 client key (tungstenite verifies
/// the response's Sec-WebSocket-Accept against exactly this value).
fn generate_ws_key() -> String {
    use base64::Engine as _;
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS randomness unavailable");
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Resolves with the next wire → JS event (open / message / error / close).
/// Parks the op on the inbound channel when nothing is buffered — tokio's
/// mpsc wakes the registered waker on send, which is the isolate's own
/// poll driver, so the JS `await` resumes without any manual bridge.
enum Taken {
    Event(WsInbound),
    Dead,
    Pending,
}

#[op2(async(deferred), fast)]
#[string]
pub(crate) async fn op_ws_next_message(state: Rc<RefCell<OpState>>, #[number] id: u64) -> String {
    if tokio::runtime::Handle::try_current().is_err() {
        return WsInbound::Close(1006, String::new()).to_json();
    }
    let id = u32::try_from(id).unwrap_or(0);
    std::future::poll_fn(move |cx| {
        let gs_rc = state.borrow().borrow::<SharedState>().clone();
        let mut gs = gs_rc.borrow_mut();
        // Extract the event under one borrow scope; the delivery side effects
        // (network-log push, registry removal) need their own `&mut JsState`.
        let taken = {
            let reg = &mut gs.ws_registry;
            match reg.sockets.get_mut(&id) {
                None => Taken::Dead,
                Some(sock) => {
                    let mut rx = sock.inbound.borrow_mut();
                    match rx.poll_recv(cx) {
                        Poll::Ready(Some(ev)) => {
                            sock.inbound_count.fetch_sub(1, Ordering::Relaxed);
                            Taken::Event(ev)
                        }
                        Poll::Ready(None) => Taken::Dead,
                        Poll::Pending => Taken::Pending,
                    }
                }
            }
        };
        match taken {
            Taken::Event(ev) => Poll::Ready(finish_event(&mut gs, id, ev)),
            Taken::Dead => Poll::Ready(WsInbound::Close(1006, String::new()).to_json()),
            Taken::Pending => Poll::Pending,
        }
    })
    .await
}

/// Side effects on delivery: the 101 goes into the script-initiated network
/// log, and a terminal close retires the registry entry (the JS pump stops
/// after `close`, so delivery is the one reliable sweep point).
fn finish_event(gs: &mut super::ops::JsState, id: u32, ev: WsInbound) -> String {
    let json = ev.to_json();
    if matches!(ev, WsInbound::Open) {
        if let Some(sock) = gs.ws_registry.sockets.get(&id) {
            let mut headers = HashMap::new();
            headers.insert("upgrade".to_string(), "websocket".to_string());
            gs.js_network_events.push(super::ops::JsNetworkEvent {
                request_id: format!("ws-{id}"),
                url: sock.url.clone(),
                method: "GET".to_string(),
                status: 101,
                response_headers: headers,
                body_size: 0,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64(),
                error: None,
            });
        }
    }
    if matches!(ev, WsInbound::Close(..)) {
        gs.ws_registry.sockets.remove(&id);
    }
    json
}

#[op2(fast)]
pub(crate) fn op_ws_send(state: &OpState, id: u32, #[string] kind: &str, #[string] payload: &str) {
    let gs_rc = state.borrow::<SharedState>().clone();
    let gs = gs_rc.borrow();
    let Some(sock) = gs.ws_registry.sockets.get(&id) else {
        return;
    };
    let msg = if kind == "binary" {
        use base64::Engine as _;
        match base64::engine::general_purpose::STANDARD.decode(payload) {
            Ok(bytes) => WsOutbound::Binary(bytes),
            Err(_) => return,
        }
    } else {
        WsOutbound::Text(payload.to_string())
    };
    let _ = sock.outbound_tx.send(msg);
}

#[op2(fast)]
pub(crate) fn op_ws_close(state: &OpState, id: u32, code: u16, #[string] reason: &str) {
    let gs_rc = state.borrow::<SharedState>().clone();
    let gs = gs_rc.borrow();
    if let Some(sock) = gs.ws_registry.sockets.get(&id) {
        let _ = sock.outbound_tx.send(WsOutbound::Close(code, reason.to_string()));
    }
}
