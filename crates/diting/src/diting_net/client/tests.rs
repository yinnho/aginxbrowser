//! The client module's tests, split from the module root (ARCHITECTURE.md
//! P2 god-file ratchet).
use super::*;
use reqwest::dns::{Name, Resolve};

#[test]
fn derive_client_hints_chrome_version() {
    let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";
    let (ch_ua, platform) = derive_client_hints(ua);
    assert!(ch_ua.contains(r#""Chromium";v="145""#));
    assert!(ch_ua.contains(r#""Google Chrome";v="145""#));
    assert_eq!(platform, r#""macOS""#);
}

#[test]
fn derive_client_hints_windows() {
    let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
    let (ch_ua, platform) = derive_client_hints(ua);
    assert!(ch_ua.contains(r#""Chromium";v="131""#));
    assert_eq!(platform, r#""Windows""#);
}

#[test]
fn derive_client_hints_linux() {
    let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";
    let (_, platform) = derive_client_hints(ua);
    assert_eq!(platform, r#""Linux""#);
}

#[test]
fn derive_client_hints_android() {
    let ua = "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Mobile Safari/537.36";
    let (_, platform) = derive_client_hints(ua);
    assert_eq!(platform, r#""Android""#);
}

#[test]
fn derive_client_hints_defaults_when_no_version() {
    // No Chrome/ token → falls back to 145.
    let ua = "Mozilla/5.0 (Macintosh) Gecko Firefox/120.0";
    let (ch_ua, _) = derive_client_hints(ua);
    assert!(ch_ua.contains(r#""Chromium";v="145""#));
}

// Env- and flag-sensitive (1f7486c pattern): the lock serializes tests
// that touch the file-access switch; the drop-guard restores both halves
// even on panic so the off-by-default contract can't leak across tests.
use super::file_access_test::file_access_guard;

fn temp_file(name: &str, body: &[u8]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("agx-file-url-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn file_url_for(path: &std::path::Path) -> Url {
    Url::from_file_path(path).unwrap()
}

#[tokio::test]
async fn file_url_gate_is_off_by_default_and_names_the_switch() {
    let _guard = file_access_guard(false);
    let url = Url::parse("file:///definitely/not/here.html").unwrap();
    let NetError::Network(msg) = fetch_file_url(&url).await.unwrap_err() else {
        panic!("expected a Network error for a gated file:// read");
    };
    assert!(msg.contains("--allow-file-access"), "msg: {msg}");
    assert!(msg.contains("AGINXBROWSER_ALLOW_FILE_ACCESS"), "msg: {msg}");
}

#[tokio::test]
async fn file_url_read_with_gate_open_sets_content_type() {
    let _guard = file_access_guard(true);
    let path = temp_file("page.html", b"<html><title>local</title></html>");
    let resp = fetch_file_url(&file_url_for(&path)).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.headers.get("content-type").unwrap(), "text/html");
    assert_eq!(resp.body, b"<html><title>local</title></html>");

    let blob = temp_file("blob.bin", b"\x00\x01");
    let resp = fetch_file_url(&file_url_for(&blob)).await.unwrap();
    assert_eq!(resp.headers.get("content-type").unwrap(), "application/octet-stream");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&blob);
}

#[tokio::test]
async fn file_url_missing_file_is_a_read_error() {
    let _guard = file_access_guard(true);
    let url = Url::parse("file:///definitely/not/here.html").unwrap();
    let NetError::Network(msg) = fetch_file_url(&url).await.unwrap_err() else {
        panic!("expected a Network error for a missing file");
    };
    assert!(msg.contains("Failed to read file"), "msg: {msg}");
}

#[tokio::test]
async fn file_url_percent_decoding_survives_spaces_and_cjk() {
    let _guard = file_access_guard(true);
    let path = temp_file("a b 世界.html", b"<h1>ok</h1>");
    // Url::from_file_path percent-encodes; the loader must decode back.
    let url = file_url_for(&path);
    assert!(url.as_str().contains("%"), "expected encoding: {url}");
    let resp = fetch_file_url(&url).await.unwrap();
    assert_eq!(resp.body, b"<h1>ok</h1>");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn file_url_rejects_remote_hosts() {
    let _guard = file_access_guard(true);
    // A file URL with a non-local host must not fall through to a local
    // path read; to_file_path refuses it and so do we.
    let url = Url::parse("file://example.com/etc/passwd").unwrap();
    let NetError::Network(msg) = fetch_file_url(&url).await.unwrap_err() else {
        panic!("expected a Network error for a remote-host file URL");
    };
    assert!(msg.contains("Invalid file URL"), "msg: {msg}");
}

// Env-sensitive: AGINXBROWSER_ALLOW_PRIVATE_NETWORK overrides rejection, so this
// runs under the crate-wide env lock with the variable cleared.
#[tokio::test]
async fn validate_url_ssrf_rules() {
    let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

    for bad in [
        "http://127.0.0.1/",
        "http://127.1.2.3:8080/admin",
        "http://10.0.0.5/",
        "http://192.168.1.1/",
        "http://172.16.0.1/",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/",
        "http://localhost:3000/",
    ] {
        let url = Url::parse(bad).unwrap();
        assert!(validate_url(&url, false).is_err(), "{bad} must be rejected");
    }

    for good in ["http://example.com/", "https://example.com:8443/a?b=c", "file:///tmp/x.html"] {
        let url = Url::parse(good).unwrap();
        assert!(validate_url(&url, false).is_ok(), "{good} must be allowed");
    }
    let url = Url::parse("ftp://example.com/").unwrap();
    assert!(validate_url(&url, false).is_err(), "ftp must be rejected");
}

/// Drop-guard fixture for the scoped allow-network knob: clears
/// `AGINXBROWSER_ALLOW_NETWORK` and the CLI list even on panic, so the
/// deny-set-by-default contract can't leak across tests (1f7486c
/// pattern). Hold `PRIVATE_NET_ENV_LOCK` while asserting.
struct AllowNetworkGuard;
impl Drop for AllowNetworkGuard {
    fn drop(&mut self) {
        std::env::remove_var("AGINXBROWSER_ALLOW_NETWORK");
        set_allow_network(None);
    }
}

#[test]
fn parse_scoped_cidrs_bare_ip_and_bounds() {
    use std::str::FromStr;
    let list = parse_scoped_cidrs("10.20.0.0/16, 127.0.0.1, ::1/128, banana, 10.0.0.0/33");
    // "banana" and the /33 are skipped (fail closed), never widened.
    assert_eq!(list.len(), 3, "parsed: {list:?}");
    assert_eq!(list[0], ScopedCidr { net: IpAddr::from_str("10.20.0.0").unwrap(), prefix: 16 });
    assert_eq!(list[1], ScopedCidr { net: IpAddr::from_str("127.0.0.1").unwrap(), prefix: 32 });
    assert_eq!(list[2], ScopedCidr { net: IpAddr::from_str("::1").unwrap(), prefix: 128 });
}

#[test]
fn scoped_allow_network_opens_only_listed_cidrs() {
    use std::str::FromStr;
    let _lock = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    let _guard = AllowNetworkGuard;
    std::env::set_var(
        "AGINXBROWSER_ALLOW_NETWORK",
        "10.20.0.0/16, 127.0.0.1, ::1",
    );

    let f = |s: &str| is_forbidden_ip(IpAddr::from_str(s).unwrap());
    // Listed ranges open...
    assert!(!f("10.20.3.4"), "10.20.3.4 must pass the /16 entry");
    assert!(!f("127.0.0.1"), "bare-IP entry opens loopback");
    assert!(!f("::1"), "v6 entry opens ::1");
    // ...everything else stays shut, metadata endpoints included...
    assert!(f("10.21.0.1"), "outside the /16 stays forbidden");
    assert!(f("192.168.1.1"), "unlisted RFC1918 stays forbidden");
    assert!(f("169.254.169.254"), "cloud metadata stays closed");
    assert!(f("100.100.100.200"), "CGNAT metadata stays closed");
    // ...and the embedded-IPv4 recursions respect the list at the leaf.
    assert!(!f("::ffff:10.20.3.4"), "mapped allow-listed v4 passes");
    assert!(f("::ffff:169.254.169.254"), "mapped metadata stays closed");
    assert!(!f("2002:0a14:0304::"), "6to4-wrapped allow-listed v4 passes");
    assert!(f("2002:a9fe:a9fe::"), "6to4-wrapped metadata stays closed");
    assert!(!f("64:ff9b::a14:304"), "NAT64-wrapped allow-listed v4 passes");
    // Teredo is the deliberate exception: the three-slot check (server,
    // client, XOR-obfuscated client) needs every slot individually
    // allowed, and a 10.20.0.0/16 client obfuscates to a public
    // 245.235.x.y — so the whole form stays blocked. Fail closed.
    assert!(f("2001:0:a14:304::"), "Teredo stays closed even with the server slot allow-listed");
}

#[test]
fn validate_url_honors_scoped_allow_network() {
    let _lock = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    let _guard = AllowNetworkGuard;
    std::env::set_var("AGINXBROWSER_ALLOW_NETWORK", "10.20.0.0/16");

    let url = Url::parse("http://10.20.3.4:3000/admin").unwrap();
    assert!(
        validate_url(&url, false).is_ok(),
        "listed /16 must pass validate_url without allow-private-network"
    );
    let url = Url::parse("http://169.254.169.254/latest/meta-data").unwrap();
    assert!(
        validate_url(&url, false).is_err(),
        "metadata endpoint stays rejected under a scoped list"
    );
}

#[test]
fn cli_allow_network_flag_and_clear_restore_gate() {
    use std::str::FromStr;
    let _lock = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    let _guard = AllowNetworkGuard;
    std::env::remove_var("AGINXBROWSER_ALLOW_NETWORK");

    set_allow_network(Some("10.0.0.0/8"));
    assert!(!is_forbidden_ip(IpAddr::from_str("10.20.3.4").unwrap()));
    set_allow_network(None);
    assert!(is_forbidden_ip(IpAddr::from_str("10.20.3.4").unwrap()));
}

#[test]
fn is_forbidden_ip_covers_mapped_and_unspecified() {
    use std::str::FromStr;
    for bad in [
        "127.0.0.1",
        "10.1.2.3",
        "192.168.0.1",
        "172.16.5.4",
        "169.254.169.254",
        "0.0.0.0",
        "255.255.255.255",
        "192.0.2.1", // documentation
        "::1",
        "::",
        "fc00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1", // IPv4-mapped
    ] {
        assert!(
            is_forbidden_ip(IpAddr::from_str(bad).unwrap()),
            "{bad} must be forbidden"
        );
    }
    for good in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
        assert!(
            !is_forbidden_ip(IpAddr::from_str(good).unwrap()),
            "{good} must be allowed"
        );
    }
}

#[test]
fn is_forbidden_ip_covers_iana_special_purposes() {
    use std::str::FromStr;
    for bad in [
        "0.0.0.0", "0.1.2.3",                  // 0.0.0.0/8 "this network"
        "100.64.0.1", "100.100.100.200",       // 100.64.0.0/10 CGNAT (Alibaba metadata)
        "100.127.255.254",                     // CGNAT upper edge
        "192.0.0.192",                         // 192.0.0.0/24 (Oracle)
        "192.31.196.1", "192.52.193.1",        // AS112
        "192.88.99.1",                         // 6to4 relay
        "192.175.48.1",                        // Portmap
        "198.18.0.1", "198.19.255.254",        // 198.18.0.0/15 benchmarking
        "224.0.0.1", "239.1.1.1",              // multicast
        "240.0.0.1", "250.1.2.3",              // reserved/future
        "2001:db8::1",                         // documentation v6
        "100::1",                              // discard-only 100::/64
        "ff02::1", "ff0e::1",                  // ff00::/8
    ] {
        assert!(is_forbidden_ip(IpAddr::from_str(bad).unwrap()), "{bad} must be forbidden");
    }
    // CGNAT lower/upper neighbors stay reachable.
    for good in ["100.63.255.254", "100.128.0.1", "198.17.255.254", "198.20.0.1", "223.255.255.254"] {
        assert!(!is_forbidden_ip(IpAddr::from_str(good).unwrap()), "{good} must be allowed");
    }
}

#[test]
fn is_forbidden_ip_unwraps_embedded_ipv4_from_6to4_and_nat64() {
    use std::str::FromStr;
    for bad in [
        // 6to4 2002::/16 wrapping forbidden v4 targets.
        "2002:a9fe:a9fe::",   // 169.254.169.254 cloud metadata
        "2002:7f00:1::",      // 127.0.0.1 loopback
        "2002:a00:1::",       // 10.0.0.1 RFC1918
        "2002:6464:64c8::",   // 100.100.100.200 Alibaba metadata
        "2002:c0a8:101::",    // 192.168.1.1
        // NAT64 well-known prefix 64:ff9b::/96.
        "64:ff9b::7f00:1",    // 127.0.0.1
        "64:ff9b::a9fe:a9fe", // 169.254.169.254
        "64:ff9b:1::7f00:1",  // local-use 64:ff9b:1::/48 (RFC 8215) → 127.0.0.1
        // IPv4-compatible and mapped re-checked through the v4 deny-set.
        "::127.0.0.1", "::ffff:169.254.169.254",
    ] {
        assert!(is_forbidden_ip(IpAddr::from_str(bad).unwrap()), "{bad} must be forbidden");
    }
    // A 6to4/NAT64 wrapper around a PUBLIC v4 stays allowed — the guard
    // re-checks the embedded address, it does not blanket-ban the prefix.
    for good in ["2002:0808:0808::", /* 8.8.8.8 */ "64:ff9b::808:808"] {
        assert!(!is_forbidden_ip(IpAddr::from_str(good).unwrap()), "{good} must be allowed");
    }
}

#[test]
fn is_forbidden_ip_unwraps_teredo_embedded_ipv4() {
    use std::str::FromStr;
    for bad in [
        // Client slot XOR-obfuscated with 0xffff (RFC 4380 wire format):
        // 80fe:fefe ^ ffff:ffff = 7f01:0101 = 127.1.1.1.
        "2001:0:4136:e378:8000:63bf:80fe:fefe",
        // Server slot (bits 32-63) is raw attacker bytes too.
        "2001:0:a9fe:a9fe::", // 169.254.169.254 cloud metadata
        "2001:0:7f00:1::",    // 127.0.0.1
        "2001:0:6464:64c8::", // 100.100.100.200 Alibaba metadata
        // Client slot written raw (unobfuscated).
        "2001:0:cf2e:d242::7f00:1",
        // Client slot obfuscated metadata: 5601:5601 ^ ffff = a9fe:a9fe.
        "2001:0:cf2e:d242:eb00:1234:5601:5601",
    ] {
        assert!(is_forbidden_ip(IpAddr::from_str(bad).unwrap()), "{bad} must be forbidden");
    }
    // A Teredo wrapper with a public v4 in every slot stays allowed —
    // server 65.54.227.120 (real Teredo server), raw client 176.16.33.80,
    // de-obfuscated client 79.239.222.175.
    for good in ["2001:0:4136:e378:8000:63bf:b010:2150"] {
        assert!(!is_forbidden_ip(IpAddr::from_str(good).unwrap()), "{good} must be allowed");
    }
}

#[test]
fn validate_url_rejects_embedded_ipv6_literal_hosts() {
    let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

    for bad in [
        "http://[2002:7f00:1::]/",         // 6to4-wrapped loopback
        "http://[64:ff9b::7f00:1]/",       // NAT64-wrapped loopback
        "http://[2002:a9fe:a9fe::]/latest/meta-data", // 6to4-wrapped metadata
        "http://[2001:0:4136:e378:8000:63bf:80fe:fefe]/", // Teredo-obfuscated loopback
    ] {
        let url = Url::parse(bad).unwrap();
        assert!(validate_url(&url, false).is_err(), "{bad} must be rejected");
    }
}

#[tokio::test]
async fn ssrf_guard_resolver_blocks_loopback_resolution() {
    let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

    let guarded = SsrfGuardResolver::new(false);
    assert!(
        guarded.resolve(Name::from_str("localhost").unwrap()).await.is_err(),
        "localhost must be rejected by the guarded resolver"
    );

    let permissive = SsrfGuardResolver::new(true);
    assert!(
        permissive.resolve(Name::from_str("localhost").unwrap()).await.is_ok(),
        "allow_private must pass localhost through"
    );
}

use std::str::FromStr;

/// Serve a canned 200 response on a NON-loopback local address.
///
/// reqwest exempts loopback destinations from env-proxy matching (like
/// Chrome's implicit localhost bypass), so a 127.0.0.1 fixture is blind
/// to exactly the failure these tests pin — the first probe round here
/// reported "env ignored" purely because of that exemption. The LAN
/// address is discovered via a UDP connect (no packet leaves; it only
/// makes the routing table pick an interface).
async fn lan_http_origin(body: &'static str) -> Option<(Url, tokio::task::JoinHandle<()>)> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:80").ok()?;
    let ip = probe.local_addr().ok()?.ip();
    if ip.is_loopback() {
        return None;
    }
    let listener = tokio::net::TcpListener::bind((ip, 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    stream.read(&mut buf),
                )
                .await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    Some((Url::parse(&format!("http://{addr}/")).unwrap(), handle))
}

fn set_dead_standard_proxy_env() {
    for (k, v) in [
        ("HTTP_PROXY", "http://127.0.0.1:1"),
        ("HTTPS_PROXY", "http://127.0.0.1:1"),
        ("ALL_PROXY", "http://127.0.0.1:1"),
        ("http_proxy", "http://127.0.0.1:1"),
        ("https_proxy", "http://127.0.0.1:1"),
        ("all_proxy", "http://127.0.0.1:1"),
    ] {
        unsafe { std::env::set_var(k, v) };
    }
}

fn clear_standard_proxy_env() {
    for k in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        unsafe { std::env::remove_var(k) };
    }
}

/// obscura#491 class: standard proxy env vars must not silently route
/// engine traffic through a proxy the operator never configured in the
/// engine. If they do, a dead HTTP_PROXY takes down every public fetch
/// with an error that never mentions a proxy. Runs under the crate env
/// lock because the env mutation is process-global.
#[allow(clippy::await_holding_lock)] // env guard must span the fixture fetch — that's the serialization
#[tokio::test]
async fn standard_proxy_env_cannot_hijack_engine_clients() {
    let Some((url, origin)) = lan_http_origin("env-proxy-hijack").await else {
        eprintln!("skip: no non-loopback local address to serve on");
        return;
    };
    let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    set_dead_standard_proxy_env();

    // proxy_url None + private-network allowed (the LAN fixture address is
    // RFC1918): the operator asked for a direct fetch.
    let client = HttpClient::with_full_options(
        std::sync::Arc::new(CookieJar::new()),
        None,
        true,
    );
    let fetched = client.fetch(&url).await;

    clear_standard_proxy_env();
    origin.abort();

    match fetched {
        Ok(resp) => assert_eq!(resp.status, 200, "direct fetch must succeed"),
        Err(e) => panic!("standard proxy env hijacked a proxy-less client: {e:?}"),
    }
}

/// #664 lesson applied to the proxy path: when the configured upstream
/// proxy is unreachable, the error must name the proxy and its knob —
/// the reader should not have to go verbose-logging to learn a proxy is
/// involved at all (the original #491 debugging cost).
#[allow(clippy::await_holding_lock)] // env guard must span the fixture fetch — that's the serialization
#[tokio::test]
async fn dead_upstream_proxy_error_names_the_proxy() {
    let Some((url, origin)) = lan_http_origin("proxy-naming").await else {
        eprintln!("skip: no non-loopback local address to serve on");
        return;
    };
    let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    clear_standard_proxy_env();

    let client = HttpClient::with_options(
        std::sync::Arc::new(CookieJar::new()),
        Some("http://127.0.0.1:1"),
    );
    // The engine's own proxy gate: RFC1918 target needs the opt-in even
    // though the dial actually goes to the dead proxy.
    std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
    let fetched = client.fetch(&url).await;
    std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    origin.abort();

    let err = fetched.expect_err("dialing a dead proxy must fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("upstream proxy") && msg.contains("http://127.0.0.1:1"),
        "error must name the unreachable proxy, got: {msg}"
    );
    assert!(
        msg.contains("AGINXBROWSER_PROXY"),
        "error must name the knob, got: {msg}"
    );
}

/// Same LAN placement as [`lan_http_origin`], but the response body is
/// the received request head (one `name: value` line per header). Let's
/// the tests assert exactly what went out on the wire.
async fn header_echo_origin() -> Option<(Url, tokio::task::JoinHandle<()>)> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:80").ok()?;
    let ip = probe.local_addr().ok()?.ip();
    if ip.is_loopback() {
        return None;
    }
    let listener = tokio::net::TcpListener::bind((ip, 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = Vec::new();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    async {
                        let mut chunk = [0u8; 2048];
                        // Consume the body too, so small POSTs never hit a
                        // reset mid-write on the client side.
                        loop {
                            let n = stream.read(&mut chunk).await?;
                            buf.extend_from_slice(&chunk[..n]);
                            let head_end = buf
                                .windows(4)
                                .position(|w| w == b"\r\n\r\n")
                                .map(|p| p + 4);
                            if let Some(end) = head_end {
                                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                                let claimed: usize = head
                                    .lines()
                                    .find_map(|l| {
                                        l.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .and_then(|v| v.trim().parse().ok())
                                    })
                                    .unwrap_or(0);
                                if buf.len() >= end + claimed {
                                    break;
                                }
                            }
                            if buf.len() > 64 * 1024 {
                                break;
                            }
                        }
                        Ok::<(), std::io::Error>(())
                    },
                )
                .await;
                let head = String::from_utf8_lossy(&buf).to_string();
                let echo: Vec<String> = head
                    .lines()
                    .skip(1)
                    .take_while(|l| !l.is_empty())
                    .map(|l| l.to_ascii_lowercase())
                    .collect();
                let body = echo.join("\n");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    Some((Url::parse(&format!("http://{addr}/")).unwrap(), handle))
}

fn echoed<'a>(echo: &'a str, name: &str) -> Vec<&'a str> {
    echo.lines()
        .filter_map(|l| l.strip_prefix(&format!("{name}:")).map(|v| v.trim()))
        .collect()
}

fn referrer_client() -> HttpClient {
    HttpClient::with_full_options(std::sync::Arc::new(CookieJar::new()), None, true)
}

/// Page-subresource GETs carry the initiating document as Referer
/// (strict-origin-when-cross-origin: same-origin keeps the full URL
/// minus the fragment). Domain-whitelist service APIs reject bare
/// requests — this is the #268 wire contract.
#[tokio::test]
async fn subresource_get_carries_document_referer() {
    let Some((base, server)) = header_echo_origin().await else {
        eprintln!("skip: no non-loopback local address to serve on");
        return;
    };
    let doc = format!("{}doc/page.html?a=1#frag", base);
    let target = format!("{}img/pin.png", base);
    let fetched = referrer_client()
        .fetch_subresource(&Url::parse(&target).unwrap(), Some(&doc))
        .await;
    server.abort();

    let resp = fetched.expect("echo fetch must succeed");
    let echo = String::from_utf8_lossy(&resp.body).to_string();
    let want = doc.split('#').next().unwrap().to_string();
    let refs = echoed(&echo, "referer");
    assert_eq!(refs.len(), 1, "exactly one Referer, echo: {echo}");
    assert_eq!(refs[0], want, "fragment must be trimmed, echo: {echo}");
}

/// Non-GET subresource loads carry Origin alongside Referer — same-origin
/// POSTs included (b744b9b fetch()-op semantics).
#[tokio::test]
async fn post_form_carries_origin_and_referer() {
    let Some((base, server)) = header_echo_origin().await else {
        eprintln!("skip: no non-loopback local address to serve on");
        return;
    };
    let doc = format!("{}doc/form.html", base);
    let fetched = referrer_client()
        .post_form_with_callbacks(&base, "a=1", None, ResourceType::Fetch, Some(&doc))
        .await;
    server.abort();

    let resp = fetched.expect("echo post must succeed");
    let echo = String::from_utf8_lossy(&resp.body).to_string();
    let origins = echoed(&echo, "origin");
    assert_eq!(origins.len(), 1, "exactly one Origin, echo: {echo}");
    assert_eq!(origins[0], base.as_str().trim_end_matches('/'), "echo: {echo}");
    assert!(!echoed(&echo, "referer").is_empty(), "echo: {echo}");
}

/// setExtraHTTPHeaders-style overrides win over the computed Referer —
/// a tool that pins a header must not be silently re-prefixed.
#[tokio::test]
async fn extra_headers_override_computed_referer() {
    let Some((base, server)) = header_echo_origin().await else {
        eprintln!("skip: no non-loopback local address to serve on");
        return;
    };
    let client = referrer_client();
    client
        .set_extra_headers(HashMap::from([(
            "Referer".to_string(),
            "https://override.example/page".to_string(),
        )]))
        .await;
    let doc = format!("{}doc/page.html", base);
    let fetched = client
        .fetch_subresource(&base.clone(), Some(&doc))
        .await;
    server.abort();

    let resp = fetched.expect("echo fetch must succeed");
    let echo = String::from_utf8_lossy(&resp.body).to_string();
    let refs = echoed(&echo, "referer");
    assert_eq!(refs.len(), 1, "echo: {echo}");
    assert_eq!(refs[0], "https://override.example/page", "echo: {echo}");
}

/// Direct automation navs (tool-initiated fetch() with no referrer)
/// stay bare — edb1785 semantics preserved by the same code path.
#[tokio::test]
async fn untraced_fetch_stays_bare() {
    let Some((base, server)) = header_echo_origin().await else {
        eprintln!("skip: no non-loopback local address to serve on");
        return;
    };
    let fetched = referrer_client().fetch(&base.clone()).await;
    server.abort();

    let resp = fetched.expect("echo fetch must succeed");
    let echo = String::from_utf8_lossy(&resp.body).to_string();
    assert!(
        echoed(&echo, "referer").is_empty(),
        "tool fetch must not invent a Referer, echo: {echo}"
    );
}

/// 127.0.0.1:1 is a closed port: both transports fail fast with
/// connection refused, but the GET error must carry the legacy-attempt
/// marker proving the fallback actually fired.
#[cfg(feature = "stealth")]
#[tokio::test]
async fn legacy_tls_retries_get_transport_failures() {
    let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
    let url = Url::parse("http://127.0.0.1:1/dead").unwrap();
    let err = client.fetch(&url).await.expect_err("closed port must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("legacy TLS transport"),
        "GET must reach the legacy retry, got: {msg}"
    );
}

/// 127.0.0.1:1 is a closed port: connection refused is connect-stage by
/// definition, so a POST (with its body) rides the legacy retry just
/// like a GET — the request provably never left the machine.
#[cfg(feature = "stealth")]
#[tokio::test]
async fn legacy_tls_retries_connect_stage_post() {
    let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
    let url = Url::parse("http://127.0.0.1:1/submit").unwrap();
    let err = client
        .fetch_with_method(Method::POST, &url, Some(b"a=1".to_vec()))
        .await
        .expect_err("closed port must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("legacy TLS transport"),
        "connect-stage POST must reach the legacy retry, got: {msg}"
    );
}

/// A POST that failed AFTER the bytes went out must keep its original
/// error — that is the double-submit guard. The fixture accepts the
/// request, then closes mid-body (Content-Length promises more than it
/// delivers), which reqwest classifies as a body-read failure, not a
/// connect failure.
#[cfg(feature = "stealth")]
#[tokio::test]
async fn legacy_tls_keeps_post_error_after_the_request_left() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let _env = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                // Promise 100 bytes, deliver 3, close.
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\nabc",
                    )
                    .await;
            });
        }
    });

    let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
    let url = Url::parse(&format!("http://127.0.0.1:{port}/submit")).unwrap();
    let err = client
        .fetch_with_method(Method::POST, &url, Some(b"a=1".to_vec()))
        .await
        .expect_err("truncated body must fail");
    std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    let msg = err.to_string();
    assert!(
        !msg.contains("legacy TLS transport"),
        "post-send POST failure must not be retried (double-submit), got: {msg}"
    );
    assert!(
        msg.contains("Failed to read body"),
        "the original transport error must surface, got: {msg}"
    );
}

/// The op_fetch_url entry point takes a raw reqwest error string, not a
/// NetError — same closed-port probe, same markers, proving the helper
/// forwards into the guarded retry.
#[cfg(feature = "stealth")]
#[tokio::test]
async fn scripted_fetch_fallback_fires_for_get() {
    let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
    let url = Url::parse("http://127.0.0.1:1/dead").unwrap();
    let err = client
        .scripted_fetch_fallback(&Method::GET, &url, "error sending request", None, None, false, None, true)
        .await
        .expect_err("closed port must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("legacy TLS transport"),
        "scripted GET must reach the legacy retry, got: {msg}"
    );
}

/// A scripted POST flagged connect-stage rides the fallback with its
/// body — the caller's classification is trusted only for this hop.
#[cfg(feature = "stealth")]
#[tokio::test]
async fn scripted_fetch_fallback_retries_connect_stage_post() {
    let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
    let url = Url::parse("http://127.0.0.1:1/submit").unwrap();
    let err = client
        .scripted_fetch_fallback(
            &Method::POST,
            &url,
            "error sending request",
            Some(b"a=1".as_slice()),
            Some("application/x-www-form-urlencoded"),
            true,
            None,
            true,
        )
        .await
        .expect_err("closed port must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("legacy TLS transport"),
        "connect-stage POST must reach the legacy retry, got: {msg}"
    );
}

/// A scripted POST that already left the machine (post-send failure)
/// keeps its original error — the double-submit guard.
#[cfg(feature = "stealth")]
#[tokio::test]
async fn scripted_fetch_fallback_keeps_post_error_after_send() {
    let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
    let url = Url::parse("http://127.0.0.1:1/submit").unwrap();
    let err = client
        .scripted_fetch_fallback(
            &Method::POST,
            &url,
            "error reading a body from connection",
            Some(b"a=1".as_slice()),
            Some("application/x-www-form-urlencoded"),
            false,
            None,
            true,
        )
        .await
        .expect_err("must fail");
    let msg = err.to_string();
    assert!(
        !msg.contains("legacy TLS transport"),
        "scripted POST must not be retried, got: {msg}"
    );
    assert!(
        msg.contains("error reading a body from connection"),
        "guard skip must surface the original transport error, got: {msg}"
    );
}

/// A gate rejection travels as NetError::Network just like a transport
/// failure — the re-validation inside the fallback is what keeps SSRF
/// denials from ever reaching the legacy stack.
#[cfg(feature = "stealth")]
#[tokio::test]
async fn legacy_tls_never_retries_ssrf_gate_rejections() {
    let _env = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, false);
    let url = Url::parse("http://127.0.0.1:1/dead").unwrap();
    let err = client.fetch(&url).await.expect_err("gate must reject");
    let msg = err.to_string();
    assert!(
        !msg.contains("legacy TLS transport"),
        "SSRF denial must not fall through to the legacy stack, got: {msg}"
    );
    std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
}
