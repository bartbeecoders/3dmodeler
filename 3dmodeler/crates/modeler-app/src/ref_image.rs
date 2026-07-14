//! Reference images: import, viewport rendering, and the two-point scale
//! calibration tool.
//!
//! The document model (`modeler_core::ReferenceImage`) stores the image
//! bytes base64-embedded, so this module only handles the app concerns:
//! decoding to a GPU texture, drawing an alpha-blended quad on the chosen
//! axis plane, the async file picker (same pattern as io.rs — a blocking
//! dialog inside the render loop would freeze winit), and calibration.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use modeler_core::glam;
use modeler_core::{MarkerKind, ReferenceImage, Scene};
use std::collections::HashMap;
use std::sync::Mutex;
use three_d::*;

// ---------------------------------------------------------------- import --

/// (file stem, raw file bytes) delivered by the async picker.
static PENDING_IMAGE: Mutex<Option<(String, Vec<u8>)>> = Mutex::new(None);

pub fn poll_image() -> Option<(String, Vec<u8>)> {
    PENDING_IMAGE.lock().ok().and_then(|mut p| p.take())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn request_image() {
    std::thread::spawn(|| {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Image", &["png", "jpg", "jpeg"])
            .pick_file()
        else {
            return;
        };
        let Ok(bytes) = std::fs::read(&path) else { return };
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Image".into());
        if let Ok(mut pending) = PENDING_IMAGE.lock() {
            *pending = Some((name, bytes));
        }
    });
}

#[cfg(target_arch = "wasm32")]
pub fn request_image() {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
    let Ok(el) = document.create_element("input") else { return };
    let Ok(input) = el.dyn_into::<web_sys::HtmlInputElement>() else { return };
    input.set_type("file");
    input.set_accept("image/png,image/jpeg");
    if let Some(html_el) = input.dyn_ref::<web_sys::HtmlElement>() {
        let _ = html_el.style().set_property("display", "none");
    }
    if let Some(body) = document.body() {
        let _ = body.append_child(&input);
    }

    let input_for_closure = input.clone();
    let onchange = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
        let Some(file) = input_for_closure.files().and_then(|f| f.get(0)) else { return };
        let name = file
            .name()
            .rsplit_once('.')
            .map(|(stem, _)| stem.to_string())
            .unwrap_or_else(|| file.name());
        let Ok(reader) = web_sys::FileReader::new() else { return };
        let reader_for_load = reader.clone();
        let onload = Closure::once(move || {
            let Ok(result) = reader_for_load.result() else { return };
            let array = js_sys::Uint8Array::new(&result);
            let bytes = array.to_vec();
            if let Ok(mut pending) = PENDING_IMAGE.lock() {
                *pending = Some((name, bytes));
            }
        });
        reader.set_onload(Some(onload.as_ref().unchecked_ref()));
        onload.forget();
        let _ = reader.read_as_array_buffer(&file);
        input_for_closure.remove();
    });
    input.set_onchange(Some(onchange.as_ref().unchecked_ref()));
    input.click();
    onchange.forget();
}

/// Files picked for the smart reference setup dialog (multi-select) — kept
/// separate from `PENDING_IMAGE` so they land in the dialog's tray instead
/// of being added straight to the scene.
static PENDING_SETUP: Mutex<Vec<(String, Vec<u8>)>> = Mutex::new(Vec::new());

pub fn poll_setup_images() -> Vec<(String, Vec<u8>)> {
    PENDING_SETUP
        .lock()
        .map(|mut p| std::mem::take(&mut *p))
        .unwrap_or_default()
}

