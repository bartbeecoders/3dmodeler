//! HTTP control API for external tools (the MCP server in `modeler-mcp`).
//! Native builds only — the browser can't host a TCP listener.
//!
//! A background thread accepts POST requests with a JSON command body and
//! forwards them to the render loop through a channel; the render loop
//! executes them against the live scene and replies. Screenshots are
//! captured right after the frame is rendered.

use crate::physics::PhysicsMirror;
use crate::selection::Selection;
use modeler_core::glam::{EulerRot, Quat, Vec3};
use modeler_core::{library, Library, LibraryAsset, ObjectId, Primitive, Scene, Transform, WallCutout};
use serde_json::{json, Value};
use std::sync::mpsc::{channel, Receiver, Sender};

pub const DEFAULT_PORT: u16 = 8323;

type Reply = Sender<Value>;

pub struct ControlServer {
    requests: Receiver<(Value, Reply)>,
    /// Screenshot requests wait until after the next render.
    pub pending_screenshots: Vec<Reply>,
    port: u16,
    commands_handled: u64,
    last_command: Option<std::time::Instant>,
}

impl ControlServer {
    /// Spawn the HTTP listener thread. Returns None if the port is taken
    /// (e.g. a second app instance) — the app just runs without control.
    pub fn start() -> Option<Self> {
        let port = std::env::var("MODELER_CONTROL_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PORT);
        let server = tiny_http::Server::http(("127.0.0.1", port)).ok()?;
        let (tx, rx) = channel::<(Value, Reply)>();

        std::thread::spawn(move || {
            for mut http_request in server.incoming_requests() {
                let mut body = String::new();
                let _ = http_request.as_reader().read_to_string(&mut body);

                let response_json = match serde_json::from_str::<Value>(&body) {
                    Err(e) => json!({"ok": false, "error": format!("invalid JSON: {e}")}),
                    Ok(command) => {
                        let (reply_tx, reply_rx) = channel();
                        if tx.send((command, reply_tx)).is_err() {
                            json!({"ok": false, "error": "app is shutting down"})
                        } else {
                            // screenshots wait for the next frame; allow time
                            reply_rx
                                .recv_timeout(std::time::Duration::from_secs(10))
                                .unwrap_or_else(|_| {
                                    json!({"ok": false, "error": "timed out waiting for the app"})
                                })
                        }
                    }
                };
                let data = response_json.to_string();
                let response = tiny_http::Response::from_string(data).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap(),
                );
                let _ = http_request.respond(response);
            }
        });

        println!("control API listening on http://127.0.0.1:{port} (for modeler-mcp)");
        Some(Self {
            requests: rx,
            pending_screenshots: Vec::new(),
            port,
            commands_handled: 0,
            last_command: None,
        })
    }

    /// Status for the UI indicator.
    pub fn status(&self) -> crate::ui::McpStatus {
        crate::ui::McpStatus {
            port: self.port,
            commands_handled: self.commands_handled,
            seconds_since_last: self
                .last_command
                .map(|t| t.elapsed().as_secs_f32()),
        }
    }

    /// Execute queued commands. Call once per frame from the render loop.
    pub fn poll(
        &mut self,
        scene: &mut Scene,
        selection: &mut Selection,
        physics: &mut PhysicsMirror,
        library_doc: &mut Library,
    ) {
        while let Ok((command, reply)) = self.requests.try_recv() {
            self.commands_handled += 1;
            self.last_command = Some(std::time::Instant::now());
            if command["cmd"] == "screenshot" {
                self.pending_screenshots.push(reply);
                continue;
            }
            let response = execute(&command, scene, selection, physics, library_doc);
            let _ = reply.send(response);
        }
    }
}

/// Resolve an object reference: numeric id or (unique) name.
fn resolve(scene: &Scene, reference: &Value) -> Result<ObjectId, String> {
    if let Some(n) = reference.as_u64() {
        let id = ObjectId(n);
        return scene
            .object(id)
            .map(|_| id)
            .ok_or_else(|| format!("no object with id {n}"));
    }
    let name = reference
        .as_str()
        .ok_or("object reference must be a name or an id")?;
    if let Some(object) = scene.objects().iter().find(|o| o.name == name) {
        return Ok(object.id);
    }
    // allow "42" as a string id too
    if let Ok(n) = name.parse::<u64>() {
        if scene.object(ObjectId(n)).is_some() {
            return Ok(ObjectId(n));
        }
    }
    Err(format!("no object named '{name}'"))
}

fn vec3_from(value: &Value) -> Option<Vec3> {
    let array = value.as_array()?;
    if array.len() != 3 {
        return None;
    }
    Some(Vec3::new(
        array[0].as_f64()? as f32,
        array[1].as_f64()? as f32,
        array[2].as_f64()? as f32,
    ))
}

fn object_json(scene: &Scene, object: &modeler_core::Object) -> Value {
    let (rx, ry, rz) = object.transform.rotation.to_euler(EulerRot::XYZ);
    let world = scene.world_transform(object.id);
    let dims = object.primitive.dimensions() * world.scale.abs();
    let mut json = json!({
        "id": object.id.0,
        "name": object.name,
        "primitive": format!("{:?}", object.primitive),
        "location": [object.transform.location.x, object.transform.location.y, object.transform.location.z],
        "rotation_euler_deg": [rx.to_degrees(), ry.to_degrees(), rz.to_degrees()],
        "scale": [object.transform.scale.x, object.transform.scale.y, object.transform.scale.z],
        "world_location": [world.location.x, world.location.y, world.location.z],
        "parent": object.parent.map(|p| p.0),
        "pivot": [object.pivot.x, object.pivot.y, object.pivot.z],
        "anchor": [object.anchor.x, object.anchor.y, object.anchor.z],
        "group": object.group,
        "visible": object.visible,
        "smooth": object.smooth,
        "dynamic": object.dynamic,
        "density": object.density,
        "color": object.material.base_color,
        "show_label": object.show_label,
        "show_dimensions": object.show_dimensions,
        "dimensions_m": [dims.x, dims.y, dims.z],
    });
    if matches!(object.primitive, modeler_core::Primitive::Wall { .. }) {
        json["cutouts"] = object
            .cutouts
            .iter()
            .map(|c| {
                json!({
                    "offset": c.offset, "width": c.width,
                    "bottom": c.bottom, "height": c.height,
                })
            })
            .collect();
    }
    json
}

