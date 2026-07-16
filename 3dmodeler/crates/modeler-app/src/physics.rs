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

const FIXED_DT: f32 = 1.0 / 60.0;
const SUBSTEPS: i32 = 4;
const GRAVITY: Vec3 = Vec3::new(0.0, 0.0, -9.81); // Z-up world

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
}

impl ShapeKey {
    fn of(object: &modeler_core::Object, world_scale: Vec3) -> Self {
        Self {
            primitive: object.primitive,
            mesh_revision: object.mesh_revision,
            edited: object.edited_mesh.is_some(),
            cutouts: object.cutouts.len(),
            floor_outline: object.floor_outline.len(),
            scale: world_scale,
            density: object.density,
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

/// Destroy a mirrored body and free the mesh data its shapes referenced.
unsafe fn destroy_entry(entry: &mut BodyEntry) {
    ffi::b3DestroyBody(entry.body);
    for mesh in entry.meshes.drain(..) {
        ffi::b3DestroyMesh(mesh);
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

pub struct PhysicsMirror {
    world: ffi::b3WorldId,
    worker_count: u32,
    synced_version: Option<u64>,
    entries: HashMap<ObjectId, BodyEntry>,
    /// Active rope simulations (play mode only).
    ropes: HashMap<ObjectId, RopeSim>,
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
                ground: None,
                sim_order: Vec::new(),
                sim: SimState::Stopped,
                ground_plane: true,
                snapshot: Vec::new(),
                rope_length_snapshot: Vec::new(),
                accumulator: 0.0,
            }
        }
    }

    pub fn sim_state(&self) -> SimState {
        self.sim
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
            let key = ShapeKey::of(object, world.scale);

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
                    true
                }
                _ => false,
            };
            if !moved_in_place {
                if let Some(mut old) = self.entries.remove(&object.id) {
                    unsafe { destroy_entry(&mut old) };
                }
                let entry = unsafe { self.create_entry(object, &world, key, false) };
                self.entries.insert(object.id, entry);
            }
        }
    }

    fn destroy_all(&mut self) {
        unsafe {
            for (_, mut rope) in self.ropes.drain() {
                destroy_rope(&mut rope);
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
        }
        let body = ffi::b3CreateBody(self.world, &body_def);

        let mut shape_def = ffi::b3DefaultShapeDef();
        shape_def.userData = object.id.0 as usize as *mut c_void;
        shape_def.density = object.density.max(0.001);

        let mut meshes = Vec::new();
        Self::create_shape(self.sim, body, &shape_def, object, world.scale, &mut meshes);
        BodyEntry {
            body,
            meshes,
            key,
            location: world.location,
            rotation: world.rotation,
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
        scale: Vec3, // WORLD scale (baked into geometry)
        meshes: &mut Vec<*mut ffi::b3MeshData>,
    ) {
        let uniform = (scale.x - scale.y).abs() < 1e-6 && (scale.x - scale.z).abs() < 1e-6;

        // edited meshes lose their primitive identity: collide as a convex
        // hull of the deformed vertices
        if object.edited_mesh.is_some() {
            let mesh = object.collision_mesh();
            let points: Vec<ffi::b3Vec3> =
                mesh.positions.iter().map(|p| bvec(*p * scale)).collect();
            let hull = ffi::b3CreateHull(points.as_ptr(), points.len() as i32, 32);
            if !hull.is_null() {
                ffi::b3CreateHullShape(body, shape_def, hull);
                ffi::b3DestroyHull(hull); // b3CreateHullShape copies
            }
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
                let mesh = object.primitive.generate(true);
                let points: Vec<ffi::b3Vec3> =
                    mesh.positions.iter().map(|p| bvec(*p * scale)).collect();
                let hull = ffi::b3CreateHull(points.as_ptr(), points.len() as i32, 32);
                if !hull.is_null() {
                    ffi::b3CreateHullShape(body, shape_def, hull);
                    ffi::b3DestroyHull(hull); // b3CreateHullShape copies
                }
            }
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
            .filter(|o| o.visible && (o.dynamic || o.primitive.is_rope()))
            .map(|o| match o.primitive {
                Primitive::Rope { segments, .. } => segments.clamp(2, 64) as usize + 1,
                _ => 1,
            })
            .sum::<usize>();
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
                // empties and lights are markers: pickable while editing
                // (static mirror), but never collide or simulate
                if matches!(
                    object.primitive,
                    Primitive::Empty { .. } | Primitive::Light { .. }
                ) {
                    continue;
                }
                // ropes get their own multi-body chain below
                if object.primitive.is_rope() {
                    continue;
                }
                let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
                let key = ShapeKey::of(object, world.scale);
                let entry = self.create_entry(object, &world, key, true);
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
        // Drop the live draped polyline and force a mesh rebuild. Without a
        // mesh_revision bump the render cache keeps the last sim frame's
        // rope shape parked at the restored transform — ends in the wrong place.
        let rope_ids: Vec<ObjectId> = scene
            .objects()
            .iter()
            .filter(|o| o.primitive.is_rope())
            .map(|o| o.id)
            .collect();
        for id in rope_ids {
            if let Some(object) = scene.object_mut(id) {
                if object.rope_nodes.take().is_some() {
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
        let mut stepped = false;
        while self.accumulator >= FIXED_DT {
            unsafe { ffi::b3World_Step(self.world, FIXED_DT, SUBSTEPS) };
            self.accumulator -= FIXED_DT;
            stepped = true;
        }
        if !stepped {
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
        // children follow their root through the hierarchy
        let roots: Vec<ObjectId> = selected
            .iter()
            .copied()
            .filter(|&id| {
                scene
                    .object(id)
                    .is_some_and(|o| o.parent.map_or(true, |p| !selected.contains(&p)))
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
