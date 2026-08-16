//! box3d physics mirror and simulation.
//!
//! Edit mode: every visible object owns one STATIC body + shape, kept in sync
//! with the scene INCREMENTALLY — transform-only edits move the existing body
//! (`b3Body_SetTransform`); only geometry changes (primitive params, mesh
//! edits, world scale) rebuild that object's body. All spatial queries
//! (picking, overlap warnings, drop to floor) run against this world.
//!
//! Simulate mode (play/pause/stop): the world is rebuilt with dynamic bodies
//! for objects marked dynamic, stepped at a fixed 60 Hz, and body transforms
//! are written back into the scene each frame. Stop restores the transform
//! snapshot taken at play. Large playbacks (many dynamic bodies) enable
//! box3d's internal worker threads; small scenes stay serial, where threading
//! measurably hurts (see Vibecoding/performance-plan.md).

use crate::selection::Selection;
use box3d_sys as ffi;
use modeler_core::glam::{Quat, Vec3};
use modeler_core::{ObjectId, Primitive, Scene, Transform};
use std::collections::{HashMap, HashSet};
use std::os::raw::c_void;

fn bvec(v: Vec3) -> ffi::b3Vec3 {
    ffi::b3Vec3 { x: v.x, y: v.y, z: v.z }
}

fn bquat(q: Quat) -> ffi::b3Quat {
    ffi::b3Quat { v: ffi::b3Vec3 { x: q.x, y: q.y, z: q.z }, s: q.w }
}

fn from_bvec(v: ffi::b3Vec3) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

/// Reposition structural edge capsules so each spans the current world segment
/// between its two cloth nodes (expressed in body A's local frame).
unsafe fn update_cloth_edge_capsules(
    bodies: &[ffi::b3BodyId],
    edges: &[ClothEdgeCollider],
) {
    for edge in edges {
        if edge.body_a >= bodies.len() || edge.body_b >= bodies.len() {
            continue;
        }
        let body_a = bodies[edge.body_a];
        let body_b = bodies[edge.body_b];
        if !ffi::b3Body_IsValid(body_a) || !ffi::b3Body_IsValid(body_b) {
            continue;
        }
        if !ffi::b3Shape_IsValid(edge.shape) {
            continue;
        }
        let p_a = from_bvec(ffi::b3Body_GetPosition(body_a));
        let p_b = from_bvec(ffi::b3Body_GetPosition(body_b));
        let span = p_b - p_a;
        let len = span.length();
        if len < 1e-4 {
            continue;
        }
        // Inset capsule ends so node spheres own the tips (avoids double mass
        // contact spikes). Leave at least a short segment for edge coverage.
        let inset = (edge.radius * 0.85).min(len * 0.4);
        let dir = span / len;
        let w0 = p_a + dir * inset;
        let w1 = p_b - dir * inset;
        let c1 = ffi::b3Body_GetLocalPoint(body_a, bvec(w0));
        let c2 = ffi::b3Body_GetLocalPoint(body_a, bvec(w1));
        let capsule = ffi::b3Capsule {
            center1: c1,
            center2: c2,
            radius: edge.radius,
        };
        ffi::b3Shape_SetCapsule(edge.shape, &capsule);
    }
}

const FIXED_DT: f32 = 1.0 / 60.0;
const SUBSTEPS: i32 = 4;
const GRAVITY: Vec3 = Vec3::new(0.0, 0.0, -9.81); // Z-up world

/// Hard cap on fixed steps per rendered frame. Without it the accumulator
/// catch-up compounds: on a 7.6k-brick scene one 77 ms step makes the next
/// frame ask for 4 steps, then 9, then 14 — 0.9 fps while the sim still only
/// advances at 0.23x real time. Dropping the surplus keeps the viewport at the
/// cost of running in visible slow motion (reported by `slow_motion()`).
const MAX_STEPS_PER_FRAME: u32 = 2;

/// Above this many dynamic bodies, drop from `SUBSTEPS` to `SUBSTEPS_HEAVY`.
/// Worth ~10% on a big rubble pile; below it, keep 4 for stack stability.
const HEAVY_BODY_THRESHOLD: usize = 1000;
const SUBSTEPS_HEAVY: i32 = 2;

/// Linear speed below which a body may sleep, m/s. box3d's default (0.05) is
/// too tight for a settled brick pile: 7,300 of 7,591 bodies jitter just above
/// it forever, at 75 ms/step. At 0.2 the same pile sleeps ~5 s in, at 7 us/step.
/// 0.2 m/s is 3.3 mm per 60 Hz frame.
const SLEEP_THRESHOLD: f32 = 0.2;

/// Above this many dynamic bodies, playback recreates the world with box3d's
/// internal scheduler enabled (native only — wasm has no threads). Small
/// scenes stay serial: box3d's own benchmarks show threads HURTING on small /
/// broad-phase-heavy workloads (large_world.csv scales negatively).
const THREADED_BODY_THRESHOLD: usize = 500;

