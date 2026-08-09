# Physics performance — house-test8 (7,591 dynamic bricks)

Investigation of "running the physics is slow" on
`~/Documents/3dmodels/house-test8.bee3d`, measured 2026-07-29 on a Ryzen 9 7950X
(16 cores / 32 threads, 62 GB), Rust release profile, `libbox3d.a` Release `-O3
-DNDEBUG` (SSE2 build unless stated).

Follows on from `Vibecoding/performance-plan.md` (Phases 0–3, v0.2.28), which
raised the brick ceiling to 5,000 bodies. This scene is 7,591 — past that
ceiling — and the bottleneck has moved somewhere new.

## Verdict in three lines

1. **The app runs ~14 physics steps per rendered frame.** One step costs ~75 ms,
   so a frame costs ~1.1 s → **0.9 fps**, with the simulation still only
   advancing at 0.23× real time. The fixed-step accumulator turns a 13 fps
   problem into a 0.9 fps one. Capping steps-per-frame is a ~10-line fix.
2. **Nothing ever goes to sleep.** The house free-falls at play (6 mm mortar
   gaps), collapses into rubble with ~207,000 contacts, and then 7,300 of 7,591
   bodies stay awake *forever*, jittering just above the 0.05 m/s sleep
   threshold. Raising the threshold to 0.2 m/s makes the pile sleep after ~5 s
   of sim: **75 ms → 7 µs per step.**
3. **Everything else is noise.** The solver is 94% of the frame; all app-side
   per-frame work (world transforms, write-back, material resolve, instance
   hashing) totals ~5 ms. Rendering is already instanced and is not the problem.

## The scene

| | |
|---|---|
| objects | 7,591 — 7,589 cubes + 1 floor + 1 sphere |
| dynamic | **7,591 (all of them)** |
| folders | 8 brick folders (walls + window glass), from `break_into_bricks` |
| parents / groups | none — every object is a root |
| brick sizes | 0.38 × 0.19 × 0.20 m down to **0.06 × 0.014 × 0.014 m** |
| density | 1.0 on every body |
| initial force | non-zero on 2 objects |
| file | 17.4 MB JSON |

Two structural facts matter more than the body count:

- **`break_into_bricks` leaves a 6 mm mortar gap** (`object_ops.rs:535,693`,
  `const GAP: f32 = 0.006`, "keeps stacked bricks collision-free"). At rest the
  scene has only **~2,583 touching/overlapping brick pairs**. Press play and
  every brick free-falls 6 mm, the walls disintegrate unprompted, and the pile
  settles at **~207,000 contacts** — 80× more contact work than the standing
  house would need.
- **A ~28:1 size spread in one pile** (0.38 m bricks against 1.4 cm window-glass
  bricks) ≈ a 20,000:1 mass ratio at uniform density. That is exactly the
  configuration that converges slowly and keeps micro-jittering.

## Measured: where the time goes

`cargo test --release -p modeler-app -- --ignored --nocapture perf_scene_file`

```
objects 7591, dynamic 7591
json parse                        19–25 ms      (once, at load)
scene restore                       4–6 ms      (once)
static mirror build (edit mode)    29–34 ms     (once)
play() build_simulation            35 ms        (once, per play)
stop()                             28–41 ms     (once, per stop)

per frame, b3World_Step         41–82 ms   <-- 94% of the frame
per frame, world_transforms()      1.0 ms
per frame, scene write-back        1.7 ms
per frame, 2x material resolve  1.6–2.3 ms
per frame, instance sig hash      0.27 ms
```

box3d counters/profile for a settled frame:

```
bodies 7592  shapes 7592  contacts 206681  awake-contacts 206187  islands 222
profile ms: step 57.6  pairs 3.2  collide 7.0  solve 47.4
            (prepare 12.1  warmStart 6.5  solveImpulses 11.4  relax 14.1  store 2.0)
            refit 0.7  sleep 0.0
```

Solve time is spread evenly across prepare / warm-start / solve / relax — i.e.
it is **proportional to contact count**, with no single hot stage to optimise.
Halving contacts halves the step. There is no algorithmic bug in the solver
path; there are simply 207k live contacts.

### The accumulator amplifier (the actual "slow")

`PhysicsMirror::update` (`physics.rs:1187-1204`) accumulates frame time and runs
`while accumulator >= 1/60`, with the accumulator clamped to 0.25 s — up to
**15 fixed steps per rendered frame**. Feeding it the real frame durations it
produces (`perf_frame_loop` probe):

