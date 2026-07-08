//! MCP (Model Context Protocol) server for the 3D modeler.
//!
//! Speaks newline-delimited JSON-RPC 2.0 on stdio (the standard MCP stdio
//! transport) and forwards tool calls to the running native modeler app's
//! control API on localhost (see modeler-app/src/control.rs).
//!
//! Hand-rolled on purpose: the protocol subset MCP clients need — initialize,
//! tools/list, tools/call, ping — is small, and this keeps the binary free of
//! async runtimes.

// the tool_definitions() json! literal nests deeply (wall cutout schemas)
#![recursion_limit = "256"]

use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";

fn control_url() -> String {
    let port = std::env::var("MODELER_CONTROL_PORT").unwrap_or_else(|_| "8323".to_string());
    format!("http://127.0.0.1:{port}/")
}

/// Send a command to the modeler app; friendly error when it isn't running.
fn call_modeler(command: Value) -> Result<Value, String> {
    let response = minreq::post(control_url())
        .with_header("Content-Type", "application/json")
        .with_body(command.to_string())
        .with_timeout(15)
        .send()
        .map_err(|_| {
            "The 3D modeler app is not running (or the control port is blocked). \
             Start it natively with `cargo run -p modeler-app` from the 3dmodeler \
             directory — the MCP bridge only works with the native app, not the \
             browser build."
                .to_string()
        })?;
    let body: Value = serde_json::from_str(response.as_str().map_err(|e| e.to_string())?)
        .map_err(|e| format!("bad response from the app: {e}"))?;
    if body["ok"] == json!(true) {
        Ok(body)
    } else {
        Err(body["error"].as_str().unwrap_or("unknown error").to_string())
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "get_scene",
            "description": "Read the full scene: all objects (id, name, primitive, local & world transforms, parent, pivot & anchor points, group flag, color, physics flags, dimensions in meters), measurements and the simulation state. Call this first to see what exists.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "screenshot",
            "description": "Render the current viewport and return it as a PNG image. Use it to visually inspect the scene you are building.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "set_view",
            "description": "Switch the viewport shading and lighting before a screenshot. Shading: wireframe (edges only), solid (neutral gray studio), shaded (full materials — the default). Lighting applies to shaded: studio (built-in rig) or scene (the scene's light objects, with shadows).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "shading": {"type": "string", "enum": ["wireframe", "solid", "shaded"]},
                    "lighting": {"type": "string", "enum": ["studio", "scene"]}
                }
            }
        },
        {
            "name": "add_object",
            "description": "Add a primitive to the scene. Units are meters; the world is Z-up (the ground plane is XY). New objects appear at the origin unless a location is given. A 'wall' runs along its local +X axis from its origin, stands on z=0, and takes length/height/thickness plus rectangular door/window cutouts. 'light'/'sun'/'spot' add light sources (viewport Shaded mode with Scene lighting): color/intensity set brightness, sun & spot shine along their local -Z (aim with rotation_euler_deg), spot takes spot_angle_deg, sun & spot cast shadows unless disabled.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "primitive": {"type": "string", "enum": ["plane", "cube", "sphere", "icosphere", "cylinder", "cone", "torus", "wall", "floor", "empty", "light", "sun", "spot"]},
                    "intensity": {"type": "number", "description": "Lights only: brightness multiplier (default 3 point, 1.5 sun, 5 spot)"},
                    "spot_angle_deg": {"type": "number", "description": "Spot lights only: full cone angle in degrees (default 45)"},
                    "shadows": {"type": "boolean", "description": "Sun/spot lights only: cast shadows (default true; point lights never do)"},
                    "length": {"type": "number", "description": "Wall only: length in meters (default 2)"},
                    "height": {"type": "number", "description": "Wall only: height in meters (default 2.5)"},
                    "thickness": {"type": "number", "description": "Wall only: thickness in meters (default 0.2)"},
                    "cutouts": {"type": "array", "items": {"type": "object", "properties": {
                        "offset": {"type": "number", "description": "Distance from the wall start to the opening's left edge, meters"},
                        "width": {"type": "number"},
                        "bottom": {"type": "number", "description": "Sill height above the floor; 0 for doors"},
                        "height": {"type": "number"}
                    }, "required": ["offset", "width", "bottom", "height"]}, "description": "Wall only: door/window openings cut through the wall"},
                    "new_name": {"type": "string", "description": "Optional name (defaults to Blender-style Cube, Cube.001, ...)"},
                    "location": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] in meters"},
                    "rotation_euler_deg": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] Euler angles in degrees"},
                    "scale": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] scale factors"},
                    "color": {"type": "array", "items": {"type": "number"}, "description": "[r, g, b] each 0..1"},
                    "smooth": {"type": "boolean", "description": "Smooth shading"},
                    "dynamic": {"type": "boolean", "description": "Falls & collides when the physics simulation plays"},
                    "density": {"type": "number"},
                    "show_label": {"type": "boolean", "description": "Show the name as a viewport label"},
                    "show_dimensions": {"type": "boolean", "description": "Show W×D×H dimensions in the viewport"},
                    "pivot": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] local-space pivot point: interactive rotations (R) spin the object around it"},
                    "anchor": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] local-space anchor point: where the object attaches to another object (attach_object)"},
                    "group": {"type": "boolean", "description": "Group root flag: this object + its (later-parented) descendants select as ONE unit in the viewport"}
                },
                "required": ["primitive"]
            }
        },
        {
            "name": "add_floor",
            "description": "Add a floor slab under walls, standing on z=0. When the walls chain end-to-end into a closed loop the floor follows their shape (centerline polygon, concave rooms included); otherwise it covers their bounding rectangle. Takes the same optional object fields as add_object (color, new_name, ...).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "walls": {"type": "array", "items": {"type": "string"}, "description": "Wall names (or ids as strings) to size the floor from; omit to use every wall in the scene"},
                    "new_name": {"type": "string"},
                    "color": {"type": "array", "items": {"type": "number"}, "description": "[r, g, b] each 0..1"}
                }
            }
        },
        {
            "name": "break_into_bricks",
            "description": "Replace an object with individual dynamic bricks in a running bond (they collide and tumble when the simulation plays). Walls keep their door/window openings; other shapes (cubes, spheres, cones, floors, ...) are filled with bricks, curved surfaces getting a stepped approximation. The bricks land in a '<name> bricks' folder that can rebuild the original.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "object": {"type": "string", "description": "Object name (or id as string)"}
                },
                "required": ["object"]
            }
        },
        {
            "name": "update_object",
            "description": "Change any properties of an existing object (same optional fields as add_object, plus new_name to rename). Reference the object by name or id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "object": {"type": "string", "description": "Object name (or id as string)"},
                    "new_name": {"type": "string"},
                    "length": {"type": "number", "description": "Wall only: length in meters"},
                    "height": {"type": "number", "description": "Wall only: height in meters"},
                    "thickness": {"type": "number", "description": "Wall only: thickness in meters"},
                    "cutouts": {"type": "array", "items": {"type": "object", "properties": {
                        "offset": {"type": "number"},
                        "width": {"type": "number"},
                        "bottom": {"type": "number", "description": "Sill height; 0 for doors"},
                        "height": {"type": "number"}
                    }, "required": ["offset", "width", "bottom", "height"]}, "description": "Wall only: REPLACES the full list of door/window openings"},
                    "location": {"type": "array", "items": {"type": "number"}},
                    "rotation_euler_deg": {"type": "array", "items": {"type": "number"}},
                    "scale": {"type": "array", "items": {"type": "number"}},
                    "color": {"type": "array", "items": {"type": "number"}, "description": "[r, g, b] 0..1; on lights this sets the light color"},
                    "light_kind": {"type": "string", "enum": ["point", "sun", "spot"], "description": "Lights only: change the light kind"},
                    "intensity": {"type": "number", "description": "Lights only: brightness multiplier"},
                    "spot_angle_deg": {"type": "number", "description": "Spot lights only: full cone angle in degrees"},
                    "shadows": {"type": "boolean", "description": "Sun/spot lights only: cast shadows"},
                    "smooth": {"type": "boolean"},
                    "visible": {"type": "boolean"},
                    "dynamic": {"type": "boolean"},
                    "density": {"type": "number"},
                    "show_label": {"type": "boolean"},
                    "show_dimensions": {"type": "boolean"},
                    "pivot": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] local-space rotation pivot"},
                    "anchor": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] local-space attachment point"},
                    "group": {"type": "boolean", "description": "Group root flag: this object + descendants select as ONE unit in the viewport (placed library assets set it on their root; false = ungroup)"}
                },
                "required": ["object"]
            }
        },
        {
            "name": "delete_object",
            "description": "Delete an object by name or id. Its children (if any) keep their world position and become unparented.",
            "inputSchema": {
                "type": "object",
                "properties": {"object": {"type": "string"}},
                "required": ["object"]
            }
        },
        {
            "name": "set_parent",
            "description": "Parent one object to another (child follows the parent's transforms; world placement is preserved at link time). Pass parent = null to unparent. Cycles are rejected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "child": {"type": "string"},
                    "parent": {"type": ["string", "null"]}
                },
                "required": ["child"]
            }
        },
        {
            "name": "group_objects",
            "description": "Group scene objects into ONE unit: every object is parented to the root (world placement preserved) and the root gets the group flag — viewport clicks then select the whole assembly, and it moves/rotates/scales as one. Placed library assets come pre-grouped. Returns the group root.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "objects": {"type": "array", "items": {"type": "string"}, "description": "Names/ids of the objects to group (at least 2)"},
                    "root": {"type": "string", "description": "Which of 'objects' becomes the group root (default: the first)"}
                },
                "required": ["objects"]
            }
        },
        {
            "name": "ungroup_object",
            "description": "Break a group apart: pass any member (or the root) and the group flag is cleared, so parts are selectable individually again. The parent hierarchy is KEPT — use set_parent with parent=null on the children to fully detach them. Returns the former root.",
            "inputSchema": {
                "type": "object",
                "properties": {"object": {"type": "string", "description": "Any object of the group (name or id)"}},
                "required": ["object"]
            }
        },
        {
            "name": "attach_object",
            "description": "Attach one object to another: the object is MOVED so its anchor point lands on the attachment point (the target's anchor point, or an explicit world location), then parented there. Set anchor points via add/update_object. Cycles are rejected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "object": {"type": "string", "description": "Object to attach (name or id)"},
                    "to": {"type": "string", "description": "Target object it attaches to"},
                    "location": {"type": "array", "items": {"type": "number"}, "description": "Optional [x, y, z] world attachment point (default: the target's anchor point)"}
                },
                "required": ["object", "to"]
            }
        },
        {
            "name": "add_measurement",
            "description": "Add a persistent ruler measurement between two world-space points; returns the distance in meters and draws it in the viewport.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "a": {"type": "array", "items": {"type": "number"}},
                    "b": {"type": "array", "items": {"type": "number"}}
                },
                "required": ["a", "b"]
            }
        },
        {
            "name": "simulate",
            "description": "Control the physics simulation: 'play' drops dynamic objects under gravity onto the ground plane / other objects, 'pause' freezes, 'stop' restores the scene to its pre-play state.",
            "inputSchema": {
                "type": "object",
                "properties": {"action": {"type": "string", "enum": ["play", "pause", "stop"]}},
                "required": ["action"]
            }
        },
        {
            "name": "new_scene",
            "description": "Reset to the default scene (a single cube). Destroys all current objects.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "add_reference_image",
            "description": "Place a PNG/JPEG in the viewport as a semi-transparent reference image locked to an axis plane (x = side/YZ, y = front/XZ, z = floor/XY). Pass either a file path readable by the modeler app or the raw image bytes as base64. The image keeps its aspect ratio; width_m sets its world size. It is embedded in the scene file, so it saves/loads with the scene. get_scene lists all reference images including their pixel dimensions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to a .png/.jpg on the machine running the modeler app"},
                    "data_base64": {"type": "string", "description": "Alternative to 'path': base64-encoded PNG/JPEG bytes"},
                    "name": {"type": "string", "description": "Optional name (unique-suffixed like objects)"},
                    "plane": {"type": "string", "enum": ["x", "y", "z"], "description": "Axis the image plane is perpendicular to (default y = front view)"},
                    "location": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] center of the image in meters"},
                    "rotation_deg": {"type": "number", "description": "In-plane rotation in degrees"},
                    "width_m": {"type": "number", "description": "World width in meters (default 2; height follows the aspect ratio)"},
                    "opacity": {"type": "number", "description": "0..1, default 0.5"},
                    "visible": {"type": "boolean"}
                }
            }
        },
        {
            "name": "update_reference_image",
            "description": "Change a reference image's plane, location, rotation_deg, width_m, opacity, visibility or name. Reference it by name or id (see get_scene).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "image": {"type": "string", "description": "Reference image name (or id as string)"},
                    "new_name": {"type": "string"},
                    "plane": {"type": "string", "enum": ["x", "y", "z"]},
                    "location": {"type": "array", "items": {"type": "number"}},
                    "rotation_deg": {"type": "number"},
                    "width_m": {"type": "number"},
                    "opacity": {"type": "number"},
                    "visible": {"type": "boolean"}
                },
                "required": ["image"]
            }
        },
        {
            "name": "delete_reference_image",
            "description": "Remove a reference image from the scene (by name or id).",
            "inputSchema": {
                "type": "object",
                "properties": {"image": {"type": "string"}},
                "required": ["image"]
            }
        },
        {
            "name": "get_library",
            "description": "List the object library: reusable assets (each a named collection of objects with a description and preview) that can be placed into any scene with place_library_object. The library persists across scenes and sessions.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "create_library_object",
            "description": "Save a group of scene objects as a reusable library asset. Pass 'objects' (names/ids — their children are captured automatically) or omit it to capture the user's current selection. The group is stored normalized: centered in x/y with its lowest point at z=0, so placing it at a point puts it ON that point. A small isometric preview image is rendered automatically unless preview_png_base64 is given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Asset name (unique-suffixed like Table, Table.001, ...)"},
                    "description": {"type": "string"},
                    "objects": {"type": "array", "items": {"type": "string"}, "description": "Scene object names/ids to capture (children included). Defaults to the current selection."},
                    "preview_png_base64": {"type": "string", "description": "Optional custom preview image (PNG, base64)"},
                    "pivot": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] asset-space pivot: lands on the location when placed on the ground (default [0,0,0] = footprint center at the lowest point)"},
                    "anchor": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] asset-space anchor: lands on the attachment point when placed with attach_to"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "update_library_object",
            "description": "Update a library asset by name or id: rename (new_name), change the description, replace its contents with new scene objects ('objects', regenerates the preview), or set a custom preview_png_base64.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "asset": {"type": "string", "description": "Library asset name (or id as string)"},
                    "new_name": {"type": "string"},
                    "description": {"type": "string"},
                    "objects": {"type": "array", "items": {"type": "string"}, "description": "Replace the asset's contents with these scene objects (children included)"},
                    "preview_png_base64": {"type": "string"},
                    "pivot": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] asset-space placement/rotation reference"},
                    "anchor": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] asset-space attachment point"}
                },
                "required": ["asset"]
            }
        },
        {
            "name": "delete_library_object",
            "description": "Delete an asset from the object library (by name or id). Scene objects are not affected.",
            "inputSchema": {
                "type": "object",
                "properties": {"asset": {"type": "string"}},
                "required": ["asset"]
            }
        },
        {
            "name": "place_library_object",
            "description": "Instantiate a library asset into the scene. Default: the asset's PIVOT point lands on 'location' ([0,0,0] if omitted). With 'attach_to': the asset's ANCHOR point lands on the attachment point (location, or the target object's anchor point) and the asset is parented to that object. Objects get fresh ids and unique names, hierarchy preserved, and the instance is GROUPED under one root (clicks select it as one unit; update_object group=false ungroups); the new objects become the selection. Returns their ids and names.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "asset": {"type": "string", "description": "Library asset name (or id as string)"},
                    "location": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] in meters"},
                    "attach_to": {"type": "string", "description": "Optional scene object (name or id) to attach the placed asset to"}
                },
                "required": ["asset"]
            }
        },
        {
            "name": "calibrate_reference_image",
            "description": "Scale a reference image to real-world size from two points: give two pixel coordinates IN THE SOURCE IMAGE (origin top-left; get_scene reports width_px/height_px) and the real distance between them in meters. The image is rescaled so that pixel span matches the distance — e.g. a blueprint's known 4 m wall. Returns the currently-measured span and the updated image.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "image": {"type": "string", "description": "Reference image name (or id as string)"},
                    "point_a_px": {"type": "array", "items": {"type": "number"}, "description": "[x, y] in source-image pixels"},
                    "point_b_px": {"type": "array", "items": {"type": "number"}, "description": "[x, y] in source-image pixels"},
                    "real_distance_m": {"type": "number", "description": "Real-world distance between the two points, meters"}
                },
                "required": ["image", "point_a_px", "point_b_px", "real_distance_m"]
            }
        }
    ])
}

