# AginxBrowser Architecture

> **Status: authoritative.** This file legislates the layering of the codebase.
> A change that moves code across a layer boundary updates this file first — in
> the same commit — or it does not land.
>
> Audit basis: 2026-10-19, v0.6.0, commit f3bcf08 (god-file baselines + bootstrap
> `crypto_subtle` split). Prior program note: the 2026-09 upgrade (P0–P3) is
> largely executed inside the product crate; the four-crate *product* end state
> in §4 (core / authoring / thin bin) remains aspirational and **soft-deferred**
> (see §6). Engine workspace already ships `diting` + companion crates
> `diting_ops` / `diting_runtime`. God-file ratchet: 1500 **production** lines
> (`scripts/audit_layers.py`); `#[cfg(test)]` items and `tests.rs` do not count —
> do not split colocated tests (or production modules) just to dodge the cap.

## 1. What AginxBrowser is

A server-side browser engine with a built-in V8, exposed as an information-acquisition
layer for agents: pages are fetched, rendered, searched, watched, driven, and re-emitted
as data or artifacts — with no Chromium. The product thesis is one sentence:

> **The agent's eyes, hands, and mouth — on an engine we own, behind an HTTP door.**

- **Eyes** — acquire: fetch (tiered render pipeline), metasearch, download, cache.
- **Hands** — interact: sessions, flows, accounts/personas, recording & replay.
- **Mouth** — author: screenshots, PDF, video, PPTX/DOCX, markdown documents.
- **Foundation** — the Diting (谛听) engine: DOM, CSS, layout, paint, fonts, JS runtime,
  network, page orchestration.
- **Door** — the protocol face: HTTP (REST). Assembly also includes the CLI / `doctor`
  entrypoints. MCP and CDP faces were removed (`439cbb7`); they are not part of the
  current product surface and must not be reintroduced casually.

## 2. Organizing principle: architecture is the directory

A reader who understands the file listing understands the system. Every module carries
exactly one responsibility and says so in one line. Boundaries between modules are typed
contracts (values, enums, errors crossing the line), not conventions.

The dependency direction is fixed and one-way:

```
        ┌──────────────────────────────────────────────┐
        │  faces: HTTP (routers) · CLI / doctor          │  door — no business logic
        └───────────────┬──────────────────────────────┘
                        ▼
        ┌───────────────────────────────┐   ┌──────────────────────┐
        │  product (eyes + hands +      │──▶│  authoring cluster    │
        │  mouth, still one crate):     │   │  (same crate today):  │
        │  sessions, flows, accounts,   │   │  pdf, video, ooxml,   │
        │  fetch, search, cache, guard  │   │  docgen, page pump    │
        └───────────────┬───────────────┘   └──────────┬───────────┘
                        ▼                              │
        ┌─────────────────────────────────────────────┴──────────┐
        │  diting: the engine — net · dom · css · layout · paint  │
        │  fonts · js runtime (+ bootstrap.js) · browser (Page)   │
        └─────────────────────────────────────────────────────────┘
```

Rules (enforced in review, checked in CI by `scripts/audit_layers.py` — its
exception table is empty):

- **R1 — Downward only.** `faces → product → diting`. Nothing imports faces.
  Nothing in diting imports product/faces.
- **R2 — Engine purity.** The `diting` crate is a browser engine, self-contained.
  Its only allowed product-facing surface is the `Page`-level API. The former
  `screenshot_reference` / blitz cross-check lives in the product tree, not in
  the engine (R2b clean).
- **R3 — Faces are thin.** Faces hold parameter structs, routing, and wire-compat
  shims (`routers/`, `firecrawl_compat`, `doctor_cli`, thin `main.rs`). Business
  logic lives in product modules. If a face grows an `if` about page behavior,
  the logic moves down.
- **R4 — Contracts are types.** Anything crossing a boundary is a named type
  (`SessionCommand`, `FetchRequest`, `SessionClickResponse`, …) with docs. Ad-hoc
  JSON shapes do not cross layers (flow DSL `Value` steps are the deliberate
  exception — validated at run time).
- **R5 — Features stay few.** Exactly three build gates exist today — `stealth`,
  `screenshot`, `blitz-reference` — each with a written reason in `Cargo.toml`.
  New features require an entry here justifying the compile-time cost.

## 3. Where the code is today (audit)

### 3.1 Already right

- **Engine/product boundary is real and CI-backed.** R1a/R1b/R2a/R2b/R3/G all
  green; `R1B_EXCEPTIONS` empty. Engine never names product modules.
