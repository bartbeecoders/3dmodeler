//! Physics-mode poke tool.
//!
//! While the simulation is PLAYING (Space), the left mouse button charges a
//! kick: press and hold to charge — the longer the hold, the stronger — and
//! release to apply the impulse to the dynamic object under the cursor, at
//! the exact surface point the ray hits. A ring at the cursor shows the
//! charge; its color runs from the theme accent to the error color at full
//! power. The press is consumed so simulation clicks never change the
//! selection.

use crate::camera::BlenderCamera;
use crate::physics::{PhysicsMirror, SimState};
use modeler_core::glam::Vec3;
use three_d::egui;
use three_d::{Event, MouseButton, Viewport};

/// Velocity change at the hit point (m/s): MIN at a tap, MAX at 100%.
const MIN_SPEED: f32 = 2.0;
const MAX_SPEED: f32 = 15.0;
/// Seconds of holding to reach 100%.
const CHARGE_TIME: f32 = 1.2;
/// Keep charging past 100%, up to 300% (three times the 100% strength).
const MAX_CHARGE: f32 = 3.0;

pub struct PokeTool {
    /// Seconds the button has been held; None = not charging.
    charge: Option<f32>,
}

impl PokeTool {
    pub fn new() -> Self {
        Self { charge: None }
    }

    /// Advance the charge; call once per frame (dt in seconds, wasm-safe).
    pub fn update(&mut self, dt: f32, physics: &PhysicsMirror) {
        if physics.sim_state() != SimState::Playing {
            self.charge = None; // pause/stop cancels a pending kick
            return;
        }
        if let Some(t) = &mut self.charge {
            *t += dt.clamp(0.0, 0.1);
        }
    }

    /// Charge on LMB press, kick on release. Returns a status-bar message
    /// when a kick lands.
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        physics: &mut PhysicsMirror,
        camera: &BlenderCamera,
        viewport: Viewport,
        pointer_over_ui: bool,
    ) -> Option<String> {
        if physics.sim_state() != SimState::Playing {
            self.charge = None;
            return None;
        }
        let mut status = None;
        for event in events.iter_mut() {
            match event {
                Event::MousePress {
                    button: MouseButton::Left,
                    handled,
                    ..
                } if !*handled && !pointer_over_ui && self.charge.is_none() => {
                    self.charge = Some(0.0);
                    *handled = true; // never falls through to selection
                }
                Event::MouseRelease {
                    button: MouseButton::Left,
                    position,
                    handled,
                    ..
                } if self.charge.is_some() => {
                    let held = self.charge.take().unwrap_or(0.0);
                    let strength = (held / CHARGE_TIME).clamp(0.0, MAX_CHARGE);
                    let speed = MIN_SPEED + (MAX_SPEED - MIN_SPEED) * strength;
                    let (origin, dir) =
                        camera.pick_ray(viewport, position.x, position.y);
                    let origin = Vec3::new(origin.x, origin.y, origin.z);
                    let dir = Vec3::new(dir.x, dir.y, dir.z);
                    if physics.poke(origin, dir, speed).is_some() {
                        status =
                            Some(format!("poked at {:.0}% power", strength * 100.0));
                    }
                    *handled = true;
                }
                _ => {}
            }
        }
        status
    }

    /// Charge ring at the cursor (drawn from the egui pass).
    pub fn draw(&self, ctx: &egui::Context) {
        let Some(held) = self.charge else { return };
        let Some(pos) = ctx.pointer_hover_pos().or_else(|| ctx.pointer_latest_pos())
        else {
            return;
        };
        let t = (held / CHARGE_TIME).clamp(0.0, MAX_CHARGE);
        let visuals = ctx.global_style().visuals.clone();
        let color = lerp_color(
            visuals.hyperlink_color,
            visuals.error_fg_color,
            t / MAX_CHARGE,
        );

        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("poke-charge"),
        ));
        let radius = 10.0 + 12.0 * t;
        painter.circle_stroke(pos, radius, egui::Stroke::new(3.0, color));
        painter.circle_filled(pos, 3.0, color);
        painter.text(
            pos + egui::vec2(radius + 8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            format!("{:.0}%", t * 100.0),
            egui::FontId::proportional(12.0),
            color,
        );
        ctx.request_repaint(); // keep the ring growing while held
    }
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    egui::Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}
