//! The 3D viewport: Aether's renderer, and its image on the window.
//!
//! Two things live here because they are the seam between the engine and the
//! app, and neither belongs to either side alone.
//!
//! **The renderer draws into its own texture, not into the window.**
//! [`aether_render::Renderer::render`] records and submits its own command
//! buffer, ending with a tonemapped `Rgba8UnormSrgb` image in `targets.ldr`.
//! The window's swapchain is a different texture in a different format — usually
//! `Bgra8UnormSrgb` — so it cannot be a texture-to-texture copy, which requires
//! the formats to match. [`Viewport3d::blit`] is the fullscreen triangle that
//! moves it across, and it is also where the interface's frame gets its
//! background: the blit runs first, egui paints over it.
//!
//! **The camera has to be translated.** The app's camera measures a Blender
//! viewport — Z up, an orthographic mode that axis views switch into — and the
//! engine's wants a position, a target and a [`aether_render::Projection`].
//! [`aether_camera`] is that translation, and the orthographic half of it is the
//! reason the engine grew a projection enum.

use aether_render::{Gpu, Renderer, RendererConfig};

use super::camera::{Camera, Projection};

const BLIT_SHADER: &str = r#"
struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// One triangle covering the screen, not two: no seam down the diagonal, and
// half the vertex work. The third vertex sits off-screen by design.
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    var out: VsOut;
    let x = f32(i32(index & 1u) * 4 - 1);
    let y = f32(i32(index >> 1u) * 4 - 1);
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    // Clip space is +y up and a texture is +v down.
    out.uv = vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5);
    return out;
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Both textures are sRGB, so the hardware decodes here and re-encodes on
    // write: the image survives the trip unchanged.
    return textureSampleLevel(source, source_sampler, in.uv, 0.0);
}
"#;

pub struct Viewport3d {
    renderer: Renderer,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Rebuilt whenever the renderer's targets are reallocated, because the
    /// texture view it names is freed by a resize.
    bind_group: wgpu::BindGroup,
}

impl Viewport3d {
    pub fn new(gpu: Gpu, width: u32, height: u32, target_format: wgpu::TextureFormat) -> Self {
        let mut renderer = Renderer::new(
            gpu.clone(),
            RendererConfig { width: width.max(1), height: height.max(1), temporal_jitter: true },
        );
        calibrate_exposure(&mut renderer);
        calm_fog(&mut renderer);
        calm_grain(&mut renderer);

        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("viewport_blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
        });
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("viewport_blit_bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewport_blit_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("viewport_blit_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        // Linear, because the renderer's target and the window are not always
        // the same size: a window resized between frames scales rather than
        // showing nearest-neighbour blocks for a frame.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("viewport_blit_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bind_group = Self::make_bind_group(device, &bind_group_layout, &sampler, &renderer);
        Self { renderer, pipeline, bind_group_layout, sampler, bind_group }
    }

