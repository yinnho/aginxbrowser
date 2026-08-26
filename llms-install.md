# llms-install.md — Installing AginxBrowser

AginxBrowser is an agent-first browser server: one Rust binary exposing an HTTP API, a Firecrawl-compatible `/v1/scrape`, a CDP bridge, and native MCP (14 tools). No Chromium, no Docker.

## Option A — Hosted instance (zero install)

The public instance is already running. Register it as remote MCP over streamable HTTP:

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

For other MCP clients, point them at `https://browser.aginx.net/mcp` (transport: `http`, no auth required today).

Verify:

```bash
curl -sS https://browser.aginx.net/health
# → {"status":"ok","screenshot":true,"stealth":true,...}
```

## Option B — Self-host from a release binary

1. Download the archive for your platform from https://github.com/yinnho/aginxbrowser/releases/latest (`aginxbrowser-<os>-<arch>.tar.gz`), then:

```bash
tar xzf aginxbrowser-*.tar.gz && chmod +x aginxbrowser
```

2. Run in one of two modes:

```bash
./aginxbrowser          # HTTP server on 0.0.0.0:8089 (REST + CDP + /v1/scrape)
./aginxbrowser --mcp    # native MCP on stdio (for desktop clients)
```

3. Register with an MCP client:

```bash
# Remote-style registration against your own instance
claude mcp add aginxbrowser --transport http http://127.0.0.1:8089/mcp

# Or stdio directly (client spawns the process)
claude mcp add aginxbrowser -- /path/to/aginxbrowser --mcp
```

Verify:

```bash
curl -sS http://127.0.0.1:8089/health   # → {"status":"ok",...}
```

## Tools exposed

fetch, eval, click, search, download, screenshot plus 9 stateful-session tools (open/navigate/click/type/screenshot/close across persistent sessions). Full reference: [API.md](API.md).
