//! Derives the viewport's draw lists from the `modeler-core` scene document,
//! plus Blender-style selection outlines and overlap warnings.
//!
//! Objects that share a mesh and material (brick piles, duplicated furniture)
//! are grouped into a single instanced draw with their base color riding per
//! instance; everything else is drawn on its own.
//!
//! # What this module owns
//!
//! The document is the truth; [`RenderScene`] is a mirror of it that the engine
//! can draw. [`SceneRender::sync`] rebuilds that mirror each frame from the
//! visible objects, and the caches under it exist so that rebuilding is cheap:
//! a mesh is uploaded only when its *identity* changes, and objects whose
//! geometry is identical share one upload, which is what lets the engine batch
//! them into a single instanced draw.
//!
//! Per-face materials become submeshes rather than separate meshes. The
//! triangles are sorted by material slot at upload so each slot is one
//! contiguous index range, which is exactly what [`Submesh`] names — a cube
//! with one face painted differently stays one mesh and two draws.

use crate::gfx::*;
use crate::selection::Selection;
use aether_render::types::{material_flags, GpuLight, GpuMaterial};
use aether_render::{
    Gpu, MaterialHandle, MeshHandle, ObjectHandle, RenderObject, RenderScene, Submesh, SunLight,
};
use modeler_core::glam;
use modeler_core::{LightKind, MeshData, Material, ObjectId, Primitive, Scene};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// Resolve the object's full render material (master + MPC + world effects).
fn render_material(scene: &Scene, id: ObjectId) -> Material {
    scene
        .object_material_for_render(id)
        .unwrap_or_default()
}

/// Viewport shading mode (Blender's four).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ShadeMode {
    /// Only the objects' sharp edges, drawn by the overlay.
    Wireframe,
    /// Neutral gray studio look, object materials ignored.
    Solid,
    /// Full materials under the built-in studio rig (key + image-based fill);
    /// the scene's own lights are ignored. The default.
    MaterialPreview,
    /// The final image: the scene's light objects (with shadows) over a faint
    /// ambient, metered by the renderer's auto-exposure.
    Rendered,
}

impl ShadeMode {
    /// The name the control API uses for this mode.
    pub fn name(self) -> &'static str {
        match self {
            Self::Wireframe => "wireframe",
            Self::Solid => "solid",
            Self::MaterialPreview => "material",
            Self::Rendered => "rendered",
        }
    }
}

// Blender's outline colors: light orange for the active object, darker
// orange for other selected objects; red warns about overlaps while placing.
// Purple marks a non-empty (unapplied) modifier stack on the selected mesh.
const ACTIVE_OUTLINE: Srgba = Srgba::new(255, 170, 64, 255);
const SELECTED_OUTLINE: Srgba = Srgba::new(230, 110, 20, 255);
const ACTIVE_MODIFIER_OUTLINE: Srgba = Srgba::new(200, 140, 255, 255);
const SELECTED_MODIFIER_OUTLINE: Srgba = Srgba::new(150, 80, 230, 255);
const OVERLAP_OUTLINE: Srgba = Srgba::new(235, 60, 50, 255);

/// Selection tier of an object. Part of the instancing group key: outlines
/// are drawn per group, so members of one group must share a tier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OutlineTier {
    None,
    Selected,
    Active,
    /// Selected, with a non-empty unapplied modifier stack.
    SelectedModifier,
    /// Active selection, with a non-empty unapplied modifier stack.
    ActiveModifier,
    Overlap,
}

impl OutlineTier {
    pub fn of(
        id: ObjectId,
        selection: &Selection,
        overlaps: &HashSet<ObjectId>,
        has_modifiers: bool,
    ) -> Self {
        if !selection.is_selected(id) {
            Self::None
        } else if overlaps.contains(&id) {
            Self::Overlap
        } else if selection.active() == Some(id) {
            if has_modifiers {
                Self::ActiveModifier
            } else {
                Self::Active
            }
        } else if has_modifiers {
            Self::SelectedModifier
        } else {
            Self::Selected
        }
    }

    // outline: overlap warning > active/selected (+ purple when modifiers)
    pub fn color(self) -> Option<Srgba> {
        match self {
            Self::None => None,
            Self::Selected => Some(SELECTED_OUTLINE),
            Self::Active => Some(ACTIVE_OUTLINE),
            Self::SelectedModifier => Some(SELECTED_MODIFIER_OUTLINE),
            Self::ActiveModifier => Some(ACTIVE_MODIFIER_OUTLINE),
            Self::Overlap => Some(OVERLAP_OUTLINE),
        }
    }
}

/// The mesh the viewport shows: the base mesh with the object's modifier stack
/// applied (subdivision surface, live boolean previews). Editing and collision
/// keep using the base mesh (the cage), like Blender.
fn display_mesh(scene: &Scene, object: &modeler_core::Object) -> MeshData {
    crate::modifiers::evaluate(scene, object.id)
}

fn hash_f32<H: Hasher>(h: &mut H, f: f32) {
    f.to_bits().hash(h);
}

/// Linear RGB, as the engine wants every colour.
fn linear_rgb(c: Srgba) -> aether_math::Vec3 {
    let v = c.to_linear();
    aether_math::Vec3::new(v.x, v.y, v.z)
}

