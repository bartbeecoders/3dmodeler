//! Blender-style object edit mode.
//!
//! Tab enters/leaves edit mode on the active object. Inside, the selection
//! mode is vertex / edge / face — picked with 1 / 2 / 3, matched on the
//! TYPED character so AZERTY's unshifted & / é / " work too. Click selects
//! an element, G moves it (X/Y/Z constrain to an axis, LMB/Enter confirm,
//! RMB/Esc cancel). The first edit bakes the primitive into an editable
//! mesh stored on the object (saved with the scene).
//!
//! Topology is a welded view of the triangle mesh: coincident vertices are
//! merged, faces are connected coplanar triangle groups (a cube shows 6
//! faces, not 12 triangles) and edges are the boundaries between groups.

use crate::camera::BlenderCamera;
use crate::selection::Selection;
use crate::settings::Unit;
use modeler_core::glam::Vec3;
use modeler_core::{MeshData, ObjectId, Scene, Transform};
use three_d::{Event, Key, MouseButton, Viewport};

const VERTEX_PICK_PX: f32 = 14.0;
const EDGE_PICK_PX: f32 = 10.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectMode {
    Vertex,
    Edge,
    Face,
}

impl SelectMode {
    pub fn label(self) -> &'static str {
        match self {
            SelectMode::Vertex => "Vertex",
            SelectMode::Edge => "Edge",
            SelectMode::Face => "Face",
        }
    }
}

/// A selected element, in welded-topology indices.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Element {
    Vertex(usize),
    /// Welded vertex pair, sorted.
    Edge(usize, usize),
    /// Index into `Topology::faces`.
    Face(usize),
}

/// One face of the welded topology: a connected, coplanar triangle group.
pub struct FaceGroup {
    pub tris: Vec<usize>,
    pub verts: Vec<usize>,
    /// Boundary edges (welded pairs) for drawing the outline.
    pub outline: Vec<(usize, usize)>,
}

/// Welded view of a triangle mesh.
pub struct Topology {
    /// Unique positions (local space).
    pub verts: Vec<Vec3>,
    /// Welded index per mesh-position index.
    pub weld_of: Vec<usize>,
    /// Welded corner ids per triangle.
    pub tris: Vec<[usize; 3]>,
    pub faces: Vec<FaceGroup>,
    /// Sharp edges: boundaries of the face groups, deduplicated.
    pub edges: Vec<(usize, usize)>,
}

fn weld_key(p: Vec3) -> (i64, i64, i64) {
    let q = 1.0 / 1e-5;
    ((p.x * q).round() as i64, (p.y * q).round() as i64, (p.z * q).round() as i64)
}

