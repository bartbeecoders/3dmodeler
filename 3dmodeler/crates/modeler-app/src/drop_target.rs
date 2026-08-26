//! OS file drops onto the window/canvas, routed to the right importer.
//!
//! Native: winit delivers `WindowEvent::DroppedFile` paths (see main.rs),
//! which land in [`handle_path`]. Browser: winit's web backend never sees OS
//! drops, so [`init`] registers document-level dragover/drop listeners; the
//! dropped `File`s are read in place and land in `handle_bytes`.
//!
//! Dispatch is by extension: .glb/.gltf parse in Rust on both targets,
//! .blend drives a local Blender (native only), PDFs feed the
//! reference-setup tray, and images queue for the reference-vs-3D-model
//! choice dialog (native; the browser has no TRELLIS, so images go straight
//! to the reference tray there). Anything else leaves a one-shot status
//! line in [`poll_status`].

use std::sync::Mutex;

static STATUS: Mutex<Option<String>> = Mutex::new(None);

/// Status line for drops that could not be imported.
pub fn poll_status() -> Option<String> {
    STATUS.lock().ok().and_then(|mut s| s.take())
}

fn set_status(message: String) {
    if let Ok(mut status) = STATUS.lock() {
        *status = Some(message);
    }
}

fn unsupported_message(ext: &str) -> String {
    let dot_ext = if ext.is_empty() { "files without an extension".into() } else { format!(".{ext} files") };
    format!("can't import {dot_ext} — drop .glb/.gltf models, .blend scenes, images or PDFs")
}

/// Dropped images awaiting the user's choice (reference image vs. TRELLIS
/// 3D conversion) — drained by `ui`'s image-drop dialog each frame.
#[cfg(not(target_arch = "wasm32"))]
static PENDING_IMAGES: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());

#[cfg(not(target_arch = "wasm32"))]
pub fn poll_images() -> Vec<std::path::PathBuf> {
    PENDING_IMAGES.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default()
}

/// Call once at startup. Registers the browser drop listeners on wasm;
/// a no-op natively (winit delivers drops there).
pub fn init() {
    #[cfg(target_arch = "wasm32")]
    web::init();
}

/// A file dropped onto the native window.
#[cfg(not(target_arch = "wasm32"))]
pub fn handle_path(path: std::path::PathBuf) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "glb" | "gltf" => crate::gltf_import::import_path(path),
        "blend" => crate::blend::import_path(path),
        // pictures wait for the user's call: reference image, or TRELLIS
        // image→3D conversion (the dialog lives in ui::image_drop_window)
        "png" | "jpg" | "jpeg" | "webp" => {
            if let Ok(mut pending) = PENDING_IMAGES.lock() {
                pending.push(path);
            }
        }
        // plan sets are unambiguous — straight to the reference-setup tray
        "pdf" => crate::ref_image::push_setup_file(&path),
        _ => set_status(unsupported_message(&ext)),
    }
}

/// A file dropped onto the browser page, already read into memory.
#[cfg(target_arch = "wasm32")]
fn handle_bytes(file_name: String, bytes: Vec<u8>) {
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), ext.to_ascii_lowercase()),
        None => (file_name.clone(), String::new()),
    };
    match ext.as_str() {
        "glb" | "gltf" => crate::gltf_import::import_bytes(file_name, bytes),
        // no TRELLIS in the browser (it needs the local GPU workspace), so
        // images go straight to the reference tray there
        "png" | "jpg" | "jpeg" | "webp" | "pdf" => crate::ref_image::push_setup_bytes(stem, bytes),
        "blend" => set_status(
            ".blend import needs the desktop app (the browser can't run Blender) — \
             export as .glb instead"
                .into(),
        ),
        _ => set_status(unsupported_message(&ext)),
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    pub fn init() {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
        // without preventDefault on dragover, the drop event never fires and
        // the browser navigates to the dropped file instead
        let dragover = Closure::<dyn FnMut(web_sys::DragEvent)>::new(
            |event: web_sys::DragEvent| event.prevent_default(),
        );
        let _ = document
            .add_event_listener_with_callback("dragover", dragover.as_ref().unchecked_ref());
        dragover.forget();

        let drop = Closure::<dyn FnMut(web_sys::DragEvent)>::new(|event: web_sys::DragEvent| {
            event.prevent_default();
            let Some(files) = event.data_transfer().and_then(|dt| dt.files()) else { return };
            for i in 0..files.length() {
                let Some(file) = files.get(i) else { continue };
                let name = file.name();
                let Ok(reader) = web_sys::FileReader::new() else { continue };
                let reader_for_load = reader.clone();
                let onload = Closure::once(move || {
                    let Ok(result) = reader_for_load.result() else { return };
                    let bytes = js_sys::Uint8Array::new(&result).to_vec();
                    super::handle_bytes(name, bytes);
                });
                reader.set_onload(Some(onload.as_ref().unchecked_ref()));
                onload.forget();
                let _ = reader.read_as_array_buffer(&file);
            }
        });
        let _ = document.add_event_listener_with_callback("drop", drop.as_ref().unchecked_ref());
        drop.forget();
    }
}