/// Hash a primitive's identity (variant + parameters) by float bit patterns.
pub(crate) fn hash_primitive<H: Hasher>(h: &mut H, p: &Primitive) {
    match *p {
        Primitive::Plane { size } => {
            0u8.hash(h);
            hash_f32(h, size);
        }
        Primitive::Cube { size } => {
            1u8.hash(h);
            hash_f32(h, size);
        }
        Primitive::UvSphere { segments, rings, radius } => {
            2u8.hash(h);
            (segments, rings).hash(h);
            hash_f32(h, radius);
        }
        Primitive::IcoSphere { subdivisions, radius } => {
            3u8.hash(h);
            subdivisions.hash(h);
            hash_f32(h, radius);
        }
        Primitive::Cylinder { vertices, radius, depth } => {
            4u8.hash(h);
            vertices.hash(h);
            hash_f32(h, radius);
            hash_f32(h, depth);
        }
        Primitive::Cone { vertices, radius_bottom, radius_top, depth } => {
            5u8.hash(h);
            vertices.hash(h);
            hash_f32(h, radius_bottom);
            hash_f32(h, radius_top);
            hash_f32(h, depth);
        }
        Primitive::Torus { major_segments, minor_segments, major_radius, minor_radius } => {
            6u8.hash(h);
            (major_segments, minor_segments).hash(h);
            hash_f32(h, major_radius);
            hash_f32(h, minor_radius);
        }
        Primitive::Wall { length, height, thickness } => {
            7u8.hash(h);
            hash_f32(h, length);
            hash_f32(h, height);
            hash_f32(h, thickness);
        }
        Primitive::Floor { width, depth, thickness } => {
            8u8.hash(h);
            hash_f32(h, width);
            hash_f32(h, depth);
            hash_f32(h, thickness);
        }
        Primitive::Empty { size } => {
            9u8.hash(h);
            hash_f32(h, size);
        }
        Primitive::Roof { kind, width, depth, height, overhang, ridge_x } => {
            11u8.hash(h);
            modeler_core::RoofKind::ALL
                .iter()
                .position(|&k| k == kind)
                .unwrap_or(0)
                .hash(h);
            hash_f32(h, width);
            hash_f32(h, depth);
            hash_f32(h, height);
            hash_f32(h, overhang);
            ridge_x.hash(h);
        }
        Primitive::Light { kind, color, intensity, spot_angle_deg, shadows } => {
            10u8.hash(h);
            match kind {
                LightKind::Point => 0u8.hash(h),
                LightKind::Sun => 1u8.hash(h),
                LightKind::Spot => 2u8.hash(h),
            }
            for c in color {
                hash_f32(h, c);
            }
            hash_f32(h, intensity);
            hash_f32(h, spot_angle_deg);
            shadows.hash(h);
        }
        Primitive::Camera {
            fov_deg,
            clip_start,
            clip_end,
        } => {
            14u8.hash(h);
            hash_f32(h, fov_deg);
            hash_f32(h, clip_start);
            hash_f32(h, clip_end);
        }
        Primitive::Rope {
            length,
            radius,
            segments,
        } => {
            12u8.hash(h);
            hash_f32(h, length);
            hash_f32(h, radius);
            segments.hash(h);
        }
        Primitive::Cloth {
            width,
            height,
            segments_u,
            segments_v,
            stiffness,
        } => {
            13u8.hash(h);
            hash_f32(h, width);
            hash_f32(h, height);
            segments_u.hash(h);
            segments_v.hash(h);
            hash_f32(h, stiffness);
        }
        Primitive::Terrain { size, resolution, height, seed } => {
            15u8.hash(h);
            hash_f32(h, size);
            resolution.hash(h);
            hash_f32(h, height);
            seed.hash(h);
        }
        Primitive::Prop { kind, seed, size } => {
            16u8.hash(h);
            modeler_core::PropKind::ALL
                .iter()
                .position(|&k| k == kind)
                .unwrap_or(0)
                .hash(h);
            seed.hash(h);
            hash_f32(h, size);
        }
    }
}

/// Content signature of everything that affects scene lights and their
/// shadow maps: light objects (parameters + placement) and shadow casters
/// (geometry identity + placement + visibility). Replaces keying on the
/// global scene version, which regenerated EVERY shadow map on ANY edit —
/// a drag bumps the version every frame.
fn lighting_signature(scene: &Scene) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let worlds = scene.world_transforms();
    for object in scene.objects() {
        if !object.visible {
            continue;
        }
        let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
        object.id.0.hash(&mut h);
        for f in [
            world.location.x,
            world.location.y,
            world.location.z,
            world.rotation.x,
            world.rotation.y,
            world.rotation.z,
            world.rotation.w,
            world.scale.x,
            world.scale.y,
            world.scale.z,
        ] {
            hash_f32(&mut h, f);
        }
        hash_primitive(&mut h, &object.primitive);
        // casters: mesh identity (edits bump the revision; smooth changes
        // re-upload the mesh the shadow map renders)
        object.mesh_revision.hash(&mut h);
        object.smooth.hash(&mut h);
    }
    h.finish()
}

pub const STUDIO_KEY: (f32, Srgba, [f32; 3]) = (1.4, Srgba::WHITE, [-0.4, 0.35, -0.85]);
/// A mesh uploaded to the engine, and where each face-material slot lives in it.
struct CachedMesh {
    handle: MeshHandle,
    /// `(slot, range)` per face-material slot in use, slot 0 first. Empty when
    /// the object has no live per-face assignment and draws whole.
    slots: Vec<(u32, Submesh)>,
}

pub struct SceneRender {
    /// The engine's mirror of the document. Public because the renderer, the
    /// overlay and the camera render all read it.
    pub scene: RenderScene,
    /// Uploaded meshes, by geometry identity. Two objects with the same key are
    /// the same geometry and share one upload, which is what the engine batches
    /// into an instanced draw.
    meshes: HashMap<u64, CachedMesh>,
    materials: HashMap<u64, MaterialHandle>,
    /// The engine object for each (document object, material slot).
    ///
    /// Keyed rather than rebuilt so a moving object keeps its handle from frame
    /// to frame. That is what makes motion vectors work: the engine derives
    /// them from the difference between an object's transform and its previous
    /// one, and an object recreated every frame has no previous one — TAA then
    /// has nothing to reproject and smears anything that moves.
    objects: HashMap<(ObjectId, u32), ObjectHandle>,
    /// The subset of `objects` that are light, camera and empty gizmos. The
    /// camera render hides them: they are the editor showing you where things
    /// are, not part of the picture the camera takes.
    gizmos: Vec<ObjectHandle>,
    /// What the outline pass traces: the selected objects, as the engine wants
    /// them.
    selection: Vec<(MeshHandle, aether_math::Transform)>,
    /// Ids drawn this frame, in document order — the camera render and the
    /// wireframe both need to know what was visible.
    visible: Vec<ObjectId>,
}

/// The geometry an object draws, and everything that changes it.
///
/// Two objects with the same key are guaranteed to want the same vertices, so
/// they share an upload. An object whose mesh cannot be shared — an edited
/// mesh, a wall with cutouts, a draped rope, anything with a boolean modifier —
/// mixes its own id into the key, which gives it a private entry in the same
/// cache rather than a second code path.
fn mesh_key(scene: &Scene, object: &modeler_core::Object) -> u64 {
    let mut h = DefaultHasher::new();
    hash_primitive(&mut h, &object.primitive);
    object.smooth.hash(&mut h);
    crate::modifiers::stamp(scene, object.id).hash(&mut h);
    // Per-face assignment changes how the index buffer is ordered, so it is
    // part of the geometry rather than of the material.
    object.face_materials.len().hash(&mut h);
    if !shareable(object) {
        object.id.0.hash(&mut h);
        object.mesh_revision.hash(&mut h);
        // live sculpt strokes: the renderer follows every dab, while the
        // physics ShapeKey deliberately ignores this counter (see Object)
        object.sculpt_revision.hash(&mut h);
    }
    h.finish()
}

/// Whether an object's geometry is a pure function of its primitive.
fn shareable(object: &modeler_core::Object) -> bool {
    object.edited_mesh.is_none()
        && object.rope_nodes.is_none()
        && object.cutouts.is_empty()
        && object.floor_outline.is_empty()
        // the noise stack lives on the object, so terrain geometry is not
        // a function of the primitive alone (edits bump mesh_revision)
        && object.terrain.is_none()
        && object.subdivision_only_levels().is_some()
}

