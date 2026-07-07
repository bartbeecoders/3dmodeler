//! box3d physics mirror and simulation.
//!
//! Edit mode: every visible object owns one STATIC body + shape, kept in sync
//! with the scene; all spatial queries (picking, overlap warnings, drop to
//! floor) run against this world.
//!
//! Simulate mode (play/pause/stop): the world is rebuilt with dynamic bodies
//! for objects marked dynamic, stepped at a fixed 60 Hz, and body transforms
//! are written back into the scene each frame. Stop restores the transform
//! snapshot taken at play.

use crate::selection::Selection;
use box3d_sys as ffi;
use modeler_core::glam::Vec3;
use modeler_core::{ObjectId, Primitive, Scene, Transform};
use std::collections::HashSet;
use std::os::raw::c_void;

fn bvec(v: Vec3) -> ffi::b3Vec3 {
    ffi::b3Vec3 { x: v.x, y: v.y, z: v.z }
}

const FIXED_DT: f32 = 1.0 / 60.0;
const SUBSTEPS: i32 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimState {
    Stopped,
    Playing,
    Paused,
}

pub struct PhysicsMirror {
    world: ffi::b3WorldId,
    synced_version: Option<u64>,
    bodies: Vec<(ObjectId, ffi::b3BodyId)>,
    /// Mesh data referenced by mesh shapes; box3d does not copy it, so it must
    /// outlive the bodies (see RagdollOnMesh sample).
    meshes: Vec<*mut ffi::b3MeshData>,
    sim: SimState,
    pub ground_plane: bool,
    snapshot: Vec<(ObjectId, Transform)>,
    accumulator: f32,
}