- **HTTP is the only protocol face.** MCP and CDP modules are gone; faces are
  `src/routers/`, `firecrawl_compat`, `doctor_cli`, and thin `main.rs`.
- **Session is modular.** `src/session/{manager,commands,interact,record,state,
  screenshot,preload_recipes}` — the old five-job monolith is repaid.
- **Page is decomposed.** `crates/diting/.../diting_browser/page/{mod,navigate,
  eval,net,scripts,viewport}` (+ colocated `tests.rs`). Live rich-eval API is
  `evaluate_remote*` / `call_function_on_remote*` (CDP-era names retired).
- **Search split by domain.** `src/search/` orchestration + per-engine files +
  `captcha` / `obs` / `validate` / `dual_path` (`99eb67c`) — split when domains
  diverge, not when line counts scare CI.
- **Acquire façade split by job.** `src/server/{browser,fetch,outputs,search}.rs`
  behind a thin `server/mod.rs` re-export — browser/jar factory, challenge +
  do_fetch/click/eval, screenshot/video/pdf glue, search orchestration.
- **Feature surface is small and documented** (three gates, none accidental).
- **Tests are colocated.** Prefer keeping large `tests.rs` next to the code they
  pin; the god-file ratchet counts **production** lines only (`5f3cbed`).
- **Workspace started.** Engine packages: `crates/diting`, `crates/diting_ops`,
  `crates/diting_runtime`; the product binary remains `aginxbrowser` at the
  workspace root. Further *product* splits (`core` / `authoring` / thin bin)
  are the §4 *end state* — soft-deferred.

### 3.2 The four debts — status

| # | Debt | Status (2026-10-13) |
|---|------|---------------------|
| D1 | **Page was a god object.** | **Mostly repaid.** Live code lives under `page/`; large colocated tests remain. Rich-eval API renamed to `evaluate_remote*`. |
| D2 | **session.rs carried five jobs.** | **Repaid.** Tree under `src/session/`; interact holds the observe/act contract. |
| D3 | **Three-face tax (HTTP + MCP + CDP).** | **Eliminated** by removing MCP/CDP. Residual: `routers/sessions.rs` is a wide but thin catalog of handlers — optional navigation split only if handler count keeps growing. |
| D4 | **Authoring compiled into the acquisition server.** | **Still true as lines-in-tree** (`pages` / `video` / `ooxml` / `pptx` / `docgen`). Weaker as day-to-day pain now that CDP-only builds are gone. Prefer an `authoring` **feature gate** (R5) if device/musl size or CI time bites; full crate extract is not the next batch. |

**Active product cohesion watch (not a 2026-09 D-row):** `src/server/` was
split by job (`browser` / `fetch` / `outputs` / `search`); the old grandfather
entry on `server/mod.rs` is retired. `src/flow/mod.rs` (~1486 prod) is one
interpreter near the cap; do not carve it for the ratchet alone.

**`screenshot.rs`:** ~600 production / ~900 test lines. Eye-side HTML→PNG path;
domain already distinct from `session/screenshot`, `pages`, and
`screenshot_reference`. **Keep monolithic.** Live band paint and re-parse paths
must stay separate (different damage signatures / caches).

## 4. Target layout — Cargo workspace

> **Progress (2026-10):** Engine workspace members are
> `crates/diting`, `crates/diting_ops`, and `crates/diting_runtime`
> (`Cargo.toml` `[workspace].members`). `diting` owns the page pipeline;
> `diting_ops` is the `#[diting_op]` glue generator; `diting_runtime` is the
> owned JS event-loop / op-settlement layer (deno_core retirement campaign).
> The diagram below still shows the *product* end state (`core` / `authoring` /
> thin bin). After MCP/CDP removal, extracting those product crates is
> **soft-deferred**: highest ROI is keeping legislation true, domain-splitting
> `server/` when edited, and carving engine seams only when a change already
> forces one. Revisit full product crate packaging if a second binary/face or
> external library consumers appear. Prefer feature-gating authoring (D4
> outcome) over workspace surgery when binary size matters.

```
aginxbrowser/                    # workspace root
├── ARCHITECTURE.md              # this file
├── Cargo.toml                   # [workspace] members + shared release profile
├── crates/
│   ├── diting/                  # the engine (foundation) — EXISTS
│   ├── diting_ops/              # #[diting_op] glue (engine companion) — EXISTS
│   ├── diting_runtime/          # owned JS runtime / settle loop — EXISTS
│   ├── core/                    # aginxbrowser-core: eyes + hands — deferred
│   ├── authoring/               # aginxbrowser-authoring: mouth — deferred
│   └── aginxbrowser/            # the binary + faces — deferred (bin still at root)
├── js/                          # bootstrap.js lives under crates/diting/js today
├── workflow/ · web/ · docs/ · ...
```

