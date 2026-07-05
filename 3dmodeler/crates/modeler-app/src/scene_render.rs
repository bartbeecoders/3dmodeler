//! Derives three-d GPU models from the `modeler-core` scene document, plus
//! Blender-style selection outlines and overlap warnings.
//!
//! Models are cached per object: transform-only changes (modal drags, physics
//! playback) just update the transformation matrix; meshes are regenerated
//! only when primitive parameters, shading or material change.

use crate::selection::Selection;
use modeler_core::glam;
use modeler_core::{MeshData, Material, ObjectId, Primitive, Scene, Transform};
use std::collections::{HashMap, HashSet};
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

fn physical_material(
    context: &Context,
    material: &Material,
    mode: ShadeMode,
    xray: bool,
) -> PhysicalMaterial {
    // Solid ignores the object material for a uniform studio look
    let ([r, g, b], roughness, metallic) = match mode {
        ShadeMode::Solid => ([0.72, 0.72, 0.75], 0.85, 0.0),
        _ => (material.base_color, material.roughness, material.metallic),
    };
    let cpu = CpuMaterial {
        albedo: Srgba::new(
            (r * 255.0) as u8,
            (g * 255.0) as u8,
            (b * 255.0) as u8,
            if xray { 110 } else { 255 },
        ),
        roughness,
        metallic,
        ..Default::default()
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

        for object in scene.objects() {
            if !object.visible {
                continue;
            }
            seen.insert(object.id);
            self.order.push(object.id);
            let transformation = transform_mat(&scene.world_transform(object.id));

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
                let cpu_mesh = to_cpu_mesh(&object.render_mesh());
                let model = Gm::new(
                    Mesh::new(context, &cpu_mesh),
                    physical_material(context, &object.material, mode, xray),
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
            } else if rebuild_material {
                let cached = self.cache.get_mut(&object.id).unwrap();
                cached.material = object.material;
                cached.mode = mode;
                cached.xray = xray;
                cached.model.material =
                    physical_material(context, &object.material, mode, xray);
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
                let topo = crate::edit_mode::build_topology(&object.render_mesh());
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
            let world = scene.world_transform(object.id);
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