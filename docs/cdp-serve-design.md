# Design note: CDP `serve` for multi-engine distribution

> Tracked follow-up after HTTP snapshot + agent shim. **Not implemented.** Goal: unlock `agent-browser --engine aginx` the same way Obscura/Lightpanda attach today.

## Why

`vercel-labs/agent-browser` engine adapters (see `cli/src/native/cdp/obscura.rs`) do:

1. Resolve binary (`which obscura` or `--executable-path`)
2. `obscura serve --host 127.0.0.1 --port <ephemeral>`
3. Poll CDP discovery for `webSocketDebuggerUrl`
4. Drive all verbs (snapshot, click, fill, screenshot) over the shared CDP client

AginxBrowser already has internal CDP-shaped types and network event comments in `diting_browser`, but **no public CDP listener**. HTTP `/snapshot` + `ref` is the agent-native surface we ship first; CDP is the distribution surface for agent-browser and Playwright-shaped tools.

## Proposed CLI face

```bash
aginxbrowser serve --cdp --host 127.0.0.1 --port 0
# prints or exposes /json/version with webSocketDebuggerUrl
```

Minimum domains for agent-browser smoke (mirror Obscura E2E scope, not Chrome parity):

| Domain | Methods (v1) |
|--------|----------------|
| Target / Browser | createTarget, getTargets, close |
| Page | navigate, enable, loadEventFired, bringToFront |
| Runtime | enable, evaluate, callFunctionOn (enough for fill/click helpers) |
| DOM / Accessibility | enough for snapshot tree **or** map Aginx `/snapshot` into CDP AX nodes |
| Input | dispatchMouseEvent / dispatchKeyEvent **or** bridge to existing interact |
| Network | optional in v1 |

Honest non-goals for v1: extensions, headed UI, full Chrome AX fidelity, MCP.

## Mapping to existing HTTP

Prefer **bridging** CDP commands onto the session manager that already backs HTTP:

- `Page.navigate` → session navigate
- Accessibility.getFullAXTree (or agent-browser's snapshot helper) → reuse `session::snapshot` element map, assign backendNodeId ↔ `eN` index
- Input click/type → existing click/input by index
- `Runtime.evaluate` → session eval

One process can keep serving HTTP on `:8089` and CDP on another port so current agents do not break.

## Upstream PR plan (after serve exists)

1. Fork `vercel-labs/agent-browser`
2. Add `Engine::Aginx` beside Obscura/Lightpanda
3. Launch: `aginxbrowser serve --cdp --port …` (flags TBD to match Obscura's discovery)
4. Docs page under `engines/aginx` + schema enum
5. E2E: open → snapshot → click → close on a loopback fixture

**Until step 0 (CDP serve) lands, do not open the upstream PR** — it would be a stub with no runtime. Status line for public docs: *upstream PR pending CDP*.

## Acceptance for "done"

- [ ] `aginxbrowser serve --cdp` advertises a discoverable WebSocket
- [ ] agent-browser (patched or upstream) can `open` + `snapshot` against it
- [ ] Layering audit / `cargo test` stay green
- [ ] Document gaps vs Chrome (same honesty as Obscura engine page)

## References

- Obscura adapter: https://github.com/vercel-labs/agent-browser/blob/main/cli/src/native/cdp/obscura.rs
- Obscura engine docs: https://agent-browser.dev/engines/obscura
- Aginx HTTP agent path: [`agent-browser-engine.md`](agent-browser-engine.md)