    fn make_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        renderer: &Renderer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewport_blit_bg"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&renderer.targets.ldr.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Matches the render targets to the drawable size.
    ///
    /// A no-op at the same size: reallocating every full-screen target is the
    /// most expensive thing this file can do, and a window drag delivers a
    /// resize per frame.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if self.renderer.targets.width == width && self.renderer.targets.height == height {
            return;
        }
        self.renderer.resize(width, height);
        self.bind_group = Self::make_bind_group(
            &self.renderer.gpu.device.clone(),
            &self.bind_group_layout,
            &self.sampler,
            &self.renderer,
        );
    }

    /// Draws the scene. Submits its own command buffer.
    pub fn render(&mut self, scene: &aether_render::RenderScene, delta_seconds: f32) {
        self.renderer.render(scene, delta_seconds);
    }

    /// Puts the rendered image on the window.
    pub fn blit(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("viewport_blit"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    // Clear rather than load: the blit covers every pixel, so
                    // loading would be a read of something about to be
                    // overwritten in full.
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    /// The last rendered frame as tightly packed RGBA8, top row first.
    pub fn read_output(&self) -> Vec<u8> {
        self.renderer.read_output()
    }
}

/// Exposure compensation when the histogram meters the frame (Lights mode),
/// in stops.
///
/// Measured rather than guessed: with the studio rig and no compensation, a
/// default 0.8-albedo material clipped to white while a 0.03-albedo one sat at
/// about half. Those two points put the default material roughly three and a
/// half stops over, which is this number.
const SCENE_EXPOSURE_COMPENSATION_EV: f32 = -3.5;

/// Exposure compensation for the studio rig's fixed exposure, in stops.
///
/// Measured against the automatic metering this replaces, so switching to
/// manual did not change the viewport's familiar brightness: with the physical
/// camera at f/8, 1/160 s, ISO 100 (see [`aether_camera`]), this puts the
/// default cube's side face at the same level the metered viewport converged
/// to in a perspective view of the default scene.
const STUDIO_EXPOSURE_COMPENSATION_EV: f32 = -0.9;

/// Calibrates the renderer's exposure for a modelling viewport.
pub fn calibrate_exposure(renderer: &mut Renderer) {
    sync_exposure(renderer, false);
}

/// Points the exposure at what actually sets the level of the image.
///
/// The studio rig is a light of known, constant strength, so under it exposure
/// is *manual*: the physical camera in [`aether_camera`] fixes the level, and
/// a mid-grey material reads as the same mid grey in every view. Automatic
/// metering here is not just unnecessary, it is wrong — snap to a front or
/// side view and the frame is almost entirely empty sky, the histogram dives
/// toward its moonless-night floor, and over a few seconds the whole image
/// brightens into a white haze while the meter "adapts" to blackness it was
/// never meant to expose for. (Disabling the pass outright is not an option:
/// it owns the buffer the final grade multiplies by, and with it off nothing
/// ever writes that buffer — the image comes out black. Manual mode keeps the
/// pass writing, from the camera instead of the histogram.)
///
/// The scene's own lights (Lights mode) are the opposite case: their absolute
/// level is whatever the user typed, from a candle to a floodlight, and the
/// light calibration in `scene_render` deliberately only promises *relative*
/// brightness — automatic metering is what turns that into a readable image,
/// anchored by the fill dome so it cannot run away.
pub fn sync_exposure(renderer: &mut Renderer, scene_lighting: bool) {
    use aether_render::passes::exposure::ExposureMode;
    let config = renderer.exposure_config_mut();
    if scene_lighting {
        config.mode = ExposureMode::Automatic;
        config.compensation_ev = SCENE_EXPOSURE_COMPENSATION_EV;
    } else {
        config.mode = ExposureMode::Manual;
        config.compensation_ev = STUDIO_EXPOSURE_COMPENSATION_EV;
    }
}

/// Stills the volumetric fog for a modelling viewport.
///
/// The engine's default fog drifts its density noise with a wind vector — a
/// weather effect. On a still scene in a tool that reads as the corner of the
/// screen faintly flickering, and while the fog is animated its temporal
/// filter never converges. With the drift off, the engine detects the still
/// scene and lets the haze settle to a stable image.
pub fn calm_fog(renderer: &mut Renderer) {
    let fog = renderer.fog_config_mut();
    fog.noise_strength = 0.0;
    fog.wind = [0.0; 3];

}

/// Turns the film grain off for a modelling viewport.
///
/// The grain is applied after the temporal filter, so it is the one effect
/// that never settles no matter how long the scene sits still. In a film
/// frame that is the point — emulsion boils — but in a tool it reads as the
/// whole screen faintly shimmering behind a static model. The quantisation
/// dither underneath it stays on: half a code value of triangular noise is
/// what keeps the fog's gradients from banding, and it is below anything a
/// screen can show.
pub fn calm_grain(renderer: &mut Renderer) {
    renderer.grade_config_mut().grain_enabled = false;
}

/// The app's camera, as the engine's.
///
/// The projections are translated rather than approximated. An axis view in
/// this app is orthographic because a front elevation traced against a
/// reference image has to keep parallel edges parallel; a long-lens perspective
/// standing in for it would put convergence into exactly the view that must not
/// have any.
pub fn aether_camera(camera: &Camera) -> aether_render::Camera {
    let projection = match camera.projection_kind() {
        Projection::Perspective { fov_y } => {
            aether_render::Projection::Perspective { fov_y_radians: fov_y.radians() }
        }
        Projection::Orthographic { height } => {
            aether_render::Projection::Orthographic { height, far: camera.z_far() }
        }
    };
    aether_render::Camera {
        position: camera.position(),
        target: camera.target(),
        up: camera.up(),
        projection,
        near: camera.z_near(),
        // The scene is a building at most, so the default 120 m would fit
        // cascades around empty space. Tied to the far plane instead, which the
        // app already scales to what it is looking at.
        shadow_distance: camera.z_far().min(200.0),
        // A daylight exposure, because the studio rig is a daylight-strength
        // key (see `SUN_LUX_PER_UNIT`). The engine's default f/5.6 at 1/125 is
        // an indoor setting and blows a lit scene out by about three stops —
        // the symptom is a white cube, not a bright one.
        aperture_f_stops: 8.0,
        shutter_speed: 1.0 / 160.0,
        iso: 100.0,
        ..aether_render::Camera::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::math::{degrees, vec3, Viewport};

    #[test]
    fn an_axis_view_stays_orthographic_through_the_translation() {
        // The whole reason the engine grew a projection enum: if this came back
        // perspective, every traced elevation would be subtly wrong.
        let camera = Camera::new_orthographic(
            Viewport::new_at_origo(800, 600),
            vec3(0.0, -10.0, 0.0),
            vec3(0.0, 0.0, 0.0),
            vec3(0.0, 0.0, 1.0),
            8.0,
            0.1,
            500.0,
        );
        let engine = aether_camera(&camera);
        assert!(engine.is_orthographic());
        assert_eq!(
            engine.projection,
            aether_render::Projection::Orthographic { height: 8.0, far: 500.0 }
        );
    }

    #[test]
    fn a_user_perspective_view_keeps_its_field_of_view() {
        let camera = Camera::new_perspective(
            Viewport::new_at_origo(800, 600),
            vec3(0.0, -10.0, 4.0),
            vec3(0.0, 0.0, 0.0),
            vec3(0.0, 0.0, 1.0),
            degrees(45.0),
            0.1,
            500.0,
        );
        let engine = aether_camera(&camera);
        let fov = engine.fov_y_radians().expect("a perspective camera has a field of view");
        assert!((fov.to_degrees() - 45.0).abs() < 1e-3);
        assert_eq!(engine.position, camera.position());
        assert_eq!(engine.target, camera.target());
    }
}
