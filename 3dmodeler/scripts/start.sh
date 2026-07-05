#!/usr/bin/env bash
# Start the 3D modeler with its MCP integration ready.
#
# What this does:
#   1. builds modeler-app and modeler-mcp (release) — always, so you get the
#      latest code even if a binary already exists (cargo no-ops fast when
#      nothing changed)
#   2. starts the native modeler app (it hosts the control API on
#      localhost:${MODELER_CONTROL_PORT:-8323})
#   3. verifies the MCP bridge end-to-end (initialize -> get_scene)
#   4. prints how to register the server in Claude Code & friends
#
# Note on the MCP server: it speaks stdio, so your MCP client (Claude Code,
# Cursor, ...) SPAWNS modeler-mcp itself for each session — it must not be
# started standalone. This script therefore keeps the APP running (Ctrl+C
# stops it) and only smoke-tests the bridge.
#
# Usage:
#   scripts/start.sh            # release build (recommended)
#   scripts/start.sh --debug    # debug build
#   MODELER_CONTROL_PORT=9000 scripts/start.sh

set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=release
CARGO_FLAGS=(--release)
if [[ "${1:-}" == "--debug" ]]; then
    PROFILE=debug
    CARGO_FLAGS=()
fi

PORT="${MODELER_CONTROL_PORT:-8323}"
APP="target/$PROFILE/modeler-app"
MCP="target/$PROFILE/modeler-mcp"

# 1. always build (cargo no-ops fast if nothing changed; picks up any edits)
echo "building modeler-app + modeler-mcp ($PROFILE)…"
cargo build "${CARGO_FLAGS[@]}" -p modeler-app -p modeler-mcp

# 2. start the app
if curl -sf -o /dev/null -X POST --data '{"cmd":"get_scene"}' "http://127.0.0.1:$PORT/" 2>/dev/null; then
    echo "a modeler app is already running on port $PORT — using it (restart it yourself to pick up the freshly built binary)."
    APP_PID=""
else
    "$APP" &
    APP_PID=$!
    trap '[[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null; exit 0' INT TERM

    echo -n "waiting for the control API on port $PORT "
    for _ in $(seq 1 50); do
        if curl -sf -o /dev/null -X POST --data '{"cmd":"get_scene"}' "http://127.0.0.1:$PORT/" 2>/dev/null; then
            break
        fi
        echo -n "."
        sleep 0.2
    done
    echo
fi

# 3. smoke-test the MCP bridge (stdio round trip through modeler-mcp)
SMOKE=$(printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_scene","arguments":{}}}' \
    | MODELER_CONTROL_PORT="$PORT" "$MCP" | tail -1)
if echo "$SMOKE" | grep -q 'objects' && ! echo "$SMOKE" | grep -q '"isError":true'; then
    echo "✅ MCP bridge OK — modeler-mcp can reach the app."
else
    echo "❌ MCP bridge check failed: $SMOKE"
    [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null
    exit 1
fi

# 4. registration instructions
MCP_PATH="$(pwd)/$MCP"
cat <<EOF

3D modeler is running (control API on http://127.0.0.1:$PORT).

Register the MCP server in your agent (one-time):
  Claude Code:   claude mcp add modeler -- $MCP_PATH
  other clients: see docs/mcp.md (Claude Desktop, Cursor, Windsurf, VS Code)

The MCP client spawns modeler-mcp itself — just keep this app running.
EOF

if [[ -n "$APP_PID" ]]; then
    echo "Press Ctrl+C to stop the modeler."
    wait "$APP_PID"
fi