fn primitive_from_name(name: &str) -> Option<Primitive> {
    let catalog = Primitive::catalog();
    match name.to_ascii_lowercase().as_str() {
        "plane" => Some(catalog[0]),
        "cube" => Some(catalog[1]),
        "uv_sphere" | "sphere" => Some(catalog[2]),
        "ico_sphere" | "icosphere" => Some(catalog[3]),
        "cylinder" => Some(catalog[4]),
        "cone" => Some(catalog[5]),
        "torus" => Some(catalog[6]),
        "wall" => Some(Primitive::Wall { length: 2.0, height: 2.5, thickness: 0.2 }),
        "empty" => Some(catalog[7]),
        _ => None,
    }
}

/// Apply optional fields from `params` onto an object. Shared by add/update.
fn apply_object_params(
    scene: &mut Scene,
    id: ObjectId,
    params: &Value,
) -> Result<(), String> {
    let location = params.get("location").map(|v| {
        vec3_from(v).ok_or("location must be [x, y, z]")
    });
    let rotation = params.get("rotation_euler_deg").map(|v| {
        vec3_from(v).ok_or("rotation_euler_deg must be [x, y, z] in degrees")
    });
    let scale = params.get("scale").map(|v| vec3_from(v).ok_or("scale must be [x, y, z]"));
    let color = params.get("color").map(|v| vec3_from(v).ok_or("color must be [r, g, b] 0..1"));
    let pivot = params.get("pivot").map(|v| vec3_from(v).ok_or("pivot must be [x, y, z]"));
    let anchor = params.get("anchor").map(|v| vec3_from(v).ok_or("anchor must be [x, y, z]"));
    // wall openings: full replacement of the cutout list
    let cutouts: Option<Result<Vec<WallCutout>, String>> =
        params.get("cutouts").filter(|v| !v.is_null()).map(|v| {
            v.as_array()
                .ok_or_else(|| "cutouts must be an array".to_string())?
                .iter()
                .map(|c| {
                    let field = |k: &str| {
                        c.get(k).and_then(Value::as_f64).map(|v| v as f32).ok_or_else(|| {
                            format!("each cutout needs numeric '{k}' (offset/width/bottom/height, meters)")
                        })
                    };
                    Ok(WallCutout {
                        offset: field("offset")?,
                        width: field("width")?,
                        bottom: field("bottom")?,
                        height: field("height")?,
                    })
                })
                .collect()
        });

    let object = scene.object_mut(id).ok_or("object vanished")?;
    // wall dimensions (walls only; ignored elsewhere)
    if let Primitive::Wall { length, height, thickness } = object.primitive {
        let get = |k: &str| params.get(k).and_then(Value::as_f64).map(|v| v as f32);
        let (l, h, t) = (get("length"), get("height"), get("thickness"));
        if l.is_some() || h.is_some() || t.is_some() {
            object.primitive = Primitive::Wall {
                length: l.unwrap_or(length).max(0.01),
                height: h.unwrap_or(height).max(0.01),
                thickness: t.unwrap_or(thickness).max(0.002),
            };
        }
    }
    if let Some(cutouts) = cutouts {
        object.cutouts = cutouts?;
        object.mesh_revision += 1; // render/physics caches key on it
    }
    if let Some(location) = location {
        object.transform.location = location?;
    }
    if let Some(rotation) = rotation {
        let r = rotation?;
        object.transform.rotation = Quat::from_euler(
            EulerRot::XYZ,
            r.x.to_radians(),
            r.y.to_radians(),
            r.z.to_radians(),
        );
    }
    if let Some(scale) = scale {
        object.transform.scale = scale?;
    }
    if let Some(pivot) = pivot {
        object.pivot = pivot?;
    }
    if let Some(anchor) = anchor {
        object.anchor = anchor?;
    }
    if let Some(color) = color {
        let c = color?;
        object.material.base_color = [c.x, c.y, c.z];
    }
    if let Some(v) = params.get("smooth").and_then(Value::as_bool) {
        object.smooth = v;
    }
    if let Some(v) = params.get("visible").and_then(Value::as_bool) {
        object.visible = v;
    }
    if let Some(v) = params.get("dynamic").and_then(Value::as_bool) {
        object.dynamic = v;
    }
    if let Some(v) = params.get("density").and_then(Value::as_f64) {
        object.density = v as f32;
    }
    if let Some(v) = params.get("show_label").and_then(Value::as_bool) {
        object.show_label = v;
    }
    if let Some(v) = params.get("show_dimensions").and_then(Value::as_bool) {
        object.show_dimensions = v;
    }
    if let Some(v) = params.get("group").and_then(Value::as_bool) {
        object.group = v;
    }
    if let Some(v) = params.get("new_name").and_then(Value::as_str) {
        if !v.trim().is_empty() {
            object.name = v.trim().to_string();
        }
    }
    Ok(())
}

