//! Separate OS window for live camera preview (native only).
//!
//! F12 opens a dedicated window you can drag to another monitor. While it
//! stays open the main app pushes a new frame every redraw so the view
//! tracks camera moves and scene edits in real time. Implemented with
//! minifb on a background thread so it never shares the main OpenGL /
//! winit event loop.
//!
//! Important: `push_frame` only updates an **already open** window. After the
//! user hits the window close button (or F12/Esc), the window stays closed
//! until `open` is called again — the main loop must not recreate it.

#![cfg(not(target_arch = "wasm32"))]

use minifb::{Key, KeyRepeat, ScaleMode, Window, WindowOptions};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// One frame ready for display (0x00RRGGBB pixels, top-left origin).
struct Frame {
    title: String,
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

struct Shared {
    /// Latest frame to show; the preview thread takes updates while open.
    frame: Mutex<Option<Frame>>,
    /// Bumped whenever `frame` is replaced so the thread refreshes.
    generation: AtomicU64,
    /// Set when the host wants the thread to exit (app shutdown / close()).
    stop: AtomicBool,
    /// True only while the OS window is alive. Cleared as soon as the user
    /// closes it (or F12/Esc) so the main loop can stop live mode.
    open: AtomicBool,
}

/// Owns the background live-preview window (if any).
pub struct RenderPreview {
    shared: Option<Arc<Shared>>,
    join: Option<JoinHandle<()>>,
    /// Reused conversion buffer (RGBA → 0x00RRGGBB).
    convert_buf: Vec<u32>,
}

impl Default for RenderPreview {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderPreview {
    pub fn new() -> Self {
        Self {
            shared: None,
            join: None,
            convert_buf: Vec::new(),
        }
    }

    /// True while the OS preview window is still open.
    pub fn is_open(&mut self) -> bool {
        self.reap_if_finished();
        self.shared
            .as_ref()
            .is_some_and(|s| s.open.load(Ordering::SeqCst))
    }

