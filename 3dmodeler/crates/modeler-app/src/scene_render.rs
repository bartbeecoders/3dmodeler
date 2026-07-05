//! Derives three-d GPU models from the `modeler-core` scene document, plus
//! Blender-style selection outlines and overlap warnings.
//!
//! Models are cached per object: transform-only changes (modal drags, physics
//! playback) just update the transformation matrix; meshes are regenerated
//! only when primitive parameters, shading or material change.

use crate::selection::Selection;
use modeler_core::{MeshData, Material, ObjectId, Primitive, Scene, Transform};
use std::collections::{HashMap, HashSet};
use three_d::*;

// Blender's outline colors: light orange for the active object, darker
// orange for other selected objects; red warns about overlaps while placing.
const ACTIVE_OUTLINE: Srgba = Srgba::new(255, 170, 64, 255);
const SELECTED_OUTLINE: Srgba = Srgba::new(230, 110, 20, 255);
const OVERLAP_OUTLINE: Srgba = Srgba::new(235, 60, 50, 255);
const OUTLINE_SCALE: f32 = 1.03;

struct CachedModel {
    primitive: Primitive,
    smooth: bool,
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

fn physical_material(context: &Context, material: &Material) -> PhysicalMaterial {
    let [r, g, b] = material.base_color;
    PhysicalMaterial::new_opaque(
        context,
        &CpuMaterial {
            albedo: Srgba::new((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8, 255),
            roughness: material.roughness,
            metallic: material.metallic,
            ..Default::default()
        },
    )
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
    ) {
        self.order.clear();
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
                    cached.primitive != object.primitive || cached.smooth != object.smooth
                }
                None => true,
            };
            let rebuild_material = match self.cache.get(&object.id) {
                Some(cached) => cached.material != object.material,
                None => true,
            };

            if rebuild_mesh {
                let cpu_mesh = to_cpu_mesh(&object.primitive.generate(object.smooth));
                let model = Gm::new(
                    Mesh::new(context, &cpu_mesh),
                    physical_material(context, &object.material),
                );
                self.cache.insert(
                    object.id,
                    CachedModel {
                        primitive: object.primitive,
                        smooth: object.smooth,
                        material: object.material,
                        cpu_mesh,
                        model,
                        outline: None,
                    },
                );
            } else if rebuild_material {
                let cached = self.cache.get_mut(&object.id).unwrap();
                cached.material = object.material;
                cached.model.material = physical_material(context, &object.material);
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