```
  frame   1: dt in    16.7 ms ->  1 steps, physics    77.0 ms  (13.0 fps)
  frame   2: dt in    77.0 ms ->  4 steps, physics    78.4 ms  (12.8 fps)
  frame   3: dt in    78.4 ms ->  5 steps, physics   145.1 ms  ( 6.9 fps)
  frame   4: dt in   145.1 ms ->  9 steps, physics   306.6 ms  ( 3.3 fps)
  frame   5: dt in   306.6 ms -> 14 steps, physics   599.0 ms  ( 1.7 fps)
  ...
  frame  25: dt in  1104.0 ms -> 14 steps, physics  1118.9 ms  ( 0.9 fps)
  => sim advanced 5.22 s in 22.53 s of wall clock (0.23x real time)
```

It spirals in five frames and parks at 0.9 fps. Note the sim is in slow motion
*anyway* (0.23×) — the catch-up buys nothing and costs 14× the frame rate.

### Sleep: measured, and it is the biggest single lever

`BEE3D_SLEEP_THRESHOLD=<t> cargo test --release -p modeler-app -- --ignored
--nocapture perf_sleep_probe` — 600 steps (10 s of sim), one config per process,
4 runs each:

| body sleep threshold | outcome |
|---|---|
| **0.05 m/s (box3d default)** | awake in 3 of 4 runs: 67, 80, 76 ms/step at t=10 s, 7,261–7,376 bodies still awake. Slept in 1 of 4. |
| **0.2 m/s** | slept in **4 of 4** runs, ~4.7 s of sim in: **6.8–10.4 µs/step**, 1 body awake, 0 awake contacts. |

The pile sits right on the sleep boundary, so whether the scene becomes instant
or stays at 0.9 fps is currently a coin flip. (Related: the multithreaded solver
is not run-to-run reproducible here — contact counts vary by ±15% between
identical runs.)

Trace at 0.2 m/s — the collapse still costs full price, only the settled state
is free:

```
    t=0.50s awake  7591  step   46.74ms
    t=2.50s awake  7474  step   77.51ms
    t=4.50s awake  7261  step  104.38ms
    t=5.00s awake     3  step  223.96µs
    t=10.0s awake     1  step   37.27µs
```

**Tested and rejected:** calling `b3Body_SetAwake(body, false)` on every body at
play does *not* stick — broad-phase pair creation wakes all 7,591 back up within
0.5 s of sim. Bodies must be **static**, not asleep, to stay quiet.

### Only part of the house dynamic

Same scene, N nearest bodies dynamic, the rest static (60-step average):

| dynamic bodies | step | contacts |
|---|---|---|
| 250 | **0.64 ms** | 1,351 |
| 1,000 | **1.95 ms** | 6,844 |
| 2,500 | **4.48 ms** | 22,403 |
| 7,591 (today) | 41–82 ms | 207,000 |

2,500 active bodies fit in a 16.7 ms frame with 3× headroom. The cost is
super-linear in body count because contacts-per-body rises as the rubble packs.

### Worker count × substeps (settled pile, 60-step average)

| workers | substeps 1 | 2 | 4 (current) |
|---|---|---|---|
| 0 (serial) | 113 ms | 110 ms | 120 ms |
| 4 | 49 ms | 47 ms | 51 ms |
| 8 | 36 ms | 39 ms | 49 ms |
| 16 (current) | **34 ms** | 42 ms | 46 ms |

Threading is already on and already right (`THREADED_BODY_THRESHOLD = 500`,
16 workers). Serial would be 3× worse. Substeps 4 → 2 is worth ~10%, 4 → 1
~25%, at a stacking-stability cost.

### AVX2 box3d build

`BOX3D_AVX2=ON` library swapped in for the default SSE2 one, 120 steps from
play, 3 runs each:

| build | mean step |
|---|---|
| SSE2 (shipped) | 58.0 / 59.6 / 59.0 ms → **58.8 ms** |
| AVX2 (`build-avx2/`) | 54.7 / 54.6 / 48.7 ms → **52.7 ms** |

**~10%**, free, on any x86-64-v3 machine. Consistent with the Phase 2.2 numbers.

## Status: 1, 2 and 5 are implemented (v0.2.54)