### Crate responsibilities (one line each)

| Crate | Package | Responsibility | Status |
|-------|---------|----------------|--------|
| diting | `diting` | Render and drive a page: network → DOM → CSS → layout → paint; V8 runtime; the `Page` API. | **Shipped** |
| diting_ops | `diting_ops` | `#[diting_op]` op-glue generator (replacing `#[op2]` family by family). | **Shipped** (engine companion) |
| diting_runtime | `diting_runtime` | Owned JS event loop, op-settlement pipeline, promise table (deno_core retirement). | **Shipped** (engine companion) |
| core | `aginxbrowser-core` | Everything an agent does with a page short of authoring: sessions & interaction contracts, flows, accounts, fetch/search/download/cache, robots/rate/sanitize governance. | Deferred (code lives in root `src/` today) |
| authoring | `aginxbrowser-authoring` | Turn pages and documents into artifacts: PDF/PNG page pump, video, PPTX/DOCX, docgen. | Deferred (same crate; feature-gate is the cheaper D4 lever) |
| aginxbrowser | `aginxbrowser` (bin) | Door: HTTP routers, CLI, doctor — assembly and parameter structs only. | Bin at workspace root today |

### Module map (actual homes today → deferred crate)

| Today | Home now | Deferred crate (if P1 resumes) |
|---|---|---|
| `crates/diting` (`diting_*` + `js/bootstrap.js` + companions) | diting (+ `diting_ops` / `diting_runtime`) | unchanged |
| `screenshot_reference` / bridge cross-check | product (`src/`) | stays out of engine |
| `session/` | product | core `session/` |
| `server/` `search/` `download` `store` `robots` `rate` `sanitize` `har` `captcha` `curl_import` `cookie` `verdict` `risk_markers` | product | core `acquire/` / `governance/` |
| `account` `flow/` `browser` (product Page adapter) | product | core `interact/` |
| `screenshot.rs` `render.rs` | product | core `render/` |
| `pages` `video` `ooxml` `pptx_native` `docgen/` | product | authoring |
| `firecrawl_compat` | face | bin `compat/` |
| `main.rs` + `routers/{acquisition,sessions,outputs,render}` | face | bin |
| `config` `error` `doctor_cli` | split today | config/error → core; doctor → bin `cli/` |

MCP / CDP rows are intentionally absent — those faces do not exist.

### 4.1 Workflow assets (deployable data, not crates)

`workflow/<name>/` is the delivery unit for platform policy: flows (`flow.json`)
and opt-in document-start recipes (`recipe.js` included from
`src/session/preload_recipes.rs`). The engine stays generic Web/compat — no
per-host auto-mount, no new platform hardcoding in crates. Edit JS under
`workflow/`; rebuild only when a recipe body changes.

**Known clones — do not extract yet** (shared eval blobs are intentional;
wait for a green publish witness before collapsing):

| Cluster | Why it stays duplicated |
|---------|-------------------------|
| `taobao-publish` / `tmall-publish` | one fill/submit chain each (merged 2026-10-21 from four per-SKU-mode clones); they differ in publish domain, title/brand shapes and the Tmall gate, so a shared recipe extract would risk a green→red publish |
| `doudian-upload` per-image eval steps (~35 near-copies) | chunked upload died on #240 face-2; one-step-per-image is the stable shape until a safer compaction lands |

Safe recent extractions (recipes): `xhs-sign`, `xhs-qrcode`, `x-p256`,
`x-bearer`, `x-auth-ctx`, `taobao-block-everyhelp`, `taobao-block-qn-header`.
Details and mount shapes: `workflow/README.md`.

Vendor *signals* (e.g. `risk_markers`, verdict rule overlays) may live in
product core so agents can detect walls without loading a flow. Vendor
*actions* (sign, publish, upload) stay in `workflow/`.

## 5. Contracts at the boundaries

Existing typed contracts: `Page` (engine), `SessionCommand` / `Session*Response`
(session actor), HTTP REST request/response structs in `routers/`.