/// Resolve a reference-image reference: numeric id or (unique) name.
fn resolve_ref_image(scene: &Scene, reference: &Value) -> Result<u64, String> {
    if let Some(n) = reference.as_u64() {
        if scene.reference_images().iter().any(|r| r.id == n) {
            return Ok(n);
        }
        return Err(format!("no reference image with id {n}"));
    }
    let name = reference
        .as_str()
        .ok_or("image reference must be a name or an id")?;
    if let Some(image) = scene.reference_images().iter().find(|r| r.name == name) {
        return Ok(image.id);
    }
    if let Ok(n) = name.parse::<u64>() {
        if scene.reference_images().iter().any(|r| r.id == n) {
            return Ok(n);
        }
    }
    Err(format!("no reference image named '{name}'"))
}

fn image_plane_from(value: &Value) -> Result<Option<modeler_core::ImagePlane>, String> {
    let Some(s) = value.as_str() else { return Ok(None) };
    match s.to_ascii_lowercase().as_str() {
        "x" => Ok(Some(modeler_core::ImagePlane::X)),
        "y" => Ok(Some(modeler_core::ImagePlane::Y)),
        "z" => Ok(Some(modeler_core::ImagePlane::Z)),
        other => Err(format!("unknown plane '{other}' (x|y|z)")),
    }
}

/// Apply optional fields onto a reference image. Shared by add/update.
fn apply_ref_image_params(scene: &mut Scene, id: u64, params: &Value) -> Result<(), String> {
    let plane = image_plane_from(&params["plane"])?;
    let location = params.get("location").map(|v| {
        vec3_from(v).ok_or("location must be [x, y, z]")
    });
    let image = scene
        .reference_image_mut(id)
        .ok_or("reference image vanished")?;
    if let Some(plane) = plane {
        image.plane = plane;
    }
    if let Some(location) = location {
        image.location = location?;
    }
    if let Some(v) = params.get("rotation_deg").and_then(Value::as_f64) {
        image.rotation_deg = v as f32;
    }
    if let Some(v) = params.get("width_m").and_then(Value::as_f64) {
        if v <= 0.0 {
            return Err("width_m must be > 0".to_string());
        }
        image.width_m = v as f32;
    }
    if let Some(v) = params.get("opacity").and_then(Value::as_f64) {
        image.opacity = (v as f32).clamp(0.0, 1.0);
    }
    if let Some(v) = params.get("visible").and_then(Value::as_bool) {
        image.visible = v;
    }
    if let Some(v) = params.get("new_name").and_then(Value::as_str) {
        if !v.trim().is_empty() {
            image.name = v.trim().to_string();
        }
    }
    Ok(())
}

fn ref_image_json(image: &modeler_core::ReferenceImage) -> Value {
    let px = crate::ref_image::decoded_size(&image.data_base64);
    json!({
        "id": image.id,
        "name": image.name,
        "plane": format!("{:?}", image.plane),
        "location": [image.location.x, image.location.y, image.location.z],
        "rotation_deg": image.rotation_deg,
        "width_m": image.width_m,
        "height_m": image.height_m(),
        "opacity": image.opacity,
        "visible": image.visible,
        "width_px": px.map(|(w, _)| w),
        "height_px": px.map(|(_, h)| h),
    })
}

/// Resolve a library-asset reference: numeric id or (unique) name.
fn resolve_asset(library_doc: &Library, reference: &Value) -> Result<u64, String> {
    if let Some(n) = reference.as_u64() {
        if library_doc.asset(n).is_some() {
            return Ok(n);
        }
        return Err(format!("no library item with id {n}"));
    }
    let name = reference
        .as_str()
        .ok_or("library item reference must be a name or an id")?;
    if let Some(asset) = library_doc.assets().iter().find(|a| a.name == name) {
        return Ok(asset.id);
    }
    if let Ok(n) = name.parse::<u64>() {
        if library_doc.asset(n).is_some() {
            return Ok(n);
        }
    }
    Err(format!("no library item named '{name}'"))
}

fn asset_json(asset: &LibraryAsset) -> Value {
    json!({
        "id": asset.id,
        "name": asset.name,
        "description": asset.description,
        "object_count": asset.objects.len(),
        "objects": asset.objects.iter().map(|o| o.name.clone()).collect::<Vec<_>>(),
        "has_preview": asset.preview_png_base64.is_some(),
        "pivot": [asset.pivot.x, asset.pivot.y, asset.pivot.z],
        "anchor": [asset.anchor.x, asset.anchor.y, asset.anchor.z],
    })
}

/// Apply optional pivot/anchor fields onto a library asset.
fn apply_asset_points(
    library_doc: &mut Library,
    id: u64,
    params: &Value,
) -> Result<(), String> {
    let pivot = params.get("pivot").map(|v| vec3_from(v).ok_or("pivot must be [x, y, z]"));
    let anchor = params.get("anchor").map(|v| vec3_from(v).ok_or("anchor must be [x, y, z]"));
    if pivot.is_none() && anchor.is_none() {
        return Ok(());
    }
    let asset = library_doc.asset_mut(id).ok_or("library item vanished")?;
    if let Some(pivot) = pivot {
        asset.pivot = pivot?;
    }
    if let Some(anchor) = anchor {
        asset.anchor = anchor?;
    }
    Ok(())
}

/// The objects a library create/update captures: explicit references from
/// `params["objects"]`, or the current viewport selection when omitted.
fn capture_for_library(
    scene: &Scene,
    selection: &Selection,
    params: &Value,
) -> Result<Vec<modeler_core::Object>, String> {
    let ids: Vec<ObjectId> = match params.get("objects").filter(|v| !v.is_null()) {
        Some(refs) => refs
            .as_array()
            .ok_or("'objects' must be an array of names/ids")?
            .iter()
            .map(|r| resolve(scene, r))
            .collect::<Result<Vec<_>, _>>()?,
        None => selection.selected().to_vec(),
    };
    let objects = library::capture_objects(scene, &ids);
    if objects.is_empty() {
        return Err(
            "no objects to capture: pass 'objects' (names/ids) or select some first".to_string(),
        );
    }
    Ok(objects)
}

