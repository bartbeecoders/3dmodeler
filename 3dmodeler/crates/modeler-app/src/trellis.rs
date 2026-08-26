//! TRELLIS.2 image → 3D model conversion (native only).
//!
//! Turns a dropped picture into a textured GLB using Microsoft TRELLIS.2 on
//! the local GPU, then feeds the GLB through [`crate::gltf_import`] so it
//! merges into the scene like any other model. The pattern mirrors
//! [`crate::blend`] driving a headless Blender: the heavy tool lives outside
//! the app, an embedded worker script talks to it, and progress arrives via
//! a poll the status bar reads.
//!
//! The runtime workspace is the `trellis-poc/` directory (TRELLIS.2 checkout
//! plus a self-contained conda env — see its README): [`workspace`] finds it
//! via `$TRELLIS2_ROOT` or by walking up from the executable/cwd, and the
//! conversion UI simply isn't offered on machines without it. The worker
//! script and the `@@STAGE` line protocol are shared with the standalone
//! `ShapeCreator/img2model` CLI, where they originated.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

const INFER_PY: &str = include_str!("trellis_scripts/infer.py");

/// Voxel resolution passed to the pipeline; 512 is the safe choice for a
/// 16 GB GPU (and what the PoC defaults to).
const PIPELINE_TYPE: &str = "512";

static PROGRESS: Mutex<Option<String>> = Mutex::new(None);
/// One conversion chain at a time — the GPU can't run two anyway.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Latest progress/status line ("crown.webp — generating 3D model…").
pub fn poll_progress() -> Option<String> {
    PROGRESS.lock().ok().and_then(|mut p| p.take())
}

fn set_progress(message: String) {
    if let Ok(mut p) = PROGRESS.lock() {
        *p = Some(message);
    }
}

pub fn busy() -> bool {
    BUSY.load(Ordering::SeqCst)
}

/// The trellis-poc workspace on this machine, if any. Cached — the drop
/// dialog asks every frame.
pub fn workspace() -> Option<&'static Path> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(find_root).as_deref()
}

/// A workspace holds the TRELLIS.2 checkout and the dedicated conda env.
fn is_workspace(dir: &Path) -> bool {
    dir.join("TRELLIS.2").is_dir() && dir.join("miniforge3/envs/trellis2").is_dir()
}

/// `$TRELLIS2_ROOT`, or the first ancestor of the executable / cwd that is
/// (or contains) a `trellis-poc` workspace.
fn find_root() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(root) = std::env::var("TRELLIS2_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    for start in [std::env::current_exe().ok(), std::env::current_dir().ok()] {
        let Some(start) = start else { continue };
        candidates.extend(start.ancestors().skip(1).map(Path::to_path_buf));
    }
    candidates
        .iter()
        .flat_map(|c| [c.clone(), c.join("trellis-poc")])
        .find(|c| is_workspace(c))
}

/// Convert images to 3D models on a background thread; each finished GLB
/// imports into the scene through the normal glTF path. Progress lands in
/// [`poll_progress`].
pub fn convert(images: Vec<PathBuf>) {
    let Some(root) = workspace() else {
        set_progress("TRELLIS workspace (trellis-poc/) not found".into());
        return;
    };
    if BUSY.swap(true, Ordering::SeqCst) {
        set_progress("a TRELLIS conversion is already running — try again when it finishes".into());
        return;
    }
    std::thread::spawn(move || {
        for image in images {
            let name = image
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image".into());
            match run_one(root, &image, &name) {
                Ok(glb) => {
                    set_progress(format!("TRELLIS {name} — importing the model…"));
                    crate::gltf_import::import_path(glb);
                }
                Err(e) => set_progress(format!("TRELLIS {name} failed: {e}")),
            }
        }
        BUSY.store(false, Ordering::SeqCst);
    });
}

/// One image → `<workspace>/output/<stem>.glb`, streaming stage progress.
fn run_one(root: &Path, image: &Path, name: &str) -> Result<PathBuf, String> {
    use std::io::BufRead;

    let image = std::fs::canonicalize(image).map_err(|e| format!("input: {e}"))?;
    let stem = image
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model".into());
    let out_dir = root.join("output");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("output dir: {e}"))?;
    let glb = out_dir.join(format!("{stem}.glb"));

    // materialize the worker script (own dir — the img2model CLI keeps its
    // identical copy in .img2model/)
    let script_dir = root.join(".modeler");
    std::fs::create_dir_all(&script_dir).map_err(|e| format!("script dir: {e}"))?;
    let script = script_dir.join("infer.py");
    std::fs::write(&script, INFER_PY).map_err(|e| format!("script: {e}"))?;
    let stderr_log = script_dir.join("worker-stderr.log");

    set_progress(format!("TRELLIS {name} — starting worker…"));
    let mut child = worker_command(root)?
        .arg(&script)
        .arg("--input")
        .arg(&image)
        .arg("--output")
        .arg(&glb)
        .arg("--pipeline-type")
        .arg(PIPELINE_TYPE)
        .arg("--trellis-repo")
        .arg(root.join("TRELLIS.2"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::fs::File::create(&stderr_log).map_err(|e| format!("stderr log: {e}"))?)
        .spawn()
        .map_err(|e| format!("failed to launch the python worker: {e}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    for line in std::io::BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if let Some(message) = stage_message(&line) {
            set_progress(format!("TRELLIS {name} — {message}"));
        }
    }
    let status = child.wait().map_err(|e| format!("waiting for the worker: {e}"))?;
    if !status.success() {
        return Err(format!("worker exited with {status}: {}", stderr_tail(&stderr_log)));
    }
    glb.is_file().then_some(glb).ok_or_else(|| "worker wrote no GLB".into())
}

