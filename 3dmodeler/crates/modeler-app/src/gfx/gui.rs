//! The egui frame: input in, tessellated triangles out.
//!
//! `three_d::GUI` did three things — translate the app's events into egui's raw
//! input, run the interface, and paint the result — and the app leaned on all
//! three. This is the same three things, with [`super::EguiPainter`] doing the
//! painting. The signatures match what `three_d::GUI` offered, because a hundred
//! lines of `ui.rs` are written against them.
//!
//! # The `handled` contract, which the tools depend on
//!
//! After the interface has run, events egui consumed are flagged so the tools
//! behind it skip them. The rule is deliberately three-d's, down to which
//! predicate is asked:
//!
//! * pointer events are claimed when egui **is using** the pointer, meaning a
//!   drag is in progress — *not* when it merely wants it;
//! * key events are claimed when egui **wants** the keyboard, meaning a text
//!   field has focus.
//!
//! The asymmetry looks like an oversight and is not. A plain click on a widget
//! never sets `is_using_pointer`, so clicks are *not* flagged, and `main.rs`
//! compensates with its own `pointer_over_ui` test against the panel rectangles.
//! Tightening this to `wants_pointer_input` would double-suppress: the viewport
//! would stop seeing clicks that the compensation already filters, and orbiting
//! with the cursor over a panel edge would die.
//!
//! # Coordinates
//!
//! The app's positions are physical pixels from the bottom left (see
//! [`super::frame`]); egui's are logical points from the top left. Both the flip
//! and the scale happen here, in one place, and are tested.

use super::event::{Event, Key, Modifiers, MouseButton};
use super::math::Viewport;
use super::EguiPainter;

pub struct Gui {
    context: egui::Context,
    painter: EguiPainter,
    /// The interface built by [`Self::update`], waiting to be painted. Taken by
    /// [`Self::render`], so painting twice from one update is a panic rather
    /// than a silently duplicated frame.
    output: Option<egui::FullOutput>,
    viewport: Viewport,
    pixels_per_point: f32,
    /// Modifiers held as of the last event that reported any. The app's events
    /// each carry their own, but `RawInput` wants the state at the *start* of
    /// the frame, which is the last thing we were told.
    modifiers: Modifiers,
}

impl Gui {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        Self {
            context: egui::Context::default(),
            painter: EguiPainter::new(device, target_format),
            output: None,
            viewport: Viewport::new_at_origo(1, 1),
            pixels_per_point: 1.0,
            modifiers: Modifiers::default(),
        }
    }

    pub fn context(&self) -> &egui::Context {
        &self.context
    }

    /// Runs one frame of the interface.
    ///
    /// Build the panels in `callback`. Returns whether egui wants input, which
    /// is the caller's cue that a redraw is worth doing.
    pub fn update(
        &mut self,
        events: &mut [Event],
        accumulated_time_in_ms: f64,
        viewport: Viewport,
        device_pixel_ratio: f32,
        callback: impl FnMut(&egui::Context),
    ) -> bool {
        self.context.set_pixels_per_point(device_pixel_ratio);
        self.viewport = viewport;
        self.pixels_per_point = device_pixel_ratio;

        for event in events.iter() {
            if let Some(m) = modifiers_of(event) {
                self.modifiers = m;
            }
        }

        let raw_input = raw_input(
            events,
            accumulated_time_in_ms,
            viewport,
            device_pixel_ratio,
            self.modifiers,
        );

        // `run_ui` rather than the `run` that matches this callback's shape:
        // `run` is deprecated, and the panels the app builds all take a
        // `&Context` anyway — `ui.ctx()` is the one it is running under.
        let mut callback = callback;
        self.output = Some(self.context.run_ui(raw_input, |ui| callback(ui.ctx())));

        // See the module note: which predicate goes with which event kind is
        // the part that matters.
        let using_pointer = self.context.egui_is_using_pointer();
        let wants_keyboard = self.context.egui_wants_keyboard_input();
        for event in events.iter_mut() {
            match event {
                Event::MousePress { handled, .. }
                | Event::MouseRelease { handled, .. }
                | Event::MouseMotion { handled, .. }
                | Event::MouseWheel { handled, .. }
                | Event::PinchGesture { handled, .. }
                    if using_pointer =>
                {
                    *handled = true;
                }
                Event::KeyPress { handled, .. } | Event::KeyRelease { handled, .. }
                    if wants_keyboard =>
                {
                    *handled = true;
                }
                _ => {}
            }
        }

        self.context.egui_wants_pointer_input() || wants_keyboard
    }

    /// Paints the interface built by the last [`Self::update`] over `target`.
    ///
    /// Draw the 3D first: this loads the target rather than clearing it, and
    /// the interface belongs on top.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
    ) {
        let output = self.output.take().expect("Gui::update must run before Gui::render");
        let primitives = self.context.tessellate(output.shapes, self.pixels_per_point);
        // Textures first: this frame's primitives may reference an atlas that
        // only exists in the delta.
        self.painter.apply_textures(device, queue, &output.textures_delta);
        self.painter.paint(
            device,
            queue,
            encoder,
            target,
            &primitives,
            [self.viewport.width, self.viewport.height],
            self.pixels_per_point,
        );
    }
}

