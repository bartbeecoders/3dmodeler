//! Object-level operations: delete via X (with Blender's confirm popup) or
//! the Delete key (immediate).

use crate::selection::Selection;
use modeler_core::glam::{Vec2, Vec3};
use modeler_core::{ObjectId, Primitive, Scene, Transform};
use three_d::egui;
use three_d::{Event, Key, Viewport};

pub struct DeleteTool {
    confirm_open: bool,
    position: egui::Pos2,
    last_mouse: egui::Pos2,
}

/// Convert a three-d event position (physical px, bottom-left origin) to
/// egui logical coordinates (top-left origin).
pub fn event_pos_to_egui(
    x: f32,
    y: f32,
    viewport: Viewport,
    device_pixel_ratio: f32,
) -> egui::Pos2 {
    egui::Pos2::new(
        x / device_pixel_ratio,
        (viewport.height as f32 - y) / device_pixel_ratio,
    )
}

impl DeleteTool {
    pub fn new() -> Self {
        Self {
            confirm_open: false,
            position: egui::Pos2::new(200.0, 200.0),
            last_mouse: egui::Pos2::new(200.0, 200.0),
        }
    }

    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        viewport: Viewport,
        device_pixel_ratio: f32,
        egui_owns_keyboard: bool,
        scene: &mut Scene,
        selection: &mut Selection,
    ) {
        for event in events.iter_mut() {
            match event {
                Event::MouseMotion { position, .. } => {
                    self.last_mouse =
                        event_pos_to_egui(position.x, position.y, viewport, device_pixel_ratio);
                }
                // Blender: X opens a small "Delete" confirmation at the cursor
                Event::Text(text) if text == "x" && !egui_owns_keyboard => {
                    if !selection.is_empty() {
                        self.confirm_open = true;
                        self.position = self.last_mouse;
                        text.clear();
                    }
                }
                // Delete key removes immediately
                Event::KeyPress {
                    kind: Key::Delete,
                    handled,
                    ..
                } if !*handled => {
                    delete_selected(scene, selection);
                    *handled = true;
                }
                Event::KeyPress {
                    kind: Key::Escape,
                    handled,
                    ..
                } if !*handled && self.confirm_open => {
                    self.confirm_open = false;
                    *handled = true;
                }
                Event::MousePress { handled, .. } if self.confirm_open => {
                    // click anywhere else closes the popup; the popup's own
                    // button click is consumed by egui before we see it
                    if !*handled {
                        self.confirm_open = false;
                    }
                }
                _ => {}
            }
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, scene: &mut Scene, selection: &mut Selection) {
        if !self.confirm_open {
            return;
        }
        // the selection can empty out under the popup (e.g. Delete key)
        if selection.is_empty() {
            self.confirm_open = false;
            return;
        }
        egui::Area::new(egui::Id::new("delete-confirm"))
            .fixed_pos(self.position)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(100.0);
                    let count = selection.selected().len();
                    let label = if count == 1 {
                        "Delete?".to_string()
                    } else {
                        format!("Delete {count} objects?")
                    };
                    ui.label(egui::RichText::new(label).strong().size(12.0));
                    ui.separator();
                    if ui.button("Delete").clicked() {
                        delete_selected(scene, selection);
                        self.confirm_open = false;
                    }
                });
            });
    }
}

pub fn delete_selected(scene: &mut Scene, selection: &mut Selection) {
    for id in selection.selected().to_vec() {
        scene.remove_object(id);
    }
    selection.retain_existing(|id| scene.object(id).is_some());
}

/// Place the selection on the ground (Object menu; End drops onto supports
/// via physics instead): each selection root (a selected object whose parent
/// is not selected) is moved vertically so the lowest point of its whole
/// subtree sits at z = 0 — a grouped assembly lands as one piece, keeping
/// its internal offsets.
pub fn place_on_ground(scene: &mut Scene, selection: &Selection) {
    let selected = selection.selected().to_vec();
    let roots: Vec<_> = selected
        .iter()
        .copied()
        .filter(|&id| {
            scene
                .object(id)
                .is_some_and(|o| o.parent.map_or(true, |p| !selected.contains(&p)))
        })
        .collect();
    for root in roots {
        let lowest = scene
            .subtree(root)
            .iter()
            .map(|&member| scene.lowest_point_z(member))
            .fold(f32::INFINITY, f32::min);
        if !lowest.is_finite() {
            continue;
        }
        let mut world = scene.world_transform(root);
        world.location.z -= lowest;
        scene.set_world_transform(root, world);
    }
}

// --- apply scale (Blender's Ctrl+A ▸ Scale) -----------------------------------

fn approx(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-4 * a.abs().max(b.abs()).max(1.0)
}

/// Bake the scale into the primitive's parameters when the shape can
/// represent the result (per-axis for boxes/walls/floors, uniform for round
/// shapes). Returns false when it cannot — the caller bakes the mesh instead.
fn bake_into_primitive(primitive: &mut Primitive, s: Vec3) -> bool {
    match primitive {
        Primitive::Plane { size } if approx(s.x, s.y) => {
            *size *= s.x;
            true
        }
        Primitive::Cube { size } if approx(s.x, s.y) && approx(s.y, s.z) => {
            *size *= s.x;
            true
        }
        Primitive::UvSphere { radius, .. } | Primitive::IcoSphere { radius, .. }
            if approx(s.x, s.y) && approx(s.y, s.z) =>
        {
            *radius *= s.x;
            true
        }
        Primitive::Cylinder { radius, depth, .. } if approx(s.x, s.y) => {
            *radius *= s.x;
            *depth *= s.z;
            true
        }
        Primitive::Cone { radius_bottom, radius_top, depth, .. } if approx(s.x, s.y) => {
            *radius_bottom *= s.x;
            *radius_top *= s.x;
            *depth *= s.z;
            true
        }
        Primitive::Torus { major_radius, minor_radius, .. }
            if approx(s.x, s.y) && approx(s.y, s.z) =>
        {
            *major_radius *= s.x;
            *minor_radius *= s.x;
            true
        }
        Primitive::Wall { length, height, thickness } => {
            *length *= s.x;
            *thickness *= s.y;
            *height *= s.z;
            true
        }
        Primitive::Floor { width, depth, thickness } => {
            *width *= s.x;
            *depth *= s.y;
            *thickness *= s.z;
            true
        }
        // an empty is a marker: fold the dominant factor into its draw size
        Primitive::Empty { size } => {
            *size *= s.abs().max_element();
            true
        }
        _ => false,
    }
}

