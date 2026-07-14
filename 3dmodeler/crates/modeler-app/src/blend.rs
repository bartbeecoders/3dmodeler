//! Blender .blend import/export via a headless Blender (native only).
//!
//! The .blend format is Blender's internal DNA/memory-dump format — nothing
//! outside Blender writes it reliably — so, like Godot, we drive an installed
//! Blender in background mode as the converter. Two embedded Python scripts
//! translate between .blend and a JSON interchange (see `blend_scripts/`);
//! both apps are Z-up meters, so transforms map 1:1.
//!
//! Conversions follow the async request/poll pattern of [`crate::io`]: the
//! file dialog AND the Blender run happen on a background thread (a big file
//! can take seconds), results land in `poll_import` / `poll_export`, and
//! `poll_progress` surfaces "converting…" status lines along the way.

use modeler_core::glam::{Quat, Vec3};
use modeler_core::{
    LightKind, Material, MeshData, Object, ObjectId, Primitive, Scene, Transform,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const BLEND_TO_JSON: &str = include_str!("blend_scripts/blend_to_json.py");
const JSON_TO_BLEND: &str = include_str!("blend_scripts/json_to_blend.py");

/// Kill a stuck Blender after this long; real conversions finish well within.
const TIMEOUT_SECS: u64 = 300;

// ------------------------------------------------------------ interchange --

/// The JSON payload the Python scripts read/write.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BlendScene {
    #[serde(default)]
    pub blender_version: Option<String>,
    pub objects: Vec<BlendObject>,
    /// Blender object types that could not be represented, with counts
    /// (import only): CAMERA, ARMATURE, face-less meshes, …
    #[serde(default)]
    pub skipped: HashMap<String, u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlendObject {
    pub name: String,
    /// Name of the parent object; transforms are relative to it. Objects are
    /// listed parents-first on both sides.
    #[serde(default)]
    pub parent: Option<String>,
    pub location: [f32; 3],
    pub rotation_wxyz: [f32; 4],
    pub scale: [f32; 3],
    /// "mesh" | "empty" | "light"
    pub kind: String,
    #[serde(default)]
    pub mesh: Option<BlendMesh>,
    #[serde(default)]
    pub material: Option<BlendMaterial>,
    #[serde(default)]
    pub light: Option<BlendLight>,
    /// Empty display size ("empty" kind only).
    #[serde(default)]
    pub size: Option<f32>,
    #[serde(default = "default_true")]
    pub visible: bool,
}

fn default_true() -> bool {
    true
}

/// Flat triangle mesh: xyz triples, one normal per vertex, triangle indices.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BlendMesh {
    pub positions: Vec<f32>,
    #[serde(default)]
    pub normals: Vec<f32>,
    pub indices: Vec<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlendMaterial {
    pub base_color: [f32; 3],
    pub roughness: f32,
    pub metallic: f32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlendLight {
    /// "Point" | "Sun" | "Spot"
    pub kind: String,
    pub color: [f32; 3],
    pub intensity: f32,
    pub spot_angle_deg: f32,
    pub shadows: bool,
}

// -------------------------------------------------------- request / poll --

static PENDING_IMPORT: Mutex<Option<Result<(PathBuf, BlendScene), String>>> = Mutex::new(None);
static PENDING_EXPORT: Mutex<Option<Result<PathBuf, String>>> = Mutex::new(None);
static PROGRESS: Mutex<Option<String>> = Mutex::new(None);
/// One conversion (dialog included) at a time, mirroring `io::DIALOG_OPEN`.
static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn poll_import() -> Option<Result<(PathBuf, BlendScene), String>> {
    PENDING_IMPORT.lock().ok().and_then(|mut p| p.take())
}

pub fn poll_export() -> Option<Result<PathBuf, String>> {
    PENDING_EXPORT.lock().ok().and_then(|mut p| p.take())
}

/// Transient status lines from the conversion thread ("converting …").
pub fn poll_progress() -> Option<String> {
    PROGRESS.lock().ok().and_then(|mut p| p.take())
}

fn set_progress(message: String) {
    if let Ok(mut p) = PROGRESS.lock() {
        *p = Some(message);
    }
}

/// Pick a .blend file and convert it; the result lands in `poll_import`.
pub fn request_import(start_dir: Option<PathBuf>) {
    use std::sync::atomic::Ordering;
    if BUSY.swap(true, Ordering::SeqCst) {
        return; // a dialog or conversion is already running
    }
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new().add_filter("Blender scene", &["blend"]);
        if let Some(dir) = start_dir.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            convert_import(&path);
        }
        BUSY.store(false, Ordering::SeqCst);
    });
}