**Interaction contract (P3) — largely landed** (issues #45 / #46):

- `session_state` (observe) emits per-element index, identity, and a lightweight
  semantic signature (tag/id/name/value/checked/disabled/visibility) plus a
  page-level fingerprint.
- `session_click` / `session_input` (act) verify before mutating: element
  enabled, visible, not occluded (center-point `elementFromPoint` containment).
- Failure is structured, never a false success: `covered_by`, `disabled`,
  `not_visible` — a caller can distinguish "acted, no effect" from "refused,
  here is why". (`identity_changed` remains a soft/residual reason name if
  tightened later; do not treat §5 as unfinished work for the shipped reasons.)
- `Element.checkVisibility` is a real implementation in `bootstrap.js`
  (display / visibility / opacity chains; #45 tests in the JS runtime suite).

## 6. Migration program — status

| Phase | Deliverable | Status |
|-------|-------------|--------|
| **P0** | This document (legislation). | **Done**; this revision restores fidelity after MCP/CDP removal and in-crate P2/P3 landings. |
| **P1** | Workspace split into four crates; behavior byte-identical. | **~diting only.** Soft-deferred as a program; see §4. Prefer authoring feature-gate over full extract when D4 bites. |
| **P2** | Internal modularization + layering audit in CI. | **Mostly done:** `session/`, `routers/`, `page/`, `search/` domain split, `audit_layers.py` (prod-line god ratchet). Remaining: engine layout/css/ops carved only on touch (`server/` job split done). |
| **P3** | Interaction contract (§5); closes #45/#46. | **Mostly done** in code and tests. Docs no longer list it as future work. |

Each future batch still ships small, independently revertible, and green on the
full suite. **Explicit non-goals this quarter:** split `screenshot.rs` for line
count; extract taobao-publish / doudian-upload clones without witnesses;
reintroduce MCP/CDP; host auto-mount of recipes.

## 7. Non-negotiable migration discipline

1. **Wire freeze.** HTTP protocol behavior stays byte-identical through any
   structural move. Any wire-visible change is its own feature batch, never
   smuggled into a move. (MCP/CDP are gone — there is no multi-face wire tax.)
2. **Moves ≠ changes.** Pure code moves (rename/re-path) and logic changes never
   share a batch. A moved file that needed an edit gets the edit in the next batch.
3. **Every batch green.** `cargo test --features screenshot` full-suite pass
   (count may only grow vs the last recorded CI baseline) plus clippy with no
   new warnings in the touched hunks.
4. **No bulk rustfmt.** History readability outweighs formatting uniformity;
   formatting only ever touches lines a batch already edits.
5. **Coarse to fine.** Crates split before their internals do; no crate is born
   pre-shattered. Soft-deferral of P1 does not license random micro-splits.
6. **Feature gates travel with their code** and keep their `Cargo.toml`
   justification comments verbatim.
7. **Split for domains, not caps.** The 1500-production-line ratchet stops
   silent growth; it is not a mandate to shatter a cohesive module that sits
   three lines under or over the number.

## Appendix A — line-count snapshot (2026-10-19, f3bcf08)

Counts are **production lines** from `scripts/audit_layers.py` unless noted
(total = all lines including tests).

**Faces:** `main.rs` ~925 prod · `routers/sessions.rs` ~1236 ·
`routers/acquisition.rs` ~669 · `routers/outputs.rs` ~404.

**Product (selected):** `server/{browser,fetch,outputs,search}.rs` (façade) · `flow/mod.rs` 1486 ·
`session/interact.rs` 1348 · `session/manager/actor.rs` 1148 · `search/mod.rs`
1025 · `screenshot.rs` 600 prod / 1497 total · `video.rs` 887 prod ·
`pages.rs` 673 prod · `render.rs` 482 prod.

**Engine (grandfathered / large):** `js/bootstrap.js` 17447 (post-`crypto_subtle`
split; companion `js/crypto_subtle.js` 778) · `diting_layout/mod.rs` 9306 ·
`diting_css/mod.rs` 6627 · `diting_js/ops/mod.rs` 4703 ·
`diting_layout/paint.rs` 2739 · `diting_layout/fork_deltas.rs` 2257 ·
`diting_js/runtime.rs` 1678 · `diting_browser/page/mod.rs` 1127 ·
`page/navigate.rs` 911 · `page/scripts.rs` 762 · `page/tests.rs` 3557
(tests; excluded from ratchet).

**Workflow (deployable data):** ~44 workflow dirs · 36 `flow.json` · 7 `recipe.js`.

**Test baseline:** historical 2026-09 figure was 1453 passed (`--features
screenshot`). Attribute density on HEAD is higher (`#[test]` / `#[tokio::test]`
well above that); treat CI’s latest full-suite run as the source of truth rather
than freezing a stale number here.

**Version:** crate `0.6.0`.
