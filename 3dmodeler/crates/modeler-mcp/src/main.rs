//! MCP (Model Context Protocol) server for the 3D modeler.
//!
//! Speaks newline-delimited JSON-RPC 2.0 on stdio (the standard MCP stdio
//! transport) and forwards tool calls to the running native modeler app's
//! control API on localhost (see modeler-app/src/control.rs).
//!
//! Hand-rolled on purpose: the protocol subset MCP clients need — initialize,
//! tools/list, tools/call, ping — is small, and this keeps the binary free of
//! async runtimes.

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
            "description": "Read the full scene: all objects (id, name, primitive, local & world transforms, parent, color, physics flags, dimensions in meters), measurements and the simulation state. Call this first to see what exists.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "screenshot",
            "description": "Render the current viewport and return it as a PNG image. Use it to visually inspect the scene you are building.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "add_object",
            "description": "Add a primitive to the scene. Units are meters; the world is Z-up (the ground plane is XY). New objects appear at the origin unless a location is given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "primitive": {"type": "string", "enum": ["plane", "cube", "sphere", "icosphere", "cylinder", "cone", "torus"]},
                    "new_name": {"type": "string", "description": "Optional name (defaults to Blender-style Cube, Cube.001, ...)"},
                    "location": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] in meters"},
                    "rotation_euler_deg": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] Euler angles in degrees"},
                    "scale": {"type": "array", "items": {"type": "number"}, "description": "[x, y, z] scale factors"},
                    "color": {"type": "array", "items": {"type": "number"}, "description": "[r, g, b] each 0..1"},
                    "smooth": {"type": "boolean", "description": "Smooth shading"},
                    "dynamic": {"type": "boolean", "description": "Falls & collides when the physics simulation plays"},
                    "density": {"type": "number"},
                    "show_label": {"type": "boolean", "description": "Show the name as a viewport label"},
                    "show_dimensions": {"type": "boolean", "description": "Show W×D×H dimensions in the viewport"}
                },
                "required": ["primitive"]
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
                    "location": {"type": "array", "items": {"type": "number"}},
                    "rotation_euler_deg": {"type": "array", "items": {"type": "number"}},
                    "scale": {"type": "array", "items": {"type": "number"}},
                    "color": {"type": "array", "items": {"type": "number"}},
                    "smooth": {"type": "boolean"},
                    "visible": {"type": "boolean"},
                    "dynamic": {"type": "boolean"},
                    "density": {"type": "number"},
                    "show_label": {"type": "boolean"},
                    "show_dimensions": {"type": "boolean"}
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
        }
    ])
}

/// Execute one MCP tool call and produce the MCP result content.
fn handle_tool_call(name: &str, arguments: &Value) -> Value {
    let command = match name {
        "get_scene" => json!({"cmd": "get_scene"}),
        "screenshot" => json!({"cmd": "screenshot"}),
        "new_scene" => json!({"cmd": "new_scene"}),
        "add_object" | "update_object" | "delete_object" | "set_parent" | "add_measurement"
        | "simulate" => {
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
