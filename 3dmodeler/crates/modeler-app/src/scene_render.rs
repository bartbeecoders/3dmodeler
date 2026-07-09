//! Derives three-d GPU models from the `modeler-core` scene document, plus
//! Blender-style selection outlines and overlap warnings.
//!
//! Objects that share a mesh and material (brick piles, duplicated
//! furniture) are grouped into a single instanced draw call with their
//! base color riding per instance; everything else keeps a per-object
//! cached model where transform-only changes (modal drags, physics
//! playback) just update the transformation matrix and meshes are
//! regenerated only when primitive parameters, shading or material change.

use crate::selection::Selection;
use modeler_core::glam;
use modeler_core::{LightKind, MeshData, Material, ObjectId, Primitive, Scene, Transform};
use std::collections::hash_map::{DefaultHasher, Entry};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use three_d::*;

/// Viewport shading mode (Blender's Z pie, reduced).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ShadeMode {
    /// Only the objects' sharp edges, drawn by the overlay.
    Wireframe,
    /// Neutral gray studio look, object materials ignored.
    Solid,
    /// Full materials and lights (the default).
    Shaded,
}

/// What illuminates the Shaded viewport (Blender's "scene lights" toggle).
/// Solid mode always uses the studio rig.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LightingMode {
    /// Built-in studio rig (ambient + key + fill); scene lights ignored.
    Studio,
    /// The scene's light objects (with shadows) over a faint ambient.
    Scene,
}

// Blender's outline colors: light orange for the active object, darker
// orange for other selected objects; red warns about overlaps while placing.
const ACTIVE_OUTLINE: Srgba = Srgba::new(255, 170, 64, 255);
const SELECTED_OUTLINE: Srgba = Srgba::new(230, 110, 20, 255);
const OVERLAP_OUTLINE: Srgba = Srgba::new(235, 60, 50, 255);
const OUTLINE_SCALE: f32 = 1.03;

struct CachedModel {
    primitive: Primitive,
    smooth: bool,
    mesh_revision: u64,
    mode: ShadeMode,
    xray: bool,
    material: Material,
    cpu_mesh: CpuMesh,
    model: Gm<Mesh, PhysicalMaterial>,
    outline: Option<(Srgba, Gm<Mesh, ColorMaterial>)>,
}

/// Selection tier of an object. Part of the instancing group key: outlines
/// are drawn per group, so members of one group must share a tier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum OutlineTier {
    None,
    Selected,
    Active,
    Overlap,
}

impl OutlineTier {
    fn of(id: ObjectId, selection: &Selection, overlaps: &HashSet<ObjectId>) -> Self {
        if !selection.is_selected(id) {
            Self::None
        } else if overlaps.contains(&id) {
            Self::Overlap
        } else if selection.active() == Some(id) {
            Self::Active
        } else {
            Self::Selected
        }
    }

    // outline: overlap warning > active > selected
    fn color(self) -> Option<Srgba> {
        match self {
            Self::None => None,
            Self::Selected => Some(SELECTED_OUTLINE),
            Self::Active => Some(ACTIVE_OUTLINE),
            Self::Overlap => Some(OVERLAP_OUTLINE),
        }
    }
}

/// Everything two objects must share to be drawn by one instanced call.
/// The base color is NOT here — it rides per instance, so a brick pile
/// with per-brick shade variation is still a single draw.
#[derive(Clone, Copy, PartialEq, Debug)]
struct GroupKey {
    primitive: Primitive,
    smooth: bool,
    subdivision: u8,
    /// Material minus base color; zeroed in Solid mode, which ignores
    /// object materials entirely (so any-material duplicates group there).
    roughness: f32,
    metallic: f32,
    mode: ShadeMode,
    xray: bool,
    tier: OutlineTier,
}