/// The display mesh as the engine wants it, with the triangles grouped by
/// material slot.
///
/// Returns the mesh and the slot ranges. Grouping here rather than at draw time
/// is what makes a slot one contiguous [`Submesh`]; the alternative is a copy of
/// the vertex buffer per slot.
fn to_aether_mesh(
    data: &MeshData,
    name: String,
    slots: usize,
) -> (aether_render::mesh::MeshData, Vec<(u32, Submesh)>) {
    use aether_render::mesh::Vertex;

    let mut data = data.clone();
    data.ensure_uvs();
    let has_uvs = data.uvs.len() == data.positions.len();
    let has_normals = data.normals.len() == data.positions.len();
    let vertices: Vec<Vertex> = data
        .positions
        .iter()
        .enumerate()
        .map(|(i, p)| Vertex {
            position: [p.x, p.y, p.z],
            normal: if has_normals {
                let n = data.normals[i];
                [n.x, n.y, n.z]
            } else {
                [0.0, 0.0, 1.0]
            },
            // Recomputed below once the whole mesh is assembled.
            tangent: [1.0, 0.0, 0.0, 1.0],
            uv: if has_uvs { [data.uvs[i].x, data.uvs[i].y] } else { [0.0, 0.0] },
        })
        .collect();

    // Group the triangles by slot. A stale `tri_materials` — mesh surgery
    // rebuilds the index buffer and drops it — means "everything is slot 0",
    // which is also what an object with no face materials wants.
    let tri_count = data.indices.len() / 3;
    let live = data.tri_materials.len() == tri_count && slots > 0;
    let slot_of = |tri: usize| -> u32 {
        if !live {
            return 0;
        }
        let slot = data.tri_materials[tri];
        if slot as usize <= slots {
            slot
        } else {
            0 // an out-of-range slot falls back to the object's own material
        }
    };

    let mut used: Vec<u32> = (0..tri_count).map(slot_of).collect();
    used.sort_unstable();
    used.dedup();

    let mut indices: Vec<u32> = Vec::with_capacity(data.indices.len());
    let mut ranges: Vec<(u32, Submesh)> = Vec::new();
    for slot in used {
        let offset = indices.len() as u32;
        for tri in 0..tri_count {
            if slot_of(tri) == slot {
                indices.extend_from_slice(&data.indices[tri * 3..tri * 3 + 3]);
            }
        }
        let count = indices.len() as u32 - offset;
        if count > 0 {
            ranges.push((slot, Submesh { index_offset: offset, index_count: count }));
        }
    }

    let mut mesh = aether_render::mesh::MeshData::new(name, vertices, indices);
    // Normal maps sample in tangent space, and without tangents the surface
    // normals collapse: the studio look survives on ambient alone but anything
    // lit by the scene's own lights reads as unlit.
    mesh.recompute_tangents();
    // A single range covering everything is the common case and needs no
    // submesh at all.
    if ranges.len() == 1 && ranges[0].0 == 0 {
        ranges.clear();
    }
    (mesh, ranges)
}

/// The engine material for one modeler material.
///
/// `tex` is the bridged texture set, when the material has maps: its
/// template carries the array-texture layer indices and `HAS_*` flags, and
/// each mapped channel neutralizes the matching scalar (the shader
/// multiplies map × scalar).
fn gpu_material(
    material: &Material,
    primitive: &Primitive,
    mode: ShadeMode,
    xray: bool,
    tex: Option<&crate::texture_bridge::TextureSet>,
) -> GpuMaterial {
    let mut m = tex.map(|t| t.template).unwrap_or_default();
    let alpha = if xray { 0.35 } else { material.alpha.clamp(0.0, 1.0) };

    let (base, roughness, metallic, emissive) = match primitive {
        // Light gizmos glow in their own colour in every shading mode: the
        // gizmo IS the light as far as the user is concerned.
        Primitive::Light { color, .. } => {
            ([0.1, 0.1, 0.1], 1.0, 0.0, [color[0], color[1], color[2]])
        }
        Primitive::Camera { .. } => ([0.16, 0.16, 0.19], 0.6, 0.2, [0.05, 0.12, 0.2]),
        // Solid ignores the object material entirely for a uniform studio look.
        _ if mode == ShadeMode::Solid => ([0.72, 0.72, 0.75], 0.85, 0.0, [0.0; 3]),
        _ => (
            material.base_color,
            material.roughness,
            material.metallic,
            material.emissive_rgb(),
        ),
    };

    let (base, roughness, metallic) = match tex {
        Some(t) => (
            if t.has_albedo { [1.0, 1.0, 1.0] } else { base },
            if t.has_roughness { 1.0 } else { roughness },
            if t.has_metallic { 1.0 } else { metallic },
        ),
        None => (base, roughness, metallic),
    };
    m.base_color = vec4(base[0], base[1], base[2], alpha).into();
    m.emissive = vec4(emissive[0], emissive[1], emissive[2], EMISSIVE_NITS).into();
    m.metallic_roughness_reflectance_ao = vec4(
        metallic.clamp(0.0, 1.0),
        roughness.clamp(0.02, 1.0),
        material.specular.clamp(0.0, 1.0),
        material.occlusion.clamp(0.0, 1.0),
    )
    .into();
    if tex.is_some() {
        // UV tiling (repeats per unit of mesh UV); rotation is unsupported
        let s = material.textures.uv_scale;
        m.aniso_uv = vec4(m.aniso_uv.x, m.aniso_uv.y, s[0], s[1]).into();
    }
    if material.coat > 1.0e-4 {
        m.clearcoat_sheen =
            vec4(material.coat, material.coat_roughness, material.sheen, 0.5).into();
        m.add_flags(material_flags::CLEARCOAT);
    }
    if alpha < 0.999 {
        // Double-sided with it: a transparent object shows its own back faces,
        // and culling them leaves a hollow shell.
        m.add_flags(material_flags::ALPHA_BLEND | material_flags::DOUBLE_SIDED);
    }
    m
}

/// The synthesized material for a terrain's water sheet (mesh slot 1). The
/// baked tint texture carries the colors; this sets the surface response:
/// glossy non-metal, dither-transparent through `alpha`.
fn water_material(water: &modeler_core::terrain::WaterLayer) -> Material {
    let mut m = Material::default();
    m.base_color = water.deep; // fallback for looks that skip the bake
    m.roughness = water.roughness.clamp(0.02, 1.0);
    m.metallic = 0.0;
    m.specular = 0.9;
    m.alpha = water.opacity.clamp(0.05, 1.0);
    m
}

