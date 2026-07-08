//! Blender-style viewport camera: turntable orbit, pan, zoom, numpad views.
//!
//! Coordinate convention is Blender's: right-handed, Z up, ground plane XY.
//! Front view (numpad 1) looks from -Y toward +Y.

use three_d::*;

pub const FOV_DEG: f32 = 45.0;
const ORBIT_SENSITIVITY: f32 = 0.008; // radians per logical pixel
const WHEEL_ZOOM_FACTOR: f32 = 0.86; // per wheel notch (24 delta units)

pub struct BlenderCamera {
    pub pivot: Vec3,
    /// Radians around world Z. 0 = front view (camera on -Y).
    pub yaw: f32,
    /// Radians above the horizon. +PI/2 = top view.
    pub pitch: f32,
    pub distance: f32,
    pub ortho: bool,
    /// True when ortho was switched on automatically by an axis view preset;
    /// orbiting away then returns to perspective (Blender's auto-perspective).
    ortho_is_auto: bool,
}

impl BlenderCamera {
    pub fn new() -> Self {
        // Similar to Blender's startup view: slightly right of front, above.
        Self {
            pivot: vec3(0.0, 0.0, 0.0),
            yaw: 0.6,
            pitch: 0.5,
            distance: 12.0,
            ortho: false,
            ortho_is_auto: false,
        }
    }

    /// Unit vector from pivot toward the camera, and the camera up vector.
    /// The up formula is the pitch-derivative of the direction, which stays
    /// well-defined at the poles (exact top/bottom views).
    pub fn direction_up(&self) -> (Vec3, Vec3) {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        let dir = vec3(sy * cp, -cy * cp, sp);
        let up = vec3(-sy * sp, cy * sp, cp);
        (dir, up)
    }


    pub fn position(&self) -> Vec3 {
        let (dir, _) = self.direction_up();
        self.pivot + dir * self.distance
    }

    /// World-space ray through a viewport pixel. three-d event positions are
    /// physical pixels with a BOTTOM-left origin (see its egui conversion).
    /// Returns (origin, direction).
    pub fn pick_ray(&self, viewport: Viewport, x_px: f32, y_px: f32) -> (Vec3, Vec3) {
        let w = viewport.width as f32;
        let h = viewport.height as f32;
        let ndx = 2.0 * x_px / w.max(1.0) - 1.0;
        let ndy = 2.0 * y_px / h.max(1.0) - 1.0;
        let (right, up, forward) = self.screen_basis();
        let tan_half = (0.5 * FOV_DEG.to_radians()).tan();
        let aspect = w / h.max(1.0);

        if self.ortho {
            let half_h = self.distance * tan_half;
            let half_w = half_h * aspect;
            let origin = self.position() + right * (ndx * half_w) + up * (ndy * half_h)
                - forward * (10.0 * self.distance); // start well behind the pivot
            (origin, forward)
        } else {
            let dir = (forward + right * (ndx * tan_half * aspect) + up * (ndy * tan_half)).normalize();
            (self.position(), dir)
        }
    }

    /// Like `world_to_screen` but returns None for points behind the camera
    /// (used by overlays, which must not draw mirrored labels).
    pub fn project(&self, viewport: Viewport, p: Vec3) -> Option<(f32, f32)> {
        if !self.ortho {
            let (_, _, forward) = self.screen_basis();
            if (p - self.position()).dot(forward) < 1e-3 {
                return None;
            }
        }
        Some(self.world_to_screen(viewport, p))
    }

