//! JS source builders for the eval / callFunctionOn wrapper family — split
//! from the module root (god-file ratchet). Pure string assembly, no runtime
//! state: an expression in, the exact script text `execute_script` will run
//! out.
use super::JsRuntime;

// Anonymous Function trampoline around indirect eval: user stacks must never
// surface our bootstrap source or wrapper names (stack-shape detectors). The leading `_namedBoot` (#48) installs window named access before the script runs — a one-flag no-op after the first successful scan; build_args carries the same call for the callFunctionOn wrappers (no INLINE_EVAL_JS there).
const INLINE_EVAL_JS: &str = "(new Function(\"s\",\"globalThis._namedBoot&&globalThis._namedBoot();try{return (0,eval)(s)}catch(x){if(x instanceof SyntaxError){return (new Function(s))()}throw x}\"))";

impl JsRuntime {
    /// Object-store slot expression with the id embedded as a JSON string
    /// literal, so an objectId — minted or inbound — can never splice itself
    /// into the surrounding script (obscura#843 class).
    pub(super) fn object_slot(oid: &str) -> String {
        let lit = serde_json::to_string(oid).unwrap_or_else(|_| "\"\"".to_string());
        format!("globalThis.__diting_objects[{lit}]")
    }

    /// Shared catch tail of the await-settle wrappers: store the exception,
    /// mark the settle rejected, set the done sentinel — all synchronously,
    /// so even the failure path needs no event-loop turn (#114).
    fn settle_fail_tail(oid: &str, done_counter: u64) -> String {
        format!(
            "{slot} = e;\n\
             globalThis.__diting_await_meta = {exc};\n\
             globalThis.__diting_await_rejected = true;\n\
             globalThis.__diting_done_{done_counter} = true;",
            slot = Self::object_slot(oid),
            exc = Self::exception_meta_extract_js("e"),
            done_counter = done_counter,
        )
    }

    /// Meta-code for Runtime.evaluate. The await variant is the #114 shape:
    /// run the expression, then only `await` when the result is actually a
    /// thenable — everything else (probes, assignments, reads: the majority
    /// of evals) assigns the slot and sets the done sentinel inside this very
    /// execute_script call. An unconditional `await` parks the sentinel in a
    /// microtask, and deno_core executes scripts with no microtask
    /// checkpoint, so the sentinel can only turn true on the next event-loop
    /// turn — which a churny page (a revived persistent session re-running
    /// its boot scripts) holds hostage behind multi-second synchronous page
    /// scripts until the eval burns its whole budget into EVAL_TIMEOUT.
    pub(super) fn eval_meta_code(
        expression: &str,
        oid: &str,
        done_counter: u64,
        await_promise: bool,
    ) -> String {
        // CDP Runtime.evaluate semantics: the input is a *script*, not an
        // expression — statements are legal and the completion value of the
        // last statement is the result (see wrap_expression for the
        // indirect-eval rationale). A bare `{...}` body is parenthesized so
        // pasted JSON evaluates as an object literal, not a valueless block.
        let trimmed = expression.trim();
        let body = if trimmed.starts_with('{') && trimmed.ends_with('}') {
            format!("({})", trimmed)
        } else {
            trimmed.to_string()
        };
        let expr_literal = serde_json::to_string(&body).unwrap_or_else(|_| "\"\"".to_string());
        let slot = Self::object_slot(oid);
        // Both paths set __diting_await_meta + __diting_await_rejected so the
        // read-back after the IIFE is uniform whether the expression was
        // awaited or run synchronously.
        if !await_promise {
            return format!(
                "(function() {{\n\
                    var __result;\n\
                    try {{\n\
                        __result = {INLINE_EVAL_JS}({expr});\n\
                        {slot} = __result;\n\
                        globalThis.__diting_await_meta = {meta};\n\
                        globalThis.__diting_await_rejected = false;\n\
                    }} catch(e) {{\n\
                        {slot} = e;\n\
                        globalThis.__diting_await_meta = {exc};\n\
                        globalThis.__diting_await_rejected = true;\n\
                    }}\n\
                }})()",
                expr = expr_literal,
                slot = slot,
                meta = Self::meta_extract_js("__result"),
                exc = Self::exception_meta_extract_js("e"),
            );
        }
        let fail = Self::settle_fail_tail(oid, done_counter);
        format!(
            "(async function() {{\n\
                var __result;\n\
                try {{ __result = {INLINE_EVAL_JS}({expr}); }}\n\
                catch(e) {{ {fail} return; }}\n\
                if (__result && typeof __result.then === 'function') {{\n\
                    try {{ __result = await __result; }}\n\
                    catch(e) {{ {fail} return; }}\n\
                }}\n\
                {slot} = __result;\n\
                globalThis.__diting_await_meta = {meta};\n\
                globalThis.__diting_await_rejected = false;\n\
                globalThis.__diting_done_{done_counter} = true;\n\
            }})()",
            expr = expr_literal,
            fail = fail,
            slot = slot,
            meta = Self::meta_extract_js("__result"),
            done_counter = done_counter,
        )
    }

