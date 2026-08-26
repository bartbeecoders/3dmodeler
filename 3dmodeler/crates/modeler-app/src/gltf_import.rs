//! glTF 2.0 (.glb / .gltf) import — pure Rust, native and browser.
//!
//! Drag-and-drop (see [`crate::drop_target`]) and File ▸ Import both land
//! here. The parser reads the subset of glTF the interchange can hold —
//! triangle meshes with authored UVs, node transforms/hierarchy, and
//! pbrMetallicRoughness materials including their textures — and emits a
//! [`crate::blend::BlendScene`], so glTF imports merge into the scene
//! through the same path as .blend files. Texture images (base color,
//! normal, and the metallic-roughness/occlusion maps, split into the
//! single-channel files the texture bridge expects) travel along as
//! `texture_files` and land in the PBR texture cache on merge. Skinning,
//! animation, Draco compression and sparse accessors are out of scope; what
//! a file uses of them is counted in `skipped`.
//!
//! glTF is Y-up right-handed; root nodes are rotated +90° about X into this
//! app's Z-up world (the same fix Blender's importer applies).
//!
//! Results follow the request/poll pattern of [`crate::blend`]: parsing
//! happens on a background thread natively (in the FileReader callback in
//! the browser — no threads there) and finished scenes land in
//! [`poll_imports`].

use crate::blend::{BlendMaterial, BlendMesh, BlendObject, BlendScene};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use modeler_core::glam::{Mat4, Quat, Vec3};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

// -------------------------------------------------------- request / poll --

/// Finished imports: (file name, scene) or an error for the status bar.
/// A Vec, not a slot — one drop can carry several files.
static PENDING: Mutex<Vec<Result<(String, BlendScene), String>>> = Mutex::new(Vec::new());

pub fn poll_imports() -> Vec<Result<(String, BlendScene), String>> {
    PENDING.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default()
}

fn deliver(result: Result<(String, BlendScene), String>) {
    if let Ok(mut pending) = PENDING.lock() {
        pending.push(result);
    }
}

/// Parse a known .glb/.gltf path (OS file drop) off the event loop.
#[cfg(not(target_arch = "wasm32"))]
pub fn import_path(path: std::path::PathBuf) {
    std::thread::spawn(move || convert(&path));
}

#[cfg(not(target_arch = "wasm32"))]
fn convert(path: &std::path::Path) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model".into());
    let result = std::fs::read(path)
        .map_err(|e| format!("reading {}: {e}", path.display()))
        .and_then(|bytes| parse(&bytes, path.parent()))
        .map(|scene| (name.clone(), scene))
        .map_err(|e| format!("{name}: {e}"));
    deliver(result);
}

/// Parse in-memory file bytes (browser drops and pickers — no paths there).
/// External .bin buffer URIs cannot be resolved on this route.
#[cfg(target_arch = "wasm32")]
pub fn import_bytes(name: String, bytes: Vec<u8>) {
    let result = parse(&bytes, None)
        .map(|scene| (name.clone(), scene))
        .map_err(|e| format!("{name}: {e}"));
    deliver(result);
}

/// Pick .glb/.gltf files and parse them; results land in [`poll_imports`].
#[cfg(not(target_arch = "wasm32"))]
pub fn request_import(start_dir: Option<std::path::PathBuf>) {
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new().add_filter("glTF model", &["glb", "gltf"]);
        if let Some(dir) = start_dir.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        for path in dialog.pick_files().unwrap_or_default() {
            convert(&path);
        }
    });
}

/// Browser file picker (a hidden `<input type=file>`, like
/// [`crate::ref_image::request_setup_images`]).
#[cfg(target_arch = "wasm32")]
pub fn request_import(_start_dir: Option<std::path::PathBuf>) {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
    let Ok(el) = document.create_element("input") else { return };
    let Ok(input) = el.dyn_into::<web_sys::HtmlInputElement>() else { return };
    input.set_type("file");
    input.set_accept(".glb,.gltf");
    input.set_multiple(true);
    if let Some(html_el) = input.dyn_ref::<web_sys::HtmlElement>() {
        let _ = html_el.style().set_property("display", "none");
    }
    if let Some(body) = document.body() {
        let _ = body.append_child(&input);
    }

    let input_for_closure = input.clone();
    let onchange = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
        let Some(files) = input_for_closure.files() else { return };
        for i in 0..files.length() {
            let Some(file) = files.get(i) else { continue };
            let name = file.name();
            let Ok(reader) = web_sys::FileReader::new() else { continue };
            let reader_for_load = reader.clone();
            let onload = Closure::once(move || {
                let Ok(result) = reader_for_load.result() else { return };
                let bytes = js_sys::Uint8Array::new(&result).to_vec();
                import_bytes(name, bytes);
            });
            reader.set_onload(Some(onload.as_ref().unchecked_ref()));
            onload.forget();
            let _ = reader.read_as_array_buffer(&file);
        }
        input_for_closure.remove();
    });
    input.set_onchange(Some(onchange.as_ref().unchecked_ref()));
    input.click();
    onchange.forget();
}