    /// Project a world point to viewport pixels (physical, bottom-left origin
    /// — the same space as three-d mouse event positions). Falls back to the
    /// viewport center for points behind the camera.
    pub fn world_to_screen(&self, viewport: Viewport, p: Vec3) -> (f32, f32) {
        let w = viewport.width as f32;
        let h = viewport.height as f32;
        let (right, up, forward) = self.screen_basis();
        let rel = p - self.position();
        let x = rel.dot(right);
        let y = rel.dot(up);
        let z = rel.dot(forward);
        let tan_half = (0.5 * FOV_DEG.to_radians()).tan();
        let aspect = w / h.max(1.0);

        let (ndx, ndy) = if self.ortho {
            let half_h = self.distance * tan_half;
            (x / (half_h * aspect), y / half_h)
        } else {
            if z < 1e-4 {
                return (0.5 * w, 0.5 * h);
            }
            (x / (z * tan_half * aspect), y / (z * tan_half))
        };
        ((ndx + 1.0) * 0.5 * w, (ndy + 1.0) * 0.5 * h)
    }

    /// World units per physical pixel at the given depth along the view
    /// direction (or anywhere, when orthographic).
    pub fn world_per_pixel_at(&self, viewport: Viewport, world_point: Vec3) -> f32 {
        let h = viewport.height as f32;
        let tan_half = (0.5 * FOV_DEG.to_radians()).tan();
        if self.ortho {
            2.0 * self.distance * tan_half / h.max(1.0)
        } else {
            let (_, _, forward) = self.screen_basis();
            let z = (world_point - self.position()).dot(forward).max(0.01);
            2.0 * z * tan_half / h.max(1.0)
        }
    }

    /// Screen-space basis in world coordinates: (right, up, forward).
    pub fn screen_basis(&self) -> (Vec3, Vec3, Vec3) {
        let (dir, up) = self.direction_up();
        let forward = -dir; // direction the camera looks
        let right = forward.cross(up).normalize();
        (right, up, forward)
    }

    pub fn camera(&self, viewport: Viewport) -> Camera {
        let (dir, up) = self.direction_up();
        let position = self.pivot + dir * self.distance;
        let near = (0.002 * self.distance).max(0.01);
        let far = 100.0 * self.distance.max(1.0) + 100.0;
        if self.ortho {
            // three-d scales the ortho height by the camera-target distance
            // internally, so pass height per unit distance. This matches the
            // perspective framing at the pivot exactly.
            let height = 2.0 * (0.5 * FOV_DEG.to_radians()).tan();
            Camera::new_orthographic(viewport, position, self.pivot, up, height, near, far)
        } else {
            Camera::new_perspective(viewport, position, self.pivot, up, degrees(FOV_DEG), near, far)
        }
    }

    /// World units per logical pixel at the pivot depth.
    fn world_per_pixel(&self, logical_viewport_height: f32) -> f32 {
        2.0 * self.distance * (0.5 * FOV_DEG.to_radians()).tan() / logical_viewport_height.max(1.0)
    }

    fn orbit(&mut self, dx: f32, dy: f32) {
        // Blender direction: the scene follows the mouse (like spinning a
        // globe), so the camera orbits opposite to the drag.
        self.yaw -= dx * ORBIT_SENSITIVITY;
        self.pitch = (self.pitch + dy * ORBIT_SENSITIVITY)
            .clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
        if self.ortho && self.ortho_is_auto {
            // Blender auto-perspective: leaving an axis view restores perspective
            self.ortho = false;
            self.ortho_is_auto = false;
        }
    }

    fn pan(&mut self, dx: f32, dy: f32, logical_viewport_height: f32) {
        let wpp = self.world_per_pixel(logical_viewport_height);
        let (right, up, _) = self.screen_basis();
        self.pivot += (-right * dx + up * dy) * wpp;
    }

    fn zoom_by_factor(&mut self, factor: f32) {
        self.distance = (self.distance * factor).clamp(0.05, 10_000.0);
    }

    pub fn set_view(&mut self, yaw_deg: f32, pitch_deg: f32) {
        self.yaw = yaw_deg.to_radians();
        self.pitch = pitch_deg.to_radians();
        // Blender switches to orthographic for axis-aligned views
        if !self.ortho {
            self.ortho = true;
            self.ortho_is_auto = true;
        }
    }

    pub fn toggle_ortho(&mut self) {
        self.ortho = !self.ortho;
        self.ortho_is_auto = false;
    }