/// Execute one MCP tool call and produce the MCP result content.
fn handle_tool_call(name: &str, arguments: &Value) -> Value {
    let command = match name {
        "get_scene" => json!({"cmd": "get_scene"}),
        "screenshot" => json!({"cmd": "screenshot"}),
        "new_scene" => json!({"cmd": "new_scene"}),
        "get_library" => json!({"cmd": "get_library"}),
        "add_object" | "add_floor" | "break_into_bricks" | "update_object" | "delete_object" | "set_parent" | "attach_object"
        | "group_objects" | "ungroup_object" | "add_measurement" | "simulate" | "set_view"
        | "add_reference_image" | "update_reference_image" | "delete_reference_image"
        | "calibrate_reference_image" | "create_library_object" | "update_library_object"
        | "delete_library_object" | "place_library_object" => {
            let mut command = arguments.clone();
            if !command.is_object() {
                command = json!({});
            }
            command["cmd"] = json!(name);
            command
        }
        other => {
            return json!({
                "content": [{"type": "text", "text": format!("unknown tool '{other}'")}],
                "isError": true
            })
        }
    };

    match call_modeler(command) {
        Err(message) => json!({
            "content": [{"type": "text", "text": message}],
            "isError": true
        }),
        Ok(mut body) => {
            if name == "screenshot" {
                let data = body["png_base64"].as_str().unwrap_or_default().to_string();
                return json!({
                    "content": [{"type": "image", "data": data, "mimeType": "image/png"}]
                });
            }
            // strip the transport field; return the payload as pretty JSON
            if let Some(map) = body.as_object_mut() {
                map.remove("ok");
            }
            let text = if body.as_object().is_some_and(|m| m.is_empty()) {
                "done".to_string()
            } else {
                serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
            };
            json!({"content": [{"type": "text", "text": text}]})
        }
    }
}

fn respond(id: &Value, result: Value) {
    let message = json!({"jsonrpc": "2.0", "id": id, "result": result});
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

fn respond_error(id: &Value, code: i64, message: &str) {
    let message = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    });
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

fn main() {
    let stdin = std::io::stdin().lock();
    for line in stdin.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message["id"].clone();
        let method = message["method"].as_str().unwrap_or_default();

        match method {
            "initialize" => respond(
                &id,
                json!({
                    "protocolVersion": message["params"]["protocolVersion"]
                        .as_str()
                        .unwrap_or(PROTOCOL_VERSION),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "modeler-mcp", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Controls a running 3D modeler (Z-up, meters). Start with get_scene to see the current contents, use screenshot to look at the viewport."
                }),
            ),
            "ping" => respond(&id, json!({})),
            "tools/list" => respond(&id, json!({"tools": tool_definitions()})),
            "tools/call" => {
                let name = message["params"]["name"].as_str().unwrap_or_default();
                let arguments = &message["params"]["arguments"];
                let result = handle_tool_call(name, arguments);
                respond(&id, result);
            }
            // notifications (no id) are acknowledged silently
            _ if id.is_null() => {}
            other => respond_error(&id, -32601, &format!("method not found: {other}")),
        }
    }
}