| # | change | where |
|---|---|---|
| 1 | `MAX_STEPS_PER_FRAME = 2`, surplus dropped, `slow_motion()` in the footer | `physics.rs:83`, `update()`, `ui.rs` status bar |
| 2 | `body_def.sleepThreshold = 0.2` on every dynamic body | `physics.rs` `create_entry` |
| 5 | substeps 4 → 2 above 1,000 dynamic bodies | `physics.rs` `build_simulation` |

Re-measured on the same scene after the change:

```
frame loop:  no spiral. 14.7 fps -> 20-32 fps through the collapse, worst
             frame 5.0 fps at peak contacts (was: parked at 0.9 fps)
             sim advanced 0.82 s in 1.63 s  =  0.50x real time (was 0.23x)

sleep:       4 of 4 runs sleep, 5.0-6.5 s of sim in (was 1 of 4)
             settled step 6.6-9.0 us  (was 67-80 ms)
             collapse-phase step 25-39 ms mean (was 41-82 ms)
```

Remaining: 3 (static-until-hit) is still the architectural fix, then 6, 7, 8.

## Speed suggestions, ranked

Effort is rough; "gain" is on this scene, measured unless marked *(estimate)*.

### 1. Cap fixed steps per frame — 0.9 fps → ~13 fps
**Effort: ~10 lines. Risk: none. Gain: 14×.**
In `PhysicsMirror::update`, replace the unbounded catch-up loop with a hard cap
(1 step/frame, or 2) and drop the surplus instead of banking it
(`self.accumulator = self.accumulator.min(FIXED_DT)` after the loop). The
simulation already runs in slow motion under load; the accumulator only makes
the viewport unusable. Show "slow motion 0.2×" in the footer when steps are
being dropped so the behaviour is visible rather than mysterious.

### 2. Raise the sleep threshold — settled rubble becomes free
**Effort: 1–2 lines + tuning. Risk: low. Gain: 75 ms → 7 µs once settled.**
Call `b3Body_SetSleepThreshold(body, 0.2)` when creating simulation bodies (or
scale it with body extent — small bricks need a proportionally larger threshold
to be considered at rest). 0.2 m/s = 3.3 mm/frame at 60 Hz; watch for bricks
freezing while still visibly creeping and back off if it looks wrong. The
cleaner variant, if the jitter itself is the target, is `b3World_SetContactTuning`
(higher damping ratio / lower `contactSpeed`) so the pile stops jittering rather
than being declared asleep while it does — more work, better looking.

### 3. Keep untouched bricks static; promote on impact
**Effort: 1–2 days. Risk: medium. Gain: 41–82 ms → 0.6–4.5 ms.**
The architectural fix, and the one that makes a 7.6k-brick house genuinely
interactive. At play, create every `dynamic` body as **static**. On a poke /
impulse / initial force, promote bodies within a radius to dynamic
(`b3Body_SetType`), and cascade: when a dynamic body touches a static
simulation body, promote that one too. Demote back to static after N seconds
below the sleep threshold. Measured budget: ~2,500 simultaneously active bodies
fits 60 Hz. This subsumes suggestion 2 for everything the player never hits.

### 4. Stop the house collapsing on its own
**Effort: hours to days. Risk: medium (changes brick layout). Gain: 207k → ~2.6k contacts.**
The 6 mm mortar gap is why pressing play demolishes an untouched house. Options,
cheapest first: (a) place bricks in contact and keep the gap only in the render
mesh (a visual mortar line, not a physical one); (b) settle the scene silently
for a few steps at play with heavy damping before handing control over; (c) do
nothing here and rely on 3, which makes the gap irrelevant because unhit bricks
never fall. **(c) is the recommendation** — but if the intended experience is
"the house stands until I hit it", (a) is the honest fix.

### 5. Scale substeps to body count — ~10–25%
**Effort: minutes. Risk: low (stack stability).**
`SUBSTEPS` is a hard 4. Use 4 below ~1,000 dynamic bodies and 2 above; measured
46 → 42 ms at 16 workers on this scene. Do not go to 1 for stacked geometry.

### 6. Ship the AVX2 box3d build — ~10%
**Effort: half a day. Risk: low.**
`BOX3D_AVX2=ON` already exists and is already ~10% faster here. What is missing
is selection: either a `box3d-sys` cargo feature that points at `build-avx2/`,
or (better) runtime CPU detection with the SSE2 library as fallback, since AVX2
is not universal and SSE2 stays the deterministic reference.

