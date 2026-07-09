# Performance & CUDA Investigation — Plan (July 2026)

> Original questions: How can we improve performance? Investigate how we can update the box3d
> library to use CUDA for physics calculations. Investigate if we can use CUDA for 3D rendering
> as well. Make a clean plan before we start implementing anything.

Investigated by 8 parallel agents (3 code deep-reads, 2 web research sweeps, then a fact-check /
steelman / critique review pass). Every file:line claim below was independently re-verified.

## Answers first

**CUDA inside the box3d solver: not now.** GPU rigid-body solvers beat a well-tuned
multithreaded SIMD CPU solver only above roughly 10k–100k active bodies in a *single interactive
scene* (NVIDIA's own PhysX docs say "several thousand active actors" minimum; MuJoCo MJX warns a
single scene can be ~10x *slower* on GPU; Box2D v3's CPU solver steps a 5,050-body pile in
0.9 ms). Meanwhile box3d currently runs in the modeler **single-threaded by configuration** with
**no AVX2 path in the engine**, and the modeler **rebuilds the entire physics world every frame
during any drag**. There is roughly an order of magnitude of CPU headroom untouched. A CUDA
backend would also break box3d's cross-platform determinism, could never ship in the WASM/browser
build, and every precedent is negative (Bullet 3's GPU pipeline: abandoned; PhysX GPU: 500+ CUDA
kernels sustained only by NVIDIA; Jolt's author: rejected CUDA in favor of a vendor-neutral
compute interface).

**CUDA for viewport rendering: no, categorically.** CUDA is not a rasterization technology. The
best 2026 CUDA software rasterizer (CuRast) is 7–8x *slower* than the hardware raster pipeline on
CAD-scale scenes and only wins at billions of pixel-sized triangles. No CAD/DCC app (Blender,
SolidWorks, Onshape, Fusion, Plasticity) rasterizes its viewport with CUDA. This app's render
costs are CPU-side bookkeeping that CUDA cannot touch. DLSS doesn't support OpenGL at all.

**Where the GPU (and literally CUDA) does win for this project:**
1. **Photorealistic renders** — a "Render" button that exports to Blender via the existing MCP
   bridge runs Cycles+OptiX on the RTX 4080 SUPER. That *is* CUDA, used where it's strong.
2. **A contained CUDA experiment** (optional, timeboxed) — a standalone 100k-body debris demo
   that produces first-party numbers for the GPU-physics decision gate. Zero product risk.
3. **GPU particles/smoke** (roadmap item) — as portable compute/instancing, ships in browser too.

**Pick the goal, it changes the order:**
- *(a) Felt editor speed* → Phase 0 + Phase 1 now.
- *(b) Massive destruction showcase* (lift the 600-brick cap) → Phases 0–3, then gate 4.3.
- *(c) Learning CUDA / putting the 4080 to work* → 4.1 + 4.2 can start anytime, in parallel.

## What the investigation found

### The real, recorded performance ceiling
`modeler-app/src/object_ops.rs:330-336, 458-466` hard-caps break-into-bricks at
**MAX_BRICKS = 600** and silently enlarges bricks to fit, with the comment *"physics with
thousands of bodies would crawl."* That cap is the project's actual physics performance complaint
— and a full house at realistic brick modules is 10k–50k bodies, which is genuinely at the edge
of the GPU crossover band. But the crawl at 600 today is almost certainly *not* solver
arithmetic; it's the app-side hot paths below, on a solver pinned to one thread.

### The modeler (where the felt slowness lives)
- box3d configured **single-threaded**: `workerCount = 0` (`physics.rs:50`). Note: box3d's
  built-in scheduler needs `workerCount > 1` (`physics_world.c:364`); 0 and 1 are equivalent.
- **Hot path #1:** `physics.sync` destroys and recreates the *entire* box3d world — every body,
  convex hull, tri-mesh — on any scene version bump (`physics.rs:77-148`). `object_mut` bumps the
  global version on *every access* (`modeler-core lib.rs:737-740`), and a modal G/R/S drag writes
  targets every frame even when the mouse is idle (`modal.rs:257, 676-680`) → full O(N) world
  rebuild per frame, every drag.