/// Blender's Object ▸ Apply ▸ Scale: bake each selected object's scale into
/// its geometry and reset the transform scale to 1. Parametric primitives
/// absorb the scale into their parameters where the shape can represent it;
/// otherwise (non-uniform scale on a round shape, or an already-edited mesh)
/// the mesh itself is baked, like Blender writing into mesh data. Pivot,
/// anchor, wall cutouts and floor outlines scale along, and direct children
/// keep their world placement.
pub fn apply_scale(scene: &mut Scene, selection: &Selection) -> String {
    let mut applied = 0usize;
    let mut baked_meshes = 0usize;
    let mut skipped_lights = 0usize;

    for id in selection.selected().to_vec() {
        let Some(object) = scene.object(id) else { continue };
        let s = object.transform.scale;
        if (s - Vec3::ONE).abs().max_element() < 1e-6 {
            continue;
        }
        if matches!(object.primitive, Primitive::Light { .. }) {
            skipped_lights += 1; // a light's gizmo size is intrinsic
            continue;
        }
        let child_ids: Vec<ObjectId> = scene
            .objects()
            .iter()
            .filter(|o| o.parent == Some(id))
            .map(|o| o.id)
            .collect();

        let Some(object) = scene.object_mut(id) else { continue };
        let scale_mesh = |mesh: &mut modeler_core::MeshData| {
            for p in &mut mesh.positions {
                *p *= s;
            }
            for n in &mut mesh.normals {
                *n = (*n / s).normalize_or_zero();
            }
        };
        if let Some(mesh) = &mut object.edited_mesh {
            scale_mesh(mesh);
        } else if bake_into_primitive(&mut object.primitive, s) {
            // parametric bake: openings and outlines live in local meters
            for cutout in &mut object.cutouts {
                cutout.offset *= s.x;
                cutout.width *= s.x;
                cutout.bottom *= s.z;
                cutout.height *= s.z;
            }
            for point in &mut object.floor_outline {
                point.x *= s.x;
                point.y *= s.y;
            }
        } else {
            let mut mesh = object.render_mesh();
            scale_mesh(&mut mesh);
            object.edited_mesh = Some(mesh);
            baked_meshes += 1;
        }
        // pivot and anchor are local-space points
        object.pivot *= s;
        object.anchor *= s;
        object.transform.scale = Vec3::ONE;
        object.mesh_revision += 1;

        // children keep their world placement: fold the parent's old scale
        // into their local transforms (the same SRT approximation compose
        // uses for non-uniform scales)
        for child in child_ids {
            if let Some(child) = scene.object_mut(child) {
                child.transform.location *= s;
                child.transform.scale *= s;
            }
        }
        applied += 1;
    }

    let mut message = match applied {
        0 => "nothing to apply: selection has no scale".to_string(),
        n => format!("applied scale to {n} object{}", if n == 1 { "" } else { "s" }),
    };
    if baked_meshes > 0 {
        message += &format!(" ({baked_meshes} baked into mesh data)");
    }
    if skipped_lights > 0 {
        message += &format!(" — {skipped_lights} light(s) skipped");
    }
    message
}

// --- mesh booleans (CSG) -------------------------------------------------------

/// Add a boolean MODIFIER to the ACTIVE object for each other selected
/// object (Object menu): the viewport previews the result live — the tools
/// are hidden but stay in the scene and can still be moved — and nothing
/// is baked until the user applies the modifier from the sidebar. Returns
/// a status-bar message.
pub fn boolean_selected(
    scene: &mut Scene,
    selection: &mut Selection,
    op: modeler_core::BooleanOp,
) -> String {
    let Some(target) = selection.active() else {
        return "boolean needs an active object to receive the result".to_string();
    };
    let tools: Vec<ObjectId> = selection
        .selected()
        .iter()
        .copied()
        .filter(|&id| id != target)
        .collect();
    if tools.is_empty() {
        return "select at least two objects — the active one receives the result"
            .to_string();
    }
    match crate::modifiers::add_boolean(scene, target, &tools, op) {
        Ok(message) => {
            selection.set(vec![target], Some(target));
            message
        }
        Err(message) => message,
    }
}

/// The id-list core of [`boolean_selected`]; also driven directly by the
/// control API (`boolean_objects` command). The tool meshes are brought
/// into the target's local frame, combined one by one, and the result is
/// stored as the target's edited mesh; the tool objects are removed (their
/// children survive, unparented in place).
pub fn boolean_apply(
    scene: &mut Scene,
    target: ObjectId,
    tools: &[ObjectId],
    op: modeler_core::BooleanOp,
) -> Result<String, String> {
    let no_volume = |o: &modeler_core::Object| {
        o.primitive.is_light() || matches!(o.primitive, Primitive::Empty { .. })
    };
    let target_object = scene.object(target).ok_or("no such target object")?;
    if no_volume(target_object) {
        return Err(format!(
            "'{}' is a light/empty — it has no volume to combine",
            target_object.name
        ));
    }
    let mut tool_ids: Vec<ObjectId> = Vec::new();
    for &tool in tools {
        if tool == target {
            return Err("the target cannot be one of the tools".to_string());
        }
        let tool_object = scene
            .object(tool)
            .ok_or_else(|| format!("no tool object with id {}", tool.0))?;
        if no_volume(tool_object) {
            return Err(format!(
                "'{}' is a light/empty — it has no volume to combine",
                tool_object.name
            ));
        }
        if !tool_ids.contains(&tool) {
            tool_ids.push(tool);
        }
    }
    if tool_ids.is_empty() {
        return Err("no tool objects given".to_string());
    }

    let target_world = scene.world_transform(target);
    let target_object = scene.object(target).expect("checked above");
    let target_name = target_object.name.clone();
    let mut result = target_object.render_mesh();
    for &tool in &tool_ids {
        let tool_object = scene.object(tool).expect("checked above");
        let tool_mesh = modeler_core::mesh_to_frame(
            &tool_object.render_mesh(),
            &scene.world_transform(tool),
            &target_world,
        );
        result = modeler_core::mesh_boolean(&result, &tool_mesh, op);
    }
    if result.indices.is_empty() {
        return Err(
            "boolean result is empty (nothing would remain) — not applied".to_string()
        );
    }

    for &tool in &tool_ids {
        scene.remove_object(tool);
    }
    if let Some(object) = scene.object_mut(target) {
        object.edited_mesh = Some(result);
        object.mesh_revision += 1;
    }
    let count = tool_ids.len();
    let plural = if count == 1 { "object" } else { "objects" };
    Ok(match op {
        modeler_core::BooleanOp::Union => {
            format!("merged {count} {plural} into '{target_name}'")
        }
        modeler_core::BooleanOp::Subtract => {
            format!("subtracted {count} {plural} from '{target_name}'")
        }
        modeler_core::BooleanOp::Intersect => {
            format!("intersected '{target_name}' with {count} {plural}")
        }
    })
}

/// Break-into-bricks target count bounds: the UI slider range and the MCP
/// clamp. 5,000 bricks step comfortably at 60 Hz (see
/// Vibecoding/performance-plan.md — the old hard cap of 600 predates the
/// incremental physics mirror and threaded stepping).
pub const MIN_BRICKS: usize = 100;
pub const MAX_BRICKS: usize = 5000;
pub const DEFAULT_BRICKS: usize = 1000;

