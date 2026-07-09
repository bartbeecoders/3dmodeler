# CUDA investigation

> Original questions: How can we improve performance? Investigate how we can update the box3d
> library to use CUDA for physics calculations. Investigate if we can use CUDA for 3D rendering
> as well. Make a clean plan before we start implementing anything.

**Answered.** The full investigation result and the phased plan live in
[performance-plan.md](performance-plan.md).

Short version: CUDA inside the box3d solver and CUDA viewport rendering are both rejected on the
evidence (GPU rigid-body crossover is ~10k–100k bodies per scene; CUDA rasterization loses to the
hardware pipeline by 7–8x at CAD scale; the WASM build could never ship it). The plan instead
exploits ~10x of untouched CPU headroom (the modeler rebuilds the whole physics world every frame
during drags, runs box3d single-threaded, and the engine has no AVX2 path), and gives the GPU the
two jobs it is actually good at here: Cycles+OptiX photorealistic renders via the Blender MCP
bridge, and an optional contained CUDA debris experiment to produce first-party data for the
GPU-physics decision gate.
