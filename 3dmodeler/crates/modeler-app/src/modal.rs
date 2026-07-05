//! Blender-style modal transform operators.
//!
//! G = grab/move, R = rotate, S = scale. While active the operator owns the
//! mouse and keyboard: X/Y/Z constrain to an axis (Shift+axis = plane lock),
//! typing digits gives exact values, Ctrl snaps, LMB/Enter confirms,
//! RMB/Escape cancels. Shift+D duplicates the selection and drops into grab.
//!
//! All letter shortcuts match on `Event::Text` (the typed character) so they
//! follow the user's keyboard layout — `Key::*` codes are physical positions
//! on the web backend, which breaks AZERTY and friends.

use crate::camera::BlenderCamera;
use crate::selection::Selection;
use crate::settings::Unit;
use modeler_core::glam::{Quat, Vec3};
use modeler_core::{ObjectId, Scene, Transform};
use three_d::{Event, Key, MouseButton, Viewport};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Grab,
    Rotate,
    Scale,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Constraint {
    Free,
    Axis(usize),
    Plane(usize), // everything except this axis
}

struct OriginalEntry {
    id: ObjectId,
    local: Transform,
    world: Transform,
    parent: Option<ObjectId>,
}

struct ModalState {
    kind: Kind,
    constraint: Constraint,
    originals: Vec<OriginalEntry>,
    /// Non-selected descendants of transformed objects (rotate/scale only —
    /// empty for grab, where children follow the parent). These keep their
    /// world placement: their local transforms are re-derived every frame to
    /// cancel the ancestor's motion.
    frozen: Vec<OriginalEntry>,
    pivot: Vec3,
    start_mouse: (f32, f32), // physical px, bottom-left origin
    cur_mouse: (f32, f32),
    snap: bool,
    numeric: String,
    status: String,
    /// Vertex-snap candidates: world-space vertices of all NON-moving
    /// objects, collected at operator start.
    snap_candidates: Vec<Vec3>,
    /// World-space vertices of the moving selection (at start).
    snap_sources: Vec<Vec3>,
    /// Snapped-to vertex this frame (drawn by the overlay).
    snap_target: Option<Vec3>,
    /// Rotate only: the applied angle as it appears on screen (CCW positive
    /// in bottom-left-origin pixels), after snap/numeric input. Drives the
    /// overlay's rotation arc.
    screen_sweep: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum GuideKind {
    Move,
    Rotate,
    Scale,
}

/// Snapshot of the active operator for the viewport overlay: axis guide
/// lines and the rotation arc. Mouse positions are physical pixels with a
/// bottom-left origin (the three-d event convention).
pub struct Guides {
    pub kind: GuideKind,
    /// World-axis indices (0=X, 1=Y, 2=Z) to draw through the pivot: one for
    /// an axis constraint, the two free axes for a plane lock, none for free.
    pub axes: Vec<usize>,
    pub pivot: Vec3,
    pub start_mouse: (f32, f32),
    pub cur_mouse: (f32, f32),
    pub screen_sweep: f32,
    /// Vertex-snap target to highlight.
    pub snap_target: Option<Vec3>,
}

pub struct ModalTransform {
    state: Option<ModalState>,
    last_mouse: (f32, f32),
}

fn axis_vec(i: usize) -> Vec3 {
    [Vec3::X, Vec3::Y, Vec3::Z][i]
}

fn axis_name(i: usize) -> &'static str {
    ["X", "Y", "Z"][i]
}

fn gv(v: three_d::Vec3) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

fn cg(v: Vec3) -> three_d::Vec3 {
    three_d::vec3(v.x, v.y, v.z)
}

impl ModalTransform {
    pub fn new() -> Self {
        Self {
            state: None,
            last_mouse: (0.0, 0.0),
        }
    }

    pub fn active(&self) -> bool {
        self.state.is_some()
    }

    pub fn status_line(&self) -> Option<String> {
        self.state.as_ref().map(|s| s.status.clone())
    }

