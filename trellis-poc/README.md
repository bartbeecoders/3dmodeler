# trellis-poc — image → 3D model proof of concept

Turns a single image into a textured 3D model (GLB with PBR materials) using
[Microsoft TRELLIS.2](https://github.com/microsoft/TRELLIS.2), driven by a small
Rust console app. Targets the local RTX 4080 SUPER (16 GB VRAM).

## Layout

```
trellis-poc/
├── img2model/          Rust console app (cargo project)
│   └── python/infer.py Inference worker, embedded into the binary at compile time
├── TRELLIS.2/          Microsoft TRELLIS.2 checkout (pipeline code + assets)
├── miniforge3/         Self-contained conda install
│   └── envs/trellis2/  Python 3.10 + torch 2.6.0+cu124 + CUDA 12.4 toolchain
├── extensions/         Compiled CUDA extensions (nvdiffrast, nvdiffrec, CuMesh,
│                       FlexGEMM; o-voxel comes from TRELLIS.2/o-voxel)
├── build_extensions.sh Rebuilds all CUDA extensions into the env
├── examples/           Demo input images
└── output/             Generated .glb models land here
```

## Usage

```sh
cd img2model
cargo build --release

# sanity check: GPU, torch, all extension imports
./target/release/img2model --check

# single image → output/crown.glb
./target/release/img2model ../examples/crown.webp -o ../output

# batch, with a turntable video per model
./target/release/img2model ../examples/*.webp -o ../output --video

# higher resolution (more VRAM!), fixed seed, smaller texture
./target/release/img2model ../examples/pineapple.webp -r 1024-cascade --seed 7 --texture-size 1024
```

Options: `-r/--resolution 512|1024|1024-cascade|1536-cascade` (default 512 — the
safe choice for 16 GB), `--seed`, `--texture-size`, `--decimation`, `--video`,
`--attn <backend>` (`flash_attn` default, `sdpa` fallback), `--check`, `--root`.

## How it works

1. The Rust binary embeds `python/infer.py` (`include_str!`) and writes it to
   `.img2model/` at startup — the binary is self-contained apart from the env.
2. It locates the workspace (this directory) via `--root`, `$TRELLIS2_ROOT`, or
   by walking up from the executable/cwd.
3. It spawns `miniforge3/envs/trellis2/bin/python infer.py ...` with
   `PYTHONPATH=TRELLIS.2`, `TORCH_CUDA_ARCH_LIST=8.9`, and
   `PYTORCH_CUDA_ALLOC_CONF=expandable_segments:True` (keeps 16 GB workable).
4. The worker loads `microsoft/TRELLIS.2-4B` (auto-downloaded to the HF cache on
   first run), runs the pipeline, and exports a GLB via `o_voxel.postprocess.to_glb`
   (decimation + texture baking). Progress streams back as `@@STAGE {json}` lines
   which the Rust side renders with timings.

## Notes for 3dmodeler integration

- The Rust↔Python boundary is a subprocess + line protocol; the same pattern can
  be lifted into 3dmodeler directly (spawn from the app, parse `@@STAGE` events
  for a progress bar, load the resulting GLB).
- TRELLIS.2 officially targets ≥24 GB VRAM; on 16 GB stick to the `512`
  pipeline. `1024-cascade` may work depending on scene complexity — it reduces
  resolution automatically when the token budget overflows.
- The model is ~13 GB on disk (HF cache) and takes ~10-20 s to load; a long-lived
  worker process (load once, many generations) is the obvious next step for
  interactive use.