- **Hot path #2:** scene-mode shadow maps are keyed on `scene.version()` (`scene_render.rs:322`)
  → ALL casters × lights re-render on any drag frame in Scene lighting.
- **O(N²) per frame:** `world_transform` does a linear object lookup + parent recursion per
  object (`lib.rs:840-851`), called from `SceneRender::sync` for every visible object every frame.
- One draw call per object (+1 inverted-hull per selected), zero instancing; wireframe and labels
  are CPU-projected through the egui painter every frame.
- The render mesh layer itself is *well cached* (rebuild only on `mesh_revision` change;
  transform edits are just a matrix update) — physics mirror and shadows are the culprits.
- Undo deep-clones the entire SceneData per settled edit (`undo.rs:52-64`) — amortized, but
  O(scene) hiccups with big meshes.
- The WASM build compiles box3d **scalar** (no SIMD128: `box3d-sys/build.rs` passes no
  `-msimd128`, and `core.h:43` only detects WASM via `__EMSCRIPTEN__`) and serial.

### The engine (box3d, vendored Box2D-v3-style C17)
- "TGS Soft" architecture: broad phase → parallel narrow phase → graph-colored (24 colors)
  sub-stepping Gauss-Seidel → finalize/CCD → BVH refit → sleeping (`b3World_Step`,
  `physics_world.c:1025`).
- SIMD hardwired to width 4 (SSE2/NEON); **no AVX2 path, no `-march` flags** — x64 builds are
  baseline SSE2. Only convex contacts are vectorized; **joints, mesh contacts, and the overflow
  color are scalar** (`constraint_graph.c:147`).
- Thread scaling ~6x at 8 threads on constraint-heavy scenes (junkyard 24.5s → 4.1s), but
  **negative** on broad-phase-heavy scenes (large_world 9.7ms@1 → 15.4ms@7) — serial stages +
  solver spin-wait. Threading is not free for small scenes.
- Scalar-vs-SSE2 delta is ~8% end-to-end on one scene at one thread count (washer @8 threads;
  the scalar dataset is otherwise empty — re-measure in Phase 0). SIMD looks weak because the
  scalar stages dominate.
- Determinism is a design pillar: unconditional FMA/rsqrt avoidance (`contact_solver.c:880,
  2087, 2149`), plus world-state hashing when a recording is attached (`physics_world.c:1168`).
  A GPU path forfeits cross-platform determinism (GPU results differ across hardware).
- Hypothetical GPU offload transfer bill: ~10–16 MB PCIe round-trip per step for a
  10k-body/30k-contact scene ≈ 1.3–3 ms — comparable to the whole CPU step. Plus a fixed
  ~0.3–1 ms floor from kernel launches (~3–7 µs × dozens per substep) and sync.

### Build footgun (fix before measuring anything)
`build.sh` configures CMake with **no `CMAKE_BUILD_TYPE`** → empty type = no `-O` flag; a fresh
checkout builds `libbox3d.a` at -O0 and `box3d-sys/build.rs` links whatever it finds in
`build/src/`. The current cache happens to be Release, but any baseline must record the linked
library's build type. *(Fixed 2026-07-08: `build.sh` now defaults to Release.)*

---

## Status & measured results (2026-07-08 — Phases 0 & 1 DONE, v0.2.18)

Phase 0 and Phase 1 are implemented. All 70 modeler tests pass (including new
incremental-mirror equivalence tests); `check.sh` (native tests + native + wasm checks) is green.

**Measured on this machine (RTX 4080 SUPER box, Rust release profile, libbox3d.a Release
`-O3 -DNDEBUG`) via `cargo test --release -p modeler-app -- --ignored --nocapture perf_baseline`:**