fn group_hash(key: &GroupKey) -> u64 {
    let mut h = DefaultHasher::new();
    hash_primitive(&mut h, &key.primitive);
    key.smooth.hash(&mut h);
    key.subdivision.hash(&mut h);
    hash_f32(&mut h, key.roughness);
    hash_f32(&mut h, key.metallic);
    key.mode.hash(&mut h);
    key.xray.hash(&mut h);
    key.tier.hash(&mut h);
    h.finish()
}

/// Mesh identity within a group key (what `display_mesh` depends on for
/// instanceable objects) — the shared CpuMesh cache is keyed on this so
/// selection changes move a group between tiers without re-meshing.
fn group_mesh_hash(key: &GroupKey) -> u64 {
    let mut h = DefaultHasher::new();
    hash_primitive(&mut h, &key.primitive);
    key.smooth.hash(&mut h);
    key.subdivision.hash(&mut h);
    h.finish()
}

/// The instancing group an object belongs to, or None when its mesh is
/// unique (edited meshes, walls with cutouts, shaped floors) or it is a
/// light gizmo (colored by its primitive; excluded from shadow casters).
fn instance_key(
    object: &modeler_core::Object,
    tier: OutlineTier,
    mode: ShadeMode,
    xray: bool,
) -> Option<(u64, GroupKey)> {
    if object.edited_mesh.is_some() || object.primitive.is_light() {
        return None;
    }
    if matches!(object.primitive, Primitive::Wall { .. }) && !object.cutouts.is_empty() {
        return None;
    }
    if matches!(object.primitive, Primitive::Floor { .. }) && !object.floor_outline.is_empty() {
        return None;
    }
    let (roughness, metallic) = match mode {
        ShadeMode::Solid => (0.0, 0.0),
        _ => (object.material.roughness, object.material.metallic),
    };
    let key = GroupKey {
        primitive: object.primitive,
        smooth: object.smooth,
        subdivision: object.subdivision,
        roughness,
        metallic,
        mode,
        xray,
        tier,
    };
    Some((group_hash(&key), key))
}

struct InstancedGroup {
    key: GroupKey,
    model: Gm<InstancedMesh, PhysicalMaterial>,
    outline: Option<Gm<InstancedMesh, ColorMaterial>>,
    /// Hash of member ids + transforms + colors; instance buffers are
    /// re-uploaded only when it changes.
    instances_sig: u64,
}

pub struct SceneRender {
    cache: HashMap<ObjectId, CachedModel>,
    order: Vec<ObjectId>,
    groups: HashMap<u64, InstancedGroup>,
    group_order: Vec<u64>,
    /// Shared meshes of the instanced groups, keyed by mesh identity.
    mesh_cache: HashMap<u64, CpuMesh>,
}

/// The mesh the viewport shows: the base mesh with the object's
/// subdivision-surface levels applied. Editing and collision keep using
/// the base mesh (the cage), like Blender's subsurf modifier.
fn display_mesh(object: &modeler_core::Object) -> MeshData {
    let base = object.render_mesh();
    if object.subdivision == 0 {
        return base;
    }
    crate::mesh_edit::subdivide(&base, object.subdivision, object.smooth)
}

fn to_cpu_mesh(data: &MeshData) -> CpuMesh {
    CpuMesh {
        positions: Positions::F32(data.positions.iter().map(|p| vec3(p.x, p.y, p.z)).collect()),
        normals: Some(data.normals.iter().map(|n| vec3(n.x, n.y, n.z)).collect()),
        indices: Indices::U32(data.indices.clone()),
        ..Default::default()
    }
}

pub fn transform_mat(t: &Transform) -> Mat4 {
    let q = Quat::new(t.rotation.w, t.rotation.x, t.rotation.y, t.rotation.z);
    Mat4::from_translation(vec3(t.location.x, t.location.y, t.location.z))
        * Mat4::from(q)
        * Mat4::from_nonuniform_scale(t.scale.x, t.scale.y, t.scale.z)
}

