//! Terrain sculpt tool: brush strokes that push the selected terrain's
//! surface around (raise / lower / smooth / flatten). Strokes write into the
//! terrain's non-destructive sculpt layer (`TerrainData::sculpt`), never
//! into the noise stack, so the procedural base stays intact.
//!
//! Live feedback vs. physics: every dab bumps `Object::sculpt_revision`
//! (renderer re-uploads, stack evaluation is memoized so only the sculpt
//! add re-runs) while `mesh_revision` — which the physics mirror keys on —
//! is bumped ONCE at stroke end. Mid-stroke picking therefore never uses
//! the physics mirror; the tool ray-marches the evaluated height grid
//! itself (`terrain::raycast_grid`).

use crate::gfx::egui;
use crate::gfx::{Event, Key, MouseButton, Viewport};
use modeler_core::glam::{Vec2, Vec3};
use modeler_core::terrain::{self, BrushMode, SculptLayer};
use modeler_core::{ObjectId, Primitive, Scene};

pub struct SculptTool {
    active: bool,
    target: Option<ObjectId>,
    pub mode: BrushMode,
    /// Brush radius in meters.
    pub radius: f32,
    /// Raise/Lower: meters per second of push. Smooth/Flatten: lerp per second.
    pub strength: f32,
    /// Soft fraction of the radius (0 hard rim .. 1 all falloff).
    pub falloff: f32,
    /// LMB is down and dabs are being applied.
    stroking: bool,
    /// Terrain-local hit under the cursor, if any (drives the brush ring).
    hover: Option<Vec3>,
    last_mouse: (f32, f32), // physical px, bottom-left origin
    /// Height (meters) Flatten pulls toward — sampled at stroke start.
    flatten_target: f32,
    ctrl_down: bool,
}

impl SculptTool {
    pub fn new() -> Self {
        Self {
            active: false,
            target: None,
            mode: BrushMode::Raise,
            radius: 8.0,
            strength: 0.6,
            falloff: 0.6,
            stroking: false,
            hover: None,
            last_mouse: (0.0, 0.0),
            flatten_target: 0.0,
            ctrl_down: false,
        }
    }

