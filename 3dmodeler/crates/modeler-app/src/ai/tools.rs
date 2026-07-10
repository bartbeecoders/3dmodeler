//! The assistant's tool surface: the command set from `commands.rs`
//! described as JSON-Schema tools, plus the system prompt that turns a
//! general model into a 3D modeling assistant.
//!
//! Tool names equal command names, so dispatch is: merge `{"cmd": name}`
//! into the arguments and hand it to `commands::execute`. Adding a command
//! there + a spec here makes it available to every provider at once.

use super::ToolContext;
use crate::commands;
use modeler_ai::ToolSpec;
use serde_json::{json, Value};

pub fn system_prompt() -> String {
    r#"You are an experienced 3D modeling assistant embedded in a 3D modeler. You build and edit the user's live scene by calling tools.

WORLD
- Z is up. Units are meters. The ground plane is z=0.
- Objects have location [x,y,z], rotation_euler_deg [x,y,z], scale [x,y,z].
- Primitives at scale 1: cube is 2 m per side (spans -1..1 in each local axis), sphere radius 1, cylinder/cone radius 1 height 2, plane 2x2, torus major radius 1. So a cube with scale [0.5,0.5,2] is 1x1x4 m, and its center must sit at z=2 to rest on the ground.
- Colors are [r,g,b] with components 0..1.

WORKFLOW
- Call get_scene first when you need to know what exists (ids, names, transforms). Don't guess ids.
- Build composite structures from many primitives. Give every object a meaningful new_name, group logical assemblies with group_objects, and keep parts aligned and touching (no floating or intersecting parts unless intended).
- After building or changing something substantial, call screenshot to check your work visually, then fix what looks wrong. The viewport shows the whole scene.
- Lights: primitives "light" (point), "sun", "spot" with color, intensity, spot_angle_deg, shadows. Lights only affect the render when the viewport lighting is "scene" — call set_view {"lighting":"scene"} when the user cares about lighting/mood. "Night" = dim bluish sun (intensity ~0.2) + warm point/spot lights; "day" = one strong sun (intensity ~3).
- Reuse: create_library_object captures objects as a named asset; place_library_object stamps copies (a whole building, tree, lamppost). For a city: build one of each thing, save to the library, then place many instances — far cheaper than re-modeling.
- new_scene erases everything without confirmation — only call it when the user explicitly asks for a fresh/empty scene.
- Physics: simulate {"action":"play"|"pause"|"stop"} runs the box3d simulation; objects with dynamic=true fall and collide.
- Scale sanity: a person is ~1.8 m, a door ~2.1x0.9 m, a storey ~3 m, a car ~4.5 m long. Keep proportions realistic unless asked otherwise.

STYLE
- Be concise. Say what you built/changed in one or two sentences; no tool-by-tool narration.
- If a request is ambiguous, make a reasonable choice and state it briefly rather than asking.
- You may be shown screenshots (your screenshot tool). Judge composition, proportions and lighting from them and iterate."#
        .to_string()
}

/// Schema fragments shared by add_object / update_object.
fn object_properties() -> Value {
    json!({
        "new_name": {"type": "string", "description": "object name"},
        "location": {"type": "array", "items": {"type": "number"}, "description": "[x,y,z] meters"},
        "rotation_euler_deg": {"type": "array", "items": {"type": "number"}, "description": "[x,y,z] degrees"},
        "scale": {"type": "array", "items": {"type": "number"}, "description": "[x,y,z] factors"},
        "color": {"type": "array", "items": {"type": "number"}, "description": "[r,g,b] 0..1"},
        "smooth": {"type": "boolean", "description": "smooth shading"},
        "subdivision": {"type": "integer", "description": "subsurf level 0..4"},
        "visible": {"type": "boolean"},
        "dynamic": {"type": "boolean", "description": "physics: falls & collides when simulating"},
        "density": {"type": "number", "description": "kg/m³ for dynamic objects"},
        "pivot": {"type": "array", "items": {"type": "number"}, "description": "pivot point, object space"},
        "anchor": {"type": "array", "items": {"type": "number"}, "description": "attach point, object space"},
        "intensity": {"type": "number", "description": "lights only"},
        "spot_angle_deg": {"type": "number", "description": "spot lights only, 1..160"},
        "shadows": {"type": "boolean", "description": "lights only"},
        "length": {"type": "number", "description": "walls only, meters"},
        "height": {"type": "number", "description": "walls only, meters"},
        "thickness": {"type": "number", "description": "walls only, meters"},
        "cutouts": {
            "type": "array",
            "description": "walls only: door/window openings, replaces the list",
            "items": {"type": "object", "properties": {
                "offset": {"type": "number"}, "width": {"type": "number"},
                "bottom": {"type": "number"}, "height": {"type": "number"}
            }}
        }
    })
}

