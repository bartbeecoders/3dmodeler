"""TRELLIS.2 image-to-3D inference worker.

This script is embedded in the modeler app (see `crate::trellis`) and executed
inside the `trellis2` conda environment of the trellis-poc workspace. It
communicates progress back to the Rust host via `@@STAGE <json>` lines on
stdout. It originated as `ShapeCreator/img2model/python/infer.py` — keep the
two in sync when the protocol changes.
"""
import argparse
import json
import os
import sys
import time

os.environ.setdefault("OPENCV_IO_ENABLE_OPENEXR", "1")
os.environ.setdefault("PYTORCH_CUDA_ALLOC_CONF", "expandable_segments:True")


def stage(name, **extra):
    payload = {"stage": name, "t": time.time()}
    payload.update(extra)
    print(f"@@STAGE {json.dumps(payload)}", flush=True)


def main():
    parser = argparse.ArgumentParser(description="TRELLIS.2 image-to-3D worker")
    parser.add_argument("--input", required=True, help="input image path")
    parser.add_argument("--output", required=True, help="output .glb path")
    parser.add_argument("--pipeline-type", default="512",
                        choices=["512", "1024", "1024_cascade", "1536_cascade"])
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--texture-size", type=int, default=2048)
    parser.add_argument("--decimation-target", type=int, default=500000)
    parser.add_argument("--model", default="microsoft/TRELLIS.2-4B")
    parser.add_argument("--video", action="store_true",
                        help="also render a turntable PBR video next to the GLB")
    parser.add_argument("--export-textures", action="store_true",
                        help="also save the baked texture maps as PNGs next to the GLB")
    parser.add_argument("--webp", action="store_true",
                        help="store GLB textures as WebP (EXT_texture_webp; smaller file, "
                             "but not every glTF loader supports it). Default is PNG.")
    parser.add_argument("--trellis-repo", required=True,
                        help="path to the TRELLIS.2 checkout (for assets/hdri)")
    args = parser.parse_args()

    stage("startup", python=sys.version.split()[0])

    import torch
    from PIL import Image

    stage("torch_loaded", torch=torch.__version__, cuda=torch.version.cuda,
          gpu=torch.cuda.get_device_name(0) if torch.cuda.is_available() else "NONE")
    if not torch.cuda.is_available():
        stage("error", message="CUDA is not available in this environment")
        sys.exit(2)

    if "ATTN_BACKEND" not in os.environ:
        try:
            import flash_attn  # noqa: F401
        except ImportError:
            os.environ["ATTN_BACKEND"] = "sdpa"
            stage("attn_fallback", backend="sdpa")

    from trellis2.pipelines import Trellis2ImageTo3DPipeline
    import o_voxel

    stage("loading_model", model=args.model)
    t0 = time.time()
    pipeline = Trellis2ImageTo3DPipeline.from_pretrained(args.model)
    pipeline.cuda()
    stage("model_loaded", seconds=round(time.time() - t0, 1))

    image = Image.open(args.input)
    stage("generating", input=args.input, pipeline_type=args.pipeline_type,
          seed=args.seed)
    t0 = time.time()
    mesh = pipeline.run(
        image,
        seed=args.seed,
        pipeline_type=args.pipeline_type,
    )[0]
    torch.cuda.empty_cache()
    stage("generated", seconds=round(time.time() - t0, 1),
          vertices=int(mesh.vertices.shape[0]), faces=int(mesh.faces.shape[0]))

    mesh.simplify(16777216)  # nvdiffrast limit

    if args.video:
        stage("rendering_video")
        t0 = time.time()
        import cv2
        import imageio
        from trellis2.utils import render_utils
        from trellis2.renderers import EnvMap
        hdri = os.path.join(args.trellis_repo, "assets", "hdri", "forest.exr")
        envmap = EnvMap(torch.tensor(
            cv2.cvtColor(cv2.imread(hdri, cv2.IMREAD_UNCHANGED), cv2.COLOR_BGR2RGB),
            dtype=torch.float32, device="cuda",
        ))
        video = render_utils.make_pbr_vis_frames(
            render_utils.render_video(mesh, envmap=envmap))
        video_path = os.path.splitext(args.output)[0] + ".mp4"
        imageio.mimsave(video_path, video, fps=15)
        stage("video_done", seconds=round(time.time() - t0, 1), path=video_path)

    stage("exporting_glb", texture_size=args.texture_size,
          decimation_target=args.decimation_target)
    t0 = time.time()
    glb = o_voxel.postprocess.to_glb(
        vertices=mesh.vertices,
        faces=mesh.faces,
        attr_volume=mesh.attrs,
        coords=mesh.coords,
        attr_layout=mesh.layout,
        voxel_size=mesh.voxel_size,
        aabb=[[-0.5, -0.5, -0.5], [0.5, 0.5, 0.5]],
        decimation_target=args.decimation_target,
        texture_size=args.texture_size,
        remesh=True,
        remesh_band=1,
        remesh_project=0,
        verbose=True,
    )
    glb.export(args.output, extension_webp=args.webp)

    if args.export_textures:
        # to_glb returns a trimesh.Trimesh with a PBRMaterial holding the baked
        # maps: baseColorTexture is RGBA, metallicRoughnessTexture is packed
        # per glTF convention (G = roughness, B = metallic).
        mat = glb.visual.material
        stem = os.path.splitext(args.output)[0]
        textures = {}
        if mat.baseColorTexture is not None:
            path = f"{stem}_basecolor.png"
            mat.baseColorTexture.save(path)
            textures["basecolor"] = path
        if mat.metallicRoughnessTexture is not None:
            mr = mat.metallicRoughnessTexture
            path = f"{stem}_metallic_roughness.png"
            mr.save(path)
            textures["metallic_roughness"] = path
            _, g, b = mr.convert("RGB").split()[:3]
            g.save(f"{stem}_roughness.png")
            b.save(f"{stem}_metallic.png")
            textures["roughness"] = f"{stem}_roughness.png"
            textures["metallic"] = f"{stem}_metallic.png"
        if getattr(mat, "normalTexture", None) is not None:
            path = f"{stem}_normal.png"
            mat.normalTexture.save(path)
            textures["normal"] = path
        stage("textures_saved", **textures)

    stage("done", seconds=round(time.time() - t0, 1), path=args.output,
          size_mb=round(os.path.getsize(args.output) / 1e6, 2))


if __name__ == "__main__":
    main()