/// Preview for an asset: explicit `preview_png_base64` wins, otherwise a
/// rendered isometric thumbnail of the captured objects.
fn preview_for_library(
    params: &Value,
    objects: &[modeler_core::Object],
) -> Result<Option<String>, String> {
    match params.get("preview_png_base64").and_then(Value::as_str) {
        Some(data) => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| format!("bad preview_png_base64: {e}"))?;
            Ok(Some(data.to_string()))
        }
        None => Ok(crate::preview::render_preview_base64(objects)),
    }
}

pub fn execute(
    command: &Value,
    scene: &mut Scene,
    selection: &mut Selection,
    physics: &mut PhysicsMirror,
    library_doc: &mut Library,
) -> Value {
    let result = execute_inner(command, scene, selection, physics, library_doc);
    match result {
        Ok(value) => {
            let mut response = json!({"ok": true});
            if let Some(map) = response.as_object_mut() {
                if let Some(extra) = value.as_object() {
                    for (k, v) in extra {
                        map.insert(k.clone(), v.clone());
                    }
                }
            }
            response
        }
        Err(message) => json!({"ok": false, "error": message}),
    }
}

fn execute_inner(
    command: &Value,
    scene: &mut Scene,
    selection: &mut Selection,
    physics: &mut PhysicsMirror,
    library_doc: &mut Library,
) -> Result<Value, String> {
    let cmd = command["cmd"].as_str().ok_or("missing 'cmd'")?;
    match cmd {
        "get_scene" => {
            let objects: Vec<Value> =
                scene.objects().iter().map(|o| object_json(scene, o)).collect();
            let measurements: Vec<Value> = scene
                .measurements()
                .iter()
                .map(|m| {
                    json!({
                        "a": [m.a.x, m.a.y, m.a.z],
                        "b": [m.b.x, m.b.y, m.b.z],
                        "length_m": m.length(),
                    })
                })
                .collect();
            // reference images without their (large) embedded pixel data
            let reference_images: Vec<Value> =
                scene.reference_images().iter().map(ref_image_json).collect();
            Ok(json!({
                "objects": objects,
                "measurements": measurements,
                "reference_images": reference_images,
                "sim_state": format!("{:?}", physics.sim_state()),
            }))
        }
        "new_scene" => {
            physics.stop(scene);
            *scene = Scene::default_scene();
            selection.set(Vec::new(), None);
            Ok(json!({}))
        }
        "add_object" => {
            let primitive_name = command["primitive"]
                .as_str()
                .ok_or("missing 'primitive' (plane|cube|sphere|icosphere|cylinder|cone|torus|wall)")?;
            let primitive = primitive_from_name(primitive_name)
                .ok_or_else(|| format!("unknown primitive '{primitive_name}'"))?;
            let id = scene.add_object(primitive, Transform::default());
            apply_object_params(scene, id, command)?;
            let object = scene.object(id).unwrap();
            Ok(json!({"id": id.0, "name": object.name}))
        }
        "update_object" => {
            let id = resolve(scene, &command["object"])?;
            apply_object_params(scene, id, command)?;
            Ok(json!({"object": object_json(scene, scene.object(id).unwrap())}))
        }
        "delete_object" => {
            let id = resolve(scene, &command["object"])?;
            scene.remove_object(id);
            selection.retain_existing(|i| scene.object(i).is_some());
            Ok(json!({}))
        }
        "set_parent" => {
            let child = resolve(scene, &command["child"])?;
            let parent = if command["parent"].is_null() {
                None
            } else {
                Some(resolve(scene, &command["parent"])?)
            };
            if scene.set_parent(child, parent) {
                Ok(json!({}))
            } else {
                Err("parenting rejected (cycle or missing object)".to_string())
            }
        }
        "group_objects" => {
            let refs = command["objects"]
                .as_array()
                .filter(|a| a.len() >= 2)
                .ok_or("'objects' must be an array of at least 2 names/ids")?;
            let ids: Vec<ObjectId> = refs
                .iter()
                .map(|r| resolve(scene, r))
                .collect::<Result<Vec<_>, _>>()?;
            let root = match command.get("root").filter(|v| !v.is_null()) {
                Some(r) => {
                    let root = resolve(scene, r)?;
                    if !ids.contains(&root) {
                        return Err("'root' must be one of 'objects'".to_string());
                    }
                    root
                }
                None => ids[0],
            };
            for &id in &ids {
                if id != root && !scene.set_parent(id, Some(root)) {
                    return Err(format!(
                        "cannot parent object {} under the group root (cycle)",
                        id.0
                    ));
                }
            }
            if let Some(object) = scene.object_mut(root) {
                object.group = true;
            }
            selection.set(ids, Some(root));
            Ok(json!({"root": object_json(scene, scene.object(root).unwrap())}))
        }
        "ungroup_object" => {
            let id = resolve(scene, &command["object"])?;
            let root = scene
                .group_root(id)
                .ok_or("object is not part of a group")?;
            if let Some(object) = scene.object_mut(root) {
                object.group = false;
            }
            Ok(json!({"root": object_json(scene, scene.object(root).unwrap())}))
        }
        "attach_object" => {
            let child = resolve(scene, &command["object"])?;
            let parent = resolve(scene, &command["to"])?;
            let at = match command.get("location").filter(|v| !v.is_null()) {
                Some(v) => Some(vec3_from(v).ok_or("location must be [x, y, z]")?),
                None => None,
            };
            if scene.attach(child, parent, at) {
                Ok(json!({"object": object_json(scene, scene.object(child).unwrap())}))
            } else {
                Err("attach rejected (cycle or missing object)".to_string())
            }
        }
        "add_measurement" => {
            let a = vec3_from(&command["a"]).ok_or("'a' must be [x, y, z]")?;
            let b = vec3_from(&command["b"]).ok_or("'b' must be [x, y, z]")?;
            scene.add_measurement(a, b);
            Ok(json!({"length_m": (b - a).length()}))
        }
        "simulate" => {
            let action = command["action"].as_str().ok_or("missing 'action' (play|pause|stop)")?;
            match action {
                "play" => physics.play(scene),
                "pause" => physics.pause(),
                "stop" => physics.stop(scene),
                other => return Err(format!("unknown action '{other}'")),
            }
            Ok(json!({"sim_state": format!("{:?}", physics.sim_state())}))
        }
        "add_reference_image" => {
            // image bytes: from a file path (native app's filesystem) or
            // base64 passed directly
            let (default_name, bytes): (String, Vec<u8>) =
                if let Some(path) = command["path"].as_str() {
                    let bytes = std::fs::read(path)
                        .map_err(|e| format!("cannot read '{path}': {e}"))?;
                    let name = std::path::Path::new(path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Image".to_string());
                    (name, bytes)
                } else if let Some(data) = command["data_base64"].as_str() {
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|e| format!("bad data_base64: {e}"))?;
                    ("Image".to_string(), bytes)
                } else {
                    return Err("missing 'path' or 'data_base64'".to_string());
                };
            let name = command["name"]
                .as_str()
                .filter(|n| !n.trim().is_empty())
                .map(|n| n.trim().to_string())
                .unwrap_or(default_name);
            let image = crate::ref_image::make_reference(name, &bytes)?;
            let id = scene.add_reference_image(image);
            apply_ref_image_params(scene, id, command)?;
            let image = scene.reference_images().iter().find(|r| r.id == id).unwrap();
            Ok(json!({"image": ref_image_json(image)}))
        }
        "update_reference_image" => {
            let id = resolve_ref_image(scene, &command["image"])?;
            apply_ref_image_params(scene, id, command)?;
            let image = scene.reference_images().iter().find(|r| r.id == id).unwrap();
            Ok(json!({"image": ref_image_json(image)}))
        }
        "delete_reference_image" => {
            let id = resolve_ref_image(scene, &command["image"])?;
            scene.remove_reference_image(id);
            Ok(json!({}))
        }
        "calibrate_reference_image" => {
            // two points in SOURCE-IMAGE PIXELS + the real distance between
            // them; rescales the image so that span matches reality
            let id = resolve_ref_image(scene, &command["image"])?;
            let point_px = |key: &str| -> Result<(f64, f64), String> {
                let a = command[key]
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .ok_or_else(|| format!("'{key}' must be [x, y] in image pixels"))?;
                Ok((
                    a[0].as_f64().ok_or_else(|| format!("bad '{key}'"))?,
                    a[1].as_f64().ok_or_else(|| format!("bad '{key}'"))?,
                ))
            };
            let a = point_px("point_a_px")?;
            let b = point_px("point_b_px")?;
            let real_m = command["real_distance_m"]
                .as_f64()
                .filter(|v| *v > 0.0)
                .ok_or("missing 'real_distance_m' (> 0)")?;

            let image = scene
                .reference_images()
                .iter()
                .find(|r| r.id == id)
                .ok_or("reference image vanished")?;
            let (width_px, _) = crate::ref_image::decoded_size(&image.data_base64)
                .ok_or("embedded image data is not decodable")?;
            let dist_px = ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
            if dist_px < 1e-6 {
                return Err("the two points must differ".to_string());
            }
            // pixel span -> meters at the image's CURRENT scale
            let measured_m = dist_px as f32 * image.width_m / width_px.max(1) as f32;
            let old_width = image.width_m;
            let image = scene.reference_image_mut(id).unwrap();
            crate::ref_image::CalibrateTool::apply_scale(image, measured_m, real_m as f32);
            Ok(json!({
                "measured_m": measured_m,
                "old_width_m": old_width,
                "image": ref_image_json(scene.reference_images().iter().find(|r| r.id == id).unwrap()),
            }))
        }
        "get_library" => {
            let assets: Vec<Value> = library_doc.assets().iter().map(asset_json).collect();
            Ok(json!({"assets": assets}))
        }
        "create_library_object" => {
            let name = command["name"]
                .as_str()
                .filter(|n| !n.trim().is_empty())
                .ok_or("missing 'name'")?;
            let description = command["description"].as_str().unwrap_or_default();
            let objects = capture_for_library(scene, selection, command)?;
            let preview = preview_for_library(command, &objects)?;
            let id = library_doc.add_asset(name.trim(), description.trim(), objects, preview);
            apply_asset_points(library_doc, id, command)?;
            Ok(json!({"asset": asset_json(library_doc.asset(id).unwrap())}))
        }
        "update_library_object" => {
            let id = resolve_asset(library_doc, &command["asset"])?;
            // recapture contents only when 'objects' is given
            let recaptured = if command.get("objects").is_some_and(|v| !v.is_null()) {
                let objects = capture_for_library(scene, selection, command)?;
                let preview = preview_for_library(command, &objects)?;
                Some((objects, preview))
            } else {
                None
            };
            if let Some(name) = command["new_name"].as_str() {
                library_doc.rename_asset(id, name);
            }
            let asset = library_doc.asset_mut(id).ok_or("library item vanished")?;
            if let Some(description) = command["description"].as_str() {
                asset.description = description.trim().to_string();
            }
            match recaptured {
                Some((objects, preview)) => {
                    asset.objects = objects;
                    asset.preview_png_base64 = preview;
                }
                None => {
                    if let Some(data) = command["preview_png_base64"].as_str() {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .map_err(|e| format!("bad preview_png_base64: {e}"))?;
                        asset.preview_png_base64 = Some(data.to_string());
                    }
                }
            }
            apply_asset_points(library_doc, id, command)?;
            Ok(json!({"asset": asset_json(library_doc.asset(id).unwrap())}))
        }
        "delete_library_object" => {
            let id = resolve_asset(library_doc, &command["asset"])?;
            library_doc.remove_asset(id);
            Ok(json!({}))
        }
        "place_library_object" => {
            let id = resolve_asset(library_doc, &command["asset"])?;
            let location = match command.get("location").filter(|v| !v.is_null()) {
                Some(v) => Some(vec3_from(v).ok_or("location must be [x, y, z]")?),
                None => None,
            };
            let asset = library_doc.asset(id).unwrap().clone();
            // attach_to: the asset's ANCHOR lands on the attachment point
            // (location, or the target's anchor point) and the asset parents
            // there; otherwise the PIVOT lands on the location
            let target = match command.get("attach_to").filter(|v| !v.is_null()) {
                Some(reference) => Some(resolve(scene, reference)?),
                None => None,
            };
            let new_ids = match target {
                Some(target) => {
                    let point = location.unwrap_or_else(|| scene.world_anchor(target));
                    let ids = library::instantiate(scene, &asset, point - asset.anchor);
                    let roots: Vec<ObjectId> = ids
                        .iter()
                        .copied()
                        .filter(|&i| scene.object(i).is_some_and(|o| o.parent.is_none()))
                        .collect();
                    for root in roots {
                        scene.set_parent(root, Some(target));
                    }
                    ids
                }
                None => library::instantiate(
                    scene,
                    &asset,
                    location.unwrap_or(Vec3::ZERO) - asset.pivot,
                ),
            };
            let active = new_ids.first().copied();
            selection.set(new_ids.clone(), active);
            let placed: Vec<Value> = new_ids
                .iter()
                .filter_map(|&id| scene.object(id))
                .map(|o| json!({"id": o.id.0, "name": o.name}))
                .collect();
            Ok(json!({"placed": placed}))
        }
        other => Err(format!(
            "unknown cmd '{other}' (get_scene, new_scene, add_object, update_object, \
             delete_object, set_parent, attach_object, group_objects, ungroup_object, \
             add_measurement, simulate, screenshot, add_reference_image, \
             update_reference_image, delete_reference_image, calibrate_reference_image, \
             get_library, create_library_object, update_library_object, \
             delete_library_object, place_library_object)"
        )),
    }
}