/// Replace a wall with individual DYNAMIC bricks in a running bond (odd
/// courses start with a half brick), openings respected. The bricks keep the
/// wall's material (with a subtle per-brick shade variation) and density;
/// they collide and can tumble once the simulation plays. `target_bricks`
/// sizes the brick module so the wall yields roughly that many bricks
/// (openings and course rounding change the exact count). Returns the new
/// brick ids — None when the object is not a pristine wall.
pub fn break_wall_into_bricks(
    scene: &mut Scene,
    id: ObjectId,
    target_bricks: usize,
) -> Option<Vec<ObjectId>> {
    let object = scene.object(id)?;
    let Primitive::Wall { length, height, thickness } = object.primitive else {
        return None;
    };
    if object.edited_mesh.is_some() {
        return None;
    }
    let wall = scene.world_transform(id);
    let cutouts = object.cutouts.clone();

    // brick module ≈ 0.42 × 0.21 m, scaled (keeping its aspect) so the wall
    // yields roughly the requested count; bricks never get thinner than 2 cm
    let mut cell_x = 0.42_f32;
    let mut cell_z = 0.21_f32;
    let estimate = (length / cell_x).max(1.0) * (height / cell_z).max(1.0);
    let target = target_bricks.max(1) as f32;
    let scale = (estimate / target).sqrt();
    cell_x = (cell_x * scale).max(0.02);
    cell_z = (cell_z * scale).max(0.02);
    let rows = ((height / cell_z).round().max(1.0)) as usize;
    let cell_z = height / rows as f32;
    const GAP: f32 = 0.006; // mortar joint, keeps stacked bricks collision-free

    // course layout in the wall's local frame (X along the length, Z up)
    let mut layout: Vec<(f32, f32, f32)> = Vec::new(); // (center x, center z, width)
    for row in 0..rows {
        let z0 = row as f32 * cell_z;
        let z1 = z0 + cell_z;
        // openings overlapping this course block their x-range
        let blocked: Vec<(f32, f32)> = cutouts
            .iter()
            .filter(|c| c.bottom < z1 - 1e-3 && c.bottom + c.height > z0 + 1e-3)
            .map(|c| (c.offset, c.offset + c.width))
            .collect();
        let mut x = 0.0_f32;
        let mut half_first = row % 2 == 1;
        while x < length - 1e-3 {
            let step = if half_first { 0.5 * cell_x } else { cell_x };
            half_first = false;
            let end = (x + step).min(length);
            for (s0, s1) in subtract_ranges(x, end, &blocked) {
                if s1 - s0 < 0.22 * cell_x {
                    continue; // skip slivers at opening edges
                }
                layout.push((0.5 * (s0 + s1), 0.5 * (z0 + z1), s1 - s0));
            }
            x = end;
        }
    }
    let bricks: Vec<Transform> = layout
        .into_iter()
        .map(|(cx, cz, w)| Transform {
            location: wall.transform_point(Vec3::new(cx, 0.0, cz)),
            rotation: wall.rotation,
            scale: wall.scale
                * Vec3::new(
                    (w - GAP).max(0.02),
                    (thickness - GAP).max(0.02),
                    (cell_z - GAP).max(0.02),
                ),
        })
        .collect();
    replace_with_bricks(scene, id, bricks)
}

/// Shared tail of the break-into-bricks operations: spawn one dynamic
/// brick per transform (the source material with a subtle deterministic
/// shade variation so the bond reads), file them in a "<name> bricks"
/// folder that stores the original object for Rebuild, and remove the
/// original. Returns None (changing nothing) when `bricks` is empty.
fn replace_with_bricks(
    scene: &mut Scene,
    id: ObjectId,
    bricks: Vec<Transform>,
) -> Option<Vec<ObjectId>> {
    if bricks.is_empty() {
        return None;
    }
    let object = scene.object(id)?;
    let base_name = object.name.clone();
    let material = object.material;
    let density = object.density;

    let mut ids = Vec::with_capacity(bricks.len());
    let folder = scene.add_folder(&format!("{base_name} bricks"));
    for (i, transform) in bricks.into_iter().enumerate() {
        let brick = scene.add_object(Primitive::Cube { size: 1.0 }, transform);
        if let Some(o) = scene.object_mut(brick) {
            o.name = format!("{base_name} brick {}", i + 1);
            o.folder = Some(folder);
            o.dynamic = true;
            o.density = density;
            o.material = material;
            let shade = 0.88 + 0.24 * ((i as f32) * 0.618_034).fract();
            for channel in &mut o.material.base_color {
                *channel = (*channel * shade).clamp(0.0, 1.0);
            }
        }
        ids.push(brick);
    }
    // keep the original object on the folder so it can be rebuilt later
    if let Some(original) = scene.remove_object(id) {
        if let Some(f) = scene.folder_mut(folder) {
            f.source_wall = Some(Box::new(original));
        }
    }
    Some(ids)
}

/// Break ANY solid object into dynamic bricks: pristine walls keep their
/// specialized course layout (running bond around door/window openings);
/// everything else is filled with a running-bond brick grid in its local
/// frame, dropping bricks whose center falls outside the mesh — spheres,
/// cones, tori and shaped floors break into their stepped brick
/// approximation. `target_bricks` sizes the brick module for roughly that
/// many bricks (curved shapes land under it: bounding-box cells outside the
/// mesh are dropped). Lights and empties have no volume: None.
pub fn break_into_bricks(
    scene: &mut Scene,
    id: ObjectId,
    target_bricks: usize,
) -> Option<Vec<ObjectId>> {
    let object = scene.object(id)?;
    if matches!(object.primitive, Primitive::Wall { .. }) && object.edited_mesh.is_none() {
        return break_wall_into_bricks(scene, id, target_bricks);
    }
    if object.primitive.is_light() || matches!(object.primitive, Primitive::Empty { .. }) {
        return None;
    }
    let world = scene.world_transform(id);
    let mesh = object.collision_mesh();
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for p in &mesh.positions {
        min = min.min(*p);
        max = max.max(*p);
    }
    if !min.x.is_finite() {
        return None;
    }
    let extent = max - min;

    // brick module as in walls, scaled (keeping its aspect) toward the
    // requested count; bricks never get thinner than 2 cm
    let mut cell = Vec3::new(0.42, 0.21, 0.21);
    let estimate = (extent.x / cell.x).max(1.0)
        * (extent.y / cell.y).max(1.0)
        * (extent.z / cell.z).max(1.0);
    let target = target_bricks.max(1) as f32;
    let scale = (estimate / target).cbrt();
    cell = Vec3::new(
        (cell.x * scale).max(0.02),
        (cell.y * scale).max(0.02),
        (cell.z * scale).max(0.02),
    );
    const GAP: f32 = 0.006; // mortar joint, keeps stacked bricks collision-free
    // fit whole cells into the bounding box
    let cells = |e: f32, c: f32| ((e / c).round().max(1.0)) as i32;
    let (nx, ny, nz) = (cells(extent.x, cell.x), cells(extent.y, cell.y), cells(extent.z, cell.z));
    let cell = Vec3::new(extent.x / nx as f32, extent.y / ny as f32, extent.z / nz as f32);

    let mut bricks: Vec<Transform> = Vec::new();
    for k in 0..nz {
        let z = min.z + (k as f32 + 0.5) * cell.z;
        // running bond: odd layers shift half a brick along X, boundary
        // bricks clamped to the box (half bricks, like wall courses)
        let (x_first, x_count) = if k % 2 == 1 { (-0.5 * cell.x, nx + 1) } else { (0.0, nx) };
        for j in 0..ny {
            let y = min.y + (j as f32 + 0.5) * cell.y;
            for i in 0..x_count {
                let x0 = (min.x + x_first + i as f32 * cell.x).max(min.x);
                let x1 = (min.x + x_first + (i + 1) as f32 * cell.x).min(max.x);
                if x1 - x0 < 0.22 * cell.x {
                    continue; // skip slivers at the box edges
                }
                let center = Vec3::new(0.5 * (x0 + x1), y, z);
                if !point_in_mesh(&mesh, center) {
                    continue;
                }
                bricks.push(Transform {
                    location: world.transform_point(center),
                    rotation: world.rotation,
                    scale: world.scale
                        * Vec3::new(
                            (x1 - x0 - GAP).max(0.02),
                            (cell.y - GAP).max(0.02),
                            (cell.z - GAP).max(0.02),
                        ),
                });
            }
        }
    }
    replace_with_bricks(scene, id, bricks)
}

