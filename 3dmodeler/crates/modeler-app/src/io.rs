//! Platform save/load for the modeler's own scene format (.bee3d = JSON).
//!
//! Native: real file dialogs (`rfd`) plus a recent-files list persisted in
//! the user's config dir. Web: browsers have no arbitrary filesystem access,
//! so "save" downloads a file and "open" drives a hidden `<input type=file>`
//! picker; the recent list caches file content in localStorage so a recent
//! entry can be reloaded without asking the user to re-pick it.
//!
//! Dialogs are ASYNC everywhere: `request_open()` / `request_save()` return
//! immediately and the outcome is picked up later via `poll_open()` /
//! `poll_save()`. On native the dialog runs on a background thread — calling
//! the blocking `rfd` dialog from inside the render loop froze the winit
//! event loop, and the stale input events replayed after it unblocked broke
//! mouse navigation (stuck modifiers/buttons) and even the window's
//! maximized state. Keeping the loop alive also keeps the viewport and the
//! MCP control API responsive while a dialog is up.

use modeler_core::{Scene, SceneData};
use std::sync::Mutex;

pub const EXTENSION: &str = "bee3d";
pub const DEFAULT_NAME: &str = "scene.bee3d";
const RECENT_LIMIT: usize = 8;

/// An entry in the File > Recent list.
pub struct RecentEntry {
    pub label: String,
    pub handle: FileHandle,
}

static PENDING_OPEN: Mutex<Option<Result<(FileHandle, SceneData), String>>> = Mutex::new(None);
static PENDING_SAVE: Mutex<Option<Result<FileHandle, String>>> = Mutex::new(None);
/// One dialog at a time (native): repeated Ctrl+S/Ctrl+O while a dialog is
/// already up must not stack a second one behind it.
#[cfg(not(target_arch = "wasm32"))]
static DIALOG_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Consume the result of an in-flight `request_open()`, if it has completed.
/// A cancelled dialog never produces a result.
pub fn poll_open() -> Option<Result<(FileHandle, SceneData), String>> {
    PENDING_OPEN.lock().ok().and_then(|mut p| p.take())
}

/// Consume the result of an in-flight `request_save()`, if it has completed.
/// `Ok(handle)` means the file was written; a cancelled dialog never
/// produces a result.
pub fn poll_save() -> Option<Result<FileHandle, String>> {
    PENDING_SAVE.lock().ok().and_then(|mut p| p.take())
}

/// Write text to a file (native) or trigger a browser download (web). Used
/// for the OBJ export, which is a one-shot dump rather than a re-openable
/// scene file.
pub fn export_file(filename: &str, text: &str) -> Result<String, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::fs::write(filename, text).map_err(|e| e.to_string())?;
        Ok(format!("wrote {filename}"))
    }
    #[cfg(target_arch = "wasm32")]
    {
        download(filename, text)?;
        Ok(format!("downloading {filename}"))
    }
}

// ---------------------------------------------------------------- native --

#[cfg(not(target_arch = "wasm32"))]
pub type FileHandle = std::path::PathBuf;

#[cfg(not(target_arch = "wasm32"))]
pub fn display_name(handle: &FileHandle) -> String {
    handle
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| handle.display().to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save(scene: &Scene, handle: &FileHandle) -> Result<(), String> {
    std::fs::write(handle, scene.to_json()).map_err(|e| e.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load(handle: &FileHandle) -> Result<SceneData, String> {
    let json = std::fs::read_to_string(handle).map_err(|e| e.to_string())?;
    Scene::from_json(&json)
}

/// Show the Open dialog on a background thread; result lands in `poll_open`.
/// `start_dir` (the Preferences default save location) sets where the dialog
/// opens; None keeps the platform's last-used location.
#[cfg(not(target_arch = "wasm32"))]
pub fn request_open(start_dir: Option<std::path::PathBuf>) {
    use std::sync::atomic::Ordering;
    if DIALOG_OPEN.swap(true, Ordering::SeqCst) {
        return; // a dialog is already up
    }
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new().add_filter("Bee3D scene", &[EXTENSION]);
        if let Some(dir) = start_dir.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            let result = load(&path).map(|data| (path, data));
            if let Ok(mut pending) = PENDING_OPEN.lock() {
                *pending = Some(result);
            }
        }
        DIALOG_OPEN.store(false, Ordering::SeqCst);
    });
}

