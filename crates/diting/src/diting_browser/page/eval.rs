//! JS evaluation surface: the evaluate family (plain + CDP remote-object
//! protocol), DOM access helpers and object-store release. Split from page/mod.rs (ARCHITECTURE.md P2 batch 3); behavior unchanged.
use super::*;

impl Page {
    #[cfg_attr(not(test), allow(dead_code))] // snapshot helper exercised by tests; DOM consumers read via evaluate
    pub fn with_dom<R>(&self, f: impl FnOnce(&DomTree) -> R) -> Option<R> {
        if let Some(js) = &self.js {
            return js.with_dom(f);
        }
        self.dom.as_ref().map(f)
    }

    #[allow(dead_code)] // CDP DOM.getFlattenedDocument parity — no CDP client to serve it yet
    pub fn dom(&self) -> Option<&DomTree> {
        self.dom.as_ref()
    }

    /// V8 isolate handle for this page's runtime, if it has been initialized.
    /// Lets the CDP dispatcher arm a per-command watchdog (which bounds any one
    /// command so a hung page cannot hold the process-wide V8 lock forever)
    /// without taking `&mut self`.
    #[allow(dead_code)] // CDP per-command-watchdog plumbing — the CDP server itself is not absorbed
    pub fn isolate_handle(&self) -> Option<crate::diting_js::runtime::IsolateHandle> {
        self.js.as_ref().map(|js| js.isolate_handle())
    }

    /// Clear a V8 termination left by a per-command watchdog so the next command
    /// on this page can run. No-op if the runtime is absent or not terminating.
    #[allow(dead_code)] // ditto — watchdog-clearing half of the CDP plumbing above
    pub fn cancel_v8_termination(&mut self) {
        if let Some(js) = self.js.as_mut() {
            js.cancel_termination();
        }
    }

    /// Like [`Self::evaluate`] but bounded by a V8 watchdog so a runaway
    /// expression cannot hang the process. A non-zero `timeout` of zero falls
    /// back to the unbounded path.
    pub fn evaluate_with_timeout(
        &mut self,
        expression: &str,
        timeout: std::time::Duration,
    ) -> serde_json::Value {
        if let Some(js) = &mut self.js {
            match js.evaluate_with_timeout(expression, timeout) {
                Ok(val) => val,
                Err(e) => {
                    let preview: String = expression.chars().take(80).collect();
                    tracing::debug!("JS eval error/timeout for '{}': {}", preview, e);
                    serde_json::Value::Null
                }
            }
        } else {
            self.evaluate(expression)
        }
    }

    pub fn evaluate(&mut self, expression: &str) -> serde_json::Value {
        if let Some(js) = &mut self.js {
            match js.evaluate(expression) {
                Ok(val) => val,
                Err(e) => {
                    let preview: String = expression.chars().take(80).collect();
                    tracing::debug!("JS eval error for '{}': {}", preview, e);
                    serde_json::Value::Null
                }
            }
        } else {
            match expression.trim() {
                "document.title" => serde_json::Value::String(self.title.clone()),
                "document.URL" | "document.location.href" | "window.location.href" => {
                    serde_json::Value::String(self.url_string())
                }
                _ => serde_json::Value::Null,
            }
        }
    }

    pub async fn evaluate_for_cdp(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
    ) -> crate::diting_js::runtime::RemoteObjectInfo {
        if let Some(js) = &mut self.js {
            match js.evaluate_for_cdp(expression, return_by_value, await_promise).await {
                Ok(info) => info,
                Err(e) => {
                    // Bug #24 diagnosis aid: an erroring eval previously surfaced
                    // as a silent `null` at the HTTP layer (and only a debug log
                    // here), which is indistinguishable from a JS null return.
                    // warn! so a wedged/degraded runtime is visible in logs.
                    let preview: String = expression.chars().take(120).collect();
                    tracing::warn!("evaluate_for_cdp error for '{}': {}", preview, e);
                    crate::diting_js::runtime::RemoteObjectInfo {
                        js_type: "undefined".into(),
                        subtype: None,
                        class_name: String::new(),
                        description: String::new(),
                        object_id: None,
                        value: None,
                    }
                }
            }
        } else {
            let val = self.evaluate(expression);
            crate::diting_js::runtime::RemoteObjectInfo {
                js_type: match &val {
                    serde_json::Value::String(_) => "string".into(),
                    serde_json::Value::Number(_) => "number".into(),
                    serde_json::Value::Bool(_) => "boolean".into(),
                    _ => "undefined".into(),
                },
                subtype: None,
                class_name: String::new(),
                description: String::new(),
                object_id: None,
                value: Some(val),
            }
        }
    }

