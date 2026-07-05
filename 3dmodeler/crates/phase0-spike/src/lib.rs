//! Phase 0 spike: prove that box3d simulates and answers spatial queries
//! inside a browser WASM module.
//!
//! The scenario mirrors what the modeler needs from box3d:
//!  1. create a world (serial, no threads)
//!  2. add a static ground and a dynamic sphere, step the simulation
//!  3. mouse-pick style ray cast (`b3World_CastRayClosest`)

use box3d_sys as ffi;

fn vec3(x: f32, y: f32, z: f32) -> ffi::b3Vec3 {
    ffi::b3Vec3 { x, y, z }
}

pub fn run_spike() -> (bool, String) {
    let mut report = String::new();
    let mut ok = true;
    let check = |report: &mut String, label: &str, pass: bool, detail: String| {
        report.push_str(&format!("[{}] {} — {}\n", if pass { "ok" } else { "FAIL" }, label, detail));
        pass
    };

    unsafe {
        // 1. world, forced serial (browser: no threads)
        let mut world_def = ffi::b3DefaultWorldDef();
        world_def.workerCount = 0;
        let world = ffi::b3CreateWorld(&world_def);
        ok &= check(&mut report, "create world", ffi::b3World_IsValid(world), format!("index {}", world.index1));

        // 2. static ground box, top surface at y = 0.5
        let ground_def = ffi::b3DefaultBodyDef();
        let ground = ffi::b3CreateBody(world, &ground_def);
        let shape_def = ffi::b3DefaultShapeDef();
        let hull = ffi::b3MakeBoxHull(20.0, 0.5, 20.0);
        ffi::b3CreateHullShape(ground, &shape_def, &hull.base);

        // dynamic sphere dropped from y = 5
        let mut ball_def = ffi::b3DefaultBodyDef();
        ball_def.type_ = ffi::b3BodyType_b3_dynamicBody;
        ball_def.position = vec3(0.0, 5.0, 0.0);
        let ball = ffi::b3CreateBody(world, &ball_def);
        let sphere = ffi::b3Sphere { center: vec3(0.0, 0.0, 0.0), radius: 0.5 };
        ffi::b3CreateSphereShape(ball, &shape_def, &sphere);

        // 3. simulate 2 seconds
        for _ in 0..120 {
            ffi::b3World_Step(world, 1.0 / 60.0, 4);
        }

        // ball should rest on the ground: center y = ground top (0.5) + radius (0.5)
        let p = ffi::b3Body_GetPosition(ball);
        ok &= check(
            &mut report,
            "sphere settled",
            (p.y - 1.0).abs() < 0.05,
            format!("position ({:.3}, {:.3}, {:.3}), expected y ≈ 1.0", p.x, p.y, p.z),
        );

        // 4. picking-style ray cast straight down onto the ball
        let result = ffi::b3World_CastRayClosest(
            world,
            vec3(0.0, 10.0, 0.0),
            vec3(0.0, -20.0, 0.0),
            ffi::b3DefaultQueryFilter(),
        );
        // ray should hit the top of the ball at y ≈ 1.5
        ok &= check(
            &mut report,
            "ray cast hit",
            result.hit && (result.point.y - 1.5).abs() < 0.05,
            format!(
                "hit={} point ({:.3}, {:.3}, {:.3}), expected y ≈ 1.5",
                result.hit, result.point.x, result.point.y, result.point.z
            ),
        );

        // hit shape should be the ball, not the ground
        let hit_body = ffi::b3Shape_GetBody(result.shapeId);
        ok &= check(
            &mut report,
            "picked correct object",
            hit_body.index1 == ball.index1,
            format!("hit body index {}, ball index {}", hit_body.index1, ball.index1),
        );

        ffi::b3DestroyWorld(world);
    }

    report.push_str(if ok { "PHASE 0: PASS\n" } else { "PHASE 0: FAIL\n" });
    (ok, report)
}

// --- wasm entry points ------------------------------------------------

#[cfg(target_arch = "wasm32")]
mod wasm {
    extern "C" {
        // Provided by the JS host (browser page or node runner).
        fn host_log(ptr: *const u8, len: usize);
    }

    fn log(msg: &str) {
        unsafe { host_log(msg.as_ptr(), msg.len()) }
    }

    /// Runs the spike and reports through the console. Returns 0 on success.
    #[no_mangle]
    pub extern "C" fn phase0_run() -> i32 {
        let (ok, report) = super::run_spike();
        log(&report);
        if ok {
            0
        } else {
            1
        }
    }

    /// Called by wasm_shims.c: box3d's printf/assert output.
    #[no_mangle]
    pub extern "C" fn js_log(ptr: *const u8, len: usize) {
        unsafe { host_log(ptr, len) }
    }
}