pub fn build_topology(mesh: &MeshData) -> Topology {
    use std::collections::HashMap;

    let mut key_to_weld: HashMap<(i64, i64, i64), usize> = HashMap::new();
    let mut verts: Vec<Vec3> = Vec::new();
    let mut weld_of: Vec<usize> = Vec::with_capacity(mesh.positions.len());
    for &p in &mesh.positions {
        let id = *key_to_weld.entry(weld_key(p)).or_insert_with(|| {
            verts.push(p);
            verts.len() - 1
        });
        weld_of.push(id);
    }

    let tris: Vec<[usize; 3]> = mesh
        .indices
        .chunks_exact(3)
        .map(|t| [weld_of[t[0] as usize], weld_of[t[1] as usize], weld_of[t[2] as usize]])
        .collect();
    let tri_normal = |t: &[usize; 3]| -> Vec3 {
        (verts[t[1]] - verts[t[0]])
            .cross(verts[t[2]] - verts[t[0]])
            .normalize_or_zero()
    };
    let normals: Vec<Vec3> = tris.iter().map(tri_normal).collect();

    // adjacency: welded edge -> triangles using it
    let edge_key = |a: usize, b: usize| (a.min(b), a.max(b));
    let mut edge_tris: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    for (ti, t) in tris.iter().enumerate() {
        for k in 0..3 {
            edge_tris.entry(edge_key(t[k], t[(k + 1) % 3])).or_default().push(ti);
        }
    }

    // faces: flood-fill connected triangles with matching normals
    let mut face_of = vec![usize::MAX; tris.len()];
    let mut faces: Vec<FaceGroup> = Vec::new();
    for seed in 0..tris.len() {
        if face_of[seed] != usize::MAX {
            continue;
        }
        let face_index = faces.len();
        let mut stack = vec![seed];
        let mut members = Vec::new();
        face_of[seed] = face_index;
        while let Some(ti) = stack.pop() {
            members.push(ti);
            let t = tris[ti];
            for k in 0..3 {
                for &other in &edge_tris[&edge_key(t[k], t[(k + 1) % 3])] {
                    if face_of[other] == usize::MAX
                        && normals[other].dot(normals[seed]) > 0.9995
                    {
                        face_of[other] = face_index;
                        stack.push(other);
                    }
                }
            }
        }
        // boundary edges: used by exactly one triangle of this group
        let mut edge_use: HashMap<(usize, usize), u32> = HashMap::new();
        for &ti in &members {
            let t = tris[ti];
            for k in 0..3 {
                *edge_use.entry(edge_key(t[k], t[(k + 1) % 3])).or_default() += 1;
            }
        }
        let outline: Vec<(usize, usize)> =
            edge_use.iter().filter(|(_, &n)| n == 1).map(|(&e, _)| e).collect();
        let mut group_verts: Vec<usize> = members.iter().flat_map(|&ti| tris[ti]).collect();
        group_verts.sort_unstable();
        group_verts.dedup();
        faces.push(FaceGroup { tris: members, verts: group_verts, outline });
    }

    // sharp edges = union of all face outlines
    let mut edges: Vec<(usize, usize)> = faces.iter().flat_map(|f| f.outline.iter().copied()).collect();
    edges.sort_unstable();
    edges.dedup();

    Topology { verts, weld_of, tris, faces, edges }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum TransformKind {
    Move,
    Rotate,
    Scale,
}

struct Grab {
    kind: TransformKind,
    /// Affected welded ids with their original local positions.
    originals: Vec<(usize, Vec3)>,
    start_mouse: (f32, f32),
    cur_mouse: (f32, f32),
    /// None = screen plane / view axis / uniform, Some(axis) = world axis.
    constraint: Option<usize>,
    /// World-space pivot at grab start: element centroid (move uses it for
    /// world-per-pixel scaling; rotate/scale transform around it).
    pivot_world: Vec3,
    status: String,
}

pub struct EditMode {
    active: Option<ObjectId>,
    pub mode: SelectMode,
    selected: Option<Element>,
    topo: Option<Topology>,
    topo_revision: u64,
    /// Scene instance we entered on: a replaced document (File ▸ New,
    /// control new_scene) restarts object ids, so the active id could
    /// silently rebind to an unrelated object.
    scene_instance: u64,
    grab: Option<Grab>,
    last_mouse: (f32, f32),
}

fn gv(v: three_d::Vec3) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

fn local_to_world(t: &Transform, p: Vec3) -> Vec3 {
    t.location + t.rotation * (p * t.scale)
}

fn world_to_local(t: &Transform, w: Vec3) -> Vec3 {
    t.inverse_transform_point(w)
}

/// Rotate local mesh positions around a world-space pivot: local -> world,
/// rotate, back to local. Exact for any object transform, since edited
/// meshes store arbitrary per-vertex positions.
fn rotate_positions(
    world: &Transform,
    originals: &[(usize, Vec3)],
    pivot_world: Vec3,
    rotation: modeler_core::glam::Quat,
) -> Vec<(usize, Vec3)> {
    originals
        .iter()
        .map(|&(weld, local)| {
            let w = local_to_world(world, local);
            (weld, world_to_local(world, pivot_world + rotation * (w - pivot_world)))
        })
        .collect()
}

/// Scale local mesh positions around a world-space pivot (per-world-axis
/// factors, like the object-mode S operator).
fn scale_positions(
    world: &Transform,
    originals: &[(usize, Vec3)],
    pivot_world: Vec3,
    factors: Vec3,
) -> Vec<(usize, Vec3)> {
    originals
        .iter()
        .map(|&(weld, local)| {
            let w = local_to_world(world, local);
            (weld, world_to_local(world, pivot_world + (w - pivot_world) * factors))
        })
        .collect()
}

impl EditMode {
    pub fn new() -> Self {
        Self {
            active: None,
            mode: SelectMode::Face,
            selected: None,
            topo: None,
            topo_revision: 0,
            scene_instance: 0,
            grab: None,
            last_mouse: (0.0, 0.0),
        }
    }

    pub fn active(&self) -> bool {
        self.active.is_some()
    }

    pub fn grabbing(&self) -> bool {
        self.grab.is_some()
    }

    /// The object being edited, if edit mode is on.
    pub fn active_object(&self) -> Option<ObjectId> {
        self.active
    }

    /// Local-space point of the current element selection: the vertex
    /// position, the edge midpoint, or the face centroid. Local mesh space
    /// is exactly the space pivot/anchor points live in.
    pub fn selected_point(&self) -> Option<Vec3> {
        let topo = self.topo.as_ref()?;
        Some(match self.selected? {
            Element::Vertex(v) => *topo.verts.get(v)?,
            Element::Edge(a, b) => 0.5 * (*topo.verts.get(a)? + *topo.verts.get(b)?),
            Element::Face(f) => {
                let group = topo.faces.get(f)?;
                group.verts.iter().map(|&v| topo.verts[v]).sum::<Vec3>()
                    / group.verts.len().max(1) as f32
            }
        })
    }

    /// Right-click pick for the context menu: select the element under the
    /// cursor and return (object, local point, element kind label).
    pub fn context_pick(
        &mut self,
        scene: &Scene,
        camera: &BlenderCamera,
        viewport: Viewport,
        x: f32,
        y: f32,
    ) -> Option<(ObjectId, Vec3, &'static str)> {
        let id = self.active?;
        self.pick(scene, camera, viewport, x, y);
        let point = self.selected_point()?;
        let label = match self.selected? {
            Element::Vertex(_) => "Vertex",
            Element::Edge(..) => "Edge",
            Element::Face(_) => "Face",
        };
        Some((id, point, label))
    }

    /// Set the edited object's pivot or anchor to the selected element.
    fn set_point_from_selection(&self, scene: &mut Scene, anchor: bool) {
        let (Some(id), Some(point)) = (self.active, self.selected_point()) else { return };
        if let Some(object) = scene.object_mut(id) {
            if anchor {
                object.anchor = point;
            } else {
                object.pivot = point;
            }
        }
    }

    /// Status-bar line while edit mode is on.
    pub fn status_line(&self) -> Option<String> {
        let _ = self.active?;
        Some(match &self.grab {
            Some(grab) => grab.status.clone(),
            None => format!(
                "EDIT MODE ({}) · click select · G/R/S transform · P/A set pivot/anchor · \
                 1/2/3 vertex/edge/face · Tab exit",
                self.mode.label()
            ),
        })
    }

    /// Leave edit mode (also called when the object vanishes).
    pub fn exit(&mut self, scene: &mut Scene) {
        if let Some(grab) = self.grab.take() {
            self.cancel_grab_inner(scene, grab);
        }
        self.active = None;
        self.selected = None;
        self.topo = None;
    }

    fn enter(&mut self, id: ObjectId, scene: &Scene) {
        self.active = Some(id);
        self.selected = None;
        self.grab = None;
        self.scene_instance = scene.instance();
        self.rebuild_topology(scene);
    }

    fn rebuild_topology(&mut self, scene: &Scene) {
        let Some(object) = self.active.and_then(|id| scene.object(id)) else {
            self.topo = None;
            return;
        };
        self.topo = Some(build_topology(&object.render_mesh()));
        self.topo_revision = object.mesh_revision;
    }

    /// Keep state consistent with the scene (object deleted, mesh changed
    /// by undo, …). Call once per frame before handling events.
    pub fn sync(&mut self, scene: &mut Scene) {
        let Some(id) = self.active else { return };
        if scene.instance() != self.scene_instance {
            self.exit(scene); // whole document replaced under us
            return;
        }
        match scene.object(id) {
            None => self.exit(scene),
            Some(object) => {
                if object.mesh_revision != self.topo_revision || self.topo.is_none() {
                    if self.grab.is_none() {
                        let sel = self.selected;
                        self.rebuild_topology(scene);
                        // keep the selection if it still exists
                        self.selected = sel.filter(|e| self.element_exists(*e));
                    }
                }
            }
        }
    }

    fn element_exists(&self, element: Element) -> bool {
        let Some(topo) = &self.topo else { return false };
        match element {
            Element::Vertex(v) => v < topo.verts.len(),
            Element::Edge(a, b) => a < topo.verts.len() && b < topo.verts.len(),
            Element::Face(f) => f < topo.faces.len(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        camera: &BlenderCamera,
        viewport: Viewport,
        scene: &mut Scene,
        selection: &Selection,
        egui_owns_keyboard: bool,
        tab_pressed: bool,
        sim_stopped: bool,
        unit: Unit,
    ) {
        // Tab (pre-claimed in main before egui) toggles edit mode
        if tab_pressed && sim_stopped {
            if self.active.is_some() {
                self.exit(scene);
            } else if let Some(id) = selection.active() {
                self.enter(id, scene);
            }
        }
        if self.active.is_none() {
            return;
        }

        let mut confirm = false;
        let mut cancel = false;
        let mut click: Option<(f32, f32)> = None;

        for event in events.iter_mut() {
            match event {
                Event::MouseMotion { position, handled, .. } => {
                    self.last_mouse = (position.x, position.y);
                    if let Some(grab) = &mut self.grab {
                        grab.cur_mouse = self.last_mouse;
                        *handled = true;
                    }
                }
                Event::MousePress { button, position, handled, .. }
                    if !*handled && *button != MouseButton::Middle =>
                {
                    self.last_mouse = (position.x, position.y);
                    if self.grab.is_some() {
                        match button {
                            MouseButton::Left => confirm = true,
                            MouseButton::Right => cancel = true,
                            MouseButton::Middle => {}
                        }
                    } else if *button == MouseButton::Left {
                        click = Some((position.x, position.y));
                    }
                    *handled = true;
                }
                Event::MouseRelease { button, handled, .. }
                    if self.grab.is_some() && *button != MouseButton::Middle =>
                {
                    *handled = true;
                }
                Event::KeyPress { kind, handled, .. } if !*handled && !egui_owns_keyboard => {
                    match kind {
                        Key::Enter if self.grab.is_some() => {
                            confirm = true;
                            *handled = true;
                        }
                        Key::Escape if self.grab.is_some() => {
                            cancel = true;
                            *handled = true;
                        }
                        // the tool owns the keyboard while grabbing (digits
                        // are camera views otherwise)
                        _ if self.grab.is_some() => *handled = true,
                        _ => {}
                    }
                }
                Event::Text(text) if !egui_owns_keyboard && !text.is_empty() => {
                    let consumed = self.text_input(text.as_str(), scene);
                    if consumed {
                        text.clear();
                    }
                }
                _ => {}
            }
        }

        if cancel {
            if let Some(grab) = self.grab.take() {
                self.cancel_grab_inner(scene, grab);
            }
        }
        if let Some((x, y)) = click {
            self.pick(scene, camera, viewport, x, y);
        }
        self.apply_grab(scene, camera, viewport, unit);
        if confirm {
            self.grab = None; // positions already applied
        }
    }

    /// Typed characters: selection modes (AZERTY unshifted digits included)
    /// and G. Returns true when consumed.
    fn text_input(&mut self, text: &str, scene: &mut Scene) -> bool {
        match text {
            // 1 / 2 / 3 — AZERTY types & / é / " without shift
            "1" | "&" => {
                self.set_mode(SelectMode::Vertex);
                true
            }
            "2" | "é" => {
                self.set_mode(SelectMode::Edge);
                true
            }
            "3" | "\"" => {
                self.set_mode(SelectMode::Face);
                true
            }
            "g" | "G" => {
                self.start_transform(TransformKind::Move, scene);
                true
            }
            "r" | "R" => {
                if self.grab.is_none() {
                    self.start_transform(TransformKind::Rotate, scene);
                }
                true
            }
            "s" | "S" => {
                if self.grab.is_none() {
                    self.start_transform(TransformKind::Scale, scene);
                }
                true
            }
            // P / A: the selected element becomes the pivot / anchor point
            "p" | "P" => {
                if self.grab.is_none() {
                    self.set_point_from_selection(scene, false);
                }
                true
            }
            "a" | "A" => {
                if self.grab.is_none() {
                    self.set_point_from_selection(scene, true);
                }
                true
            }
            // axis constraints while transforming; swallow the object-mode
            // tools so they can't fire mid-edit
            "x" | "X" | "y" | "Y" | "z" | "Z" | "D" => {
                if self.grab.is_some() {
                    self.grab_constraint(text);
                }
                true
            }
            _ => false,
        }
    }

    fn set_mode(&mut self, mode: SelectMode) {
        if self.mode != mode {
            self.mode = mode;
            self.selected = None;
        }
    }

    fn grab_constraint(&mut self, text: &str) {
        let Some(grab) = &mut self.grab else { return };
        let axis = match text.to_ascii_lowercase().as_str() {
            "x" => Some(0),
            "y" => Some(1),
            "z" => Some(2),
            _ => return,
        };
        grab.constraint = if grab.constraint == axis { None } else { axis };
    }

    // --- picking -----------------------------------------------------------

    fn pick(
        &mut self,
        scene: &Scene,
        camera: &BlenderCamera,
        viewport: Viewport,
        x: f32,
        y: f32,
    ) {
        let Some(object) = self.active.and_then(|id| scene.object(id)) else { return };
        let Some(topo) = &self.topo else { return };
        let world = scene.world_transform(object.id);
        let to_screen = |p: Vec3| -> (f32, f32) {
            let w = local_to_world(&world, p);
            camera.world_to_screen(viewport, three_d::vec3(w.x, w.y, w.z))
        };

        self.selected = match self.mode {
            SelectMode::Vertex => {
                let mut best: Option<(f32, usize)> = None;
                for (i, &v) in topo.verts.iter().enumerate() {
                    let (sx, sy) = to_screen(v);
                    let d = ((sx - x).powi(2) + (sy - y).powi(2)).sqrt();
                    if d < VERTEX_PICK_PX && best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, i));
                    }
                }
                best.map(|(_, i)| Element::Vertex(i))
            }
            SelectMode::Edge => {
                let mut best: Option<(f32, (usize, usize))> = None;
                for &(a, b) in &topo.edges {
                    let pa = to_screen(topo.verts[a]);
                    let pb = to_screen(topo.verts[b]);
                    let d = point_segment_distance((x, y), pa, pb);
                    if d < EDGE_PICK_PX && best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, (a, b)));
                    }
                }
                best.map(|(_, (a, b))| Element::Edge(a, b))
            }
            SelectMode::Face => {
                // ray-cast the object's triangles in world space
                let (origin, direction) = camera.pick_ray(viewport, x, y);
                let (origin, direction) = (gv(origin), gv(direction));
                let mut best: Option<(f32, usize)> = None;
                for (ti, t) in topo.tris.iter().enumerate() {
                    let a = local_to_world(&world, topo.verts[t[0]]);
                    let b = local_to_world(&world, topo.verts[t[1]]);
                    let c = local_to_world(&world, topo.verts[t[2]]);
                    if let Some(hit) = ray_triangle(origin, direction, a, b, c) {
                        if best.is_none_or(|(bt, _)| hit < bt) {
                            best = Some((hit, ti));
                        }
                    }
                }
                best.map(|(_, ti)| {
                    Element::Face(
                        self.topo
                            .as_ref()
                            .unwrap()
                            .faces
                            .iter()
                            .position(|f| f.tris.contains(&ti))
                            .unwrap_or(0),
                    )
                })
            }
        };
    }

    // --- grab (move) --------------------------------------------------------

    fn affected_verts(&self) -> Vec<usize> {
        let Some(topo) = &self.topo else { return Vec::new() };
        match self.selected {
            Some(Element::Vertex(v)) => vec![v],
            Some(Element::Edge(a, b)) => vec![a, b],
            Some(Element::Face(f)) => topo.faces.get(f).map(|g| g.verts.clone()).unwrap_or_default(),
            None => Vec::new(),
        }
    }

    fn start_transform(&mut self, kind: TransformKind, scene: &mut Scene) {
        if self.grab.is_some() {
            return;
        }
        let affected = self.affected_verts();
        if affected.is_empty() {
            return;
        }
        // rotating/scaling a single vertex is a no-op — needs an edge/face
        if kind != TransformKind::Move && affected.len() < 2 {
            return;
        }
        let Some(id) = self.active else { return };
        // first edit bakes the primitive into an editable mesh
        let needs_bake = scene.object(id).is_some_and(|o| o.edited_mesh.is_none());
        if needs_bake {
            let mesh = scene.object(id).unwrap().render_mesh();
            if let Some(object) = scene.object_mut(id) {
                object.edited_mesh = Some(mesh);
            }
        }
        let Some(topo) = &self.topo else { return };
        let world = scene.world_transform(id);
        let centroid = affected.iter().map(|&v| topo.verts[v]).sum::<Vec3>()
            / affected.len() as f32;
        self.grab = Some(Grab {
            kind,
            originals: affected.iter().map(|&v| (v, topo.verts[v])).collect(),
            start_mouse: self.last_mouse,
            cur_mouse: self.last_mouse,
            constraint: None,
            pivot_world: local_to_world(&world, centroid),
            status: String::new(),
        });
    }

    fn apply_grab(
        &mut self,
        scene: &mut Scene,
        camera: &BlenderCamera,
        viewport: Viewport,
        unit: Unit,
    ) {
        let Some(id) = self.active else { return };
        let Some(grab) = &mut self.grab else { return };

        let world = scene.world_transform(id);
        let element = match self.selected {
            Some(Element::Vertex(_)) => "vertex",
            Some(Element::Edge(..)) => "edge",
            Some(Element::Face(_)) => "face",
            None => "?",
        };
        let constraint_tag = match grab.constraint {
            Some(a) => format!("  along {}", ["X", "Y", "Z"][a]),
            None => String::new(),
        };
        let pivot_cg = three_d::vec3(grab.pivot_world.x, grab.pivot_world.y, grab.pivot_world.z);

        let targets: Vec<(usize, Vec3)> = match grab.kind {
            TransformKind::Move => {
                let (right, up, _) = camera.screen_basis();
                let (right, up) = (gv(right), gv(up));
                let wpp = camera.world_per_pixel_at(viewport, pivot_cg);
                let dx = grab.cur_mouse.0 - grab.start_mouse.0;
                let dy = grab.cur_mouse.1 - grab.start_mouse.1;
                let mut delta = right * (dx * wpp) + up * (dy * wpp);
                if let Some(axis) = grab.constraint {
                    let a = [Vec3::X, Vec3::Y, Vec3::Z][axis];
                    delta = a * delta.dot(a);
                }

                let shown = delta * unit.per_meter();
                grab.status = format!(
                    "Move {element}: ({:.p$}, {:.p$}, {:.p$}) {}{constraint_tag}   |   \
                     LMB/Enter confirm · RMB/Esc cancel",
                    shown.x,
                    shown.y,
                    shown.z,
                    unit.suffix(),
                    p = unit.decimals(),
                );

                // world delta -> local delta (mesh positions are local)
                let local_delta = world_to_local(&world, world.location + delta)
                    - world_to_local(&world, world.location);
                grab.originals.iter().map(|&(w, p)| (w, p + local_delta)).collect()
            }
            TransformKind::Rotate => {
                let pivot_screen = camera.world_to_screen(viewport, pivot_cg);
                let a0 = (grab.start_mouse.1 - pivot_screen.1)
                    .atan2(grab.start_mouse.0 - pivot_screen.0);
                let a1 = (grab.cur_mouse.1 - pivot_screen.1)
                    .atan2(grab.cur_mouse.0 - pivot_screen.0);
                // rotation axis: view axis (toward the viewer) or a world axis
                let (_, _, forward) = camera.screen_basis();
                let view_axis = -gv(forward);
                let (axis, sign) = match grab.constraint {
                    None => (view_axis, 1.0),
                    Some(i) => {
                        let axis = [Vec3::X, Vec3::Y, Vec3::Z][i];
                        (axis, if axis.dot(view_axis) >= 0.0 { 1.0 } else { -1.0 })
                    }
                };
                let angle = sign * (a1 - a0);
                grab.status = format!(
                    "Rotate {element}: {:.1}°{constraint_tag}   |   \
                     LMB/Enter confirm · RMB/Esc cancel",
                    angle.to_degrees(),
                );
                let rotation =
                    modeler_core::glam::Quat::from_axis_angle(axis.normalize_or_zero(), angle);
                rotate_positions(&world, &grab.originals, grab.pivot_world, rotation)
            }
            TransformKind::Scale => {
                let pivot_screen = camera.world_to_screen(viewport, pivot_cg);
                let d0 = ((grab.start_mouse.0 - pivot_screen.0).powi(2)
                    + (grab.start_mouse.1 - pivot_screen.1).powi(2))
                .sqrt()
                .max(1.0);
                let d1 = ((grab.cur_mouse.0 - pivot_screen.0).powi(2)
                    + (grab.cur_mouse.1 - pivot_screen.1).powi(2))
                .sqrt();
                let factor = d1 / d0;
                let factors = match grab.constraint {
                    None => Vec3::splat(factor),
                    Some(i) => {
                        let mut f = Vec3::ONE;
                        f[i] = factor;
                        f
                    }
                };
                grab.status = format!(
                    "Scale {element}: {factor:.3}{constraint_tag}   |   \
                     LMB/Enter confirm · RMB/Esc cancel",
                );
                scale_positions(&world, &grab.originals, grab.pivot_world, factors)
            }
        };

        self.write_positions(scene, &targets);
    }

    /// Write absolute local positions for welded vertices, updating the
    /// topology copy and the object's mesh (all duplicated mesh vertices
    /// follow).
    fn write_positions(&mut self, scene: &mut Scene, positions: &[(usize, Vec3)]) {
        let Some(id) = self.active else { return };
        let Some(topo) = &mut self.topo else { return };
        let Some(object) = scene.object_mut(id) else { return };
        let Some(mesh) = &mut object.edited_mesh else { return };

        for &(weld, new_pos) in positions {
            topo.verts[weld] = new_pos;
            for (i, &w) in topo.weld_of.iter().enumerate() {
                if w == weld {
                    mesh.positions[i] = new_pos;
                }
            }
        }
        mesh.recompute_normals();
        object.mesh_revision += 1;
        self.topo_revision = object.mesh_revision;
    }

    fn cancel_grab_inner(&mut self, scene: &mut Scene, grab: Grab) {
        self.write_positions(scene, &grab.originals);
    }

    // --- overlay data --------------------------------------------------------

    /// World-space geometry for the viewport overlay.
    pub fn overlay(&self, scene: &Scene) -> Option<EditOverlay> {
        let object = self.active.and_then(|id| scene.object(id))?;
        let topo = self.topo.as_ref()?;
        let world = scene.world_transform(object.id);
        let vert = |v: usize| local_to_world(&world, topo.verts[v]);

        let edges: Vec<(Vec3, Vec3)> =
            topo.edges.iter().map(|&(a, b)| (vert(a), vert(b))).collect();
        let verts: Vec<Vec3> = match self.mode {
            SelectMode::Vertex => topo.verts.iter().enumerate().map(|(i, _)| vert(i)).collect(),
            _ => Vec::new(),
        };
        let selected = self.selected.map(|element| match element {
            Element::Vertex(v) => SelectedShape::Point(vert(v)),
            Element::Edge(a, b) => SelectedShape::Line(vert(a), vert(b)),
            Element::Face(f) => {
                let group = &topo.faces[f];
                SelectedShape::Polygon {
                    tris: group
                        .tris
                        .iter()
                        .map(|&ti| {
                            let t = topo.tris[ti];
                            [vert(t[0]), vert(t[1]), vert(t[2])]
                        })
                        .collect(),
                    outline: group.outline.iter().map(|&(a, b)| (vert(a), vert(b))).collect(),
                }
            }
        });
        Some(EditOverlay { edges, verts, selected })
    }
}

