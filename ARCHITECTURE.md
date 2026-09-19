# AginxBrowser Architecture

> **Status: authoritative.** This file legislates the layering of the codebase and the
> 2026-09 architecture upgrade program (phases P0–P3). A change that moves code across a
> layer boundary updates this file first — in the same commit — or it does not land.
>
> Audit basis: 2026-09-19, v0.4.5, commit 5718f2b. Line counts in Appendix A.

## 1. What AginxBrowser is

A server-side browser engine with a built-in V8, exposed as an information-acquisition
layer for agents: pages are fetched, rendered, searched, watched, driven, and re-emitted
as data or artifacts — with no Chromium. The product thesis is one sentence:

> **The agent's eyes, hands, and mouth — on an engine we own, behind protocol doors.**

- **Eyes** — acquire: fetch (tiered render pipeline), metasearch, download, cache.
- **Hands** — interact: sessions, flows, accounts/personas, recording & replay.
- **Mouth** — author: screenshots, PDF, video, PPTX/DOCX, markdown documents.
- **Foundation** — the Diting (谛听) engine: DOM, CSS, layout, paint, fonts, JS runtime,
  network, page orchestration.
- **Doors** — the protocol faces: HTTP (REST), MCP (stdio + streamable HTTP), CDP
  (WebSocket, Playwright/Puppeteer-compatible).

## 2. Organizing principle: architecture is the directory

A reader who understands the file listing understands the system. Every module carries
exactly one responsibility and says so in one line. Boundaries between modules are typed
contracts (values, enums, errors crossing the line), not conventions.

The dependency direction is fixed and one-way:

```
        ┌──────────────────────────────────────────────┐
        │  faces: HTTP · MCP · CDP · CLI               │  doors — no business logic
        └───────────────┬──────────────────────────────┘
                        ▼
        ┌───────────────────────────────┐   ┌──────────────────────┐
        │  core: eyes + hands           │──▶│  authoring: mouth    │
        │  sessions, flows, accounts,   │   │  pdf, video, ooxml,  │
        │  fetch, search, cache, guard  │   │  docgen, page pump   │
        └───────────────┬───────────────┘   └──────────┬───────────┘
                        ▼                              │
        ┌─────────────────────────────────────────────┴──────────┐
        │  diting: the engine — net · dom · css · layout · paint  │
        │  fonts · js runtime (+ bootstrap.js) · browser (Page)   │
        └─────────────────────────────────────────────────────────┘
```

Rules (enforced in review, checked in CI by `scripts/audit_layers.py` — the
P2 layering audit; its exception table is empty):

- **R1 — Downward only.** `faces → core → diting`, `authoring → core → diting`.
  Nothing imports faces. Nothing in diting imports core/authoring/faces.
- **R2 — Engine purity.** The `diting` crate is a browser engine, self-contained.
  Its only allowed product-facing surface is the `Page`-level API. (Today's single
  exception — the `screenshot_reference` fork-delta cross-check — moves to a
  dev-dependency test path in P1 and stops being an exception.)
- **R3 — Faces are thin.** Faces hold parameter structs, routing, tool declarations,
  and wire-compat shims. Business logic lives in core. If a face grows an `if` about
  page behavior, the logic moves down.
- **R4 — Contracts are types.** Anything crossing a boundary is a named type
  (`SessionCommand`, `FetchRequest`, `SessionClickResponse`, …) with docs. Ad-hoc
  JSON shapes do not cross layers.
- **R5 — Features stay few.** Exactly three build gates exist today — `stealth`,
  `screenshot`, `blitz-reference` — each with a written reason in `Cargo.toml`.
  New features require an entry here justifying the compile-time cost.

## 3. Where the code is today (audit)

### 3.1 Already right

- **Engine/product boundary is real.** Across all `diting_*` modules there is exactly
  one import of product code (`diting_layout → screenshot_reference`, the dual-engine
  cross-check). The `diting_*` naming discipline held.