/// Hash of everything [`gpu_material`] reads, so two objects that would produce
/// the same engine material share one. `tex_hash` is the bridged texture
/// identity (set key / bake stamp), 0 when untextured.
fn material_key(
    material: &Material,
    primitive: &Primitive,
    mode: ShadeMode,
    xray: bool,
    tex_hash: u64,
) -> u64 {
    let mut h = DefaultHasher::new();
    tex_hash.hash(&mut h);
    for f in material.textures.uv_scale {
        hash_f32(&mut h, f);
    }
    for f in [
        material.base_color[0],
        material.base_color[1],
        material.base_color[2],
        material.roughness,
        material.metallic,
        material.specular,
        material.alpha,
        material.coat,
        material.coat_roughness,
        material.sheen,
        material.occlusion,
    ] {
        hash_f32(&mut h, f);
    }
    for f in material.emissive_rgb() {
        hash_f32(&mut h, f);
    }
    // Gizmo colouring is per primitive, so the primitive is part of the key.
    hash_primitive(&mut h, primitive);
    mode.hash(&mut h);
    xray.hash(&mut h);
    h.finish()
}

impl SceneRender {
    pub fn new() -> Self {
        Self {
            scene: RenderScene::new(),
            meshes: HashMap::new(),
            materials: HashMap::new(),
            objects: HashMap::new(),
            gizmos: Vec::new(),
            selection: Vec::new(),
            visible: Vec::new(),
        }
    }

    /// Rebuilds the engine's scene from the document.
    ///
    /// Wireframe mode draws no surfaces at all — the edges come from
    /// [`crate::wire_render`] through the overlay — so it syncs an empty scene
    /// while keeping the caches warm for the switch back.
    #[allow(clippy::too_many_arguments)]
    pub fn sync(
        &mut self,
        gpu: &Gpu,
        renderer: &mut aether_render::Renderer,
        bridge: &mut crate::texture_bridge::TextureBridge,
        scene: &Scene,
        selection: &Selection,
        overlaps: &HashSet<ObjectId>,
        mode: ShadeMode,
        xray: bool,
    ) {
        bridge.begin_frame();
        // This frame's transforms are about to be written, so what is in the
        // scene now is the previous frame's. Taking the snapshot here is what
        // gives every object a `prev_transform` one frame behind its current
        // one.
        self.scene.snapshot_transforms();
        self.gizmos.clear();
        self.selection.clear();
        self.visible.clear();
        if mode == ShadeMode::Wireframe {
            self.retire_objects(&HashSet::new());
            return;
        }

        let worlds = scene.world_transforms(); // one O(N) pass for the frame
        let mut live_meshes: HashSet<u64> = HashSet::new();
        let mut live_materials: HashSet<u64> = HashSet::new();
        let mut live_objects: HashSet<(ObjectId, u32)> = HashSet::new();

        for object in scene.objects() {
            if !object.visible {
                continue;
            }
            let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
            let transform = aether_math::Transform {
                translation: world.location,
                rotation: world.rotation,
                scale: world.scale,
            };

            // terrain water rides in the mesh as slot-1 triangles even though
            // the object has no face_materials — its material is synthesized
            // from the WaterLayer below
            let water = object
                .terrain
                .as_ref()
                .and_then(|data| data.water)
                .filter(|w| w.enabled)
                .filter(|_| matches!(object.primitive, Primitive::Terrain { .. }));

            let key = mesh_key(scene, object);
            live_meshes.insert(key);
            if !self.meshes.contains_key(&key) {
                let data = display_mesh(scene, object);
                if data.indices.is_empty() {
                    continue;
                }
                let slot_count = object
                    .face_materials
                    .len()
                    .max(if water.is_some() { 1 } else { 0 });
                let (mesh, slots) = to_aether_mesh(&data, object.name.clone(), slot_count);
                let handle = self.scene.add_mesh(gpu, &mesh);
                self.meshes.insert(key, CachedMesh { handle, slots });
            }
            let cached = &self.meshes[&key];
            let mesh = cached.handle;
            let slots = cached.slots.clone();

            // A gizmo sits inside the light it represents; letting it cast
            // would shadow the whole scene from the inside.
            let is_gizmo = object.primitive.is_gizmo();
            let casts_shadow = !is_gizmo;

            // Textures are skipped in the two looks that ignore materials.
            let flat_look = mode == ShadeMode::Solid || xray;
            // Terrain biome color: bake the rule chain to an albedo texture
            // keyed per object; the stamp follows committed edits
            // (mesh_revision — mid-stroke sculpt keeps the last bake).
            let terrain_bake: Option<(crate::texture_bridge::TextureSet, u64)> = if flat_look {
                None
            } else if let (
                Primitive::Terrain { size, resolution, height, seed },
                Some(data),
            ) = (object.primitive, object.terrain.as_ref())
            {
                data.color.map(|color| {
                    let stamp = {
                        let mut h = DefaultHasher::new();
                        object.mesh_revision.hash(&mut h);
                        hash_primitive(&mut h, &object.primitive);
                        color.stamp().hash(&mut h);
                        h.finish()
                    };
                    let set = bridge.resolve_baked_albedo(
                        renderer,
                        &format!("terrain-color:{}", object.id.0),
                        stamp,
                        || {
                            data.bake_color(seed, resolution, size, height, 1024)
                                .expect("color is Some")
                        },
                    );
                    (set, stamp)
                })
            } else {
                None
            };
            // Water tint bake, same debounced path as the biome color.
            let water_bake: Option<(crate::texture_bridge::TextureSet, u64)> = if flat_look {
                None
            } else if let (
                Primitive::Terrain { size, resolution, height, seed },
                Some(data),
                Some(layer),
            ) = (object.primitive, object.terrain.as_ref(), water)
            {
                let stamp = {
                    let mut h = DefaultHasher::new();
                    object.mesh_revision.hash(&mut h);
                    hash_primitive(&mut h, &object.primitive);
                    layer.stamp().hash(&mut h);
                    h.finish()
                };
                let set = bridge.resolve_baked_albedo(
                    renderer,
                    &format!("terrain-water:{}", object.id.0),
                    stamp,
                    || {
                        data.bake_water_color(seed, resolution, size, height, 512)
                            .expect("water is Some and enabled")
                    },
                );
                Some((set, stamp))
            } else {
                None
            };

            let mut upsert = |this: &mut Self,
                              slot: u32,
                              material: &Material,
                              submesh: Option<Submesh>| {
                // slot 0 of a colored terrain wears the baked biome albedo;
                // everything else resolves its own texture maps (if any)
                let (tex, tex_hash) = if flat_look {
                    (None, 0)
                } else if slot == 0 && terrain_bake.is_some() {
                    let (set, stamp) = terrain_bake.as_ref().unwrap();
                    (Some(*set), *stamp)
                } else if slot == 1 && water_bake.is_some() {
                    let (set, stamp) = water_bake.as_ref().unwrap();
                    (Some(*set), *stamp)
                } else {
                    match crate::texture_bridge::TextureBridge::set_key(&material.textures)
                    {
                        Some(key) => {
                            let set = bridge.resolve_files(renderer, &material.textures);
                            let mut h = DefaultHasher::new();
                            key.hash(&mut h);
                            set.is_some().hash(&mut h);
                            (set, h.finish())
                        }
                        None => (None, 0),
                    }
                };
                let mkey = material_key(material, &object.primitive, mode, xray, tex_hash);
                live_materials.insert(mkey);
                let material = *this.materials.entry(mkey).or_insert_with(|| {
                    this_add_material(
                        &mut this.scene,
                        material,
                        &object.primitive,
                        mode,
                        xray,
                        tex.as_ref(),
                    )
                });

                let slot_key = (object.id, slot);
                live_objects.insert(slot_key);
                // Updating in place keeps `prev_transform`; the mesh and
                // material are written every frame because either can change
                // under an object that has not otherwise moved.
                let existing = this.objects.get(&slot_key).copied().filter(|handle| {
                    match this.scene.object_mut(*handle) {
                        Some(render) => {
                            render.mesh = mesh;
                            render.material = material;
                            render.transform = transform;
                            render.submesh = submesh;
                            render.casts_shadow = casts_shadow;
                            // Put back what a camera render may have hidden.
                            render.visible = true;
                            true
                        }
                        None => false,
                    }
                });
                let handle = existing.unwrap_or_else(|| {
                    let mut render = RenderObject::new(mesh, material, transform);
                    render.casts_shadow = casts_shadow;
                    render.submesh = submesh;
                    let handle = this.scene.add_object(render);
                    this.objects.insert(slot_key, handle);
                    handle
                });
                if is_gizmo {
                    this.gizmos.push(handle);
                }
            };

            let own = render_material(scene, object.id);
            if slots.is_empty() {
                upsert(self, 0, &own, None);
            } else {
                for (slot, range) in &slots {
                    let material = match slot {
                        0 => own.clone(),
                        1 if water.is_some() => water_material(&water.unwrap()),
                        k => object
                            .face_materials
                            .get(*k as usize - 1)
                            .cloned()
                            .unwrap_or_else(|| own.clone()),
                    };
                    upsert(self, *slot, &material, Some(*range));
                }
            }

            self.visible.push(object.id);
            let tier =
                OutlineTier::of(object.id, selection, overlaps, !object.modifiers.is_empty());
            if tier != OutlineTier::None {
                self.selection.push((mesh, transform));
            }
        }

        self.retire_objects(&live_objects);
        bridge.end_frame(renderer);

        // Retire what this frame did not use. Meshes hold GPU buffers, so a
        // modeller that keeps re-meshing while the user drags a parameter would
        // otherwise leak one upload per frame.
        let stale: Vec<u64> =
            self.meshes.keys().copied().filter(|k| !live_meshes.contains(k)).collect();
        for key in stale {
            if let Some(cached) = self.meshes.remove(&key) {
                self.scene.remove_mesh(cached.handle);
            }
        }
        let stale: Vec<u64> =
            self.materials.keys().copied().filter(|k| !live_materials.contains(k)).collect();
        for key in stale {
            if let Some(handle) = self.materials.remove(&key) {
                self.scene.remove_material(handle);
            }
        }
    }

