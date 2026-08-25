//! img2model — proof-of-concept console app that turns an image into a 3D
//! model (GLB with PBR textures) using Microsoft TRELLIS.2.
//!
//! The heavy lifting is done by the TRELLIS.2 PyTorch pipeline running on the
//! GPU. This binary embeds the inference script, materializes it at runtime,
//! launches it inside the dedicated `trellis2` conda environment, and streams
//! structured progress back to the console.

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

/// The Python worker script, embedded in the binary at compile time.
const INFER_PY: &str = include_str!("../python/infer.py");

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Resolution {
    /// 512³ voxel grid — fastest, fits comfortably in 16 GB VRAM
    #[value(name = "512")]
    R512,
    /// 1024³ voxel grid
    #[value(name = "1024")]
    R1024,
    /// 512³ shape pass upscaled to 1024³ (cascade)
    #[value(name = "1024-cascade")]
    R1024Cascade,
    /// cascade up to 1536³ — needs the most VRAM
    #[value(name = "1536-cascade")]
    R1536Cascade,
}

impl Resolution {
    fn pipeline_type(self) -> &'static str {
        match self {
            Resolution::R512 => "512",
            Resolution::R1024 => "1024",
            Resolution::R1024Cascade => "1024_cascade",
            Resolution::R1536Cascade => "1536_cascade",
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "img2model", version, about = "Turn an image into a 3D model (GLB) with TRELLIS.2")]
struct Args {
    /// Input image(s) (PNG/JPG/WebP). An object on a clean background works best.
    #[arg(required_unless_present = "check")]
    images: Vec<PathBuf>,

    /// Output directory (GLB files are named after the input image)
    #[arg(short, long, default_value = "output")]
    output: PathBuf,

    /// Voxel resolution of the generated model
    #[arg(short, long, value_enum, default_value = "512")]
    resolution: Resolution,

    /// Random seed
    #[arg(short, long, default_value_t = 42)]
    seed: u32,

    /// Baked texture resolution (pixels)
    #[arg(long, default_value_t = 2048)]
    texture_size: u32,

    /// Target face count for mesh decimation
    #[arg(long, default_value_t = 500_000)]
    decimation: u32,

    /// Also render a turntable PBR video (mp4) next to each GLB
    #[arg(long)]
    video: bool,

    /// Attention backend override (flash_attn, sdpa, xformers, naive)
    #[arg(long)]
    attn: Option<String>,

    /// Verify the environment (conda env, CUDA, model imports) and exit
    #[arg(long)]
    check: bool,

    /// Root of the trellis-poc workspace (contains TRELLIS.2/ and miniforge3/).
    /// Defaults to $TRELLIS2_ROOT or an ancestor of the executable / cwd.
    #[arg(long)]
    root: Option<PathBuf>,
}

/// Progress messages emitted by the Python worker as `@@STAGE {json}` lines.
#[derive(Deserialize)]
struct StageMsg {
    stage: String,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

struct Env {
    python: PathBuf,
    env_dir: PathBuf,
    trellis_repo: PathBuf,
    infer_script: PathBuf,
}

/// Locate the workspace root: --root flag, $TRELLIS2_ROOT, or the first
/// ancestor of the exe/cwd containing both TRELLIS.2/ and miniforge3/.
fn find_root(cli_root: &Option<PathBuf>) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(r) = cli_root {
        candidates.push(r.clone());
    }
    if let Ok(r) = std::env::var("TRELLIS2_ROOT") {
        candidates.push(PathBuf::from(r));
    }
    let mut starts = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        starts.push(exe);
    }
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    for start in starts {
        let mut dir: &Path = &start;
        while let Some(parent) = dir.parent() {
            candidates.push(parent.to_path_buf());
            dir = parent;
        }
    }
    for c in candidates {
        if c.join("TRELLIS.2").is_dir() && c.join("miniforge3/envs/trellis2").is_dir() {
            return Ok(c);
        }
    }
    bail!(
        "could not locate the trellis-poc workspace (a directory containing \
         TRELLIS.2/ and miniforge3/envs/trellis2/). Pass --root or set TRELLIS2_ROOT."
    );
}

fn prepare_env(args: &Args) -> Result<Env> {
    let root = find_root(&args.root)?;
    let env_dir = root.join("miniforge3/envs/trellis2");
    let python = env_dir.join("bin/python");
    let trellis_repo = root.join("TRELLIS.2");
    if !python.is_file() {
        bail!("python not found at {}", python.display());
    }

    // Materialize the embedded worker script.
    let script_dir = root.join(".img2model");
    std::fs::create_dir_all(&script_dir)?;
    let infer_script = script_dir.join("infer.py");
    std::fs::write(&infer_script, INFER_PY)?;

    Ok(Env { python, env_dir, trellis_repo, infer_script })
}

