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

/// Destroy a mirrored body and free the mesh data its shapes referenced.
unsafe fn destroy_entry(entry: &mut BodyEntry) {
    ffi::b3DestroyBody(entry.body);
    for mesh in entry.meshes.drain(..) {
        ffi::b3DestroyMesh(mesh);
    }
}

pub struct PhysicsMirror {
    world: ffi::b3WorldId,
    worker_count: u32,
    synced_version: Option<u64>,
    entries: HashMap<ObjectId, BodyEntry>,
    /// Simulate-mode ground plane (never has an ObjectId).
    ground: Option<ffi::b3BodyId>,
    /// Dynamic bodies in parent-before-child order for the per-step
    /// write-back; built at play (the mapping is frozen while simulating).
    sim_order: Vec<ObjectId>,
    sim: SimState,
    pub ground_plane: bool,
    snapshot: Vec<(ObjectId, Transform)>,
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
                ground: None,
                sim_order: Vec::new(),
                sim: SimState::Stopped,
                ground_plane: true,
                snapshot: Vec::new(),
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
            .filter(|o| o.visible && o.dynamic)
            .count();
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
                let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
                let key = ShapeKey::of(object, world.scale);
                let entry = self.create_entry(object, &world, key, true);
                if ffi::b3Body_GetType(entry.body) == ffi::b3BodyType_b3_dynamicBody {
                    self.sim_order.push(object.id);
                }
                self.entries.insert(object.id, entry);
            }
        }
        // parents first so children's local conversions see updated parents
        self.sim_order.sort_by_key(|id| scene.depth(*id));
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
    }

    // --- queries ------------------------------------------------------------

    /// Mouse picking: closest object hit by the ray, Blender-style.
    pub fn pick(&self, origin: Vec3, direction: Vec3) -> Option<ObjectId> {
        unsafe {
            let result = ffi::b3World_CastRayClosest(
                self.world,
                bvec(origin),
                bvec(direction * 10_000.0),
                ffi::b3DefaultQueryFilter(),
            );
            if result.hit {
                let user_data = ffi::b3Shape_GetUserData(result.shapeId) as usize as u64;
                if user_data == 0 {
                    return None; // ground plane
                }
                Some(ObjectId(user_data))
            } else {
                None
            }
        }
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
}