fn srgb(c: [f32; 3], alpha: u8) -> Srgba {
    Srgba::new(
        (c[0].clamp(0.0, 1.0) * 255.0) as u8,
        (c[1].clamp(0.0, 1.0) * 255.0) as u8,
        (c[2].clamp(0.0, 1.0) * 255.0) as u8,
        alpha,
    )
}

fn physical_material(
    context: &Context,
    material: &Material,
    primitive: &Primitive,
    mode: ShadeMode,
    xray: bool,
) -> PhysicalMaterial {
    let alpha = if xray { 110 } else { 255 };
    let cpu = if let Primitive::Light { color, .. } = primitive {
        // light gizmos glow in their own color, in every shading mode
        CpuMaterial {
            albedo: Srgba::new(25, 25, 25, alpha),
            emissive: srgb(*color, 255),
            roughness: 1.0,
            metallic: 0.0,
            ..Default::default()
        }
    } else {
        // Solid ignores the object material for a uniform studio look
        let ([r, g, b], roughness, metallic) = match mode {
            ShadeMode::Solid => ([0.72, 0.72, 0.75], 0.85, 0.0),
            _ => (material.base_color, material.roughness, material.metallic),
        };
        CpuMaterial {
            albedo: srgb([r, g, b], alpha),
            roughness,
            metallic,
            ..Default::default()
        }
    };
    if xray {
        // see-through: alpha blend both faces
        let mut m = PhysicalMaterial::new_transparent(context, &cpu);
        m.render_states.cull = Cull::None;
        m
    } else {
        PhysicalMaterial::new_opaque(context, &cpu)
    }
}

/// Per-instance color: the object's base color in Shaded mode (multiplied
/// onto the group's white albedo); white in Solid, whose gray albedo lives
/// on the material.
fn instance_color(object: &modeler_core::Object, mode: ShadeMode) -> Srgba {
    match mode {
        ShadeMode::Shaded => srgb(object.material.base_color, 255),
        _ => Srgba::WHITE,
    }
}