    /// Arm the tool on a terrain object.
    pub fn start(&mut self, target: ObjectId) {
        self.active = true;
        self.target = Some(target);
        self.stroking = false;
        self.hover = None;
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn target(&self) -> Option<ObjectId> {
        self.target.filter(|_| self.active)
    }

    /// A stroke is in flight (used to batch undo checkpoints).
    pub fn stroking(&self) -> bool {
        self.active && self.stroking
    }

    /// Turn the tool off, resyncing physics if a stroke was cut short.
    pub fn abort(&mut self, scene: &mut Scene) {
        if self.stroking {
            self.finish_stroke(scene);
        }
        self.active = false;
        self.hover = None;
    }

    pub fn status_line(&self, unit: crate::settings::Unit) -> Option<String> {
        if !self.active {
            return None;
        }
        Some(format!(
            "Sculpt ({}): drag to {} · radius {} · Ctrl inverts raise/lower   |   Esc/RMB done",
            self.mode.label(),
            self.mode.label().to_ascii_lowercase(),
            unit.format(self.radius),
        ))
    }

    /// The effective mode: Ctrl swaps Raise ↔ Lower (Blender habit).
    fn effective_mode(&self) -> BrushMode {
        match (self.mode, self.ctrl_down) {
            (BrushMode::Raise, true) => BrushMode::Lower,
            (BrushMode::Lower, true) => BrushMode::Raise,
            (mode, _) => mode,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        camera: &crate::camera::BlenderCamera,
        viewport: Viewport,
        scene: &mut Scene,
        egui_owns_keyboard: bool,
        pointer_over_ui: bool,
        dt_seconds: f32,
    ) {
        if !self.active {
            return;
        }
        // target vanished (deleted, new scene): quietly put the tool away
        let Some(target) = self.target else {
            self.active = false;
            return;
        };
        if scene
            .object(target)
            .map(|o| !matches!(o.primitive, Primitive::Terrain { .. }))
            .unwrap_or(true)
        {
            self.active = false;
            self.stroking = false;
            return;
        }

        let mut exit = false;
        for event in events.iter_mut() {
            match event {
                Event::MouseMotion { position, modifiers, .. } => {
                    self.last_mouse = (position.x, position.y);
                    self.ctrl_down = modifiers.ctrl;
                }
                Event::MousePress { button, position, modifiers, handled } => {
                    self.last_mouse = (position.x, position.y);
                    self.ctrl_down = modifiers.ctrl;
                    if *handled || pointer_over_ui {
                        continue;
                    }
                    match button {
                        MouseButton::Left => {
                            self.stroking = true;
                            // Flatten locks onto the height under the cursor
                            if let Some(hit) = self.hover {
                                self.flatten_target = hit.z;
                            }
                            *handled = true;
                        }
                        MouseButton::Right => {
                            exit = true;
                            *handled = true;
                        }
                        MouseButton::Middle => continue, // camera keeps orbiting
                    }
                }
                Event::MouseRelease { button, handled, .. } => {
                    if *button == MouseButton::Left && self.stroking {
                        self.finish_stroke(scene);
                        *handled = true;
                    }
                }
                Event::KeyPress { kind: Key::Escape, handled, .. } if !*handled => {
                    exit = true;
                    *handled = true;
                }
                // the tool owns typed input: keep G/R/S/X/… inert
                Event::Text(text) if !egui_owns_keyboard && !text.is_empty() => {
                    text.clear();
                }
                _ => {}
            }
        }

        if exit {
            self.abort(scene);
            return;
        }

        // where is the cursor on the terrain?
        self.hover = self.pick(scene, target, camera, viewport);

        if self.stroking {
            match self.hover {
                Some(hit) => self.apply_dab(scene, target, hit, dt_seconds),
                None => {} // stroke continues when the cursor comes back
            }
        }
    }

    /// Ray-march the terrain's evaluated height grid under the cursor.
    fn pick(
        &self,
        scene: &Scene,
        target: ObjectId,
        camera: &crate::camera::BlenderCamera,
        viewport: Viewport,
    ) -> Option<Vec3> {
        let object = scene.object(target)?;
        let Primitive::Terrain { size, resolution, height, seed } = object.primitive else {
            return None;
        };
        let (origin, direction) =
            camera.pick_ray(viewport, self.last_mouse.0, self.last_mouse.1);
        let world = scene.world_transform(target);
        let local_origin =
            world.inverse_transform_point(Vec3::new(origin.x, origin.y, origin.z));
        // direction: rotate back and unscale (approximate under shear-free SRT)
        let safe = |s: f32| if s.abs() < 1e-9 { 1.0 } else { s };
        let dir = Vec3::new(direction.x, direction.y, direction.z);
        let local_dir = (world.rotation.inverse() * dir)
            / Vec3::new(safe(world.scale.x), safe(world.scale.y), safe(world.scale.z));
        let grid = match &object.terrain {
            Some(data) => data.eval_grid(seed, resolution, size, height),
            None => return None,
        };
        terrain::raycast_grid(&grid, resolution, size, local_origin, local_dir)
    }

    /// One brush application at the local-space hit point.
    fn apply_dab(&mut self, scene: &mut Scene, target: ObjectId, hit: Vec3, dt: f32) {
        let Some(object) = scene.object_mut(target) else { return };
        let Primitive::Terrain { size, resolution, height, seed } = object.primitive else {
            return;
        };
        let data = object.terrain.get_or_insert_with(Default::default);

        // the sculpt grid follows the terrain's mesh resolution
        let sculpt_res = resolution.clamp(terrain::MIN_RESOLUTION, terrain::MAX_RESOLUTION);
        match &mut data.sculpt {
            Some(s) if s.resolution != sculpt_res => *s = s.resample(sculpt_res),
            Some(_) => {}
            None => data.sculpt = Some(SculptLayer::new(sculpt_res)),
        }

        let mode = self.effective_mode();
        // Smooth/Flatten read the current surface; the base is memoized so
        // this re-evaluation is just the sculpt add
        let current = match mode {
            BrushMode::Smooth | BrushMode::Flatten => {
                data.eval_grid(seed, resolution, size, height)
            }
            _ => Vec::new(),
        };
        let dt = dt.clamp(0.0, 0.1); // a hitch must not gouge the terrain
        let amount = match mode {
            // meters of push per dab
            BrushMode::Raise | BrushMode::Lower => self.strength * 12.0 * dt,
            // lerp fraction per dab
            BrushMode::Smooth | BrushMode::Flatten => (self.strength * 6.0 * dt).min(1.0),
        };
        let sculpt = data.sculpt.as_mut().expect("ensured above");
        sculpt.brush(
            mode,
            Vec2::new(hit.x, hit.y),
            self.radius,
            amount,
            self.falloff,
            size,
            &current,
            self.flatten_target,
        );
        object.sculpt_revision += 1; // renderer follows; physics waits
    }

    /// Stroke ended: let the physics mirror catch up with the new surface.
    fn finish_stroke(&mut self, scene: &mut Scene) {
        self.stroking = false;
        if let Some(target) = self.target {
            if let Some(object) = scene.object_mut(target) {
                object.mesh_revision += 1;
            }
        }
    }

    /// Brush ring + crosshair, conforming to the terrain surface. Drawn
    /// through the engine overlay pass (x-ray so it reads inside dips).
    pub fn overlay(&self, scene: &Scene, draws: &mut aether_render::passes::overlay::OverlayDraws) {
        if !self.active {
            return;
        }
        let (Some(target), Some(hit)) = (self.target, self.hover) else {
            return;
        };
        let Some(object) = scene.object(target) else { return };
        let Primitive::Terrain { size, resolution, height, seed } = object.primitive else {
            return;
        };
        let Some(data) = &object.terrain else { return };
        let grid = data.eval_grid(seed, resolution, size, height);
        let world = scene.world_transform(target);
        let color = match self.effective_mode() {
            BrushMode::Lower => aether_math::Vec4::new(0.9, 0.35, 0.2, 0.9),
            BrushMode::Smooth => aether_math::Vec4::new(0.3, 0.6, 0.95, 0.9),
            BrushMode::Flatten => aether_math::Vec4::new(0.85, 0.8, 0.25, 0.9),
            BrushMode::Raise => aether_math::Vec4::new(1.0, 1.0, 1.0, 0.9),
        };
        const SEGMENTS: usize = 48;
        let half = 0.5 * size;
        let mut prev: Option<Vec3> = None;
        for i in 0..=SEGMENTS {
            let angle = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            let x = (hit.x + self.radius * angle.cos()).clamp(-half, half);
            let y = (hit.y + self.radius * angle.sin()).clamp(-half, half);
            let z = terrain::sample_height(&grid, resolution, size, x, y) + 0.05;
            let p = world.transform_point(Vec3::new(x, y, z));
            if let Some(prev) = prev {
                draws.line_xray(
                    aether_math::Vec3::new(prev.x, prev.y, prev.z),
                    aether_math::Vec3::new(p.x, p.y, p.z),
                    color,
                );
            }
            prev = Some(p);
        }
        // center tick so the exact brush point reads on steep slopes
        let center = world.transform_point(hit + Vec3::new(0.0, 0.0, 0.05));
        let tip = world.transform_point(hit + Vec3::new(0.0, 0.0, 0.05 + self.radius * 0.15));
        draws.line_xray(
            aether_math::Vec3::new(center.x, center.y, center.z),
            aether_math::Vec3::new(tip.x, tip.y, tip.z),
            color,
        );
    }

    /// Floating tool panel (drawn while active). Returns true when "Done".
    pub fn panel(&mut self, ctx: &egui::Context, scene: &mut Scene, top_offset: f32) -> bool {
        if !self.active {
            return false;
        }
        let mut done = false;
        egui::Window::new("Sculpt")
            .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, top_offset + 34.0))
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for mode in BrushMode::ALL {
                        if ui.selectable_label(self.mode == mode, mode.label()).clicked() {
                            self.mode = mode;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Radius");
                    ui.add(
                        egui::Slider::new(&mut self.radius, 0.5..=100.0)
                            .logarithmic(true)
                            .suffix(" m"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Strength");
                    ui.add(egui::Slider::new(&mut self.strength, 0.05..=1.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Falloff");
                    ui.add(egui::Slider::new(&mut self.falloff, 0.05..=1.0));
                });
                ui.horizontal(|ui| {
                    let has_sculpt = self
                        .target
                        .and_then(|id| scene.object(id))
                        .and_then(|o| o.terrain.as_ref())
                        .and_then(|t| t.sculpt.as_ref())
                        .is_some_and(|s| !s.is_empty());
                    if ui
                        .add_enabled(has_sculpt, egui::Button::new("Clear sculpt"))
                        .on_hover_text("Remove all hand-sculpted offsets (undoable)")
                        .clicked()
                    {
                        if let Some(object) = self.target.and_then(|id| scene.object_mut(id)) {
                            if let Some(data) = &mut object.terrain {
                                data.sculpt = None;
                                object.mesh_revision += 1;
                            }
                        }
                    }
                    if ui.button("Done").clicked() {
                        done = true;
                    }
                });
                ui.label(
                    egui::RichText::new("Drag to sculpt · Ctrl inverts · Esc exits")
                        .weak()
                        .size(10.0),
                );
            });
        if done {
            self.abort(scene);
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::Transform;

    #[test]
    fn dab_bumps_sculpt_revision_and_stroke_end_bumps_mesh_revision() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::default_terrain(), Transform::default());
        let mut tool = SculptTool::new();
        tool.start(id);
        tool.stroking = true;

        let before = scene.object(id).unwrap().mesh_revision;
        tool.apply_dab(&mut scene, id, Vec3::new(0.0, 0.0, 3.0), 1.0 / 60.0);
        let object = scene.object(id).unwrap();
        assert_eq!(object.sculpt_revision, 1);
        assert_eq!(object.mesh_revision, before, "physics must not rebuild mid-stroke");
        assert!(object.terrain.as_ref().unwrap().sculpt.is_some());

        tool.finish_stroke(&mut scene);
        assert_eq!(scene.object(id).unwrap().mesh_revision, before + 1);
        assert!(!tool.stroking());
    }

    #[test]
    fn raise_dab_actually_raises_the_surface() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::default_terrain(), Transform::default());
        let Primitive::Terrain { size, resolution, height, seed } =
            scene.object(id).unwrap().primitive
        else {
            panic!("not a terrain")
        };
        let before = {
            let data = scene.object(id).unwrap().terrain.as_ref().unwrap();
            let grid = data.eval_grid(seed, resolution, size, height);
            terrain::sample_height(&grid, resolution, size, 0.0, 0.0)
        };
        let mut tool = SculptTool::new();
        tool.start(id);
        tool.stroking = true;
        for _ in 0..30 {
            tool.apply_dab(&mut scene, id, Vec3::new(0.0, 0.0, before), 1.0 / 60.0);
        }
        let after = {
            let data = scene.object(id).unwrap().terrain.as_ref().unwrap();
            let grid = data.eval_grid(seed, resolution, size, height);
            terrain::sample_height(&grid, resolution, size, 0.0, 0.0)
        };
        assert!(after > before + 1.0, "30 dabs at default strength: {before} -> {after}");
    }

    #[test]
    fn tool_deactivates_when_the_target_vanishes() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::default_terrain(), Transform::default());
        let mut tool = SculptTool::new();
        tool.start(id);
        scene.remove_object(id);
        let camera = crate::camera::BlenderCamera::new();
        tool.handle_events(
            &mut [],
            &camera,
            Viewport::new_at_origo(640, 480),
            &mut scene,
            false,
            false,
            1.0 / 60.0,
        );
        assert!(!tool.active());
    }
}
