# 3D Modeler — architecture diagrams (LikeC4)

LikeC4 sources for the **3D Modeler** workspace (Box3D physics engine + Rust modeler under `3dmodeler/`).

## Preview

From a machine that has the [LikeC4](https://likec4.dev) CLI (or the likec4 monorepo):

```bash
# if likec4 is installed globally / via npx
npx likec4 serve /run/media/bart/Development/dev/bartbeecoders/3dModeler/architecture

# from the likec4-ai monorepo
./scripts/start.sh --no-mcp /run/media/bart/Development/dev/bartbeecoders/3dModeler/architecture
# or with Node 22+:
node packages/likec4/bin/likec4.mjs serve /run/media/bart/Development/dev/bartbeecoders/3dModeler/architecture
```

## Views

| View id | Description |
|---------|-------------|
| `index` | Landscape: people + externals |
| `workspace` | Box3D engine vs 3D Modeler product |
| `box3d` | Physics library internals + samples/tests |
| `modeler` | Rust crates under `3dmodeler/` |
| `app-internals` | modeler-app modules |
| `physics-mirror` | Scene ↔ box3d dual mode |
| `agent-control` | MCP + AI chat share command executor |
| `deploy` | Native / WASM / static hosting |
| `mcp-command` | Dynamic: agent → MCP → HTTP → scene |
| `physics-play` | Dynamic: play / step / stop |

## Files

- `likec4.config.json` — project `3d-modeler`
- `spec.c4` — element kinds and tags
- `model.c4` — systems, packages, relationships
- `views.c4` — diagrams

Sources of truth for facts: `README.md`, `3dmodeler/README.md`, `3dmodeler/plan.md`, crate READMEs/`Cargo.toml`, `3dmodeler/docs/mcp.md`.