    #[allow(dead_code)] // CDP Runtime.callFunctionOn parity; our eval path goes through evaluate_for_cdp
    pub async fn call_function_on_for_cdp(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        args: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
    ) -> crate::diting_js::runtime::RemoteObjectInfo {
        if let Some(js) = &mut self.js {
            match js.call_function_on_for_cdp(function_declaration, object_id, args, return_by_value, await_promise).await {
                Ok(info) => info,
                Err(e) => {
                    tracing::debug!("callFunctionOn error: {}", e);
                    crate::diting_js::runtime::RemoteObjectInfo {
                        js_type: "undefined".into(),
                        subtype: None,
                        class_name: String::new(),
                        description: String::new(),
                        object_id: None,
                        value: None,
                    }
                }
            }
        } else {
            crate::diting_js::runtime::RemoteObjectInfo {
                js_type: "undefined".into(),
                subtype: None,
                class_name: String::new(),
                description: String::new(),
                object_id: None,
                value: None,
            }
        }
    }

    /// Exception-preserving variant of [`evaluate_for_cdp`]: a thrown/rejected
    /// expression comes back as an `EvalOutcome` exception so the CDP layer can
    /// emit `Runtime.exceptionThrown` + `exceptionDetails` instead of collapsing
    /// the throw to `undefined`.
    pub async fn evaluate_for_cdp_outcome(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
        await_budget_ms: u64,
        frame_nid: Option<u32>,
    ) -> crate::diting_js::runtime::EvalOutcome {
        if let Some(js) = &mut self.js {
            match js
                .evaluate_for_cdp_outcome(
                    expression,
                    return_by_value,
                    await_promise,
                    await_budget_ms,
                    frame_nid,
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    let preview: String = expression.chars().take(120).collect();
                    tracing::warn!("evaluate_for_cdp error for '{}': {}", preview, e);
                    // The outcome variant exists to NOT collapse failures into
                    // a silent undefined (that's the plain variant's job) — a
                    // watchdog timeout folded to `value: null` here made an
                    // eval overrun indistinguishable from a genuine null
                    // return. Surface it as an exception instead.
                    crate::diting_js::runtime::EvalOutcome {
                        info: crate::diting_js::runtime::RemoteObjectInfo {
                            js_type: "undefined".into(),
                            subtype: None,
                            class_name: String::new(),
                            description: String::new(),
                            object_id: None,
                            value: None,
                        },
                        exception: Some(crate::diting_js::runtime::ExceptionInfo {
                            text: "Uncaught".into(),
                            description: e,
                            class_name: String::new(),
                            object_id: None,
                            stack_first: None,
                            line: None,
                            col: None,
                        }),
                    }
                }
            }
        } else {
            let val = self.evaluate(expression);
            crate::diting_js::runtime::EvalOutcome {
                info: crate::diting_js::runtime::RemoteObjectInfo {
                    js_type: match &val {
                        serde_json::Value::String(_) => "string".into(),
                        serde_json::Value::Number(_) => "number".into(),
                        serde_json::Value::Bool(_) => "boolean".into(),
                        _ => "undefined".into(),
                    },
                    subtype: None,
                    class_name: String::new(),
                    description: String::new(),
                    object_id: None,
                    value: Some(val),
                },
                exception: None,
            }
        }
    }

    /// Exception-preserving variant of [`call_function_on_for_cdp`].
    pub async fn call_function_on_for_cdp_outcome(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        args: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
        frame_nid: Option<u32>,
    ) -> crate::diting_js::runtime::EvalOutcome {
        if let Some(js) = &mut self.js {
            match js
                .call_function_on_for_cdp_outcome(
                    function_declaration,
                    object_id,
                    args,
                    return_by_value,
                    await_promise,
                    frame_nid,
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    tracing::debug!("callFunctionOn error: {}", e);
                    crate::diting_js::runtime::EvalOutcome {
                        info: crate::diting_js::runtime::RemoteObjectInfo {
                            js_type: "undefined".into(),
                            subtype: None,
                            class_name: String::new(),
                            description: String::new(),
                            object_id: None,
                            value: None,
                        },
                        exception: Some(crate::diting_js::runtime::ExceptionInfo {
                            text: "Uncaught".into(),
                            description: e,
                            class_name: String::new(),
                            object_id: None,
                            stack_first: None,
                            line: None,
                            col: None,
                        }),
                    }
                }
            }
        } else {
            crate::diting_js::runtime::EvalOutcome {
                info: crate::diting_js::runtime::RemoteObjectInfo {
                    js_type: "undefined".into(),
                    subtype: None,
                    class_name: String::new(),
                    description: String::new(),
                    object_id: None,
                    value: None,
                },
                exception: None,
            }
        }
    }

    #[allow(dead_code)] // CDP Runtime.releaseObject parity — object-store ids are never handed out
    pub fn release_object(&mut self, object_id: &str) {
        if let Some(js) = &mut self.js {
            js.release_object(object_id);
        }
    }

    #[allow(dead_code)] // CDP Runtime.releaseObjectGroup parity
    pub fn release_object_group(&mut self) {
        if let Some(js) = &mut self.js {
            js.release_object_group();
        }
    }
}