/// One frame of app input, as egui's raw input.
fn raw_input(
    events: &[Event],
    accumulated_time_in_ms: f64,
    viewport: Viewport,
    ppp: f32,
    modifiers: Modifiers,
) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(screen_rect(viewport, ppp)),
        time: Some(accumulated_time_in_ms * 0.001),
        modifiers: to_egui_modifiers(&modifiers),
        events: events.iter().filter_map(|e| translate_event(e, viewport, ppp)).collect(),
        ..Default::default()
    }
}

/// The window rectangle, in points.
fn screen_rect(viewport: Viewport, ppp: f32) -> egui::Rect {
    let ppp = if ppp > 0.0 { ppp } else { 1.0 };
    egui::Rect {
        min: egui::pos2(viewport.x as f32 / ppp, viewport.y as f32 / ppp),
        max: egui::pos2(
            (viewport.x as f32 + viewport.width as f32) / ppp,
            (viewport.y as f32 + viewport.height as f32) / ppp,
        ),
    }
}

/// A cursor position in the app's convention, as egui wants it.
///
/// The y flip is the whole point: the app measures up from the bottom of the
/// viewport, egui measures down from the top.
fn to_egui_pos(p: super::math::PhysicalPoint, viewport: Viewport, ppp: f32) -> egui::Pos2 {
    let ppp = if ppp > 0.0 { ppp } else { 1.0 };
    egui::pos2(p.x / ppp, (viewport.height as f32 - p.y) / ppp)
}

/// The modifiers an event reports, if it carries any.
fn modifiers_of(event: &Event) -> Option<Modifiers> {
    match event {
        Event::MousePress { modifiers, .. }
        | Event::MouseRelease { modifiers, .. }
        | Event::MouseMotion { modifiers, .. }
        | Event::MouseWheel { modifiers, .. }
        | Event::PinchGesture { modifiers, .. }
        | Event::KeyPress { modifiers, .. }
        | Event::KeyRelease { modifiers, .. } => Some(*modifiers),
        Event::Text(_) | Event::DroppedFile(_) => None,
    }
}

/// One app event as an egui event.
///
/// Already-handled events are dropped: something ahead of the interface in the
/// chain claimed them, and feeding them to egui as well is the double-handling
/// the `handled` flag exists to prevent.
fn translate_event(event: &Event, viewport: Viewport, ppp: f32) -> Option<egui::Event> {
    Some(match event {
        Event::KeyPress { kind, modifiers, handled } if !handled => egui::Event::Key {
            key: translate_key(*kind)?,
            pressed: true,
            modifiers: to_egui_modifiers(modifiers),
            repeat: false,
            physical_key: None,
        },
        Event::KeyRelease { kind, modifiers, handled } if !handled => egui::Event::Key {
            key: translate_key(*kind)?,
            pressed: false,
            modifiers: to_egui_modifiers(modifiers),
            repeat: false,
            physical_key: None,
        },
        Event::MousePress { button, position, modifiers, handled } if !handled => {
            egui::Event::PointerButton {
                pos: to_egui_pos(*position, viewport, ppp),
                button: translate_button(*button),
                pressed: true,
                modifiers: to_egui_modifiers(modifiers),
            }
        }
        Event::MouseRelease { button, position, modifiers, handled } if !handled => {
            egui::Event::PointerButton {
                pos: to_egui_pos(*position, viewport, ppp),
                button: translate_button(*button),
                pressed: false,
                modifiers: to_egui_modifiers(modifiers),
            }
        }
        Event::MouseMotion { position, handled, .. } if !handled => {
            egui::Event::PointerMoved(to_egui_pos(*position, viewport, ppp))
        }
        Event::MouseWheel { delta, modifiers, handled, .. } if !handled => {
            egui::Event::MouseWheel {
                delta: egui::vec2(delta.0, delta.1),
                unit: egui::MouseWheelUnit::Point,
                modifiers: to_egui_modifiers(modifiers),
                phase: egui::TouchPhase::Move,
            }
        }
        // A pinch is reported as a log-scale increment; egui wants the factor.
        Event::PinchGesture { delta, handled, .. } if !handled => egui::Event::Zoom(delta.exp()),
        Event::Text(text) => egui::Event::Text(text.clone()),
        _ => return None,
    })
}