fn object_ref(description: &str) -> Value {
    json!({"type": ["string", "integer"], "description": description})
}

fn tool(name: &'static str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name,
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    }
}

/// Every tool the assistant may call.
pub fn catalog() -> Vec<ToolSpec> {
    let mut add_properties = object_properties();
    add_properties["primitive"] = json!({
        "type": "string",
        "enum": ["plane", "cube", "sphere", "icosphere", "cylinder", "cone", "torus",
                 "wall", "floor", "empty", "light", "sun", "spot"],
        "description": "what to add ('light' = point light)"
    });
    let mut update_properties = object_properties();
    update_properties["object"] = object_ref("object name or id");
    update_properties["light_kind"] =
        json!({"type": "string", "enum": ["point", "sun", "spot"], "description": "retype a light"});
    update_properties["group"] =
        json!({"type": "boolean", "description": "false = ungroup a placed asset"});

    vec![
        tool(
            "get_scene",
            "Everything in the scene: objects (ids, names, transforms, colors, lights, walls), measurements, reference images, simulation state.",
            json!({}),
            &[],
        ),
        tool("add_object", "Add a primitive to the scene.", add_properties, &["primitive"]),
        tool("update_object", "Change any properties of an object.", update_properties, &["object"]),
        tool(
            "delete_object",
            "Remove an object (children survive, re-rooted).",
            json!({"object": object_ref("object name or id")}),
            &["object"],
        ),
        tool(
            "set_parent",
            "Parent child to parent (world transform preserved); parent=null clears.",
            json!({
                "child": object_ref("child name or id"),
                "parent": object_ref("parent name or id, or null to unparent")
            }),
            &["child"],
        ),
        tool(
            "attach_object",
            "Snap an object's anchor onto a target's anchor point (or an explicit world location) and parent it there.",
            json!({
                "object": object_ref("object to attach"),
                "to": object_ref("target object"),
                "location": {"type": "array", "items": {"type": "number"}, "description": "optional world point [x,y,z]"}
            }),
            &["object", "to"],
        ),
        tool(
            "group_objects",
            "Group objects so they select and move as one; the root carries the group.",
            json!({
                "objects": {"type": "array", "items": {"type": ["string", "integer"]}, "description": "at least 2 names/ids"},
                "root": object_ref("optional: which member is the root (default: first)")
            }),
            &["objects"],
        ),
        tool(
            "ungroup_object",
            "Dissolve the group an object belongs to (hierarchy is kept).",
            json!({"object": object_ref("any group member")}),
            &["object"],
        ),
        tool(
            "add_floor",
            "Add a floor slab spanning the given walls (default: every wall).",
            json!({"walls": {"type": "array", "items": {"type": ["string", "integer"]}}}),
            &[],
        ),
        tool(
            "break_into_bricks",
            "Shatter an object into physics bricks (for demolition scenes).",
            json!({
                "object": object_ref("object name or id"),
                "bricks": {"type": "integer", "description": "target count"}
            }),
            &["object"],
        ),
        tool(
            "add_measurement",
            "Add a ruler measurement between two world points.",
            json!({
                "a": {"type": "array", "items": {"type": "number"}},
                "b": {"type": "array", "items": {"type": "number"}}
            }),
            &["a", "b"],
        ),
        tool(
            "simulate",
            "Control the physics simulation.",
            json!({"action": {"type": "string", "enum": ["play", "pause", "stop"]}}),
            &["action"],
        ),
        tool(
            "new_scene",
            "Erase EVERYTHING and start an empty scene. Only when the user explicitly asks.",
            json!({}),
            &[],
        ),
        tool("get_library", "List the reusable asset library.", json!({}), &[]),
        tool(
            "create_library_object",
            "Capture objects (with children) as a reusable library asset.",
            json!({
                "name": {"type": "string"},
                "description": {"type": "string"},
                "objects": {"type": "array", "items": {"type": ["string", "integer"]}, "description": "root objects to capture"},
                "pivot": {"type": "array", "items": {"type": "number"}},
                "anchor": {"type": "array", "items": {"type": "number"}}
            }),
            &["name"],
        ),
        tool(
            "place_library_object",
            "Stamp a copy of a library asset into the scene (the fast way to repeat buildings, trees, props).",
            json!({
                "asset": {"type": ["string", "integer"], "description": "asset name or id"},
                "location": {"type": "array", "items": {"type": "number"}, "description": "world [x,y,z]; the asset's pivot lands here"},
                "attach_to": object_ref("optional: attach onto this object instead")
            }),
            &["asset"],
        ),
        tool(
            "delete_library_object",
            "Remove an asset from the library.",
            json!({"asset": {"type": ["string", "integer"]}}),
            &["asset"],
        ),
        tool(
            "set_view",
            "Viewport shading/lighting. lighting 'scene' renders the scene's own lights (needed for day/night moods); 'studio' is neutral work lighting.",
            json!({
                "shading": {"type": "string", "enum": ["wireframe", "solid", "shaded"]},
                "lighting": {"type": "string", "enum": ["studio", "scene"]}
            }),
            &[],
        ),
        tool(
            "screenshot",
            "See the current viewport as an image. Use it to verify your work after substantial changes.",
            json!({}),
            &[],
        ),
    ]
}

