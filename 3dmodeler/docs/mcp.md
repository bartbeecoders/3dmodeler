# MCP server — let coding agents drive the 3D modeler

`modeler-mcp` is an [MCP](https://modelcontextprotocol.io) (Model Context
Protocol) server that lets AI coding agents — Claude Code, Claude Desktop,
Cursor, Windsurf, and anything else that speaks MCP — inspect and edit the
scene of a **running** 3D modeler, control the physics simulation, and take
viewport screenshots so the agent can *see* what it is building.

```
agent (Claude Code, …)
  │  MCP over stdio (JSON-RPC)
  ▼
modeler-mcp  (this binary)
  │  HTTP on localhost:8323
  ▼
modeler-app  (the native app — commands run on the render loop
              against the live scene you're looking at)
```

## Quickstart

```bash
3dmodeler/scripts/start.sh
```

builds whatever is missing, starts the modeler, smoke-tests the MCP bridge
end-to-end and prints the registration command. Ctrl+C stops the app.
The manual steps below do the same thing piecewise.

## 1. Build the pieces

```bash
cd 3dmodeler
cargo build --release -p modeler-mcp     # the MCP server binary
cargo build --release -p modeler-app     # the modeler itself
```

Binaries land in `3dmodeler/target/release/`. Use absolute paths in the
configs below (shown as `<REPO>` = the absolute path to this repository).

## 2. Run the modeler

The MCP bridge talks to the **native** app (the browser build cannot host a
TCP server):

```bash
cargo run --release -p modeler-app
# prints: control API listening on http://127.0.0.1:8323 (for modeler-mcp)
```

Keep it running while the agent works — you'll see every change live. Set
`MODELER_CONTROL_PORT` (for both app and MCP server) to use another port.

The status bar (bottom right) shows the integration state: green
**MCP :8323** = control API listening, bright **MCP active** = agent commands
arriving right now, red **MCP off** = the port was already taken. Hover it
for the command count and the registration hint.

## 3. Register the MCP server

### Claude Code (CLI)

```bash
claude mcp add modeler -- <REPO>/3dmodeler/target/release/modeler-mcp
```

Scopes: add `--scope project` to share it via `.mcp.json` in the repo, or
`--scope user` to have it in every project. Check with `/mcp` inside a
session. Remove with `claude mcp remove modeler`.

Equivalent `.mcp.json` (project root, for `--scope project`):

```json
{
  "mcpServers": {
    "modeler": {
      "command": "<REPO>/3dmodeler/target/release/modeler-mcp",
      "env": {}
    }
  }
}
```

### Claude Desktop

Edit `~/.config/Claude/claude_desktop_config.json` (Linux) or
`~/Library/Application Support/Claude/claude_desktop_config.json` (macOS):

```json
{
  "mcpServers": {
    "modeler": {
      "command": "<REPO>/3dmodeler/target/release/modeler-mcp"
    }
  }
}
```

Restart Claude Desktop afterwards.

### Cursor

Settings → MCP → *Add new global MCP server*, or create `.cursor/mcp.json`
in the project:

```json
{
  "mcpServers": {
    "modeler": {
      "command": "<REPO>/3dmodeler/target/release/modeler-mcp"
    }
  }
}
```

### Windsurf

`~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "modeler": {
      "command": "<REPO>/3dmodeler/target/release/modeler-mcp"
    }
  }
}
```

### VS Code (native MCP / GitHub Copilot agent mode)

`.vscode/mcp.json` in the workspace:

```json
{
  "servers": {
    "modeler": {
      "type": "stdio",
      "command": "<REPO>/3dmodeler/target/release/modeler-mcp"
    }
  }
}
```

Other MCP clients follow the same pattern: a stdio server with no arguments;
optionally pass `MODELER_CONTROL_PORT` in `env`.

## 4. Tools

| Tool | What it does |
| --- | --- |
| `get_scene` | Full scene dump: objects (name, id, primitive, local & world transforms, parent, color, physics flags, dimensions in m), measurements, sim state |
| `screenshot` | Renders the viewport and returns a PNG **image** — the agent's eyes |
| `add_object` | Add `plane` / `cube` / `sphere` / `icosphere` / `cylinder` / `cone` / `torus` with optional name, location, rotation (Euler °), scale, color, physics & adornment flags |
| `update_object` | Change any of the above on an existing object (by name or id); `new_name` renames |
| `delete_object` | Remove an object (children stay in place, unparented) |
| `set_parent` | Link objects into a hierarchy (world placement preserved; cycles rejected); `parent: null` unparents |
| `add_measurement` | Persistent ruler between two points, returns the distance in meters |
| `simulate` | `play` / `pause` / `stop` the physics simulation (stop restores the scene) |
| `new_scene` | Reset to the default scene |

Conventions the agent should know (also sent in the server's MCP
`instructions`): units are **meters**, the world is **Z-up** (ground = XY
plane), rotations are XYZ Euler degrees, colors are `[r, g, b]` in 0–1.

## 5. Try it

With the app running and the server registered in Claude Code:

> Build a small table: four cylinder legs (0.05 m radius, 0.7 m tall) at the
> corners of a 1.2 × 0.8 m top, a thin cube as the top, parent everything to
> the top, then take a screenshot to check the proportions.

Useful prompt patterns:
- "take a screenshot" after edits — the agent sees the actual viewport
- "make the ball dynamic and play the simulation for a moment, then stop"
- "measure the distance between the top of X and the floor"

## 6. Troubleshooting

| Symptom | Fix |
| --- | --- |
| Tool calls fail with "The 3D modeler app is not running" | Start `cargo run --release -p modeler-app` (native, not the browser build) and keep it open |
| "timed out waiting for the app" | The app window is minimized on some systems the frame loop throttles — bring the window to the front |
| Port conflict / firewall | Set `MODELER_CONTROL_PORT=9000` for BOTH the app and the MCP server (via the client config's `env`) |
| Server not listed in the client | Use an absolute path to `modeler-mcp`; restart the client; check the client's MCP logs |
| Second app instance | Only the first instance binds the control port; the second runs without control (a message is printed) |

## 7. Protocol notes

`modeler-mcp` implements the MCP stdio transport by hand (newline-delimited
JSON-RPC 2.0; `initialize`, `tools/list`, `tools/call`, `ping`) — no async
runtime, ~300 lines. The HTTP hop is localhost-only. Undo integration comes
for free: agent edits land in the same undo stack as manual edits, so Ctrl+Z
in the app steps back through what the agent did.
