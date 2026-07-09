//! Derives three-d GPU models from the `modeler-core` scene document, plus
//! Blender-style selection outlines and overlap warnings.
//!
//! Models are cached per object: transform-only changes (modal drags, physics
//! playback) just update the transformation matrix; meshes are regenerated
//! only when primitive parameters, shading or material change.

use crate::selection::Selection;
use modeler_core::glam;
use modeler_core::{LightKind, MeshData, Material, ObjectId, Primitive, Scene, Transform};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use three_d::*;

/// Viewport shading mode (Blender's Z pie, reduced).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

pub struct SceneRender {
    cache: HashMap<ObjectId, CachedModel>,
    order: Vec<ObjectId>,
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

impl SceneRender {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            order: Vec::new(),
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
        if mode == ShadeMode::Wireframe {
            return; // edges are drawn by the overlay; keep the cache warm
        }
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let worlds = scene.world_transforms(); // one O(N) pass for the frame

        for object in scene.objects() {
            if !object.visible {
                continue;
            }
            seen.insert(object.id);
            self.order.push(object.id);
            let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
            let transformation = transform_mat(&world);

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

            // outline: overlap warning > active > selected
            let desired_color = if selection.is_selected(object.id) {
                Some(if overlaps.contains(&object.id) {
                    OVERLAP_OUTLINE
                } else if selection.active() == Some(object.id) {
                    ACTIVE_OUTLINE
                } else {
                    SELECTED_OUTLINE
                })
            } else {
                None
            };

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

        self.cache.retain(|id, _| seen.contains(id));
    }

    pub fn models(&self) -> impl Iterator<Item = &Gm<Mesh, PhysicalMaterial>> {
        self.order
            .iter()
            .filter_map(|id| self.cache.get(id).map(|c| &c.model))
    }

    pub fn outlines(&self) -> impl Iterator<Item = &Gm<Mesh, ColorMaterial>> {
        self.order
            .iter()
            .filter_map(|id| self.cache.get(id).and_then(|c| c.outline.as_ref().map(|(_, gm)| gm)))
    }

    /// Geometry for shadow maps: every visible model EXCEPT light gizmos —
    /// a light sits inside its own gizmo mesh, which would shadow the whole
    /// scene.
    pub fn shadow_casters(&self) -> Vec<&Mesh> {
        self.order
            .iter()
            .filter_map(|id| self.cache.get(id))
            .filter(|c| !c.primitive.is_light())
            .map(|c| &c.model.geometry)
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
fn hash_primitive<H: Hasher>(h: &mut H, p: &Primitive) {
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