    /// Meta-code for the awaitPromise variant of Runtime.callFunctionOn —
    /// same #114 sync-first shape as [`Self::eval_meta_code`]: the call runs
    /// synchronously, and only a thenable result takes the `await` branch.
    pub(super) fn call_fn_meta_code(
        setup: &str,
        fn_decl: &str,
        this_expr: &str,
        args_list: &str,
        oid: &str,
        done_counter: u64,
    ) -> String {
        let fail = Self::settle_fail_tail(oid, done_counter);
        format!(
            "(async function() {{\n\
                {setup}\n\
                var __fn = ({fn_decl});\n\
                var __this = ({this_expr});\n\
                var __result;\n\
                try {{ __result = __fn.call(__this, {args}); }}\n\
                catch(e) {{ {fail} return; }}\n\
                if (__result && typeof __result.then === 'function') {{\n\
                    try {{ __result = await __result; }}\n\
                    catch(e) {{ {fail} return; }}\n\
                }}\n\
                {slot} = __result;\n\
                globalThis.__diting_await_meta = {meta};\n\
                globalThis.__diting_await_rejected = false;\n\
                globalThis.__diting_done_{done_counter} = true;\n\
            }})()",
            setup = setup,
            fn_decl = fn_decl,
            this_expr = this_expr,
            args = args_list,
            fail = fail,
            slot = Self::object_slot(oid),
            meta = Self::meta_extract_js("__result"),
            done_counter = done_counter,
        )
    }

    pub(super) fn wrap_expression(expression: &str) -> String {
        // CDP Runtime.evaluate semantics: the input is a *script*, not an
        // expression — statements are legal and the completion value of the
        // last statement is the result. Indirect eval (`(0,eval)(...)`) runs
        // it at global scope and hands back the completion value, so
        // `var x=1; x*2` returns 2 exactly like Chrome. The previous
        // `return (...);` expression wrap turned any statement syntax into
        // `Unexpected token ';'` (unless the script happened to start with
        // one of six hard-coded statement keywords), which silently broke
        // clients that evaluate statement scripts — e.g. probing a
        // WAF challenge page with `try { readygo(); } catch(e) {...}`.
        // serde_json::to_string emits a JS-safe string literal (quotes,
        // newlines, U+2028/2029 all escaped), and a trailing
        // `//# sourceURL=...` line comment lives inside the literal instead
        // of eating a closing paren.
        //
        // One convenience divergence from raw script semantics: a bare
        // `{...}` input is parenthesized so pasted JSON evaluates as an
        // object literal (like DevTools console), not as a block whose
        // completion value is undefined.
        let trimmed = expression.trim();
        let body = if trimmed.starts_with('{') && trimmed.ends_with('}') {
            format!("({})", trimmed)
        } else {
            trimmed.to_string()
        };
        let literal = serde_json::to_string(&body).unwrap_or_else(|_| "\"\"".to_string());
        format!(
            "(function() {{ try {{ return {INLINE_EVAL_JS}({}); }} catch(e) {{ return null; }} }})()",
            literal
        )
    }