    pub fn guides(&self) -> Option<Guides> {
        let state = self.state.as_ref()?;
        Some(Guides {
            kind: match state.kind {
                Kind::Grab => GuideKind::Move,
                Kind::Rotate => GuideKind::Rotate,
                Kind::Scale => GuideKind::Scale,
            },
            axes: match state.constraint {
                Constraint::Free => Vec::new(),
                Constraint::Axis(i) => vec![i],
                Constraint::Plane(i) => (0..3).filter(|&a| a != i).collect(),
            },
            pivot: state.pivot,
            start_mouse: state.start_mouse,
            cur_mouse: state.cur_mouse,
            screen_sweep: state.screen_sweep,
            snap_target: state.snap_target,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        camera: &BlenderCamera,
        viewport: Viewport,
        scene: &mut Scene,
        selection: &mut Selection,
        egui_owns_keyboard: bool,
        snap_to_grid: bool,
        snap_to_vertex: bool,
        grid_spacing: f32,
        unit: Unit,
    ) {
        let mut confirm = false;
        let mut cancel = false;

        for event in events.iter_mut() {
            match event {
                Event::MouseMotion {
                    position,
                    modifiers,
                    handled,
                    ..
                } => {
                    self.last_mouse = (position.x, position.y);
                    if let Some(state) = &mut self.state {
                        state.cur_mouse = self.last_mouse;
                        state.snap = modifiers.ctrl;
                        *handled = true;
                    }
                }
                Event::MousePress {
                    button,
                    position,
                    handled,
                    ..
                } => {
                    self.last_mouse = (position.x, position.y);
                    if self.state.is_some() && !*handled {
                        match button {
                            MouseButton::Left => confirm = true,
                            MouseButton::Right => cancel = true,
                            MouseButton::Middle => {}
                        }
                        if *button != MouseButton::Middle {
                            *handled = true;
                        }
                    }
                }
                Event::MouseRelease { button, handled, .. }
                    if self.state.is_some() && *button != MouseButton::Middle =>
                {
                    *handled = true;
                }
                Event::KeyPress {
                    kind,
                    modifiers,
                    handled,
                } if self.state.is_some() && !*handled => {
                    if let Some(state) = &mut self.state {
                        state.snap = modifiers.ctrl;
                        match kind {
                            Key::Enter => {
                                confirm = true;
                                *handled = true;
                            }
                            Key::Escape => {
                                cancel = true;
                                *handled = true;
                            }
                            Key::Backspace => {
                                state.numeric.pop();
                                *handled = true;
                            }
                            // the operator owns the keyboard: swallow every
                            // other key so camera shortcuts don't fire while
                            // typing (digits are the numpad view keys!)
                            _ => *handled = true,
                        }
                    }
                }
                Event::Text(text) if !egui_owns_keyboard && !text.is_empty() => {
                    if self.state.is_some() {
                        self.text_while_active(text.clone().as_str());
                        text.clear(); // the operator owns typed input
                    } else {
                        self.maybe_start(text.clone().as_str(), scene, selection);
                        if self.state.is_some() {
                            text.clear();
                        }
                    }
                }
                _ => {}
            }
        }

        if cancel {
            if let Some(state) = self.state.take() {
                for entry in state.originals.iter().chain(&state.frozen) {
                    if let Some(object) = scene.object_mut(entry.id) {
                        object.transform = entry.local;
                    }
                }
            }
            return;
        }

        self.apply(camera, viewport, scene, snap_to_grid, snap_to_vertex, grid_spacing, unit);

        if confirm {
            self.state = None; // transforms already applied
        }
    }

    /// Start a grab on the current selection (used by the Object menu's
    /// Duplicate, which mirrors Shift+D, and the toolbar).
    pub fn begin_grab(&mut self, scene: &Scene, selection: &Selection) {
        self.start(Kind::Grab, scene, selection);
    }

    /// Start a rotate on the current selection (toolbar).
    pub fn begin_rotate(&mut self, scene: &Scene, selection: &Selection) {
        self.start(Kind::Rotate, scene, selection);
    }

    /// Start a scale on the current selection (toolbar).
    pub fn begin_scale(&mut self, scene: &Scene, selection: &Selection) {
        self.start(Kind::Scale, scene, selection);
    }

    /// Start an operator from a typed character, or duplicate on Shift+D.
    fn maybe_start(&mut self, text: &str, scene: &mut Scene, selection: &mut Selection) {
        let kind = match text {
            "g" => Kind::Grab,
            "r" => Kind::Rotate,
            "s" => Kind::Scale,
            "D" => {
                // Shift+D: duplicate, then grab the copies (Blender behavior)
                if selection.is_empty() || !duplicate_selection(scene, selection) {
                    return;
                }
                Kind::Grab
            }
            _ => return,
        };
        self.start(kind, scene, selection);
    }

    fn start(&mut self, kind: Kind, scene: &Scene, selection: &Selection) {
        if selection.is_empty() {
            return;
        }

        // Grab moves the whole hierarchy (children follow, like Blender);
        // Rotate/Scale apply ONLY to the selected objects — linked children
        // keep their world placement (explicit deviation from Blender)
        let selected: Vec<ObjectId> = selection.selected().to_vec();
        let entry = |o: &modeler_core::Object| OriginalEntry {
            id: o.id,
            local: o.transform,
            world: scene.world_transform(o.id),
            parent: o.parent.filter(|p| scene.object(*p).is_some()),
        };
        let originals: Vec<OriginalEntry> = scene
            .objects()
            .iter()
            .filter(|o| selection.is_selected(o.id))
            .map(entry)
            .collect();
        if originals.is_empty() {
            return;
        }
        // rotate/scale: non-selected descendants must keep their world
        // placement while their ancestors transform — capture where they are.
        // grab: leave empty so children ride along through the hierarchy.
        let frozen: Vec<OriginalEntry> = if kind == Kind::Grab {
            Vec::new()
        } else {
            scene
                .objects()
                .iter()
                .filter(|o| {
                    !selection.is_selected(o.id)
                        && selected.iter().any(|&s| scene.is_ancestor(s, o.id))
                })
                .map(entry)
                .collect()
        };
        let pivot = originals.iter().map(|e| e.world.location).sum::<Vec3>()
            / originals.len() as f32;

        // vertex snap: moving geometry (selection + followers) vs the rest
        let moving: Vec<ObjectId> = scene
            .objects()
            .iter()
            .filter(|o| {
                selection.is_selected(o.id)
                    || selected.iter().any(|&s| scene.is_ancestor(s, o.id))
            })
            .map(|o| o.id)
            .collect();
        let world_verts = |id: ObjectId| -> Vec<Vec3> {
            let Some(object) = scene.object(id) else { return Vec::new() };
            let world = scene.world_transform(id);
            object
                .render_mesh()
                .positions
                .iter()
                .map(|p| world.location + world.rotation * (*p * world.scale))
                .collect()
        };
        let snap_candidates: Vec<Vec3> = scene
            .objects()
            .iter()
            .filter(|o| o.visible && !moving.contains(&o.id))
            .flat_map(|o| world_verts(o.id))
            .collect();
        let snap_sources: Vec<Vec3> = originals.iter().flat_map(|e| world_verts(e.id)).collect();

        self.state = Some(ModalState {
            kind,
            constraint: Constraint::Free,
            originals,
            frozen,
            pivot,
            start_mouse: self.last_mouse,
            cur_mouse: self.last_mouse,
            snap: false,
            numeric: String::new(),
            status: String::new(),
            screen_sweep: 0.0,
            snap_candidates,
            snap_sources,
            snap_target: None,
        });
    }

    fn text_while_active(&mut self, text: &str) {
        let Some(state) = &mut self.state else { return };
        match text {
            "x" | "X" | "y" | "Y" | "z" | "Z" => {
                let i = match text.to_ascii_lowercase().as_str() {
                    "x" => 0,
                    "y" => 1,
                    _ => 2,
                };
                let plane = text.chars().next().unwrap().is_ascii_uppercase();
                let new = if plane { Constraint::Plane(i) } else { Constraint::Axis(i) };
                // same constraint again toggles back to free (Blender cycles
                // global -> local -> free; local axes are a later refinement)
                state.constraint = if state.constraint == new { Constraint::Free } else { new };
            }
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                state.numeric.push_str(text);
            }
            "." | "," => {
                if !state.numeric.contains('.') {
                    state.numeric.push('.');
                }
            }
            "-" => {
                // Blender: minus toggles the sign
                if state.numeric.starts_with('-') {
                    state.numeric.remove(0);
                } else {
                    state.numeric.insert(0, '-');
                }
            }
            _ => {}
        }
    }

    fn numeric_value(state: &ModalState) -> Option<f32> {
        state.numeric.parse::<f32>().ok()
    }

    /// Recompute the transform of every selected object from its original —
    /// absolute application keeps this robust (no drift, trivial cancel).
    fn apply(
        &mut self,
        camera: &BlenderCamera,
        viewport: Viewport,
        scene: &mut Scene,
        snap_to_grid: bool,
        snap_to_vertex: bool,
        grid_spacing: f32,
        unit: Unit,
    ) {
        let Some(state) = &mut self.state else { return };

        let (right, up, forward) = camera.screen_basis();
        let (right, up, forward) = (gv(right), gv(up), gv(forward));
        let dx = state.cur_mouse.0 - state.start_mouse.0;
        let dy = state.cur_mouse.1 - state.start_mouse.1;
        let numeric = Self::numeric_value(state);

        let constraint_tag = match state.constraint {
            Constraint::Free => String::new(),
            Constraint::Axis(i) => format!(" along {}", axis_name(i)),
            Constraint::Plane(i) => format!(" locking {}", axis_name(i)),
        };
        let numeric_tag = if state.numeric.is_empty() {
            String::new()
        } else {
            format!("  [{}]", state.numeric)
        };

        // target WORLD transform per selected object, parallel to `originals`
        let mut targets: Vec<Transform> = Vec::with_capacity(state.originals.len());

        match state.kind {
            Kind::Grab => {
                let wpp = camera.world_per_pixel_at(viewport, cg(state.pivot));
                let mut delta = right * (dx * wpp) + up * (dy * wpp);
                match state.constraint {
                    Constraint::Free => {}
                    Constraint::Axis(i) => {
                        let axis = axis_vec(i);
                        delta = axis * delta.dot(axis);
                    }
                    Constraint::Plane(i) => {
                        let axis = axis_vec(i);
                        delta -= axis * delta.dot(axis);
                    }
                }
                // vertex snap: pull the selection so its closest vertex
                // lands on the non-moving vertex nearest to the cursor
                state.snap_target = None;
                let mut vertex_snapped = false;
                if snap_to_vertex && numeric.is_none() {
                    let screen_of = |p: Vec3| camera.world_to_screen(viewport, cg(p));
                    if let Some((raw, target)) = vertex_snap_delta(
                        &state.snap_candidates,
                        &state.snap_sources,
                        state.cur_mouse,
                        screen_of,
                        30.0,
                    ) {
                        delta = match state.constraint {
                            Constraint::Free => raw,
                            Constraint::Axis(i) => {
                                let axis = axis_vec(i);
                                axis * raw.dot(axis)
                            }
                            Constraint::Plane(i) => {
                                let axis = axis_vec(i);
                                raw - axis * raw.dot(axis)
                            }
                        };
                        state.snap_target = Some(target);
                        vertex_snapped = true;
                    }
                }

                // grid snap: the toggle enables it, Ctrl inverts while dragging
                let snapping = (snap_to_grid != state.snap) && !vertex_snapped;
                if let Some(v) = numeric {
                    // typed value: along the constrained axis, X by default,
                    // interpreted in the display unit (Preferences ▸ Units)
                    let axis = match state.constraint {
                        Constraint::Axis(i) => axis_vec(i),
                        Constraint::Plane(_) | Constraint::Free => Vec3::X,
                    };
                    delta = axis * unit.to_meters(v);
                }
                let shown = delta * unit.per_meter();
                state.status = format!(
                    "Move: ({:.p$}, {:.p$}, {:.p$}) {}{}{}{}",
                    shown.x,
                    shown.y,
                    shown.z,
                    unit.suffix(),
                    constraint_tag,
                    numeric_tag,
                    if vertex_snapped {
                        "  [snap: vertex]"
                    } else if snapping && numeric.is_none() {
                        "  [snap]"
                    } else {
                        ""
                    },
                    p = unit.decimals(),
                );
                for entry in &state.originals {
                    let mut world = entry.world;
                    world.location = entry.world.location + delta;
                    if snapping && numeric.is_none() {
                        // absolute grid positions, like Blender's grid snap
                        world.location =
                            (world.location / grid_spacing).round() * grid_spacing;
                    }
                    targets.push(world);
                }
            }
            Kind::Rotate => {
                let pivot_screen = camera.world_to_screen(viewport, cg(state.pivot));
                let a0 = (state.start_mouse.1 - pivot_screen.1)
                    .atan2(state.start_mouse.0 - pivot_screen.0);
                let a1 =
                    (state.cur_mouse.1 - pivot_screen.1).atan2(state.cur_mouse.0 - pivot_screen.0);
                let mouse_angle = a1 - a0;

                // rotation axis: view axis (toward the viewer) or a world axis
                let view_axis = -forward;
                let (axis, sign) = match state.constraint {
                    Constraint::Free => (view_axis, 1.0),
                    Constraint::Axis(i) | Constraint::Plane(i) => {
                        let axis = axis_vec(i);
                        (axis, if axis.dot(view_axis) >= 0.0 { 1.0 } else { -1.0 })
                    }
                };
                let mut angle = match numeric {
                    Some(v) => v.to_radians(),
                    None => sign * mouse_angle,
                };
                if numeric.is_none() && state.snap {
                    let step = 5f32.to_radians();
                    angle = (angle / step).round() * step;
                }
                // how the applied angle looks from this viewpoint (sign folds
                // the world axis back into screen space; sign*sign = 1)
                state.screen_sweep = sign * angle;
                state.status = format!(
                    "Rotate: {:.1}°{}{}",
                    angle.to_degrees(),
                    constraint_tag,
                    numeric_tag
                );
                let rotation = Quat::from_axis_angle(axis.normalize_or_zero(), angle);
                for entry in &state.originals {
                    let mut world = entry.world;
                    world.location = state.pivot + rotation * (entry.world.location - state.pivot);
                    world.rotation = (rotation * entry.world.rotation).normalize();
                    targets.push(world);
                }
            }
            Kind::Scale => {
                let pivot_screen = camera.world_to_screen(viewport, cg(state.pivot));
                let d0 = ((state.start_mouse.0 - pivot_screen.0).powi(2)
                    + (state.start_mouse.1 - pivot_screen.1).powi(2))
                .sqrt()
                .max(1.0);
                let d1 = ((state.cur_mouse.0 - pivot_screen.0).powi(2)
                    + (state.cur_mouse.1 - pivot_screen.1).powi(2))
                .sqrt();
                let mut factor = match numeric {
                    Some(v) => v,
                    None => d1 / d0,
                };
                if numeric.is_none() && state.snap {
                    factor = (factor / 0.1).round() * 0.1;
                }
                let factors = match state.constraint {
                    Constraint::Free => Vec3::splat(factor),
                    Constraint::Axis(i) => {
                        let mut f = Vec3::ONE;
                        f[i] = factor;
                        f
                    }
                    Constraint::Plane(i) => {
                        let mut f = Vec3::splat(factor);
                        f[i] = 1.0;
                        f
                    }
                };
                state.status = format!("Scale: {:.3}{}{}", factor, constraint_tag, numeric_tag);
                for entry in &state.originals {
                    let mut world = entry.world;
                    world.location = state.pivot + (entry.world.location - state.pivot) * factors;
                    world.scale = entry.world.scale * factors;
                    targets.push(world);
                }
            }
        }

        write_targets(scene, &state.originals, &targets, &state.frozen);
    }
}

/// Write each target WORLD transform back as a local transform. A parent may
/// itself be transformed (selected) or frozen, so locals are derived from
/// the parent's NEW world — then frozen descendants are re-pinned so linked
/// children keep their world placement while only the selection moves.
fn write_targets(
    scene: &mut Scene,
    originals: &[OriginalEntry],
    targets: &[Transform],
    frozen: &[OriginalEntry],
) {
    // the new world of any object: its target (selected), its captured world
    // (frozen), or its current scene world (untouched by the operator)
    let new_world = |scene: &Scene, id: ObjectId| -> Transform {
        if let Some(i) = originals.iter().position(|e| e.id == id) {
            targets[i]
        } else if let Some(f) = frozen.iter().find(|f| f.id == id) {
            f.world
        } else {
            scene.world_transform(id)
        }
    };

    let mut writes: Vec<(ObjectId, Transform)> =
        Vec::with_capacity(originals.len() + frozen.len());
    for (entry, target) in originals.iter().zip(targets) {
        let local = match entry.parent {
            Some(p) => new_world(scene, p).to_local(target),
            None => *target,
        };
        writes.push((entry.id, local));
    }
    for f in frozen {
        // frozen objects always have a parent (they are descendants)
        let Some(p) = f.parent else { continue };
        writes.push((f.id, new_world(scene, p).to_local(&f.world)));
    }
    for (id, local) in writes {
        if let Some(object) = scene.object_mut(id) {
            object.transform = local;
        }
    }
}

/// Clone the selected objects (Blender Shift+D). The clones become the new
/// selection. Returns false if there was nothing to duplicate.
pub fn duplicate_selection(scene: &mut Scene, selection: &mut Selection) -> bool {
    let sources: Vec<modeler_core::Object> = scene
        .objects()
        .iter()
        .filter(|o| selection.is_selected(o.id))
        .cloned()
        .collect();
    if sources.is_empty() {
        return false;
    }
    let mut new_ids = Vec::with_capacity(sources.len());
    let mut new_active = None;
    let mut id_map: std::collections::HashMap<ObjectId, ObjectId> =
        std::collections::HashMap::new();
    for source in &sources {
        let id = scene.add_object(source.primitive, source.transform);
        if let Some(object) = scene.object_mut(id) {
            object.smooth = source.smooth;
            object.material = source.material;
            object.dynamic = source.dynamic;
            object.density = source.density;
            object.parent = source.parent; // remapped below if inside the set
            object.show_label = source.show_label;
            object.show_dimensions = source.show_dimensions;
            object.edited_mesh = source.edited_mesh.clone();
        }
        id_map.insert(source.id, id);
        if selection.active() == Some(source.id) {
            new_active = Some(id);
        }
        new_ids.push(id);
    }
    // duplicates of parented objects follow the DUPLICATED parent, like Blender
    for &new_id in &new_ids {
        let parent = scene.object(new_id).and_then(|o| o.parent);
        if let Some(p) = parent {
            if let Some(&remapped) = id_map.get(&p) {
                if let Some(object) = scene.object_mut(new_id) {
                    object.parent = Some(remapped);
                }
            }
        }
    }
    let active = new_active.or_else(|| new_ids.last().copied());
    selection.set(new_ids, active);
    true
}

/// Vertex snap: find the candidate vertex nearest to the cursor (within
/// `max_px` on screen), then the moving-source vertex nearest to it in world
/// space (Blender's "closest"). Returns (raw delta = target - source, target).
fn vertex_snap_delta(
    candidates: &[Vec3],
    sources: &[Vec3],
    mouse: (f32, f32),
    screen_of: impl Fn(Vec3) -> (f32, f32),
    max_px: f32,
) -> Option<(Vec3, Vec3)> {
    if sources.is_empty() {
        return None;
    }
    let mut target: Option<(f32, Vec3)> = None;
    for &c in candidates {
        let (sx, sy) = screen_of(c);
        let d = ((sx - mouse.0).powi(2) + (sy - mouse.1).powi(2)).sqrt();
        if d < max_px && target.is_none_or(|(bd, _)| d < bd) {
            target = Some((d, c));
        }
    }
    let (_, target) = target?;
    let source = sources
        .iter()
        .copied()
        .min_by(|a, b| {
            (*a - target)
                .length_squared()
                .total_cmp(&(*b - target).length_squared())
        })?;
    Some((target - source, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_snap_picks_cursor_candidate_and_closest_source() {
        // screen mapping: x/y passthrough (z ignored)
        let screen = |p: Vec3| (p.x, p.y);
        let candidates = vec![Vec3::new(100.0, 100.0, 0.0), Vec3::new(500.0, 500.0, 2.0)];
        let sources = vec![Vec3::new(90.0, 90.0, 0.0), Vec3::new(400.0, 400.0, 0.0)];

        // cursor near the first candidate -> snapped to it, from the nearest source
        let (delta, target) =
            vertex_snap_delta(&candidates, &sources, (105.0, 98.0), screen, 30.0).unwrap();
        assert_eq!(target, Vec3::new(100.0, 100.0, 0.0));
        assert_eq!(delta, Vec3::new(10.0, 10.0, 0.0));

        // cursor far from every candidate -> no snap
        assert!(vertex_snap_delta(&candidates, &sources, (300.0, 0.0), screen, 30.0).is_none());
        // no moving vertices -> no snap
        assert!(vertex_snap_delta(&candidates, &[], (105.0, 98.0), screen, 30.0).is_none());
    }
}
