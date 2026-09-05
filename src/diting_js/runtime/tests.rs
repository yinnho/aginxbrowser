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

    /// Computed-style property reads must resolve through the lookup chain
    /// (inline → bounding-rect geometry → defaults), not get short-circuited
    /// by element.style's named-property surface: that surface claims every
    /// CSS property, so the Proxy's `prop in target` branch answered ''
    /// for width while getBoundingClientRect reported real geometry — the
    /// exact read react-virtuoso-style libraries branch on.
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
            ("HTMLDivElement.prototype === Element.prototype", true),
            ("document.getElementById('d').constructor === Element", true),
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
        assert_eq!(
            rt.take_pending_console_calls(),
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

    #[test]
    fn test_location() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let href = rt.evaluate("location.href").unwrap();
        assert_eq!(href, serde_json::json!("http://example.com/test"));
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
            .evaluate_for_cdp_outcome("(() => { throw new Error('sync-boom') })()", false, false)
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
            .evaluate_for_cdp_outcome("(() => { throw new Error('bv-boom') })()", true, false)
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
            .evaluate_for_cdp_outcome("Promise.reject(new Error('boom'))", true, true)
            .await
            .unwrap();
        let exc = outcome.exception.expect("expected exception");
        assert_eq!(exc.text, "Uncaught (in promise)");
        assert_eq!(exc.description, "Error: boom");
        assert_eq!(exc.class_name, "Error");
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

    /// The module path must answer to the same deny-by-default posture as
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
        let mut rt = setup_runtime("<html><body></body></html>");
        let len_in = rt.evaluate("document.elementsFromPoint(10, 10).length").unwrap();
        assert_eq!(len_in.as_f64().unwrap() as i64, 1);
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

    #[test]
    fn test_domparser_xml_parsererror_on_malformed() {
        // Upstream 53295fa+6927f11+869f700+20c4628: XML mime types get a
        // well-formedness pass; malformed input yields a <parsererror>
        // documentElement that querySelector('parsererror') finds, matching
        // Chrome. Self-closing roots count as complete elements.
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
                "OK:HTML", "OK:HTML",
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
        // (style/attribute reflection/event dispatch), assigning .src must
        // emulate a successful decode (complete flips, load fires on both
        // the onload property and listeners), and a pre-defined
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
            img.src = '/pixel.png';
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
        let mut rt = setup_runtime(r#"<html><body><div id="d"></div></body></html>"#);
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