impl PhysicsMirror {
    pub fn new() -> Self {
        unsafe {
            let mut def = ffi::b3DefaultWorldDef();
            def.workerCount = 0; // serial: required on wasm, fine natively
            def.gravity = bvec(Vec3::new(0.0, 0.0, -9.81)); // Z-up world
            Self {
                world: ffi::b3CreateWorld(&def),
                synced_version: None,
                bodies: Vec::new(),
                meshes: Vec::new(),
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

    /// Rebuild the static mirror if the scene changed. No-op while simulating
    /// (the simulation owns the world then).
    pub fn sync(&mut self, scene: &Scene) {
        if self.sim != SimState::Stopped {
            return;
        }
        if self.synced_version == Some(scene.version()) {
            return;
        }
        self.synced_version = Some(scene.version());
        self.rebuild(scene, false);
    }

    fn destroy_bodies(&mut self) {
        unsafe {
            for (_, body) in self.bodies.drain(..) {
                ffi::b3DestroyBody(body);
            }
            for mesh in self.meshes.drain(..) {
                ffi::b3DestroyMesh(mesh);
            }
        }
    }

    /// `simulate`: honor per-object dynamic flags and add the ground plane.
    fn rebuild(&mut self, scene: &Scene, simulate: bool) {
        self.destroy_bodies();
        unsafe {
            if simulate && self.ground_plane {
                let mut body_def = ffi::b3DefaultBodyDef();
                body_def.position = bvec(Vec3::new(0.0, 0.0, -0.5));
                let ground = ffi::b3CreateBody(self.world, &body_def);
                let shape_def = ffi::b3DefaultShapeDef();
                let hull = ffi::b3MakeBoxHull(200.0, 200.0, 0.5); // top at z = 0
                ffi::b3CreateHullShape(ground, &shape_def, &hull.base);
                // ground has no ObjectId; userData 0 is never a valid id
                self.bodies.push((ObjectId(0), ground));
            }

            for object in scene.objects() {
                if !object.visible {
                    continue; // hidden objects are not pickable / simulated
                }
                let t = scene.world_transform(object.id);
                let mut body_def = ffi::b3DefaultBodyDef();
                body_def.position = bvec(t.location);
                body_def.rotation = ffi::b3Quat {
                    v: ffi::b3Vec3 { x: t.rotation.x, y: t.rotation.y, z: t.rotation.z },
                    s: t.rotation.w,
                };
                if simulate && object.dynamic {
                    body_def.type_ = ffi::b3BodyType_b3_dynamicBody;
                }
                let body = ffi::b3CreateBody(self.world, &body_def);

                let mut shape_def = ffi::b3DefaultShapeDef();
                shape_def.userData = object.id.0 as usize as *mut c_void;
                shape_def.density = object.density.max(0.001);

                self.create_shape(body, &shape_def, object, t.scale);
                self.bodies.push((object.id, body));
            }
        }
    }

    /// Scale is baked into the shape geometry; position/rotation live on the
    /// body.
    unsafe fn create_shape(
        &mut self,
        body: ffi::b3BodyId,
        shape_def: &ffi::b3ShapeDef,
        object: &modeler_core::Object,
        scale: Vec3, // WORLD scale (baked into geometry)
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
            Primitive::Torus { .. } if !object.dynamic || self.sim == SimState::Stopped => {
                let mesh = object.primitive.generate(true); // shared-vertex topology
                self.create_mesh_shape(body, shape_def, &mesh, scale);
            }
            // walls with door/window cutouts: exact triangle mesh so rays and
            // bodies pass through the openings (solid walls stay convex hulls)
            Primitive::Wall { .. }
                if !object.cutouts.is_empty()
                    && (!object.dynamic || self.sim == SimState::Stopped) =>
            {
                let mesh = object.collision_mesh();
                self.create_mesh_shape(body, shape_def, &mesh, scale);
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
    /// mesh data, so it is stored until the bodies are destroyed.
    unsafe fn create_mesh_shape(
        &mut self,
        body: ffi::b3BodyId,
        shape_def: &ffi::b3ShapeDef,
        mesh: &modeler_core::MeshData,
        scale: Vec3,
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
            self.meshes.push(mesh_data); // shape references it; keep alive
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
                self.rebuild(scene, true);
                self.accumulator = 0.0;
            }
        }
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
        self.synced_version = None; // force a static rebuild on next sync
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
        let mut updates: Vec<(ObjectId, Transform)> = Vec::new();
        unsafe {
            for (id, body) in &self.bodies {
                if id.0 == 0 {
                    continue; // ground plane
                }
                if ffi::b3Body_GetType(*body) != ffi::b3BodyType_b3_dynamicBody {
                    continue;
                }
                let t = ffi::b3Body_GetTransform(*body);
                let mut world = scene.world_transform(*id);
                world.location = Vec3::new(t.p.x, t.p.y, t.p.z);
                world.rotation =
                    modeler_core::glam::Quat::from_xyzw(t.q.v.x, t.q.v.y, t.q.v.z, t.q.s);
                updates.push((*id, world));
            }
        }
        // parents first so children's local conversions see updated parents
        updates.sort_by_key(|(id, _)| scene.depth(*id));
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
            exclude: HashSet<u64>,
            hit: bool,
        }
        unsafe extern "C" fn callback(shape: ffi::b3ShapeId, context: *mut c_void) -> bool {
            let ctx = &mut *(context as *mut Ctx);
            let user_data = ffi::b3Shape_GetUserData(shape) as usize as u64;
            if user_data != 0 && !ctx.exclude.contains(&user_data) {
                ctx.hit = true;
                return false; // found one, stop the query
            }
            true
        }

        let mut result = HashSet::new();
        let exclude: HashSet<u64> = ids.iter().map(|id| id.0).collect();
        unsafe {
            for (id, body) in &self.bodies {
                if !exclude.contains(&id.0) {
                    continue;
                }
                let mut shapes: [ffi::b3ShapeId; 4] = std::mem::zeroed();
                let count = ffi::b3Body_GetShapes(*body, shapes.as_mut_ptr(), 4);
                for shape in shapes.iter().take(count as usize) {
                    let aabb = ffi::b3Shape_GetAABB(*shape);
                    let mut ctx = Ctx { exclude: exclude.clone(), hit: false };
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

    /// Drop each selected object straight down onto whatever is below it
    /// (other objects via box3d ray cast, else the ground plane at z = 0).
    pub fn drop_to_floor(&self, scene: &mut Scene, selection: &Selection) {
        struct Ctx {
            exclude: HashSet<u64>,
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
            if ctx.exclude.contains(&user_data) {
                return -1.0; // ignore selected objects, keep going
            }
            let z = point.z;
            ctx.best_z = Some(ctx.best_z.map_or(z, |b: f32| b.max(z)));
            fraction // clip: we only care about the closest hit below
        }

        let ids: Vec<ObjectId> = selection.selected().to_vec();
        let exclude: HashSet<u64> = ids.iter().map(|id| id.0).collect();
        for id in ids {
            let Some(object) = scene.object(id) else { continue };
            let world = scene.world_transform(id);
            let origin = world.location;
            let bottom = object.primitive.bottom_offset() * world.scale.z.abs();

            let mut ctx = Ctx { exclude: exclude.clone(), best_z: None };
            unsafe {
                ffi::b3World_CastRay(
                    self.world,
                    bvec(origin),
                    bvec(Vec3::new(0.0, 0.0, -1000.0)),
                    ffi::b3DefaultQueryFilter(),
                    Some(callback),
                    &mut ctx as *mut Ctx as *mut c_void,
                );
            }
            let floor_z = ctx.best_z.unwrap_or(0.0).max(0.0);
            let mut world = scene.world_transform(id);
            world.location.z = floor_z + bottom;
            scene.set_world_transform(id, world);
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
}

impl Drop for PhysicsMirror {
    fn drop(&mut self) {
        unsafe {
            ffi::b3DestroyWorld(self.world); // takes the bodies with it
            for mesh in self.meshes.drain(..) {
                ffi::b3DestroyMesh(mesh);
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
    fn poke_kicks_dynamic_bodies_only() {
        let _guard = ffi_lock();
        let mut scene = Scene::new();
        let mut t = Transform::default();
        t.location.z = 5.0;
        let cube = scene.add_object(Primitive::Cube { size: 2.0 }, t);
        scene.object_mut(cube).unwrap().dynamic = true;
        let mut wall_t = Transform::default();
        wall_t.location.x = 10.0;
        let wall = scene.add_object(Primitive::Cube { size: 2.0 }, wall_t); // static
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
}