// ------------------------------------------------------------- json model --

/// The glTF JSON subset this importer reads. Everything defaults so a
/// sparse file (or a rich one full of extensions) still deserializes.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Doc {
    scene: Option<usize>,
    scenes: Vec<SceneDef>,
    nodes: Vec<Node>,
    meshes: Vec<MeshDef>,
    accessors: Vec<Accessor>,
    buffer_views: Vec<View>,
    buffers: Vec<Buffer>,
    materials: Vec<MaterialDef>,
    textures: Vec<TextureDef>,
    images: Vec<ImageDef>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SceneDef {
    nodes: Vec<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Node {
    name: Option<String>,
    children: Vec<usize>,
    mesh: Option<usize>,
    camera: Option<usize>,
    matrix: Option<[f32; 16]>,
    translation: Option<[f32; 3]>,
    /// xyzw, glTF's quaternion order.
    rotation: Option<[f32; 4]>,
    scale: Option<[f32; 3]>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MeshDef {
    name: Option<String>,
    primitives: Vec<Prim>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Prim {
    attributes: HashMap<String, usize>,
    indices: Option<usize>,
    material: Option<usize>,
    /// Topology; 4 (the default) = triangles, the only mode imported.
    mode: Option<u32>,
    extensions: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Accessor {
    buffer_view: Option<usize>,
    byte_offset: usize,
    component_type: u32,
    count: usize,
    #[serde(rename = "type")]
    kind: String,
    sparse: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct View {
    buffer: usize,
    byte_offset: usize,
    byte_length: usize,
    byte_stride: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Buffer {
    uri: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct MaterialDef {
    pbr_metallic_roughness: Option<Pbr>,
    normal_texture: Option<TexRef>,
    occlusion_texture: Option<TexRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Pbr {
    base_color_factor: [f32; 4],
    metallic_factor: f32,
    roughness_factor: f32,
    base_color_texture: Option<TexRef>,
    metallic_roughness_texture: Option<TexRef>,
}

impl Default for Pbr {
    fn default() -> Self {
        // glTF's own defaults: white, fully metallic, fully rough
        Self {
            base_color_factor: [1.0; 4],
            metallic_factor: 1.0,
            roughness_factor: 1.0,
            base_color_texture: None,
            metallic_roughness_texture: None,
        }
    }
}

/// A material's reference to one of the document's textures. The extra
/// per-slot fields (normal scale, occlusion strength) are ignored.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct TexRef {
    index: usize,
    /// Which TEXCOORD_n set the texture samples; only set 0 is imported.
    tex_coord: u32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct TextureDef {
    source: Option<usize>,
    extensions: HashMap<String, serde_json::Value>,
}

impl TextureDef {
    /// The image index — plain `source`, or the one EXT_texture_webp moves
    /// into its extension block.
    fn image_index(&self) -> Option<usize> {
        self.source.or_else(|| {
            let webp = self.extensions.get("EXT_texture_webp")?;
            webp.get("source")?.as_u64().map(|s| s as usize)
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ImageDef {
    uri: Option<String>,
    buffer_view: Option<usize>,
}

// -------------------------------------------------------------- container --

/// Parse .glb or .gltf bytes into the interchange. `base_dir` resolves
/// external buffer URIs (native path imports only; `None` elsewhere).
pub fn parse(bytes: &[u8], base_dir: Option<&std::path::Path>) -> Result<BlendScene, String> {
    let (json, bin) = split_container(bytes)?;
    let doc: Doc =
        serde_json::from_slice(json).map_err(|e| format!("not a glTF file: {e}"))?;
    let buffers: Vec<Vec<u8>> = doc
        .buffers
        .iter()
        .map(|b| buffer_bytes(b, bin, base_dir))
        .collect::<Result<_, _>>()?;
    Ok(convert_doc(&doc, &buffers, base_dir))
}

/// .glb container → (JSON chunk, BIN chunk); bare .gltf JSON passes through.
fn split_container(bytes: &[u8]) -> Result<(&[u8], Option<&[u8]>), String> {
    if !bytes.starts_with(b"glTF") {
        return Ok((bytes, None)); // a .gltf: the whole file is the JSON
    }
    if bytes.len() < 12 {
        return Err("truncated .glb header".into());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != 2 {
        return Err(format!("unsupported .glb version {version}"));
    }
    let mut offset = 12;
    let (mut json, mut bin) = (None, None);
    while offset + 8 <= bytes.len() {
        let length = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let kind = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let start = offset + 8;
        let end = start.checked_add(length).filter(|&e| e <= bytes.len());
        let Some(end) = end else { return Err("truncated .glb chunk".into()) };
        match kind {
            0x4E4F534A => json = Some(&bytes[start..end]), // "JSON"
            0x004E4942 => bin = Some(&bytes[start..end]),  // "BIN\0"
            _ => {}
        }
        // chunks are 4-byte aligned; tolerate writers that leave the
        // padding out of the declared length
        offset = start + length.div_ceil(4) * 4;
    }
    json.map(|j| (j, bin)).ok_or_else(|| "no JSON chunk in .glb".to_string())
}

/// Resolve one buffer: the .glb BIN chunk, a base64 data: URI, or (native
/// imports only) a file next to the .gltf.
fn buffer_bytes(
    buffer: &Buffer,
    bin: Option<&[u8]>,
    base_dir: Option<&std::path::Path>,
) -> Result<Vec<u8>, String> {
    let Some(uri) = &buffer.uri else {
        return bin.map(<[u8]>::to_vec).ok_or_else(|| "buffer needs a BIN chunk".into());
    };
    uri_bytes(uri, base_dir)
}

/// Bytes behind a buffer or image URI: base64 data: URI, or (native imports
/// only) a file next to the .gltf.
fn uri_bytes(uri: &str, base_dir: Option<&std::path::Path>) -> Result<Vec<u8>, String> {
    if let Some(data) = uri.strip_prefix("data:") {
        let (meta, payload) =
            data.split_once(',').ok_or_else(|| "malformed data: URI".to_string())?;
        if !meta.ends_with(";base64") {
            return Err("data: URI is not base64".into());
        }
        return BASE64.decode(payload).map_err(|e| format!("data: URI: {e}"));
    }
    external_bytes(uri, base_dir)
}

#[cfg(not(target_arch = "wasm32"))]
fn external_bytes(uri: &str, base_dir: Option<&std::path::Path>) -> Result<Vec<u8>, String> {
    let dir = base_dir.ok_or_else(|| format!("external buffer '{uri}' unavailable"))?;
    std::fs::read(dir.join(percent_decode(uri)))
        .map_err(|e| format!("external buffer '{uri}': {e}"))
}

#[cfg(target_arch = "wasm32")]
fn external_bytes(uri: &str, _base_dir: Option<&std::path::Path>) -> Result<Vec<u8>, String> {
    Err(format!("external buffer '{uri}' — pack the model as a .glb for the browser"))
}

/// Minimal %XX decoding — buffer URIs like `scene%20data.bin`, and the
/// file:// URIs Wayland drops deliver (see [`crate::wayland_drop`]).
#[cfg(not(target_arch = "wasm32"))]
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| {
                std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            })
            .flatten();
        match escaped {
            Some(value) => {
                out.push(value);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// -------------------------------------------------------------- accessors --

/// Locate an accessor's bytes: (buffer, start offset, stride) for `count`
/// elements of `elem_size` bytes each.
fn accessor_layout<'a>(
    doc: &Doc,
    buffers: &'a [Vec<u8>],
    accessor: &Accessor,
    elem_size: usize,
) -> Result<(&'a [u8], usize, usize), String> {
    if accessor.sparse.is_some() {
        return Err("sparse accessor".into());
    }
    let view_index = accessor.buffer_view.ok_or("accessor without data")?;
    let view = doc.buffer_views.get(view_index).ok_or("bad bufferView index")?;
    let data = buffers.get(view.buffer).ok_or("bad buffer index")?;
    let stride = view.byte_stride.unwrap_or(elem_size).max(elem_size);
    let start = view.byte_offset + accessor.byte_offset;
    let end = start + accessor.count.saturating_sub(1) * stride + elem_size;
    if accessor.count > 0 && end > data.len() {
        return Err("accessor past end of buffer".into());
    }
    Ok((data, start, stride))
}

/// Float accessor of `comps` components ("VEC3" positions/normals, "VEC2"
/// UVs) → flat floats.
fn read_floats(
    doc: &Doc,
    buffers: &[Vec<u8>],
    index: usize,
    kind: &str,
    comps: usize,
) -> Result<Vec<f32>, String> {
    let accessor = doc.accessors.get(index).ok_or("bad accessor index")?;
    if accessor.kind != kind || accessor.component_type != 5126 {
        return Err(format!(
            "expected float {kind}, got {} ({})",
            accessor.kind, accessor.component_type
        ));
    }
    let (data, start, stride) = accessor_layout(doc, buffers, accessor, 4 * comps)?;
    let mut out = Vec::with_capacity(accessor.count * comps);
    for i in 0..accessor.count {
        let offset = start + i * stride;
        for c in 0..comps {
            let at = offset + 4 * c;
            out.push(f32::from_le_bytes(data[at..at + 4].try_into().unwrap()));
        }
    }
    Ok(out)
}

/// SCALAR index accessor (u8/u16/u32) → u32 indices.
fn read_indices(doc: &Doc, buffers: &[Vec<u8>], index: usize) -> Result<Vec<u32>, String> {
    let accessor = doc.accessors.get(index).ok_or("bad accessor index")?;
    let elem_size = match accessor.component_type {
        5121 => 1, // u8
        5123 => 2, // u16
        5125 => 4, // u32
        other => return Err(format!("unsupported index type {other}")),
    };
    if accessor.kind != "SCALAR" {
        return Err(format!("expected SCALAR indices, got {}", accessor.kind));
    }
    let (data, start, stride) = accessor_layout(doc, buffers, accessor, elem_size)?;
    let mut out = Vec::with_capacity(accessor.count);
    for i in 0..accessor.count {
        let at = start + i * stride;
        out.push(match elem_size {
            1 => data[at] as u32,
            2 => u16::from_le_bytes(data[at..at + 2].try_into().unwrap()) as u32,
            _ => u32::from_le_bytes(data[at..at + 4].try_into().unwrap()),
        });
    }
    Ok(out)
}

// ------------------------------------------------------------ conversion --

/// glTF document → interchange scene: depth-first over the scene's root
/// nodes, parents before children (what `merge_into_scene` expects).
fn convert_doc(doc: &Doc, buffers: &[Vec<u8>], base_dir: Option<&std::path::Path>) -> BlendScene {
    let mut out = BlendScene::default();
    // materials convert once, up front — an image shared by many primitives
    // stores once and primitives clone their material by index
    let materials = convert_materials(doc, buffers, base_dir, &mut out.texture_files);
    let mut used_names = HashSet::new();
    let mut visited = HashSet::new();
    // Y-up → Z-up, applied to root nodes only; children inherit it
    let up_fix = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
    for root in root_nodes(doc) {
        visit_node(
            doc, buffers, &materials, root, None, Some(up_fix), &mut out, &mut used_names,
            &mut visited,
        );
    }
    out
}

/// Every material in interchange form, with its texture images resolved,
/// (re)encoded where needed, and queued in `files` under content-hash keys.
fn convert_materials(
    doc: &Doc,
    buffers: &[Vec<u8>],
    base_dir: Option<&std::path::Path>,
    files: &mut Vec<(String, Vec<u8>)>,
) -> Vec<BlendMaterial> {
    let mut store = TextureStore { doc, buffers, base_dir, files, seen: HashSet::new() };
    doc.materials
        .iter()
        .map(|m| {
            let pbr = m.pbr_metallic_roughness.clone().unwrap_or_default();
            let [r, g, b, _] = pbr.base_color_factor;
            BlendMaterial {
                base_color: [r, g, b],
                roughness: pbr.roughness_factor,
                metallic: pbr.metallic_factor,
                // base color and normal maps pass through as-is; the packed
                // metallicRoughness map (G = roughness, B = metallic) and the
                // occlusion map (R) become the single-channel greyscale files
                // the texture bridge composes its ORM from
                albedo_texture: store.store_raw(&pbr.base_color_texture),
                normal_texture: store.store_raw(&m.normal_texture),
                roughness_texture: store.store_channel(&pbr.metallic_roughness_texture, 1, "rough"),
                metallic_texture: store.store_channel(&pbr.metallic_roughness_texture, 2, "metal"),
                occlusion_texture: store.store_channel(&m.occlusion_texture, 0, "occl"),
            }
        })
        .collect()
}

/// Resolves texture references to image bytes and queues them for the app's
/// texture cache under `gltf/<content hash>` keys, deduplicating shared
/// images. Anything unresolvable (missing data, a format the app can't
/// decode, a secondary UV set) yields `None` — the material keeps scalars.
struct TextureStore<'a> {
    doc: &'a Doc,
    buffers: &'a [Vec<u8>],
    base_dir: Option<&'a std::path::Path>,
    files: &'a mut Vec<(String, Vec<u8>)>,
    seen: HashSet<String>,
}

impl TextureStore<'_> {
    /// The raw image bytes behind a texture reference: an embedded buffer
    /// view, a data: URI, or (native) a file next to the .gltf.
    fn raw(&self, tex: &TexRef) -> Option<Vec<u8>> {
        let image = self.doc.images.get(self.doc.textures.get(tex.index)?.image_index()?)?;
        if let Some(view_index) = image.buffer_view {
            let view = self.doc.buffer_views.get(view_index)?;
            let data = self.buffers.get(view.buffer)?;
            return data.get(view.byte_offset..view.byte_offset + view.byte_length).map(<[u8]>::to_vec);
        }
        uri_bytes(image.uri.as_deref()?, self.base_dir).ok()
    }

    fn usable(tex: &Option<TexRef>) -> Option<&TexRef> {
        tex.as_ref().filter(|t| t.tex_coord == 0) // only TEXCOORD_0 imports
    }

    /// Store the image bytes unchanged (base color / normal maps).
    fn store_raw(&mut self, tex: &Option<TexRef>) -> Option<String> {
        let bytes = self.raw(Self::usable(tex)?)?;
        let ext = sniff_ext(&bytes)?;
        self.push(format!("gltf/{:016x}.{ext}", fnv64(&bytes)), bytes)
    }

    /// Store one channel of the image as a greyscale PNG. Decodes the source
    /// once per call — packed maps are rare enough that caching isn't worth
    /// the bookkeeping.
    fn store_channel(&mut self, tex: &Option<TexRef>, channel: usize, tag: &str) -> Option<String> {
        let bytes = self.raw(Self::usable(tex)?)?;
        let key = format!("gltf/{:016x}.{tag}.png", fnv64(&bytes));
        if self.seen.contains(&key) {
            return Some(key);
        }
        let rgb = image::load_from_memory(&bytes).ok()?.to_rgb8();
        let grey = image::GrayImage::from_fn(rgb.width(), rgb.height(), |x, y| {
            image::Luma([rgb.get_pixel(x, y)[channel]])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(grey).write_to(&mut png, image::ImageFormat::Png).ok()?;
        self.push(key, png.into_inner())
    }

    fn push(&mut self, key: String, bytes: Vec<u8>) -> Option<String> {
        if self.seen.insert(key.clone()) {
            self.files.push((key.clone(), bytes));
        }
        Some(key)
    }
}

/// Content hash for texture cache keys: FNV-1a, deterministic forever (the
/// keys end up in saved scene files).
fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// File extension by magic bytes — the app's texture decoder reads PNG,
/// JPEG and WebP; anything else stays unimported.
fn sniff_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        Some("jpg")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("webp")
    } else {
        None
    }
}

/// The nodes to import: the default scene's roots, or — scene-less file —
/// every node nothing points at.
fn root_nodes(doc: &Doc) -> Vec<usize> {
    if let Some(scene) = doc.scenes.get(doc.scene.unwrap_or(0)) {
        return scene.nodes.clone();
    }
    let children: HashSet<usize> =
        doc.nodes.iter().flat_map(|n| n.children.iter().copied()).collect();
    (0..doc.nodes.len()).filter(|i| !children.contains(i)).collect()
}

#[allow(clippy::too_many_arguments)] // internal recursion, not an API
fn visit_node(
    doc: &Doc,
    buffers: &[Vec<u8>],
    materials: &[BlendMaterial],
    index: usize,
    parent: Option<&str>,
    root_fix: Option<Quat>,
    out: &mut BlendScene,
    used_names: &mut HashSet<String>,
    visited: &mut HashSet<usize>,
) {
    let Some(node) = doc.nodes.get(index) else { return };
    if !visited.insert(index) {
        return; // malformed file with a node cycle
    }
    let (mut location, mut rotation, scale) = node_trs(node);
    if let Some(fix) = root_fix {
        location = fix * location;
        rotation = fix * rotation;
    }

    if node.camera.is_some() {
        *out.skipped.entry("CAMERA".into()).or_default() += 1;
    }
    let mesh = node.mesh.and_then(|m| doc.meshes.get(m));
    let base_name = node
        .name
        .clone()
        .or_else(|| mesh.and_then(|m| m.name.clone()))
        .unwrap_or_else(|| if mesh.is_some() { "Mesh".into() } else { "Empty".into() });
    let name = unique_name(&base_name, used_names);

    // every prim of this node's mesh; the first backs the node's own object,
    // the rest become identity-transform children so per-prim materials keep
    let mut prims = mesh
        .map(|m| m.primitives.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|prim| import_prim(doc, buffers, materials, prim, &mut out.skipped))
        .collect::<Vec<_>>()
        .into_iter();
    let (kind, mesh_data, material) = match prims.next() {
        Some((mesh_data, material)) => ("mesh", Some(mesh_data), material),
        None => ("empty", None, None),
    };
    out.objects.push(BlendObject {
        name: name.clone(),
        parent: parent.map(str::to_owned),
        location: location.to_array(),
        rotation_wxyz: [rotation.w, rotation.x, rotation.y, rotation.z],
        scale: scale.to_array(),
        kind: kind.into(),
        mesh: mesh_data,
        material,
        light: None,
        size: None,
        visible: true,
    });
    for (mesh_data, material) in prims {
        let part = unique_name(&base_name, used_names);
        out.objects.push(BlendObject {
            name: part,
            parent: Some(name.clone()),
            location: [0.0; 3],
            rotation_wxyz: [1.0, 0.0, 0.0, 0.0],
            scale: [1.0; 3],
            kind: "mesh".into(),
            mesh: Some(mesh_data),
            material,
            light: None,
            size: None,
            visible: true,
        });
    }
    for &child in &node.children {
        visit_node(doc, buffers, materials, child, Some(&name), None, out, used_names, visited);
    }
}

/// A node's local transform: TRS fields, or the decomposed matrix.
fn node_trs(node: &Node) -> (Vec3, Quat, Vec3) {
    if let Some(m) = node.matrix {
        let (scale, rotation, translation) =
            Mat4::from_cols_array(&m).to_scale_rotation_translation();
        return (translation, rotation.normalize(), scale);
    }
    let rotation = node
        .rotation
        .map(|[x, y, z, w]| Quat::from_xyzw(x, y, z, w).normalize())
        .unwrap_or(Quat::IDENTITY);
    (
        Vec3::from_array(node.translation.unwrap_or([0.0; 3])),
        rotation,
        Vec3::from_array(node.scale.unwrap_or([1.0; 3])),
    )
}

/// One primitive → interchange mesh + material; `None` (and a `skipped`
/// count) when it uses something the importer can't read.
fn import_prim(
    doc: &Doc,
    buffers: &[Vec<u8>],
    materials: &[BlendMaterial],
    prim: &Prim,
    skipped: &mut HashMap<String, u32>,
) -> Option<(BlendMesh, Option<BlendMaterial>)> {
    let mut skip = |key: &str| {
        *skipped.entry(key.into()).or_default() += 1;
        None
    };
    if prim.extensions.contains_key("KHR_draco_mesh_compression") {
        return skip("DRACO PRIMITIVE");
    }
    if prim.mode.unwrap_or(4) != 4 {
        return skip("NON-TRIANGLE PRIMITIVE");
    }
    let Some(&position) = prim.attributes.get("POSITION") else {
        return skip("PRIMITIVE WITHOUT POSITIONS");
    };
    let Ok(positions) = read_floats(doc, buffers, position, "VEC3", 3) else {
        return skip("UNREADABLE PRIMITIVE");
    };
    let normals = prim
        .attributes
        .get("NORMAL")
        .and_then(|&n| read_floats(doc, buffers, n, "VEC3", 3).ok())
        .unwrap_or_default(); // missing/odd normals: recomputed on merge
    // authored UVs; a missing/odd set falls back to box projection on merge
    let uvs = prim
        .attributes
        .get("TEXCOORD_0")
        .and_then(|&t| read_floats(doc, buffers, t, "VEC2", 2).ok())
        .filter(|uvs| uvs.len() / 2 == positions.len() / 3)
        .unwrap_or_default();
    let indices = match prim.indices {
        Some(i) => match read_indices(doc, buffers, i) {
            Ok(indices) => indices,
            Err(_) => return skip("UNREADABLE PRIMITIVE"),
        },
        None => (0..(positions.len() / 3) as u32).collect(),
    };
    let material = prim.material.and_then(|m| materials.get(m)).cloned();
    Some((BlendMesh { positions, normals, indices, uvs }, material))
}

/// Blender-style unique names: `Cube`, `Cube.001`, `Cube.002`, … Uniqueness
/// inside one payload matters because `merge_into_scene` resolves parents by
/// name (the scene then renames again on collision with existing objects).
fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    let mut name = base.to_string();
    let mut counter = 0;
    while !used.insert(name.clone()) {
        counter += 1;
        name = format!("{base}.{counter:03}");
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-triangle .glb: u16 indices, positions interleaved at stride 12,
    /// a red material, one named node.
    fn tiny_glb(extra_json: Option<serde_json::Value>) -> Vec<u8> {
        let positions: [f32; 9] = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        let indices: [u16; 3] = [0, 1, 2];
        let mut bin: Vec<u8> = positions.iter().flat_map(|f| f.to_le_bytes()).collect();
        bin.extend(indices.iter().flat_map(|i| i.to_le_bytes()));
        bin.resize(bin.len().div_ceil(4) * 4, 0);

        let mut json = serde_json::json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"name": "Tri", "mesh": 0, "translation": [0.0, 2.0, 0.0]}],
            "meshes": [{"primitives": [
                {"attributes": {"POSITION": 0}, "indices": 1, "material": 0}
            ]}],
            "materials": [{"pbrMetallicRoughness": {
                "baseColorFactor": [1.0, 0.0, 0.0, 1.0],
                "roughnessFactor": 0.25, "metallicFactor": 0.5
            }}],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3"},
                {"bufferView": 1, "componentType": 5123, "count": 3, "type": "SCALAR"}
            ],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 36},
                {"buffer": 0, "byteOffset": 36, "byteLength": 6}
            ],
            "buffers": [{"byteLength": bin.len()}]
        });
        if let Some(extra) = extra_json {
            json.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        }
        let mut json_bytes = serde_json::to_vec(&json).unwrap();
        json_bytes.resize(json_bytes.len().div_ceil(4) * 4, b' ');

        let mut glb = Vec::new();
        glb.extend(b"glTF");
        glb.extend(2u32.to_le_bytes());
        glb.extend(((12 + 8 + json_bytes.len() + 8 + bin.len()) as u32).to_le_bytes());
        glb.extend((json_bytes.len() as u32).to_le_bytes());
        glb.extend(0x4E4F534Au32.to_le_bytes());
        glb.extend(&json_bytes);
        glb.extend((bin.len() as u32).to_le_bytes());
        glb.extend(0x004E4942u32.to_le_bytes());
        glb.extend(&bin);
        glb
    }

    #[test]
    fn imports_a_glb_triangle_with_material_and_up_fix() {
        let scene = parse(&tiny_glb(None), None).expect("parses");
        assert_eq!(scene.objects.len(), 1);
        let object = &scene.objects[0];
        assert_eq!(object.name, "Tri");
        assert_eq!(object.kind, "mesh");
        let mesh = object.mesh.as_ref().unwrap();
        assert_eq!(mesh.positions.len(), 9);
        assert_eq!(mesh.indices, vec![0, 1, 2]);
        let material = object.material.as_ref().unwrap();
        assert_eq!(material.base_color, [1.0, 0.0, 0.0]);
        // Y-up → Z-up: the node at y=2 lands at z=2, rotated 90° about X
        assert!((Vec3::from_array(object.location) - Vec3::new(0.0, 0.0, 2.0)).length() < 1e-5);
        let [w, x, ..] = object.rotation_wxyz;
        assert!((w - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
        assert!((x - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
    }

    #[test]
    fn imports_gltf_json_with_data_uri_buffer() {
        let positions: [f32; 9] = [0.0; 9];
        let bin: Vec<u8> = positions.iter().flat_map(|f| f.to_le_bytes()).collect();
        let gltf = serde_json::json!({
            "asset": {"version": "2.0"},
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3"}
            ],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": 36}],
            "buffers": [{
                "byteLength": bin.len(),
                "uri": format!("data:application/octet-stream;base64,{}", BASE64.encode(&bin))
            }]
        });
        let scene = parse(serde_json::to_vec(&gltf).unwrap().as_slice(), None).expect("parses");
        assert_eq!(scene.objects.len(), 1);
        // no indices accessor: non-indexed triangles get 0..n
        assert_eq!(scene.objects[0].mesh.as_ref().unwrap().indices, vec![0, 1, 2]);
    }

    #[test]
    fn extra_primitives_become_children_and_bad_modes_are_counted() {
        let scene = parse(
            &tiny_glb(Some(serde_json::json!({
                "meshes": [{"primitives": [
                    {"attributes": {"POSITION": 0}, "indices": 1, "material": 0},
                    {"attributes": {"POSITION": 0}, "indices": 1},
                    {"attributes": {"POSITION": 0}, "mode": 1}
                ]}]
            }))),
            None,
        )
        .expect("parses");
        assert_eq!(scene.objects.len(), 2);
        assert_eq!(scene.objects[1].parent.as_deref(), Some("Tri"));
        assert_eq!(scene.objects[1].name, "Tri.001");
        assert_eq!(scene.skipped.get("NON-TRIANGLE PRIMITIVE"), Some(&1));
    }

    #[test]
    fn child_nodes_keep_local_transforms_and_empties_fill_gaps() {
        let scene = parse(
            &tiny_glb(Some(serde_json::json!({
                "nodes": [
                    {"name": "Root", "children": [1]},
                    {"name": "Leaf", "mesh": 0, "translation": [0.0, 2.0, 0.0]}
                ]
            }))),
            None,
        )
        .expect("parses");
        assert_eq!(scene.objects.len(), 2);
        assert_eq!(scene.objects[0].kind, "empty");
        let leaf = &scene.objects[1];
        assert_eq!(leaf.parent.as_deref(), Some("Root"));
        // the up-fix lives on the root; the child transform stays local
        assert_eq!(leaf.location, [0.0, 2.0, 0.0]);
    }

    #[test]
    fn interleaved_positions_respect_byte_stride() {
        // xyz + 12 junk bytes per vertex, stride 24
        let mut bin = Vec::new();
        for v in 0..3 {
            for c in 0..3 {
                bin.extend(((v * 3 + c) as f32).to_le_bytes());
            }
            bin.extend([0xAAu8; 12]);
        }
        let gltf = serde_json::json!({
            "asset": {"version": "2.0"},
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3"}
            ],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 72, "byteStride": 24}
            ],
            "buffers": [{
                "byteLength": bin.len(),
                "uri": format!("data:application/octet-stream;base64,{}", BASE64.encode(&bin))
            }]
        });
        let scene = parse(serde_json::to_vec(&gltf).unwrap().as_slice(), None).expect("parses");
        let mesh = scene.objects[0].mesh.as_ref().unwrap();
        assert_eq!(mesh.positions, (0..9).map(|i| i as f32).collect::<Vec<_>>());
    }

    #[test]
    fn imports_texture_uvs_and_splits_packed_mr_maps() {
        // 2×1 image with distinct channels per pixel
        let mut rgb = image::RgbImage::new(2, 1);
        rgb.put_pixel(0, 0, image::Rgb([255, 10, 20]));
        rgb.put_pixel(1, 0, image::Rgb([0, 200, 100]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(rgb).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();

        // buffer: 3 positions, 3 UVs, then the embedded png
        let positions: [f32; 9] = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        let uvs: [f32; 6] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0];
        let mut bin: Vec<u8> = positions.iter().flat_map(|f| f.to_le_bytes()).collect();
        bin.extend(uvs.iter().flat_map(|f| f.to_le_bytes()));
        let png_offset = bin.len();
        bin.extend(&png);

        let gltf = serde_json::json!({
            "asset": {"version": "2.0"},
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{
                "attributes": {"POSITION": 0, "TEXCOORD_0": 1}, "material": 0
            }]}],
            "materials": [{"pbrMetallicRoughness": {
                "baseColorTexture": {"index": 0},
                "metallicRoughnessTexture": {"index": 0}
            }}],
            "textures": [{"source": 0}],
            "images": [{"bufferView": 2, "mimeType": "image/png"}],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3"},
                {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC2"}
            ],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 36},
                {"buffer": 0, "byteOffset": 36, "byteLength": 24},
                {"buffer": 0, "byteOffset": png_offset, "byteLength": png.len()}
            ],
            "buffers": [{
                "byteLength": bin.len(),
                "uri": format!("data:application/octet-stream;base64,{}", BASE64.encode(&bin))
            }]
        });
        let scene = parse(serde_json::to_vec(&gltf).unwrap().as_slice(), None).expect("parses");

        let object = &scene.objects[0];
        assert_eq!(object.mesh.as_ref().unwrap().uvs, uvs.to_vec());
        let material = object.material.as_ref().unwrap();
        let file = |key: &str| {
            scene
                .texture_files
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, bytes)| bytes.clone())
                .expect("texture stored")
        };
        // base color passes through raw, under a content-hash key
        let albedo = material.albedo_texture.as_deref().expect("albedo key");
        assert!(albedo.starts_with("gltf/") && albedo.ends_with(".png"));
        assert_eq!(file(albedo), png);
        // the packed MR map split into greyscale files: G → rough, B → metal
        let grey = |key: &str| image::load_from_memory(&file(key)).unwrap().to_luma8();
        let rough = grey(material.roughness_texture.as_deref().unwrap());
        assert_eq!((rough.get_pixel(0, 0)[0], rough.get_pixel(1, 0)[0]), (10, 200));
        let metal = grey(material.metallic_texture.as_deref().unwrap());
        assert_eq!((metal.get_pixel(0, 0)[0], metal.get_pixel(1, 0)[0]), (20, 100));
        // the shared image stored exactly three files: albedo + two splits
        assert_eq!(scene.texture_files.len(), 3);
        assert!(material.normal_texture.is_none() && material.occlusion_texture.is_none());
    }
}