fn desired_worker_count(dynamic_bodies: usize) -> u32 {
    if cfg!(target_arch = "wasm32") || dynamic_bodies < THREADED_BODY_THRESHOLD {
        return 0; // wasm has no threads; small scenes are faster serial
    }
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .min(16)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimState {
    Stopped,
    Playing,
    Paused,
}

/// Everything that determines an object's collision GEOMETRY (not its
/// placement). While this is unchanged an edit reuses the existing body and
/// just moves it; when it changes the object's body is rebuilt.
#[derive(Clone, PartialEq, Debug)]
struct ShapeKey {
    primitive: Primitive,
    /// Bumped on mesh edits, cutout and floor-outline changes.
    mesh_revision: u64,
    edited: bool,
    cutouts: usize,
    floor_outline: usize,
    /// World scale — baked into the shape geometry.
    scale: Vec3,
    density: f32,
    /// Fingerprint of the enabled modifier stack (0 when empty): the
    /// collision shape follows boolean cuts, so it must rebuild when the
    /// stack, its parameters, or a tool object's placement changes.
    modifier_stamp: u64,
}

impl ShapeKey {
    fn of(scene: &Scene, object: &modeler_core::Object, world_scale: Vec3) -> Self {
        Self {
            primitive: object.primitive,
            mesh_revision: object.mesh_revision,
            edited: object.edited_mesh.is_some(),
            cutouts: object.cutouts.len(),
            floor_outline: object.floor_outline.len(),
            scale: world_scale,
            density: object.density,
            modifier_stamp: if object.modifiers.iter().any(|m| m.enabled) {
                crate::modifiers::stamp(scene, object.id)
            } else {
                0
            },
        }
    }
}

/// One scene object mirrored as a box3d body. Owns the body and any mesh
/// data its shapes reference (box3d does not copy mesh data — see the
/// RagdollOnMesh sample).
struct BodyEntry {
    body: ffi::b3BodyId,
    meshes: Vec<*mut ffi::b3MeshData>,
    key: ShapeKey,
    location: Vec3,
    rotation: Quat,
    /// Mirrored `Object::bounciness`: retuned in place on the live shapes
    /// (no rebuild) when the property changes.
    bounciness: f32,
}

/// Simulated rope: a chain of segment bodies + distance joints, not a
/// single mirrored body. Segment positions drive `Object::rope_nodes`.
struct RopeSim {
    object_id: ObjectId,
    /// First `node_count` entries are the dynamic segment nodes; any further
    /// bodies are static pin anchors created when the attach target has no
    /// physics body of its own.
    bodies: Vec<ffi::b3BodyId>,
    node_count: usize,
    joints: Vec<ffi::b3JointId>,
}

/// One structural edge with a capsule collider that tracks both nodes.
struct ClothEdgeCollider {
    shape: ffi::b3ShapeId,
    /// Owning body index (capsule is expressed in this body's local space).
    body_a: usize,
    body_b: usize,
    radius: f32,
}

/// Simulated cloth: a grid of segment bodies + structural/shear distance
/// joints. Node positions drive `Object::cloth_nodes` (row-major).
struct ClothSim {
    object_id: ObjectId,
    bodies: Vec<ffi::b3BodyId>,
    /// Grid nodes only (excludes any extra static pin bodies).
    node_count: usize,
    joints: Vec<ffi::b3JointId>,
    /// Capsules along structural edges — updated every step so the sheet
    /// cannot cut through solid corners between sparse particle contacts.
    edge_colliders: Vec<ClothEdgeCollider>,
}

/// Destroy a mirrored body and free the mesh data its shapes referenced.
unsafe fn destroy_entry(entry: &mut BodyEntry) {
    ffi::b3DestroyBody(entry.body);
    for mesh in entry.meshes.drain(..) {
        ffi::b3DestroyMesh(mesh);
    }
}

/// Retune the coefficient of restitution on a live body's shapes. Cheap
/// enough to do on any edit — no body or mesh is rebuilt.
unsafe fn set_entry_restitution(entry: &BodyEntry, bounciness: f32) {
    let count = ffi::b3Body_GetShapeCount(entry.body);
    if count <= 0 {
        return;
    }
    let mut shapes = vec![std::mem::zeroed::<ffi::b3ShapeId>(); count as usize];
    let got = ffi::b3Body_GetShapes(entry.body, shapes.as_mut_ptr(), count);
    for shape in &shapes[..got.max(0) as usize] {
        ffi::b3Shape_SetRestitution(*shape, bounciness.clamp(0.0, 1.0));
    }
}

unsafe fn destroy_rope(rope: &mut RopeSim) {
    for joint in rope.joints.drain(..) {
        if ffi::b3Joint_IsValid(joint) {
            ffi::b3DestroyJoint(joint, false);
        }
    }
    for body in rope.bodies.drain(..) {
        if ffi::b3Body_IsValid(body) {
            ffi::b3DestroyBody(body);
        }
    }
    rope.node_count = 0;
}

unsafe fn destroy_cloth(cloth: &mut ClothSim) {
    for joint in cloth.joints.drain(..) {
        if ffi::b3Joint_IsValid(joint) {
            ffi::b3DestroyJoint(joint, false);
        }
    }
    for body in cloth.bodies.drain(..) {
        if ffi::b3Body_IsValid(body) {
            ffi::b3DestroyBody(body);
        }
    }
    cloth.node_count = 0;
}

pub struct PhysicsMirror {
    world: ffi::b3WorldId,
    worker_count: u32,
    synced_version: Option<u64>,
    entries: HashMap<ObjectId, BodyEntry>,
    /// Active rope simulations (play mode only).
    ropes: HashMap<ObjectId, RopeSim>,
    /// Active cloth simulations (play mode only).
    cloths: HashMap<ObjectId, ClothSim>,
    /// Simulate-mode ground plane (never has an ObjectId).
    ground: Option<ffi::b3BodyId>,
    /// Dynamic bodies in parent-before-child order for the per-step
    /// write-back; built at play (the mapping is frozen while simulating).
    sim_order: Vec<ObjectId>,
    sim: SimState,
    pub ground_plane: bool,
    /// Transforms at play; restored on stop.
    snapshot: Vec<(ObjectId, Transform)>,
    /// Rope design lengths at play; restored on stop so sim never shortens
    /// a sagging cord to its current span.
    rope_length_snapshot: Vec<(ObjectId, f32)>,
    accumulator: f32,
    /// Solver substeps for this world; lowered on heavy scenes at play.
    substeps: i32,
    /// Fraction of real time the sim is actually advancing at (1.0 = real
    /// time). Below 1 when `MAX_STEPS_PER_FRAME` is dropping steps.
    slow_motion: f32,
}

impl PhysicsMirror {
    pub fn new() -> Self {
        unsafe {
            let mut def = ffi::b3DefaultWorldDef();
            def.workerCount = 0; // serial: required on wasm, right for queries
            def.gravity = bvec(GRAVITY);
            Self {
                world: ffi::b3CreateWorld(&def),
                worker_count: 0,
                synced_version: None,
                entries: HashMap::new(),
                ropes: HashMap::new(),
                cloths: HashMap::new(),
                ground: None,
                sim_order: Vec::new(),
                sim: SimState::Stopped,
                ground_plane: true,
                snapshot: Vec::new(),
                rope_length_snapshot: Vec::new(),
                accumulator: 0.0,
                substeps: SUBSTEPS,
                slow_motion: 1.0,
            }
        }
    }

    pub fn sim_state(&self) -> SimState {
        self.sim
    }

    /// 1.0 when the sim keeps up with the wall clock; lower when steps are
    /// being dropped to protect the frame rate. Only meaningful while playing.
    pub fn slow_motion(&self) -> f32 {
        self.slow_motion
    }

    pub fn is_stopped(&self) -> bool {
        self.sim == SimState::Stopped
    }

    // --- edit-mode sync ---------------------------------------------------

    /// Bring the static mirror up to date with the scene. Incremental:
    /// transform-only changes move existing bodies, geometry changes rebuild
    /// only the affected object, adds/removes create/destroy one body. No-op
    /// while simulating (the simulation owns the world then).
    pub fn sync(&mut self, scene: &Scene) {
        if self.sim != SimState::Stopped {
            return;
        }
        if self.synced_version == Some(scene.version()) {
            return;
        }
        self.synced_version = Some(scene.version());

        let worlds = scene.world_transforms();

        // drop bodies whose object is gone or hidden
        self.entries.retain(|id, entry| {
            let keep = scene.object(*id).is_some_and(|o| o.visible);
            if !keep {
                unsafe { destroy_entry(entry) };
            }
            keep
        });

        for object in scene.objects() {
            if !object.visible {
                continue; // hidden objects are not pickable / simulated
            }
            let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
            let key = ShapeKey::of(scene, object, world.scale);

            let moved_in_place = match self.entries.get_mut(&object.id) {
                Some(entry) if entry.key == key => {
                    if entry.location != world.location || entry.rotation != world.rotation {
                        unsafe {
                            ffi::b3Body_SetTransform(
                                entry.body,
                                bvec(world.location),
                                bquat(world.rotation),
                            );
                        }
                        entry.location = world.location;
                        entry.rotation = world.rotation;
                    }
                    // Surface property, not geometry: retune the existing
                    // shapes instead of rebuilding the body.
                    if entry.bounciness != object.bounciness {
                        unsafe { set_entry_restitution(entry, object.bounciness) };
                        entry.bounciness = object.bounciness;
                    }
                    true
                }
                _ => false,
            };
            if !moved_in_place {
                if let Some(mut old) = self.entries.remove(&object.id) {
                    unsafe { destroy_entry(&mut old) };
                }
                let entry = unsafe { self.create_entry(scene, object, &world, key, false) };
                self.entries.insert(object.id, entry);
            }
        }
    }

    fn destroy_all(&mut self) {
        unsafe {
            for (_, mut rope) in self.ropes.drain() {
                destroy_rope(&mut rope);
            }
            for (_, mut cloth) in self.cloths.drain() {
                destroy_cloth(&mut cloth);
            }
            for (_, mut entry) in self.entries.drain() {
                destroy_entry(&mut entry);
            }
            if let Some(ground) = self.ground.take() {
                ffi::b3DestroyBody(ground);
            }
        }
        self.sim_order.clear();
    }

    /// Tear down and recreate the box3d world itself (used to switch the
    /// internal scheduler on/off around large playbacks).
    fn recreate_world(&mut self, worker_count: u32) {
        self.destroy_all();
        unsafe {
            ffi::b3DestroyWorld(self.world);
            let mut def = ffi::b3DefaultWorldDef();
            def.workerCount = worker_count;
            def.gravity = bvec(GRAVITY);
            self.world = ffi::b3CreateWorld(&def);
        }
        self.worker_count = worker_count;
        self.synced_version = None;
    }

    /// Create the body + shapes for one object. `simulate` honors the
    /// per-object dynamic flag (play mode); the static mirror passes false.
    unsafe fn create_entry(
        &self,
        scene: &Scene,
        object: &modeler_core::Object,
        world: &Transform,
        key: ShapeKey,
        simulate: bool,
    ) -> BodyEntry {
        let mut body_def = ffi::b3DefaultBodyDef();
        body_def.position = bvec(world.location);
        body_def.rotation = bquat(world.rotation);
        if simulate && object.dynamic {
            body_def.type_ = ffi::b3BodyType_b3_dynamicBody;
            body_def.sleepThreshold = SLEEP_THRESHOLD;
        }
        let body = ffi::b3CreateBody(self.world, &body_def);

        let mut shape_def = ffi::b3DefaultShapeDef();
        shape_def.userData = object.id.0 as usize as *mut c_void;
        shape_def.density = object.density.max(0.001);
        shape_def.baseMaterial.restitution = object.bounciness.clamp(0.0, 1.0);

        // A modifier stack changes the geometry the user sees, so it must
        // change what they collide with too — a boolean hole is a real hole.
        let modified = object
            .modifiers
            .iter()
            .any(|m| m.enabled)
            .then(|| crate::modifiers::evaluate(scene, object.id))
            .filter(|mesh| !mesh.indices.is_empty());

        let mut meshes = Vec::new();
        Self::create_shape(
            self.sim,
            body,
            &shape_def,
            object,
            modified.as_ref(),
            world.scale,
            &mut meshes,
        );
        BodyEntry {
            body,
            meshes,
            key,
            location: world.location,
            rotation: world.rotation,
            bounciness: object.bounciness,
        }
    }

    /// Scale is baked into the shape geometry; position/rotation live on the
    /// body. Mesh data created here is returned via `meshes` — box3d
    /// references it, so it must outlive the body.
    unsafe fn create_shape(
        sim: SimState,
        body: ffi::b3BodyId,
        shape_def: &ffi::b3ShapeDef,
        object: &modeler_core::Object,
        // evaluated modifier stack, when the object has one: it replaces the
        // primitive geometry entirely
        modified: Option<&modeler_core::MeshData>,
        scale: Vec3, // WORLD scale (baked into geometry)
        meshes: &mut Vec<*mut ffi::b3MeshData>,
    ) {
        let uniform = (scale.x - scale.y).abs() < 1e-6 && (scale.x - scale.z).abs() < 1e-6;

        // Modified geometry (boolean cuts, subdivision) is what the user
        // sees and expects to collide with. Booleans routinely make it
        // concave — a plate with a hole is the point of the feature — so it
        // needs an exact triangle mesh. Mesh shapes cannot be dynamic in
        // box3d, so a dynamic body while playing falls back to a convex
        // hull (its holes fill in, but it is still the modified shape).
        if let Some(mesh) = modified {
            if !object.dynamic || sim == SimState::Stopped {
                Self::create_mesh_shape(body, shape_def, mesh, scale, meshes);
            } else {
                Self::create_hull_shape(body, shape_def, mesh, scale);
            }
            return;
        }

        // edited meshes lose their primitive identity: collide as a convex
        // hull of the deformed vertices
        if object.edited_mesh.is_some() {
            Self::create_hull_shape(body, shape_def, &object.collision_mesh(), scale);
            return;
        }

        match object.primitive {
            // exact sphere when uniformly scaled
            Primitive::UvSphere { radius, .. } | Primitive::IcoSphere { radius, .. } if uniform => {
                let sphere = ffi::b3Sphere {
                    center: bvec(Vec3::ZERO),
                    radius: (radius * scale.x.abs()).max(1e-4),
                };
                ffi::b3CreateSphereShape(body, shape_def, &sphere);
            }
            // a plane is flat: thin box hull
            Primitive::Plane { size } => {
                let hull = ffi::b3MakeBoxHull(
                    (0.5 * size * scale.x.abs()).max(1e-3),
                    (0.5 * size * scale.y.abs()).max(1e-3),
                    0.01,
                );
                ffi::b3CreateHullShape(body, shape_def, &hull.base);
            }
            // edit-mode rope: thin capsule along local +X for picking
            Primitive::Rope { length, radius, .. } => {
                let r = (radius * scale.y.abs().max(scale.z.abs())).max(1e-4);
                let capsule = ffi::b3Capsule {
                    center1: bvec(Vec3::ZERO),
                    center2: bvec(Vec3::new((length * scale.x.abs()).max(1e-3), 0.0, 0.0)),
                    radius: r,
                };
                ffi::b3CreateCapsuleShape(body, shape_def, &capsule);
            }
            // edit-mode cloth: thicker box so the zero-thickness sheet is pickable
            Primitive::Cloth { width, height, .. } => {
                let hull = ffi::b3MakeBoxHull(
                    (0.5 * width * scale.x.abs()).max(1e-3),
                    (0.5 * height * scale.y.abs()).max(1e-3),
                    0.06, // pick thickness (visual mesh stays flat)
                );
                ffi::b3CreateHullShape(body, shape_def, &hull.base);
            }
            // torus is not convex: exact triangle mesh so the hole stays a hole.
            // NOTE: mesh shapes cannot be dynamic in box3d; dynamic tori fall
            // back to a convex hull below.
            Primitive::Torus { .. } if !object.dynamic || sim == SimState::Stopped => {
                let mesh = object.primitive.generate(true); // shared-vertex topology
                Self::create_mesh_shape(body, shape_def, &mesh, scale, meshes);
            }
            // walls with door/window cutouts: exact triangle mesh so rays and
            // bodies pass through the openings (solid walls stay convex hulls)
            Primitive::Wall { .. }
                if !object.cutouts.is_empty()
                    && (!object.dynamic || sim == SimState::Stopped) =>
            {
                let mesh = object.collision_mesh();
                Self::create_mesh_shape(body, shape_def, &mesh, scale, meshes);
            }
            // terrain is concave by nature: exact triangle mesh of the
            // generated height grid so picking, drop-to-floor and rolling
            // bodies follow the actual surface. Height fields are static-only
            // in box3d (and Y-up, unlike this Z-up world), so a terrain made
            // dynamic falls back to the convex hull below while playing.
            Primitive::Terrain { .. } if !object.dynamic || sim == SimState::Stopped => {
                let mesh = object.collision_mesh();
                Self::create_mesh_shape(body, shape_def, &mesh, scale, meshes);
            }
            // dynamic terrain while playing: hull of the object's own stack
            // mesh (the catch-all below would use the default stack)
            Primitive::Terrain { .. } => {
                Self::create_hull_shape(body, shape_def, &object.collision_mesh(), scale);
            }
            // floors shaped to walls may be concave (L/U rooms): exact
            // triangle mesh so the notches stay open
            Primitive::Floor { .. }
                if !object.floor_outline.is_empty()
                    && (!object.dynamic || sim == SimState::Stopped) =>
            {
                let mesh = object.collision_mesh();
                Self::create_mesh_shape(body, shape_def, &mesh, scale, meshes);
            }
            // everything else is convex: simplified hull of the scaled mesh
            _ => {
                Self::create_hull_shape(body, shape_def, &object.primitive.generate(true), scale);
            }
        }
    }

    /// Convex hull of a mesh's points, scaled into world size.
    unsafe fn create_hull_shape(
        body: ffi::b3BodyId,
        shape_def: &ffi::b3ShapeDef,
        mesh: &modeler_core::MeshData,
        scale: Vec3,
    ) {
        let points: Vec<ffi::b3Vec3> = mesh.positions.iter().map(|p| bvec(*p * scale)).collect();
        let hull = ffi::b3CreateHull(points.as_ptr(), points.len() as i32, 32);
        if !hull.is_null() {
            ffi::b3CreateHullShape(body, shape_def, hull);
            ffi::b3DestroyHull(hull); // b3CreateHullShape copies
        }
    }

    /// Exact (non-convex) triangle-mesh shape; box3d keeps a reference to the
    /// mesh data, so it is stored until the body is destroyed.
    unsafe fn create_mesh_shape(
        body: ffi::b3BodyId,
        shape_def: &ffi::b3ShapeDef,
        mesh: &modeler_core::MeshData,
        scale: Vec3,
        meshes: &mut Vec<*mut ffi::b3MeshData>,
    ) {
        let mut vertices: Vec<ffi::b3Vec3> =
            mesh.positions.iter().map(|p| bvec(*p * scale)).collect();
        let mut indices: Vec<i32> = mesh.indices.iter().map(|&i| i as i32).collect();

        let mut def: ffi::b3MeshDef = std::mem::zeroed();
        def.vertices = vertices.as_mut_ptr();
        def.indices = indices.as_mut_ptr();
        def.vertexCount = vertices.len() as i32;
        def.triangleCount = (indices.len() / 3) as i32;
        let mesh_data = ffi::b3CreateMesh(&def, std::ptr::null_mut(), 0);
        if !mesh_data.is_null() {
            ffi::b3CreateMeshShape(body, shape_def, mesh_data, bvec(Vec3::ONE));
            meshes.push(mesh_data); // shape references it; keep alive
        }
    }

    // --- simulation -------------------------------------------------------

    pub fn play(&mut self, scene: &Scene) {
        match self.sim {
            SimState::Playing => {}
            SimState::Paused => self.sim = SimState::Playing,
            SimState::Stopped => {
                self.snapshot = scene
                    .objects()
                    .iter()
                    .map(|o| (o.id, o.transform))
                    .collect();
                // freeze design length for the whole sim session
                self.rope_length_snapshot = scene
                    .objects()
                    .iter()
                    .filter_map(|o| match o.primitive {
                        Primitive::Rope { length, .. } => Some((o.id, length)),
                        _ => None,
                    })
                    .collect();
                self.sim = SimState::Playing; // set before rebuild: torus hull fallback
                self.build_simulation(scene);
                self.accumulator = 0.0;
                self.slow_motion = 1.0;
            }
        }
    }

    /// Full build for play(): ground plane, per-object dynamic flags, and
    /// the depth-sorted dynamic write-back order. Enables worker threads for
    /// large body counts (see `THREADED_BODY_THRESHOLD`).
    fn build_simulation(&mut self, scene: &Scene) {
        let dynamic_bodies = scene
            .objects()
            .iter()
            .filter(|o| o.visible && (o.dynamic || o.primitive.is_soft_sim()))
            .map(|o| match o.primitive {
                Primitive::Rope { segments, .. } => segments.clamp(2, 64) as usize + 1,
                Primitive::Cloth {
                    segments_u,
                    segments_v,
                    ..
                } => {
                    let su = segments_u.clamp(1, 24) as usize + 1;
                    let sv = segments_v.clamp(1, 24) as usize + 1;
                    su * sv
                }
                _ => 1,
            })
            .sum::<usize>();
        // Heavy scenes trade solver substeps for step time (~10%); light ones
        // keep 4 substeps, which stacked geometry needs to stay stable.
        self.substeps = if dynamic_bodies > HEAVY_BODY_THRESHOLD {
            SUBSTEPS_HEAVY
        } else {
            SUBSTEPS
        };
        let want = desired_worker_count(dynamic_bodies);
        if want != self.worker_count {
            self.recreate_world(want);
        } else {
            self.destroy_all();
        }
        self.synced_version = None; // static mirror must rebuild after stop

        let worlds = scene.world_transforms();
        unsafe {
            if self.ground_plane {
                let mut body_def = ffi::b3DefaultBodyDef();
                body_def.position = bvec(Vec3::new(0.0, 0.0, -0.5));
                let ground = ffi::b3CreateBody(self.world, &body_def);
                let shape_def = ffi::b3DefaultShapeDef();
                let hull = ffi::b3MakeBoxHull(200.0, 200.0, 0.5); // top at z = 0
                ffi::b3CreateHullShape(ground, &shape_def, &hull.base);
                self.ground = Some(ground);
            }

            // pass 1: solid bodies (ropes need their attach targets to exist)
            for object in scene.objects() {
                if !object.visible {
                    continue;
                }
                // empties, lights and cameras are markers: pickable while
                // editing (static mirror), but never collide or simulate
                if object.primitive.is_gizmo() {
                    continue;
                }
                // ropes / cloth get their own multi-body chains below
                if object.primitive.is_soft_sim() {
                    continue;
                }
                let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
                let key = ShapeKey::of(scene, object, world.scale);
                let entry = self.create_entry(scene, object, &world, key, true);
                if ffi::b3Body_GetType(entry.body) == ffi::b3BodyType_b3_dynamicBody {
                    // one-shot world-space impulse at play (N·s); zero is a no-op
                    if object.initial_force.length_squared() > 1e-12 {
                        ffi::b3Body_ApplyLinearImpulseToCenter(
                            entry.body,
                            bvec(object.initial_force),
                            true,
                        );
                    }
                    self.sim_order.push(object.id);
                }
                self.entries.insert(object.id, entry);
            }
            // pass 2: ropes (segment chains + pins to attach targets)
            for object in scene.objects() {
                if !object.visible || !object.primitive.is_rope() {
                    continue;
                }
                if let Some(rope) = self.build_rope(scene, object) {
                    self.ropes.insert(object.id, rope);
                }
            }
            // pass 3: cloth grids
            for object in scene.objects() {
                if !object.visible || !object.primitive.is_cloth() {
                    continue;
                }
                if let Some(cloth) = self.build_cloth(scene, object) {
                    self.cloths.insert(object.id, cloth);
                }
            }
        }
        // parents first so children's local conversions see updated parents
        self.sim_order.sort_by_key(|id| scene.depth(*id));
    }

}

/// Place `n_links + 1` nodes for a rope of design `length` between two pins.
/// When the rope is longer than the pin span, add a parabolic sag so the
/// polyline arc length is approximately `length` (slack for swinging).
/// When shorter or equal, place taut along the span (rope will pull).
fn rope_node_positions(start: Vec3, end: Vec3, length: f32, n_links: usize) -> Vec<Vec3> {
    let n_nodes = n_links + 1;
    let span = end - start;
    let span_len = span.length();
    let dir = if span_len > 1e-4 {
        span / span_len
    } else {
        Vec3::NEG_Z
    };

    // taut placement along the chord
    let mut pts: Vec<Vec3> = (0..n_nodes)
        .map(|i| {
            let t = i as f32 / n_links as f32;
            if span_len > 1e-4 {
                start + span * t
            } else {
                start + dir * (length * t)
            }
        })
        .collect();

    if length > span_len + 1e-3 && span_len > 1e-4 {
        // binary-search sag so polyline length ≈ design length
        let mut lo = 0.0f32;
        let mut hi = (length - span_len) * 2.0 + 0.5;
        for _ in 0..16 {
            let mid = 0.5 * (lo + hi);
            let mut poly = 0.0f32;
            let mut prev = start;
            for i in 0..n_nodes {
                let t = i as f32 / n_links as f32;
                let p = start + span * t - Vec3::Z * (4.0 * mid * t * (1.0 - t));
                if i > 0 {
                    poly += (p - prev).length();
                }
                prev = p;
            }
            if poly < length {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let sag = 0.5 * (lo + hi);
        for i in 0..n_nodes {
            let t = i as f32 / n_links as f32;
            pts[i] = start + span * t - Vec3::Z * (4.0 * sag * t * (1.0 - t));
        }
    }
    pts
}

impl PhysicsMirror {
    /// Build a multi-segment rope: light spheres at nodes, rigid distance
    /// joints sized to the **actual** initial segment lengths, and rigid
    /// pins to attached bodies. Joint rest length must match the spawn
    /// spacing — a fixed design `seg_len` while nodes sit on a shorter
    /// chord made the chain explode under gravity.
    unsafe fn build_rope(
        &self,
        scene: &Scene,
        object: &modeler_core::Object,
    ) -> Option<RopeSim> {
        let Primitive::Rope {
            length,
            radius,
            segments,
        } = object.primitive
        else {
            return None;
        };
        let length = length.max(0.05);
        let radius = radius.max(0.005);
        let n_links = segments.clamp(2, 64) as usize;
        let n_nodes = n_links + 1;

        let start_w = scene.rope_end_world(object.id, true);
        let end_w = scene.rope_end_world(object.id, false);
        let positions = rope_node_positions(start_w, end_w, length, n_links);

        let mut shape_def = ffi::b3DefaultShapeDef();
        shape_def.userData = object.id.0 as usize as *mut c_void;

        let mut bodies = Vec::with_capacity(n_nodes + 2);
        for pos in &positions {
            let mut body_def = ffi::b3DefaultBodyDef();
            body_def.type_ = ffi::b3BodyType_b3_dynamicBody;
            body_def.position = bvec(*pos);
            // Light damping so a hanging mass can still swing.
            body_def.linearDamping = 0.3;
            body_def.angularDamping = 0.8;
            body_def.enableSleep = false;
            let body = ffi::b3CreateBody(self.world, &body_def);
            let sphere = ffi::b3Sphere {
                center: bvec(Vec3::ZERO),
                radius,
            };
            let mut node_shape = shape_def;
            node_shape.baseMaterial.restitution = 0.0;
            node_shape.baseMaterial.friction = 0.4;
            // Sensors: no collision with the hanging mass (avoids explosions).
            // Density is high so node mass is not tiny vs. a hanging cube —
            // extreme mass ratios make long distance-joint chains stretch.
            node_shape.density = 40.0;
            node_shape.isSensor = true;
            ffi::b3CreateSphereShape(body, &node_shape, &sphere);
            bodies.push(body);
        }

        let mut joints = Vec::new();

        // Rigid distance joints at the **actual** spawn spacing (proven by
        // the distance_joint_holds_two_spheres test).
        for i in 0..n_links {
            let seg = (positions[i + 1] - positions[i]).length().max(1e-3);
            let mut joint_def = ffi::b3DefaultDistanceJointDef();
            joint_def.base.bodyIdA = bodies[i];
            joint_def.base.bodyIdB = bodies[i + 1];
            joint_def.base.localFrameA.p = bvec(Vec3::ZERO);
            joint_def.base.localFrameB.p = bvec(Vec3::ZERO);
            joint_def.base.collideConnected = false;
            joint_def.length = seg;
            joint_def.enableSpring = false;
            joint_def.enableLimit = false;
            joints.push(ffi::b3CreateDistanceJoint(self.world, &joint_def));
        }

        // Rigid pins to attach targets (length must be > 0 for the API).
        for (is_start, end, node_idx) in [
            (true, object.rope_start, 0usize),
            (false, object.rope_end, n_nodes - 1),
        ] {
            let Some(_target_id) = end.object else {
                continue;
            };
            let world_pt = scene.rope_end_world(object.id, is_start);
            let pin_body = if let Some(entry) = self.entries.get(&_target_id) {
                entry.body
            } else {
                let mut body_def = ffi::b3DefaultBodyDef();
                body_def.position = bvec(world_pt);
                let anchor = ffi::b3CreateBody(self.world, &body_def);
                bodies.push(anchor);
                anchor
            };

            let local_on_target = ffi::b3Body_GetLocalPoint(pin_body, bvec(world_pt));
            let mut joint_def = ffi::b3DefaultDistanceJointDef();
            joint_def.base.bodyIdA = pin_body;
            joint_def.base.bodyIdB = bodies[node_idx];
            joint_def.base.localFrameA.p = local_on_target;
            joint_def.base.localFrameB.p = bvec(Vec3::ZERO);
            joint_def.base.collideConnected = false;
            joint_def.length = 0.005; // API requires length > 0
            joint_def.enableSpring = false;
            joint_def.enableLimit = false;
            joints.push(ffi::b3CreateDistanceJoint(self.world, &joint_def));
        }

        Some(RopeSim {
            object_id: object.id,
            bodies,
            node_count: n_nodes,
            joints,
        })
    }

    /// Cloth: grid of light spheres with structural + shear distance joints,
    /// pins from `cloth_anchors` to attach targets.
    unsafe fn build_cloth(
        &self,
        scene: &Scene,
        object: &modeler_core::Object,
    ) -> Option<ClothSim> {
        let Primitive::Cloth {
            width,
            height,
            segments_u,
            segments_v,
            stiffness,
        } = object.primitive
        else {
            return None;
        };
        let width = width.max(0.05);
        let height = height.max(0.05);
        let su = segments_u.clamp(1, 24);
        let sv = segments_v.clamp(1, 24);
        // 0 = soft drape, 1 = near-rigid
        let stiff = stiffness.clamp(0.0, 1.0);
        let nu = (su + 1) as usize;
        let nv = (sv + 1) as usize;
        let n_nodes = nu * nv;
        let world = scene.world_transform(object.id);

        let mut positions = Vec::with_capacity(n_nodes);
        for v in 0..=sv {
            for u in 0..=su {
                let local =
                    modeler_core::mesh::cloth_vertex_local(width, height, su, sv, u, v);
                positions.push(world.transform_point(local));
            }
        }

        // Collision radius ~ half cell so neighboring spheres nearly touch at
        // rest. Tiny radii left large gaps so the visual mesh cut through
        // solid corners between sparse particle contacts.
        let cell = (width / su as f32)
            .min(height / sv as f32)
            .max(0.02);
        let radius = (0.42 * cell).clamp(0.02, 0.2);

        let mut shape_def = ffi::b3DefaultShapeDef();
        shape_def.userData = object.id.0 as usize as *mut c_void;
        // Negative group index: all shapes on this cloth never collide with
        // each other (spheres/capsules), but still hit the world.
        shape_def.filter.groupIndex = -(object.id.0 as i32).saturating_add(1).max(1);

        // Softer cloth = lighter nodes so gravity folds the sheet more
        let node_density = 1.5 + 4.0 * stiff;
        let lin_damp = 0.15 + 0.35 * stiff;

        let mut bodies = Vec::with_capacity(n_nodes + object.cloth_anchors.len());
        for pos in &positions {
            let mut body_def = ffi::b3DefaultBodyDef();
            body_def.type_ = ffi::b3BodyType_b3_dynamicBody;
            body_def.position = bvec(*pos);
            body_def.linearDamping = lin_damp;
            body_def.angularDamping = 0.5 + 0.5 * stiff;
            body_def.enableSleep = false;
            let body = ffi::b3CreateBody(self.world, &body_def);
            let sphere = ffi::b3Sphere {
                center: bvec(Vec3::ZERO),
                radius,
            };
            let mut node_shape = shape_def;
            node_shape.baseMaterial.restitution = 0.0;
            node_shape.baseMaterial.friction = 0.65;
            // Not sensors so cloth can rest on floors/props.
            node_shape.density = node_density;
            node_shape.isSensor = false;
            ffi::b3CreateSphereShape(body, &node_shape, &sphere);
            bodies.push(body);
        }

        // Structural edge capsules (owned by body A, retargeted each step).
        let mut edge_colliders = Vec::new();
        let mut edge_shape_def = shape_def;
        edge_shape_def.density = 0.0; // no mass contribution; spheres carry mass
        edge_shape_def.baseMaterial.friction = 0.65;
        edge_shape_def.baseMaterial.restitution = 0.0;
        let add_edge = |bodies: &[ffi::b3BodyId],
                        edges: &mut Vec<ClothEdgeCollider>,
                        a: usize,
                        b: usize| {
            let p0 = positions[a];
            let p1 = positions[b];
            let mid = 0.5 * (p0 + p1);
            // Temporary capsule in body A local space (updated before first step)
            let la = Vec3::ZERO;
            let lb = p1 - p0; // approximate if A is at p0 with identity rot
            let capsule = ffi::b3Capsule {
                center1: bvec(la),
                center2: bvec(lb),
                radius,
            };
            let _ = mid;
            let shape = ffi::b3CreateCapsuleShape(bodies[a], &edge_shape_def, &capsule);
            edges.push(ClothEdgeCollider {
                shape,
                body_a: a,
                body_b: b,
                radius,
            });
        };
        for v in 0..=sv {
            for u in 0..su {
                let a = (u + v * (su + 1)) as usize;
                let b = a + 1;
                add_edge(&bodies, &mut edge_colliders, a, b);
            }
        }
        for v in 0..sv {
            for u in 0..=su {
                let a = (u + v * (su + 1)) as usize;
                let b = a + (su as usize + 1);
                add_edge(&bodies, &mut edge_colliders, a, b);
            }
        }
        // Initial capsule placement in correct local frames
        update_cloth_edge_capsules(&bodies, &edge_colliders);

        let mut joints = Vec::new();
        let idx = |u: u32, v: u32| (u as usize) + (v as usize) * nu;

        // Map stiffness → spring hertz. At 1.0 use rigid distance joints.
        // Structural stays taut; shear/bend are softer so folds form.
        let rigid = stiff >= 0.98;
        let struct_hz = 2.0 + 28.0 * stiff;
        let shear_hz = 1.0 + 14.0 * stiff;
        let bend_hz = 0.5 + 8.0 * stiff;
        let damp = 0.35 + 0.35 * stiff;
        // Soft cloth: allow a little extra rest length so it can wrinkle
        let soft = 1.0 - stiff;
        let stretch_struct = 1.0;
        let stretch_shear = 1.0 + 0.06 * soft;
        let stretch_bend = 1.0 + 0.15 * soft;

        let link = |bodies: &[ffi::b3BodyId],
                        joints: &mut Vec<ffi::b3JointId>,
                        positions: &[Vec3],
                        a: usize,
                        b: usize,
                        stretch: f32,
                        hertz: f32| {
            let seg = (positions[b] - positions[a]).length().max(1e-3) * stretch;
            let mut joint_def = ffi::b3DefaultDistanceJointDef();
            joint_def.base.bodyIdA = bodies[a];
            joint_def.base.bodyIdB = bodies[b];
            joint_def.base.localFrameA.p = bvec(Vec3::ZERO);
            joint_def.base.localFrameB.p = bvec(Vec3::ZERO);
            joint_def.base.collideConnected = false;
            joint_def.length = seg;
            if rigid {
                joint_def.enableSpring = false;
                joint_def.enableLimit = false;
            } else {
                joint_def.enableSpring = true;
                joint_def.hertz = hertz.max(0.25);
                joint_def.dampingRatio = damp;
                // Limit stretch so edges don't open wide gaps that cut solids
                joint_def.enableLimit = true;
                joint_def.minLength = (seg * 0.9).max(1e-3);
                joint_def.maxLength = seg * (1.0 + 0.12 * soft + 0.03);
            }
            joints.push(ffi::b3CreateDistanceJoint(self.world, &joint_def));
        };

        // structural (edges)
        for v in 0..=sv {
            for u in 0..su {
                link(
                    &bodies,
                    &mut joints,
                    &positions,
                    idx(u, v),
                    idx(u + 1, v),
                    stretch_struct,
                    struct_hz,
                );
            }
        }
        for v in 0..sv {
            for u in 0..=su {
                link(
                    &bodies,
                    &mut joints,
                    &positions,
                    idx(u, v),
                    idx(u, v + 1),
                    stretch_struct,
                    struct_hz,
                );
            }
        }
        // shear (diagonals)
        for v in 0..sv {
            for u in 0..su {
                link(
                    &bodies,
                    &mut joints,
                    &positions,
                    idx(u, v),
                    idx(u + 1, v + 1),
                    stretch_shear,
                    shear_hz,
                );
                link(
                    &bodies,
                    &mut joints,
                    &positions,
                    idx(u + 1, v),
                    idx(u, v + 1),
                    stretch_shear,
                    shear_hz,
                );
            }
        }
        // bend (skip-one) — skip on very soft cloth for freer drape
        if stiff > 0.08 {
            for v in 0..=sv {
                for u in 0..su.saturating_sub(1) {
                    link(
                        &bodies,
                        &mut joints,
                        &positions,
                        idx(u, v),
                        idx(u + 2, v),
                        stretch_bend,
                        bend_hz,
                    );
                }
            }
            for v in 0..sv.saturating_sub(1) {
                for u in 0..=su {
                    link(
                        &bodies,
                        &mut joints,
                        &positions,
                        idx(u, v),
                        idx(u, v + 2),
                        stretch_bend,
                        bend_hz,
                    );
                }
            }
        }

        // pins from anchors
        for anchor in &object.cloth_anchors {
            let Some(target_id) = anchor.object else {
                continue;
            };
            if scene.object(target_id).is_none() {
                continue;
            }
            let u = anchor.u.min(su);
            let v = anchor.v.min(sv);
            let node_idx = idx(u, v);
            let world_pt = scene
                .world_transform(target_id)
                .transform_point(anchor.local_point);

            let pin_body = if let Some(entry) = self.entries.get(&target_id) {
                entry.body
            } else {
                let mut body_def = ffi::b3DefaultBodyDef();
                body_def.position = bvec(world_pt);
                let anchor_body = ffi::b3CreateBody(self.world, &body_def);
                bodies.push(anchor_body);
                anchor_body
            };

            let local_on_target = ffi::b3Body_GetLocalPoint(pin_body, bvec(world_pt));
            let mut joint_def = ffi::b3DefaultDistanceJointDef();
            joint_def.base.bodyIdA = pin_body;
            joint_def.base.bodyIdB = bodies[node_idx];
            joint_def.base.localFrameA.p = local_on_target;
            joint_def.base.localFrameB.p = bvec(Vec3::ZERO);
            joint_def.base.collideConnected = false;
            joint_def.length = 0.005;
            joint_def.enableSpring = false;
            joint_def.enableLimit = false;
            joints.push(ffi::b3CreateDistanceJoint(self.world, &joint_def));
        }

        Some(ClothSim {
            object_id: object.id,
            bodies,
            node_count: n_nodes,
            joints,
            edge_colliders,
        })
    }

    pub fn pause(&mut self) {
        if self.sim == SimState::Playing {
            self.sim = SimState::Paused;
        }
    }

    /// Stop and restore the transforms captured at play.
    pub fn stop(&mut self, scene: &mut Scene) {
        if self.sim == SimState::Stopped {
            return;
        }
        self.sim = SimState::Stopped;
        for (id, transform) in self.snapshot.drain(..) {
            if let Some(object) = scene.object_mut(id) {
                object.transform = transform;
            }
        }
        // Restore design length — never leave a rope shortened to the
        // post-sim attach span (sync used to rewrite length = |end-start|).
        for (id, length) in self.rope_length_snapshot.drain(..) {
            if let Some(object) = scene.object_mut(id) {
                if let Primitive::Rope {
                    length: l,
                    radius: _,
                    segments: _,
                } = &mut object.primitive
                {
                    *l = length;
                }
            }
        }
        // Drop live draped soft-body meshes and force a mesh rebuild. Without a
        // mesh_revision bump the render cache keeps the last sim frame's
        // shape parked at the restored transform.
        let soft_ids: Vec<ObjectId> = scene
            .objects()
            .iter()
            .filter(|o| o.primitive.is_soft_sim())
            .map(|o| o.id)
            .collect();
        for id in soft_ids {
            if let Some(object) = scene.object_mut(id) {
                let mut dirty = false;
                if object.rope_nodes.take().is_some() {
                    dirty = true;
                }
                if object.cloth_nodes.take().is_some() {
                    dirty = true;
                }
                if dirty {
                    object.mesh_revision = object.mesh_revision.wrapping_add(1);
                }
            }
        }
        // Re-seat attached ropes on their design-mode pins WITHOUT changing
        // length (a long rope between two close pins should stay long).
        crate::rope_handles::sync_attached_ropes(scene);

        // back to the serial query world; forces a static rebuild on sync
        if self.worker_count != 0 {
            self.recreate_world(0);
        } else {
            self.destroy_all();
            self.synced_version = None;
        }
    }

    /// Step the simulation and write body transforms back into the scene.
    pub fn update(&mut self, scene: &mut Scene, frame_dt: f32) {
        if self.sim != SimState::Playing {
            return;
        }
        self.accumulator = (self.accumulator + frame_dt).min(0.25);
        let wanted = (self.accumulator / FIXED_DT).floor() as u32;
        let steps = wanted.min(MAX_STEPS_PER_FRAME);
        for _ in 0..steps {
            // Keep structural edge capsules spanning current node pairs so
            // the cloth mesh cannot tunnel through object corners.
            unsafe {
                for cloth in self.cloths.values() {
                    update_cloth_edge_capsules(&cloth.bodies, &cloth.edge_colliders);
                }
            }
            unsafe { ffi::b3World_Step(self.world, FIXED_DT, self.substeps) };
            self.accumulator -= FIXED_DT;
        }
        // Drop the surplus instead of banking it: a scene too slow to keep up
        // stays in slow motion rather than spiralling into 14 steps per frame.
        if wanted > steps {
            self.accumulator = self.accumulator.min(FIXED_DT);
            // exponential smoothing so the footer reading does not flicker
            let ratio = steps as f32 / wanted as f32;
            self.slow_motion += 0.2 * (ratio - self.slow_motion);
        } else {
            self.slow_motion += 0.2 * (1.0 - self.slow_motion);
        }
        if steps == 0 {
            return;
        }
        // read every body first (scale comes from the scene), THEN write in
        // parent-before-child order so local conversions see updated parents
        let worlds = scene.world_transforms();
        let mut updates: Vec<(ObjectId, Transform)> =
            Vec::with_capacity(self.sim_order.len());
        unsafe {
            for id in &self.sim_order {
                let Some(entry) = self.entries.get(id) else { continue };
                let t = ffi::b3Body_GetTransform(entry.body);
                let mut world = worlds.get(id).copied().unwrap_or_default();
                world.location = Vec3::new(t.p.x, t.p.y, t.p.z);
                world.rotation = Quat::from_xyzw(t.q.v.x, t.q.v.y, t.q.v.z, t.q.s);
                updates.push((*id, world));
            }
        }
        for (id, world) in updates {
            scene.set_world_transform(id, world);
        }

        // ropes: write node positions and park the object origin on the
        // first node so the local tube mesh lines up
        let mut rope_updates: Vec<(ObjectId, Vec3, Vec<Vec3>)> = Vec::new();
        unsafe {
            for rope in self.ropes.values() {
                let mut nodes = Vec::with_capacity(rope.node_count);
                for body in rope.bodies.iter().take(rope.node_count) {
                    let p = ffi::b3Body_GetPosition(*body);
                    nodes.push(Vec3::new(p.x, p.y, p.z));
                }
                if nodes.len() < 2 {
                    continue;
                }
                let origin = nodes[0];
                rope_updates.push((rope.object_id, origin, nodes));
            }
        }
        for (id, origin, nodes) in rope_updates {
            // mesh is built as world deltas from the first node; park the
            // object at that origin with identity rotation so the deltas
            // land in the right place (stop restores the snapshot)
            scene.set_world_transform(
                id,
                Transform {
                    location: origin,
                    rotation: Quat::IDENTITY,
                    scale: Vec3::ONE,
                },
            );
            if let Some(object) = scene.object_mut(id) {
                object.rope_nodes = Some(nodes);
                // bump so the renderer rebuilds the draped mesh
                object.mesh_revision = object.mesh_revision.wrapping_add(1);
            }
        }

        // cloth: same origin-at-first-node trick for the draped grid
        let mut cloth_updates: Vec<(ObjectId, Vec3, Vec<Vec3>)> = Vec::new();
        unsafe {
            for cloth in self.cloths.values() {
                let mut nodes = Vec::with_capacity(cloth.node_count);
                for body in cloth.bodies.iter().take(cloth.node_count) {
                    let p = ffi::b3Body_GetPosition(*body);
                    nodes.push(Vec3::new(p.x, p.y, p.z));
                }
                if nodes.is_empty() {
                    continue;
                }
                let origin = nodes[0];
                cloth_updates.push((cloth.object_id, origin, nodes));
            }
        }
        for (id, origin, nodes) in cloth_updates {
            scene.set_world_transform(
                id,
                Transform {
                    location: origin,
                    rotation: Quat::IDENTITY,
                    scale: Vec3::ONE,
                },
            );
            if let Some(object) = scene.object_mut(id) {
                object.cloth_nodes = Some(nodes);
                object.mesh_revision = object.mesh_revision.wrapping_add(1);
            }
        }
    }

    // --- queries ------------------------------------------------------------

    /// Mouse picking: closest object hit by the ray, Blender-style.
    pub fn pick(&self, origin: Vec3, direction: Vec3) -> Option<ObjectId> {
        self.pick_surface(origin, direction, &[]).map(|(id, _)| id)
    }

    /// Closest ray hit on a scene object, with optional exclusions (e.g. the
    /// rope being dragged so its own capsule does not steal the cast).
    /// Returns `(object id, world hit point)`.
    pub fn pick_surface(
        &self,
        origin: Vec3,
        direction: Vec3,
        exclude: &[ObjectId],
    ) -> Option<(ObjectId, Vec3)> {
        struct Ctx {
            exclude: *const HashSet<u64>,
            best_frac: f32,
            hit_id: u64,
            point: ffi::b3Pos,
            found: bool,
        }
        unsafe extern "C" fn callback(
            shape: ffi::b3ShapeId,
            point: ffi::b3Pos,
            _normal: ffi::b3Vec3,
            fraction: f32,
            _material: u64,
            _triangle: i32,
            _child: i32,
            context: *mut c_void,
        ) -> f32 {
            let ctx = &mut *(context as *mut Ctx);
            let user_data = ffi::b3Shape_GetUserData(shape) as usize as u64;
            if user_data == 0 || (*ctx.exclude).contains(&user_data) {
                return -1.0; // ignore ground / excluded
            }
            if fraction < ctx.best_frac {
                ctx.best_frac = fraction;
                ctx.hit_id = user_data;
                ctx.point = point;
                ctx.found = true;
                return fraction; // clip to this hit
            }
            ctx.best_frac
        }

        let exclude_set: HashSet<u64> = exclude.iter().map(|id| id.0).collect();
        let mut ctx = Ctx {
            exclude: &exclude_set,
            best_frac: 1.0,
            hit_id: 0,
            point: ffi::b3Pos {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            found: false,
        };
        unsafe {
            ffi::b3World_CastRay(
                self.world,
                bvec(origin),
                bvec(direction * 10_000.0),
                ffi::b3DefaultQueryFilter(),
                Some(callback),
                &mut ctx as *mut Ctx as *mut c_void,
            );
        }
        if ctx.found {
            Some((
                ObjectId(ctx.hit_id),
                Vec3::new(ctx.point.x, ctx.point.y, ctx.point.z),
            ))
        } else {
            None
        }
    }

    /// Closest surface point among non-excluded bodies to a world probe
    /// (used as a magnetic snap when the ray barely misses an object).
    pub fn closest_surface_point(
        &self,
        probe: Vec3,
        exclude: &[ObjectId],
        max_dist: f32,
    ) -> Option<(ObjectId, Vec3)> {
        let exclude_set: HashSet<u64> = exclude.iter().map(|id| id.0).collect();
        let max_d2 = max_dist * max_dist;
        let mut best: Option<(f32, ObjectId, Vec3)> = None;
        unsafe {
            for (id, entry) in &self.entries {
                if exclude_set.contains(&id.0) {
                    continue;
                }
                let mut shapes: [ffi::b3ShapeId; 8] = std::mem::zeroed();
                let count = ffi::b3Body_GetShapes(entry.body, shapes.as_mut_ptr(), 8);
                for shape in shapes.iter().take(count as usize) {
                    let aabb = ffi::b3Shape_GetAABB(*shape);
                    let min = Vec3::new(aabb.lowerBound.x, aabb.lowerBound.y, aabb.lowerBound.z);
                    let max = Vec3::new(aabb.upperBound.x, aabb.upperBound.y, aabb.upperBound.z);
                    // Closest point on AABB ≈ surface for box-like shapes;
                    // good enough for magnetic snap assist.
                    let closest = probe.clamp(min, max);
                    // If probe is inside, push to the nearest face
                    let closest = if (closest - probe).length_squared() < 1e-12 {
                        let dx = (probe.x - min.x).min(max.x - probe.x);
                        let dy = (probe.y - min.y).min(max.y - probe.y);
                        let dz = (probe.z - min.z).min(max.z - probe.z);
                        if dx <= dy && dx <= dz {
                            Vec3::new(
                                if probe.x - min.x < max.x - probe.x {
                                    min.x
                                } else {
                                    max.x
                                },
                                probe.y,
                                probe.z,
                            )
                        } else if dy <= dz {
                            Vec3::new(
                                probe.x,
                                if probe.y - min.y < max.y - probe.y {
                                    min.y
                                } else {
                                    max.y
                                },
                                probe.z,
                            )
                        } else {
                            Vec3::new(
                                probe.x,
                                probe.y,
                                if probe.z - min.z < max.z - probe.z {
                                    min.z
                                } else {
                                    max.z
                                },
                            )
                        }
                    } else {
                        closest
                    };
                    let d2 = (closest - probe).length_squared();
                    if d2 <= max_d2 && best.is_none_or(|(bd, _, _)| d2 < bd) {
                        best = Some((d2, *id, closest));
                    }
                }
            }
        }
        best.map(|(_, id, p)| (id, p))
    }

    /// Physics-mode poke: cast the ray and kick the closest DYNAMIC body,
    /// changing its velocity at the hit point by `speed` m/s along the ray
    /// (mass-relative, so light and heavy objects react alike). Returns the
    /// kicked object.
    pub fn poke(&mut self, origin: Vec3, direction: Vec3, speed: f32) -> Option<ObjectId> {
        if self.sim != SimState::Playing {
            return None;
        }
        unsafe {
            let result = ffi::b3World_CastRayClosest(
                self.world,
                bvec(origin),
                bvec(direction * 10_000.0),
                ffi::b3DefaultQueryFilter(),
            );
            if !result.hit {
                return None;
            }
            let user_data = ffi::b3Shape_GetUserData(result.shapeId) as usize as u64;
            if user_data == 0 {
                return None; // ground plane
            }
            let body = ffi::b3Shape_GetBody(result.shapeId);
            if ffi::b3Body_GetType(body) != ffi::b3BodyType_b3_dynamicBody {
                return None;
            }
            let mass = ffi::b3Body_GetMass(body).max(1e-6);
            let dir = direction.normalize_or_zero();
            ffi::b3Body_ApplyLinearImpulse(body, bvec(dir * (mass * speed)), result.point, true);
            Some(ObjectId(user_data))
        }
    }

    /// AABB-based overlap test for the given objects (coarse warning while
    /// placing). Returns the subset that overlaps something else.
    pub fn overlapping(&self, ids: &[ObjectId]) -> HashSet<ObjectId> {
        struct Ctx {
            exclude: *const HashSet<u64>,
            hit: bool,
        }
        unsafe extern "C" fn callback(shape: ffi::b3ShapeId, context: *mut c_void) -> bool {
            let ctx = &mut *(context as *mut Ctx);
            let user_data = ffi::b3Shape_GetUserData(shape) as usize as u64;
            if user_data != 0 && !(*ctx.exclude).contains(&user_data) {
                ctx.hit = true;
                return false; // found one, stop the query
            }
            true
        }

        let mut result = HashSet::new();
        let exclude: HashSet<u64> = ids.iter().map(|id| id.0).collect();
        unsafe {
            for id in ids {
                let Some(entry) = self.entries.get(id) else { continue };
                let mut shapes: [ffi::b3ShapeId; 4] = std::mem::zeroed();
                let count = ffi::b3Body_GetShapes(entry.body, shapes.as_mut_ptr(), 4);
                for shape in shapes.iter().take(count as usize) {
                    let aabb = ffi::b3Shape_GetAABB(*shape);
                    let mut ctx = Ctx { exclude: &exclude, hit: false };
                    ffi::b3World_OverlapAABB(
                        self.world,
                        aabb,
                        ffi::b3DefaultQueryFilter(),
                        Some(callback),
                        &mut ctx as *mut Ctx as *mut c_void,
                    );
                    if ctx.hit {
                        result.insert(*id);
                        break;
                    }
                }
            }
        }
        result
    }

    /// Drop the selection straight down onto whatever is below it: the
    /// ground plane (z = 0) or the highest object underneath, whichever is
    /// higher (End key). Each selection root moves with its whole subtree
    /// as one piece; support is probed with a ray grid over the subtree's
    /// world-space footprint so partial overhangs still land on their
    /// support instead of falling through.
    pub fn drop_to_floor(&self, scene: &mut Scene, selection: &Selection) {
        struct Ctx {
            exclude: *const HashSet<u64>,
            best_z: Option<f32>,
        }
        unsafe extern "C" fn callback(
            shape: ffi::b3ShapeId,
            point: ffi::b3Pos,
            _normal: ffi::b3Vec3,
            fraction: f32,
            _material: u64,
            _triangle: i32,
            _child: i32,
            context: *mut c_void,
        ) -> f32 {
            let ctx = &mut *(context as *mut Ctx);
            let user_data = ffi::b3Shape_GetUserData(shape) as usize as u64;
            if (*ctx.exclude).contains(&user_data) {
                return -1.0; // ignore the moving objects, keep going
            }
            let z = point.z;
            ctx.best_z = Some(ctx.best_z.map_or(z, |b: f32| b.max(z)));
            fraction // clip: we only care about the closest hit below
        }

        let selected = selection.selected().to_vec();
        // selection roots: selected objects whose parent is not selected —
        // children follow their root through the hierarchy. Locked objects
        // stay put (End / Drop to floor).
        let roots: Vec<ObjectId> = selected
            .iter()
            .copied()
            .filter(|&id| {
                scene.object(id).is_some_and(|o| {
                    !o.locked && o.parent.map_or(true, |p| !selected.contains(&p))
                })
            })
            .collect();
        // the rays ignore every moving object, subtrees included
        let exclude: HashSet<u64> = roots
            .iter()
            .flat_map(|&root| scene.subtree(root))
            .map(|id| id.0)
            .collect();

        for root in roots {
            // Each member probes its own footprint; the assembly moves by
            // the most constraining member (its bottom meets its support,
            // everything else stays at or above theirs) — a table selected
            // with a high overhang stacks by the leg, not the overhang.
            let mut delta = f32::NEG_INFINITY;
            for member in scene.subtree(root) {
                let Some(object) = scene.object(member) else { continue };
                // member's world AABB from the actual collision mesh
                // (rotation- and scale-aware)
                let world = scene.world_transform(member);
                let mut min = Vec3::splat(f32::INFINITY);
                let mut max = Vec3::splat(f32::NEG_INFINITY);
                for p in object.collision_mesh().positions {
                    let w = world.transform_point(p);
                    min = min.min(w);
                    max = max.max(w);
                }
                if !min.z.is_finite() {
                    continue;
                }
                // ray grid over the footprint, cast from just above the
                // member's lowest point; best_z accumulates across rays
                const GRID: usize = 5;
                let mut ctx = Ctx { exclude: &exclude, best_z: None };
                for i in 0..GRID {
                    for j in 0..GRID {
                        let x = min.x + (max.x - min.x) * i as f32 / (GRID - 1) as f32;
                        let y = min.y + (max.y - min.y) * j as f32 / (GRID - 1) as f32;
                        unsafe {
                            ffi::b3World_CastRay(
                                self.world,
                                bvec(Vec3::new(x, y, min.z + 1e-3)),
                                bvec(Vec3::new(0.0, 0.0, -1000.0)),
                                ffi::b3DefaultQueryFilter(),
                                Some(callback),
                                &mut ctx as *mut Ctx as *mut c_void,
                            );
                        }
                    }
                }
                // this member's support: highest hit below it, or the ground
                let support = ctx.best_z.unwrap_or(0.0).max(0.0);
                delta = delta.max(support - min.z);
            }
            if delta.is_finite() {
                let mut world = scene.world_transform(root);
                world.location.z += delta;
                scene.set_world_transform(root, world);
            }
        }
    }

    /// Ray cast returning the world-space hit point (measure tool). Falls
    /// back to the z=0 grid plane when nothing is hit.
    pub fn pick_point(&self, origin: Vec3, direction: Vec3) -> Option<Vec3> {
        unsafe {
            let result = ffi::b3World_CastRayClosest(
                self.world,
                bvec(origin),
                bvec(direction * 10_000.0),
                ffi::b3DefaultQueryFilter(),
            );
            if result.hit {
                return Some(Vec3::new(result.point.x, result.point.y, result.point.z));
            }
        }
        // grid plane fallback
        if direction.z.abs() > 1e-6 {
            let t = -origin.z / direction.z;
            if t > 0.0 {
                return Some(origin + direction * t);
            }
        }
        None
    }

    /// Test hook: the underlying box3d body handle for an object — stable
    /// across transform-only syncs, replaced when geometry changes.
    #[cfg(test)]
    fn body_handle(&self, id: ObjectId) -> Option<(i32, u16)> {
        self.entries
            .get(&id)
            .map(|e| (e.body.index1, e.body.generation))
    }
}

impl Drop for PhysicsMirror {
    fn drop(&mut self) {
        unsafe {
            ffi::b3DestroyWorld(self.world); // takes the bodies with it
            for (_, entry) in self.entries.drain() {
                for mesh in entry.meshes {
                    ffi::b3DestroyMesh(mesh);
                }
            }
        }
    }
}

/// box3d keeps global state that is not safe to touch from multiple threads
/// at once (cargo test runs tests in parallel) — EVERY test that creates a
/// world (any `PhysicsMirror::new`), in any module, must hold this lock.
#[cfg(test)]
pub(crate) fn ffi_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static FFI_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    FFI_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::Primitive;

    fn ffi_lock() -> std::sync::MutexGuard<'static, ()> {
        ffi_test_lock()
    }

    fn scene_with_dynamic_cube_at(z: f32) -> (Scene, ObjectId) {
        let mut scene = Scene::new();
        let mut t = Transform::default();
        t.location.z = z;
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, t);
        scene.object_mut(id).unwrap().dynamic = true;
        (scene, id)
    }

    #[test]
    fn dynamic_cube_falls_and_rests_on_ground() {
        let _guard = ffi_lock();
        let (mut scene, id) = scene_with_dynamic_cube_at(3.0);
        let mut physics = PhysicsMirror::new();
        physics.play(&scene);
        assert_eq!(physics.sim_state(), SimState::Playing);

        for _ in 0..180 {
            physics.update(&mut scene, FIXED_DT);
        }
        // cube (half size 1) should rest on the ground plane at z = 0
        let z = scene.object(id).unwrap().transform.location.z;
        assert!((z - 1.0).abs() < 0.05, "cube should rest at z=1, got {z}");
    }

    /// Drop a cube and report the highest point it reaches after its first
    /// contact with the ground (0 if it never leaves the floor).
    fn rebound_height(bounciness: f32, floor_bounciness: f32) -> f32 {
        let (mut scene, id) = scene_with_dynamic_cube_at(4.0);
        scene.object_mut(id).unwrap().bounciness = bounciness;
        // static slab to land on, its own top at z = 0 like the ground plane
        let mut t = Transform::default();
        t.location = Vec3::new(0.0, 0.0, -1.0);
        t.scale = Vec3::new(10.0, 10.0, 1.0);
        let floor = scene.add_object(Primitive::Cube { size: 2.0 }, t);
        scene.object_mut(floor).unwrap().bounciness = floor_bounciness;

        let mut physics = PhysicsMirror::new();
        physics.play(&scene);
        let mut landed = false;
        let mut peak = 0.0_f32;
        for _ in 0..600 {
            physics.update(&mut scene, FIXED_DT);
            let z = scene.object(id).unwrap().transform.location.z;
            // resting height is 1 (half of a 2 m cube)
            if z < 1.1 {
                landed = true;
            } else if landed {
                peak = peak.max(z - 1.0);
            }
        }
        assert!(landed, "cube never reached the floor");
        peak
    }

    #[test]
    fn bouncy_object_rebounds_and_dead_one_does_not() {
        let _guard = ffi_lock();
        let dead = rebound_height(0.0, 0.0);
        let bouncy = rebound_height(0.85, 0.0);
        assert!(dead < 0.05, "a bounciness-0 cube should thud, rose {dead}");
        assert!(
            bouncy > 0.5,
            "a bounciness-0.85 cube should rebound, rose {bouncy}"
        );
    }

    #[test]
    fn bouncy_floor_bounces_a_dead_object() {
        // contacts take the higher of the two values, so bounciness on a
        // STATIC floor is enough — this is what the Physics panel promises
        let _guard = ffi_lock();
        let on_bouncy_floor = rebound_height(0.0, 0.85);
        assert!(
            on_bouncy_floor > 0.5,
            "a bouncy floor should throw a dead cube back up, rose {on_bouncy_floor}"
        );
    }

    #[test]
    fn bounciness_edit_retunes_the_static_mirror_without_rebuild() {
        let _guard = ffi_lock();
        let (mut scene, id) = scene_with_dynamic_cube_at(3.0);
        let mut physics = PhysicsMirror::new();
        physics.sync(&scene);
        let body = physics.entries[&id].body;
        let body_ident = (body.index1, body.world0, body.generation);
        scene.object_mut(id).unwrap().bounciness = 0.7;
        physics.sync(&scene);
        let entry = &physics.entries[&id];
        assert_eq!(entry.bounciness, 0.7);
        assert!(
            unsafe { ffi::b3Body_IsValid(body) }
                && (entry.body.index1, entry.body.world0, entry.body.generation) == body_ident,
            "changing bounciness must not rebuild the body"
        );
        let mut shape = [unsafe { std::mem::zeroed::<ffi::b3ShapeId>() }];
        let got = unsafe { ffi::b3Body_GetShapes(entry.body, shape.as_mut_ptr(), 1) };
        assert_eq!(got, 1);
        let r = unsafe { ffi::b3Shape_GetRestitution(shape[0]) };
        assert!((r - 0.7).abs() < 1e-6, "shape restitution not applied: {r}");
    }

    /// Plate at z = 2 with an optional boolean hole punched through it, and
    /// a ball dropped from z = 5 straight down the middle. Returns the ball's
    /// resting height.
    fn drop_ball_onto_plate(punch_hole: bool) -> f32 {
        let mut scene = Scene::new();
        // 6 x 6 x 0.4 m plate, static
        let mut t = Transform::default();
        t.location = Vec3::new(0.0, 0.0, 2.0);
        t.scale = Vec3::new(3.0, 3.0, 0.2);
        let plate = scene.add_object(Primitive::Cube { size: 2.0 }, t);

        if punch_hole {
            // 2 m cutter through the plate's middle
            let mut t = Transform::default();
            t.location = Vec3::new(0.0, 0.0, 2.0);
            let cutter = scene.add_object(Primitive::Cube { size: 2.0 }, t);
            crate::modifiers::add_boolean(
                &mut scene,
                plate,
                &[cutter],
                modeler_core::BooleanOp::Subtract,
            )
            .expect("boolean added");
        }

        let mut t = Transform::default();
        t.location = Vec3::new(0.0, 0.0, 5.0);
        t.scale = Vec3::splat(0.3); // 0.3 m radius — fits the 2 m hole
        let ball = scene.add_object(Primitive::UvSphere { segments: 16, rings: 8, radius: 1.0 }, t);
        scene.object_mut(ball).unwrap().dynamic = true;

        let mut physics = PhysicsMirror::new();
        physics.play(&scene);
        for _ in 0..420 {
            physics.update(&mut scene, FIXED_DT);
        }
        scene.object(ball).unwrap().transform.location.z
    }

    #[test]
    fn a_ball_falls_through_a_boolean_hole() {
        let _guard = ffi_lock();
        // solid plate: the ball lands on top (plate top 2.2 + radius 0.3)
        let on_plate = drop_ball_onto_plate(false);
        assert!(
            (on_plate - 2.5).abs() < 0.1,
            "ball should rest on the solid plate at z≈2.5, got {on_plate}"
        );
        // same plate with a hole cut through it: the ball drops to the ground
        let through = drop_ball_onto_plate(true);
        assert!(
            (through - 0.3).abs() < 0.1,
            "ball should fall through the hole to the ground at z≈0.3, got {through}"
        );
    }

    #[test]
    fn a_boolean_hole_is_not_pickable() {
        // the static mirror is also what viewport clicks ray-cast against
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let mut t = Transform::default();
        t.scale = Vec3::new(3.0, 3.0, 0.2);
        let plate = scene.add_object(Primitive::Cube { size: 2.0 }, t);
        let cutter = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        crate::modifiers::add_boolean(
            &mut scene,
            plate,
            &[cutter],
            modeler_core::BooleanOp::Subtract,
        )
        .expect("boolean added");

        let mut physics = PhysicsMirror::new();
        physics.sync(&scene);
        let down = Vec3::new(0.0, 0.0, -1.0);
        assert_eq!(
            physics.pick(Vec3::new(0.0, 0.0, 5.0), down),
            None,
            "a ray down the hole must miss the plate"
        );
        assert_eq!(
            physics.pick(Vec3::new(2.0, 2.0, 5.0), down),
            Some(plate),
            "a ray onto solid material must still hit the plate"
        );
    }

    #[test]
    fn initial_force_kicks_dynamic_body_on_play() {
        let _guard = ffi_lock();
        // float the cube so gravity has little time to matter
        let (mut scene, id) = scene_with_dynamic_cube_at(5.0);
        // impulse along +X: mass of a 2 m cube at density 1 is volume=8
        scene.object_mut(id).unwrap().initial_force = Vec3::new(40.0, 0.0, 0.0);
        let mut physics = PhysicsMirror::new();
        physics.play(&scene);
        // a few steps so the impulse integrates into displacement
        for _ in 0..30 {
            physics.update(&mut scene, FIXED_DT);
        }
        let x = scene.object(id).unwrap().transform.location.x;
        assert!(
            x > 0.3,
            "initial force along +X should move the cube, got x={x}"
        );
        physics.stop(&mut scene);
        let restored = scene.object(id).unwrap().transform.location;
        assert!(
            restored.x.abs() < 1e-4 && (restored.z - 5.0).abs() < 1e-4,
            "stop must restore the pre-play transform, got {restored:?}"
        );
    }

    #[test]
    fn hanging_cube_on_rope_sways_and_stays_off_ground() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        // static ceiling
        let mut t = Transform::default();
        t.location = Vec3::new(0.0, 0.0, 6.0);
        let ceiling = scene.add_object(Primitive::Cube { size: 2.0 }, t);
        scene.object_mut(ceiling).unwrap().dynamic = false;
        // small dynamic weight, offset in X so it can pendulum
        let mut t = Transform::default();
        t.location = Vec3::new(1.5, 0.0, 4.0);
        t.scale = Vec3::splat(0.3);
        let weight = scene.add_object(Primitive::Cube { size: 2.0 }, t);
        scene.object_mut(weight).unwrap().dynamic = true;
        let rope = scene.add_object(
            Primitive::Rope {
                length: 2.0,
                radius: 0.03,
                segments: 12,
            },
            Transform::default(),
        );
        {
            let o = scene.object_mut(rope).unwrap();
            o.rope_start = modeler_core::RopeEnd {
                object: Some(ceiling),
                local_point: Vec3::new(0.0, 0.0, -1.0),
            };
            o.rope_end = modeler_core::RopeEnd {
                object: Some(weight),
                local_point: Vec3::new(0.0, 0.0, 1.0),
            };
        }
        crate::rope_handles::snap_rope_rest_pose(&mut scene, rope);
        if let Some(o) = scene.object_mut(rope) {
            if let Primitive::Rope { length, .. } = &mut o.primitive {
                *length = 2.0;
            }
        }

        let x0 = scene.object(weight).unwrap().transform.location.x;
        let z0 = scene.object(weight).unwrap().transform.location.z;
        let mut physics = PhysicsMirror::new();
        physics.play(&scene);
        let mut min_z = z0;
        let mut max_x_travel = 0.0f32;
        for _ in 0..180 {
            physics.update(&mut scene, FIXED_DT);
            let p = scene.object(weight).unwrap().transform.location;
            min_z = min_z.min(p.z);
            max_x_travel = max_x_travel.max((p.x - x0).abs());
        }
        assert!(
            min_z > 1.0,
            "hanging weight should stay off the ground, min_z={min_z}"
        );
        assert!(
            max_x_travel > 0.3,
            "offset weight should sway under the rope, max_x_travel={max_x_travel}"
        );
        physics.stop(&mut scene);
        match scene.object(rope).unwrap().primitive {
            Primitive::Rope { length, .. } => {
                assert!((length - 2.0).abs() < 1e-3, "length must stay 2 m, got {length}")
            }
            _ => panic!("expected rope"),
        }
    }

    #[test]
    fn stop_restores_snapshot() {
        let _guard = ffi_lock();
        let (mut scene, id) = scene_with_dynamic_cube_at(3.0);
        let mut physics = PhysicsMirror::new();
        physics.play(&scene);
        for _ in 0..60 {
            physics.update(&mut scene, FIXED_DT);
        }
        assert!(scene.object(id).unwrap().transform.location.z < 2.9);

        physics.stop(&mut scene);
        assert_eq!(physics.sim_state(), SimState::Stopped);
        let z = scene.object(id).unwrap().transform.location.z;
        assert!((z - 3.0).abs() < 1e-5, "stop must restore z=3, got {z}");
    }

    #[test]
    fn static_objects_do_not_move() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let mut t = Transform::default();
        t.location.z = 3.0;
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, t); // static
        let mut physics = PhysicsMirror::new();
        physics.play(&scene);
        for _ in 0..60 {
            physics.update(&mut scene, FIXED_DT);
        }
        let z = scene.object(id).unwrap().transform.location.z;
        assert!((z - 3.0).abs() < 1e-5, "static object moved to {z}");
    }

    #[test]
    fn empties_never_collide_in_simulation() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        scene.add_object(Primitive::Empty { size: 1.0 }, Transform::default());
        let mut t = Transform::default();
        t.location.z = 3.0;
        let cube = scene.add_object(Primitive::Cube { size: 2.0 }, t);
        scene.object_mut(cube).unwrap().dynamic = true;

        let mut physics = PhysicsMirror::new();
        physics.ground_plane = false;
        physics.play(&scene);
        for _ in 0..90 {
            physics.update(&mut scene, FIXED_DT);
        }
        // the cube fell straight through the empty at the origin
        let z = scene.object(cube).unwrap().transform.location.z;
        assert!(z < -2.0, "cube must fall through the empty, got z={z}");
        physics.stop(&mut scene);

        // but empties stay pickable in the editing (static) mirror
        physics.sync(&scene);
        let hit = physics.pick(Vec3::new(0.0, -10.0, 0.0), Vec3::Y);
        assert_eq!(hit, scene.objects().first().map(|o| o.id));
    }

    #[test]
    fn poke_kicks_dynamic_bodies_only() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let mut t = Transform::default();
        t.location.z = 5.0;
        let cube = scene.add_object(Primitive::Cube { size: 2.0 }, t);
        scene.object_mut(cube).unwrap().dynamic = true;
        let mut wall_t = Transform::default();
        wall_t.location.x = 10.0;
        let _wall = scene.add_object(Primitive::Cube { size: 2.0 }, wall_t); // static
        let mut physics = PhysicsMirror::new();

        // no kick while stopped
        assert_eq!(physics.poke(Vec3::new(-10.0, 0.0, 5.0), Vec3::X, 10.0), None);

        physics.play(&scene);
        // kick the dynamic cube along +X, at its center height
        let hit = physics.poke(Vec3::new(-10.0, 0.0, 5.0), Vec3::X, 10.0);
        assert_eq!(hit, Some(cube));
        // static objects are never kicked
        assert_eq!(physics.poke(Vec3::new(10.0, -10.0, 1.0), Vec3::Y, 10.0), None);

        for _ in 0..12 {
            physics.update(&mut scene, FIXED_DT);
        }
        let x = scene.object(cube).unwrap().transform.location.x;
        assert!(x > 0.5, "kicked cube must fly along +X, got x={x}");
        physics.stop(&mut scene);
    }

    #[test]
    fn pause_freezes_and_resume_continues() {
        let _guard = ffi_lock();
        let (mut scene, id) = scene_with_dynamic_cube_at(5.0);
        let mut physics = PhysicsMirror::new();
        physics.play(&scene);
        for _ in 0..30 {
            physics.update(&mut scene, FIXED_DT);
        }
        physics.pause();
        let frozen = scene.object(id).unwrap().transform.location.z;
        for _ in 0..30 {
            physics.update(&mut scene, FIXED_DT);
        }
        assert_eq!(scene.object(id).unwrap().transform.location.z, frozen);

        physics.play(&scene); // resume, not restart
        for _ in 0..30 {
            physics.update(&mut scene, FIXED_DT);
        }
        assert!(scene.object(id).unwrap().transform.location.z < frozen);
    }

    #[test]
    fn drop_to_floor_lands_on_ground_and_stacks() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let mut t = Transform::default();
        t.location.z = 5.0;
        let cube = scene.add_object(Primitive::Cube { size: 2.0 }, t); // half height 1

        let mut physics = PhysicsMirror::new();
        physics.sync(&scene);

        let mut sel = crate::selection::Selection::default();
        sel.click(Some(cube), false);
        physics.drop_to_floor(&mut scene, &sel);
        let z = scene.object(cube).unwrap().transform.location.z;
        assert!((z - 1.0).abs() < 1e-3, "cube should land at z=1, got {z}");

        // sphere above the cube drops onto its top face (z=2 + radius 1)
        let mut t2 = Transform::default();
        t2.location.z = 10.0;
        let sphere = scene.add_object(
            Primitive::UvSphere { segments: 16, rings: 8, radius: 1.0 },
            t2,
        );
        physics.sync(&scene);
        sel.click(Some(sphere), false);
        physics.drop_to_floor(&mut scene, &sel);
        let z = scene.object(sphere).unwrap().transform.location.z;
        assert!((z - 3.0).abs() < 0.02, "sphere should rest at z=3, got {z}");
    }

    #[test]
    fn drop_to_floor_moves_assemblies_as_one_piece() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let at = |scene: &mut Scene, x: f32, y: f32, z: f32| {
            let mut t = Transform::default();
            t.location = Vec3::new(x, y, z);
            scene.add_object(Primitive::Cube { size: 2.0 }, t)
        };
        // a floating pair: root at z=5, child hanging at z=8 over a table
        let root = at(&mut scene, 0.0, 0.0, 5.0);
        let child = at(&mut scene, 0.0, 3.0, 8.0);
        scene.set_parent(child, Some(root));
        // static table under the CHILD's footprint only, top at z = 2
        at(&mut scene, 0.0, 3.0, 1.0);

        let mut physics = PhysicsMirror::new();
        physics.sync(&scene);
        let mut sel = crate::selection::Selection::default();
        sel.set(vec![root, child], Some(root));
        physics.drop_to_floor(&mut scene, &sel);

        // the root's ground contact constrains the drop: root rests at z=0
        // (center 1), the child keeps its 3 m offset and floats above the
        // table instead of sinking into it
        let root_z = scene.world_transform(root).location.z;
        let child_z = scene.world_transform(child).location.z;
        assert!((root_z - 1.0).abs() < 1e-3, "root center at z=1, got {root_z}");
        assert!((child_z - 4.0).abs() < 1e-3, "child center at z=4, got {child_z}");

        // drop again: already resting — nothing moves (idempotent)
        physics.sync(&scene);
        physics.drop_to_floor(&mut scene, &sel);
        let again = scene.world_transform(root).location.z;
        assert!((again - 1.0).abs() < 1e-3, "stable on repeat, got {again}");
    }

    #[test]
    fn pick_point_hits_objects_and_grid() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let mut t = Transform::default();
        t.location.z = 1.0; // cube top at z = 2
        scene.add_object(Primitive::Cube { size: 2.0 }, t);
        let mut physics = PhysicsMirror::new();
        physics.sync(&scene);

        // straight down onto the cube
        let hit = physics
            .pick_point(Vec3::new(0.0, 0.0, 10.0), Vec3::new(0.0, 0.0, -1.0))
            .expect("must hit the cube");
        assert!((hit.z - 2.0).abs() < 1e-3, "hit top at z=2, got {}", hit.z);

        // miss everything -> grid plane fallback at z = 0
        let hit = physics
            .pick_point(Vec3::new(50.0, 50.0, 10.0), Vec3::new(0.0, 0.0, -1.0))
            .expect("grid fallback");
        assert!(hit.z.abs() < 1e-4);
        assert!((hit.x - 50.0).abs() < 1e-4);
    }

    #[test]
    fn overlap_detects_intersecting_objects() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let a = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mut t = Transform::default();
        t.location.x = 0.5; // overlapping the first cube
        let b = scene.add_object(Primitive::Cube { size: 2.0 }, t);

        let mut physics = PhysicsMirror::new();
        physics.sync(&scene);
        let overlaps = physics.overlapping(&[b]);
        assert!(overlaps.contains(&b), "cubes at 0 and 0.5 must overlap");

        // move it far away: no overlap
        scene.object_mut(b).unwrap().transform.location.x = 10.0;
        physics.sync(&scene);
        let overlaps = physics.overlapping(&[b]);
        assert!(overlaps.is_empty(), "cubes 10 apart must not overlap");
        let _ = a;
    }

    // --- incremental-mirror guarantees ---------------------------------

    #[test]
    fn transform_edits_move_the_existing_body() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mut physics = PhysicsMirror::new();
        physics.sync(&scene);
        let before = physics.body_handle(id).expect("body exists");

        // move: body must be reused, not recreated
        scene.object_mut(id).unwrap().transform.location = Vec3::new(7.0, 0.0, 1.0);
        physics.sync(&scene);
        assert_eq!(physics.body_handle(id), Some(before), "move must reuse the body");
        let hit = physics.pick(Vec3::new(7.0, -10.0, 1.0), Vec3::Y);
        assert_eq!(hit, Some(id), "picking must see the new position");

        // scale: geometry is baked, body must be rebuilt
        scene.object_mut(id).unwrap().transform.scale = Vec3::splat(2.0);
        physics.sync(&scene);
        assert_ne!(physics.body_handle(id), Some(before), "scale must rebuild the body");

        // mesh revision bump (cutout/mesh edit path) also rebuilds
        let handle = physics.body_handle(id).unwrap();
        scene.object_mut(id).unwrap().mesh_revision += 1;
        physics.sync(&scene);
        assert_ne!(physics.body_handle(id), Some(handle), "mesh edits rebuild the body");
    }

    #[test]
    fn parent_moves_carry_children_in_the_mirror() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let parent = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mut t = Transform::default();
        t.location.x = 3.0;
        let child = scene.add_object(Primitive::Cube { size: 2.0 }, t);
        scene.set_parent(child, Some(parent));

        let mut physics = PhysicsMirror::new();
        physics.sync(&scene);
        let child_body = physics.body_handle(child).unwrap();

        // move the parent: the child's WORLD transform changes, its body is
        // reused but must be at the new place
        scene.object_mut(parent).unwrap().transform.location = Vec3::new(0.0, 0.0, 5.0);
        physics.sync(&scene);
        assert_eq!(physics.body_handle(child), Some(child_body));
        let hit = physics.pick(Vec3::new(3.0, -10.0, 5.0), Vec3::Y);
        assert_eq!(hit, Some(child), "child must be pickable at its new world position");
    }

    #[test]
    fn hidden_and_deleted_objects_leave_the_mirror() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mut physics = PhysicsMirror::new();
        physics.sync(&scene);
        assert!(physics.pick(Vec3::new(0.0, -10.0, 0.0), Vec3::Y).is_some());

        scene.object_mut(id).unwrap().visible = false;
        physics.sync(&scene);
        assert!(physics.pick(Vec3::new(0.0, -10.0, 0.0), Vec3::Y).is_none());
        assert!(physics.body_handle(id).is_none(), "hidden object has no body");

        scene.object_mut(id).unwrap().visible = true;
        physics.sync(&scene);
        assert_eq!(physics.pick(Vec3::new(0.0, -10.0, 0.0), Vec3::Y), Some(id));

        scene.remove_object(id);
        physics.sync(&scene);
        assert!(physics.pick(Vec3::new(0.0, -10.0, 0.0), Vec3::Y).is_none());
    }

    /// Deterministic xorshift so the equivalence script is reproducible.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn f(&mut self) -> f32 {
            (self.next() >> 40) as f32 / (1u64 << 24) as f32
        }
        fn range(&mut self, n: usize) -> usize {
            (self.next() >> 33) as usize % n.max(1)
        }
    }

    /// The incremental mirror must answer every query exactly like a mirror
    /// rebuilt from scratch against the same scene.
    fn assert_matches_fresh(scene: &Scene, incremental: &mut PhysicsMirror, step: usize) {
        incremental.sync(scene);
        let mut fresh = PhysicsMirror::new();
        fresh.sync(scene);

        // ray grid from above and from the side
        for i in 0..7 {
            for j in 0..7 {
                let x = -9.0 + 3.0 * i as f32;
                let y = -9.0 + 3.0 * j as f32;
                let down_o = Vec3::new(x, y, 60.0);
                let side_o = Vec3::new(x, -60.0, 0.5 + 0.7 * j as f32);
                for (origin, dir) in [(down_o, Vec3::NEG_Z), (side_o, Vec3::Y)] {
                    assert_eq!(
                        incremental.pick(origin, dir),
                        fresh.pick(origin, dir),
                        "step {step}: pick diverged at origin {origin:?}"
                    );
                    let a = incremental.pick_point(origin, dir);
                    let b = fresh.pick_point(origin, dir);
                    match (a, b) {
                        (Some(a), Some(b)) => assert!(
                            (a - b).length() < 1e-3,
                            "step {step}: hit points diverged: {a:?} vs {b:?}"
                        ),
                        (a, b) => assert_eq!(
                            a.is_some(),
                            b.is_some(),
                            "step {step}: hit/miss diverged at {origin:?}"
                        ),
                    }
                }
            }
        }
        // overlap parity per object
        for object in scene.objects() {
            assert_eq!(
                incremental.overlapping(&[object.id]),
                fresh.overlapping(&[object.id]),
                "step {step}: overlap diverged for {:?}",
                object.id
            );
        }
    }

    /// Phase 0 performance baseline (see Vibecoding/performance-plan.md).
    /// Ignored by default — run explicitly in release mode:
    ///
    ///   cargo test --release -p modeler-app -- --ignored --nocapture perf_baseline
    #[test]
    #[ignore = "perf baseline: run in --release with --nocapture"]
    fn perf_baseline() {
        let _guard = ffi_lock();
        use std::time::Instant;

        // --- house-scale scene: 200 objects, 50 dynamic -----------------
        let mut rng = Rng(42);
        let mut scene = Scene::new();
        for i in 0..200 {
            let mut t = Transform::default();
            t.location = Vec3::new(
                rng.f() * 40.0 - 20.0,
                rng.f() * 40.0 - 20.0,
                rng.f() * 4.0 + 1.0,
            );
            let id = match i % 4 {
                0 => scene.add_object(
                    Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 },
                    t,
                ),
                1 => scene.add_object(Primitive::Cube { size: 1.0 }, t),
                2 => scene.add_object(
                    Primitive::UvSphere { segments: 32, rings: 16, radius: 0.5 },
                    t,
                ),
                _ => scene.add_object(
                    Primitive::Cylinder { vertices: 32, radius: 0.4, depth: 1.0 },
                    t,
                ),
            };
            if i % 4 == 1 {
                scene.object_mut(id).unwrap().dynamic = true; // 50 dynamic
            }
        }

        let mut physics = PhysicsMirror::new();
        let t0 = Instant::now();
        physics.sync(&scene);
        let full = t0.elapsed();

        // a drag frame: one object moves, everything else is unchanged
        let ids: Vec<ObjectId> = scene.objects().iter().map(|o| o.id).collect();
        const DRAG_FRAMES: u32 = 200;
        let t0 = Instant::now();
        for f in 0..DRAG_FRAMES {
            let id = ids[f as usize % ids.len()];
            if let Some(o) = scene.object_mut(id) {
                o.transform.location.x += 0.01;
            }
            physics.sync(&scene);
        }
        let incremental = t0.elapsed() / DRAG_FRAMES;

        physics.play(&scene);
        const STEPS: u32 = 300;
        let t0 = Instant::now();
        for _ in 0..STEPS {
            physics.update(&mut scene, FIXED_DT);
        }
        let step = t0.elapsed() / STEPS;
        physics.stop(&mut scene);

        // undo checkpoint cost: deep clone + deep compare of the document
        let t0 = Instant::now();
        let snap = scene.snapshot();
        let clone_t = t0.elapsed();
        let t0 = Instant::now();
        let unchanged = snap == scene.snapshot();
        let compare_t = t0.elapsed();

        println!("house-scale (200 objects, 50 dynamic):");
        println!("  full mirror rebuild:            {full:>12.2?}");
        println!("  incremental sync (drag frame):  {incremental:>12.2?}  (was a full rebuild)");
        println!("  simulation step (60Hz, 4 sub):  {step:>12.2?}");
        println!("  undo snapshot: clone {clone_t:.2?}, compare {compare_t:.2?} (eq={unchanged})");

        // --- brick piles (break-into-bricks / poke workload) -------------
        // 400 stays serial; >=500 dynamic bodies enable worker threads.
        for count in [400usize, 600, 2000, 5000] {
            let mut scene = Scene::new();
            let per_layer = 10 * (count as f32 / 10.0).sqrt().ceil() as usize;
            let cols = per_layer / 10;
            let mut placed = 0;
            let mut z = 0.2f32;
            'outer: loop {
                for row in 0..10 {
                    for col in 0..cols.max(1) {
                        if placed >= count {
                            break 'outer;
                        }
                        let mut t = Transform::default();
                        t.location = Vec3::new(
                            col as f32 * 0.45 - cols as f32 * 0.22,
                            row as f32 * 0.45 - 2.25,
                            z,
                        );
                        let id = scene.add_object(Primitive::Cube { size: 0.4 }, t);
                        scene.object_mut(id).unwrap().dynamic = true;
                        placed += 1;
                    }
                }
                z += 0.45;
            }

            let mut physics = PhysicsMirror::new();
            let t0 = Instant::now();
            physics.play(&scene);
            let build = t0.elapsed();

            const BRICK_STEPS: u32 = 120;
            let t0 = Instant::now();
            for _ in 0..BRICK_STEPS {
                physics.update(&mut scene, FIXED_DT);
            }
            let step = t0.elapsed() / BRICK_STEPS;
            physics.stop(&mut scene);
            println!(
                "bricks {count:>5}: play-button build {build:>10.2?}, avg step {step:>10.2?} \
                 (workers: {})",
                desired_worker_count(count)
            );
        }
    }

    /// Per-frame cost breakdown for a REAL saved scene (default: the
    /// 7.6k-brick house). Ignored by default:
    ///
    ///   cargo test --release -p modeler-app -- --ignored --nocapture perf_scene_file
    ///
    /// Override the file with `BEE3D_PERF_SCENE=/path/to/scene.bee3d`.
    #[test]
    #[ignore = "perf probe on a saved scene: run in --release with --nocapture"]
    fn perf_scene_file() {
        let _guard = ffi_lock();
        use std::time::Instant;

        let path = std::env::var("BEE3D_PERF_SCENE")
            .unwrap_or_else(|_| "/home/bart/Documents/3dmodels/house-test8.bee3d".to_string());
        let Ok(json) = std::fs::read_to_string(&path) else {
            println!("skip: cannot read {path}");
            return;
        };
        let t0 = Instant::now();
        let data = Scene::from_json(&json).expect("scene parses");
        let parse = t0.elapsed();
        let mut scene = Scene::new();
        let t0 = Instant::now();
        scene.restore(&data);
        let restore = t0.elapsed();

        let total = scene.objects().len();
        let dynamic = scene.objects().iter().filter(|o| o.visible && o.dynamic).count();
        println!("scene {path}");
        println!("  objects {total}, dynamic {dynamic}");
        println!("  json parse {parse:.2?}, restore {restore:.2?}");

        // --- edit-mode mirror ------------------------------------------
        let mut physics = PhysicsMirror::new();
        let t0 = Instant::now();
        physics.sync(&scene);
        println!("  static mirror build            {:>12.2?}", t0.elapsed());

        // --- play ------------------------------------------------------
        let t0 = Instant::now();
        physics.play(&scene);
        println!(
            "  play() build_simulation        {:>12.2?}  (workers {})",
            t0.elapsed(),
            desired_worker_count(dynamic)
        );

        // --- steady-state frame, split into solver vs scene write-back --
        const STEPS: u32 = 60;
        let mut solver = std::time::Duration::ZERO;
        let mut wt = std::time::Duration::ZERO;
        let mut write = std::time::Duration::ZERO;
        for _ in 0..STEPS {
            let t0 = Instant::now();
            unsafe { ffi::b3World_Step(physics.world, FIXED_DT, SUBSTEPS) };
            solver += t0.elapsed();

            let t0 = Instant::now();
            let worlds = scene.world_transforms();
            wt += t0.elapsed();

            let t0 = Instant::now();
            let mut updates: Vec<(ObjectId, Transform)> =
                Vec::with_capacity(physics.sim_order.len());
            unsafe {
                for id in &physics.sim_order {
                    let Some(entry) = physics.entries.get(id) else { continue };
                    let t = ffi::b3Body_GetTransform(entry.body);
                    let mut world = worlds.get(id).copied().unwrap_or_default();
                    world.location = Vec3::new(t.p.x, t.p.y, t.p.z);
                    world.rotation = Quat::from_xyzw(t.q.v.x, t.q.v.y, t.q.v.z, t.q.s);
                    updates.push((*id, world));
                }
            }
            for (id, world) in updates {
                scene.set_world_transform(id, world);
            }
            write += t0.elapsed();
        }
        println!("  per frame, b3World_Step        {:>12.2?}", solver / STEPS);
        println!("  per frame, world_transforms()  {:>12.2?}", wt / STEPS);
        println!("  per frame, scene write-back    {:>12.2?}", write / STEPS);

        // --- renderer CPU proxy (what SceneRender::sync does per frame) --
        let ids: Vec<ObjectId> = scene.objects().iter().map(|o| o.id).collect();
        let t0 = Instant::now();
        let mut sink = 0.0f32;
        for &id in &ids {
            // instance_key() and instance_color() each resolve the material
            let a = scene.object_material_for_render(id).unwrap_or_default();
            let b = scene.object_material_for_render(id).unwrap_or_default();
            sink += a.roughness + b.metallic;
        }
        println!(
            "  per frame, 2x material resolve {:>12.2?}  (sink {sink:.1})",
            t0.elapsed()
        );

        // instance signature hashing: id + 16 matrix floats + color per member
        let worlds = scene.world_transforms();
        let t0 = Instant::now();
        {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            for &id in &ids {
                id.0.hash(&mut h);
                let w = worlds.get(&id).copied().unwrap_or_default();
                for f in [
                    w.location.x, w.location.y, w.location.z,
                    w.rotation.x, w.rotation.y, w.rotation.z, w.rotation.w,
                    w.scale.x, w.scale.y, w.scale.z,
                ] {
                    f.to_bits().hash(&mut h);
                }
            }
            println!(
                "  per frame, instance sig hash   {:>12.2?}  (h {})",
                t0.elapsed(),
                h.finish() % 10
            );
        }

        unsafe {
            let c = ffi::b3World_GetCounters(physics.world);
            println!(
                "  counters: bodies {} shapes {} contacts {} awake-contacts {} islands {} \
                 tree-h {} sat-calls {}",
                c.bodyCount, c.shapeCount, c.contactCount, c.awakeContactCount,
                c.islandCount, c.treeHeight, c.satCallCount
            );
            let p = ffi::b3World_GetProfile(physics.world);
            println!(
                "  profile ms: step {:.2} pairs {:.2} collide {:.2} solve {:.2} \
                 (setup {:.2} prepare {:.2} warmStart {:.2} solveImpulses {:.2} \
                 relax {:.2} restitution {:.2} store {:.2}) refit {:.2} bullets {:.2} \
                 sleep {:.2} transforms {:.2}",
                p.step, p.pairs, p.collide, p.solve, p.solverSetup, p.prepareConstraints,
                p.warmStart, p.solveImpulses, p.relaxImpulses, p.applyRestitution,
                p.storeImpulses, p.refit, p.bullets, p.sleepIslands, p.transforms
            );
        }

        let t0 = Instant::now();
        physics.stop(&mut scene);
        println!("  stop()                         {:>12.2?}", t0.elapsed());

        // --- settling trace: does the pile ever sleep? -------------------
        {
            println!("  --- 600-step trace (10 s of sim) ---");
            let mut p = PhysicsMirror::new();
            p.play(&scene);
            let mut awake_bodies = 0;
            for i in 1..=600u32 {
                let t0 = Instant::now();
                unsafe { ffi::b3World_Step(p.world, FIXED_DT, SUBSTEPS) };
                let dt = t0.elapsed();
                if i % 60 == 0 {
                    unsafe {
                        let c = ffi::b3World_GetCounters(p.world);
                        awake_bodies = p
                            .entries
                            .values()
                            .filter(|e| ffi::b3Body_IsAwake(e.body))
                            .count();
                        println!(
                            "    t={:>4.1}s step {:>8.2?} contacts {:>7} awake-contacts {:>7} \
                             awake-bodies {:>5} islands {:>4}",
                            i as f32 / 60.0,
                            dt,
                            c.contactCount,
                            c.awakeContactCount,
                            awake_bodies,
                            c.islandCount
                        );
                    }
                }
            }
            let _ = awake_bodies;
        }

        // --- only a subset dynamic (impact-local simulation) ------------
        println!("  --- N nearest bodies dynamic, rest static (60-step avg) ---");
        let center = Vec3::new(0.0, 0.0, 1.0);
        let mut by_dist: Vec<(ObjectId, f32)> = scene
            .objects()
            .iter()
            .map(|o| (o.id, (o.transform.location - center).length()))
            .collect();
        by_dist.sort_by(|a, b| a.1.total_cmp(&b.1));
        for n in [250usize, 1000, 2500] {
            let mut s = Scene::new();
            s.restore(&scene.snapshot());
            let keep: HashSet<ObjectId> =
                by_dist.iter().take(n).map(|(id, _)| *id).collect();
            let ids: Vec<ObjectId> = s.objects().iter().map(|o| o.id).collect();
            for id in ids {
                if let Some(o) = s.object_mut(id) {
                    o.dynamic = keep.contains(&id);
                }
            }
            let mut p = PhysicsMirror::new();
            p.play(&s);
            let mut solver = std::time::Duration::ZERO;
            for _ in 0..STEPS {
                let t0 = Instant::now();
                unsafe { ffi::b3World_Step(p.world, FIXED_DT, SUBSTEPS) };
                solver += t0.elapsed();
            }
            unsafe {
                let c = ffi::b3World_GetCounters(p.world);
                println!(
                    "    {n:>5} dynamic: {:>9.2?}/step, contacts {:>7}, awake-contacts {:>7}",
                    solver / STEPS,
                    c.contactCount,
                    c.awakeContactCount
                );
            }
        }

        // --- A/B: worker count x substeps ------------------------------
        println!("  --- solver-only A/B (60 steps each, fresh play) ---");
        for workers in [0u32, 4, 8, 16] {
            for substeps in [1i32, 2, 4] {
                let mut p = PhysicsMirror::new();
                p.recreate_world(workers); // real world uses `workers`
                // build_simulation only recreates when the count differs, so
                // report the count it wants and keep the world we just made
                p.worker_count = desired_worker_count(dynamic);
                p.play(&scene);
                let mut solver = std::time::Duration::ZERO;
                for _ in 0..STEPS {
                    let t0 = Instant::now();
                    unsafe { ffi::b3World_Step(p.world, FIXED_DT, substeps) };
                    solver += t0.elapsed();
                }
                println!(
                    "    workers {workers:>2}, substeps {substeps}: {:>10.2?}/step",
                    solver / STEPS
                );
            }
        }
    }

    /// Emulate the real frame loop: feed `update()` the wall-clock time the
    /// previous frame actually took, and report how many fixed steps it runs
    /// per frame (the accumulator catch-up), the frame time and the ratio of
    /// simulated time to real time.
    #[test]
    #[ignore = "perf probe: run in --release with --nocapture"]
    fn perf_frame_loop() {
        let _guard = ffi_lock();
        use std::time::Instant;
        let path = std::env::var("BEE3D_PERF_SCENE")
            .unwrap_or_else(|_| "/home/bart/Documents/3dmodels/house-test8.bee3d".to_string());
        let Ok(json) = std::fs::read_to_string(&path) else { return };
        let data = Scene::from_json(&json).expect("scene parses");
        let mut scene = Scene::new();
        scene.restore(&data);

        let mut physics = PhysicsMirror::new();
        physics.play(&scene);
        let mut frame_dt = 1.0 / 60.0f32; // first frame: assume vsync
        let mut sim_time = 0.0f32;
        let mut real_time = 0.0f32;
        for frame in 1..=25u32 {
            let before = physics.accumulator;
            let t0 = Instant::now();
            physics.update(&mut scene, frame_dt);
            let elapsed = t0.elapsed().as_secs_f32();
            let steps = (((before + frame_dt.min(0.25)).min(0.25) / FIXED_DT).floor() as u32)
                .min(MAX_STEPS_PER_FRAME);
            sim_time += steps as f32 * FIXED_DT;
            real_time += elapsed;
            println!(
                "  frame {frame:>3}: dt in {:>7.1} ms -> {steps:>2} steps, \
                 physics {:>7.1} ms  ({:>4.1} fps)",
                frame_dt * 1000.0,
                elapsed * 1000.0,
                1.0 / elapsed
            );
            frame_dt = elapsed; // next frame gets this frame's real duration
        }
        println!(
            "  => sim advanced {sim_time:.2} s in {real_time:.2} s of wall clock \
             ({:.2}x real time)",
            sim_time / real_time
        );
        physics.stop(&mut scene);
    }

    #[test]
    #[ignore = "perf probe: run in --release with --nocapture"]
    fn perf_sleep_probe() {
        let _guard = ffi_lock();
        use std::time::Instant;
        let path = std::env::var("BEE3D_PERF_SCENE")
            .unwrap_or_else(|_| "/home/bart/Documents/3dmodels/house-test8.bee3d".to_string());
        let Ok(json) = std::fs::read_to_string(&path) else { return };
        let data = Scene::from_json(&json).expect("scene parses");
        let mut scene = Scene::new();
        scene.restore(&data);

        // one config per process: box3d world slots and chaotic divergence
        // make back-to-back configs in one process unreliable
        let thresh: f32 = std::env::var("BEE3D_SLEEP_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        {
            let label = format!("sleepThreshold {thresh}");
            let mut p = PhysicsMirror::new();
            p.play(&scene);
            if thresh > 0.0 {
                unsafe {
                    for e in p.entries.values() {
                        ffi::b3Body_SetSleepThreshold(e.body, thresh);
                    }
                }
            }
            // optional: park every body that was not kicked at play, so only
            // the struck region simulates (contact wakes the neighbours)
            if std::env::var("BEE3D_START_ASLEEP").is_ok() {
                unsafe {
                    for (id, e) in p.entries.iter() {
                        let kicked = scene
                            .object(*id)
                            .is_some_and(|o| o.initial_force.length_squared() > 1e-12);
                        if !kicked {
                            ffi::b3Body_SetAwake(e.body, false);
                        }
                    }
                }
            }
            let steps: u32 = std::env::var("BEE3D_STEPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600);
            let quiet = std::env::var("BEE3D_QUIET").is_ok();
            let mut last = std::time::Duration::ZERO;
            let mut all = std::time::Duration::ZERO;
            for i in 0..steps {
                let t0 = Instant::now();
                unsafe { ffi::b3World_Step(p.world, FIXED_DT, SUBSTEPS) };
                all += t0.elapsed();
                if i + 60 >= steps {
                    last += t0.elapsed();
                }
                if !quiet && (i + 1) % 30 == 0 {
                    unsafe {
                        let awake = p
                            .entries
                            .values()
                            .filter(|e| ffi::b3Body_IsAwake(e.body))
                            .count();
                        println!(
                            "    t={:>4.2}s awake {awake:>5}  step {:>9.2?}",
                            (i + 1) as f32 / 60.0,
                            t0.elapsed()
                        );
                    }
                }
            }
            unsafe {
                let awake = p.entries.values().filter(|e| ffi::b3Body_IsAwake(e.body)).count();
                let dynamic_bodies = p
                    .entries
                    .values()
                    .filter(|e| ffi::b3Body_GetType(e.body) == ffi::b3BodyType_b3_dynamicBody)
                    .count();
                let c = ffi::b3World_GetCounters(p.world);
                println!(
                    "{label:>24}: all {:>9.2?}/step  last60 {:>9.2?}/step  \
                     awake {awake:>5}/{dynamic_bodies}  contacts {:>7} awake-contacts {:>7}",
                    all / steps,
                    last / 60,
                    c.contactCount,
                    c.awakeContactCount
                );
            }
        }
    }

    #[test]
    fn incremental_sync_matches_full_rebuild_under_random_edits() {
        let _guard = ffi_lock();
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut scene = Scene::new();
        let mut physics = PhysicsMirror::new();

        // seed a few objects
        for _ in 0..6 {
            let mut t = Transform::default();
            t.location = Vec3::new(rng.f() * 16.0 - 8.0, rng.f() * 16.0 - 8.0, rng.f() * 3.0);
            scene.add_object(Primitive::Cube { size: 1.0 + rng.f() }, t);
        }
        assert_matches_fresh(&scene, &mut physics, 0);

        for step in 1..=40 {
            let ids: Vec<ObjectId> = scene.objects().iter().map(|o| o.id).collect();
            match rng.range(8) {
                // add an object (varied primitive)
                0 => {
                    let mut t = Transform::default();
                    t.location =
                        Vec3::new(rng.f() * 16.0 - 8.0, rng.f() * 16.0 - 8.0, rng.f() * 3.0);
                    let primitive = match rng.range(4) {
                        0 => Primitive::Cube { size: 1.0 + rng.f() },
                        1 => Primitive::UvSphere {
                            segments: 12,
                            rings: 6,
                            radius: 0.5 + rng.f(),
                        },
                        2 => Primitive::Wall { length: 3.0, height: 2.0, thickness: 0.2 },
                        _ => Primitive::Empty { size: 1.0 },
                    };
                    scene.add_object(primitive, t);
                }
                // move
                1 | 2 => {
                    if !ids.is_empty() {
                        let id = ids[rng.range(ids.len())];
                        if let Some(o) = scene.object_mut(id) {
                            o.transform.location +=
                                Vec3::new(rng.f() * 4.0 - 2.0, rng.f() * 4.0 - 2.0, rng.f());
                        }
                    }
                }
                // rotate
                3 => {
                    if !ids.is_empty() {
                        let id = ids[rng.range(ids.len())];
                        if let Some(o) = scene.object_mut(id) {
                            o.transform.rotation = modeler_core::glam::Quat::from_rotation_z(
                                rng.f() * std::f32::consts::TAU,
                            );
                        }
                    }
                }
                // scale (geometry rebuild path)
                4 => {
                    if !ids.is_empty() {
                        let id = ids[rng.range(ids.len())];
                        if let Some(o) = scene.object_mut(id) {
                            o.transform.scale = Vec3::splat(0.5 + rng.f() * 2.0);
                        }
                    }
                }
                // toggle visibility
                5 => {
                    if !ids.is_empty() {
                        let id = ids[rng.range(ids.len())];
                        if let Some(o) = scene.object_mut(id) {
                            o.visible = !o.visible;
                        }
                    }
                }
                // reparent / unparent (world transforms of the subtree shift)
                6 => {
                    if ids.len() >= 2 {
                        let child = ids[rng.range(ids.len())];
                        let parent = ids[rng.range(ids.len())];
                        if rng.range(2) == 0 {
                            scene.set_parent(child, None);
                        } else {
                            scene.set_parent(child, Some(parent));
                        }
                    }
                }
                // delete
                _ => {
                    if ids.len() > 3 {
                        let id = ids[rng.range(ids.len())];
                        scene.remove_object(id);
                    }
                }
            }
            assert_matches_fresh(&scene, &mut physics, step);
        }
    }

    #[test]
    fn distance_joint_holds_two_spheres() {
        let _guard = ffi_lock();
        unsafe {
            let mut def = ffi::b3DefaultWorldDef();
            def.gravity = bvec(Vec3::new(0.0, 0.0, -9.81));
            let world = ffi::b3CreateWorld(&def);
            let mut bd = ffi::b3DefaultBodyDef();
            let ground = ffi::b3CreateBody(world, &bd);
            bd.type_ = ffi::b3BodyType_b3_dynamicBody;
            bd.position = bvec(Vec3::new(0.0, 0.0, 2.0));
            let a = ffi::b3CreateBody(world, &bd);
            bd.position = bvec(Vec3::new(0.0, 0.0, 1.0));
            let b = ffi::b3CreateBody(world, &bd);
            let mut sd = ffi::b3DefaultShapeDef();
            sd.density = 1.0;
            let sphere = ffi::b3Sphere {
                center: bvec(Vec3::ZERO),
                radius: 0.1,
            };
            ffi::b3CreateSphereShape(a, &sd, &sphere);
            ffi::b3CreateSphereShape(b, &sd, &sphere);
            let mut jd = ffi::b3DefaultDistanceJointDef();
            jd.base.bodyIdA = ground;
            jd.base.bodyIdB = a;
            jd.base.localFrameA.p = bvec(Vec3::new(0.0, 0.0, 2.0));
            jd.base.localFrameB.p = bvec(Vec3::ZERO);
            jd.length = 0.005;
            jd.enableSpring = false;
            let j1 = ffi::b3CreateDistanceJoint(world, &jd);
            let mut jd = ffi::b3DefaultDistanceJointDef();
            jd.base.bodyIdA = a;
            jd.base.bodyIdB = b;
            jd.base.localFrameA.p = bvec(Vec3::ZERO);
            jd.base.localFrameB.p = bvec(Vec3::ZERO);
            jd.length = 1.0;
            jd.enableSpring = false;
            let j2 = ffi::b3CreateDistanceJoint(world, &jd);
            assert!(ffi::b3Joint_IsValid(j1) && ffi::b3Joint_IsValid(j2));
            for step in 0..120 {
                ffi::b3World_Step(world, 1.0 / 60.0, 4);
                if step % 30 == 0 {
                    let pa = ffi::b3Body_GetPosition(a);
                    let pb = ffi::b3Body_GetPosition(b);
                    let d = ((pa.x - pb.x).powi(2)
                        + (pa.y - pb.y).powi(2)
                        + (pa.z - pb.z).powi(2))
                    .sqrt();
                    println!(
                        "step {step}: a.z={:.3} b.z={:.3} dist={:.3}",
                        pa.z, pb.z, d
                    );
                }
            }
            let pb = ffi::b3Body_GetPosition(b);
            assert!(
                pb.z > 0.5,
                "lower sphere should hang, not free-fall, z={}",
                pb.z
            );
            ffi::b3DestroyWorld(world);
        }
    }
}