/// Convert a known .blend path (OS file drop) without showing a dialog.
pub fn import_path(path: PathBuf) {
    use std::sync::atomic::Ordering;
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        convert_import(&path);
        BUSY.store(false, Ordering::SeqCst);
    });
}

fn convert_import(path: &Path) {
    set_progress(format!(
        "importing {} via Blender…",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    ));
    let result = import_blend(path).map(|scene| (path.to_path_buf(), scene));
    if let Ok(mut pending) = PENDING_IMPORT.lock() {
        *pending = Some(result);
    }
}

/// Pick a save path and write `payload` (see [`export_payload`]) to it as a
/// .blend; the result lands in `poll_export`.
pub fn request_export(payload: String, default_name: String, start_dir: Option<PathBuf>) {
    use std::sync::atomic::Ordering;
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Blender scene", &["blend"])
            .set_file_name(&default_name);
        if let Some(dir) = start_dir.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(mut path) = dialog.save_file() {
            if path.extension().is_none() {
                path.set_extension("blend");
            }
            set_progress(format!(
                "exporting {} via Blender…",
                path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            ));
            let result = export_blend(&payload, &path).map(|_| path);
            if let Ok(mut pending) = PENDING_EXPORT.lock() {
                *pending = Some(result);
            }
        }
        BUSY.store(false, Ordering::SeqCst);
    });
}

// -------------------------------------------------------- blender process --

/// Locate a Blender executable: $BLENDER_PATH, then $PATH, then .desktop
/// entries (covers tarball installs launched via a desktop shortcut), then
/// well-known locations.
pub fn find_blender() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BLENDER_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = if cfg!(windows) { "blender.exe" } else { "blender" };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let mut desktop_dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
    ];
    if let Some(data) = dirs::data_dir() {
        desktop_dirs.insert(0, data.join("applications"));
    }
    for dir in desktop_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !name.contains("blender") || !name.ends_with(".desktop") {
                continue;
            }
            if let Some(path) = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|text| desktop_exec_path(&text))
            {
                return Some(path);
            }
        }
    }
    let fixed = [
        "/usr/bin/blender",
        "/opt/blender/blender",
        "/Applications/Blender.app/Contents/MacOS/Blender",
        r"C:\Program Files\Blender Foundation\Blender\blender.exe",
    ];
    fixed.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Pull the executable out of a .desktop `Exec=` line, e.g.
/// `Exec='/opt/blender/blender' %f`.
fn desktop_exec_path(desktop_file: &str) -> Option<PathBuf> {
    let exec = desktop_file
        .lines()
        .find_map(|l| l.strip_prefix("Exec="))?
        .trim();
    let path = match exec.chars().next()? {
        q @ ('\'' | '"') => exec[1..].split(q).next()?,
        _ => exec.split_whitespace().next()?,
    };
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

fn no_blender_message() -> String {
    "Blender not found — install Blender or point the BLENDER_PATH \
     environment variable at its executable"
        .to_string()
}

/// Fresh per-call scratch dir for the script + JSON handoff files.
fn scratch_dir() -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "modeler-blend-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("temp dir: {e}"))?;
    Ok(dir)
}

/// Run Blender to completion, killing it after [`TIMEOUT_SECS`]. Returns
/// stderr+stdout tail on failure.
fn run_blender(mut command: std::process::Command) -> Result<(), String> {
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch Blender: {e}"))?;
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed().as_secs() > TIMEOUT_SECS {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("Blender timed out after {TIMEOUT_SECS}s"));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(format!("waiting for Blender: {e}")),
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut detail = String::new();
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read;
        let _ = err.read_to_string(&mut detail);
    }
    if detail.trim().is_empty() {
        if let Some(mut out) = child.stdout.take() {
            use std::io::Read;
            let _ = out.read_to_string(&mut detail);
        }
    }
    let tail: Vec<&str> = detail.lines().rev().take(8).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Err(format!("Blender failed ({status}): {}", tail.join(" | ")))
}

