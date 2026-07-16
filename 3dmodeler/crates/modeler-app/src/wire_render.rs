//! GPU wireframe for the Wireframe shading mode: all sharp edges of all
//! visible objects in at most three GL_LINES draw calls (one per selection
//! tier), replacing the per-frame CPU projection through the egui painter.
//!
//! The world-space vertex buffer is rebuilt only when a content signature
//! (mesh identities, world transforms, selection tiers) changes — orbiting
//! the camera costs one uniform upload, not an O(edges) re-projection.

use crate::scene_render::{hash_primitive, WireframeCache};
use crate::selection::Selection;
use modeler_core::Scene;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use three_d::*;

const VERTEX_SHADER: &str = "
uniform mat4 viewProjection;
in vec3 position;
void main() {
    gl_Position = viewProjection * vec4(position, 1.0);
}
";

const FRAGMENT_SHADER: &str = "
uniform vec4 color;
layout (location = 0) out vec4 outColor;
void main() {
    outColor = color;
}
";

/// Tier colors (normal, selected, active, selected+modifiers, active+modifiers)
/// — echo the selection outline colors (orange default, purple with modifiers).
const TIER_COLORS: [[f32; 4]; 5] = [
    [150.0 / 255.0, 160.0 / 255.0, 175.0 / 255.0, 1.0],
    [230.0 / 255.0, 110.0 / 255.0, 20.0 / 255.0, 1.0],
    [255.0 / 255.0, 170.0 / 255.0, 64.0 / 255.0, 1.0],
    [150.0 / 255.0, 80.0 / 255.0, 230.0 / 255.0, 1.0],
    [200.0 / 255.0, 140.0 / 255.0, 255.0 / 255.0, 1.0],
];

pub struct WireRender {
    context: Context,
    program: Program,
    cache: WireframeCache,
    positions: Option<VertexBuffer<Vec3>>,
    /// (first vertex, vertex count) per tier within `positions`.
    ranges: [(u32, u32); 5],
    signature: Option<u64>,
}

/// Everything the world-space line buffer depends on: per visible object
/// its mesh identity, world placement and selection tier.
fn wire_signature(scene: &Scene, selection: &Selection) -> u64 {
    let mut h = DefaultHasher::new();
    let worlds = scene.world_transforms();
    for object in scene.objects() {
        if !object.visible {
            continue;
        }
        object.id.0.hash(&mut h);
        hash_primitive(&mut h, &object.primitive);
        object.smooth.hash(&mut h);
        object.mesh_revision.hash(&mut h);
        let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
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
            f.to_bits().hash(&mut h);
        }
        let has_mod = !object.modifiers.is_empty();
        has_mod.hash(&mut h);
        let tier: u8 = if selection.active() == Some(object.id) {
            if has_mod { 4 } else { 2 }
        } else if selection.is_selected(object.id) {
            if has_mod { 3 } else { 1 }
        } else {
            0
        };
        tier.hash(&mut h);
    }
    h.finish()
}

impl WireRender {
    pub fn new(context: &Context) -> Self {
        Self {
            context: context.clone(),
            program: Program::from_source(context, VERTEX_SHADER, FRAGMENT_SHADER)
                .expect("wireframe shaders compile"),
            cache: WireframeCache::new(),
            positions: None,
            ranges: [(0, 0); 5],
            signature: None,
        }
    }

    /// Rebuild the line buffer if the wireframe-relevant content changed.
    pub fn sync(&mut self, scene: &Scene, selection: &Selection) {
        let signature = wire_signature(scene, selection);
        if self.signature == Some(signature) {
            return;
        }
        self.signature = Some(signature);

        let segments = self.cache.segments(scene, selection);
        let mut tiers: [Vec<Vec3>; 5] = Default::default();
        for (a, b, tier) in segments {
            let t = &mut tiers[usize::from(tier).min(4)];
            t.push(vec3(a.x, a.y, a.z));
            t.push(vec3(b.x, b.y, b.z));
        }
        let mut all: Vec<Vec3> = Vec::with_capacity(tiers.iter().map(Vec::len).sum());
        for (i, tier) in tiers.iter().enumerate() {
            self.ranges[i] = (all.len() as u32, tier.len() as u32);
            all.extend_from_slice(tier);
        }
        self.positions =
            (!all.is_empty()).then(|| VertexBuffer::new_with_data(&self.context, &all));
    }

    /// Draw the lines over the current render target (no depth test, like
    /// the egui overlay this replaces).
    pub fn render(&self, viewport: Viewport, camera: &Camera) {
        let Some(positions) = &self.positions else {
            return;
        };
        self.program
            .use_uniform("viewProjection", camera.projection() * camera.view());
        let render_states = RenderStates {
            depth_test: DepthTest::Always,
            write_mask: WriteMask::COLOR,
            blend: Blend::Disabled,
            cull: Cull::None,
        };
        for (i, &(first, count)) in self.ranges.iter().enumerate() {
            if count == 0 {
                continue;
            }
            self.program.use_uniform("color", vec4(
                TIER_COLORS[i][0],
                TIER_COLORS[i][1],
                TIER_COLORS[i][2],
                TIER_COLORS[i][3],
            ));
            self.program.use_vertex_attribute("position", positions);
            self.program.draw_with(render_states, viewport, || unsafe {
                use three_d::context::HasContext as _;
                self.context
                    .draw_arrays(three_d::context::LINES, first as i32, count as i32);
            });
        }
    }
}
