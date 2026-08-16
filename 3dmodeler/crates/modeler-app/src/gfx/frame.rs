//! What one frame is handed, what it hands back, and the winit translation
//! that produces the former.
//!
//! The app is written as a single closure taking a [`FrameInput`] and returning
//! a [`FrameOutput`] — that shape came from three-d and is worth keeping, so
//! both types mirror three-d's field for field.
//!
//! [`FrameInputGenerator`] is the part that had to be rewritten rather than
//! shimmed, because winit 0.28 (which three-d pins) and winit 0.30 (which wgpu
//! 26 requires, for `raw-window-handle` 0.6) disagree about keyboard input:
//! `VirtualKeyCode` became the physical/logical `KeyCode` split, and
//! `ReceivedCharacter` is gone — typed text now rides on the key event itself.
//!
//! Two conventions in here are load-bearing and neither is obvious. Both are
//! three-d's, kept because the app's tools, camera and overlays are all written
//! against them, and both have a test.
//!
//! **Mouse positions are physical pixels with a bottom-left origin.** winit
//! reports logical pixels from the top left. Everything that consumes a
//! position — picking, gizmo hit tests, the overlay — assumes the flip has
//! already happened.
//!
//! **Motion deltas are logical pixels and are *not* flipped.** They stay
//! top-down positive, which is the opposite handedness to the position in the
//! same event. It looks like an oversight and is not: the orbit and pan
//! sensitivities in `camera.rs` are tuned against it, so "fixing" it inverts
//! vertical dragging everywhere.

use super::event::{Event, Key, Modifiers, MouseButton};
use super::math::Viewport;

/// Everything one frame of the app gets to look at.
#[derive(Clone, Debug)]
pub struct FrameInput {
    /// Input since the last frame. Consumers claim events by setting `handled`
    /// — see [`super::event`].
    pub events: Vec<Event>,
    /// Milliseconds since the previous frame.
    pub elapsed_time: f64,
    /// Milliseconds since the app started.
    pub accumulated_time: f64,
    /// The window's drawable area, in physical pixels.
    pub viewport: Viewport,
    /// Window width in logical pixels.
    pub window_width: u32,
    /// Window height in logical pixels.
    pub window_height: u32,
    /// Physical pixels per logical pixel.
    pub device_pixel_ratio: f32,
    /// True on the first frame, and again after the window is un-occluded —
    /// caches keyed on "have I drawn yet" must treat both the same.
    pub first_frame: bool,
}

/// What the frame asks the event loop to do next.
#[derive(Clone, Debug)]
pub struct FrameOutput {
    /// Close the window and stop the loop.
    pub exit: bool,
    /// Present what was drawn. False reuses the previous image.
    pub swap_buffers: bool,
    /// Sleep until the next input event instead of drawing continuously.
    pub wait_next_event: bool,
    /// Retitle the OS window (e.g. "Simulating — …"). None = leave as is.
    pub title: Option<String>,
}

impl Default for FrameOutput {
    fn default() -> Self {
        Self { exit: false, swap_buffers: true, wait_next_event: false, title: None }
    }
}

/// Everything a frame needs to put pixels on the screen.
///
/// Under three-d the frame *was* the target — `frame_input.screen()` handed
/// back something with `clear` and `render` on it. wgpu splits that into a
/// device, a queue, an encoder to record into and a view to record against, and
/// they have to be acquired and submitted around the frame rather than inside
/// it. This is those four, so the frame body still reads as one place where the
/// drawing happens.
pub struct FramePaint<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    /// Commands recorded here are submitted after the frame returns.
    pub encoder: &'a mut wgpu::CommandEncoder,
    /// The swapchain image this frame is drawn into.
    pub view: &'a wgpu::TextureView,
}

