# 3D Modeler — Project Plan

A browser-hosted 3D modeler in Rust + WebAssembly, with Blender-style UI and
interaction, using the box3d library where it adds value.

Status legend: `[ ]` todo · `[x]` done · `[~]` in progress · `[!]` blocked

---

## 1. Feasibility & the role of box3d

**Yes, this is feasible — but with a clear-eyed division of labor.**

box3d is a *physics engine*, not a modeling/geometry kernel. It has no concept
of editable meshes, extrusion, or booleans. What it *does* provide, and what we
will use it for:

| Modeler need | box3d feature |
| --- | --- |
| Click-select objects in the viewport | `b3World_CastRayClosest` (mouse ray picking) |
| Box/lasso select candidates | `b3World_OverlapAABB` / `b3World_OverlapShape` |
| Snapping, overlap warnings | shape cast / overlap queries |
| "Drop to floor" placement | `b3World_CastRay` downward |
| Physics preview mode (unique selling point!) | full rigid body simulation, `b3World_Step` |

Everything else — mesh generation, scene graph, transforms, rendering, UI — is
Rust code we write. The architecture keeps a **physics mirror**: every scene
object owns a static body + shape in a `b3World`, kept in sync with the scene,
so all spatial queries go through box3d.

### The critical risk: box3d (C17) inside browser WASM

The Rust web ecosystem needs the `wasm32-unknown-unknown` target, which has
**no C standard library**. Analysis of `nm -u libbox3d.a` shows box3d needs
only:

- memory: `aligned_alloc`, `free`, `memcpy`, `memset`, `memcmp`, `strncpy`
- math: `sinf`, `sqrtf`, `remainderf` + libm bits; `qsort`, `snprintf`
- stdio (`fopen`/`fprintf`/…): **only** used by debug dump & recording-to-file — can be stubbed
- pthreads/semaphores/`clock_gettime`/`nanosleep`: **only** used by the worker
  scheduler — avoided with `workerCount = 0/1`, stubbed at link time

Plan: cross-compile box3d with `clang --target=wasm32-unknown-unknown` using
the wasi-sdk sysroot for headers/libc pieces, plus a small `wasm_shims.c`
(pthread/sem/clock/stdio no-op stubs, allocator routed to Rust's allocator).
This is Phase 0 and gates the rest; fallbacks are listed there.

### Tech stack

- **Language**: Rust (native desktop build for fast iteration + wasm32 for browser)
- **Rendering + windowing**: `three-d` (WebGL2/OpenGL, runs on wasm32-unknown-unknown, mid-level control for grid/outline/gizmo rendering)
- **UI**: `egui` (integrates with three-d; panels, menus, shortcuts)
- **Physics/picking**: box3d via bindgen FFI (same approach as `../RustTest`)
- **Web packaging**: `trunk` (build/serve/deploy wasm + JS glue)
- Alternative kept in reserve: `eframe` + raw `wgpu` if three-d proves limiting.

---

## 2. Architecture

```
3dmodeler/
├── plan.md                  ← this file
├── crates/
│   ├── box3d-sys/           ← bindgen FFI + build.rs (native: link ../build libs;
│   │                           wasm: clang cross-compile + shims)
│   ├── modeler-core/        ← scene graph, mesh primitives, commands/undo,
│   │                           (de)serialization — no rendering, no FFI types leaked
│   └── modeler-app/         ← viewport, camera, tools, egui UI, physics mirror,
│                               native main + wasm entry
├── index.html               ← trunk entry
└── assets/
```

Principles:
- Scene document is the single source of truth; renderer and physics mirror are
  derived state, updated through a change queue.
- All user edits go through a **command** enum (needed for undo/redo from day one).
- Desktop-first dev loop (`cargo run`), browser verified at the end of every phase.

---

## 3. Phases

### Phase 0 — WASM feasibility spike (gate for everything else) ✅ PASSED 2026-07-03
Goal: prove box3d runs in the browser inside a Rust wasm module.

- [x] wasi sysroot vendored via `tools/get-wasi-sysroot.sh` (wasi-sdk-33, no root needed)
- [x] `box3d-sys` build.rs cross-compiles box3d C sources with clang `--target=wasm32-wasip1` + sysroot; objects link cleanly into the `wasm32-unknown-unknown` module
- [x] `shims/wasm_shims.c`: pthread/sem/clock/sched stubs (never executed — box3d takes its serial fallback at `workerCount ≤ 1`, verified in `physics_world.c`), stdio stubs, and a self-contained mini-printf that shadows musl's printf family (musl's would either recurse into our shims or drag in WASI `fd_write`)
- [x] `box3d-sys` crate: bindgen bindings, dual-target build.rs (native links `../build/src/libbox3d.a`, wasm cross-compiles)
- [x] Spike scenario (`crates/phase0-spike`): world + ground + falling sphere, 120 steps, `b3World_CastRayClosest` picking — passes **natively and in wasm with identical results**
- [x] Headless verification: `cargo build -p phase0-spike --target wasm32-unknown-unknown --release && node web/run_node.mjs` → PHASE 0: PASS
- [x] Browser page at `web/index.html` (same glue module as the node runner); serve repo with `python -m http.server` and open `/web/`
- [x] Decision checkpoint recorded in §5