    /// Drops the engine objects this frame did not touch — hidden objects,
    /// deleted ones, and material slots that no longer exist.
    fn retire_objects(&mut self, live: &HashSet<(ObjectId, u32)>) {
        let stale: Vec<(ObjectId, u32)> =
            self.objects.keys().copied().filter(|k| !live.contains(k)).collect();
        for key in stale {
            if let Some(handle) = self.objects.remove(&key) {
                self.scene.remove_object(handle);
            }
        }
    }

    /// Shows or hides the editor's gizmos.
    ///
    /// Used around a camera render, which frames the scene rather than the
    /// tool: a light's wireframe marker in the middle of a rendered image is
    /// the editor leaking into the photograph.
    pub fn set_gizmos_visible(&mut self, visible: bool) {
        for handle in &self.gizmos {
            if let Some(object) = self.scene.object_mut(*handle) {
                object.visible = visible;
            }
        }
    }

    /// The selection, as the overlay's outline pass wants it.
    pub fn selection(&self) -> &[(MeshHandle, aether_math::Transform)] {
        &self.selection
    }

    /// The outline colour for the current selection, which is the strongest
    /// tier present: an overlap warning outranks the active object.
    pub fn outline_color(
        scene: &Scene,
        selection: &Selection,
        overlaps: &HashSet<ObjectId>,
    ) -> Srgba {
        scene
            .objects()
            .iter()
            .filter(|o| selection.is_selected(o.id))
            .map(|o| OutlineTier::of(o.id, selection, overlaps, !o.modifiers.is_empty()))
            .max_by_key(|t| match t {
                OutlineTier::Overlap => 4,
                OutlineTier::ActiveModifier => 3,
                OutlineTier::Active => 2,
                OutlineTier::SelectedModifier => 1,
                _ => 0,
            })
            .and_then(OutlineTier::color)
            .unwrap_or(ACTIVE_OUTLINE)
    }

    /// The ids drawn this frame, in document order. Used by the camera render,
    /// which shows the scene without the editor's gizmos.
    #[allow(dead_code)] // wired up with the camera render
    pub fn visible(&self) -> &[ObjectId] {
        &self.visible
    }
}

/// Adds a material to the engine's scene.
///
/// Free-standing because the closure that calls it already holds a mutable
/// borrow of the cache it would otherwise reach through.
fn this_add_material(
    scene: &mut RenderScene,
    material: &Material,
    primitive: &Primitive,
    mode: ShadeMode,
    xray: bool,
    tex: Option<&crate::texture_bridge::TextureSet>,
) -> MaterialHandle {
    scene.add_material(gpu_material(material, primitive, mode, xray, tex))
}

/// What lights the viewport, and how that maps onto a physically based engine.
///
/// # Two lighting models meeting
///
/// The document's lights carry an `intensity` in the arbitrary units the old
/// renderer used — the studio key was 1.4 — while the engine works in lux for
/// the sun and candela for everything else. There is no correct conversion
/// between them, only a calibration, and the numbers below are it. What makes
/// this safe rather than arbitrary is the engine's auto-exposure: it meters the
/// frame and adapts, so these set the *relative* brightness of one light
/// against another, which is what the user is actually adjusting, and the
/// absolute level takes care of itself.
pub struct SceneLights {
    use_scene: bool,
    synced: Option<(u64, u64, ShadeMode)>,
}