    /// Close the preview window if it is open (blocks until the thread exits).
    pub fn close(&mut self) {
        if let Some(shared) = &self.shared {
            shared.stop.store(true, Ordering::SeqCst);
            shared.open.store(false, Ordering::SeqCst);
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.shared = None;
    }

    /// Open a new preview window (or replace an existing one) with this frame.
    /// Call this only when starting live mode (F12 on), not every frame.
    pub fn open(&mut self, title: String, width: u32, height: u32, rgba: &[u8]) {
        let Some(frame) = self.make_frame(title, width, height, rgba) else {
            return;
        };
        self.close();

        let shared = Arc::new(Shared {
            frame: Mutex::new(Some(frame)),
            generation: AtomicU64::new(1),
            stop: AtomicBool::new(false),
            open: AtomicBool::new(true),
        });
        let thread_shared = Arc::clone(&shared);
        let join = thread::Builder::new()
            .name("render-preview".into())
            .spawn(move || preview_loop(thread_shared))
            .expect("spawn render-preview thread");
        self.shared = Some(shared);
        self.join = Some(join);
    }

    /// Push a new image to the open window. **No-op if the window is closed**
    /// (does not reopen — that was recreating the window after the user hit ✕).
    /// Returns whether the window is still open.
    pub fn push_frame(&mut self, title: String, width: u32, height: u32, rgba: &[u8]) -> bool {
        self.reap_if_finished();
        let still_open = self
            .shared
            .as_ref()
            .is_some_and(|s| s.open.load(Ordering::SeqCst));
        if !still_open {
            // User closed the window; clean up the finished (or finishing) thread.
            if self.join.as_ref().is_some_and(|j| j.is_finished()) {
                if let Some(j) = self.join.take() {
                    let _ = j.join();
                }
                self.shared = None;
            }
            return false;
        }
        let Some(frame) = self.make_frame(title, width, height, rgba) else {
            return true;
        };
        let shared = self.shared.as_ref().unwrap();
        *shared.frame.lock().unwrap() = Some(frame);
        shared.generation.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn make_frame(
        &mut self,
        title: String,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Option<Frame> {
        let w = width.max(1) as usize;
        let h = height.max(1) as usize;
        let expected = w * h * 4;
        if rgba.len() < expected {
            eprintln!(
                "render preview: buffer too small ({} < {})",
                rgba.len(),
                expected
            );
            return None;
        }
        let mut pixels = std::mem::take(&mut self.convert_buf);
        if pixels.len() != w * h {
            pixels.resize(w * h, 0);
        }
        for i in 0..(w * h) {
            let o = i * 4;
            let r = rgba[o] as u32;
            let g = rgba[o + 1] as u32;
            let b = rgba[o + 2] as u32;
            pixels[i] = (r << 16) | (g << 8) | b;
        }
        Some(Frame {
            title,
            width: w,
            height: h,
            pixels,
        })
    }

    fn reap_if_finished(&mut self) {
        if self.join.as_ref().is_some_and(|j| j.is_finished()) {
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
            // If the thread ended, the window is gone.
            if let Some(shared) = &self.shared {
                shared.open.store(false, Ordering::SeqCst);
            }
            self.shared = None;
        }
    }
}

impl Drop for RenderPreview {
    fn drop(&mut self) {
        self.close();
    }
}

fn preview_loop(shared: Arc<Shared>) {
    let first = loop {
        if shared.stop.load(Ordering::SeqCst) || !shared.open.load(Ordering::SeqCst) {
            shared.open.store(false, Ordering::SeqCst);
            return;
        }
        if let Some(frame) = shared.frame.lock().unwrap().take() {
            break frame;
        }
        thread::sleep(Duration::from_millis(8));
    };

    let mut width = first.width;
    let mut height = first.height;
    let mut pixels = first.pixels;
    let mut title = first.title;
    let mut generation = shared.generation.load(Ordering::SeqCst);

    let mut window = match Window::new(
        &format!("Camera View — {title} (live · F12 / ✕ closes)"),
        width,
        height,
        WindowOptions {
            resize: true,
            scale_mode: ScaleMode::AspectRatioStretch,
            ..WindowOptions::default()
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("render preview window failed: {e}");
            shared.open.store(false, Ordering::SeqCst);
            return;
        }
    };
    window.set_target_fps(60);

    while shared.open.load(Ordering::SeqCst) && !shared.stop.load(Ordering::SeqCst) {
        // Process window events first so the close button is honored promptly.
        if !window.is_open() {
            break;
        }

        let gen = shared.generation.load(Ordering::SeqCst);
        if gen != generation {
            generation = gen;
            if let Some(frame) = shared.frame.lock().unwrap().take() {
                if frame.width != width || frame.height != height {
                    width = frame.width;
                    height = frame.height;
                    title = frame.title;
                    pixels = frame.pixels;
                    match Window::new(
                        &format!("Camera View — {title} (live · F12 / ✕ closes)"),
                        width,
                        height,
                        WindowOptions {
                            resize: true,
                            scale_mode: ScaleMode::AspectRatioStretch,
                            ..WindowOptions::default()
                        },
                    ) {
                        Ok(w) => {
                            window = w;
                            window.set_target_fps(60);
                        }
                        Err(e) => {
                            eprintln!("render preview recreate failed: {e}");
                            break;
                        }
                    }
                } else {
                    if frame.title != title {
                        title = frame.title;
                        window.set_title(&format!(
                            "Camera View — {title} (live · F12 / ✕ closes)"
                        ));
                    }
                    pixels = frame.pixels;
                }
            }
        }

        if let Err(e) = window.update_with_buffer(&pixels, width, height) {
            eprintln!("render preview update failed: {e}");
            break;
        }

        // Close button is reflected in is_open after update.
        if !window.is_open() {
            break;
        }

        // When this window has focus the main app never sees F12.
        if window.is_key_pressed(Key::F12, KeyRepeat::No)
            || window.is_key_pressed(Key::Escape, KeyRepeat::No)
        {
            break;
        }
    }

    // Mark closed *before* dropping the window so the main thread stops
    // treating the preview as live (and never reopens it via push_frame).
    shared.open.store(false, Ordering::SeqCst);
    shared.stop.store(true, Ordering::SeqCst);
    drop(window);
}