fn base_command(env: &Env, attn: &Option<String>) -> Command {
    let mut cmd = Command::new(&env.python);
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.env("PYTHONPATH", &env.trellis_repo)
        .env("PYTHONUNBUFFERED", "1")
        .env("CUDA_HOME", &env.env_dir)
        .env("PATH", format!("{}/bin:{}", env.env_dir.display(), path))
        // RTX 4080 SUPER is Ada Lovelace (sm_89); pin it so JIT-compiled
        // extensions don't build for every architecture.
        .env("TORCH_CUDA_ARCH_LIST", "8.9")
        // conda-forge cuda-toolkit keeps headers/libs under targets/x86_64-linux;
        // needed when torch JIT-compiles extension plugins (e.g. nvdiffrast).
        .env("CPATH", env.env_dir.join("targets/x86_64-linux/include"))
        .env(
            "LIBRARY_PATH",
            format!(
                "{0}/targets/x86_64-linux/lib:{0}/targets/x86_64-linux/lib/stubs:{0}/lib",
                env.env_dir.display()
            ),
        )
        .env("CC", env.env_dir.join("bin/x86_64-conda-linux-gnu-cc"))
        .env("CXX", env.env_dir.join("bin/x86_64-conda-linux-gnu-c++"))
        .env(
            "NVCC_PREPEND_FLAGS",
            format!("-ccbin {}/bin/x86_64-conda-linux-gnu-c++", env.env_dir.display()),
        )
        .env("OPENCV_IO_ENABLE_OPENEXR", "1")
        .env("PYTORCH_CUDA_ALLOC_CONF", "expandable_segments:True")
        .current_dir(&env.trellis_repo);
    if let Some(attn) = attn {
        cmd.env("ATTN_BACKEND", attn);
    }
    cmd
}

fn run_check(env: &Env, attn: &Option<String>) -> Result<()> {
    println!("checking environment at {} ...", env.env_dir.display());
    let code = r#"
import torch, importlib
print(f"  torch {torch.__version__} | cuda {torch.version.cuda} | available: {torch.cuda.is_available()}")
if torch.cuda.is_available():
    p = torch.cuda.get_device_properties(0)
    print(f"  gpu: {p.name} | {p.total_memory/2**30:.1f} GiB | sm_{p.major}{p.minor}")
for m in ["trellis2", "o_voxel", "cumesh", "flex_gemm", "nvdiffrast", "flash_attn", "utils3d"]:
    try:
        importlib.import_module(m)
        print(f"  {m}: ok")
    except Exception as e:
        print(f"  {m}: MISSING ({type(e).__name__}: {e})")
"#;
    let status = base_command(env, attn).args(["-c", code]).status()?;
    if !status.success() {
        bail!("environment check failed");
    }
    Ok(())
}

fn run_inference(args: &Args, env: &Env, image: &Path, output: &Path) -> Result<()> {
    let started = Instant::now();
    println!();
    println!("=== {} -> {}", image.display(), output.display());

    let mut cmd = base_command(env, &args.attn);
    cmd.arg(&env.infer_script)
        .arg("--input").arg(std::fs::canonicalize(image)?)
        .arg("--output").arg(output)
        .arg("--pipeline-type").arg(args.resolution.pipeline_type())
        .arg("--seed").arg(args.seed.to_string())
        .arg("--texture-size").arg(args.texture_size.to_string())
        .arg("--decimation-target").arg(args.decimation.to_string())
        .arg("--trellis-repo").arg(&env.trellis_repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if args.video {
        cmd.arg("--video");
    }

    let mut child = cmd.spawn().context("failed to launch python worker")?;
    let stdout = child.stdout.take().unwrap();
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if let Some(json) = line.strip_prefix("@@STAGE ") {
            match serde_json::from_str::<StageMsg>(json) {
                Ok(msg) => {
                    let details: Vec<String> = msg
                        .extra
                        .iter()
                        .filter(|(k, _)| *k != "t")
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    println!(
                        "[{:7.1}s] {:<16} {}",
                        started.elapsed().as_secs_f32(),
                        msg.stage,
                        details.join("  ")
                    );
                }
                Err(_) => println!("{line}"),
            }
        } else {
            println!("  | {line}");
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("worker exited with {status}");
    }
    println!(
        "=== finished in {:.1}s: {}",
        started.elapsed().as_secs_f32(),
        output.display()
    );
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let env = prepare_env(&args)?;

    if args.check {
        return run_check(&env, &args.attn);
    }

    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("cannot create output dir {}", args.output.display()))?;

    let mut failures = 0;
    for image in &args.images {
        if !image.is_file() {
            eprintln!("skipping {}: not a file", image.display());
            failures += 1;
            continue;
        }
        let stem = image
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "model".into());
        let output = std::fs::canonicalize(&args.output)?.join(format!("{stem}.glb"));
        if let Err(e) = run_inference(&args, &env, image, &output) {
            eprintln!("FAILED {}: {e:#}", image.display());
            failures += 1;
        }
    }
    if failures > 0 {
        bail!("{failures} image(s) failed");
    }
    Ok(())
}
