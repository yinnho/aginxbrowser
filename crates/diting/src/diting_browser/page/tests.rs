    use super::*;

    /// Holds PRIVATE_NET_ENV_LOCK for the test's duration and clears
    /// AGINXBROWSER_ALLOW_PRIVATE_NETWORK on exit so the ambient env never leaks
    /// into the next test (several diting_net tests assert on the unset
    /// state). The field is the point: holding the guard is what locks.
    #[allow(dead_code)]
    struct NetGuard(std::sync::MutexGuard<'static, ()>);
    impl Drop for NetGuard {
        fn drop(&mut self) {
            std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        }
    }
    fn net_test_guard() -> NetGuard {
        let guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        NetGuard(guard)
    }

    /// Same discipline for the nav-chain knob: the default-cap exhaustion
    /// test and the raised-cap test must not interleave their env writes —
    /// an unguarded "12" leaking into a concurrent navigate() would let the
    /// exhaustion test sail past the cap it exists to pin.
    #[allow(dead_code)] // the guard field is never read; holding it is the effect
    struct NavChainGuard(std::sync::MutexGuard<'static, ()>);
    impl Drop for NavChainGuard {
        fn drop(&mut self) {
            std::env::remove_var("AGINXBROWSER_NAV_CHAIN_LIMIT");
        }
    }
    static NAV_CHAIN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn nav_chain_guard() -> NavChainGuard {
        let guard = NAV_CHAIN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        NavChainGuard(guard)
    }

    /// #66 busy-limit knob: the freeze test shrinks the window to 2s and
    /// must restore the unset ambient state so no other test's pump
    /// accounts against a 2s window.
    #[allow(dead_code)] // holding the guard is the effect
    struct BusyLimitGuard(std::sync::MutexGuard<'static, ()>);
    impl Drop for BusyLimitGuard {
        fn drop(&mut self) {
            std::env::remove_var("AGINXBROWSER_JS_BUSY_LIMIT_SECS");
        }
    }
    static BUSY_LIMIT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn busy_limit_guard(secs: &str) -> BusyLimitGuard {
        let guard = BUSY_LIMIT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AGINXBROWSER_JS_BUSY_LIMIT_SECS", secs);
        BusyLimitGuard(guard)
    }

    /// Multi-path local HTTP server on 127.0.0.1. Bodies are owned Strings so
    /// a route can embed the port of another server (cross-origin tests).
    /// Answers up to 64 requests: one navigation may pull the document plus
    /// stylesheets and scripts. Unmatched paths get a 404.
    fn local_http_server(routes: Vec<(&'static str, u16, String)>) -> u16 {
        local_http_server_typed(
            routes
                .into_iter()
                .map(|(p, s, b)| (p, s, "text/html", b))
                .collect(),
        )
    }

    /// `local_http_server` with a per-route Content-Type (batch 2: response
    /// bodies store text lossy-UTF-8 vs binary base64, so tests need to
    /// control it).
    fn local_http_server_typed(routes: Vec<(&'static str, u16, &'static str, String)>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let (status, ctype, body) = routes
                    .iter()
                    .find(|(p, _, _, _)| *p == path)
                    .map(|(_, s, c, b)| (*s, *c, b.clone()))
                    .unwrap_or((404, "text/html", String::new()));
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    /// `local_http_server_typed` for bodies that are not valid UTF-8 (the
    /// GBK tests). Byte-exact on the wire.
    fn local_http_server_bytes(routes: Vec<(&'static str, u16, &'static str, Vec<u8>)>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let (status, ctype, body) = routes
                    .iter()
                    .find(|(p, _, _, _)| *p == path)
                    .map(|(_, s, c, b)| (*s, *c, b.clone()))
                    .unwrap_or((404, "text/html", Vec::new()));
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        port
    }

    fn test_page() -> Page {
        let context = Arc::new(BrowserContext::with_storage_and_network(
            "test".into(),
            None,
            false,
            None,
            None,
            true, // allow_private_network: tests talk to 127.0.0.1
            None,
        ));
        Page::new("page-test".into(), context)
    }

    // ---- pure functions -------------------------------------------------

    #[test]
    fn subresource_allowed_policy_matrix() {
        let http_page = Url::parse("http://example.com/page").ok();
        let file_page = Url::parse("file:///tmp/x.html").ok();
        assert!(subresource_allowed(http_page.as_ref(), "https://cdn.example.com/a.js"));
        assert!(subresource_allowed(http_page.as_ref(), "data:text/javascript,1"));
        assert!(!subresource_allowed(http_page.as_ref(), "file:///etc/passwd"));
        assert!(subresource_allowed(file_page.as_ref(), "file:///tmp/sibling.js"));
        assert!(!subresource_allowed(http_page.as_ref(), "javascript:alert(1)"));
        assert!(!subresource_allowed(http_page.as_ref(), "not a url"));
        // No page URL yet (pre-navigation): http(s) is still fine — there is
        // nothing origin-sensitive to protect yet — but file: stays blocked.
        assert!(subresource_allowed(None, "https://example.com/a.js"));
        assert!(!subresource_allowed(None, "file:///etc/passwd"));
    }

    #[test]
    fn cross_scheme_to_file_matrix() {
        assert!(cross_scheme_to_file("http://a.com/", "file:///etc/passwd"));
        assert!(cross_scheme_to_file("https://a.com/", "FILE:///etc/passwd"));
        assert!(!cross_scheme_to_file("file:///tmp/a", "file:///tmp/b"));
        assert!(!cross_scheme_to_file("http://a.com/", "https://b.com/"));
        // Unparseable source is treated as non-file: block.
        assert!(cross_scheme_to_file("::not-a-url::", "file:///etc/passwd"));
    }

    #[test]
    fn navigation_referrer_matrix() {
        let same = |a: &str, b: &str| {
            crate::diting_net::client::HttpClient::navigation_referrer(&Url::parse(a).unwrap(), &Url::parse(b).unwrap())
        };
        // Same-origin: full URL, fragment and credentials stripped.
        assert_eq!(
            same("http://example.com/a#frag", "http://example.com/b"),
            "http://example.com/a"
        );
        assert_eq!(
            same("http://user:pw@example.com/a", "http://example.com/b"),
            "http://example.com/a"
        );
        // Cross-origin: origin + '/' only.
        assert_eq!(
            same("http://example.com/a/b?c=1", "http://other.com/d"),
            "http://example.com/"
        );
        // Downgrade and non-HTTP schemes: nothing.
        assert_eq!(same("https://example.com/a", "http://example.com/b"), "");
        assert_eq!(same("file:///tmp/a", "http://example.com/b"), "");
    }

    #[test]
    fn decode_data_uri_variants() {
        assert_eq!(
            decode_data_uri("data:text/html,%3Cp%3Ehi%3C/p%3E"),
            Some(b"<p>hi</p>".to_vec())
        );
        assert_eq!(
            decode_data_uri("data:application/js;base64,d2luZG93LmE9MQ=="),
            Some(b"window.a=1".to_vec())
        );
        // Base64 with embedded whitespace is tolerated.
        assert_eq!(decode_data_uri("data:;base64,\n aGk="), Some(b"hi".to_vec()));
        assert_eq!(decode_data_uri("data:no-comma"), None);
        assert_eq!(decode_data_uri("http://example.com/"), None);
    }

    #[test]
    fn escape_for_js_template_literal_blocks_breakouts() {
        // Exact escaped forms: every breakout character becomes a backslash
        // escape, so no unescaped ` or ${ can terminate the literal early.
        assert_eq!(escape_for_js_template_literal("a`b${c}"), "a\\`b\\${c}");
        assert_eq!(escape_for_js_template_literal("x\u{2028}y\u{2029}"), "x\\u2028y\\u2029");
        // \n has no dedicated arm; it falls through the generic <0x20 branch.
        assert_eq!(escape_for_js_template_literal("\0\r\n"), "\\0\\r\\u000a");
        assert_eq!(escape_for_js_template_literal("plain"), "plain");
    }

    // ---- history --------------------------------------------------------

    #[test]
    fn push_history_dedupes_consecutive_and_truncates_forward() {
        let mut p = test_page();
        p.push_history("http://a/1".into());
        p.push_history("http://a/1".into()); // duplicate: ignored
        assert_eq!(p.history, vec!["http://a/1"]);
        p.push_history("http://a/2".into());
        assert_eq!(p.history_index, 1);
        p.set_history_index(0); // go back
        p.push_history("http://a/3".into()); // clobbers forward entry
        assert_eq!(p.history, vec!["http://a/1", "http://a/3"]);
        assert_eq!(p.history_index, 1);
        p.set_history_index(99); // out of bounds: no-op
        assert_eq!(p.history_index, 1);
    }

    // ---- no-network navigations ------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn about_blank_initializes_realm_and_runs_preload_scripts() {
        let mut p = test_page();
        p.set_preload_scripts(vec!["window.__pre = 'yes';".into()]);
        p.navigate("about:blank").await.unwrap();
        assert_eq!(p.lifecycle, LifecycleState::Loaded);
        assert_eq!(p.evaluate("window.__pre"), serde_json::json!("yes"));
        assert_eq!(p.url_string(), "about:blank");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn data_uri_document_executes_script_and_sets_title() {
        let mut p = test_page();
        p.navigate("data:text/html,%3Cscript%3Ewindow.__x%3D'ran'%3C/script%3E%3Ctitle%3ET%3C/title%3E")
            .await
            .unwrap();
        assert_eq!(p.evaluate("window.__x"), serde_json::json!("ran"));
        assert_eq!(p.title, "T");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn navigate_blank_resets_state() {
        let mut p = test_page();
        p.title = "stale".into();
        p.navigate_blank();
        assert_eq!(p.url_string(), "about:blank");
        assert_eq!(p.title, "");
        assert_eq!(p.lifecycle, LifecycleState::Loaded);
        assert!(p.js.is_none());
    }

    #[test]
    fn module_eval_budget_parses_override_and_default() {
        assert_eq!(super::module_eval_budget_from(None), 10_000);
        assert_eq!(super::module_eval_budget_from(Some("not-a-number")), 10_000);
        assert_eq!(super::module_eval_budget_from(Some("30000")), 30_000);
    }

    #[test]
    fn env_init_script_reads_file_and_skips_empty() {
        assert_eq!(super::env_init_script_from(None), None);
        assert_eq!(super::env_init_script_from(Some("/nonexistent/init.js")), None);
        let dir = std::env::temp_dir().join("aginxbrowser_init_script_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.js");
        std::fs::write(&path, "window.__probed = true;").unwrap();
        assert_eq!(
            super::env_init_script_from(Some(path.to_str().unwrap())).as_deref(),
            Some("window.__probed = true;")
        );
        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(super::env_init_script_from(Some(path.to_str().unwrap())), None);
        std::fs::remove_file(&path).ok();
    }

    // ---- navigation chains over a local server ---------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn js_triggered_navigation_chain_lands_on_final_page() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            ("/a", 200, "<html><script>location.href = '/b';</script></html>".into()),
            ("/b", 200, "<html><head><title>B-Landed</title></head><body><script>window.__here = document.URL;</script></body></html>".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.url_string(), format!("http://127.0.0.1:{port}/b"));
        assert_eq!(p.title, "B-Landed");
        assert!(p.evaluate("window.__here").as_str().unwrap().ends_with("/b"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chain_hop_sets_same_origin_referrer() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            ("/a", 200, "<html><script>location.href = '/b';</script></html>".into()),
            ("/b", 200, "<html><script>window.__ref = document.referrer;</script></html>".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(
            p.evaluate("window.__ref"),
            serde_json::json!(format!("http://127.0.0.1:{port}/a"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cross_origin_chain_referrer_is_origin_only() {
        let _g = net_test_guard();
        // Different ports on 127.0.0.1 are different origins.
        let port_b = local_http_server(vec![(
            "/b",
            200,
            "<html><script>window.__ref = document.referrer;</script></html>".into(),
        )]);
        let port_a = local_http_server(vec![(
            "/a",
            200,
            format!(
                "<html><script>location.href = 'http://127.0.0.1:{port_b}/b';</script></html>"
            ),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port_a}/a")).await.unwrap();
        assert_eq!(
            p.evaluate("window.__ref"),
            serde_json::json!(format!("http://127.0.0.1:{port_a}/"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_navigation_leaves_referrer_empty() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><script>window.__ref = document.referrer;</script></html>".into(),
        )]);
        let mut p = test_page();
        // An earlier navigation must not leak into the next one's referrer.
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__ref"), serde_json::json!(""));
    }

    // document.domain: the getter mirrors the origin's host (bilibili's
    // log-reporter derives its cookie scope from `document.domain.split(".")`
    // and died on undefined), and the legacy relaxation setter accepts the
    // same host but rejects foreign values.
    #[tokio::test(flavor = "current_thread")]
    async fn document_domain_getter_and_legacy_setter() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><script>window.__d = document.domain; document.domain = '127.0.0.1'; window.__relaxed = document.domain; try { document.domain = 'evil.com'; window.__bad = 'no-throw'; } catch (e) { window.__bad = e.name; }</script></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__d"), serde_json::json!("127.0.0.1"));
        assert_eq!(p.evaluate("window.__relaxed"), serde_json::json!("127.0.0.1"));
        assert_eq!(p.evaluate("window.__bad"), serde_json::json!("SecurityError"));
    }

    // `el.options = x` on a non-select element is an expando in Chrome (the
    // accessor only exists on HTMLSelectElement). The shared-prototype
    // getter made it a TypeError instead — bilibili's player binds component
    // state that way and its whole init died on it.
    #[tokio::test(flavor = "current_thread")]
    async fn element_options_assignment_is_expando_off_select() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><body><div id=d></div><select id=s><option>x</option></select><script>const d = document.getElementById('d'); d.options = {a: 1}; window.__d = d.options.a; window.__len = document.getElementById('s').options.length; try { document.getElementById('s').options = {a: 2}; } catch (e) { window.__sel = 'threw'; }</script></body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__d").as_f64().unwrap(), 1.0);
        // select.options stays the live collection, assignment is a silent no-op.
        assert_eq!(p.evaluate("window.__len").as_f64().unwrap(), 1.0);
        assert_eq!(p.evaluate("window.__sel === 'threw'"), serde_json::json!(false));
        assert_eq!(p.evaluate("document.getElementById('s').options.length").as_f64().unwrap(), 1.0);
    }

    // MSE surface: capability gate → attach handshake (createObjectURL →
    // video.src = blob:… → async sourceopen) → SourceBuffer append cycle.
    // bilibili's dash-only player leaves the player container empty
    // without `window.MediaSource && isTypeSupported(...)` succeeding.
    #[tokio::test(flavor = "current_thread")]
    async fn mse_stub_attach_handshake_and_append_cycle() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><body><script>window.__log = []; if (window.MediaSource && MediaSource.isTypeSupported('video/mp4; codecs=\"avc1.42E01E,mp4a.40.2\"')) { const ms = new MediaSource(); const url = URL.createObjectURL(ms); const v = document.createElement('video'); document.body.appendChild(v); v.src = url; ms.addEventListener('sourceopen', function() { const sb = ms.addSourceBuffer('video/mp4; codecs=\"avc1.42E01E\"'); window.__log.push('open:' + ms.readyState + ':' + ms.sourceBuffers.length); sb.addEventListener('updateend', function() { window.__log.push('appended:' + sb.updating); ms.endOfStream(); window.__log.push('eos:' + ms.readyState); }); sb.appendBuffer(new Uint8Array([1,2,3])); }); } else { window.__log.push('no-mse'); }</script></body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        // The handshake is async (sourceopen on a macrotask) and this page
        // has no pending network, so nothing will turn the isolate's event
        // loop after scripts finish — pump it the way wait_for_network_idle
        // does (50ms bounded slices).
        for _ in 0..5 {
            if let Some(js) = p.js.as_mut() {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    js.run_event_loop(),
                )
                .await;
            }
        }
        let log = p.evaluate("window.__log.join('|')").as_str().unwrap().to_string();
        assert_eq!(log, "open:open:1|appended:false|eos:ended");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mse_ladder_derives_buffered_and_fires_readiness_events() {
        let _g = net_test_guard();
        // Hand-built fMP4 mirroring bilibili's muxer: moov with mvex/trex
        // BEFORE trak (default durations ride the trex — the trun carries
        // sizes only), tkhd track 1, mdhd timescale 1000, trex default
        // duration 100; then two moofs (tfdt 0 and 1000, trun count 10
        // each) → ranges [0,1] then coalesced [0,2]. The ladder must fire
        // the Chrome readiness events on the element — bilibili's nano
        // player only mounts from onCanplay.
        let js = "window.__log = []; window.__events = []; \
function u32(n){return [n>>>24&255,n>>>16&255,n>>>8&255,n&255];} \
function box(t){let b=[];for(let i=1;i<arguments.length;i++)b=b.concat(arguments[i]);return [0,0,0,b.length+8].concat(t.split('').map(function(c){return c.charCodeAt(0);}),b);} \
function fb(){return [0,0,0,0];} \
const moov = box('moov', box('mvex', box('trex', fb(), u32(1), u32(0), u32(100), u32(0), u32(0))), box('trak', box('tkhd', fb(), u32(0), u32(0), u32(1), u32(0), u32(0)), box('mdia', box('mdhd', fb(), u32(0), u32(0), u32(1000), u32(0))))); \
const moof0 = box('moof', box('traf', box('tfhd', fb(), u32(1)), box('tfdt', fb(), u32(0)), box('trun', fb(), u32(10)))); \
const moof1 = box('moof', box('traf', box('tfhd', fb(), u32(1)), box('tfdt', fb(), u32(1000)), box('trun', fb(), u32(10)))); \
const ms = new MediaSource(); const url = URL.createObjectURL(ms); \
const v = document.createElement('video'); document.body.appendChild(v); \
['loadstart','durationchange','loadedmetadata','loadeddata','canplay','canplaythrough','progress'].forEach(function(t){v.addEventListener(t,function(){window.__events.push(t);});}); \
v.src = url; \
ms.addEventListener('sourceopen', function(){ \
  const sb = ms.addSourceBuffer('video/mp4'); \
  const segs = [moov, moof0, moof1]; let i = 0; \
  sb.addEventListener('updateend', function(){ \
    if (sb.buffered.length) window.__log.push('b' + i + ':' + sb.buffered.start(0).toFixed(3) + '-' + sb.buffered.end(0).toFixed(3)); \
    i++; \
    if (i < segs.length) { sb.appendBuffer(new Uint8Array(segs[i])); return; } \
    queueMicrotask(function(){ \
      ms.duration = 5; \
      window.__state = 'rs=' + v.readyState + '|nbuf=' + sb.buffered.length + '|' + sb.buffered.start(0).toFixed(3) + '-' + sb.buffered.end(0).toFixed(3) + '|dur=' + v.duration + '|msdur=' + ms.duration + '|net=' + v.networkState + '|HAVE=' + v.HAVE_ENOUGH_DATA + '|seek=' + v.seekable.length + ':' + v.seekable.end(0); \
      window.__ev = window.__events.join(','); \
    }); \
  }); \
  sb.appendBuffer(new Uint8Array(segs[0])); \
});";
        let port = local_http_server(vec![(
            "/a",
            200,
            format!("<html><body><script>{js}</script></body></html>"),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        for _ in 0..5 {
            if let Some(js) = p.js.as_mut() {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    js.run_event_loop(),
                )
                .await;
            }
        }
        let log = p.evaluate("window.__log.join('|')").as_str().unwrap().to_string();
        assert_eq!(log, "b1:0.000-1.000|b2:0.000-2.000", "buffered ranges from bytes, coalesced");
        let state = p
            .evaluate("window.__state || 'unset'")
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            state,
            "rs=4|nbuf=1|0.000-2.000|dur=5|msdur=5|net=1|HAVE=4|seek=1:5",
            "readiness/network/duration/seekable all derived"
        );
        let ev = p
            .evaluate("window.__ev || 'unset'")
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            ev,
            "loadstart,progress,durationchange,loadedmetadata,loadeddata,canplay,canplaythrough,progress,durationchange",
            "Chrome-shaped readiness ladder on the element"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sop_blocks_js_navigation_into_file_scheme() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><script>location.href = 'file:///etc/passwd';</script></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        // The chain loop breaks instead of navigating; we stay on /a.
        assert_eq!(p.url_string(), format!("http://127.0.0.1:{port}/a"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subresource_gate_blocks_file_script_src() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><body><script src=\"file:///tmp/evil.js\"></script><script>window.__pwned = 'inline-ran';</script></body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        // The inline script after the file: src still ran, but nothing could
        // have been loaded from file: — assert the page survived and no file
        // fetch shows up in the network log.
        assert_eq!(p.evaluate("window.__pwned"), serde_json::json!("inline-ran"));
        assert!(p.network_events.iter().all(|e| !e.url.starts_with("file:")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn js_chain_over_limit_reports_too_many_redirects() {
        let _g = net_test_guard();
        let _chain = nav_chain_guard();
        // Every /hopN page redirects to /hop{N+1}: an infinite JS chain that
        // must stop at the default chain limit (10 documents) with
        // TooManyClientNavigations — and the message must not blame HTTP
        // redirects (obscura#664: a message pointing at the wrong layer
        // costs more than none).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/hop0")
                    .to_string();
                let next = match path.strip_prefix("/hop").and_then(|n| n.parse::<usize>().ok()) {
                    Some(n) => format!("/hop{}", n + 1),
                    None => "/hop0".to_string(),
                };
                let body = format!("<html><script>location.href = '{next}';</script></html>");
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        let mut p = test_page();
        let err = p
            .navigate(&format!("http://127.0.0.1:{port}/hop0"))
            .await
            .unwrap_err();
        assert!(matches!(err, PageError::TooManyClientNavigations(10)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("client navigation chain"), "message must name the layer: {msg}");
        assert!(
            !msg.starts_with("Too many redirects"),
            "message must not blame HTTP redirects — the server never 3xx'd: {msg}"
        );
        assert!(
            msg.contains("AGINXBROWSER_NAV_CHAIN_LIMIT"),
            "message must name the operator remedy: {msg}"
        );
    }

    /// obscura#664 class: the navigation-chain cap counts documents, not
    /// HTTP redirects, and an operator with a legitimate long chain (SSO
    /// handover across several providers) must be able to raise it. Env
    /// knob, in the shape of `AGINXBROWSER_NAV_TIMEOUT_MS`.
    #[tokio::test(flavor = "current_thread")]
    async fn nav_chain_limit_env_unblocks_long_chains() {
        let _g = net_test_guard();
        let _chain = nav_chain_guard();
        // Finite chain /hop0 → /hop1 → … → /hop10 (terminal document):
        // 11 documents total, one past the default cap of 10.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/hop0")
                    .to_string();
                let body = match path.strip_prefix("/hop").and_then(|n| n.parse::<usize>().ok()) {
                    Some(10) => "<html><body>done</body></html>".to_string(),
                    Some(n) => {
                        format!("<html><script>location.href = '/hop{}';</script></html>", n + 1)
                    }
                    None => "/hop0".to_string(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        std::env::set_var("AGINXBROWSER_NAV_CHAIN_LIMIT", "12");
        let mut p = test_page();
        let res = p.navigate(&format!("http://127.0.0.1:{port}/hop0")).await;
        res.unwrap();
        assert_eq!(p.url_string(), format!("http://127.0.0.1:{port}/hop10"));
    }

    // ---- wait semantics & network events ---------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn subresources_recorded_as_network_events() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            ("/a", 200, "<html><head><link rel=\"stylesheet\" href=\"/s.css\"></head><body><script src=\"/x.js\"></script></body></html>".into()),
            ("/s.css", 200, "body{color:red}".into()),
            ("/x.js", 200, "window.__js = 'loaded';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__js"), serde_json::json!("loaded"));
        let kinds: Vec<&str> = p.network_events.iter().map(|e| e.resource_type.as_str()).collect();
        assert!(kinds.contains(&"Document"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"Stylesheet"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"Script"), "kinds: {kinds:?}");
        // request_id is page-id scoped.
        assert!(p
            .network_events
            .iter()
            .all(|e| e.request_id.starts_with("page-test.")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn domcontentloaded_wait_returns_after_scripts_execute() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><head><title>DCL</title></head><body><script>window.__ran = 'yes';</script></body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate_with_wait(
            &format!("http://127.0.0.1:{port}/a"),
            crate::diting_browser::lifecycle::WaitUntil::DomContentLoaded,
        )
        .await
        .unwrap();
        // DCL means DOM parsed AND scripts executed.
        assert_eq!(p.evaluate("window.__ran"), serde_json::json!("yes"));
        assert_eq!(p.lifecycle, LifecycleState::DomContentLoaded);
    }

    // ---- body onload forwarding (byte-WAF challenge prerequisite) ----------

    #[tokio::test(flavor = "current_thread")]
    async fn body_onload_attribute_handler_runs_on_window_load() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            // `<body onload="...">` is a Window-level handler per HTML spec;
            // the load event fires on window, so the content attribute must
            // be forwarded there or the handler never runs.
            "<html><head></head><body onload=\"window.__bodyOnloadRan = 'yes'\">x</body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__bodyOnloadRan"), serde_json::json!("yes"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn window_load_paths_all_fire() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><head></head><body><script>\
             window.__viaListener = 'no';\
             window.addEventListener('load', function() { window.__viaListener = 'yes'; });\
             window.onload = function() { window.__viaProperty = 'yes'; };\
             </script></body></html>"
                .into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__viaListener"), serde_json::json!("yes"));
        assert_eq!(p.evaluate("window.__viaProperty"), serde_json::json!("yes"));
    }

    /// (#37) The load path fires the window.onload property form exactly once
    /// (the old code called it directly AND the dispatch wrapper fired it
    /// again), hands it a real Event, and flips readyState to complete first
    /// like Chrome.
    #[tokio::test(flavor = "current_thread")]
    async fn window_onload_property_fires_once_with_event() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/a",
            200,
            "<html><head></head><body><script>\
             window.__n = 0;\
             window.onload = function(e) {\
                 window.__n++;\
                 window.__evt = (e && e.type) || 'none';\
                 window.__rs = document.readyState;\
             };\
             </script></body></html>"
                .into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(p.evaluate("window.__n").as_f64(), Some(1.0),
            "the property form fires exactly once, not direct-call + dispatch");
        assert_eq!(p.evaluate("window.__evt"), serde_json::json!("load"),
            "the handler receives a real load Event, not a bare argument-less call");
        assert_eq!(p.evaluate("window.__rs"), serde_json::json!("complete"),
            "readyState is already complete when load fires, like Chrome");
    }

    /// (#44) Script elements — parser-inserted and dynamically-inserted
    /// alike — delay the window load event until they execute. A load
    /// listener registered BY a late dynamic script (proxydetect visual
    /// suites hang their scheduler on `window.addEventListener("load", …)`
    /// from inside pd-lib.js) must catch the event; the old order fired load
    /// straight after the parser phase, before the settle loop let dynamic
    /// scripts land, so the listener missed the event and the suite never
    /// started.
    #[tokio::test(flavor = "current_thread")]
    async fn load_event_waits_for_dynamically_inserted_scripts() {
        let _g = net_test_guard();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..8 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let (ctype, body) = if path == "/slow.js" {
                    // Hold the response well past the parser phase so the
                    // script is provably still in flight when DCL fires.
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    (
                        "application/javascript",
                        "window.__order.push('slow-exec');\
                         window.addEventListener('load', function() { window.__order.push('late-load'); });"
                            .to_string(),
                    )
                } else {
                    (
                        "text/html",
                        "<html><head></head><body><script>\
                         window.__order = [];\
                         window.addEventListener('load', function() { window.__order.push('load'); });\
                         var s = document.createElement('script');\
                         s.src = 'http://127.0.0.1:PORT/slow.js';\
                         document.head.appendChild(s);\
                         window.__order.push('appended');\
                         </script></body></html>"
                            .replace("PORT", &port.to_string()),
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a")).await.unwrap();
        assert_eq!(
            p.evaluate("JSON.stringify(window.__order)"),
            serde_json::json!("[\"appended\",\"slow-exec\",\"load\",\"late-load\"]"),
            "load must fire after the dynamic script executes, and the \
             listener the script registered must catch the event"
        );
    }

    /// Byte-WAF JS challenge (juejin.cn class) auto-solves end to end:
    /// `<body onload="readygo()">` drives a setInterval PoW over the inline
    /// SHA-256 helpers, sets the `_wafchallengeid` answer cookie, and
    /// `location.reload()` re-requests with it fast enough to beat the
    /// Max-Age=1 window. The local server plays the WAF: no cookie ->
    /// challenge page, valid answer -> real page.
    #[tokio::test(flavor = "current_thread")]
    async fn byte_waf_js_challenge_autosolves() {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let _g = net_test_guard();

        let prefix: Vec<u8> = (0u8..32).collect();
        let secret = 7u64; // solution lives at i=7
        let expect: Vec<u8> = {
            let mut m = prefix.clone();
            m.extend_from_slice(secret.to_string().as_bytes());
            Sha256::digest(&m).to_vec()
        };
        let cs = serde_json::json!({
            "v": {
                "a": base64::engine::general_purpose::STANDARD.encode(&prefix),
                "b": 1787795373i64,
                "c": base64::engine::general_purpose::STANDARD.encode(&expect),
            },
            "s": base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
        })
        .to_string();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served_real = false;
        let challenge_hits = 0u32;
        let log = std::sync::Arc::new(std::sync::Mutex::new((served_real, challenge_hits)));
        let log2 = log.clone();
        let prefix2 = prefix.clone();
        let expect2 = expect.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut served_real, mut challenge_hits) = *log2.lock().unwrap();
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let cookie_hdr = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
                    .map(|l| l.to_string())
                    .unwrap_or_default();
                let passed = cookie_hdr
                    .split("_wafchallengeid=")
                    .nth(1)
                    .and_then(|v| v.split(';').next())
                    .map(str::trim)
                    .and_then(|v| {
                        base64::engine::general_purpose::STANDARD.decode(v).ok()
                    })
                    .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
                    .and_then(|c| {
                        c.get("d")
                            .and_then(|d| d.as_str())
                            .and_then(|d| {
                                base64::engine::general_purpose::STANDARD.decode(d).ok()
                            })
                            .map(|answer| {
                                let mut m = prefix2.clone();
                                m.extend_from_slice(&answer);
                                Sha256::digest(&m).to_vec() == expect2
                            })
                    })
                    .unwrap_or(false);
                let body = if passed {
                    served_real = true;
                    "<html><head><title>Real Page</title></head><body>unlocked</body></html>".to_string()
                } else {
                    challenge_hits += 1;
                    format!(
                        "<html><head><title>challenge</title></head><body onload=\"readygo()\">\
                         <script>window.WAFJS = function(){{}};</script>\
                         <script>{helpers}</script>\
                         <script>function readygo(){{var wci=\"_wafchallengeid\",cs=\"{cs}\",c=JSON.parse(atob(cs)),\
                         prefix=b64tou8a(c.v.a),expect=b64tohex(c.v.c),i=0,\
                         iid=setInterval(function(){{expect===s256(prefix,\"\"+i)&&\
                         (c.d=btoa(\"\"+i),clearInterval(iid),\
                         document.cookie=wci+\"=\"+btoa(JSON.stringify(c))+\"; Max-Age=1\",\
                         window.location.reload()),i++,i>1e6&&clearInterval(iid)}},1)}}</script>\
                         Please wait...</body></html>",
                        helpers = include_str!("../../../tests/waf_sha256_helpers.js"),
                        cs = base64::engine::general_purpose::STANDARD.encode(cs.as_bytes()),
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
                *log2.lock().unwrap() = (served_real, challenge_hits);
            }
        });

        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/")).await.unwrap();
        // The challenge page must have auto-passed: readygo ran from the body
        // onload attribute, solved the PoW, and reloaded with the answer.
        assert_eq!(p.evaluate("document.title"), serde_json::json!("Real Page"));
        let (served_real, challenge_hits) = *log.lock().unwrap();
        assert!(served_real, "server never served the real page");
        assert!(challenge_hits >= 1, "challenge was never issued");
    }

    
    // ---- suspend / resume ---------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn suspend_resume_preserves_dom_and_rebuilds_realm() {
        let mut p = test_page();
        p.navigate("data:text/html,%3Cscript%3Ewindow.__mark%3D'old'%3C/script%3E%3Ctitle%3ECarry%3C/title%3E")
            .await
            .unwrap();
        assert_eq!(p.evaluate("window.__mark"), serde_json::json!("old"));
        p.suspend_js();
        assert!(!p.has_js());
        // DOM survives suspension and stays queryable.
        let title = p
            .with_dom(|dom| {
                dom.query_selector("title")
                    .ok()
                    .flatten()
                    .map(|nid| dom.text_content(nid))
            })
            .flatten();
        assert_eq!(title.as_deref(), Some("Carry"));
        // Static evaluate fallback with no runtime.
        assert_eq!(p.evaluate("document.title"), serde_json::json!("Carry"));
        assert_eq!(
            p.evaluate("window.location.href"),
            serde_json::json!(p.url_string())
        );
        assert_eq!(p.evaluate("1 + 1"), serde_json::Value::Null);
        // Resume rebuilds the realm: page state (window.__mark) is gone —
        // init_js never carries the old realm across.
        p.resume_js();
        assert!(p.has_js());
        assert_eq!(p.evaluate("window.__mark"), serde_json::Value::Null);
        assert_eq!(p.evaluate("document.title"), serde_json::json!("Carry"));
    }

    // ---- sessionStorage persistence (#678) ---------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn session_storage_survives_same_origin_navigation() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            ("/a", 200, "<html><title>A</title></html>".into()),
            ("/b", 200, "<html><title>B</title></html>".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/a"))
            .await
            .unwrap();
        p.evaluate("sessionStorage.setItem('k', 'v1')");
        assert_eq!(
            p.evaluate("sessionStorage.getItem('k')"),
            serde_json::json!("v1")
        );
        // Same-origin navigation must preserve sessionStorage (reload-like).
        p.navigate(&format!("http://127.0.0.1:{port}/b"))
            .await
            .unwrap();
        assert_eq!(
            p.evaluate("sessionStorage.getItem('k')"),
            serde_json::json!("v1")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_storage_survives_suspend_resume() {
        let mut p = test_page();
        p.navigate("data:text/html,<html><title>S</title></html>")
            .await
            .unwrap();
        p.evaluate("sessionStorage.setItem('foo', 'bar')");
        assert_eq!(
            p.evaluate("sessionStorage.getItem('foo')"),
            serde_json::json!("bar")
        );
        // A second target's evaluate parks this realm (suspend_js) and later
        // resumes it via init_js; sessionStorage must survive the round-trip.
        p.suspend_js();
        p.resume_js();
        assert_eq!(
            p.evaluate("sessionStorage.getItem('foo')"),
            serde_json::json!("bar")
        );
    }

    // Console calls logged before a suspension must survive it (obscura#971
    // same hole): the realm drops, but the client reading the console ring
    // after a resume still expects everything it hasn't drained yet.
    #[tokio::test(flavor = "current_thread")]
    async fn console_calls_survive_suspend_resume() {
        let mut p = test_page();
        p.navigate("data:text/html,<html><title>C</title></html>")
            .await
            .unwrap();
        p.evaluate("console.error('pre-suspend')");
        p.suspend_js();
        // The realm is gone here — pre-fix, a take at this point returned
        // empty and the message was destroyed with it.
        p.resume_js();
        p.evaluate("console.error('post-resume')");
        let calls = p.take_pending_console_calls();
        let msgs: Vec<&str> = calls.iter().map(|(_, m, _)| m.as_str()).collect();
        assert_eq!(
            msgs,
            vec!["pre-suspend", "post-resume"],
            "suspension buffer merges ahead of the live realm's calls"
        );
        assert!(
            p.take_pending_console_calls().is_empty(),
            "a take drains both sources"
        );
    }

    // Console calls logged by the outgoing document must survive the realm
    // swap in init_js/navigate_blank the same way suspend_js preserves them —
    // Chrome keeps per-tab console history across navigations.
    #[tokio::test(flavor = "current_thread")]
    async fn console_calls_survive_navigation() {
        let mut p = test_page();
        p.navigate("data:text/html,<html><title>one</title></html>")
            .await
            .unwrap();
        p.evaluate("console.error('pre-nav')");
        // Real navigation path (init_js realm rebuild), not suspend/resume.
        p.navigate("data:text/html,<html><title>two</title></html>")
            .await
            .unwrap();
        let calls = p.take_pending_console_calls();
        let msgs: Vec<&str> = calls.iter().map(|(_, m, _)| m.as_str()).collect();
        assert_eq!(msgs, vec!["pre-nav"], "outgoing document's calls survive");
        assert!(
            p.take_pending_console_calls().is_empty(),
            "navigation buffer drains with the take"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_storage_survives_data_url_navigation() {
        let mut p = test_page();
        p.navigate("data:text/html,<html><title>one</title></html>")
            .await
            .unwrap();
        p.evaluate("sessionStorage.setItem('ticket', 'abc-123')");
        assert_eq!(
            p.evaluate("sessionStorage.getItem('ticket')"),
            serde_json::json!("abc-123")
        );
        p.navigate("data:text/html,<html><title>two</title></html>")
            .await
            .unwrap();
        assert_eq!(
            p.evaluate("sessionStorage.getItem('ticket')"),
            serde_json::json!("abc-123"),
            "data: URL same-origin (opaque) navigation must preserve sessionStorage"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_storage_cleared_on_cross_origin_navigation() {
        let _g = net_test_guard();
        let port_a = local_http_server(vec![("/a", 200, "<html><title>A</title></html>".into())]);
        let port_b = local_http_server(vec![("/b", 200, "<html><title>B</title></html>".into())]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port_a}/a"))
            .await
            .unwrap();
        p.evaluate("sessionStorage.setItem('k', 'v1')");
        // Different port = different origin: the old store must be discarded,
        // matching per-tab-per-origin semantics.
        p.navigate(&format!("http://127.0.0.1:{port_b}/b"))
            .await
            .unwrap();
        assert_eq!(
            p.evaluate("sessionStorage.getItem('k')"),
            serde_json::Value::Null
        );
    }

    // ---- batch 1: fork_virtual_url + preload push + nav timeout -----------

    #[tokio::test(flavor = "current_thread")]
    async fn process_pending_navigation_adopts_spa_pushstate_route() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/app",
            200,
            "<html><body><script>window.__booted = 1;</script></body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app")).await.unwrap();
        // SPA click handler: renders in place, moves the URL via pushState.
        // (evaluate wraps single expressions only — `return (a; b)` would be
        // a SyntaxError — so the call and the probe run separately.)
        p.evaluate("history.pushState(null, '', '/app/settings')");
        assert_eq!(
            p.evaluate("globalThis.__virtualUrl"),
            serde_json::json!(format!("http://127.0.0.1:{port}/app/settings"))
        );
        // The session pump has no pending navigation to process, but the page
        // still routed itself — that counts (fork_virtual_url.rs).
        assert!(p.process_pending_navigation().await.unwrap());
        assert!(p.url_string().ends_with("/app/settings"));
        assert!(p.history.last().unwrap().ends_with("/app/settings"));
        // Idempotent: adopting the same virtual URL again changes nothing.
        assert!(!p.process_pending_navigation().await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn process_pending_navigation_without_route_returns_false() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/plain",
            200,
            "<html><body>no scripts</body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/plain")).await.unwrap();
        assert!(!p.process_pending_navigation().await.unwrap());
        assert!(p.url_string().ends_with("/plain"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn process_pending_navigation_carries_hop1_referrer() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            (
                "/sender",
                200,
                "<html><body>plain document</body></html>".into(),
            ),
            (
                "/receiver",
                200,
                "<html><script>window.__ref = document.referrer;</script></html>".into(),
            ),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/sender")).await.unwrap();
        assert!(p.url_string().ends_with("/sender"));
        // Queue a navigation AFTER navigate() returned, the way a click
        // handler on a live page does — the inner chain is done, so only the
        // session pump's process_pending_navigation can pick it up.
        p.evaluate("setTimeout(() => { location.href = '/receiver'; }, 100)");
        let _ = p.settle_until_idle(3_000).await;
        assert!(p.process_pending_navigation().await.unwrap());
        assert!(p.url_string().ends_with("/receiver"));
        // The page asked for this navigation itself, so unlike direct
        // automation navigations the first hop carries a referrer.
        assert_eq!(
            p.evaluate("window.__ref"),
            serde_json::json!(format!("http://127.0.0.1:{port}/sender"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn add_preload_script_appends_in_order() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/target",
            200,
            "<html><script>window.__pageRan = (window.__order || '') + 'P';</script></html>".into(),
        )]);
        let mut p = test_page();
        p.add_preload_script(
            "window.__order = (window.__order || '') + '1';".into(),
        );
        p.add_preload_script(
            "window.__order = (window.__order || '') + '2';".into(),
        );
        p.navigate(&format!("http://127.0.0.1:{port}/target")).await.unwrap();
        // Push semantics: both scripts ran, in registration order, before the
        // page's own script.
        assert_eq!(p.evaluate("window.__pageRan"), serde_json::json!("12P"));
    }

    #[test]
    fn navigation_timeout_field_overrides_env_default() {
        let mut p = test_page();
        let env_or_default = std::env::var("AGINXBROWSER_NAV_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30_000);
        assert_eq!(p.navigation_timeout().as_millis() as u64, env_or_default);
        p.set_navigation_timeout(Some(1_500));
        assert_eq!(p.navigation_timeout().as_millis() as u64, 1_500);
        p.set_navigation_timeout(None);
        assert_eq!(p.navigation_timeout().as_millis() as u64, env_or_default);
    }

    // ---- batch 2: network callbacks + response bodies ----------------------

    /// Shared recorder for callback tests: on_request/on_response push into
    /// these from inside the registry's fire path.
    #[derive(Default)]
    struct NetLog {
        requests: std::sync::Mutex<Vec<(String, String)>>, // (resource_type, url)
        responses: std::sync::Mutex<Vec<(String, String, usize)>>, // (type, url, body len)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn on_request_and_on_response_fire_for_document_and_subresources() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            (
                "/page",
                200,
                format!(
                    "<html><head><link rel=stylesheet href='/s.css'></head>\
                     <script src='/j.js'></script></html>"
                ),
            ),
            ("/s.css", 200, "body{color:red}".into()),
            ("/j.js", 200, "window.__j = 1;".into()),
        ]);
        let log = Arc::new(NetLog::default());
        let mut p = test_page();
        {
            let log = log.clone();
            p.on_request(Arc::new(move |info| {
                log.requests
                    .lock()
                    .unwrap()
                    .push((info.resource_type.as_str().to_string(), info.url.to_string()));
            }));
        }
        {
            let log = log.clone();
            p.on_response(Arc::new(move |info, resp| {
                log.responses.lock().unwrap().push((
                    info.resource_type.as_str().to_string(),
                    info.url.to_string(),
                    resp.body.len(),
                ));
            }));
        }
        p.navigate(&format!("http://127.0.0.1:{port}/page")).await.unwrap();

        let reqs = log.requests.lock().unwrap();
        let kinds: Vec<&str> = reqs.iter().map(|(k, _)| k.as_str()).collect();
        assert!(kinds.contains(&"Document"), "requests: {reqs:?}");
        assert!(kinds.contains(&"Script"), "requests: {reqs:?}");
        assert!(kinds.contains(&"Stylesheet"), "requests: {reqs:?}");
        // The document observer saw the fully-built header set, not an empty
        // one: our client always sends User-Agent.
        let resps = log.responses.lock().unwrap();
        assert!(
            resps
                .iter()
                .any(|(k, _, len)| k == "Document" && *len > 0),
            "responses: {resps:?}"
        );
        assert!(
            resps.iter().any(|(k, _, _)| k == "Stylesheet"),
            "responses: {resps:?}"
        );
    }

    /// External stylesheets fetched at navigation must reach three places:
    /// the cascade (getComputedStyle), document.styleSheets' rule lists, and
    /// the `__diting_css` global. Before the ext_sheets plumbing the fetch
    /// succeeded but only fed the unused global — computed reads came back
    /// as initial values and cssRules was an empty stub.
    #[tokio::test(flavor = "current_thread")]
    async fn external_stylesheet_applies_and_lists_rules() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            (
                "/page",
                200,
                "<html><head><link rel=\"stylesheet\" href=\"/s.css\"></head>\
                 <body><div class=\"box\">x</div></body></html>"
                    .into(),
            ),
            (
                "/s.css",
                200,
                ".box{position:absolute;left:10px;background-color:#ff0000}".into(),
            ),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/page")).await.unwrap();

        // styleSheets lists the loaded sheet with engine-parsed rules.
        assert_eq!(p.evaluate("document.styleSheets.length").as_f64(), Some(1.0));
        assert_eq!(
            p.evaluate("document.styleSheets[0].href"),
            serde_json::json!(format!("http://127.0.0.1:{port}/s.css"))
        );
        let rules = p
            .evaluate("Array.from(document.styleSheets[0].cssRules, r => r.selectorText)")
            .as_array()
            .expect("cssRules array")
            .clone();
        assert_eq!(rules, vec![serde_json::json!(".box")]);
        // The declaration block round-trips through the same parser the
        // cascade uses, so both readers quote the sheet identically.
        let decls = p
            .evaluate(
                "document.styleSheets[0].cssRules[0].style.getPropertyValue('background-color')",
            )
            .as_str()
            .expect("declaration string")
            .to_string();
        assert_eq!(decls, "#ff0000");

        // The cascade sees the external rules too, not just inline <style>.
        #[cfg(feature = "screenshot")]
        {
            let pos = p
                .evaluate("getComputedStyle(document.querySelector('.box')).position")
                .as_str()
                .expect("position string")
                .to_string();
            assert_eq!(pos, "absolute");
            let bg = p
                .evaluate("getComputedStyle(document.querySelector('.box')).backgroundColor")
                .as_str()
                .expect("color string")
                .to_string();
            assert!(bg.starts_with("rgb(255"), "backgroundColor: {bg}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn viewport_override_drives_media_queries_and_survives_navigation() {
        let _g = net_test_guard();
        let page_html = |body: &'static str| {
            format!(
                "<html><head><style>\
                 @media (max-width:600px) {{ body {{ background-color:#010203 }} }}\
                 </style></head><body>{body}</body></html>"
            )
        };
        let port = local_http_server(vec![
            ("/a", 200, page_html("x")),
            ("/b", 200, page_html("y")),
        ]);
        let mut p = test_page();
        p.set_viewport_override(375.0, 667.0, true, Some(2.625));
        p.navigate(&format!("http://127.0.0.1:{port}/a"))
            .await
            .unwrap();
        // The persona dpr, sampled with no override pinned.
        p.clear_viewport_override();
        let persona_dpr = p.evaluate("devicePixelRatio").as_f64().unwrap();
        p.set_viewport_override(375.0, 667.0, true, Some(2.625));

        // Scripts see the emulated window — inner* and the pinned dpr (CDP
        // deviceScaleFactor semantics: what scripts report, not the persona's
        // panel).
        assert_eq!(p.evaluate("innerWidth").as_f64(), Some(375.0));
        assert_eq!(p.evaluate("innerHeight").as_f64(), Some(667.0));
        assert_eq!(p.evaluate("devicePixelRatio").as_f64(), Some(2.625));
        assert_eq!(
            p.evaluate("matchMedia('(max-width:600px)').matches"),
            serde_json::json!(true)
        );
        // Mobile flips the pointer/hover persona too — those move together
        // or the viewport contradicts maxTouchPoints.
        assert_eq!(
            p.evaluate("matchMedia('(pointer:coarse)').matches"),
            serde_json::json!(true)
        );
        assert_eq!(
            p.evaluate("matchMedia('(hover:none)').matches"),
            serde_json::json!(true)
        );
        assert_eq!(p.evaluate("navigator.maxTouchPoints").as_f64(), Some(5.0));

        // The layout ICB moved with it: the mobile-only rule applied in
        // the cascade (a desktop viewport leaves the background initial).
        let bg = p
            .evaluate("getComputedStyle(document.body).backgroundColor")
            .as_str()
            .expect("color string")
            .to_string();
        assert!(bg.starts_with("rgb(1, 2, 3"), "mobile media rule applied, got {bg}");

        // Navigation rebuilds the realm and republishes the persona
        // viewport; the override has to be replayed on top or the page
        // silently flips back mid-session.
        p.navigate(&format!("http://127.0.0.1:{port}/b"))
            .await
            .unwrap();
        assert_eq!(p.evaluate("innerWidth").as_f64(), Some(375.0));
        assert_eq!(p.evaluate("devicePixelRatio").as_f64(), Some(2.625));
        assert_eq!(
            p.evaluate("matchMedia('(pointer:coarse)').matches"),
            serde_json::json!(true)
        );

        // Clearing returns the persona viewport, desktop answers, and the
        // persona dpr.
        p.clear_viewport_override();
        let w = p.evaluate("innerWidth").as_f64().unwrap();
        assert!(w > 600.0, "persona viewport restored, got {w}");
        assert_eq!(p.evaluate("devicePixelRatio").as_f64(), Some(persona_dpr));
        assert_eq!(
            p.evaluate("matchMedia('(pointer:coarse)').matches"),
            serde_json::json!(false)
        );
        assert_eq!(p.evaluate("navigator.maxTouchPoints").as_f64(), Some(0.0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn off_request_detaches_observer() {
        let _g = net_test_guard();
        let port = local_http_server(vec![("/x", 200, "<html></html>".into())]);
        let log = Arc::new(NetLog::default());
        let mut p = test_page();
        let id = {
            let log = log.clone();
            p.on_request(Arc::new(move |info| {
                log.requests
                    .lock()
                    .unwrap()
                    .push((info.resource_type.as_str().to_string(), info.url.to_string()));
            }))
        };
        assert!(p.off_request(id));
        // Double detach is a visible no-op.
        assert!(!p.off_request(id));
        p.navigate(&format!("http://127.0.0.1:{port}/x")).await.unwrap();
        assert!(log.requests.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn document_body_retrievable_and_take_removes() {
        let _g = net_test_guard();
        let port = local_http_server(vec![(
            "/doc",
            200,
            "<html><head><title>BodyStore</title></head><body>MARKER-42</body></html>".into(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/doc")).await.unwrap();
        let doc_event = p
            .network_events
            .iter()
            .find(|e| e.resource_type == "Document")
            .expect("document network event");
        let rid = doc_event.request_id.clone();
        let stored = p.get_response_body(&rid).expect("stored document body");
        assert!(!stored.base64_encoded);
        assert!(stored.body.contains("MARKER-42"), "{}", stored.body);
        // take_response_body_raw hands over the bytes and drops the entry.
        let raw = p.take_response_body_raw(&rid).expect("raw bytes");
        assert!(String::from_utf8_lossy(&raw).contains("MARKER-42"));
        assert!(p.get_response_body(&rid).is_none());
        assert!(p.take_response_body_raw(&rid).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn alias_response_body_renames_entry() {
        let _g = net_test_guard();
        let port = local_http_server(vec![("/d", 200, "<html>ALIAS-ME</html>".into())]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/d")).await.unwrap();
        let rid = p
            .network_events
            .iter()
            .find(|e| e.resource_type == "Document")
            .unwrap()
            .request_id
            .clone();
        // Chrome's requestId === loaderId convention (upstream #340).
        p.alias_response_body(&rid, "loader-1");
        assert!(p.get_response_body("loader-1").is_some());
        assert!(p.get_response_body(&rid).is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn binary_body_stored_base64_and_take_is_byte_exact() {
        let _g = net_test_guard();
        // Non-text Content-Type forces base64 storage; the expectation is the
        // exact wire bytes (a Rust String holds UTF-8, so chars >= 0x80 are
        // sent as their multi-byte encodings — take must return those, lossless).
        let bin: String = vec![0u8, 159, 146, 150, 255, 1, 2]
            .into_iter()
            .map(|b| b as char)
            .collect();
        let wire_bytes = bin.as_bytes().to_vec();
        let port = local_http_server_typed(vec![
            ("/bin", 200, "application/octet-stream", bin),
            (
                "/host",
                200,
                "text/html",
                "<html><script src='/bin'></script></html>".into(),
            ),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/host")).await.unwrap();
        let rid = p
            .network_events
            .iter()
            .find(|e| e.url.ends_with("/bin"))
            .expect("script event for binary body")
            .request_id
            .clone();
        let stored = p.get_response_body(&rid).expect("binary body stored");
        assert!(stored.base64_encoded, "octet-stream must store base64");
        let raw = p.take_response_body_raw(&rid).expect("raw bytes");
        assert_eq!(raw, wire_bytes);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn declared_gbk_document_stores_decoded_text() {
        let _g = net_test_guard();
        // Chrome 152 raw CDP returns a declared-GBK page as decoded text with
        // base64Encoded=false (verified 2026-09-02, obscura #791) — the same
        // policy now governs the store, so getResponseBody hands back 中文.
        let gbk: Vec<u8> = vec![0xD6, 0xD0, 0xCE, 0xC4, b'-', b't', b'a', b'i', b'l'];
        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "content-type".to_string(),
            "text/html; charset=gbk".to_string(),
        );
        let mut p = test_page();
        let rid = p.record_network_event_with_body(
            "http://127.0.0.1:1/gbk",
            "GET",
            "Document",
            200,
            &headers,
            &gbk,
        );
        let stored = p.get_response_body(&rid).expect("gbk body stored");
        assert!(!stored.base64_encoded, "declared GBK must decode to text");
        assert_eq!(stored.body, "中文-tail");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn undecodable_text_bodies_still_store_base64() {
        let _g = net_test_guard();
        // Opaque bytes under a text-ish MIME (Chrome 152 verified for JSON)
        // and invalid bytes under a declared UTF-8 label both travel base64.
        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "content-type".to_string(),
            "application/json".to_string(),
        );
        let mut p = test_page();
        let rid = p.record_network_event_with_body(
            "http://127.0.0.1:1/badjson",
            "GET",
            "XHR",
            200,
            &headers,
            &[0xFF],
        );
        let stored = p.get_response_body(&rid).expect("json body stored");
        assert!(stored.base64_encoded, "undecodable JSON must store base64");
        assert_eq!(p.take_response_body_raw(&rid).unwrap(), vec![0xFF]);

        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "content-type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        );
        let rid = p.record_network_event_with_body(
            "http://127.0.0.1:1/badtxt",
            "GET",
            "Document",
            200,
            &headers,
            &[b'a', 0xFF, b'b'],
        );
        let stored = p.get_response_body(&rid).expect("txt body stored");
        assert!(stored.base64_encoded, "invalid declared-UTF-8 must store base64");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn charsetless_text_stores_decoded_1252_text() {
        let _g = net_test_guard();
        // Chrome 152 verified: text/plain without a charset — and no
        // Content-Type at all — still return text (windows-1252 is total).
        let bytes: Vec<u8> = vec![0x81, 0x8D];
        let mut headers = std::collections::HashMap::new();
        headers.insert("content-type".to_string(), "text/plain".to_string());
        let mut p = test_page();
        let rid = p.record_network_event_with_body(
            "http://127.0.0.1:1/nocharset",
            "GET",
            "Document",
            200,
            &headers,
            &bytes,
        );
        let stored = p.get_response_body(&rid).expect("plain body stored");
        assert!(!stored.base64_encoded);
        assert_eq!(stored.body, "\u{0081}\u{008D}");

        let rid = p.record_network_event_with_body(
            "http://127.0.0.1:1/noct",
            "GET",
            "Document",
            200,
            &std::collections::HashMap::new(),
            &bytes,
        );
        let stored = p.get_response_body(&rid).expect("noct body stored");
        assert!(!stored.base64_encoded);
        assert_eq!(stored.body, "\u{0081}\u{008D}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn js_fetch_declared_gbk_stores_decoded_text() {
        let _g = net_test_guard();
        let gbk: Vec<u8> = vec![0xD6, 0xD0, 0xCE, 0xC4, b'-', b't', b'a', b'i', b'l'];
        let host = "<html><script>\
                     fetch('/api').then(r => r.text()).then(t => { window.__got = t; });\
                     </script></html>"
            .as_bytes()
            .to_vec();
        let port = local_http_server_bytes(vec![
            ("/app", 200, "text/html", host),
            ("/api", 200, "text/html; charset=gbk", gbk.clone()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app")).await.unwrap();
        assert!(p.evaluate("window.__got").is_string());
        p.sync_js_network_events();
        let ev = p
            .network_events
            .iter()
            .find(|e| e.url.ends_with("/api"))
            .expect("fetch network event after sync");
        let rid = ev.request_id.clone();
        let stored = p.get_response_body(&rid).expect("fetch body stored");
        assert!(!stored.base64_encoded, "declared GBK fetch must decode to text");
        assert_eq!(stored.body, "中文-tail");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn js_fetch_surfaces_as_network_event_with_body() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            (
                "/app",
                200,
                "<html><script>\
                 fetch('/api').then(r => r.text()).then(t => { window.__got = t; });\
                 </script></html>"
                    .into(),
            ),
            ("/api", 200, "{\"v\": 7}".into()),
        ]);
        let log = Arc::new(NetLog::default());
        let mut p = test_page();
        {
            let log = log.clone();
            p.on_response(Arc::new(move |info, resp| {
                log.responses.lock().unwrap().push((
                    info.resource_type.as_str().to_string(),
                    info.url.to_string(),
                    resp.body.len(),
                ));
            }));
        }
        p.navigate(&format!("http://127.0.0.1:{port}/app")).await.unwrap();
        assert_eq!(p.evaluate("window.__got"), serde_json::json!("{\"v\": 7}"));

        // Script-initiated traffic fires the page's observers too…
        let resps = log.responses.lock().unwrap();
        assert!(
            resps.iter().any(|(k, u, _)| k == "Fetch" && u.ends_with("/api")),
            "responses: {resps:?}"
        );
        drop(resps);

        // …and syncs into the page's network events with a fetch-{N} id whose
        // body resolves through the JS-side store.
        p.sync_js_network_events();
        let ev = p
            .network_events
            .iter()
            .find(|e| e.url.ends_with("/api"))
            .expect("fetch network event after sync");
        assert_eq!(ev.resource_type, "Fetch");
        assert!(ev.request_id.starts_with("fetch-"), "{}", ev.request_id);
        let stored = p.get_response_body(&ev.request_id).expect("fetch body");
        assert!(!stored.base64_encoded);
        assert_eq!(stored.body, "{\"v\": 7}");
        // Idempotent drain.
        p.sync_js_network_events();
        assert_eq!(
            p.network_events.iter().filter(|e| e.url.ends_with("/api")).count(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_response_bodies_drops_page_and_js_stores() {
        let _g = net_test_guard();
        let port = local_http_server(vec![
            (
                "/app",
                200,
                "<html><script>fetch('/a1').then(r => r.text());</script></html>".into(),
            ),
            ("/a1", 200, "one".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app")).await.unwrap();
        let doc_rid = p
            .network_events
            .iter()
            .find(|e| e.resource_type == "Document")
            .unwrap()
            .request_id
            .clone();
        assert!(p.get_response_body(&doc_rid).is_some());
        p.sync_js_network_events();
        let fetch_rid = p
            .network_events
            .iter()
            .find(|e| e.url.ends_with("/a1"))
            .unwrap()
            .request_id
            .clone();
        assert!(p.get_response_body(&fetch_rid).is_some());

        p.clear_response_bodies();
        assert!(p.get_response_body(&doc_rid).is_none());
        assert!(p.get_response_body(&fetch_rid).is_none());
    }

    // ---- import maps (upstream 34373c3) ----------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn parser_import_map_before_first_module_controls_resolution() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![
            ("/app/index.html", 200, "text/html",
             r#"<html><head>
                <script type="importmap">{"imports":{"ordered":"./before.js"}}</script>
                <script type="module">
                    import { value } from "ordered";
                    globalThis.__parser_import_map_value = value;
                </script>
                <script type="importmap">{"imports":{"ordered":"./after.js"}}</script>
            </head><body></body></html>"#.into()),
            ("/app/before.js", 200, "application/javascript", "export const value = 'before-first-module';".into()),
            ("/app/after.js", 200, "application/javascript", "export const value = 'later-map';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app/index.html")).await.unwrap();
        assert_eq!(
            p.evaluate("globalThis.__parser_import_map_value"),
            serde_json::json!("before-first-module")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn later_import_map_adds_unrelated_rule_without_rebinding_resolved_rule() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![
            ("/app/index.html", 200, "text/html",
             r#"<html><head>
                <script type="importmap">{"imports":{"fixed":"./before.js"}}</script>
                <script type="module">
                    import { value } from "fixed";
                    globalThis.__first_map_value = value;
                </script>
                <script type="importmap">{"imports":{"fixed":"./after.js","later":"./later.js"}}</script>
                <script type="module">
                    import { value as fixed } from "fixed";
                    import { value as later } from "later";
                    globalThis.__later_map_values = [fixed, later];
                </script>
            </head><body></body></html>"#.into()),
            ("/app/before.js", 200, "application/javascript", "export const value = 'before-first-module';".into()),
            ("/app/later.js", 200, "application/javascript", "export const value = 'later-map';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app/index.html")).await.unwrap();
        assert_eq!(
            p.evaluate("globalThis.__first_map_value"),
            serde_json::json!("before-first-module")
        );
        assert_eq!(
            p.evaluate("globalThis.__later_map_values"),
            serde_json::json!(["before-first-module", "later-map"])
        );
        // after.js must never have been fetched: the second map's rebind of
        // "fixed" was discarded because the first module already resolved it.
        let urls: Vec<&str> = p.network_events.iter().map(|e| e.url.as_str()).collect();
        assert!(!urls.iter().any(|u| u.ends_with("/after.js")), "urls: {urls:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dynamically_inserted_import_map_controls_later_dynamic_import() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![
            ("/app/index.html", 200, "text/html",
             r#"<html><head></head><body>
                <script>
                    const map = document.createElement("script");
                    map.type = "importmap";
                    map.textContent = JSON.stringify({imports:{dynamicName:"./later.js"}});
                    document.head.appendChild(map);
                    import("dynamicName")
                        .then(module => globalThis.__dynamic_map_value = module.value)
                        .catch(error => globalThis.__dynamic_map_value = error.message);
                </script>
            </body></html>"#.into()),
            ("/app/later.js", 200, "application/javascript", "export const value = 'later-map';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app/index.html")).await.unwrap();
        assert_eq!(
            p.evaluate("globalThis.__dynamic_map_value"),
            serde_json::json!("later-map")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn later_base_element_does_not_rebase_an_earlier_import_map() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![
            ("/app/index.html", 200, "text/html",
             r#"<html><head>
                <script type="importmap">{"imports":{"fixed":"./before.js"}}</script>
                <base href="/assets/">
                <script type="module">
                    import { value } from "fixed";
                    globalThis.__temporal_base_value = value;
                </script>
            </head><body></body></html>"#.into()),
            ("/assets/before.js", 200, "application/javascript", "export const value = 'wrong-base';".into()),
            ("/app/before.js", 200, "application/javascript", "export const value = 'before-first-module';".into()),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/app/index.html")).await.unwrap();
        assert_eq!(
            p.evaluate("globalThis.__temporal_base_value"),
            serde_json::json!("before-first-module")
        );
    }

    // ---- script-phase overrun (weixin DCL hang; v0.3.1 report P1-3) -------

    /// Env-knob guard discipline (obscura#853 family) for the script-deadline
    /// knob: a leaked small deadline would truncate a concurrently running
    /// navigation's script phase too.
    #[allow(dead_code)] // the guard field is never read; holding it is the effect
    struct ScriptDeadlineGuard(std::sync::MutexGuard<'static, ()>);
    impl Drop for ScriptDeadlineGuard {
        fn drop(&mut self) {
            std::env::remove_var("AGINXBROWSER_SCRIPT_DEADLINE_MS");
        }
    }
    static SCRIPT_DEADLINE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn script_deadline_guard(ms: &str) -> ScriptDeadlineGuard {
        let guard = SCRIPT_DEADLINE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AGINXBROWSER_SCRIPT_DEADLINE_MS", ms);
        ScriptDeadlineGuard(guard)
    }

    /// Same discipline for the module-eval budget knob.
    #[allow(dead_code)] // the guard field is never read; holding it is the effect
    struct ModuleEvalGuard(std::sync::MutexGuard<'static, ()>);
    impl Drop for ModuleEvalGuard {
        fn drop(&mut self) {
            std::env::remove_var("AGINXBROWSER_MODULE_EVAL_TIMEOUT_MS");
        }
    }
    static MODULE_EVAL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn module_eval_guard(ms: &str) -> ModuleEvalGuard {
        let guard = MODULE_EVAL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AGINXBROWSER_MODULE_EVAL_TIMEOUT_MS", ms);
        ModuleEvalGuard(guard)
    }

    /// Regression (weixin article pages; v0.3.1 Windows report P1-3): when
    /// the classic script phase overran its deadline, the exec watchdog's
    /// terminate_execution stayed pending past its phase — the module phase
    /// no-opped and the `<load-events>` script failed silently, so
    /// readyState was wedged in "loading" forever and DOMContentLoaded
    /// never fired. The phase boundary now disarms (and thereby heals) the
    /// isolate before the load lifecycle runs. Chrome semantics: a killed
    /// script ends the script, not the document's load lifecycle.
    #[tokio::test(flavor = "current_thread")]
    async fn script_phase_overrun_still_fires_load_lifecycle() {
        let _net = net_test_guard();
        let _deadline = script_deadline_guard("1500");
        // Pad the spinner past the 10 KB threshold so the 5s per-script
        // guard applies — the phase watchdog (deadline + 1s) terminates it
        // well before that, which is exactly the overrun shape the bug had.
        let pad = " ".repeat(10_100);
        let spinner = format!("var pad = '{pad}'; while (true) {{}}");
        let port = local_http_server_typed(vec![(
            "/hang.html",
            200,
            "text/html",
            format!(
                r#"<html><head>
                    <script>window.__dcl__ = false;
                        document.addEventListener('DOMContentLoaded', function() {{ window.__dcl__ = true; }});</script>
                    <script>{spinner}</script>
                    <script>window.__after_spin__ = true;</script>
                </head><body>body text</body></html>"#
            ),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/hang.html")).await.unwrap();
        assert_eq!(
            p.evaluate("document.readyState"),
            serde_json::json!("complete"),
            "an overrun classic phase must not wedge the load lifecycle"
        );
        assert_eq!(p.evaluate("window.__dcl__"), serde_json::json!(true));
        // The script after the spinner is skipped by design — the deadline
        // bounds the classic phase. What must survive is the lifecycle.
        assert_eq!(p.evaluate("window.__after_spin__"), serde_json::Value::Null);
    }

    /// The per-script 5s guard's other half: a script killed by ITS OWN
    /// watchdog (not the phase deadline's) used to leave V8's termination
    /// flag pending, so every later script on the page died instantly —
    /// guarded ones logged "killed after 5s" for spins they never ran,
    /// unguarded ones errored, module evals came back "Uncaught null"
    /// (weixin article pages: one genuine spinner, 30+ collateral kills).
    /// The kill site now cancels the termination; the scripts after a killed
    /// one keep running, exactly like Chrome.
    #[tokio::test(flavor = "current_thread")]
    async fn per_script_guard_kill_does_not_poison_later_scripts() {
        let _net = net_test_guard();
        // Generous phase deadline so the exec watchdog never fires here —
        // the ONLY termination in play is the spinner's own 5s guard.
        let _deadline = script_deadline_guard("20000");
        let pad = " ".repeat(10_100);
        let spinner = format!("var pad = '{pad}'; while (true) {{}}");
        let port = local_http_server_typed(vec![
            (
                "/spin.html",
                200,
                "text/html",
                format!(
                    r#"<html><head>
                        <script>window.__dcl__ = false;
                            document.addEventListener('DOMContentLoaded', function() {{ window.__dcl__ = true; }});</script>
                        <script>{spinner}</script>
                        <script>window.__after_spin__ = 'ran';</script>
                        <script src="/late.js"></script>
                    </head><body>body text</body></html>"#
                ),
            ),
            (
                "/late.js",
                200,
                "application/javascript",
                "window.__late_external__ = 'ran';".to_string(),
            ),
        ]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/spin.html")).await.unwrap();
        // The killed spinner ends the spinner — nothing else.
        assert_eq!(
            p.evaluate("window.__after_spin__"),
            serde_json::json!("ran"),
            "the inline script after a guard-killed one must still run"
        );
        assert_eq!(
            p.evaluate("window.__late_external__"),
            serde_json::json!("ran"),
            "the external script after a guard-killed one must still run"
        );
        assert_eq!(p.evaluate("document.readyState"), serde_json::json!("complete"));
        assert_eq!(p.evaluate("window.__dcl__"), serde_json::json!(true));
    }

    /// Regression (module half of the same weixin hang): a module whose
    /// top-level spins forever pinned the session thread inside V8 —
    /// `mod_evaluate` runs the top-level synchronously, the tokio budget
    /// around it only wraps futures, so neither the module budget nor the
    /// 30s navigation deadline could ever fire and the HTTP caller got no
    /// answer at all (>2min, no response). The eval now runs under a V8
    /// watchdog 250ms past the budget; the killed module ends the module,
    /// never the page's load lifecycle.
    #[tokio::test(flavor = "current_thread")]
    async fn module_top_level_spin_cannot_wedge_navigation() {
        let _net = net_test_guard();
        let _budget = module_eval_guard("1000");
        let port = local_http_server_typed(vec![(
            "/mod.html",
            200,
            "text/html",
            r#"<html><head>
                <script>window.__dcl__ = false;
                    document.addEventListener('DOMContentLoaded', function() { window.__dcl__ = true; });</script>
                <script type="module">while (true) {}</script>
            </head><body>body text</body></html>"#
                .to_string(),
        )]);
        let mut p = test_page();
        p.navigate(&format!("http://127.0.0.1:{port}/mod.html"))
            .await
            .unwrap();
        assert_eq!(
            p.evaluate("document.readyState"),
            serde_json::json!("complete"),
            "a spinning module top-level must not wedge the load lifecycle"
        );
        assert_eq!(p.evaluate("window.__dcl__"), serde_json::json!(true));
    }

    // Network.setBlockedURLs must reach render-path fetches too (the
    // obscura 97ff86d / #890 same-hole): the band-image pump shares the
    // hard-block list the static script/stylesheet loaders already enforce,
    // so a matched URL never opens a connection. The unblocked sibling in
    // the same batch is the negative control — it must still arrive.
    #[cfg(feature = "screenshot")]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn blocked_urls_stop_band_image_fetches() {
        let _net = net_test_guard();
        // A server that records every request path it serves.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let recorder = seen.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"/>"#;
            for _ in 0..8 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                recorder.lock().unwrap().push(path);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: image/svg+xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    svg.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(svg);
                let _ = stream.flush();
            }
        });

        let mut p = test_page();
        p.set_blocked_urls(vec!["*blocked.svg*".to_string()]);
        p.fetch_band_images(vec![
            format!("http://127.0.0.1:{port}/blocked.svg"),
            format!("http://127.0.0.1:{port}/allowed.svg"),
        ])
        .await;

        let served = seen.lock().unwrap().clone();
        assert!(
            served.iter().all(|path| path == "/allowed.svg"),
            "a setBlockedURLs match must not open a render-path connection, served: {served:?}"
        );
        assert_eq!(
            served.len(),
            1,
            "the unblocked sibling in the batch must still load (negative control)"
        );
    }

    /// A bare SVG document navigated as a document: Chrome sizes the root
    /// svg at 100%x100% of the viewport with no body UA margin. Author
    /// width/height attributes on the root stay authoritative.
    #[cfg(feature = "screenshot")]
    #[tokio::test(flavor = "current_thread")]
    async fn svg_document_root_fills_viewport() {
        let _g = net_test_guard();
        let bare = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512"><rect width="512" height="512" fill="#0d1520"/></svg>"##.to_string();
        let authored = r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20" viewBox="0 0 512 512"/>"#.to_string();
        let port = local_http_server_typed(vec![
            ("/bare.svg", 200, "image/svg+xml", bare),
            ("/authored.svg", 200, "image/svg+xml", authored),
        ]);
        let mut p = test_page();
        p.set_viewport_override(300.0, 200.0, false, None);
        p.navigate(&format!("http://127.0.0.1:{port}/bare.svg"))
            .await
            .unwrap();
        // x=0 pins the body UA margin as reset; 300x200 pins the injected
        // viewport intrinsic (not the 512x512 viewBox, not 300x150).
        assert_eq!(
            p.evaluate("document.querySelector('svg').getBoundingClientRect().x").as_f64(),
            Some(0.0),
            "body margin must be reset"
        );
        assert_eq!(
            p.evaluate("document.querySelector('svg').getBoundingClientRect().y").as_f64(),
            Some(0.0)
        );
        assert_eq!(
            p.evaluate("document.querySelector('svg').getBoundingClientRect().width").as_f64(),
            Some(300.0)
        );
        assert_eq!(
            p.evaluate("document.querySelector('svg').getBoundingClientRect().height").as_f64(),
            Some(200.0)
        );

        // Author attrs on the document root win over the injection.
        p.navigate(&format!("http://127.0.0.1:{port}/authored.svg"))
            .await
            .unwrap();
        assert_eq!(
            p.evaluate("document.querySelector('svg').getBoundingClientRect().width").as_f64(),
            Some(40.0)
        );
        assert_eq!(
            p.evaluate("document.querySelector('svg').getBoundingClientRect().height").as_f64(),
            Some(20.0)
        );
    }

    // ---- non-HTML document pipeline (moli#514 class) ---------------------

    #[test]
    fn text_document_classification() {
        for mime in [
            "text/plain",
            "text/csv",
            "text/css",
            "text/javascript",
            "application/json",
            "application/javascript",
            "application/xml",
            "text/xml",
            "application/rss+xml",
            "application/atom+xml",
            "application/ld+json",
        ] {
            assert!(renders_as_text_document(mime), "{mime} must render as text");
        }
        for mime in ["text/html", "application/xhtml+xml", "image/svg+xml", "image/png", ""] {
            assert!(!renders_as_text_document(mime), "{mime} must not render as text");
        }
    }

    #[test]
    fn plain_text_document_wraps_pre_verbatim() {
        let dom = plain_text_document("a < b && c > d\nsecond line");
        let pre = dom.query_selector("body > pre").unwrap().expect("pre wrapper");
        assert_eq!(dom.text_content(pre), "a < b && c > d\nsecond line");
        assert!(
            dom.query_selector("b").unwrap().is_none(),
            "markup-looking text must stay inert"
        );
    }

    /// moli#514 同洞: a text/plain response must surface as a `<pre>`
    /// document with `document.contentType` from the response header — not
    /// the "text/html" the URL sniffing used to invent.
    #[tokio::test(flavor = "current_thread")]
    async fn text_plain_navigation_wraps_pre_and_reports_content_type() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![
            (
                "/log",
                200,
                "text/plain; charset=utf-8",
                "first line <notatag>\nsecond & line".to_string(),
            ),
            ("/data", 200, "application/json", "{\"a\": 1}\n{\"b\": 2}".to_string()),
        ]);
        let mut p = test_page();

        p.navigate(&format!("http://127.0.0.1:{port}/log")).await.unwrap();
        assert_eq!(p.evaluate("document.contentType").as_str(), Some("text/plain"));
        assert_eq!(p.evaluate("document.body.firstElementChild.tagName").as_str(), Some("PRE"));
        assert_eq!(
            p.evaluate("document.body.textContent").as_str(),
            Some("first line <notatag>\nsecond & line"),
            "source must survive verbatim — newlines and markup chars included"
        );
        assert_eq!(p.evaluate("document.querySelector('notatag')").as_str(), None);

        p.navigate(&format!("http://127.0.0.1:{port}/data")).await.unwrap();
        assert_eq!(p.evaluate("document.contentType").as_str(), Some("application/json"));
        assert_eq!(p.evaluate("document.body.firstElementChild.tagName").as_str(), Some("PRE"));
        assert_eq!(p.evaluate("document.body.textContent").as_str(), Some("{\"a\": 1}\n{\"b\": 2}"));
    }

    /// Regression face: text/html keeps the HTML pipeline — no `<pre>`
    /// wrapper, and contentType comes from the header, not URL sniffing.
    #[tokio::test(flavor = "current_thread")]
    async fn html_response_stays_on_html_pipeline() {
        let _g = net_test_guard();
        let port = local_http_server_typed(vec![(
            "/feed.xml",
            200,
            "text/html",
            "<html><body><p>real html</p></body></html>".to_string(),
        )]);
        let mut p = test_page();
        // The .xml extension used to make contentType report
        // "application/xml"; the response header wins in Chrome.
        p.navigate(&format!("http://127.0.0.1:{port}/feed.xml")).await.unwrap();
        assert_eq!(p.evaluate("document.contentType").as_str(), Some("text/html"));
        assert_eq!(p.evaluate("document.querySelector('p').textContent").as_str(), Some("real html"));
        assert_eq!(p.evaluate("document.body.firstElementChild.tagName").as_str(), Some("P"));
    }

    // ---- #66 busy-storm freeze -------------------------------------------

    /// The under-budget burner (issue #66's shape): depth-bounded mutual
    /// recursion inside an interval callback whose busy time stays below
    /// every watchdog budget — settle never fires, no RangeError (bounded
    /// depth), the loop never idles. Before the fix this pinned the session
    /// thread for its whole life (measured 45-48% of a core, zero watchdog
    /// fires). After: the trailing busy window freezes the realm, the pump
    /// parks without re-entering V8, and a navigation unfreezes.
    #[tokio::test(flavor = "current_thread")]
    async fn busy_storm_freezes_park_and_navigation_unfreezes() {
        let _g = busy_limit_guard("2");
        let mut p = test_page();
        // 46ms busy per 50ms tick: nominal 92% duty, but in-engine timer
        // dispatch adds ~30-40ms per tick (measured: a 36/40 burner lands
        // at ~46% real duty), so this sits ~55-60% actual — safely over the
        // 40% freeze line with margin on both sides.
        p.navigate(
            "data:text/html,%3Cscript%3Evar%20mark%3D0%3Bfunction%20a%28n%29%7Bmark%2B%2B%3Bif%28n%3C%3D0%29return%200%3Breturn%20b%28n-1%29%7Dfunction%20b%28n%29%7Bmark%2B%2B%3Bif%28n%3C%3D0%29return%200%3Breturn%20a%28n-1%29%7DsetInterval%28function%28%29%7Bvar%20t%3DDate.now%28%29%3Bwhile%28Date.now%28%29-t%3C46%29%7Ba%28250%29%7D%7D%2C50%29%3C%2Fscript%3E",
        )
        .await
        .unwrap();

        // Drive the idle pump like the session loop does until the 2s busy
        // window closes. The burner keeps the loop non-idle, and its real
        // in-engine duty (~55%) is what the window must catch — nominal
        // duty is a lie once timer dispatch latency is paid.
        let t0 = std::time::Instant::now();
        while !p.js_busy_frozen() && t0.elapsed() < std::time::Duration::from_secs(6) {
            p.pump_event_loop_slice(200).await;
        }
        assert!(p.js_busy_frozen(), "burner must trip the busy freeze");
        // The freeze is accounting-driven, not termination-driven: no
        // watchdog ever fired on this realm.
        assert_eq!(p.js.as_ref().unwrap().watchdog_fired_total(), 0);

        // Frozen realm parks instead of re-entering V8: cumulative active
        // time stops moving.
        let active = p.js.as_ref().unwrap().v8_active_ns();
        p.pump_event_loop_slice(300).await;
        assert_eq!(p.js.as_ref().unwrap().v8_active_ns(), active);

        // session_console face: the freeze left an error entry.
        let entries = p.take_pending_console_calls();
        assert!(
            entries
                .iter()
                .any(|(level, msg, _)| level == "error" && msg.contains("frozen")),
            "freeze must explain itself in the console ring, got {:?}",
            entries
        );

        // Unfreeze: a document swap rebuilds the realm and clears the flag;
        // fresh JS runs again.
        p.navigate("data:text/html,%3Cscript%3Ewindow.__revived%3D1%3C/script%3E")
            .await
            .unwrap();
        assert!(!p.js_busy_frozen());
        assert_eq!(p.evaluate("window.__revived").as_f64(), Some(1.0));
    }

    /// False-positive guard: a page parked on a long timer is never idle,
    /// but its poll-execution time is ~zero — the busy window must not
    /// freeze it.
    #[tokio::test(flavor = "current_thread")]
    async fn parked_timer_page_never_freezes() {
        let _g = busy_limit_guard("2");
        let mut p = test_page();
        p.navigate(
            "data:text/html,%3Cscript%3Ewindow.__ticked%3D0%3BsetTimeout(function()%7Bwindow.__ticked%3D1%7D%2C10000)%3C/script%3E",
        )
        .await
        .unwrap();
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_secs(3) {
            p.pump_event_loop_slice(200).await;
            assert!(!p.js_busy_frozen(), "a parked 10s timer must never trip the freeze");
        }
        // The timer is still pending (not yet fired) and the realm is alive.
        assert_eq!(p.evaluate("window.__ticked").as_f64(), Some(0.0));
    }