/// Run one tool call against the live scene.
pub fn dispatch(name: &str, input: &Value, ctx: &mut ToolContext) -> Value {
    // set_view touches render-loop state, not the scene
    if name == "set_view" {
        return commands::set_view(input, ctx.shade_mode, ctx.lighting_mode);
    }
    if !catalog().iter().any(|t| t.name == name) {
        return json!({"ok": false, "error": format!("unknown tool '{name}'")});
    }
    let mut command = input.clone();
    if !command.is_object() {
        command = json!({});
    }
    command["cmd"] = json!(name);
    commands::execute(&command, ctx.scene, ctx.selection, ctx.physics, ctx.library)
}

/// One-line log summary of a tool call's arguments (the panel prefixes the
/// tool name), e.g. `cube Trunk` for an add_object.
pub fn summarize(_name: &str, input: &Value, response: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in ["primitive", "new_name", "name", "object", "asset", "child", "parent",
                "action", "shading", "lighting"] {
        if let Some(v) = input.get(key) {
            match v {
                Value::String(s) => parts.push(s.clone()),
                other => parts.push(other.to_string()),
            }
        }
    }
    if let Some(error) = response["error"].as_str() {
        let error: String = error.chars().take(120).collect();
        if parts.is_empty() {
            return error;
        }
        return format!("{} — {error}", parts.join(" "));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::physics::PhysicsMirror;
    use crate::scene_render::{LightingMode, ShadeMode};
    use crate::selection::Selection;
    use modeler_core::{Library, Scene};

    #[test]
    fn catalog_schemas_are_objects() {
        for tool in catalog() {
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert!(!tool.description.is_empty(), "{}", tool.name);
        }
    }

    #[test]
    fn dispatch_runs_commands_and_set_view() {
        let _guard = crate::physics::ffi_test_lock();
        let mut scene = Scene::default_scene();
        let mut selection = Selection::default();
        let mut physics = PhysicsMirror::new();
        let mut library = Library::default();
        let mut shade = ShadeMode::Shaded;
        let mut lighting = LightingMode::Studio;
        let mut ctx = ToolContext {
            scene: &mut scene,
            selection: &mut selection,
            physics: &mut physics,
            library: &mut library,
            shade_mode: &mut shade,
            lighting_mode: &mut lighting,
        };

        let response = dispatch(
            "add_object",
            &json!({"primitive": "cube", "new_name": "Tower", "location": [0, 0, 5]}),
            &mut ctx,
        );
        assert_eq!(response["ok"], true, "{response}");

        let response = dispatch("set_view", &json!({"lighting": "scene"}), &mut ctx);
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(*ctx.lighting_mode, LightingMode::Scene);

        let response = dispatch("no_such_tool", &json!({}), &mut ctx);
        assert_eq!(response["ok"], false);
        assert_eq!(scene.objects().iter().filter(|o| o.name == "Tower").count(), 1);
    }

    #[test]
    fn summaries_are_compact() {
        let summary = summarize(
            "add_object",
            &json!({"primitive": "cube", "new_name": "Trunk", "location": [0,0,1]}),
            &json!({"ok": true}),
        );
        assert_eq!(summary, "cube Trunk");
        let summary = summarize(
            "delete_object",
            &json!({"object": "Nope"}),
            &json!({"ok": false, "error": "no object named 'Nope'"}),
        );
        assert!(summary.contains("Nope —"));
    }
}