/// Point-in-mesh via ray-crossing parity — meaningful for closed meshes
/// (every primitive except lights/empties generates one; open edited
/// meshes degrade gracefully to "outside"). The tilted ray direction
/// dodges exact edge/vertex grazes on axis-aligned geometry.
fn point_in_mesh(mesh: &modeler_core::MeshData, p: Vec3) -> bool {
    let dir = Vec3::new(0.9830, 0.1359, 0.1236);
    let mut crossings = 0u32;
    for tri in mesh.indices.chunks_exact(3) {
        let a = mesh.positions[tri[0] as usize];
        let b = mesh.positions[tri[1] as usize];
        let c = mesh.positions[tri[2] as usize];
        // Möller–Trumbore
        let e1 = b - a;
        let e2 = c - a;
        let pv = dir.cross(e2);
        let det = e1.dot(pv);
        if det.abs() < 1e-9 {
            continue;
        }
        let inv = 1.0 / det;
        let tv = p - a;
        let u = tv.dot(pv) * inv;
        if !(0.0..=1.0).contains(&u) {
            continue;
        }
        let qv = tv.cross(e1);
        let v = dir.dot(qv) * inv;
        if v < 0.0 || u + v > 1.0 {
            continue;
        }
        if e2.dot(qv) * inv > 1e-6 {
            crossings += 1;
        }
    }
    crossings % 2 == 1
}

/// The bricks folder a member of which can rebuild the wall, if any.
pub fn rebuildable_folder(scene: &Scene, id: ObjectId) -> Option<u64> {
    let folder = scene.object(id)?.folder?;
    scene
        .folder(folder)
        .is_some_and(|f| f.source_wall.is_some())
        .then_some(folder)
}

/// Inverse of `break_wall_into_bricks`: remove every object filed in the
/// bricks folder (wherever the simulation scattered them), delete the folder
/// and restore the stored wall at its original place. Returns the wall's
/// new id.
pub fn rebuild_wall_from_folder(scene: &mut Scene, folder_id: u64) -> Option<ObjectId> {
    let wall = scene.folder(folder_id)?.source_wall.clone()?;
    let members: Vec<ObjectId> = scene
        .objects()
        .iter()
        .filter(|o| o.folder == Some(folder_id))
        .map(|o| o.id)
        .collect();
    for id in members {
        scene.remove_object(id);
    }
    scene.remove_folder(folder_id);
    let mut wall = *wall;
    wall.folder = None;
    // the original parent may have been deleted in the meantime
    wall.parent = wall.parent.filter(|&p| scene.object(p).is_some());
    Some(scene.insert_object(wall))
}

/// Floor slab thickness (m) for Add ▸ Floor.
const FLOOR_THICKNESS: f32 = 0.1;
/// Fallback footprint (m) when there are no walls to size the floor from.
const FLOOR_DEFAULT_SIZE: f32 = 4.0;

/// Add ▸ Floor: a slab standing on z = 0 under the selected walls (walls
/// inside selected groups count too; with no wall selected, ALL walls).
/// When the walls chain into a closed loop the floor follows their shape
/// (centerline polygon); otherwise it is their bounding rectangle; with no
/// walls at all it falls back to a `FLOOR_DEFAULT_SIZE` square at the
/// origin. Selects the new floor and returns a status-bar message.
pub fn add_floor(scene: &mut Scene, selection: &mut Selection) -> String {
    let selected: std::collections::HashSet<ObjectId> =
        selection.selected().iter().copied().collect();
    // a wall counts as selected when it or any ancestor is in the selection
    // (clicking a grouped assembly selects its root, not the walls in it)
    let in_selection = |mut id: ObjectId| loop {
        if selected.contains(&id) {
            return true;
        }
        match scene.object(id).and_then(|o| o.parent) {
            Some(parent) => id = parent,
            None => return false,
        }
    };
    let all_walls: Vec<ObjectId> = scene
        .objects()
        .iter()
        .filter(|o| matches!(o.primitive, Primitive::Wall { .. }))
        .map(|o| o.id)
        .collect();
    let mut walls: Vec<ObjectId> =
        all_walls.iter().copied().filter(|&id| in_selection(id)).collect();
    if walls.is_empty() {
        walls = all_walls;
    }
    let (id, message) = add_floor_for_walls(scene, &walls);
    selection.set(vec![id], Some(id));
    message
}

/// The wall-list core of [`add_floor`]; also driven directly by the control
/// API (`add_floor` command). Returns the new floor and a status message.
pub fn add_floor_for_walls(scene: &mut Scene, walls: &[ObjectId]) -> (ObjectId, String) {
    // preferred: the walls close a loop — the floor follows their shape
    if let Some(points) = wall_loop_outline(scene, walls) {
        let mut min = Vec2::splat(f32::INFINITY);
        let mut max = Vec2::splat(f32::NEG_INFINITY);
        for p in &points {
            min = min.min(*p);
            max = max.max(*p);
        }
        let center = 0.5 * (min + max);
        let primitive = Primitive::Floor {
            // informational: dimensions display & bounding radius
            width: (max.x - min.x).max(0.1),
            depth: (max.y - min.y).max(0.1),
            thickness: FLOOR_THICKNESS,
        };
        let location = Vec3::new(center.x, center.y, 0.0);
        let id =
            scene.add_object(primitive, Transform { location, ..Transform::default() });
        if let Some(object) = scene.object_mut(id) {
            object.floor_outline = points.iter().map(|p| *p - center).collect();
            object.mesh_revision += 1;
        }
        return (id, format!("floor added following {} walls", walls.len()));
    }

    // fallback: world-space XY bounds over every wall corner
    // (rotation/scale-safe)
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for &id in walls {
        let Some(object) = scene.object(id) else { continue };
        let Primitive::Wall { length, height, thickness } = object.primitive else {
            continue;
        };
        let world = scene.world_transform(id);
        for x in [0.0, length] {
            for y in [-0.5 * thickness, 0.5 * thickness] {
                for z in [0.0, height] {
                    let p = world.transform_point(Vec3::new(x, y, z));
                    min = min.min(p);
                    max = max.max(p);
                }
            }
        }
    }

    let (primitive, location, message) = if min.x.is_finite() {
        (
            Primitive::Floor {
                width: (max.x - min.x).max(0.1),
                depth: (max.y - min.y).max(0.1),
                thickness: FLOOR_THICKNESS,
            },
            Vec3::new(0.5 * (min.x + max.x), 0.5 * (min.y + max.y), 0.0),
            format!(
                "floor added under {} wall{}",
                walls.len(),
                if walls.len() == 1 { "" } else { "s" }
            ),
        )
    } else {
        (
            Primitive::Floor {
                width: FLOOR_DEFAULT_SIZE,
                depth: FLOOR_DEFAULT_SIZE,
                thickness: FLOOR_THICKNESS,
            },
            Vec3::ZERO,
            "floor added — no walls to size from, using a 4 m square".to_string(),
        )
    };
    let id = scene.add_object(primitive, Transform { location, ..Transform::default() });
    (id, message)
}

/// Default eave overhang (m) for Add ▸ Roof.
pub const ROOF_OVERHANG: f32 = 0.3;
/// Fallback footprint (m) when there are no walls to size the roof from.
const ROOF_DEFAULT_SIZE: f32 = 4.0;

/// Add ▸ Roof: a solid roof lid sitting on top of the selected walls (walls
/// inside selected groups count too). Returns None when the selection holds
/// no walls — the caller starts the draw-the-footprint tool instead.
pub fn add_roof(
    scene: &mut Scene,
    selection: &mut Selection,
    kind: modeler_core::RoofKind,
) -> Option<String> {
    let selected: std::collections::HashSet<ObjectId> =
        selection.selected().iter().copied().collect();
    // a wall counts as selected when it or any ancestor is in the selection
    let in_selection = |mut id: ObjectId| loop {
        if selected.contains(&id) {
            return true;
        }
        match scene.object(id).and_then(|o| o.parent) {
            Some(parent) => id = parent,
            None => return false,
        }
    };
    let walls: Vec<ObjectId> = scene
        .objects()
        .iter()
        .filter(|o| matches!(o.primitive, Primitive::Wall { .. }) && in_selection(o.id))
        .map(|o| o.id)
        .collect();
    if walls.is_empty() {
        return None;
    }
    let (id, message) = add_roof_for_walls(scene, &walls, kind);
    selection.set(vec![id], Some(id));
    Some(message)
}