- **Feature surface is small and documented** (three gates, none accidental).
- **Tests are colocated and massive**: 1453 passing (baseline, `--features screenshot`),
  contract suites live beside the code they pin (e.g. `diting_js/runtime/tests.rs`,
  13.6k lines / 260 tests).
- **One crate, one binary** — currently `aginxbrowser` with no workspace.

### 3.2 The four debts this program repays

| # | Debt | Evidence |
|---|------|----------|
| D1 | **Page is a god object.** Navigation, eval, layout cache, interaction, network sniffing on one struct. | `diting_browser/page.rs` = 5,275 lines; 9 product files import `diting_browser` directly. |
| D2 | **session.rs carries five jobs.** Session actor, command enum, interaction JS, recording/replay, snapshot persistence (+ humanized drag). | `session.rs` = 5,423 lines; `session_thread` alone ≈ 900. |
| D3 | **The three-face tax.** Every capability is wired three times (HTTP route in `main.rs`, MCP tool in `mcp.rs`, CDP method), each face re-declaring parameters. | 52 inline routes in `main.rs` (2,726 lines), 37 tools in `mcp.rs` (2,125 lines). |
| D4 | **Authoring is compiled into the acquisition server.** | `docgen/` 12.4k + `video.rs` + `pptx_native.rs` + `ooxml.rs` + `pages.rs` ≈ 17k lines that CDP-only and device builds never execute. |

## 4. Target layout — Cargo workspace

```
aginxbrowser/                    # workspace root
├── ARCHITECTURE.md              # this file
├── Cargo.toml                   # [workspace] members + shared release profile
├── crates/
│   ├── diting/                  # the engine (foundation)
│   ├── core/                    # aginxbrowser-core: eyes + hands
│   ├── authoring/               # aginxbrowser-authoring: mouth (feature-gated)
│   └── aginxbrowser/            # the binary + faces (doors)
├── js/                          # bootstrap.js — part of diting, moves with it
├── flows/ · web/ · docs/ · ...
```

### Crate responsibilities (one line each)

| Crate | Package | Responsibility |
|-------|---------|----------------|
| diting | `diting` | Render and drive a page: network → DOM → CSS → layout → paint; V8 runtime; the `Page` API. |
| core | `aginxbrowser-core` | Everything an agent does with a page short of authoring: sessions & interaction contracts, flows, accounts, fetch/search/download/cache, robots/rate/sanitize governance. |
| authoring | `aginxbrowser-authoring` | Turn pages and documents into artifacts: PDF/PNG page pump, video, PPTX/DOCX, docgen. |
| aginxbrowser | `aginxbrowser` (bin) | Doors: HTTP routes, MCP tools, CDP mounting, CLI, doctor — assembly and parameter structs only. |

### Module → crate map (P1/P2 ledger)

| Today (src/) | → Crate | → Module inside |
|---|---|---|
| `diting_js` `diting_dom` `diting_css` `diting_layout` `diting_fonts(.rs)` `diting_net` `diting_browser` + `js/bootstrap.js` | diting | unchanged names |
| `diting_cdp` | aginxbrowser | `cdp/` — CDP is a face riding core/engine |
| `screenshot_reference.rs` | diting (dev-dep) | cross-check tests only, `blitz-reference` |
| `session.rs` | core | `session/{mod,manager,commands,interact,record,state}` |
| `server.rs` `search/` `download.rs` `store.rs` `robots.rs` `rate.rs` `sanitize.rs` `har.rs` `captcha.rs` `curl_import.rs` `cookie.rs` | core | `acquire/`, `governance/` |
| `account.rs` `flow.rs` `browser.rs` | core | `interact/` |
| `screenshot.rs` `render.rs` | core | `render/` (eyes: the /screenshot capability + band primitives) |
| `pages.rs` `video.rs` `ooxml.rs` `pptx_native.rs` `docgen/` | authoring | `pagepump/`, `video/`, `ooxml/`, `docgen/` |
| `firecrawl_compat.rs` | aginxbrowser | `compat/` (wire shim = face) |
| `main.rs` | aginxbrowser | `main.rs` (thin) + `routers/{acquisition,sessions,outputs}` + `cli/` |
| `mcp.rs` | aginxbrowser | `mcp/` |
| `config.rs` `error.rs` `doctor_cli.rs` | split | config/error → core; doctor → aginxbrowser `cli/` |

