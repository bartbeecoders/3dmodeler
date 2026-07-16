//! Modifier-stack evaluation and operations (Blender-style).
//!
//! Modifiers are non-destructive: the viewport shows the base mesh with the
//! enabled stack applied ([`evaluate`]), updating live as parameters change
//! or a boolean's tool object moves — that IS the preview. Nothing touches
//! the document until the user applies the stack ([`apply`]), which bakes
//! the result into `Object::edited_mesh`. Editing (Tab) and physics keep
//! using the base mesh — the cage — like Blender.
//!
//! Renderers key their caches on [`stamp`], which fingerprints everything
//! the evaluated mesh depends on (including boolean tools and their
//! placement relative to the target).

use modeler_core::{
    boolean, MeshData, Modifier, ModifierKind, ObjectId, Primitive, Scene, Transform,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// The mesh the viewport shows for an object: the base mesh (edited or
/// parametric) with every enabled modifier applied in stack order.
pub fn evaluate(scene: &Scene, id: ObjectId) -> MeshData {
    evaluate_stack(scene, id, usize::MAX, &mut Vec::new(), false)
        .expect("non-strict evaluation cannot fail")
}

/// Base mesh with the first `count` stack entries applied (enabled ones
/// only). Boolean tools are evaluated with THEIR full stacks; `visiting`
/// breaks reference cycles (A cuts B, B cuts A) by falling back to the
/// base mesh. A boolean whose result would be empty is skipped in preview
/// (`strict = false`) so the object never becomes unselectable, and is an
/// error at apply time (`strict = true`).
fn evaluate_stack(
    scene: &Scene,
    id: ObjectId,
    count: usize,
    visiting: &mut Vec<ObjectId>,
    strict: bool,
) -> Result<MeshData, String> {
    let Some(object) = scene.object(id) else {
        return Ok(MeshData::default());
    };
    let mut mesh = object.render_mesh();
    if object.modifiers.is_empty() || visiting.contains(&id) {
        return Ok(mesh);
    }
    visiting.push(id);
    let world = scene.world_transform(id);
    for modifier in object.modifiers.iter().take(count).filter(|m| m.enabled) {
        match modifier.kind {
            ModifierKind::Subdivision { levels } => {
                if levels > 0 {
                    mesh = crate::mesh_edit::subdivide(&mesh, levels, object.smooth);
                }
            }
            ModifierKind::Boolean { op, object: tool } => {
                if tool == id || scene.object(tool).is_none() {
                    continue; // unset or dangling tool: no-op
                }
                let tool_mesh =
                    evaluate_stack(scene, tool, usize::MAX, visiting, false)?;
                let tool_mesh = boolean::mesh_to_frame(
                    &tool_mesh,
                    &scene.world_transform(tool),
                    &world,
                );
                let result = boolean::mesh_boolean(&mesh, &tool_mesh, op);
                if !result.indices.is_empty() {
                    mesh = result;
                } else if strict {
                    visiting.pop();
                    return Err(format!(
                        "boolean against '{}' leaves nothing of the mesh",
                        scene.object(tool).map(|o| o.name.as_str()).unwrap_or("?")
                    ));
                }
            }
        }
    }
    visiting.pop();
    Ok(mesh)
}

/// Fingerprint of everything [`evaluate`] reads for an object: its own mesh
/// identity (revision, primitive parameters, shading) and every enabled
/// modifier — booleans add the tool's recursive fingerprint plus both world
/// transforms, so moving either object re-meshes the preview.
pub fn stamp(scene: &Scene, id: ObjectId) -> u64 {
    let mut h = DefaultHasher::new();
    stamp_into(scene, id, &mut h, &mut Vec::new());
    h.finish()
}

fn stamp_into(
    scene: &Scene,
    id: ObjectId,
    h: &mut DefaultHasher,
    visiting: &mut Vec<ObjectId>,
) {
    let Some(object) = scene.object(id) else {
        u64::MAX.hash(h);
        return;
    };
    object.mesh_revision.hash(h);
    object.smooth.hash(h);
    crate::scene_render::hash_primitive(h, &object.primitive);
    if visiting.contains(&id) {
        return;
    }
    visiting.push(id);
    for modifier in object.modifiers.iter().filter(|m| m.enabled) {
        match modifier.kind {
            ModifierKind::Subdivision { levels } => {
                1u8.hash(h);
                levels.hash(h);
            }
            ModifierKind::Boolean { op, object: tool } => {
                2u8.hash(h);
                (op as u8).hash(h);
                tool.0.hash(h);
                // the result lives in the target's frame: both placements count
                hash_transform(h, &scene.world_transform(id));
                hash_transform(h, &scene.world_transform(tool));
                stamp_into(scene, tool, h, visiting);
            }
        }
    }
    visiting.pop();
}

fn hash_transform(h: &mut DefaultHasher, t: &Transform) {
    for f in [
        t.location.x, t.location.y, t.location.z,
        t.rotation.x, t.rotation.y, t.rotation.z, t.rotation.w,
        t.scale.x, t.scale.y, t.scale.z,
    ] {
        f.to_bits().hash(h);
    }
}

fn no_volume(object: &modeler_core::Object) -> bool {
    object.primitive.is_light()
        || matches!(object.primitive, Primitive::Empty { .. })
        || object.primitive.is_rope()
}

/// True when any object's stack references `tool` as a boolean tool.
pub fn tool_referenced(scene: &Scene, tool: ObjectId) -> bool {
    scene.objects().iter().any(|o| {
        o.modifiers.iter().any(
            |m| matches!(m.kind, ModifierKind::Boolean { object, .. } if object == tool),
        )
    })
}

/// Add a boolean modifier to `target` for each tool object. The tools stay
/// in the scene (the modifier follows them live) but are hidden so the
/// preview is visible; removing the modifier shows them again. Returns a
/// status message, or an error without touching anything.
pub fn add_boolean(
    scene: &mut Scene,
    target: ObjectId,
    tools: &[ObjectId],
    op: boolean::BooleanOp,
) -> Result<String, String> {
    let target_object = scene.object(target).ok_or("no such target object")?;
    if no_volume(target_object) {
        return Err(format!(
            "'{}' is a light/empty — it has no volume to combine",
            target_object.name
        ));
    }
    let target_name = target_object.name.clone();
    let mut tool_ids: Vec<ObjectId> = Vec::new();
    for &tool in tools {
        if tool == target {
            return Err("the target cannot be one of the tools".to_string());
        }
        let tool_object = scene
            .object(tool)
            .ok_or_else(|| format!("no tool object with id {}", tool.0))?;
        if no_volume(tool_object) {
            return Err(format!(
                "'{}' is a light/empty — it has no volume to combine",
                tool_object.name
            ));
        }
        if !tool_ids.contains(&tool) {
            tool_ids.push(tool);
        }
    }
    if tool_ids.is_empty() {
        return Err("no tool objects given".to_string());
    }
    for &tool in &tool_ids {
        if let Some(object) = scene.object_mut(tool) {
            object.visible = false;
        }
    }
    if let Some(object) = scene.object_mut(target) {
        for &tool in &tool_ids {
            object
                .modifiers
                .push(Modifier::new(ModifierKind::Boolean { op, object: tool }));
        }
    }
    let count = tool_ids.len();
    Ok(format!(
        "added {count} {} modifier{} to '{target_name}' — tool{} hidden; \
         preview is live, Apply from the sidebar",
        op.label().to_lowercase(),
        if count == 1 { "" } else { "s" },
        if count == 1 { "" } else { "s" },
    ))
}

/// Bake the first `count` stack entries into the object's edited mesh and
/// drop them from the stack (disabled entries in the range are discarded).
/// Tool objects of applied booleans are consumed when nothing references
/// them anymore. Refuses (changing nothing) when the result would be empty.
pub fn apply(scene: &mut Scene, id: ObjectId, count: usize) -> Result<String, String> {
    let object = scene.object(id).ok_or("no such object")?;
    let name = object.name.clone();
    let count = count.min(object.modifiers.len());
    if count == 0 {
        return Err("no modifiers to apply".to_string());
    }
    let mesh = evaluate_stack(scene, id, count, &mut Vec::new(), true)
        .map_err(|e| format!("{e} — not applied"))?;
    if mesh.indices.is_empty() {
        return Err(
            "modifier result is empty (nothing would remain) — not applied".to_string()
        );
    }
    let applied: Vec<Modifier>;
    {
        let object = scene.object_mut(id).expect("checked above");
        let rest = object.modifiers.split_off(count);
        applied = std::mem::replace(&mut object.modifiers, rest);
        object.edited_mesh = Some(mesh);
        object.mesh_revision += 1;
    }
    // consume the applied booleans' tools once nothing else uses them
    let mut consumed = 0;
    for modifier in &applied {
        if let ModifierKind::Boolean { object: tool, .. } = modifier.kind {
            if modifier.enabled
                && tool != id
                && scene.object(tool).is_some()
                && !tool_referenced(scene, tool)
            {
                scene.remove_object(tool);
                consumed += 1;
            }
        }
    }
    let enabled = applied.iter().filter(|m| m.enabled).count();
    let mut message = format!(
        "applied {enabled} modifier{} to '{name}'",
        if enabled == 1 { "" } else { "s" }
    );
    if consumed > 0 {
        message += &format!(" — {consumed} tool object{} consumed", if consumed == 1 { "" } else { "s" });
    }
    Ok(message)
}

/// Remove one stack entry. A boolean's tool object is shown again when no
/// remaining modifier (on any object) references it.
pub fn remove(scene: &mut Scene, id: ObjectId, index: usize) -> Result<String, String> {
    let object = scene.object(id).ok_or("no such object")?;
    if index >= object.modifiers.len() {
        return Err(format!("no modifier at index {index}"));
    }
    let name = object.name.clone();
    let removed = {
        let object = scene.object_mut(id).expect("checked above");
        object.modifiers.remove(index)
    };
    if let ModifierKind::Boolean { object: tool, .. } = removed.kind {
        if scene.object(tool).is_some() && !tool_referenced(scene, tool) {
            if let Some(tool_object) = scene.object_mut(tool) {
                tool_object.visible = true;
            }
        }
    }
    Ok(format!("removed {} modifier from '{name}'", removed.kind.label().to_lowercase()))
}

/// Compatibility shim for the `subdivision` object parameter: set the total
/// subdivision to `levels` by updating the first subdivision modifier in
/// the stack (adding one at the end / removing them all as needed).
#[cfg_attr(not(test), allow(dead_code))]
pub fn set_subdivision(scene: &mut Scene, id: ObjectId, levels: u8) {
    if let Some(object) = scene.object_mut(id) {
        set_subdivision_on(object, levels);
    }
}

/// [`set_subdivision`] on an already-borrowed object (command parameter
/// handling works inside one `object_mut` scope).
pub fn set_subdivision_on(object: &mut modeler_core::Object, levels: u8) {
    if levels == 0 {
        object
            .modifiers
            .retain(|m| !matches!(m.kind, ModifierKind::Subdivision { .. }));
        return;
    }
    for modifier in &mut object.modifiers {
        if let ModifierKind::Subdivision { levels: current } = &mut modifier.kind {
            *current = levels;
            modifier.enabled = true;
            return;
        }
    }
    object
        .modifiers
        .push(Modifier::new(ModifierKind::Subdivision { levels }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::boolean::BooleanOp;
    use modeler_core::glam::Vec3;
    use modeler_core::{Primitive, Transform};

    fn mesh_volume(m: &MeshData) -> f32 {
        m.indices
            .chunks_exact(3)
            .map(|tri| {
                let a = m.positions[tri[0] as usize];
                let b = m.positions[tri[1] as usize];
                let c = m.positions[tri[2] as usize];
                a.dot(b.cross(c)) / 6.0
            })
            .sum()
    }

    fn two_cubes(scene: &mut Scene) -> (ObjectId, ObjectId) {
        let target = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        let tool = scene.add_object(
            Primitive::Cube { size: 1.0 },
            Transform { location: Vec3::splat(0.25), ..Transform::default() },
        );
        (target, tool)
    }

    #[test]
    fn boolean_modifier_previews_without_touching_the_document() {
        let mut scene = Scene::new();
        let (target, tool) = two_cubes(&mut scene);
        let message =
            add_boolean(&mut scene, target, &[tool], BooleanOp::Subtract).unwrap();
        assert!(message.contains("preview is live"), "{message}");
        assert!(!scene.object(tool).unwrap().visible, "tool hidden for preview");

        // preview shows the carved shape…
        let preview = evaluate(&scene, target);
        let expected = 1.0 - 0.75f32.powi(3);
        assert!((mesh_volume(&preview) - expected).abs() < 1e-3);
        // …but the document is untouched: no edited mesh, tool still there
        assert!(scene.object(target).unwrap().edited_mesh.is_none());
        assert!(scene.object(tool).is_some());

        // moving the tool changes the preview (and the cache stamp)
        let before = stamp(&scene, target);
        scene.object_mut(tool).unwrap().transform.location = Vec3::splat(0.5);
        assert_ne!(stamp(&scene, target), before, "stamp follows the tool");
        let expected = 1.0 - 0.5f32.powi(3);
        assert!((mesh_volume(&evaluate(&scene, target)) - expected).abs() < 1e-3);

        // disabling the modifier restores the base mesh in the preview
        scene.object_mut(target).unwrap().modifiers[0].enabled = false;
        assert!((mesh_volume(&evaluate(&scene, target)) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn apply_bakes_the_stack_and_consumes_the_tool() {
        let mut scene = Scene::new();
        let (target, tool) = two_cubes(&mut scene);
        add_boolean(&mut scene, target, &[tool], BooleanOp::Subtract).unwrap();

        let message = apply(&mut scene, target, usize::MAX).unwrap();
        assert!(message.contains("applied 1 modifier"), "{message}");
        assert!(message.contains("1 tool object consumed"), "{message}");

        let object = scene.object(target).unwrap();
        assert!(object.modifiers.is_empty(), "stack drained");
        let mesh = object.edited_mesh.as_ref().expect("baked into the mesh");
        let expected = 1.0 - 0.75f32.powi(3);
        assert!((mesh_volume(mesh) - expected).abs() < 1e-3);
        assert!(scene.object(tool).is_none(), "tool consumed");
    }

    #[test]
    fn shared_tools_survive_until_the_last_reference_applies() {
        let mut scene = Scene::new();
        let a = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        let b = scene.add_object(
            Primitive::Cube { size: 1.0 },
            Transform { location: Vec3::new(0.5, 0.0, 0.0), ..Transform::default() },
        );
        let cutter = scene.add_object(
            Primitive::Cube { size: 0.5 },
            Transform { location: Vec3::new(0.25, 0.0, 0.0), ..Transform::default() },
        );
        add_boolean(&mut scene, a, &[cutter], BooleanOp::Subtract).unwrap();
        add_boolean(&mut scene, b, &[cutter], BooleanOp::Subtract).unwrap();

        apply(&mut scene, a, usize::MAX).unwrap();
        assert!(scene.object(cutter).is_some(), "still referenced by b");
        apply(&mut scene, b, usize::MAX).unwrap();
        assert!(scene.object(cutter).is_none(), "last reference consumes it");
    }

    #[test]
    fn remove_restores_the_tools_visibility() {
        let mut scene = Scene::new();
        let (target, tool) = two_cubes(&mut scene);
        add_boolean(&mut scene, target, &[tool], BooleanOp::Union).unwrap();
        assert!(!scene.object(tool).unwrap().visible);
        remove(&mut scene, target, 0).unwrap();
        assert!(scene.object(target).unwrap().modifiers.is_empty());
        assert!(scene.object(tool).unwrap().visible, "tool shown again");
    }

    #[test]
    fn subdivision_modifier_smooths_and_applies() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        set_subdivision(&mut scene, id, 2);
        assert_eq!(scene.object(id).unwrap().subdivision_only_levels(), Some(2));

        let preview = evaluate(&scene, id);
        assert!(preview.indices.len() > 12 * 3, "more triangles than the cube");
        // Catmull-Clark pulls a lone cube well toward its limit ball
        let v = mesh_volume(&preview);
        assert!(v > 2.0 && v < 8.0, "rounded cube volume {v}");

        apply(&mut scene, id, usize::MAX).unwrap();
        let object = scene.object(id).unwrap();
        assert!(object.modifiers.is_empty());
        let baked = object.edited_mesh.as_ref().unwrap();
        assert!((mesh_volume(baked) - v).abs() < 1e-3, "apply matches the preview");

        // the compatibility shim also updates and removes
        set_subdivision(&mut scene, id, 3);
        set_subdivision(&mut scene, id, 1);
        assert_eq!(scene.object(id).unwrap().modifiers.len(), 1);
        set_subdivision(&mut scene, id, 0);
        assert!(scene.object(id).unwrap().modifiers.is_empty());
    }

    #[test]
    fn boolean_cycles_fall_back_to_base_meshes() {
        let mut scene = Scene::new();
        let (a, b) = two_cubes(&mut scene);
        scene.object_mut(a).unwrap().modifiers.push(Modifier::new(
            ModifierKind::Boolean { op: BooleanOp::Subtract, object: b },
        ));
        scene.object_mut(b).unwrap().modifiers.push(Modifier::new(
            ModifierKind::Boolean { op: BooleanOp::Subtract, object: a },
        ));
        // no hang, no stack overflow. The cycle breaks at the second visit
        // (the tool evaluates against the other's BASE mesh), so each tool
        // loses exactly its overlap first and the subtract removes nothing:
        // both previews stay their base cubes — stable and deterministic.
        assert!((mesh_volume(&evaluate(&scene, a)) - 1.0).abs() < 1e-3);
        assert!((mesh_volume(&evaluate(&scene, b)) - 1.0).abs() < 1e-3);
        // stamps terminate too
        let _ = stamp(&scene, a);
    }

    #[test]
    fn empty_boolean_results_are_skipped_in_preview_and_refused_on_apply() {
        let mut scene = Scene::new();
        let target = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        let swallower =
            scene.add_object(Primitive::Cube { size: 5.0 }, Transform::default());
        add_boolean(&mut scene, target, &[swallower], BooleanOp::Subtract).unwrap();
        // preview: the stage is skipped, the object stays selectable
        assert!((mesh_volume(&evaluate(&scene, target)) - 1.0).abs() < 1e-3);
        // apply: refused because the whole result would be empty
        assert!(apply(&mut scene, target, usize::MAX).is_err());
        assert!(scene.object(target).unwrap().edited_mesh.is_none());
    }
}