| Workload | Before | After |
|---|---|---|
| Drag frame, 200-object scene (physics mirror) | full rebuild, **1.37 ms** | incremental sync, **15.4 µs** (~89x) |
| Simulation step, 200 objects / 50 dynamic | — | 22.6 µs |
| 400 bricks: avg step (serial) | — | 213 µs |
| 600 bricks (MAX_BRICKS today): avg step | — | **206 µs** (16 workers) |
| 2,000 bricks: avg step | — | 584 µs |
| 5,000 bricks: avg step | — | **1.36–1.9 ms** (16 workers) — ~10% of a 60 Hz frame |
| Undo checkpoint, 200 objects | — | clone 19 µs + compare 31 µs (amortized; harmless) |

**Conclusion: physics is ready for MAX_BRICKS = 5,000 today.** The binding constraint for
raising the cap is now RENDERING (5,000 un-instanced draw calls) — i.e. Phase 3.1 instancing,
not physics.

What landed, per Phase 1 item:
1. ✅ Release profile `opt-level=3, lto="thin", codegen-units=1`; trunk `data-wasm-opt="4"`
   for release web builds; `build.sh` defaults `CMAKE_BUILD_TYPE=Release`.
2. ✅ `Scene` keeps an id→index map (O(1) `object()`/`object_mut()`) and
   `Scene::world_transforms()` computes all world transforms in one memoized O(N) pass;
   renderer, wireframe cache, lights signature and physics all use it.
3. ✅ Change detection via value diffing (physics `ShapeKey` + transform compare) and a
   lighting content signature — the global version stays authoritative for undo/staleness,
   so nothing broke; no new revision fields were needed beyond `mesh_revision`.
4. ✅ Incremental physics mirror: transform-only edits call `b3Body_SetTransform`; geometry
   changes (primitive params / world scale / mesh_revision / cutouts / outline / density)
   rebuild only that object's body; adds/removes touch one body. Gated by a 40-step
   randomized equivalence test (pick, pick_point, overlap parity vs a from-scratch mirror)
   plus body-handle stability tests (move reuses, scale/mesh-edit rebuilds).
5. ✅ Shadow maps re-key on a lights+casters content signature instead of `scene.version()` —
   material tweaks, panel access, selection changes and idle frames no longer regenerate
   every shadow map. Dragging a caster still does (its shadow really moves); a per-light
   frustum cache is a possible follow-up if ever needed.
6. ✅ Modal idle skip: `write_targets` drops bit-identical writes, so a G/R/S operator with a
   still mouse stops bumping the version (and invalidating caches) every frame.
7. ✅ Micro-fixes: edit-drag vertex moves update VBOs in place when topology is unchanged
   (no mesh+material+outline recreation per frame); a weld→vertex inverse map kills the
   O(moved × all-verts) scan; physics write-back order is pre-sorted once at play;
   overlap/drop queries stopped cloning a HashSet per shape/ray.
8. ✅ Threaded stepping, gated: ≥500 dynamic bodies → box3d's internal scheduler with
   `workerCount = cores (≤16)`, native only; the world returns to serial on stop. Below the
   threshold everything stays serial (large_world.csv shows threading hurting small scenes).

Also verified in Phase 0: the native loop repaints continuously even when idle
(`ControlFlow::Poll` + unconditional `request_redraw` in main.rs) — event-driven redraw is a
possible follow-up, deliberately left alone in Phase 1.

Local box3d benchmark-suite baseline (`Vibecoding/bench-local/*.csv`, Ryzen 9 7950X — the
same CPU family as the committed `benchmark/amd7950x_sse2` results, here with the Release
`-O3` build, `-t=8 -r=1`): junkyard 17.16 s @1 thread → 3.44 s @8 (5.0x); washer 21.81 s →
4.31 s (5.1x); **large_world negative scaling reproduced: 8.12 ms @1 → 18.64 ms @8** —
Phase 2.4 (serial stages + solver spin-wait) is confirmed real on this machine.

---

## The plan

### Phase 0 — Measure (and fix the measurement) — ~1 day
1. Fix `build.sh` to default `-DCMAKE_BUILD_TYPE=Release`; make Phase 0 notes record the build
   type of the linked `libbox3d.a`.