/// The wall-list core of [`add_roof`]; also driven directly by the control
/// API (`add_roof` command). The roof covers the walls' world-space XY
/// bounds (rotation/scale-safe) and stands on the tallest wall top; the
/// ridge follows the longer side. With no walls it falls back to a
/// `ROOF_DEFAULT_SIZE` square at the origin.
pub fn add_roof_for_walls(
    scene: &mut Scene,
    walls: &[ObjectId],
    kind: modeler_core::RoofKind,
) -> (ObjectId, String) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for &id in walls {
        let Some(object) = scene.object(id) else { continue };
        let Primitive::Wall { length, height, thickness } = object.primitive else {
            continue;
        };
        let world = scene.world_transform(id);
        for x in [0.0, length] {
            for y in [-0.5 * thickness, 0.5 * thickness] {
                for z in [0.0, height] {
                    let p = world.transform_point(Vec3::new(x, y, z));
                    min = min.min(p);
                    max = max.max(p);
                }
            }
        }
    }

    let (width, depth, location, message) = if min.x.is_finite() {
        (
            (max.x - min.x).max(0.1),
            (max.y - min.y).max(0.1),
            Vec3::new(0.5 * (min.x + max.x), 0.5 * (min.y + max.y), max.z),
            format!(
                "{} roof added over {} wall{}",
                kind.label().to_lowercase(),
                walls.len(),
                if walls.len() == 1 { "" } else { "s" }
            ),
        )
    } else {
        (
            ROOF_DEFAULT_SIZE,
            ROOF_DEFAULT_SIZE,
            Vec3::ZERO,
            "roof added — no walls to size from, using a 4 m square".to_string(),
        )
    };
    let primitive = Primitive::Roof {
        kind,
        width,
        depth,
        height: kind.default_height(width.min(depth)),
        overhang: ROOF_OVERHANG,
        ridge_x: width >= depth,
    };
    let id = scene.add_object(primitive, Transform { location, ..Transform::default() });
    (id, message)
}

/// The world-space centerline polygon of the walls, when they chain
/// end-to-end into ONE closed loop (each wall runs from its origin along
/// local +X). Returns None for open runs, branches, disjoint loops or
/// fewer than three walls — callers fall back to the bounding rectangle.
fn wall_loop_outline(scene: &Scene, walls: &[ObjectId]) -> Option<Vec<Vec2>> {
    let mut segments: Vec<(Vec2, Vec2)> = walls
        .iter()
        .filter_map(|&id| {
            let object = scene.object(id)?;
            let Primitive::Wall { length, .. } = object.primitive else { return None };
            let world = scene.world_transform(id);
            let a = world.transform_point(Vec3::ZERO);
            let b = world.transform_point(Vec3::new(length, 0.0, 0.0));
            Some((Vec2::new(a.x, a.y), Vec2::new(b.x, b.y)))
        })
        .collect();
    if segments.len() < 3 {
        return None;
    }
    // corner-match tolerance: hand-drawn rooms rarely close exactly — the
    // wall tool snaps consecutive corners, but the final click is freehand
    // and can miss the start by a couple of decimeters. Scale the tolerance
    // with the shortest wall so distinct corners of tiny structures never
    // get merged.
    let shortest = segments
        .iter()
        .map(|(a, b)| a.distance(*b))
        .fold(f32::INFINITY, f32::min);
    let eps = (0.4 * shortest).clamp(0.05, 0.5);
    let (start, mut end) = segments.swap_remove(0);
    let mut points = vec![start];
    while !segments.is_empty() {
        points.push(end);
        // the next wall touches the current end with either of its endpoints
        let i = segments
            .iter()
            .position(|&(a, b)| a.distance(end) < eps || b.distance(end) < eps)?;
        let (a, b) = segments.swap_remove(i);
        end = if a.distance(end) < eps { b } else { a };
    }
    // all walls used AND the chain returns to its start: a closed loop
    (end.distance(start) < eps && points.len() >= 3).then_some(points)
}