/// Encode an RGBA frame as a base64 PNG. three-d's `read_color` already
/// returns rows top-down, so no flip is needed.
pub fn encode_screenshot(pixels: &[[u8; 4]], width: u32, height: u32) -> Result<String, String> {
    let mut flipped: Vec<u8> = Vec::with_capacity(pixels.len() * 4);
    for row in 0..height as usize {
        let start = row * width as usize;
        for pixel in &pixels[start..start + width as usize] {
            flipped.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
    }
    let mut png_bytes: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&flipped)
            .map_err(|e| e.to_string())?;
    }
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(&png_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Scene, Selection, PhysicsMirror, Library) {
        (
            Scene::default_scene(),
            Selection::default(),
            PhysicsMirror::new(),
            Library::default(),
        )
    }

    /// A valid 8x4 white PNG, base64-encoded (via the screenshot encoder).
    fn tiny_png_base64() -> String {
        let pixels = vec![[255u8, 255, 255, 255]; 8 * 4];
        encode_screenshot(&pixels, 8, 4).expect("encode")
    }

    #[test]
    fn reference_image_commands_roundtrip() {
        let _guard = crate::physics::ffi_test_lock();
        let (mut scene, mut sel, mut physics, mut lib) = setup();

        // add from base64 with overrides (agents may not share a filesystem)
        let response = execute(
            &json!({
                "cmd": "add_reference_image", "data_base64": tiny_png_base64(),
                "name": "floorplan", "plane": "z", "location": [1.0, 2.0, 0.0],
                "opacity": 0.7
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        let image = &response["image"];
        assert_eq!(image["name"], "floorplan");
        assert_eq!(image["plane"], "Z");
        assert_eq!(image["width_px"], 8);
        assert_eq!(image["height_px"], 4);
        // 8x4 px at the default 2 m width -> 1 m tall
        assert!((image["height_m"].as_f64().unwrap() - 1.0).abs() < 1e-5);

        // pixel-space calibration: 4 px span = 1 m at the current scale;
        // telling the app it is really 5 m must scale width 2 m -> 10 m
        let response = execute(
            &json!({
                "cmd": "calibrate_reference_image", "image": "floorplan",
                "point_a_px": [0, 2], "point_b_px": [4, 2], "real_distance_m": 5.0
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert!((response["measured_m"].as_f64().unwrap() - 1.0).abs() < 1e-5);
        assert!((response["image"]["width_m"].as_f64().unwrap() - 10.0).abs() < 1e-4);

        // update by id, then delete
        let id = response["image"]["id"].as_u64().unwrap();
        let response = execute(
            &json!({"cmd": "update_reference_image", "image": id, "rotation_deg": 90.0, "visible": false}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["image"]["visible"], false);

        let response = execute(
            &json!({"cmd": "delete_reference_image", "image": "floorplan"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert!(scene.reference_images().is_empty());

        // errors: unknown image, bad data
        let response = execute(
            &json!({"cmd": "calibrate_reference_image", "image": "nope",
                    "point_a_px": [0,0], "point_b_px": [1,0], "real_distance_m": 1.0}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);
        let response = execute(
            &json!({"cmd": "add_reference_image", "data_base64": "bm90IGFuIGltYWdl"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);
    }

    #[test]
    fn control_commands_roundtrip() {
        let _guard = crate::physics::ffi_test_lock();
        let (mut scene, mut sel, mut physics, mut lib) = setup();

        // add a red dynamic sphere at (2, 0, 3)
        let response = execute(
            &json!({
                "cmd": "add_object", "primitive": "sphere", "new_name": "Ball",
                "location": [2.0, 0.0, 3.0], "color": [1.0, 0.2, 0.2], "dynamic": true
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["name"], "Ball");

        // parent it to the default cube
        let response = execute(
            &json!({"cmd": "set_parent", "child": "Ball", "parent": "Cube"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");

        // scene reflects everything
        let response = execute(
            &json!({"cmd": "get_scene"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        let objects = response["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 2);
        let ball = objects.iter().find(|o| o["name"] == "Ball").unwrap();
        assert_eq!(ball["dynamic"], true);
        assert_eq!(ball["parent"], objects.iter().find(|o| o["name"] == "Cube").unwrap()["id"]);

        // update + measurement + errors
        let response = execute(
            &json!({"cmd": "update_object", "object": "Ball", "scale": [2.0, 2.0, 2.0]}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true);
        let response = execute(
            &json!({"cmd": "add_measurement", "a": [0, 0, 0], "b": [0, 0, 5]}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["length_m"], 5.0);
        let response = execute(
            &json!({"cmd": "delete_object", "object": "Nope"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);

        // cycle rejected through the API too
        let response = execute(
            &json!({"cmd": "set_parent", "child": "Cube", "parent": "Ball"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);
    }

    #[test]
    fn wall_commands_roundtrip() {
        let _guard = crate::physics::ffi_test_lock();
        let (mut scene, mut sel, mut physics, mut lib) = setup();

        // add a wall with explicit dimensions and a door
        let response = execute(
            &json!({
                "cmd": "add_object", "primitive": "wall", "new_name": "South",
                "length": 4.0, "height": 2.7, "thickness": 0.15,
                "cutouts": [{"offset": 1.0, "width": 0.9, "bottom": 0.0, "height": 2.1}]
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        let wall = scene.objects().iter().find(|o| o.name == "South").unwrap();
        assert_eq!(
            wall.primitive,
            Primitive::Wall { length: 4.0, height: 2.7, thickness: 0.15 }
        );
        assert_eq!(wall.cutouts.len(), 1);

        // get_scene reports the cutouts and the wall dimensions
        let response = execute(
            &json!({"cmd": "get_scene"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        let south = response["objects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["name"] == "South")
            .unwrap();
        assert_eq!(south["cutouts"].as_array().unwrap().len(), 1);
        assert_eq!(south["dimensions_m"][0], 4.0);
        assert_eq!(south["dimensions_m"][2].as_f64().unwrap() as f32, 2.7);

        // update: change the height, replace the openings with a window
        let response = execute(
            &json!({
                "cmd": "update_object", "object": "South", "height": 3.0,
                "cutouts": [{"offset": 2.5, "width": 1.2, "bottom": 0.9, "height": 1.2}]
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        let wall = scene.objects().iter().find(|o| o.name == "South").unwrap();
        assert_eq!(
            wall.primitive,
            Primitive::Wall { length: 4.0, height: 3.0, thickness: 0.15 }
        );
        assert!(!wall.cutouts[0].is_door());

        // malformed cutouts are a friendly error
        let response = execute(
            &json!({"cmd": "update_object", "object": "South", "cutouts": [{"offset": 1.0}]}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);
    }

    #[test]
    fn pivot_anchor_and_attach_commands() {
        let _guard = crate::physics::ffi_test_lock();
        let (mut scene, mut sel, mut physics, mut lib) = setup();

        // the default cube becomes a "table" with an anchor on its top face
        let response = execute(
            &json!({
                "cmd": "update_object", "object": "Cube",
                "pivot": [0.0, 0.0, -1.0], "anchor": [0.0, 0.0, 1.0]
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["object"]["pivot"][2], -1.0);
        assert_eq!(response["object"]["anchor"][2], 1.0);

        // a "cup" anchored at the bottom of its base, off to the side
        let response = execute(
            &json!({
                "cmd": "add_object", "primitive": "cylinder", "new_name": "Cup",
                "location": [5.0, 0.0, 0.5], "scale": [0.25, 0.25, 0.25],
                "anchor": [0.0, 0.0, -1.0]
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");

        // attach: cup's anchor lands on the cube's anchor (top face center)
        let response = execute(
            &json!({"cmd": "attach_object", "object": "Cup", "to": "Cube"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        let cup = scene.objects().iter().find(|o| o.name == "Cup").unwrap().id;
        let anchor_world = scene.world_anchor(cup);
        assert!(
            (anchor_world - Vec3::new(0.0, 0.0, 1.0)).length() < 1e-4,
            "{anchor_world:?}"
        );
        assert!(scene.object(cup).unwrap().parent.is_some());

        // attach at an explicit point
        let response = execute(
            &json!({"cmd": "attach_object", "object": "Cup", "to": "Cube",
                    "location": [0.5, 0.5, 1.0]}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert!((scene.world_anchor(cup) - Vec3::new(0.5, 0.5, 1.0)).length() < 1e-4);

        // library asset with explicit pivot/anchor; placement honors them
        let response = execute(
            &json!({
                "cmd": "create_library_object", "name": "CupKit", "objects": ["Cup"],
                "pivot": [0.0, 0.0, 0.25], "anchor": [1.0, 0.0, 0.0]
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["asset"]["pivot"][2], 0.25);
        assert_eq!(response["asset"]["anchor"][0], 1.0);

        // ground placement: the PIVOT lands on the location
        let response = execute(
            &json!({"cmd": "place_library_object", "asset": "CupKit",
                    "location": [10.0, 0.0, 1.0]}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        let placed = response["placed"][0]["id"].as_u64().unwrap();
        let w = scene.world_transform(ObjectId(placed));
        // the asset pivot (0,0,0.25) lands on (10,0,1): the normalized root
        // (at 0,0,0.25 in asset space) ends up exactly there
        assert!((w.location - Vec3::new(10.0, 0.0, 1.0)).length() < 1e-4, "{:?}", w.location);

        // attach placement: the ANCHOR lands on the target's anchor point
        let response = execute(
            &json!({"cmd": "place_library_object", "asset": "CupKit", "attach_to": "Cube"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        let placed = ObjectId(response["placed"][0]["id"].as_u64().unwrap());
        let object = scene.object(placed).unwrap();
        assert!(object.parent.is_some(), "attached asset must be parented");
        // asset anchor (1,0,0) landed on the cube's world anchor (0,0,1):
        // the placed root's world location is (0,0,1) - (1,0,0) + root offset
        let w = scene.world_transform(placed);
        assert!((w.location.x - (-1.0)).abs() < 1e-3, "{:?}", w.location);
    }

    #[test]
    fn group_and_ungroup_commands() {
        let _guard = crate::physics::ffi_test_lock();
        let (mut scene, mut sel, mut physics, mut lib) = setup();

        for name in ["Seat", "Leg"] {
            let response = execute(
                &json!({"cmd": "add_object", "primitive": "cube", "new_name": name}),
                &mut scene,
                &mut sel,
                &mut physics,
                &mut lib,
            );
            assert_eq!(response["ok"], true, "{response}");
        }

        // group with an explicit root: members are parented, flag set,
        // selection = the group
        let response = execute(
            &json!({"cmd": "group_objects", "objects": ["Cube", "Seat", "Leg"], "root": "Seat"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["root"]["name"], "Seat");
        assert_eq!(response["root"]["group"], true);
        let seat = scene.objects().iter().find(|o| o.name == "Seat").unwrap().id;
        let leg = scene.objects().iter().find(|o| o.name == "Leg").unwrap().id;
        assert_eq!(scene.object(leg).unwrap().parent, Some(seat));
        assert_eq!(scene.group_root(leg), Some(seat));
        assert_eq!(sel.active(), Some(seat));

        // ungroup by a MEMBER: root flag cleared, hierarchy kept
        let response = execute(
            &json!({"cmd": "ungroup_object", "object": "Leg"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["root"]["name"], "Seat");
        assert_eq!(response["root"]["group"], false);
        assert_eq!(scene.object(leg).unwrap().parent, Some(seat));

        // errors: ungroup on a non-group, group with < 2 objects, bad root
        let response = execute(
            &json!({"cmd": "ungroup_object", "object": "Leg"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);
        let response = execute(
            &json!({"cmd": "group_objects", "objects": ["Seat"]}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);
        let response = execute(
            &json!({"cmd": "group_objects", "objects": ["Seat", "Leg"], "root": "Cube"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);
    }

    #[test]
    fn library_commands_roundtrip() {
        let _guard = crate::physics::ffi_test_lock();
        let (mut scene, mut sel, mut physics, mut lib) = setup();

        // build a two-object group and store it by explicit reference
        let response = execute(
            &json!({
                "cmd": "add_object", "primitive": "cylinder", "new_name": "Leg",
                "location": [3.0, 0.0, 1.0]
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        let response = execute(
            &json!({"cmd": "set_parent", "child": "Leg", "parent": "Cube"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");

        // selecting the root captures the child too; preview auto-renders
        let response = execute(
            &json!({
                "cmd": "create_library_object", "name": "Table",
                "description": "cube with a leg", "objects": ["Cube"]
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["asset"]["name"], "Table");
        assert_eq!(response["asset"]["object_count"], 2);
        assert_eq!(response["asset"]["has_preview"], true);

        // list it
        let response = execute(
            &json!({"cmd": "get_library"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["assets"].as_array().unwrap().len(), 1);

        // place two copies; each gets fresh unique names and is selected
        let response = execute(
            &json!({"cmd": "place_library_object", "asset": "Table", "location": [5.0, 5.0, 0.0]}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["placed"].as_array().unwrap().len(), 2);
        assert_eq!(sel.selected().len(), 2);
        // the placed instance is one GROUP rooted at the placed root; an
        // update_object group=false ungroups it
        let placed_root = ObjectId(response["placed"][0]["id"].as_u64().unwrap());
        assert!(scene.object(placed_root).unwrap().group);
        let response = execute(
            &json!({"cmd": "update_object", "object": placed_root.0, "group": false}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["object"]["group"], false);
        let response = execute(
            &json!({"cmd": "place_library_object", "asset": "Table"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(scene.objects().len(), 6); // 2 originals + 2 placements of 2

        // update: rename + description without touching the contents
        let response = execute(
            &json!({
                "cmd": "update_library_object", "asset": "Table",
                "new_name": "Desk", "description": "renamed"
            }),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["asset"]["name"], "Desk");
        assert_eq!(response["asset"]["object_count"], 2);

        // delete, then errors on the gone item
        let response = execute(
            &json!({"cmd": "delete_library_object", "asset": "Desk"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert!(lib.assets().is_empty());
        let response = execute(
            &json!({"cmd": "place_library_object", "asset": "Desk"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);

        // create with nothing selected and no refs is a friendly error
        sel.set(Vec::new(), None);
        let response = execute(
            &json!({"cmd": "create_library_object", "name": "Empty"}),
            &mut scene,
            &mut sel,
            &mut physics,
            &mut lib,
        );
        assert_eq!(response["ok"], false);
    }

    #[test]
    fn screenshot_encoder_produces_png() {
        let pixels = vec![[255u8, 0, 0, 255]; 4];
        let b64 = encode_screenshot(&pixels, 2, 2).unwrap();
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
    }
}
