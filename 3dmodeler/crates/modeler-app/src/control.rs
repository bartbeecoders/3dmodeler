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
use modeler_core::{ObjectId, Primitive, Scene, Transform};
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
    ) {
        while let Ok((command, reply)) = self.requests.try_recv() {
            self.commands_handled += 1;
            self.last_command = Some(std::time::Instant::now());
            if command["cmd"] == "screenshot" {
                self.pending_screenshots.push(reply);
                continue;
            }
            let response = execute(&command, scene, selection, physics);
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
    json!({
        "id": object.id.0,
        "name": object.name,
        "primitive": format!("{:?}", object.primitive),
        "location": [object.transform.location.x, object.transform.location.y, object.transform.location.z],
        "rotation_euler_deg": [rx.to_degrees(), ry.to_degrees(), rz.to_degrees()],
        "scale": [object.transform.scale.x, object.transform.scale.y, object.transform.scale.z],
        "world_location": [world.location.x, world.location.y, world.location.z],
        "parent": object.parent.map(|p| p.0),
        "visible": object.visible,
        "smooth": object.smooth,
        "dynamic": object.dynamic,
        "density": object.density,
        "color": object.material.base_color,
        "show_label": object.show_label,
        "show_dimensions": object.show_dimensions,
        "dimensions_m": [dims.x, dims.y, dims.z],
    })
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

    let object = scene.object_mut(id).ok_or("object vanished")?;
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
    if let Some(v) = params.get("new_name").and_then(Value::as_str) {
        if !v.trim().is_empty() {
            object.name = v.trim().to_string();
        }
    }
    Ok(())
}

pub fn execute(
    command: &Value,
    scene: &mut Scene,
    selection: &mut Selection,
    physics: &mut PhysicsMirror,
) -> Value {
    let result = execute_inner(command, scene, selection, physics);
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
            let reference_images: Vec<Value> = scene
                .reference_images()
                .iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "name": r.name,
                        "plane": format!("{:?}", r.plane),
                        "location": [r.location.x, r.location.y, r.location.z],
                        "rotation_deg": r.rotation_deg,
                        "width_m": r.width_m,
                        "height_m": r.height_m(),
                        "opacity": r.opacity,
                        "visible": r.visible,
                    })
                })
                .collect();
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
                .ok_or("missing 'primitive' (plane|cube|sphere|icosphere|cylinder|cone|torus)")?;
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
        other => Err(format!(
            "unknown cmd '{other}' (get_scene, new_scene, add_object, update_object, \
             delete_object, set_parent, add_measurement, simulate, screenshot)"
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

    fn setup() -> (Scene, Selection, PhysicsMirror) {
        (Scene::default_scene(), Selection::default(), PhysicsMirror::new())
    }

    #[test]
    fn control_commands_roundtrip() {
        let (mut scene, mut sel, mut physics) = setup();

        // add a red dynamic sphere at (2, 0, 3)
        let response = execute(
            &json!({
                "cmd": "add_object", "primitive": "sphere", "new_name": "Ball",
                "location": [2.0, 0.0, 3.0], "color": [1.0, 0.2, 0.2], "dynamic": true
            }),
            &mut scene,
            &mut sel,
            &mut physics,
        );
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["name"], "Ball");

        // parent it to the default cube
        let response = execute(
            &json!({"cmd": "set_parent", "child": "Ball", "parent": "Cube"}),
            &mut scene,
            &mut sel,
            &mut physics,
        );
        assert_eq!(response["ok"], true, "{response}");

        // scene reflects everything
        let response = execute(&json!({"cmd": "get_scene"}), &mut scene, &mut sel, &mut physics);
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
        );
        assert_eq!(response["ok"], true);
        let response = execute(
            &json!({"cmd": "add_measurement", "a": [0, 0, 0], "b": [0, 0, 5]}),
            &mut scene,
            &mut sel,
            &mut physics,
        );
        assert_eq!(response["length_m"], 5.0);
        let response = execute(
            &json!({"cmd": "delete_object", "object": "Nope"}),
            &mut scene,
            &mut sel,
            &mut physics,
        );
        assert_eq!(response["ok"], false);

        // cycle rejected through the API too
        let response = execute(
            &json!({"cmd": "set_parent", "child": "Cube", "parent": "Ball"}),
            &mut scene,
            &mut sel,
            &mut physics,
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
