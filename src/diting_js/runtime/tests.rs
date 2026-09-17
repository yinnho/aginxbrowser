    use super::*;
    use crate::diting_dom::parse_html;

    fn setup_runtime(html: &str) -> JsRuntime {
        let dom = parse_html(html);
        let rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_url("http://example.com/test");
        rt.set_title("Test Page");
        rt
    }

    #[test]
    fn test_document_title() {
        let mut rt = setup_runtime("<html><head><title>Test</title></head><body></body></html>");
        let title = rt.evaluate("document.title").unwrap();
        assert_eq!(title, serde_json::json!("Test Page"));
    }

    /// The hardware persona (screen/dpr/GPU/canvas) is drawn from a seed the
    /// host owns: the realm is rebuilt per navigation and __diting_init
    /// self-deletes after drawing, so a Page re-pins the same seed via
    /// set_fingerprint_seed on every fresh realm — a real machine does not
    /// change its screen or GPU between pages of one visit (the identity
    /// flipping per page is its own automation tell).
    #[test]
    fn fingerprint_seed_pins_hardware_identity_across_realms() {
        let probe = "JSON.stringify([screen.width, screen.height, devicePixelRatio, \
                     navigator.hardwareConcurrency])";
        let id_a = {
            let mut a = setup_runtime("<html><body></body></html>");
            a.set_fingerprint_seed(0x5EED_0001);
            a.evaluate(probe).unwrap()
        };
        let id_b = {
            let mut b = setup_runtime("<html><body></body></html>");
            b.set_fingerprint_seed(0x5EED_0001);
            let v = b.evaluate(probe).unwrap();
            assert_eq!(
                b.evaluate("_fpSeed").unwrap(),
                serde_json::json!(0x5EED_0001u64 as f64),
                "the pinned seed lands verbatim and drives the persona draws"
            );
            v
        };
        assert_eq!(
            id_a, id_b,
            "same seed must reproduce the same machine persona in a fresh realm"
        );
    }

    /// obscura#734 lineage: Intl's default locale must follow the configured
    /// language source, not the process locale. Two layers keep them agreed:
    /// set_language pins ICU's default (fresh isolates), and bootstrap.js
    /// binds undefined locale args to `__diting_lang` per call (V8 caches the
    /// resolved default per-isolate after first Intl use, so a re-pin alone
    /// can't refresh an existing isolate). A three-way (Intl / navigator /
    /// Accept-Language) mismatch is a hard headless tell.
    #[test]
    fn set_language_keeps_intl_and_navigator_locale_agreed() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_language("zh-CN,zh;q=0.9,en;q=0.8");
        let locale = rt
            .evaluate("Intl.DateTimeFormat().resolvedOptions().locale")
            .unwrap();
        assert_eq!(locale, serde_json::json!("zh-CN"), "Intl follows the configured language");
        let nav = rt.evaluate("navigator.language").unwrap();
        assert_eq!(nav, serde_json::json!("zh-CN"), "navigator.language agrees with Intl");
        let num = rt.evaluate("Intl.NumberFormat().resolvedOptions().locale").unwrap();
        assert_eq!(num, serde_json::json!("zh-CN"), "generic Intl wrappers bind too");
        // And back the other way on the SAME isolate: the bootstrap binding
        // reads `__diting_lang` per call, so this flips even though V8 has
        // already cached an ICU default for the isolate.
        rt.set_language("en-US,en;q=0.9");
        let locale = rt
            .evaluate("Intl.DateTimeFormat().resolvedOptions().locale")
            .unwrap();
        assert_eq!(locale, serde_json::json!("en-US"));
        let nav = rt.evaluate("navigator.language").unwrap();
        assert_eq!(nav, serde_json::json!("en-US"));
    }

    /// obscura#777 class: the CDP acceptLanguage override seeds the persona
    /// on a LIVE isolate (set_navigator_language), which must move navigator
    /// and Intl's undefined-locale binding while leaving the process-global
    /// ICU pin alone — set_default_locale is not per-isolate, so re-pinning
    /// mid-session would contaminate sibling isolates (obscura #778 hazard).
    #[test]
    fn set_navigator_language_moves_persona_without_repinning_icu() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_language("zh-CN,zh;q=0.9,en;q=0.8");
        assert_eq!(rt.evaluate("navigator.language").unwrap(), serde_json::json!("zh-CN"));
        // Mid-session persona move via the CDP acceptLanguage path.
        rt.set_navigator_language("fr-FR,fr;q=0.9");
        assert_eq!(rt.evaluate("navigator.language").unwrap(), serde_json::json!("fr-FR"));
        assert_eq!(
            rt.evaluate("navigator.languages.join('|')").unwrap(),
            serde_json::json!("fr-FR|fr")
        );
        // Intl follows through the bootstrap binding (reads __diting_lang per
        // call), so the persona stays coherent — same contract as set_language
        // minus the process-global ICU re-pin.
        let locale = rt
            .evaluate("Intl.DateTimeFormat().resolvedOptions().locale")
            .unwrap();
        assert_eq!(locale, serde_json::json!("fr-FR"));
    }

    /// obscura#737 lineage probe: matchMedia answered `(min-width:640px)`
    /// with false while the persona published a 2560px innerWidth — a page's
    /// JS branching disagreed with the @media rules the CSS cascade applied,
    /// and the self-contradiction was itself a fingerprint tell (scripts
    /// cross-check the two). The evaluator now reads the live window
    /// viewport, so the two cannot drift.
    #[test]
    fn match_media_agrees_with_published_viewport() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let checks = rt.evaluate(r#"
            const vw = innerWidth, vh = innerHeight;
            return [
                matchMedia("").matches,
                matchMedia("(min-width: 640px)").matches === (vw >= 640),
                matchMedia("(min-width: 99999px)").matches,
                matchMedia("(max-width: 10px)").matches === (vw <= 10),
                matchMedia("(min-width: " + vw + "px)").matches,
                matchMedia("(max-width: " + (vw - 0.01) + "px)").matches,
                matchMedia("screen and (min-width: 100px)").matches,
                matchMedia("print").matches,
                matchMedia("not print").matches,
                matchMedia("(min-width: 99999px), (min-width: 100px)").matches,
                matchMedia("(orientation: landscape)").matches === (vw >= vh),
                matchMedia("(prefers-color-scheme: light)").matches,
                matchMedia("(prefers-color-scheme: dark)").matches,
                matchMedia("(unknown-feature: 3)").matches,
            ];
        "#).unwrap();
        let parts = checks.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!(true), "empty query matches everything");
        assert_eq!(parts[1], serde_json::json!(true), "min-width tracks the real viewport");
        assert_eq!(parts[2], serde_json::json!(false), "impossible min-width is false");
        assert_eq!(parts[3], serde_json::json!(true), "max-width tracks the real viewport");
        assert_eq!(parts[4], serde_json::json!(true), "boundary width is inclusive");
        assert_eq!(parts[5], serde_json::json!(false), "just-below viewport is false");
        assert_eq!(parts[6], serde_json::json!(true), "screen and (...) matches");
        assert_eq!(parts[7], serde_json::json!(false), "print does not match");
        assert_eq!(parts[8], serde_json::json!(true), "not print matches");
        assert_eq!(parts[9], serde_json::json!(true), "comma list is OR");
        assert_eq!(parts[10], serde_json::json!(true), "orientation follows the viewport");
        assert_eq!(parts[11], serde_json::json!(true), "persona is light color scheme");
        assert_eq!(parts[12], serde_json::json!(false), "dark scheme does not match");
        assert_eq!(parts[13], serde_json::json!(false), "unknown features are false");
    }

    /// Issue #25 / obscura#1007: MediaQueryList objects must be live —
    /// `matches` re-evaluates against the current viewport (not frozen at
    /// creation), listeners actually register, and a viewport change fires
    /// `change` (carrying the new matches/media) on every subscribed
    /// object that crossed. The old face returned a disposable literal
    /// whose addListener was a no-op, so responsive pages never saw a
    /// breakpoint flip after session_viewport.
    #[test]
    fn matchmedia_live_objects_fire_change_on_viewport_flip() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let out = rt.evaluate(r#"
            var hits = [];
            var wide = matchMedia('(min-width: 800px)');
            wide.addEventListener('change', function (e) { hits.push('ac:' + e.matches + ':' + e.media); });
            var narrow = matchMedia('(max-width: 500px)');
            narrow.onchange = function (e) { hits.push('oc:' + e.matches); };
            var inert = matchMedia('(min-width: 800px)'); // never subscribed
            var before = wide.matches;
            __diting_setViewport(400, 800, false, undefined);
            var out = [before, wide.matches, narrow.matches, inert.matches,
                       hits.join(';')].join('|');
            __diting_setViewport(1920, 1000, false, undefined);
            return out;
        "#).unwrap();
        // Default viewport is wide: min-width 800 starts true; after the
        // 400px viewport it flips (firing change with the query string),
        // max-width 500 crosses to true (onchange fires), and the
        // unsubscribed object still reports live matches without events.
        assert_eq!(
            out.as_str().unwrap(),
            "true|false|true|false|ac:false:(min-width: 800px);oc:true"
        );
    }

    /// Same object surface, override half: the mobile viewport emulation
    /// flips pointer/hover answers, and the legacy addListener API must
    /// observe the crossing too.
    #[test]
    fn matchmedia_override_flip_reaches_legacy_listeners() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let out = rt.evaluate(r#"
            var hits = [];
            var coarse = matchMedia('(pointer: coarse)');
            coarse.addListener(function (e) { hits.push('legacy:' + e.matches); });
            __diting_setViewport(400, 800, true, undefined);
            var out = coarse.matches + '|' + hits.join(';');
            __diting_setViewport(1920, 1000, false, undefined);
            return out;
        "#).unwrap();
        assert_eq!(out.as_str().unwrap(), "true|legacy:true");
    }

    /// Issue #29: `Emulation.setEmulatedMedia` (Playwright's
    /// page.emulateMedia) must flip all three faces together — the
    /// matchMedia script face, its change events, AND the @media cascade
    /// that getComputedStyle resolves through. The pre-emulation gCS read
    /// matters: it pins the cascade snapshot at the old epoch, and the
    /// regression was exactly that ordering serving the stale table after
    /// the flip.
    #[cfg(feature = "screenshot")]
    #[test]
    fn media_emulation_flips_matchmedia_and_cascade() {
        let mut rt = setup_runtime(
            "<html><head><style>\
             @media (prefers-color-scheme: dark) { #t { color: rgb(0, 0, 255); } }\
             @media (prefers-reduced-motion: reduce) { #m { display: none; } }\
             </style></head><body><div id='t'>x</div><div id='m'>y</div></body></html>",
        );
        assert_eq!(
            rt.evaluate("getComputedStyle(document.getElementById('t')).color").unwrap(),
            serde_json::json!("rgb(0, 0, 0)"),
            "persona default is light — dark arm does not apply before emulation"
        );
        let out = rt.evaluate(r#"
            var hits = [];
            var q = matchMedia('(prefers-color-scheme: dark)');
            q.addEventListener('change', function (e) { hits.push(e.matches); });
            __diting_setMediaFeatures('[["prefers-color-scheme","dark"],["prefers-reduced-motion","reduce"]]', 'screen');
            return [
                matchMedia('(prefers-color-scheme: dark)').matches,
                matchMedia('(prefers-reduced-motion: reduce)').matches,
                hits.join(';'),
                getComputedStyle(document.getElementById('t')).color,
                getComputedStyle(document.getElementById('m')).display,
            ].join('|');
        "#).unwrap();
        assert_eq!(
            out.as_str().unwrap(),
            "true|true|true|rgb(0, 0, 255)|none",
            "script face, change event, and cascade must flip together"
        );
        // Chrome semantics on the follow-ups: `features: []` REPLACES the
        // emulated set (dark arm drops, color falls back) while `media:
        // 'print'` flips the type; then `media: ''` clears back to screen.
        let out2 = rt.evaluate(r#"
            __diting_setMediaFeatures('[]', 'print');
            var a = [matchMedia('print').matches, matchMedia('screen').matches,
                     getComputedStyle(document.getElementById('t')).color].join('|');
            __diting_setMediaFeatures('[]', '');
            return a + '#' + matchMedia('print').matches;
        "#).unwrap();
        assert_eq!(
            out2.as_str().unwrap(),
            "true|false|rgb(0, 0, 0)#false",
            "features replace, media replaces, empty media clears"
        );
    }

    /// Small-caps batch: `font-variant-caps` parses, inherits, rides both
    /// shorthands (`font: small-caps …` / `font-variant: small-caps`) and
    /// reads back through getComputedStyle as "small-caps"/"normal".
    #[cfg(feature = "screenshot")]
    #[test]
    fn font_variant_caps_parse_inherit_and_computed() {
        let mut rt = setup_runtime(
            "<html><head><style>\
             #t { font-variant-caps: small-caps; }\
             #f { font: small-caps 20px serif; }\
             #v { font-variant: small-caps; }\
             </style></head><body>\
             <div id='t'><span id='c'>x</span></div>\
             <div id='f'>y</div><div id='v'>z</div><div id='n'>w</div>\
             </body></html>",
        );
        let mut gcs = |sel: &str, expr: &str| {
            rt.evaluate(&format!(
                "getComputedStyle(document.getElementById('{}')).{}",
                sel, expr
            ))
            .unwrap()
        };
        assert_eq!(gcs("t", "getPropertyValue('font-variant-caps')"), serde_json::json!("small-caps"));
        assert_eq!(gcs("c", "fontVariantCaps"), serde_json::json!("small-caps"), "font-variant-caps inherits");
        assert_eq!(gcs("f", "fontVariantCaps"), serde_json::json!("small-caps"), "font shorthand carries small-caps");
        assert_eq!(gcs("v", "fontVariantCaps"), serde_json::json!("small-caps"));
        assert_eq!(gcs("v", "getPropertyValue('font-variant')"), serde_json::json!("small-caps"), "font-variant shorthand sets both faces");
        assert_eq!(gcs("n", "fontVariantCaps"), serde_json::json!("normal"), "unset computes to normal");
    }

    /// Issue #30: engine-internal `_`-prefixed state must be invisible to
    /// enumerability. Chrome keeps DOM state in native slots —
    /// `Object.keys(div)` is `[]` there — and `for..in` walkers like zone.js's
    /// patchClass see only the interface operations. Our JS-implemented DOM
    /// stores internals as own props; they used to enumerate on every
    /// element, observer, and prototype (a one-line engine tell).
    #[cfg(feature = "screenshot")]
    #[test]
    fn internal_underscore_state_is_not_enumerable() {
        let mut rt = setup_runtime(
            "<html><body><div id='d' class='x'>t</div><a id='l' rel='nofollow'>l</a></body></html>",
        );
        let out = rt.evaluate(r#"
            var el = document.getElementById('d');
            el.classList.add('y');                 // lazy _classList
            var link = document.getElementById('l');
            void link.relList;                     // lazy _relList
            var mo = new MutationObserver(function(){});
            var ro = new ResizeObserver(function(){});
            var io = new IntersectionObserver(function(){});
            var forIn = [];
            for (var k in mo) forIn.push(k);       // zone.js patchClass walk
            return JSON.stringify([
                Object.keys(el),
                Object.keys(document.body),
                Object.keys(document.documentElement),
                Object.keys(mo), Object.keys(ro), Object.keys(io),
                Object.keys(new FileReader())
                    .filter(function(k){ return k[0] === '_'; }),
                Object.keys(new URL('http://x.test/a?b=c')),
                Object.keys(new Headers()),
                Object.keys(new URLSearchParams('a=b')),
                Object.keys(MutationObserver.prototype),
                Object.keys(ResizeObserver.prototype),
                Object.keys(IntersectionObserver.prototype),
                Object.keys(FileReader.prototype).filter(function(k){ return k[0] === '_'; }),
                forIn,
            ]);
        "#).unwrap();
        let faces: Vec<Vec<String>> = serde_json::from_str(out.as_str().unwrap()).unwrap();
        // All instance faces (0-9) hold no own ENUMERABLE props: that is the
        // Chrome face — for-in walkers (zone.js patchClass, Object.keys
        // fingerprints) see nothing. Chrome's `[]` comes from native slots;
        // our `_` internals still exist as non-enumerable own props (v1
        // boundary, issue #30), which only getOwnPropertyNames reveals.
        for (i, face) in faces.iter().take(10).enumerate() {
            assert!(face.is_empty(), "face {i} leaked internals: {face:?}");
        }
        // Prototypes expose the public operations only — no `_notify`,
        // `_fireFor`, `_read`, `_fire`, `_pull`, `_push`.
        for (i, face) in faces.iter().enumerate().skip(10).take(4) {
            let bad: Vec<&String> = face.iter().filter(|k| k.starts_with('_')).collect();
            assert!(bad.is_empty(), "prototype face {i} leaked internals: {bad:?}");
        }
        // The zone.js walk still finds the interface operations (that's the
        // #27 face — ops must stay enumerable) and nothing else.
        assert_eq!(
            faces[14], vec!["observe", "disconnect", "takeRecords"],
            "for-in over a MutationObserver must see exactly the operations"
        );
    }

    /// Issue #31: public interface members must live on the PROTOTYPE as
    /// enumerable accessor pairs — instances carry nothing. In Chrome
    /// `Object.keys(new WebSocket(...))` is `[]` and
    /// `Object.keys(WebSocket.prototype)` holds the 15 interface members;
    /// our shims used to put publics (url, readyState, onopen, ...) on the
    /// instance as plain data props, so both faces fingerprinted wrong.
    /// The constructors keep their `this.x = ...` writes — those now route
    /// through the prototype setters into hidden `_x` fields.
    #[cfg(feature = "screenshot")]
    #[test]
    fn public_members_live_on_the_prototype() {
        let mut rt = setup_runtime("<html><body><div id='t'>x</div></body></html>");
        let out = rt.evaluate(r#"
            var ws = new WebSocket('ws://127.0.0.1:1/unreachable');
            var rs = new ReadableStream({ start: function(c){ c.enqueue('a'); } });
            var wst = new WritableStream();
            var rLock = [];
            rLock.push(rs.locked);
            var rd = rs.getReader(); rLock.push(rs.locked);
            rd.releaseLock(); rLock.push(rs.locked);
            var wLock = [];
            wLock.push(wst.locked);
            var wr = wst.getWriter(); wLock.push(wst.locked);
            wr.releaseLock(); wLock.push(wst.locked);
            var fired = 0;
            ws.addEventListener('error', function(){ fired++; });
            ws.dispatchEvent({ type: 'error' });
            return JSON.stringify({
                inst: [
                    Object.keys(new FileReader()),
                    Object.keys(ws),
                    Object.keys(new EventSource('/x')),
                    Object.keys(new BroadcastChannel('t')),
                    Object.keys(new ReadableStream()),
                    Object.keys(wst),
                ],
                wsProto: Object.keys(WebSocket.prototype),
                rLock: rLock, wLock: wLock,
                wsWrite: (function(){ ws.readyState = 3; return ws.readyState; })(),
                fired: fired,
                desc: (function(){
                    var d = Object.getOwnPropertyDescriptor(
                        WebSocket.prototype, 'readyState');
                    return !!(d && d.get && d.set && d.enumerable);
                })(),
            });
        "#).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        // All six instance faces are empty — the Chrome face.
        for (i, face) in v["inst"].as_array().unwrap().iter().enumerate() {
            assert_eq!(
                face, &serde_json::json!([]),
                "instance face {i} must be [] (publics belong on the prototype)"
            );
        }
        // WebSocket.prototype carries exactly Chrome's 15 members.
        assert_eq!(
            v["wsProto"],
            serde_json::json!([
                "send", "close",
                "url", "readyState", "bufferedAmount", "binaryType",
                "extensions", "protocol",
                "onopen", "onmessage", "onerror", "onclose",
                "addEventListener", "removeEventListener", "dispatchEvent",
            ]),
            "WebSocket.prototype member set must match Chrome"
        );
        // Lock semantics survive the prototype-getter conversion.
        assert_eq!(v["rLock"], serde_json::json!([false, true, false]));
        assert_eq!(v["wLock"], serde_json::json!([false, true, false]));
        // Ctor-style public writes route through the setter (no throw).
        assert_eq!(v["wsWrite"], serde_json::json!(3));
        // Prototype listener methods actually fire.
        assert_eq!(v["fired"], serde_json::json!(1));
        // The accessors are enumerable getter/setter pairs on the prototype.
        assert_eq!(v["desc"], serde_json::json!(true));
    }

    /// Issue #32: legacy reflected attributes (align on div/p/h*) are real
    /// [Reflect] DOMString members on the per-tag interfaces — Chrome's
    /// Object.keys(HTMLDivElement.prototype) is ["align"] — and
    /// ReadableStream gains the enumerable values() member with
    /// Symbol.asyncIterator aliased to it (#31's v1 boundary).
    #[cfg(feature = "screenshot")]
    #[test]
    fn reflected_align_and_stream_values() {
        let mut rt = setup_runtime("<html><body><div id='t'>x</div></body></html>");
        let out = rt.evaluate(r#"
            var d = document.createElement('div');
            d.setAttribute('align', 'center');
            var viaGetter = d.align;
            d.align = 'right';
            var viaSetter = d.getAttribute('align');
            var p = document.createElement('p'); p.align = 'left';
            var h = document.createElement('h2'); h.align = 'top';
            var rs = new ReadableStream({ start: function(c){ c.enqueue('a'); } });
            var it = rs.values();
            return JSON.stringify({
                faces: [
                    Object.keys(HTMLDivElement.prototype),
                    Object.keys(HTMLParagraphElement.prototype),
                    Object.keys(HTMLHeadingElement.prototype),
                    Object.keys(HTMLSpanElement.prototype),
                    Object.keys(ReadableStream.prototype).slice(-1),
                ],
                align: [viaGetter, viaSetter,
                        p.getAttribute('align'), h.getAttribute('align')],
                blank: document.createElement('div').align,
                span: document.createElement('span').align,
                hasValues: typeof rs.values,
                alias: ReadableStream.prototype[Symbol.asyncIterator]
                        === ReadableStream.prototype.values,
                itNext: typeof it.next(),
                itSelf: it[Symbol.asyncIterator]() === it,
            });
        "#).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        let faces = v["faces"].as_array().unwrap();
        assert_eq!(faces[0], serde_json::json!(["align"]), "div prototype face");
        assert_eq!(faces[1], serde_json::json!(["align"]), "p prototype face");
        assert_eq!(faces[2], serde_json::json!(["align"]), "heading prototype face");
        assert_eq!(faces[3], serde_json::json!([]), "span has no align");
        assert_eq!(faces[4], serde_json::json!(["values"]), "values is the newest RS member");
        assert_eq!(
            v["align"], serde_json::json!(["center", "right", "left", "top"]),
            "align get/set round-trips through the content attribute"
        );
        assert_eq!(v["blank"], serde_json::json!(""), "no attribute -> empty string");
        assert_eq!(v["span"], serde_json::Value::Null, "span.align is undefined");
        assert_eq!(v["hasValues"], serde_json::json!("function"));
        assert_eq!(v["alias"], serde_json::json!(true), "Symbol.asyncIterator aliases values");
        assert_eq!(v["itNext"], serde_json::json!("object"), "values().next() is a promise");
        assert_eq!(v["itSelf"], serde_json::json!(true), "iterator is self-async-iterable");
    }

    #[test]
    fn reflected_body_color_and_table_family() {
        let mut rt = setup_runtime(
            "<html><body bgcolor='#eeeeee' text='navy'><div id='t'>x</div></body></html>",
        );
        let out = rt.evaluate(r#"
            var b = document.body, t = document.createElement('table');
            var d = document.createElement('div');
            var parsed0 = [b.bgColor, b.text];
            b.text = 'red'; b.aLink = 'maroon'; b.vLink = 'purple';
            b.link = 'blue'; b.background = 'bg.png';
            t.align = 'center'; t.border = '2'; t.bgColor = '#f0f0f0';
            t.cellPadding = '4'; t.cellSpacing = '0';
            return JSON.stringify({
                bodyFace: Object.keys(HTMLBodyElement.prototype).sort(),
                tblFace: Object.keys(HTMLTableElement.prototype).sort(),
                parsed: parsed0,
                bodyAttrs: [b.getAttribute('text'), b.getAttribute('alink'),
                            b.getAttribute('vlink'), b.getAttribute('link'),
                            b.getAttribute('background')],
                tblAttrs: [t.getAttribute('align'), t.getAttribute('border'),
                           t.getAttribute('bgcolor'), t.getAttribute('cellpadding'),
                           t.getAttribute('cellspacing')],
                attrToProp: (t.setAttribute('border', '5'), t.border),
                // [LegacyNullToEmptyString] applies to null only — the
                // [Reflect] plain members coerce null -> "null".
                nullEmpty: [(b.text = null, b.getAttribute('text')),
                            (t.cellPadding = null, t.getAttribute('cellpadding'))],
                plainNull: [(t.border = null, t.getAttribute('border')),
                            (d.align = null, d.getAttribute('align'))],
                undef: (t.border = undefined, t.getAttribute('border')),
                blank: [document.createElement('table').border, b.aLink === 'maroon' && ''],
            });
        "#).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(
            v["bodyFace"],
            serde_json::json!(["aLink", "background", "bgColor", "link",
                // (#37) the 24 window-reflecting on* members are enumerable
                // on the prototype too, exactly like Chrome 152
                "onafterprint", "onbeforeprint", "onbeforeunload", "onblur",
                "onerror", "onfocus", "ongamepadconnected", "ongamepaddisconnected",
                "onhashchange", "onlanguagechange", "onload", "onmessage",
                "onmessageerror", "onoffline", "ononline", "onpagehide",
                "onpageshow", "onpopstate", "onrejectionhandled", "onresize",
                "onscroll", "onstorage", "onunhandledrejection", "onunload",
                "text", "vLink"]),
            "body prototype carries the legacy color family + the on* family"
        );
        assert_eq!(
            v["tblFace"],
            serde_json::json!(["align", "bgColor", "border", "caption", "cellPadding",
                               "cellSpacing", "frame", "rows", "rules", "summary",
                               "tBodies", "tFoot", "tHead", "width"]),
            "table prototype carries the legacy family + table DOM members"
        );
        assert_eq!(v["parsed"], serde_json::json!(["#eeeeee", "navy"]),
            "content attrs parsed into the document are readable through the accessors");
        assert_eq!(
            v["bodyAttrs"],
            serde_json::json!(["red", "maroon", "purple", "blue", "bg.png"]),
            "IDL writes land under the lowercase content-attr names (alink/vlink)"
        );
        assert_eq!(
            v["tblAttrs"],
            serde_json::json!(["center", "2", "#f0f0f0", "4", "0"]),
            "table IDL writes land under cellpadding/cellspacing"
        );
        assert_eq!(v["attrToProp"], serde_json::json!("5"), "attr write -> accessor read");
        assert_eq!(v["nullEmpty"], serde_json::json!(["", ""]),
            "[LegacyNullToEmptyString]: null setter -> empty content attribute");
        assert_eq!(v["plainNull"], serde_json::json!(["null", "null"]),
            "plain [Reflect]: null setter -> \"null\" (incl. #32's div.align correction)");
        assert_eq!(v["undef"], serde_json::json!("undefined"),
            "undefined -> \"undefined\" in both modes");
        assert_eq!(v["blank"], serde_json::json!(["", ""]),
            "absent attribute -> empty string getter");
    }

    #[test]
    fn reflected_table_section_row_cell_families() {
        let mut rt = setup_runtime(
            "<html><body><table><thead char='.' charoff='2'><tr><th>H</th></tr></thead>\
             <tbody align='center' valign='top'><tr><td nowrap axis='name'>x</td></tr></tbody>\
             </table></body></html>",
        );
        let out = rt.evaluate(r#"
            var thead = document.querySelector('thead');
            var td = document.querySelector('td');
            var sec = document.createElement('tbody'), row = document.createElement('tr');
            var cell = document.createElement('td');
            sec.ch = '.'; sec.chOff = '3'; sec.align = 'left'; sec.vAlign = 'bottom';
            row.ch = '|'; row.bgColor = '#eee';
            cell.axis = 'ax'; cell.width = '30';
            var secNull = (sec.align = null, sec.getAttribute('align'));
            var rowBgNull = (row.bgColor = null, row.getAttribute('bgcolor'));
            var nw = document.createElement('td');
            var nwAbsent = nw.noWrap;
            nw.noWrap = true;
            var nwTrueAttr = nw.getAttribute('nowrap');
            var nwTrueGet = nw.noWrap;
            nw.noWrap = '0';
            var nwStringZero = nw.noWrap;
            nw.noWrap = false;
            var nwAfterFalse = [nw.hasAttribute('nowrap'), nw.noWrap];
            return JSON.stringify({
                faces: [Object.keys(HTMLTableSectionElement.prototype).sort(),
                        Object.keys(HTMLTableRowElement.prototype).sort(),
                        Object.keys(HTMLTableCellElement.prototype).sort()],
                parsed: [thead.ch, thead.chOff, td.noWrap, td.axis,
                         document.querySelector('tbody').align,
                         document.querySelector('tbody').vAlign],
                secAttrs: [sec.getAttribute('char'), sec.getAttribute('charoff'),
                           sec.getAttribute('valign')],
                rowAttrs: [row.getAttribute('char')],
                cellAttrs: [cell.getAttribute('axis'), cell.getAttribute('width')],
                attrToProp: (sec.setAttribute('char', '!'), sec.ch),
                nullModes: [secNull, rowBgNull],
                nw: [nwAbsent, nwTrueAttr, nwTrueGet, nwStringZero, nwAfterFalse],
            });
        "#).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        let faces = v["faces"].as_array().unwrap();
        assert_eq!(faces[0], serde_json::json!(["align", "ch", "chOff", "vAlign"]),
            "section prototype face");
        assert_eq!(
            faces[1],
            serde_json::json!(["align", "bgColor", "cells", "ch", "chOff",
                               "rowIndex", "sectionRowIndex", "vAlign"]),
            "row prototype face: legacy five + the DOM members"
        );
        assert_eq!(
            faces[2],
            serde_json::json!(["abbr", "align", "axis", "bgColor", "cellIndex", "ch",
                               "chOff", "colSpan", "headers", "height", "noWrap",
                               "rowSpan", "scope", "vAlign", "width"]),
            "cell prototype face: matches Chrome's enumerable face exactly (#35)"
        );
        assert_eq!(
            v["parsed"], serde_json::json!([".", "2", true, "name", "center", "top"]),
            "parsed-in markup reads through the accessors (char/charoff/nowrap/axis/align/valign)"
        );
        assert_eq!(v["secAttrs"], serde_json::json!([".", "3", "bottom"]),
            "ch/chOff write the char/charoff content attributes");
        assert_eq!(v["rowAttrs"], serde_json::json!(["|"]));
        assert_eq!(v["cellAttrs"], serde_json::json!(["ax", "30"]));
        assert_eq!(v["attrToProp"], serde_json::json!("!"), "char attr write -> ch read");
        assert_eq!(v["nullModes"], serde_json::json!(["null", ""]),
            "plain align -> \"null\"; nullEmpty bgColor -> \"\"");
        assert_eq!(
            v["nw"],
            serde_json::json!([false, "", true, true, [false, false]]),
            "boolean reflect: absent false; true -> empty-valued attribute; '0' -> true; false -> removed"
        );
    }

    #[test]
    fn reflected_ulong_and_keyword_families() {
        let mut rt = setup_runtime(
            "<html><body><table><tr><td id='c' colspan='2' rowspan='3' headers='h1 h2'>x</td>\
             <th id='h' scope='col'>H</th></tr></table></body></html>",
        );
        let out = rt.evaluate(r#"
            var td = document.getElementById('c'), th = document.getElementById('h');
            var col = document.createElement('col'), cg = document.createElement('colgroup');
            var fresh = document.createElement('td');
            var defaults = [fresh.colSpan, fresh.rowSpan, fresh.headers, fresh.scope, col.span];
            var parsed = [td.colSpan, td.rowSpan, td.headers, th.scope];
            // GETTER: clamp to the ReflectRange boundaries, default on
            // missing/garbage. HTML non-negative integer parsing: leading
            // whitespace, '+', digits, stop at first non-digit.
            var g = function(el, prop, attr, val) { el.setAttribute(attr, val); return el[prop]; };
            var gets = [
                g(fresh, 'colSpan', 'colspan', '3'), g(fresh, 'colSpan', 'colspan', '0'),
                g(fresh, 'colSpan', 'colspan', '1001'), g(fresh, 'colSpan', 'colspan', '4294967296'),
                g(fresh, 'colSpan', 'colspan', 'abc'), g(fresh, 'colSpan', 'colspan', '-2'),
                g(fresh, 'colSpan', 'colspan', '  7  '), g(fresh, 'colSpan', 'colspan', '+9'),
                g(fresh, 'colSpan', 'colspan', '12abc'), g(fresh, 'colSpan', 'colspan', '3.9'),
                g(fresh, 'rowSpan', 'rowspan', '0'), g(fresh, 'rowSpan', 'rowspan', '65535'),
                g(fresh, 'rowSpan', 'rowspan', '70000'),
                g(col, 'span', 'span', '0'), g(col, 'span', 'span', '1001'),
            ];
            // SETTER: never clamps — ToUint32 (Number(v) >>> 0), written
            // verbatim inside [0, 2^31-1], default written above that.
            var s = function(el, prop, attr, v) { el[prop] = v; var a = el.getAttribute(attr); el.removeAttribute(attr); return a; };
            var sets = [
                s(fresh, 'colSpan', 'colspan', 0), s(fresh, 'colSpan', 'colspan', 5000),
                s(fresh, 'colSpan', 'colspan', -1), s(fresh, 'colSpan', 'colspan', 4294967295),
                s(fresh, 'colSpan', 'colspan', 4294967296), s(fresh, 'colSpan', 'colspan', null),
                s(fresh, 'colSpan', 'colspan', undefined), s(fresh, 'colSpan', 'colspan', '12'),
                s(fresh, 'colSpan', 'colspan', 3.9), s(fresh, 'colSpan', 'colspan', 'abc'),
                s(fresh, 'colSpan', 'colspan', true), s(fresh, 'colSpan', 'colspan', 1e10),
                s(fresh, 'rowSpan', 'rowspan', 0), s(fresh, 'rowSpan', 'rowspan', 70000),
                s(col, 'span', 'span', 0), s(col, 'span', 'span', 2000),
            ];
            // The 0 quirk: setter writes "0" (legal), getter reads 1 (clamped).
            var zeroTrip = (fresh.colSpan = 0, [fresh.getAttribute('colspan'), fresh.colSpan]);
            var overTrip = (fresh.colSpan = 1001, [fresh.getAttribute('colspan'), fresh.colSpan]);
            // scope: lowercase exact keyword match, no "auto", no trimming; setter verbatim.
            var kw = (th.setAttribute('scope', 'ROW'), th.scope);
            var kwAuto = (th.setAttribute('scope', 'auto'), th.scope);
            var kwBogus = (th.setAttribute('scope', 'bogus'), th.scope);
            var kwSpaces = (th.setAttribute('scope', ' col '), th.scope);
            th.scope = 'bogus';
            var kwSetAttr = th.getAttribute('scope');
            th.scope = null;
            var kwSetNull = th.getAttribute('scope');
            var headersNull = (fresh.headers = null, fresh.getAttribute('headers'));
            return JSON.stringify({
                defaults: defaults,
                ifaces: [col.constructor.name, cg.constructor.name],
                colFace: Object.keys(HTMLTableColElement.prototype).sort(),
                parsed: parsed,
                gets: gets, sets: sets,
                trips: [zeroTrip, overTrip],
                kw: [kw, kwAuto, kwBogus, kwSpaces, kwSetAttr, kwSetNull],
                headersNull: headersNull,
            });
        "#).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["defaults"], serde_json::json!([1, 1, "", "", 1]),
            "ReflectDefault: colSpan/rowSpan/span 1, string reflects ''");
        assert_eq!(v["ifaces"], serde_json::json!(["HTMLTableColElement", "HTMLTableColElement"]),
            "col and colgroup share HTMLTableColElement");
        assert_eq!(v["colFace"],
            serde_json::json!(["align", "ch", "chOff", "span", "vAlign", "width"]),
            "col prototype face: legacy five + ulong span");
        assert_eq!(v["parsed"], serde_json::json!([2, 3, "h1 h2", "col"]),
            "parsed-in colspan/rowspan/headers/scope read through");
        assert_eq!(
            v["gets"],
            serde_json::json!([3, 1, 1000, 1000, 1, 1, 7, 9, 12, 3, 0, 65534, 65534, 1, 1000]),
            "getter clamps to [min,max], defaults on missing/garbage, parses per HTML integer rules"
        );
        assert_eq!(
            v["sets"],
            serde_json::json!(["0", "5000", "1", "1", "0", "0", "0", "12", "3", "0", "1",
                               "1410065408", "0", "70000", "0", "2000"]),
            "setter never clamps: ToUint32 verbatim within [0,2^31-1], default above"
        );
        assert_eq!(v["trips"], serde_json::json!([["0", 1], ["1001", 1000]]),
            "the 0/1001 round trips: attribute keeps what was written, getter clamps");
        assert_eq!(v["kw"], serde_json::json!(["row", "", "", "", "bogus", "null"]),
            "scope keyword reflect: canonical lowercase match, auto/bogus/spaces -> '', setter verbatim");
        assert_eq!(v["headersNull"], serde_json::json!("null"),
            "headers is plain reflect: null -> \"null\"");
    }

    #[test]
    fn reflected_long_form_control_families() {
        let mut rt = setup_runtime(
            "<html><body><input id='i' size='7' maxlength='5'>\
             <textarea id='t' rows='4' cols='6' wrap='HARD' maxlength='5'>x</textarea></body></html>",
        );
        let out = rt.evaluate(r#"
            var i = document.getElementById('i'), ta = document.getElementById('t');
            var inp = document.createElement('input'), tx = document.createElement('textarea');
            var defaults = [inp.size, inp.maxLength, inp.minLength,
                            tx.rows, tx.cols, tx.wrap, tx.maxLength, tx.minLength];
            var parsed = [i.size, i.maxLength, ta.rows, ta.cols, ta.wrap, ta.maxLength];
            // GETTER: HTML integer parse. size/rows/cols are positive-only
            // (0/garbage/overflow -> default); maxLength is signed with a -1
            // missing-value default (Chrome 152 — MDN's 0 is stale).
            var g = function(el, prop, attr, val) { el.setAttribute(attr, val); return el[prop]; };
            var gets = [
                g(inp, 'size', 'size', '0'), g(inp, 'size', 'size', 'abc'),
                g(inp, 'size', 'size', '-2'), g(inp, 'size', 'size', '  3  '),
                g(inp, 'size', 'size', '12abc'), g(inp, 'size', 'size', '+5'),
                g(inp, 'size', 'size', '2147483647'), g(inp, 'size', 'size', '2147483648'),
                g(inp, 'size', 'size', '3.9'),
                g(inp, 'maxLength', 'maxlength', '0'), g(inp, 'maxLength', 'maxlength', '-2'),
                g(inp, 'maxLength', 'maxlength', 'abc'), g(inp, 'maxLength', 'maxlength', '3.9'),
                g(inp, 'maxLength', 'maxlength', '2147483648'),
                g(tx, 'rows', 'rows', '0'), g(tx, 'rows', 'rows', 'abc'),
                g(tx, 'rows', 'rows', '-2'), g(tx, 'rows', 'rows', '10abc'),
                g(tx, 'cols', 'cols', '0'), g(tx, 'cols', 'cols', '+33'),
            ];
            // SETTER: three behaviors — size THROWS on a ToUint32 of 0,
            // maxLength/minLength THROW on a ToInt32 negative (including -1,
            // the getter's own default), rows/cols never throw and write the
            // DEFAULT string on 0 or 2^31 overflow.
            var T = function(f) { try { f(); return 'ok'; } catch(e) { return 'THROW:'+e.name; } };
            var s = function(el, prop, attr, v) {
                el.removeAttribute(attr);
                var r = T(function(){ el[prop] = v; });
                var a = el.getAttribute(attr);
                return [r, a];
            };
            var sets = [
                s(inp, 'size', 'size', 0), s(inp, 'size', 'size', 1e10),
                s(inp, 'size', 'size', -1), s(inp, 'size', 'size', 12),
                s(inp, 'maxLength', 'maxlength', 0), s(inp, 'maxLength', 'maxlength', -5),
                s(inp, 'maxLength', 'maxlength', -1), s(inp, 'maxLength', 'maxlength', 2147483648),
                s(inp, 'maxLength', 'maxlength', null), s(inp, 'maxLength', 'maxlength', '5x'),
                s(inp, 'maxLength', 'maxlength', 3.9),
                s(tx, 'minLength', 'minlength', -1), s(tx, 'minLength', 'minlength', 5),
                s(tx, 'rows', 'rows', 0), s(tx, 'rows', 'rows', null),
                s(tx, 'rows', 'rows', 1e10), s(tx, 'rows', 'rows', -2),
                s(tx, 'cols', 'cols', 0), s(tx, 'cols', 'cols', 40),
            ];
            // The throw is an IndexSizeError naming the receiving interface.
            var errMsg = (function(){ try { inp.size = 0; } catch(e) { return e.name + '|' + e.message.indexOf('HTMLInputElement'); } return '?'; })();
            // wrap is a PLAIN reflect: verbatim getter, String(v) setter.
            ta.setAttribute('wrap', ' hard ');
            var wVerbatim = ta.wrap;
            ta.wrap = 'soft';
            var wSet = ta.getAttribute('wrap');
            ta.wrap = null;
            var wSetNull = ta.getAttribute('wrap');
            // Negative maxlength attribute reads back as the default.
            ta.setAttribute('maxlength', '-5');
            var tripNeg = ta.maxLength;
            var inFace = Object.keys(HTMLInputElement.prototype);
            var taFace = Object.keys(HTMLTextAreaElement.prototype);
            return JSON.stringify({
                defaults: defaults,
                ifaces: [i.constructor.name, ta.constructor.name],
                parsed: parsed,
                gets: gets, sets: sets,
                errMsg: errMsg,
                wrap: [wVerbatim, wSet, wSetNull],
                tripNeg: tripNeg,
                faces: [
                    ['size', 'maxLength', 'minLength'].every(function(k){ return inFace.indexOf(k) >= 0; }),
                    ['rows', 'cols', 'wrap'].every(function(k){ return inFace.indexOf(k) < 0; }),
                    ['rows', 'cols', 'wrap', 'maxLength', 'minLength'].every(function(k){ return taFace.indexOf(k) >= 0; }),
                ],
            });
        "#).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["defaults"], serde_json::json!([20, -1, -1, 2, 20, "", -1, -1]),
            "ReflectDefault: size 20, maxLength/minLength -1 (not MDN's 0), rows 2, cols 20, wrap ''");
        assert_eq!(v["ifaces"], serde_json::json!(["HTMLInputElement", "HTMLTextAreaElement"]));
        assert_eq!(v["parsed"], serde_json::json!([7, 5, 4, 6, "HARD", 5]),
            "parsed-in size/maxlength/rows/cols/wrap read through (#36)");
        assert_eq!(
            v["gets"],
            serde_json::json!([20, 20, 20, 3, 12, 5, 2147483647, 20, 3,
                               0, -1, -1, 3, -1,
                               2, 2, 2, 10,
                               20, 33]),
            "positive-only parse defaults on 0/garbage/overflow; maxLength keeps 0 and defaults -1"
        );
        assert_eq!(
            v["sets"],
            serde_json::json!([["THROW:IndexSizeError", null], ["ok", "1410065408"],
                               ["ok", "20"], ["ok", "12"],
                               ["ok", "0"], ["THROW:IndexSizeError", null],
                               ["THROW:IndexSizeError", null], ["THROW:IndexSizeError", null],
                               ["ok", "0"], ["ok", "0"], ["ok", "3"],
                               ["THROW:IndexSizeError", null], ["ok", "5"],
                               ["ok", "2"], ["ok", "2"],
                               ["ok", "1410065408"], ["ok", "2"],
                               ["ok", "20"], ["ok", "40"]]),
            "size throws on 0; maxLength throws on ToInt32 negatives; rows/cols write defaults"
        );
        assert!(v["errMsg"].as_str().unwrap().starts_with("IndexSizeError|"),
            "the throw is an IndexSizeError naming HTMLInputElement, got {:?}", v["errMsg"]);
        assert_eq!(v["wrap"], serde_json::json!([" hard ", "soft", "null"]),
            "wrap is a plain reflect: no keyword folding, null -> \"null\"");
        assert_eq!(v["tripNeg"], serde_json::json!(-1),
            "maxlength='-5' attribute reads back as the -1 default");
        assert_eq!(v["faces"], serde_json::json!([true, true, true]),
            "input face gains size/maxLength/minLength only; textarea gains all five");
    }

    /// (#37) HTMLBodyElement's window-reflecting on* family: 24 names whose
    /// body accessors ARE the window's — shared identity both directions,
    /// `<body onX>` content attributes install as the window's handler and
    /// fire on window dispatch, non-callables store null (suppressing the
    /// attribute handler), click stays element-local. Chrome 152 truth,
    /// probed on headless 152 before writing any of this.

    #[test]
    fn body_window_reflecting_on_family() {
        let mut rt = setup_runtime(
            "<html><body onresize='window.__attrResizeRan=1' onclick='window.__attrClickRan=1'>x</body></html>",
        );
        let out = rt.evaluate(r#"
            var b = document.body;
            var names = ['afterprint','beforeprint','beforeunload','blur','error','focus',
                         'gamepadconnected','gamepaddisconnected','hashchange','languagechange',
                         'load','message','messageerror','offline','online','pagehide',
                         'pageshow','popstate','rejectionhandled','resize','scroll','storage',
                         'unhandledrejection','unload'];
            var protoKeys = Object.keys(HTMLBodyElement.prototype);
            var face = names.every(function(n){ return protoKeys.indexOf('on'+n) >= 0; });
            var d = Object.getOwnPropertyDescriptor(HTMLBodyElement.prototype, 'onhashchange');
            var desc = d ? ['get' in d ? 'acc' : 'data', d.enumerable, d.configurable].join(':') : 'none';
            // shared slot, identity both directions (dirty hashchange/popstate;
            // resize keeps its attribute for the exposure checks below)
            var f1 = function(){}, f2 = function(){};
            b.onhashchange = f1; var bodyToWin = window.onhashchange === f1;
            window.onpopstate = f2; var winToBody = b.onpopstate === f2;
            // <body onresize> is the WINDOW's handler: same function from both
            // getters, and it runs when the event dispatches on window.
            var attrBody = typeof b.onresize, attrWin = typeof window.onresize;
            var attrSame = b.onresize === window.onresize;
            window.dispatchEvent(new Event('resize'));
            var attrFired = window.__attrResizeRan || 0;
            // non-callable assignment stores null AND suppresses the attribute
            // handler (an explicit null overwrites, it does not fall through)
            b.onresize = 'garbage';
            var afterGarbage = window.onresize === null;
            window.__attrResizeRan = 0;
            window.dispatchEvent(new Event('resize'));
            var nullSuppresses = window.__attrResizeRan || 0;
            // click is NOT in the family: element-local store, no window
            // forwarding, and its content attribute runs on the element path
            var f3 = function(){};
            b.onclick = f3;
            var clickForwarded = window.onclick === f3;
            b.onclick = null;
            b.dispatchEvent(new Event('click'));
            var clickFired = window.__attrClickRan || 0;
            var gamepad = ['ongamepadconnected','ongamepaddisconnected'].every(function(k){ return k in window; });
            return JSON.stringify({
                face: face, desc: desc, bodyToWin: bodyToWin, winToBody: winToBody,
                attrBody: attrBody, attrWin: attrWin, attrSame: attrSame, attrFired: attrFired,
                afterGarbage: afterGarbage, nullSuppresses: nullSuppresses,
                clickForwarded: clickForwarded, clickFired: clickFired, gamepad: gamepad,
            });
        "#).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["face"], serde_json::json!(true),
            "all 24 window-reflecting names are enumerable own members of HTMLBodyElement.prototype");
        assert_eq!(v["desc"], serde_json::json!("acc:true:true"),
            "accessor + enumerable + configurable, like Chrome 152");
        assert_eq!(v["bodyToWin"], serde_json::json!(true), "body.X = f makes window.X === f");
        assert_eq!(v["winToBody"], serde_json::json!(true), "window.X = f makes body.X === f");
        assert_eq!(v["attrBody"], serde_json::json!("function"),
            "<body onresize> reads back as a function from the body getter");
        assert_eq!(v["attrWin"], serde_json::json!("function"),
            "...and from the window getter");
        assert_eq!(v["attrSame"], serde_json::json!(true),
            "both getters return the SAME compiled function — one handler");
        assert_eq!(v["attrFired"], serde_json::json!(1),
            "<body onresize> runs when resize dispatches on window");
        assert_eq!(v["afterGarbage"], serde_json::json!(true),
            "non-callable assignment stores null");
        assert_eq!(v["nullSuppresses"], serde_json::json!(0),
            "an explicit null overwrites the attribute handler — no fall-through");
        assert_eq!(v["clickForwarded"], serde_json::json!(false),
            "click is element-local: no window forwarding");
        assert_eq!(v["clickFired"], serde_json::json!(1),
            "<body onclick> still runs on the element dispatch path");
        assert_eq!(v["gamepad"], serde_json::json!(true),
            "ongamepadconnected/ongamepaddisconnected exist on window too");
    }


    #[cfg(feature = "screenshot")]
    #[test]
    fn computed_style_width_resolves_past_the_inline_surface() {
        let mut rt = setup_runtime(
            "<html><body><div id=\"a\">alpha</div><div id=\"b\" style=\"width: 123px\">bravo</div></body></html>",
        );
        let checks = rt.evaluate(r#"
            const a = document.getElementById("a"), b = document.getElementById("b");
            return [
                getComputedStyle(a).width,
                getComputedStyle(b).width,
                typeof getComputedStyle(a).getPropertyValue,
                typeof getComputedStyle(a).length,
            ];
        "#).unwrap();
        let parts = checks.as_array().expect("array result");
        let block_w = parts[0].as_str().expect("geometry-backed width string");
        assert!(block_w.ends_with("px") && block_w != "0px",
            "width comes from the layout rect, not the empty inline value");
        assert_eq!(parts[1], serde_json::json!("123px"), "inline width still wins the cascade-less approximation");
        assert_eq!(parts[2], serde_json::json!("function"), "interface methods still route to the target");
        assert_eq!(parts[3], serde_json::json!("number"), "interface members still route to the target");
    }

    /// obscura#734 follow-up: replacing a native Intl constructor with a
    /// plain JS function is itself a fingerprint — Function.prototype.toString
    /// stops returning [native code], name/length drift, and
    /// prototype.constructor stops closing on Intl.X. The wrappers are
    /// proxies with only a construct trap, plus a toString disguise for the
    /// one gap V8's proxy source-text resolution leaves (the anonymous
    /// "function () { [native code] }" form, without the name).
    #[test]
    fn intl_wrappers_render_native_identity() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let native_str = rt
            .evaluate(
                "(function(){\
                 \nreturn [Function.prototype.toString.call(Intl.NumberFormat),\
                 \nString(Intl.DateTimeFormat),\
                 \nFunction.prototype.toString.call(Intl.DateTimeFormat.prototype.resolvedOptions)]\
                 \n.join('|')})()",
            )
            .unwrap();
        let s = native_str.as_str().unwrap();
        for part in s.split('|') {
            assert!(part.contains("[native code]"), "expected native code form, got: {}", part);
        }
        assert!(s.contains("NumberFormat"), "toString should carry the constructor name");
        assert!(s.contains("resolvedOptions"), "resolvedOptions toString should carry its name");
        assert!(!s.contains("Wrapped"), "wrapper function name leaked");

        let ident = rt
            .evaluate(
                "[Intl.NumberFormat.name, Intl.DateTimeFormat.name,\
                 \nIntl.NumberFormat.length, Intl.DateTimeFormat.length,\
                 \nIntl.NumberFormat.prototype.constructor === Intl.NumberFormat,\
                 \nIntl.DateTimeFormat.prototype.constructor === Intl.DateTimeFormat,\
                 \n(new Intl.NumberFormat()) instanceof Intl.NumberFormat].join('|')",
            )
            .unwrap();
        assert_eq!(
            ident,
            serde_json::json!("NumberFormat|DateTimeFormat|0|0|true|true|true"),
            "name/length/constructor identity must match stock"
        );

        // The toString disguise must not eat real source: plain functions
        // still render their own text, and the disguise itself renders
        // native (spec resolves a callable proxy's source from its target).
        let passthrough = rt
            .evaluate(
                "(function(){var f=function foo(a){return a+1};\
                 \nreturn [f.toString().indexOf('return a+1')>=0,\
                 \nFunction.prototype.toString.toString().indexOf('[native code]')>=0].join('|')})()",
            )
            .unwrap();
        assert_eq!(passthrough, serde_json::json!("true|true"));
    }

    /// obscura#734 (WorkOS differ follow-up): a proxy trap inserts one extra
    /// frame into error stacks that stock does not have, and V8 labels it
    /// after the handler's constructor and the trap key. A plain-object
    /// handler reads "Object.construct"; the handler carrier is named after
    /// the wrapped constructor so the frame reads "NumberFormat.construct" —
    /// a relabel, since the frame itself is inherent to trapping in JS.
    #[test]
    fn intl_wrapper_trap_frames_carry_the_wrapped_name() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let stack = rt
            .evaluate(
                "(function(){\
                 \ntry { new Intl.NumberFormat('en-US', {localeMatcher: 'bogus'}); }\
                 \ncatch (e) { return e.stack; }\
                 \nreturn 'no-throw';\
                 \n})()",
            )
            .unwrap();
        let s = stack.as_str().expect("stack string");
        assert!(
            s.contains("NumberFormat.construct"),
            "trap frame should carry the wrapped constructor name, got:\n{}",
            s
        );
        assert!(
            !s.contains("Object.construct"),
            "plain-object handler label leaked, stack:\n{}",
            s
        );
    }

    #[test]
    fn test_document_url() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let url = rt.evaluate("document.URL").unwrap();
        assert_eq!(url, serde_json::json!("http://example.com/test"));
    }

    #[test]
    fn test_query_selector() {
        let mut rt = setup_runtime("<html><body><h1>Hello</h1><p>World</p></body></html>");
        let text = rt.evaluate("document.querySelector('h1').textContent").unwrap();
        assert_eq!(text, serde_json::json!("Hello"));
    }

    #[test]
    fn test_query_selector_all() {
        let mut rt = setup_runtime("<ul><li>A</li><li>B</li><li>C</li></ul>");
        let count = rt.evaluate("document.querySelectorAll('li').length").unwrap();
        assert_eq!(count.as_f64().unwrap() as i64, 3);
    }

    #[test]
    fn test_get_element_by_id() {
        let mut rt = setup_runtime(r#"<div id="test">Content</div>"#);
        let tag = rt.evaluate("document.getElementById('test').tagName").unwrap();
        assert_eq!(tag, serde_json::json!("DIV"));
    }

    #[test]
    fn document_fragment_get_element_by_id_searches_descendants() {
        let mut rt = setup_runtime(r#"<div id="target">document</div>"#);
        let result = rt
            .evaluate(
                r#"
                (() => {
                    const frag = document.createDocumentFragment();
                    const section = document.createElement('section');
                    section.innerHTML = '<div><span id="target">fragment</span></div><p id="a.b">literal</p>';
                    frag.appendChild(section);

                    const dup = document.createDocumentFragment();
                    const deepParent = document.createElement('div');
                    deepParent.innerHTML = '<span id="dup">deep</span>';
                    const shallow = document.createElement('p');
                    shallow.id = 'dup';
                    shallow.textContent = 'shallow';
                    dup.appendChild(deepParent);
                    dup.appendChild(shallow);

                    return [
                        frag.getElementById('target').textContent,
                        frag.getElementById('missing') === null,
                        frag.getElementById('a.b').textContent,
                        frag.getElementById(123) === null,
                        dup.getElementById('dup').textContent,
                    ];
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(["fragment", true, "literal", true, "deep"])
        );
    }

    #[test]
    fn test_inner_html() {
        let mut rt = setup_runtime(r#"<div id="x"><p>Hello</p></div>"#);
        let html = rt.evaluate("document.getElementById('x').innerHTML").unwrap();
        assert!(html.as_str().unwrap().contains("<p>"));
    }

    #[test]
    fn test_script_execution() {
        let mut rt = setup_runtime("<ul><li>A</li><li>B</li></ul>");
        rt.execute_script(
            "test",
            r#"
            globalThis.__result = [];
            document.querySelectorAll('li').forEach(function(el) {
                globalThis.__result.push(el.textContent);
            });
        "#,
        )
        .unwrap();
        let result = rt.evaluate("globalThis.__result").unwrap();
        assert_eq!(result, serde_json::json!(["A", "B"]));
    }

    /// Regression test for #147: a TypeError in one script must not poison
    /// the runtime so that subsequent scripts (or DOM queries) collapse to
    /// empty. The reporter saw `--dump text` return 1 byte after offside.js
    /// crashed; that cascade should never happen.
    #[test]
    fn script_typeerror_does_not_poison_subsequent_execution() {
        let mut rt = setup_runtime(
            "<html><body><p id=hit>BODY_TEXT</p></body></html>",
        );

        // 1. First script throws the same flavor of error offside.js produced
        //    (`Cannot read properties of undefined (reading 'classList')`).
        let err = rt
            .execute_script("buggy", "var x; x.classList.add('y');")
            .unwrap_err();
        assert!(err.contains("classList") || err.contains("undefined"),
                "expected classList/undefined error, got: {}", err);

        // 2. The runtime must still be usable: a follow-up script runs.
        rt.execute_script("ok", "globalThis.__after_error = 'still alive';")
            .unwrap();
        let result = rt.evaluate("globalThis.__after_error").unwrap();
        assert_eq!(result, serde_json::json!("still alive"));

        // 3. DOM queries still work after the script error.
        let text = rt
            .evaluate("document.querySelector('#hit').textContent")
            .unwrap();
        assert_eq!(text, serde_json::json!("BODY_TEXT"));
    }

    /// Regression for #105: `element.querySelector` and `querySelectorAll`
    /// must scope to the receiver's subtree, not the whole document.
    #[test]
    fn element_query_selector_is_scoped_to_subtree() {
        let mut rt = setup_runtime(
            r#"<div id="a"><span class="x">in a</span></div><div id="b"><span class="x">in b</span></div>"#,
        );
        let text = rt
            .evaluate("document.getElementById('a').querySelector('.x').textContent")
            .unwrap();
        assert_eq!(text, serde_json::json!("in a"));

        let count_in_a = rt
            .evaluate("document.getElementById('a').querySelectorAll('.x').length")
            .unwrap();
        assert_eq!(count_in_a.as_f64().unwrap() as i64, 1);

        // Document-scoped query still sees both.
        let count_doc = rt.evaluate("document.querySelectorAll('.x').length").unwrap();
        assert_eq!(count_doc.as_f64().unwrap() as i64, 2);
    }

    /// `:scope` must bind to the element a query is rooted at — the archify
    /// viewer's Intent Trace does `container.querySelector(':scope > svg')`
    /// and used to get null back, killing its IIFU with a TypeError.
    #[test]
    fn scope_pseudo_class_binds_to_query_root() {
        let mut rt = setup_runtime(
            r#"<div id="a"><svg id="direct"></svg><div><svg id="deep"></svg></div></div>
               <div id="b"><svg id="other"></svg></div>"#,
        );
        let direct = rt
            .evaluate(
                "document.getElementById('a').querySelector(':scope > svg') ? 'yes' : 'no'",
            )
            .unwrap();
        assert_eq!(direct, serde_json::json!("yes"));

        let deep_excluded = rt
            .evaluate("document.getElementById('a').querySelectorAll(':scope > svg').length")
            .unwrap();
        assert_eq!(deep_excluded.as_f64().unwrap() as i64, 1);

        // :scope in matches() is the element itself — the feature-detect idiom.
        let self_match = rt
            .evaluate("document.getElementById('a').matches(':scope')")
            .unwrap();
        assert_eq!(self_match, serde_json::json!(true));
        let class_match = rt
            .evaluate("document.getElementById('a').matches(':scope#a')")
            .unwrap();
        assert_eq!(class_match, serde_json::json!(true));

        // Document-rooted :scope means the document element (html).
        let doc_scope = rt
            .evaluate("document.querySelector(':scope') === document.documentElement")
            .unwrap();
        assert_eq!(doc_scope, serde_json::json!(true));
    }

    /// `svg.viewBox` must reflect as an animated rect on the fit-to-viewbox
    /// SVG elements — the archify viewer's export path reads
    /// `svg.viewBox.baseVal.width` directly, so an undefined `viewBox`
    /// crashes one property later (found by the artifact viewer smoke probe).
    #[test]
    fn svg_viewbox_reflects_animated_rect() {
        let mut rt = setup_runtime(
            r#"<svg id="a" viewBox="0 0 880 588"></svg><svg id="b"></svg><div id="h"></div>"#,
        );
        let w = rt
            .evaluate("document.getElementById('a').viewBox.baseVal.width")
            .unwrap();
        assert_eq!(w.as_f64().unwrap(), 880.0);
        let h = rt
            .evaluate("document.getElementById('a').viewBox.animVal.height")
            .unwrap();
        assert_eq!(h.as_f64().unwrap(), 588.0);

        // Missing attribute: still an animated rect, all zeros (Chrome shape).
        let absent = rt
            .evaluate("var vb = document.getElementById('b').viewBox.baseVal; vb.x + ',' + vb.y + ',' + vb.width + ',' + vb.height")
            .unwrap();
        assert_eq!(absent, serde_json::json!("0,0,0,0"));

        // Malformed attribute parses as absent, not as garbage.
        let bad = rt
            .evaluate("document.getElementById('b').setAttribute('viewBox','10 20 100'); document.getElementById('b').viewBox.baseVal.width")
            .unwrap();
        assert_eq!(bad.as_f64().unwrap(), 0.0);
        let good = rt
            .evaluate("document.getElementById('b').setAttribute('viewBox','10 20 100 50'); var v = document.getElementById('b').viewBox.baseVal; v.x + ',' + v.width")
            .unwrap();
        assert_eq!(good, serde_json::json!("10,100"));

        // HTML elements keep no viewBox (Chrome exposes it only on the
        // fit-to-viewbox SVG tags).
        let html = rt
            .evaluate("document.getElementById('h').viewBox === undefined")
            .unwrap();
        assert_eq!(html, serde_json::json!(true));
    }

    /// Regression for #105: `document.forms` / `images` / `links` must be
    /// live, not hardcoded `[]`. jQuery 1.x's submit-event setup iterates
    /// `document.forms` and crashes when it's empty for pages that have forms.
    #[test]
    fn document_forms_images_links_are_live() {
        let mut rt = setup_runtime(
            r#"<form></form><form></form><img><a href="x">l</a><a>no-href</a>"#,
        );
        assert_eq!(rt.evaluate("document.forms.length").unwrap().as_f64().unwrap() as i64, 2);
        assert_eq!(rt.evaluate("document.images.length").unwrap().as_f64().unwrap() as i64, 1);
        assert_eq!(rt.evaluate("document.links.length").unwrap().as_f64().unwrap() as i64, 1);
    }

    /// obscura #930 family: slot assignment APIs exist. A slot OUTSIDE any
    /// shadow tree has no assignment and no fallback — the spec-correct
    /// answer is empty, which keeps feature-detect code on its slot-aware
    /// path instead of crashing on `undefined`.
    #[test]
    fn slot_assignment_apis_are_present_and_empty() {
        let mut rt = setup_runtime(r#"<slot id="s"><span id="c"></span></slot>"#);
        let assigned = rt
            .evaluate(
                "const s = document.getElementById('s'); \
                 [typeof s.assignedElements, s.assignedElements().length, \
                  typeof s.assignedNodes, s.assignedNodes().length, \
                  JSON.stringify(document.getElementById('c').assignedSlots)].join('|')",
            )
            .unwrap();
        assert_eq!(assigned, serde_json::json!("function|0|function|0|[]"));
        let is_slot = rt
            .evaluate("document.getElementById('s') instanceof HTMLSlotElement")
            .unwrap();
        assert_eq!(is_slot, serde_json::json!(true));
    }

    /// Native shadow trees: attachShadow registers a real arena root with
    /// its own child list, so the ordinary nid-based Node methods operate on
    /// the shadow tree — while the light tree, document queries, and shadow
    /// queries each stay inside their own scope.
    #[test]
    fn attach_shadow_builds_a_real_scoped_tree() {
        let mut rt = setup_runtime(r#"<div id="host"><span id="light">L</span></div>"#);
        let out = rt
            .evaluate(
                "const host = document.getElementById('host'); \
                 const sr = host.attachShadow({ mode: 'open' }); \
                 sr.innerHTML = '<p id=\"shadow-p\">S</p>'; \
                 [ \
                   sr instanceof ShadowRoot, \
                   sr instanceof DocumentFragment, \
                   sr.nodeType, \
                   sr.nodeName, \
                   sr.host === host, \
                   host.shadowRoot === sr, \
                   sr.querySelector('#shadow-p').textContent, \
                   document.querySelector('#shadow-p') === null, \
                   document.getElementById('shadow-p') === null, \
                   host.querySelector('#light') !== null, \
                   sr.querySelector('#light') === null, \
                   sr.firstChild.nodeType, \
                   sr.textContent \
                 ].join('|')",
            )
            .unwrap();
        assert_eq!(
            out,
            serde_json::json!("true|true|11|#document-fragment|true|true|S|true|true|true|true|1|S")
        );
    }

    /// Shadow-root identity contract: closed mode hides the root from the
    /// host property, getRootNode answers per scope (default stops at the
    /// ShadowRoot, composed crosses the host edge), connectivity follows the
    /// host, re-attachment throws, and there is no public constructor.
    #[test]
    fn shadow_root_identity_get_root_and_connectivity() {
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let out = rt
            .evaluate(
                "const host = document.getElementById('host'); \
                 const sr = host.attachShadow({ mode: 'closed' }); \
                 sr.innerHTML = '<span id=\"in-shadow\"></span>'; \
                 const inner = sr.querySelector('#in-shadow'); \
                 const results = [ \
                   host.shadowRoot === null, \
                   inner.getRootNode() === sr, \
                   inner.getRootNode({ composed: true }) === document, \
                   inner.isConnected === true, \
                   sr.isConnected === true \
                 ]; \
                 try { host.attachShadow({ mode: 'open' }); results.push(false); } \
                 catch (e) { results.push(e.name === 'NotSupportedError'); } \
                 try { new ShadowRoot(); results.push(false); } \
                 catch (e) { results.push(e instanceof TypeError); } \
                 results.join('|')",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("true|true|true|true|true|true|true"));
    }

    /// The DocumentFragment-unwrap paths (appendChild/insertBefore/
    /// replaceChild) splice a fragment's children into the parent — a
    /// ShadowRoot shares nodeType/nodeName with a fragment and must NOT be
    /// unwrapped, or appending a root would destructively empty the shadow
    /// tree. Detached hosts: content is connected exactly when the host is.
    #[test]
    fn shadow_content_is_never_unwrapped_and_follows_host_connectivity() {
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let out = rt
            .evaluate(
                "const host = document.getElementById('host'); \
                 const sr = host.attachShadow({ mode: 'open' }); \
                 sr.innerHTML = '<b>one</b>'; \
                 document.body.appendChild(sr); \
                 const guardHeld = sr.innerHTML === '<b>one</b>' \
                   && document.body.querySelector('b') === null; \
                 const detached = document.createElement('div'); \
                 const sr2 = detached.attachShadow({ mode: 'open' }); \
                 sr2.innerHTML = '<i>x</i>'; \
                 const inner = sr2.querySelector('i'); \
                 const whileDetached = inner.isConnected === false; \
                 document.body.appendChild(detached); \
                 [guardHeld, whileDetached, inner.isConnected === true].join('|')",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("true|true|true"));
    }

    /// Slot assignment real values over a native shadow tree: exact name
    /// matching (slot attr ↔ slot name), first unnamed slot absorbs
    /// nameless light children, an unreferenced name leaves assignedSlots
    /// empty, and an unassigned slot serves its fallback children.
    #[test]
    fn slot_assignment_serves_light_children_through_the_shadow() {
        let mut rt = setup_runtime(
            r#"<my-host id="h"><span slot="a" id="sa">A</span><span id="anon">B</span><span slot="nobody" id="nb"></span></my-host>"#,
        );
        let out = rt
            .evaluate(
                "const host = document.getElementById('h'); \
                 const sr = host.attachShadow({ mode: 'open' }); \
                 sr.innerHTML = '<slot name=\"a\"></slot><slot id=\"def\"></slot><slot name=\"ghost\">fallback</slot>'; \
                 const named = sr.querySelector('slot[name=\"a\"]'); \
                 const def = sr.getElementById('def'); \
                 const ghost = sr.querySelector('slot[name=\"ghost\"]'); \
                 const sa = document.getElementById('sa'); \
                 const anon = document.getElementById('anon'); \
                 const nb = document.getElementById('nb'); \
                 [ \
                   named.assignedNodes().length, \
                   named.assignedNodes()[0] === sa, \
                   named.assignedElements().length, \
                   JSON.stringify(sa.assignedSlots.map(s => s.getAttribute('name'))), \
                   JSON.stringify(nb.assignedSlots), \
                   def.assignedNodes().length, \
                   def.assignedNodes()[0] === anon, \
                   def.assignedElements().length, \
                   ghost.assignedNodes().length, \
                   ghost.assignedNodes()[0].textContent, \
                   ghost.assignedElements().length \
                 ].join('|')",
            )
            .unwrap();
        assert_eq!(
            out,
            serde_json::json!("1|true|1|[\"a\"]|[]|1|true|1|1|fallback|0")
        );
    }

    /// Shadow layout phase 2, composition: a shadow host's box contains its
    /// shadow tree, not its light children — gBCR on shadow content reports
    /// real composed-tree geometry, while unassigned light children report
    /// nothing (no box, per spec).
    #[cfg(feature = "screenshot")]
    #[test]
    fn shadow_content_renders_unassigned_light_children_do_not() {
        let mut rt = setup_runtime(r#"<div id="host"><span id="orphan">hidden</span></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const sr = host.attachShadow({ mode: 'open' });
            sr.innerHTML = '<div id="in-shadow" style="width:60px;height:30px"></div>';
            const inner = sr.getElementById('in-shadow');
            const r = inner.getBoundingClientRect();
            const orphan = document.getElementById('orphan').getBoundingClientRect();
            const hostR = host.getBoundingClientRect();
            return [r.width, r.height, orphan.width, orphan.height,
                    hostR.width >= r.width - 0.5, hostR.height >= r.height - 0.5];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!(60), "shadow box has real width");
        assert_eq!(parts[1], serde_json::json!(30), "shadow box has real height");
        assert_eq!(
            parts[2],
            serde_json::json!(0),
            "unassigned light child renders nothing"
        );
        assert_eq!(parts[3], serde_json::json!(0), "no height either");
        assert_eq!(parts[4], serde_json::json!(true), "host wraps the shadow box");
        assert_eq!(parts[5], serde_json::json!(true), "host height tracks shadow content");
    }

    /// Slot composition: slotted light children render AT their slot's
    /// position inside the shadow tree, and a slot with no assignment serves
    /// its fallback children in place.
    #[cfg(feature = "screenshot")]
    #[test]
    fn slotted_light_children_render_at_the_slot_position() {
        let mut rt = setup_runtime(
            r#"<my-el id="h"><b slot="t" id="slotted" style="display:block;width:44px;height:11px">S</b></my-el>"#,
        );
        let result = rt.evaluate(r#"
            const host = document.getElementById('h');
            const sr = host.attachShadow({ mode: 'open' });
            sr.innerHTML = '<div style="height:10px" id="pad"></div><slot name="t" id="sl"></slot><slot name="none" id="fb"><i id="fi" style="display:block;width:22px;height:7px"></i></slot>';
            const slotted = document.getElementById('slotted').getBoundingClientRect();
            const pad = sr.getElementById('pad').getBoundingClientRect();
            const fb = sr.getElementById('fi').getBoundingClientRect();
            const slotBox = sr.getElementById('sl').getBoundingClientRect();
            return [slotted.width, slotted.height,
                    Math.abs(slotted.y - (pad.y + pad.height)) < 0.5,
                    fb.width, slotBox.width];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!(44), "slotted child keeps its own size");
        assert_eq!(parts[1], serde_json::json!(11), "slotted child height");
        assert_eq!(
            parts[2],
            serde_json::json!(true),
            "slotted child sits at the slot's position (after the pad div)"
        );
        assert_eq!(parts[3], serde_json::json!(22), "fallback child renders in the empty slot");
        assert_eq!(parts[4], serde_json::json!(0), "the slot element itself never boxes");
    }

    /// Flat-tree inheritance through the slot, pinned to local Chrome
    /// (2026-09-16, headless 152): inherited properties reach a slotted
    /// light child FROM THE SLOT — `color` declared on the slot colors the
    /// slotted span, and with it undeclared the host's value walks through.
    /// (The classic "slotted content only takes ::slotted rules" gotcha is
    /// about selector matching crossing the boundary, not inheritance.)
    #[cfg(feature = "screenshot")]
    #[test]
    fn slotted_content_inherits_through_the_slot() {
        let mut rt = setup_runtime(
            r#"<div id="outer" style="color:rgb(0,0,255)"><my-el id="h1"><span id="s1">a</span></my-el><my-el id="h2"><span id="s2">b</span></my-el></div>"#,
        );
        let result = rt.evaluate(r#"
            const sr1 = document.getElementById('h1').attachShadow({ mode: 'open' });
            sr1.innerHTML = '<slot style="color:rgb(255,0,0)"></slot>';
            const sr2 = document.getElementById('h2').attachShadow({ mode: 'open' });
            sr2.innerHTML = '<slot></slot>';
            return [getComputedStyle(document.getElementById('s1')).color,
                    getComputedStyle(sr1.querySelector('slot')).color,
                    getComputedStyle(document.getElementById('s2')).color,
                    getComputedStyle(sr2.querySelector('slot')).color];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!("rgb(255, 0, 0)"),
            "color declared on the slot inherits into the slotted light child");
        assert_eq!(parts[1], serde_json::json!("rgb(255, 0, 0)"),
            "the slot itself computes its inline color (not a JS-side fallback)");
        assert_eq!(parts[2], serde_json::json!("rgb(0, 0, 255)"),
            "with the slot undeclared, blue walks outer -> host -> slot -> slotted");
        assert_eq!(parts[3], serde_json::json!("rgb(0, 0, 255)"),
            "shadow content itself inherits from the host");
    }

    /// Shadow `<style>` joins the CSS pool and styles shadow content; rules
    /// match by class across the tree scopes (global-pool approximation).
    #[cfg(feature = "screenshot")]
    #[test]
    fn shadow_style_sheets_style_shadow_content() {
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const sr = host.attachShadow({ mode: 'open' });
            sr.innerHTML = '<style>.card { width: 120px; height: 55px; }</style><div class="card" id="c"></div>';
            const c = sr.getElementById('c');
            const r = c.getBoundingClientRect();
            const cs = getComputedStyle(c);
            return [r.width, r.height, cs.width];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!(120), "shadow style sizes the card");
        assert_eq!(parts[1], serde_json::json!(55), "height from the shadow sheet");
        assert_eq!(parts[2], serde_json::json!("120px"), "getComputedStyle sees the shadow rule");
    }

    /// getComputedStyle overflow face pinned to the Chrome probe matrix
    /// (css-overflow-3): the pair form reads as "x y", single keywords stay
    /// single, and the §3.1 coercion surfaces through overflowY ("auto").
    #[cfg(feature = "screenshot")]
    #[test]
    fn computed_style_overflow_per_axis_matrix() {
        let mut rt = setup_runtime(
            r#"<body><div id="a" style="overflow: clip"></div><div id="b" style="overflow: hidden auto"></div><div id="c" style="overflow-x: hidden"></div><div id="d"></div></body>"#,
        );
        let result = rt.evaluate(r#"
            const g = id => getComputedStyle(document.getElementById(id));
            return {
                a: g('a').overflow, aX: g('a').overflowX,
                b: g('b').overflow, bX: g('b').overflowX, bY: g('b').overflowY,
                c: g('c').overflow, cX: g('c').overflowX, cY: g('c').overflowY,
                d: g('d').overflow,
            };
        "#).unwrap();
        let v = result;
        assert_eq!(v["a"], serde_json::json!("clip"), "single keyword serializes alone");
        assert_eq!(v["aX"], serde_json::json!("clip"));
        assert_eq!(v["b"], serde_json::json!("hidden auto"), "pair form reads back as pair");
        assert_eq!(v["bX"], serde_json::json!("hidden"));
        assert_eq!(v["bY"], serde_json::json!("auto"));
        assert_eq!(v["c"], serde_json::json!("hidden auto"), "§3.1: visible axis coerces to auto");
        assert_eq!(v["cX"], serde_json::json!("hidden"));
        assert_eq!(v["cY"], serde_json::json!("auto"), "Chrome reports the coerced pair");
        assert_eq!(v["d"], serde_json::json!("visible"), "default stays visible");
    }

    /// Table DOM API family (WHATWG §4.9): script-built tables — insertRow
    /// auto-creates the tbody, `rows` reads in spec order (thead, tbodies,
    /// tfoot — not tree order), and the per-row/cell index getters report
    /// position (Sina's TabSwitchController trips exactly these).
    #[test]
    fn table_dom_api_family() {
        let mut rt = setup_runtime(r#"<body><table id="t"></table></body>"#);
        let result = rt.evaluate(r#"
            const t = document.getElementById('t');
            const head = t.createTHead();
            const hr = head.insertRow(); hr.insertCell().textContent = 'h';
            const r0 = t.insertRow(); r0.insertCell().textContent = 'a';
            const r1 = t.insertRow(); r1.insertCell().textContent = 'b';
            const fr = t.createTFoot().insertRow(); fr.insertCell().textContent = 'f';
            const rows = Array.from(t.rows).map((r) => r.cells[0].textContent);
            const rowIndexes = Array.from(t.rows).map((r) => r.rowIndex);
            const sectionRowIndexes = [r0.sectionRowIndex, r1.sectionRowIndex];
            const cellIndex = r1.cells[0].cellIndex;
            const tbodies = t.tBodies.length;
            const caption = t.createCaption().localName;
            const mid = t.insertRow(0);
            let threw = null;
            try { t.insertRow(99); } catch (err) { threw = err.name; }
            t.deleteRow(-1);
            const blank = document.createElement('table');
            const auto = blank.insertRow();
            return {
                rows, rowIndexes, sectionRowIndexes, cellIndex, tbodies, caption,
                midInSection: mid.parentNode.localName,
                midCells: mid.cells.length,
                threw,
                afterDelete: t.rows.length,
                autoBody: [blank.tBodies.length, auto.parentNode.localName],
            };
        "#).unwrap();
        let v = result;
        assert_eq!(
            v["rows"],
            serde_json::json!(["h", "a", "b", "f"]),
            "spec order: thead rows, then tbodies, then tfoot — not tree order"
        );
        assert_eq!(v["rowIndexes"], serde_json::json!([0, 1, 2, 3]));
        assert_eq!(v["sectionRowIndexes"], serde_json::json!([0, 1]));
        assert_eq!(v["cellIndex"], serde_json::json!(0));
        assert_eq!(v["tbodies"], serde_json::json!(1));
        assert_eq!(v["caption"], serde_json::json!("caption"));
        assert_eq!(
            v["midInSection"],
            serde_json::json!("thead"),
            "insertRow(0) targets the section OWNING rows[0] (thead's row), never the table directly"
        );
        assert_eq!(v["midCells"], serde_json::json!(0), "insertRow creates bare rows");
        assert_eq!(v["threw"], serde_json::json!("IndexSizeError"), "out-of-range index throws");
        assert_eq!(
            v["afterDelete"],
            serde_json::json!(4),
            "deleteRow(-1) drops the table-order last row (tfoot's), the bare thead row stays"
        );
        assert_eq!(v["autoBody"], serde_json::json!([1, "tbody"]), "empty-table insertRow synthesizes a tbody");
    }

    /// Select surface residuals (obscura#991's list): selectedOptions /
    /// multiple / size, and the per-tag interface discriminators the table
    /// family relies on.
    #[test]
    fn select_options_surface_and_table_interfaces() {
        let mut rt = setup_runtime(
            r#"<body><select id="s" multiple size="3"><option value="x">x</option><option value="y" selected>y</option></select></body>"#,
        );
        let result = rt.evaluate(r#"
            const s = document.getElementById('s');
            s.options.add(new Option('z', 'z'));
            return {
                optionsLen: s.options.length,
                selected: Array.from(s.selectedOptions).map((o) => o.value),
                multiple: s.multiple,
                size: s.size,
                optgroup: document.createElement('optgroup') instanceof HTMLOptGroupElement,
                row: document.createElement('tr') instanceof HTMLTableRowElement,
                cell: document.createElement('th') instanceof HTMLTableCellElement,
                optCtor: s.options[2] instanceof HTMLOptionElement,
            };
        "#).unwrap();
        let v = result;
        assert_eq!(v["optionsLen"], serde_json::json!(3), "new Option + options.add");
        assert_eq!(v["selected"], serde_json::json!(["y"]));
        assert_eq!(v["multiple"], serde_json::json!(true));
        assert_eq!(v["size"], serde_json::json!(3));
        assert_eq!(v["optgroup"], serde_json::json!(true));
        assert_eq!(v["row"], serde_json::json!(true));
        assert_eq!(v["cell"], serde_json::json!(true));
        assert_eq!(v["optCtor"], serde_json::json!(true));
    }

    /// Containing-block resolution for out-of-flow boxes (blitz#764 family):
    /// an absolute box reparents to the nearest POSITIONED ancestor's padding
    /// box (not its DOM parent's flow position), a transformed ancestor is a
    /// containing block for BOTH absolute and fixed descendants (CSS
    /// Transforms — positioned ancestors don't pin fixed), and fixed with no
    /// such ancestor stays viewport-anchored on the ICB.
    #[cfg(feature = "screenshot")]
    #[test]
    fn abspos_containing_block_resolution() {
        let mut rt = setup_runtime(
            r#"<body style="margin:8px">
              <div id="outer" style="position:relative">
                <div id="mid" style="margin-left:100px">
                  <div id="a" style="position:absolute; left:0; top:0">X</div>
                </div>
              </div>
              <div id="tf" style="transform:translateX(50px)">
                <div id="f" style="position:fixed; left:0; top:0">F</div>
              </div>
              <div id="tg" style="transform:translateX(30px)">
                <div><div id="a2" style="position:absolute; left:0; top:0">A2</div></div>
              </div>
              <div id="pos" style="position:relative; margin-left:200px">
                <div id="tr2" style="transform:translateX(10px)">
                  <div id="f3" style="position:fixed; left:0; top:0">F3</div>
                </div>
              </div>
              <div id="f2" style="position:fixed; left:0; top:0">F2</div>
              <div id="outer2" style="position:relative; padding:20px">
                <div style="height:10px"></div>
                <div id="b" style="position:absolute">B</div>
              </div>
            </body>"#,
        );
        let result = rt.evaluate(r#"
            const g = (id) => {
                const b = document.getElementById(id).getBoundingClientRect();
                return [Math.round(b.x), Math.round(b.y)];
            };
            return {
                a: g('a'),
                f: g('f'),
                a2: g('a2'),
                f3: g('f3'),
                f2: g('f2'),
                b: g('b'),
            };
        "#).unwrap();
        let v = result;
        assert_eq!(
            v["a"], serde_json::json!([8, 8]),
            "absolute anchors to the positioned ancestor's padding box, not the DOM parent's flow spot"
        );
        assert_eq!(
            v["f"], serde_json::json!([58, 8]),
            "fixed under a transformed ancestor anchors to that ancestor (8 + translateX(50))"
        );
        assert_eq!(
            v["a2"], serde_json::json!([38, 8]),
            "a transformed (non-positioned) ancestor is the absolute containing block"
        );
        assert_eq!(
            v["f3"], serde_json::json!([218, 8]),
            "fixed skips the positioned ancestor but still pins to the farther transformed one"
        );
        assert_eq!(
            v["f2"], serde_json::json!([0, 0]),
            "fixed with no transformed ancestor stays viewport-anchored (ICB)"
        );
        assert_eq!(
            v["b"], serde_json::json!([28, 38]),
            "auto insets resolve at the static position (body margin + padding + sibling)"
        );
    }

    /// Float + percentage width + margin (obscura #757 family): a float's
    /// percentage width must survive adding a margin (percent or px) — the
    /// combination is the Bootstrap 3 offset grid. Chrome keeps every column
    /// at 83.333% of the 1600px row regardless of the margin.
    #[cfg(feature = "screenshot")]
    #[test]
    fn float_percentage_width_survives_margin() {
        let mut rt = setup_runtime(
            r#"<style>
              body{margin:0}
              .row{width:1600px}
              .inner{width:370px}
            </style>
            <div class="row"><div id="c1" style="float:left;width:83.33333333%"><div class="inner">1</div></div></div>
            <div class="row"><div id="c2" style="float:left;width:83.33333333%;margin-left:8.33333333%"><div class="inner">2</div></div></div>
            <div class="row"><div id="c3" style="float:left;width:83.33333333%;margin-left:133px"><div class="inner">3</div></div></div>
            <div class="row"><div id="c4" style="width:83.33333333%;margin-left:8.33333333%"><div class="inner">4</div></div></div>"#,
        );
        let result = rt.evaluate(r#"
            const g = (id) => {
                const b = document.getElementById(id).getBoundingClientRect();
                return [Math.round(b.x), Math.round(b.width)];
            };
            return { c1: g('c1'), c2: g('c2'), c3: g('c3'), c4: g('c4') };
        "#).unwrap();
        let v = result;
        // Widths wobble 1333/1334 with the integral-coordinate posture
        // (obscura #576): gBCR width is round(right) - round(left), so a
        // 1333.33px column shifts by 1 depending on its fractional x. The
        // property under test is NO collapse — upstream #757 dropped the
        // column to 420/873 the moment a margin appeared.
        assert_eq!(
            v["c1"], serde_json::json!([0, 1333]),
            "bare float percentage width resolves against the row"
        );
        assert_eq!(
            v["c2"], serde_json::json!([133, 1334]),
            "percentage margin must not collapse the float's percentage width"
        );
        assert_eq!(
            v["c3"], serde_json::json!([133, 1333]),
            "px margin must not collapse the float's percentage width either"
        );
        assert_eq!(
            v["c4"], serde_json::json!([133, 1334]),
            "in-flow control: percentage width + percentage margin without float"
        );
    }

    /// Regression for #105: `HTMLFormElement` must expose `.elements` so
    /// frameworks that probe form field collections work.
    #[test]
    fn html_form_element_exposes_elements_collection() {
        let mut rt = setup_runtime(
            r#"<form id="f"><input name=a><input name=b><textarea></textarea></form>"#,
        );
        let n = rt
            .evaluate("document.getElementById('f').elements.length")
            .unwrap();
        assert_eq!(n.as_f64().unwrap() as i64, 3);
        let is_form = rt
            .evaluate("document.getElementById('f') instanceof HTMLFormElement")
            .unwrap();
        assert_eq!(is_form, serde_json::json!(true));
    }

    /// Regression for #222: HTML interface globals must discriminate by tag.
    /// The old `= Element` aliases made `head instanceof HTMLIFrameElement`
    /// true for every element, so webpack style-loader handed the head
    /// element to `contentDocument.head` and bilibili's player core threw
    /// "Couldn't find a style target" before the player could mount.
    #[test]
    fn html_interface_instanceof_discriminates_by_tag() {
        let mut rt = setup_runtime(
            r#"<head id="h"></head><body><div id="d"></div><iframe id="i"></iframe><h2 id="t"></h2><form id="f"></form></body>"#,
        );
        let checks: &[(&str, bool)] = &[
            ("document.getElementById('d') instanceof HTMLDivElement", true),
            ("document.getElementById('d') instanceof HTMLElement", true),
            ("document.getElementById('d') instanceof HTMLIFrameElement", false),
            ("document.getElementById('i') instanceof HTMLIFrameElement", true),
            ("document.getElementById('i') instanceof HTMLDivElement", false),
            ("document.getElementById('h') instanceof HTMLIFrameElement", false),
            ("document.getElementById('h') instanceof HTMLHeadElement", true),
            ("document.getElementById('t') instanceof HTMLHeadingElement", true),
            ("document.getElementById('t') instanceof HTMLDivElement", false),
            ("document.getElementById('f') instanceof HTMLFormElement", true),
            ("document.getElementById('f') instanceof HTMLDivElement", false),
            ("'x' instanceof HTMLIFrameElement", false),
            // #27: each interface owns its prototype now (they used to share
            // Element.prototype, which made constructor.name read "Element"
            // for every tag and let per-interface patches leak everywhere).
            ("HTMLDivElement.prototype === Element.prototype", false),
            ("document.getElementById('d').constructor === HTMLDivElement", true),
        ];
        for (expr, expected) in checks {
            let got = rt.evaluate(expr).unwrap();
            assert_eq!(got, serde_json::json!(expected), "expr: {expr}");
        }
        let ctor = rt
            .evaluate("(() => { try { new HTMLDivElement(); } catch (e) { return e.name; } return 'no-throw'; })()")
            .unwrap();
        assert_eq!(ctor, serde_json::json!("TypeError"));
    }

    /// #27: the interface prototype chain must read like Chrome's —
    /// HTMLDivElement → HTMLElement → Element → Node → EventTarget — with
    /// per-interface prototypes that actually isolate patches, and
    /// constructor.name per tag (unknown tags report HTMLElement).
    #[test]
    fn interface_prototype_chain_and_patch_isolation() {
        let mut rt = setup_runtime(
            r#"<body><div id="d"></div><span id="s"></span><x-foo id="x"></x-foo></body>"#,
        );
        let checks: &[(&str, bool)] = &[
            ("document.getElementById('d') instanceof HTMLElement", true),
            ("document.getElementById('d') instanceof Element", true),
            ("document.getElementById('d') instanceof Node", true),
            ("document.getElementById('d') instanceof EventTarget", true),
            ("Object.getPrototypeOf(HTMLElement.prototype) === Element.prototype", true),
            ("Object.getPrototypeOf(Element.prototype) === Node.prototype", true),
            ("Object.getPrototypeOf(Node.prototype) === EventTarget.prototype", true),
            ("document instanceof EventTarget", true),
            ("EventTarget.prototype !== Node.prototype", true),
        ];
        for (expr, expected) in checks {
            let got = rt.evaluate(expr).unwrap();
            assert_eq!(got, serde_json::json!(expected), "expr: {expr}");
        }
        // constructor.name per interface; unknown tag falls back to HTMLElement.
        let names = rt
            .evaluate(
                r#"JSON.stringify([
                    document.getElementById('d').constructor.name,
                    document.getElementById('s').constructor.name,
                    document.getElementById('x').constructor.name,
                ])"#,
            )
            .unwrap();
        assert_eq!(
            names,
            serde_json::json!(r#"["HTMLDivElement","HTMLSpanElement","HTMLElement"]"#)
        );
        // A patch on one interface's prototype reaches its own tags only.
        let isolation = rt
            .evaluate(
                r#"(() => {
                    HTMLDivElement.prototype._probe = 'div';
                    Element.prototype._probe2 = 'all';
                    const d = document.getElementById('d');
                    const s = document.getElementById('s');
                    return JSON.stringify([d._probe, s._probe === undefined, d._probe2, s._probe2]);
                })()"#,
            )
            .unwrap();
        assert_eq!(
            isolation,
            serde_json::json!(r#"["div",true,"all","all"]"#)
        );
    }

    /// #27 (obscura#999 face): Web IDL operations are enumerable — zone.js
    /// patchClass() discovers methods with `for (prop in instance)` and only
    /// walks enumerable properties, so non-enumerable class methods made
    /// Angular die with "n.observe is not a function".
    #[test]
    fn interface_operations_are_enumerable() {
        let mut rt = setup_runtime(r#"<body><div id="d"></div></body>"#);
        let checks: &[(&str, bool)] = &[
            (
                "Object.getOwnPropertyDescriptor(MutationObserver.prototype, 'observe').enumerable",
                true,
            ),
            (
                "Object.getOwnPropertyDescriptor(Element.prototype, 'getAttribute').enumerable",
                true,
            ),
            (
                "Object.getOwnPropertyDescriptor(Node.prototype, 'appendChild').enumerable",
                true,
            ),
            // The zone.js patchClass discovery pattern: for-in over an
            // instance must surface the interface operations as functions.
            (
                "(() => { const mo = new MutationObserver(function(){}); let saw = 0; \
                  for (const k in mo) { if (k === 'observe' && typeof mo[k] === 'function') saw++; } \
                  return saw === 1; })()",
                true,
            ),
            // window.constructor.prototype members (WindowProxy face) stay out
            // of scope: Object.prototype built-ins must NOT become enumerable.
            (
                "Object.getOwnPropertyDescriptor(Object.prototype, 'hasOwnProperty').enumerable",
                false,
            ),
        ];
        for (expr, expected) in checks {
            let got = rt.evaluate(expr).unwrap();
            assert_eq!(got, serde_json::json!(expected), "expr: {expr}");
        }
    }

    /// #39 (obscura#999 residue): WebKitMutationObserver is an alias of the
    /// same constructor (zone.js patches both names; Chrome 151 face), and
    /// IntersectionObserver's prototype carries scrollMargin/delay/
    /// trackVisibility alongside root/rootMargin/thresholds — the sorted
    /// 10-name Chrome-parity face, getters enumerable like every other
    /// interface member.
    #[test]
    fn webkit_mutation_observer_alias_and_io_attribute_face() {
        let mut rt = setup_runtime(r#"<body><div id="d"></div></body>"#);
        let out = rt
            .evaluate(
                r#"(() => {
                    const ioKeys = Object.keys(IntersectionObserver.prototype).sort();
                    const io = new IntersectionObserver(function(){});
                    let sawScrollMargin = false;
                    for (const k in io) { if (k === 'scrollMargin') sawScrollMargin = true; }
                    return JSON.stringify([
                        typeof WebKitMutationObserver,
                        WebKitMutationObserver === MutationObserver,
                        ioKeys,
                        io.delay, io.trackVisibility, io.scrollMargin,
                        sawScrollMargin,
                    ]);
                })()"#,
            )
            .unwrap();
        assert_eq!(
            out,
            serde_json::json!(
                r#"["function",true,["delay","disconnect","observe","root","rootMargin","scrollMargin","takeRecords","thresholds","trackVisibility","unobserve"],0,false,"0px 0px 0px 0px",true]"#
            )
        );
    }

    /// #40: navigator.platform under a Windows UA must be "Win32" (real
    /// Chrome reports Win32 on 64-bit Windows and 64-bit Chrome alike) while
    /// userAgentData.platform says "Windows" — the two faces must never
    /// collide. Linux personas (obscura#987 same face) must derive
    /// "Linux x86_64" so a Linux deployment's JS face agrees with its
    /// kernel's SYN fingerprint.
    #[test]
    fn navigator_platform_matches_ua_os_family() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // The production path: set_user_agent writes __diting_ua and
        // refreshes the persona; the getters derive from the UA after.
        rt.set_user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36");
        assert_eq!(
            rt.evaluate("navigator.platform").unwrap(),
            serde_json::json!("Win32")
        );
        assert_eq!(
            rt.evaluate("navigator.userAgentData.platform").unwrap(),
            serde_json::json!("Windows")
        );
        rt.set_user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36");
        assert_eq!(
            rt.evaluate("navigator.platform").unwrap(),
            serde_json::json!("Linux x86_64")
        );
        assert_eq!(
            rt.evaluate("navigator.userAgentData.platform").unwrap(),
            serde_json::json!("Linux")
        );
        rt.set_user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36");
        assert_eq!(
            rt.evaluate("navigator.platform").unwrap(),
            serde_json::json!("MacIntel")
        );
        assert_eq!(
            rt.evaluate("navigator.userAgentData.platform").unwrap(),
            serde_json::json!("macOS")
        );
    }

    /// #28: SVG elements wrap in their real interfaces (constructor.name,
    /// instanceof, patch isolation) and read the true per-node namespace from
    /// the tree — parsed SVG children inherit theirs, createElementNS keeps
    /// the XML tag case, and svg/HTML <a> discriminate by namespace.
    #[test]
    fn svg_interface_family_and_namespace() {
        let mut rt = setup_runtime(
            r#"<body><svg id="sv"><path id="pa"></path><linearGradient id="lg"></linearGradient><a id="sa"></a></svg><a id="ha" href="/h">h</a></body>"#,
        );
        let checks: &[(&str, bool)] = &[
            // Chrome chain: SVGSVGElement → SVGGraphicsElement → SVGElement
            // → HTMLElement → Element (Blink routes SVG through HTMLElement).
            ("document.getElementById('sv') instanceof SVGSVGElement", true),
            ("document.getElementById('sv') instanceof SVGGraphicsElement", true),
            ("document.getElementById('sv') instanceof SVGElement", true),
            ("document.getElementById('sv') instanceof HTMLElement", true),
            ("document.getElementById('sv') instanceof Element", true),
            ("document.getElementById('pa') instanceof SVGGeometryElement", true),
            // svg is graphics but NOT geometry; shapes are both.
            ("document.getElementById('sv') instanceof SVGGeometryElement", false),
            // Namespace discrimination: svg <a> vs HTML <a> never collide.
            ("document.getElementById('sa') instanceof SVGAElement", true),
            ("document.getElementById('sa') instanceof HTMLAnchorElement", false),
            ("document.getElementById('ha') instanceof HTMLAnchorElement", true),
            ("document.getElementById('ha') instanceof SVGAElement", false),
            // The old bare alias made EVERY node instanceof SVGSVGElement.
            ("document.body instanceof SVGSVGElement", false),
            ("SVGSVGElement !== Element", true),
            (
                "Object.getPrototypeOf(SVGSVGElement.prototype) === SVGGraphicsElement.prototype",
                true,
            ),
            (
                "Object.getPrototypeOf(SVGGraphicsElement.prototype) === SVGElement.prototype",
                true,
            ),
            (
                "Object.getPrototypeOf(SVGElement.prototype) === HTMLElement.prototype",
                true,
            ),
            // Parsed SVG children inherit the namespace from the tree; HTML
            // elements keep XHTML. Case: linearGradient keeps its camelCase.
            (
                "document.getElementById('pa').namespaceURI === 'http://www.w3.org/2000/svg'",
                true,
            ),
            (
                "document.getElementById('ha').namespaceURI === 'http://www.w3.org/1999/xhtml'",
                true,
            ),
            ("document.getElementById('lg').localName === 'linearGradient'", true),
            ("document.getElementById('lg').tagName === 'linearGradient'", true),
            // createElementNS: XML tag case preserved, namespace true.
            (
                "(() => { const c = document.createElementNS('http://www.w3.org/2000/svg', 'linearGradient'); \
                  return c.tagName === 'linearGradient' && c.namespaceURI === 'http://www.w3.org/2000/svg' \
                  && c instanceof SVGLinearGradientElement; })()",
                true,
            ),
            // Empty namespace string is the null namespace per spec.
            ("document.createElementNS('', 'foo').namespaceURI === null", true),
            // Class-body methods stay enumerable after the tail pass.
            (
                "Object.getOwnPropertyDescriptor(SVGGraphicsElement.prototype, 'getBBox').enumerable",
                true,
            ),
        ];
        for (expr, expected) in checks {
            let got = rt.evaluate(expr).unwrap();
            assert_eq!(got, serde_json::json!(expected), "expr: {expr}");
        }
        let names = rt
            .evaluate(
                r#"JSON.stringify([
                    document.getElementById('sv').constructor.name,
                    document.getElementById('pa').constructor.name,
                    document.getElementById('lg').constructor.name,
                    document.getElementById('sa').constructor.name,
                    document.getElementById('ha').constructor.name,
                ])"#,
            )
            .unwrap();
        assert_eq!(
            names,
            serde_json::json!(
                r#"["SVGSVGElement","SVGPathElement","SVGLinearGradientElement","SVGAElement","HTMLAnchorElement"]"#
            )
        );
        // A patch on SVGSVGElement's prototype reaches the svg root only.
        let isolation = rt
            .evaluate(
                r#"(() => {
                    SVGSVGElement.prototype._probe = 'svg';
                    const sv = document.getElementById('sv');
                    return JSON.stringify([sv._probe, document.body._probe === undefined]);
                })()"#,
            )
            .unwrap();
        assert_eq!(isolation, serde_json::json!(r#"["svg",true]"#));
    }

    /// Regression for #105: `Element.prepend` must actually insert at the
    /// start, not silently no-op.
    #[test]
    fn element_prepend_inserts_at_start() {
        let mut rt = setup_runtime(r#"<div id="c"><span>existing</span></div>"#);
        rt.evaluate(
            r#"
            const c = document.getElementById('c');
            const n = document.createElement('span');
            n.id = 'first';
            c.prepend(n);
            "#,
        )
        .unwrap();
        let first_id = rt.evaluate("document.getElementById('c').firstChild.id").unwrap();
        assert_eq!(first_id, serde_json::json!("first"));
        let count = rt.evaluate("document.getElementById('c').childNodes.length").unwrap();
        assert_eq!(count.as_f64().unwrap() as i64, 2);
    }

    /// Regression for #105: `isEqualNode` compares structure, not identity.
    /// Framework diff algorithms rely on this.
    #[test]
    fn is_equal_node_does_structural_compare() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                const a = document.createElement('div'); a.setAttribute('class', 'x'); a.innerHTML = '<span>hi</span>';
                const b = document.createElement('div'); b.setAttribute('class', 'x'); b.innerHTML = '<span>hi</span>';
                const c = document.createElement('div'); c.innerHTML = '<span>bye</span>';
                return [a.isEqualNode(b), a.isEqualNode(c), a.isSameNode(b)];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([true, false, false]));
    }

    /// Regression for the long-standing insert_before arg-order bug noted
    /// in CLAUDE.md: bootstrap.js was passing (parent, new, ref) but `_dom`
    /// forwards only two args, silently dropping `ref`. With the fix,
    /// `insertBefore` actually inserts.
    #[test]
    fn insert_before_inserts_node_at_correct_position() {
        let mut rt = setup_runtime(r#"<div id="p"><span id="b">b</span><span id="c">c</span></div>"#);
        let order = rt
            .evaluate(
                r#"
                const p = document.getElementById('p');
                const a = document.createElement('span');
                a.id = 'a';
                p.insertBefore(a, document.getElementById('b'));
                return Array.from(p.children).map(e => e.id).join(',');
                "#,
            )
            .unwrap();
        assert_eq!(order, serde_json::json!("a,b,c"));
    }

    #[test]
    fn test_console_log() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script("test", "console.log('Hello from V8!')").unwrap();
    }

    #[test]
    fn test_console_calls_are_queued_for_cdp() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "test",
            "console.log('hello'); console.warn('careful'); console.error('boom')",
        )
        .unwrap();
        let stripped: Vec<(String, String)> = rt
            .take_pending_console_calls()
            .into_iter()
            .map(|(level, msg, _url)| (level, msg))
            .collect();
        assert_eq!(
            stripped,
            vec![
                ("log".to_string(), "hello".to_string()),
                ("warn".to_string(), "careful".to_string()),
                ("error".to_string(), "boom".to_string()),
            ]
        );
        // take() drains: a second take sees nothing, so the CDP layer can't
        // re-emit the same console line on the next dispatch.
        assert!(rt.take_pending_console_calls().is_empty());
    }

    /// alert/confirm/prompt are auto-answered from the dialog policy and
    /// recorded as level-"dialog" console entries. Default dismisses; the
    /// accepted prompt falls back to the call's default argument when no
    /// session prompt_text is set.
    #[test]
    fn test_dialogs_answer_from_policy_and_log() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let out = rt
            .evaluate(
                "[String(alert('hi')), String(confirm('go?')), String(prompt('name?', 'anon'))].join('|')",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("undefined|false|null"), "dismiss default");
        let stripped: Vec<(String, String)> = rt
            .take_pending_console_calls()
            .into_iter()
            .map(|(level, msg, _url)| (level, msg))
            .collect();
        let parsed: Vec<serde_json::Value> = stripped
            .iter()
            .map(|(l, m)| {
                assert_eq!(l, "dialog");
                serde_json::from_str(m).unwrap()
            })
            .collect();
        assert_eq!(parsed[0]["dialog"], "alert");
        assert_eq!(parsed[0]["message"], "hi");
        assert!(parsed[0].get("answer").is_none(), "alert carries no answer");
        assert_eq!(parsed[1]["dialog"], "confirm");
        assert_eq!(parsed[1]["answer"], false);
        assert_eq!(parsed[2]["dialog"], "prompt");
        assert_eq!(parsed[2]["answer"], false);

        rt.set_dialog_policy(true, None);
        let out = rt
            .evaluate("[String(confirm('go?')), String(prompt('name?', 'anon')), String(prompt('name?'))].join('|')")
            .unwrap();
        assert_eq!(
            out,
            serde_json::json!("true|anon|"),
            "accept: prompt falls back to its default argument"
        );
        // prompt_text wins over the call's default once set.
        rt.set_dialog_policy(true, Some("ada".into()));
        let out = rt.evaluate("prompt('name?', 'anon')").unwrap();
        assert_eq!(out, serde_json::json!("ada"));

        rt.set_dialog_policy(false, None);
        let out = rt.evaluate("String(confirm('go?'))").unwrap();
        assert_eq!(out, serde_json::json!("false"));
    }

    #[test]
    fn test_location() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let href = rt.evaluate("location.href").unwrap();
        assert_eq!(href, serde_json::json!("http://example.com/test"));
    }

    /// The tmall report's P0-①: `window.isSecureContext` read undefined on an
    /// HTTPS page, the security SDK printed "未使用 HTTPS" and walked its
    /// degraded branch. The property must exist as a boolean and track the
    /// live URL (Secure Contexts: https/wss secure, http/ws only on the
    /// localhost family, data:/file: secure, blob: inherits the inner scheme).
    #[test]
    fn test_is_secure_context() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // Plain http off localhost: exists, boolean, false.
        let out = rt
            .evaluate("typeof isSecureContext + '|' + String(isSecureContext)")
            .unwrap();
        assert_eq!(out, serde_json::json!("boolean|false"));

        rt.set_url("https://shop.world.tmall.com/shop/view_shop.htm");
        assert_eq!(
            rt.evaluate("String(isSecureContext)").unwrap(),
            serde_json::json!("true"),
            "https pages are secure contexts"
        );

        // The localhost family is potentially trustworthy over plain http.
        rt.set_url("http://localhost:3000/");
        assert_eq!(rt.evaluate("String(isSecureContext)").unwrap(), serde_json::json!("true"));
        rt.set_url("http://127.0.0.1:8089/");
        assert_eq!(rt.evaluate("String(isSecureContext)").unwrap(), serde_json::json!("true"));

        // data: is secure; a blob: URL inherits its inner scheme.
        rt.set_url("data:text/html,<p>x</p>");
        assert_eq!(rt.evaluate("String(isSecureContext)").unwrap(), serde_json::json!("true"));
        rt.set_url("blob:https://shop.world.tmall.com/uuid-1");
        assert_eq!(rt.evaluate("String(isSecureContext)").unwrap(), serde_json::json!("true"));
    }

    /// Response.url/redirected must follow Chrome semantics (final URL after
    /// redirects, `redirected` true when hops were followed). The tmall
    /// report's mtop API redirected into `_____tmd_____/punish` while the
    /// fetch row still claimed the API URL — the visibility hole behind the
    /// "promise pending with no signal" diagnosis.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_reports_final_url_and_redirected_flag() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            // Hop 1: /start answers 302 -> /landing. Hop 2: /landing answers 200.
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).unwrap();
            stream
                .write_all(b"HTTP/1.1 302 Found\r\nlocation: /landing\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/test", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const r = await fetch('/start');
                    return { url: r.url, redirected: String(r.redirected), body: await r.text() };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "url": format!("http://127.0.0.1:{}/landing", port),
                "redirected": "true",
                "body": "ok",
            })
        );
    }

    /// fetch must honor AbortSignal (batch-82 leftover): pre-flight reject
    /// (no request at all), mid-flight reject with the signal's reason while
    /// the underlying walk is still pending, AbortSignal.timeout's
    /// TimeoutError, reason identity for abort(reason), throwIfAborted, and a
    /// real default signal on Request objects.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_honors_abort_signal() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            // Two slow responses: leg 1 (mid-flight abort) and leg 3
            // (AbortSignal.timeout) both hit the server but must reject long
            // before the 800ms body lands. Leg 2 (pre-aborted) never connects.
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(800));
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
                let _ = stream.flush();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/test", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const out = {};
                    // Leg 1: abort lands while the response is still in flight.
                    let fired = 0;
                    const ac1 = new AbortController();
                    ac1.signal.addEventListener('abort', () => { fired++; });
                    const p1 = fetch('/slow', { signal: ac1.signal })
                        .then(() => 'resolved', e => 'rejected:' + e.name);
                    setTimeout(() => ac1.abort(), 50);
                    out.midFlight = await p1;
                    out.abortEventFired = fired === 1;

                    // Leg 2: pre-aborted signal rejects with the caller's
                    // reason before any network activity.
                    const ac2 = new AbortController();
                    const myErr = new Error('mine');
                    ac2.abort(myErr);
                    out.reasonIdentity = ac2.signal.reason === myErr;
                    let threw = null;
                    try { ac2.signal.throwIfAborted(); } catch (e) { threw = e; }
                    out.throwIfAbortedSameObject = threw === myErr;
                    out.preAborted = await fetch('/slow', { signal: ac2.signal })
                        .then(() => 'resolved', e => 'rejected:' + e.name);

                    // Leg 3: AbortSignal.timeout rejects with TimeoutError.
                    out.timeout = await fetch('/slow', { signal: AbortSignal.timeout(120) })
                        .then(() => 'resolved', e => 'rejected:' + e.name);

                    // Surface: default reason is an AbortError DOMException,
                    // any() follows an already-aborted source, and Request's
                    // default signal is a real AbortSignal.
                    const ac4 = new AbortController();
                    ac4.abort();
                    out.defaultReasonName = ac4.signal.reason.name;
                    out.anyFollowsAborted = AbortSignal.any([ac2.signal]).aborted === true;
                    out.requestDefaultSignal = new Request('/x').signal instanceof AbortSignal;
                    return out;
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "midFlight": "rejected:AbortError",
                "abortEventFired": true,
                "reasonIdentity": true,
                "throwIfAbortedSameObject": true,
                // plain Error('mine') — the reason passes through untouched
                "preAborted": "rejected:Error",
                "timeout": "rejected:TimeoutError",
                "defaultReasonName": "AbortError",
                "anyFollowsAborted": true,
                "requestDefaultSignal": true,
            })
        );
    }

    /// The document's own Referrer Policy (batch-84 leftover): a policy from
    /// the navigation response's Referrer-Policy header beats every <meta
    /// name=referrer>; among metas the FIRST whose content yields a valid
    /// token wins; within one value the LAST valid comma token wins. fetch()
    /// with no init policy and sync XHR resolve the document policy
    /// Rust-side at op time; an explicit init policy still overrides.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_honors_document_referrer_policy() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        // /probe answers with the request's Referer header value, "(none)"
        // when the header is absent.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap();
                let head = String::from_utf8_lossy(&buf[..n]);
                let referer = head
                    .lines()
                    .find_map(|l| {
                        let lower = l.to_ascii_lowercase();
                        lower
                            .starts_with("referer:")
                            .then(|| lower["referer:".len()..].trim().to_string())
                    })
                    .unwrap_or_else(|| "(none)".to_string());
                let body = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    referer.len(),
                    referer
                );
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });

        // Page A: two referrer metas — the first valid one ("origin") wins
        // over the later "no-referrer" (tree order, not last-wins).
        let mut rt = setup_runtime(
            r#"<html><head>
                <meta name="referrer" content="origin">
                <meta name="referrer" content="no-referrer">
            </head><body></body></html>"#,
        );
        rt.set_url(&format!("http://127.0.0.1:{}/test", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const out = {};
                    out.surface = document.referrerPolicy;
                    // No init policy → document policy ("origin") → Referer is
                    // just the origin.
                    out.metaFirstValid = await (await fetch('/probe')).text();
                    // Explicit init still beats the document policy.
                    out.initOverridesDocument =
                        await (await fetch('/probe', { referrerPolicy: 'no-referrer' })).text();
                    return out;
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "surface": "origin",
                "metaFirstValid": format!("http://127.0.0.1:{}/", port),
                "initOverridesDocument": "(none)",
            })
        );

        // Page B: comma list keeps the LAST valid token ("no-referrer"); a
        // header-delivered policy (set_referrer_policy) then beats the meta
        // outright — both on the surface and on the wire.
        let mut rt = setup_runtime(
            r#"<html><head>
                <meta name="referrer" content="unsafe-url, garbage">
            </head><body></body></html>"#,
        );
        rt.set_url(&format!("http://127.0.0.1:{}/test", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const out = {};
                    out.lastValidTokenSurface = document.referrerPolicy;
                    out.lastValidToken = await (await fetch('/probe')).text();
                    return out;
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "lastValidTokenSurface": "unsafe-url",
                "lastValidToken": format!("http://127.0.0.1:{}/test", port),
            })
        );
        rt.set_referrer_policy("no-referrer");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const out = {};
                    out.headerBeatsMetaSurface = document.referrerPolicy;
                    out.headerBeatsMeta = await (await fetch('/probe')).text();
                    return out;
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "headerBeatsMetaSurface": "no-referrer",
                "headerBeatsMeta": "(none)",
            })
        );

        // Page C: an invalid first meta is skipped (its content yields no
        // valid token), the second applies; sync XHR resolves the same
        // document policy.
        let mut rt = setup_runtime(
            r#"<html><head>
                <meta name="referrer" content="trash">
                <meta name="referrer" content="no-referrer">
            </head><body></body></html>"#,
        );
        rt.set_url(&format!("http://127.0.0.1:{}/test", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const out = {};
                    out.invalidMetaSkipped = await (await fetch('/probe')).text();
                    const xhr = new XMLHttpRequest();
                    xhr.open('GET', '/probe', false);
                    xhr.send();
                    out.syncXhr = xhr.responseText;
                    return out;
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "invalidMetaSkipped": "(none)",
                "syncXhr": "(none)",
            })
        );
    }

    /// Sync XHR (obscura#908): open(..., false) + send() must issue the
    /// request and return with status/headers/body populated — before any
    /// event-loop turn. The upstream reporter's legacy pages (dojo/jQuery
    /// era) read xhr.responseText on the line right after send() and see ""
    /// + status 0 forever when the flag is ignored.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn sync_xhr_get_populates_status_and_body_inline() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-probe: yes\r\ncontent-length: 6\r\nconnection: close\r\n\r\nsyncok")
                .unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"() => {
                    const x = new XMLHttpRequest();
                    x.open('GET', '/api', false);
                    let events = 0;
                    const bump = () => { events += 1; };
                    x.addEventListener('loadstart', bump);
                    x.addEventListener('load', bump);
                    x.addEventListener('loadend', bump);
                    x.addEventListener('readystatechange', bump);
                    x.send();
                    // All response fields must already be populated here —
                    // no promise, no event loop turn.
                    return {
                        status: x.status,
                        text: x.responseText,
                        probe: x.getResponseHeader('x-probe'),
                        allHeaders: x.getAllResponseHeaders().includes('content-type'),
                        state: x.readyState,
                        events: events,
                    };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "status": 200,
                "text": "syncok",
                "probe": "yes",
                "allHeaders": true,
                "state": 4,
                "events": 0,
            })
        );
    }

    /// Sync POST round-trips the body; the server must have received the
    /// exact bytes send() was handed (form-POST legacy flows).
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn sync_xhr_post_body_roundtrip() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read until the full body (per content-length) has landed.
            let body;
            loop {
                let n = stream.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    let clen: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse().unwrap_or(0))
                        .unwrap_or(0);
                    let have = buf.len() - (pos + 4);
                    if have >= clen {
                        body = buf[pos + 4..pos + 4 + clen].to_vec();
                        break;
                    }
                }
                if n == 0 {
                    body = Vec::new();
                    break;
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"() => {
                    const x = new XMLHttpRequest();
                    x.open('POST', '/submit', false);
                    x.setRequestHeader('content-type', 'application/x-www-form-urlencoded');
                    x.send('a=1&b=two');
                    return { status: x.status, echoed: x.responseText, state: x.readyState };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "status": 200,
                "echoed": "a=1&b=two",
                "state": 4,
            })
        );
    }

    /// Sync XHR to a dead endpoint is a network error surfaced through the
    /// fields (status 0, readyState 4), not an exception.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn sync_xhr_dead_endpoint_status_zero() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"() => {
                    const x = new XMLHttpRequest();
                    x.open('GET', '/gone', false);
                    let threw = 'no';
                    try { x.send(); } catch (e) { threw = 'yes'; }
                    return { threw, status: x.status, text: x.responseText, state: x.readyState };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "threw": "no",
                "status": 0,
                "text": "",
                "state": 4,
            })
        );
    }

    /// Set-Cookie on a sync same-origin XHR must land in the cookie jar and
    /// become visible to document.cookie.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn sync_xhr_set_cookie_reaches_jar() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nset-cookie: synck=1; Path=/\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .unwrap();
            stream.flush().unwrap();
        });

        let (mut rt, _jar) = setup_runtime_with_cookies("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"() => {
                    const x = new XMLHttpRequest();
                    x.open('GET', '/login', false);
                    x.send();
                    return { status: x.status, cookie: document.cookie };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "status": 200,
                "cookie": "synck=1",
            })
        );
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
    }

    /// A dynamically inserted `<link rel=stylesheet>` must fetch its sheet,
    /// expose it through document.styleSheets, and fire the element's `load`
    /// event. The event is the load-bearing part: webpack's mini-css chunk
    /// runtime (d.f.miniCss) resolves chunk promises from it with no timeout
    /// — without it, tmall pc-shop-webapp's lazy routes render a styled
    /// blank page (router parked at navigation:"loading", Component never
    /// attached).
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn dynamic_stylesheet_link_fires_load_and_registers_sheet() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let css = "#probe { color: rgb(4, 5, 6); }";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/css\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                css.len(),
                css
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body><p id=\"probe\">x</p></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const link = document.createElement('link');
                    link.rel = 'stylesheet';
                    link.href = '/sheet.css';
                    let heardListener = 'no';
                    link.addEventListener('load', () => { heardListener = 'yes'; }, { once: true });
                    const loaded = new Promise(r => { link.onload = () => r('fired'); });
                    document.head.appendChild(link);
                    const how = await loaded;
                    const sheet = Array.from(document.styleSheets).find(s => (s.href || '').endsWith('/sheet.css'));
                    return {
                        how,
                        heardListener,
                        sheet: String(sheet != null),
                        rules: sheet ? String(sheet.cssRules.length) : 'none',
                    };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        // The landed sheet must also reach the cascade (layout cache drop on
        // ext_sheet_put). Cascade reads are screenshot-feature ops.
        #[cfg(feature = "screenshot")]
        let color = rt
            .evaluate("getComputedStyle(document.getElementById('probe')).color")
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let v = result.value.unwrap();
        assert_eq!(v["how"], serde_json::json!("fired"));
        assert_eq!(v["heardListener"], serde_json::json!("yes"));
        assert_eq!(v["sheet"], serde_json::json!("true"));
        assert_eq!(v["rules"], serde_json::json!("1"));
        #[cfg(feature = "screenshot")]
        assert_eq!(color, serde_json::json!("rgb(4, 5, 6)"));
    }

    /// The failure side of the same contract: a stylesheet link whose fetch
    /// fails fires `error` (both the on* property and listeners), never a
    /// phantom `load`.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn dynamic_stylesheet_link_fires_error_on_failed_fetch() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        // Bind then drop: the port answers connection-refused.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const link = document.createElement('link');
                    link.rel = 'stylesheet';
                    link.href = '/missing.css';
                    let sawLoad = false;
                    link.addEventListener('load', () => { sawLoad = true; }, { once: true });
                    const failed = new Promise(r => { link.onerror = () => r('fired'); });
                    document.head.appendChild(link);
                    const how = await failed;
                    return { how, sawLoad: String(sawLoad) };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let v = result.value.unwrap();
        assert_eq!(v["how"], serde_json::json!("fired"));
        assert_eq!(v["sawLoad"], serde_json::json!("false"));
    }

    /// (#41) `new Image()` must gate `load` on a real fetch: a 2xx response
    /// fires `load` with complete=true; the old stub fired `load`
    /// unconditionally without any network attempt.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn image_factory_fires_load_on_2xx_fetch() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        // Minimal PNG-magic body — the load face gates on transport status,
        // not on decode (no JS-face decoder; documented boundary in #41).
        let png: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                png.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.write_all(png).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const img = new Image();
                    let sawError = false;
                    img.addEventListener('error', () => { sawError = true; }, { once: true });
                    const loaded = new Promise(r => { img.onload = () => r('fired'); });
                    img.src = '/x.png';
                    const how = await loaded;
                    return { how, sawError: String(sawError), complete: String(img.complete) };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let v = result.value.unwrap();
        assert_eq!(v["how"], serde_json::json!("fired"));
        assert_eq!(v["sawError"], serde_json::json!("false"));
        assert_eq!(v["complete"], serde_json::json!("true"));
    }

    /// (#41) failure side: an unreachable src (connection-refused port) must
    /// fire `error`, never the phantom `load` the old stub produced.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn image_factory_fires_error_on_refused_port() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        // Bind then drop: the port answers connection-refused.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const img = new Image();
                    let sawLoad = false;
                    img.addEventListener('load', () => { sawLoad = true; }, { once: true });
                    const failed = new Promise(r => { img.onerror = () => r('fired'); });
                    img.src = '/x.png';
                    const how = await failed;
                    return { how, sawLoad: String(sawLoad), complete: String(img.complete) };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let v = result.value.unwrap();
        assert_eq!(v["how"], serde_json::json!("fired"));
        assert_eq!(v["sawLoad"], serde_json::json!("false"));
        assert_eq!(v["complete"], serde_json::json!("false"));
    }

    /// (#41) regression: data: URLs keep resolving locally and still fire
    /// `load` without any network attempt.
    #[tokio::test(flavor = "current_thread")]
    async fn image_factory_data_url_still_fires_load() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const img = new Image();
                    let sawError = false;
                    img.addEventListener('error', () => { sawError = true; }, { once: true });
                    const loaded = new Promise(r => { img.onload = () => r('fired'); });
                    img.src = 'data:image/gif;base64,R0lGODlhAQABAAAAACw=';
                    const how = await loaded;
                    return { how, sawError: String(sawError), complete: String(img.complete) };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        let v = result.value.unwrap();
        assert_eq!(v["how"], serde_json::json!("fired"));
        assert_eq!(v["sawError"], serde_json::json!("false"));
        assert_eq!(v["complete"], serde_json::json!("true"));
    }

    /// (#42) Sync XHR resolves data: URLs locally — status 200, decoded
    /// payload, content-type header, responseURL keeps the data: URL. The
    /// old behavior fell into the network-op catch and zeroed status.
    #[tokio::test(flavor = "current_thread")]
    async fn xhr_sync_data_url_reports_200() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const x = new XMLHttpRequest();
                    x.open('GET', 'data:text/plain,XYZZY', false);
                    x.send();
                    return {
                        status: String(x.status),
                        body: x.responseText,
                        ready: String(x.readyState),
                        url: x.responseURL === 'data:text/plain,XYZZY',
                        ct: x.getResponseHeader('content-type'),
                    };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        let v = result.value.unwrap();
        assert_eq!(v["status"], serde_json::json!("200"));
        assert_eq!(v["body"], serde_json::json!("XYZZY"));
        assert_eq!(v["ready"], serde_json::json!("4"));
        assert_eq!(v["url"], serde_json::json!(true));
        assert_eq!(v["ct"], serde_json::json!("text/plain"));
    }

    /// (#42) base64 payloads decode through the same RFC 2397 walk, and an
    /// unparseable data: URL lands on status 0 (Chrome: network error face).
    #[tokio::test(flavor = "current_thread")]
    async fn xhr_sync_data_url_base64_and_broken() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const b = new XMLHttpRequest();
                    b.open('GET', 'data:text/plain;base64,WFlaWlk=', false);
                    b.send();
                    const broken = new XMLHttpRequest();
                    broken.open('GET', 'data:text/plain-nocomma', false);
                    broken.send();
                    return {
                        status: String(b.status),
                        body: b.responseText,
                        brokenStatus: String(broken.status),
                    };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        let v = result.value.unwrap();
        assert_eq!(v["status"], serde_json::json!("200"));
        assert_eq!(v["body"], serde_json::json!("XYZZY"));
        assert_eq!(v["brokenStatus"], serde_json::json!("0"));
    }

    /// (#42) blob: URLs read through the createObjectURL registry with the
    /// blob's own type surfacing as content-type.
    #[tokio::test(flavor = "current_thread")]
    async fn xhr_sync_blob_url_reads_registry() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const u = URL.createObjectURL(new Blob(['BLOBOK'], { type: 'text/x-demo' }));
                    const x = new XMLHttpRequest();
                    x.open('GET', u, false);
                    x.send();
                    return {
                        status: String(x.status),
                        body: x.responseText,
                        ct: x.getResponseHeader('content-type'),
                    };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        let v = result.value.unwrap();
        assert_eq!(v["status"], serde_json::json!("200"));
        assert_eq!(v["body"], serde_json::json!("BLOBOK"));
        assert_eq!(v["ct"], serde_json::json!("text/x-demo"));
    }

    /// (#42) regression: async data: XHR keeps firing load through the same
    /// local branch instead of the fetch() path.
    #[tokio::test(flavor = "current_thread")]
    async fn xhr_async_data_url_fires_load() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const x = new XMLHttpRequest();
                    const how = await new Promise((resolve) => {
                        x.onload = () => resolve('load');
                        x.onerror = () => resolve('error');
                        x.open('GET', 'data:text/plain,XYZZY', true);
                        x.send();
                    });
                    return {
                        how,
                        status: String(x.status),
                        body: x.responseText,
                    };
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        let v = result.value.unwrap();
        assert_eq!(v["how"], serde_json::json!("load"));
        assert_eq!(v["status"], serde_json::json!("200"));
        assert_eq!(v["body"], serde_json::json!("XYZZY"));
    }

    #[test]
    fn test_button_click_dispatches_listener() {
        let mut rt = setup_runtime(r#"<button id="go">Go</button>"#);
        let result = rt.evaluate(r#"
            const button = document.getElementById('go');
            button.addEventListener('click', () => { button.dataset.clicked = 'yes'; });
            button.click();
            return button.dataset.clicked;
        "#).unwrap();
        assert_eq!(result, serde_json::json!("yes"));
    }

    #[test]
    fn test_dispatch_mouse_event_runs_listener() {
        let mut rt = setup_runtime(r#"<button id="go">Go</button>"#);
        let result = rt.evaluate(r#"
            const button = document.getElementById('go');
            let count = 0;
            button.addEventListener('click', () => { count += 1; });
            button.dispatchEvent(new MouseEvent('click', { bubbles: true }));
            return count;
        "#).unwrap();
        assert_eq!(result.as_f64().unwrap() as i64, 1);
    }

    #[test]
    fn test_location_href_assignment_updates_navigation_state() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let href = rt.evaluate("const next = '/next'; location.href = next; return location.href;").unwrap();
        assert_eq!(href, serde_json::json!("http://example.com/next"));
        assert_eq!(
            rt.take_pending_navigation(),
            Some(("http://example.com/next".to_string(), "GET".to_string(), "".to_string()))
        );
    }

    #[test]
    fn test_location_reload_triggers_navigation() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // reload() used to be a no-op, so a challenge that reloaded after
        // setting a token cookie never re-fetched. It now navigates to the
        // current href like assign/replace.
        rt.evaluate("location.reload();").unwrap();
        assert_eq!(
            rt.take_pending_navigation(),
            Some(("http://example.com/test".to_string(), "GET".to_string(), "".to_string()))
        );
    }

    #[test]
    fn test_structured_clone_preserves_buffers_and_collections() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // The old JSON.parse(JSON.stringify) fallback dropped ArrayBuffer and
        // TypedArray to {}. Real structuredClone keeps them intact.
        let result = rt.evaluate(r#"
            const ab = new ArrayBuffer(4);
            new Uint8Array(ab).set([1, 2, 3, 4]);
            const c = structuredClone({
                buf: ab,
                view: new Uint16Array([5, 6]),
                map: new Map([["k", new Uint8Array([7])]]),
                set: new Set([8]),
                date: new Date(0),
                re: /ab+c/gi,
            });
            return [
                c.buf instanceof ArrayBuffer,
                Array.from(new Uint8Array(c.buf)),
                c.view instanceof Uint16Array,
                Array.from(c.view),
                Array.from(c.map.get("k")),
                c.set.has(8),
                c.date.getTime(),
                c.re.source,
                c.re.flags,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, [1,2,3,4], true, [5,6], [7], true, 0, "ab+c", "gi"])
        );
    }

    #[test]
    fn test_structured_clone_handles_cycles_and_error_cause() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // Cycles preserve identity; an Error whose `cause` points back at
        // itself must not recurse until stack overflow.
        let result = rt.evaluate(r#"
            const obj = { name: 'a' };
            obj.self = obj;
            const c = structuredClone(obj);
            const cycleOk = c.self === c && c.name === 'a' && c !== obj;

            const err = new Error('boom');
            err.cause = err;
            const ec = structuredClone(err);
            const causeOk = ec.cause === ec && ec.message === 'boom';
            return [cycleOk, causeOk];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, true]));
    }

    #[test]
    fn test_structured_clone_own_proto_and_function_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // An own enumerable `__proto__` data property (what JSON.parse yields)
        // must clone as an own property, not reparent the clone. Functions are
        // not structured-cloneable and must throw DataCloneError.
        let result = rt.evaluate(r#"
            const obj = JSON.parse('{"__proto__": {"x": 1}, "y": 2}');
            const c = structuredClone(obj);
            const protoOk = Object.getPrototypeOf(c) === Object.prototype
                && c.y === 2
                && c.__proto__.x === 1;

            let threw = false;
            try { structuredClone({ f: function() {} }); } catch (e) {
                threw = e instanceof DOMException && e.name === "DataCloneError";
            }
            return [protoOk, threw];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, true]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn crypto_derive_output_lengths_are_capped() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // obscura #910 family: page-chosen crypto output lengths (HMAC
        // generateKey key length, HKDF deriveBits output) must fail with a
        // catchable error, not allocate the requested size first — the old
        // shape was an instant OOM abort on `vec![0u8; 2**28]`.
        let script = r#"async () => {
            const results = [];
            try {
                await crypto.subtle.generateKey({ name: 'HMAC', hash: 'SHA-256', length: 2 ** 31 }, false, ['sign']);
                results.push('hmac-generated');
            } catch (e) { results.push(e.message); }
            try {
                const k = await crypto.subtle.importKey('raw', new Uint8Array(16), { name: 'HKDF' }, false, ['deriveBits']);
                await crypto.subtle.deriveBits(
                    { name: 'HKDF', hash: 'SHA-256', salt: new Uint8Array(0), info: new Uint8Array(0) },
                    k, 2 ** 31);
                results.push('hkdf-derived');
            } catch (e) { results.push(e.message); }
            return results;
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        let values = result.value.unwrap();
        let arr = values.as_array().unwrap();
        assert!(
            arr[0].as_str().unwrap().contains("65536"),
            "oversized HMAC key must be rejected: {arr:?}"
        );
        assert!(
            arr[1].as_str().unwrap().contains("65536"),
            "oversized HKDF output must be rejected: {arr:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_data_and_blob_urls_resolve_locally() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // obscura #907 family: fetch()/XHR on data: and blob: URLs. These
        // never touch the HTTP client (it cannot fetch either scheme) — the
        // data: processor decodes inline bytes, and createObjectURL feeds a
        // registry fetch reads back. The old createObjectURL was a fake stub
        // whose URL nothing could resolve.
        let script = r#"async () => {
            const out = [];
            const t = await fetch('data:text/plain,hello%20w%C3%B6rld');
            out.push(t.status, t.headers.get('content-type'), await t.text());
            const j = await fetch('data:application/json;base64,eyJhIjoxfQ==');
            out.push(j.headers.get('content-type'), await j.text());
            const b = new Blob([new Uint8Array([104, 105])], { type: 'text/x-custom' });
            const u = URL.createObjectURL(b);
            out.push(typeof u === 'string' && u.startsWith('blob:'));
            const r = await fetch(u);
            out.push(r.status, r.headers.get('content-type'), await r.text());
            URL.revokeObjectURL(u);
            let revoked = false;
            try { await fetch(u); } catch (e) { revoked = true; }
            out.push(revoked);
            // XHR rides the same fetch branch.
            const xhrText = await new Promise((res, rej) => {
                const x = new XMLHttpRequest();
                x.open('GET', 'data:text/plain,xhr%21');
                x.onload = () => res(x.responseText + ':' + x.status);
                x.onerror = () => rej(new Error('xhr data: fetch failed'));
                x.send();
            });
            out.push(xhrText);
            return out;
        }"#;
        let result = rt
            .call_function_on_for_cdp(script, None, &[], true, true)
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([
                200, "text/plain", "hello wörld",
                "application/json", "{\"a\":1}",
                true, 200, "text/x-custom", "hi",
                true,
                "xhr!:200"
            ])
        );
    }

    #[test]
    fn btoa_atob_latin1_contract() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // Latin1 per the HTML spec: btoa("é") is the single byte 0xE9 →
        // "6Q==", not the two-byte UTF-8 sequence ("w6k="), and anything
        // above 0xFF throws InvalidCharacterError. atob must reject
        // non-alphabet input (it used to decode garbage through indexOf's
        // -1) while ignoring ASCII whitespace like Chrome.
        assert_eq!(
            rt.evaluate("btoa('hello')").unwrap(),
            serde_json::json!("aGVsbG8=")
        );
        assert_eq!(
            rt.evaluate("btoa('\\u00e9')").unwrap(),
            serde_json::json!("6Q==")
        );
        assert_eq!(
            rt.evaluate("try { btoa('\\u4e2d') } catch (e) { e.name }").unwrap(),
            serde_json::json!("InvalidCharacterError")
        );
        assert_eq!(
            rt.evaluate("atob('6Q==')").unwrap(),
            serde_json::json!("\u{e9}")
        );
        assert_eq!(
            rt.evaluate("try { atob('aGVs!!o') } catch (e) { e.name }").unwrap(),
            serde_json::json!("InvalidCharacterError")
        );
        // A lone trailing char is not a decodable quantum.
        assert_eq!(
            rt.evaluate("try { atob('Q') } catch (e) { e.name }").unwrap(),
            serde_json::json!("InvalidCharacterError")
        );
        // ASCII whitespace is ignored; the four chars here decode "hel".
        assert_eq!(
            rt.evaluate("atob('aG Vs\\n')").unwrap(),
            serde_json::json!("hel")
        );
        assert_eq!(
            rt.evaluate("atob(btoa('AB'))").unwrap(),
            serde_json::json!("AB")
        );
        // WebIDL DOMString: the argument is ToString-coerced before the
        // Latin-1 scan, so numbers/arrays encode their string form and the
        // >0xFF throw still fires on a coerced value (issue #22).
        assert_eq!(
            rt.evaluate("btoa(123)").unwrap(),
            serde_json::json!("MTIz")
        );
        assert_eq!(
            rt.evaluate("btoa([1,2])").unwrap(),
            serde_json::json!("MSwy")
        );
        assert_eq!(
            rt.evaluate(
                "try { btoa({toString: () => '\\u4e2d'}) } catch (e) { e.name }"
            )
            .unwrap(),
            serde_json::json!("InvalidCharacterError")
        );
        // Large payloads must survive: the decoder used to finish with
        // String.fromCharCode(...bytes), whose spread blew the call stack on
        // real upload sizes (file content arrives through this path).
        assert_eq!(
            rt.evaluate(
                "(function () { const s = 'A'.repeat(262144); \
                 return atob(btoa(s)).length === s.length ? 'ok' : 'len:' + atob(btoa(s)).length; })()"
            )
            .unwrap(),
            serde_json::json!("ok")
        );
    }

    /// input.files is the file-upload leg: assignment stores File objects per
    /// node (stable-nid store, same pattern as _formValues), value synthesizes
    /// Chrome's fakepath string, and FormData(form) turns the selection into
    /// real multipart File entries — an unset file input still contributes an
    /// empty octet-stream part like the spec's "constructing the form data set".
    #[test]
    fn file_input_files_value_and_formdata() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"(function () {
            document.body.innerHTML =
                '<form id="f"><input type="file" name="up" id="file1">' +
                '<input type="text" name="t" id="txt" value="x">' +
                '<input type="file" name="empty" id="file2"></form>';
            const out = [];
            const f1 = document.getElementById('file1');
            out.push('len0:' + f1.files.length);
            out.push('val0:' + f1.value);
            out.push('textfiles:' + (document.getElementById('txt').files === null));
            const fileA = new File([new Uint8Array([104, 105])], 'a.png', { type: 'image/png' });
            const fileB = new File([new Uint8Array([66])], 'b.jpg');
            f1.files = [fileA, fileB];
            const fl = f1.files;
            out.push('len:' + fl.length);
            out.push('item:' + fl.item(0).name + ',' + fl.item(1).name + ',oob:' + fl.item(5));
            out.push('idx:' + fl[0].name + '/' + fl[1].name);
            const names = []; for (const f of fl) names.push(f.name);
            out.push('iter:' + names.join('+'));
            out.push('tag:' + Object.prototype.toString.call(fl));
            out.push('value:' + f1.value);
            out.push('size:' + fileA.size + ',type:' + fileA.type);
            // Junk tolerance: non-array-like and non-File entries are dropped
            // silently, the way Chrome tolerates polyfill reassignments. An
            // ignored assignment keeps the previous selection intact.
            f1.files = 'nope';
            out.push('junkstr:' + f1.files.length);
            f1.files = [fileA, 'string', null];
            out.push('junkmix:' + f1.files.length + ':' + f1.files.item(0).name);
            // Value assignment: non-empty throws InvalidStateError (Chrome
            // message contract), empty clears the selection.
            let threw = ''; try { f1.value = 'C:\\x'; } catch (e) { threw = e.name; }
            out.push('setthrow:' + threw);
            f1.value = '';
            out.push('cleared:' + f1.files.length + ':' + f1.value);
            // FormData(form): the selection rides as File entries, an unset
            // file input still yields an empty octet-stream part.
            f1.files = [fileA];
            const entries = [];
            for (const [k, v] of new FormData(document.getElementById('f')).entries())
                entries.push(k + '=' + (typeof File === 'function' && v instanceof File ? 'File(' + v.name + ',' + v.type + ',' + v.size + ')' : v));
            out.push('fd:' + entries.join('|'));
            return out.join('\n');
        })()"#,
            )
            .unwrap();
        let text = result.as_str().unwrap();
        let expected = [
            "len0:0",
            "val0:",
            "textfiles:true",
            "len:2",
            "item:a.png,b.jpg,oob:null",
            "idx:a.png/b.jpg",
            "iter:a.png+b.jpg",
            "tag:[object FileList]",
            "value:C:\\fakepath\\a.png",
            "size:2,type:image/png",
            "junkstr:2",
            "junkmix:1:a.png",
            "setthrow:InvalidStateError",
            "cleared:0:",
            "fd:up=File(a.png,image/png,2)|t=x|empty=File(,application/octet-stream,0)",
        ].join("\n");
        assert_eq!(text, expected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_subtle_digest_variants_and_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // SHA-512/224 and SHA-512/256 were silently falling through to SHA-256,
        // and unknown names (MD5) returned a SHA-256 hash with no error. Verify
        // the FIPS 180-4 test vectors and the NotSupportedError rejection.
        let script = r#"async () => {
            const hex = (buf) => Array.from(new Uint8Array(buf)).map(b => b.toString(16).padStart(2, '0')).join('');
            const enc = new TextEncoder();
            const sha256 = hex(await crypto.subtle.digest('SHA-256', enc.encode('abc')));
            const sha512_224 = hex(await crypto.subtle.digest('SHA-512/224', enc.encode('abc')));
            const sha512_256 = hex(await crypto.subtle.digest('SHA-512/256', enc.encode('abc')));
            let threw = false;
            try { await crypto.subtle.digest('MD5', enc.encode('abc')); } catch (e) {
                threw = e.name === 'NotSupportedError';
            }
            return [sha256, sha512_224, sha512_256, threw];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa",
                "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23",
                true
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_webcrypto_secret_key_roundtrips() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // HMAC sign/verify, AES-GCM and AES-CBC encrypt/decrypt roundtrips, and
        // PBKDF2/HKDF derivation all work through the RustCrypto ops (the old
        // stubs returned fake data).
        let script = r#"async () => {
            const enc = new TextEncoder();
            const dec = new TextDecoder();

            // HMAC sign/verify (RFC 4231 key/data).
            const hk = await crypto.subtle.importKey('raw', enc.encode('key'), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign', 'verify']);
            const sig = await crypto.subtle.sign('HMAC', hk, enc.encode('The quick brown fox jumps over the lazy dog'));
            const sigHex = Array.from(new Uint8Array(sig)).map(b => b.toString(16).padStart(2, '0')).join('');
            const verifyOk = await crypto.subtle.verify('HMAC', hk, sig, enc.encode('The quick brown fox jumps over the lazy dog'));
            const verifyBad = await crypto.subtle.verify('HMAC', hk, sig, enc.encode('tampered'));

            // AES-GCM roundtrip.
            const gk = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, true, ['encrypt', 'decrypt']);
            const giv = crypto.getRandomValues(new Uint8Array(12));
            const ct = await crypto.subtle.encrypt({ name: 'AES-GCM', iv: giv }, gk, enc.encode('hello gcm'));
            const pt = dec.decode(await crypto.subtle.decrypt({ name: 'AES-GCM', iv: giv }, gk, ct));

            // AES-CBC roundtrip.
            const ck = await crypto.subtle.generateKey({ name: 'AES-CBC', length: 128 }, true, ['encrypt', 'decrypt']);
            const civ = crypto.getRandomValues(new Uint8Array(16));
            const cct = await crypto.subtle.encrypt({ name: 'AES-CBC', iv: civ }, ck, enc.encode('hello cbc'));
            const cpt = dec.decode(await crypto.subtle.decrypt({ name: 'AES-CBC', iv: civ }, ck, cct));

            // PBKDF2 derivation (RFC 6070 vector: PBKDF2-HMAC-SHA256, 1 iter).
            const pk = await crypto.subtle.importKey('raw', enc.encode('password'), { name: 'PBKDF2' }, false, ['deriveBits']);
            const dk = await crypto.subtle.deriveBits({ name: 'PBKDF2', hash: 'SHA-256', salt: enc.encode('salt'), iterations: 1 }, pk, 256);
            const dkHex = Array.from(new Uint8Array(dk)).map(b => b.toString(16).padStart(2, '0')).join('');

            return [sigHex, verifyOk, !verifyBad, pt, cpt, dkHex];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        // RFC 4231 HMAC-SHA-256("key", "The quick brown fox...") =
        //   f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8
        // RFC 6070 PBKDF2-HMAC-SHA256("password", "salt", 1, 32) =
        //   120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([
                "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8",
                true, true, "hello gcm", "hello cbc",
                "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_webcrypto_pbkdf2_rejects_excessive_iterations() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // A page asking for 2^32 iterations must not pin the single-threaded
        // runtime; the op rejects it with OperationError (upstream cfda91b).
        let script = r#"async () => {
            const enc = new TextEncoder();
            const pk = await crypto.subtle.importKey('raw', enc.encode('password'), { name: 'PBKDF2' }, false, ['deriveBits']);
            try {
                await crypto.subtle.deriveBits({ name: 'PBKDF2', hash: 'SHA-256', salt: enc.encode('salt'), iterations: 4294967295 }, pk, 256);
                return 'no-throw';
            } catch (e) {
                return e.name;
            }
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("OperationError"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_structured_clone_preserves_cryptokey_identity() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // A CryptoKey reached twice in a graph must clone to one shared object
        // that crypto.subtle still accepts (upstream 8698afc + a921668).
        let script = r#"async () => {
            const enc = new TextEncoder();
            const key = await crypto.subtle.importKey('raw', enc.encode('k'), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
            const c = structuredClone({ a: key, b: key });
            const sameObject = c.a === c.b;
            // The clone stays usable by crypto.subtle (key material re-registered).
            const sig = await crypto.subtle.sign('HMAC', c.a, enc.encode('msg'));
            return [sameObject, sig instanceof ArrayBuffer, c.a instanceof CryptoKey];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([true, true, true]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_get_random_values_and_uuid_from_csprng() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // getRandomValues fills integer typed arrays, randomUUID returns a v4
        // UUID shape, and both reject/fill sensibly.
        let script = r#"() => {
            const u8 = new Uint8Array(32);
            crypto.getRandomValues(u8);
            const nonZero = u8.some(b => b !== 0);
            const uuid = crypto.randomUUID();
            const uuidOk = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuid);
            let typeErr = false;
            try { crypto.getRandomValues(new Float64Array(4)); } catch (e) { typeErr = e.name === 'TypeMismatchError'; }
            return [nonZero, uuidOk, typeErr];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, false).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([true, true, true]));
    }

    #[test]
    fn test_node_iterator_returns_root_and_has_detach() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // createNodeIterator was an alias of createTreeWalker, so the first
        // nextNode() silently skipped the root and detach was missing (#467).
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<a></a>';
            const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT);
            const tags = [];
            let n;
            while ((n = it.nextNode())) tags.push(n.tagName);
            return [tags, typeof it.detach, it.root === root, it.referenceNode.tagName, it.pointerBeforeReferenceNode];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([["DIV", "A"], "function", true, "A", false])
        );
    }

    #[test]
    fn test_treewalker_next_document_order_and_reject_prunes() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // Document-order forward walk (#432); FILTER_REJECT prunes the whole
        // subtree (#461) rather than just skipping the node.
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<a><b></b></a><c></c>';
            const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                acceptNode(n) { return n.tagName === 'A' ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT; }
            });
            const tags = [];
            let n;
            while ((n = w.nextNode())) tags.push(n.tagName);
            return tags;
        "#).unwrap();
        // A is rejected, so its child B is pruned too; C still follows. Root
        // (DIV) is never returned by a TreeWalker's nextNode.
        assert_eq!(result, serde_json::json!(["C"]));
    }

    #[test]
    fn test_treewalker_skip_descends_into_children() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // FILTER_SKIP must still expose a skipped node's children (#469); the
        // old firstChild stepped straight to the next sibling and returned null.
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<section><a></a></section>';
            const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                acceptNode(n) { return n.tagName === 'SECTION' ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT; }
            });
            const first = w.firstChild();
            return first ? first.tagName : null;
        "#).unwrap();
        assert_eq!(result, serde_json::json!("A"));
    }

    #[test]
    fn test_treewalker_previousnode_reverse_order() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // previousNode walked reverse document order and died mid-tree when a
        // candidate was filtered (#462). Walk forward to the end, then back.
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<a><b></b></a><c></c>';
            const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                acceptNode(n) { return n.tagName === 'B' ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT; }
            });
            while (w.nextNode()) {}
            const tags = [];
            let n;
            while ((n = w.previousNode())) tags.push(n.tagName);
            return tags;
        "#).unwrap();
        // Reverse document order with B skipped: the walk from C finds A, then
        // stops at root (which a backward traversal never returns).
        assert_eq!(result, serde_json::json!(["A"]));
    }

    #[test]
    fn test_treewalker_parentnode_stays_within_root() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // parentNode returned a node OUTSIDE the subtree when currentNode was
        // root, and null instead of root when an accepted ancestor was root
        // itself (#475).
        let result = rt.evaluate(r#"
            const root = document.createElement('div');
            root.innerHTML = '<a><b></b></a>';
            // Skip A, so parentNode must climb past it to the accepted ancestor root.
            const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                acceptNode(n) { return n.tagName === 'A' ? NodeFilter.FILTER_SKIP : NodeFilter.FILTER_ACCEPT; }
            });
            const b = root.querySelector('b');
            w.currentNode = b;
            const parent = w.parentNode();
            // At root, parentNode must not surface <body> above it.
            w.currentNode = root;
            const above = w.parentNode();
            return [parent === root, above];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, null]));
    }

    #[test]
    fn test_insert_before_flattens_document_fragment_in_order() {
        let mut rt = setup_runtime(r#"<main id="host"><article id="last"></article></main>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const last = document.getElementById('last');
            const fragment = document.createDocumentFragment();
            const first = document.createElement('article');
            const second = document.createElement('article');
            first.id = 'first';
            second.id = 'second';
            fragment.appendChild(first);
            fragment.appendChild(second);

            const returned = host.insertBefore(fragment, last);
            return [
                returned === fragment,
                Array.from(host.children).map(node => node.id),
                fragment.childNodes.length,
                first.parentElement === host,
                second.parentElement === host,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, ["first", "second", "last"], 0, true, true])
        );
    }

    #[test]
    fn test_replace_child_flattens_document_fragment_and_removes_old_child() {
        let mut rt = setup_runtime(
            r#"<main id="host"><article id="old"></article><article id="tail"></article></main>"#,
        );
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const old = document.getElementById('old');
            const fragment = document.createDocumentFragment();
            const first = document.createElement('article');
            const second = document.createElement('article');
            first.id = 'first';
            second.id = 'second';
            fragment.appendChild(first);
            fragment.appendChild(second);

            const returned = host.replaceChild(fragment, old);
            return [
                returned === old,
                Array.from(host.children).map(node => node.id),
                fragment.childNodes.length,
                old.parentNode === null,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, ["first", "second", "tail"], 0, true])
        );
    }

    #[test]
    fn test_insert_before_and_replace_child_report_to_mutation_observers() {
        // insertBefore/replaceChild ran the tree mutation but never notified
        // MutationObserver — before()/after()/replaceWith() route through
        // insertBefore, so they were silent too.
        let mut rt = setup_runtime(r#"<main id="host"><p id="a"></p><p id="b"></p></main>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const observer = new MutationObserver(() => {});
            observer.observe(host, { childList: true });
            const x = document.createElement('x-i');
            host.insertBefore(x, document.getElementById('b'));
            const y = document.createElement('x-r');
            host.replaceChild(y, document.getElementById('a'));
            // Observer delivery is a microtask; takeRecords() drains
            // synchronously.
            return observer.takeRecords().map(r => [r.addedNodes.length, r.removedNodes.length]);
        "#).unwrap();
        assert_eq!(result, serde_json::json!([[1, 0], [1, 1]]));
    }

    #[test]
    fn test_checkbox_radio_default_value_on() {
        // A checkbox/radio with no value attribute returns "on" in a real
        // browser, not the empty string; an explicit value attribute wins.
        let mut rt = setup_runtime(
            r#"<input id="cb" type="checkbox"><input id="rd" type="radio"><input id="cbv" type="checkbox" value="yes"><input id="txt" type="text">"#,
        );
        let result = rt.evaluate(r#"
            return [
                document.getElementById('cb').value,
                document.getElementById('rd').value,
                document.getElementById('cbv').value,
                document.getElementById('txt').value,
            ];
        "#).unwrap();
        assert_eq!(result, serde_json::json!(["on", "on", "yes", ""]));
    }

    #[test]
    fn test_child_nodes_is_a_real_nodelist() {
        // childNodes returned a plain Array (Array.isArray true, toString
        // "[object Array]") — an instant fingerprinting tell. A real browser
        // reports "[object NodeList]" and Array.isArray false.
        let mut rt = setup_runtime(r#"<div id="host"><p>A</p><p>B</p></div>"#);
        let result = rt.evaluate(r#"
            const list = document.getElementById('host').childNodes;
            const seen = [];
            list.forEach((n, i) => seen.push([i, n.tagName]));
            return [
                Array.isArray(list),
                Object.prototype.toString.call(list),
                list instanceof NodeList,
                list.length,
                list.item(0).tagName,
                list.item(7),
                [...list].map(n => n.tagName),
                Array.from(list.keys()),
                seen,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                false, "[object NodeList]", true, 2, "P", null,
                ["P", "P"], [0, 1], [[0, "P"], [1, "P"]]
            ])
        );
    }

    #[test]
    fn test_adopt_node_and_toggle_attribute() {
        // Lit/Stencil and several ad SDKs call both; the missing methods threw.
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            const child = document.createElement('span');
            host.appendChild(child);
            const adopted = document.adoptNode(child);
            const toggles = [
                host.toggleAttribute('hidden'),
                host.toggleAttribute('hidden'),
                host.toggleAttribute('data-x', true),
                host.toggleAttribute('data-x', true),
                host.toggleAttribute('data-x', false),
                host.toggleAttribute('data-x', false),
            ];
            return [
                adopted === child,
                toggles,
                host.hasAttribute('hidden'),
                host.hasAttribute('data-x'),
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, [true, false, true, true, false, false], false, false])
        );
    }

    #[test]
    fn test_clone_node_shallow_preserves_attributes_and_isolation() {
        let mut rt = setup_runtime(
            r#"<section id="src" class="source" data-token="original"><span>child</span></section>"#,
        );
        let result = rt.evaluate(r#"
            const source = document.getElementById('src');
            const clone = source.cloneNode(false);
            clone.className = 'clone';
            source.setAttribute('data-token', 'changed');
            return [
                clone instanceof Element,
                clone.tagName,
                clone.id,
                clone.className,
                clone.getAttribute('data-token'),
                clone.childNodes.length,
                clone.parentNode === null,
                clone !== source,
                source.className,
                source.getAttribute('data-token'),
                source.childNodes.length,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true, "SECTION", "src", "clone", "original", 0, true, true,
                "source", "changed", 1
            ])
        );
    }

    #[test]
    fn test_clone_node_deep_keeps_table_children_and_template_contents() {
        // The old innerHTML round-trip parsed through a <div> context, which
        // discards <tr>/<td>/<option> as invalid children. Structural cloning
        // has no parsing context, so they survive; <template> contents hang
        // off a separate fragment and need their own remapped clone.
        let mut rt = setup_runtime(
            r#"<table id="tbl"><tr><td>c1</td></tr></table><template id="tpl"><p>in-template</p></template>"#,
        );
        let result = rt.evaluate(r#"
            const tblClone = document.getElementById('tbl').cloneNode(true);
            const tplClone = document.getElementById('tpl').cloneNode(true);
            return [
                tblClone.querySelectorAll('td').length,
                tblClone.querySelector('td').textContent,
                tplClone.content.childNodes.length,
                tplClone.content.querySelector('p').textContent,
                tplClone.content !== document.getElementById('tpl').content,
            ];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([1, "c1", 1, "in-template", true]));
    }

    #[test]
    fn test_clone_node_deep_subtree_does_not_overflow() {
        // Structural cloning uses an explicit stack in Rust, so a pathological
        // nesting depth cannot overflow the JS stack.
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            let node = document.getElementById('host');
            for (let i = 0; i < 2000; i++) {
                const child = document.createElement('div');
                node.appendChild(child);
                node = child;
            }
            const clone = document.getElementById('host').cloneNode(true);
            let depth = 0, cur = clone;
            while (cur.firstChild) { depth++; cur = cur.firstChild; }
            return [depth];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([2000]));
    }

    #[test]
    fn test_submit_button_click_handler_can_prevent_default_and_navigate() {
        let mut rt = setup_runtime(r#"<form><button type="submit" id="submit">Submit</button></form>"#);
        let href = rt.evaluate(r#"
            const form = document.querySelector('form');
            form.addEventListener('submit', (event) => {
                event.preventDefault();
                location.href = '/submitted';
            });
            document.getElementById('submit').click();
            return location.href;
        "#).unwrap();
        assert_eq!(href, serde_json::json!("http://example.com/submitted"));
        assert_eq!(
            rt.take_pending_navigation(),
            Some(("http://example.com/submitted".to_string(), "GET".to_string(), "".to_string()))
        );
    }

    #[test]
    fn test_click_fieldset_disabled_controls_do_not_activate() {
        // <fieldset disabled> disables its descendant controls — no toggle and
        // no click event at all — except descendants of its FIRST <legend>
        // (HTML spec actually-disabled semantics; obscura#721 edge matrix).
        let mut rt = setup_runtime(r#"<form><fieldset disabled>
            <legend><input type=checkbox id=first></legend>
            <legend><input type=checkbox id=second></legend>
            <input type=checkbox id=body>
            </fieldset><input type=checkbox id=outside></form>"#);
        let result = rt.evaluate(r#"
            const hits = [];
            for (const id of ['first','second','body','outside']) {
                const el = document.getElementById(id);
                el.addEventListener('click', () => hits.push(id));
                el.click();
            }
            return [document.getElementById('first').checked,
                    document.getElementById('second').checked,
                    document.getElementById('body').checked,
                    document.getElementById('outside').checked,
                    hits];
        "#).unwrap();
        // First-legend control activates; second-legend and fieldset-body
        // controls are actually-disabled (no toggle, no event); outside is
        // unaffected.
        assert_eq!(
            result,
            serde_json::json!([true, false, false, true, ["first", "outside"]])
        );
    }

    #[test]
    fn test_checkbox_click_clears_indeterminate_and_cancel_restores() {
        // Checkbox activation clears `indeterminate` before the event fires
        // (a listener sees the cleared state); a cancelled click restores
        // both `checked` and `indeterminate`. `indeterminate` is a real IDL
        // property on the prototype, not an expando — `'indeterminate' in el`
        // is true and fresh elements default to false (obscura#721 edge matrix;
        // upstream deliberately skipped this because stock has no property).
        let mut rt = setup_runtime(r#"<input type=checkbox id=plain><input type=checkbox id=cxl><input type=checkbox id=fresh>"#);
        let result = rt.evaluate(r#"
            const plain = document.getElementById('plain'), cxl = document.getElementById('cxl');
            const propReal = ['indeterminate' in plain,
                              Object.getOwnPropertyNames(Object.getPrototypeOf(plain)).includes('indeterminate'),
                              document.getElementById('fresh').indeterminate];
            plain.indeterminate = true;
            let seen = null;
            plain.addEventListener('click', () => { seen = [plain.checked, plain.indeterminate]; });
            plain.click();
            const plainAfter = [plain.checked, plain.indeterminate];
            cxl.indeterminate = true;
            cxl.addEventListener('click', (e) => e.preventDefault());
            cxl.click();
            return [propReal, seen, plainAfter, [cxl.checked, cxl.indeterminate]];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                [true, true, false], // real prototype property; defaults false
                [true, false],       // handler observes the flip AND the cleared indeterminate
                [true, false],       // uncancelled click keeps the cleared state
                [false, true],       // cancelled click restores checked AND indeterminate
            ])
        );
    }

    #[test]
    fn test_dispatchevent_click_runs_activation_like_chrome() {
        // Chrome runs activation behavior for untrusted clicks too:
        // `cb.dispatchEvent(new MouseEvent('click'))` toggles the checkbox,
        // a preventDefault listener cancels the flip, and a synthetic click
        // on a label forwards to its labeled control (obscura#826 lineage —
        // our .click()/label.click() already matched; this closes the
        // dispatchEvent arm).
        let mut rt = setup_runtime(r#"<input type=checkbox id=syn><input type=checkbox id=cxl>
            <label for=syn id=lb>go</label><input type=radio name=g id=r1><input type=radio name=g id=r2 checked>"#);
        let result = rt.evaluate(r#"
            const syn = document.getElementById('syn'), cxl = document.getElementById('cxl');
            let changes = 0;
            syn.addEventListener('change', () => changes++);
            syn.dispatchEvent(new MouseEvent('click', {bubbles: true, cancelable: true}));
            const synAfter = syn.checked;
            cxl.addEventListener('click', (e) => e.preventDefault());
            cxl.dispatchEvent(new MouseEvent('click', {bubbles: true, cancelable: true}));
            const cxlAfter = cxl.checked;
            document.getElementById('lb').dispatchEvent(new MouseEvent('click', {bubbles: true, cancelable: true}));
            document.getElementById('r2').dispatchEvent(new MouseEvent('click', {bubbles: true, cancelable: true}));
            return [synAfter, cxlAfter, syn.checked, changes,
                    document.getElementById('r1').checked, document.getElementById('r2').checked];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true,  // synthetic dispatch toggles
                false, // preventDefault cancels the flip
                false, // label dispatch forwards and toggles back
                2,     // change fired for both syn toggles
                false, // peer radio stays unchecked
                true,  // clicked radio checked
            ])
        );
    }

    #[test]
    fn test_element_labels_and_label_control_getters() {
        // obscura#835 lineage: label↔control association must be readable in
        // both directions — Playwright's getByLabel() leans on `label.control`
        // and `el.labels` in the page. Per spec, a label's control is the
        // for-referenced element (if `for` is present, empty for = nothing)
        // else the first labelable descendant; `labels` is the reverse map.
        let mut rt = setup_runtime(r#"<div id=d></div>
            <input type=hidden id=h>
            <label for=a id=la>Name</label><input id=a>
            <label id=lb><input id=b><input id=b2></label>
            <label for=a id=lc><input id=c></label>"#);
        let result = rt.evaluate(r#"
            const $ = (id) => document.getElementById(id);
            return [
                // non-labelable: empty NodeList, property present
                $('d').labels.length, $('h').labels.length,
                // for-linked: BOTH for=a labels associate (la and lc), tree order
                $('a').labels.length, $('a').labels[0] === $('la'), $('la').control === $('a'),
                // wrapping: only the FIRST labelable descendant associates
                $('b').labels.length, $('b').labels[0] === $('lb'), $('b2').labels.length,
                // a wrapping label whose `for` points elsewhere: not associated
                // with the wrapped input, and still controls its for-target
                $('c').labels.length, $('lc').control === $('a'),
                // label with no for and no labelable descendant: control null
                $('d').closest ? $('lc').control !== $('c') : true,
                // NodeList shape
                $('a').labels instanceof NodeList,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                0, 0,
                2, true, true,
                1, true, 0,
                0, true,
                true,
                true,
            ])
        );
    }

    #[test]
    fn test_document_spec_collections_forms_images_links_scripts() {
        // glama.ai admin hydration died on `document.forms.namedItem is not a
        // function` — the Document class had no collection getters at all.
        // forms/images/links/scripts are the spec set frameworks probe first;
        // namedItem and Proxy named access come from HTMLCollection itself.
        let mut rt = setup_runtime(r#"<form id=login name=primary></form>
            <form name=secondary></form>
            <img id=picture>
            <a id=withhref href="/x"></a><a id=nohref></a>
            <script id=inline>void 0;</script>"#);
        let result = rt.evaluate(r#"
            return [
                document.forms instanceof HTMLCollection,
                document.forms.length,
                document.forms.namedItem('login').name,
                document.forms.namedItem('primary').id,
                document.forms.primary === document.forms.namedItem('primary'),
                document.forms.item(1).name,
                document.images.length, document.images.namedItem('picture').id,
                // links counts only a[href]/area[href]: the bare <a> is out
                document.links.length, document.links[0].id,
                document.scripts.length,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true,
                2,
                "primary",
                "login",
                true,
                "secondary",
                1, "picture",
                1, "withhref",
                1,
            ])
        );
    }

    #[test]
    fn test_form_named_control_access() {
        // `form.fieldName` — HTMLFormElement's own named access to its listed
        // controls. Surfaced by the hosted live-view page: an inline onclick
        // reading document.forms[0].q.value threw, because the forms
        // *collection* shipped without the form element's named access. Real
        // props must still win, so a control named "submit" cannot shadow
        // form.submit (same precedence as Chrome).
        let mut rt = setup_runtime(r#"<form id=f>
            <input name=q value=hello><input id=alt name=second value=world><button name=go>Go</button>
            </form>
            <input name=q value=outside>"#);
        let result = rt.evaluate(r#"
            const f = document.forms[0];
            return [
                f.q.value,                                  // by name
                f.alt.value,                                // by id
                f.second === f.alt,                         // both names hit one control
                f.go.tagName,                               // buttons are listed elements too
                ('q' in f), ('nope' in f),
                f.q === document.forms.namedItem('f').q,    // stable identity through the cache
                typeof f.submit === 'function',             // real methods win over named props
                f.q === document.querySelector('input[name=q]'), // the wrapper itself
                document.createElement('form').constructor === HTMLFormElement, // createElement path too
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                "hello",
                "world",
                true,
                "BUTTON",
                true, false,
                true,
                true,
                true,
                true,
            ])
        );
    }

    #[test]
    fn test_form_value_paint_mirror() {
        // el.value = x is DIRTY state: the prototype setter mirrors it into
        // NodeData::Element::live_value for the paint walk, and it must never
        // leak into any serialized DOM surface (Chrome's dirty value lives in
        // the same nowhere-land). form.reset() then restores the PARSED
        // defaults (value attr / child text / selected attr / checked attr)
        // after a cancelable 'reset' event — the old reset wiped everything
        // to '' without firing anything.
        let mut rt = setup_runtime(r#"<form id=f>
            <input id=t value=stale><textarea id=ta>ta0</textarea>
            <input id=c type=checkbox checked>
            <select id=s><option value=a>A</option><option value=b selected>B</option></select>
            </form>"#);
        let result = rt.evaluate(r#"
            const f = document.getElementById('f');
            const t = document.getElementById('t');
            const ta = document.getElementById('ta');
            const c = document.getElementById('c');
            const s = document.getElementById('s');
            t.value = 'typed';
            ta.value = 'typed ta';
            c.checked = false;
            s.value = 'a';
            const before = [
                t.value, ta.value,
                t.getAttribute('value'),
                t.outerHTML.includes('typed'),
                ta.outerHTML.includes('typed ta'),
                document.forms[0].t.value,
                c.checked, s.value,
            ];
            let sawReset = 0, cancelled = false;
            f.addEventListener('reset', (e) => { sawReset++; cancelled = e.cancelable; });
            f.reset();
            const after = [sawReset, cancelled, t.value, ta.value, c.checked, s.value];
            // A vetoed reset leaves every control untouched.
            f.addEventListener('reset', (e) => e.preventDefault());
            t.value = 'typed2';
            f.reset();
            return [before, after, t.value, t.getAttribute('value'), t.outerHTML.includes('typed2')];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                ["typed", "typed ta", "stale", false, false, "typed", false, "a"],
                [1, true, "stale", "ta0", true, "b"],
                "typed2", "stale", false,
            ])
        );
    }

    #[test]
    fn test_form_paint_mirrors_from_js() {
        // The form paint batch's JS halves, read from the Rust tree: every
        // select mutation entry point (value / selectedIndex / option
        // .selected / form.reset) recomputes the displayed label into
        // live_value, and a checked write lands as live_checked — the layout
        // tests drive the Rust setters directly, so this is the only place
        // proving the bootstrap actually fires the mirrors.
        let mut rt = setup_runtime(r#"<form id=f>
            <select id=s><option value=a>A</option><option value=b selected>B</option></select>
            <input id=c type=checkbox>
            </form>"#);
        let label_of = |rt: &JsRuntime| {
            rt.with_dom(|dom| {
                let s = dom.query_selector_all("#s").unwrap()[0];
                dom.with_node(s, |n| n.live_value().map(str::to_string)).flatten()
            })
            .flatten()
        };
        let checked_of = |rt: &JsRuntime| {
            rt.with_dom(|dom| {
                let c = dom.query_selector_all("#c").unwrap()[0];
                dom.with_node(c, |n| n.live_checked()).flatten()
            })
            .flatten()
        };

        rt.evaluate("document.getElementById('s').value = 'a'").unwrap();
        assert_eq!(label_of(&rt).as_deref(), Some("A"), "value setter mirrors the label");

        rt.evaluate("document.getElementById('s').selectedIndex = 1").unwrap();
        assert_eq!(label_of(&rt).as_deref(), Some("B"), "selectedIndex mirrors despite bypassing the property setter");

        rt.evaluate("document.getElementById('s').options[0].selected = true").unwrap();
        assert_eq!(label_of(&rt).as_deref(), Some("A"), "option.selected mirrors through its owning select");

        rt.evaluate("document.getElementById('c').checked = true").unwrap();
        assert_eq!(checked_of(&rt), Some(true), "checked setter mirrors live_checked");

        rt.evaluate("document.getElementById('f').reset()").unwrap();
        assert_eq!(label_of(&rt).as_deref(), Some("B"), "reset mirrors the parsed selected attr");
        assert_eq!(checked_of(&rt), Some(false), "reset mirrors the parsed (absent) checked attr");
    }

    #[test]
    fn test_selection_mirror_records_js_writes() {
        // The typing-cursor batch's JS halves, read from the Rust tree: focus,
        // setSelectionRange, value writes, selectionStart writes, and blur all
        // keep the (node, start, end) selection mirror true to Chrome's
        // semantics — focus with no record parks the caret at the value end,
        // a value write resets it to the new end, blur keeps the record while
        // the focus moves on.
        let mut rt = setup_runtime(r#"<input id="q" value="abcd">"#);
        let sel_of = |rt: &JsRuntime| {
            rt.with_dom(|dom| {
                let q = dom.query_selector_all("#q").unwrap()[0];
                (
                    dom.focused_node() == Some(q),
                    dom.selection().filter(|(nid, _, _)| *nid == q),
                )
            })
            .expect("dom handle")
        };

        rt.evaluate("document.getElementById('q').focus()").unwrap();
        let (focused, sel) = sel_of(&rt);
        assert!(focused, "focus() records focused_node");
        let (_q, x, y) = sel.expect("focus() writes a selection record");
        assert_eq!((x, y), (4, 4), "focus() with no record parks at the value end");

        rt.evaluate("document.getElementById('q').setSelectionRange(1, 3)").unwrap();
        let (_, sel) = sel_of(&rt);
        assert_eq!(sel.map(|(_, s, e)| (s, e)), Some((1, 3)), "setSelectionRange mirrors");

        rt.evaluate("document.getElementById('q').value = 'ab'").unwrap();
        let (_, sel) = sel_of(&rt);
        assert_eq!(sel.map(|(_, s, e)| (s, e)), Some((2, 2)), "value write resets the mirror to the new end");

        rt.evaluate("document.getElementById('q').selectionStart = 1").unwrap();
        let (_, sel) = sel_of(&rt);
        assert_eq!(sel.map(|(_, s, e)| (s, e)), Some((1, 2)), "selectionStart write mirrors");

        rt.evaluate("document.getElementById('q').blur()").unwrap();
        let (focused, sel) = sel_of(&rt);
        assert!(!focused, "blur clears focused_node");
        assert_eq!(sel.map(|(_, s, e)| (s, e)), Some((1, 2)), "blur keeps the selection record for the refocus");
    }

    #[test]
    fn test_option_legacy_factory() {
        // glama.ai admin form chunk populates selects via `new Option(label,
        // value)`; without the global its hydration died on "Option is not
        // defined". Same deal as `new Image()`: return a real <option> element
        // so value/selected semantics and select.add() come for free.
        let mut rt = setup_runtime("<select id=s></select>");
        let result = rt.evaluate(r#"
            const sel = document.getElementById('s');
            const opt = new Option('Label A', 'a');
            sel.appendChild(opt);
            const opt2 = new Option('B', 'b', false, true);
            sel.appendChild(opt2);
            // HTMLOptionsCollection surface on select.options
            sel.options.add(new Option('C', 'c'));
            sel.options.remove(1);
            return [
                typeof Option,
                opt.tagName, opt.text, opt.value,
                sel.options.length, sel.options[0].value,
                opt2.selected, opt2.textContent,
                typeof sel.options.add,
                sel.options.length, sel.options[1].value,
                sel.options.selectedIndex,
                // option.selected = true is exclusive: the sibling that carried
                // the selected attribute loses value/selectedIndex (glama form).
                (sel.options[1].selected = true),
                sel.value,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                "function",
                "OPTION", "Label A", "a",
                2, "a",
                true, "B",
                "function",
                2, "c",
                0,
                true, "c",
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_unhandled_rejection_dispatches_window_events() {
        // Chrome surfaces unhandled promise rejections on the window as
        // PromiseRejectionEvents (obscura#797 lineage): `unhandledrejection`
        // carries the real promise + reason, and `rejectionhandled` fires
        // when a handler is attached late. deno_core routes both through the
        // core callbacks bootstrap registers.
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate(r#"
            globalThis.__seen = [];
            addEventListener('unhandledrejection', function (e) {
                __seen.push(['unhandledrejection', String(e.reason && e.reason.message), e.promise instanceof Promise, e.cancelable]);
            });
            addEventListener('rejectionhandled', function (e) {
                __seen.push(['rejectionhandled', e.promise instanceof Promise]);
            });
            Promise.reject(new Error('boom-xyz'));
            globalThis.__late = Promise.reject(new Error('late-1'));
        "#).unwrap();
        let _ = rt.run_event_loop_bounded(300).await;
        rt.evaluate("globalThis.__late.catch(function () {})").unwrap();
        let _ = rt.run_event_loop_bounded(300).await;
        let result = rt.evaluate("globalThis.__seen").unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                ["unhandledrejection", "boom-xyz", true, true],
                ["unhandledrejection", "late-1", true, true],
                ["rejectionhandled", true],
            ])
        );
    }

    #[test]
    fn test_url_reflection_src_and_href_resolve_absolute() {
        // Next.js/Turbopack webpack runtime does `new URL(x, document.currentScript.src)`
        // to derive its chunk base. If src/href return the raw relative attribute,
        // the base has no scheme and URL construction throws "TypeError: Invalid
        // scheme", so React never hydrates. URL-reflection attributes must return
        // the resolved absolute URL like real browsers.
        let mut rt = setup_runtime(r#"<html><head><script src="/app.js"></script>
            <link rel="stylesheet" href="/style.css"></head><body>
            <img id="logo" src="/logos/x.png"><a id="link" href="/docs">docs</a></body></html>"#);
        let res = rt.evaluate(r#"
            const out = {};
            out.scriptSrc = document.querySelector('script').src;
            out.linkHref = document.querySelector('link').href;
            out.imgSrc = document.getElementById('logo').src;
            out.anchorHref = document.getElementById('link').href;
            out.dataSrc = (function(){ const i = document.createElement('img'); i.setAttribute('src', 'data:image/png;base64,AAA'); return i.src; })();
            out.absSrc = (function(){ const i = document.createElement('img'); i.setAttribute('src', 'https://cdn.example.com/x.png'); return i.src; })();
            out.emptySrc = (function(){ const i = document.createElement('img'); i.setAttribute('src', ''); return i.src; })();
            out.missingSrc = document.createElement('img').src;
            return JSON.stringify(out);
        "#).unwrap();
        let v = serde_json::from_str::<serde_json::Value>(res.as_str().unwrap()).unwrap();
        assert_eq!(v["scriptSrc"], "http://example.com/app.js");
        assert_eq!(v["linkHref"], "http://example.com/style.css");
        assert_eq!(v["imgSrc"], "http://example.com/logos/x.png");
        assert_eq!(v["anchorHref"], "http://example.com/docs");
        assert_eq!(v["dataSrc"], "data:image/png;base64,AAA", "data: URLs stay absolute");
        assert_eq!(v["absSrc"], "https://cdn.example.com/x.png", "absolute stays absolute");
        assert_eq!(v["emptySrc"], "http://example.com/test", "empty src resolves to the document URL");
        assert_eq!(v["missingSrc"], "", "missing src attribute reflects as empty");
    }

    #[test]
    fn test_stealth_fingerprint_apis_pluginarray_and_webgl() {
        // authk.smithery.ai (WorkOS+Cloudflare) crashed after hydration:
        // `ReferenceError: PluginArray is not defined` (bot-detector references
        // the constructor) and `e.uniform2f is not a function` (missing WebGL
        // methods). These must exist and behave like real browsers.
        let mut rt = setup_runtime("<html><body><canvas id='c'></canvas></body></html>");
        let res = rt.evaluate(r#"
            const out = {};
            out.pluginArrayDefined = typeof PluginArray !== 'undefined';
            out.pluginsIsInstance = navigator.plugins instanceof PluginArray;
            out.pluginsLength = navigator.plugins.length;
            out.pluginsIdentity = navigator.plugins === navigator.plugins;
            out.mimeIdentity = navigator.mimeTypes === navigator.mimeTypes;
            out.pluginLength = navigator.plugins[0] && navigator.plugins[0].length;
            out.mimeIsInstance = navigator.mimeTypes instanceof MimeTypeArray;
            const c = document.getElementById('c');
            const gl = c.getContext('webgl');
            out.gl = !!gl;
            out.glIdentity = c.getContext('webgl') === gl;
            out.glInstanceof = gl instanceof WebGLRenderingContext;
            out.gl2Instanceof = document.createElement('canvas').getContext('webgl2') instanceof WebGL2RenderingContext;
            out.glNotThenable = gl.then === undefined;
            out.glSymbolUndefined = gl[Symbol.iterator] === undefined;
            out.uniform2f = typeof gl.uniform2f === 'function';
            out.getContextAttributes = typeof gl.getContextAttributes === 'function' && !!gl.getContextAttributes();
            out.getError = gl.getError() === 0;
            out.unknownMethod = (function(){ try { return typeof gl.someUnknownMethod === 'function' && gl.someUnknownMethod(1,2) === 0; } catch(e) { return 'threw:' + e.message; } })();
            return JSON.stringify(out);
        "#).unwrap();
        let v = serde_json::from_str::<serde_json::Value>(res.as_str().unwrap()).unwrap();
        assert_eq!(v["pluginArrayDefined"], true, "PluginArray global must exist");
        assert_eq!(v["pluginsIsInstance"], true, "navigator.plugins must be a PluginArray");
        assert_eq!(v["pluginsLength"].as_i64().unwrap(), 5);
        assert_eq!(v["pluginsIdentity"], true, "plugins must be a cached singleton (identity is fingerprintable)");
        assert_eq!(v["mimeIdentity"], true, "mimeTypes must be a cached singleton");
        assert_eq!(v["pluginLength"].as_i64().unwrap(), 1, "PDF plugins report one supported mime type");
        assert_eq!(v["mimeIsInstance"], true);
        assert_eq!(v["gl"], true);
        assert_eq!(v["glIdentity"], true, "getContext must return the same context on repeat calls");
        assert_eq!(v["glInstanceof"], true, "gl must be instanceof WebGLRenderingContext");
        assert_eq!(v["gl2Instanceof"], true, "webgl2 context must be instanceof WebGL2RenderingContext");
        assert_eq!(v["glNotThenable"], true, "gl.then must stay undefined or the context becomes thenable");
        assert_eq!(v["glSymbolUndefined"], true, "symbol props must not hit the numNoop fallback");
        assert_eq!(v["uniform2f"], true, "uniform2f must exist");
        assert_eq!(v["getContextAttributes"], true);
        assert_eq!(v["getError"], true);
        assert_eq!(v["unknownMethod"], true, "unknown WebGL methods must not throw");
    }

    #[test]
    fn test_navigator() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let ua = rt.evaluate("navigator.userAgent").unwrap();
        assert!(ua.as_str().unwrap().contains("Chrome"), "UA should contain Chrome: {}", ua);
        let wd = rt.evaluate("navigator.webdriver").unwrap();
        assert_eq!(wd, serde_json::Value::Null);
        let plugins = rt.evaluate("navigator.plugins.length").unwrap();
        assert!(plugins.as_f64().unwrap() > 0.0, "Should have plugins");
        let chrome = rt.evaluate("typeof window.chrome").unwrap();
        assert_eq!(chrome, serde_json::json!("object"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_no_args() {
        let mut rt = setup_runtime("<html><head><title>Test</title></head><body></body></html>");
        let result = rt
            .call_function_on("() => document.title", None, &[], true)
            .await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("Test Page"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_with_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let args = vec![
            serde_json::json!({"value": 10}),
            serde_json::json!({"value": 20}),
        ];
        let result = rt.call_function_on("(a, b) => a + b", None, &args, true).await.unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 30);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_with_string_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let args = vec![
            serde_json::json!({"value": "hello"}),
            serde_json::json!({"value": " world"}),
        ];
        let result = rt.call_function_on("(a, b) => a + b", None, &args, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("hello world"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_with_object_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let args = vec![serde_json::json!({"value": {"name": "test", "count": 5}})];
        let result = rt
            .call_function_on("(obj) => obj.name + ':' + obj.count", None, &args, true)
            .await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("test:5"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_return_object() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on("() => ({a: 1, b: 2})", None, &[], true)
            .await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!({"a": 1, "b": 2}));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_object_ref_preserves_methods() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on(
                "() => ({ items: [1,2,3], getLen: function() { return this.items.length; } })",
                None,
                &[],
                false,
            )
            .await.unwrap();
        let oid = result.object_id.unwrap();

        let result2 = rt
            .call_function_on("function() { return this.getLen(); }", Some(&oid), &[], true)
            .await.unwrap();
        assert_eq!(result2.value.unwrap().as_f64().unwrap() as i64, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_detects_node() {
        let mut rt = setup_runtime("<html><body><h1>Hello</h1></body></html>");
        let result = rt
            .evaluate_for_cdp("document.querySelector('h1')", false, false)
            .await.unwrap();
        assert_eq!(result.subtype.as_deref(), Some("node"));
        assert_eq!(result.js_type, "object");
        assert!(result.object_id.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_detects_document() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate_for_cdp("document", false, false).await.unwrap();
        assert_eq!(result.subtype.as_deref(), Some("node"));
        assert_eq!(result.class_name, "HTMLDocument");
    }


    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_awaits_resolved_promise() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate_for_cdp("Promise.resolve(42)", true, true).await.unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 42);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_awaits_timer_promise() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate_for_cdp("new Promise(resolve => setTimeout(() => resolve('done'), 1))", true, true).await.unwrap();
        assert_eq!(result.value.unwrap().as_str().unwrap(), "done");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_awaits_async_function() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate_for_cdp("(async () => 'async-ok')()", true, true).await.unwrap();
        assert_eq!(result.value.unwrap().as_str().unwrap(), "async-ok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_reports_promise_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let err = rt.evaluate_for_cdp("Promise.reject(new Error('boom'))", true, true).await.unwrap_err();
        assert!(err.contains("boom"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_outcome_reports_sync_throw() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .evaluate_for_cdp_outcome("(() => { throw new Error('sync-boom') })()", false, false, DEFAULT_AWAIT_BUDGET_MS)
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught");
        assert_eq!(exc.description, "Error: sync-boom");
        assert_eq!(exc.class_name, "Error");
        assert_eq!(outcome.info.subtype.as_deref(), Some("error"));
        assert!(outcome.info.object_id.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_outcome_reports_sync_throw_by_value() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .evaluate_for_cdp_outcome("(() => { throw new Error('bv-boom') })()", true, false, DEFAULT_AWAIT_BUDGET_MS)
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught");
        assert_eq!(exc.description, "Error: bv-boom");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_outcome_reports_await_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .evaluate_for_cdp_outcome("Promise.reject(new Error('boom'))", true, true, DEFAULT_AWAIT_BUDGET_MS)
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught (in promise)");
        assert_eq!(exc.description, "Error: boom");
        assert_eq!(exc.class_name, "Error");
    }

    // 0.4.1 taobao report problem 2: a script whose promise outlives the await
    // budget used to fall through to the never-assigned result slot and answer
    // HTTP 200 {result: null}. The settle deadline must now be an error — and
    // the message must tell the caller the script may still be running, so a
    // blind retry (double upload!) is the obviously wrong move.
    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_outcome_times_out_on_never_settling_promise() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let err = rt
            .evaluate_for_cdp_outcome("new Promise(() => {})", true, true, 150)
            .await
            .unwrap_err();
        assert!(err.contains("EVAL_TIMEOUT"), "got: {err}");
        assert!(err.contains("verify side effects"), "got: {err}");
        assert!(err.contains("150"), "message names the budget: {err}");
    }

    // The widened-budget half of the same fix: the identical late promise
    // succeeds when the caller passes a budget that actually covers it —
    // timeout_ms exists so slow page-side work is a parameter, not a failure.
    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_outcome_late_promise_lands_within_widened_budget() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .evaluate_for_cdp_outcome(
                "new Promise(r => setTimeout(() => r('late-ok'), 150))",
                true,
                true,
                DEFAULT_AWAIT_BUDGET_MS,
            )
            .await
            .unwrap();
        assert_eq!(outcome.info.value.unwrap().as_str().unwrap(), "late-ok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_outcome_reports_throw() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .call_function_on_for_cdp_outcome(
                "() => { throw new Error('fn-boom') }",
                None,
                &[],
                false,
                false,
            )
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught");
        assert_eq!(exc.description, "Error: fn-boom");
        assert_eq!(exc.class_name, "Error");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_outcome_reports_await_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let outcome = rt
            .call_function_on_for_cdp_outcome(
                "() => Promise.reject(new Error('async-fn-boom'))",
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught (in promise)");
        assert_eq!(exc.description, "Error: async-fn-boom");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_dom_interaction() {
        let mut rt = setup_runtime(r#"<div id="items"><span>A</span><span>B</span></div>"#);
        let args = vec![serde_json::json!({"value": "span"})];
        let result = rt
            .call_function_on(
                "(sel) => document.querySelectorAll(sel).length",
                None,
                &args,
                true,
            )
            .await.unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 2);
    }

    #[test]
    fn test_inner_html_setter() {
        let mut rt = setup_runtime(r#"<div id="target"><p>Old</p></div>"#);
        rt.execute_script("test", r#"
            var el = document.getElementById('target');
            el.innerHTML = '<strong>Bold</strong><em>Italic</em>';
        "#).unwrap();
        let result = rt.evaluate("document.getElementById('target').innerHTML").unwrap();
        let html = result.as_str().unwrap();
        assert!(html.contains("<strong>"), "innerHTML should contain <strong>, got: {}", html);
        assert!(html.contains("<em>"), "innerHTML should contain <em>, got: {}", html);
        assert!(!html.contains("Old"), "innerHTML should not contain old content, got: {}", html);
    }

    #[test]
    fn test_inner_html_with_nested() {
        let mut rt = setup_runtime(r#"<div id="root"></div>"#);
        rt.execute_script("test", r#"
            var el = document.getElementById('root');
            el.innerHTML = '<ul><li>A</li><li>B</li><li>C</li></ul>';
        "#).unwrap();
        let count = rt.evaluate("document.querySelectorAll('li').length").unwrap();
        assert_eq!(count.as_f64().unwrap() as i64, 3, "Should find 3 li elements after innerHTML set");

        let text = rt.evaluate("document.querySelector('li').textContent").unwrap();
        assert_eq!(text, serde_json::json!("A"));
    }

    #[test]
    fn test_fake_receiver_dom_probe_throws_and_does_not_wipe_document() {
        // Bot detectors probe with a fake receiver:
        //   Object.create(HTMLSelectElement.prototype).setHTMLUnsafe(...)
        // A real browser throws TypeError("Illegal invocation"). Our shim must
        // do the same (this._nid is undefined on the fake object), and must NOT
        // let the undefined nid fall through to Rust as node 0 = document.
        let mut rt = setup_runtime(r#"<div id="target"><p>Survive</p></div>"#);
        let result = rt.evaluate(
            r#"(function() {
                var threw = false, msg = '';
                try {
                    Object.create(HTMLSelectElement.prototype).setHTMLUnsafe('<strong>Wiped</strong>');
                } catch (e) {
                    threw = true; msg = e.name;
                }
                var body = document.getElementById('target');
                return JSON.stringify([threw, msg, document.body.children.length,
                    body ? body.innerHTML : null]);
            })()"#,
        ).unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(arr[0], serde_json::json!(true), "fake-receiver probe should throw");
        assert_eq!(arr[1], serde_json::json!("TypeError"), "should throw TypeError");
        assert!(arr[2].as_u64().unwrap() >= 1, "document body should still have children");
        assert!(arr[3].as_str().unwrap().contains("Survive"), "document content must survive: {}", arr[3]);
    }

    #[test]
    fn test_input_value() {
        let mut rt = setup_runtime(r#"<form><input id="name" type="text" value="initial"><textarea id="bio">old text</textarea></form>"#);
        let val = rt.evaluate("document.getElementById('name').value").unwrap();
        assert_eq!(val, serde_json::json!("initial"));
        rt.execute_script("test", "document.getElementById('name').value = 'new value';").unwrap();
        let val2 = rt.evaluate("document.getElementById('name').value").unwrap();
        assert_eq!(val2, serde_json::json!("new value"));
        let bio = rt.evaluate("document.getElementById('bio').value").unwrap();
        assert_eq!(bio, serde_json::json!("old text"));
    }

    #[test]
    fn test_sequential_runtime_swap() {
        let mut rt1 = setup_runtime("<html><body><h1>Page1</h1></body></html>");
        let title1 = rt1.evaluate("document.querySelector('h1').textContent").unwrap();
        assert_eq!(title1, serde_json::json!("Page1"));

        let dom1 = rt1.take_dom();
        drop(rt1);

        let mut rt2 = setup_runtime("<html><body><h1>Page2</h1></body></html>");
        let title2 = rt2.evaluate("document.querySelector('h1').textContent").unwrap();
        assert_eq!(title2, serde_json::json!("Page2"));
        drop(rt2);

        if let Some(dom) = dom1 {
            let rt1b = JsRuntime::new();
            rt1b.set_dom(dom);
            rt1b.set_url("http://example.com");
            rt1b.set_title("Page1");
            let mut rt1b = rt1b;
            let title1b = rt1b.evaluate("document.querySelector('h1').textContent").unwrap();
            assert_eq!(title1b, serde_json::json!("Page1"));
        }
    }

    #[test]
    fn test_checkbox_checked() {
        let mut rt = setup_runtime(r#"<input id="cb" type="checkbox" checked>"#);
        let checked = rt.evaluate("document.getElementById('cb').checked").unwrap();
        assert_eq!(checked, serde_json::json!(true));
        rt.execute_script("test", "document.getElementById('cb').checked = false;").unwrap();
        let checked2 = rt.evaluate("document.getElementById('cb').checked").unwrap();
        assert_eq!(checked2, serde_json::json!(false));
    }

    #[test]
    fn test_matches_and_closest() {
        let mut rt = setup_runtime(r#"<div class="outer"><div class="inner"><span id="target">Hi</span></div></div>"#);
        let matches = rt.evaluate("document.getElementById('target').matches('span')").unwrap();
        assert_eq!(matches, serde_json::json!(true));
        let closest = rt.evaluate("document.getElementById('target').closest('.outer').className").unwrap();
        assert_eq!(closest, serde_json::json!("outer"));
        let no_match = rt.evaluate("document.getElementById('target').closest('.nonexistent')").unwrap();
        assert_eq!(no_match, serde_json::Value::Null);
    }

    #[test]
    fn test_clone_node_deep() {
        let mut rt = setup_runtime(r#"<div id="src"><p>A</p><p>B</p></div>"#);
        rt.execute_script("test", r#"
            var src = document.getElementById('src');
            var clone = src.cloneNode(true);
            document.body.appendChild(clone);
        "#).unwrap();
        let count = rt.evaluate("document.querySelectorAll('p').length").unwrap();
        assert!(count.as_f64().unwrap() as i64 >= 4, "Deep clone should duplicate <p> children, got: {}", count);
    }

    #[test]
    fn test_evaluate_multistatement() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate("var x = 5; var y = 10; return x + y;").unwrap();
        assert_eq!(result.as_f64().unwrap() as i64, 15);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_object_ref_as_argument() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let obj = rt
            .call_function_on("() => ({ x: 42 })", None, &[], false)
            .await.unwrap();
        let oid = obj.object_id.unwrap();

        let args = vec![serde_json::json!({"objectId": oid})];
        let result = rt
            .call_function_on("(obj) => obj.x * 2", None, &args, true)
            .await.unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 84);
    }

    #[test]
    fn resolve_this_parses_numeric_node_ids_only() {
        let rt = setup_runtime("<html><body></body></html>");
        let good = rt.resolve_this(Some("node-7"));
        assert!(good.contains("var nid = 7"), "{good}");
        // Anything after the digits must not ride along as script source.
        for bad in ["node-1; globalThis.__injected = true", "node-", "node-x", "node-1.5"] {
            assert_eq!(rt.resolve_this(Some(bad)), "globalThis", "{bad}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn adversarial_object_id_does_not_inject_script() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let payload = "node-1; globalThis.__injected = 42";
        let result = rt
            .call_function_on(
                "function() { return typeof globalThis.__injected; }",
                Some(payload),
                &[],
                true,
            )
            .await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("undefined"));
        let probe = rt
            .call_function_on(
                "function() { return globalThis.__injected === undefined; }",
                None,
                &[],
                true,
            )
            .await.unwrap();
        assert_eq!(probe.value.unwrap(), serde_json::json!(true));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn release_object_with_adversarial_id_is_inert() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.release_object("x']; globalThis.__injected = 42; ('");
        let probe = rt
            .call_function_on(
                "function() { return globalThis.__injected === undefined; }",
                None,
                &[],
                true,
            )
            .await.unwrap();
        assert_eq!(probe.value.unwrap(), serde_json::json!(true));
    }

    fn setup_runtime_with_cookies(html: &str) -> (JsRuntime, std::sync::Arc<crate::diting_net::CookieJar>) {
        let dom = crate::diting_dom::parse_html(html);
        let jar = std::sync::Arc::new(crate::diting_net::CookieJar::new());
        let rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_url("http://example.com/test");
        rt.set_title("Test Page");
        rt.set_cookie_jar(jar.clone());
        (rt, jar)
    }

    #[test]
    fn test_document_cookie_reads_http_cookies() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        jar.set_cookie("session=abc123; Path=/", &url);
        jar.set_cookie("theme=dark; Path=/", &url);
        let result = rt.evaluate("document.cookie").unwrap();
        let cookie_str = result.as_str().unwrap();
        assert!(cookie_str.contains("session=abc123"), "expected session cookie, got: {}", cookie_str);
        assert!(cookie_str.contains("theme=dark"), "expected theme cookie, got: {}", cookie_str);
    }

    #[test]
    fn test_document_cookie_excludes_httponly() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        jar.set_cookie("visible=yes; Path=/", &url);
        jar.set_cookie("secret=token; Path=/; HttpOnly", &url);
        let result = rt.evaluate("document.cookie").unwrap();
        let cookie_str = result.as_str().unwrap();
        assert!(cookie_str.contains("visible=yes"), "expected visible cookie, got: {}", cookie_str);
        assert!(!cookie_str.contains("secret"), "httpOnly cookie should not be visible to JS, got: {}", cookie_str);
    }

    #[test]
    fn test_document_cookie_setter_stores_in_jar() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        rt.evaluate("document.cookie = 'foo=bar; Path=/'").unwrap();
        let url = url::Url::parse("http://example.com/test").unwrap();
        let result = rt.evaluate("document.cookie").unwrap();
        assert!(result.as_str().unwrap().contains("foo=bar"));
        let header = jar.get_cookie_header(&url);
        assert!(header.contains("foo=bar"), "cookie should be in jar, got: {}", header);
    }

    #[test]
    fn test_document_cookie_delete_via_max_age() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        rt.evaluate("document.cookie = 'temp=val; Path=/'").unwrap();
        assert!(rt.evaluate("document.cookie").unwrap().as_str().unwrap().contains("temp=val"));
        rt.evaluate("document.cookie = 'temp=; Max-Age=0'").unwrap();
        let result = rt.evaluate("document.cookie").unwrap();
        assert!(!result.as_str().unwrap().contains("temp="), "cookie should be deleted, got: {}", result);
        assert!(!jar.get_cookie_header(&url).contains("temp="));
    }

    #[test]
    fn test_document_cookie_js_and_http_merge() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        jar.set_cookie("server_sid=xyz; Path=/", &url);
        rt.evaluate("document.cookie = 'client_pref=light'").unwrap();
        let result = rt.evaluate("document.cookie").unwrap();
        let cookie_str = result.as_str().unwrap();
        assert!(cookie_str.contains("server_sid=xyz"), "expected server cookie, got: {}", cookie_str);
        assert!(cookie_str.contains("client_pref=light"), "expected client cookie, got: {}", cookie_str);
    }

    #[test]
    fn test_document_cookie_empty_when_no_cookies() {
        let (mut rt, _jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let result = rt.evaluate("document.cookie").unwrap();
        assert_eq!(result.as_str().unwrap(), "");
    }

    #[test]
    fn queued_navigation_does_not_move_the_cookie_context() {
        // obscura #940 shape: op_navigate must only queue the navigation —
        // the realm URL moves on commit, not at assignment time. The early
        // move let synchronous JS between `location.href = target` and the
        // actual navigation read and write the TARGET origin's cookies
        // through document.cookie, whose ops derive the domain from that
        // URL (SOP bypass). The navigation tuple itself must still be queued.
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let victim = url::Url::parse("https://victim.example/").unwrap();
        jar.set_cookie("secret=victimtoken; Path=/", &victim);

        rt.evaluate("location.href = 'https://victim.example/'").unwrap();
        let cookie_str = rt.evaluate("document.cookie").unwrap().as_str().unwrap().to_string();
        assert!(
            !cookie_str.contains("victimtoken"),
            "document.cookie must not expose another origin's cookies while the navigation is only queued, got: {}",
            cookie_str
        );

        // The write side of the same hole: a JS cookie set while the
        // navigation is queued must land on the CURRENT origin, not the
        // navigation target.
        rt.evaluate("document.cookie = 'poison=1; Path=/'").unwrap();
        assert!(
            !jar.get_cookie_header(&victim).contains("poison=1"),
            "a JS cookie written before navigation commit must not land on the target origin"
        );

        // The navigation itself is unaffected: still queued, GET, empty body.
        assert_eq!(
            rt.take_pending_navigation(),
            Some(("https://victim.example/".to_string(), "GET".to_string(), "".to_string()))
        );
    }

    #[test]
    fn evaluate_accepts_statement_scripts_with_completion_value() {
        // CDP Runtime.evaluate semantics: the input is a script — statements
        // are legal and the completion value of the last statement comes
        // back. The old expression-only wrap turned any statement syntax
        // into an uncatchable `Unexpected token ';'`.
        let mut rt = setup_runtime("<html><body></body></html>");
        assert_eq!(rt.evaluate("var x = 2; x * 3").unwrap(), serde_json::json!(6.0));
        assert_eq!(
            rt.evaluate("try { JSON.parse('x') } catch(e) { 'caught:' + e.name }").unwrap(),
            serde_json::json!("caught:SyntaxError")
        );
        // try/catch is itself statement syntax — it must parse.
        assert_eq!(rt.evaluate("typeof eval").unwrap(), serde_json::json!("function"));
    }

    #[test]
    fn evaluate_supports_function_body_style_top_level_return() {
        // Legacy engine contract: scripts written as function bodies with a
        // top-level `return` (illegal in script position) still evaluate via
        // the Function-body fallback.
        let mut rt = setup_runtime("<html><body><span id='h'>x</span></body></html>");
        assert_eq!(
            rt.evaluate(
                r#"
                const el = document.getElementById('h');
                return el ? el.tagName : null;
                "#
            ).unwrap(),
            serde_json::json!("SPAN")
        );
    }

    #[test]
    fn evaluate_bare_object_literal_returns_object() {
        // Pasted JSON evaluates as an object literal (DevTools console
        // behavior), not as a block whose completion value is undefined.
        let mut rt = setup_runtime("<html><body></body></html>");
        assert_eq!(rt.evaluate(r#"{"k": "v", "n": 1}"#).unwrap(), serde_json::json!({"k": "v", "n": 1}));
    }

    #[test]
    fn test_document_cookie_no_jar_returns_empty() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate("document.cookie").unwrap();
        assert_eq!(result.as_str().unwrap(), "");
    }

    #[test]
    fn test_document_write_appends_to_body() {
        let mut rt = setup_runtime("<html><body><p>Existing</p></body></html>");
        rt.evaluate("document.write('<div>Added</div>')").unwrap();
        let html = rt.evaluate("document.body.innerHTML").unwrap();
        let body = html.as_str().unwrap();
        assert!(body.contains("Existing"), "existing content should remain, got: {}", body);
        assert!(body.contains("Added"), "written content should appear, got: {}", body);
    }

    #[test]
    fn test_document_writeln() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate("document.writeln('Hello')").unwrap();
        let html = rt.evaluate("document.body.innerHTML").unwrap();
        assert!(html.as_str().unwrap().contains("Hello"));
    }

    #[test]
    fn test_document_write_multiple_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate("document.write('Hello', ' ', 'World')").unwrap();
        let text = rt.evaluate("document.body.textContent").unwrap();
        assert_eq!(text.as_str().unwrap().trim(), "Hello World");
    }

    #[test]
    fn test_document_open_clears_body() {
        let mut rt = setup_runtime("<html><body><p>Old content</p></body></html>");
        rt.evaluate("document.open()").unwrap();
        let html = rt.evaluate("document.body.innerHTML").unwrap();
        assert_eq!(html.as_str().unwrap(), "");
    }

    #[test]
    fn test_document_write_html_elements() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate(r#"document.write('<h1 id="title">Test</h1><p>Para</p>')"#).unwrap();
        let h1 = rt.evaluate("document.querySelector('h1').textContent").unwrap();
        assert_eq!(h1.as_str().unwrap(), "Test");
        let p = rt.evaluate("document.querySelector('p').textContent").unwrap();
        assert_eq!(p.as_str().unwrap(), "Para");
    }

    #[test]
    fn test_url_relative_resolution() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate("new URL('data.json', 'http://example.com/path/page.html').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "http://example.com/path/data.json");

        let result = rt.evaluate("new URL('/api/data', 'http://example.com/path/page.html').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "http://example.com/api/data");

        let result = rt.evaluate("new URL('https://other.com/foo', 'http://example.com/bar').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "https://other.com/foo");

        let result = rt.evaluate("new URL('sub/file.js', 'http://example.com/a/b/c.html').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "http://example.com/a/b/sub/file.js");

        let result = rt.evaluate("new URL('api.json', 'http://localhost:8080/dir/index.html').href").unwrap();
        assert_eq!(result.as_str().unwrap(), "http://localhost:8080/dir/api.json");
    }

    #[test]
    fn url_setter_search_hash_port_edges() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // WHATWG setter semantics, verified against Chrome/ada (obscura#1008
        // same face): a bare "?" / "#" sets an EMPTY component that still
        // serializes its delimiter — only the empty string removes it — and
        // the port state parser consumes the leading ASCII digits
        // ("8080abc" → 8080) while rejecting no-digit or >65535 inputs.
        let case = |rt: &mut JsRuntime, expr: &str| -> String {
            rt.evaluate(expr).unwrap().as_str().unwrap().to_string()
        };
        // search: bare "?" keeps the delimiter (with or without a prior query)
        assert_eq!(case(&mut rt, "(() => { const u = new URL('http://x.com/?a'); u.search = '?'; return u.href; })()"), "http://x.com/?");
        assert_eq!(case(&mut rt, "(() => { const u = new URL('http://x.com/'); u.search = '?'; return u.href; })()"), "http://x.com/?");
        // search: empty string removes the query entirely (no bare "?")
        assert_eq!(case(&mut rt, "(() => { const u = new URL('http://x.com/?a'); u.search = ''; return u.href; })()"), "http://x.com/");
        // hash mirrors search
        assert_eq!(case(&mut rt, "(() => { const u = new URL('http://x.com/#f'); u.hash = '#'; return u.href; })()"), "http://x.com/#");
        assert_eq!(case(&mut rt, "(() => { const u = new URL('http://x.com/#f'); u.hash = ''; return u.href; })()"), "http://x.com/");
        // port: leading digits win, no-digit / overflow inputs are ignored
        assert_eq!(case(&mut rt, "(() => { const u = new URL('http://x.com:9/'); u.port = '8080abc'; return u.href; })()"), "http://x.com:8080/");
        assert_eq!(case(&mut rt, "(() => { const u = new URL('http://x.com:9/'); u.port = '99999'; return u.href; })()"), "http://x.com:9/");
        assert_eq!(case(&mut rt, "(() => { const u = new URL('http://x.com:9/'); u.port = 'abc'; return u.href; })()"), "http://x.com:9/");
        assert_eq!(case(&mut rt, "(() => { const u = new URL('http://x.com:9/'); u.port = ''; return u.href; })()"), "http://x.com/");
    }

    // Blob URLs must look like Chrome's: `blob:<document origin>/<uuid>`.
    // The pre-rename engine handed out `blob:obscura/<base36>`, so a page
    // reading back its own blob URL (a Worker's script URL, an anchor href,
    // performance entries) could name the engine with one startsWith.
    // Non-Blob input throws Chrome's TypeErrors instead of minting a
    // fallback URL, and revoke drops the blob from both stores so a
    // revoked URL can no longer construct a Worker synchronously.
    #[test]
    fn blob_urls_are_chrome_shaped_and_non_blob_input_throws() {
        let mut rt = JsRuntime::with_base_url("https://example.com/page");
        let result = rt
            .evaluate(
                r#"return (() => {
                    // with_base_url only feeds the module loader; the realm's
                    // location reads __virtualUrl/document_url. Pin it so the
                    // minted blob URL carries a real origin, like a page.
                    globalThis.__virtualUrl = 'https://example.com/page';
                    const out = {};
                    const url = URL.createObjectURL(new Blob(['hello'], {type: 'text/plain'}));
                    out.url = url;
                    out.shaped = /^blob:https:\/\/example\.com\/[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(url);
                    out.inStore = !!globalThis.__blobObjs[url];
                    URL.revokeObjectURL(url);
                    out.afterRevoke = !!globalThis.__blobObjs[url];
                    out.missingArg = (() => { try { URL.createObjectURL(); return 'no-throw'; } catch (e) { return e.name + '|' + e.message; } })();
                    out.wrongType = (() => { try { URL.createObjectURL('nope'); return 'no-throw'; } catch (e) { return e.name + '|' + e.message; } })();
                    return out;
                })()"#,
            )
            .unwrap();
        let out = result.as_object().expect("object result");
        let url = out["url"].as_str().unwrap();
        assert_eq!(out["shaped"], serde_json::json!(true), "blob url not chrome-shaped: {url}");
        assert_eq!(out["inStore"], serde_json::json!(true));
        assert_eq!(out["afterRevoke"], serde_json::json!(false));
        assert_eq!(
            out["missingArg"],
            serde_json::json!("TypeError|Failed to execute 'createObjectURL' on 'URL': 1 argument required, but only 0 present.")
        );
        assert_eq!(
            out["wrongType"],
            serde_json::json!("TypeError|Failed to execute 'createObjectURL' on 'URL': parameter 1 is not of type 'Blob'.")
        );
    }

    // importScripts() was a silent no-op stub: Yahoo Finance's worker imports
    // protobuf at startup, got `undefined` instead, and the SDK died to a
    // TypeError pages could not see (obscura#827 family). The worker source
    // executes synchronously inside new Function, so imports cannot be
    // fetched at call time — they are preloaded from string-literal targets
    // in the source, then replayed from cache via indirect eval, which puts
    // top-level `var` in the global lexical scope the Function-wrapped body
    // can see (a plain `var` assignment would also work, but declarations
    // would not — pin the declaration semantics).
    #[tokio::test(flavor = "current_thread")]
    async fn worker_import_scripts_expose_declarations_to_worker_body() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate_for_cdp(
                r#"new Promise((resolve, reject) => {
                    const body = "importScripts('data:text/javascript,var%20pbVersion=42');" +
                        "self.onmessage = function(e) { self.postMessage(pbVersion + '|' + typeof window); };";
                    const w = new Worker(URL.createObjectURL(new Blob([body])));
                    w.onerror = e => reject(new Error('worker error: ' + e.message));
                    w.onmessage = e => resolve(e.data);
                    w.postMessage('go');
                })"#,
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("42|undefined"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_import_scripts_failed_target_fires_error_event() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate_for_cdp(
                r#"new Promise((resolve) => {
                    const body = "importScripts('data:text/javascript;base64,!!!!');" +
                        "self.onmessage = function(e) { self.postMessage('unreached'); };";
                    const w = new Worker(URL.createObjectURL(new Blob([body])));
                    w.onerror = e => resolve('err|' + e.message);
                    w.onmessage = e => resolve('msg|' + e.data);
                    w.postMessage('go');
                })"#,
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap();
        let msg = msg.as_str().unwrap();
        assert!(msg.starts_with("err|"), "worker delivered instead of erroring: {msg}");
        assert!(msg.contains("failed to load"), "error lost the importScripts origin: {msg}");
    }

    #[test]
    fn worker_import_url_extraction_skips_dynamic_arguments() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"return globalThis.__ditingWorkerImportUrls(
                    "importScripts('a.js', \"b.js\"); importScripts(runtimePath); importScripts()")"#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["a.js", "b.js"]));
    }

    // A bare `onmessage = fn` (no self. prefix) inside the worker body must
    // land on the worker scope, not create window.onmessage — the plain
    // Function wrapper let the assignment fall through to the page global
    // and the worker never received anything (obscura#867 family).
    #[tokio::test(flavor = "current_thread")]
    async fn worker_bare_onmessage_assignment_receives_messages() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate_for_cdp(
                r#"new Promise((resolve, reject) => {
                    const before = globalThis.onmessage;
                    const body = "onmessage = function(e) { postMessage('bare|' + e.data); };";
                    const w = new Worker(URL.createObjectURL(new Blob([body])));
                    w.onerror = e => reject(new Error('worker error: ' + e.message));
                    w.onmessage = e => resolve([e.data, globalThis.onmessage === before]);
                    w.postMessage('go');
                })"#,
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!(["bare|go", true]),
            "bare onmessage works and leaves the page global untouched"
        );
    }

    // Anti-fraud SDKs probe the worker's HTTP surface before using it and
    // bail with "no supported http request object" when it is bare. fetch is
    // on the synthetic scope; XMLHttpRequest resolves through to the shared
    // realm's constructor — pin all three readable by bare identifier
    // (obscura#851 family).
    #[tokio::test(flavor = "current_thread")]
    async fn worker_scope_exposes_http_request_surface() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate_for_cdp(
                r#"new Promise((resolve, reject) => {
                    const body = "onmessage = function(e) { postMessage(typeof fetch + '|' + typeof XMLHttpRequest + '|' + typeof Request); };";
                    const w = new Worker(URL.createObjectURL(new Blob([body])));
                    w.onerror = e => reject(new Error('worker error: ' + e.message));
                    w.onmessage = e => resolve(e.data);
                    w.postMessage('go');
                })"#,
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!("function|function|function")
        );
    }

    // A real worker executes its top-level script the moment the source
    // lands — collectors that only set up timers/probes and never wait for
    // a parent message must still run. The old flow deferred the whole body
    // to the first postMessage, so message-less workers never booted
    // (obscura#851 family).
    #[tokio::test(flavor = "current_thread")]
    async fn worker_top_level_runs_without_any_postmessage() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate_for_cdp(
                r#"new Promise((resolve, reject) => {
                    const body = "postMessage('booted');";
                    const w = new Worker(URL.createObjectURL(new Blob([body])));
                    w.onerror = e => reject(new Error('worker error: ' + e.message));
                    w.onmessage = e => resolve(e.data);
                })"#,
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("booted"));
    }

    // WebIDL (Node or string) coercion: replaceWith(undefined) inserts the
    // text node "undefined" — jQuery 1.12's domManip routes raw fragments
    // through here, and a silent no-op desyncs the DOM the page just asked
    // to update (obscura#888 family). Detached no-parent calls stay no-ops.
    #[test]
    fn replace_with_coerces_non_node_arguments_to_text() {
        let mut rt = setup_runtime("<html><body><div id=p><b id=c>old</b></div></body></html>");
        let result = rt
            .evaluate(
                r#"(() => {
                    document.getElementById('c').replaceWith(undefined);
                    const coerced = document.getElementById('p').textContent;
                    const d = document.createElement('div');
                    let detachedNoop = false;
                    try { d.replaceWith(); d.replaceWith(undefined); detachedNoop = true; } catch {}
                    return [coerced, detachedNoop];
                })()"#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["undefined", true]));
    }

    // One stream per document. The tokenizer carries its state across the calls.
    // https://html.spec.whatwg.org/multipage/dynamic-markup-insertion.html#dom-document-write
    #[test]
    fn document_write_joins_an_element_split_across_calls() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                document.write('<di');
                document.write('v id="split">');
                document.write('content</div>');
                const el = document.getElementById('split');
                return el ? el.textContent : null;
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("content"));
    }

    #[test]
    fn document_write_joins_a_tag_name_split_across_calls() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                document.write('<spa');
                document.write('n id="half">x</span>');
                const el = document.getElementById('half');
                return el ? el.tagName : null;
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("SPAN"));
    }

    // The shape the UI5 cachebuster writes: "<script", one per attribute, then ">".
    #[test]
    fn document_write_runs_a_script_split_across_calls() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__splitScriptRan = false;
                document.write('<scr' + 'ipt');
                document.write(' id="split-script"');
                document.write('>');
                document.write('globalThis.__splitScriptRan = true;');
                document.write('<\/scr' + 'ipt>');
                return [!!document.getElementById('split-script'), globalThis.__splitScriptRan];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([true, true]));
    }

    // A script in the <head> inserts behind itself, so that what it writes runs before what
    // the parser saw after it.
    #[test]
    fn document_write_inserts_at_the_writing_scripts_position() {
        let mut rt = setup_runtime(
            r#"<html><head><script id="writer"></script></head><body><p id="existing">x</p></body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                // What the production path sets while a script runs; bootstrap.js
                // assigns __currentScriptNid around every script it prepares.
                globalThis.__currentScriptNid = document.getElementById('writer')._nid;
                document.write('<span id="written"></span>');
                return JSON.stringify({
                  head: Array.from(document.head.children).map(e => e.id || e.tagName),
                  body: Array.from(document.body.children).map(e => e.id || e.tagName),
                });
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(r#"{"head":["writer","written"],"body":["existing"]}"#)
        );
    }

    // Holding back until the close would lose everything written after it. It belongs inside.
    #[test]
    fn document_write_shows_an_element_that_is_never_closed() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                document.write('<div id="unclosed">hello');
                const el = document.getElementById('unclosed');
                return el ? el.textContent : null;
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("hello"));
    }

    #[test]
    fn document_write_grows_an_open_element_across_calls() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                document.write('<div id="wrap">');
                document.write('<span id="inner">y</span>');
                const inner = document.getElementById('inner');
                return JSON.stringify({
                  wrap: !!document.getElementById('wrap'),
                  inner: !!inner,
                  nested: !!(inner && inner.parentElement && inner.parentElement.id === 'wrap'),
                });
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(r#"{"wrap":true,"inner":true,"nested":true}"#)
        );
    }

    #[test]
    fn document_write_keeps_call_order_at_the_insertion_point() {
        let mut rt = setup_runtime(
            r#"<html><head><script id="writer"></script></head><body></body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__currentScriptNid = document.getElementById('writer')._nid;
                document.write('<span id="one"></span>');
                document.write('<span id="two"></span>');
                return Array.from(document.head.children).map(e => e.id).join(',');
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("writer,one,two"));
    }

    #[test]
    fn document_write_reports_to_mutation_observers() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__seen = [];
                const observer = new MutationObserver((records) => {
                  for (const record of records) {
                    for (const node of record.addedNodes) globalThis.__seen.push(node.nodeName);
                  }
                });
                observer.observe(document.body, { childList: true });
                document.write('<span id="watched">z</span>');
                observer.takeRecords().forEach((record) => {
                  for (const node of record.addedNodes) globalThis.__seen.push(node.nodeName);
                });
                return globalThis.__seen.join(',');
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("SPAN"));
    }

    /// A Continue resolution that rewrites the request URL must pass the same
    /// SSRF gate as the original request and as redirect hops — otherwise a
    /// rewrite to an internal address bypasses validate_fetch_url entirely.
    #[tokio::test(flavor = "current_thread")]
    async fn test_intercept_url_rewrite_is_revalidated_against_ssrf() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let mut rt = setup_runtime("<html><body></body></html>");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        rt.set_intercept_tx(tx);
        rt.set_intercept_enabled(true);

        // Answer every intercepted request with a rewrite to a loopback address.
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.resolver.send(crate::diting_js::ops::InterceptResolution::Continue {
                    url: Some("http://127.0.0.1:9/secret".to_string()),
                    method: None,
                    headers: None,
                    body: None,
                });
            }
        });

        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                try {
                    await fetch("http://example.com/data.json");
                    return "not-blocked";
                } catch (e) {
                    return "blocked:" + (e && e.message);
                }
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();

        let v = result.value.unwrap();
        assert_eq!(v, serde_json::json!("blocked:net::ERR_FAILED"));
    }

    /// The taobao shop-SPA report (0.3.0): a blocked or CORS-refused fetch
    /// left NO row in the network log, so a page whose API calls all died at
    /// the SSRF gate read as "never issued a request". The early exit must
    /// record a status-0 js_network_event carrying the reason.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn ssrf_blocked_fetch_records_status_zero_event_with_reason() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        await fetch("http://127.0.0.1:9/secret");
                        return "not-blocked";
                    } catch (e) {
                        return "rejected";
                    }
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("rejected"));

        let events = rt.take_js_network_events();
        let blocked = events
            .iter()
            .find(|e| e.url.contains("127.0.0.1:9"))
            .expect("SSRF-blocked fetch must leave a network event");
        assert_eq!(blocked.status, 0);
        let error = blocked.error.as_deref().unwrap_or("");
        assert!(
            error.contains("private/internal IP address"),
            "event must carry the block reason, got: {error}"
        );
    }

    /// The punished-mtop shape: the response body arrives but carries no
    /// Access-Control-Allow-Origin, so the post-response CORS gate refuses it
    /// — status 0 to JS, and (before this fix) invisible in /network. The
    /// refusal must record a status-0 event with the CORS reason.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn cors_refused_fetch_records_status_zero_event_with_reason() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = b"{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        await fetch("http://127.0.0.1:PORT/api");
                        return "not-blocked";
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#
                .replace("PORT", &port.to_string())
                .as_str(),
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert!(msg.starts_with("rejected:Failed to fetch"), "got: {msg}");

        let events = rt.take_js_network_events();
        let refused = events
            .iter()
            .find(|e| e.url.contains(&format!(":{port}/api")))
            .expect("CORS-refused fetch must leave a network event");
        assert_eq!(refused.status, 0);
        let error = refused.error.as_deref().unwrap_or("");
        assert!(
            error.contains("CORS error"),
            "event must carry the CORS reason, got: {error}"
        );
    }

    /// Preflight permission enforcement (obscura "enforce CORS preflight
    /// permissions", 04f0475 same-hole): a preflight that allows the origin
    /// but never lists Access-Control-Allow-Methods does not consent to a
    /// cross-origin PUT — the actual request must not go out. Before the fix
    /// the origin-only check passed and the PUT reached the server anyway.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn preflight_without_allow_methods_blocks_the_actual_request() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            // Only the OPTIONS preflight should ever land here; a second
            // connection means the actual PUT leaked through and the test's
            // assertion below fires (recording it either way aids the
            // failure message).
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let req = String::from_utf8_lossy(&buf).to_string();
                seen.lock().unwrap().push(req.lines().next().unwrap_or("").to_string());
                let response = "HTTP/1.1 200 OK\r\naccess-control-allow-origin: *\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        const r = await fetch("http://127.0.0.1:PORT/api", {
                            method: "PUT",
                            headers: { "content-type": "application/json" },
                            body: "{}",
                        });
                        return "not-blocked:" + r.status;
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#
                .replace("PORT", &port.to_string())
                .as_str(),
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert!(
            msg.starts_with("rejected:CORS preflight did not allow method 'PUT'"),
            "server listed no Allow-Methods; the PUT must be refused, got: {msg}"
        );

        // The actual request never reached the wire: only the OPTIONS hop.
        let served = requests.lock().unwrap().clone();
        assert!(
            served.iter().all(|line| line.starts_with("OPTIONS")),
            "only the preflight may go out, served: {served:?}"
        );

        let events = rt.take_js_network_events();
        let refused = events
            .iter()
            .find(|e| e.url.contains(&format!(":{port}/api")))
            .expect("preflight-refused fetch must leave a network event");
        assert_eq!(refused.status, 0);
        let error = refused.error.as_deref().unwrap_or("");
        assert!(
            error.contains("did not allow method 'PUT'"),
            "event must carry the preflight reason, got: {error}"
        );
    }

    /// The positive companion: when the preflight DOES list the method and
    /// headers, the actual request proceeds and resolves. Guards against the
    /// enforcement over-blocking (the safelist value check must not reject
    /// what the preflight explicitly allows).
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn preflight_that_allows_the_method_lets_the_request_through() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            // Hop 1: the OPTIONS preflight with a full consent set.
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let preflight = concat!(
                "HTTP/1.1 204 No Content\r\n",
                "access-control-allow-origin: *\r\n",
                "access-control-allow-methods: PUT, POST\r\n",
                "access-control-allow-headers: content-type\r\n",
                "access-control-max-age: 600\r\n",
                "content-length: 0\r\nconnection: close\r\n\r\n",
            );
            stream.write_all(preflight.as_bytes()).unwrap();
            stream.flush().unwrap();
            // Hop 2: the actual PUT.
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let body = b"ok";
            let response = format!(
                "HTTP/1.1 200 OK\r\naccess-control-allow-origin: *\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        const r = await fetch("http://127.0.0.1:PORT/api", {
                            method: "PUT",
                            headers: { "content-type": "application/json" },
                            body: "{}",
                        });
                        return "ok:" + r.status + ":" + (await r.text());
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#
                .replace("PORT", &port.to_string())
                .as_str(),
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert_eq!(msg, "ok:200:ok", "consented preflight must let the PUT through, got: {msg}");
    }

    /// Redirect credential retention (obscura#967 same hole): a redirect
    /// chain that never leaves the origin keeps the scripted Authorization
    /// header on every hop. Guards against over-stripping implementations
    /// that drop credentials after ANY redirect.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn same_origin_redirect_keeps_scripted_credentials() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                seen.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
                let response = match seen.lock().unwrap().len() {
                    1 => concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "access-control-allow-origin: *\r\n",
                        "access-control-allow-methods: GET\r\n",
                        "access-control-allow-headers: authorization\r\n",
                        "content-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .to_string(),
                    // The 302 itself carries ACAO because a cors-tainted
                    // request CORS-checks every response in the chain.
                    2 => format!(
                        "HTTP/1.1 302 Found\r\naccess-control-allow-origin: *\r\nlocation: /data\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    ),
                    _ => {
                        let body = b"done";
                        format!(
                            "HTTP/1.1 200 OK\r\naccess-control-allow-origin: *\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        ) + std::str::from_utf8(body).unwrap()
                    }
                };
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        const r = await fetch("http://127.0.0.1:PORT/hop", {
                            headers: { "Authorization": "Bearer token123" },
                        });
                        return "ok:" + r.status + ":" + (await r.text());
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#
                .replace("PORT", &port.to_string())
                .as_str(),
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert_eq!(msg, "ok:200:done", "same-origin redirect chain must complete, got: {msg}");

        let hops = requests.lock().unwrap();
        assert_eq!(hops.len(), 3, "expected preflight + /hop + /data");
        let data_hop = hops[2].to_ascii_lowercase();
        assert!(
            data_hop.starts_with("get /data"),
            "third hop must be the redirected GET, got: {}",
            hops[2].lines().next().unwrap_or("")
        );
        assert!(
            data_hop.contains("authorization: bearer token123"),
            "same-origin redirect must keep the Authorization header, got: {data_hop}"
        );
    }

    /// Redirect credential stripping (obscura#967 same hole): once the chain
    /// crosses origins the scripted Authorization header must stop riding —
    /// it went out on the first hop (that origin was told at fetch time) but
    /// must never reach the second, different origin.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn cross_origin_redirect_strips_scripted_credentials() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener_a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_a = listener_a.local_addr().unwrap().port();
        let requests_a = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_a = requests_a.clone();
        let listener_b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_b = listener_b.local_addr().unwrap().port();
        let requests_b = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_b = requests_b.clone();

        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener_a.accept() else { return };
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                seen_a.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
                let response = match seen_a.lock().unwrap().len() {
                    1 => concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "access-control-allow-origin: *\r\n",
                        "access-control-allow-methods: GET\r\n",
                        "access-control-allow-headers: authorization\r\n",
                        "content-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .to_string(),
                    _ => format!(
                        "HTTP/1.1 302 Found\r\naccess-control-allow-origin: *\r\nlocation: http://127.0.0.1:{}/data\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        port_b
                    ),
                };
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut stream, _)) = listener_b.accept() else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            seen_b.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
            let body = b"secret";
            let response = format!(
                "HTTP/1.1 200 OK\r\naccess-control-allow-origin: *\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        const r = await fetch("http://127.0.0.1:PORTA/hop", {
                            headers: { "Authorization": "Bearer secret-token" },
                        });
                        return "ok:" + r.status + ":" + (await r.text());
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#
                .replace("PORTA", &port_a.to_string())
                .as_str(),
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert_eq!(msg, "ok:200:secret", "redirect chain must complete, got: {msg}");

        let hops_a = requests_a.lock().unwrap();
        assert_eq!(hops_a.len(), 2, "origin A sees the preflight and the first GET");
        let first_hop = hops_a[1].to_ascii_lowercase();
        assert!(
            first_hop.contains("authorization: bearer secret-token"),
            "the initial cross-origin request carries the header it was given, got: {first_hop}"
        );

        let hops_b = requests_b.lock().unwrap();
        assert_eq!(hops_b.len(), 1, "origin B sees exactly the redirected GET");
        let second_hop = hops_b[0].to_ascii_lowercase();
        assert!(
            second_hop.starts_with("get /data"),
            "hop 2 must be the redirected GET, got: {}",
            hops_b[0].lines().next().unwrap_or("")
        );
        assert!(
            !second_hop.contains("authorization"),
            "cross-origin redirect must strip the Authorization header, got: {second_hop}"
        );
    }

    /// Redirect CORS enforcement (obscura#973 same hole): with a cors-tainted
    /// request, the 302 response itself must pass the CORS check before the
    /// follow happens — a redirect response without
    /// Access-Control-Allow-Origin blocks the fetch (status 0 to JS) and the
    /// second hop never goes out.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn cross_origin_redirect_without_acao_is_blocked() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            // Exactly one connection: a second one would mean the redirected
            // GET leaked past the redirect-response CORS check.
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{}/data\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                port
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        await fetch("http://127.0.0.1:PORT/hop");
                        return "not-blocked";
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#
                .replace("PORT", &port.to_string())
                .as_str(),
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert!(msg.starts_with("rejected:Failed to fetch"), "got: {msg}");

        let events = rt.take_js_network_events();
        let blocked = events
            .iter()
            .find(|e| e.error.as_deref().unwrap_or("").contains("redirect"))
            .expect("blocked redirect must leave a network event");
        assert_eq!(blocked.status, 0);
        let error = blocked.error.as_deref().unwrap_or("");
        assert!(
            error.contains("CORS error"),
            "event must carry the redirect-CORS reason, got: {error}"
        );
    }

    /// Redirect downgrade (obscura#973 family, body half): a POST that lands
    /// on a 302 re-issues as GET with the body headers gone — content-type
    /// describes a body that no longer exists.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn redirect_downgrade_drops_body_headers() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                seen.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
                let response = match seen.lock().unwrap().len() {
                    1 => concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "access-control-allow-origin: *\r\n",
                        "access-control-allow-methods: POST\r\n",
                        "access-control-allow-headers: content-type\r\n",
                        "content-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .to_string(),
                    2 => format!(
                        "HTTP/1.1 302 Found\r\naccess-control-allow-origin: *\r\nlocation: /data\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    ),
                    _ => {
                        let body = b"moved";
                        format!(
                            "HTTP/1.1 200 OK\r\naccess-control-allow-origin: *\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        ) + std::str::from_utf8(body).unwrap()
                    }
                };
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        const r = await fetch("http://127.0.0.1:PORT/hop", {
                            method: "POST",
                            headers: { "content-type": "application/json" },
                            body: "{\"a\":1}",
                        });
                        return "ok:" + r.status + ":" + (await r.text());
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#
                .replace("PORT", &port.to_string())
                .as_str(),
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert_eq!(msg, "ok:200:moved", "redirected POST must complete as GET, got: {msg}");

        let hops = requests.lock().unwrap();
        assert_eq!(hops.len(), 3, "expected preflight + POST + redirected GET");
        assert!(
            hops[1].starts_with("POST /hop"),
            "hop 1 must be the POST, got: {}",
            hops[1].lines().next().unwrap_or("")
        );
        let downgraded = hops[2].to_ascii_lowercase();
        assert!(
            downgraded.starts_with("get /data"),
            "302 must downgrade POST to GET, got: {}",
            hops[2].lines().next().unwrap_or("")
        );
        assert!(
            !downgraded.contains("content-type"),
            "the downgraded GET must drop the body's content-type, got: {downgraded}"
        );
    }

    /// navigator.sendBeacon used to be a stub that returned true without
    /// sending anything — pd-lib's three detection beacons silently vanished
    /// (proxydetect.live never settled). A beacon must leave the machine as a
    /// fire-and-forget POST; the relative URL resolves against the document.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn send_beacon_relative_url_posts_with_text_plain() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            seen.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
            let response = "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{port}/page"));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const ok = navigator.sendBeacon("/beacon", "hello");
                    await new Promise(r => setTimeout(r, 800));
                    return "queued:" + ok;
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert_eq!(msg, "queued:true", "sendBeacon must report a queued transfer");

        let hops = requests.lock().unwrap();
        assert_eq!(hops.len(), 1, "exactly one beacon must reach the wire");
        let raw = hops[0].to_ascii_lowercase();
        assert!(
            raw.starts_with("post /beacon"),
            "beacon must POST to the resolved relative URL, got: {}",
            hops[0].lines().next().unwrap_or("")
        );
        assert!(
            raw.contains("content-type: text/plain;charset=UTF-8".to_ascii_lowercase().as_str()),
            "string data must ride text/plain;charset=UTF-8, got: {raw}"
        );
        assert!(raw.contains("\r\n\r\nhello"), "string data must be the body, got: {raw}");
    }

    /// Spec content-type mapping by data type: URLSearchParams → urlencoded
    /// (fetch's own default), BufferSource → application/octet-stream (the
    /// caller must supply it — fetch does not), Blob → the blob's own type.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn send_beacon_body_type_to_content_type_map() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                seen.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
                let response = "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const base = "http://127.0.0.1:PORT/beacon";
                    navigator.sendBeacon(base + "?k=urlsp", new URLSearchParams("a=1&b=2"));
                    navigator.sendBeacon(base + "?k=binary", new Uint8Array([104, 105]));
                    navigator.sendBeacon(base + "?k=blob", new Blob(["csv,data"], { type: "text/csv" }));
                    await new Promise(r => setTimeout(r, 1200));
                    return "queued";
                }"#
                .replace("PORT", &port.to_string())
                .as_str(),
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert_eq!(msg, "queued");

        let hops = requests.lock().unwrap();
        assert_eq!(hops.len(), 3, "all three beacons must reach the wire");
        let find = |needle: &str| hops.iter().any(|h| h.to_ascii_lowercase().contains(needle));
        assert!(
            find("content-type: application/x-www-form-urlencoded;charset=utf-8") && find("\r\n\r\na=1&b=2"),
            "URLSearchParams must ride the urlencoded content-type with its serialized body, got: {hops:?}"
        );
        assert!(
            find("content-type: application/octet-stream") && find("\r\n\r\nhi"),
            "BufferSource must ride octet-stream with its raw bytes, got: {hops:?}"
        );
        assert!(
            find("content-type: text/csv") && find("\r\n\r\ncsv,data"),
            "Blob must ride its own type, got: {hops:?}"
        );
    }

    /// An unresolvable URL fails synchronously with false — the caller gets to
    /// fall back to XHR, exactly like a real browser (no queued, no network).
    #[tokio::test(flavor = "current_thread")]
    async fn send_beacon_invalid_url_returns_false() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"() => {
                    return "bad:" + navigator.sendBeacon("http://[bad", "x");
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert_eq!(msg, "bad:false", "an unresolvable URL must fail synchronously with false");
    }

    /// Preflight validation order (obscura#973 same hole): the preflight's
    /// HTTP status is checked before its CORS headers, so a 403 preflight
    /// reports the status — not a misleading "origin not allowed" error.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn preflight_http_status_reported_before_origin() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let response = "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("https://example.com/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        await fetch("http://127.0.0.1:PORT/api", { method: "PUT" });
                        return "not-blocked";
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#
                .replace("PORT", &port.to_string())
                .as_str(),
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert!(
            msg.contains("CORS preflight returned HTTP 403"),
            "403 preflight must report its status, got: {msg}"
        );
        assert!(
            !msg.contains("not in Access-Control-Allow-Origin"),
            "status check must fire before the origin check, got: {msg}"
        );
    }

    /// op_fetch_url walks redirects on a raw reqwest client — the one
    /// subresource path that had no Tier2 legacy-TLS fallback (the
    /// g.alicdn.com shape: plain rustls dies on the handshake, the stealth
    /// stack's BoringSSL connects). A closed port fails both transports
    /// fast; the JS-visible rejection must carry the legacy marker for GET
    /// (fallback fired).
    #[cfg(feature = "stealth")]
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_transport_failure_falls_back_to_legacy_tls() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_http_client(std::sync::Arc::new(crate::diting_net::HttpClient::with_full_options(
            std::sync::Arc::new(crate::diting_net::CookieJar::new()),
            None,
            true,
        )));

        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        await fetch("http://127.0.0.1:1/dead");
                        return "unexpectedly-resolved";
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert!(
            msg.contains("legacy TLS transport"),
            "GET transport failure must surface the legacy fallback attempt, got: {msg}"
        );
    }

    /// Same closed-port probe with a POST: connection refused is
    /// connect-stage — the request never left the machine — so the fallback
    /// fires for the body-carrying method too and the marker must appear.
    /// The post-send arm (no retry, double-submit guard) is pinned at the
    /// client layer; this e2e pins the op's `is_connect` gate feeding it.
    #[cfg(feature = "stealth")]
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_transport_failure_post_rides_connect_stage_fallback() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_http_client(std::sync::Arc::new(crate::diting_net::HttpClient::with_full_options(
            std::sync::Arc::new(crate::diting_net::CookieJar::new()),
            None,
            true,
        )));

        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    try {
                        await fetch("http://127.0.0.1:1/submit", { method: "POST", body: "k=v" });
                        return "unexpectedly-resolved";
                    } catch (e) {
                        return "rejected:" + (e && e.message);
                    }
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let msg = result.value.unwrap().as_str().unwrap_or_default().to_string();
        assert!(
            msg.starts_with("rejected:") && msg.contains("legacy TLS transport"),
            "connect-stage POST must surface the legacy fallback attempt, got: {msg}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_fetch_url_input_decodes_binary_body_base64() {
        // Serves a binary body from a real local server: the bootstrap deletes
        // the `Deno` global (stealth), so the op cannot be monkey-patched from
        // JS. URL-object input resolves against document.URL, and the binary
        // body must reach JS intact via the op's base64 envelope.
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (path_tx, path_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request.lines().next().unwrap_or("").to_string();
            let body = [0u8, 97, 115, 109, 1, 0, 0, 0];
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/wasm\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
            path_tx.send(path).unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/test", port));
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const response = await fetch(new URL("/pkg/app_bg.wasm", document.URL));
                return {
                    status: response.status,
                    bytes: Array.from(new Uint8Array(await response.arrayBuffer())),
                };
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "status": 200,
                "bytes": [0, 97, 115, 109, 1, 0, 0, 0],
            })
        );
        let request_line = path_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            request_line.starts_with("GET /pkg/app_bg.wasm "),
            "server should see the resolved URL path, got: {}",
            request_line
        );
    }

    /// obscura #754/#716 class: XHR `responseType: "arraybuffer"`/`"blob"`
    /// must round-trip the raw response bytes. The old path took `resp.text()`
    /// (lossy UTF-8) and re-encoded it, mangling every non-UTF-8 byte — PNG
    /// magic came back with 0x89/0x1A replaced by U+FFFD (EF BF BD).
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn xhr_binary_response_types_roundtrip_bytes() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).unwrap();
                let body: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
                stream.flush().unwrap();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/test", port));
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const asType = (type) => new Promise((resolve, reject) => {
                    const xhr = new XMLHttpRequest();
                    xhr.open("GET", "/img.png");
                    xhr.responseType = type;
                    xhr.onload = () => resolve(xhr.response);
                    xhr.onerror = () => reject(new Error("xhr error"));
                    xhr.send();
                });
                const bytes = Array.from(new Uint8Array(await asType("arraybuffer")));
                const blobBytes = Array.from(new Uint8Array(await (await asType("blob")).arrayBuffer()));
                return { bytes, blobBytes };
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let png_magic = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "bytes": png_magic,
                "blobBytes": png_magic,
            })
        );
    }

    /// 0.4.1 taobao report problem 5: the Ali SDK opens XHRs with lowercase
    /// methods (`xhr.open('get', …)`), and the network layer compares methods
    /// case-sensitively — so the request went out literally `get`, breaking
    /// CORS safelist matching. open() now uppercases a token that
    /// byte-uppercases to a standard method (Fetch-spec "normalize a
    /// method"); custom tokens ride as authored, as in Chrome.
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn xhr_open_normalizes_method_case() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (line_tx, line_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]);
                let line = request.lines().next().unwrap_or("").to_string();
                let body = b"ok";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
                stream.flush().unwrap();
                line_tx.send(line).unwrap();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/test", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const send = (m) => new Promise((resolve, reject) => {
                        const xhr = new XMLHttpRequest();
                        xhr.open(m, "/probe");
                        xhr.onload = () => resolve(xhr.status);
                        xhr.onerror = () => reject(new Error("xhr error"));
                        xhr.send();
                    });
                    return [await send("get"), await send("put"), await send("x-custom")];
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(result.value.unwrap(), serde_json::json!([200, 200, 200]));
        // What the wire actually saw: standard tokens uppercased, custom as-is.
        let lines: Vec<String> = (0..3)
            .map(|_| line_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
            .collect();
        assert!(lines[0].starts_with("GET /probe "), "got: {}", lines[0]);
        assert!(lines[1].starts_with("PUT /probe "), "got: {}", lines[1]);
        assert!(lines[2].starts_with("x-custom /probe "), "got: {}", lines[2]);
    }

    /// obscura#664 class: the fetch/XHR redirect budget is the Fetch spec's
    /// fixed 20 — WPT `fetch/api/redirect/redirect-count.any.js` pins both
    /// ends: the 20th hop succeeds, the 21st fails. (HTTP-3xx during
    /// document navigation and the JS navigation-chain document cap are
    /// separate budgets; the chain cap lives in diting_browser/page.rs and
    /// counts documents, not redirects.)
    #[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_redirect_count_matches_wpt_pair() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        // /r/N → 302 → /r/{N-1}; /r/0 is the terminal document.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..48 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let path = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/r/0")
                    .to_string();
                let resp = match path.strip_prefix("/r/").and_then(|n| n.parse::<u32>().ok()) {
                    Some(0) => "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 9\r\nconnection: close\r\n\r\nchain-end"
                        .to_string(),
                    Some(n) => format!(
                        "HTTP/1.1 302 Found\r\nlocation: /r/{}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        n - 1
                    ),
                    None => "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string(),
                };
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/r/0", port));
        let result = rt.call_function_on_for_cdp(
                r#"async () => {
                    const outcomes = {};
                    try {
                        const r = await fetch(new URL("/r/20", document.URL));
                        outcomes.twenty = r.status + ":" + (await r.text());
                    } catch (e) { outcomes.twenty = "rejected"; }
                    try {
                        const r = await fetch(new URL("/r/21", document.URL));
                        outcomes.twentyOne = r.status + ":" + (await r.text());
                    } catch (e) { outcomes.twentyOne = "rejected"; }
                    return outcomes;
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let outcomes = result.value.unwrap();
        assert_eq!(
            outcomes["twenty"],
            serde_json::json!("200:chain-end"),
            "the 20th redirect hop must still succeed (http-redirect-fetch step 7: count 20 passes)"
        );
        assert_eq!(
            outcomes["twentyOne"],
            serde_json::json!("rejected"),
            "the 21st redirect hop must fail (count 21 → network error)"
        );
    }

    /// Upstream #581 class: op_fetch_url buffered the entire response with
    /// `.bytes().await` before any limit was consulted, so page JS fetching
    /// a multi-GB body OOMed the process (the retained-for-CDP byte limit
    /// only gates the cache, not the allocation). The cap must reject as a
    /// network-style failure — both when Content-Length advertises it and
    /// when an unbounded stream crosses it mid-body.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_rejects_response_body_over_limit() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        std::env::set_var("AGINXBROWSER_FETCH_BODY_LIMIT", "1024");

        // Advertised Content-Length over the cap: rejected before buffering.
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 99999999\r\nconnection: close\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            });
            let mut rt = setup_runtime("<html><body></body></html>");
            rt.set_url(&format!("http://127.0.0.1:{}/huge", port));
            let result = rt
                .call_function_on_for_cdp(
                    r#"async () => {
                        try { await fetch(document.URL); return "resolved"; }
                        catch (e) { return "rejected"; }
                    }"#,
                    None,
                    &[],
                    true,
                    true,
                )
                .await
                .unwrap();
            assert_eq!(
                result.value.unwrap(),
                serde_json::json!("rejected"),
                "oversized advertised Content-Length must reject the fetch"
            );
        }

        // No Content-Length: the stream itself must cross the cap and reject.
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes());
                // 8 KiB in small writes: an unbounded stream with no framing.
                for _ in 0..64 {
                    if stream.write_all(&[b'x'; 128]).is_err() {
                        break; // client hung up once the cap tripped
                    }
                }
                let _ = stream.flush();
            });
            let mut rt = setup_runtime("<html><body></body></html>");
            rt.set_url(&format!("http://127.0.0.1:{}/stream", port));
            let result = rt
                .call_function_on_for_cdp(
                    r#"async () => {
                        try { await fetch(document.URL); return "resolved"; }
                        catch (e) { return "rejected"; }
                    }"#,
                    None,
                    &[],
                    true,
                    true,
                )
                .await
                .unwrap();
            assert_eq!(
                result.value.unwrap(),
                serde_json::json!("rejected"),
                "unbounded stream over the cap must reject the fetch"
            );
        }

        std::env::remove_var("AGINXBROWSER_FETCH_BODY_LIMIT");
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    }

    /// obscura #849 class: the ES module loader is a second page-controlled
    /// fetch path, so the #581 body cap must hold there too — same env knob,
    /// same streaming check, and the failure surfaces as a catchable
    /// import() rejection (not a silent empty module).
    #[tokio::test(flavor = "current_thread")]
    async fn module_import_rejects_body_over_limit() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        std::env::set_var("AGINXBROWSER_FETCH_BODY_LIMIT", "1024");

        // Advertised Content-Length over the cap: rejected before buffering.
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/javascript\r\ncontent-length: 99999999\r\nconnection: close\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            });
            let mut rt = setup_runtime("<html><body></body></html>");
            rt.set_url(&format!("http://127.0.0.1:{}/", port));
            let result = rt
                .call_function_on_for_cdp(
                    r#"async () => {
                        try { await import(document.URL + "big.mjs"); return "resolved"; }
                        catch (e) { return "rejected: " + e.message; }
                    }"#,
                    None,
                    &[],
                    true,
                    true,
                )
                .await
                .unwrap();
            let v = result.value.unwrap();
            assert_eq!(
                v,
                serde_json::json!(
                    "rejected: Module http://127.0.0.1:PORT/big.mjs response body too large: content-length 99999999 exceeds limit 1024 bytes"
                    .replace("PORT", &port.to_string())
                ),
                "oversized advertised Content-Length must reject the import before buffering"
            );
        }

        // No Content-Length: the stream itself must cross the cap and reject.
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/javascript\r\nconnection: close\r\n\r\nexport const ok = true;\n";
                let _ = stream.write_all(resp.as_bytes());
                for _ in 0..64 {
                    if stream.write_all(&[b'x'; 128]).is_err() {
                        break; // client hung up once the cap tripped
                    }
                }
                let _ = stream.flush();
            });
            let mut rt = setup_runtime("<html><body></body></html>");
            rt.set_url(&format!("http://127.0.0.1:{}/", port));
            let result = rt
                .call_function_on_for_cdp(
                    r#"async () => {
                        try { await import(document.URL + "stream.mjs"); return "resolved"; }
                        catch (e) { return "rejected: " + e.message; }
                    }"#,
                    None,
                    &[],
                    true,
                    true,
                )
                .await
                .unwrap();
            let v = result.value.unwrap();
            let msg = v.as_str().unwrap();
            assert!(
                msg.starts_with("rejected: ") && msg.contains("exceeded limit of 1024 bytes"),
                "unbounded stream over the cap must reject the import, got: {}",
                msg
            );
        }

        std::env::remove_var("AGINXBROWSER_FETCH_BODY_LIMIT");
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    }

    /// A module above obscura's hardcoded 33554432-byte cap must load fine
    /// under our default (the Angular 18 `main` bundle from the issue is
    /// 37.9 MB) — the knob is the only policy, and the default sits above
    /// routine production bundles.
    #[tokio::test(flavor = "current_thread")]
    async fn module_import_allows_body_above_upstream_cap() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        std::env::remove_var("AGINXBROWSER_FETCH_BODY_LIMIT");

        const BIG: usize = 34 * 1024 * 1024; // 34 MiB > upstream's 32 MiB cap
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            // A 34 MiB line comment: the full body must be downloaded (that
            // is the point) without V8 parsing megabytes of tokens.
            let body = format!("//{}\nglobalThis.__BIG_MODULE__ = true;\nexport const ok = true;\n", "x".repeat(BIG));
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/javascript\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        });
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    await import(document.URL + "big.mjs");
                    return "resolved:" + (globalThis.__BIG_MODULE__ === true);
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!("resolved:true"),
            "a 34 MiB module must load and execute under the default cap"
        );

        std::env::remove_var("AGINXBROWSER_FETCH_BODY_LIMIT");
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    }

    /// DOMTokenList.supports() must answer for the lists the spec gives
    /// supported tokens (<link> relList, iframe sandbox) and throw for the
    /// rest (classList, a/area relList). Resource loaders feature-probe via
    /// relList.supports("preload") — an unguarded throw there aborts the
    /// whole load chain (2captcha's turnstile loader died exactly this way).
    #[tokio::test(flavor = "current_thread")]
    async fn tokenlist_supported_sets_match_spec() {
        let mut rt = setup_runtime(
            r##"<html><body><a id="x" href="#">a</a><iframe id="f" sandbox="allow-scripts"></iframe><link id="l" rel="stylesheet" href="s.css"></body></html>"##,
        );
        let result = rt
            .evaluate_for_cdp(
                r#"(() => {
                    const out = {};
                    const link = document.getElementById("l");
                    const anchor = document.getElementById("x");
                    const frame = document.getElementById("f");
                    out.linkPreload = link.relList.supports("preload");
                    out.linkModulepreload = link.relList.supports("modulepreload");
                    out.linkNonsense = link.relList.supports("nonsense");
                    try { anchor.relList.supports("preload"); out.anchorThrows = false; }
                    catch (e) { out.anchorThrows = e instanceof TypeError; }
                    out.sandboxScripts = frame.sandbox.supports("allow-scripts");
                    out.sandboxBogus = frame.sandbox.supports("allow-bogus");
                    try { document.body.classList.supports("x"); out.classThrows = false; }
                    catch (e) { out.classThrows = e instanceof TypeError; }
                    return JSON.stringify(out);
                })()"#,
                false,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!(
                r#"{"linkPreload":true,"linkModulepreload":true,"linkNonsense":false,"anchorThrows":true,"sandboxScripts":true,"sandboxBogus":false,"classThrows":true}"#
            ),
            "supported-token sets must match the spec/Chrome shape per element kind"
        );
    }
    /// fetch(): file:// and private/internal hosts rejected, private hosts
    /// reachable again once the operator opts in.
    #[tokio::test(flavor = "current_thread")]
    async fn module_import_honors_fetch_url_policy() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        std::env::remove_var("AGINXBROWSER_FETCH_BODY_LIMIT");

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url("http://example.invalid/page");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const out = {};
                    try { await import("file:///etc/hostname"); out.file = "resolved"; }
                    catch (e) { out.file = e.message; }
                    try { await import("http://127.0.0.1:9/x.mjs"); out.loopback = "resolved"; }
                    catch (e) { out.loopback = e.message; }
                    return out;
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        let out = result.value.unwrap();
        assert!(
            out["file"].as_str().unwrap().contains("Forbidden URL scheme 'file'"),
            "file:// import must be rejected by scheme, got: {}",
            out["file"]
        );
        assert!(
            out["loopback"].as_str().unwrap().contains("Access to private/internal"),
            "loopback import must be rejected by the private-network policy, got: {}",
            out["loopback"]
        );

        // Operator opt-in reopens private hosts for the module path too.
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = "globalThis.__PRIVATE_OK__ = true;\nexport const ok = true;\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/javascript\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        });
        rt.set_url(&format!("http://127.0.0.1:{}/", port));
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    await import(document.URL + "local.mjs");
                    return "resolved:" + (globalThis.__PRIVATE_OK__ === true);
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!("resolved:true"),
            "with the opt-in env set, a loopback module must load"
        );

        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    }

    /// Browsers send Origin on every non-GET/HEAD request, including
    /// same-origin POSTs (SolidStart server functions 403 without it).
    /// Regression: we only set Origin cross-origin, so a same-origin POST
    /// reached the wire bare and got rejected.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_same_origin_post_sends_origin_header() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (hdr_tx, hdr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            let origin_line = request
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("origin:"))
                .unwrap_or("").to_string();
            let body = b"{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
            hdr_tx.send(origin_line).unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/submit", port));
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const r = await fetch(new URL("/_serverFn/x", document.URL), {
                    method: "POST",
                    headers: { "_h": "{\"x-tsr-serverfn\":\"true\"}" },
                    body: JSON.stringify({ _d: [["name", "AginxBrowser"]] }),
                });
                return { status: r.status };
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(result.value.unwrap(), serde_json::json!({ "status": 200 }));
        let origin_line = hdr_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            origin_line.to_ascii_lowercase().starts_with("origin: http://127.0.0.1:"),
            "same-origin POST must carry Origin, got: {:?}",
            origin_line
        );
    }

    /// Browsers send Fetch-Metadata (sec-fetch-*) and client-hint headers on
    /// scripted requests; WAFs key on `sec-fetch-site: same-origin` and 403
    /// requests without them. Regression: op_fetch_url only set User-Agent.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_same_origin_post_sends_fetch_metadata_headers() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (hdr_tx, hdr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            let lower = request.to_ascii_lowercase();
            let hdrs = [
                "sec-fetch-site",
                "sec-fetch-mode",
                "sec-fetch-dest",
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "accept",
            ].iter().map(|h| {
                let v = lower.lines().find(|l| l.starts_with(&format!("{}:", h)))
                    .unwrap_or("").trim().to_string();
                (h.to_string(), v)
            }).collect::<std::collections::HashMap<_, _>>();
            let body = b"{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
            hdr_tx.send(hdrs).unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/submit", port));
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const r = await fetch(new URL("/api", document.URL), {
                    method: "POST",
                    body: "{}",
                });
                return { status: r.status };
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(result.value.unwrap(), serde_json::json!({ "status": 200 }));
        let hdrs = hdr_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(hdrs.get("sec-fetch-site").map(String::as_str), Some("sec-fetch-site: same-origin"));
        assert_eq!(hdrs.get("sec-fetch-mode").map(String::as_str), Some("sec-fetch-mode: cors"));
        assert_eq!(hdrs.get("sec-fetch-dest").map(String::as_str), Some("sec-fetch-dest: empty"));
        assert_eq!(hdrs.get("sec-ch-ua-mobile").map(String::as_str), Some("sec-ch-ua-mobile: ?0"));
        assert!(
            hdrs.get("sec-ch-ua").map(|s| {
                let l = s.to_ascii_lowercase();
                l.contains("chromium") && l.contains("google chrome")
            }).unwrap_or(false),
            "sec-ch-ua missing, got: {:?}",
            hdrs.get("sec-ch-ua")
        );
        assert!(hdrs.get("accept").map(|s| s.contains("*/*")).unwrap_or(false));
    }

    /// The Fetch standard allows 20 redirect hops and rejects the 21st
    /// (upstream 4b90ec3). A local chain of exactly 20 must arrive; one of 21
    /// must fail.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_follows_twenty_redirects_and_rejects_twenty_one() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        fn chain_server(hops: usize) -> u16 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                for _ in 0..=hops {
                    let Ok((mut stream, _)) = listener.accept() else { return };
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let step: usize = path
                        .trim_start_matches("/hop")
                        .parse()
                        .unwrap_or(0);
                    let response = if step < hops {
                        format!(
                            "HTTP/1.1 302 Found\r\nlocation: /hop{}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            step + 1
                        )
                    } else {
                        "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
                            .to_string()
                    };
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            port
        }

        let fetch_status = |port: u16| {
            let rt = setup_runtime("<html><body></body></html>");
            rt.set_url(&format!("http://127.0.0.1:{}/", port));
            rt
        };
        let script = r#"async () => {
            try {
                const r = await fetch("/hop0");
                return "status:" + r.status;
            } catch (e) {
                return "error:" + (e && e.message);
            }
        }"#;

        let port20 = chain_server(20);
        let mut rt = fetch_status(port20);
        let ok = rt
            .call_function_on_for_cdp(script, None, &[], true, true)
            .await
            .unwrap();
        assert_eq!(ok.value.unwrap(), serde_json::json!("status:200"));

        let port21 = chain_server(21);
        let mut rt = fetch_status(port21);
        let err = rt
            .call_function_on_for_cdp(script, None, &[], true, true)
            .await
            .unwrap();
        assert_eq!(err.value.unwrap(), serde_json::json!("error:net::ERR_FAILED"));

        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
    }

    /// fetch() must serialize FormData (incl. File parts with filename and
    /// Content-Type), Blob, and TypedArray bodies the way a browser does
    /// (upstream 3eb28da / 260c4c0). String(body) used to send "[object Blob]".
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_serializes_formdata_blob_and_typed_bodies() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..4 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                // Read headers, then exactly content-length body bytes.
                let mut header_end = None;
                let mut content_len = 0usize;
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 { break; }
                    raw.extend_from_slice(&buf[..n]);
                    if header_end.is_none() {
                        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = Some(pos + 4);
                            let head = String::from_utf8_lossy(&raw[..pos]);
                            for line in head.lines() {
                                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                    if let Some(end) = header_end {
                        if raw.len() >= end + content_len { break; }
                    }
                }
                req_tx.send(raw).unwrap();
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                );
                let _ = stream.flush();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/", port));
        let result = rt.call_function_on_for_cdp(
            r#"async (port) => {
                const out = [];
                const run = async (tag, fn) => { try { out.push(tag + ":" + (await fn()).status); } catch (e) { out.push(tag + "!:" + (e && (e.message || e.name))); } };
                const fd = new FormData();
                fd.append("field", "value");
                fd.append("upload", new File([new Uint8Array([1, 2, 3])], "a.bin", { type: "application/octet-stream" }));
                await run("plain", () => fetch("http://127.0.0.1:" + port + "/plain", { method: "POST", body: "x=1" }));
                await run("fd", () => fetch("http://127.0.0.1:" + port + "/fd", { method: "POST", body: fd }));
                await run("blob", () => fetch("http://127.0.0.1:" + port + "/blob", { method: "POST", body: new Blob(["hello"], { type: "text/plain" }) }));
                await run("typed", () => fetch("http://127.0.0.1:" + port + "/typed", { method: "POST", body: new Uint8Array([65, 66, 67]) }));
                return out.join("|");
            }"#,
            None,
            &[serde_json::json!({ "value": port })],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(result.value.unwrap(), serde_json::json!("plain:200|fd:200|blob:200|typed:200"));

        let plain_raw = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(plain_raw.ends_with(b"x=1"), "plain body mismatch: {:?}", plain_raw);

        let fd_req = String::from_utf8_lossy(&req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap()).into_owned();
        assert!(fd_req.contains("content-type: multipart/form-data; boundary="), "missing multipart header: {}", fd_req);
        assert!(fd_req.contains("name=\"field\"\r\n\r\nvalue"), "missing field part: {}", fd_req);
        assert!(fd_req.contains("filename=\"a.bin\""), "missing filename: {}", fd_req);
        assert!(fd_req.contains("application/octet-stream"), "missing part content-type: {}", fd_req);

        let blob_raw = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let blob_req = String::from_utf8_lossy(&blob_raw).into_owned();
        assert!(blob_req.contains("content-type: text/plain"), "missing blob content-type: {}", blob_req);
        assert!(blob_req.ends_with("hello"), "blob body mismatch: {}", blob_req);

        let typed_raw = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(typed_raw.ends_with(b"ABC"), "typed body mismatch: {:?}", typed_raw);
    }

    /// Binary request bodies must arrive byte-exact. The deno `#[string]`
    /// boundary used to UTF-8-encode the Latin-1 binary-string body channel,
    /// corrupting `[0,128,255]` into `[0,194,128,195,191]` (upstream obscura
    /// #716). Bodies are now base64-encoded in the JS shim (ASCII-safe across
    /// that boundary) and decoded in `op_fetch_url`, so non-ASCII bytes survive
    /// intact across the typed-array, Blob and multipart File paths.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_binary_bodies_are_byte_exact() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let mut header_end = None;
                let mut content_len = 0usize;
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 { break; }
                    raw.extend_from_slice(&buf[..n]);
                    if header_end.is_none() {
                        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = Some(pos + 4);
                            let head = String::from_utf8_lossy(&raw[..pos]);
                            for line in head.lines() {
                                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                    if let Some(end) = header_end {
                        if raw.len() >= end + content_len { break; }
                    }
                }
                req_tx.send(raw).unwrap();
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                );
                let _ = stream.flush();
            }
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/", port));
        let result = rt.call_function_on_for_cdp(
            r#"async (port) => {
                const u8 = new Uint8Array([0, 128, 255, 16]);
                await fetch("http://127.0.0.1:" + port + "/typed", { method: "POST", body: u8 });
                await fetch("http://127.0.0.1:" + port + "/blob", { method: "POST", body: new Blob([u8]) });
                const fd = new FormData();
                fd.append("f", new File([u8], "b.bin", { type: "application/octet-stream" }));
                await fetch("http://127.0.0.1:" + port + "/fd", { method: "POST", body: fd });
                return "ok";
            }"#,
            None,
            &[serde_json::json!({ "value": port })],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        assert_eq!(result.value.unwrap(), serde_json::json!("ok"));

        let body_bytes = |raw: &[u8]| -> Vec<u8> {
            let pos = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(0);
            raw[pos..].to_vec()
        };

        let typed = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(body_bytes(&typed), vec![0, 128, 255, 16], "typed array body corrupted: {:?}", &typed[typed.len().saturating_sub(16)..]);

        let blob = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(body_bytes(&blob), vec![0, 128, 255, 16], "blob body corrupted");

        let fd = req_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let needle = [0u8, 128, 255, 16];
        assert!(
            fd.windows(4).any(|w| w == needle),
            "multipart file part corrupted: {:?}",
            &fd[fd.len().saturating_sub(48)..]
        );
    }

    /// Request.formData() / Response.formData(): react-router and remix route
    /// actions open every form submission with request.formData() — the missing
    /// method wedged the client action chain behind "e.formData is not a
    /// function" (glama.ai admin form). Covers the FormData-body passthrough,
    /// urlencoded pair parsing, multipart parts (File entries), and the spec
    /// TypeError on a non-form content-type.
    #[tokio::test(flavor = "current_thread")]
    async fn request_response_formdata_parse() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate(r#"
            globalThis.__out = [];
            const fd = new FormData();
            fd.append("intent", "build");
            fd.append("steps", '["a","b"]');
            new Request("https://x.test/post", { method: "POST", body: fd })
                .formData()
                .then((f) => {
                    __out.push(f.get("intent") === "build", f.get("steps") === '["a","b"]');
                    // react-router converts file-less form submissions to a
                    // URLSearchParams body (default urlencoded encType) and the
                    // browser infers the content-type — the engine must too.
                    const uspReq = new Request("https://x.test/post", { method: "POST", body: new URLSearchParams("a=1&b=x+y") });
                    __out.push(uspReq.headers.get("content-type") === "application/x-www-form-urlencoded;charset=UTF-8");
                    return uspReq.formData();
                })
                .then((f) => {
                    __out.push(f.get("a") === "1", f.get("b") === "x y");
                    return new Request("https://x.test/post", {
                        method: "POST",
                        headers: { "content-type": "application/x-www-form-urlencoded;charset=UTF-8" },
                        body: "a=1&b=x+y&c=%E4%B8%AD",
                    }).formData();
                })
                .then((f) => {
                    __out.push(f.get("a") === "1", f.get("b") === "x y", f.get("c") === "中");
                    const b = "----ditingT";
                    const mp = [
                        "--" + b,
                        'Content-Disposition: form-data; name="field"',
                        "",
                        "plain",
                        "--" + b,
                        'Content-Disposition: form-data; name="up"; filename="a.bin"',
                        "Content-Type: application/octet-stream",
                        "",
                        "BIN",
                        "--" + b + "--",
                        "",
                    ].join("\r\n");
                    return new Response(mp, {
                        headers: { "content-type": "multipart/form-data; boundary=" + b },
                    }).formData();
                })
                .then((f) => {
                    const up = f.get("up");
                    __out.push(f.get("field") === "plain", up instanceof File, up.name === "a.bin", up.type === "application/octet-stream");
                    return new Request("https://x.test/post", {
                        method: "POST", body: "hi", headers: { "content-type": "text/plain" },
                    }).formData().then(() => "no-throw", (e) => (e instanceof TypeError ? "TypeError" : "other"));
                })
                .then((v) => { __out.push(v); })
                .catch((e) => { __out.push("ERR:" + (e && (e.message || e.name))); });
        "#).unwrap();
        let _ = rt.run_event_loop_bounded(300).await;
        let result = rt.evaluate("globalThis.__out").unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, true, true, true, true, true, true, true, true, true, true, true, "TypeError"])
        );
    }

    /// RequestCredentials end-to-end (upstream b744b9b): same-origin (the
    /// default) neither sends nor stores cookies cross-origin; "include" does
    /// both, and a credentialed CORS response without Allow-Credentials +
    /// exact origin is blocked.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_honors_request_credentials_across_origins() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        use std::io::{Read, Write};
        fn read_request(stream: &mut std::net::TcpStream) -> String {
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            String::from_utf8_lossy(&buf[..n]).into_owned()
        }
        fn cookie_header(req: &str) -> String {
            req.lines()
                .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
                .map(|l| l[7..].trim().to_string())
                .unwrap_or_default()
        }

        // Page origin: stores a cookie so the same-origin store path runs.
        let listener_a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_a = listener_a.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener_a.accept().unwrap();
            read_request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nset-cookie: a=1; Path=/\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").unwrap();
        });

        // Cross origin B: mirrors CORS for the page origin, sets b=1 each time.
        let listener_b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_b = listener_b.local_addr().unwrap().port();
        let (cookie_tx, cookie_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let origin = format!("http://127.0.0.1:{}", port_a);
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener_b.accept() else { return };
                let req = read_request(&mut stream);
                cookie_tx.send(cookie_header(&req)).unwrap();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\naccess-control-allow-origin: {}\r\naccess-control-allow-credentials: true\r\nset-cookie: b=1; Path=/\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    origin
                );
                stream.write_all(resp.as_bytes()).unwrap();
            }
        });

        // Cross origin C: wildcard ACAO without Allow-Credentials — fine for
        // non-credentialed, blocked for credentials:include.
        let listener_c = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_c = listener_c.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener_c.accept().unwrap();
            read_request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\naccess-control-allow-origin: *\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").unwrap();
        });

        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port_a));
        let result = rt.call_function_on_for_cdp(
            r#"async (pa, pb, pc) => {
                const A = "http://127.0.0.1:" + pa, B = "http://127.0.0.1:" + pb, C = "http://127.0.0.1:" + pc;
                const out = [];
                await fetch(A + "/seed");
                out.push("r1:" + (await fetch(B + "/x")).status);
                out.push("r2:" + (await fetch(B + "/x", { credentials: "include" })).status);
                out.push("r3:" + (await fetch(B + "/x", { credentials: "include" })).status);
                try {
                    await fetch(C + "/x", { credentials: "include" });
                    out.push("c:ok");
                } catch (e) {
                    out.push("c:" + (e && e.message));
                }
                return out.join("|");
            }"#,
            None,
            &[
                serde_json::json!({ "value": port_a }),
                serde_json::json!({ "value": port_b }),
                serde_json::json!({ "value": port_c }),
            ],
            true,
            true,
        ).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        let expected = format!(
            "r1:200|r2:200|r3:200|c:Failed to fetch: CORS error: credentialed request requires Access-Control-Allow-Origin 'http://127.0.0.1:{}' and Access-Control-Allow-Credentials 'true'",
            port_a
        );
        assert_eq!(result.value.unwrap(), serde_json::json!(expected));
        let c1 = cookie_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let c2 = cookie_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let c3 = cookie_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        // Cookies are host-scoped (RFC 6265 ignores the port), so once
        // credentials are allowed, B receives every 127.0.0.1 cookie.
        assert_eq!((c1.as_str(), c2.as_str()), ("", "a=1"));
        assert!(c3.split("; ").any(|c| c == "b=1"), "stored cookie missing: {}", c3);
        let b_url = url::Url::parse(&format!("http://127.0.0.1:{}/", port_b)).unwrap();
        assert!(jar.get_cookie_header(&b_url).split("; ").any(|c| c == "b=1"));
    }

    /// Setting innerHTML on the <html> element parses in the "before head"
    /// insertion mode, which synthesizes head and body. The importer must keep
    /// both; it previously returned the synthesized body and dropped the head
    /// (so a <title>/<meta> assigned this way vanished).
    #[test]
    fn documentelement_inner_html_keeps_head_and_body() {
        let mut rt = setup_runtime("<html><head></head><body></body></html>");
        let v = rt
            .evaluate(
                "(function(){ document.documentElement.innerHTML = '<head><title>T</title></head><body><p>hi</p></body>'; \
                 var t = document.querySelector('title'); var p = document.querySelector('p'); \
                 return (t ? t.textContent : 'no-title') + '|' + (p ? p.textContent : 'no-p'); })()",
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("T|hi"));
    }

    /// Regression guard: innerHTML on an ordinary element still imports the
    /// parsed nodes directly (no head/body is synthesized for a div context),
    /// so the fix above must not change the common case.
    #[test]
    fn ordinary_element_inner_html_imports_content_directly() {
        let mut rt = setup_runtime("<html><body><div id=\"d\"></div></body></html>");
        let v = rt
            .evaluate(
                "(function(){ var d=document.getElementById('d'); d.innerHTML='<span>a</span><span>b</span>'; \
                 return d.children.length + '|' + d.textContent; })()",
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("2|ab"));
    }

    #[test]
    fn insert_adjacent_html_keeps_leading_comments_in_table_contexts() {
        let mut rt = setup_runtime(
            r#"<html><body><table><tbody id="tb"><tr id="row"></tr></tbody></table></body></html>"#,
        );
        let out = rt
            .evaluate(
                "(function(){var tb=document.getElementById('tb');tb.insertAdjacentHTML('beforeend','<!--m--><tr><td>v</td></tr>');var row=document.getElementById('row');row.insertAdjacentHTML('beforeend','<!--n--><td>x</td>');return Array.from(tb.childNodes).map(function(n){return n.nodeName}).join('|')+';'+Array.from(row.childNodes).map(function(n){return n.nodeName}).join('|');})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("TR|#comment|TR;#comment|TD"));
    }

    #[test]
    fn insert_adjacent_html_uses_the_insertion_element_as_context() {
        let mut rt = setup_runtime(
            r#"<html><body><div id="d"></div><table id="table"><tbody id="tb"></tbody></table></body></html>"#,
        );
        let out = rt
            .evaluate(
                "(function(){var d=document.getElementById('d');d.insertAdjacentHTML('beforeend','<tr><td>v</td></tr>');var table=document.getElementById('table');table.insertAdjacentHTML('beforeend','<tr><td>x</td></tr>');var tb=document.getElementById('tb');tb.insertAdjacentHTML('beforeend','<tr><td>y</td></tr>tail');return d.firstChild.nodeName+':'+d.textContent+';'+table.lastElementChild.tagName+';'+Array.from(tb.childNodes).map(function(n){return n.nodeName+(n.data?':'+n.data:'')}).join('|');})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("#text:v;TBODY;TR|#text:tail"));
    }

    /// tmp.childNodes is a LIVE list: indexing it while moving nodes into the
    /// document skips every other node. Regression guard for the firstChild-pop
    /// loop in insertAdjacentHTML.
    #[test]
    fn insert_adjacent_html_moves_all_sibling_nodes() {
        let mut rt = setup_runtime(r#"<html><body><div id="d"></div></body></html>"#);
        let out = rt
            .evaluate(
                "(function(){var d=document.getElementById('d');d.insertAdjacentHTML('beforeend','<span>a</span><span>b</span><span>c</span><span>d</span>');return d.children.length+'|'+d.textContent;})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("4|abcd"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_response_array_buffer_preserves_typed_array_view() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const bytes = new Uint8Array([9, 0, 97, 115, 109, 1, 8]);
                const response = new Response(bytes.subarray(1, 6));
                return Array.from(new Uint8Array(await response.arrayBuffer()));
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();

        assert_eq!(result.value.unwrap(), serde_json::json!([0, 97, 115, 109, 1]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_wasm_instantiate_streaming_uses_response_array_buffer() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.call_function_on_for_cdp(
            r#"async () => {
                const bytes = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
                const result = await WebAssembly.instantiateStreaming(
                    Promise.resolve(new Response(bytes)),
                    {},
                );
                return result.instance instanceof WebAssembly.Instance;
            }"#,
            None,
            &[],
            true,
            true,
        ).await.unwrap();

        assert_eq!(result.value.unwrap(), serde_json::json!(true));
    }

    #[test]
    fn test_text_decoder_respects_typed_array_view() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(
            "new TextDecoder().decode(new Uint8Array([65, 66, 67]).subarray(1, 2))"
        ).unwrap();
        assert_eq!(result.as_str().unwrap(), "B");
    }

    #[test]
    fn test_document_doctype() {
        let mut rt = setup_runtime("<!DOCTYPE html><html><body></body></html>");
        let result = rt.evaluate("document.doctype !== null").unwrap();
        assert_eq!(result, serde_json::json!(true));

        let name = rt.evaluate("document.doctype.name").unwrap();
        assert_eq!(name, serde_json::json!("html"));

        let node_type = rt.evaluate("document.doctype.nodeType").unwrap();
        assert_eq!(node_type.as_f64().unwrap() as i64, 10);
    }

    #[test]
    fn test_document_doctype_null_when_missing() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate("document.doctype === null").unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn test_xml_serializer_doctype() {
        let mut rt = setup_runtime("<!DOCTYPE html><html><body></body></html>");
        let result = rt.evaluate(
            "new XMLSerializer().serializeToString(document.doctype)"
        ).unwrap();
        assert_eq!(result.as_str().unwrap(), "<!DOCTYPE html>");
    }

    #[test]
    fn test_xml_serializer_element() {
        let mut rt = setup_runtime(r#"<html><body><div id="x">Hello</div></body></html>"#);
        let result = rt.evaluate(
            "new XMLSerializer().serializeToString(document.getElementById('x'))"
        ).unwrap();
        let html = result.as_str().unwrap();
        assert!(html.contains("<div"));
        assert!(html.contains("Hello"));
    }

    #[test]
    fn test_create_event_custom_event_has_init_method() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let kind = rt
            .evaluate("typeof document.createEvent('CustomEvent').initCustomEvent")
            .unwrap();
        assert_eq!(kind, serde_json::json!("function"));
    }

    #[test]
    fn test_init_custom_event_sets_fields() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "test",
            r#"
            globalThis.__e = document.createEvent('CustomEvent');
            globalThis.__e.initCustomEvent('myevent', true, false, {hello: 'world'});
        "#,
        )
        .unwrap();
        let t = rt.evaluate("globalThis.__e.type").unwrap();
        assert_eq!(t, serde_json::json!("myevent"));
        let b = rt.evaluate("globalThis.__e.bubbles").unwrap();
        assert_eq!(b, serde_json::json!(true));
        let c = rt.evaluate("globalThis.__e.cancelable").unwrap();
        assert_eq!(c, serde_json::json!(false));
        let d = rt.evaluate("globalThis.__e.detail.hello").unwrap();
        assert_eq!(d, serde_json::json!("world"));
    }

    #[test]
    fn test_create_event_returns_correct_class() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let cust = rt
            .evaluate("document.createEvent('CustomEvent') instanceof CustomEvent")
            .unwrap();
        assert_eq!(cust, serde_json::json!(true));
        let mouse = rt
            .evaluate("document.createEvent('MouseEvent') instanceof MouseEvent")
            .unwrap();
        assert_eq!(mouse, serde_json::json!(true));
        let mouses = rt
            .evaluate("document.createEvent('MouseEvents') instanceof MouseEvent")
            .unwrap();
        assert_eq!(mouses, serde_json::json!(true));
        let kb = rt
            .evaluate("document.createEvent('KeyboardEvent') instanceof KeyboardEvent")
            .unwrap();
        assert_eq!(kb, serde_json::json!(true));
    }

    #[test]
    fn test_create_event_unknown_type_returns_event() {
        // 7e6f403 flipped the contract: unknown interface names now throw
        // NotSupportedError (Chrome behavior) instead of returning a generic
        // Event whose init* methods would all be missing.
        let mut rt = setup_runtime("<html><body></body></html>");
        let kind = rt
            .evaluate(
                r#"(() => {
                    try { document.createEvent('NotARealType'); return 'no-throw'; }
                    catch (e) { return e.name; }
                })()"#,
            )
            .unwrap();
        assert_eq!(kind, serde_json::json!("NotSupportedError"));
    }

    #[test]
    fn test_html_to_markdown_headings() {
        let mut rt = setup_runtime("<html><body><h1>Title</h1><h2>Sub</h2><p>Body</p></body></html>");
        let md = rt
            .evaluate(crate::diting_js::HTML_TO_MARKDOWN_JS)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(md.contains("# Title"), "missing H1: {}", md);
        assert!(md.contains("## Sub"), "missing H2: {}", md);
        assert!(md.contains("Body"), "missing paragraph text: {}", md);
    }

    #[test]
    fn test_html_to_markdown_links_and_inline() {
        let mut rt = setup_runtime(
            r#"<html><body><p>Hello <strong>world</strong> <a href="https://x.test/">link</a> <em>em</em></p></body></html>"#,
        );
        let md = rt
            .evaluate(crate::diting_js::HTML_TO_MARKDOWN_JS)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(md.contains("**world**"), "missing strong: {}", md);
        assert!(md.contains("*em*"), "missing em: {}", md);
        assert!(
            md.contains("[link](https://x.test/)"),
            "missing link: {}",
            md
        );
    }

    #[test]
    fn test_html_to_markdown_lists() {
        let mut rt = setup_runtime(
            "<html><body><ul><li>A</li><li>B</li></ul><ol><li>X</li><li>Y</li></ol></body></html>",
        );
        let md = rt
            .evaluate(crate::diting_js::HTML_TO_MARKDOWN_JS)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(md.contains("- A"), "missing unordered A: {}", md);
        assert!(md.contains("- B"), "missing unordered B: {}", md);
        assert!(md.contains("1. X"), "missing ordered X: {}", md);
    }

    #[test]
    fn test_html_to_markdown_skips_script_and_style() {
        let mut rt = setup_runtime(
            "<html><body><p>Text</p><script>alert(1)</script><style>body{color:red}</style></body></html>",
        );
        let md = rt
            .evaluate(crate::diting_js::HTML_TO_MARKDOWN_JS)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert!(md.contains("Text"), "missing visible text: {}", md);
        assert!(!md.contains("alert"), "leaked script content: {}", md);
        assert!(!md.contains("color:red"), "leaked style content: {}", md);
    }

    #[test]
    fn test_page_content_puppeteer_pattern() {
        let mut rt = setup_runtime("<!DOCTYPE html><html><head></head><body><p>Test</p></body></html>");
        let result = rt.evaluate(
            "(function() { let retVal = ''; if (document.doctype) retVal = new XMLSerializer().serializeToString(document.doctype); if (document.documentElement) retVal += document.documentElement.outerHTML; return retVal; })()"
        ).unwrap();
        let html = result.as_str().unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<html>"));
        assert!(html.contains("<p>Test</p>"));
    }

    #[test]
    fn test_element_from_point_is_function() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let kind = rt.evaluate("typeof document.elementFromPoint").unwrap();
        assert_eq!(kind, serde_json::json!("function"));
        let kind2 = rt.evaluate("typeof document.elementsFromPoint").unwrap();
        assert_eq!(kind2, serde_json::json!("function"));
    }

    // Regression (obscura #738): hit-testing among overlapping positioned
    // siblings must follow paint order, not document order. The close
    // button (z-index 1002) precedes the loading overlay (z-index 1001) in
    // the DOM; the old nid tie-break handed the click to the overlay — a
    // coordinate click aimed at the visibly topmost element was delivered
    // to the z-index-below one. Feature-gated: real rects need the layout
    // stack (as above).
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_element_from_point_respects_z_index_among_positioned_siblings() {
        let mut rt = setup_runtime(r#"<html><head><style>
          .dialog { position: absolute; top: 100px; left: 100px; width: 400px; height: 200px; }
          .close-button { position: absolute; top: 10px; left: 350px; width: 32px; height: 32px; z-index: 1002; }
          .loading-overlay { position: absolute; top: 0; left: 0; width: 400px; height: 200px; z-index: 1001; }
        </style></head><body>
        <div class="dialog"><button class="close-button">x</button><div class="loading-overlay"></div></div>
        </body></html>"#);
        let diag = rt
            .evaluate(
                r#"JSON.stringify((() => {
                    const btn = document.querySelector('.close-button');
                    const r = btn.getBoundingClientRect();
                    return {
                        z: getComputedStyle(btn).zIndex,
                        overlayZ: getComputedStyle(document.querySelector('.loading-overlay')).zIndex,
                        pos: getComputedStyle(btn).position,
                        rect: [r.left, r.top, r.width, r.height],
                    };
                })())"#,
            )
            .unwrap();
        println!("z-index hit-test diagnostics: {diag}");
        let hit = rt
            .evaluate(
                r#"(function() {
                    const btn = document.querySelector('.close-button');
                    const r = btn.getBoundingClientRect();
                    return document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2)?.className;
                })()"#,
            )
            .unwrap();
        assert_eq!(
            hit,
            serde_json::json!("close-button"),
            "topmost z-index must win the hit, got {hit} (diag: {diag})"
        );
        // elementsFromPoint mirrors the ranking: front-to-back, the button
        // before the overlay it visually sits on.
        let stack = rt
            .evaluate(
                r#"(function() {
                    const btn = document.querySelector('.close-button');
                    const r = btn.getBoundingClientRect();
                    return document
                        .elementsFromPoint(r.left + r.width / 2, r.top + r.height / 2)
                        .slice(0, 2)
                        .map((el) => el.className)
                        .join(',');
                })()"#,
            )
            .unwrap();
        assert_eq!(
            stack,
            serde_json::json!("close-button,loading-overlay"),
            "elementsFromPoint must be front-to-back by paint order, got {stack}"
        );
    }

    /// Chrome ends every elementsFromPoint stack with `<body>` then `<html>`;
    /// both span the viewport, so they are appended after the ranked
    /// descendants, never ranked among them — ranking a viewport-spanning
    /// box would shadow every real descendant in elementFromPoint
    /// (absorbing obscura PR #848's append-not-rank resolution).
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_elements_from_point_appends_body_and_html() {
        let mut rt = setup_runtime(r#"<html><head><style>
          .dialog { position: absolute; top: 100px; left: 100px; width: 400px; height: 200px; }
        </style></head><body>
        <div class="dialog"></div>
        </body></html>"#);
        let stack = rt
            .evaluate(
                r#"(function() {
                    const d = document.querySelector('.dialog');
                    const r = d.getBoundingClientRect();
                    return document
                        .elementsFromPoint(r.left + r.width / 2, r.top + r.height / 2)
                        .map((el) => el.tagName + (el.className ? '.' + el.className : ''))
                        .join('|');
                })()"#,
            )
            .unwrap();
        assert_eq!(
            stack,
            serde_json::json!("DIV.dialog|BODY|HTML"),
            "stack must end with BODY then HTML, got {stack}"
        );
    }

    // Event-coordinate surface under transforms (blitz #663 family).
    // pageX/pageY ride the client point plus root scroll; x/y alias
    // clientX/clientY; offsetX/offsetY inverse-map the hit point through
    // the element's TOTAL paint transform into its local space. The box is
    // 100x100 at doc (100,100) rotated 45° about its center (150,150); a
    // hit at (150,110) inverse-maps to local (121.716, 121.716), i.e.
    // offset (21.716, 21.716) from the box origin.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_mouse_event_coordinates_under_rotation() {
        let mut rt = setup_runtime(
            r#"<html><head><style>
            body { margin: 0; }
            #rot { position: absolute; left: 100px; top: 100px; width: 100px; height: 100px;
                   transform: rotate(45deg); }
          </style></head><body><div id="rot"></div></body></html>"#,
        );
        let out = rt
            .evaluate(
                r#"JSON.stringify((function() {
                    const d = document.getElementById('rot');
                    const ev = new MouseEvent('click', { clientX: 150, clientY: 110 });
                    d.dispatchEvent(ev);
                    const r = d.getBoundingClientRect();
                    return {
                        ox: +ev.offsetX.toFixed(3), oy: +ev.offsetY.toFixed(3),
                        px: ev.pageX, py: ev.pageY, x: ev.x, y: ev.y,
                        gleft: +r.left.toFixed(2), gtop: +r.top.toFixed(2),
                        gwidth: +r.width.toFixed(2),
                        offsetLeft: d.offsetLeft, offsetTop: d.offsetTop,
                    };
                })())"#,
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["px"], serde_json::json!(150), "pageX = clientX + root scroll (0 here)");
        assert_eq!(v["py"], serde_json::json!(110));
        assert_eq!(v["x"], serde_json::json!(150), "x aliases clientX");
        assert_eq!(v["y"], serde_json::json!(110));
        let ox = v["ox"].as_f64().unwrap();
        let oy = v["oy"].as_f64().unwrap();
        assert!(
            (ox - 21.716).abs() < 0.05 && (oy - 21.716).abs() < 0.05,
            "offset must be the inverse-mapped local point (21.716,21.716), got ({ox},{oy})"
        );
        // gBCR carries the mapped bounding box (a rotated square's AABB is
        // the 141.42 diagonal box); offsetLeft/Top stay layout-true.
        assert_eq!(v["offsetLeft"], serde_json::json!(100), "offsetLeft ignores the transform");
        assert_eq!(v["offsetTop"], serde_json::json!(100));
        assert_eq!(v["gleft"], serde_json::json!(79.29), "gBCR is the mapped AABB");
        assert_eq!(v["gtop"], serde_json::json!(79.29));
        assert_eq!(v["gwidth"], serde_json::json!(141.42));
    }

    // Exact-shape hit testing (blitz #663 family): the corner of a rotated
    // box's AABB covers no pixel of the element, so elementFromPoint there
    // must NOT hand back the rotated element — while its center still hits.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_element_from_point_rejects_rotated_aabb_corners() {
        let mut rt = setup_runtime(
            r#"<html><head><style>
            body { margin: 0; }
            #rot { position: absolute; left: 100px; top: 100px; width: 100px; height: 100px;
                   transform: rotate(45deg); }
          </style></head><body><div id="rot"></div></body></html>"#,
        );
        let out = rt
            .evaluate(
                r#"JSON.stringify((function() {
                    return {
                        center: document.elementFromPoint(150, 110)?.id || null,
                        corner: document.elementFromPoint(85, 85)?.id || null,
                    };
                })())"#,
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(
            v["center"],
            serde_json::json!("rot"),
            "a point inside the rotated shape must hit the element"
        );
        assert_ne!(
            v["corner"],
            serde_json::json!("rot"),
            "the AABB corner (85,85) is outside the rotated diamond — must not hit"
        );
    }

    // obscura #976: elements of a sync-created iframe document are an
    // orphan Rust DOM subtree the main-document layout run never covers, so
    // gBCR answered all zeros. The sub-run measures in the IFRAME'S OWN
    // viewport (300x150 — the box the fabricated window publishes), so a
    // body with its UA 8px margin puts the first div at (8,8), like Chrome.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_sync_iframe_document_elements_have_layout_rects() {
        let mut rt = setup_runtime(r#"<html><body></body></html>"#);
        let out = rt
            .evaluate(
                r#"JSON.stringify((function() {
                    const f = document.createElement('iframe');
                    document.body.appendChild(f);
                    const doc = f.contentDocument;
                    doc.body.innerHTML = '<style>body{margin:8px}div{width:100px;height:20px}</style><div id=a></div><div id=b></div>';
                    const a = doc.getElementById('a');
                    const b = doc.getElementById('b');
                    const ra = a.getBoundingClientRect();
                    const rb = b.getBoundingClientRect();
                    const before = {
                        ax: ra.x, ay: ra.y, aw: ra.width, ah: ra.height, ab: ra.bottom,
                        bx: rb.x, by: rb.y,
                        rects: a.getClientRects().length,
                        ow: a.offsetWidth, oh: a.offsetHeight,
                    };
                    // A style write inside the iframe doc must re-measure.
                    a.style.width = '60px';
                    return { before: before, aw2: a.getBoundingClientRect().width };
                })())"#,
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        let b = &v["before"];
        assert_eq!(b["ax"], serde_json::json!(8), "body UA margin 8px, in the iframe's own viewport");
        assert_eq!(b["ay"], serde_json::json!(8));
        assert_eq!(b["aw"], serde_json::json!(100));
        assert_eq!(b["ah"], serde_json::json!(20));
        assert_eq!(b["ab"], serde_json::json!(28), "bottom = y + height");
        assert_eq!(b["bx"], serde_json::json!(8));
        assert_eq!(b["by"], serde_json::json!(28), "second block stacks below the first");
        assert_eq!(b["rects"], serde_json::json!(1));
        assert_eq!(b["ow"], serde_json::json!(100), "offsetWidth rides the same sub-run");
        assert_eq!(b["oh"], serde_json::json!(20));
        assert_eq!(
            v["aw2"], serde_json::json!(60),
            "a style write inside the iframe doc must invalidate the sub-run cache"
        );
    }

    // The two coordinate worlds must not bleed into each other: the iframe
    // doc's own html/body span the IFRAME's 300x150 default box, while
    // host-page elements keep measuring against the host viewport.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_iframe_document_viewport_is_independent() {
        let mut rt = setup_runtime(
            r#"<html><head><style>body{margin:0}#main{width:400px;height:50px}</style></head><body><div id="main"></div></body></html>"#,
        );
        let out = rt
            .evaluate(
                r#"JSON.stringify((function() {
                    const f = document.createElement('iframe');
                    document.body.appendChild(f);
                    const doc = f.contentDocument;
                    doc.body.innerHTML = '<p>hi</p>';
                    const rRoot = doc.documentElement.getBoundingClientRect();
                    const rBody = doc.body.getBoundingClientRect();
                    const main = document.getElementById('main').getBoundingClientRect();
                    return {
                        rootW: rRoot.width, rootH: rRoot.height,
                        bodyW: rBody.width, bodyH: rBody.height,
                        mainW: main.width, mainH: main.height,
                    };
                })())"#,
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["rootW"], serde_json::json!(300), "iframe html spans the 300x150 default box");
        assert_eq!(v["rootH"], serde_json::json!(150));
        assert_eq!(v["bodyW"], serde_json::json!(300));
        assert_eq!(v["bodyH"], serde_json::json!(150));
        assert_eq!(v["mainW"], serde_json::json!(400), "host-page geometry unchanged");
        assert_eq!(v["mainH"], serde_json::json!(50));
    }

    /// A `display: none` element generates no box, so it cannot be hit however
    /// high its z-index: the zero-size rect filter runs before ranking ever
    /// sees it. Pinning this because a boxless element entering the ranking
    /// would win by accident (obscura PR #848's second test, same shape).
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_element_from_point_ignores_display_none_sibling() {
        let mut rt = setup_runtime(r#"<html><head><style>
          .real { position: absolute; top: 100px; left: 100px; width: 200px; height: 100px; }
          .hidden { position: absolute; top: 100px; left: 100px; width: 200px; height: 100px; z-index: 9999; display: none; }
        </style></head><body>
        <div class="hidden"></div><div class="real"></div>
        </body></html>"#);
        let hit = rt
            .evaluate(
                r#"(function() {
                    const r = document.querySelector('.real').getBoundingClientRect();
                    return document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2)?.className;
                })()"#,
            )
            .unwrap();
        assert_eq!(
            hit,
            serde_json::json!("real"),
            "a display:none sibling must never take the hit, got {hit}"
        );
    }

    /// A nid absent from the rects map is a legitimate miss on a fresh cache
    /// (<head>, display:none subtrees, svg children under the svg v1 one-box
    /// model) — not a staleness signal. Conflating the two re-ran the
    /// whole-page layout once per boxless lookup; on svg-heavy pages (archify
    /// workflow artifacts: ~530 boxless svg nodes) a single elementFromPoint
    /// walk re-laid-out the tree hundreds of times and tripped the 10s eval
    /// watchdog, surfacing as silent `null` eval results and dead clicks.
    /// The timing itself isn't unit-assertable without wall-clock flakiness,
    /// so this pins the semantics: hit testing stays correct on a
    /// boxless-heavy DOM, both on first layout and after an epoch bump.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_element_from_point_on_boxless_heavy_dom() {
        let mut circles = String::new();
        for i in 0..200 {
            circles.push_str(&format!(
                "<circle cx=\"{}\" cy=\"{}\" r=\"4\"/>\n",
                20 + (i % 20) * 19,
                20 + (i / 20) * 19
            ));
        }
        let html = format!(
            r#"<html><head><style>
          .panel {{ position: absolute; top: 40px; left: 40px; width: 200px; height: 100px; }}
          .ghost {{ position: absolute; top: 40px; left: 40px; width: 200px; height: 100px; display: none; }}
          svg {{ position: absolute; top: 0; left: 0; }}
        </style></head><body>
        <svg width="400" height="400" viewBox="0 0 400 400">{circles}</svg>
        <div class="ghost"></div><div class="panel"></div>
        </body></html>"#
        );
        let mut rt = setup_runtime(&html);
        let probe = |rt: &mut JsRuntime| {
            let v = rt
                .evaluate(
                    r#"(function() {
                    const p = document.querySelector('.panel').getBoundingClientRect();
                    const hit = document.elementFromPoint(p.left + p.width / 2, p.top + p.height / 2);
                    const circles = document.querySelectorAll('circle').length;
                    // boxless elements must not poison the walk: 200 svg
                    // children + a display:none sibling all lack rects
                    const second = document.elementFromPoint(370, 380);
                    return JSON.stringify({
                        hit: hit ? hit.className || hit.tagName : null,
                        circles,
                        svgArea: second ? second.tagName : null,
                    });
                })()"#,
                )
                .unwrap();
            serde_json::from_str::<serde_json::Value>(v.as_str().unwrap()).unwrap()
        };
        let first = probe(&mut rt);
        assert_eq!(first["circles"], serde_json::json!(200));
        assert_eq!(first["hit"], serde_json::json!("panel"));
        // SVG-namespace elements keep their source-case tag name (Chrome
        // tagName uppercases only the HTML namespace).
        assert_eq!(first["svgArea"], serde_json::json!("svg"));

        // Mutate: the epoch bumps, the memoized run must be re-run (not
        // served from the stale cache), and the panel must move accordingly.
        rt.evaluate(
            r#"document.querySelector('.panel').style.top = '300px'; 'ok'"#,
        )
        .unwrap();
        let after = probe(&mut rt);
        assert_eq!(after["hit"], serde_json::json!("panel"));
    }

    #[test]
    #[cfg(feature = "screenshot")]
    fn test_element_from_point_in_viewport_returns_body() {
        let mut rt = setup_runtime("<html><body><h1>Hi</h1></body></html>");
        // With diting-layout rects backing getBoundingClientRect, hit testing
        // is real. The h1's UA box (body 8px margin + h1 .67em margin-top,
        // then the 2em line box) starts around y≈30, so (10,40) lands on it;
        // (10,10) sits in the h1's margin — body territory, like Chrome.
        //
        // Feature-gated: the layout_rects_all pipeline (and the
        // _domRaw("layout_rect") op behind gBCR) only exists under the
        // `screenshot` feature; without it gBCR falls back to bootstrap.js's
        // synthetic nid-hashed grid (h1 lands at 450,190), which this test
        // was never meant to pin. The long-standing bare-`cargo test` red
        // was exactly that gate, not a layout regression — bisect pointed at
        // 83129c5 only because that's when the test started asserting real
        // rects.
        let tag = rt.evaluate("document.elementFromPoint(10, 40)?.tagName").unwrap();
        assert_eq!(tag, serde_json::json!("H1"));
        let margin_area = rt.evaluate("document.elementFromPoint(10, 10)?.tagName").unwrap();
        assert_eq!(margin_area, serde_json::json!("BODY"));
        // Below the h1's line box the point falls through to the body.
        let below = rt.evaluate("document.elementFromPoint(10, 500)?.tagName").unwrap();
        assert_eq!(below, serde_json::json!("BODY"));
    }

    // Regression (obscura #740): translate percentages resolve against the
    // element's own box — translate(-50%, -50%) centers a top:50% box. The
    // Y axis used to come out wrong while X was right. Feature-gated: real
    // geometry needs the layout stack.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_translate_negative_percent_centers_the_box() {
        let mut rt = setup_runtime(r#"<html><head><style>
          .dialog { position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); width: 400px; height: 140px; }
        </style></head><body>
        <div class="dialog"></div>
        </body></html>"#);
        let diag = rt
            .evaluate(
                r#"JSON.stringify((() => {
                    const r = document.querySelector('.dialog').getBoundingClientRect();
                    return { top: r.top, left: r.left, w: r.width, h: r.height,
                             vw: window.innerWidth, vh: window.innerHeight };
                })())"#,
            )
            .unwrap();
        println!("translate diagnostics: {diag}");
        let v: serde_json::Value = serde_json::from_str(diag.as_str().unwrap()).unwrap();
        let vh = v["vh"].as_f64().unwrap();
        let vw = v["vw"].as_f64().unwrap();
        let top = v["top"].as_f64().unwrap();
        let left = v["left"].as_f64().unwrap();
        assert_eq!(v["w"].as_f64().unwrap(), 400.0);
        assert_eq!(v["h"].as_f64().unwrap(), 140.0);
        // top:50% of the viewport, minus half the box: dead center.
        assert_eq!(top, vh / 2.0 - 70.0, "Y must be viewport-centered, diag: {diag}");
        assert_eq!(left, vw / 2.0 - 200.0, "X must be viewport-centered, diag: {diag}");
    }

    // Nested translate (px form): the ancestor's translate carries the
    // whole subtree, the child's own translate stacks on top — and the
    // layout-internal position of the child inside the ancestor is
    // unchanged (transforms never feed back into layout).
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_translate_px_stacks_down_the_subtree() {
        let mut rt = setup_runtime(r#"<html><head><style>
          .outer { transform: translate(10px, 20px); width: 200px; height: 100px; }
          .inner { transform: translate(0, -5px); width: 50px; height: 50px; }
        </style></head><body>
        <div class="outer"><div class="inner"></div></div>
        </body></html>"#);
        let diag = rt
            .evaluate(
                r#"JSON.stringify((() => {
                    const o = document.querySelector('.outer').getBoundingClientRect();
                    const i = document.querySelector('.inner').getBoundingClientRect();
                    return { dx: i.left - o.left, dy: i.top - o.top,
                             ow: o.width, oh: o.height, iw: i.width, ih: i.height };
                })())"#,
            )
            .unwrap();
        println!("translate px diagnostics: {diag}");
        let v: serde_json::Value = serde_json::from_str(diag.as_str().unwrap()).unwrap();
        // Layout kept the child at the ancestor's content origin; only the
        // two translates move it: 0 + 0 = 0 in X, 0 + (-5) = -5 in Y.
        assert_eq!(v["dx"].as_f64().unwrap(), 0.0, "diag: {diag}");
        assert_eq!(v["dy"].as_f64().unwrap(), -5.0, "diag: {diag}");
        assert_eq!(v["ow"].as_f64().unwrap(), 200.0);
        assert_eq!(v["iw"].as_f64().unwrap(), 50.0);
    }

    // obscura #937 family: transforms only apply to transformable boxes
    // (block-level or atomic inline-level). A non-replaced inline box is
    // not transformable — Chrome ignores its transform for gBCR,
    // hit-testing AND ink extent (paint already ignores it here).
    #[test]
    #[cfg(feature = "screenshot")]
    fn transform_on_non_replaced_inline_is_ignored_by_gbcr() {
        let mut rt = setup_runtime(r#"<html><head><style>
          #i1, #i2 { display: inline; }
          #i2 { transform: translateX(100px); }
          #b1, #b2 { display: inline-block; }
          #b2 { transform: translateX(100px); }
          .row { width: 50px; }
        </style></head><body>
        <div class="row"><span id="i1">hi</span></div>
        <div class="row"><span id="i2">hi</span></div>
        <div><span id="b1">yo</span></div>
        <div><span id="b2">yo</span></div>
        </body></html>"#);
        let diag = rt
            .evaluate(
                r#"JSON.stringify((() => {
                    try {
                        const e = id => document.getElementById(id);
                        const g = id => e(id).getBoundingClientRect();
                        const i1 = g('i1'), i2 = g('i2'), b1 = g('b1'), b2 = g('b2');
                        const midY = i2.top + i2.height / 2;
                        const atStatic = document.elementFromPoint(i2.left + i2.width / 2, midY);
                        const atShifted = document.elementFromPoint(i2.left + 100 + i2.width / 2, midY);
                        return { iDx: i2.left - i1.left, bDx: b2.left - b1.left,
                                 atStatic: atStatic ? atStatic.id : null,
                                 atShifted: atShifted ? atShifted.id : null,
                                 sw1: e('i1').parentElement.scrollWidth,
                                 sw2: e('i2').parentElement.scrollWidth };
                    } catch (err) { return { error: String(err) }; }
                })())"#,
            )
            .unwrap();
        println!("inline transform diagnostics: {diag}");
        let v: serde_json::Value = serde_json::from_str(diag.as_str().unwrap()).unwrap();
        assert_eq!(
            v["bDx"].as_f64().unwrap(),
            100.0,
            "control: inline-block IS transformable, must shift: {diag}"
        );
        assert_eq!(
            v["iDx"].as_f64().unwrap(),
            0.0,
            "non-replaced inline is not transformable — gBCR must ignore the transform: {diag}"
        );
        let static_hit = v["atStatic"].as_str().unwrap_or("(null)");
        let shifted_hit = v["atShifted"].as_str().unwrap_or("(null)");
        assert_eq!(
            static_hit, "i2",
            "hit-testing must also ignore the inline transform: {diag}"
        );
        assert_ne!(
            shifted_hit, "i2",
            "the shifted position must not hit the inline box: {diag}"
        );
        assert_eq!(
            v["sw1"].as_u64().unwrap(),
            v["sw2"].as_u64().unwrap(),
            "ink extent must ignore the inline transform too: {diag}"
        );
    }

    // Batch-67 leftover: form widgets honor position:absolute. An input
    // carrying `position:absolute; left/top` must land at that offset like
    // any other box — replaced-widget boxes used to stay in flow position.
    #[test]
    #[cfg(feature = "screenshot")]
    fn form_widget_honors_absolute_positioning() {
        let mut rt = setup_runtime(r#"<html><head><style>
          .abs { position: absolute; left: 100px; top: 50px; }
        </style></head><body>
        <div id="d" class="abs">x</div>
        <input id="txt" class="abs" type="text" value="hi">
        <input id="chk" class="abs" type="checkbox">
        </body></html>"#);
        let diag = rt
            .evaluate(
                r#"JSON.stringify((() => {
                    try {
                        const g = id => { const r = document.getElementById(id).getBoundingClientRect();
                                          return [Math.round(r.left), Math.round(r.top)]; };
                        return { d: g('d'), txt: g('txt'), chk: g('chk') };
                    } catch (err) { return { error: String(err) }; }
                })())"#,
            )
            .unwrap();
        println!("form widget abspos diagnostics: {diag}");
        let v: serde_json::Value = serde_json::from_str(diag.as_str().unwrap()).unwrap();
        if v.get("error").is_some() {
            panic!("probe threw: {diag}");
        }
        assert_eq!(v["d"], serde_json::json!([100, 50]), "control div: {diag}");
        assert_eq!(
            v["txt"],
            serde_json::json!([100, 50]),
            "text input must honor position:absolute: {diag}"
        );
        assert_eq!(
            v["chk"],
            serde_json::json!([100, 50]),
            "checkbox must honor position:absolute: {diag}"
        );
    }

    // Regression (companion to obscura #738): getComputedStyle consulted
    // inline styles, dimensions and a defaults table — but never the
    // stylesheet cascade, so a z-index/position set in a <style> block
    // read back as "auto"/"static" no matter what the sheet said. Any
    // script branching on computed layout properties (jQuery .css(),
    // overlay/positioning logic) took the wrong branch silently.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_get_computed_style_reads_the_stylesheet_cascade() {
        let mut rt = setup_runtime(r#"<html><head><style>
          .panel { position: absolute; z-index: 42; display: flex; color: rgb(1, 2, 3); }
          .plain { color: #abcdef; }
        </style></head><body>
        <div class="panel" id="p"><span class="plain" id="s">x</span></div>
        </body></html>"#);
        let out = rt
            .evaluate(
                r#"JSON.stringify({
                    pos: getComputedStyle(document.getElementById('p')).position,
                    z: getComputedStyle(document.getElementById('p')).zIndex,
                    disp: getComputedStyle(document.getElementById('p')).display,
                    color: getComputedStyle(document.getElementById('p')).color,
                    camel: getComputedStyle(document.getElementById('p')).zIndex,
                    viaCall: getComputedStyle(document.getElementById('p')).getPropertyValue('z-index'),
                })"#,
            )
            .unwrap();
        println!("gcs cascade diagnostics: {out}");
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["pos"], serde_json::json!("absolute"));
        assert_eq!(v["z"], serde_json::json!("42"));
        assert_eq!(v["camel"], serde_json::json!("42"));
        assert_eq!(v["viaCall"], serde_json::json!("42"));
        assert_eq!(v["disp"], serde_json::json!("flex"));
        assert_eq!(v["color"], serde_json::json!("rgb(1, 2, 3)"));
        // Inline still wins the cascade over the stylesheet rule.
        let inline = rt
            .evaluate(
                r#"(function() {
                    const p = document.getElementById('p');
                    p.style.zIndex = '7';
                    return getComputedStyle(p).zIndex;
                })()"#,
            )
            .unwrap();
        assert_eq!(inline, serde_json::json!("7"));
        // A property no rule targets keeps its initial value, not garbage.
        let initial = rt
            .evaluate("getComputedStyle(document.getElementById('s')).zIndex")
            .unwrap();
        assert_eq!(initial, serde_json::json!("auto"));
    }

    // CSS custom properties end-to-end (#229 residue): --* declared in a
    // stylesheet must (a) feed var() substitution in the cascade, (b) be
    // readable back through getComputedStyle().getPropertyValue('--*')
    // case-sensitively, and (c) survive the inline-style round-trip without
    // the kebab-lowercase pass corrupting --mainColor into --main-color.
    // background-image rides along raw (longhand only).
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_custom_properties_and_var_substitution() {
        let mut rt = setup_runtime(r#"<html><head><style>
          :root { --brand: rgb(200, 10, 10); --card-w: 120px; }
          .card { width: var(--card-w); color: var(--brand); background-image: linear-gradient(to right, var(--brand), blue); }
          .fallback { color: var(--missing, rgb(9, 9, 9)); }
        </style></head><body>
        <div class="card" id="c">x</div>
        <div class="fallback" id="f">y</div>
        </body></html>"#);
        let out = rt
            .evaluate(
                r#"JSON.stringify({
                    color: getComputedStyle(document.getElementById('c')).color,
                    bg: getComputedStyle(document.getElementById('c')).backgroundImage,
                    rootVar: getComputedStyle(document.getElementById('c')).getPropertyValue('--brand'),
                    inheritedVar: getComputedStyle(document.getElementById('c')).getPropertyValue('--card-w'),
                    fallback: getComputedStyle(document.getElementById('f')).color,
                    camelInline: (function() {
                        document.body.style.setProperty('--mainColor', '#123456');
                        return getComputedStyle(document.body).getPropertyValue('--mainColor');
                    })(),
                })"#,
            )
            .unwrap();
        println!("custom props diagnostics: {out}");
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["color"], serde_json::json!("rgb(200, 10, 10)"), "var() substitutes in the cascade");
        assert_eq!(
            v["bg"],
            serde_json::json!("linear-gradient(to right, rgb(200, 10, 10), blue)"),
            "background-image passes through with vars substituted"
        );
        assert_eq!(v["rootVar"], serde_json::json!("rgb(200, 10, 10)"), "--brand readable on descendants");
        assert_eq!(v["inheritedVar"], serde_json::json!("120px"), "--card-w inherits from :root");
        assert_eq!(v["fallback"], serde_json::json!("rgb(9, 9, 9)"), "var() fallback applies");
        assert_eq!(v["camelInline"], serde_json::json!("#123456"), "--mainColor survives case-sensitively");
    }

    // Regression (obscura #771 wrong-value rows): getComputedStyle served the
    // layout engine's Block bucket for the table family and `stretch` /
    // `flex-start` for unset align-items/justify-content. Chrome answers
    // table/table-row/table-cell/list-item and `normal` — an element's box
    // type was otherwise unreadable over CDP. An author `display: block`
    // (responsive table collapses) must still win over the UA table.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_get_computed_style_table_family_and_flex_initials() {
        let mut rt = setup_runtime(r#"<html><head><style>
          .flat { display: block; }
        </style></head><body>
        <table><tr><td id="cell">a</td><td class="flat" id="flat">b</td></tr></table>
        <ul><li id="item">x</li></ul>
        <div id="box"><span id="in">y</span></div>
        </body></html>"#);
        let out = rt
            .evaluate(
                r#"JSON.stringify({
                    table: getComputedStyle(document.querySelector('table')).display,
                    row: getComputedStyle(document.querySelector('tr')).display,
                    cell: getComputedStyle(document.getElementById('cell')).display,
                    authorBlock: getComputedStyle(document.getElementById('flat')).display,
                    listItem: getComputedStyle(document.getElementById('item')).display,
                    div: getComputedStyle(document.getElementById('box')).display,
                    span: getComputedStyle(document.getElementById('in')).display,
                    alignNormal: getComputedStyle(document.getElementById('box')).alignItems,
                    justifyNormal: getComputedStyle(document.getElementById('box')).justifyContent,
                })"#,
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["table"], serde_json::json!("table"));
        assert_eq!(v["row"], serde_json::json!("table-row"));
        assert_eq!(v["cell"], serde_json::json!("table-cell"));
        assert_eq!(v["authorBlock"], serde_json::json!("block"));
        assert_eq!(v["listItem"], serde_json::json!("list-item"));
        assert_eq!(v["div"], serde_json::json!("block"));
        assert_eq!(v["span"], serde_json::json!("inline"));
        assert_eq!(v["alignNormal"], serde_json::json!("normal"));
        assert_eq!(v["justifyNormal"], serde_json::json!("normal"));
    }

    // Regression (obscura #771 empty-value rows): 41 computed properties read
    // back '' on an unstyled element; '' is indistinguishable from "not set",
    // so feature probes silently took the wrong branch. The defaults table
    // now carries Chrome's initial values (audited against Chromium 147).
    #[test]
    fn test_get_computed_style_initial_values_not_empty() {
        let mut rt = setup_runtime(r#"<html><body><div id="d">x</div></body></html>"#);
        let out = rt
            .evaluate(
                r#"JSON.stringify({
                    bg: getComputedStyle(document.getElementById('d')).backgroundImage,
                    bgPos: getComputedStyle(document.getElementById('d')).backgroundPosition,
                    bgRepeat: getComputedStyle(document.getElementById('d')).backgroundRepeat,
                    fontStyle: getComputedStyle(document.getElementById('d')).fontStyle,
                    flexGrow: getComputedStyle(document.getElementById('d')).flexGrow,
                    flexShrink: getComputedStyle(document.getElementById('d')).flexShrink,
                    flexBasis: getComputedStyle(document.getElementById('d')).flexBasis,
                    transProp: getComputedStyle(document.getElementById('d')).transitionProperty,
                    animName: getComputedStyle(document.getElementById('d')).animationName,
                    animIter: getComputedStyle(document.getElementById('d')).animationIterationCount,
                    animTiming: getComputedStyle(document.getElementById('d')).animationTimingFunction,
                    userSelect: getComputedStyle(document.getElementById('d')).userSelect,
                    direction: getComputedStyle(document.getElementById('d')).direction,
                    zoom: getComputedStyle(document.getElementById('d')).zoom,
                    minHeight: getComputedStyle(document.getElementById('d')).minHeight,
                    order: getComputedStyle(document.getElementById('d')).order,
                    objectFit: getComputedStyle(document.getElementById('d')).objectFit,
                    aspectRatio: getComputedStyle(document.getElementById('d')).aspectRatio,
                    outlineWidth: getComputedStyle(document.getElementById('d')).outlineWidth,
                })"#,
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.as_str().unwrap()).unwrap();
        assert_eq!(v["bg"], serde_json::json!("none"));
        assert_eq!(v["bgPos"], serde_json::json!("0% 0%"));
        assert_eq!(v["bgRepeat"], serde_json::json!("repeat"));
        assert_eq!(v["fontStyle"], serde_json::json!("normal"));
        assert_eq!(v["flexGrow"], serde_json::json!("0"));
        assert_eq!(v["flexShrink"], serde_json::json!("1"));
        assert_eq!(v["flexBasis"], serde_json::json!("auto"));
        assert_eq!(v["transProp"], serde_json::json!("all"));
        assert_eq!(v["animName"], serde_json::json!("none"));
        assert_eq!(v["animIter"], serde_json::json!("1"));
        assert_eq!(v["animTiming"], serde_json::json!("ease"));
        assert_eq!(v["userSelect"], serde_json::json!("auto"));
        assert_eq!(v["direction"], serde_json::json!("ltr"));
        assert_eq!(v["zoom"], serde_json::json!("1"));
        assert_eq!(v["minHeight"], serde_json::json!("0px"));
        assert_eq!(v["order"], serde_json::json!("0"));
        assert_eq!(v["objectFit"], serde_json::json!("fill"));
        assert_eq!(v["aspectRatio"], serde_json::json!("auto"));
        // Verified against local Chromium: the computed value stays `medium`
        // (3px) even with outline-style none — not the used value 0px.
        assert_eq!(v["outlineWidth"], serde_json::json!("3px"));
    }

    #[test]
    fn test_element_from_point_out_of_viewport_returns_null() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let neg_x = rt.evaluate("document.elementFromPoint(-1, 10)").unwrap();
        assert_eq!(neg_x, serde_json::Value::Null);
        let neg_y = rt.evaluate("document.elementFromPoint(10, -1)").unwrap();
        assert_eq!(neg_y, serde_json::Value::Null);
        let huge = rt.evaluate("document.elementFromPoint(99999, 99999)").unwrap();
        assert_eq!(huge, serde_json::Value::Null);
    }

    #[test]
    fn test_elements_from_point_returns_array() {
        // Length 2, not 1: Chrome answers [BODY, HTML] on a bare page (the
        // old expectation of 1 encoded our pre-append behaviour, not the
        // web's — same correction as obscura PR #848, measured against
        // headless Chrome rather than assumed).
        let mut rt = setup_runtime("<html><body></body></html>");
        let len_in = rt.evaluate("document.elementsFromPoint(10, 10).length").unwrap();
        assert_eq!(len_in.as_f64().unwrap() as i64, 2);
        let len_out = rt.evaluate("document.elementsFromPoint(-1, -1).length").unwrap();
        assert_eq!(len_out.as_f64().unwrap() as i64, 0);
    }

    #[test]
    fn test_element_from_point_non_numeric_returns_null() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let nan = rt.evaluate("document.elementFromPoint(NaN, 10)").unwrap();
        assert_eq!(nan, serde_json::Value::Null);
        let inf = rt.evaluate("document.elementFromPoint(Infinity, 10)").unwrap();
        assert_eq!(inf, serde_json::Value::Null);
    }

    // Issue #139 — proxy_url must thread through to both the ES-module
    // loader (module_loader.rs) and op_fetch_url's reqwest client
    // (ops.rs::build_request_client). Pre-fix both built clients with
    // `Client::builder().build()` — no proxy — so JS fetch/XHR and
    // dynamic imports silently bypassed BrowserContext.proxy_url.
    //
    // Phase 5.5 RED check: each test references a symbol that does NOT
    // exist on main (proxy_url() accessor, with_proxy ctor,
    // with_base_url_and_proxy ctor), so the tests fail to compile without
    // the prod fix.
    #[test]
    fn http_client_round_trips_proxy_url() {
        use crate::diting_net::{CookieJar, HttpClient};
        let jar = std::sync::Arc::new(CookieJar::new());
        let configured =
            HttpClient::with_options(jar.clone(), Some("http://proxy.test:8080"));
        assert_eq!(
            configured.proxy_url(),
            Some("http://proxy.test:8080"),
            "proxy_url() must expose the value passed to with_options"
        );

        let direct = HttpClient::with_options(jar, None);
        assert_eq!(
            direct.proxy_url(),
            None,
            "proxy_url() must return None when no proxy was configured"
        );
    }

    #[test]
    fn module_loader_stores_proxy_for_dynamic_imports() {
        use crate::diting_js::module_loader::DitingModuleLoader;
        let loader = DitingModuleLoader::with_proxy(
            "https://example.com/",
            Some("http://proxy.test:8080".to_string()),
        );
        assert_eq!(loader.proxy_url.as_deref(), Some("http://proxy.test:8080"));
        assert_eq!(loader.base_url, "https://example.com/");

        // Default constructor must keep the historical "no proxy" behaviour.
        let direct = DitingModuleLoader::new("https://example.com/");
        assert_eq!(direct.proxy_url, None);
    }

    #[test]
    fn runtime_with_base_url_and_proxy_constructs_successfully() {
        // Sanity-check the public ctor that page.rs uses to thread proxy
        // through to the module loader. Direct (None) and proxied paths
        // must both initialise the JS environment.
        let _direct = JsRuntime::with_base_url_and_proxy("https://example.com/", None);
        let _proxied = JsRuntime::with_base_url_and_proxy(
            "https://example.com/",
            Some("http://proxy.test:8080".to_string()),
        );
    }

    // ── Issue #45 (Playwright actionability) regression tests ────────────────
    // Kept at the end of the module so they don't share textual context with
    // unrelated test additions in other branches (avoids spurious merge
    // conflicts when both this branch and an unrelated bootstrap.js change
    // add tests near the start of `mod tests`).

    /// Playwright >= 1.25 calls `element.checkVisibility(...)` before every
    /// input event. If the method isn't defined Playwright retries until its
    /// action timeout fires. Without a layout engine we can't compute it
    /// properly, so the stub always returns true — still strictly better
    /// than the undefined path.
    #[test]
    fn element_check_visibility_is_callable() {
        let mut rt = setup_runtime(r#"<div id="x">x</div>"#);
        let result = rt
            .evaluate("document.getElementById('x').checkVisibility({checkOpacity: true})")
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        let typeof_method = rt
            .evaluate("typeof document.getElementById('x').checkVisibility")
            .unwrap();
        assert_eq!(typeof_method, serde_json::json!("function"));
    }

    /// Playwright's `getByRole` / `getByLabel` locators resolve via ARIA
    /// reflection properties. Without the getters those locators always
    /// fail. Reflect the underlying aria-* attributes.
    #[test]
    fn element_aria_reflection_properties_read_aria_attrs() {
        let mut rt = setup_runtime(
            r#"<button id="b" role="tab" aria-label="Settings" aria-selected="true">x</button>"#,
        );
        let result = rt
            .evaluate(
                r#"
                const el = document.getElementById('b');
                return [el.role, el.ariaLabel, el.ariaSelected];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["tab", "Settings", "true"]));
    }

    /// Setting an ARIA reflection property must write through to the
    /// underlying attribute so frameworks that toggle state via
    /// `el.ariaExpanded = 'true'` actually update the DOM.
    #[test]
    fn element_aria_reflection_setters_write_through() {
        let mut rt = setup_runtime(r#"<div id="d"></div>"#);
        let result = rt
            .evaluate(
                r#"
                const el = document.getElementById('d');
                el.role = 'menu';
                el.ariaExpanded = 'true';
                return [el.getAttribute('role'), el.getAttribute('aria-expanded')];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["menu", "true"]));
    }

    /// Upstream 846ed7d: the Function.prototype.toString override must have a
    /// native function's shape — name, length, non-constructible, no own
    /// `prototype` property.
    #[test]
    fn function_to_string_has_native_function_shape() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                r#"(() => {
                    const fn = Function.prototype.toString;
                    let constructible = true;
                    try { Reflect.construct(function () {}, [], fn); } catch (e) { constructible = false; }
                    return [fn.toString(), fn.name, fn.length,
                            Object.prototype.hasOwnProperty.call(fn, "prototype"),
                            constructible].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!("function toString() { [native code] }|toString|0|false|false")
        );
    }

    /// Upstream 4c33f6d (tamperedFunctions): JS-backed builtins — constructors,
    /// prototype methods, and accessors — must all report [native code].
    #[test]
    fn builtin_members_report_native_code() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                r#"(() => {
                    const nodeTypeGet = Object.getOwnPropertyDescriptor(Node.prototype, "nodeType").get;
                    return [String(Element), String(Node),
                            String(Element.prototype.getAttribute),
                            String(nodeTypeGet)].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!(
                "function Element() { [native code] }|function Node() { [native code] }|function getAttribute() { [native code] }|function get nodeType() { [native code] }"
            )
        );
    }

    /// Upstream 4c33f6d (unusualWindowProperties): internal globals must not
    /// surface through any reflection API on the global object.
    #[test]
    fn internal_globals_are_hidden_from_all_reflection_apis() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                r#"(() => {
                    const bad = (a) => a.filter(n => typeof n === "string" &&
                        (n[0] === "_" || n.includes("obscura") || n.includes("Obscura") || n.includes("diting") || n.includes("Diting"))).length;
                    const descs = Object.getOwnPropertyDescriptors(window);
                    return [bad(Object.getOwnPropertyNames(window)),
                            bad(Reflect.ownKeys(window)),
                            bad(Object.keys(window)),
                            bad(Object.keys(descs))].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("0|0|0|0"));
    }

    /// Upstream c7e7c70: WebIDL interface globals are non-enumerable in a real
    /// browser (and stay callable).
    #[test]
    fn webidl_interface_globals_are_non_enumerable() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                r#"(() => {
                    const names = ["Node", "Element", "Document", "Window",
                                   "CSSStyleDeclaration", "DOMStringMap"];
                    const enumerable = names.filter(n => {
                        const d = Object.getOwnPropertyDescriptor(window, n);
                        return !d || d.enumerable !== false;
                    });
                    return [enumerable.length, Object.keys(window).includes("Node"),
                            typeof Node, document.body instanceof Element].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("0|false|function|true"));
    }

    /// Upstream a0e1ba5: CSSStyleDeclaration is a real global interface — the
    /// type of element.style — not merely pre-declared.
    #[test]
    fn cssstyledeclaration_is_a_usable_global_interface() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                "(function(){var d=Object.getOwnPropertyDescriptor(window,'CSSStyleDeclaration');return (typeof window.CSSStyleDeclaration)+'|'+(document.body.style instanceof CSSStyleDeclaration)+'|'+(d?d.enumerable:'missing');})()",
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("function|true|false"));
    }

    /// Upstream ec05ed0: dataset is backed by a real DOMStringMap instance
    /// while data-* reflection stays dynamic.
    #[test]
    fn dom_string_map_is_exposed_and_backs_dataset() {
        let mut rt =
            setup_runtime(r#"<html><body><div id="x" data-foo="bar"></div></body></html>"#);
        let v = rt
            .evaluate(
                r#"(() => {
                    const el = document.getElementById("x");
                    const ds = el.dataset;
                    const iface = window.DOMStringMap;
                    const d = Object.getOwnPropertyDescriptor(window, "DOMStringMap");
                    let illegal = false;
                    try { new iface(); } catch (e) { illegal = e instanceof TypeError; }
                    ds.newKey = "1";
                    const reflected = el.getAttribute("data-new-key");
                    delete ds.foo;
                    return [typeof iface, ds instanceof iface,
                            Object.getPrototypeOf(ds) === iface.prototype,
                            ds.constructor === iface,
                            Object.prototype.toString.call(ds),
                            d ? d.enumerable : "missing", illegal, reflected,
                            el.hasAttribute("data-foo"), ds === el.dataset].join("|");
                })()"#,
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!(
                "function|true|true|true|[object DOMStringMap]|false|true|1|false|true"
            )
        );
    }

    /// Upstream 9dfc67a: the global's constructor identity is Window, not the
    /// inherited Object — framework environment gates check it directly.
    #[test]
    fn global_window_has_browser_constructor_identity() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                "(() => [window === self, self.constructor === Window, window instanceof Window, self.document === document, self.navigator === navigator])()",
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!([true, true, true, true, true])
        );
    }

    #[test]
    fn test_style_in_and_object_keys_cssom_parity() {
        // el.style was a bare get/set proxy: `'color' in el.style`,
        // Object.keys(el.style), and camelCase↔dashed sync all failed.
        let mut rt = setup_runtime(r#"<div id="el"></div>"#);
        let result = rt.evaluate(r#"
            const s = document.getElementById('el').style;
            s.fontSize = '20px';
            const keys = Object.keys(s);
            return [
                'color' in s,
                'gap' in s,
                'object-fit' in s,
                s.getPropertyValue('font-size'),
                s.fontSize,
                keys.includes('color'),
                keys.includes('fontSize'),
                s.cssText,
                s.length,
                s.item(0),
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true, true, true, "20px", "20px", true, true, "font-size: 20px;", 1, "font-size"
            ])
        );
    }

    #[test]
    fn test_dataset_in_and_object_keys() {
        // `'foo' in el.dataset` and Object.keys(el.dataset) must reflect data-*
        // attributes (CSSOM/DOMStringMap parity).
        let mut rt = setup_runtime(r#"<div id="el" data-foo-bar="1" data-baz="2"></div>"#);
        let result = rt.evaluate(r#"
            const d = document.getElementById('el').dataset;
            return [
                'fooBar' in d,
                'baz' in d,
                'missing' in d,
                Object.keys(d).sort(),
                d.fooBar,
                d.baz,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, true, false, ["baz", "fooBar"], "1", "2"])
        );
    }

    #[test]
    fn test_style_attribute_syncs_both_directions() {
        // CSSStyleDeclaration was in-memory only: parsed inline styles were
        // invisible to el.style.*, and el.style.x = … never reached the
        // attribute or serialization.
        let mut rt = setup_runtime(r#"<div id="el" style="color: red"></div>"#);
        let result = rt.evaluate(r#"
            const el = document.getElementById('el');
            const before = el.style.color;
            el.style.color = 'blue';
            const attrAfterSet = el.getAttribute('style');
            el.setAttribute('style', 'margin: 5px');
            const margin = el.style.margin;
            const colorGone = el.style.color;
            el.style.removeProperty('margin');
            return [before, attrAfterSet, margin, colorGone, el.getAttribute('style')];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!(["red", "color: blue;", "5px", "", null])
        );
    }

    #[test]
    fn test_insert_adjacent_html_case_insensitive_and_syntax_error() {
        // Position was matched case-sensitively (so 'BeforeEnd' silently
        // no-op'd) and an invalid position didn't throw SyntaxError.
        let mut rt = setup_runtime(r#"<div id="el"><span>child</span></div>"#);
        let result = rt.evaluate(r#"
            const el = document.getElementById('el');
            el.insertAdjacentHTML('BeforeEnd', '<b>X</b>');
            let threw = null;
            try { el.insertAdjacentHTML('sideways', '<i>Y</i>'); } catch (e) { threw = e.name; }
            return [el.innerHTML, threw];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!(["<span>child</span><b>X</b>", "SyntaxError"])
        );
    }

    #[test]
    fn test_script_runs_once_across_dom_move() {
        // Moving a <script> in the DOM must not execute its inline body a
        // second time (upstream 41a8e1c — "already started" flag).
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            window.__count = 0;
            const s = document.createElement('script');
            s.textContent = 'window.__count = (window.__count || 0) + 1;';
            host.appendChild(s);
            const afterFirst = window.__count;
            host.removeChild(s);
            host.appendChild(s);
            const afterMove = window.__count;
            const afterReinsert = (() => { host.removeChild(s); host.appendChild(s); return window.__count; })();
            return [afterFirst, afterMove, afterReinsert];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([1, 1, 1]));
    }

    #[test]
    fn test_cloned_script_does_not_rerun() {
        // cloneNode of a subtree whose script already ran must not run the
        // clone's script (started state propagates to the clone).
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            window.__count = 0;
            const box = document.createElement('div');
            const s = document.createElement('script');
            s.textContent = 'window.__count = (window.__count || 0) + 1;';
            box.appendChild(s);
            host.appendChild(box);
            const afterFirst = window.__count;
            const clone = box.cloneNode(true);
            host.appendChild(clone);
            return [afterFirst, window.__count];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([1, 1]));
    }

    #[test]
    fn test_innerhtml_script_is_inert() {
        // Scripts created by innerHTML never execute (per spec), unlike direct
        // DOM insertion.
        let mut rt = setup_runtime(r#"<div id="host"></div>"#);
        let result = rt.evaluate(r#"
            const host = document.getElementById('host');
            window.__count = 0;
            host.innerHTML = '<script>window.__count = 1;</script>';
            const afterInner = window.__count;
            // A directly-inserted script still runs.
            const s = document.createElement('script');
            s.textContent = 'window.__count = 2;';
            host.appendChild(s);
            return [afterInner, window.__count];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([0, 2]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_data_url_script_executes() {
        // Upstream 0c4740a + f841205: op_fetch_url's HTTP client cannot fetch
        // the data: scheme, so dynamic <script src="data:..."> never ran. The
        // decoder accepts any MIME, %-escapes, fragments, unpadded base64, and
        // non-ASCII via a UTF-8 round-trip — and load fires on every path.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            let loads = 0;
            const mk = (url) => {
                const s = document.createElement('script');
                s.setAttribute('src', url);
                s.addEventListener('load', () => loads++);
                s.addEventListener('error', () => loads -= 100);
                document.body.appendChild(s);
            };
            mk('data:,window.__a=1');
            mk("data:text/plain,window.__g='%C3%A9'");
            mk("data:text/javascript,window.__h='é'");
            mk('data:text/javascript,window.__i=9#frag');
            mk('data:text/javascript;base64,d2luZG93Ll9fYz0z');
            mk('data:text/javascript;base64,d2luZG93Ll9fZD00NA');
            await new Promise(r => setTimeout(r, 20));
            return [window.__a, window.__g, window.__h, window.__i, window.__c, window.__d, loads];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([1, "é", "é", 9, 3, 44, 6])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_data_url_script_invalid_base64_errors() {
        // Upstream f841205: a payload whose length % 4 === 1 can never be
        // valid base64; the decoder must throw instead of executing garbage,
        // and the script element fires error without evaluating anything.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            let errors = 0;
            const mk = (url) => {
                const s = document.createElement('script');
                s.setAttribute('src', url);
                s.addEventListener('error', () => errors++);
                document.body.appendChild(s);
            };
            mk('data:text/javascript;base64,AAAAA');
            mk('data:text/javascript;base64,ab!c');
            mk('data:,window.__ok=1');
            await new Promise(r => setTimeout(r, 20));
            return [errors, window.__ok];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([2, 1]));
    }

    // #15 (obscura#984): the DOM conversion trio must match Chrome —
    // contains() self-short-circuits to true, isSameNode() is identity (not
    // equality), and getElementById() coerces its argument to string (a
    // numeric 42 finds id="42") with the empty needle returning null.
    #[test]
    fn dom_conversion_trio_matches_chrome() {
        let mut rt = setup_runtime(
            r#"<html><body><div id="p"><span id="c">x</span></div><div id="q">y</div><div id="42">z</div></body></html>"#,
        );
        let v = rt
            .evaluate(
                r#"JSON.stringify([
                    document.getElementById('p').contains(document.getElementById('p')),
                    document.getElementById('p').contains(document.getElementById('c')),
                    document.getElementById('c').contains(document.getElementById('p')),
                    document.getElementById('p').contains(document.getElementById('q')),
                    document.getElementById('p').isSameNode(document.getElementById('p')),
                    document.getElementById('p').isSameNode(document.getElementById('q')),
                    document.getElementById('p').isSameNode(null),
                    document.getElementById(42) === document.getElementById('42'),
                    document.getElementById('') === null
                ])"#,
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!(r#"[true,true,false,false,true,false,false,true,true]"#)
        );
    }

    // #17: an uncaught exception inside a timer callback reaches BOTH error
    // surfaces Chrome exposes — window.onerror with (message, source, line)
    // and an ErrorEvent on window whose .error preserves the Error object.
    #[tokio::test(flavor = "current_thread")]
    async fn uncaught_timer_error_fires_onerror_and_error_event() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            const out = {};
            window.onerror = function (msg, src, line) {
                out.onerror = [
                    String(msg).indexOf('boom') !== -1,
                    typeof src === 'string' && src.length > 0,
                    typeof line === 'number' && line > 0,
                ];
            };
            window.addEventListener('error', function (e) {
                out.evt = [
                    e instanceof ErrorEvent,
                    String(e.message).indexOf('boom') !== -1,
                    e.error instanceof Error,
                    e.error && e.error.message === 'boom',
                    typeof e.filename === 'string' && e.filename.length > 0,
                    typeof e.lineno === 'number' && e.lineno > 0,
                ];
            });
            setTimeout(function () { throw new Error('boom'); }, 0);
            await new Promise(r => setTimeout(r, 20));
            return [out.onerror, out.evt];
        }"#;
        let result = rt
            .call_function_on_for_cdp(script, None, &[], true, true)
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([
                [true, true, true],
                [true, true, true, true, true, true]
            ])
        );
    }

    // #17: an exception raised INSIDE onerror must not re-enter the error
    // pipeline (Chrome does not re-report errors thrown by the error
    // handler) — exactly one onerror call for the original throw.
    #[tokio::test(flavor = "current_thread")]
    async fn throwing_onerror_does_not_reenter_error_pipeline() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            window.__calls = 0;
            window.onerror = function () { window.__calls++; throw new Error('inside onerror'); };
            setTimeout(function () { throw new Error('outer'); }, 0);
            await new Promise(r => setTimeout(r, 20));
            return window.__calls;
        }"#;
        let result = rt
            .call_function_on_for_cdp(script, None, &[], true, true)
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!(1));
    }

    // b12405d closed fetch()/XHR and classic <script src=data:>; the module
    // path was the remaining gap — import() and <script type=module> died in
    // the Rust module loader (validate_fetch_url/reqwest). The loader now
    // resolves data: locally, with Chromium's module MIME gate: empty essence
    // reads as text/plain and is refused, classic-style MIME blindness does
    // not apply here.
    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_import_data_url_module() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            const out = {};
            const imp = (k, u) => import(u)
                .then(m => { out[k] = String(m.default) + ':' + m.n; })
                .catch(e => { out[k] = 'ERR'; });
            await imp('a', 'data:text/javascript,export default 41; export const n = 1;');
            await imp('b', 'data:application/javascript;base64,ZXhwb3J0IGRlZmF1bHQgNDI7IGV4cG9ydCBjb25zdCBuID0gMjs=');
            // A charset param between the essence and the payload is fine.
            await imp('c', 'data:text/javascript;charset=utf-8,export default 43; export const n = 4;');
            await imp('d', 'data:,export default 1;');
            await imp('e', 'data:text/plain,export default 1;');
            return out;
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        let out = result.value.unwrap();
        assert_eq!(out["a"], "41:1");
        assert_eq!(out["b"], "42:2");
        assert_eq!(out["c"], "43:4");
        // Empty essence reads as text/plain and is refused, like Chrome.
        assert_eq!(out["d"], "ERR");
        assert_eq!(out["e"], "ERR");
    }

    // import(blob:) resolves through the Rust-side mirror of
    // URL.createObjectURL: works after create, rejects after revoke, and a
    // non-JavaScript blob type is refused like Chrome refuses it.
    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_import_blob_url_module() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            const out = {};
            const url = URL.createObjectURL(
                new Blob(['export default 7; export const n = 3;'], { type: 'text/javascript' }));
            try {
                const m = await import(url);
                out.ok = String(m.default) + ':' + m.n;
            } catch (e) { out.ok = 'ERR'; }
            // Revoked before it was ever imported: the fetch reaches the
            // mirror and misses, like Chrome. (A URL imported once and then
            // revoked keeps resolving from the module map — also Chrome.)
            const dead = URL.createObjectURL(
                new Blob(['export default 9;'], { type: 'text/javascript' }));
            URL.revokeObjectURL(dead);
            try { await import(dead); out.after = 'resolved'; }
            catch (e) { out.after = 'rejected'; }
            const u2 = URL.createObjectURL(new Blob(['export default 1;'], { type: 'text/plain' }));
            try { await import(u2); out.mime = 'resolved'; }
            catch (e) { out.mime = 'rejected'; }
            return out;
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        let out = result.value.unwrap();
        assert_eq!(out["ok"], "7:3");
        assert_eq!(out["after"], "rejected");
        assert_eq!(out["mime"], "rejected");
    }

    // Classic dynamic <script src="blob:"> executes the blob body (no MIME
    // gate, like every other classic path); a revoked URL fires error and
    // evaluates nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_blob_url_script_executes() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            let loads = 0, errors = 0;
            const url = URL.createObjectURL(
                new Blob(['window.__bs = 5;'], { type: 'text/javascript' }));
            const s = document.createElement('script');
            s.setAttribute('src', url);
            s.addEventListener('load', () => loads++);
            s.addEventListener('error', () => errors++);
            document.body.appendChild(s);
            const dead = URL.createObjectURL(new Blob(['window.__dead = 1;']));
            URL.revokeObjectURL(dead);
            const s2 = document.createElement('script');
            s2.setAttribute('src', dead);
            s2.addEventListener('error', () => errors++);
            document.body.appendChild(s2);
            await new Promise(r => setTimeout(r, 20));
            return [window.__bs, window.__dead === undefined, loads, errors];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([5, true, 1, 1]));
    }

    /// Upstream f61493f: the HTML script-fetch algorithm treats an
    /// unsuccessful HTTP response as a network error. A 404 body (here, one
    /// that would clobber a global if it ran) must never become script source.
    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_script_non_2xx_body_not_evaluated() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = b"window.__leak = 1;";
            let response = format!(
                "HTTP/1.1 404 Not Found\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let script = format!(r#"async () => {{
            let errors = 0, loads = 0;
            const s = document.createElement('script');
            s.setAttribute('src', 'http://127.0.0.1:{port}/missing.js');
            s.addEventListener('error', () => errors++);
            s.addEventListener('load', () => loads++);
            document.body.appendChild(s);
            await new Promise(r => setTimeout(r, 50));
            return [errors, loads, window.__leak === undefined];
        }}"#);
        let result = rt.call_function_on_for_cdp(&script, None, &[], true, true).await.unwrap();
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert_eq!(result.value.unwrap(), serde_json::json!([1, 0, true]));
    }

    /// Upstream a6bb741: a dynamic external script slower than the settle
    /// loop's 500ms fast-path deadline must still be visible as pending while
    /// in flight (so the loop keeps pumping) and must land once its fetch
    /// resolves — including after a failed fetch, where the finally-bracket
    /// must return the counter to zero.
    #[tokio::test(flavor = "current_thread")]
    async fn test_slow_dynamic_script_visible_as_pending_until_lands() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            std::thread::sleep(std::time::Duration::from_millis(300));
            let body = b"window.__slow = 1;";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/javascript\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        });

        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", port));
        let insert = format!(r#"
            const s = document.createElement('script');
            s.setAttribute('src', 'http://127.0.0.1:{port}/slow.js');
            document.body.appendChild(s);
        "#);
        rt.evaluate(&insert).unwrap();

        // Pump the event loop past 500ms while the 300ms-slow fetch is in
        // flight; the counter must be observed live at least once, the script
        // must land, and the counter must drain back to zero afterwards.
        let start = tokio::time::Instant::now();
        let mut saw_pending = false;
        while start.elapsed() < std::time::Duration::from_millis(2_000) {
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(20),
                rt.run_event_loop(),
            ).await;
            if rt.has_pending_dynamic_scripts() {
                saw_pending = true;
            }
            if saw_pending
                && !rt.has_pending_dynamic_scripts()
                && rt.evaluate("window.__slow").unwrap().as_f64() == Some(1.0)
            {
                break;
            }
        }
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

        assert!(saw_pending, "slow dynamic script fetch should be observable as pending");
        assert!(!rt.has_pending_dynamic_scripts(), "counter must drain after the fetch lands");
        assert_eq!(rt.evaluate("window.__slow").unwrap().as_f64(), Some(1.0));
    }

    /// Real browsers send and store cookies on cross-origin classic
    /// `<script src>` requests (the JSONP-era contract: taobao mtop's token
    /// refresh rotates `_m_h5_tk` via Set-Cookie on a cross-origin JSONP GET,
    /// and with credentials "same-origin" the refresh looped on
    /// FAIL_SYS_TOKEN_EMPTY forever, leaving decorated shop pages empty).
    /// Page on port A, script server on port B — different ports are
    /// different origins, same 127.0.0.1 host so jar cookies domain-match.
    #[tokio::test(flavor = "current_thread")]
    async fn test_dynamic_cross_origin_script_sends_and_stores_cookies() {
        // The tuple field is the RAII payload (holding the env lock); Drop
        // is its reader — it removes the env var when the guard releases.
        #[allow(dead_code)]
        struct PrivateNetGuard(std::sync::MutexGuard<'static, ()>);
        impl Drop for PrivateNetGuard {
            fn drop(&mut self) {
                std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
            }
        }
        let guard = PrivateNetGuard(crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap());
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

        // Owns the page origin's port (nothing serves it — the runtime's
        // document is injected locally; binding keeps the OS from reusing it).
        let page_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let page_port = page_listener.local_addr().unwrap().port();

        let script_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let script_port = script_listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..3 {
                let Ok((mut stream, _)) = script_listener.accept() else { return };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let sent_cookie = req.lines().any(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower.starts_with("cookie:") && lower.contains("probe=")
                });
                let body: &[u8] = if sent_cookie {
                    // Runs in the page realm, so document.cookie must already
                    // reflect the Set-Cookie stored from these very headers.
                    b"window.__dynEcho = 'COOKIE_SENT';"
                } else {
                    b"window.__dynEcho = 'NO_COOKIE';"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/javascript\r\nset-cookie: dyn_sc=stored; Path=/\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
                stream.flush().unwrap();
            }
        });

        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        rt.set_url(&format!("http://127.0.0.1:{}/page", page_port));
        jar.set_cookie(
            "probe=1; Path=/",
            &url::Url::parse(&format!("http://127.0.0.1:{}/page", page_port)).unwrap(),
        );

        let script = format!(r#"async () => {{
            const s = document.createElement('script');
            s.setAttribute('src', 'http://127.0.0.1:{script_port}/jsonp.js');
            document.body.appendChild(s);
            await new Promise(r => setTimeout(r, 300));
            return window.__dynEcho;
        }}"#);
        let result = rt.call_function_on_for_cdp(&script, None, &[], true, true).await.unwrap();
        drop(guard);

        assert_eq!(result.value.unwrap(), serde_json::json!("COOKIE_SENT"));
        let all = jar.get_all_cookies();
        let stored = all
            .iter()
            .find(|c| c.name == "dyn_sc")
            .expect("Set-Cookie from cross-origin dynamic script must land in the jar");
        assert_eq!(stored.value, "stored");
    }

    #[test]
    fn test_domparser_xml_parsererror_on_malformed() {
        // Upstream 53295fa+6927f11+869f700+20c4628: XML mime types get a
        // well-formedness pass; malformed input yields a <parsererror>
        // documentElement that querySelector('parsererror') finds, matching
        // Chrome. Self-closing roots count as complete elements. Well-formed
        // input (obscura#883) now builds a real tree: the no-xmlns root keeps
        // its source name read back through the HTML-namespace tagName
        // convention (uppercased), not an <html> collapse.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const check = (src) => {
                const doc = new DOMParser().parseFromString(src, "application/xml");
                const err = doc.querySelector('parsererror');
                return err ? ('E:' + doc.documentElement.tagName) : ('OK:' + doc.documentElement.tagName);
            };
            return [
                check('<root><a></b></root>'),   // tag mismatch
                check('<root></a></root>'),      // closing tag mismatch
                check('<root/><b/>'),            // extra content after root
                check('<root><a>'),              // unclosed tag
                check('<root><a>1</a></root>'),  // well-formed
                check('<root/>'),                // self-closing root is complete
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                "E:PARSERERROR", "E:PARSERERROR", "E:PARSERERROR", "E:PARSERERROR",
                "OK:root", "OK:root",
            ])
        );
    }

    #[test]
    fn test_domparser_xml_strict_fallback_and_html_unaffected() {
        // The hand-rolled state machine catches what the regex pass cannot
        // (here: zero root elements) and swaps in the generic parsererror.
        // HTML mime types never run either check; comments/CDATA/PI/DOCTYPE
        // are skipped by both layers.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const doc1 = new DOMParser().parseFromString('not xml at all', 'application/xml');
            const textOnly = !!doc1.querySelector('parsererror');
            const doc2 = new DOMParser().parseFromString('<div>hi</div>', 'text/html');
            const htmlOk = !doc2.querySelector('parsererror') && !!doc2.querySelector('div');
            const doc3 = new DOMParser().parseFromString(
                '<?xml version="1.0"?><!-- c --><root><![CDATA[x<y]]></root>', 'application/xml');
            const skipsNoise = !doc3.querySelector('parsererror');
            return [textOnly, htmlOk, skipsNoise];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, true, true]));
    }

    #[test]
    fn test_domparser_xml_tree_builder_namespaces_and_structure() {
        // obscura#883: valid XML under an XML mime must come back as a real
        // tree — source-case qualified names, xmlns-resolved namespaceURI
        // (prefixed scopes and default-namespace inheritance), self-closing
        // nesting, CDATA sections, decoded attribute entities — not the
        // <html> collapse the HTML fragment parser produced.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const p = (src, mime) => new DOMParser().parseFromString(src, mime || 'application/xml');
            const d1 = p('<edmx:Edmx xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices/></edmx:Edmx>');
            const d2 = p('<feed xmlns="http://www.w3.org/2005/Atom"><entry title="a&amp;b">hi</entry></feed>');
            const d3 = p('<r xmlns="http://example.com/x"><c><![CDATA[a<b & c]]></c><pi/></r>');
            const d4 = p('<bad><a></b></bad>');
            const d5 = p('<div>html path</div>', 'text/html');
            const root1 = d1.documentElement;
            const entry = d2.documentElement.firstElementChild;
            return [
                root1.tagName,
                root1.namespaceURI,
                root1.firstElementChild.tagName,
                root1.firstElementChild.localName,
                root1.firstElementChild.namespaceURI,
                d2.documentElement.tagName,
                d2.documentElement.namespaceURI,
                entry.namespaceURI,
                entry.getAttribute('title'),
                entry.querySelectorAll('ENTRY').length,
                d2.documentElement.querySelectorAll('entry').length,
                d3.documentElement.querySelectorAll('c').length,
                d3.documentElement.firstElementChild.firstChild.nodeType,
                d3.documentElement.firstElementChild.firstChild.data,
                d3.documentElement.children.length,
                !!d4.querySelector('parsererror'),
                d5.documentElement.tagName,
            ];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([
            "edmx:Edmx",
            "http://docs.oasis-open.org/odata/ns/edmx",
            "edmx:DataServices",
            "DataServices",
            "http://docs.oasis-open.org/odata/ns/edmx",
            "feed",
            "http://www.w3.org/2005/Atom",
            "http://www.w3.org/2005/Atom",
            "a&b",
            0,
            1,
            1,
            4,
            "a<b & c",
            2,
            true,
            "HTML",
        ]));
    }

    #[test]
    fn test_domparser_xml_img_serializes_with_closing_tag() {
        // Report 2026-09-15: an XML `<img size="123">text</img>` serialized
        // as a self-closing HTML void element — text child and closing tag
        // both dropped from outerHTML. Void-element self-closing is HTML-
        // only; null-namespace (no xmlns) XML elements must keep theirs.
        // No-xmlns XML elements also carry the null namespace, so tagName
        // keeps its source case (Chrome: XML docs never uppercase).
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const doc = new DOMParser().parseFromString('<img size="123">text</img>', 'text/xml');
            const root = doc.documentElement;
            return [
                root.tagName,
                root.getAttribute('size'),
                root.textContent,
                root.outerHTML,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                "img", "123", "text", "<img size=\"123\">text</img>",
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_dialog_escape_helper_closes_modal_only() {
        // obscura#952: the bootstrap helper behind the CDP Escape arm walks
        // from the focused element to the containing modal dialog (falls back
        // to the open modal dialog in document order), runs the close request
        // (cancelable `cancel`), and never touches non-modal dialogs.
        // show()/showModal() schedule setTimeout toggles, whose op_sleep
        // needs a current-thread reactor — plain #[test] evaluate has none.
        let mut rt = setup_runtime(r#"<html><body>
            <dialog id="m"><input id="in"><button id="ok">OK</button></dialog>
            <dialog id="nm">non-modal</dialog>
        </body></html>"#);
        let result = rt.evaluate(r#"
            const m = document.getElementById('m');
            const nm = document.getElementById('nm');
            nm.show();
            let cancels = 0, wasCancelable = false;
            m.addEventListener('cancel', () => { cancels++; });
            // No modal dialog anywhere: helper is a no-op, non-modal untouched.
            const noneOpen = globalThis.__diting_dialogEscapeClose();
            const nmUntouched = nm.hasAttribute('open');
            // Modal open, focus inside it: cancel fires, dialog closes.
            m.showModal();
            document.getElementById('in').focus();
            const viaFocus = globalThis.__diting_dialogEscapeClose();
            const closedAfterFocus = !m.hasAttribute('open');
            // preventDefault inside cancel keeps the modal open (Chrome Escape
            // semantics); the CDP arm checks keydown defaultPrevented too.
            m.showModal();
            m.addEventListener('cancel', (e) => { wasCancelable = e.cancelable && !e.defaultPrevented; e.preventDefault(); });
            document.getElementById('ok').focus();
            globalThis.__diting_dialogEscapeClose();
            const stillOpenAfterPrevent = m.hasAttribute('open');
            return [noneOpen, nmUntouched, viaFocus, closedAfterFocus, stillOpenAfterPrevent, cancels, wasCancelable];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([false, true, true, true, true, 2, true]));
    }

    #[test]
    fn test_form_submit_bypasses_event_request_submit_fires_it() {
        // Upstream 7e2cabf + ccfa5fb: submit() is a direct pass-through that
        // a submit listener cannot veto; only requestSubmit() (and user
        // clicks) fire the cancelable submit event. requestSubmit's submitter
        // must be a submit button owned by this form.
        let mut rt = setup_runtime(r#"
            <form id="f" action="/go"><input name="q" value="x">
                <button type="submit" id="b">Go</button></form>
            <form id="other"><button type="submit" id="ob">Go</button></form>
            <div id="notabutton"></div>"#);
        // submit(): no event, navigation happens.
        let r = rt.evaluate(r#"
            const form = document.getElementById('f');
            globalThis.__evts = 0;
            form.addEventListener('submit', () => globalThis.__evts++);
            form.submit();
            return [globalThis.__evts];
        "#).unwrap();
        assert_eq!(r, serde_json::json!([0]));
        assert!(rt.take_pending_navigation().is_some(), "submit() must navigate");

        // requestSubmit(): event fires; preventDefault stops navigation.
        let r = rt.evaluate(r#"
            const form = document.getElementById('f');
            form.addEventListener('submit', e => e.preventDefault());
            form.requestSubmit();
            return [globalThis.__evts];
        "#).unwrap();
        assert_eq!(r, serde_json::json!([1]));
        assert!(rt.take_pending_navigation().is_none(), "preventDefault must veto navigation");

        // Submitter validation (ccfa5fb): non-submit-button -> TypeError;
        // foreign submit button -> NotFoundError; valid one fires the event.
        let r = rt.evaluate(r#"
            const form = document.getElementById('f');
            const out = {};
            try { form.requestSubmit(document.getElementById('notabutton')); out.a = 'no-throw'; }
            catch (e) { out.a = e.name; }
            try { form.requestSubmit(document.getElementById('ob')); out.b = 'no-throw'; }
            catch (e) { out.b = e.name; }
            form.requestSubmit(document.getElementById('b'));
            out.c = globalThis.__evts;
            return [out.a, out.b, out.c];
        "#).unwrap();
        // The preventDefault listener from the previous step is still attached,
        // so the valid requestSubmit fires the event (2 total) but does not
        // navigate.
        assert_eq!(r, serde_json::json!(["TypeError", "NotFoundError", 2]));
        assert!(rt.take_pending_navigation().is_none());
    }

    #[test]
    fn test_select_parity_type_selectedindex_add_no_change_on_assign() {
        // Upstream 5308e04: select/textarea report fixed IDL types;
        // a single select implicitly selects its first option (a multiple
        // one idles at -1); programmatic value assignment never fires
        // change (assigning inside a change handler used to loop forever).
        let mut rt = setup_runtime(r#"
            <select id="s"><option value="a">A</option><option value="b">B</option></select>
            <select id="m" multiple><option value="a">A</option></select>
            <textarea id="t"></textarea>"#);
        let result = rt.evaluate(r#"
            const s = document.getElementById('s');
            const m = document.getElementById('m');
            const t = document.getElementById('t');
            let changes = 0;
            s.addEventListener('change', () => changes++);
            s.value = 'b';
            const afterAssign = [changes, s.value, s.selectedIndex];
            s.selectedIndex = 0;
            const afterIndex = [s.value, s.selectedIndex];
            const types = [s.type, m.type, t.type];
            const emptySingle = document.createElement('select');
            const emptyMultiple = document.createElement('select');
            emptyMultiple.setAttribute('multiple', '');
            const opt = document.createElement('option');
            opt.setAttribute('value', 'c'); opt.textContent = 'C';
            s.add(opt);
            return [
                afterAssign, afterIndex, types,
                emptySingle.selectedIndex, emptyMultiple.selectedIndex,
                s.options.length, changes,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                [0, "b", 1],          // no change on assignment; selection moved
                ["a", 0],             // selectedIndex setter works both ways
                ["select-one", "select-multiple", "textarea"],
                -1, -1,               // empty selects idle at -1
                3,                    // add() appended the option
                0,                    // assignment never fired change
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_image_is_real_element_and_emulates_load() {
        // Upstream a5a8de7 + 891d850: new Image() must be a real element
        // (style/attribute reflection/event dispatch), assigning a src must
        // fire `load` on both the onload property and listeners (#41: for a
        // data: URL the load face is local; network srcs now gate on a real
        // fetch — see image_factory_fires_* tests), and a pre-defined
        // non-configurable own src (Booking.com instrumentation) must not
        // crash the constructor.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            const img = new Image(10, 20);
            const isEl = img instanceof globalThis.HTMLImageElement;
            const styleOk = img.style instanceof globalThis.CSSStyleDeclaration;
            img.style.width = '30px';
            const styleSet = img.style.width === '30px';
            img.width = 10; img.height = 20;
            let viaProp = 0, viaListener = 0;
            img.onload = () => viaProp++;
            img.addEventListener('load', () => viaListener++);
            img.src = 'data:image/gif;base64,R0lGODlhAQABAAAAACw=';
            const earlyComplete = img.complete;
            await new Promise(r => setTimeout(r, 20));
            // Anti-bot pattern: hijack createElement and pre-define a
            // non-configurable own src on every <img>.
            const origCreate = document.createElement.bind(document);
            document.createElement = function (tag) {
                const el = origCreate(tag);
                if (String(tag).toLowerCase() === 'img') {
                    Object.defineProperty(el, 'src', { value: '', writable: true, configurable: false });
                }
                return el;
            };
            let hijackSurvived = false, hijackW = 0;
            try {
                const img2 = new Image(7, 8);
                hijackSurvived = true;
                hijackW = img2.width;
            } catch (e) { hijackSurvived = e.message; }
            document.createElement = origCreate;
            return [isEl, styleOk, styleSet, earlyComplete, img.complete,
                    img.naturalWidth, viaProp, viaListener, hijackSurvived, hijackW];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([true, true, true, false, true, 10, 1, 1, true, 7])
        );
    }

    #[test]
    fn test_network_information_event_listeners() {
        // Upstream fc9f524: navigator.connection was a data-only object with
        // no event methods at all; analytics libs calling addEventListener
        // threw. dispatchEvent must run registered listeners with the
        // connection as receiver, honor the on* property, and respect
        // removeEventListener.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const connection = navigator.connection;
            let calls = 0, receiverMatches = false, viaProp = 0;
            function listener(event) {
                calls += 1;
                receiverMatches = this === connection && event.type === 'change';
            }
            connection.addEventListener('change', listener);
            connection.onchange = () => viaProp++;
            const dispatchResult = connection.dispatchEvent(new Event('change'));
            connection.removeEventListener('change', listener);
            connection.dispatchEvent(new Event('change'));
            return [
                typeof connection.addEventListener,
                typeof connection.removeEventListener,
                typeof connection.dispatchEvent,
                dispatchResult, calls, receiverMatches, viaProp,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!(["function", "function", "function", true, 1, true, 2])
        );
    }

    #[test]
    fn test_document_referrer_semantics() {
        // Upstream edb1785: document.referrer is explicit navigation state —
        // empty for direct automation navigations, the strict-origin-
        // when-cross-origin value for document-initiated hops.
        let mut rt = setup_runtime("<html><body></body></html>");
        assert_eq!(rt.evaluate("document.referrer").unwrap(), serde_json::json!(""));
        rt.set_referrer("https://source.example/path?q=1");
        assert_eq!(
            rt.evaluate("document.referrer").unwrap(),
            serde_json::json!("https://source.example/path?q=1")
        );
    }

    #[test]
    fn test_thrown_error_in_one_script_does_not_stop_later_scripts() {
        // Upstream 5c3d560 (regression for #355/#358): an uncaught throw in
        // one inline script must not prevent later independent scripts from
        // running — the babel-polyfill double-load pattern.
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script("s1", "globalThis.__ran1 = true;").unwrap();
        let err = rt
            .execute_script("s2", "throw new Error('only one instance of babel-polyfill is allowed');")
            .unwrap_err();
        assert!(err.contains("babel-polyfill"), "expected the thrown message, got: {}", err);
        rt.execute_script("s3", "globalThis.__ran3 = true;").unwrap();
        let ran = rt
            .evaluate("[globalThis.__ran1 === true, globalThis.__ran3 === true]")
            .unwrap();
        assert_eq!(ran, serde_json::json!([true, true]));
    }

    #[test]
    fn test_event_constructor_webidl_semantics() {
        // Upstream af1e15f: no-arg constructors throw, type coerces to string,
        // CustomEvent.detail defaults to null, createEvent still builds "" type.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const out = [];
            try { new Event(); out.push('no-throw'); } catch (e) { out.push(e.name); }
            try { new CustomEvent(); out.push('no-throw'); } catch (e) { out.push(e.name); }
            out.push(new Event(123).type + ':' + typeof new Event(123).type);
            out.push(String(new CustomEvent('x').detail));
            out.push(String(new CustomEvent('x', { detail: 7 }).detail));
            out.push(new Event('click').type);
            out.push(document.createEvent('Event').type);
            return out.join('|');
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!("TypeError|TypeError|123:string|null|7|click|")
        );
    }

    #[test]
    fn test_promise_rejection_event_requires_promise() {
        // Upstream 0ff1ba0 + 776c915: the promise member is required; the
        // class must exist globally (core-js feature-detects it).
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const promise = Promise.resolve(1);
            const event = new PromiseRejectionEvent('unhandledrejection', { promise, reason: 'failed' });
            let missingThrows = false;
            try { new PromiseRejectionEvent('unhandledrejection'); } catch (e) { missingThrows = e instanceof TypeError; }
            let nullInitThrows = false;
            try { new PromiseRejectionEvent('unhandledrejection', {}); } catch (e) { nullInitThrows = e instanceof TypeError; }
            return [event instanceof Event, event.promise === promise, event.reason, missingThrows, nullInitThrows];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, true, "failed", true, true]));
    }

    #[test]
    fn test_storage_event_constructor_and_legacy_factory() {
        // Upstream 776c915: StorageEvent global + legacy createEvent/initStorageEvent path.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const event = new StorageEvent('storage', {
                key: 'theme', oldValue: 'light', newValue: 'dark', url: 'https://example.test/'
            });
            const legacy = document.createEvent('StorageEvent');
            legacy.initStorageEvent('storage', false, false, 'count', '1', '2', 'https://example.test/', null);
            return [
                event instanceof Event,
                event.key, event.oldValue, event.newValue, event.url,
                legacy instanceof StorageEvent, legacy.key, legacy.newValue
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, "theme", "light", "dark", "https://example.test/", true, "count", "2"])
        );
    }

    #[test]
    fn test_create_event_rejects_unknown_and_supports_legacy_aliases() {
        // Upstream 7e6f403: unknown interface names throw NotSupportedError;
        // the DOM Level 2 aliases and hashchange/message map entries resolve.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            let rejected = null;
            try { document.createEvent('NotAnEventInterface'); } catch (e) { rejected = [e.name, e instanceof DOMException]; }
            const aliases = ['Event', 'Events', 'HTMLEvents', 'SVGEvents'].map(name => {
                const event = document.createEvent(name);
                return [event instanceof Event, event.constructor === Event, event.type];
            });
            const hash = document.createEvent('HashChangeEvent') instanceof HashChangeEvent;
            const message = document.createEvent('MessageEvent') instanceof MessageEvent;
            let preRejects = null;
            try { document.createEvent('PromiseRejectionEvent'); } catch (e) { preRejects = e.name; }
            return [rejected, aliases, hash, message, preRejects];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                ["NotSupportedError", true],
                [[true, true, ""], [true, true, ""], [true, true, ""], [true, true, ""]],
                true, true, "NotSupportedError"
            ])
        );
    }

    #[test]
    fn test_iframe_document_event_listeners() {
        // Upstream 2e3f5d8: addEventListener/removeEventListener/dispatchEvent
        // on an iframe document used to be no-ops.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const iframe = document.createElement('iframe');
            document.body.appendChild(iframe);
            const doc = iframe.contentDocument;
            let calls = 0;
            const listener = () => calls++;
            doc.addEventListener('probe', listener);
            doc.dispatchEvent(new Event('probe'));
            const afterRegister = calls;
            doc.addEventListener('probe', listener);
            doc.addEventListener('probe', listener);
            doc.dispatchEvent(new Event('probe'));
            const afterDuplicate = calls;
            doc.removeEventListener('probe', listener);
            doc.dispatchEvent(new Event('probe'));
            const afterRemove = calls;
            doc.addEventListener('cancelme', e => e.preventDefault());
            const cancelReturn = doc.dispatchEvent(new Event('cancelme', { cancelable: true }));
            const plainReturn = doc.dispatchEvent(new Event('nolisteners'));
            return [!!doc, afterRegister, afterDuplicate, afterRemove, cancelReturn, plainReturn];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, 1, 2, 2, false, true]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_element_scroll_offsets_and_scroll_event_coalescing() {
        // Upstream 29e20ae + 1c7402d: scrollTop/scrollLeft round-trip, direct
        // assignment fires a scroll event (only on change), and scroll
        // operations coalesce to one event per call.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            const el = document.createElement('div');
            document.body.appendChild(el);
            let events = 0;
            el.addEventListener('scroll', () => events++);
            el.scrollTop = 100;          // changed -> 1 event
            el.scrollTop = 100;          // unchanged -> no event
            el.scrollTo(0, 250);         // one coalesced event
            el.scrollBy({ left: 30, top: 50 });
            el.scroll(0, -5);            // clamps both axes back to 0, 1 event
            const offsets = [el.scrollTop, el.scrollLeft];
            await new Promise(r => setTimeout(r, 10));
            return [offsets, events];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([[0, 0], 4]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_window_scroll_moves_page_offset_shared_with_scrolling_element() {
        // Upstream f6ca133: window scroll methods move the page offset stored
        // on the scrolling element; scrollX/scrollY/pageXOffset/pageYOffset are
        // views of it, and a window scroll reaches document AND window listeners.
        // Tall page: the root scroller clamps to the real scroll range now, so
        // the offsets below must stay inside it to actually move.
        let mut rt = setup_runtime(r#"<html><body><div id="d"></div><div style="height:5000px"></div><div style="width:5000px;height:1px"></div></body></html>"#);
        let script = r#"async () => {
            const isDocEl = document.scrollingElement === document.documentElement;
            window.scrollTo(0, 500);
            const afterTo = [window.scrollX, window.scrollY];
            window.scrollBy(0, 200);
            const afterBy = [window.pageXOffset, window.pageYOffset];
            window.scrollTo({ left: 10, top: 40 });
            const afterOptions = [window.scrollX, window.scrollY];
            window.scrollTo(0, -100);
            const afterClamp = window.scrollY;
            document.scrollingElement.scrollTop = 90;
            const viaWindow = window.scrollY;
            let win = 0, doc = 0;
            window.addEventListener('scroll', () => win++);
            document.addEventListener('scroll', () => doc++);
            window.scrollBy(0, 400);
            await new Promise(r => setTimeout(r, 10));
            // Five window scroll ops ran in total (four above the listeners
            // plus the final scrollBy); each fires exactly one scroll at the
            // document and one at the window, all drained by the await.
            return [isDocEl, afterTo, afterBy, afterOptions, afterClamp, viaWindow, win, doc, window.scrollY];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([true, [0, 500], [0, 700], [10, 40], 0, 90, 5, 5, 490])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_scrollend_fires_after_scroll_completes_and_not_without_translation() {
        // Blitz#354: scrollend trails scroll once scrolling finishes (same
        // tick here — our scrolls are instant) and, unlike scroll, bubbles —
        // so the element scrollend also reaches document listeners (as in
        // Chrome), while window listeners hear only the window-path fire.
        // The negative clause holds too: a scroll op that translated nothing
        // fires neither scroll nor scrollend. Tall page so the window scroll
        // has a real range to move in (the root scroller clamps to it now).
        let mut rt = setup_runtime("<html><body><div style=\"height:5000px\"></div></body></html>");
        let script = r#"async () => {
            const el = document.createElement('div');
            document.body.appendChild(el);
            let se = 0, wse = 0, dse = 0;
            el.addEventListener('scrollend', () => se++);
            window.addEventListener('scrollend', () => wse++);
            document.addEventListener('scrollend', () => dse++);
            el.scrollTop = 100;       // moved -> element scrollend, bubbles to document
            el.scrollTo(0, 100);      // same position -> no move -> nothing
            window.scrollTo(0, 400);  // moved -> document + window scrollend
            window.scrollTo(0, 400);  // same position -> no move -> nothing
            await new Promise(r => setTimeout(r, 10));
            return [se, wse, dse];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([1, 1, 2]));
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_root_scroll_mirrors_to_native_band_paint_state() {
        // AginxOS P0: the CDP band painter reads the root scroll offset from
        // JsState. window.scrollTo and direct root writes (through either the
        // html or the body wrapper — CSSOM View proxies body to the scrolling
        // element) must all land in that one native pair.
        // Tall AND wide: the root scroller clamps to the real scroll range on
        // both axes now, so the horizontal writes below need a wide document
        // to stay inside the range (a viewport-wide page pins scrollLeft at 0,
        // the same way Chrome does).
        let mut rt = setup_runtime(r#"<html><body><div style="height:5000px"></div><div style="width:5000px;height:1px"></div></body></html>"#);
        let script = r#"async () => {
            window.scrollTo(0, 300);
            document.documentElement.scrollLeft = 20;
            document.body.scrollTop = 1000;   // proxies to the scrolling element
            await new Promise(r => setTimeout(r, 10));
            return [
                window.scrollY,
                document.body.scrollTop,
                document.documentElement.scrollTop,
            ];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([1000, 1000, 1000]));
        let (sx, sy) = rt.with_state(|st| st.scroll_offset);
        assert_eq!((sx, sy), (20.0, 1000.0));
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_sticky_header_pins_gbcr_and_hit_test_under_root_scroll() {
        // #434 sticky v1: sticky vs the ROOT scroller. gBCR serves doc-space
        // boxes with the sticky shift folded in, so a top:0 header that has
        // stuck reads top === scrollY; elementFromPoint is a CLIENT-space
        // query, so the same stuck header must win at client y=5 once the
        // hit-test rides the root scroll. The tall later static sibling is
        // the load-bearing part of the hit assertion: its box covers the
        // stuck band, so the header only wins because sticky is a POSITIONED
        // paint level (App. E step 8), not document order.
        let mut rt = setup_runtime(
            r#"<html><body><div id="head" style="position:sticky; top:0; height:40px; background:#ccc">H</div><div id="tall" style="height:4000px; background:#eee">filler</div></body></html>"#,
        );
        let script = r#"async () => {
            const head = document.getElementById('head');
            const pos = getComputedStyle(head).position;
            const inFlow = head.getBoundingClientRect().top;
            window.scrollTo(0, 300);
            const stuck = head.getBoundingClientRect().top;
            const hitEl = document.elementFromPoint(10, 5);
            const hitId = hitEl ? (hitEl.id || '') : 'null';
            window.scrollTo(0, 0);
            await new Promise(r => setTimeout(r, 10));
            const back = head.getBoundingClientRect().top;
            return [
                pos,
                inFlow < 50,
                Math.abs(stuck - 300) < 0.5,
                hitId,
                Math.abs(back - inFlow) < 0.5,
            ];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!(["sticky", true, true, "head", true]),
            "computed face, in-flow rest, stuck top == scrollY, client hit lands on the stuck header, back to in-flow"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_sticky_with_z_index_outranks_later_static_content() {
        // #434 second half: a sticky with an explicit z-index joins the
        // hoisted positive band. On uv-docs (mkdocs-material) the stuck
        // .md-header carries z-index:4 while a later static .md-container
        // spans the whole scroll — before sticky counted as positioned, the
        // header ranked static (level 0) and lost both paint and hit order
        // to that sibling. (Cross-level ordering vs an absolute overlay
        // parented at the ICB stays out of scope: per-parent bands are the
        // engine's documented approximation.)
        let mut rt = setup_runtime(
            r#"<html><body><div id="head" style="position:sticky; top:0; height:40px; background:#ccc; z-index:4">H</div><div id="tall" style="height:4000px; background:#eee">filler</div></body></html>"#,
        );
        let script = r#"async () => {
            window.scrollTo(0, 300);
            await new Promise(r => setTimeout(r, 10));
            const pick = (y) => {
                const el = document.elementFromPoint(10, y);
                return el ? (el.id || el.tagName) : 'null';
            };
            const stack = document.elementsFromPoint(10, 5)
                .slice(0, 2).map(el => el.id || el.tagName);
            const out = [pick(5), pick(45), stack];
            window.scrollTo(0, 0);
            return out;
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!(["head", "tall", ["head", "tall"]]),
            "stuck z:4 header owns its band; past the band the static sibling answers again"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_sticky_header_stops_at_containing_block_end() {
        // #434: the sticky shift clamps to the containing block — a 40px
        // header inside a 200px wrap never scrolls past wrap.bottom - 40,
        // however deep the document scrolls.
        let mut rt = setup_runtime(
            r#"<html><body><div id="wrap" style="height:200px"><div id="head" style="position:sticky; top:0; height:40px; background:#ccc">H</div></div><div style="height:4000px"></div></body></html>"#,
        );
        let script = r#"async () => {
            const head = document.getElementById('head');
            const wrap = document.getElementById('wrap');
            window.scrollTo(0, 300);
            await new Promise(r => setTimeout(r, 10));
            const stuck = head.getBoundingClientRect().top;
            const stop = wrap.getBoundingClientRect().top + wrap.getBoundingClientRect().height - 40;
            return [Math.abs(stuck - stop) < 0.5, stuck];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        let value = result.value.unwrap();
        assert_eq!(
            value.as_array().unwrap().first().unwrap(),
            &serde_json::json!(true),
            "stuck top == wrap.bottom - 40 (raw value: {value})"
        );
    }

    // Cross-parent z (the narrow slice): a positioned z>0 child of a
    // non-stacking-context parent hoists to the nearest context ancestor.
    // Chrome ground truth: elementFromPoint(50,50) is B, not the later
    // sibling C(z:1) — diting used to answer C because B never left A's
    // per-parent band.
    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_cross_parent_z_hoists_to_stacking_context() {
        let mut rt = setup_runtime(
            r#"<!doctype html><html><body style="margin:0">
    <div id="A" style="position:relative;height:100px"><div id="B" style="position:absolute;z-index:5;top:0;left:0;width:100px;height:100px;background:red"></div></div>
    <div id="C" style="position:relative;z-index:1;margin-top:-100px;width:100px;height:100px;background:blue"></div>
    </body></html>"#,
        );
        let script = r#"() => {
          const el = document.elementFromPoint(50, 50);
          return el ? el.id : 'none';
        }"#;
        let out = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(out.value.unwrap(), serde_json::json!("B"));
    }

    // The escape bubbles through a CHAIN of non-context frames: two
    // relative z-auto intermediates both refuse to consume B(z:5), so it
    // lands at the root context and out-paints C(z:1) — Chrome parity,
    // since relative z:auto establishes no context.
    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_cross_parent_z_bubbles_through_two_non_context_levels() {
        let mut rt = setup_runtime(
            r#"<!doctype html><html><body style="margin:0">
    <div id="A" style="position:relative;height:100px"><div id="M" style="position:relative;height:50px"><div id="B" style="position:absolute;z-index:5;top:0;left:0;width:100px;height:100px;background:red"></div></div></div>
    <div id="C" style="position:relative;z-index:1;margin-top:-100px;width:100px;height:100px;background:blue"></div>
    </body></html>"#,
        );
        let script = r#"() => {
          const el = document.elementFromPoint(50, 50);
          return el ? el.id : 'none';
        }"#;
        let out = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(out.value.unwrap(), serde_json::json!("B"));
    }

    // A context intermediate consumes the hoist: opacity<1 makes A its own
    // stacking context, so B(z:5) is confined inside A's group and the
    // later C(z:1) out-paints the whole group — Chrome parity.
    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_cross_parent_z_context_intermediate_consume() {
        let mut rt = setup_runtime(
            r#"<!doctype html><html><body style="margin:0">
    <div id="A" style="position:relative;height:100px;opacity:0.5"><div id="B" style="position:absolute;z-index:5;top:0;left:0;width:100px;height:100px;background:red"></div></div>
    <div id="C" style="position:relative;z-index:1;margin-top:-100px;width:100px;height:100px;background:blue"></div>
    </body></html>"#,
        );
        let script = r#"() => {
          const el = document.elementFromPoint(50, 50);
          return el ? el.id : 'none';
        }"#;
        let out = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(out.value.unwrap(), serde_json::json!("C"));
    }

    // DOCUMENTED DEVIATION (narrow slice): a clipping intermediate pins the
    // z child locally (a hoisted range would tear the Clip/PopClip pair).
    // Chrome hoists AND keeps the clip, answering B here; diting answers C
    // until clip-pair surgery makes hoisting clip-aware — flip this pin
    // consciously when that lands.
    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_cross_parent_z_clip_intermediate_pins_locally() {
        let mut rt = setup_runtime(
            r#"<!doctype html><html><body style="margin:0">
    <div id="A" style="position:relative;height:100px;overflow:hidden"><div id="B" style="position:absolute;z-index:5;top:0;left:0;width:100px;height:100px;background:red"></div></div>
    <div id="C" style="position:relative;z-index:1;margin-top:-100px;width:100px;height:100px;background:blue"></div>
    </body></html>"#,
        );
        let script = r#"() => {
          const el = document.elementFromPoint(50, 50);
          return el ? el.id : 'none';
        }"#;
        let out = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(out.value.unwrap(), serde_json::json!("C"));
    }
    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_element_scroller_clamps_and_shifts_descendant_gbcr() {
        // sticky v2: an overflow:auto element is a real scroll container.
        // scrollTop clamps to extent-client (1000-200), the write mirrors
        // into the native scroll table, and the child's gBCR travels by
        // exactly the scroll — gBCR and paint read the same shift map.
        let mut rt = setup_runtime(
            r#"<html><body><div id="box" style="height:200px; overflow:auto; margin:0; padding:0; border:none"><div id="kid" style="height:1000px">x</div></div></body></html>"#,
        );
        let script = r#"async () => {
            const box = document.getElementById('box');
            const kid = document.getElementById('kid');
            const before = kid.getBoundingClientRect().top;
            box.scrollTop = 99999;
            await new Promise(r => setTimeout(r, 10));
            const after = kid.getBoundingClientRect().top;
            const sh = box.scrollHeight;
            return [box.scrollTop, Math.round(before - after), sh >= 1000 && sh <= 1016];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([800, 800, true]),
            "clamped to extent-client, child gBCR drops by the scroll, scrollHeight reflects content"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_sticky_pins_inside_element_scroller() {
        // sticky v2's composition case: a sticky header inside a scrolled
        // container reads TOTAL zero (its own +400 pin cancels the
        // scroller's -400 base) yet must stay pinned at the port top in
        // gBCR — the delta-liveness semantics apply_sticky_to_items pins.
        let mut rt = setup_runtime(
            r#"<html><body><div id="box" style="height:300px; overflow:auto; margin:0; padding:0; border:none"><div id="head" style="position:sticky; top:0; height:30px">H</div><div id="tall" style="height:1500px">filler</div></div></body></html>"#,
        );
        let script = r#"async () => {
            const box = document.getElementById('box');
            const head = document.getElementById('head');
            const tall = document.getElementById('tall');
            const boxTop = box.getBoundingClientRect().top;
            const rest = head.getBoundingClientRect().top;
            const tallBefore = tall.getBoundingClientRect().top;
            box.scrollTop = 400;
            await new Promise(r => setTimeout(r, 10));
            const mid = box.scrollTop;
            const stuck = head.getBoundingClientRect().top;
            const tallAfter = tall.getBoundingClientRect().top;
            box.scrollTop = 0;
            await new Promise(r => setTimeout(r, 10));
            const back = head.getBoundingClientRect().top;
            return [
                mid,
                Math.abs(stuck - boxTop) < 0.5,
                Math.abs((tallBefore - tallAfter) - 400) < 0.5,
                Math.abs(back - rest) < 0.5,
            ];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([400, true, true, true]),
            "head stays at the scroller's port top, tall travels with the scroll, reset restores in-flow"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_nested_element_scrollers_compose_shifts() {
        // Two stacked scrollers: outer=100 shifts the inner BOX (and its
        // subtree) by -100; inner=50 shifts the leaf another -50. The
        // nearest-source base telescopes, so the leaf's gBCR drop is the
        // SUM, 150 — outer's later write must not clobber inner's more
        // specific descendant shift.
        let mut rt = setup_runtime(
            r#"<html><body><div id="outer" style="height:200px; overflow:auto; margin:0; padding:0; border:none"><div id="spacer" style="height:500px"></div><div id="inner" style="height:150px; overflow:auto; margin:0; padding:0; border:none"><div id="leaf" style="height:600px">L</div></div></div></body></html>"#,
        );
        let script = r#"async () => {
            const outer = document.getElementById('outer');
            const inner = document.getElementById('inner');
            const leaf = document.getElementById('leaf');
            const before = leaf.getBoundingClientRect().top;
            outer.scrollTop = 100;
            inner.scrollTop = 50;
            await new Promise(r => setTimeout(r, 10));
            const after = leaf.getBoundingClientRect().top;
            return [outer.scrollTop, inner.scrollTop, Math.round(before - after)];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([100, 50, 150]),
            "both writes round-trip clamped, leaf drop = outer + inner"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_css_time_clock_and_animation_extent_reach_native_state() {
        // #417: a declarative CSS animation (SVG keyframes) has no
        // __timelines entry — the video pump drives a virtual clock through
        // set_css_time and reads the timeline length from css_extent, both
        // folded by the layout run.
        let mut rt = setup_runtime(
            r#"<html><head><style>
                @keyframes k { from { opacity: 0 } to { opacity: 1 } }
                @keyframes slow { from { opacity: 0 } to { opacity: 1 } }
                .a { animation: k 1s .5s forwards }
                .b { animation: slow 2s forwards }
            </style></head><body><div class="a">x</div><div class="b">y</div></body></html>"#,
        );
        // Any layout-driven read runs the layout pass that folds the extent.
        let script = r#"() => document.querySelector('.a').getBoundingClientRect().height"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert!(result.value.is_some(), "layout read succeeds");
        let extent = rt.with_state(|st| st.css_extent.get());
        assert!((extent - 2.0).abs() < 1e-6, "max(delay+duration) = 2.0, got {extent}");
        assert_eq!(rt.with_state(|st| st.css_time), None, "clock starts unset (static)");

        // The pump drives the clock through the same op the Page method uses.
        async fn set_time(rt: &mut JsRuntime, t: &str) {
            let script = format!(r#"() => __diting_domRaw('set_css_time', '{t}')"#);
            rt.call_function_on_for_cdp(&script, None, &[], true, true).await.unwrap();
        }
        set_time(&mut rt, "0.75").await;
        assert_eq!(rt.with_state(|st| st.css_time), Some(0.75));
        // Non-finite and negative inputs are rejected; the last valid time
        // holds.
        set_time(&mut rt, "-1").await;
        set_time(&mut rt, "NaN").await;
        set_time(&mut rt, "abc").await;
        assert_eq!(rt.with_state(|st| st.css_time), Some(0.75));

        // The sampler answers to the clock end-to-end: getComputedStyle
        // opacity mid-animation reflects the eased stop.
        let script = r#"() => {
            __diting_domRaw('set_css_time', '1.5');
            return getComputedStyle(document.querySelector('.b')).opacity;
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        // .b: 2s linear... no easing token → default ease; at t=1.5/2 = .75
        // progress, eased well past 0.75 and below 1.
        let opacity = result.value.unwrap().as_str().unwrap().parse::<f64>().unwrap();
        assert!(opacity > 0.5 && opacity < 1.0, "mid-flight eased opacity: {opacity}");
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg(feature = "screenshot")]
    async fn test_inert_attribute_writes_keep_the_layout_cache() {
        // obscura#983: a docs theme writing tabindex on every heading used
        // to drop the layout cache per write, so geometry reads alternated
        // with those writes each paid a full re-layout.
        let mut rt = setup_runtime(
            r#"<html><head><style>[data-x] { color: red }</style></head>
               <body><h1 id="h">heading</h1></body></html>"#,
        );
        let force = r#"() => document.getElementById('h').getBoundingClientRect().height"#;
        rt.call_function_on_for_cdp(force, None, &[], true, true).await.unwrap();
        let rev0 = rt.with_state(|st| st.layout_rev.get());

        // tabindex is layout-inert and no rule selects on it: no cache drop.
        let write = r#"() => { document.getElementById('h').setAttribute('tabindex', '0'); }"#;
        rt.call_function_on_for_cdp(write, None, &[], true, true).await.unwrap();
        assert_eq!(rt.with_state(|st| st.layout_rev.get()), rev0, "tabindex write keeps the cache");
        rt.call_function_on_for_cdp(force, None, &[], true, true).await.unwrap();
        assert_eq!(rt.with_state(|st| st.layout_rev.get()), rev0, "geometry read after inert write");

        // Same for data-* when NO rule references the name — but the
        // [data-x] rule in this document does, so that write must drop.
        let write = r#"() => { document.getElementById('h').setAttribute('data-y', '1'); }"#;
        rt.call_function_on_for_cdp(write, None, &[], true, true).await.unwrap();
        assert_eq!(rt.with_state(|st| st.layout_rev.get()), rev0, "unreferenced data-* write keeps the cache");
        let write = r#"() => { document.getElementById('h').setAttribute('data-x', '1'); }"#;
        rt.call_function_on_for_cdp(write, None, &[], true, true).await.unwrap();
        assert!(rt.with_state(|st| st.layout_rev.get()) > rev0, "selector-referenced attr invalidates");

        // class always invalidates (selector matching depends on it).
        let rev1 = rt.with_state(|st| st.layout_rev.get());
        let write = r#"() => { document.getElementById('h').setAttribute('class', 'c'); }"#;
        rt.call_function_on_for_cdp(write, None, &[], true, true).await.unwrap();
        assert!(rt.with_state(|st| st.layout_rev.get()) > rev1, "class write invalidates");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_iframe_load_reaches_onload_and_addeventlistener() {
        // Upstream 2e3f5d8: iframe load used to call el.onload() directly,
        // bypassing addEventListener('load') listeners.
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            return await new Promise(resolve => {
                const iframe = document.createElement('iframe');
                const events = [];
                iframe.onload = () => {
                    events.push('property');
                    Promise.resolve().then(() => resolve(events));
                };
                iframe.addEventListener('load', () => events.push('listener'));
                document.body.appendChild(iframe);
                // Unroutable port: fetch rejects, the catch path still fires load.
                iframe.src = 'http://127.0.0.1:1/';
            });
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!(["property", "listener"])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_timer_string_handlers_run_in_global_scope_at_fire_time() {
        // Upstream 452cc85: string setTimeout/setInterval handlers run as
        // global-scope classic scripts at fire time — declarations become
        // globals, and a syntax error surfaces when the timer elapses instead
        // of being swallowed at scheduling. We used to drop string handlers
        // entirely (silent no-op that still returned a timer id).
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            setTimeout('var strVarDecl = 7; window.__strRan = "ran";', 0);
            let scheduleThrew = false;
            try { setTimeout('this is (not javascript', 0); } catch (e) { scheduleThrew = true; }
            window.__intervalCount = 0;
            const iid = setInterval('window.__intervalCount++; clearInterval(window.__iid);', 0);
            window.__iid = iid;
            await new Promise(r => setTimeout(r, 10));
            return [window.__strRan, strVarDecl, scheduleThrew, window.__intervalCount, typeof iid];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!(["ran", 7, false, 1, "number"])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_throwing_timer_is_contained_and_later_timers_still_fire() {
        // Upstream #394: a page timer that throws (Booking.com's
        // "Cannot redefine property: src" inside a timer) took the whole
        // obscura process down with it. Timer callbacks are page code —
        // a throw must be caught at the timer boundary and the loop must
        // keep servicing later timers (same for setInterval ticks).
        let mut rt = setup_runtime("<html><body></body></html>");
        let script = r#"async () => {
            setTimeout(() => { throw new TypeError('boom-timeout'); }, 0);
            let intervalTicks = 0;
            const iid = setInterval(() => {
                intervalTicks++;
                if (intervalTicks === 1) throw new RangeError('boom-interval');
                if (intervalTicks >= 3) clearInterval(iid);
            }, 0);
            setTimeout(() => { window.__survivor = 'ran'; }, 0);
            await new Promise(r => setTimeout(r, 30));
            return [window.__survivor, intervalTicks];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!(["ran", 3]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_intersection_observer_drives_infinite_scroll_to_completion() {
        // The obstacle-course `observer-intersection` pattern (obscura-benchmark,
        // discussed upstream in #671): a sentinel is observed, each intersection
        // appends a batch, and the feed must reach the cap and set the done flag.
        // Our IO is geometry-naive (always intersecting) but re-fires on DOM
        // mutations + a burst schedule, so the pattern completes regardless of
        // font-metric-driven card heights (real-geometry engines flip pass/fail
        // on the default 18px line height).
        let mut rt = setup_runtime("<html><body><div id=feed></div><div id=sentinel></div></body></html>");
        let script = r#"async () => {
            const feed = document.getElementById('feed');
            let loaded = 0;
            // 30, not the fixture's 50: completion is driven by the burst
            // schedule (120/500/1500/3500/7000ms), and a unit test should
            // not wait out the late bursts. The invariant is "completes and
            // terminates", not "reaches 50".
            const BATCH = 10, MAX = 30;
            const io = new IntersectionObserver((entries) => {
                for (const e of entries) {
                    if (e.isIntersecting && loaded < MAX) {
                        for (let i = 0; i < BATCH && loaded < MAX; i++) {
                            const card = document.createElement('div');
                            card.className = 'card';
                            feed.appendChild(card);
                            loaded++;
                        }
                        if (loaded >= MAX) { io.disconnect(); window.__done = 'io:' + loaded; }
                    }
                }
            });
            io.observe(document.getElementById('sentinel'));
            // The chain advances one batch per loop turn (mutation hop or
            // burst timer), so poll until the flag lands rather than a
            // single fixed sleep.
            for (let k = 0; k < 40 && !window.__done; k++) {
                await new Promise(r => setTimeout(r, 25));
            }
            return [loaded, String(window.__done || '')];
        }"#;
        let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([30, "io:30"]));
    }

    #[test]
    fn test_pointer_event_class_matches_chrome_defaults() {
        // A weak `PointerEvent extends Event` used to squat on the name,
        // keeping the real MouseEvent subclass dead behind its typeof-guard.
        // Chrome shape: pointer events are MouseEvents; constructor defaults
        // are pointerId 0, pointerType '', isPrimary false, pressure 0,
        // width/height 1 — only real input carries 'mouse'/'pen'/'touch'.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const bare = new PointerEvent('pointerdown');
            const full = new PointerEvent('pointermove', {
                pointerId: 7, pointerType: 'touch', isPrimary: true,
                pressure: 0.25, clientX: 5, clientY: 6, button: 0,
            });
            return [
                bare instanceof MouseEvent, bare instanceof UIEvent, bare instanceof Event,
                bare.pointerId, bare.pointerType, bare.isPrimary, bare.pressure,
                bare.width, bare.height,
                full.pointerId, full.pointerType, full.isPrimary, full.pressure,
                full.clientX, full.bubbles,
            ];
        "#).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true, true, true,
                0, "", false, 0,
                1, 1,
                7, "touch", true, 0.25,
                5, false,
            ])
        );
    }

    #[test]
    fn test_performance_now_is_offset_monotonic_and_bounded() {
        // Upstream cdab919 + d93ff51: now() reports ms since timeOrigin (not
        // the raw epoch), never goes backwards under bursty calls, and does
        // not run ahead of real elapsed time. timeOrigin carries ±50ms of
        // persona jitter, so allow a slightly negative floor.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const n1 = performance.now();
            const offsetSane = n1 > -100 && n1 < 60000;
            let bad = 0, prev = -Infinity;
            for (let i = 0; i < 10000; i++) {
                const t = performance.now();
                if (t < prev) bad++;
                prev = t;
            }
            const lead = performance.now() - (Date.now() - performance.timeOrigin);
            return [offsetSane, bad, lead <= 1];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([true, 0, true]));
    }

    #[test]
    fn test_performance_timeline_mark_measure_navigation_paint() {
        // User-timing marks/measures are recorded and queryable; navigation
        // and paint entries are derived from performance.timing (upstream
        // v0.2.1 landed the same surface). mark()/measure() argument
        // validation matches Chrome's TypeError/SyntaxError.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            performance.mark('a');
            performance.mark('b');
            const m = performance.measure('ab', 'a', 'b');
            const marks = performance.getEntriesByType('mark').map(e => e.name);
            const measures = performance.getEntriesByType('measure').map(e => e.name);
            const nav = performance.getEntriesByType('navigation')[0];
            const paint = performance.getEntriesByType('paint').map(e => e.name);
            const dcl = nav.domContentLoadedEventEnd, loadEnd = nav.loadEventEnd;
            performance.clearMarks('a');
            const afterClear = performance.getEntriesByType('mark').map(e => e.name);
            const byName = performance.getEntriesByName('b', 'mark').length;
            const sum = performance.getEntries().length;
            let errName = 'no-throw';
            try { performance.measure('x', 'nope'); } catch (e) { errName = e.name; }
            return [marks, measures, paint,
                    nav.entryType, nav.startTime === 0, nav.type,
                    dcl > 0, loadEnd >= dcl, nav.duration >= loadEnd,
                    m.duration >= 0, m.startTime > -100,
                    afterClear, byName, sum >= 5, errName,
                    performance.getEntriesByType('resource').length];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([
            ["a", "b"], ["ab"], ["first-paint", "first-contentful-paint"],
            "navigation", true, "navigate",
            true, true, true,
            true, true,
            ["b"], 1, true, "SyntaxError",
            0
        ]));
    }

    #[test]
    fn test_performance_observer_supported_entry_types_is_honest() {
        // PerformanceObserver.supportedEntryTypes (exposed upstream in the
        // #840 batch) must list exactly the entry types the Performance
        // timeline actually records — advertising LCP/CLS/longtask would push
        // web-vitals wrappers into a wait that never resolves.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const listed = PerformanceObserver.supportedEntryTypes;
            performance.mark('probe');
            const recorded = performance.getEntries().map(e => e.entryType);
            const unrecorded = recorded.filter(t => !listed.includes(t));
            return [listed, unrecorded, listed.includes('largest-contentful-paint')];
        "#).unwrap();
        assert_eq!(result, serde_json::json!([
            ["mark", "measure", "navigation", "paint"],
            [],
            false
        ]));
    }

    #[test]
    fn test_location_navigation_coerces_url_objects() {
        // Upstream fe26417: a URL object passed to location.href/assign/replace
        // must coerce to its href string (our _resolveUrl called .startsWith on
        // it and threw).
        let mut rt = setup_runtime("<html><body></body></html>");
        let hrefs = rt.evaluate(r#"
            const before = location.href;
            location.href = new URL('/from-href', before);
            const href = location.href;
            location.assign(new URL('/from-assign', location.href));
            const assigned = location.href;
            location.replace(new URL('/from-replace', location.href));
            return [href, assigned, location.href];
        "#).unwrap();
        assert_eq!(
            hrefs,
            serde_json::json!([
                "http://example.com/from-href",
                "http://example.com/from-assign",
                "http://example.com/from-replace"
            ])
        );
        assert_eq!(
            rt.take_pending_navigation(),
            Some((
                "http://example.com/from-replace".to_string(),
                "GET".to_string(),
                "".to_string()
            ))
        );
    }

    #[test]
    fn test_push_replace_state_without_url_preserves_current_location() {
        // Upstream 1fc5a24: pushState/replaceState with a missing url keep the
        // current document URL — the new history entry must not reset location
        // back to the original document URL.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate(r#"
            const first = history.pushState({}, '', '/dashboard');
            const afterReplace = (history.replaceState({scroll:1}), location.pathname);
            history.pushState({}, '', '/a');
            const afterPush = (history.pushState({b:1}), location.pathname);
            return [afterReplace, afterPush];
        "#).unwrap();
        assert_eq!(result, serde_json::json!(["/dashboard", "/a"]));
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_layout_rect_returns_real_geometry_through_get_bounding_client_rect() {
        // Task #108: getBoundingClientRect serves diting-layout geometry, not
        // the synthetic hit-test grid. Two block siblings must land at
        // distinct stacked y positions with full-content width — the grid
        // scatter would give them unrelated (x, y) cells of 100x20.
        let mut rt = setup_runtime(
            "<html><body><div id=\"a\">alpha</div><div id=\"b\">bravo</div></body></html>",
        );
        let result = rt.evaluate(r#"
            const a = document.getElementById("a").getBoundingClientRect();
            const b = document.getElementById("b").getBoundingClientRect();
            return [a.x, a.y, a.width, a.height, b.y > a.y + a.height - 1, b.x === a.x,
                    a.width === innerWidth - 16];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!(8), "block x at body's 8px UA content edge");
        assert_eq!(parts[1], serde_json::json!(8), "first block at body content top");
        // Width agrees with the PERSONA viewport minus body's 8px UA margins
        // (set_viewport publishes it to the layout layer; the old hard-coded
        // 1920 broke whenever the persona pool drew a narrower screen).
        assert_eq!(parts[6], serde_json::json!(true), "block spans viewport width minus body margins");
        assert_eq!(parts[4], serde_json::json!(true), "second block stacks below first");
        assert_eq!(parts[5], serde_json::json!(true), "siblings share left edge");
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_layout_rect_cache_invalidates_on_mutation() {
        // A node allocation bumps the tree epoch; the next rect read must
        // reflect the mutated tree (the inserted sibling pushes #b down),
        // not the memoized pre-insert layout.
        let mut rt = setup_runtime(
            "<html><body><div id=\"a\" style=\"height:50px\">a</div><div id=\"b\">b</div></body></html>",
        );
        let before = rt
            .evaluate("document.getElementById('b').getBoundingClientRect().y")
            .unwrap();
        rt.evaluate(
            "const d = document.createElement('div'); d.style.height = '30px'; document.body.insertBefore(d, document.getElementById('b'))",
        )
        .unwrap();
        let after = rt
            .evaluate("document.getElementById('b').getBoundingClientRect().y")
            .unwrap();
        assert_ne!(
            before, after,
            "inserting a 30px block above #b must push it down"
        );
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_offset_geometry_matches_real_layout_rect() {
        // offsetWidth/Height/Top/Left and clientWidth/Height read the same
        // taffy-layout source as getBoundingClientRect (rounded): a script
        // sizing a container via offsetWidth (map-lib init) must not see the
        // 100x20 stub while gBCR reports the real box.
        let mut rt = setup_runtime(
            "<html><body><div id=\"box\" style=\"width:300px;height:120px;margin:10px\">x</div></body></html>",
        );
        let result = rt.evaluate(r#"
            const el = document.getElementById("box");
            const r = el.getBoundingClientRect();
            return [el.offsetWidth === 300, el.offsetHeight === 120,
                    el.offsetWidth === r.width, el.offsetHeight === r.height,
                    el.offsetTop === Math.round(r.y), el.offsetLeft === Math.round(r.x),
                    el.clientWidth === r.width, el.clientHeight === r.height,
                    document.body.clientWidth === innerWidth];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!(true), "offsetWidth serves real layout width");
        assert_eq!(parts[1], serde_json::json!(true), "offsetHeight serves real layout height");
        assert_eq!(parts[2], serde_json::json!(true), "offsetWidth matches gBCR width");
        assert_eq!(parts[3], serde_json::json!(true), "offsetHeight matches gBCR height");
        assert_eq!(parts[4], serde_json::json!(true), "offsetTop matches gBCR y (document coords)");
        assert_eq!(parts[5], serde_json::json!(true), "offsetLeft matches gBCR x");
        assert_eq!(parts[6], serde_json::json!(true), "clientWidth matches gBCR width");
        assert_eq!(parts[7], serde_json::json!(true), "clientHeight matches gBCR height");
        assert_eq!(parts[8], serde_json::json!(true), "body stays a viewport root");
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_offset_geometry_zero_while_hidden_real_after_show() {
        // CSSOM: offset*/client*/scroll* of a display:none element are 0 —
        // the init-in-hidden-container pattern (map libs size while hidden,
        // re-measure on show) must read 0 while hidden and the real box once
        // shown. Hidden via stylesheet to cover the cascade path, not just
        // inline style; the show write must invalidate the memoized layout
        // run (set_attribute drops it), or the re-measure would serve the
        // stale cached "none".
        let mut rt = setup_runtime(
            "<html><head><style>#h { display: none; }</style></head><body><div id=\"h\" style=\"width:300px;height:80px\">h</div><div id=\"s\">s</div></body></html>",
        );
        let result = rt.evaluate(r#"
            const el = document.getElementById("h");
            const hidden = [el.offsetWidth, el.offsetHeight, el.clientWidth,
                            el.clientHeight, el.scrollWidth, el.scrollHeight];
            el.style.display = "block";
            const shown = [el.offsetWidth, el.offsetHeight];
            return [hidden, shown, el.offsetWidth === 300];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(
            parts[0],
            serde_json::json!([0, 0, 0, 0, 0, 0]),
            "all six box metrics read 0 while display:none"
        );
        assert_eq!(
            parts[1],
            serde_json::json!([300, 80]),
            "unhiding serves the real layout box, not a stale cached none"
        );
        assert_eq!(parts[2], serde_json::json!(true));
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_scroll_height_reports_overflow_extent_not_own_box() {
        // blitz#444 family: scrollHeight must be the scrollable overflow
        // extent (union of the laid-out subtree), not the element's own box
        // height — otherwise scrollHeight - clientHeight is always 0 and
        // neither our wheel helper nor a page's lazy loader can detect
        // scrollability. client* keep the box contract; scroll* must move.
        let mut rt = setup_runtime(
            "<html><body><div id=\"box\" style=\"width:200px;height:100px;overflow:auto\"><div style=\"height:500px\"></div></div></body></html>",
        );
        let result = rt.evaluate(r#"
            const el = document.getElementById("box");
            return [el.scrollHeight, el.clientHeight,
                    el.scrollWidth, el.clientWidth,
                    el.scrollHeight - el.clientHeight > 0];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(
            parts[0],
            serde_json::json!(500),
            "scrollHeight serves the descendant overflow extent, not 100"
        );
        assert_eq!(parts[1], serde_json::json!(100), "clientHeight stays the box");
        assert_eq!(
            parts[2],
            serde_json::json!(200),
            "no horizontal overflow -> scrollWidth equals the box width"
        );
        assert_eq!(parts[3], serde_json::json!(200));
        assert_eq!(
            parts[4],
            serde_json::json!(true),
            "scrollability is now detectable via scrollHeight - clientHeight"
        );
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_rotated_text_ink_does_not_fake_horizontal_scroll() {
        // blitz#841 transform half: the root ink-extent fold must map
        // bracketed text through its transform. A nowrap rotate(90deg) line
        // is a ~14px-wide vertical stripe on screen; folding its raw LOCAL
        // x + est width instead reports ~8000px of horizontal ink and fakes
        // a scrollable page. The element box is width:100 at left:0, so its
        // mapped stripe (x within ~[43,57]) sits inside every persona
        // viewport — the ONLY thing that can push scrollWidth past
        // innerWidth is the stale untransformed ink fold. 8000px of text
        // overshoots every screen the persona can draw, so the assertion
        // holds relative to whatever viewport it picked. gBCR already
        // serves the mapped AABB (the element-box half of the walk is
        // transform-correct), so the text fold was the only stale source.
        let html = format!(
            "<html><body><div id=\"r\" style=\"position:absolute;left:0px;top:40px;width:100px;height:14px;font-size:10px;white-space:nowrap;transform:rotate(90deg)\">{}</div></body></html>",
            "字".repeat(800)
        );
        let mut rt = setup_runtime(&html);
        let result = rt.evaluate(r#"
            const r = document.getElementById("r").getBoundingClientRect();
            return [document.documentElement.scrollWidth === window.innerWidth,
                    r.width < 50,
                    document.documentElement.scrollHeight > 7500];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(
            parts[0],
            serde_json::json!(true),
            "rotated text must not leak its unrotated width into scrollWidth"
        );
        assert_eq!(
            parts[1],
            serde_json::json!(true),
            "gBCR keeps serving the mapped (vertical stripe) box"
        );
        assert_eq!(
            parts[2],
            serde_json::json!(true),
            "the rotated length lands vertically in the mapped box"
        );
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_document_scroll_height_tracks_page_extent_with_viewport_floor() {
        // The document scrolling area must track real page extent (tall page
        // -> scrollHeight > innerHeight) and clamp UP to the viewport on
        // short pages — Chrome never lets the root scrolling area be smaller
        // than the window. clientHeight keeps the viewport contract either
        // way, so scrollHeight - clientHeight stays a working overflow probe.
        // 5000px, not 2000px: the stealth persona draws innerHeight from its
        // screen pool (a 4K panel gives 2080), so a "tall" page must be tall
        // against the largest plausible persona viewport, not just 800.
        let mut rt = setup_runtime("<html><body><div style=\"height:5000px\"></div></body></html>");
        let result = rt.evaluate(r#"
            return [document.documentElement.scrollHeight > innerHeight,
                    document.body.scrollHeight > innerHeight,
                    document.documentElement.scrollHeight >= document.body.scrollHeight];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(
            parts[0],
            serde_json::json!(true),
            "documentElement.scrollHeight reports the real page extent"
        );
        assert_eq!(
            parts[1],
            serde_json::json!(true),
            "body.scrollHeight reports the real content extent"
        );
        assert_eq!(
            parts[2],
            serde_json::json!(true),
            "html extent contains the body extent"
        );

        let mut rt = setup_runtime("<html><body><p>short</p></body></html>");
        let result = rt.evaluate(r#"
            return [document.documentElement.scrollHeight === innerHeight,
                    document.body.scrollHeight === innerHeight,
                    document.documentElement.clientHeight === innerHeight];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(
            parts[0],
            serde_json::json!(true),
            "short page clamps the root scrolling area up to the viewport"
        );
        assert_eq!(parts[1], serde_json::json!(true));
        assert_eq!(
            parts[2],
            serde_json::json!(true),
            "clientHeight keeps the viewport contract"
        );
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_body_overflow_hidden_propagates_to_viewport() {
        // blitz#880 (css-overflow-3 §3.3): body's overflow hands up to the
        // viewport when html is visible — the scrolling area collapses to
        // exactly the viewport (window.scrollTo pins at 0), while body's own
        // scrollHeight keeps its content extent (its used overflow flipped
        // back to `visible`). Before the fix the viewport scrolled freely:
        // the extent walk never consulted overflow at all.
        let mut rt = setup_runtime(
            "<html><body style=\"overflow:hidden\"><div style=\"height:5000px\"></div></body></html>",
        );
        let result = rt.evaluate(r#"
            window.scrollTo(0, 1000);
            return [document.documentElement.scrollHeight === innerHeight,
                    window.scrollY === 0,
                    document.body.scrollHeight > innerHeight];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(
            parts[0],
            serde_json::json!(true),
            "propagated hidden collapses scrollingElement.scrollHeight to the viewport"
        );
        assert_eq!(
            parts[1],
            serde_json::json!(true),
            "window scrolling is pinned at 0 for a hidden viewport"
        );
        assert_eq!(
            parts[2],
            serde_json::json!(true),
            "body keeps its content extent (used overflow became visible)"
        );
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_html_overflow_hidden_collapses_scroll_range() {
        // Same propagation rule for html's own overflow: it belongs to the
        // viewport, so hidden there pins the scroll range at the viewport
        // instead of leaving html/body clamping up to a 5000px content
        // extent. A visible body does NOT propagate over it — html's own
        // non-visible value wins either way.
        let mut rt = setup_runtime(
            "<html style=\"overflow:hidden\"><body><div style=\"height:5000px\"></div></body></html>",
        );
        let result = rt.evaluate(r#"
            window.scrollTo(0, 1000);
            return [document.documentElement.scrollHeight === innerHeight,
                    window.scrollY === 0];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!(true));
        assert_eq!(parts[1], serde_json::json!(true));
    }

    #[cfg(feature = "screenshot")]
    #[tokio::test(flavor = "current_thread")]
    async fn test_body_overflow_scroll_still_scrollable() {
        // Regression guard for the propagation fix: an explicit scrollable
        // value on body propagates too (viewport overflow = auto/scroll) and
        // the viewport must keep scrolling — the collapse only fires for
        // hidden/clip.
        let mut rt = setup_runtime(
            "<html><body style=\"overflow:scroll\"><div style=\"height:5000px\"></div></body></html>",
        );
        let result = rt.evaluate(r#"
            window.scrollTo(0, 1000);
            return [document.documentElement.scrollHeight > innerHeight,
                    window.scrollY === 1000];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(parts[0], serde_json::json!(true));
        assert_eq!(
            parts[1],
            serde_json::json!(true),
            "propagated scroll keeps the viewport scrollable"
        );
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn test_bare_text_body_scroll_height_serves_text_extent() {
        // blitz#444's text-only residual: a body holding nothing but text
        // nodes has no element box past html/body — and those stretch to the
        // viewport — so the element walk sees no overflow and the page could
        // never scroll past its first screenful. The Text paint items carry
        // the true extent (wrap-model estimate, errs high); the root fold
        // lifts scrollHeight past innerHeight. 1000 chars at width 20 ≈ 9600
        // estimated px, safely above the largest persona viewport (2080).
        let html = format!(
            "<html><body style=\"width:20px;font-size:16px;line-height:20px\">{}</body></html>",
            "x".repeat(1000)
        );
        let mut rt = setup_runtime(&html);
        let result = rt.evaluate(r#"
            return [document.documentElement.scrollHeight > innerHeight,
                    document.body.scrollHeight > innerHeight,
                    document.documentElement.scrollHeight >= document.body.scrollHeight];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(
            parts[0],
            serde_json::json!(true),
            "bare text taller than the viewport must make the root scrollable"
        );
        assert_eq!(
            parts[1],
            serde_json::json!(true),
            "body scrollHeight must serve the text extent, not its stretched box"
        );
        assert_eq!(
            parts[2],
            serde_json::json!(true),
            "html extent contains the body extent"
        );
    }

    /// blitz#841 shape: a `position: fixed` element never contributes to the
    /// root scrollable overflow — not even when a transform pushes its paint
    /// far past the viewport edge. Upstream's viewport became scrollable
    /// because the fixed element's (transformed) box entered the overflow
    /// walk; Chrome keeps scrollWidth pinned to the viewport for fixed boxes.
    #[cfg(feature = "screenshot")]
    #[test]
    fn test_fixed_transformed_element_does_not_expand_scroll_extent() {
        let mut rt = setup_runtime(
            "<html><body><p>body text</p>\
             <div id=\"fx\" style=\"position:fixed;left:0;top:0;width:50px;height:50px;\
             transform:translateX(2500px)\"></div></body></html>",
        );
        let result = rt.evaluate(r#"
            return [document.documentElement.scrollWidth <= innerWidth + 50,
                    document.body.scrollWidth <= innerWidth + 50];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(
            parts[0],
            serde_json::json!(true),
            "a fixed box translated past the viewport must not widen the root scroll area"
        );
        assert_eq!(
            parts[1],
            serde_json::json!(true),
            "nor the body's scroll area — fixed is out of every scroller's flow"
        );
    }

    /// blitz#840 shape (narrowed to what applies without transitions): an
    /// out-of-flow element inside an INLINE-LEVEL container must still carry
    /// its transform — in gBCR here, and the paint side reads the same
    /// resolved matrix. Upstream lost the transform entirely when the subtree
    /// was pruned at an inline ancestor; the probe pins the abspos-in-span
    /// arrangement plus a trailing sibling, their exact three conditions.
    #[cfg(feature = "screenshot")]
    #[test]
    fn test_abspos_child_in_inline_container_keeps_transform() {
        let mut rt = setup_runtime(
            "<html><body style=\"margin:0\">\
             <span class=\"box\" style=\"position:relative;width:42px;height:22px\">\
             <span id=\"dot\" style=\"position:absolute;top:0;left:2px;width:18px;height:18px;\
             transform:translateX(20px)\"></span></span>\
             <div></div></body></html>",
        );
        let result = rt.evaluate(r#"
            const dot = document.getElementById("dot");
            const r = dot.getBoundingClientRect();
            return [Math.round(r.x), Math.round(r.width)];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        assert_eq!(
            parts[0],
            serde_json::json!(22),
            "abspos dot at left:2 + translateX(20) reports x=22 through the inline container"
        );
        assert_eq!(parts[1], serde_json::json!(18), "transform does not resize the box");
    }

    /// blitz#839's complaint through the whole stack: focus() must reach the
    /// Rust tree (selector matching), so :focus/:focus-within/:focus-visible
    /// rules actually re-style, and blur() clears them again. The old focus()
    /// flipped a JS global only — every focus-dependent rule stayed inert.
    #[cfg(feature = "screenshot")]
    #[test]
    fn test_focus_pseudo_styles_react_to_focus_and_blur() {
        let mut rt = setup_runtime(
            "<html><head><style>\
             #q:focus { background-color: rgb(10, 20, 30); }\
             form:focus-within { margin-top: 7px; }\
             #q:focus-visible { color: rgb(1, 2, 3); }\
             </style></head><body><form><input id=\"q\"></form></body></html>",
        );
        let result = rt.evaluate(r#"
            const q = document.getElementById("q");
            const form = document.querySelector("form");
            const before = getComputedStyle(q).backgroundColor;
            const formBefore = getComputedStyle(form).marginTop;
            q.focus();
            const focusedBg = getComputedStyle(q).backgroundColor;
            const visibleColor = getComputedStyle(q).color;
            const formWithin = getComputedStyle(form).marginTop;
            q.blur();
            return [before, focusedBg, visibleColor, formBefore, formWithin,
                    getComputedStyle(q).backgroundColor];
        "#).unwrap();
        let parts = result.as_array().expect("array result");
        let before = parts[0].as_str().expect("before is a string");
        let focused = parts[1].as_str().expect("focused is a string");
        assert_ne!(
            before, focused,
            ":focus rule must re-style the focused input"
        );
        assert_eq!(
            focused, "rgb(10, 20, 30)",
            "the :focus background lands in computed style"
        );
        assert_eq!(
            parts[2].as_str().expect("color string"),
            "rgb(1, 2, 3)",
            ":focus-visible matches — a text input shows the ring on programmatic focus"
        );
        assert_ne!(
            parts[3].as_str().expect("form before"),
            parts[4].as_str().expect("form within"),
            ":focus-within re-styles the containing form while the input holds focus"
        );
        assert_eq!(
            parts[5].as_str().expect("after blur"),
            before,
            "blur() returns the input to its unfocused style"
        );
    }

    /// Upstream obscura #704: postMessage's targetOrigin argument must gate
    /// delivery — '*' or a matching origin delivers, a mismatched origin
    /// drops silently (browsers never throw), '/' requires same-origin with
    /// the calling document. The pre-fix wrappers delivered unconditionally,
    /// leaking caller-restricted payloads to whatever frame was targeted.
    #[tokio::test(flavor = "current_thread")]
    async fn test_post_message_target_origin_gates_delivery() {
        let mut rt = setup_runtime(
            "<html><body><iframe src=\"https://frame.example/widget\"></iframe></body></html>",
        );

        // One async eval drives all three gates: mismatched targetOrigin
        // drops silently (iframe origin is frame.example; the caller
        // restricted delivery to trusted.example), matching origin delivers,
        // '*' wildcard delivers.
        let result = rt.evaluate_for_cdp(
            "(async function(){ \
                window.__leak = []; \
                window.addEventListener('message', function(e){ window.__leak.push(e.data) }); \
                const w = document.querySelector('iframe').contentWindow; \
                w.postMessage('secret', 'https://trusted.example'); \
                await new Promise(r => setTimeout(r, 20)); \
                const afterMismatch = window.__leak.slice(); \
                w.postMessage('hello', 'https://frame.example'); \
                await new Promise(r => setTimeout(r, 20)); \
                const afterMatch = window.__leak.slice(); \
                w.postMessage('wild', '*'); \
                await new Promise(r => setTimeout(r, 20)); \
                return [afterMismatch, afterMatch, window.__leak.slice()]; \
            })()",
            true,
            true,
        ).await.unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([
                [],
                ["hello"],
                ["hello", "wild"],
            ])
        );
    }

    /// Same-origin '/' targetOrigin delivers on a same-origin frame and the
    /// self-targeted window.postMessage(path) honors the same gate.
    #[tokio::test(flavor = "current_thread")]
    async fn test_post_message_same_origin_slash_and_self_gate() {
        let mut rt = setup_runtime(
            "<html><body><iframe src=\"http://example.com/frame\"></iframe></body></html>",
        );
        rt.set_url("http://example.com/test");

        // '/': iframe origin equals page origin → deliver. Self-targeted
        // with mismatched explicit origin → drop. Self-targeted matching
        // origin → deliver.
        rt.evaluate(
            "(function(){ window.__got = []; window.addEventListener('message', function(e){ window.__got.push(e.data) }); document.querySelector('iframe').contentWindow.postMessage('same-origin', '/'); postMessage('self-mismatch', 'https://other.example'); postMessage('self-ok', 'http://example.com'); })()",
        )
        .unwrap();

        let got = rt.evaluate_for_cdp(
            "new Promise(r => setTimeout(() => r(window.__got), 50))",
            true,
            true,
        ).await.unwrap();
        assert_eq!(
            got.value.unwrap(),
            serde_json::json!(["same-origin", "self-ok"])
        );
    }

    /// Upstream obscura #658: relative URL resolution (anchor href, form
    /// action, fetch/XHR input) must resolve against the document BASE url —
    /// the document URL with <base href> folded in — while document.URL
    /// itself stays the plain document URL.
    #[tokio::test(flavor = "current_thread")]
    async fn test_relative_urls_resolve_against_base_href() {
        let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let mut rt = setup_runtime(
            "<html><head><base href=\"/assets/\"></head><body>\
             <a id=\"a\" href=\"page.html\">x</a><form id=\"f\" action=\"submit\"></form></body></html>",
        );
        rt.set_url("https://example.com/app/index");

        // Anchor href resolves against /assets/.
        assert_eq!(
            rt.evaluate("document.getElementById('a').href").unwrap(),
            serde_json::json!("https://example.com/assets/page.html")
        );
        // Form action likewise.
        assert_eq!(
            rt.evaluate("document.getElementById('f').action").unwrap(),
            serde_json::json!("https://example.com/assets/submit")
        );
        // fetch() input resolution uses the base as well: a real local server
        // records the path the runtime actually requests.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (path_tx, path_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let body = b"{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
            path_tx.send(path).unwrap();
        });
        rt.set_url(&format!("https://example.com/app/index"));
        // Point <base href> at the local server so the resolved fetch lands
        // there (the page URL itself is non-fetchable https).
        rt.evaluate(&format!(
            "document.querySelector('base').setAttribute('href', 'http://127.0.0.1:{}/assets/')", port
        ))
        .unwrap();
        let _ = rt.evaluate_for_cdp(
            "(async function(){ try { await fetch('data.json'); } catch(e) {} })()",
            true,
            true,
        )
        .await;
        let seen = path_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_default();
        assert_eq!(seen, "/assets/data.json");
        // Identity surfaces stay on the plain document URL.
        assert_eq!(
            rt.evaluate("document.URL").unwrap(),
            serde_json::json!("https://example.com/app/index")
        );
    }

    // localStorage persistence (obscura#629 class): writes flush (debounced)
    // to one JSON file per origin, and a fresh realm — the process-restart
    // equivalent — reads them back. sessionStorage stays memory-only: its
    // per-tab lifetime is the spec'd behavior, and nothing it holds may
    // reach the disk.
    static STORAGE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[allow(clippy::await_holding_lock)] // the env guard must span the awaits
    #[tokio::test(flavor = "current_thread")]
    async fn local_storage_persists_across_realms_and_session_storage_does_not() {
        // Ephemeral flips storage_file to None, killing every flush below —
        // hold its lock so the ephemeral tests can't race this one.
        let _ephemeral = crate::config::EPHEMERAL_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = STORAGE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("diting-ls-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AGINXBROWSER_STORAGE_DIR", &dir);
        crate::diting_js::ops::reset_local_storage_for_tests();

        // Realm 1: the method path and the property path both flush, and
        // sessionStorage stays out of the picture.
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate_for_cdp(
                "(async () => { localStorage.setItem('token', 'abc'); localStorage.theme = 'dark'; \
                 sessionStorage.setItem('s', '1'); await new Promise(r => setTimeout(r, 250)); return 'ok'; })()",
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("ok"));

        let ls_dir = dir.join("localStorage");
        let files: Vec<_> = std::fs::read_dir(&ls_dir)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1, "one file per origin");
        let on_disk = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(on_disk.contains("\"token\":\"abc\""), "disk copy: {on_disk}");
        assert!(!on_disk.contains("\"s\""), "sessionStorage never reaches disk: {on_disk}");

        // Realm 2 = restart equivalent: drop the in-memory mirror and
        // rebuild; the fresh store reads the same values back from disk.
        crate::diting_js::ops::reset_local_storage_for_tests();
        drop(rt);
        let mut rt2 = setup_runtime("<html><body></body></html>");
        assert_eq!(
            rt2.evaluate("localStorage.getItem('token')").unwrap(),
            serde_json::json!("abc")
        );
        assert_eq!(
            rt2.evaluate("localStorage.theme").unwrap(),
            serde_json::json!("dark")
        );
        assert_eq!(
            rt2.evaluate("sessionStorage.getItem('s')").unwrap(),
            serde_json::json!(null)
        );

        // The deletion flushes too, not just additions.
        let result = rt2
            .evaluate_for_cdp(
                "(async () => { localStorage.removeItem('token'); await new Promise(r => setTimeout(r, 250)); return 'ok'; })()",
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("ok"));
        let on_disk = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(!on_disk.contains("token"), "removal reached disk: {on_disk}");

        std::env::remove_var("AGINXBROWSER_STORAGE_DIR");
        crate::diting_js::ops::reset_local_storage_for_tests();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// obscura#841 mechanism, pinned. deno_core's isolate-level
    /// `promise_reject_callback` (runtime/bindings.rs) unconditionally looks
    /// up the CURRENT context's CONTEXT_STATE_SLOT embedder data and bumps the
    /// `Rc` found there — no null guard (`state_from_scope` ->
    /// `clone_rc_raw` -> `Rc::increment_strong_count`). Only
    /// `JsRuntime::new_inner` initializes that slot, and only for the main
    /// context. A context created via raw `v8::Context::from_snapshot` —
    /// obscura's `create_realm_context` path for frame realms — never gets
    /// the slot set, so the first promise rejection fired while such a
    /// context is entered refcounts NULL: fault at 0xfffffffffffffff0, the
    /// `ldr x9, [x8, #-0x10]!` frame from #841. We run one main context per
    /// JsRuntime, so the product never creates such a context; this test
    /// documents the hazard for anyone adding multi-realm support. Manual
    /// only: it segfaults the harness by design.
    #[test]
    #[ignore = "segfaults by design (obscura#841 probe); run explicitly with --ignored"]
    fn raw_snapshot_context_promise_reject_segfaults() {
        let mut rt = JsRuntime::new();
        let context = {
            deno_core::scope!(scope, rt.runtime);
            let ctx = deno_core::v8::Context::from_snapshot(scope, 1, Default::default())
                .or_else(|| {
                    deno_core::v8::Context::from_snapshot(scope, 0, Default::default())
                })
                .expect("snapshot context to restore");
            deno_core::v8::Global::new(scope, ctx)
        };
        let isolate = rt.runtime.v8_isolate();
        deno_core::v8::scope_with_context!(cscope, isolate, &context);
        let src = deno_core::v8::String::new(cscope, "Promise.reject(1)").unwrap();
        let script = deno_core::v8::Script::compile(cscope, src, None).unwrap();
        let _ = script.run(cscope);
        panic!("unreachable: the rejection should have crashed the process");
    }

    /// obscura#828 lineage: innerText is rendered text, not textContent —
    /// script/style/template/noscript bodies, display:none subtrees and
    /// visibility:hidden text contribute nothing; block boxes break lines;
    /// collapsible whitespace collapses. The old getter was a textContent
    /// passthrough, interleaving script source with visible text (~100x
    /// bloat on script-heavy pages).
    #[test]
    fn inner_text_excludes_script_and_style_bodies() {
        let mut rt = setup_runtime(
            "<html><body><p>visible</p><script>var secret = 'leakme';</script>\
             <style>.x { color: red }</style><noscript>nojs</noscript></body></html>",
        );
        let t = rt.evaluate("document.body.innerText").unwrap();
        assert_eq!(t, serde_json::json!("visible"), "got: {t}");
    }

    #[test]
    fn inner_text_skips_display_none_subtree_and_hidden_text() {
        let mut rt = setup_runtime(
            "<html><body>\
             <div style=\"display:none\">gone</div>\
             <div style=\"visibility:hidden\">veiled</div>\
             <div hidden>also-gone</div>\
             kept</body></html>",
        );
        let t = rt.evaluate("document.body.innerText").unwrap();
        assert_eq!(t, serde_json::json!("kept"), "got: {t}");
    }

    #[test]
    fn inner_text_breaks_lines_on_blocks_and_collapses_whitespace() {
        let mut rt = setup_runtime(
            "<html><body><p>hello   world</p><p>second</p>\
             <div><span>a</span><span>b</span> c</div>tail</body></html>",
        );
        let t = rt.evaluate("document.body.innerText").unwrap();
        // Two blocks -> single newline between; inline flow stays inline with
        // collapsed internal whitespace; text after a closed block starts a
        // new line.
        assert_eq!(t, serde_json::json!("hello world\nsecond\nab c\ntail"), "got: {t}");
    }

    #[test]
    fn inner_text_preserves_pre_content_verbatim() {
        let mut rt = setup_runtime(
            "<html><body><pre>  keep\n   me  </pre><p>after</p></body></html>",
        );
        let t = rt.evaluate("document.body.innerText").unwrap();
        assert_eq!(t, serde_json::json!("  keep\n   me  \nafter"), "got: {t}");
    }

    #[test]
    fn inner_text_br_forces_line_break() {
        let mut rt = setup_runtime("<html><body><div>one<br>two</div></body></html>");
        let t = rt.evaluate("document.body.innerText").unwrap();
        assert_eq!(t, serde_json::json!("one\ntwo"), "got: {t}");
    }

    // UA `q` marks (Chrome q::before/::after open-quote/close-quote): diting
    // has no generated content, so the layout synthesizes the quote leaves
    // around flattened q content. They must take layout space in front of
    // the q's own children and give the (boxless-union) q a wider rect.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_q_element_quote_marks_take_layout_space() {
        let mut rt = setup_runtime(
            r#"<html><body style="margin:0">
              <p style="margin:0"><span id="plain">hi</span></p>
              <p style="margin:0"><q><span id="quoted">hi</span></q></p>
              <p style="margin:0"><q id="empty"></q><span id="after">x</span></p>
            </body></html>"#,
        );
        let v = rt
            .evaluate(
                r#"JSON.stringify((() => {
                    const g = (id) => document.getElementById(id).getBoundingClientRect();
                    const plain = g('plain');
                    const quoted = g('quoted');
                    const q = document.querySelector('q').getBoundingClientRect();
                    const emptyQ = g('empty');
                    const after = g('after');
                    return {
                        shift: quoted.left - plain.left,
                        qWider: q.width > quoted.width,
                        emptyW: emptyQ.width,
                        afterShift: after.left - emptyQ.left,
                    };
                })())"#,
            )
            .unwrap();
        let d: serde_json::Value = serde_json::from_str(v.as_str().unwrap()).unwrap();
        let shift = d["shift"].as_f64().unwrap();
        assert!(
            shift > 1.0,
            "open quote must push the q's content right (shift={shift}, diag={d})"
        );
        assert!(d["qWider"].as_bool().unwrap(), "q union rect must include the marks (diag={d})");
        let empty_w = d["emptyW"].as_f64().unwrap();
        assert!(empty_w > 0.0, "empty <q></q> still renders both marks (w={empty_w})");
        let after_shift = d["afterShift"].as_f64().unwrap();
        assert!(
            after_shift > 1.0,
            "close quote must occupy space before the next sibling (shift={after_shift})"
        );
    }

    // vertical-align authored lengths/percentages are baseline raises on the
    // line (CSS2 §10.8.1): positive lifts, negative drops, % of the element's
    // own line-height. Mixed shifts on one line keep the RELATIVE offsets —
    // the whole line grows so the extreme baseline still fits.
    #[test]
    #[cfg(feature = "screenshot")]
    fn test_vertical_align_length_percent_shift() {
        let mut rt = setup_runtime(
            r#"<html><body style="margin:0">
              <p style="margin:0;line-height:40px">
                <span id="plain">ab</span>
                <span id="up" style="vertical-align:10px">ab</span>
                <span id="pct" style="vertical-align:50%">ab</span>
                <span id="down" style="vertical-align:-8px">ab</span>
              </p>
            </body></html>"#,
        );
        let v = rt
            .evaluate(
                r#"JSON.stringify((() => {
                    const g = (id) => document.getElementById(id).getBoundingClientRect();
                    const cs = (id) => getComputedStyle(document.getElementById(id)).verticalAlign;
                    const plain = g('plain').top;
                    return {
                        up: plain - g('up').top,
                        pct: plain - g('pct').top,
                        down: g('down').top - plain,
                        csUp: cs('up'),
                        csPct: cs('pct'),
                        csDown: cs('down'),
                    };
                })())"#,
            )
            .unwrap();
        let d: serde_json::Value = serde_json::from_str(v.as_str().unwrap()).unwrap();
        let close = |key: &str, want: f64| -> bool {
            d[key].as_f64().map(|got| (got - want).abs() < 2.5).unwrap_or(false)
        };
        assert!(close("up", 10.0), "vertical-align:10px must lift 10px (diag={d})");
        assert!(close("pct", 20.0), "50% of line-height:40px must lift 20px (diag={d})");
        assert!(close("down", 8.0), "vertical-align:-8px must drop 8px (diag={d})");
        assert_eq!(d["csUp"].as_str().unwrap(), "10px");
        assert_eq!(d["csPct"].as_str().unwrap(), "50%");
        assert_eq!(d["csDown"].as_str().unwrap(), "-8px");
    }

    /// Mono batch: a UA-monospace element (<code>) shapes its ASCII on the
    /// bundled Noto Sans Mono — fixed 0.6em advance, Chrome's parity — while
    /// a plain span stays proportional, an explicit `font-family: monospace`
    /// on any element routes the same way, and CJK inside the run keeps the
    /// CJK face's full-em advance (per-char fallback).
    #[test]
    fn test_monospace_face_routes_ascii_runs() {
        let mut rt = setup_runtime(
            r#"<html><body style="margin:0;font-size:20px">
              <code id="c">0000000000</code>
              <span id="p">0000000000</span>
              <span id="m" style="font-family:monospace">0000000000</span>
              <code id="cjk">汉字</code>
            </body></html>"#,
        );
        let v = rt
            .evaluate(
                r#"JSON.stringify((() => {
                    const w = (id) => document.getElementById(id).getBoundingClientRect().width;
                    return { code: w('c'), plain: w('p'), explicit: w('m'), cjk: w('cjk') };
                })())"#,
            )
            .unwrap();
        let d: serde_json::Value = serde_json::from_str(v.as_str().unwrap()).unwrap();
        let w = |key: &str| d[key].as_f64().unwrap();
        assert!((w("code") - 120.0).abs() < 1.5, "10 chars × 0.6em @20px = 120 (diag={d})");
        assert!((w("explicit") - 120.0).abs() < 1.5, "font-family:monospace routes the same (diag={d})");
        assert!((w("plain") - w("code")).abs() > 1.0, "proportional digits differ from mono (diag={d})");
        assert!((w("cjk") - 40.0).abs() < 1.5, "CJK in a mono run keeps full-em (diag={d})");
    }

    /// Anon-cell batch: children of a table-row box that are not table-cells
    /// wrap into ONE anonymous cell per consecutive run (CSS2.2 §17.2.1),
    /// claiming a column slot like any td. Static HTML never reaches here —
    /// the parser foster-parents bare text out of the table (verified:
    /// `<tr>AAA<td>` puts "AAA" on body before the table, Chrome's exact
    /// behavior) — so the two real entries are CSS-authored rows
    /// (display:table-row with non-cell content) and JS tree mutations.
    #[test]
    fn test_table_anonymous_cell_synthesis() {
        let mut rt = setup_runtime(
            r#"<html><body style="margin:0;font-size:16px">
              <div style="display:table">
                <div style="display:table-row">AAA<div id="b1" style="display:table-cell">B</div></div>
                <div style="display:table-row"><div id="c2" style="display:table-cell">C</div><div id="d2" style="display:table-cell">D</div></div>
              </div>
              <div id="t2" style="display:table">
                <div style="display:table-row">AA<span>BB</span>CC<div id="x1" style="display:table-cell">x</div></div>
              </div>
              <table><tr id="r3"><td id="y1">Y</td></tr></table>
              <table id="t4"><tr><td id="m1">AA<span>BB</span>CC</td><td id="m2">x</td></tr></table>
            </body></html>"#,
        );
        let v = rt
            .evaluate(
                r#"JSON.stringify((() => {
                    const r = (id) => { const b = document.getElementById(id).getBoundingClientRect();
                                        return { x: b.x, w: b.width }; };
                    const y1Before = r('y1').x;
                    document.getElementById('r3').insertBefore(document.createTextNode('ZZ'), document.getElementById('y1'));
                    return { b1: r('b1'), c2: r('c2'), d2: r('d2'), x1: r('x1'),
                             t2h: document.getElementById('t2').getBoundingClientRect().height,
                             m1: r('m1'), m2x: r('m2').x, t4h: document.getElementById('t4').getBoundingClientRect().height,
                             y1Before, y1After: r('y1').x };
                })())"#,
            )
            .unwrap();
        let d: serde_json::Value = serde_json::from_str(v.as_str().unwrap()).unwrap();
        let (b1x, c2x, d2x) = (d["b1"]["x"].as_f64().unwrap(), d["c2"]["x"].as_f64().unwrap(), d["d2"]["x"].as_f64().unwrap());
        assert!(b1x > c2x + 5.0, "the anon cell claims column 0, pushing B right (diag={d})");
        assert!((b1x - d2x).abs() < 1.0, "B and D share column 1 across rows (diag={d})");
        assert!(d["x1"]["x"].as_f64().unwrap() > 5.0, "the inline group claims its column (diag={d})");
        let t2h = d["t2h"].as_f64().unwrap();
        let t4h = d["t4h"].as_f64().unwrap();
        let (x1x, m2x) = (d["x1"]["x"].as_f64().unwrap(), d["m2x"].as_f64().unwrap());
        assert!(
            (t2h - t4h).abs() <= 1.0 && (x1x - m2x).abs() < 1.0,
            "anon cell matches a real cell with identical content (t2h={t2h} t4h={t4h} x1x={x1x} m2x={m2x}, diag={d})"
        );
        assert!(
            t4h < 30.0,
            "the mixed-run cell fits on ONE line at its max-content pin (no fractional-rounding wrap) (t4h={t4h}, diag={d})"
        );
        let (yb, ya) = (d["y1Before"].as_f64().unwrap(), d["y1After"].as_f64().unwrap());
        assert!(ya > yb + 5.0, "JS-inserted text synthesizes a cell on re-layout ({yb} -> {ya})");
    }

/// white-space / text-overflow ride the computed-style table (blitz#888):
/// the author's declaration reports back verbatim, white-space inherits
/// through the cascade while text-overflow does not, and the geometry
/// keeps the full text (scrollWidth outgrows the clip box — Chrome parity,
/// the ellipsis marker is a paint-time effect).
#[cfg(feature = "screenshot")]
#[test]
fn computed_style_and_geometry_for_nowrap_ellipsis() {
    let mut rt = setup_runtime(
        r#"<div id="clip" style="width:80px;overflow:hidden;text-overflow:ellipsis"><span id="nw" style="white-space:nowrap">alpha beta gamma delta epsilon zeta eta theta</span></div><div id="plain">hi</div>"#,
    );
    let parts = rt
        .evaluate(
            r#"
            const clip = document.getElementById("clip"),
                  nw = document.getElementById("nw"),
                  plain = document.getElementById("plain");
            const csC = getComputedStyle(clip), csN = getComputedStyle(nw), csP = getComputedStyle(plain);
            return [
                csC.getPropertyValue("white-space"), csC.getPropertyValue("text-overflow"),
                csN.whiteSpace, csN.textOverflow,
                csP.whiteSpace, csP.textOverflow,
                clip.scrollWidth, clip.clientWidth,
                nw.getBoundingClientRect().height,
            ];
        "#,
        )
        .unwrap();
    let parts = parts.as_array().expect("array result");
    assert_eq!(parts[0], serde_json::json!("normal"), "white-space unset on the outer box");
    assert_eq!(parts[1], serde_json::json!("ellipsis"));
    assert_eq!(
        parts[2],
        serde_json::json!("nowrap"),
        "white-space comes from the declaration"
    );
    assert_eq!(
        parts[3],
        serde_json::json!("clip"),
        "text-overflow does not inherit (CSS UI §5.2)"
    );
    assert_eq!(parts[4], serde_json::json!("normal"));
    assert_eq!(parts[5], serde_json::json!("clip"), "initial text-overflow is clip");
    let (sw, cw) = (parts[6].as_f64().unwrap(), parts[7].as_f64().unwrap());
    assert!(
        sw > cw + 20.0,
        "scrollWidth keeps the full nowrap text ({sw} > {cw})"
    );
    let h = parts[8].as_f64().unwrap();
    assert!(h > 10.0 && h < 30.0, "nowrap keeps the run on one line ({h})");
}

#[tokio::test(flavor = "current_thread")]
async fn test_css_animation_start_end_events() {
    // blitz#863 family 1: diting samples CSS animations at a fixed clock (no
    // compositor loop), so `await animationend` scripts used to hang forever.
    // The collapsed lifecycle fires animationstart synchronously when the
    // animation becomes observable and animationend after the declared
    // active duration, with elapsedTime = that duration.
    let mut rt = setup_runtime(
        "<html><head><style>@keyframes fade{from{opacity:1}to{opacity:0}} .box{animation:fade 100ms linear;}</style></head><body></body></html>",
    );
    let script = r#"async () => {
        const seq = [];
        // In an isolated test process the first layout run (font book init)
        // blocks the loop for ~0.9s; it lands inside the check drain here, so
        // the end timer is armed only after that block and a wall-clock
        // await armed earlier expires first — await the event, not a window.
        const ended = new Promise(r =>
            document.body.addEventListener('animationend', r));
        document.body.addEventListener('animationstart', (e) =>
            seq.push(['start', e.animationName, e.elapsedTime, e instanceof AnimationEvent, e.target.id]));
        document.body.addEventListener('animationend', (e) =>
            seq.push(['end', e.animationName, e.elapsedTime]));
        const el = document.createElement('div');
        el.id = 'box';
        el.setAttribute('class', 'box');
        document.body.appendChild(el);
        await Promise.race([ended, new Promise(r => setTimeout(r, 3000))]);
        return seq;
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(
        result.value.unwrap(),
        serde_json::json!([["start", "fade", 0, true, "box"], ["end", "fade", 0.1]])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_css_animation_cancel_on_class_removal() {
    // Removing the animation between start and end must fire animationcancel
    // and suppress animationend (the pending end timer is cleared).
    let mut rt = setup_runtime(
        "<html><head><style>@keyframes fade{from{opacity:1}to{opacity:0}} .box{animation:fade 100ms;}</style></head><body></body></html>",
    );
    let script = r#"async () => {
        const seq = [];
        const el = document.createElement('div');
        el.addEventListener('animationstart', () => seq.push('start'));
        el.addEventListener('animationcancel', (e) => seq.push('cancel:' + e.animationName));
        el.addEventListener('animationend', () => seq.push('end'));
        document.body.appendChild(el);
        el.setAttribute('class', 'box');
        await new Promise(r => setTimeout(r, 10));
        el.removeAttribute('class');
        await new Promise(r => setTimeout(r, 30));
        return seq;
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(result.value.unwrap(), serde_json::json!(["start", "cancel:fade"]));
}

#[tokio::test(flavor = "current_thread")]
async fn test_css_transition_computed_values() {
    // The computed table spells transition-* in seconds with `all` as the
    // property default and `ease` as the timing default (Chrome spellings).
    let mut rt = setup_runtime(
        "<html><head><style>#b{transition:opacity 1s ease 250ms}\n#c{transition-property:color;transition-duration:2s}</style></head><body><div id=\"b\"></div><div id=\"c\"></div><div id=\"p\"></div></body></html>",
    );
    let script = r#"() => {
        const cs = (id) => {
            const s = getComputedStyle(document.getElementById(id));
            return [s.transitionProperty, s.transitionDuration, s.transitionDelay, s.transitionTimingFunction];
        };
        return [cs('b'), cs('c'), cs('p')];
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(
        result.value.unwrap(),
        serde_json::json!([
            ["opacity", "1s", "0.25s", "ease"],
            ["color", "2s", "0s", "ease"],
            ["all", "0s", "0s", "ease"],
        ])
    );
}

#[cfg(feature = "screenshot")]
#[tokio::test(flavor = "current_thread")]
async fn test_css_transition_events_and_registry() {
    // The JS face diffs watched properties across style writes: the first
    // resolution only observes (no transition on a first style resolution),
    // the second registers an entry (start=0 on the video timeline) and
    // fires run → start → end with the declared curve length.
    let mut rt = setup_runtime("<html><body><div id=\"d\">x</div></body></html>");
    let script = r#"async () => {
        const el = document.getElementById('d');
        const seq = [];
        el.addEventListener('transitionrun', (e) => seq.push(['run', e.propertyName, e.elapsedTime, e instanceof TransitionEvent]));
        el.addEventListener('transitionstart', (e) => seq.push(['start', e.propertyName]));
        el.addEventListener('transitionend', (e) => seq.push(['end', e.propertyName, e.elapsedTime]));
        el.style.transition = 'opacity 80ms linear';
        el.style.opacity = '1';
        await new Promise(r => setTimeout(r, 5));
        el.style.opacity = '0';
        await new Promise(r => setTimeout(r, 200));
        return seq;
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(
        result.value.unwrap(),
        serde_json::json!([
            ["run", "opacity", 0, true],
            ["start", "opacity"],
            ["end", "opacity", 0.08],
        ])
    );
    let list = rt.with_state(|st| st.css_transitions.borrow().clone());
    assert_eq!(list.len(), 1, "one registered transition");
    let tr = &list[0];
    assert_eq!(tr.property, "opacity");
    assert_eq!(tr.from, crate::diting_css::TransitionValue::Opacity(1.0));
    assert_eq!(tr.to, crate::diting_css::TransitionValue::Opacity(0.0));
    assert_eq!(tr.start, 0.0, "video-timeline outset");
    assert!((tr.duration - 0.08).abs() < 1e-5);
    assert!(matches!(tr.easing, crate::diting_css::Easing::Linear));
}

#[cfg(feature = "screenshot")]
#[tokio::test(flavor = "current_thread")]
async fn test_css_transition_cancel_on_retrigger_and_detach() {
    // Chrome fires transitioncancel two ways: a re-trigger on the same
    // property preempts the in-flight entry (elapsedTime = active time so
    // far), and removing the element from the document cancels every
    // pending timer. Both legs here, transform included in the watched set.
    let mut rt = setup_runtime("<html><body><div id=\"d\">x</div><div id=\"e\">y</div></body></html>");
    let script = r#"async () => {
        const d = document.getElementById('d');
        const e = document.getElementById('e');
        const seq = [];
        d.addEventListener('transitioncancel', (ev) => seq.push(['cancel', ev.propertyName, Math.round(ev.elapsedTime * 1000)]));
        d.addEventListener('transitionend', () => seq.push(['end']));
        // Check 1 observes (no transition on a first style resolution),
        // check 2 arms the run, check 3 re-triggers mid-flight — that is
        // the write that must preempt check 2's entry with a cancel.
        d.style.transition = 'opacity 500ms linear';
        d.style.opacity = '0.5';
        await new Promise(r => setTimeout(r, 60));
        d.style.opacity = '0';
        await new Promise(r => setTimeout(r, 60));
        d.style.opacity = '1';
        await new Promise(r => setTimeout(r, 40));
        const eSeq = [];
        e.addEventListener('transitionrun', (ev) => eSeq.push(['run', ev.propertyName]));
        e.addEventListener('transitioncancel', (ev) => eSeq.push(['cancel', ev.propertyName]));
        e.style.transition = 'transform 500ms linear';
        e.style.transform = 'translate(40px, 0px)';
        await new Promise(r => setTimeout(r, 40));
        e.style.transform = 'translate(0px, 0px)';
        await new Promise(r => setTimeout(r, 30));
        e.remove();
        return [seq, eSeq];
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    let (seq, e_seq) = match result.value.unwrap() {
        serde_json::Value::Array(a) => (a[0].clone(), a[1].clone()),
        _ => panic!("expected [seq, eSeq]"),
    };
    assert_eq!(seq.as_array().unwrap().len(), 1, "one cancel, old end timer cleared");
    let entry = seq.as_array().unwrap().last().unwrap();
    assert_eq!(entry[0], "cancel");
    assert_eq!(entry[1], "opacity");
    let elapsed_ms = entry[2].as_i64().unwrap();
    assert!(
        elapsed_ms > 20 && elapsed_ms < 300,
        "cancel elapsed tracks active time, got {elapsed_ms}ms"
    );
    assert_eq!(
        e_seq,
        serde_json::json!([["run", "transform"], ["cancel", "transform"]])
    );
    let list = rt.with_state(|st| st.css_transitions.borrow().clone());
    assert_eq!(
        list.len(),
        2,
        "d's re-trigger replaced its entry; e's stale entry stays but the sampler skips detached nids"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_element_animate_lifecycle_events() {
    // WAAPI Element.animate fires the same lifecycle events with an empty
    // animationName (Chrome parity); detached elements never start.
    let mut rt = setup_runtime("<html><body></body></html>");
    let script = r#"async () => {
        const seq = [];
        const el = document.createElement('div');
        document.body.appendChild(el);
        el.addEventListener('animationstart', (e) => seq.push(['start', e.animationName]));
        el.addEventListener('animationend', (e) => seq.push(['end', e.animationName, e.elapsedTime]));
        const anim = el.animate([{opacity: 1}, {opacity: 0}], {duration: 80});
        const detached = document.createElement('div');
        detached.animate([{opacity: 1}, {opacity: 0}], {duration: 10});
        await new Promise(r => setTimeout(r, 150));
        return [seq, anim.playState];
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(
        result.value.unwrap(),
        serde_json::json!([[["start", ""], ["end", "", 0.08]], "finished"])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_static_markup_animation_listener_scan() {
    // Animations already present in initial markup have no mutation hook to
    // ride on; registering an animation* listener is the signal that the page
    // cares and triggers a capped subtree scan, so the lifecycle still fires.
    let mut rt = setup_runtime(
        "<html><head><style>@keyframes pulse{from{opacity:0}to{opacity:1}} .p{animation:pulse 50ms;}</style></head><body><div id=\"t\" class=\"p\">x</div></body></html>",
    );
    let script = r#"async () => {
        const seq = [];
        // Await the end event itself: in an isolated test process the first
        // layout run (font book init) blocks the loop for ~0.9s, so any
        // wall-clock await armed before the listener-scan microtask would
        // expire before the end timer — a test artifact, not engine behavior.
        const ended = new Promise(r =>
            document.getElementById('t').addEventListener('animationend', r));
        document.getElementById('t').addEventListener('animationstart', (e) =>
            seq.push(['start', e.animationName]));
        document.getElementById('t').addEventListener('animationend', (e) =>
            seq.push(['end', e.animationName]));
        await ended;
        return seq;
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(result.value.unwrap(), serde_json::json!([["start", "pulse"], ["end", "pulse"]]));
}

#[tokio::test(flavor = "current_thread")]
async fn test_css_animation_iteration_event() {
    // blitz#863 family 1: the interior iteration boundary of a 2-iteration
    // animation fires animationiteration (elapsedTime = iteration start =
    // delay + duration×k) between start and end. Values chosen so every
    // elapsedTime is an exact binary float (0.05×2 is 0.1 bit-for-bit).
    let mut rt = setup_runtime(
        "<html><head><style>@keyframes pulse{from{opacity:0}to{opacity:1}} .p{animation:pulse 50ms linear 2;}</style></head><body></body></html>",
    );
    let script = r#"async () => {
        const seq = [];
        const ended = new Promise(r =>
            document.body.addEventListener('animationend', r));
        document.body.addEventListener('animationstart', (e) =>
            seq.push(['start', e.animationName, e.elapsedTime]));
        document.body.addEventListener('animationiteration', (e) =>
            seq.push(['iteration', e.animationName, e.elapsedTime, e instanceof AnimationEvent]));
        document.body.addEventListener('animationend', (e) =>
            seq.push(['end', e.animationName, e.elapsedTime]));
        const el = document.createElement('div');
        el.setAttribute('class', 'p');
        document.body.appendChild(el);
        await Promise.race([ended, new Promise(r => setTimeout(r, 3000))]);
        return seq;
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(
        result.value.unwrap(),
        serde_json::json!([
            ["start", "pulse", 0],
            ["iteration", "pulse", 0.05, true],
            ["end", "pulse", 0.1]
        ])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_style_element_load_event_on_insert() {
    // blitz#863 family 3: a dynamically connected <style> fires `load` as a
    // task (diting's CSS parser drops @import, so there is nothing to wait
    // for beyond the element's own text). Initial-markup <style> elements
    // come through __prepareInitialStylesheets instead — page.rs invokes it
    // before the script loop (see initial_parse_stylesheets_fire_load test).
    let mut rt = setup_runtime("<html><head></head><body></body></html>");
    let script = r#"async () => {
        const s = document.createElement('style');
        s.textContent = '.x { color: red; }';
        const fired = new Promise(r => s.addEventListener('load', (e) =>
            r([e.type, e instanceof Event, e.target === s, e.bubbles])));
        document.head.appendChild(s);
        await Promise.race([fired, new Promise(r => setTimeout(r, 2000))]);
        return fired;
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(result.value.unwrap(), serde_json::json!(["load", true, true, false]));
}

/// Batch-78 leftover: initial-parse `<style>`/`<link rel=stylesheet>` also
/// fire `load`. In Chrome those events are queued as tasks once each sheet
/// applies, and an inline script registering a listener during the parse
/// still catches them — page.rs reproduces that ordering by invoking
/// __prepareInitialStylesheets BEFORE the script loop, so the 0ms tasks land
/// between script executions. This test mirrors the exact page.rs sequence:
/// initial-sheets call, then an "inline script" registering listeners, then
/// the pump. The <link> leg rides the async fetch here (no navigation
/// prefetch in this harness — production hits the ext-sheet fast path).
#[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
#[tokio::test(flavor = "current_thread")]
async fn initial_parse_stylesheets_fire_load_for_early_inline_scripts() {
    let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

    let css = ".x { color: rgb(9, 9, 9); }";
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).unwrap();
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/css\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            css.len(),
            css
        );
        stream.write_all(resp.as_bytes()).unwrap();
        stream.flush().unwrap();
    });

    let mut rt = setup_runtime(
        "<html><head>\
         <style id=\"s\">.x { color: red; }</style>\
         <link id=\"l\" rel=\"stylesheet\" href=\"/sheet.css\">\
         </head><body></body></html>",
    );
    rt.set_url(&format!("http://127.0.0.1:{}/page", port));

    // Exactly the line page.rs runs before the script loop.
    rt.execute_script(
        "<initial-sheets>",
        "if (typeof __prepareInitialStylesheets === 'function') __prepareInitialStylesheets();",
    )
    .unwrap();
    // The "inline script": execute_script is synchronous (no event-loop
    // pump), so these listeners land before the queued 0ms tasks fire.
    rt.execute_script(
        "<inline-1>",
        "globalThis.__heard = []; \
         for (const id of ['s', 'l']) { \
           const el = document.getElementById(id); \
           el.addEventListener('load', (e) => { \
             globalThis.__heard.push([id, e.type, e.target === el, e.bubbles]); \
           }); \
         }",
    )
    .unwrap();

    let result = rt
        .call_function_on_for_cdp(
            r#"async () => {
                await new Promise(r => setTimeout(r, 1500));
                const h = globalThis.__heard || [];
                const pick = (id) => h.find(x => x[0] === id) || null;
                return [pick('s'), pick('l')];
            }"#,
            None,
            &[],
            true,
            true,
        )
        .await
        .unwrap();
    std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");

    let v = result.value.unwrap();
    assert_eq!(
        v,
        serde_json::json!([
            ["s", "load", true, false],
            ["l", "load", true, false]
        ]),
        "both initial-parse sheets fire load, catchable by an earlier inline script"
    );
}

/// innerHTML-connected markup gets the same style-load treatment as
/// appendChild-inserted markup: the setter queues the 0ms task, and a
/// listener attached synchronously after the innerHTML write still catches
/// it (no pump happens in between).
#[tokio::test(flavor = "current_thread")]
async fn inner_html_style_element_fires_load() {
    let mut rt = setup_runtime("<html><head></head><body><div id=\"host\"></div></body></html>");
    let script = r#"async () => {
        const host = document.getElementById('host');
        host.innerHTML = '<style id="is">.y { color: blue; }</style>';
        const s = document.getElementById('is');
        const fired = new Promise(r => s.addEventListener('load', (e) =>
            r([e.type, e.target === s])));
        await Promise.race([fired, new Promise(r => setTimeout(r, 2000))]);
        return fired;
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(result.value.unwrap(), serde_json::json!(["load", true]));
}

#[tokio::test(flavor = "current_thread")]
async fn test_transition_event_interface() {
    // blitz#863 family 2: TransitionEvent carries propertyName / elapsedTime /
    // pseudoElement with the WebIDL defaults and the 1-argument requirement —
    // it used to be an empty Event subclass, so every read was undefined.
    let mut rt = setup_runtime("<html><body></body></html>");
    let script = r#"() => {
        const e1 = new TransitionEvent('transitionend', {
            propertyName: 'opacity', elapsedTime: 1.5, pseudoElement: '::before' });
        const e2 = new TransitionEvent('transitionrun');
        let threw = '';
        try { new TransitionEvent(); } catch (err) { threw = err.constructor.name; }
        return [
            e1 instanceof TransitionEvent, e1 instanceof Event,
            e1.propertyName, e1.elapsedTime, e1.pseudoElement,
            e2.propertyName, e2.elapsedTime, e2.pseudoElement, threw,
        ];
    }"#;
    let result = rt.call_function_on_for_cdp(script, None, &[], true, true).await.unwrap();
    assert_eq!(
        result.value.unwrap(),
        serde_json::json!([true, true, "opacity", 1.5, "::before", "", 0, "", "TypeError"])
    );
}

#[test]
fn fetch_referer_policy_table() {
    use crate::diting_js::ops::fetch_referer;
    let doc = url::Url::parse("http://example.com/page?a=1").unwrap();
    let same = url::Url::parse("http://example.com/other").unwrap();
    let cross = url::Url::parse("http://other.example/x").unwrap();
    let http_target = url::Url::parse("http://127.0.0.1:9/x").unwrap();
    let https_doc = url::Url::parse("https://example.com/page").unwrap();

    // Default policy (empty token): same-origin full URL, cross-origin origin.
    assert_eq!(
        fetch_referer("", "about:client", &doc, &same),
        "http://example.com/page?a=1"
    );
    assert_eq!(
        fetch_referer("", "about:client", &doc, &cross),
        "http://example.com/"
    );

    // Explicit no-referrer and empty-string referrer suppress entirely.
    assert_eq!(fetch_referer("no-referrer", "about:client", &doc, &same), "");
    assert_eq!(fetch_referer("", "", &doc, &same), "");

    // unsafe-url sends the full URL cross-origin.
    assert_eq!(
        fetch_referer("unsafe-url", "about:client", &doc, &cross),
        "http://example.com/page?a=1"
    );

    // origin strips even same-origin down to the origin.
    assert_eq!(
        fetch_referer("origin", "about:client", &doc, &same),
        "http://example.com/"
    );

    // strict-origin and the default suppress on https->http downgrade;
    // the legacy origin policies do not.
    assert_eq!(
        fetch_referer("strict-origin", "about:client", &https_doc, &http_target),
        ""
    );
    assert_eq!(
        fetch_referer("", "about:client", &https_doc, &http_target),
        ""
    );
    assert_eq!(
        fetch_referer("origin", "about:client", &https_doc, &http_target),
        "https://example.com/"
    );

    // no-referrer-when-downgrade keeps the full URL (same scheme).
    assert_eq!(
        fetch_referer("no-referrer-when-downgrade", "about:client", &doc, &same),
        "http://example.com/page?a=1"
    );

    // Explicit referrer override replaces the document; this override is
    // same-origin with the target, so the default policy keeps the full URL
    // (fragment stripped). The policy still strips cross-origin overrides,
    // and credentials/fragments never reach the wire.
    assert_eq!(
        fetch_referer("", "http://other.example/deep#frag", &doc, &cross),
        "http://other.example/deep"
    );
    assert_eq!(
        fetch_referer("unsafe-url", "http://u:p@example.com/a#f", &doc, &cross),
        "http://example.com/a"
    );

    // Non-HTTP(S) referrer values are no-referrer, per spec.
    assert_eq!(fetch_referer("unsafe-url", "file:///etc/passwd", &doc, &cross), "");
}

/// Comma-separated policy values (Referrer-Policy header or <meta> content):
/// tokens are case-insensitive, invalid ones are skipped, the LAST valid
/// token wins; all-invalid yields no policy.
#[test]
fn last_valid_referrer_token_parsing() {
    use crate::diting_js::ops::last_valid_referrer_token;
    assert_eq!(
        last_valid_referrer_token("no-referrer").as_deref(),
        Some("no-referrer")
    );
    // Case-insensitive, surrounding whitespace trimmed.
    assert_eq!(last_valid_referrer_token("  ORIGIN ").as_deref(), Some("origin"));
    // The last valid token wins over an earlier one.
    assert_eq!(
        last_valid_referrer_token("origin, no-referrer").as_deref(),
        Some("no-referrer")
    );
    assert_eq!(
        last_valid_referrer_token("no-referrer, origin").as_deref(),
        Some("origin")
    );
    // Invalid tokens are skipped around a valid one.
    assert_eq!(
        last_valid_referrer_token("garbage, strict-origin").as_deref(),
        Some("strict-origin")
    );
    // All-invalid (including near-miss prefixes) yields no policy.
    assert_eq!(last_valid_referrer_token("only-garbage"), None);
    assert_eq!(last_valid_referrer_token(""), None);
    assert_eq!(last_valid_referrer_token("no-referer"), None);
}

/// RequestInit's referrerPolicy/referrer reach the wire (obscura#875
/// family): the default policy strips cross-origin to the origin,
/// no-referrer sends nothing, unsafe-url sends the full document URL, and an
/// explicit referrer override rides through the same policy table. Before
/// the fix all three scripted options were ignored on the wire.
#[allow(clippy::await_holding_lock)] // the env guard must span the await — that's the serialization
#[tokio::test(flavor = "current_thread")]
async fn fetch_referrer_policy_reaches_the_wire() {
    let _env_guard = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
    std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = captured.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for _ in 0..4 {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => break,
            };
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            seen.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_string());
            let body = b"{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\naccess-control-allow-origin: *\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        }
    });

    let mut rt = setup_runtime("<html><body></body></html>");
    rt.set_url("http://example.com/page?a=1");
    let result = rt
        .call_function_on_for_cdp(
            r#"async () => {
                const base = "http://127.0.0.1:PORT";
                await fetch(base + "/default");
                await fetch(base + "/noref", { referrerPolicy: "no-referrer" });
                await fetch(base + "/unsafe", { referrerPolicy: "unsafe-url" });
                await fetch(base + "/explicit", { referrer: "http://other.example/deep" });
                return "done";
            }"#
            .replace("PORT", &port.to_string())
            .as_str(),
            None,
            &[],
            true,
            true,
        )
        .await
        .unwrap();
    assert_eq!(result.value.unwrap().as_str().unwrap_or_default(), "done");

    let requests = captured.lock().unwrap();
    let referer_of = |path: &str| -> Option<String> {
        requests
            .iter()
            .find(|r| r.contains(&format!("GET {path} ")))
            .and_then(|r| {
                r.lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("referer:"))
                    .map(|l| l.splitn(2, ':').nth(1).unwrap_or("").trim().to_string())
            })
    };
    assert_eq!(referer_of("/default").as_deref(), Some("http://example.com/"));
    assert_eq!(referer_of("/noref"), None);
    assert_eq!(
        referer_of("/unsafe").as_deref(),
        Some("http://example.com/page?a=1")
    );
    assert_eq!(referer_of("/explicit").as_deref(), Some("http://other.example/"));
}

/// Generated content v1 (batch 99): ::before/::after boxes synthesize into
/// layout. Block pseudos grow the host (the Bootstrap 3 clearfix family —
/// `display:table` coerces to block for flow presence), inline pseudos
/// merge onto the adjacent text run without adding a line, attr() resolves
/// against the host, and content:none cascades but produces no box.
#[cfg(feature = "screenshot")]
#[test]
fn generated_content_before_after_layout() {
    let mut rt = setup_runtime(
        r#"<style>
          body{margin:0}
          .grow::after{content:"";display:block;height:20px}
          .tbl::before{content:"";display:table;height:15px}
          .merge li::before{content:"• "}
          .attr::after{content:attr(data-tip)}
          .none::after{content:none}
        </style>
        <ul class="merge"><li id="li">one</li></ul>
        <ul><li id="liCtrl">one</li></ul>
        <div id="grow" class="grow"></div>
        <div id="tbl" class="tbl"></div>
        <div id="attr" class="attr" data-tip="abc"></div>
        <div id="none" class="none"></div>"#,
    );
    let v = rt
        .evaluate(
            r#"
            const h = (id) => Math.round(document.getElementById(id).getBoundingClientRect().height);
            return { li: h('li'), liCtrl: h('liCtrl'), grow: h('grow'), tbl: h('tbl'), attr: h('attr'), none: h('none') };
        "#,
        )
        .unwrap();
    assert_eq!(
        v["li"], v["liCtrl"],
        "inline ::before merges onto the adjacent run: no extra line"
    );
    assert_eq!(
        v["grow"], serde_json::json!(20),
        "block ::after with a height grows the host (clearfix box)"
    );
    assert_eq!(
        v["tbl"], serde_json::json!(15),
        "display:table pseudos coerce to block: flow presence, not the table walk"
    );
    assert!(
        v["attr"].as_f64().unwrap_or(0.0) >= 10.0,
        "attr() content on a childless host still produces a line"
    );
    assert_eq!(
        v["none"], serde_json::json!(0),
        "content:none cascades but produces no box"
    );
}

/// getComputedStyle face for `content` (batch 99): the default reads
/// "normal" (Chrome's initial), declared values re-serialize quoted,
/// attr() keeps its functional shape, and pseudo-only declarations never
/// leak onto the host element.
#[cfg(feature = "screenshot")]
#[test]
fn computed_style_content_face() {
    let mut rt = setup_runtime(
        r#"<style>
          #q::before{content:"P"}
          #a{content:attr(k)}
          #s{content:"S"}
        </style>
        <span id="q"></span><span id="a" k="v1"></span><span id="s"></span><span id="d"></span>"#,
    );
    let cs = |rt: &mut JsRuntime, sel: &str| {
        rt.evaluate(&format!(
            "getComputedStyle(document.querySelector('{}')).content",
            sel
        ))
        .unwrap()
    };
    assert_eq!(
        cs(&mut rt, "#q"),
        serde_json::json!("normal"),
        "pseudo-only rule never lands on the host"
    );
    assert_eq!(cs(&mut rt, "#a"), serde_json::json!("attr(k)"));
    assert_eq!(cs(&mut rt, "#s"), serde_json::json!("\"S\""));
    assert_eq!(
        cs(&mut rt, "#d"),
        serde_json::json!("normal"),
        "default computed content is Chrome's 'normal'"
    );
}

/// obscura #722 probe: a plain inline element sharing a line with an atomic
/// inline (inline-block sibling) or a float must still report real gBCR
/// geometry — 0x0 there means the run refused to fold and dropped the box.
#[cfg(feature = "screenshot")]
#[test]
fn inline_next_to_atomic_inline_has_rect() {
    let mut rt = setup_runtime(
        r#"<style>p{margin:0;font-size:16px;line-height:20px}</style>
        <p><span style="display:inline-block;width:50px;height:20px"></span><b id="b">hi</b></p>
        <p><i id="alone">solo</i></p>"#,
    );
    let box_of = |rt: &mut JsRuntime, sel: &str| {
        rt.evaluate(&format!(
            "(function(){{const r=document.querySelector('{}').getBoundingClientRect();\
              return {{w:Math.round(r.width),h:Math.round(r.height),x:Math.round(r.x)}};}})()",
            sel
        ))
        .unwrap()
    };
    let b = box_of(&mut rt, "#b");
    let alone = box_of(&mut rt, "#alone");
    assert!(
        b["w"].as_f64().unwrap_or(0.0) > 0.0 && b["h"].as_f64().unwrap_or(0.0) > 0.0,
        "inline beside an atomic inline reports 0x0: {:?} (control {:?})",
        b,
        alone
    );
    assert!(
        alone["w"].as_f64().unwrap_or(0.0) > 0.0,
        "control inline alone must have a rect"
    );
}

/// obscura #767 probe: width:calc(100% - 32px) inside a flex subtree must
/// resolve against the flex container's content box (400px → 368), same as
/// the identical block-level control.
#[cfg(feature = "screenshot")]
#[test]
fn calc_width_inside_flex_subtree() {
    let mut rt = setup_runtime(
        r#"<style>
          #flex{display:flex;width:400px}
          #item{width:calc(100% - 32px);height:10px}
          #wrap{width:400px}
          #blk{width:calc(100% - 32px);height:10px}
        </style>
        <div id="flex"><div id="item"></div></div>
        <div id="wrap"><div id="blk"></div></div>"#,
    );
    let w_of = |rt: &mut JsRuntime, sel: &str| {
        rt.evaluate(&format!(
            "Math.round(document.querySelector('{}').getBoundingClientRect().width)",
            sel
        ))
        .unwrap()
    };
    let flex_w = w_of(&mut rt, "#item").as_f64().unwrap_or(0.0);
    let blk_w = w_of(&mut rt, "#blk").as_f64().unwrap_or(0.0);
    assert!(
        (flex_w - 368.0).abs() <= 1.0,
        "calc() width collapsed in flex subtree: {} (block control {})",
        flex_w,
        blk_w
    );
}

/// Batch 99 follow-up probe: the Bootstrap 3 clearfix. A host whose only
/// child is a float must reach the float's height once its ::after runs
/// `content:"";display:table;clear:both` — the pseudo leaf clears below the
/// float zone and the host's height accounts for the pushed-out box.
#[cfg(feature = "screenshot")]
#[test]
fn clearfix_pseudo_clears_float_zone() {
    let mut rt = setup_runtime(
        r#"<style>
          .row::after{content:"";display:table;clear:both}
          .col{float:left;width:50px;height:40px}
        </style>
        <div class="row" id="row"><div class="col"></div></div>
        <div id="plain" style="position:relative"><div class="col"></div></div>"#,
    );
    let h_of = |rt: &mut JsRuntime, sel: &str| {
        rt.evaluate(&format!(
            "Math.round(document.querySelector('{}').getBoundingClientRect().height)",
            sel
        ))
        .unwrap()
    };
    let row_h = h_of(&mut rt, "#row").as_f64().unwrap_or(0.0);
    let plain_h = h_of(&mut rt, "#plain").as_f64().unwrap_or(0.0);
    assert!(
        (row_h - 40.0).abs() <= 1.0,
        "clearfix row collapsed to {} (plain float host {}); ::after clear:both must end the float zone",
        row_h,
        plain_h
    );
    // Posture pin: this engine contains floats in the parent unconditionally
    // (Chrome does so only for BFC roots, where a clearfix-less host would
    // collapse to 0). The protective posture makes the Bootstrap 3 idiom
    // work without the pseudo's clear being load-bearing. If this ever
    // tightens to Chrome's exact BFC semantics, the ::after clear path must
    // start carrying the containment (see zone_end_at_budget, which reads
    // clear_side by NodeId and cannot see pseudo leaves).
    assert!(
        (plain_h - 40.0).abs() <= 1.0,
        "float containment posture changed: plain host is now {} (was 40 = contains)",
        plain_h
    );
}

/// Batch 102: getComputedStyle(el, pseudoElt) — the pseudo-element computed
/// face. The second argument routes to the host's cascaded ::before/::after
/// styles (batch 99 cascade), pseudos with no matching rule answer the
/// initial-value table (Chrome's no-throw posture extends to unknown `::`
/// forms), junk arguments throw TypeError, and pseudo declarations never
/// leak into the host face.
#[cfg(feature = "screenshot")]
#[test]
fn computed_style_pseudo_element_face() {
    let mut rt = setup_runtime(
        r#"<style>
          #q::before{content:"P";color:rgb(255, 0, 0)}
          #q{color:rgb(0, 0, 255)}
          #a::after{content:attr(k)}
        </style>
        <span id="q">host</span><span id="a" k="v1"></span><span id="n"></span>"#,
    );
    let read = |rt: &mut JsRuntime, expr: &str| -> String {
        match rt.evaluate(expr) {
            Ok(serde_json::Value::String(s)) => s,
            other => panic!("{expr} -> {other:?}"),
        }
    };
    // Pseudo face reads the pseudo cascade, not the host's.
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('q'),'::before').getPropertyValue('content')"),
        r#""P""#
    );
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('q'),'::before').color"),
        "rgb(255, 0, 0)"
    );
    // Host face untouched by the pseudo declarations.
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('q')).color"),
        "rgb(0, 0, 255)"
    );
    // Legacy single-colon spelling, ASCII case-insensitive.
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('q'),':BEFORE').getPropertyValue('content')"),
        r#""P""#
    );
    // attr() resolves against the host's attributes at cascade time.
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('a'),'::after').getPropertyValue('content')"),
        r#""v1""#
    );
    // Pseudo with no matching rule (and unmodeled `::name` forms) answers
    // the initial-value table.
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('q'),'::after').getPropertyValue('content')"),
        "normal"
    );
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('n'),'::before').getPropertyValue('content')"),
        "normal"
    );
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('q'),'::fancy').getPropertyValue('content')"),
        "normal"
    );
    // The pseudo generates no box: geometry reads 'auto' instead of the
    // host's bounding rect.
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('q'),'::before').width"),
        "auto"
    );
    // Junk arguments throw TypeError (CSSOM argument grammar: only the four
    // legacy single-colon names and `::name` forms are valid). The eval
    // harness swallows throws to null, so the probe catches in-script.
    for junk in ["fancy", ":selection"] {
        let expr = format!(
            "(function(){{try{{getComputedStyle(document.getElementById('q'),'{}');return 'no-throw';}}catch(e){{return e instanceof TypeError?'TypeError':'other:'+e;}}}})()",
            junk
        );
        assert_eq!(read(&mut rt, &expr), "TypeError", "junk arg {junk}");
    }
}

/// Batch 105: counter()/counters() numbering — the css-lists-3 §4.3 worked
/// example's canonical shapes: flat 1..n, nested ol joins "3.1", the li after
/// the nested list continues the OUTER counter (inner counter never reaches
/// following siblings of its creator's parent), and a sibling ol restarts
/// because reset shadows previous-sibling-origin counters (§4.4.2).
#[cfg(feature = "screenshot")]
#[test]
fn counters_numbering_scoping_and_restart() {
    let mut rt = setup_runtime(
        r#"<style>
          ol{counter-reset:item;list-style:none;margin:0;padding:0}
          li::before{counter-increment:item;content:counters(item,".") " "}
        </style>
        <ol>
          <li id="a">alpha</li>
          <li id="b">beta</li>
          <li id="c">gamma<ol><li id="d">delta</li></ol></li>
          <li id="e">epsilon</li>
        </ol>
        <ol><li id="f">zeta</li></ol>"#,
    );
    let before = |rt: &mut JsRuntime, id: &str| -> String {
        match rt.evaluate(&format!(
            "getComputedStyle(document.getElementById('{}'),'::before').getPropertyValue('content')",
            id
        )) {
            Ok(serde_json::Value::String(s)) => s,
            other => panic!("{id} ::before -> {other:?}"),
        }
    };
    assert_eq!(before(&mut rt, "a"), "\"1 \"");
    assert_eq!(before(&mut rt, "b"), "\"2 \"");
    assert_eq!(before(&mut rt, "c"), "\"3 \"");
    assert_eq!(before(&mut rt, "d"), "\"3.1 \"", "nested ol joins the join");
    assert_eq!(
        before(&mut rt, "e"),
        "\"4 \"",
        "li after the nested list keeps counting the outer counter"
    );
    assert_eq!(
        before(&mut rt, "f"),
        "\"1 \"",
        "sibling ol's reset shadows the previous sibling's counter"
    );
}

/// Batch 105: counter style formatting, implicit-zero counters, same-element
/// reset+increment composition (reset then increment → 6), and the host
/// gCS face keeping the source shape functional (Chrome parity).
#[cfg(feature = "screenshot")]
#[test]
fn counter_styles_composition_and_host_face() {
    let mut rt = setup_runtime(
        r#"<style>
          .r::before{content:counter(x, upper-roman) " "}
          .al::before{content:counter(y, lower-alpha) " "}
          .ci::before{content:counter(n) " "}
          .z::before{content:counter(ghost)}
          #h{content:counter(w, upper-roman) " " attr(data-k)}
        </style>
        <div style="counter-reset:x 3"><span class="r"></span></div>
        <div style="counter-reset:y"><span class="al"></span></div>
        <span class="ci" style="counter-reset:n 5;counter-increment:n"></span>
        <span class="z"></span>
        <span id="h" data-k="K"></span>"#,
    );
    let read = |rt: &mut JsRuntime, expr: &str| -> String {
        match rt.evaluate(expr) {
            Ok(serde_json::Value::String(s)) => s,
            other => panic!("{expr} -> {other:?}"),
        }
    };
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.querySelector('.r'),'::before').getPropertyValue('content')"),
        "\"III \"",
        "reset to 3 renders upper-roman"
    );
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.querySelector('.al'),'::before').getPropertyValue('content')"),
        "\"0 \"",
        "lower-alpha of 0 falls back to decimal"
    );
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.querySelector('.ci'),'::before').getPropertyValue('content')"),
        "\"6 \"",
        "reset 5 then increment on the same element"
    );
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.querySelector('.z'),'::before').getPropertyValue('content')"),
        "\"0\"",
        "counter of a name never reset is 0"
    );
    // Host face: source shape round-trips functional notation.
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('h')).content"),
        "counter(w, upper-roman) \" \" attr(data-k)"
    );
}

/// Batch 105: quotes — open/close-quote pick the depth'th pair (last pair
/// repeats), `quotes: none` yields empty strings, and the property-less
/// default is curly quotes.
#[cfg(feature = "screenshot")]
#[test]
fn quotes_depth_pairs_none_and_default() {
    let mut rt = setup_runtime(
        r#"<style>
          .o::before{content:open-quote}
          .o::after{content:close-quote}
          #nn{quotes:none}
          #nn::before{content:open-quote}
          #dd::before{content:open-quote}
          #dd::after{content:close-quote}
        </style>
        <div style="quotes: '«' '»' '‹' '›'"><span class="o"><span class="o"></span></span></div>
        <span id="dd"></span>
        <span id="nn"></span>"#,
    );
    let read = |rt: &mut JsRuntime, expr: &str| -> String {
        match rt.evaluate(expr) {
            Ok(serde_json::Value::String(s)) => s,
            other => panic!("{expr} -> {other:?}"),
        }
    };
    let outer = "getComputedStyle(document.querySelector('div > span'),'";
    assert_eq!(
        read(&mut rt, &format!("{outer}::before').getPropertyValue('content')")),
        "\"«\"",
        "outermost quote takes the first pair"
    );
    assert_eq!(
        read(&mut rt, &format!("{outer}::after').getPropertyValue('content')")),
        "\"»\""
    );
    let inner = "getComputedStyle(document.querySelector('div > span > span'),'";
    assert_eq!(
        read(&mut rt, &format!("{inner}::before').getPropertyValue('content')")),
        "\"‹\"",
        "nested quote takes the depth'th pair"
    );
    assert_eq!(
        read(&mut rt, &format!("{inner}::after').getPropertyValue('content')")),
        "\"›\""
    );
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('nn'),'::before').getPropertyValue('content')"),
        "\"\"",
        "quotes:none makes open-quote empty"
    );
    assert_eq!(
        read(&mut rt, "getComputedStyle(document.getElementById('dd'),'::before').getPropertyValue('content')"),
        "\"\u{201C}\"",
        "property-less default is the curly left double quote"
    );
}

/// obscura#993 lineage: custom elements created AFTER define() must upgrade.
/// craigslist's search shell defines <cl-search-result> at boot, then
/// document.createElement's one per result inside the fetch callback — the
/// parse-time sweep in define() never sees those, so the shell rendered an
/// empty list. createElement must hand back an already-upgraded element
/// (methods attached, connectedCallback held while detached) and appendChild
/// must fire the callback so the element can render its own children.
#[test]
fn custom_elements_created_after_define_upgrade_on_insert() {
    let mut rt = setup_runtime("<html><body></body></html>");
    let js = r#"
        globalThis.__log = [];
        customElements.define('ce-after', class extends HTMLElement {
          connectedCallback() {
            globalThis.__log.push('ccb');
            const a = document.createElement('a');
            a.className = 'hit'; a.textContent = 'go';
            this.appendChild(a);
          }
        });
        const el = document.createElement('ce-after');
        const bornUpgraded = typeof el.connectedCallback === 'function';
        const silentWhileDetached = globalThis.__log.join('') === '';
        document.body.appendChild(el);
        const out = {
          bornUpgraded: bornUpgraded,
          silentWhileDetached: silentWhileDetached,
          firedOnInsert: globalThis.__log.join('') === 'ccb',
          rendered: document.querySelectorAll('ce-after a.hit').length === 1,
          text: document.querySelector('ce-after a.hit').textContent
        };
        out
    "#;
    let v = rt.evaluate(js).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "bornUpgraded": true,
            "silentWhileDetached": true,
            "firedOnInsert": true,
            "rendered": true,
            "text": "go"
        })
    );
}

/// The markup half of the same hole: elements entering through innerHTML
/// markup (never seen by createElement) must upgrade on the connected
/// container. Scripts inside innerHTML stay inert per spec — elements do not.
#[test]
fn custom_elements_in_innerhtml_markup_upgrade_when_connected() {
    let mut rt = setup_runtime("<html><body><div id='host'></div></body></html>");
    let js = r#"
        globalThis.__n = 0;
        customElements.define('ce-markup', class extends HTMLElement {
          connectedCallback() { globalThis.__n++; this.setAttribute('data-seen', 'yes'); }
        });
        const host = document.getElementById('host');
        host.innerHTML = '<p>x</p><ce-markup></ce-markup>';
        const fired = globalThis.__n === 1;
        const seen = document.querySelector('ce-markup').getAttribute('data-seen');
        host.innerHTML = '<ce-markup id="two"></ce-markup>';
        ({ fired: fired, seen: seen, refire: globalThis.__n === 2 })
    "#;
    let v = rt.evaluate(js).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "fired": true, "seen": "yes", "refire": true })
    );
}

/// Chrome's connect/disconnect cycle: an upgraded element leaving the
/// document fires disconnectedCallback, and a later re-insertion fires
/// connectedCallback again — move/reshuffle logic in lit-style frameworks
/// depends on the pairing.
#[test]
fn custom_elements_reconnect_refires_connected_callback() {
    let mut rt = setup_runtime("<html><body></body></html>");
    let js = r#"
        globalThis.__log = [];
        customElements.define('ce-cycle', class extends HTMLElement {
          connectedCallback() { globalThis.__log.push('c'); }
          disconnectedCallback() { globalThis.__log.push('d'); }
        });
        const el = document.createElement('ce-cycle');
        document.body.appendChild(el);
        document.body.removeChild(el);
        document.body.appendChild(el);
        globalThis.__log.join('')
    "#;
    let v = rt.evaluate(js).unwrap();
    assert_eq!(v, serde_json::json!("cdc"));
}