/// What the overlay should draw, world space.
pub struct EditOverlay {
    pub edges: Vec<(Vec3, Vec3)>,
    pub verts: Vec<Vec3>,
    pub selected: Option<SelectedShape>,
}

pub enum SelectedShape {
    Point(Vec3),
    Line(Vec3, Vec3),
    Polygon { tris: Vec<[Vec3; 3]>, outline: Vec<(Vec3, Vec3)> },
}

fn point_segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (px, py) = p;
    let (ax, ay) = a;
    let (bx, by) = b;
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 < 1e-9 {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (ax + t * dx, ay + t * dy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// Möller–Trumbore; returns t along the ray.
fn ray_triangle(origin: Vec3, dir: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let e1 = b - a;
    let e2 = c - a;
    let p = dir.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-9 {
        return None;
    }
    let inv = 1.0 / det;
    let s = origin - a;
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = dir.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(q) * inv;
    (t > 1e-5).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::Primitive;

    #[test]
    fn cube_topology_welds_to_blender_counts() {
        let mesh = Primitive::Cube { size: 2.0 }.generate(false); // flat: 24 verts
        let topo = build_topology(&mesh);
        assert_eq!(topo.verts.len(), 8, "welded corners");
        assert_eq!(topo.faces.len(), 6, "coplanar quads, not 12 triangles");
        assert_eq!(topo.edges.len(), 12, "sharp edges, no diagonals");
        for face in &topo.faces {
            assert_eq!(face.verts.len(), 4);
            assert_eq!(face.outline.len(), 4);
        }
    }

    #[test]
    fn moving_a_welded_vertex_moves_all_duplicates() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mesh = scene.object(id).unwrap().render_mesh();
        scene.object_mut(id).unwrap().edited_mesh = Some(mesh);

        let mut edit = EditMode::new();
        edit.active = Some(id);
        edit.rebuild_topology(&scene);

        // pick the corner at (1,1,1) and move it +0.5 in z
        let topo = edit.topo.as_ref().unwrap();
        let corner = topo
            .verts
            .iter()
            .position(|v| (*v - Vec3::ONE).length() < 1e-4)
            .expect("corner");
        let targets = vec![(corner, Vec3::ONE + Vec3::new(0.0, 0.0, 0.5))];
        edit.write_positions(&mut scene, &targets);

        let mesh = scene.object(id).unwrap().edited_mesh.as_ref().unwrap();
        let moved: Vec<&Vec3> = mesh
            .positions
            .iter()
            .filter(|p| (**p - Vec3::new(1.0, 1.0, 1.5)).length() < 1e-4)
            .collect();
        assert!(moved.len() >= 3, "every duplicated corner copy must move");
        assert!(
            !mesh.positions.iter().any(|p| (*p - Vec3::ONE).length() < 1e-4),
            "no copy may remain at the old position"
        );
        // normals were recomputed and stay unit length
        assert!(mesh.normals.iter().all(|n| (n.length() - 1.0).abs() < 1e-3));
        // revision bumped so caches resync
        assert_eq!(scene.object(id).unwrap().mesh_revision, 1);
    }

    #[test]
    fn edit_mode_exits_when_the_scene_is_replaced() {
        let mut scene = Scene::default_scene();
        let id = scene.objects()[0].id;
        let mut edit = EditMode::new();
        edit.enter(id, &scene);
        assert!(edit.active());

        // File ▸ New / control new_scene: fresh document, ids restart — the
        // new cube reuses the SAME id, but edit mode must not rebind to it
        scene = Scene::default_scene();
        assert_eq!(scene.objects()[0].id, id);
        edit.sync(&mut scene);
        assert!(!edit.active());
    }

    #[test]
    fn face_scales_toward_its_centroid() {
        // top face of a unit-ish cube: corners (±1, ±1, 1), centroid (0,0,1)
        let world = Transform::default();
        let originals: Vec<(usize, Vec3)> = [
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(-1.0, 1.0, 1.0),
            Vec3::new(-1.0, -1.0, 1.0),
            Vec3::new(1.0, -1.0, 1.0),
        ]
        .iter()
        .copied()
        .enumerate()
        .collect();
        let pivot = Vec3::new(0.0, 0.0, 1.0);

        // uniform 0.5: corners move halfway to the centroid, z stays
        let scaled = scale_positions(&world, &originals, pivot, Vec3::splat(0.5));
        assert!((scaled[0].1 - Vec3::new(0.5, 0.5, 1.0)).length() < 1e-5);
        assert!((scaled[2].1 - Vec3::new(-0.5, -0.5, 1.0)).length() < 1e-5);

        // X-constrained: only x shrinks
        let scaled = scale_positions(&world, &originals, pivot, Vec3::new(0.5, 1.0, 1.0));
        assert!((scaled[0].1 - Vec3::new(0.5, 1.0, 1.0)).length() < 1e-5);

        // a moved & scaled OBJECT still scales exactly around the world pivot
        let mut moved = Transform::default();
        moved.location = Vec3::new(10.0, 0.0, 0.0);
        moved.scale = Vec3::new(2.0, 1.0, 1.0);
        let pivot_world = local_to_world(&moved, pivot);
        let scaled = scale_positions(&moved, &originals, pivot_world, Vec3::splat(0.5));
        // world corner (12,1,1) -> (11,0.5,1) -> local (0.5, 0.5, 1)
        assert!((scaled[0].1 - Vec3::new(0.5, 0.5, 1.0)).length() < 1e-5, "{:?}", scaled[0].1);
    }

    #[test]
    fn face_rotates_around_its_centroid() {
        let world = Transform::default();
        let originals: Vec<(usize, Vec3)> = [
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(-1.0, 1.0, 1.0),
        ]
        .iter()
        .copied()
        .enumerate()
        .collect();
        let pivot = Vec3::new(0.0, 0.0, 1.0);

        // 90° about Z through the centroid: (1,1,1) -> (-1,1,1)
        let rotation = modeler_core::glam::Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let rotated = rotate_positions(&world, &originals, pivot, rotation);
        assert!((rotated[0].1 - Vec3::new(-1.0, 1.0, 1.0)).length() < 1e-5, "{:?}", rotated[0].1);
        assert!((rotated[1].1 - Vec3::new(-1.0, -1.0, 1.0)).length() < 1e-5);

        // the centroid itself is a fixed point
        let fixed = rotate_positions(&world, &[(0, pivot)], pivot, rotation);
        assert!((fixed[0].1 - pivot).length() < 1e-6);
    }

    #[test]
    fn rotate_and_scale_require_more_than_one_vertex() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mut edit = EditMode::new();
        edit.active = Some(id);
        edit.rebuild_topology(&scene);

        // single vertex: R and S refuse to start, G works
        edit.selected = Some(Element::Vertex(0));
        assert!(edit.text_input("r", &mut scene));
        assert!(edit.grab.is_none());
        assert!(edit.text_input("s", &mut scene));
        assert!(edit.grab.is_none());

        // an edge starts a rotate (and bakes the primitive into a mesh)
        let (a, b) = edit.topo.as_ref().unwrap().edges[0];
        edit.selected = Some(Element::Edge(a, b));
        assert!(edit.text_input("r", &mut scene));
        assert!(edit.grab.is_some());
        assert_eq!(edit.grab.as_ref().unwrap().kind, TransformKind::Rotate);
        assert!(scene.object(id).unwrap().edited_mesh.is_some());
        edit.grab = None;

        // a face starts a scale
        edit.selected = Some(Element::Face(0));
        assert!(edit.text_input("s", &mut scene));
        assert_eq!(edit.grab.as_ref().unwrap().kind, TransformKind::Scale);
    }

    #[test]
    fn selected_element_becomes_pivot_or_anchor() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mut edit = EditMode::new();
        edit.active = Some(id);
        edit.rebuild_topology(&scene);

        // vertex: the corner itself
        let corner = edit
            .topo
            .as_ref()
            .unwrap()
            .verts
            .iter()
            .position(|v| (*v - Vec3::ONE).length() < 1e-4)
            .expect("corner");
        edit.selected = Some(Element::Vertex(corner));
        assert_eq!(edit.selected_point(), Some(Vec3::ONE));

        // P / A set the object's pivot / anchor (typed-character path)
        assert!(edit.text_input("p", &mut scene));
        assert_eq!(scene.object(id).unwrap().pivot, Vec3::ONE);
        assert!(edit.text_input("A", &mut scene));
        assert_eq!(scene.object(id).unwrap().anchor, Vec3::ONE);

        // edge: midpoint; face: centroid
        let (a, b) = edit.topo.as_ref().unwrap().edges[0];
        let expect = {
            let topo = edit.topo.as_ref().unwrap();
            0.5 * (topo.verts[a] + topo.verts[b])
        };
        edit.selected = Some(Element::Edge(a, b));
        assert_eq!(edit.selected_point(), Some(expect));

        let expect = {
            let topo = edit.topo.as_ref().unwrap();
            let group = &topo.faces[0];
            group.verts.iter().map(|&v| topo.verts[v]).sum::<Vec3>()
                / group.verts.len() as f32
        };
        edit.selected = Some(Element::Face(0));
        assert_eq!(edit.selected_point(), Some(expect));
        assert!(edit.text_input("P", &mut scene));
        assert!((scene.object(id).unwrap().pivot - expect).length() < 1e-6);

        // no selection -> no point, P is still consumed but changes nothing
        edit.selected = None;
        assert_eq!(edit.selected_point(), None);
        let before = scene.object(id).unwrap().pivot;
        assert!(edit.text_input("p", &mut scene));
        assert_eq!(scene.object(id).unwrap().pivot, before);
    }

    #[test]
    fn ray_triangle_hits_and_misses() {
        let a = Vec3::new(-1.0, -1.0, 0.0);
        let b = Vec3::new(1.0, -1.0, 0.0);
        let c = Vec3::new(0.0, 1.0, 0.0);
        let hit = ray_triangle(Vec3::new(0.0, 0.0, 5.0), Vec3::new(0.0, 0.0, -1.0), a, b, c);
        assert!((hit.unwrap() - 5.0).abs() < 1e-5);
        let miss = ray_triangle(Vec3::new(5.0, 5.0, 5.0), Vec3::new(0.0, 0.0, -1.0), a, b, c);
        assert!(miss.is_none());
    }

    #[test]
    fn sphere_face_groups_follow_smooth_surface() {
        // a smooth uv-sphere has no coplanar neighbors: every quad/tri is its
        // own face and every edge is sharp — the tool still works, it just
        // shows the true tessellation
        let mesh = Primitive::UvSphere { segments: 8, rings: 4, radius: 1.0 }.generate(false);
        let topo = build_topology(&mesh);
        assert!(topo.faces.len() > 8);
        assert!(!topo.edges.is_empty());
    }
}