    pub(super) fn meta_extract_js(var_name: &str) -> String {
        format!(
            r#"(function(v) {{
                var t = typeof v;
                var st = null, cn = '', desc = '', val;
                if (v === null) {{ t = 'object'; st = 'null'; desc = 'null'; }}
                else if (v === undefined) {{ t = 'undefined'; }}
                else if (Array.isArray(v)) {{
                    st = 'array'; cn = 'Array';
                    desc = 'Array(' + v.length + ')';
                }}
                else if (t === 'object' && typeof v._nid === 'number') {{
                    st = 'node';
                    cn = v.constructor ? v.constructor.name : 'Node';
                    if (v.nodeType === 9) cn = 'HTMLDocument';
                    else if (v.nodeType === 1) cn = 'HTML' + (v.tagName || 'Element').charAt(0) + (v.tagName || 'Element').slice(1).toLowerCase() + 'Element';
                    desc = v.tagName ? v.tagName.toLowerCase() : (v.nodeName || 'node');
                }}
                else if (t === 'function') {{
                    cn = 'Function';
                    desc = v.name ? 'function ' + v.name + '()' : 'function()';
                }}
                else if (t === 'object') {{
                    cn = (v.constructor && v.constructor.name) || 'Object';
                    desc = cn;
                }}
                else if (t === 'number' || t === 'boolean' || t === 'string') {{
                    // Chrome puts the real value (JSON number/bool/string, not
                    // a description-string copy) on the RemoteObject — and
                    // only for these three; JSON.stringify drops the key when
                    // `val` stayed undefined, and serializing a bigint/symbol
                    // would throw.
                    val = v; desc = String(v);
                }}
                else {{ desc = String(v); }}
                return JSON.stringify({{type:t,subtype:st,className:cn,description:desc,value:val}});
            }})({var_name})"#,
            var_name = var_name,
        )
    }

    /// Extract an exception's constructor name + message as a JSON blob, the
    /// same shape `info_from_meta` reads. `meta_extract_js` stops at the
    /// constructor name ("Error") — it never pulls the message, so a thrown
    /// `new Error('boom')` would otherwise surface as description "Error"
    /// instead of "Error: boom", which is what Chrome's `exceptionDetails`
    /// reports.
    pub(super) fn exception_meta_extract_js(var_name: &str) -> String {
        format!(
            r#"(function(e) {{
                var name = '', msg = '', desc = '', frame = '', line = 0, col = 0;
                if (e !== null && e !== undefined) {{
                    if (typeof e === 'object' || typeof e === 'function') {{
                        name = e.name || (e.constructor && e.constructor.name) || '';
                        if (typeof e.message === 'string') msg = e.message;
                    }} else {{
                        try {{ msg = String(e); }} catch (_) {{}}
                    }}
                    // Prefer the <anonymous>:L:C frame — that's the caller's
                    // eval'd script; the outermost "at" frame here is the
                    // bootstrap eval wrapper, whose position points into
                    // bootstrap.js and would mislead.
                    if (typeof e.stack === 'string') {{
                        var lines = e.stack.split('\n');
                        for (var i = 0; i < lines.length; i++) {{
                            var ln = lines[i].trim();
                            if (ln.indexOf('at ') !== 0) continue;
                            var m = ln.match(/<anonymous>:(\d+):(\d+)/);
                            if (m) {{ frame = ln; line = +m[1]; col = +m[2]; break; }}
                            if (!frame) frame = ln;
                        }}
                    }}
                }}
                if (msg) {{ desc = name ? (name + ': ' + msg) : msg; }}
                else {{ desc = name || msg || 'Uncaught exception'; }}
                return JSON.stringify({{className:name, description:desc, stack_first:frame, line:line, col:col}});
            }})({var_name})"#,
            var_name = var_name,
        )
    }
}