/// Lux for one unit of document sun intensity. Roughly an overcast sky at 1.0,
/// so the default studio key lands in daylight.
/// Lux per unit of document sun intensity.
///
/// The renderer meters the frame and adapts, so this does not set how bright
/// the image is — `gfx::viewport::calibrate_exposure` does that. What it sets
/// is the key light's strength *relative* to the image-based fill below, which
/// is what the user is really adjusting when they change a light's intensity.
const SUN_LUX_PER_UNIT: f32 = 6_000.0;
/// Candela for one unit of point or spot intensity.
///
/// # The two falloffs do not have the same shape
///
/// The renderer this replaces fell off as `1 / (1 + 0.15 d^2)`, which saturates
/// near the light: at 1 m it still passes 87% of the light, and it can never
/// exceed 100%. Aether is physical — illuminance is `I / d^2` — so it has no
/// ceiling at all and runs away as a light approaches a surface.
///
/// No single constant converts between them. Matching at 6 m, which is what
/// this first tried, is the worst place to match: it agrees at arm's length and
/// is three stops too bright at 1 m, which is where lights in a room actually
/// sit, and that is what "way too bright" looked like.
///
/// So this matches near the light instead, where the old curve is steepest in
/// relative terms and where the error is most visible. A default point light
/// (intensity 3) one metre away lands at about twice the studio key, which is
/// what the old renderer gave; further out it falls below the old curve rather
/// than above it. Erring dim is the right direction — a light that is too weak
/// is a number the user turns up, and one that is too strong is a white frame
/// that hides the model.
const CANDELA_PER_UNIT: f32 = 6_000.0;
/// Luminance, in nits, of a surface whose emissive strength is 1.
///
/// The engine wants a physical figure and the document has a dimensionless
/// slider, so this is the conversion. It is set so that a strength of 1 reads
/// as clearly glowing at the exposure the viewport runs at — a light gizmo with
/// emissive 1 nit is black on screen, which made the gizmos invisible and the
/// comment above them untrue.
const EMISSIVE_NITS: f32 = 2_000.0;

/// How far a punctual light reaches. The engine culls against this, so it is a
/// performance control as much as a look one.
const LIGHT_RANGE: f32 = 30.0;

/// Lux of the overhead fill dome in Rendered mode, when the document has no sun
/// of its own. See the comment at its use for why Rendered mode needs a sun at
/// all. The level is set against the document's own lights, not the studio
/// rig: a default point light a few metres up puts a couple of hundred lux on
/// the floor beneath it, and the fill sitting a stop or two under that keeps
/// the light's pool clearly shaped while every face stays readable.
const SCENE_FILL_LUX: f32 = 100.0;

impl SceneLights {
    pub fn new() -> Self {
        Self { use_scene: false, synced: None }
    }

    /// Whether the frame is lit by the scene's own lights rather than the
    /// studio rig — the same condition the last `sync` applied.
    pub fn scene_active(&self) -> bool {
        self.use_scene
    }

    /// Applies the lighting to the engine's scene.
    ///
    /// Rendered mode is lit by the scene's own lights; every other mode uses
    /// the studio rig. Rebuilds only when the lighting-relevant content
    /// changed — lights and casters, not arbitrary edits, and not the global
    /// scene version, which bumps on every frame of a drag.
    pub fn sync(&mut self, render: &mut RenderScene, scene: &Scene, shade: ShadeMode) {
        self.use_scene = shade == ShadeMode::Rendered;
        let signature = if self.use_scene { lighting_signature(scene) } else { 0 };
        let key = (scene.instance(), signature, shade);
        if self.synced == Some(key) {
            return;
        }
        self.synced = Some(key);
        render.lights.clear();

        if !self.use_scene {
            // The studio rig: a key from above-left and a bright sky to fill.
            // The engine has one directional light, so the old fill light
            // becomes image-based ambient, which is what it was approximating.
            let (intensity, color, direction) = STUDIO_KEY;
            render.sun = SunLight {
                direction: aether_math::Vec3::from(direction).normalize(),
                color: linear_rgb(color),
                intensity: intensity * SUN_LUX_PER_UNIT,
                angular_radius: 0.02, // a soft studio source, not the real sun
                // The studio rig does not cast. It is a lighting *convention*
                // that makes form readable while modelling, not a description
                // of a room, and a shadow it cast would be a fact about a light
                // that is not in the scene — it would move when the user
                // orbited nothing, and hide geometry they are trying to edit.
                // Shadows belong to the scene's own lights, which is what
                // switching to Rendered mode asks for.
                casts_shadows: false,
            };
            // Full-strength image-based fill: this is the old rig's fill light
            // and its ambient term at once, and it is what keeps a surface
            // facing away from the key readable instead of black.
            render.ibl_intensity = aether_math::Vec3::ONE;
            return;
        }

        // Scene lighting: the document's own lights, over a fill dimmed to
        // about a third of the studio rig's. The old renderer used a constant
        // 0.12 ambient here against 0.35 in studio, and the ratio is the point:
        // the scene's lights have to be what shapes the image, but a face no
        // light reaches still has to show its albedo rather than going to
        // black — an unlit side is a surface the user cannot judge.
        //
        // The fill cannot be the obvious `sun.intensity = 0.0`. Everything
        // ambient in the engine is downstream of the sun: the sky's radiance is
        // scaled by it, and the IBL fill is captured *from* the sky — so with
        // the sun at zero the whole frame outside the lights' reach is not dim,
        // it is exactly black. The auto-exposure histogram then meters a frame
        // that is mostly true black, pegs at the bottom of its range, and
        // brightens until the one lit patch is a white hole (the symptom is
        // "the light is far too bright", with any intensity: metering
        // normalises the level, so turning the light down changes nothing).
        // What Rendered mode needs is a dim overcast dome: a weak sun straight
        // overhead, so the sky and its IBL exist, unlit faces read, and the
        // metering has something real to hold on to. It does not cast shadows —
        // it is ambience, not a light in the scene.
        render.ibl_intensity = aether_math::Vec3::splat(0.35);
        render.sun = SunLight {
            direction: aether_math::Vec3::new(0.0, 0.0, -1.0),
            color: linear_rgb(Srgba::WHITE),
            intensity: SCENE_FILL_LUX,
            angular_radius: 0.02,
            casts_shadows: false,
        };

        for object in scene.objects() {
            let Primitive::Light { kind, color, intensity, spot_angle_deg, shadows } =
                object.primitive
            else {
                continue;
            };
            if !object.visible {
                continue;
            }
            let world = scene.world_transform(object.id);
            let position = world.location;
            let direction = world.rotation * glam::Vec3::NEG_Z;
            let color = linear_rgb(srgb_of(color));
            match kind {
                LightKind::Sun => {
                    render.sun = SunLight {
                        direction,
                        color,
                        intensity: intensity * SUN_LUX_PER_UNIT,
                        angular_radius: 0.00465,
                        casts_shadows: shadows,
                    };
                }
                LightKind::Point => render.lights.push(GpuLight::point(
                    position,
                    color,
                    intensity * CANDELA_PER_UNIT,
                    LIGHT_RANGE,
                )),
                LightKind::Spot => {
                    let outer = spot_angle_deg.clamp(1.0, 160.0) * 0.5;
                    render.lights.push(GpuLight::spot(
                        position,
                        direction,
                        color,
                        intensity * CANDELA_PER_UNIT,
                        LIGHT_RANGE,
                        // A soft edge rather than a hard cone: an inner angle
                        // equal to the outer one gives a cut-out circle.
                        outer * 0.75,
                        outer,
                    ));
                }
            }
        }
    }

    #[allow(dead_code)] // read by the lighting panel once it shows the mode
    pub fn use_scene(&self) -> bool {
        self.use_scene
    }
}

