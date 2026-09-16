# Security Policy — aginxbrowser

aginxbrowser is a single-binary Rust browser engine and MCP/HTTP service. By
design it fetches and executes content from arbitrary, untrusted origins, so
the trust boundary it must hold is: **page content is data, never
instructions** — for the engine, for the host process, and for the agent
consuming tool output.

## Supported versions

Security fixes land on `main` and ship in the next tagged release. Only the
latest release line receives backports.

## Reporting a vulnerability

Use GitHub private vulnerability reporting:
[Report a vulnerability](https://github.com/yinnho/aginxbrowser/security/advisories/new).

Please do not open public issues for suspected vulnerabilities. We aim to
respond within 72 hours. A useful report includes: the binary version
(`aginxbrowser --version` / `GET /health` build commit), the feature flags it
was built with, and a minimal repro (URL or HTTP request pair).

## In-scope examples

- SSRF / private-network reachability from page-initiated requests
- Sandbox escape via page JavaScript (engine ops, Worker, dynamic import)
- Credential handling: cookie jars, `apiKey`, seeded sessions
- Prompt-injection payloads surviving the fetch/search sanitization layer
- Denial of service from untrusted page content (unbounded buffers, layout
  bombs)

## Out of scope

- Crawling or fingerprint-detection mechanics that a site deliberately
  employs against automated access — we do not treat detection itself as a
  vulnerability, and we do not accept reports requesting captcha bypass.
- Vulnerabilities in dependencies — report upstream; we track and upgrade via
  Dependabot.
