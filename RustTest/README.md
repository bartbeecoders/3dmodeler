# Box3D Rust Test

A Rust test application for the [box3d](../README.md) physics engine. It shows a
rolling 3D landscape (box3d height field), a small house built from static box
hulls, a few trees, and a ragdoll created with the `CreateHuman` helper from the
box3d shared sample library.

![screenshot](screenshot.png)

## How it works

- **FFI**: `build.rs` uses `bindgen` to generate Rust bindings from
  `../include/box3d/box3d.h` and `../shared/human.h`, and links the prebuilt
  static libraries `../build/src/libbox3d.a` and `../build/shared/libshared.a`.
- **Physics**: the terrain is a `b3CreateHeightField` height field, the house is
  a static body with transformed box hulls (`b3MakeTransformedBoxHull`) plus
  triangular gable hulls (`b3CreateHull`), trees are static capsule + sphere
  shapes, and the ragdoll is the 14-bone `CreateHuman` from `libshared.a`.
- **Rendering**: [macroquad](https://crates.io/crates/macroquad) with simple CPU
  meshes and baked directional lighting. The same height function drives both
  the physics height field and the render mesh.

## Building

Box3d must be built first (static libs in `../build`):

```bash
cd .. && ./build.sh        # or: cmake -B build && cmake --build build
cd RustTest && cargo run
```

Requires `libclang` for bindgen (part of a normal clang install).

## Controls

| Input | Action |
| --- | --- |
| Left mouse drag | Orbit camera |
| Mouse wheel | Zoom |
| Space | Throw a ball from the camera |
| R | Respawn the ragdoll |

## Self-test

`SCREENSHOT_AFTER_FRAMES=150 cargo run` runs headless-style: it throws a ball at
frame 60, respawns the ragdoll at frame 90, saves `screenshot.png` after the
given frame count, and exits.