/// The conda-env python with the environment TRELLIS.2 needs — a copy of
/// what the img2model CLI sets (CUDA paths for JIT-compiled extensions, the
/// conda compilers, sm_89 for the local GPU).
fn worker_command(root: &Path) -> Result<std::process::Command, String> {
    let env_dir = root.join("miniforge3/envs/trellis2");
    let python = env_dir.join("bin/python");
    if !python.is_file() {
        return Err(format!("python not found at {}", python.display()));
    }
    let mut cmd = std::process::Command::new(python);
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.env("PYTHONPATH", root.join("TRELLIS.2"))
        .env("PYTHONUNBUFFERED", "1")
        .env("CUDA_HOME", &env_dir)
        .env("PATH", format!("{}/bin:{}", env_dir.display(), path))
        .env("TORCH_CUDA_ARCH_LIST", "8.9")
        .env("CPATH", env_dir.join("targets/x86_64-linux/include"))
        .env(
            "LIBRARY_PATH",
            format!(
                "{0}/targets/x86_64-linux/lib:{0}/targets/x86_64-linux/lib/stubs:{0}/lib",
                env_dir.display()
            ),
        )
        .env("CC", env_dir.join("bin/x86_64-conda-linux-gnu-cc"))
        .env("CXX", env_dir.join("bin/x86_64-conda-linux-gnu-c++"))
        .env(
            "NVCC_PREPEND_FLAGS",
            format!("-ccbin {}/bin/x86_64-conda-linux-gnu-c++", env_dir.display()),
        )
        .env("OPENCV_IO_ENABLE_OPENEXR", "1")
        .env("PYTORCH_CUDA_ALLOC_CONF", "expandable_segments:True")
        .current_dir(root.join("TRELLIS.2"));
    Ok(cmd)
}

/// `@@STAGE {json}` line → status-bar text; `None` for chatter worth hiding.
fn stage_message(line: &str) -> Option<String> {
    let json = line.strip_prefix("@@STAGE ")?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let stage = value["stage"].as_str()?;
    Some(match stage {
        "startup" | "torch_loaded" => "starting worker…".into(),
        "loading_model" => "loading the TRELLIS model (first run downloads ~16 GB)…".into(),
        "model_loaded" => "model loaded — generating 3D model…".into(),
        "generating" => "generating 3D model…".into(),
        "generated" => match value["faces"].as_u64() {
            Some(faces) => format!("generated {faces} faces — baking textures…"),
            None => "generated — baking textures…".into(),
        },
        "exporting_glb" => "baking textures & exporting…".into(),
        "done" => "model ready".into(),
        "error" => format!("error: {}", value["message"].as_str().unwrap_or("unknown")),
        "attn_fallback" => return None,
        other => other.replace('_', " "),
    })
}

/// Last few stderr lines, for a one-line failure message.
fn stderr_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let tail: Vec<&str> = text.lines().rev().take(4).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    if tail.is_empty() {
        format!("(no stderr; see {})", log.display())
    } else {
        tail.join(" | ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_lines_become_status_text() {
        assert_eq!(
            stage_message(r#"@@STAGE {"stage":"generated","faces":480122,"seconds":44.0}"#),
            Some("generated 480122 faces — baking textures…".into())
        );
        assert_eq!(
            stage_message(r#"@@STAGE {"stage":"error","message":"CUDA is not available"}"#),
            Some("error: CUDA is not available".into())
        );
        assert_eq!(stage_message(r#"@@STAGE {"stage":"attn_fallback","backend":"sdpa"}"#), None);
        assert_eq!(stage_message("random worker chatter"), None);
        assert_eq!(
            stage_message(r#"@@STAGE {"stage":"rendering_video"}"#),
            Some("rendering video".into())
        );
    }

    #[test]
    fn workspace_detection_needs_both_markers() {
        let dir = std::env::temp_dir().join(format!("trellis-ws-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("TRELLIS.2")).unwrap();
        assert!(!is_workspace(&dir));
        std::fs::create_dir_all(dir.join("miniforge3/envs/trellis2")).unwrap();
        assert!(is_workspace(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Full GPU round trip on machines that have the workspace: image →
    /// TRELLIS → GLB → parsed scene. Takes minutes — run explicitly with
    /// `cargo test -- --ignored trellis`.
    #[test]
    #[ignore = "runs the real TRELLIS pipeline on the GPU (~2 min)"]
    fn converts_an_example_image_end_to_end() {
        let Some(root) = workspace() else {
            eprintln!("no trellis-poc workspace — skipping");
            return;
        };
        let image = root.join("examples/crown.webp");
        assert!(image.is_file(), "example image missing: {}", image.display());
        let glb = run_one(root, &image, "crown.webp").expect("conversion succeeds");
        let bytes = std::fs::read(&glb).expect("glb readable");
        let scene = crate::gltf_import::parse(&bytes, None).expect("glb parses");
        assert!(scene.objects.iter().any(|o| o.mesh.is_some()));
    }
}