#[cfg(test)]
mod smoke {
    /// Real-world .glb coverage: TRELLIS img2model outputs from the PoC in
    /// this repo. Skips silently on machines without them (like the real-
    /// Blender tests in blend.rs, which need an installed Blender).
    #[test]
    fn parses_real_trellis_glbs() {
        for name in ["rubber_duck", "crown", "pirate_harbor"] {
            let path = format!("../../../trellis-poc/output/{name}.glb");
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let scene = super::parse(&bytes, None).unwrap_or_else(|e| panic!("{name}: {e}"));
            let meshes: usize = scene.objects.iter().filter(|o| o.mesh.is_some()).count();
            let tris: usize = scene.objects.iter()
                .filter_map(|o| o.mesh.as_ref()).map(|m| m.indices.len() / 3).sum();
            let textured = scene.objects.iter()
                .filter(|o| o.material.as_ref().is_some_and(|m| m.albedo_texture.is_some()))
                .count();
            let uvs: usize = scene.objects.iter()
                .filter_map(|o| o.mesh.as_ref()).map(|m| m.uvs.len() / 2).sum();
            println!("{name}: {} objects, {meshes} meshes, {tris} triangles, \
                      {textured} textured, {uvs} uvs, {} texture files, skipped {:?}",
                scene.objects.len(), scene.texture_files.len(), scene.skipped);
            assert!(tris > 0, "{name} produced no triangles");
        }
    }
}
