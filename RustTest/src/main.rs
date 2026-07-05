//! Box3D test application.
//!
//! Shows a rolling 3D landscape (box3d height field), a small house built from
//! static box hulls, and a ragdoll (the `CreateHuman` helper from the box3d
//! shared sample library). Rendering is done with macroquad.
//!
//! Controls:
//!   - Left mouse drag: orbit camera
//!   - Mouse wheel:     zoom
//!   - Space:           throw a ball from the camera
//!   - R:               respawn the ragdoll

mod ffi {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

use macroquad::models::Vertex;
use macroquad::prelude::*;

// ---------------------------------------------------------------------------
// Small conversions between box3d and macroquad math types
// ---------------------------------------------------------------------------

fn v3(v: ffi::b3Vec3) -> Vec3 {
    vec3(v.x, v.y, v.z)
}

fn bv3(v: Vec3) -> ffi::b3Vec3 {
    ffi::b3Vec3 { x: v.x, y: v.y, z: v.z }
}

fn bquat(q: ffi::b3Quat) -> Quat {
    Quat::from_xyzw(q.v.x, q.v.y, q.v.z, q.s)
}

// ---------------------------------------------------------------------------
// Terrain definition (shared by physics height field and render mesh)
// ---------------------------------------------------------------------------

const GRID_N: usize = 81; // grid points per side
const CELL: f32 = 1.0; // meters per cell
const HALF_EXTENT: f32 = (GRID_N - 1) as f32 * CELL * 0.5;

/// Rolling hills, flattened around the origin so the house sits on level ground.
fn terrain_height(x: f32, z: f32) -> f32 {
    let h = 1.7 * (0.09 * x).sin() * (0.075 * z).cos()
        + 0.8 * (0.16 * x + 1.3).sin() * (0.13 * z + 0.6).sin()
        + 0.22 * (0.45 * x).sin() * (0.38 * z).cos();

    // Smoothly blend to zero height within ~6 m of the house
    let d = (x * x + z * z).sqrt();
    let t = ((d - 6.0) / 8.0).clamp(0.0, 1.0);
    let s = t * t * (3.0 - 2.0 * t);
    h * s
}

fn terrain_normal(x: f32, z: f32) -> Vec3 {
    let e = 0.25;
    let hx = terrain_height(x + e, z) - terrain_height(x - e, z);
    let hz = terrain_height(x, z + e) - terrain_height(x, z - e);
    vec3(-hx / (2.0 * e), 1.0, -hz / (2.0 * e)).normalize()
}

// ---------------------------------------------------------------------------
// Simple CPU meshes with baked directional lighting
// ---------------------------------------------------------------------------

struct CpuMesh {
    pos: Vec<Vec3>,
    nrm: Vec<Vec3>,
    idx: Vec<u16>,
}

const LIGHT_DIR: Vec3 = vec3(0.45, 0.8, 0.35);

fn lit(color: Color, normal: Vec3) -> Color {
    let l = LIGHT_DIR.normalize();
    let f = (0.35 + 0.65 * normal.dot(l).max(0.0)).min(1.0);
    Color::new(color.r * f, color.g * f, color.b * f, 1.0)
}

/// Transform a CPU mesh and bake lighting into vertex colors.
fn build_lit_mesh(base: &CpuMesh, transform: Mat4, rotation: Quat, color: Color) -> Mesh {
    let vertices: Vec<Vertex> = base
        .pos
        .iter()
        .zip(base.nrm.iter())
        .map(|(p, n)| {
            let wp = transform.transform_point3(*p);
            let wn = (rotation * *n).normalize_or_zero();
            Vertex::new(wp.x, wp.y, wp.z, 0.0, 0.0, lit(color, wn))
        })
        .collect();

    Mesh { vertices, indices: base.idx.clone(), texture: None }
}

fn unit_cube() -> CpuMesh {
    // 6 faces, 4 vertices each, half extents 1 (scale with half-extent vector)
    let faces: [(Vec3, Vec3, Vec3); 6] = [
        (Vec3::X, Vec3::Y, Vec3::Z),
        (Vec3::NEG_X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::Z, Vec3::X),
        (Vec3::NEG_Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (Vec3::NEG_Z, Vec3::Y, Vec3::X),
    ];

    let mut mesh = CpuMesh { pos: Vec::new(), nrm: Vec::new(), idx: Vec::new() };
    for (n, u, v) in faces {
        let base = mesh.pos.len() as u16;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            mesh.pos.push(n + u * su + v * sv);
            mesh.nrm.push(n);
        }
        mesh.idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    mesh
}

fn unit_sphere(rings: usize, sectors: usize) -> CpuMesh {
    let mut mesh = CpuMesh { pos: Vec::new(), nrm: Vec::new(), idx: Vec::new() };
    for r in 0..=rings {
        let phi = std::f32::consts::PI * r as f32 / rings as f32;
        for s in 0..=sectors {
            let theta = 2.0 * std::f32::consts::PI * s as f32 / sectors as f32;
            let p = vec3(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin());
            mesh.pos.push(p);
            mesh.nrm.push(p);
        }
    }
    let stride = (sectors + 1) as u16;
    for r in 0..rings as u16 {
        for s in 0..sectors as u16 {
            let a = r * stride + s;
            let b = a + stride;
            mesh.idx.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    mesh
}

/// Open-ended cylinder, radius 1, extending from y = 0 to y = 1.
fn unit_cylinder(sectors: usize) -> CpuMesh {
    let mut mesh = CpuMesh { pos: Vec::new(), nrm: Vec::new(), idx: Vec::new() };
    for s in 0..=sectors {
        let theta = 2.0 * std::f32::consts::PI * s as f32 / sectors as f32;
        let n = vec3(theta.cos(), 0.0, theta.sin());
        mesh.pos.push(n);
        mesh.nrm.push(n);
        mesh.pos.push(n + Vec3::Y);
        mesh.nrm.push(n);
    }
    for s in 0..sectors as u16 {
        let a = 2 * s;
        mesh.idx.extend_from_slice(&[a, a + 1, a + 2, a + 2, a + 1, a + 3]);
    }
    mesh
}

// ---------------------------------------------------------------------------
// Scene description
// ---------------------------------------------------------------------------

/// A box that exists both as a physics hull and a rendered cuboid.
struct SceneBox {
    offset: Vec3, // relative to the owning body origin
    rotation: Quat,
    half_extents: Vec3,
    color: Color,
}

fn house_boxes() -> Vec<SceneBox> {
    let wall = Color::from_rgba(224, 205, 168, 255);
    let floor = Color::from_rgba(139, 105, 74, 255);
    let roof = Color::from_rgba(158, 62, 52, 255);
    let stone = Color::from_rgba(130, 130, 135, 255);

    let roof_angle = (1.35f32 / 2.7).atan();
    let roof_half_w = (2.7f32 * 2.7 + 1.35 * 1.35).sqrt() * 0.5 + 0.08;

    vec![
        // floor slab
        SceneBox { offset: vec3(0.0, 0.1, 0.0), rotation: Quat::IDENTITY, half_extents: vec3(2.6, 0.1, 2.1), color: floor },
        // back wall
        SceneBox { offset: vec3(0.0, 1.3, -2.0), rotation: Quat::IDENTITY, half_extents: vec3(2.5, 1.1, 0.1), color: wall },
        // front wall, split around the door opening (door: x in [-0.6, 0.6], y up to 1.8)
        SceneBox { offset: vec3(-1.55, 1.3, 2.0), rotation: Quat::IDENTITY, half_extents: vec3(0.95, 1.1, 0.1), color: wall },
        SceneBox { offset: vec3(1.55, 1.3, 2.0), rotation: Quat::IDENTITY, half_extents: vec3(0.95, 1.1, 0.1), color: wall },
        SceneBox { offset: vec3(0.0, 2.1, 2.0), rotation: Quat::IDENTITY, half_extents: vec3(0.6, 0.3, 0.1), color: wall },
        // side walls
        SceneBox { offset: vec3(-2.4, 1.3, 0.0), rotation: Quat::IDENTITY, half_extents: vec3(0.1, 1.1, 2.1), color: wall },
        SceneBox { offset: vec3(2.4, 1.3, 0.0), rotation: Quat::IDENTITY, half_extents: vec3(0.1, 1.1, 2.1), color: wall },
        // gable roof: two slabs meeting at a ridge along the z axis
        SceneBox {
            offset: vec3(1.35, 3.05, 0.0),
            rotation: Quat::from_rotation_z(-roof_angle),
            half_extents: vec3(roof_half_w, 0.08, 2.5),
            color: roof,
        },
        SceneBox {
            offset: vec3(-1.35, 3.05, 0.0),
            rotation: Quat::from_rotation_z(roof_angle),
            half_extents: vec3(roof_half_w, 0.08, 2.5),
            color: roof,
        },
        // chimney
        SceneBox { offset: vec3(1.5, 3.6, -1.0), rotation: Quat::IDENTITY, half_extents: vec3(0.25, 0.7, 0.25), color: stone },
    ]
}

/// Triangular gable walls closing the space between wall tops and the roof.
/// Triangle in the xy plane (CCW), extruded along z.
struct Gable {
    tri: [Vec2; 3],
    z_center: f32,
    half_thickness: f32,
}

fn house_gables() -> Vec<Gable> {
    let tri = [vec2(-2.5, 2.35), vec2(2.5, 2.35), vec2(0.0, 3.6)];
    vec![
        Gable { tri, z_center: 2.0, half_thickness: 0.1 },
        Gable { tri, z_center: -2.0, half_thickness: 0.1 },
    ]
}

/// Build a render mesh for an extruded triangle (prism).
fn gable_prism(g: &Gable) -> CpuMesh {
    let z0 = g.z_center - g.half_thickness;
    let z1 = g.z_center + g.half_thickness;
    let mut mesh = CpuMesh { pos: Vec::new(), nrm: Vec::new(), idx: Vec::new() };

    // front and back triangle faces
    for (z, n) in [(z1, Vec3::Z), (z0, Vec3::NEG_Z)] {
        let base = mesh.pos.len() as u16;
        for v in g.tri {
            mesh.pos.push(vec3(v.x, v.y, z));
            mesh.nrm.push(n);
        }
        mesh.idx.extend_from_slice(&[base, base + 1, base + 2]);
    }

    // side quads
    for i in 0..3 {
        let a = g.tri[i];
        let b = g.tri[(i + 1) % 3];
        let e = b - a;
        let n = vec3(e.y, -e.x, 0.0).normalize(); // outward for CCW winding
        let base = mesh.pos.len() as u16;
        for p in [vec3(a.x, a.y, z0), vec3(b.x, b.y, z0), vec3(b.x, b.y, z1), vec3(a.x, a.y, z1)] {
            mesh.pos.push(p);
            mesh.nrm.push(n);
        }
        mesh.idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    mesh
}

const TREE_SPOTS: [(f32, f32); 7] = [
    (-9.0, -6.0),
    (8.0, -9.0),
    (-12.0, 7.0),
    (10.0, 8.0),
    (-6.0, 13.0),
    (14.0, -2.0),
    (2.0, -14.0),
];

// ---------------------------------------------------------------------------
// Physics world
// ---------------------------------------------------------------------------

struct BoneVisual {
    body: ffi::b3BodyId,
    capsule: ffi::b3Capsule,
}

struct Ball {
    body: ffi::b3BodyId,
    radius: f32,
}

struct PhysicsScene {
    world: ffi::b3WorldId,
    height_field: *mut ffi::b3HeightFieldData,
    human: ffi::Human,
    bones: Vec<BoneVisual>,
    balls: Vec<Ball>,
    _heights: Vec<f32>, // kept alive alongside the height field
}

const RAGDOLL_SPAWN: Vec3 = vec3(2.2, 2.6, 4.5);

impl PhysicsScene {
    fn new() -> Self {
        unsafe {
            let world_def = ffi::b3DefaultWorldDef();
            let world = ffi::b3CreateWorld(&world_def);

            // --- terrain height field -----------------------------------
            let mut heights = vec![0.0f32; GRID_N * GRID_N];
            let mut min_h = f32::MAX;
            let mut max_h = f32::MIN;
            for iz in 0..GRID_N {
                for ix in 0..GRID_N {
                    let x = ix as f32 * CELL - HALF_EXTENT;
                    let z = iz as f32 * CELL - HALF_EXTENT;
                    let h = terrain_height(x, z);
                    heights[iz * GRID_N + ix] = h;
                    min_h = min_h.min(h);
                    max_h = max_h.max(h);
                }
            }

            let mut hf_def: ffi::b3HeightFieldDef = std::mem::zeroed();
            hf_def.heights = heights.as_mut_ptr();
            hf_def.materialIndices = std::ptr::null_mut();
            hf_def.scale = ffi::b3Vec3 { x: CELL, y: 1.0, z: CELL };
            hf_def.countX = GRID_N as i32;
            hf_def.countZ = GRID_N as i32;
            hf_def.globalMinimumHeight = min_h - 1.0;
            hf_def.globalMaximumHeight = max_h + 1.0;
            hf_def.clockwiseWinding = false;
            let height_field = ffi::b3CreateHeightField(&hf_def);

            let mut ground_def = ffi::b3DefaultBodyDef();
            ground_def.position = bv3(vec3(-HALF_EXTENT, 0.0, -HALF_EXTENT));
            let ground = ffi::b3CreateBody(world, &ground_def);
            let shape_def = ffi::b3DefaultShapeDef();
            ffi::b3CreateHeightFieldShape(ground, &shape_def, height_field);

            // --- house (static hulls on one body at the origin) ----------
            let house_def = ffi::b3DefaultBodyDef();
            let house = ffi::b3CreateBody(world, &house_def);
            for b in house_boxes() {
                let transform = ffi::b3Transform {
                    p: bv3(b.offset),
                    q: ffi::b3Quat {
                        v: ffi::b3Vec3 { x: b.rotation.x, y: b.rotation.y, z: b.rotation.z },
                        s: b.rotation.w,
                    },
                };
                let hull = ffi::b3MakeTransformedBoxHull(
                    b.half_extents.x,
                    b.half_extents.y,
                    b.half_extents.z,
                    transform,
                );
                ffi::b3CreateHullShape(house, &shape_def, &hull.base);
            }

            for g in house_gables() {
                let mut points = Vec::with_capacity(6);
                for z in [g.z_center - g.half_thickness, g.z_center + g.half_thickness] {
                    for v in g.tri {
                        points.push(ffi::b3Vec3 { x: v.x, y: v.y, z });
                    }
                }
                let hull = ffi::b3CreateHull(points.as_ptr(), points.len() as i32, points.len() as i32);
                assert!(!hull.is_null(), "gable hull creation failed");
                ffi::b3CreateHullShape(house, &shape_def, hull);
                ffi::b3DestroyHull(hull); // b3CreateHullShape copies the data
            }

            // --- trees (static capsule trunk + sphere foliage) -----------
            for (x, z) in TREE_SPOTS {
                let mut tree_def = ffi::b3DefaultBodyDef();
                tree_def.position = bv3(vec3(x, terrain_height(x, z) - 0.1, z));
                let tree = ffi::b3CreateBody(world, &tree_def);

                let trunk = ffi::b3Capsule {
                    center1: bv3(Vec3::ZERO),
                    center2: bv3(vec3(0.0, 2.3, 0.0)),
                    radius: 0.2,
                };
                ffi::b3CreateCapsuleShape(tree, &shape_def, &trunk);

                let foliage = ffi::b3Sphere { center: bv3(vec3(0.0, 3.0, 0.0)), radius: 1.3 };
                ffi::b3CreateSphereShape(tree, &shape_def, &foliage);
            }

            let mut scene = PhysicsScene {
                world,
                height_field,
                human: std::mem::zeroed(), // Human must be zero initialized
                bones: Vec::new(),
                balls: Vec::new(),
                _heights: heights,
            };
            scene.spawn_ragdoll();
            scene
        }
    }

    fn spawn_ragdoll(&mut self) {
        unsafe {
            if self.human.isSpawned {
                ffi::DestroyHuman(&mut self.human);
            }
            let friction_torque = 4.0;
            let hertz = 1.0;
            let damping_ratio = 0.7;
            ffi::CreateHuman(
                &mut self.human,
                self.world,
                bv3(RAGDOLL_SPAWN),
                friction_torque,
                hertz,
                damping_ratio,
                1,
                std::ptr::null_mut(),
                false,
            );

            // Cache each bone's capsule for rendering
            self.bones.clear();
            for bone in self.human.bones.iter() {
                let mut shapes: [ffi::b3ShapeId; 4] = std::mem::zeroed();
                let count = ffi::b3Body_GetShapes(bone.bodyId, shapes.as_mut_ptr(), 4);
                for shape in shapes.iter().take(count as usize) {
                    if ffi::b3Shape_GetType(*shape) == ffi::b3ShapeType_b3_capsuleShape {
                        self.bones.push(BoneVisual {
                            body: bone.bodyId,
                            capsule: ffi::b3Shape_GetCapsule(*shape),
                        });
                    }
                }
            }
        }
    }

    fn throw_ball(&mut self, from: Vec3, dir: Vec3) {
        unsafe {
            let radius = 0.3;
            let mut def = ffi::b3DefaultBodyDef();
            def.type_ = ffi::b3BodyType_b3_dynamicBody;
            def.position = bv3(from);
            def.linearVelocity = bv3(dir * 22.0);
            let body = ffi::b3CreateBody(self.world, &def);

            let mut shape_def = ffi::b3DefaultShapeDef();
            shape_def.density = 4.0;
            let sphere = ffi::b3Sphere { center: bv3(Vec3::ZERO), radius };
            ffi::b3CreateSphereShape(body, &shape_def, &sphere);

            self.balls.push(Ball { body, radius });
            if self.balls.len() > 40 {
                let old = self.balls.remove(0);
                ffi::b3DestroyBody(old.body);
            }
        }
    }

    fn step(&mut self, dt: f32) {
        unsafe {
            ffi::b3World_Step(self.world, dt, 4);
        }
    }

    fn body_transform(body: ffi::b3BodyId) -> (Vec3, Quat) {
        unsafe {
            let t = ffi::b3Body_GetTransform(body);
            (v3(t.p), bquat(t.q))
        }
    }
}

impl Drop for PhysicsScene {
    fn drop(&mut self) {
        unsafe {
            ffi::b3DestroyWorld(self.world);
            ffi::b3DestroyHeightField(self.height_field);
        }
    }
}

// ---------------------------------------------------------------------------
// Static render meshes
// ---------------------------------------------------------------------------

/// Terrain is split into chunks because macroquad clamps draw calls to
/// ~10000 vertices / 5000 indices per mesh.
fn build_terrain_meshes() -> Vec<Mesh> {
    const CHUNK: usize = 16; // cells per chunk side
    let cells = GRID_N - 1;
    let chunks = cells.div_ceil(CHUNK);
    let mut meshes = Vec::with_capacity(chunks * chunks);

    for cz in 0..chunks {
        for cx in 0..chunks {
            let x0 = cx * CHUNK;
            let z0 = cz * CHUNK;
            let nx = CHUNK.min(cells - x0) + 1; // grid points in this chunk
            let nz = CHUNK.min(cells - z0) + 1;

            let mut vertices = Vec::with_capacity(nx * nz);
            for iz in 0..nz {
                for ix in 0..nx {
                    let x = (x0 + ix) as f32 * CELL - HALF_EXTENT;
                    let z = (z0 + iz) as f32 * CELL - HALF_EXTENT;
                    let h = terrain_height(x, z);
                    let n = terrain_normal(x, z);

                    // grass color varies with height
                    let t = ((h + 2.0) / 5.0).clamp(0.0, 1.0);
                    let base = Color::new(0.22 + 0.25 * t, 0.46 + 0.20 * t, 0.20 + 0.10 * t, 1.0);
                    let c = lit(base, n);
                    vertices.push(Vertex::new(x, h, z, 0.0, 0.0, c));
                }
            }

            let mut indices = Vec::with_capacity((nx - 1) * (nz - 1) * 6);
            for iz in 0..nz - 1 {
                for ix in 0..nx - 1 {
                    let a = (iz * nx + ix) as u16;
                    let b = a + 1;
                    let c = a + nx as u16;
                    let d = c + 1;
                    indices.extend_from_slice(&[a, c, b, b, c, d]);
                }
            }

            meshes.push(Mesh { vertices, indices, texture: None });
        }
    }

    meshes
}

fn build_static_meshes(cube: &CpuMesh, sphere: &CpuMesh, cylinder: &CpuMesh) -> Vec<Mesh> {
    let mut meshes = build_terrain_meshes();

    // house
    for b in house_boxes() {
        let m = Mat4::from_scale_rotation_translation(b.half_extents, b.rotation, b.offset);
        meshes.push(build_lit_mesh(cube, m, b.rotation, b.color));
    }

    // gable walls
    let wall_color = Color::from_rgba(224, 205, 168, 255);
    for g in house_gables() {
        meshes.push(build_lit_mesh(&gable_prism(&g), Mat4::IDENTITY, Quat::IDENTITY, wall_color));
    }

    // trees
    let trunk_color = Color::from_rgba(101, 72, 50, 255);
    let leaf_color = Color::from_rgba(50, 120, 55, 255);
    for (x, z) in TREE_SPOTS {
        let base = vec3(x, terrain_height(x, z) - 0.1, z);
        let m_trunk = Mat4::from_translation(base) * Mat4::from_scale(vec3(0.2, 2.3, 0.2));
        meshes.push(build_lit_mesh(cylinder, m_trunk, Quat::IDENTITY, trunk_color));
        let m_leaf = Mat4::from_scale_rotation_translation(
            Vec3::splat(1.3),
            Quat::IDENTITY,
            base + vec3(0.0, 3.0, 0.0),
        );
        meshes.push(build_lit_mesh(sphere, m_leaf, Quat::IDENTITY, leaf_color));
    }

    meshes
}

// ---------------------------------------------------------------------------
// Dynamic rendering helpers
// ---------------------------------------------------------------------------

fn draw_capsule(p1: Vec3, p2: Vec3, radius: f32, color: Color, cylinder: &CpuMesh, sphere: &CpuMesh) {
    let d = p2 - p1;
    let len = d.length();
    if len > 1e-6 {
        let rot = Quat::from_rotation_arc(Vec3::Y, d / len);
        let m = Mat4::from_translation(p1)
            * Mat4::from_quat(rot)
            * Mat4::from_scale(vec3(radius, len, radius));
        draw_mesh(&build_lit_mesh(cylinder, m, rot, color));
    }
    for p in [p1, p2] {
        let m = Mat4::from_scale_rotation_translation(Vec3::splat(radius), Quat::IDENTITY, p);
        draw_mesh(&build_lit_mesh(sphere, m, Quat::IDENTITY, color));
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn conf() -> Conf {
    Conf {
        window_title: "Box3D Rust Test - landscape, house & ragdoll".to_string(),
        window_width: 1280,
        window_height: 720,
        sample_count: 4,
        ..Default::default()
    }
}

#[macroquad::main(conf)]
async fn main() {
    let cube = unit_cube();
    let sphere = unit_sphere(12, 18);
    let cylinder = unit_cylinder(18);

    let mut scene = PhysicsScene::new();
    let static_meshes = build_static_meshes(&cube, &sphere, &cylinder);

    // orbit camera
    let mut yaw: f32 = 0.9;
    let mut pitch: f32 = 0.35;
    let mut distance: f32 = 16.0;
    let target = vec3(0.0, 1.5, 0.0);
    let mut last_mouse = mouse_position();

    let mut accumulator = 0.0f32;
    const DT: f32 = 1.0 / 60.0;

    // Optional self-test: SCREENSHOT_AFTER_FRAMES=180 cargo run
    // saves screenshot.png after that many frames and exits.
    let screenshot_after: Option<u32> = std::env::var("SCREENSHOT_AFTER_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok());
    let mut frame_count: u32 = 0;

    loop {
        // --- input ----------------------------------------------------
        let mouse = mouse_position();
        if is_mouse_button_down(MouseButton::Left) {
            yaw += (mouse.0 - last_mouse.0) * 0.005;
            pitch = (pitch + (mouse.1 - last_mouse.1) * 0.005).clamp(0.05, 1.45);
        }
        last_mouse = mouse;
        distance = (distance - mouse_wheel().1 * 1.2).clamp(5.0, 45.0);

        let cam_pos = target
            + vec3(
                yaw.cos() * pitch.cos() * distance,
                pitch.sin() * distance,
                yaw.sin() * pitch.cos() * distance,
            );
        let cam_forward = (target - cam_pos).normalize();

        if is_key_pressed(KeyCode::R) {
            scene.spawn_ragdoll();
        }
        if is_key_pressed(KeyCode::Space) {
            scene.throw_ball(cam_pos + cam_forward * 1.5, cam_forward);
        }

        // Exercise the interactive code paths when running the screenshot self-test
        if screenshot_after.is_some() {
            if frame_count == 60 {
                scene.throw_ball(cam_pos + cam_forward * 1.5, cam_forward);
            }
            if frame_count == 90 {
                scene.spawn_ragdoll();
            }
        }

        // --- physics --------------------------------------------------
        accumulator = (accumulator + get_frame_time()).min(0.25);
        while accumulator >= DT {
            scene.step(DT);
            accumulator -= DT;
        }

        // --- render ---------------------------------------------------
        clear_background(Color::from_rgba(140, 185, 235, 255));

        set_camera(&Camera3D {
            position: cam_pos,
            target,
            up: Vec3::Y,
            ..Default::default()
        });

        for mesh in &static_meshes {
            draw_mesh(mesh);
        }

        // ragdoll
        let skin = Color::from_rgba(232, 176, 130, 255);
        let cloth = Color::from_rgba(70, 105, 180, 255);
        for (i, bone) in scene.bones.iter().enumerate() {
            let (pos, rot) = PhysicsScene::body_transform(bone.body);
            let p1 = pos + rot * v3(bone.capsule.center1);
            let p2 = pos + rot * v3(bone.capsule.center2);
            // head + arms in skin color, the rest in blue
            let color = if matches!(i, 4 | 5 | 10..=13) { skin } else { cloth };
            draw_capsule(p1, p2, bone.capsule.radius, color, &cylinder, &sphere);
        }

        // thrown balls
        let ball_color = Color::from_rgba(210, 65, 65, 255);
        for ball in &scene.balls {
            let (pos, _) = PhysicsScene::body_transform(ball.body);
            let m = Mat4::from_scale_rotation_translation(Vec3::splat(ball.radius), Quat::IDENTITY, pos);
            draw_mesh(&build_lit_mesh(&sphere, m, Quat::IDENTITY, ball_color));
        }

        // --- overlay ----------------------------------------------------
        set_default_camera();
        draw_text("Left drag: orbit | Wheel: zoom | Space: throw ball | R: respawn ragdoll", 12.0, 24.0, 22.0, WHITE);
        draw_text(&format!("FPS: {}", get_fps()), 12.0, 48.0, 22.0, WHITE);

        frame_count += 1;
        if let Some(n) = screenshot_after {
            if frame_count >= n {
                get_screen_data().export_png("screenshot.png");
                break;
            }
        }

        next_frame().await;
    }
}