    /// Fit the view to a bounding sphere.
    pub fn frame(&mut self, center: Vec3, radius: f32) {
        self.pivot = center;
        self.distance = (radius.max(0.1) / (0.5 * FOV_DEG.to_radians()).tan()) * 1.15;
    }

    pub fn view_name(&self) -> String {
        let yaw = self.yaw.to_degrees().rem_euclid(360.0);
        let pitch = self.pitch.to_degrees();
        let eps = 0.5;
        let direction = if (pitch - 90.0).abs() < eps && yaw < eps {
            "Top"
        } else if (pitch + 90.0).abs() < eps && yaw < eps {
            "Bottom"
        } else if pitch.abs() < eps {
            match yaw {
                y if y < eps || y > 360.0 - eps => "Front",
                y if (y - 90.0).abs() < eps => "Right",
                y if (y - 180.0).abs() < eps => "Back",
                y if (y - 270.0).abs() < eps => "Left",
                _ => "User",
            }
        } else {
            "User"
        };
        let projection = if self.ortho { "Orthographic" } else { "Perspective" };
        format!("{direction} {projection}")
    }

    /// Consume viewport navigation events. Events already handled (e.g. by
    /// egui) are skipped; events we use are marked handled. Wheel/pinch zoom
    /// is skipped while the pointer is over UI chrome (three-d does not flag
    /// wheel events over egui panels as handled, so scrolling the outliner
    /// would also zoom the canvas).
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        logical_viewport_height: f32,
        pointer_over_ui: bool,
    ) {
        for event in events.iter_mut() {
            match event {
                Event::MouseMotion {
                    button,
                    delta,
                    modifiers,
                    handled,
                    ..
                } if !*handled => {
                    // MMB only: Alt+LMB conflicts with window-manager /
                    // browser menus on Linux (Blender has the same problem).
                    let nav_drag = *button == Some(MouseButton::Middle);
                    if nav_drag {
                        if modifiers.shift {
                            self.pan(delta.0, delta.1, logical_viewport_height);
                        } else if modifiers.ctrl {
                            self.zoom_by_factor((delta.1 * 0.005).exp());
                        } else {
                            self.orbit(delta.0, delta.1);
                        }
                        *handled = true;
                    }
                }
                Event::MouseWheel { delta, handled, .. } if !*handled && !pointer_over_ui => {
                    let notches = delta.1 / 24.0;
                    self.zoom_by_factor(WHEEL_ZOOM_FACTOR.powf(notches));
                    *handled = true;
                }
                Event::PinchGesture { delta, handled, .. } if !*handled && !pointer_over_ui => {
                    self.zoom_by_factor(1.0 - *delta);
                    *handled = true;
                }
                Event::KeyPress {
                    kind,
                    modifiers,
                    handled,
                    ..
                } if !*handled => {
                    let ctrl = modifiers.ctrl;
                    let mut used = true;
                    match kind {
                        Key::Num1 => self.set_view(if ctrl { 180.0 } else { 0.0 }, 0.0),
                        Key::Num3 => self.set_view(if ctrl { -90.0 } else { 90.0 }, 0.0),
                        Key::Num7 => self.set_view(0.0, if ctrl { -90.0 } else { 90.0 }),
                        Key::Num5 => self.toggle_ortho(),
                        // Blender: numpad 4/6/8/2 rotate the view in 15° steps
                        Key::Num4 => self.yaw -= 15f32.to_radians(),
                        Key::Num6 => self.yaw += 15f32.to_radians(),
                        Key::Num8 => {
                            self.pitch = (self.pitch + 15f32.to_radians())
                                .clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2)
                        }
                        Key::Num2 => {
                            self.pitch = (self.pitch - 15f32.to_radians())
                                .clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2)
                        }
                        _ => used = false,
                    }
                    if used {
                        *handled = true;
                    }
                }
                _ => {}
            }
        }
    }
}