**Key result: the final wasm module imports exactly ONE function — `env.host_log`. Zero WASI imports.**

Gotchas discovered (already handled in build.rs / shims):
- bindgen drops all functions for wasm targets because clang defaults to hidden visibility there → fixed with `-fvisibility=default` (bindgen parse only)
- host env `CFLAGS` (conda's `-march=nocona`) leaks into cc-rs cross-compiles → scrubbed in build.rs
- musl implements `vsnprintf` on top of `vfprintf`; shadowing one without the other causes infinite recursion → whole printf family implemented in the shim

Fallbacks (not needed):
1. Emscripten-built box3d as a *separate* wasm module bridged through JS (clunky but proven).
2. Feature-gate: box3d native-only; browser build uses `parry3d` (pure Rust) for picking/overlap only. Physics preview then native-only.

### Phase 1 — App skeleton (native + web) ✅ DONE 2026-07-03
- [x] Cargo workspace with the three crates (`box3d-sys`, `modeler-core`, `modeler-app`)
- [x] three-d 0.19 + egui window rendering a lit, rotating cube, native (`cargo run -p modeler-app`)
- [x] Same code in browser via `trunk serve --port 8322` from `crates/modeler-app` (canvas fills window, viewport tracks size)
- [x] Basic frame loop: input events → state → render (OrbitControl drag/zoom verified in Chrome)
- [x] box3d linked and smoke-tested on BOTH targets at startup (world create/destroy logs to console) — de-risks the wasm-bindgen/trunk + C-objects combination early
- [x] CI-ish check script: `./check.sh` (tests + native check + wasm check)

Notes:
- three-d on web requires an existing `<canvas>` element in the DOM (it does
  not create one); `crates/modeler-app/index.html` provides it.
- egui 0.34 deprecates top-level `Panel::show` mid-API-transition; allowed
  locally, revisit in Phase 6.
- `Trunk.toml` defaults to debug builds for readable stack traces during
  development; use `trunk build --release` for deployment.
- winit on wasm logs a benign "Using exceptions for control flow, don't mind
  me" exception — this is normal, not an error.

### Phase 2 — Viewport navigation (Blender bindings) ✅ DONE 2026-07-03
- [x] World is Z-up, ground plane XY (Blender convention) — `camera.rs`
- [x] Orbit: MMB drag turntable (pole-safe up vector, exact top/bottom views)
      — direction is "scene follows mouse" like Blender (user-verified fix);
      Alt+LMB was removed: it conflicts with Linux WM/browser menus
- [x] Pan: Shift+MMB drag (content follows cursor, correct world-per-pixel at pivot depth)
- [x] Zoom: scroll wheel + Ctrl+MMB drag (exponential); pinch gesture
- [x] Views: 1/3/7 front/right/top, Ctrl+ variants, 4/6/8/2 step-rotate, 5 ortho/persp toggle
- [x] Blender auto-perspective: axis views switch to ortho, orbiting away restores perspective
- [x] Home / Numpad-'.': frame scene (Phase 4: '.' frames the *selection* and
      makes it the orbit pivot — Blender's focus+orbit workflow; consider an
      "orbit around selection" toggle like Blender's preference)
- [x] Reference grid (1 m minor / 10 m major) + red X / green Y axis lines — `grid.rs`
- [x] Navigation gizmo top-right with click-to-snap axis balls — `axis_widget.rs`
- [x] View name overlay ("User Perspective", "Top Orthographic", …)
- [ ] Touch fallbacks for browser (two-finger orbit/pan) — deferred (three-d maps touch to Left/Right buttons already)

Notes:
- three-d's `Camera::new_orthographic` scales the `height` param by camera-target
  distance internally — pass height *per unit distance*.
- Canvas gets `tabindex` + autofocus via `index.html` so keyboard works without
  a first click.
- Shader-based infinite grid with distance fade: polish backlog.
- dev profile now `opt-level = 1` (+ deps at 2): debug wasm was 10 fps, now 60.

### Phase 3 — Scene model & primitives ✅ DONE 2026-07-03
- [x] `modeler-core`: `Scene` (version counter for derived-state sync), `Object { id, name, transform, primitive, smooth, material }`, Blender-style name dedup (Cube, Cube.001, …)
- [x] Mesh generators in `modeler-core/src/mesh.rs`: plane, cube, UV sphere, ico sphere, cylinder, cone, torus — Blender default parameters, Z-up, unit-tested (incl. winding-vs-normal consistency test)
- [x] Flat vs. smooth normals (`into_flat()` expansion; smooth uses analytic normals; per-object toggle in the panel; new objects flat like Blender)
- [x] Shift+A "Add Mesh" menu at mouse cursor (Esc / click-away closes) + inline panel section; objects placed at origin (3D cursor comes later)
- [x] Default startup scene: cube (viewport key/fill/ambient lights serve as the light until a light *object* exists — revisit with the outliner in Phase 6)
- [x] Simple materials: base color/roughness/metallic per object → three-d PhysicalMaterial
- [x] Production materials (top 5 from Blender/UE comparison): Principled PBR lobes,
      master materials + instances, material functions, world-position effects, MPC globals
      (`modeler-core/src/material.rs`, Properties panel, `scene_render` resolve)
- [x] `SceneRender`: scene-version-driven GPU model rebuild (full rebuild on change; fine at this scale)
- [x] Home/'.' now frames real scene bounds

Notes:
- egui 0.34 `menu_button` popups misrender/miss clicks inside the deprecated
  panel API — replaced with a `collapsing` section; revisit with the proper
  menu bar in Phase 6.
- Browser-verified: Shift+A popup, both add paths, torus + sphere generation,
  smooth toggle rebuild. Synthetic-input caveat: CDP can't produce modifier
  key state; tested by dispatching Shift keydown before A (matches real
  keyboards).

### Phase 4 — box3d physics mirror & picking ✅ DONE 2026-07-03
- [x] `physics.rs`: every object ⇄ one static body + shape in a b3World
      (uniform-scaled spheres → exact `b3Sphere`; plane → thin `b3MakeBoxHull`;
      torus → exact `b3CreateMesh` triangle mesh so the hole is pickable-through;
      cube/cylinder/cone/scaled spheres → `b3CreateHull` of the scaled mesh, max 32 verts).
      Scale baked into shape geometry; position/rotation on the body.
      Mesh data kept alive alongside shapes (box3d references, doesn't copy).
- [x] Version-driven resync, same policy as the renderer
- [x] `camera.pick_ray` (persp + ortho) → `b3World_CastRayClosest` → ObjectId via shape userData
- [x] Click select / Shift+click extend-toggle / click empty deselect (Blender rules, unit-tested in `selection.rs`)
- [x] Selection outlines: inverted-hull pass (front-face culled, 1.03×)
- [x] Active = light orange, selected = darker orange (Blender colors)
- [x] Panel object rows select too; `.` frames selection AND re-pivots orbit (Blender focus+orbit); Home frames all

Notes / gotchas:
- three-d event positions are physical pixels with a BOTTOM-left origin — the
  pick ray must not flip Y (their egui glue flips it for egui).
- three-d drops the first click after page load: `MouseInput` is ignored until
  a `CursorMoved` sets `cursor_pos`. Irrelevant for real mice (they move
  first); makes synthetic single-click testing confusing. Upstream quirk.
- Debugging synthetic input via console is unreliable in this setup; putting
  debug state in an egui label + screenshot was the reliable loop.

### Phase 5 — Transform tools (the Blender feel)

**Keyboard rule (learned in Phase 4, user has AZERTY):** letter shortcuts
(G/R/S/X, …) MUST match on `Event::Text` (the typed character — layout-aware),
not `Key::*` codes: winit's web backend reports PHYSICAL key positions, which
scrambles non-QWERTY layouts. Digits/numpad/F-keys/Escape are fine via `Key::*`.
See `add_menu.rs` for the pattern (incl. suppressing when egui consumed the key).

- [x] Modal operator framework (`modal.rs`): operator owns mouse+keyboard until confirm/cancel; absolute re-application from originals (no drift, trivial cancel)
- [x] `G` grab / `R` rotate / `S` scale — mouse-driven; LMB/Enter confirm, RMB/Esc cancel; live status overlay ("Move: (2.500, 0.000, 0.000) along X [2.5]")
- [x] Axis constraints X/Y/Z (Shift+axis = plane lock); same key again toggles off
      — [ ] double-tap for LOCAL axes still open (deviation from Blender, noted)
- [x] Numeric input during modal (`G X 2.5 ⏎`, `R Z 45 ⏎`, `S 2 ⏎`; '-' toggles sign, Backspace edits)
- [x] `Shift+D` duplicate (clones keep material/smooth, get .001 names, drop into grab)
- [x] `X` delete with Blender-style confirm popup at cursor; `Delete` key = immediate
- [x] Ctrl snapping: 1 m grid / 5° / 0.1 scale
- [ ] On-screen gizmo (move/rotate/scale handles) — deferred to Phase 6 (hotkeys are Blender's primary workflow)
- [ ] Shift = precision (slow) modal motion — deferred
- [ ] "Drop to floor" via box3d down-ray → Phase 7

Browser-verified (2026-07-03): G→X constraint→numeric 2.5→Enter; R Z 45; Shift+D
(Cube.001 in grab); Delete key; x-popup with live count + empty-selection guard.
All letter shortcuts go through `Event::Text` (AZERTY-safe).
Perf note: modal drag bumps the scene version each frame → full mesh rebuild;
fine at this scale, split transform-only updates from topology rebuilds later.

### Phase 6 — UI panels ✅ DONE 2026-07-03
- [x] Top menu bar (File / Add / Object / View / Help) with Blender-style
      hover-switching between open menus. Dropdowns are hand-rolled egui Areas
      (built-in `menu_button` misbehaves in the deprecated panel API); closing
      uses egui's own `clicked_elsewhere()` — event-space closing was unreliable
- [x] Outliner: click select (shift extends), double-click rename (commit on
      Enter/blur), ●/○ visibility toggle — hidden objects are skipped by BOTH
      the renderer and the physics mirror, so they're unpickable like Blender
- [x] Sidebar (N to toggle): Transform fields — location, rotation shown as
      XYZ Euler degrees (stored as quat), scale
- [x] Object properties: all primitive parameters re-editable (segments,
      radii, …, live mesh rebuild), shade-smooth, material base color picker +
      roughness/metallic sliders
- [x] Status bar: contextual hints (modal status while transforming), object
      count, fps
- [x] Keymap overlay via Help menu (F1 not available in three-d's Key enum)
- [x] File > New scene; Object menu: Duplicate (drops into grab), Shade
      Smooth/Flat, Delete; View menu: views/ortho/frame
- [ ] Transform gizmo handles — still deferred (from Phase 5)

Verification notes:
- Browser-verified: layout, menus open/hover-switch/add-primitive (UV Sphere
  via Add menu), eye toggle hides + skips picking, N toggle, gizmo offsets.
- Synthetic-input caveat GREW this phase: three-d on high-DPI web feeds egui
  press positions in logical px but MOVE positions in physical px, so
  interleaved moves make egui treat scripted clicks as drags (real mice
  unaffected — spaces agree for trusted events). Outliner rename / N-panel
  field editing / delete-popup button need a quick REAL-mouse spot check.
- pointer_over_ui (`is_pointer_over_egui`) now gates the viewport pick ray —
  previously clicks on panels also ray-cast through them (latent since Phase 4).

### Phase 7 — Physics mode (box3d showcase) ✅ DONE 2026-07-03
- [x] Per-object physics: Dynamic checkbox + density in the sidebar (static default)
- [x] Play (▶/Space) / pause / stop (⏹/Esc) — stop restores the transform
      snapshot taken at play; editing tools disabled while simulating
- [x] Fixed-60 Hz stepping with write-back of dynamic body transforms into the
      scene each frame; optional 400×400 m ground plane at z=0 (status-bar toggle)
- [x] Gravity corrected for our Z-up world: (0, 0, -9.81) — box3d defaults to Y-up
- [x] Overlap warning: selected objects flash a RED outline while a transform
      modal drags them into (AABB-)overlap with другое object
- [x] Drop to Floor (Object menu): per-object down-ray via `b3World_CastRay`
      with a filtering callback (ignores the selection itself), lands on other
      objects or the implicit floor — closes the Phase-5 deferral
- [x] SceneRender rewritten with per-object caching: transform-only changes
      (sim playback, modal drags) no longer regenerate meshes every frame
- [ ] Throw/drag objects with mouse spring while simulating — stretch, deferred

Verification: 6 native integration tests in `physics.rs` run the REAL box3d
simulation end-to-end — dynamic cube falls from z=3 and rests at z=1.0 ±0.05;
stop restores exactly; statics don't move; pause freezes/resume continues;
drop-to-floor stacks a sphere on a cube at z=3; overlap query flags
intersecting cubes only. Browser-verified: Space starts SIMULATING status,
Esc stops, playback buttons render; dynamic-fall demo needs a sidebar click
(synthetic-input limitation), one real-mouse check recommended.
Notes: box3d mesh shapes can't be dynamic — dynamic tori fall back to a convex
hull during simulation (documented in create_shape).

### Phase 8 — Persistence, undo, deploy ✅ DONE 2026-07-03
- [x] Undo/redo: snapshot-based (`undo.rs`) — a version watcher batches bursts
      (drags, slider scrubs) into single steps after 15 quiet frames; cancelled
      edits produce no step (content compare). Ctrl+Z / Ctrl+Shift+Z + Edit menu.
      Web-AZERTY caveat: Ctrl+letter combos are physical-position on the web
      backend (no Text event fires with Ctrl held) → Edit menu is the reliable
      path there.
- [x] Save/load as JSON (serde on the whole core document incl. next_id):
      native → `scene.json` in cwd; browser → localStorage. File > Export
      downloads a real .json (Blob + anchor via web-sys) / writes a file
      natively; Import = paste-JSON window (file-upload dialog deferred).
- [x] Export OBJ: world-space, triangulated, with normals, `o` groups per
      object (core `export_obj`, unit-tested)
- [x] Release pipeline: `trunk build --release --public-url ./` → 4.1 MB
      self-contained dist/ with RELATIVE urls — verified running from a plain
      static file server (deployable to GitHub Pages/Netlify/anywhere as-is;
      actual public hosting left as a user decision)
- [x] README updated with controls, architecture, build & deploy instructions
- [ ] glTF export — stretch, deferred
- [x] Browser file-upload import — done in Phase 11 (real Open/Save/Recent, `.bee3d`)
- [ ] localStorage autosave timer — still deferred

Verification: 3 undo tests (roundtrip incl. add+move, drag batches as ONE step,
cancelled edit = no step), JSON roundtrip test (preserves ids/next_id), OBJ
content test. Browser: release dist loads & runs from static hosting; Edit
menu renders with correct enabled/disabled undo state; File menu items render.

---

## 4. Milestones

| Milestone | Definition of done | Status |
| --- | --- | --- |
| M0 (Phase 0) | box3d world steps + ray casts inside a browser tab | ✅ 2026-07-03 |
| M1 (Phases 1–2) | Navigable empty scene with grid in the browser, Blender camera controls | ✅ 2026-07-03 |
| M2 (Phases 3–4) | Add primitives via Shift+A, click-select them (box3d picking) | ✅ 2026-07-03 |
| M3 (Phase 5) | G/R/S with axis constraints feels like Blender | ✅ 2026-07-03 |
| M4 (Phases 6–7) | Usable app: panels, physics play mode | ✅ 2026-07-03 |
| M5 (Phase 8) | Deployable build with save/load (public hosting = user's call) | ✅ 2026-07-03 |

**All planned phases complete.** Backlog of deferred items: transform gizmo
handles, local-axis double-tap, Shift-precision modal motion, shader-based
infinite grid, localStorage autosave timer, glTF export, mouse-spring
throwing during simulation, mesh edit mode (see §6).

### Phase 9 — CAD & organization improvements (user request 2026-07-03)

- [x] **9.1 Grid system & snap-to-grid** ✅ 2026-07-03: grid spacing selector
      (0.1–2 m, View menu) rebuilds the render grid; "snap" toggle in the
      status bar + View menu snaps grab results to ABSOLUTE grid positions;
      Ctrl inverts while dragging; "[snap]" tag in the operator status
- [x] **9.2 Metric units** ✅: move status in m, dimensions "W × D × H m",
      measurement labels "N.NNN m", grid label "grid N m"
- [x] **9.3 Object adornments** ✅: sidebar "Adornments" section per object —
      name label and dimension billboards projected above the object
      (`overlay.rs`, egui background layer, behind panels, hidden when the
      camera looks away)
- [x] **9.4 Measurements** ✅: Add ▸ Measure (ruler) — two clicks on surfaces
      via `pick_point` (box3d closest-ray; z=0 grid plane fallback, unit
      tested); persistent `Measurement` entities in the scene document
      (serialized, undo-aware), yellow line + distance label overlay,
      deletable from the sidebar list
- [x] **9.5 Duplicate** ✅: Phase 5 feature confirmed; duplicates now also
      copy physics/adornment flags and REMAP parent links inside the
      duplicated set (Blender behavior)
- [x] **9.6b Outliner drag & drop** ✅ 2026-07-03: drag a row onto another
      row to parent it there (orange highlight on the hover target); a
      "drop here to unparent" zone appears below the tree while dragging or
      whenever anything is parented. Cycle-guarded via `set_parent` with a
      status-bar message on rejection. Needs a real-mouse check (egui drag
      gestures can't be exercised with synthetic input).
- [x] **9.6 Object hierarchy** ✅: `parent: Option<ObjectId>` in core with
      world transforms composed through the chain (`Transform::compose` /
      `to_local`, unit-tested incl. rotated-parent roundtrip and cycle
      rejection); Ctrl+P parent-to-active / Alt+P clear keep world placement;
      Object menu equivalents; outliner indents children; deleting a parent
      unparents children in place; G/R/S operate in world space and write
      back through the parent (descendants of selected parents are skipped);
      physics simulation writes world transforms back depth-ordered;
      renderer/physics/picking/export all use world transforms

Verification: 5 new native tests (hierarchy math roundtrip, measurement
serde, pick_point object hit + grid fallback) → 25 tests total. Browser:
Add ▸ Measure entry, snap/grid status controls and the Adornments sidebar
section all render; full sidebar verified. Synthetic-input limits (egui
clicks >~20px from top-left get drag-rejected) mean measure-clicks,
adornment toggles, snap toggle and Ctrl+P need a REAL-mouse spot check.
Also fixed: box3d's global init is not thread-safe across concurrent world
creation — FFI tests now serialize on a mutex (worth an upstream note).

### Phase 10 — MCP server (user request 2026-07-03)

- [x] **10.1 Control API in the app** ✅ 2026-07-03 (native builds): tiny HTTP server on
      localhost:8323 (`MODELER_CONTROL_PORT` overrides); commands executed on
      the render-loop thread against the live scene: get_scene, new_scene,
      add_object, update_object, delete_object, set_parent, add_measurement,
      simulate (play/pause/stop), screenshot (PNG of the viewport)
- [x] **10.2 `modeler-mcp` crate** ✅: stdio MCP server (hand-rolled JSON-RPC,
      no async runtime) exposing those commands as MCP tools; screenshots
      returned as MCP image content so agents can SEE the viewport; helpful
      error when the app isn't running
- [x] **10.4 `scripts/start.sh`** ✅: always rebuilds (not just "if missing" —
      cargo no-ops fast when nothing changed, so this is cheap and guarantees
      the latest code runs), starts the app (or reuses a running one, with a
      note that the running instance needs a manual restart to pick up a
      fresh build), smoke-tests the MCP bridge end-to-end and prints the
      registration command; Ctrl+C stops the app
- [x] **10.5 Status-bar MCP indicator** ✅: bottom-right shows "MCP :port"
      (dim green, listening), "MCP active" (bright green while agent commands
      arrive, <3 s), or "MCP off" (red, port taken); hover shows the port,
      command count and the registration hint. Native only (hidden on web).
      Verified via the control API's own screenshot tool (idle + active).
- [x] **10.3 Documentation** ✅ (`docs/mcp.md` + README section): setup for
      Claude Code (`claude mcp add`), Claude Desktop, Cursor, Windsurf,
      VS Code/Continue and generic `mcpServers` JSON; tool reference;
      example agent workflow; troubleshooting
- Note: browser builds can't host a TCP server — the MCP integration targets
  the NATIVE app (WebSocket bridge for the web build = future work)

### Phase 11 — File storage: real save/load + recent files (user request 2026-07-03)

Closes the Phase 8 deferral ("browser file-upload import"): the scene format
gets its own extension (`.bee3d`, still plain JSON) and a proper Open/Save/
Save As/Recent flow, replacing the old fixed-`scene.json`-in-cwd shortcut.

- [x] Native: real file dialogs via `rfd` (uses the XDG desktop portal on
      Linux — confirmed, not the raw GTK3 chooser); `Save` reuses the last
      path, `Save As…` always prompts, default filename `scene.bee3d`
- [x] Web: no filesystem access, so `Open…` drives a hidden
      `<input type=file accept=".bee3d">` + `FileReader` (async — polled once
      per frame via `io::poll_open()`); `Save`/`Save As…` (small filename
      prompt window) trigger a download, same as the existing OBJ export path
- [x] Recent files list in the File menu (up to 8): native persists absolute
      paths to `~/.config/box3d-modeler/recent_files.txt` (plain lines, no
      serde needed); web caches the last 8 files' actual JSON content in
      localStorage (`modeler_recent_{i}_name`/`_json`) since a browser can't
      re-read an arbitrary path — clicking a recent entry restores from cache
      directly, no re-picking required
- [x] `File > Import .json (paste)…` kept as a manual fallback alongside the
      new `Open…`

Verification: 2 new unit tests in `io.rs` (pure `reorder_recent` dedup/order/
truncate logic — no filesystem access, won't touch a real user's recent list
during `cargo test`; save→load roundtrip via a tmp file). Native end-to-end
smoke test via `xdotool` + screenshots against a throwaway instance (port
8391, cwd outside the repo): File menu renders Open…/Save/Save As…, Save As…
opens the real portal dialog defaulting to `scene.bee3d` with the "Bee3D
scene" filter, Save writes the file and shows "saved scene.bee3d", the file
then appears under Recent, and clicking it shows "loaded scene.bee3d" with
the scene restored. Test artifacts (temp file, temp recent-files entry,
temp config dir) cleaned up afterward. `./check.sh` passes (workspace tests
incl. the 2 new ones, native + wasm `cargo check`).
Gotcha: the portal file chooser runs as a separate `xdg-desktop-portal-gtk`
process — synthetic `xdotool type`/`ctrl+l` didn't reach its text entry
reliably, but real mouse/keyboard input is unaffected (same class of
synthetic-input limitation noted in Phases 4/6).

### Phase 12 — File-operation keyboard shortcuts (user request 2026-07-03)

Ctrl+S save / Ctrl+O open / Ctrl+N new scene / Ctrl+Z undo / Ctrl+Y (or
Ctrl+Shift+Z) redo, matching Blender/standard app conventions.

- [x] `UiState` gained `action_save`/`action_open`/`action_new_scene` public
      methods so the File menu buttons and the new global shortcuts share one
      code path (no duplicated logic)
- [x] Wired in `main.rs` next to the existing Ctrl+Z handler, same guard
      (`physics.is_stopped() && !modal.active() && !egui_owns_keyboard`) and
      same physical-`Key::*`-code caveat as Ctrl+Z/Ctrl+P — AZERTY users
      should use the File/Edit menus for these combos
- [x] File menu button labels and the Help > Keymap window updated with the
      new shortcut hints

Verification: confirmed plain `S` (Scale modal) still fires correctly
(unaffected by the new Ctrl+S handler — Ctrl-held key combos don't produce
the `Event::Text` that the modal's letter-shortcut matching relies on, so
there's no collision). Ctrl+Z/Ctrl+Y verified end-to-end via `xdotool` +
screenshots (moved a cube, undo reverted it, redo restored it). Ctrl+S
verified via screenshot (opens the real Save dialog, defaults to
`scene.bee3d`). Ctrl+N verified via the MCP control API (added a second
object, sent Ctrl+N over the keyboard only, confirmed `get_scene` dropped
back to just the default Cube). Ctrl+O verified the same way: confirmed it
opens the same portal Open dialog as the File menu button (blocks the
control API while the dialog is up — expected, since `rfd`'s dialog calls
are synchronous/blocking — then unblocks cleanly on cancel).
Gotcha hunting this took a while: two `xdotool`-driven test app instances
appeared to vanish entirely mid-test with no panic, no coredump. Root cause
turned out to be environmental, not a code bug — an overlapping Chrome
window plus focus-follows-mouse meant plain mouse clicks and un-scoped key
presses were landing on Chrome instead of the modeler, and one attempted
`pkill` cleanup from a stale test caught an app instance that was actually
just blocked waiting on its own (correctly-opened but off-screen) file
dialog. Switching to `xdotool key --window <id>` (delivers to a specific
window regardless of real X focus) plus verifying via the MCP control API
instead of screenshots resolved the ambiguity and confirmed the shortcuts
work correctly.
Design note (pre-existing, not introduced here): native Open/Save dialogs
call `rfd` synchronously from inside the render-loop frame closure, so the
whole app — rendering and the MCP control API both — pauses while a native
file dialog is open. Acceptable for a modal file picker; worth knowing if a
future change wants the MCP bridge to stay responsive during Save/Open.

## 5. Decisions log

| Date | Decision | Why |
| --- | --- | --- |
| 2026-07-03 | box3d = spatial queries + physics; modeling core in Rust | box3d is a physics engine, not a geometry kernel |
| 2026-07-03 | Target `wasm32-unknown-unknown`; cross-compile box3d with clang + shims | Required by Rust web ecosystem (egui/three-d/trunk); box3d's libc needs are small (verified via `nm -u`) |
| 2026-07-03 | three-d + egui (eframe+wgpu as reserve) | Web-ready, enough rendering control for grid/outlines/gizmos, mature egui integration |
| 2026-07-03 | Single-threaded box3d in browser (`workerCount ≤ 1`) | wasm threads need COOP/COEP + atomics; not worth it for a modeler's query workload |
| 2026-07-03 | Phase 0 PASSED: keep the clang+sysroot+shims approach, no fallback needed | wasm module needs only `env.host_log`; native and wasm results identical |
| 2026-07-03 | Printf family self-implemented in `wasm_shims.c` (no external code) | musl's printf machinery pulls WASI imports or recurses into shims |
| 2026-07-03 | World coordinates are Z-up, XY ground plane | Matches Blender's conventions so navigation/views/tools translate 1:1 |

## 6. Out of scope (for now)

- Mesh *editing* (edit mode, extrude, loop cuts, booleans) — would need a real
  geometry kernel (e.g. custom half-edge mesh); revisit after M5
- Modifiers, animation, rendering engines, textures/UVs
- Multi-user / collaboration