fn to_egui_modifiers(m: &Modifiers) -> egui::Modifiers {
    egui::Modifiers {
        alt: m.alt,
        ctrl: m.ctrl,
        shift: m.shift,
        command: m.command,
        mac_cmd: cfg!(target_os = "macos") && m.command,
    }
}

fn translate_button(button: MouseButton) -> egui::PointerButton {
    match button {
        MouseButton::Left => egui::PointerButton::Primary,
        MouseButton::Right => egui::PointerButton::Secondary,
        MouseButton::Middle => egui::PointerButton::Middle,
    }
}

fn translate_key(key: Key) -> Option<egui::Key> {
    use egui::Key as E;
    Some(match key {
        Key::ArrowDown => E::ArrowDown,
        Key::ArrowLeft => E::ArrowLeft,
        Key::ArrowRight => E::ArrowRight,
        Key::ArrowUp => E::ArrowUp,
        Key::Escape => E::Escape,
        Key::Tab => E::Tab,
        Key::Backspace => E::Backspace,
        Key::Enter => E::Enter,
        Key::Space => E::Space,
        Key::Insert => E::Insert,
        Key::Delete => E::Delete,
        Key::Home => E::Home,
        Key::End => E::End,
        Key::PageUp => E::PageUp,
        Key::PageDown => E::PageDown,
        Key::Paste => E::Paste,
        Key::Num0 => E::Num0,
        Key::Num1 => E::Num1,
        Key::Num2 => E::Num2,
        Key::Num3 => E::Num3,
        Key::Num4 => E::Num4,
        Key::Num5 => E::Num5,
        Key::Num6 => E::Num6,
        Key::Num7 => E::Num7,
        Key::Num8 => E::Num8,
        Key::Num9 => E::Num9,
        Key::A => E::A,
        Key::B => E::B,
        Key::C => E::C,
        Key::D => E::D,
        Key::E => E::E,
        Key::F => E::F,
        Key::G => E::G,
        Key::H => E::H,
        Key::I => E::I,
        Key::J => E::J,
        Key::K => E::K,
        Key::L => E::L,
        Key::M => E::M,
        Key::N => E::N,
        Key::O => E::O,
        Key::P => E::P,
        Key::Q => E::Q,
        Key::R => E::R,
        Key::S => E::S,
        Key::T => E::T,
        Key::U => E::U,
        Key::V => E::V,
        Key::W => E::W,
        Key::X => E::X,
        Key::Y => E::Y,
        Key::Z => E::Z,
        Key::F1 => E::F1,
        Key::F2 => E::F2,
        Key::F3 => E::F3,
        Key::F4 => E::F4,
        Key::F5 => E::F5,
        Key::F6 => E::F6,
        Key::F7 => E::F7,
        Key::F8 => E::F8,
        Key::F9 => E::F9,
        Key::F10 => E::F10,
        Key::F11 => E::F11,
        Key::F12 => E::F12,
        Key::Period => E::Period,
        Key::Comma => E::Comma,
        Key::Minus => E::Minus,
        Key::Equals => E::Equals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::math::PhysicalPoint;

    const VIEW: Viewport = Viewport { x: 0, y: 0, width: 800, height: 600 };

    fn press_at(x: f32, y: f32, handled: bool) -> Event {
        Event::MousePress {
            button: MouseButton::Left,
            position: PhysicalPoint { x, y },
            modifiers: Modifiers::default(),
            handled,
        }
    }

    #[test]
    fn the_origin_flips_from_bottom_left_to_top_left() {
        // The app's (0, 0) is the bottom-left corner; egui's is the top-left.
        let bottom = to_egui_pos(PhysicalPoint { x: 0.0, y: 0.0 }, VIEW, 1.0);
        assert_eq!(bottom, egui::pos2(0.0, 600.0));
        let top = to_egui_pos(PhysicalPoint { x: 0.0, y: 600.0 }, VIEW, 1.0);
        assert_eq!(top, egui::pos2(0.0, 0.0));
    }

    #[test]
    fn positions_come_back_to_points() {
        // On a 2x display the menu bar is 60 physical pixels down from a
        // 600-pixel top edge, which is 30 points down in egui's coordinates.
        let p = to_egui_pos(PhysicalPoint { x: 100.0, y: 540.0 }, VIEW, 2.0);
        assert_eq!(p, egui::pos2(50.0, 30.0));
    }

    #[test]
    fn the_screen_rect_is_the_window_in_points() {
        let r = screen_rect(VIEW, 2.0);
        assert_eq!(r.min, egui::pos2(0.0, 0.0));
        assert_eq!(r.max, egui::pos2(400.0, 300.0));
    }

    #[test]
    fn a_zero_scale_factor_does_not_produce_infinities() {
        // A scale factor of zero has been seen from winit while a window is
        // being created; an infinite screen rect poisons every layout in the
        // frame that follows.
        assert!(screen_rect(VIEW, 0.0).max.x.is_finite());
        assert!(to_egui_pos(PhysicalPoint { x: 1.0, y: 1.0 }, VIEW, 0.0).x.is_finite());
    }

    #[test]
    fn an_already_handled_event_is_not_forwarded_to_egui() {
        // Tab is claimed for edit mode before the interface runs. Forwarding it
        // anyway would also walk egui's widget focus.
        assert!(translate_event(&press_at(10.0, 10.0, true), VIEW, 1.0).is_none());
        assert!(translate_event(&press_at(10.0, 10.0, false), VIEW, 1.0).is_some());
    }

    #[test]
    fn text_is_forwarded_even_though_it_carries_no_handled_flag() {
        // Typing into a field is the one input that has no contest to lose.
        assert!(matches!(
            translate_event(&Event::Text("a".into()), VIEW, 1.0),
            Some(egui::Event::Text(_))
        ));
    }

    #[test]
    fn a_pinch_becomes_a_zoom_factor_not_an_increment() {
        // egui multiplies by what it is given: a delta of 0 must mean "no
        // change", which is a factor of one, not zero.
        let Some(egui::Event::Zoom(factor)) = translate_event(
            &Event::PinchGesture {
                delta: 0.0,
                position: PhysicalPoint::default(),
                modifiers: Modifiers::default(),
                handled: false,
            },
            VIEW,
            1.0,
        ) else {
            panic!("a pinch did not become a zoom");
        };
        assert_eq!(factor, 1.0);
    }

    #[test]
    fn the_numpad_and_row_digits_reach_egui_as_digits() {
        assert_eq!(translate_key(Key::Num7), Some(egui::Key::Num7));
    }

    /// Is a widget occupying the window's top-left corner hovered when the
    /// pointer is reported at `position`?
    ///
    /// This runs the real conversion into a real `egui::Context`, which is the
    /// only way to catch a coordinate convention that is self-consistent and
    /// still wrong — every function here would pass its own test while the
    /// interface ignored the mouse.
    fn hovers_the_corner_widget(position: PhysicalPoint, viewport: Viewport, ppp: f32) -> bool {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(ppp);
        let mut hovered = false;
        // Three passes, not one: egui only knows where a widget is once it has
        // laid it out, and `add_sized` does not reach its final rectangle until
        // the pass after that. A hover can only be reported on the pass after
        // the rectangle is right.
        for pass in 0..3 {
            let events = [Event::MouseMotion {
                button: None,
                delta: (0.0, 0.0),
                position,
                modifiers: Modifiers::default(),
                handled: false,
            }];
            let input =
                raw_input(&events, pass as f64 * 16.0, viewport, ppp, Modifiers::default());
            // The output is the tessellated interface, which this test does not
            // paint — only the interaction it reports matters here.
            let _ = ctx.run_ui(input, |ui| {
                egui::Area::new("corner".into())
                    .fixed_pos(egui::pos2(0.0, 0.0))
                    .show(ui.ctx(), |ui| {
                        hovered =
                            ui.add_sized([100.0, 40.0], egui::Button::new("x")).hovered();
                    });
            });
        }
        hovered
    }

    #[test]
    fn the_pointer_reaches_the_widget_the_user_is_pointing_at() {
        let viewport = Viewport::new_at_origo(800, 600);
        // The menu bar is at the TOP of the window, which in the app's
        // bottom-left convention is a high y.
        assert!(
            hovers_the_corner_widget(PhysicalPoint { x: 20.0, y: 580.0 }, viewport, 1.0),
            "the pointer near the top of the window must reach the menu bar"
        );
        // And the low y — where an unflipped reading would put the same
        // pointer — must not, or every click lands on the wrong half of the UI.
        assert!(!hovers_the_corner_widget(PhysicalPoint { x: 20.0, y: 20.0 }, viewport, 1.0));
    }

    #[test]
    fn the_pointer_reaches_it_on_a_hidpi_display_too() {
        // 2x: the same widget is 100x40 points but 200x80 pixels, and the
        // pointer arrives in pixels.
        let viewport = Viewport::new_at_origo(1600, 1200);
        assert!(hovers_the_corner_widget(PhysicalPoint { x: 40.0, y: 1160.0 }, viewport, 2.0));
        // 150 points from the left is past the widget's 100-point width.
        assert!(!hovers_the_corner_widget(PhysicalPoint { x: 300.0, y: 1160.0 }, viewport, 2.0));
    }
}
