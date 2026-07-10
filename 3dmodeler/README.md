# 3D Modeler (Rust + WASM + box3d)

[![Build](https://github.com/bartbeecoders/3dmodeler/actions/workflows/build.yml/badge.svg)](https://github.com/bartbeecoders/3dmodeler/actions/workflows/build.yml)

**Version 0.2.1** — browser-hosted 3D modeler with Blender-style interaction.
See [plan.md](plan.md) for the roadmap and progress tracking.

## What's new in 0.2.0

Everything shipped across the 0.1.x series, now consolidated:

- **Lights** — point / sun / spot lights with color, intensity and shadows,
  plus Studio/Scene lighting modes.
- **Wall tool** — draw walls on the floor with door/window cutouts and drag
  handles on openings; break a wall into individual physics bricks and
  rebuild it back into one wall object.
- **Edit mode** (Tab) — vertex/edge/face editing with move/rotate/scale,
  vertex snapping, and setting a vertex/edge/face as pivot or anchor point.
- **Object library** — save selections as reusable assets and drag to place;
  placed objects behave as one group (Ungroup breaks them apart); full MCP
  CRUD.
- **Physics poke** — in physics mode, hold LMB to charge (up to 300%) and
  release to kick objects.
- **Pie menus** — Shift+A Add menu and the right-click object menu are
  pie/wheel menus, with pictograms in the Add menu.
- **UI themes** — color theme picker (Dark/Light/Ocean and more) with accent
  colors in the View menu.
- **Outliner upgrades** — folders (bricks land in their own folder),
  click-to-select, drag-to-parent.
- **Reference images** — load onto axis planes, two-point scale calibration,
  selectable and movable in the viewport.
- **Viewport shading modes** — wireframe / solid / shaded, plus X-ray.
- **Empty object** — plain axes helper; **End** drops the selection onto the
  ground plane.
- **Preferences** — preferences window, unit system, version shown in the
  footer.
- **CI** — GitHub Actions builds the native app for Linux, Windows and macOS.

## Status

- Phase 0 complete: box3d (C17) cross-compiles to WebAssembly and runs in the
  browser with a single JS import. See `crates/phase0-spike`.
- Phase 1 complete: app skeleton (`crates/modeler-app`) renders a lit cube with
  an egui panel via three-d, natively and in the browser, with box3d linked and
  smoke-tested on both targets.
- Phase 2 complete: Blender-style viewport — Z-up world, MMB orbit / Shift+MMB
  pan / wheel zoom, numpad views with auto-perspective, reference grid,
  click-to-snap navigation gizmo.
- Phase 3 complete: scene document with seven primitives (Blender default
  parameters and naming), Shift+A add menu, flat/smooth shading toggle,
  per-object materials.
- Phase 4 complete: box3d physics mirror — every object has a static
  body/shape in a b3World; viewport clicks select via `b3World_CastRayClosest`
  with Blender selection rules and orange outline highlights.
- Phase 5 complete: modal transform operators — G/R/S with X/Y/Z axis and
  plane constraints, numeric input, Ctrl snapping, Shift+D duplicate, X/Del
  delete. Layout-aware shortcuts (Event::Text).
- Phase 6 complete: editor UI — menu bar (File/Add/Object/View/Help), outliner
  with rename + visibility eye, properties sidebar (N) with editable transform
  / primitive parameters / material, status bar, keymap overlay (Help menu).
- Phase 7 complete: physics mode — mark objects Dynamic (+ density) in the
  sidebar, press Space/▶ to simulate (60 Hz box3d stepping with ground plane),
  Esc/⏹ restores the pre-play scene. Red overlap warnings while placing,
  Object > Drop to Floor via filtered ray cast.
- Phase 8 complete: snapshot undo/redo (Ctrl+Z, drag-batched), JSON save/load
  with export/import, OBJ export, and a relative-URL release bundle
  deployable to any static host.

- Phase 9 complete: CAD & organization — grid spacing + snap-to-grid,
  metric units, measurements (ruler), object label/dimension adornments,
  and object hierarchy (Ctrl+P parenting with world-transform preservation,
  indented outliner, hierarchy-aware duplicate/physics/transforms).

- Phase 10 complete: MCP server — coding agents (Claude Code, Cursor, …) can
  inspect and edit the live scene, run the physics sim and take viewport
  screenshots. See [docs/mcp.md](docs/mcp.md).

- Phase 11 complete: file storage — scenes save/load as `.bee3d` (JSON)
  through real file dialogs natively (or a download + file picker in the
  browser), File > Open reuses the last path, and a Recent list (up to 8)
  remembers previously used files.

## Files

Scenes are saved as `.bee3d` (plain JSON under the hood):

- **File > Open…** — native: a file picker; browser: pick a `.bee3d` file
  from disk (read via `FileReader`, no upload).
- **File > Save** — writes to the current file, or prompts once if there
  isn't one yet.
- **File > Save As…** — native: a save dialog; browser: a filename prompt,
  then downloads the file.
- **File > Recent** — the last 8 files. Native remembers their paths
  (`~/.config/box3d-modeler/recent_files.txt`); the browser can't re-read an
  arbitrary path, so it caches the actual file content in `localStorage`
  instead — picking a recent entry restores it directly.

## MCP (AI agent) integration

```bash
scripts/start.sh   # builds, starts the modeler, verifies the MCP bridge
claude mcp add modeler -- $PWD/target/release/modeler-mcp   # one-time
```

Full setup for Claude Desktop / Cursor / Windsurf / VS Code, the tool
reference and troubleshooting: [docs/mcp.md](docs/mcp.md).

# AI assistant (chat)

Toolbar **AI** button (or View ▸ AI Assistant): a chat panel where a language
model builds and edits the scene for you — *"recreate the Eiffel tower"*,
*"make it taller"*, *"add some lights"*, *"make it night time"*. The model
drives the same commands as the MCP server (add/update/delete objects,
lights, walls, groups, the asset library, physics, viewport modes) and can
take viewport screenshots to check its own work.

- **Providers**: Anthropic, OpenAI, OpenRouter, xAI, LM Studio (local), or
  any OpenAI-compatible endpoint (Ollama, vLLM, …). Configure with the ⚙
  button in the panel: API key (none for LM Studio), endpoint, then *Fetch
  models* to pick a model — each one is
  listed with its price per million tokens (from the provider's API where
  available, a built-in approximation otherwise).
- **Costs**: every interaction shows what it cost (tokens × the model's
  price), plus a running session total in the panel footer.
- Works natively and in the browser build (some providers block direct
  browser calls — Anthropic and OpenRouter allow them).
- Extending: new providers implement request-building + response-parsing in
  `crates/modeler-ai` (no transport code); new tools are one entry in
  `modeler-app/src/ai/tools.rs` on top of the shared command executor.

## Deploying

```bash
cd crates/modeler-app
trunk build --release --public-url ./
# dist/ (~4 MB) is fully self-contained — upload to GitHub Pages, Netlify,
# or any static file server.
```

## Viewport controls

| Input | Action |
| --- | --- |
| LMB click | Select (empty space deselects) |
| Shift+LMB click | Extend / toggle selection |
| MMB drag | Orbit (scene follows mouse) |
| Shift+MMB drag | Pan |
| Wheel / Ctrl+MMB drag | Zoom |
| 1 / 3 / 7 (+Ctrl for opposite) | Front / Right / Top view |
| 4 / 6 / 8 / 2 | Step-rotate view |
| 5 | Orthographic / perspective |
| . / Home | Frame scene |
| Click gizmo axis ball | Snap view to that axis |
| Shift+A | Add mesh menu |
| G / R / S | Move / rotate / scale (modal) |
| X / Y / Z (in modal) | Axis constraint (Shift+axis: plane lock) |
| digits (in modal) | Exact value, Enter to apply |
| Ctrl (in modal) | Snap (1 m / 5° / 0.1) |
| Shift+D | Duplicate |
| X / Del | Delete (popup / immediate) |
| N | Toggle sidebar |
| Space | Play / pause physics simulation |
| Esc | Stop simulation (restore scene) |
| Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y | Undo / redo (or Edit menu) |
| Ctrl+S / Ctrl+O / Ctrl+N | Save / Open / New scene (or File menu) |
| Ctrl+P / Alt+P | Parent selection to active / clear parent |
| Drag outliner row onto another | Parent it there (drop on the zone below to unparent) |
| Add ▸ Measure | Ruler: click two points, Esc cancels |
| View ▸ Grid spacing / snap | Configure grid; "snap" also in status bar |
| Double-click outliner name | Rename object |

## Running the app

```bash
# native
cargo run -p modeler-app

# browser (requires trunk: cargo install --locked trunk)
cd crates/modeler-app && trunk serve --port 8322
# open http://127.0.0.1:8322/

# checks (tests + native + wasm compile)
./check.sh
```

## Phase 0 spike

```bash
# one-time: fetch the wasi sysroot (headers + libc for wasm32)
./tools/get-wasi-sysroot.sh

# native check
cargo run -p phase0-spike --release

# wasm check (headless, node)
cargo build -p phase0-spike --target wasm32-unknown-unknown --release
node web/run_node.mjs

# wasm check (browser)
python -m http.server 8321
# then open http://127.0.0.1:8321/web/
```

Both targets run the same scenario: create a box3d world, drop a sphere onto a
box, simulate 2 s, then mouse-pick the sphere with `b3World_CastRayClosest`.

## Layout

- `crates/box3d-sys` — bindgen FFI to box3d; native links the repo's cmake
  build, wasm cross-compiles the C sources with clang + wasi sysroot +
  `shims/wasm_shims.c` (no WASI imports in the final module)
- `crates/phase0-spike` — feasibility test scenario
- `web/` — browser page + node runner sharing the same instantiation glue
- `tools/` — wasi sysroot download script
