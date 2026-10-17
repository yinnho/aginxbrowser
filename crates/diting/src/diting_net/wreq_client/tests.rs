    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use url::Url;

    use super::StealthHttpClient;
    use super::{fingerprint_family_version, ua_browser_version, warn_on_ua_tls_mismatch};
    use crate::diting_net::cookies::CookieJar;

    // #247: the resolver-level gate blocks post-DNS private targets and
    // honors the live flag flip — the whole point of the AtomicBool share.
    // localhost resolves via /etc/hosts to 127.0.0.1, the exact
    // rebinding shape the host-string check cannot see.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env guard spans the resolve, as everywhere else
    async fn wreq_ssrf_guard_resolver_blocks_loopback_resolution() {
        use wreq::dns::Resolve as _;

        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let guarded = super::WreqSsrfGuardResolver {
            allow_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        assert!(
            guarded
                .resolve(wreq::dns::Name::new("localhost".into()))
                .await
                .is_err(),
            "localhost must be rejected by the guarded resolver"
        );

        let permissive = super::WreqSsrfGuardResolver {
            allow_flag: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        assert!(
            permissive
                .resolve(wreq::dns::Name::new("localhost".into()))
                .await
                .is_ok(),
            "the live flag must pass localhost through"
        );
    }

    const CHROME152_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
    const FIREFOX133_UA: &str =
        "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0";
    const EDGE126_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";
    const SAFARI17_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4.1 Safari/605.1.15";

    #[test]
    fn ua_browser_version_parses_family_and_major() {
        assert_eq!(
            ua_browser_version(crate::diting_net::STEALTH_USER_AGENT),
            Some(("chrome", 154))
        );
        assert_eq!(ua_browser_version(CHROME152_UA), Some(("chrome", 152)));
        assert_eq!(ua_browser_version(EDGE126_UA), Some(("edge", 126)));
        assert_eq!(ua_browser_version(FIREFOX133_UA), Some(("firefox", 133)));
        assert_eq!(ua_browser_version(SAFARI17_UA), Some(("safari", 17)));
        assert_eq!(ua_browser_version("curl/8.0"), None);
    }

    #[test]
    fn fingerprint_names_parse_to_versions() {
        assert_eq!(fingerprint_family_version("chrome145"), Some(("chrome", 145)));
        assert_eq!(
            fingerprint_family_version("Chrome131"),
            Some(("chrome", 131))
        );
        assert_eq!(
            fingerprint_family_version("safari17_5"),
            Some(("safari", 17))
        );
        assert_eq!(
            fingerprint_family_version("firefox147"),
            Some(("firefox", 147))
        );
        assert_eq!(fingerprint_family_version("edge145"), Some(("edge", 145)));
        assert_eq!(fingerprint_family_version("chrome"), None);
        assert_eq!(fingerprint_family_version("dolphin9"), None);
    }

    #[test]
    fn ua_tls_mismatch_warns_on_version_drift() {
        // Report's exact case: AGINXBROWSER_UA=Chrome/152 over the default
        // Chrome145 handshake.
        assert!(warn_on_ua_tls_mismatch(CHROME152_UA, None));
        // Family drift warns too.
        assert!(warn_on_ua_tls_mismatch(FIREFOX133_UA, None));
        assert!(warn_on_ua_tls_mismatch(CHROME152_UA, Some("firefox147")));
        // Coherent pairs stay silent.
        assert!(!warn_on_ua_tls_mismatch(
            crate::diting_net::STEALTH_USER_AGENT,
            None
        ));
        assert!(!warn_on_ua_tls_mismatch(FIREFOX133_UA, Some("firefox133")));
        // Unrecognizable UA or fingerprint: no opinion, no warning.
        assert!(!warn_on_ua_tls_mismatch("curl/8.0", None));
        assert!(!warn_on_ua_tls_mismatch(CHROME152_UA, Some("chrome")));
    }

    const PLAIN_BODY: &str = "<!DOCTYPE html><html><body><p id=\"mark\">gzip ok</p></body></html>";

    // gzip (level 9) of PLAIN_BODY, hardcoded so the fixture needs no
    // compression dependency. A wrong byte fails the assert below.
    const GZIP_BODY: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0xb3, 0x51,
        0x74, 0xf1, 0x77, 0x0e, 0x89, 0x0c, 0x70, 0x55, 0xc8, 0x28, 0xc9, 0xcd,
        0xb1, 0xb3, 0x81, 0x90, 0x49, 0xf9, 0x29, 0x95, 0x76, 0x36, 0x05, 0x0a,
        0x99, 0x29, 0xb6, 0x4a, 0xb9, 0x89, 0x45, 0xd9, 0x4a, 0x76, 0xe9, 0x55,
        0x99, 0x05, 0x0a, 0xf9, 0xd9, 0x36, 0xfa, 0x05, 0x76, 0x36, 0xfa, 0x10,
        0x69, 0x7d, 0xb0, 0x5a, 0x00, 0x80, 0x3d, 0x1c, 0x5f, 0x41, 0x00, 0x00,
        0x00,
    ];

    /// Serve one `Content-Encoding: gzip` response on an ephemeral port.
    async fn gzip_fixture() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        GZIP_BODY.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(GZIP_BODY).await;
                });
            }
        });

        port
    }

    // The emulation profile advertises gzip, so origins compress. Without the
    // decoder the raw gzip bytes reach the HTML parser as document text.
    // The fixture is on loopback, so this runs under the env lock with
    // AGINXBROWSER_ALLOW_PRIVATE_NETWORK set, then restores it.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the env guard must span the fixture — that's the serialization
    async fn stealth_client_decodes_gzip_response() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let port = gzip_fixture().await;
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let result = client.fetch(&url).await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        let resp = result.unwrap();
        assert_eq!(resp.text(), PLAIN_BODY, "gzip body must be decompressed");
    }

    // The stealth path must enforce the same SSRF rules as the reqwest path.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the env guard must span the fetch — that's the serialization
    async fn stealth_fetch_rejects_loopback() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        assert!(client.fetch(&url).await.is_err(), "loopback must be rejected");
    }

    // obscura#995: the stealth transport consulted the tracker blocklist
    // unconditionally, welding tracker blocking onto stealth TLS. The field
    // defaults on (documented stealth behavior) and the env release must be
    // readable at construction time.
    #[test]
    fn stealth_client_block_trackers_defaults_on_env_releases() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_BLOCK_TRACKERS");
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        assert!(client.block_trackers, "stealth blocks trackers as documented");

        std::env::set_var("AGINXBROWSER_BLOCK_TRACKERS", "0");
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        std::env::remove_var("AGINXBROWSER_BLOCK_TRACKERS");
        assert!(!client.block_trackers, "env=0 unwelds tracker blocking");
    }

    // Live probe for the obscura#995 gate: with the default field on, a PGL
    // host never leaves the process (empty status-0 response, no request);
    // with the env release the same host gets a real response. Skipped in
    // CI — hits the network. Run explicitly:
    //   cargo test --features stealth probe_blocklist_gate_live -- --ignored
    #[tokio::test]
    #[ignore = "live network (obscura#995 probe); run explicitly with --ignored"]
    #[allow(clippy::await_holding_lock)] // the env guard must span both live halves — that's the serialization
    async fn probe_blocklist_gate_live() {
        // Same host for both halves. www.google-analytics.com is in the PGL
        // but 301s to www.google.com, which is unreachable from several
        // networks; googletagmanager.com answers directly (no redirect).
        let url = Url::parse("https://www.googletagmanager.com/gtm.js").unwrap();

        let blocked = StealthHttpClient::new(Arc::new(CookieJar::new()))
            .fetch(&url)
            .await
            .expect("a blocked tracker returns an empty 0 response, not an error");
        assert_eq!(blocked.status, 0, "default stealth must block PGL hosts");

        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_BLOCK_TRACKERS", "0");
        let released = StealthHttpClient::new(Arc::new(CookieJar::new()))
            .fetch(&url)
            .await
            .expect("released gate should reach the origin");
        std::env::remove_var("AGINXBROWSER_BLOCK_TRACKERS");
        assert_ne!(released.status, 0, "env=0 must let the request leave");
    }

    // obscura#793 same shape: the per-context allow-private flag must ride
    // the stealth client, not only the reqwest one — with the env var unset,
    // the flag alone decides.
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn stealth_fetch_honors_context_private_flag() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        let (port, _heads) = head_recording_fixture().await;
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client.allow_private_network
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let resp = client
            .fetch(&url)
            .await
            .expect("context flag must open the stealth path to loopback");
        assert_eq!(resp.status, 200);
    }

    // fetch_subresource carries the document as Referer under the same
    // strict-origin-when-cross-origin trim as the plain client's subresource
    // path — same-origin sends the full URL, cross-origin (a port difference
    // is an origin difference) only the origin — while a plain fetch stays
    // bare. The pump image fetch rides this: Referer-checking image CDNs
    // reject a bare request even with a perfect TLS fingerprint (反馈⑫).
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn stealth_subresource_carries_trimmed_referrer() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, heads) = head_recording_fixture().await;
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client.allow_private_network
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let same = Url::parse(&format!("http://127.0.0.1:{port}/same.png")).unwrap();
        let cross = Url::parse(&format!("http://127.0.0.1:{port}/cross.png")).unwrap();
        let bare = Url::parse(&format!("http://127.0.0.1:{port}/bare.png")).unwrap();
        let results = vec![
            client
                .fetch_subresource(&same, Some(&format!("http://127.0.0.1:{port}/doc.html#frag")))
                .await,
            client
                .fetch_subresource(&cross, Some("http://other.example:9/doc.html"))
                .await,
            client.fetch(&bare).await,
        ];
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        for r in results {
            assert_eq!(r.expect("fixture fetch").status, 200);
        }
        let heads = heads.lock().unwrap();
        let head_of = |path: &str| {
            heads
                .iter()
                .find(|h| h.contains(&format!("GET {path} ")))
                .unwrap_or_else(|| panic!("no request for {path}: {heads:?}"))
                .clone()
        };
        let same_head = head_of("/same.png");
        assert!(
            same_head
                .lines()
                .any(|l| l.eq_ignore_ascii_case(&format!("referer: http://127.0.0.1:{port}/doc.html"))),
            "same-origin subresource must carry the full document URL (fragment stripped): {same_head}"
        );
        let cross_head = head_of("/cross.png");
        assert!(
            cross_head
                .lines()
                .any(|l| l.eq_ignore_ascii_case("referer: http://other.example:9/")),
            "cross-origin subresource must trim to the origin: {cross_head}"
        );
        let bare_head = head_of("/bare.png");
        assert!(
            !bare_head.to_ascii_lowercase().contains("referer:"),
            "plain fetch must not invent a Referer: {bare_head}"
        );
    }

    /// Serve 200s on an ephemeral port, recording each request's raw head
    /// (request line + all header lines) into the shared vec.
    async fn head_recording_fixture() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let heads: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let heads2 = heads.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let h = heads2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let Ok(n) = stream.read(&mut buf).await else { return };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let head = req.split("\r\n\r\n").next().unwrap_or("").to_string();
                    h.lock().unwrap().push(head);
                    let body = "<!DOCTYPE html><html><body><p>ok</p></body></html>";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        (port, heads)
    }

    /// Read one full HTTP request (head until the blank line, then the
    /// declared Content-Length of body) off the stream, as "head\nbody".
    async fn read_request(
        stream: &mut tokio::net::TcpStream,
    ) -> Option<String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        for _ in 0..16 {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                let clen = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + clen {
                    return Some(format!(
                        "{head}\n{}",
                        String::from_utf8_lossy(&buf[pos + 4..])
                    ));
                }
            }
        }
        Some(String::from_utf8_lossy(&buf).to_string())
    }

    /// Recording server for method-semantics tests: every request (head +
    /// body) lands in the shared vec; `/r301` and `/r307` answer a redirect
    /// to `/land`, everything else a plain 200.
    async fn method_recording_fixture() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log2 = log.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let l = log2.clone();
                tokio::spawn(async move {
                    let Some(req) = read_request(&mut stream).await else { return };
                    l.lock().unwrap().push(req.clone());
                    let path = req.split(' ').nth(1).unwrap_or("").to_string();
                    let resp = match path.as_str() {
                        "/r301" => "HTTP/1.1 301 Moved Permanently\r\nlocation: /land\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string(),
                        "/setcookie" => "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nset-cookie: sid_from_response=1; Path=/\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_string(),
                        "/r307" => "HTTP/1.1 307 Temporary Redirect\r\nlocation: /land\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string(),
                        _ => "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_string(),
                    };
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        (port, log)
    }

    /// The legacy-TLS fallback's POST transport: method, body and declared
    /// Content-Type must all reach the wire (`fetch_with_body` used to be
    /// GET-only — the taobao seller-backend shape this exists for).
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn stealth_post_rides_method_body_and_content_type() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, log) = method_recording_fixture().await;
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client.allow_private_network
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/echo")).unwrap();
        let result = client
            .fetch_with_body(
                &url,
                None,
                "POST",
                Some(b"title=%E6%B5%8B%E8%AF%95".as_slice()),
                Some("application/x-www-form-urlencoded"),
                None,
                true,
            )
            .await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(result.expect("fixture POST").status, 200);
        let log = log.lock().unwrap();
        let req = log
            .iter()
            .find(|r| r.starts_with("POST /echo "))
            .unwrap_or_else(|| panic!("no POST /echo recorded: {log:?}"));
        assert!(
            req.lines()
                .any(|l| l.eq_ignore_ascii_case("content-type: application/x-www-form-urlencoded")),
            "declared content-type must ride the POST: {req}"
        );
        assert!(
            req.ends_with("title=%E6%B5%8B%E8%AF%95"),
            "body must ride the POST verbatim: {req}"
        );
    }

    /// Redirect method semantics on the stealth transport must mirror the
    /// plain client (and Chrome): 301 rewrites the POST to a bodyless GET,
    /// 307 preserves method + body.
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn stealth_post_redirect_downgrades_301_preserves_307() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, log) = method_recording_fixture().await;
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client.allow_private_network
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let mk = |path: &str| Url::parse(&format!("http://127.0.0.1:{port}/{path}")).unwrap();
        let r1 = client
            .fetch_with_body(&mk("r301"), None, "POST", Some(b"a=1".as_slice()), Some("application/x-www-form-urlencoded"), None, true)
            .await;
        let r2 = client
            .fetch_with_body(&mk("r307"), None, "POST", Some(b"a=1".as_slice()), Some("application/x-www-form-urlencoded"), None, true)
            .await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(r1.expect("301 walk").status, 200);
        assert_eq!(r2.expect("307 walk").status, 200);

        let log = log.lock().unwrap();
        let landed = log
            .iter()
            .filter(|r| r.contains(" /land "))
            .collect::<Vec<_>>();
        assert_eq!(landed.len(), 2, "both walks must land: {log:?}");
        let downgraded = landed
            .iter()
            .find(|r| r.starts_with("GET /land "))
            .unwrap_or_else(|| panic!("301 must land as GET: {log:?}"));
        assert!(
            !downgraded.to_ascii_lowercase().contains("content-type:"),
            "the 301 GET must not carry the POST's content-type: {downgraded}"
        );
        assert!(
            !downgraded.ends_with("a=1"),
            "the 301 GET must drop the body: {downgraded}"
        );
        let preserved = landed
            .iter()
            .find(|r| r.starts_with("POST /land "))
            .unwrap_or_else(|| panic!("307 must land as POST: {log:?}"));
        assert!(
            preserved.ends_with("a=1"),
            "the 307 POST must keep its body: {preserved}"
        );
    }

    /// The scripted escape hatch must send the same request, not a bare one:
    /// the rebuilt per-hop headers (Origin, Referer, Fetch-Metadata) reach
    /// the wire and win over the transport's defaults, while the credentials
    /// policy gates the cookie jar — a `credentials: 'omit'` POST rides the
    /// legacy transport without leaking the session cookie (the taobao
    /// seller-backend receipts proved the headered variant end to end).
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn scripted_request_headers_ride_and_credentials_gate_cookies() {
        use std::collections::HashMap;
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, log) = method_recording_fixture().await;
        let jar = Arc::new(CookieJar::new());
        let client = StealthHttpClient::new(jar.clone());
        client.allow_private_network
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mk = |path: &str| Url::parse(&format!("http://127.0.0.1:{port}/{path}")).unwrap();
        jar.set_cookie("sid=legacy", &mk("post"));

        let headers = HashMap::from([
            ("Origin".to_string(), "https://shop.example.com".to_string()),
            (
                "Referer".to_string(),
                "https://shop.example.com/sell/post.htm".to_string(),
            ),
            ("sec-fetch-site".to_string(), "same-origin".to_string()),
        ]);
        let include = client
            .fetch_with_body(
                &mk("post"),
                None,
                "POST",
                Some(b"a=1".as_slice()),
                Some("application/x-www-form-urlencoded"),
                Some(&headers),
                true,
            )
            .await;
        let omit = client
            .fetch_with_body(
                &mk("omit"),
                None,
                "POST",
                Some(b"a=1".as_slice()),
                None,
                Some(&headers),
                false,
            )
            .await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(include.expect("headered POST").status, 200);
        assert_eq!(omit.expect("omitted-credentials POST").status, 200);

        let log = log.lock().unwrap();
        let wire = |path: &str| {
            log.iter()
                .find(|r| r.starts_with(&format!("POST /{path} ")))
                .unwrap_or_else(|| panic!("no POST /{path} recorded: {log:?}"))
                .to_ascii_lowercase()
        };
        let included = wire("post");
        for want in [
            "origin: https://shop.example.com",
            "referer: https://shop.example.com/sell/post.htm",
            "sec-fetch-site: same-origin",
            "cookie: sid=legacy",
        ] {
            assert!(
                included.contains(want),
                "the retry must mirror the scripted request, missing {want:?}: {included}"
            );
        }
        let omitted = wire("omit");
        assert!(
            !omitted.contains("cookie:"),
            "credentials:'omit' must not leak the jar cookie: {omitted}"
        );
    }

    /// #254: the stealth transport's Fetch-Metadata vocabulary. The wreq
    /// emulation hardcodes document/navigate/none into every request, so a
    /// bare `fetch` looked like a typed navigation minus the two Chrome
    /// navigation markers, a subresource advertised a navigation, and a
    /// referrered navigation claimed `site: none` while carrying a Referer.
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn stealth_fetch_metadata_matches_resource_class() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, log) = method_recording_fixture().await;
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client.allow_private_network
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mk = |path: &str| Url::parse(&format!("http://127.0.0.1:{port}/{path}")).unwrap();
        let doc = format!("http://127.0.0.1:{port}/doc");

        client.fetch(&mk("bare")).await.expect("bare navigation");
        client
            .fetch_navigation(&mk("link"), Some(doc.as_str()))
            .await
            .expect("link-click navigation");
        client
            .fetch_subresource(&mk("img"), Some(doc.as_str()))
            .await
            .expect("image subresource");
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let log = log.lock().unwrap();
        let wire = |path: &str| {
            log.iter()
                .find(|r| r.starts_with(&format!("GET /{path} ")))
                .unwrap_or_else(|| panic!("no GET /{path} recorded: {log:?}"))
                .to_ascii_lowercase()
        };
        let bare = wire("bare");
        for want in [
            "sec-fetch-dest: document",
            "sec-fetch-mode: navigate",
            "sec-fetch-site: none",
            "sec-fetch-user: ?1",
            "upgrade-insecure-requests: 1",
        ] {
            assert!(
                bare.contains(want),
                "bare navigation must carry {want:?}: {bare}"
            );
        }
        let link = wire("link");
        for want in [
            "sec-fetch-site: same-origin",
            &format!("referer: {doc}"),
            "sec-fetch-user: ?1",
        ] {
            assert!(
                link.contains(want),
                "referrered navigation must carry {want:?}: {link}"
            );
        }
        let img = wire("img");
        for want in [
            "sec-fetch-dest: image",
            "sec-fetch-mode: no-cors",
            "sec-fetch-site: same-origin",
            "accept: image/avif",
        ] {
            assert!(
                img.contains(want),
                "image subresource must carry {want:?}: {img}"
            );
        }
        assert!(
            !img.contains("sec-fetch-user"),
            "subresources never carry sec-fetch-user: {img}"
        );
    }

    /// #234: the credentials policy must gate the response side too — an
    /// `credentials:'omit'` scripted fetch must not absorb the response's
    /// Set-Cookie into the jar. The plain walk gates this at its own
    /// set-cookie application; the stealth walk used to apply every
    /// set-cookie unconditionally (on every redirect hop).
    #[allow(clippy::await_holding_lock)] // env-lock guard spans the fixture fetch, as above
    #[tokio::test]
    async fn credentials_policy_gates_response_set_cookie_absorption() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, _log) = method_recording_fixture().await;
        let jar = Arc::new(CookieJar::new());
        let client = StealthHttpClient::new(jar.clone());
        client.allow_private_network
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mk = |path: &str| Url::parse(&format!("http://127.0.0.1:{port}/{path}")).unwrap();

        client
            .fetch_with_body(&mk("setcookie"), None, "POST", Some(b"a=1".as_slice()), None, None, false)
            .await
            .expect("omitted-credentials POST");
        let visible = jar.get_js_visible_cookies(&mk("land"));
        assert!(
            !visible.contains("sid_from_response"),
            "credentials:'omit' must not absorb the response Set-Cookie: {visible}"
        );

        client
            .fetch_with_body(&mk("setcookie"), None, "POST", Some(b"a=1".as_slice()), None, None, true)
            .await
            .expect("included-credentials POST");
        let visible = jar.get_js_visible_cookies(&mk("land"));
        assert!(
            visible.contains("sid_from_response=1"),
            "credentials:'include' must absorb the response Set-Cookie: {visible}"
        );
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    }

    // set_extra_headers must reach the wire per-request, and an extras
    // override must win over the client's default Accept-Language (the
    // suppression contract the per-hop merge loop implements).
    #[allow(clippy::await_holding_lock)] // env-lock guard held for the fixture fetch, as above
    #[tokio::test]
    async fn set_extra_headers_reach_the_wire() {
        let _guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let (port, heads) = head_recording_fixture().await;
        let client = StealthHttpClient::new(Arc::new(CookieJar::new()));
        client
            .set_extra_headers(
                [("x-diting-test".to_string(), "abc".to_string()), ("Accept-Language".to_string(), "ja".to_string())]
                    .into_iter()
                    .collect(),
            )
            .await;
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let result = client.fetch(&url).await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        let resp = result.expect("fixture fetch");
        assert_eq!(resp.status, 200);
        let heads = heads.lock().unwrap();
        let head = heads.first().expect("fixture saw the request").to_lowercase();
        assert!(
            head.contains("x-diting-test: abc"),
            "custom extra header must land on the wire, got:\n{head}"
        );
        assert!(
            head.contains("accept-language: ja"),
            "extras Accept-Language must override the default, got:\n{head}"
        );
    }