## 5. Contracts at the boundaries

Existing typed contracts: `Page` (engine), `SessionCommand`/`Session*Response` (session
actor), the three wire protocols (HTTP REST schema, MCP tool schemas, CDP methods).

**P3 adds the interaction contract** — observe/act with verifiable identity
(tracked in issues #45/#46):

- `session_state` (observe) emits, per element: index, identity, and a lightweight
  semantic signature (tag/id/name/value/checked/disabled/visibility) plus a page-level
  fingerprint.
- `session_click` / `session_input` (act) verify before mutating: identity unchanged,
  element enabled, visible, not occluded (center-point `elementFromPoint` containment).
- Failure is structured, never a false success: `covered_by`, `disabled`,
  `not_visible`, `identity_changed` — a caller can distinguish "acted, no effect"
  from "refused, here is why".
- `Element.checkVisibility` becomes a real implementation (display/visibility/opacity
  chains), retiring the always-true stub (#45).

## 6. Migration program

| Phase | Deliverable | Acceptance gate |
|-------|-------------|-----------------|
| **P0** | This document (legislation). | Reviewed and committed. |
| **P1** | Workspace split into the four crates; behavior byte-identical. | Full suite 1453+ green; feature-combination check (`stealth`/`screenshot`/`blitz-reference`, no-feature) green; release binary builds on the CI matrix; six distribution channels unaffected. |
| **P2** | Internal modularization inside crates: `session/` five-module split, `routers/`, `page.rs` decomposition; add the layering audit script (grep-level R1–R3 check in CI). | Same gates as P1 per batch; god files ≤ ~1,500 lines each. |
| **P3** | Interaction contract (§5) lands as typed core API; closes #45/#46. | New contract tests; agent-visible failures carry reasons; no false successes. |

Each phase ships as a series of small batches — every batch independently revertible,
every batch green on the full suite.

## 7. Non-negotiable migration discipline

1. **Wire freeze.** HTTP, MCP, and CDP protocol behavior stays byte-identical through
   P1/P2. Any wire-visible change is its own feature batch, never smuggled into a move.
2. **Moves ≠ changes.** Pure code moves (rename/re-path) and logic changes never share
   a batch. A moved file that needed an edit gets the edit in the next batch.
3. **Every batch green.** `cargo test --features screenshot` full-suite pass (1453
   baseline, count may only grow) plus clippy with no new warnings in the touched hunks.
4. **No bulk rustfmt.** History readability outweighs formatting uniformity; formatting
   only ever touches lines a batch already edits.
5. **Coarse to fine.** Crates split before their internals do; no crate is born
   pre-shattered.
6. **Feature gates travel with their code** and keep their `Cargo.toml` justification
   comments verbatim.

## Appendix A — line-count audit (2026-09-19, 5718f2b)

Tracked text lines ≈ 167k: `src/` 131,326 (112 files) · `js/` 15,683 · `docs/` 9,779 ·
`web/` 1,874 · `tests/` 242.

Engine (`diting_*`): js 24,924 (runtime tests 13,647 / 260 tests) · layout 24,079 ·
cdp 11,817 · css 8,521 · browser 5,869 (`page.rs` 5,275) · net 5,559 · dom 4,563 ·
fonts 446.

Product: session 5,423 · main 2,726 · server 2,194 · mcp 2,125 · store 1,734 ·
video 1,672 · screenshot 1,344 · pages 1,152 · flow 1,099 · pptx_native 1,034 ·
screenshot_reference 974 · har 870 · render 852 · download 692 · robots 675 ·
firecrawl_compat 651 · curl_import 558 · account 531 · ooxml 520 · captcha 413 ·
sanitize 386 · config 235 · rate 231 · browser 112 · cookie 60 · error 16 ·
`docgen/` 12,355 · `search/` 4,445.

Test baseline: 1453 passed + 1 ignored.
