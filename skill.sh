#!/usr/bin/env bash
#
# AginxBrowser skill + MCP installer.
#
# Wires up BOTH halves so your agent proactively reaches for AginxBrowser:
#   1. SKILL.md  -> the trigger (tells the agent WHEN to use the tools)
#   2. MCP server -> the tools themselves (fetch/search/screenshot/session)
#
# Usage (download, review, then run - never blind-pipe from the network):
#   curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/skill.sh -o skill.sh
#   less skill.sh
#   bash skill.sh
#   ./skill.sh
#
# Self-hosted? Point it at your own instance:
#   AGINXBROWSER_MCP=https://your-host/mcp ./skill.sh
#
set -euo pipefail

MCP="${AGINXBROWSER_MCP:-https://browser.aginx.net/mcp}"
# /doctor lives at the host root, not under /mcp.
HOST_ROOT="$(printf '%s' "$MCP" | sed 's#/mcp$##; s#/$##')"
DOCTOR="$HOST_ROOT/doctor"
SKILL_DIR="${HOME}/.claude/skills/aginxbrowser"
SKILL_URL="https://raw.githubusercontent.com/yinnho/aginxbrowser/main/SKILL.md"

echo "==> AginxBrowser skill + MCP installer"
echo "    endpoint: $MCP"
echo ""

# 1. SKILL.md - the trigger surface.
mkdir -p "$SKILL_DIR"
if curl -fsSL "$SKILL_URL" -o "$SKILL_DIR/SKILL.md"; then
  echo "  [ok] skill installed: $SKILL_DIR/SKILL.md"
else
  echo "  [fail] could not download SKILL.md from $SKILL_URL" >&2
  exit 1
fi

# 2. MCP server - the tools.
if command -v claude >/dev/null 2>&1; then
  if claude mcp add aginxbrowser --transport http "$MCP" 2>/dev/null; then
    echo "  [ok] mcp registered: aginxbrowser -> $MCP"
  else
    echo "  [skip] 'claude mcp add' did not succeed (already registered, or needs an interactive shell)."
    echo "         verify with: claude mcp list"
  fi
else
  echo "  [skip] 'claude' CLI not on PATH."
  echo "         add the server via settings.json instead:"
  echo "           {\"mcpServers\":{\"aginxbrowser\":{\"type\":\"http\",\"url\":\"$MCP\"}}}"
fi

# 3. Verify the instance is alive and report capabilities.
echo ""
echo "==> verifying instance..."
if command -v curl >/dev/null 2>&1; then
  BODY="$(curl -fsS --max-time 15 "$DOCTOR" 2>/dev/null || true)"
  if [ -n "$BODY" ]; then
    if command -v python3 >/dev/null 2>&1; then
      printf '%s\n' "$BODY" | python3 -c 'import sys,json
d=json.load(sys.stdin)
print("  engine:      ",d.get("engine","?"))
print("  version:     ",d.get("version","?"))
c=d.get("capabilities",{})
print("  screenshot:  ",c.get("screenshot"))
print("  stealth:     ",c.get("stealth"))
' 2>/dev/null || echo "  $BODY"
    else
      echo "  $BODY"
    fi
  else
    echo "  [warn] could not reach $DOCTOR (instance down, or network blocked)."
    echo "         the skill is still installed; MCP will work once the endpoint is reachable."
  fi
fi

echo ""
echo "==> done."
echo "    tell your agent: \"use aginxbrowser to read / search / screenshot / interact with web pages\""
echo "    docs: https://github.com/yinnho/aginxbrowser/blob/main/docs/API.md"
echo "    ⭐ like it? star us: https://github.com/yinnho/aginxbrowser"