/// Show the Save dialog on a background thread and write `json` to the
/// chosen path; result lands in `poll_save`. The scene is snapshotted as
/// JSON at request time, so edits made while the dialog is up are not saved.
#[cfg(not(target_arch = "wasm32"))]
pub fn request_save(json: String, default_name: String, start_dir: Option<std::path::PathBuf>) {
    use std::sync::atomic::Ordering;
    if DIALOG_OPEN.swap(true, Ordering::SeqCst) {
        return; // a dialog is already up
    }
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Bee3D scene", &[EXTENSION])
            .set_file_name(&default_name);
        if let Some(dir) = start_dir.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        let picked = dialog.save_file();
        if let Some(mut path) = picked {
            if path.extension().is_none() {
                path.set_extension(EXTENSION);
            }
            let result = std::fs::write(&path, &json)
                .map(|_| path)
                .map_err(|e| e.to_string());
            if let Ok(mut pending) = PENDING_SAVE.lock() {
                *pending = Some(result);
            }
        }
        DIALOG_OPEN.store(false, Ordering::SeqCst);
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn recent_file_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("box3d-modeler").join("recent_files.txt"))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn recent_entries() -> Vec<RecentEntry> {
    let Some(path) = recent_file_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter(|l| !l.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
        .map(|p| RecentEntry { label: display_name(&p), handle: p })
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
fn reorder_recent(
    mut paths: Vec<std::path::PathBuf>,
    handle: &std::path::Path,
    limit: usize,
) -> Vec<std::path::PathBuf> {
    paths.retain(|p| p != handle);
    paths.insert(0, handle.to_path_buf());
    paths.truncate(limit);
    paths
}

#[cfg(not(target_arch = "wasm32"))]
pub fn add_recent(handle: &FileHandle, _scene: &Scene) {
    let Some(config_path) = recent_file_path() else { return };
    let paths: Vec<std::path::PathBuf> = std::fs::read_to_string(&config_path)
        .map(|text| text.lines().map(std::path::PathBuf::from).collect())
        .unwrap_or_default();
    let paths = reorder_recent(paths, handle, RECENT_LIMIT);
    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(config_path, text);
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn reorder_recent_dedups_moves_to_front_and_truncates() {
        let existing = vec![
            PathBuf::from("/a.bee3d"),
            PathBuf::from("/b.bee3d"),
            PathBuf::from("/c.bee3d"),
        ];
        let reordered = reorder_recent(existing, std::path::Path::new("/b.bee3d"), 2);
        assert_eq!(
            reordered,
            vec![PathBuf::from("/b.bee3d"), PathBuf::from("/a.bee3d")]
        );
    }

    #[test]
    fn save_then_load_roundtrips_scene_content() {
        let path = std::env::temp_dir().join(format!("io_test_{}.bee3d", std::process::id()));
        let scene = Scene::default_scene();
        save(&scene, &path).expect("save");
        let data = load(&path).expect("load");
        let mut restored = Scene::new();
        restored.restore(&data);
        assert_eq!(restored.to_json(), scene.to_json());
        let _ = std::fs::remove_file(&path);
    }
}

// ------------------------------------------------------------------- web --

#[cfg(target_arch = "wasm32")]
pub type FileHandle = String;

#[cfg(target_arch = "wasm32")]
pub fn display_name(handle: &FileHandle) -> String {
    handle.clone()
}

#[cfg(target_arch = "wasm32")]
pub fn save(scene: &Scene, handle: &FileHandle) -> Result<(), String> {
    download(handle, &scene.to_json())
}

/// Web can only "load" a recent entry from its localStorage cache — the
/// browser never hands back a real path to re-read from disk.
#[cfg(target_arch = "wasm32")]
pub fn load(handle: &FileHandle) -> Result<SceneData, String> {
    let json = recent_json(handle)
        .ok_or_else(|| "no longer cached — use Open… to pick the file again".to_string())?;
    Scene::from_json(&json)
}

/// Browsers can't offer a save-path picker: the name decides the download
/// filename and the "dialog" completes immediately (result via `poll_save`,
/// same flow as native).
#[cfg(target_arch = "wasm32")]
pub fn request_save(json: String, name: String, _start_dir: Option<std::path::PathBuf>) {
    let result = download(&name, &json).map(|_| name);
    if let Ok(mut pending) = PENDING_SAVE.lock() {
        *pending = Some(result);
    }
}

#[cfg(target_arch = "wasm32")]
pub fn request_open(_start_dir: Option<std::path::PathBuf>) {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    let Some(window) = web_sys::window() else { return };
    let Some(document) = window.document() else { return };
    let Ok(el) = document.create_element("input") else { return };
    let Ok(input) = el.dyn_into::<web_sys::HtmlInputElement>() else { return };
    input.set_type("file");
    input.set_accept(&format!(".{EXTENSION}"));
    if let Some(html_el) = input.dyn_ref::<web_sys::HtmlElement>() {
        let _ = html_el.style().set_property("display", "none");
    }
    if let Some(body) = document.body() {
        let _ = body.append_child(&input);
    }

    let input_for_closure = input.clone();
    let onchange = Closure::<dyn FnMut(web_sys::Event)>::new(move |_event: web_sys::Event| {
        let Some(files) = input_for_closure.files() else { return };
        let Some(file) = files.get(0) else { return };
        let name = file.name();
        let Ok(reader) = web_sys::FileReader::new() else { return };
        let reader_for_load = reader.clone();
        let onload = Closure::once(move || {
            let text = reader_for_load
                .result()
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();
            let parsed = Scene::from_json(&text).map(|data| (name.clone(), data));
            if let Ok(mut pending) = PENDING_OPEN.lock() {
                *pending = Some(parsed);
            }
        });
        reader.set_onload(Some(onload.as_ref().unchecked_ref()));
        onload.forget();
        let _ = reader.read_as_text(&file);
        input_for_closure.remove();
    });
    input.set_onchange(Some(onchange.as_ref().unchecked_ref()));
    input.click();
    onchange.forget();
}

#[cfg(target_arch = "wasm32")]
pub fn recent_entries() -> Vec<RecentEntry> {
    let Some(storage) = local_storage() else { return Vec::new() };
    (0..RECENT_LIMIT)
        .filter_map(|i| {
            let name = storage
                .get_item(&format!("modeler_recent_{i}_name"))
                .ok()
                .flatten()?;
            Some(RecentEntry { label: name.clone(), handle: name })
        })
        .collect()
}

#[cfg(target_arch = "wasm32")]
pub fn add_recent(handle: &FileHandle, scene: &Scene) {
    let Some(storage) = local_storage() else { return };
    let mut entries: Vec<(String, String)> = (0..RECENT_LIMIT)
        .filter_map(|i| {
            let name = storage
                .get_item(&format!("modeler_recent_{i}_name"))
                .ok()
                .flatten()?;
            let json = storage
                .get_item(&format!("modeler_recent_{i}_json"))
                .ok()
                .flatten()
                .unwrap_or_default();
            Some((name, json))
        })
        .collect();
    entries.retain(|(n, _)| n != handle);
    entries.insert(0, (handle.clone(), scene.to_json()));
    entries.truncate(RECENT_LIMIT);

    for i in 0..RECENT_LIMIT {
        let _ = storage.remove_item(&format!("modeler_recent_{i}_name"));
        let _ = storage.remove_item(&format!("modeler_recent_{i}_json"));
    }
    for (i, (name, json)) in entries.iter().enumerate() {
        let _ = storage.set_item(&format!("modeler_recent_{i}_name"), name);
        let _ = storage.set_item(&format!("modeler_recent_{i}_json"), json);
    }
}

#[cfg(target_arch = "wasm32")]
fn recent_json(name: &str) -> Option<String> {
    let storage = local_storage()?;
    (0..RECENT_LIMIT).find_map(|i| {
        let slot_name = storage
            .get_item(&format!("modeler_recent_{i}_name"))
            .ok()
            .flatten()?;
        (slot_name == name)
            .then(|| storage.get_item(&format!("modeler_recent_{i}_json")).ok().flatten())
            .flatten()
    })
}

#[cfg(target_arch = "wasm32")]
fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

#[cfg(target_arch = "wasm32")]
fn download(filename: &str, text: &str) -> Result<(), String> {
    use wasm_bindgen::JsCast;

    let err = |m: &str| m.to_string();
    let window = web_sys::window().ok_or(err("no window"))?;
    let document = window.document().ok_or(err("no document"))?;

    let blob_parts = js_sys::Array::new();
    blob_parts.push(&wasm_bindgen::JsValue::from_str(text));
    let blob = web_sys::Blob::new_with_str_sequence(&blob_parts).map_err(|_| err("blob"))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|_| err("url"))?;

    let anchor: web_sys::HtmlAnchorElement = document
        .create_element("a")
        .map_err(|_| err("anchor"))?
        .dyn_into()
        .map_err(|_| err("anchor cast"))?;
    anchor.set_href(&url);
    anchor.set_download(filename);
    anchor.click();
    let _ = web_sys::Url::revoke_object_url(&url);
    Ok(())
}