### 7. Trim the ~5 ms of per-frame app work
**Effort: hours. Risk: low. Gain: ~5 ms → ~1 ms.**
Only 6% of today's frame, but it becomes the ceiling once 1–4 land:
- **Material resolved twice per object per frame.** `SceneRender::sync` calls
  `render_material` in `instance_key` *and* again in `instance_color`
  (`scene_render.rs:253, 487`). Each call runs `object_material_for_render`,
  which does a recursive `world_transform(id)` walk and returns a cloned
  `Material`. Resolve once and pass it down; take the transform from the
  `worlds` map the function already built. **1.6–2.3 ms/frame.**
- **`world_transforms()` runs 3× per frame** (physics write-back, `SceneRender::sync`,
  `overlay`), each building a fresh 7.6k-entry `HashMap`. Compute once per frame
  and pass it, or memoise on `scene.version()`. **~2 ms/frame.**
- **Write-back bumps the scene version 7,591× per frame.** `set_world_transform`
  → `object_mut` increments the global version per body and re-looks-up the
  parent. Every object in this scene is a root: add a fast path that assigns
  `object.transform` directly for parentless bodies and bumps the version once
  per frame. **~1 ms/frame**, plus it stops needlessly invalidating
  version-keyed caches mid-frame.
- **Instance-signature hash while simulating.** `sync_group` hashes id + 16
  matrix floats + colour for all 7.6k members to decide whether to re-upload —
  during playback the answer is always "yes". Skip the hash when the sim is
  playing. **~0.3 ms/frame.**

### 8. Step physics off the render thread
**Effort: days. Risk: medium-high. Gain: viewport stays at 60 fps regardless.**
Run the world on a worker thread, publish transforms with a double buffer, and
let the renderer draw the latest completed state. Orbit/pan/UI stay responsive
even when a step costs 75 ms. Complements 1 (which decides *how much* sim to
run); this decides *who waits for it*.

### 9. Budget and size limits
**Effort: hours. Risk: none (UX).**
`MAX_BRICKS = 5000` is enforced *per break operation* — this scene reached 7,591
across eight walls with no warning. Add a scene-wide dynamic-body budget with a
warning at play, and a minimum brick dimension: 1.4 cm bricks in the same pile as
0.38 m ones give a ~20,000:1 mass ratio, which is a real solver-convergence cost
on top of the body count.

## Not worth doing

- **More worker threads.** 16 workers is already the best measured point;
  serial is 3× worse. Nothing left here.
- **GPU / CUDA physics.** Unchanged from `Vibecoding/performance-plan.md`: the
  CPU path has ~100× of headroom in this scene from sleeping and static bodies
  alone, and none of it requires new hardware.
- **Render optimisation.** 7.5k cubes already collapse into a handful of
  instanced draw calls (Phase 3.1). Nothing here is the bottleneck.
- **Solver micro-optimisation.** The profile is flat across solve stages — this
  is a contact-count problem, not a hot-loop problem.

## Suggested order

1 and 2 first (an afternoon, and the scene goes from 0.9 fps to interactive
once settled), then 5 and 6 (another afternoon, ~20% off the collapse phase),
then 3 (the real fix), with 7 and 8 as the follow-up once the solver stops
dominating.

## Reproducing

Three `#[ignore]`d probes in `crates/modeler-app/src/physics.rs`, alongside the
existing `perf_baseline`:

```bash
cd 3dmodeler
# full breakdown: load, mirror, play, per-frame split, counters/profile,
# 10 s settling trace, subset-dynamic sweep, workers x substeps A/B
cargo test --release -p modeler-app -- --ignored --nocapture perf_scene_file

# the accumulator spiral, fed with real frame durations
cargo test --release -p modeler-app -- --ignored --nocapture perf_frame_loop

# sleep behaviour — ONE config per process (the sim is not reproducible
# run-to-run, and configs cannot be compared inside one process)
BEE3D_SLEEP_THRESHOLD=0.2 BEE3D_STEPS=600 \
  cargo test --release -p modeler-app -- --ignored --nocapture perf_sleep_probe
```

All three default to `~/Documents/3dmodels/house-test8.bee3d`; override with
`BEE3D_PERF_SCENE=/path/to/scene.bee3d`. `perf_sleep_probe` also takes
`BEE3D_START_ASLEEP=1` (the rejected experiment above) and `BEE3D_QUIET=1`.

For the AVX2 A/B: `cmake --build build-avx2`, copy
`build-avx2/src/libbox3d.a` over `build/src/libbox3d.a`, `touch
include/box3d/box3d.h` to force a relink, and run — then restore the original
library.