2. Benchmark the real workloads natively: (a) house-scale scene, ~200 bodies / 50 dynamic at
   60 Hz; (b) brick-pile poke at 600 bricks (today's cap), then 2,000 and 5,000. Record per-step
   ms and per-frame breakdown.
3. Profile one editing session with perf/hotspot (Tracy is wired in the C side only; Rust-side
   hookup is a half-day if wanted): idle orbit (also check the app isn't repainting at monitor
   rate while idle — `main.rs:962-967` is Poll + request_redraw), G-drag at 200 objects, physics
   playback, Scene-lighting drag, break-into-bricks.
4. Re-run the box3d `benchmark/` suite locally, including a *complete* scalar-vs-SIMD baseline
   (the existing ~8% figure rests on a single data point).
5. Measure undo snapshot cost on a large scene (contingent fix: Arc-backed per-object structural
   sharing, composes with 1.3).

### Phase 1 — Modeler quick wins (the felt performance) — ~1.5 weeks
In execution order (enablers first); each independently shippable.
1. **Release profile** (minutes): `opt-level = 3`, `lto = "thin"`, `codegen-units = 1` for native
   release in `3dmodeler/Cargo.toml`; add `wasm-opt -O3` to the web pipeline. Measure the delta.
2. **ObjectId index map + one-pass world transforms** (hours): `HashMap<ObjectId, usize>` +
   parent-before-child pass; kills the O(N²) in both `SceneRender::sync` and physics diffing.
3. **Per-object revisions *in addition to* the global version** (½–1 day): keep `object_mut`'s
   global bump — undo change detection (`undo.rs:38`), `PhysicsMirror::sync` (`physics.rs:81`),
   and `SceneLights` (`scene_render.rs:322`) all depend on it — and add per-object
   transform/mesh revisions, migrating consumers one at a time. Removing the global version
   would silently break undo.
4. **Incremental physics mirror** (2–3 days, the big one): transform-only changes →
   `b3Body_SetTransform`; scale/mesh/shape-identity changes (scale is baked into geometry) →
   rebuild only that body; parent moves → world-transform diffing so descendants update.
   **Gate:** a randomized-edit-script equivalence test (move/rotate/scale/parent/hide/delete/
   cutout/mesh-edit) where the incremental world must agree with a from-scratch rebuild on
   pick/overlap/drop_to_floor; plus a profile showing zero `b3CreateHull` calls during a pure
   G-drag.
5. **Shadow-map dirty keying** (hours): key `SceneLights::sync` on light objects + caster
   revisions from 1.3. Test: light moves, caster transform changes, and caster mesh edits all
   still invalidate.
6. **Modal idle skip** (minutes): skip `write_targets` when targets are unchanged since last
   frame — free rebuild/invalidation removal while a modal is active but the mouse is still.
7. **Micro-fixes** (hours): update VBO positions in place during vertex drags (skip
   Gm+material+outline recreation); precompute weld→vertex map; fold in the physics writeback
   sort (`physics.rs:329`) and `overlapping()`'s per-shape HashSet clone (`physics.rs:419`).
8. **Threaded stepping, gated** (½ day): enable `workerCount = physical cores` natively **only
   during simulation playback above a body-count threshold** (e.g. >500 dynamic bodies) — the
   large_world benchmark proves threading can *hurt* small scenes. Verify intra-world thread
   safety first (the `physics.rs:576` test mutex is about concurrent world create/destroy, not
   stepping).

**Exit criteria:** G-drag in a 200-object scene shows zero hull/mesh creation in the profile;
600-brick poke plays at 60 Hz with headroom; re-measure Phase 0 numbers and record deltas here.

### Phase 2 status (2026-07-08 — items 2.2, 2.3, 2.4 DONE, v0.2.19)

**2.2 AVX2 8-wide contact solver — done, opt-in.** New `BOX3D_AVX2` CMake option (OFF by
default): `cmake -B build-avx2 -DCMAKE_BUILD_TYPE=Release -DBOX3D_AVX2=ON`. Adds a `__m256`
`b3FloatW` arm plus width-generic gather/scatter in contact_solver.c, `B3_SIMD_SHIFT 3`, and
keeps `B3_SIMD_SSE2` defined for the 3-lane b3V32 layer. No FMA (global `-ffp-contract=off`
already enforced this). The stack allocator was already 32-byte aligned by design.
Measured (interleaved A/B, this machine): **large_pyramid 1.49 s → 1.17 s single-threaded
(~22%)**, washer 24.0 → 22.0 s (~9%, joint-heavy → scalar-dominated), large_pyramid @8 workers
~340 → ~284 ms (~16%). Unit-test output is byte-identical to the SSE2 build (34 passed both).
A `build-avx2/` directory with benchmarks is checked out locally for A/B.

**2.3 WASM SIMD128 — done.** `core.h` now detects bare-clang wasm (`__wasm__` +
`__wasm_simd128__`) and selects a new `B3_SIMD_W128` arm in contact_solver.c
(`wasm_simd128.h`, `pmin/pmax` to match SSE2 semantics); `box3d-sys/build.rs` passes
`-msimd128`; `.cargo/config.toml` enables `+simd128` for the Rust side (universal in browsers
since 2021). Validated at runtime: `cargo build -p phase0-spike --target wasm32-unknown-unknown
--release && node web/run_node.mjs` → PASS (sphere settles through the W128 contact solve).
Follow-up available: the 3-lane `b3V32` collision layer (simd.h/simd.c) still runs scalar on
bare-clang wasm — porting its ~25 shuffle-heavy ops is the next wasm win.

**2.4 large_world negative scaling — diagnosed and fixed (32% better @8 threads).** Root
cause: `large_world` is 1M static bodies with ~100 awake spheres (~16 µs/step of real work),
and (a) the solver forked `workerCount` workers that each spin through dozens of stage sync
points per step, (b) every `b3ParallelFor` paid a scheduler round-trip (semaphore wake) even
for single-block loops. Fixes, both upstreamable: a work-volume clamp on the solver fork-join
width (`solver.c` — weights: convex ×4, joints ×8, mesh contacts ×256, since mesh manifolds
are far heavier per constraint — trees100 must keep its workers), with per-worker bitsets
still cleared for ALL workers so the merge loops never see stale bits; and an inline
single-block path in `parallel_for.c`. Validated interleaved vs a pristine-HEAD build:
**large_world @8: 18.5 → 12.2 ms** (@1: 8.1 ms); trees100 74–78 vs 85–87 (no regression);
junkyard, joint_grid, rain neutral. Residual @8 gap (~4 ms/500 steps) is semaphore wakes for
2–20-block parallel-fors; batching scheduler wakes is the remaining upstream idea.

**2.1 vectorize scalar stages — DONE for the narrow phase (v0.2.20); profile falsified the
joint hypothesis.** gprof (`-pg` build, washer + junkyard @1 thread) showed the plan's premise
was wrong: **washer has ZERO joints** — it is 41k contacts, and the scalar time lives in the
narrow phase, not joint solving. Junkyard breakdown: hull-hull SAT ≈ 24% of the whole step
(`b3QueryEdgeDirections` alone 13.5%, `b3FindHullSupportVertex` 58.7M calls), wide-constraint
prepare ~10–12%, dynamic-tree queries (CCD + pairs) 5–14%, pair-hash `b3ContainsKey` 4–7%.
Joints only matter in joint_grid — and the modeler has no joints at all.

What landed (SSE2-guarded, scalar fallback kept for NEON/W128; AVX2 build inherits since it
keeps B3_SIMD_SSE2 defined):
- `b3QueryEdgeDirections` (convex_manifold.c): the O(E_A × E_B) Gauss-map filter now tests
  four A edge-pairs per iteration from per-call SoA staging; surviving lanes run the scalar
  separation in lane order. Kernel time 13.5% → 3.7% on junkyard (~4x).
- `b3FindHullSupportVertex` (hull.c): four dots per iteration with first-max-index semantics,
  only for hulls ≥16 vertices (small boxes/cylinders keep the tight scalar loop — the SIMD
  version measured SLOWER for 8-vertex hulls before the threshold was added).
- Bit-exactness by construction (same expression trees, no FMA, exact sign-flip negation,
  smallest-index-among-maxima reductions) and by evidence: unit-test output byte-identical
  (SSE2 and AVX2), and end-of-run contact counters EXACTLY equal on every benchmark scene
  (junkyard 189,564 contacts after 500 chaotic steps) — trajectories are bit-identical.

Measured (interleaved vs pristine HEAD): junkyard **20.2 → 17.8 s @1 (~12%)**, and @8 workers
(all Phase 2 changes combined): junkyard **4.08 → 3.55 s (+13%)**, trees100 +9%, washer +5%,
large_world +28%, joint_grid neutral.

Remaining 2.1 targets, in profile order (all documented, none started):
1. `b3PrepareContacts_Convex` (~11% — needs restructuring ~150 lines of per-contact setup
   into wide math over gathered inputs; the genuine 1–2 week item).
2. Dynamic-tree query + CCD (`b3ContinuousQueryCallback`, 6–14% — washer's spinning drum
   makes everything a fast body; algorithmic, not SIMD).
3. Pair-hash `b3ContainsKey` (4–7% — 34M lookups/1000 steps in washer; hash/probing micro-opt).
4. Mesh-contact solve widening (needs lane-packing whole contacts with their manifolds to
   preserve sequential-impulse semantics — design sketch in the session notes).

### Phase 2 — box3d engine headroom (CPU, benefits every embedder) — pick by Phase 0 profile
Items 1 and 2 are 1–2 weeks *each*; do them in this order (or only one) as the profile dictates.
Note: `src/` is vendored from erincatto/box3d — keep changes as upstreamable patches (upstream
Box2D v3 already has the 8-wide design, so an AVX2 port may be accepted upstream).
1. **Vectorize the scalar stages** (1–2 weeks, likely higher value): joints and mesh contacts
   are fully scalar today — that's why SIMD only buys ~8% end-to-end. Pack joint solves by type;
   widen the mesh-contact solve.
2. **AVX2 8-wide path** (1–2 weeks): `B3_SIMD_WIDTH 8` variants of `b3FloatW`, gather/scatter,
   and the wide-constraint layout, behind an opt-in CMake flag. SSE2 stays the bit-exact
   deterministic reference; AVX2 is an opt-in speed mode.
3. **WASM SIMD128** (days, cheap): mirror the Emscripten trick — box3d already maps WASM to the
   existing SSE2 code via emulation headers (`src/CMakeLists.txt:201`); add `-msimd128` + SSE
   compat detection to the `box3d-sys` wasip1 clang build, and `-C target-feature=+simd128` for
   the Rust side. Browser physics is currently scalar *and* serial.
4. **large_world negative scaling** (exploratory): serial contact creation, spin-wait
   orchestration, BVH refit; try spin backoff and task granularity. Upstream-relevant.

**Exit criteria:** benchmark-suite geomean improvement recorded with SSE2 determinism preserved;
browser physics measurably faster.

### Phase 3 — Rendering scale-up (needed for the brick showcase; optional for house editing)
1. **Instanced rendering** (days, THE draw-call lever): group cached models by
   (primitive-params, smooth, material) → three-d `InstancedMesh`. 600+ identical bricks
   collapse from 600 draws to a handful; doubly valuable on WebGL2. Selection outlines need
   per-instance exclusion, so selection state joins the grouping key.
2. **GPU wireframe** (1–2 days): cached line mesh instead of per-frame CPU projection via egui.
3. **Cull CPU overlays** (hours): frustum-test labels/dimensions before painting.

### Phase 4 — GPU where it actually wins
1. **"Render" button via the Blender MCP bridge** (days, high user value): export scene →
   Cycles+OptiX on the RTX 4080 SUPER, denoised. This is the industry split (raster viewport,
   RT final frames) and it *is* CUDA doing what CUDA is for. Ship before considering any
   embedded renderer (Embree later, if ever).
2. **Optional: standalone CUDA debris experiment** (timebox 2 weeks, zero product risk — serves
   the learning/curiosity motivation and produces first-party gate data): new `experiments/
   cuda-debris/` target, *not* linked into box3d or the modeler. Scope: 100k falling boxes,
   uniform-grid or CUB-radix-sort broad phase, XPBD or graph-colored Gauss-Seidel contacts,
   GL-interop instanced rendering. **Kill criterion:** step 100k bodies < 16 ms end-to-end
   *including readback*, A/B'd against box3d CPU on the same scene via `benchmark/`. Deliverable:
   a writeup here. Non-goals: no core changes, no modeler integration, no determinism promises.
3. **Production GPU physics gate:** only if a single-scene requirement >10k active bodies
   survives Phases 1–3 (full-house brick destruction might). Sequence: *prototype in CUDA*
   (Nsight-class tooling; the experiment above is that leg), *productionize in wgpu/WGSL*
   (portable across vendors and into the browser). Bar to beat: the AVX2+MT CPU path — Box2D
   v3's datum is 0.9 ms for 5k bodies. Reference implementations to watch: Dimforge wgrapier
   (WebGPU Soft-TGS, 93k-body browser demos), Jolt PR #1847 (vendor-neutral compute interface).
4. **GPU particles/smoke** (roadmap item in Instructions.md): instanced billboards now, wgpu
   compute later — lands the "use the GPU" instinct inside the app and still ships in browser.
5. **Cheap hedge, already mostly true:** keep solver data GPU-amenable — SoA, index-based
   references, pre-sized buffers, graph coloring (it is literally the batching scheme GPU
   Gauss-Seidel solvers use). Near-zero cost, preserves the option.
6. **Never:** CUDA viewport rasterization; DLSS on OpenGL (unsupported); per-frame CUDA-GL
   interop in the modeler.

## Acceptance target that ties it together
After Phase 1 (+ Phase 3.1 for draw calls): raise **MAX_BRICKS from 600 to ≥ 5,000** at 60 Hz
poke playback, with a profile proving where the old wall was. That converts this plan from
"you don't have a physics problem" into "your recorded physics ceiling lifts ~10x with CPU work
— and if the showcase then wants 50k bricks, the GPU gate (4.2/4.3) has first-party data."

## Why not CUDA in the solver, in one paragraph
CUDA physics solves a throughput problem (thousands of batched RL environments, state resident
on GPU, no per-frame readback) that an interactive editor does not have; interactively it adds a
fixed launch/sync/readback floor, breaks cross-platform determinism and the WASM build, locks to
NVIDIA, and the maintenance precedent is uniformly negative — while the CPU path still has ~6x
threading and roughly 2x SIMD headroom unused, and the modeler's felt slowness is caches being
rebuilt every frame, not arithmetic. CUDA rendering for a raster viewport is a category error;
the GPU's RT hardware is properly used through Cycles/OptiX for final frames via the existing
Blender bridge.

## Key sources
- PhysX 5 GPU rigid bodies: nvidia-omniverse.github.io/PhysX/physx/5.4.1/docs/GPURigidBodies.html
- MuJoCo MJX single-scene warning: mujoco.readthedocs.io/en/stable/mjx.html
- Box2D v3 "SIMD Matters" (0.9 ms / 5,050 bodies, AVX2+4 threads): box2d.org/posts/2024/08/simd-matters/
- Isaac Gym batching design: arXiv 2108.10470; Isaac Sim perf handbook (CPU faster for small scenes)
- Jolt on GPU physics ("CUDA won't be supported"): github.com/jrouwe/JoltPhysics/discussions/501, PR #1847
- Bullet 3 GPU pipeline (abandoned): multithreadingandvfx.org GPU rigid body course notes
- wgrapier (WebGPU Soft-TGS solver): wgmath.rs, dimforge.com 2025 year review
- CuRast CUDA rasterizer vs hardware raster: arXiv 2604.21749
- Kernel-launch/PCIe small-transfer overhead: Lustig & Martonosi HPCA'13; NVIDIA dev forums
- DLSS/Streamline has no OpenGL support: github.com/NVIDIA-RTX/Streamline