impl SceneRender {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            order: Vec::new(),
            groups: HashMap::new(),
            group_order: Vec::new(),
            mesh_cache: HashMap::new(),
        }
    }

    pub fn sync(
        &mut self,
        scene: &Scene,
        selection: &Selection,
        overlaps: &HashSet<ObjectId>,
        context: &Context,
        mode: ShadeMode,
        xray: bool,
    ) {
        self.order.clear();
        self.group_order.clear();
        if mode == ShadeMode::Wireframe {
            return; // edges are drawn by WireRender; keep the caches warm
        }
        let worlds = scene.world_transforms(); // one O(N) pass for the frame

        // Pass 1: partition visible objects into instancing buckets and
        // per-object "singles" (unique meshes and light gizmos).
        struct Bucket {
            key: GroupKey,
            members: Vec<(ObjectId, Mat4, Srgba)>,
        }
        let mut buckets: HashMap<u64, Bucket> = HashMap::new();
        let mut bucket_order: Vec<u64> = Vec::new();
        let mut singles: Vec<(ObjectId, Mat4, OutlineTier)> = Vec::new();
        for object in scene.objects() {
            if !object.visible {
                continue;
            }
            let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
            let transformation = transform_mat(&world);
            let tier = OutlineTier::of(object.id, selection, overlaps);
            match instance_key(object, tier, mode, xray) {
                Some((hash, key)) => match buckets.entry(hash) {
                    Entry::Occupied(mut e) if e.get().key == key => {
                        e.get_mut()
                            .members
                            .push((object.id, transformation, instance_color(object, mode)));
                    }
                    // different key hashed to the same bucket: draw solo
                    Entry::Occupied(_) => singles.push((object.id, transformation, tier)),
                    Entry::Vacant(v) => {
                        bucket_order.push(hash);
                        v.insert(Bucket {
                            key,
                            members: vec![(
                                object.id,
                                transformation,
                                instance_color(object, mode),
                            )],
                        });
                    }
                },
                None => singles.push((object.id, transformation, tier)),
            }
        }

        // Pass 2: sync the instanced groups; lone bucket members fall back
        // to the per-object path (no instancing overhead, warm caches).
        for hash in bucket_order {
            let bucket = buckets.remove(&hash).expect("bucket from this frame");
            if let [(id, transformation, _)] = bucket.members[..] {
                singles.push((id, transformation, bucket.key.tier));
                continue;
            }
            self.group_order.push(hash);
            self.sync_group(scene, context, hash, bucket.key, &bucket.members, mode, xray);
        }

        // Pass 3: per-object models for the singles, in scene order.
        for &(id, transformation, tier) in &singles {
            let Some(object) = scene.object(id) else { continue };
            self.order.push(id);
            self.sync_single(object, transformation, tier, context, mode, xray);
        }

        let single_ids: HashSet<ObjectId> = singles.iter().map(|&(id, _, _)| id).collect();
        self.cache.retain(|id, _| single_ids.contains(id));
        let group_ids: HashSet<u64> = self.group_order.iter().copied().collect();
        self.groups.retain(|hash, _| group_ids.contains(hash));
        let mesh_ids: HashSet<u64> =
            self.groups.values().map(|g| group_mesh_hash(&g.key)).collect();
        self.mesh_cache.retain(|hash, _| mesh_ids.contains(hash));
    }

    /// Create or update one instanced group (model + optional outline).
    fn sync_group(
        &mut self,
        scene: &Scene,
        context: &Context,
        hash: u64,
        key: GroupKey,
        members: &[(ObjectId, Mat4, Srgba)],
        mode: ShadeMode,
        xray: bool,
    ) {
        let mut h = DefaultHasher::new();
        for (id, m, c) in members {
            id.0.hash(&mut h);
            let cells: &[f32; 16] = m.as_ref();
            for f in cells {
                hash_f32(&mut h, *f);
            }
            (c.r, c.g, c.b, c.a).hash(&mut h);
        }
        let sig = h.finish();

        let up_to_date = match self.groups.get(&hash) {
            Some(g) => g.key == key && g.instances_sig == sig,
            None => false,
        };
        if up_to_date {
            return;
        }

        let instances = Instances {
            transformations: members.iter().map(|&(_, m, _)| m).collect(),
            colors: (mode == ShadeMode::Shaded)
                .then(|| members.iter().map(|&(_, _, c)| c).collect()),
            ..Default::default()
        };
        let outline_instances = key.tier.color().map(|_| Instances {
            transformations: members
                .iter()
                .map(|&(_, m, _)| m * Mat4::from_scale(OUTLINE_SCALE))
                .collect(),
            ..Default::default()
        });

        match self.groups.get_mut(&hash) {
            Some(group) if group.key == key => {
                group.model.geometry.set_instances(&instances);
                if let (Some(outline), Some(instances)) =
                    (&mut group.outline, &outline_instances)
                {
                    outline.geometry.set_instances(instances);
                }
                group.instances_sig = sig;
            }
            _ => {
                let mesh_hash = group_mesh_hash(&key);
                if !self.mesh_cache.contains_key(&mesh_hash) {
                    let exemplar = scene.object(members[0].0).expect("member from this frame");
                    self.mesh_cache.insert(mesh_hash, to_cpu_mesh(&display_mesh(exemplar)));
                }
                let cpu_mesh = &self.mesh_cache[&mesh_hash];
                // white albedo: the per-instance colors carry the base color
                let material = physical_material(
                    context,
                    &Material {
                        base_color: [1.0, 1.0, 1.0],
                        roughness: key.roughness,
                        metallic: key.metallic,
                    },
                    &key.primitive,
                    mode,
                    xray,
                );
                let model = Gm::new(InstancedMesh::new(context, &instances, cpu_mesh), material);
                let outline = key.tier.color().map(|color| {
                    let material = ColorMaterial {
                        color,
                        render_states: RenderStates {
                            cull: Cull::Front,
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    Gm::new(
                        InstancedMesh::new(
                            context,
                            outline_instances.as_ref().expect("tier has a color"),
                            cpu_mesh,
                        ),
                        material,
                    )
                });
                self.groups
                    .insert(hash, InstancedGroup { key, model, outline, instances_sig: sig });
            }
        }
    }

    /// Sync one per-object cached model (the pre-instancing path).
    fn sync_single(
        &mut self,
        object: &modeler_core::Object,
        transformation: Mat4,
        tier: OutlineTier,
        context: &Context,
        mode: ShadeMode,
        xray: bool,
    ) {
        let rebuild_mesh = match self.cache.get(&object.id) {
            Some(cached) => {
                cached.primitive != object.primitive
                    || cached.smooth != object.smooth
                    || cached.mesh_revision != object.mesh_revision
            }
            None => true,
        };
        let rebuild_material = match self.cache.get(&object.id) {
            Some(cached) => {
                cached.material != object.material
                    || cached.mode != mode
                    || cached.xray != xray
            }
            None => true,
        };

        if rebuild_mesh {
            let new_mesh = display_mesh(object);
            // Edit-mode vertex drags bump mesh_revision every frame but
            // keep the topology: update the existing vertex buffers in
            // place instead of recreating mesh + material + outline.
            let updated_in_place = (|| {
                let cached = self.cache.get_mut(&object.id)?;
                if cached.primitive != object.primitive || cached.smooth != object.smooth {
                    return None;
                }
                let same_topology = match (&cached.cpu_mesh.positions, &cached.cpu_mesh.indices)
                {
                    (Positions::F32(old_pos), Indices::U32(old_idx)) => {
                        old_pos.len() == new_mesh.positions.len()
                            && old_idx.as_slice() == new_mesh.indices.as_slice()
                    }
                    _ => false,
                };
                if !same_topology {
                    return None;
                }
                let positions: Vec<Vec3> =
                    new_mesh.positions.iter().map(|p| vec3(p.x, p.y, p.z)).collect();
                let normals: Vec<Vec3> =
                    new_mesh.normals.iter().map(|n| vec3(n.x, n.y, n.z)).collect();
                cached.model.geometry.set_positions(&positions).ok()?;
                cached.model.geometry.set_normals(&normals).ok()?;
                if let Some((_, outline)) = &mut cached.outline {
                    outline.geometry.set_positions(&positions).ok()?;
                    outline.geometry.set_normals(&normals).ok()?;
                }
                cached.cpu_mesh.positions = Positions::F32(positions);
                cached.cpu_mesh.normals = Some(normals);
                cached.mesh_revision = object.mesh_revision;
                Some(())
            })()
            .is_some();

            if !updated_in_place {
                let cpu_mesh = to_cpu_mesh(&new_mesh);
                let model = Gm::new(
                    Mesh::new(context, &cpu_mesh),
                    physical_material(context, &object.material, &object.primitive, mode, xray),
                );
                self.cache.insert(
                    object.id,
                    CachedModel {
                        primitive: object.primitive,
                        smooth: object.smooth,
                        mesh_revision: object.mesh_revision,
                        mode,
                        xray,
                        material: object.material,
                        cpu_mesh,
                        model,
                        outline: None,
                    },
                );
            }
        } else if rebuild_material {
            let cached = self.cache.get_mut(&object.id).unwrap();
            cached.material = object.material;
            cached.mode = mode;
            cached.xray = xray;
            cached.model.material =
                physical_material(context, &object.material, &object.primitive, mode, xray);
        }

        let cached = self.cache.get_mut(&object.id).unwrap();
        cached.model.set_transformation(transformation);

        let desired_color = tier.color();

        match (desired_color, &mut cached.outline) {
            (None, outline) => *outline = None,
            (Some(color), Some((current, gm))) if *current == color => {
                gm.set_transformation(transformation * Mat4::from_scale(OUTLINE_SCALE));
            }
            (Some(color), outline) => {
                let material = ColorMaterial {
                    color,
                    render_states: RenderStates {
                        cull: Cull::Front,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut gm = Gm::new(Mesh::new(context, &cached.cpu_mesh), material);
                gm.set_transformation(transformation * Mat4::from_scale(OUTLINE_SCALE));
                *outline = Some((color, gm));
            }
        }
    }

    pub fn models(&self) -> impl Iterator<Item = &dyn Object> {
        self.order
            .iter()
            .filter_map(|id| self.cache.get(id).map(|c| &c.model as &dyn Object))
            .chain(
                self.group_order
                    .iter()
                    .filter_map(|hash| self.groups.get(hash).map(|g| &g.model as &dyn Object)),
            )
    }

    pub fn outlines(&self) -> impl Iterator<Item = &dyn Object> {
        self.order
            .iter()
            .filter_map(|id| {
                self.cache
                    .get(id)
                    .and_then(|c| c.outline.as_ref().map(|(_, gm)| gm as &dyn Object))
            })
            .chain(self.group_order.iter().filter_map(|hash| {
                self.groups
                    .get(hash)
                    .and_then(|g| g.outline.as_ref().map(|gm| gm as &dyn Object))
            }))
    }

    /// Geometry for shadow maps: every visible model EXCEPT light gizmos —
    /// a light sits inside its own gizmo mesh, which would shadow the whole
    /// scene. (Instanced groups never contain lights.)
    pub fn shadow_casters(&self) -> Vec<&dyn Geometry> {
        self.order
            .iter()
            .filter_map(|id| self.cache.get(id))
            .filter(|c| !c.primitive.is_light())
            .map(|c| &c.model.geometry as &dyn Geometry)
            .chain(
                self.group_order
                    .iter()
                    .filter_map(|hash| self.groups.get(hash))
                    .map(|g| &g.model.geometry as &dyn Geometry),
            )
            .collect()
    }
}

/// The lights illuminating the viewport: a fixed studio rig, plus three-d
/// lights derived from the scene's light objects when the lighting mode asks
/// for them. Scene lights (and their shadow maps) are rebuilt only when the
/// scene changes.
pub struct SceneLights {
    ambient: AmbientLight,
    key: DirectionalLight,
    fill: DirectionalLight,
    /// Faint base light so a scene without lights is not pitch black.
    scene_ambient: AmbientLight,
    suns: Vec<DirectionalLight>,
    points: Vec<PointLight>,
    spots: Vec<SpotLight>,
    use_scene: bool,
    synced: Option<(u64, u64, LightingMode, ShadeMode)>,
}

/// Distance falloff for point and spot lights (quadratic, gentle enough to
/// light a room from a few meters away).
const ATTENUATION: Attenuation = Attenuation { constant: 1.0, linear: 0.0, quadratic: 0.15 };
const SHADOW_MAP_SIZE: u32 = 1024;

fn hash_f32<H: Hasher>(h: &mut H, f: f32) {
    f.to_bits().hash(h);
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

impl SceneLights {
    pub fn new(context: &Context) -> Self {
        // Z-up studio rig: key light from above-left, cool fill from the
        // opposite side.
        Self {
            ambient: AmbientLight::new(context, 0.35, Srgba::WHITE),
            key: DirectionalLight::new(context, 1.4, Srgba::WHITE, vec3(-0.4, 0.35, -0.85)),
            fill: DirectionalLight::new(
                context,
                0.5,
                Srgba::new(180, 190, 210, 255),
                vec3(0.6, -0.5, -0.2),
            ),
            scene_ambient: AmbientLight::new(context, 0.06, Srgba::WHITE),
            suns: Vec::new(),
            points: Vec::new(),
            spots: Vec::new(),
            use_scene: false,
            synced: None,
        }
    }

    /// Rebuild the scene lights (and shadow maps) if the LIGHTING-RELEVANT
    /// content changed (lights or casters — not arbitrary scene edits, and
    /// not the global version, which bumps every frame of a drag). Call
    /// AFTER `SceneRender::sync` — shadow maps render the current models.
    pub fn sync(
        &mut self,
        context: &Context,
        scene: &Scene,
        render: &SceneRender,
        shade: ShadeMode,
        mode: LightingMode,
    ) {
        self.use_scene = shade == ShadeMode::Shaded && mode == LightingMode::Scene;
        let signature = if self.use_scene { lighting_signature(scene) } else { 0 };
        let key = (scene.instance(), signature, mode, shade);
        if self.synced == Some(key) {
            return;
        }
        self.synced = Some(key);
        self.suns.clear();
        self.points.clear();
        self.spots.clear();
        if !self.use_scene {
            return;
        }

        let casters = render.shadow_casters();
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
            let position = vec3(world.location.x, world.location.y, world.location.z);
            let dir = world.rotation * glam::Vec3::NEG_Z;
            let direction = vec3(dir.x, dir.y, dir.z);
            let color = srgb(color, 255);
            match kind {
                LightKind::Sun => {
                    let mut light = DirectionalLight::new(context, intensity, color, direction);
                    if shadows {
                        let _ = light.generate_shadow_map(SHADOW_MAP_SIZE, casters.clone());
                    }
                    self.suns.push(light);
                }
                LightKind::Point => {
                    // three-d point lights cannot cast shadows
                    self.points.push(PointLight::new(
                        context, intensity, color, position, ATTENUATION,
                    ));
                }
                LightKind::Spot => {
                    let mut light = SpotLight::new(
                        context,
                        intensity,
                        color,
                        position,
                        direction,
                        degrees(0.5 * spot_angle_deg.clamp(1.0, 160.0)),
                        ATTENUATION,
                    );
                    if shadows {
                        let _ = light.generate_shadow_map(SHADOW_MAP_SIZE, casters.clone());
                    }
                    self.spots.push(light);
                }
            }
        }
    }

    /// The lights to pass to the render call this frame.
    pub fn active(&self) -> Vec<&dyn Light> {
        if self.use_scene {
            let mut lights: Vec<&dyn Light> = vec![&self.scene_ambient];
            lights.extend(self.suns.iter().map(|l| l as &dyn Light));
            lights.extend(self.points.iter().map(|l| l as &dyn Light));
            lights.extend(self.spots.iter().map(|l| l as &dyn Light));
            lights
        } else {
            vec![&self.ambient, &self.key, &self.fill]
        }
    }
}

/// Sharp-edge cache for the Wireframe shading mode: welded topology edges
/// per object (same look as edit mode), recomputed only when a mesh changes.
pub struct WireframeCache {
    cache: HashMap<ObjectId, (Primitive, bool, u64, Vec<(glam::Vec3, glam::Vec3)>)>,
}

/// One wireframe segment in world space; tier: 0 = normal, 1 = selected,
/// 2 = active object.
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
            let stale = match self.cache.get(&object.id) {
                Some((p, s, rev, _)) => {
                    *p != object.primitive || *s != object.smooth || *rev != object.mesh_revision
                }
                None => true,
            };
            if stale {
                let topo = crate::edit_mode::build_topology(&display_mesh(object));
                let edges: Vec<(glam::Vec3, glam::Vec3)> = topo
                    .edges
                    .iter()
                    .map(|&(a, b)| (topo.verts[a], topo.verts[b]))
                    .collect();
                self.cache.insert(
                    object.id,
                    (object.primitive, object.smooth, object.mesh_revision, edges),
                );
            }
            let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
            let tier = if selection.active() == Some(object.id) {
                2
            } else if selection.is_selected(object.id) {
                1
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
    use modeler_core::WallCutout;

    fn scene_with_cube() -> (Scene, ObjectId) {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        (scene, id)
    }

    fn key_of(
        scene: &Scene,
        id: ObjectId,
        tier: OutlineTier,
        mode: ShadeMode,
    ) -> Option<(u64, GroupKey)> {
        instance_key(scene.object(id).unwrap(), tier, mode, false)
    }

    #[test]
    fn identical_cubes_share_a_group_regardless_of_base_color() {
        let (mut scene, a) = scene_with_cube();
        let b = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        scene.object_mut(b).unwrap().material.base_color = [0.2, 0.4, 0.6];

        let ka = key_of(&scene, a, OutlineTier::None, ShadeMode::Shaded).unwrap();
        let kb = key_of(&scene, b, OutlineTier::None, ShadeMode::Shaded).unwrap();
        assert_eq!(ka.0, kb.0, "base color must ride per instance, not split groups");
        assert_eq!(ka.1, kb.1);
    }

    #[test]
    fn roughness_splits_groups_in_shaded_but_not_in_solid() {
        let (mut scene, a) = scene_with_cube();
        let b = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        scene.object_mut(b).unwrap().material.roughness = 0.1;

        let shaded_a = key_of(&scene, a, OutlineTier::None, ShadeMode::Shaded).unwrap();
        let shaded_b = key_of(&scene, b, OutlineTier::None, ShadeMode::Shaded).unwrap();
        assert_ne!(shaded_a.1, shaded_b.1);

        let solid_a = key_of(&scene, a, OutlineTier::None, ShadeMode::Solid).unwrap();
        let solid_b = key_of(&scene, b, OutlineTier::None, ShadeMode::Solid).unwrap();
        assert_eq!(solid_a.1, solid_b.1, "Solid ignores materials entirely");
    }

    #[test]
    fn selection_tier_and_mesh_identity_split_groups() {
        let (mut scene, a) = scene_with_cube();

        let unselected = key_of(&scene, a, OutlineTier::None, ShadeMode::Shaded).unwrap();
        let selected = key_of(&scene, a, OutlineTier::Selected, ShadeMode::Shaded).unwrap();
        assert_ne!(unselected.1, selected.1, "outlines are per group");

        scene.object_mut(a).unwrap().subdivision = 2;
        let subdivided = key_of(&scene, a, OutlineTier::None, ShadeMode::Shaded).unwrap();
        assert_ne!(unselected.1, subdivided.1);

        scene.object_mut(a).unwrap().subdivision = 0;
        scene.object_mut(a).unwrap().smooth = true;
        let smooth = key_of(&scene, a, OutlineTier::None, ShadeMode::Shaded).unwrap();
        assert_ne!(unselected.1, smooth.1);
    }

    #[test]
    fn unique_meshes_and_lights_never_instance() {
        let mut scene = Scene::new();
        let edited = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        {
            let object = scene.object_mut(edited).unwrap();
            object.edited_mesh = Some(object.render_mesh());
        }
        assert!(key_of(&scene, edited, OutlineTier::None, ShadeMode::Shaded).is_none());

        let wall = scene.add_object(
            Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 },
            Transform::default(),
        );
        assert!(
            key_of(&scene, wall, OutlineTier::None, ShadeMode::Shaded).is_some(),
            "pristine walls may instance"
        );
        scene.object_mut(wall).unwrap().cutouts.push(WallCutout::door(1.0, 2.0, 2.1));
        assert!(
            key_of(&scene, wall, OutlineTier::None, ShadeMode::Shaded).is_none(),
            "cutouts make the mesh unique"
        );

        let light = scene.add_object(
            Primitive::Light {
                kind: LightKind::Point,
                color: [1.0, 1.0, 1.0],
                intensity: 1.0,
                spot_angle_deg: 45.0,
                shadows: false,
            },
            Transform::default(),
        );
        assert!(key_of(&scene, light, OutlineTier::None, ShadeMode::Shaded).is_none());
    }
}