/// A document colour as the app's 8-bit sRGB type.
fn srgb_of(c: [f32; 3]) -> Srgba {
    Srgba::new(
        (c[0].clamp(0.0, 1.0) * 255.0) as u8,
        (c[1].clamp(0.0, 1.0) * 255.0) as u8,
        (c[2].clamp(0.0, 1.0) * 255.0) as u8,
        255,
    )
}

/// Sharp-edge cache for the Wireframe shading mode: welded topology edges
/// per object (same look as edit mode), recomputed only when a mesh changes.
pub struct WireframeCache {
    cache: HashMap<ObjectId, (Primitive, bool, u64, Vec<(glam::Vec3, glam::Vec3)>)>,
}

/// One wireframe segment in world space; tier: 0 = normal, 1 = selected,
/// 2 = active, 3 = selected with modifiers, 4 = active with modifiers.
pub type WireSegment = (glam::Vec3, glam::Vec3, u8);

impl WireframeCache {
    pub fn new() -> Self {
        Self { cache: HashMap::new() }
    }

    pub fn segments(&mut self, scene: &Scene, selection: &Selection) -> Vec<WireSegment> {
        let mut out = Vec::new();
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let worlds = scene.world_transforms(); // one O(N) pass for the frame
        for object in scene.objects() {
            if !object.visible {
                continue;
            }
            seen.insert(object.id);
            let stamp = crate::modifiers::stamp(scene, object.id);
            let stale = match self.cache.get(&object.id) {
                Some((p, s, cached_stamp, _)) => {
                    *p != object.primitive || *s != object.smooth || *cached_stamp != stamp
                }
                None => true,
            };
            if stale {
                let topo = crate::edit_mode::build_topology(&display_mesh(scene, object));
                let edges: Vec<(glam::Vec3, glam::Vec3)> = topo
                    .edges
                    .iter()
                    .map(|&(a, b)| (topo.verts[a], topo.verts[b]))
                    .collect();
                self.cache
                    .insert(object.id, (object.primitive, object.smooth, stamp, edges));
            }
            let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
            let has_mod = !object.modifiers.is_empty();
            let tier = if selection.active() == Some(object.id) {
                if has_mod { 4 } else { 2 }
            } else if selection.is_selected(object.id) {
                if has_mod { 3 } else { 1 }
            } else {
                0
            };
            let to_world = |p: glam::Vec3| world.location + world.rotation * (p * world.scale);
            let (_, _, _, edges) = &self.cache[&object.id];
            out.extend(edges.iter().map(|&(a, b)| (to_world(a), to_world(b), tier)));
        }
        self.cache.retain(|id, _| seen.contains(id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::{Transform, WallCutout};

    fn scene_with_cube() -> (Scene, ObjectId) {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        (scene, id)
    }

    fn key_of(scene: &Scene, id: ObjectId) -> u64 {
        mesh_key(scene, scene.object(id).unwrap())
    }

    #[test]
    fn identical_cubes_share_one_upload_whatever_colour_they_are() {
        // Sharing a mesh key is what lets the engine batch them into a single
        // instanced draw, so this is the instancing test.
        let (mut scene, a) = scene_with_cube();
        let b = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        scene.object_mut(b).unwrap().material.base_color = [0.2, 0.4, 0.6];
        assert_eq!(key_of(&scene, a), key_of(&scene, b));
    }

    #[test]
    fn selection_no_longer_splits_the_geometry() {
        // It used to: outlines were a second scaled-up model per selection
        // tier, so selecting an object moved it into its own draw. The outline
        // is a screen-space pass now, and selecting something must not cost a
        // re-upload of its mesh.
        let (scene, a) = scene_with_cube();
        let before = key_of(&scene, a);
        let mut selection = Selection::default();
        selection.set(vec![a], Some(a));
        assert_eq!(key_of(&scene, a), before);
    }

    #[test]
    fn geometry_changes_change_the_key() {
        let (mut scene, a) = scene_with_cube();
        let plain = key_of(&scene, a);

        crate::modifiers::set_subdivision(&mut scene, a, 2);
        assert_ne!(key_of(&scene, a), plain, "subdivision is different geometry");

        crate::modifiers::set_subdivision(&mut scene, a, 0);
        scene.object_mut(a).unwrap().smooth = true;
        assert_ne!(key_of(&scene, a), plain, "shading splits the vertex normals");
    }

    #[test]
    fn a_mesh_that_depends_on_anything_else_is_never_shared() {
        let mut scene = Scene::new();
        let edited = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        {
            let object = scene.object_mut(edited).unwrap();
            object.edited_mesh = Some(object.render_mesh());
        }
        assert!(!shareable(scene.object(edited).unwrap()));

        let wall = scene.add_object(
            Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 },
            Transform::default(),
        );
        assert!(shareable(scene.object(wall).unwrap()), "pristine walls may share");
        scene.object_mut(wall).unwrap().cutouts.push(WallCutout::door(1.0, 2.0, 2.1));
        assert!(
            !shareable(scene.object(wall).unwrap()),
            "a cutout makes the mesh this wall's own"
        );

        // A boolean modifier makes the mesh depend on another object.
        let (mut scene, a) = scene_with_cube();
        let tool = scene.add_object(Primitive::Cube { size: 0.5 }, Transform::default());
        scene.object_mut(a).unwrap().modifiers.push(modeler_core::Modifier::new(
            modeler_core::ModifierKind::Boolean {
                op: modeler_core::BooleanOp::Subtract,
                object: tool,
            },
        ));
        assert!(!shareable(scene.object(a).unwrap()));
    }

    #[test]
    fn two_objects_that_cannot_share_still_get_their_own_entries() {
        // The uniqueness rule mixes the object id in rather than taking a
        // separate code path, so two unshareable objects must not collide.
        let mut scene = Scene::new();
        let a = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        let b = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        for id in [a, b] {
            let object = scene.object_mut(id).unwrap();
            object.edited_mesh = Some(object.render_mesh());
        }
        assert_ne!(key_of(&scene, a), key_of(&scene, b));
    }

    #[test]
    fn face_materials_become_contiguous_index_ranges() {
        // The point of submeshes: one vertex buffer, one range per slot. A
        // range that interleaved slots would draw the wrong triangles.
        let mut mesh = modeler_core::Primitive::Cube { size: 1.0 }.generate(false);
        let tri_count = mesh.indices.len() / 3;
        mesh.tri_materials = vec![0; tri_count];
        mesh.tri_materials[0] = 1;
        mesh.tri_materials[1] = 1;
        mesh.tri_materials[2] = 7; // out of range: falls back to the object's own

        let (data, ranges) = to_aether_mesh(&mesh, "cube".into(), 1);
        assert_eq!(ranges.len(), 2, "slot 0 and slot 1");
        let slot0 = ranges.iter().find(|(s, _)| *s == 0).expect("slot 0").1;
        let slot1 = ranges.iter().find(|(s, _)| *s == 1).expect("slot 1").1;
        assert_eq!(slot1.index_count, 2 * 3);
        assert_eq!(slot0.index_count as usize, (tri_count - 2) * 3);
        // Contiguous and covering the whole index buffer between them.
        assert_eq!(slot0.index_count + slot1.index_count, data.indices.len() as u32);
        assert!(slot0.index_offset == 0 || slot1.index_offset == 0);
        // One vertex buffer, shared: submeshes filter indices, not vertices.
        assert_eq!(data.vertices.len(), mesh.positions.len());
    }

    #[test]
    fn a_mesh_with_no_face_assignment_needs_no_submesh_at_all() {
        let mesh = modeler_core::Primitive::Cube { size: 1.0 }.generate(false);
        let (_, ranges) = to_aether_mesh(&mesh, "cube".into(), 0);
        assert!(ranges.is_empty(), "the common case draws the mesh whole");
    }

    #[test]
    fn tangents_are_generated_so_normal_maps_work() {
        let mesh = modeler_core::Primitive::Cube { size: 1.0 }.generate(false);
        let (data, _) = to_aether_mesh(&mesh, "cube".into(), 0);
        assert!(
            data.vertices.iter().any(|v| v.tangent[0] != 1.0 || v.tangent[1] != 0.0),
            "every tangent is still the placeholder"
        );
    }

    #[test]
    fn a_transparent_material_is_blended_and_double_sided() {
        // Culling a transparent object's back faces leaves a hollow shell.
        let mut material = Material::default();
        material.alpha = 0.4;
        let m = gpu_material(&material, &Primitive::Cube { size: 1.0 }, ShadeMode::MaterialPreview, false, None);
        assert!(m.flags() & material_flags::ALPHA_BLEND != 0);
        assert!(m.flags() & material_flags::DOUBLE_SIDED != 0);
        assert!((m.base_color.w - 0.4).abs() < 1e-6);
    }

    #[test]
    fn solid_mode_ignores_the_object_material_but_not_a_light_gizmo() {
        let mut material = Material::default();
        material.base_color = [0.9, 0.1, 0.1];
        let solid =
            gpu_material(&material, &Primitive::Cube { size: 1.0 }, ShadeMode::Solid, false, None);
        assert!(
            (solid.base_color.x - solid.base_color.y).abs() < 1e-3,
            "Solid must be a neutral studio grey"
        );

        // A light gizmo glows in its own colour in every mode: the gizmo is the
        // light as far as the user is concerned.
        let light = Primitive::Light {
            kind: LightKind::Point,
            color: [1.0, 0.2, 0.1],
            intensity: 1.0,
            spot_angle_deg: 45.0,
            shadows: false,
        };
        let gizmo = gpu_material(&material, &light, ShadeMode::Solid, false, None);
        assert!(gizmo.emissive.x > gizmo.emissive.z);
    }

    #[test]
    fn xray_makes_everything_see_through() {
        let m = gpu_material(
            &Material::default(),
            &Primitive::Cube { size: 1.0 },
            ShadeMode::MaterialPreview,
            true,
            None,
        );
        assert!(m.base_color.w < 0.5);
        assert!(m.flags() & material_flags::ALPHA_BLEND != 0);
    }

    /// These need a real device, so they are opt-in: `cargo test -- --ignored`.
    /// The engine marks its GPU tests the same way.
    mod gpu {
        use super::*;

        fn synced(
            gpu: &Gpu,
            render: &mut SceneRender,
            scene: &Scene,
        ) {
            let mut renderer = aether_render::Renderer::new(
                gpu.clone(),
                aether_render::RendererConfig::default(),
            );
            let mut bridge = crate::texture_bridge::TextureBridge::new();
            render.sync(
                gpu,
                &mut renderer,
                &mut bridge,
                scene,
                &Selection::default(),
                &HashSet::new(),
                ShadeMode::MaterialPreview,
                false,
            );
        }

        #[test]
        #[ignore = "needs a GPU"]
        fn a_moving_object_keeps_a_previous_transform_to_reproject_from() {
            // The bug this guards: rebuilding the scene each frame gave every
            // object `prev_transform == transform`, so motion vectors were zero
            // and TAA smeared anything that moved instead of reprojecting it.
            let gpu = Gpu::new_blocking().expect("a GPU");
            let (mut scene, id) = scene_with_cube();
            let mut render = SceneRender::new();

            synced(&gpu, &mut render, &scene);
            let first = *render.objects.values().next().expect("one object");

            scene.object_mut(id).unwrap().transform.location = glam::Vec3::new(3.0, 0.0, 0.0);
            synced(&gpu, &mut render, &scene);

            let second = *render.objects.values().next().expect("one object");
            assert_eq!(first, second, "the object was recreated instead of moved");

            let object = render.scene.object(second).expect("live");
            assert_eq!(object.transform.translation.x, 3.0);
            assert_eq!(
                object.prev_transform.translation.x, 0.0,
                "the previous transform must be where it was last frame"
            );
        }

        #[test]
        #[ignore = "needs a GPU"]
        fn a_still_object_reports_no_motion() {
            // The other half: a snapshot taken at the wrong moment would give
            // everything a stale previous transform and smear the whole frame.
            let gpu = Gpu::new_blocking().expect("a GPU");
            let (scene, _) = scene_with_cube();
            let mut render = SceneRender::new();
            synced(&gpu, &mut render, &scene);
            synced(&gpu, &mut render, &scene);

            let handle = *render.objects.values().next().expect("one object");
            let object = render.scene.object(handle).expect("live");
            assert_eq!(object.transform.translation, object.prev_transform.translation);
        }

        #[test]
        #[ignore = "needs a GPU"]
        fn a_hidden_object_leaves_the_scene_and_comes_back() {
            let gpu = Gpu::new_blocking().expect("a GPU");
            let (mut scene, id) = scene_with_cube();
            let mut render = SceneRender::new();

            synced(&gpu, &mut render, &scene);
            assert_eq!(render.scene.object_count(), 1);

            scene.object_mut(id).unwrap().visible = false;
            synced(&gpu, &mut render, &scene);
            assert_eq!(render.scene.object_count(), 0, "a hidden object still draws");

            scene.object_mut(id).unwrap().visible = true;
            synced(&gpu, &mut render, &scene);
            assert_eq!(render.scene.object_count(), 1);
        }

        #[test]
        #[ignore = "needs a GPU"]
        fn identical_objects_share_one_upload() {
            // The instancing claim, end to end rather than at the key level.
            let gpu = Gpu::new_blocking().expect("a GPU");
            let (mut scene, _) = scene_with_cube();
            for _ in 0..4 {
                scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
            }
            let mut render = SceneRender::new();
            synced(&gpu, &mut render, &scene);

            assert_eq!(render.scene.object_count(), 5);
            assert_eq!(render.scene.mesh_count(), 1, "five cubes, one mesh");
        }
    }
}