/// Queue one picked/dropped setup file: PDFs are rendered page by page
/// (each page becomes its own tray image, delivered as it finishes so the
/// tray fills progressively), anything else is passed through as-is.
fn deliver_setup_file(name: String, bytes: Vec<u8>) {
    let push = |name: String, bytes: Vec<u8>| {
        if let Ok(mut pending) = PENDING_SETUP.lock() {
            pending.push((name, bytes));
        }
    };
    if crate::pdf::is_pdf(&bytes) {
        crate::pdf::render_pages(&name, bytes, push);
    } else {
        push(name, bytes);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn request_setup_images() {
    std::thread::spawn(|| {
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("Images & PDF", &["png", "jpg", "jpeg", "pdf"])
            .pick_files()
        else {
            return;
        };
        for path in paths {
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Image".into());
            deliver_setup_file(name, bytes);
        }
    });
}

#[cfg(target_arch = "wasm32")]
pub fn request_setup_images() {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
    let Ok(el) = document.create_element("input") else { return };
    let Ok(input) = el.dyn_into::<web_sys::HtmlInputElement>() else { return };
    input.set_type("file");
    input.set_accept("image/png,image/jpeg,application/pdf");
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
            let name = file
                .name()
                .rsplit_once('.')
                .map(|(stem, _)| stem.to_string())
                .unwrap_or_else(|| file.name());
            let Ok(reader) = web_sys::FileReader::new() else { continue };
            let reader_for_load = reader.clone();
            let onload = Closure::once(move || {
                let Ok(result) = reader_for_load.result() else { return };
                let array = js_sys::Uint8Array::new(&result);
                let bytes = array.to_vec();
                // PDF pages render inline here (no threads on wasm): the UI
                // stalls briefly on large sets, then the tray fills up
                deliver_setup_file(name, bytes);
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

/// A file dropped onto the window from the OS (file manager drag): feed it
/// to the reference setup dialog's tray. PDFs are expanded to one image per
/// page; other non-image files are ignored.
#[cfg(not(target_arch = "wasm32"))]
pub fn push_setup_file(path: &std::path::Path) {
    let supported = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "pdf"));
    if !supported {
        return;
    }
    let Ok(bytes) = std::fs::read(path) else { return };
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Image".into());
    // off the event loop: PDF page rendering takes ~0.2 s per sheet
    std::thread::spawn(move || deliver_setup_file(name, bytes));
}

/// Build a `ReferenceImage` from raw file bytes: validates/decodes the image
/// for its aspect ratio and places it upright on the front (Y) plane.
pub fn make_reference(name: String, bytes: &[u8]) -> Result<ReferenceImage, String> {
    let decoded = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let (w, h) = (decoded.width().max(1), decoded.height().max(1));
    let aspect = h as f32 / w as f32;
    let width_m = 2.0;
    Ok(ReferenceImage {
        id: 0, // assigned by the scene
        name,
        plane: modeler_core::ImagePlane::Y,
        // bottom edge on the ground
        location: glam::Vec3::new(0.0, 0.0, 0.5 * width_m * aspect),
        rotation_deg: 0.0,
        width_m,
        aspect,
        opacity: 0.5,
        visible: true,
        flip_h: false,
        flip_v: false,
        data_base64: BASE64.encode(bytes),
        markers: Vec::new(),
    })
}

// ------------------------------------------------------------- rendering --

struct Cached {
    data_len: usize,
    gm: Gm<Mesh, ColorMaterial>,
}

/// GPU cache for reference-image quads. Texture uploads happen only when the
/// image bytes change; placement and opacity are updated in place each frame.
pub struct RefImageRender {
    cache: HashMap<u64, Cached>,
    order: Vec<u64>,
}

/// Unit quad in the XY plane, centered, with image-style UVs (v=0 at top).
fn quad_mesh() -> CpuMesh {
    CpuMesh {
        positions: Positions::F32(vec![
            vec3(-0.5, -0.5, 0.0),
            vec3(0.5, -0.5, 0.0),
            vec3(0.5, 0.5, 0.0),
            vec3(-0.5, 0.5, 0.0),
        ]),
        uvs: Some(vec![
            vec2(0.0, 1.0),
            vec2(1.0, 1.0),
            vec2(1.0, 0.0),
            vec2(0.0, 0.0),
        ]),
        indices: Indices::U32(vec![0, 1, 2, 0, 2, 3]),
        ..Default::default()
    }
}

/// Pixel dimensions of an embedded image (header parse only, no full
/// decode) — used by the control API to map pixel picks to meters.
pub fn decoded_size(data_base64: &str) -> Option<(u32, u32)> {
    let bytes = BASE64.decode(data_base64).ok()?;
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn decode_texture(data_base64: &str) -> Option<CpuTexture> {
    let bytes = BASE64.decode(data_base64).ok()?;
    let rgba = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (width, height) = rgba.dimensions();
    let data: Vec<[u8; 4]> = rgba.pixels().map(|p| p.0).collect();
    Some(CpuTexture {
        data: TextureData::RgbaU8(data),
        width,
        height,
        ..Default::default()
    })
}

fn placement(image: &ReferenceImage) -> Mat4 {
    let (u, v, n) = image.oriented_basis();
    let (w, h) = (image.width_m, image.height_m());
    let p = image.location;
    Mat4::from_cols(
        vec4(u.x * w, u.y * w, u.z * w, 0.0),
        vec4(v.x * h, v.y * h, v.z * h, 0.0),
        vec4(n.x, n.y, n.z, 0.0),
        vec4(p.x, p.y, p.z, 1.0),
    )
}

impl RefImageRender {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            order: Vec::new(),
        }
    }

    pub fn sync(&mut self, scene: &Scene, context: &Context) {
        self.order.clear();
        for image in scene.reference_images() {
            if !image.visible {
                continue;
            }
            let rebuild = match self.cache.get(&image.id) {
                Some(cached) => cached.data_len != image.data_base64.len(),
                None => true,
            };
            if rebuild {
                let Some(cpu_texture) = decode_texture(&image.data_base64) else {
                    continue; // undecodable bytes: skip rendering, keep the entry
                };
                let material = ColorMaterial {
                    color: Srgba::WHITE,
                    texture: Some(Texture2DRef::from_cpu_texture(context, &cpu_texture)),
                    is_transparent: true,
                    render_states: RenderStates {
                        cull: Cull::None,
                        blend: Blend::TRANSPARENCY,
                        ..Default::default()
                    },
                };
                self.cache.insert(
                    image.id,
                    Cached {
                        data_len: image.data_base64.len(),
                        gm: Gm::new(Mesh::new(context, &quad_mesh()), material),
                    },
                );
            }
            let cached = self.cache.get_mut(&image.id).unwrap();
            cached.gm.set_transformation(placement(image));
            cached.gm.material.color.a = (image.opacity.clamp(0.0, 1.0) * 255.0) as u8;
            self.order.push(image.id);
        }
        let alive: std::collections::HashSet<u64> =
            scene.reference_images().iter().map(|i| i.id).collect();
        self.cache.retain(|id, _| alive.contains(id));
    }

    pub fn models(&self) -> impl Iterator<Item = &Gm<Mesh, ColorMaterial>> {
        self.order.iter().filter_map(|id| self.cache.get(id).map(|c| &c.gm))
    }
}

// ------------------------------------------------------------ calibration --

/// Two-point scale calibration: pick two points on the image in the
/// viewport, then type the real-world distance between them — the image is
/// rescaled so the picked span matches that distance.
pub struct CalibrateTool {
    pub target: Option<u64>,
    pub points: Vec<glam::Vec3>,
    pub distance_input: String,
}

impl CalibrateTool {
    pub fn new() -> Self {
        Self {
            target: None,
            points: Vec::new(),
            distance_input: String::new(),
        }
    }

    pub fn active(&self) -> bool {
        self.target.is_some()
    }

    /// Still picking (fewer than two points chosen)?
    pub fn picking(&self) -> bool {
        self.active() && self.points.len() < 2
    }

    pub fn start(&mut self, image_id: u64) {
        self.target = Some(image_id);
        self.points.clear();
        self.distance_input.clear();
    }

    pub fn cancel(&mut self) {
        self.target = None;
        self.points.clear();
        self.distance_input.clear();
    }

    /// Distance between the two picked points, meters.
    pub fn measured(&self) -> Option<f32> {
        (self.points.len() == 2).then(|| (self.points[1] - self.points[0]).length())
    }

    /// Rescale `image` so the picked span equals `real_m` meters.
    pub fn apply_scale(image: &mut ReferenceImage, measured_m: f32, real_m: f32) {
        let factor = real_m / measured_m.max(1e-6);
        image.width_m = (image.width_m * factor).max(0.01);
    }

    /// Feed a viewport pick ray; intersects the target image's plane.
    pub fn add_ray(&mut self, scene: &Scene, origin: glam::Vec3, direction: glam::Vec3) {
        if !self.picking() {
            return;
        }
        let Some(image) = self
            .target
            .and_then(|id| scene.reference_images().iter().find(|i| i.id == id))
        else {
            self.cancel();
            return;
        };
        let (u_axis, v_axis, normal) = image.oriented_basis();
        let denom = direction.dot(normal);
        if denom.abs() < 1e-6 {
            return; // looking along the plane — no usable intersection
        }
        let t = (image.location - origin).dot(normal) / denom;
        if t <= 0.0 {
            return;
        }
        let point = origin + direction * t;
        // only accept picks ON the image (small margin for edge clicks) —
        // clicks elsewhere are stray (missed the image / hit UI chrome)
        let rel = point - image.location;
        let (u, v) = (rel.dot(u_axis), rel.dot(v_axis));
        let margin = 0.05 * image.width_m;
        if u.abs() > 0.5 * image.width_m + margin || v.abs() > 0.5 * image.height_m() + margin {
            return;
        }
        self.points.push(point);
    }
}

// --------------------------------------------------------------- AI markers --

/// Viewport drawing tool for AI markers: pick a point / polyline / polygon
/// directly on a reference image, then name it and attach a note for the AI
/// (the save dialog lives in ui.rs). Picks intersect the image plane like
/// the calibration tool; points are kept in normalized image coordinates.
pub struct MarkerTool {
    pub target: Option<u64>,
    pub kind: MarkerKind,
    pub points: Vec<glam::Vec2>,
    /// Picking finished — the name/note dialog is up.
    pub done: bool,
    pub name_input: String,
    pub note_input: String,
}

impl MarkerTool {
    pub fn new() -> Self {
        Self {
            target: None,
            kind: MarkerKind::Point,
            points: Vec::new(),
            done: false,
            name_input: String::new(),
            note_input: String::new(),
        }
    }

    pub fn active(&self) -> bool {
        self.target.is_some()
    }

    /// Still picking points in the viewport?
    pub fn picking(&self) -> bool {
        self.active() && !self.done
    }

    pub fn start(&mut self, image_id: u64, kind: MarkerKind) {
        self.target = Some(image_id);
        self.kind = kind;
        self.points.clear();
        self.done = false;
        self.name_input.clear();
        self.note_input.clear();
    }

    pub fn cancel(&mut self) {
        self.target = None;
        self.points.clear();
        self.done = false;
        self.name_input.clear();
        self.note_input.clear();
    }

    /// Enter: accept the picked points when they make a valid marker
    /// (a point marker completes on its single click instead).
    pub fn finish(&mut self) {
        if self.picking() && self.points.len() >= self.kind.min_points() {
            self.done = true;
        }
    }

    /// Feed a viewport pick ray; intersects the target image's plane and
    /// stores the pick in image coordinates (edge picks clamp onto the
    /// image, picks well off it are stray clicks and ignored).
    pub fn add_ray(&mut self, scene: &Scene, origin: glam::Vec3, direction: glam::Vec3) {
        if !self.picking() {
            return;
        }
        let Some(image) = self
            .target
            .and_then(|id| scene.reference_images().iter().find(|i| i.id == id))
        else {
            self.cancel();
            return;
        };
        let (_, _, normal) = image.oriented_basis();
        let denom = direction.dot(normal);
        if denom.abs() < 1e-6 {
            return; // looking along the plane — no usable intersection
        }
        let t = (image.location - origin).dot(normal) / denom;
        if t <= 0.0 {
            return;
        }
        let uv = image.world_to_uv(origin + direction * t);
        let margin = 0.05;
        if uv.x < -margin || uv.x > 1.0 + margin || uv.y < -margin || uv.y > 1.0 + margin {
            return;
        }
        self.points.push(uv.clamp(glam::Vec2::ZERO, glam::Vec2::ONE));
        if self.kind == MarkerKind::Point {
            self.done = true;
        }
    }
}

// --- viewport move tool -----------------------------------------------------

struct ImageGrab {
    id: u64,
    original: glam::Vec3,
    start_mouse: (f32, f32), // physical px, bottom-left origin
    cur_mouse: (f32, f32),
    /// None = screen plane, Some(axis) = world axis constraint.
    constraint: Option<usize>,
    status: String,
}

/// G-move for the reference image selected in the viewport: same gestures as
/// the object grab (mouse moves in the screen plane, X/Y/Z constrains,
/// LMB/Enter confirms, RMB/Esc cancels and restores).
pub struct ImageMoveTool {
    state: Option<ImageGrab>,
    last_mouse: (f32, f32),
}

impl ImageMoveTool {
    pub fn new() -> Self {
        Self { state: None, last_mouse: (0.0, 0.0) }
    }

    pub fn active(&self) -> bool {
        self.state.is_some()
    }

    pub fn status_line(&self) -> Option<String> {
        self.state.as_ref().map(|s| s.status.clone())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        camera: &crate::camera::BlenderCamera,
        viewport: Viewport,
        scene: &mut Scene,
        selected_image: Option<u64>,
        egui_owns_keyboard: bool,
        unit: crate::settings::Unit,
    ) {
        let mut confirm = false;
        let mut cancel = false;

        for event in events.iter_mut() {
            match event {
                Event::MouseMotion { position, handled, .. } => {
                    self.last_mouse = (position.x, position.y);
                    if let Some(state) = &mut self.state {
                        state.cur_mouse = self.last_mouse;
                        *handled = true;
                    }
                }
                Event::MousePress { button, position, handled, .. }
                    if self.state.is_some() && !*handled =>
                {
                    self.last_mouse = (position.x, position.y);
                    match button {
                        MouseButton::Left => confirm = true,
                        MouseButton::Right => cancel = true,
                        MouseButton::Middle => {}
                    }
                    if *button != MouseButton::Middle {
                        *handled = true;
                    }
                }
                Event::KeyPress { kind, handled, .. } if self.state.is_some() && !*handled => {
                    match kind {
                        Key::Enter => confirm = true,
                        Key::Escape => cancel = true,
                        _ => {}
                    }
                    *handled = true; // the tool owns the keyboard while moving
                }
                Event::Text(text) if !egui_owns_keyboard && !text.is_empty() => {
                    let consumed = match (self.state.as_mut(), text.as_str()) {
                        (None, "g" | "G") => {
                            if let Some(id) = selected_image {
                                if let Some(image) =
                                    scene.reference_images().iter().find(|r| r.id == id)
                                {
                                    self.state = Some(ImageGrab {
                                        id,
                                        original: image.location,
                                        start_mouse: self.last_mouse,
                                        cur_mouse: self.last_mouse,
                                        constraint: None,
                                        status: String::new(),
                                    });
                                }
                                true
                            } else {
                                false
                            }
                        }
                        (Some(state), "x" | "X" | "y" | "Y" | "z" | "Z") => {
                            let axis = match text.to_ascii_lowercase().as_str() {
                                "x" => 0,
                                "y" => 1,
                                _ => 2,
                            };
                            state.constraint =
                                if state.constraint == Some(axis) { None } else { Some(axis) };
                            true
                        }
                        (Some(_), _) => true, // moving owns typed input
                        _ => false,
                    };
                    if consumed {
                        text.clear();
                    }
                }
                _ => {}
            }
        }

        if cancel {
            if let Some(state) = self.state.take() {
                if let Some(image) = scene.reference_image_mut(state.id) {
                    image.location = state.original;
                }
            }
            return;
        }

        self.apply(camera, viewport, scene, unit);

        if confirm {
            self.state = None; // location already applied
        }
    }

    fn apply(
        &mut self,
        camera: &crate::camera::BlenderCamera,
        viewport: Viewport,
        scene: &mut Scene,
        unit: crate::settings::Unit,
    ) {
        let Some(state) = &mut self.state else { return };
        let (right, up, _) = camera.screen_basis();
        let (right, up) = (
            glam::Vec3::new(right.x, right.y, right.z),
            glam::Vec3::new(up.x, up.y, up.z),
        );
        let wpp = camera.world_per_pixel_at(
            viewport,
            vec3(state.original.x, state.original.y, state.original.z),
        );
        let dx = state.cur_mouse.0 - state.start_mouse.0;
        let dy = state.cur_mouse.1 - state.start_mouse.1;
        let mut delta = right * (dx * wpp) + up * (dy * wpp);
        if let Some(axis) = state.constraint {
            let a = [glam::Vec3::X, glam::Vec3::Y, glam::Vec3::Z][axis];
            delta = a * delta.dot(a);
        }

        let shown = delta * unit.per_meter();
        state.status = format!(
            "Move image: ({:.p$}, {:.p$}, {:.p$}) {}{}   |   LMB/Enter confirm · RMB/Esc cancel",
            shown.x,
            shown.y,
            shown.z,
            unit.suffix(),
            match state.constraint {
                Some(a) => format!("  along {}", ["X", "Y", "Z"][a]),
                None => String::new(),
            },
            p = unit.decimals(),
        );

        let target = state.original + delta;
        if let Some(image) = scene.reference_image_mut(state.id) {
            image.location = target;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::{ImagePlane, Transform, Primitive};

    fn test_image(plane: ImagePlane) -> ReferenceImage {
        ReferenceImage {
            id: 0,
            name: "test".into(),
            plane,
            location: glam::Vec3::new(0.0, 0.0, 1.0),
            rotation_deg: 0.0,
            width_m: 2.0,
            aspect: 1.0,
            opacity: 0.5,
            visible: true,
            flip_h: false,
            flip_v: false,
            data_base64: String::new(),
            markers: Vec::new(),
        }
    }

    #[test]
    fn calibrate_two_points_measures_plane_distance() {
        let mut scene = Scene::new();
        scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let id = scene.add_reference_image(test_image(ImagePlane::Y));

        let mut tool = CalibrateTool::new();
        tool.start(id);
        // rays straight down -Y hit the y=0 plane at the ray's x/z
        tool.add_ray(&scene, glam::Vec3::new(-0.4, 5.0, 1.0), glam::Vec3::new(0.0, -1.0, 0.0));
        tool.add_ray(&scene, glam::Vec3::new(0.4, 5.0, 1.0), glam::Vec3::new(0.0, -1.0, 0.0));
        assert!((tool.measured().unwrap() - 0.8).abs() < 1e-5);

        // picks outside the image rectangle are rejected
        tool.points.clear();
        tool.add_ray(&scene, glam::Vec3::new(3.0, 5.0, 1.0), glam::Vec3::new(0.0, -1.0, 0.0));
        assert!(tool.points.is_empty(), "off-image pick must be ignored");

        // parallel ray is ignored
        tool.add_ray(&scene, glam::Vec3::ZERO, glam::Vec3::new(1.0, 0.0, 0.0));
        assert!(tool.points.is_empty());
    }

    #[test]
    fn marker_tool_picks_normalized_image_points() {
        let mut scene = Scene::new();
        // 2 m wide, aspect 1 -> 2 m tall, centered at (0, 0, 1) on the Y plane
        let id = scene.add_reference_image(test_image(ImagePlane::Y));

        let mut tool = MarkerTool::new();
        tool.start(id, MarkerKind::Line);
        assert!(tool.picking());
        // center pick -> uv (0.5, 0.5)
        tool.add_ray(&scene, glam::Vec3::new(0.0, 5.0, 1.0), glam::Vec3::new(0.0, -1.0, 0.0));
        // just past the right edge: clamped onto the image -> uv (1, 0.5)
        tool.add_ray(&scene, glam::Vec3::new(1.02, 5.0, 1.0), glam::Vec3::new(0.0, -1.0, 0.0));
        // a stray click far off the image is ignored
        tool.add_ray(&scene, glam::Vec3::new(4.0, 5.0, 1.0), glam::Vec3::new(0.0, -1.0, 0.0));
        assert_eq!(tool.points.len(), 2);
        assert!((tool.points[0] - glam::Vec2::new(0.5, 0.5)).length() < 1e-5);
        assert!((tool.points[1] - glam::Vec2::new(1.0, 0.5)).length() < 1e-5);
        assert!(!tool.done, "a line keeps picking until finish()");
        tool.finish();
        assert!(tool.done && !tool.picking());

        // an area needs 3 points: finish() with 2 keeps picking
        tool.start(id, MarkerKind::Area);
        tool.add_ray(&scene, glam::Vec3::new(0.0, 5.0, 1.0), glam::Vec3::new(0.0, -1.0, 0.0));
        tool.add_ray(&scene, glam::Vec3::new(0.5, 5.0, 1.0), glam::Vec3::new(0.0, -1.0, 0.0));
        tool.finish();
        assert!(tool.picking());

        // a point marker completes on its single click; v grows downward
        tool.start(id, MarkerKind::Point);
        tool.add_ray(&scene, glam::Vec3::new(0.0, 5.0, 1.5), glam::Vec3::new(0.0, -1.0, 0.0));
        assert!(tool.done);
        assert!((tool.points[0] - glam::Vec2::new(0.5, 0.25)).length() < 1e-5);
    }

    #[test]
    fn apply_scale_rescales_to_real_distance() {
        // image is 2 m wide; the user picked two points 1 m apart and says
        // the real distance is 4 m -> the image must become 8 m wide
        let mut img = test_image(ImagePlane::Y);
        CalibrateTool::apply_scale(&mut img, 1.0, 4.0);
        assert!((img.width_m - 8.0).abs() < 1e-5);
        // degenerate measured distance must not explode or zero the image
        CalibrateTool::apply_scale(&mut img, 0.0, 1.0);
        assert!(img.width_m.is_finite() && img.width_m >= 0.01);
    }

    #[test]
    fn placement_spans_width_and_height() {
        let mut img = test_image(ImagePlane::Z);
        img.aspect = 0.5;
        let m = placement(&img);
        // u column scaled by width, v column by height
        assert!((m.x.x - 2.0).abs() < 1e-6);
        assert!((m.y.y - 1.0).abs() < 1e-6);
    }

    #[test]
    fn make_reference_rejects_garbage() {
        assert!(make_reference("x".into(), &[1, 2, 3]).is_err());
    }
}