/// Turns winit window events into [`FrameInput`].
///
/// Feed it every `WindowEvent` with [`Self::handle_winit_window_event`], then
/// call [`Self::generate`] once per frame to drain them.
#[cfg(not(target_arch = "wasm32"))]
pub use native::FrameInputGenerator;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use std::time::Instant;
    use winit::event::WindowEvent;

    /// A cursor position as winit reports it: logical pixels from the top left.
    ///
    /// It is kept in this form because the *delta* between two of them is what
    /// the app wants unflipped and unscaled; the flip to the app's convention
    /// happens only in the conversion to `PhysicalPoint`.
    #[derive(Clone, Copy, Debug)]
    struct LogicalPoint {
        x: f32,
        y: f32,
        device_pixel_ratio: f32,
        /// Viewport height in physical pixels — the mirror line for the flip.
        height: f32,
    }

    impl From<LogicalPoint> for crate::gfx::math::PhysicalPoint {
        fn from(p: LogicalPoint) -> Self {
            Self {
                x: p.x * p.device_pixel_ratio,
                y: p.height - p.y * p.device_pixel_ratio,
            }
        }
    }

    pub struct FrameInputGenerator {
        last_time: Instant,
        first_frame: bool,
        events: Vec<Event>,
        accumulated_time: f64,
        viewport: Viewport,
        window_width: u32,
        window_height: u32,
        device_pixel_ratio: f64,
        cursor_pos: Option<LogicalPoint>,
        modifiers: Modifiers,
        mouse_pressed: Option<MouseButton>,
    }

    impl FrameInputGenerator {
        pub fn from_winit_window(window: &winit::window::Window) -> Self {
            Self::new(window.inner_size(), window.scale_factor())
        }

        fn new(size: winit::dpi::PhysicalSize<u32>, device_pixel_ratio: f64) -> Self {
            let (window_width, window_height): (u32, u32) =
                size.to_logical::<f32>(device_pixel_ratio).into();
            Self {
                last_time: Instant::now(),
                first_frame: true,
                events: Vec::new(),
                accumulated_time: 0.0,
                viewport: Viewport::new_at_origo(size.width, size.height),
                window_width,
                window_height,
                device_pixel_ratio,
                cursor_pos: None,
                modifiers: Modifiers::default(),
                mouse_pressed: None,
            }
        }

        /// Drain a frame's worth of input.
        pub fn generate(&mut self) -> FrameInput {
            let now = Instant::now();
            let elapsed_time = now.duration_since(self.last_time).as_secs_f64() * 1000.0;
            self.last_time = now;
            self.accumulated_time += elapsed_time;

            let input = FrameInput {
                events: std::mem::take(&mut self.events),
                elapsed_time,
                accumulated_time: self.accumulated_time,
                viewport: self.viewport,
                window_width: self.window_width,
                window_height: self.window_height,
                device_pixel_ratio: self.device_pixel_ratio as f32,
                first_frame: self.first_frame,
            };
            self.first_frame = false;
            input
        }

        pub fn handle_winit_window_event(&mut self, event: &WindowEvent) {
            match event {
                WindowEvent::Resized(size) => self.resize(*size),
                WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                    // winit 0.28 handed the new size over here; 0.30 passes an
                    // `InnerSizeWriter` instead and, left alone, resizes to the
                    // size the OS suggests — which arrives as the `Resized`
                    // below. So only the ratio is taken here and the viewport
                    // waits for that event rather than being guessed at twice.
                    self.device_pixel_ratio = *scale_factor;
                }
                // The window becoming visible again invalidates anything the
                // app cached on "I have drawn a frame".
                WindowEvent::Occluded(false) => self.first_frame = true,

                WindowEvent::ModifiersChanged(modifiers) => {
                    let state = modifiers.state();
                    self.modifiers = Modifiers {
                        alt: state.alt_key(),
                        ctrl: state.control_key(),
                        shift: state.shift_key(),
                        command: state.super_key(),
                    };
                }

                WindowEvent::KeyboardInput { event, .. } => self.key(event),

                WindowEvent::CursorMoved { position, .. } => {
                    let p = position.to_logical::<f32>(self.device_pixel_ratio);
                    // Logical, top-down — see the module note on why this is
                    // not flipped to match the position beside it.
                    let delta = match self.cursor_pos {
                        Some(last) => (p.x - last.x, p.y - last.y),
                        None => (0.0, 0.0),
                    };
                    let position = LogicalPoint {
                        x: p.x,
                        y: p.y,
                        device_pixel_ratio: self.device_pixel_ratio as f32,
                        height: self.viewport.height as f32,
                    };
                    self.events.push(Event::MouseMotion {
                        button: self.mouse_pressed,
                        delta,
                        position: position.into(),
                        modifiers: self.modifiers,
                        handled: false,
                    });
                    self.cursor_pos = Some(position);
                }

                WindowEvent::MouseInput { state, button, .. } => {
                    let (Some(position), Some(button)) =
                        (self.cursor_pos, translate_mouse_button(*button))
                    else {
                        return;
                    };
                    if *state == winit::event::ElementState::Pressed {
                        self.mouse_pressed = Some(button);
                        self.events.push(Event::MousePress {
                            button,
                            position: position.into(),
                            modifiers: self.modifiers,
                            handled: false,
                        });
                    } else {
                        self.mouse_pressed = None;
                        self.events.push(Event::MouseRelease {
                            button,
                            position: position.into(),
                            modifiers: self.modifiers,
                            handled: false,
                        });
                    }
                }

                WindowEvent::MouseWheel { delta, .. } => {
                    let Some(position) = self.cursor_pos else { return };
                    // One wheel notch must mean the same thing whether the OS
                    // reports lines or pixels; these two constants are three-d's
                    // experimentally-derived normalisation, and the zoom step in
                    // camera.rs is calibrated to the 24-per-notch it produces.
                    const LINE_HEIGHT: f64 = 24.0;
                    const BROWSER_LINE_HEIGHT: f64 = 100.0;
                    let (x, y) = match delta {
                        winit::event::MouseScrollDelta::LineDelta(x, y) => {
                            (*x as f64 * LINE_HEIGHT, *y as f64 * LINE_HEIGHT)
                        }
                        winit::event::MouseScrollDelta::PixelDelta(d) => {
                            let d = d.to_logical::<f64>(self.device_pixel_ratio);
                            (
                                d.x * LINE_HEIGHT / BROWSER_LINE_HEIGHT,
                                d.y * LINE_HEIGHT / BROWSER_LINE_HEIGHT,
                            )
                        }
                    };
                    self.events.push(Event::MouseWheel {
                        delta: (x as f32, y as f32),
                        position: position.into(),
                        modifiers: self.modifiers,
                        handled: false,
                    });
                }

                WindowEvent::PinchGesture { delta, .. } => {
                    let Some(position) = self.cursor_pos else { return };
                    self.events.push(Event::PinchGesture {
                        delta: *delta as f32,
                        position: position.into(),
                        modifiers: self.modifiers,
                        handled: false,
                    });
                }

                WindowEvent::DroppedFile(path) => {
                    self.events.push(Event::DroppedFile(path.clone()));
                }

                _ => {}
            }
        }

        fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
            self.viewport = Viewport::new_at_origo(size.width, size.height);
            let logical = size.to_logical::<f32>(self.device_pixel_ratio);
            self.window_width = logical.width as u32;
            self.window_height = logical.height as u32;
        }

        fn key(&mut self, event: &winit::event::KeyEvent) {
            let pressed = event.state == winit::event::ElementState::Pressed;

            if let winit::keyboard::PhysicalKey::Code(code) = event.physical_key {
                if let Some(kind) = translate_key_code(code) {
                    self.events.push(if pressed {
                        Event::KeyPress { kind, modifiers: self.modifiers, handled: false }
                    } else {
                        Event::KeyRelease { kind, modifiers: self.modifiers, handled: false }
                    });
                }
            }

            // Typed text, which winit 0.30 delivers here instead of through the
            // removed `ReceivedCharacter`. This is what the letter shortcuts
            // match on, so that they follow the user's layout rather than the
            // physical key position — see `Event::Text`.
            //
            // Ctrl and Cmd are excluded because a chord is a command, not
            // typing: without this, Ctrl+S both saves and types an "s" into
            // whichever field has focus.
            if pressed && !self.modifiers.ctrl && !self.modifiers.command {
                if let Some(text) = &event.text {
                    if text.chars().any(is_printable_char) {
                        self.events.push(Event::Text(text.to_string()));
                    }
                }
            }
        }
    }

    /// Excludes the C0/C1 control ranges and the private-use area, where
    /// several platforms park their non-printing keys.
    fn is_printable_char(chr: char) -> bool {
        let is_in_private_use_area = ('\u{e000}'..='\u{f8ff}').contains(&chr)
            || ('\u{f0000}'..='\u{ffffd}').contains(&chr)
            || ('\u{100000}'..='\u{10fffd}').contains(&chr);
        !is_in_private_use_area && !chr.is_ascii_control()
    }

    fn translate_mouse_button(
        button: winit::event::MouseButton,
    ) -> Option<MouseButton> {
        match button {
            winit::event::MouseButton::Left => Some(MouseButton::Left),
            winit::event::MouseButton::Middle => Some(MouseButton::Middle),
            winit::event::MouseButton::Right => Some(MouseButton::Right),
            _ => None,
        }
    }

    /// Physical key positions the app binds. Anything else arrives as
    /// [`Event::Text`] instead.
    fn translate_key_code(code: winit::keyboard::KeyCode) -> Option<Key> {
        use winit::keyboard::KeyCode as C;
        Some(match code {
            C::ArrowDown => Key::ArrowDown,
            C::ArrowLeft => Key::ArrowLeft,
            C::ArrowRight => Key::ArrowRight,
            C::ArrowUp => Key::ArrowUp,

            C::Escape => Key::Escape,
            C::Tab => Key::Tab,
            C::Backspace => Key::Backspace,
            C::Enter | C::NumpadEnter => Key::Enter,
            C::Space => Key::Space,

            C::Insert => Key::Insert,
            C::Delete => Key::Delete,
            C::Home => Key::Home,
            C::End => Key::End,
            C::PageUp => Key::PageUp,
            C::PageDown => Key::PageDown,
            C::Paste => Key::Paste,

            C::Equal | C::NumpadEqual => Key::Equals,
            C::Minus | C::NumpadSubtract => Key::Minus,
            C::Period | C::NumpadDecimal => Key::Period,
            C::Comma => Key::Comma,

            // The numpad digits are the Blender axis views, so the main row
            // and the numpad must land on the same variant.
            C::Digit0 | C::Numpad0 => Key::Num0,
            C::Digit1 | C::Numpad1 => Key::Num1,
            C::Digit2 | C::Numpad2 => Key::Num2,
            C::Digit3 | C::Numpad3 => Key::Num3,
            C::Digit4 | C::Numpad4 => Key::Num4,
            C::Digit5 | C::Numpad5 => Key::Num5,
            C::Digit6 | C::Numpad6 => Key::Num6,
            C::Digit7 | C::Numpad7 => Key::Num7,
            C::Digit8 | C::Numpad8 => Key::Num8,
            C::Digit9 | C::Numpad9 => Key::Num9,

            C::KeyA => Key::A,
            C::KeyB => Key::B,
            C::KeyC => Key::C,
            C::KeyD => Key::D,
            C::KeyE => Key::E,
            C::KeyF => Key::F,
            C::KeyG => Key::G,
            C::KeyH => Key::H,
            C::KeyI => Key::I,
            C::KeyJ => Key::J,
            C::KeyK => Key::K,
            C::KeyL => Key::L,
            C::KeyM => Key::M,
            C::KeyN => Key::N,
            C::KeyO => Key::O,
            C::KeyP => Key::P,
            C::KeyQ => Key::Q,
            C::KeyR => Key::R,
            C::KeyS => Key::S,
            C::KeyT => Key::T,
            C::KeyU => Key::U,
            C::KeyV => Key::V,
            C::KeyW => Key::W,
            C::KeyX => Key::X,
            C::KeyY => Key::Y,
            C::KeyZ => Key::Z,

            C::F1 => Key::F1,
            C::F2 => Key::F2,
            C::F3 => Key::F3,
            C::F4 => Key::F4,
            C::F5 => Key::F5,
            C::F6 => Key::F6,
            C::F7 => Key::F7,
            C::F8 => Key::F8,
            C::F9 => Key::F9,
            C::F10 => Key::F10,
            C::F11 => Key::F11,
            C::F12 => Key::F12,

            _ => return None,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::gfx::math::PhysicalPoint;

        #[test]
        fn positions_flip_to_a_bottom_left_origin_in_physical_pixels() {
            // A cursor 100 logical px below the top of a 600-physical-px
            // window at 2x is 200 physical px down, so 400 up from the bottom.
            let p: PhysicalPoint =
                LogicalPoint { x: 50.0, y: 100.0, device_pixel_ratio: 2.0, height: 600.0 }.into();
            assert_eq!(p.x, 100.0);
            assert_eq!(p.y, 400.0);
        }

        #[test]
        fn the_top_and_bottom_edges_map_to_the_full_height_and_zero() {
            let at = |y| {
                PhysicalPoint::from(LogicalPoint {
                    x: 0.0,
                    y,
                    device_pixel_ratio: 1.0,
                    height: 480.0,
                })
                .y
            };
            assert_eq!(at(0.0), 480.0, "the top edge must be the maximum, not zero");
            assert_eq!(at(480.0), 0.0);
        }

        #[test]
        fn a_wheel_notch_is_twenty_four_however_the_os_reports_it() {
            // Not a round number by choice — camera.rs's zoom step is
            // calibrated against it, so both delta encodings must agree.
            let mut g = FrameInputGenerator::new(winit::dpi::PhysicalSize::new(800, 600), 1.0);
            g.cursor_pos = Some(LogicalPoint {
                x: 0.0,
                y: 0.0,
                device_pixel_ratio: 1.0,
                height: 600.0,
            });
            g.handle_winit_window_event(&WindowEvent::MouseWheel {
                device_id: winit::event::DeviceId::dummy(),
                delta: winit::event::MouseScrollDelta::LineDelta(0.0, 1.0),
                phase: winit::event::TouchPhase::Moved,
            });
            let Some(Event::MouseWheel { delta, .. }) = g.events.first() else {
                panic!("no wheel event produced");
            };
            assert_eq!(delta.1, 24.0);
        }

        #[test]
        fn control_characters_never_become_text() {
            // Backspace and Escape arrive with `text` set on some platforms.
            // Letting them through types a glyph into the focused field.
            assert!(!is_printable_char('\u{8}'));
            assert!(!is_printable_char('\u{1b}'));
            assert!(is_printable_char('a'));
            assert!(is_printable_char('é'));
        }

        #[test]
        fn the_numpad_and_the_number_row_agree() {
            // The numpad digits are Blender's axis views; binding only one of
            // the two would work on the tester's keyboard and nowhere else.
            use winit::keyboard::KeyCode as C;
            for (row, pad) in [
                (C::Digit1, C::Numpad1),
                (C::Digit3, C::Numpad3),
                (C::Digit7, C::Numpad7),
            ] {
                assert_eq!(translate_key_code(row), translate_key_code(pad));
                assert!(translate_key_code(row).is_some());
            }
        }

        #[test]
        fn the_first_frame_flag_falls_after_one_frame_and_returns_on_unocclude() {
            let mut g = FrameInputGenerator::new(winit::dpi::PhysicalSize::new(800, 600), 1.0);
            assert!(g.generate().first_frame);
            assert!(!g.generate().first_frame);
            g.handle_winit_window_event(&WindowEvent::Occluded(false));
            assert!(g.generate().first_frame, "an un-occluded window must redraw fully");
        }

        #[test]
        fn generate_drains_the_queue() {
            let mut g = FrameInputGenerator::new(winit::dpi::PhysicalSize::new(800, 600), 1.0);
            g.events.push(Event::Text("a".into()));
            assert_eq!(g.generate().events.len(), 1);
            assert!(g.generate().events.is_empty(), "events were replayed a second frame");
        }
    }
}