/// The parts of `[x0, x1]` not covered by any blocked range.
fn subtract_ranges(x0: f32, x1: f32, blocked: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut free = vec![(x0, x1)];
    for &(b0, b1) in blocked {
        let mut next = Vec::new();
        for (f0, f1) in free {
            if b1 <= f0 || b0 >= f1 {
                next.push((f0, f1));
                continue;
            }
            if b0 > f0 {
                next.push((f0, b0));
            }
            if b1 < f1 {
                next.push((b1, f1));
            }
        }
        free = next;
    }
    free
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_breaks_into_dynamic_bricks_around_openings() {
        let mut scene = Scene::new();
        let wall = scene.add_object(
            Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 },
            Transform::default(),
        );
        // a 1 m door in the middle: 1.5..2.5 × 0..2.1
        scene.object_mut(wall).unwrap().cutouts.push(modeler_core::WallCutout {
            offset: 1.5,
            width: 1.0,
            bottom: 0.0,
            height: 2.1,
        });

        let bricks = break_wall_into_bricks(&mut scene, wall, 200).unwrap();
        assert!(scene.object(wall).is_none(), "the wall is replaced");
        assert!(bricks.len() > 40, "got {} bricks", bricks.len());

        // the bricks land in their own outliner folder
        let folder = scene
            .folders()
            .iter()
            .find(|f| f.name == "Wall bricks")
            .expect("brick folder created");

        for &id in &bricks {
            let o = scene.object(id).unwrap();
            assert!(o.dynamic, "bricks must simulate");
            assert_eq!(o.folder, Some(folder.id), "bricks are filed in the folder");
            // no brick may reach into the door opening (interior overlap)
            let (cx, cz) = (o.transform.location.x, o.transform.location.z);
            let (hw, hh) = (0.5 * o.transform.scale.x, 0.5 * o.transform.scale.z);
            let outside = cx + hw <= 1.5 + 1e-3
                || cx - hw >= 2.5 - 1e-3
                || cz - hh >= 2.1 - 1e-3;
            assert!(outside, "brick at ({cx}, {cz}) w={hw} h={hh} is inside the door");
        }

        // ...and back: rebuilding restores the wall and clears the bricks
        let brick = bricks[0];
        let folder_id = rebuildable_folder(&scene, brick).expect("bricks are rebuildable");
        let restored = rebuild_wall_from_folder(&mut scene, folder_id).unwrap();
        assert!(scene.folders().is_empty(), "bricks folder removed");
        for &id in &bricks {
            assert!(scene.object(id).is_none(), "brick {id:?} removed");
        }
        let wall = scene.object(restored).unwrap();
        assert_eq!(wall.name, "Wall");
        assert_eq!(
            wall.primitive,
            Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 }
        );
        assert_eq!(wall.cutouts.len(), 1, "the door survives the round trip");
        assert_eq!(scene.objects().len(), 1);
    }

    #[test]
    fn any_solid_object_breaks_into_bricks_and_rebuilds() {
        let mut scene = Scene::new();
        let mut t = Transform::default();
        t.location = Vec3::new(0.0, 0.0, 1.0);
        let sphere = scene.add_object(
            Primitive::UvSphere { segments: 24, rings: 12, radius: 1.0 },
            t,
        );
        scene.object_mut(sphere).unwrap().name = "Ball".to_string();

        let bricks = break_into_bricks(&mut scene, sphere, DEFAULT_BRICKS).expect("sphere breaks");
        assert!(scene.object(sphere).is_none(), "the sphere is replaced");
        // a decent fill, but corner cells of the bounding box are rejected:
        // strictly fewer bricks than the full 2×2×2 grid would hold
        assert!(bricks.len() > 50, "got {} bricks", bricks.len());
        for &id in &bricks {
            let o = scene.object(id).unwrap();
            assert!(o.dynamic);
            // every brick center lies inside the sphere (its bounding ball)
            let d = (o.transform.location - Vec3::new(0.0, 0.0, 1.0)).length();
            assert!(d < 1.0 + 1e-3, "brick center {d} outside the sphere");
        }

        // ...and back: the folder rebuilds the original sphere
        let folder = rebuildable_folder(&scene, bricks[0]).expect("rebuildable");
        let restored = rebuild_wall_from_folder(&mut scene, folder).unwrap();
        let ball = scene.object(restored).unwrap();
        assert_eq!(ball.name, "Ball");
        assert_eq!(
            ball.primitive,
            Primitive::UvSphere { segments: 24, rings: 12, radius: 1.0 }
        );
        assert_eq!(scene.objects().len(), 1);
    }

    #[test]
    fn lights_and_empties_do_not_break() {
        let mut scene = Scene::new();
        let empty = scene.add_object(Primitive::Empty { size: 1.0 }, Transform::default());
        let light =
            scene.add_object(Primitive::light_catalog()[0], Transform::default());
        assert!(break_into_bricks(&mut scene, empty, DEFAULT_BRICKS).is_none());
        assert!(break_into_bricks(&mut scene, light, DEFAULT_BRICKS).is_none());
        assert_eq!(scene.objects().len(), 2, "nothing was consumed");
    }

    #[test]
    fn brick_target_count_scales_the_bond() {
        let make_wall = |scene: &mut Scene| {
            scene.add_object(
                Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 },
                Transform::default(),
            )
        };

        let mut scene_few = Scene::new();
        let wall = make_wall(&mut scene_few);
        let few = break_wall_into_bricks(&mut scene_few, wall, 200).unwrap().len();

        let mut scene_many = Scene::new();
        let wall = make_wall(&mut scene_many);
        let many = break_wall_into_bricks(&mut scene_many, wall, 2000).unwrap().len();

        assert!(few < many, "target must scale the count: {few} !< {many}");
        // the target is approximate (course rounding, half bricks), but the
        // result must land in its ballpark
        assert!((100..=400).contains(&few), "target 200 gave {few}");
        assert!((1200..=3000).contains(&many), "target 2000 gave {many}");
    }

    #[test]
    fn subtract_ranges_cuts_blocked_spans() {
        assert_eq!(subtract_ranges(0.0, 1.0, &[]), vec![(0.0, 1.0)]);
        assert_eq!(
            subtract_ranges(0.0, 1.0, &[(0.4, 0.6)]),
            vec![(0.0, 0.4), (0.6, 1.0)]
        );
        assert_eq!(subtract_ranges(0.0, 1.0, &[(-1.0, 2.0)]), vec![]);
    }

    /// A wall from `a` to `b` on the ground (any direction, 0.2 thick).
    fn wall_between(scene: &mut Scene, a: Vec2, b: Vec2) -> ObjectId {
        let dir = b - a;
        let mut t = Transform::default();
        t.location = Vec3::new(a.x, a.y, 0.0);
        t.rotation = modeler_core::glam::Quat::from_rotation_z(dir.y.atan2(dir.x));
        scene.add_object(
            Primitive::Wall { length: dir.length(), height: 2.5, thickness: 0.2 },
            t,
        )
    }

    #[test]
    fn floor_follows_a_closed_wall_loop() {
        // an L-shaped room: 6 walls, corners (0,0) (4,0) (4,2) (2,2) (2,4) (0,4)
        let corners = [
            Vec2::new(0.0, 0.0),
            Vec2::new(4.0, 0.0),
            Vec2::new(4.0, 2.0),
            Vec2::new(2.0, 2.0),
            Vec2::new(2.0, 4.0),
            Vec2::new(0.0, 4.0),
        ];
        let mut scene = Scene::new();
        let walls: Vec<ObjectId> = (0..corners.len())
            .map(|i| wall_between(&mut scene, corners[i], corners[(i + 1) % corners.len()]))
            .collect();

        let mut selection = Selection::default();
        selection.set(walls, None);
        let message = add_floor(&mut scene, &mut selection);
        assert!(message.contains("following 6 walls"), "{message}");

        let floor = selection.active().expect("floor selected");
        let object = scene.object(floor).unwrap();
        // centered on the outline's bounding box, on the ground
        let center_error = (object.transform.location - Vec3::new(2.0, 2.0, 0.0)).length();
        assert!(center_error < 1e-4, "location {:?}", object.transform.location);
        assert_eq!(object.floor_outline.len(), 6, "{:?}", object.floor_outline);
        // the outline is the corner polygon relative to the center
        for corner in corners {
            let local = corner - Vec2::new(2.0, 2.0);
            assert!(
                object.floor_outline.iter().any(|p| p.distance(local) < 1e-4),
                "corner {local:?} missing from {:?}",
                object.floor_outline
            );
        }
        // the mesh follows the L: 6 corners → 4 cap triangles ×2 + 6 side
        // quads ×2 = 20 triangles; the notch corner (2,2) stays open — no
        // vertex of the top cap lies in the notch quadrant's interior center
        let mesh = object.render_mesh();
        assert_eq!(mesh.indices.len(), 20 * 3);
        let inside_notch = Vec2::new(3.0 - 2.0, 3.0 - 2.0); // world (3,3)
        // sample: the notch center must not be covered by any top triangle
        let covered = mesh.indices.chunks_exact(3).any(|t| {
            let (a, b, c) = (
                mesh.positions[t[0] as usize],
                mesh.positions[t[1] as usize],
                mesh.positions[t[2] as usize],
            );
            if a.z < 0.05 || b.z < 0.05 || c.z < 0.05 {
                return false; // only the top cap
            }
            let (a, b, c) =
                (Vec2::new(a.x, a.y), Vec2::new(b.x, b.y), Vec2::new(c.x, c.y));
            let s = |o: Vec2, p: Vec2, q: Vec2| (p - o).perp_dot(q - o);
            let (d0, d1, d2) = (
                s(a, b, inside_notch),
                s(b, c, inside_notch),
                s(c, a, inside_notch),
            );
            (d0 >= 0.0 && d1 >= 0.0 && d2 >= 0.0) || (d0 <= 0.0 && d1 <= 0.0 && d2 <= 0.0)
        });
        assert!(!covered, "the L notch must not be floored over");
    }

    #[test]
    fn floor_follows_a_hand_drawn_room_with_a_sloppy_closing_corner() {
        // a real hand-drawn room (wall tool): consecutive corners match to
        // the millimeter, but the closing click missed the start by ~0.24 m
        let walls: [(Vec2, f32, f32); 6] = [
            (Vec2::new(-5.123, -1.052), 104.2, 13.253837),
            (Vec2::new(-8.372, 11.798), -23.1, 12.710259),
            (Vec2::new(3.322, 6.818), -91.1, 5.3763566),
            (Vec2::new(3.214, 1.443), 144.8, 5.667274),
            (Vec2::new(-1.418, 4.708), -79.2, 5.4969807),
            (Vec2::new(-0.386, -0.692), -177.9, 4.8899403),
        ];
        let mut scene = Scene::new();
        for (loc, deg, length) in walls {
            let mut t = Transform::default();
            t.location = Vec3::new(loc.x, loc.y, 0.0);
            t.rotation = modeler_core::glam::Quat::from_rotation_z(deg.to_radians());
            scene.add_object(
                Primitive::Wall { length, height: 2.5, thickness: 0.2 },
                t,
            );
        }
        let mut selection = Selection::default();
        // nothing selected: every wall counts
        let message = add_floor(&mut scene, &mut selection);
        assert!(message.contains("following 6 walls"), "{message}");
        let floor = selection.active().unwrap();
        assert_eq!(scene.object(floor).unwrap().floor_outline.len(), 6);
    }

    #[test]
    fn open_walls_fall_back_to_the_bounding_rectangle() {
        let mut scene = Scene::new();
        // three walls of a square: the loop does not close
        let a = wall_between(&mut scene, Vec2::new(0.0, 0.0), Vec2::new(4.0, 0.0));
        let b = wall_between(&mut scene, Vec2::new(4.0, 0.0), Vec2::new(4.0, 4.0));
        let c = wall_between(&mut scene, Vec2::new(4.0, 4.0), Vec2::new(0.0, 4.0));
        let mut selection = Selection::default();
        selection.set(vec![a, b, c], None);
        let message = add_floor(&mut scene, &mut selection);
        assert!(message.contains("under 3 walls"), "{message}");
        let floor = selection.active().unwrap();
        assert!(scene.object(floor).unwrap().floor_outline.is_empty());
    }

    #[test]
    fn floor_encompasses_the_selected_walls() {
        let mut scene = Scene::new();
        // an L of two 4 m walls meeting at (4, 0): one along +X, one along
        // +Y (rotated 90° around Z), both 0.2 thick
        let wall = Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 };
        let a = scene.add_object(wall, Transform::default());
        let mut t = Transform::default();
        t.location = Vec3::new(4.0, 0.0, 0.0);
        t.rotation = modeler_core::glam::Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let b = scene.add_object(wall, t);
        // a decoy far away that is NOT selected
        let mut far = Transform::default();
        far.location = Vec3::new(50.0, 50.0, 0.0);
        scene.add_object(wall, far);

        let mut selection = Selection::default();
        selection.set(vec![a, b], Some(a));
        add_floor(&mut scene, &mut selection);

        // bounds: x ∈ [0, 4.1] (wall B's thickness pokes past x = 4),
        // y ∈ [-0.1, 4.0]
        let floor = selection.active().expect("floor selected");
        let object = scene.object(floor).unwrap();
        let Primitive::Floor { width, depth, thickness } = object.primitive else {
            panic!("expected a floor, got {:?}", object.primitive);
        };
        assert!((width - 4.1).abs() < 1e-4, "width {width}");
        assert!((depth - 4.1).abs() < 1e-4, "depth {depth}");
        assert!(thickness > 0.0);
        let loc = object.transform.location;
        assert!((loc.x - 2.05).abs() < 1e-4, "center x {}", loc.x);
        assert!((loc.y - 1.95).abs() < 1e-4, "center y {}", loc.y);
        assert_eq!(loc.z, 0.0, "the floor stands on the ground");

        // the floor survives a save/load round trip
        let primitive = object.primitive;
        let data = Scene::from_json(&scene.to_json()).expect("scene loads");
        let mut restored = Scene::new();
        restored.restore(&data);
        assert_eq!(restored.object(floor).unwrap().primitive, primitive);
    }

    #[test]
    fn roof_covers_the_selected_walls_and_sits_on_their_tops() {
        use modeler_core::RoofKind;
        let mut scene = Scene::new();
        let wall = Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 };
        let a = scene.add_object(wall, Transform::default());
        // a taller wall along +Y from (4, 0): the roof rides its top edge
        let tall = Primitive::Wall { length: 4.0, height: 3.0, thickness: 0.2 };
        let mut t = Transform::default();
        t.location = Vec3::new(4.0, 0.0, 0.0);
        t.rotation = modeler_core::glam::Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let b = scene.add_object(tall, t);
        // an unselected decoy far away
        let mut far = Transform::default();
        far.location = Vec3::new(50.0, 50.0, 0.0);
        scene.add_object(wall, far);

        let mut selection = Selection::default();
        selection.set(vec![a, b], Some(a));
        let message = add_roof(&mut scene, &mut selection, RoofKind::Gable)
            .expect("walls are selected");
        assert!(message.contains("2 walls"), "{message}");

        let roof = selection.active().expect("roof selected");
        let object = scene.object(roof).unwrap();
        let Primitive::Roof { kind, width, depth, height, overhang, .. } = object.primitive
        else {
            panic!("expected a roof, got {:?}", object.primitive);
        };
        assert_eq!(kind, RoofKind::Gable);
        // same bounds as the floor test: 4.1 × 4.1 around (2.05, 1.95)
        assert!((width - 4.1).abs() < 1e-4, "width {width}");
        assert!((depth - 4.1).abs() < 1e-4, "depth {depth}");
        assert!(height > 0.0 && overhang > 0.0);
        let loc = object.transform.location;
        assert!((loc.x - 2.05).abs() < 1e-4, "center x {}", loc.x);
        assert!((loc.y - 1.95).abs() < 1e-4, "center y {}", loc.y);
        assert!((loc.z - 3.0).abs() < 1e-4, "roof base on the tallest wall top");

        // survives save/load
        let primitive = object.primitive;
        let data = Scene::from_json(&scene.to_json()).expect("scene loads");
        let mut restored = Scene::new();
        restored.restore(&data);
        assert_eq!(restored.object(roof).unwrap().primitive, primitive);

        // nothing selected → the caller must fall back to drawing
        selection.set(Vec::new(), None);
        assert!(add_roof(&mut scene, &mut selection, RoofKind::Hip).is_none());
    }

    #[test]
    fn place_on_ground_moves_selection_roots_as_one() {
        let mut scene = Scene::new();
        let mut at = |location: Vec3| {
            let mut t = Transform::default();
            t.location = location;
            scene.add_object(Primitive::Cube { size: 2.0 }, t)
        };
        // a floating "door"-like pair: root high up, child hanging below
        let root = at(Vec3::new(0.0, 0.0, 5.0));
        let child = at(Vec3::new(0.0, 3.0, 2.0));
        // and an independent floating cube
        let loose = at(Vec3::new(4.0, 0.0, -3.0));
        scene.set_parent(child, Some(root));

        let mut selection = Selection::default();
        selection.set(vec![root, child, loose], Some(root));
        place_on_ground(&mut scene, &selection);

        // the pair moved as ONE piece: the child's bottom (the lowest point
        // of the subtree) sits at z = 0 and the 3 m gap is preserved
        assert!((scene.lowest_point_z(child) - 0.0).abs() < 1e-4);
        let root_z = scene.world_transform(root).location.z;
        let child_z = scene.world_transform(child).location.z;
        assert!((root_z - child_z - 3.0).abs() < 1e-4, "{root_z} vs {child_z}");
        // the loose cube (below ground before) came UP to rest at z = 0
        let w = scene.world_transform(loose);
        assert!((w.location.z - 1.0).abs() < 1e-4, "{:?}", w.location);
    }

    /// Signed volume of a mesh (divergence theorem, outward winding).
    fn mesh_volume(m: &modeler_core::MeshData) -> f32 {
        m.indices
            .chunks_exact(3)
            .map(|tri| {
                let a = m.positions[tri[0] as usize];
                let b = m.positions[tri[1] as usize];
                let c = m.positions[tri[2] as usize];
                a.dot(b.cross(c)) / 6.0
            })
            .sum()
    }

    #[test]
    fn boolean_menu_adds_a_preview_modifier_then_apply_bakes_it() {
        let mut scene = Scene::new();
        let target = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        // a size-2 cube scaled to 0.5 → effectively a unit cube, shifted
        // diagonally so a corner chunk (0.75³) overlaps the target
        let tool = scene.add_object(
            Primitive::Cube { size: 2.0 },
            Transform {
                location: Vec3::splat(0.25),
                scale: Vec3::splat(0.5),
                ..Transform::default()
            },
        );

        let mut selection = Selection::default();
        selection.set(vec![tool, target], Some(target));
        let message =
            boolean_selected(&mut scene, &mut selection, modeler_core::BooleanOp::Subtract);
        assert!(message.contains("preview is live"), "{message}");
        assert_eq!(selection.selected(), &[target], "the target stays selected");

        // non-destructive so far: modifier on the target, tool hidden
        let object = scene.object(target).unwrap();
        assert_eq!(object.modifiers.len(), 1);
        assert!(object.edited_mesh.is_none(), "nothing baked yet");
        assert!(scene.object(tool).is_some());
        assert!(!scene.object(tool).unwrap().visible, "tool hidden for preview");
        let expected = 1.0 - 0.75f32.powi(3);
        let preview = crate::modifiers::evaluate(&scene, target);
        assert!((mesh_volume(&preview) - expected).abs() < 1e-3);

        // applying bakes the preview and consumes the tool
        crate::modifiers::apply(&mut scene, target, usize::MAX).unwrap();
        assert!(scene.object(tool).is_none(), "tool consumed on apply");
        let mesh = scene.object(target).unwrap().edited_mesh.as_ref().unwrap();
        assert!((mesh_volume(mesh) - expected).abs() < 1e-3, "{}", mesh_volume(mesh));
    }

    #[test]
    fn boolean_union_merges_several_tools() {
        let mut scene = Scene::new();
        let at = |scene: &mut Scene, location: Vec3| {
            scene.add_object(
                Primitive::Cube { size: 1.0 },
                Transform { location, ..Transform::default() },
            )
        };
        let target = at(&mut scene, Vec3::ZERO);
        let near = at(&mut scene, Vec3::splat(0.25)); // overlaps 0.75³
        let far = at(&mut scene, Vec3::new(5.0, 0.0, 0.0)); // disjoint

        let message = boolean_apply(
            &mut scene,
            target,
            &[near, far],
            modeler_core::BooleanOp::Union,
        )
        .unwrap();
        assert!(message.contains("merged 2 objects into 'Cube'"), "{message}");
        assert!(scene.object(near).is_none() && scene.object(far).is_none());
        let mesh = scene.object(target).unwrap().edited_mesh.as_ref().unwrap();
        let expected = 3.0 - 0.75f32.powi(3);
        assert!((mesh_volume(mesh) - expected).abs() < 1e-3, "{}", mesh_volume(mesh));
    }

    #[test]
    fn boolean_rejects_bad_input_without_changing_the_scene() {
        let mut scene = Scene::new();
        let cube = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        let light =
            scene.add_object(Primitive::light_catalog()[0], Transform::default());
        // lights have no volume, on either side
        assert!(boolean_apply(&mut scene, cube, &[light], modeler_core::BooleanOp::Union)
            .is_err());
        assert!(boolean_apply(&mut scene, light, &[cube], modeler_core::BooleanOp::Union)
            .is_err());
        // target among the tools
        assert!(boolean_apply(&mut scene, cube, &[cube], modeler_core::BooleanOp::Union)
            .is_err());
        // an empty result (tool swallows the target) must not apply
        let big = scene.add_object(Primitive::Cube { size: 5.0 }, Transform::default());
        assert!(
            boolean_apply(&mut scene, cube, &[big], modeler_core::BooleanOp::Subtract)
                .is_err()
        );
        assert!(scene.object(big).is_some(), "failed boolean consumes nothing");
        assert!(scene.object(cube).unwrap().edited_mesh.is_none());
        assert_eq!(scene.objects().len(), 3);
    }

    #[test]
    fn apply_scale_bakes_uniform_cube_into_size() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        scene.object_mut(id).unwrap().transform.scale = Vec3::splat(2.0);
        scene.object_mut(id).unwrap().pivot = Vec3::new(0.5, 0.0, 0.0);

        let mut selection = Selection::default();
        selection.set(vec![id], Some(id));
        apply_scale(&mut scene, &selection);

        let object = scene.object(id).unwrap();
        assert!(matches!(object.primitive, Primitive::Cube { size } if (size - 4.0).abs() < 1e-5));
        assert_eq!(object.transform.scale, Vec3::ONE);
        assert!(object.edited_mesh.is_none(), "parametric bake, no mesh");
        assert!((object.pivot.x - 1.0).abs() < 1e-5, "pivot scales along");
    }

    #[test]
    fn apply_scale_bakes_wall_per_axis_with_cutouts() {
        let mut scene = Scene::new();
        let id = scene.add_object(
            Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 },
            Transform::default(),
        );
        {
            let object = scene.object_mut(id).unwrap();
            object.cutouts.push(modeler_core::WallCutout {
                offset: 1.0,
                width: 0.9,
                bottom: 0.5,
                height: 1.0,
            });
            object.transform.scale = Vec3::new(2.0, 3.0, 0.5);
        }
        let mut selection = Selection::default();
        selection.set(vec![id], Some(id));
        apply_scale(&mut scene, &selection);

        let object = scene.object(id).unwrap();
        let Primitive::Wall { length, height, thickness } = object.primitive else {
            panic!("still a wall");
        };
        assert!((length - 8.0).abs() < 1e-5 && (height - 1.25).abs() < 1e-5);
        assert!((thickness - 0.6).abs() < 1e-5);
        let cutout = &object.cutouts[0];
        assert!((cutout.offset - 2.0).abs() < 1e-5 && (cutout.width - 1.8).abs() < 1e-5);
        assert!((cutout.bottom - 0.25).abs() < 1e-5 && (cutout.height - 0.5).abs() < 1e-5);
        assert_eq!(object.transform.scale, Vec3::ONE);
    }

    #[test]
    fn apply_scale_bakes_nonuniform_sphere_into_mesh() {
        let mut scene = Scene::new();
        let id = scene.add_object(
            Primitive::UvSphere { segments: 16, rings: 8, radius: 1.0 },
            Transform::default(),
        );
        scene.object_mut(id).unwrap().transform.scale = Vec3::new(1.0, 2.0, 1.0);

        let mut selection = Selection::default();
        selection.set(vec![id], Some(id));
        apply_scale(&mut scene, &selection);

        let object = scene.object(id).unwrap();
        let mesh = object.edited_mesh.as_ref().expect("non-uniform round shape bakes the mesh");
        let max_y = mesh.positions.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
        assert!((max_y - 2.0).abs() < 0.05, "stretched to the applied scale: {max_y}");
        assert_eq!(object.transform.scale, Vec3::ONE);
    }

    #[test]
    fn apply_scale_keeps_children_in_place() {
        let mut scene = Scene::new();
        let parent = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let child = scene.add_object(
            Primitive::Cube { size: 1.0 },
            Transform { location: Vec3::new(3.0, 0.0, 0.0), ..Transform::default() },
        );
        scene.set_parent(child, Some(parent));
        scene.object_mut(parent).unwrap().transform.scale = Vec3::splat(2.0);
        let world_before = scene.world_transform(child);

        let mut selection = Selection::default();
        selection.set(vec![parent], Some(parent));
        apply_scale(&mut scene, &selection);

        let world_after = scene.world_transform(child);
        assert!(
            (world_after.location - world_before.location).length() < 1e-4,
            "{:?} vs {:?}",
            world_after.location,
            world_before.location
        );
        assert!((world_after.scale - world_before.scale).length() < 1e-4);
    }
}