/// .blend → interchange, via the embedded `blend_to_json.py`.
fn import_blend(path: &Path) -> Result<BlendScene, String> {
    let blender = find_blender().ok_or_else(no_blender_message)?;
    let dir = scratch_dir()?;
    let script = dir.join("blend_to_json.py");
    let out_json = dir.join("scene.json");
    std::fs::write(&script, BLEND_TO_JSON).map_err(|e| format!("temp script: {e}"))?;

    let mut command = std::process::Command::new(blender);
    command
        .arg("--background")
        .arg("--factory-startup")
        .arg(path)
        .arg("--python-exit-code")
        .arg("1")
        .arg("--python")
        .arg(&script)
        .arg("--")
        .arg(&out_json);
    let run = run_blender(command);
    let result = run.and_then(|_| {
        let text = std::fs::read_to_string(&out_json)
            .map_err(|e| format!("reading conversion output: {e}"))?;
        serde_json::from_str::<BlendScene>(&text)
            .map_err(|e| format!("parsing conversion output: {e}"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Interchange → .blend, via the embedded `json_to_blend.py`.
fn export_blend(payload: &str, out_path: &Path) -> Result<(), String> {
    let blender = find_blender().ok_or_else(no_blender_message)?;
    let dir = scratch_dir()?;
    let script = dir.join("json_to_blend.py");
    let in_json = dir.join("scene.json");
    std::fs::write(&script, JSON_TO_BLEND).map_err(|e| format!("temp script: {e}"))?;
    std::fs::write(&in_json, payload).map_err(|e| format!("temp payload: {e}"))?;

    let mut command = std::process::Command::new(blender);
    command
        .arg("--background")
        .arg("--factory-startup")
        .arg("--python-exit-code")
        .arg("1")
        .arg("--python")
        .arg(&script)
        .arg("--")
        .arg(&in_json)
        .arg(out_path);
    let run = run_blender(command);
    let result = run.and_then(|_| {
        out_path
            .is_file()
            .then_some(())
            .ok_or_else(|| "Blender wrote no output file".to_string())
    });
    let _ = std::fs::remove_dir_all(&dir);
    result
}

// -------------------------------------------------------- scene ⇄ payload --

/// Serialize the scene for `json_to_blend.py`: every object, parents-first,
/// with the mesh the viewport shows (modifier stacks evaluated via
/// `mesh_for`, like the OBJ export).
pub fn export_payload(scene: &Scene, mesh_for: impl Fn(&Scene, &Object) -> MeshData) -> String {
    // parents before children so the Blender script can link in one pass
    let mut ordered: Vec<&Object> = Vec::with_capacity(scene.objects().len());
    let mut placed: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    while ordered.len() < scene.objects().len() {
        let before = ordered.len();
        for object in scene.objects() {
            if placed.contains(&object.id) {
                continue;
            }
            let parent_ready = match object.parent.filter(|p| scene.object(*p).is_some()) {
                Some(parent) => placed.contains(&parent),
                None => true,
            };
            if parent_ready {
                ordered.push(object);
                placed.insert(object.id);
            }
        }
        if ordered.len() == before {
            break; // corrupted parent cycle: export the rest unparented
        }
    }

    let objects = ordered
        .iter()
        .map(|object| {
            let (kind, mesh, material, light, size) = match object.primitive {
                Primitive::Empty { size } => ("empty", None, None, None, Some(size)),
                Primitive::Light { kind, color, intensity, spot_angle_deg, shadows } => (
                    "light",
                    None,
                    None,
                    Some(BlendLight {
                        kind: kind.label().to_string(),
                        color,
                        intensity,
                        spot_angle_deg,
                        shadows,
                    }),
                    None,
                ),
                _ => {
                    let mesh = mesh_for(scene, object);
                    (
                        "mesh",
                        Some(BlendMesh {
                            positions: mesh.positions.iter().flat_map(|p| [p.x, p.y, p.z]).collect(),
                            normals: mesh.normals.iter().flat_map(|n| [n.x, n.y, n.z]).collect(),
                            indices: mesh.indices.clone(),
                        }),
                        Some(BlendMaterial {
                            base_color: object.material.base_color,
                            roughness: object.material.roughness,
                            metallic: object.material.metallic,
                        }),
                        None,
                        None,
                    )
                }
            };
            let parent = object
                .parent
                .and_then(|p| scene.object(p))
                .map(|p| p.name.clone());
            let t = object.transform;
            BlendObject {
                name: object.name.clone(),
                parent,
                location: t.location.to_array(),
                rotation_wxyz: [t.rotation.w, t.rotation.x, t.rotation.y, t.rotation.z],
                scale: t.scale.to_array(),
                kind: kind.to_string(),
                mesh,
                material,
                light,
                size,
                visible: object.visible,
            }
        })
        .collect();

    let payload = BlendScene {
        blender_version: None,
        objects,
        skipped: HashMap::new(),
    };
    serde_json::to_string(&payload).expect("interchange payload serializes")
}

/// Add the imported objects to the scene. Returns the new ids (for
/// selection), in payload order.
pub fn merge_into_scene(scene: &mut Scene, data: &BlendScene) -> Vec<ObjectId> {
    let mut name_to_id: HashMap<&str, ObjectId> = HashMap::new();
    let mut new_ids = Vec::with_capacity(data.objects.len());
    for imported in &data.objects {
        let (primitive, edited_mesh) = imported_primitive(imported);
        let material = imported
            .material
            .as_ref()
            .map(|m| Material {
                base_color: m.base_color.map(|c| c.clamp(0.0, 1.0)),
                roughness: m.roughness.clamp(0.0, 1.0),
                metallic: m.metallic.clamp(0.0, 1.0),
            })
            .unwrap_or_default();
        // parents come first in the payload, so the lookup already has them
        let parent = imported
            .parent
            .as_deref()
            .and_then(|name| name_to_id.get(name).copied());
        let transform = Transform {
            location: Vec3::from_array(imported.location),
            rotation: Quat::from_xyzw(
                imported.rotation_wxyz[1],
                imported.rotation_wxyz[2],
                imported.rotation_wxyz[3],
                imported.rotation_wxyz[0],
            )
            .normalize(),
            scale: Vec3::from_array(imported.scale),
        };
        let id = scene.insert_object(Object {
            id: ObjectId(0), // insert_object assigns the real id
            name: imported.name.clone(),
            transform,
            primitive,
            smooth: false,
            visible: imported.visible,
            material,
            dynamic: false,
            density: 1.0,
            parent,
            folder: None,
            show_label: false,
            show_dimensions: false,
            pivot: Vec3::ZERO,
            anchor: Vec3::ZERO,
            group: false,
            cutouts: Vec::new(),
            floor_outline: Vec::new(),
            edited_mesh,
            subdivision: 0,
            modifiers: Vec::new(),
            mesh_revision: 0,
        });
        name_to_id.insert(&imported.name, id);
        new_ids.push(id);
    }
    new_ids
}

/// Map an interchange object to a primitive (+ mesh override for "mesh").
fn imported_primitive(imported: &BlendObject) -> (Primitive, Option<MeshData>) {
    match imported.kind.as_str() {
        "light" => {
            let spec = imported.light.as_ref();
            let kind = match spec.map(|l| l.kind.as_str()) {
                Some("Sun") => LightKind::Sun,
                Some("Spot") => LightKind::Spot,
                _ => LightKind::Point,
            };
            let primitive = Primitive::Light {
                kind,
                color: spec.map(|l| l.color.map(|c| c.clamp(0.0, 1.0))).unwrap_or([1.0; 3]),
                intensity: spec.map(|l| l.intensity.max(0.0)).unwrap_or(3.0),
                spot_angle_deg: spec
                    .map(|l| l.spot_angle_deg.clamp(1.0, 179.0))
                    .unwrap_or(45.0),
                shadows: spec.map(|l| l.shadows).unwrap_or(true),
            };
            (primitive, None)
        }
        "empty" => (
            Primitive::Empty { size: imported.size.unwrap_or(1.0).max(0.01) },
            None,
        ),
        _ => {
            let mesh = imported.mesh.as_ref().map(imported_mesh).unwrap_or_default();
            // a base primitive still backs the object; the imported mesh
            // overrides it exactly like a Tab-edited mesh does
            (Primitive::Cube { size: 2.0 }, Some(mesh))
        }
    }
}

/// Interchange mesh → MeshData, dropping out-of-range triangles and
/// recomputing normals when they are missing or malformed.
fn imported_mesh(mesh: &BlendMesh) -> MeshData {
    let positions: Vec<Vec3> = mesh
        .positions
        .chunks_exact(3)
        .map(|c| Vec3::new(c[0], c[1], c[2]))
        .collect();
    let count = positions.len() as u32;
    let indices: Vec<u32> = mesh
        .indices
        .chunks_exact(3)
        .filter(|tri| tri.iter().all(|&i| i < count))
        .flatten()
        .copied()
        .collect();
    let normals: Vec<Vec3> = mesh
        .normals
        .chunks_exact(3)
        .map(|c| Vec3::new(c[0], c[1], c[2]))
        .collect();
    let mut data = MeshData { positions, normals, indices, seams: Vec::new() };
    if data.normals.len() != data.positions.len() {
        data.recompute_normals();
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported_cube(name: &str, parent: Option<&str>) -> BlendObject {
        // unit right tetrahedron: enough structure to catch axis mixups
        BlendObject {
            name: name.to_string(),
            parent: parent.map(str::to_string),
            location: [1.0, 2.0, 3.0],
            rotation_wxyz: [1.0, 0.0, 0.0, 0.0],
            scale: [1.0, 1.0, 1.0],
            kind: "mesh".to_string(),
            mesh: Some(BlendMesh {
                positions: vec![
                    0.0, 0.0, 0.0, //
                    1.0, 0.0, 0.0, //
                    0.0, 1.0, 0.0, //
                    0.0, 0.0, 1.0,
                ],
                normals: Vec::new(), // force the recompute path
                indices: vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
            }),
            material: Some(BlendMaterial {
                base_color: [0.2, 0.4, 0.6],
                roughness: 0.5,
                metallic: 1.5, // out of range on purpose
            }),
            light: None,
            size: None,
            visible: true,
        }
    }

    #[test]
    fn merge_links_parents_and_keeps_transforms() {
        let mut scene = Scene::new();
        let data = BlendScene {
            blender_version: None,
            objects: vec![imported_cube("Root", None), imported_cube("Child", Some("Root"))],
            skipped: HashMap::new(),
        };
        let ids = merge_into_scene(&mut scene, &data);
        assert_eq!(ids.len(), 2);
        let child = scene.object(ids[1]).unwrap();
        assert_eq!(child.parent, Some(ids[0]));
        assert_eq!(child.transform.location, Vec3::new(1.0, 2.0, 3.0));
        let mesh = child.edited_mesh.as_ref().unwrap();
        assert_eq!(mesh.positions.len(), 4);
        assert_eq!(mesh.normals.len(), 4); // recomputed
        assert_eq!(child.material.metallic, 1.0); // clamped
    }

    #[test]
    fn merge_renames_on_collision_and_drops_bad_triangles() {
        let mut scene = Scene::default_scene(); // already has "Cube"
        let mut cube = imported_cube("Cube", None);
        cube.mesh.as_mut().unwrap().indices.extend_from_slice(&[0, 1, 99]);
        let data = BlendScene {
            blender_version: None,
            objects: vec![cube],
            skipped: HashMap::new(),
        };
        let ids = merge_into_scene(&mut scene, &data);
        let object = scene.object(ids[0]).unwrap();
        assert_eq!(object.name, "Cube.001");
        assert_eq!(object.edited_mesh.as_ref().unwrap().indices.len(), 12);
    }

    #[test]
    fn export_payload_orders_parents_first_and_evaluates_meshes() {
        let mut scene = Scene::new();
        let child = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let parent = scene.add_object(Primitive::Empty { size: 1.0 }, Transform::default());
        scene.object_mut(child).unwrap().parent = Some(parent);

        let json = export_payload(&scene, |_, o| o.render_mesh());
        let payload: BlendScene = serde_json::from_str(&json).unwrap();
        assert_eq!(payload.objects.len(), 2);
        assert_eq!(payload.objects[0].kind, "empty");
        assert_eq!(payload.objects[1].kind, "mesh");
        assert_eq!(payload.objects[1].parent.as_deref(), Some("Empty"));
        let mesh = payload.objects[1].mesh.as_ref().unwrap();
        assert_eq!(mesh.positions.len() % 3, 0);
        assert!(!mesh.indices.is_empty());
    }

    #[test]
    fn desktop_exec_parses_quoted_and_plain_paths() {
        // quoted path with arguments (the common tarball-install shape);
        // the referenced file must exist, so use the test binary's own path
        let me = std::env::current_exe().unwrap();
        let quoted = format!("[Desktop Entry]\nExec='{}' %f\n", me.display());
        assert_eq!(desktop_exec_path(&quoted), Some(me.clone()));
        let plain = format!("Exec={} %f\n", me.display());
        assert_eq!(desktop_exec_path(&plain), Some(me));
        assert_eq!(desktop_exec_path("Exec='/nonexistent/blender' %f\n"), None);
    }

    /// The exact pipeline behind File ▸ Import .blend and .blend file drops
    /// (import_path → poll_import → merge_into_scene), against a
    /// Blender-AUTHORED file: Suzanne parented with a parent-inverse offset
    /// (which our exporterless matrix_local math must absorb), a camera that
    /// must be skipped, and a Principled material. Silently passes without a
    /// Blender installation so CI stays green.
    #[test]
    fn imports_blender_authored_file_end_to_end() {
        let Some(blender) = find_blender() else {
            eprintln!("skipping: no Blender installation found");
            return;
        };
        // author a test scene with Blender itself
        let dir = scratch_dir().unwrap();
        let blend_path = dir.join("authored.blend");
        let script = dir.join("author.py");
        std::fs::write(
            &script,
            r#"
import bpy, sys
out = sys.argv[sys.argv.index('--') + 1]
cube = bpy.data.objects['Cube']
cube.location = (1.0, 1.0, 1.0)
bpy.ops.mesh.primitive_monkey_add(location=(3.0, 0.0, 1.0))
suzanne = bpy.context.active_object
mat = bpy.data.materials.new('Red')
mat.use_nodes = True
bsdf = next(n for n in mat.node_tree.nodes if n.type == 'BSDF_PRINCIPLED')
bsdf.inputs['Base Color'].default_value = (0.9, 0.1, 0.1, 1.0)
suzanne.data.materials.append(mat)
suzanne.parent = cube
suzanne.matrix_parent_inverse = cube.matrix_world.inverted()
bpy.ops.wm.save_as_mainfile(filepath=out)
"#,
        )
        .unwrap();
        let mut command = std::process::Command::new(blender);
        command
            .arg("--background")
            .arg("--factory-startup")
            .arg("--python-exit-code")
            .arg("1")
            .arg("--python")
            .arg(&script)
            .arg("--")
            .arg(&blend_path);
        run_blender(command).expect("author test .blend");

        // drive the same entry point the app uses, wait on the same poll
        import_path(blend_path);
        let start = std::time::Instant::now();
        let (path, data) = loop {
            if let Some(result) = poll_import() {
                break result.expect("import succeeds");
            }
            assert!(start.elapsed().as_secs() < 120, "conversion timed out");
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        assert!(path.ends_with("authored.blend"));
        assert_eq!(data.skipped.get("CAMERA"), Some(&1), "camera is skipped");

        let mut scene = Scene::default_scene(); // name collision with "Cube"
        let ids = merge_into_scene(&mut scene, &data);
        assert_eq!(ids.len(), 3, "cube + suzanne + light");
        let suzanne = ids
            .iter()
            .filter_map(|&id| scene.object(id))
            .find(|o| o.name == "Suzanne")
            .expect("Suzanne imported");
        let cube = ids
            .iter()
            .filter_map(|&id| scene.object(id))
            .find(|o| o.name == "Cube.001")
            .expect("Blender cube imported under a collision-free name");
        assert_eq!(suzanne.parent, Some(cube.id), "parent link survives");
        // Suzanne world (3,0,1) under a parent at (1,1,1): local (2,-1,0)
        let local = suzanne.transform.location;
        assert!(
            (local - Vec3::new(2.0, -1.0, 0.0)).length() < 1e-4,
            "parent-inverse absorbed into local transform: {local}"
        );
        let world = scene.world_transform(suzanne.id).location;
        assert!(
            (world - Vec3::new(3.0, 0.0, 1.0)).length() < 1e-4,
            "world position preserved: {world}"
        );
        assert!(
            (suzanne.material.base_color[0] - 0.9).abs() < 1e-3,
            "Principled base color survives"
        );
        let mesh = suzanne.edited_mesh.as_ref().expect("mesh imported");
        assert!(mesh.indices.len() >= 3 * 900, "Suzanne has ~968 triangles");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Full roundtrip through a real Blender, when one is installed —
    /// silently passes otherwise so CI without Blender stays green.
    #[test]
    fn roundtrip_through_real_blender() {
        let Some(_) = find_blender() else {
            eprintln!("skipping: no Blender installation found");
            return;
        };
        let mut scene = Scene::new();
        let cube = scene.add_object(
            Primitive::Cube { size: 2.0 },
            Transform {
                location: Vec3::new(1.0, -2.0, 0.5),
                rotation: Quat::from_rotation_z(0.7),
                scale: Vec3::new(1.0, 2.0, 3.0),
            },
        );
        scene.object_mut(cube).unwrap().material.base_color = [0.9, 0.1, 0.2];
        let parent = scene.add_object(Primitive::Empty { size: 1.0 }, Transform::default());
        scene.object_mut(cube).unwrap().parent = Some(parent);
        scene.add_object(
            Primitive::Light {
                kind: LightKind::Spot,
                color: [1.0, 0.9, 0.8],
                intensity: 5.0,
                spot_angle_deg: 30.0,
                shadows: true,
            },
            Transform { location: Vec3::new(0.0, 0.0, 4.0), ..Default::default() },
        );

        let dir = scratch_dir().unwrap();
        let blend_path = dir.join("roundtrip.blend");
        let payload = export_payload(&scene, |_, o| o.render_mesh());
        export_blend(&payload, &blend_path).expect("export .blend");
        let imported = import_blend(&blend_path).expect("import .blend");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(imported.objects.len(), 3, "cube + empty + light survive");
        let cube = imported
            .objects
            .iter()
            .find(|o| o.name == "Cube")
            .expect("cube survives");
        assert_eq!(cube.kind, "mesh");
        assert_eq!(cube.parent.as_deref(), Some("Empty"));
        for (a, b) in cube.location.iter().zip([1.0, -2.0, 0.5]) {
            assert!((a - b).abs() < 1e-4, "location roundtrips: {:?}", cube.location);
        }
        for (a, b) in cube.scale.iter().zip([1.0, 2.0, 3.0]) {
            assert!((a - b).abs() < 1e-4, "scale roundtrips: {:?}", cube.scale);
        }
        let material = cube.material.as_ref().expect("material survives");
        assert!((material.base_color[0] - 0.9).abs() < 1e-3);
        let mesh = cube.mesh.as_ref().expect("mesh survives");
        assert_eq!(mesh.indices.len(), 36, "12 triangles");

        let light = imported
            .objects
            .iter()
            .find(|o| o.kind == "light")
            .expect("light survives");
        let spec = light.light.as_ref().unwrap();
        assert_eq!(spec.kind, "Spot");
        assert!((spec.intensity - 5.0).abs() < 1e-3, "Watts mapping inverts");
        assert!((spec.spot_angle_deg - 30.0).abs() < 0.1);
    }
}
