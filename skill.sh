#!/usr/bin/env bash
#
# AginxBrowser skill installer.
#
# Wires up the trigger surface so your agent proactively reaches for
# AginxBrowser: SKILL.md tells the agent WHEN to call the HTTP API
# (fetch/search/screenshot/session on a local instance).
#
# Usage (download, review, then run - never blind-pipe from the network):
#   curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/skill.sh -o skill.sh
#   less skill.sh
#   bash skill.sh
#   ./skill.sh
#
# Instance not on this machine? Point the verification at it:
#   AGINXBROWSER_URL=http://your-host:8089 ./skill.sh
#
set -euo pipefail

URL="${AGINXBROWSER_URL:-http://127.0.0.1:8089}"
DOCTOR="$URL/doctor"
SKILL_DIR="${HOME}/.claude/skills/aginxbrowser"
SKILL_URL="https://raw.githubusercontent.com/yinnho/aginxbrowser/main/SKILL.md"

echo "==> AginxBrowser skill installer"
echo "    instance: $URL"
echo ""

# 1. SKILL.md - the trigger surface.
mkdir -p "$SKILL_DIR"
if curl -fsSL "$SKILL_URL" -o "$SKILL_DIR/SKILL.md"; then
  echo "  [ok] skill installed: $SKILL_DIR/SKILL.md"
else
  echo "  [fail] could not download SKILL.md from $SKILL_URL" >&2
  exit 1
fi

# 2. Verify the instance is alive and report capabilities.
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
    echo "  [warn] could not reach $DOCTOR (instance down, or not installed here)."
    echo "         install the server first: brew install yinnho/aginxbrowser/aginxbrowser"
    echo "         then start it: aginxbrowser"
  fi
fi

echo ""
echo "==> done."
echo "    tell your agent: \"use aginxbrowser to read / search / screenshot / interact with web pages\""
echo "    docs: https://github.com/yinnho/aginxbrowser/blob/main/docs/API.md"
