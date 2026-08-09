//! An egui painter on wgpu.
//!
//! # Why this is hand-written rather than `egui-wgpu`
//!
//! No published `egui-wgpu` targets the wgpu the engine uses: 0.32 wants wgpu
//! 25, 0.33 wants 27, 0.34 wants 29, and Aether is on 26. Two wgpu crates in one
//! binary are two unrelated sets of types, so the painter could not be handed
//! the same `Device` the renderer draws with — it is not a version warning, it
//! is a type error.
//!
//! That left bumping the engine's wgpu across three majors for a reason that has
//! nothing to do with the engine, or owning ~300 lines of painter. This is the
//! painter. It also ends the coupling permanently: egui and wgpu can now move
//! independently of each other.
//!
//! It is a small thing to own. egui hands over finished triangles — positions,
//! UVs and colours, already tessellated and already batched by texture — plus a
//! list of texture updates. There is no layout, no font rasterization and no
//! state machine here.
//!
//! # The three conversions that have to be right
//!
//! * **Positions are in logical points**, not pixels, and the origin is the top
//!   left. The shader divides by the screen size in points and flips Y.
//! * **Colours are premultiplied sRGB.** They are decoded to linear in the
//!   shader and the target re-encodes on write. Skipping the decode leaves the
//!   whole interface visibly washed out; decoding the *alpha* as well leaves
//!   every fade too faint, so only RGB goes through the curve.
//! * **Scissor rectangles are in physical pixels** and must be clamped to the
//!   target. A rect one pixel outside it is a validation error that kills the
//!   frame, and egui will hand one over whenever a panel is dragged past the
//!   edge.

use std::collections::HashMap;

use egui::epaint::{ImageDelta, Primitive};
use egui::{ClippedPrimitive, TextureId, TexturesDelta};
use wgpu::util::DeviceExt as _;

/// One vertex, laid out for the GPU.
///
/// Converted from `epaint::Vertex` rather than cast: epaint only derives
/// `bytemuck::Pod` behind a feature flag, and depending on a feature for a
/// 20-byte struct is a worse trade than writing the conversion.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen_size_in_points: [f32; 2],
    _padding: [f32; 2],
}

const SHADER: &str = r#"
struct Uniforms {
    screen_size_in_points: vec2<f32>,
    _padding: vec2<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(1) @binding(0) var t: texture_2d<f32>;
@group(1) @binding(1) var s: sampler;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}

// egui's colours are sRGB-encoded with premultiplied alpha. The target
// re-encodes on write, so they have to be decoded here or every panel comes
// out too bright. Alpha is coverage and was never encoded, so it passes
// through untouched.
fn linear_from_gamma(srgb: vec3<f32>) -> vec3<f32> {
    let cutoff = srgb < vec3<f32>(0.04045);
    let lower = srgb / vec3<f32>(12.92);
    let higher = pow((srgb + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(higher, lower, cutoff);
}

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> VsOut {
    var out: VsOut;
    // Points -> clip space. egui's origin is the top left and clip space's
    // +y is up, hence the sign on y.
    out.position = vec4<f32>(
        2.0 * position.x / uniforms.screen_size_in_points.x - 1.0,
        1.0 - 2.0 * position.y / uniforms.screen_size_in_points.y,
        0.0,
        1.0,
    );
    out.uv = uv;
    out.color = vec4<f32>(linear_from_gamma(color.rgb), color.a);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // textureSampleLevel, not textureSample: the interface has no mip chain
    // and level 0 is the whole texture. It also keeps this shader legal under
    // the engine's ban on implicit derivatives.
    let texel = textureSampleLevel(t, s, in.uv, 0.0);
    return texel * in.color;
}
"#;

/// A texture egui asked us to hold: the font atlas, and any image the app registers.
struct Texture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

pub struct EguiPainter {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    texture_bgl: wgpu::BindGroupLayout,
    sampler_linear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
    textures: HashMap<TextureId, Texture>,
    vertices: Option<wgpu::Buffer>,
    indices: Option<wgpu::Buffer>,
    vertex_capacity: u64,
    index_capacity: u64,
}

impl EguiPainter {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("egui_shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("egui_uniform_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let texture_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("egui_texture_bgl"),
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

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("egui_uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("egui_uniform_bg"),
            layout: &uniform_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("egui_layout"),
            bind_group_layouts: &[&uniform_bgl, &texture_bgl],
            push_constant_ranges: &[],
        });

        const ATTRS: [wgpu::VertexAttribute; 3] =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Unorm8x4];

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("egui_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &ATTRS,
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // Premultiplied alpha: egui has already multiplied colour by
                    // coverage, so the source contributes whole and the
                    // destination is attenuated by what the source covers.
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // egui emits both windings; culling would drop half the UI.
                cull_mode: None,
                ..Default::default()
            },
            // The interface is drawn last, over the finished image, and orders
            // itself by draw order. There is nothing to test against.
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });

        let sampler = |label, filter| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some(label),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: filter,
                min_filter: filter,
                ..Default::default()
            })
        };

        Self {
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            texture_bgl,
            sampler_linear: sampler("egui_linear", wgpu::FilterMode::Linear),
            sampler_nearest: sampler("egui_nearest", wgpu::FilterMode::Nearest),
            textures: HashMap::new(),
            vertices: None,
            indices: None,
            vertex_capacity: 0,
            index_capacity: 0,
        }
    }

    /// Applies egui's texture updates. Must run before [`Self::paint`].
    pub fn apply_textures(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, delta: &TexturesDelta) {
        for (id, image) in &delta.set {
            self.set_texture(device, queue, *id, image);
        }
        // Frees come last: egui may free and re-set the same id in one frame
        // when the atlas is regrown, and freeing afterwards would drop the new
        // texture instead of the old one.
        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    fn set_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: TextureId,
        delta: &ImageDelta,
    ) {
        let egui::epaint::ImageData::Color(image) = &delta.image;
        let [w, h] = image.size;
        let (w, h) = (w as u32, h as u32);
        if w == 0 || h == 0 {
            return;
        }
        // `Color32` is four bytes in RGBA order, so the pixel slice is already
        // the upload buffer.
        let pixels: &[u8] = bytemuck::cast_slice(&image.pixels);

        let filter = match delta.options.magnification {
            egui::TextureFilter::Nearest => &self.sampler_nearest,
            egui::TextureFilter::Linear => &self.sampler_linear,
        };

        match delta.pos {
            // A patch into a texture we already hold. If we do not hold it —
            // which should not happen — there is nothing to patch into, and
            // allocating here would leave the rest of the atlas undefined.
            Some([x, y]) => {
                let Some(existing) = self.textures.get(&id) else { return };
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &existing.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: x as u32, y: y as u32, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    pixels,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(w * 4),
                        rows_per_image: Some(h),
                    },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
            }
            None => {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("egui_texture"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    // sRGB so the hardware decodes on sample, matching the
                    // decode the vertex colours get in the shader.
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    pixels,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(w * 4),
                        rows_per_image: Some(h),
                    },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("egui_texture_bg"),
                    layout: &self.texture_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(filter),
                        },
                    ],
                });
                self.textures.insert(id, Texture { texture, bind_group, width: w, height: h });
            }
        }
    }

    /// Draws the tessellated interface over `target`.
    ///
    /// `size_in_pixels` is the physical target size and `pixels_per_point` the
    /// scale factor; egui works in points and the scissor rectangles have to
    /// come back to pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        primitives: &[ClippedPrimitive],
        size_in_pixels: [u32; 2],
        pixels_per_point: f32,
    ) {
        if size_in_pixels[0] == 0 || size_in_pixels[1] == 0 {
            return;
        }
        queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::bytes_of(&Uniforms {
                screen_size_in_points: [
                    size_in_pixels[0] as f32 / pixels_per_point,
                    size_in_pixels[1] as f32 / pixels_per_point,
                ],
                _padding: [0.0; 2],
            }),
        );

        // One vertex buffer and one index buffer for the whole interface, with
        // each mesh drawn from its own range. egui hands over dozens of small
        // meshes; a buffer each would be dozens of allocations per frame.
        let mut vertices: Vec<Vertex> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut draws: Vec<(TextureId, std::ops::Range<u32>, i32, [u32; 4])> = Vec::new();

        for ClippedPrimitive { clip_rect, primitive } in primitives {
            let Primitive::Mesh(mesh) = primitive else {
                // `Primitive::Callback` is for apps that render 3D *inside* an
                // egui widget. This app puts egui on top of the viewport
                // instead, so there is nothing to run.
                continue;
            };
            if mesh.indices.is_empty() {
                continue;
            }
            let Some(scissor) = scissor_rect(clip_rect, pixels_per_point, size_in_pixels) else {
                // Entirely off-target: a panel dragged past the edge.
                continue;
            };

            let base_vertex = vertices.len() as i32;
            let first_index = indices.len() as u32;
            vertices.extend(mesh.vertices.iter().map(|v| Vertex {
                position: [v.pos.x, v.pos.y],
                uv: [v.uv.x, v.uv.y],
                color: v.color.to_array(),
            }));
            indices.extend_from_slice(&mesh.indices);
            draws.push((
                mesh.texture_id,
                first_index..indices.len() as u32,
                base_vertex,
                scissor,
            ));
        }

        if draws.is_empty() {
            return;
        }

        self.upload(device, queue, &vertices, &indices);
        let (Some(vertex_buffer), Some(index_buffer)) = (&self.vertices, &self.indices) else {
            return;
        };

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("egui"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.uniform_bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);

        for (texture_id, range, base_vertex, [x, y, w, h]) in draws {
            let Some(texture) = self.textures.get(&texture_id) else { continue };
            pass.set_bind_group(1, &texture.bind_group, &[]);
            pass.set_scissor_rect(x, y, w, h);
            pass.draw_indexed(range, base_vertex, 0..1);
        }
    }

    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        vertices: &[Vertex],
        indices: &[u32],
    ) {
        let vertex_bytes: &[u8] = bytemuck::cast_slice(vertices);
        let index_bytes: &[u8] = bytemuck::cast_slice(indices);

        // Grow-only: the interface's size is stable frame to frame, so after a
        // few frames this stops reallocating entirely.
        if self.vertices.is_none() || self.vertex_capacity < vertex_bytes.len() as u64 {
            self.vertices = Some(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("egui_vertices"),
                contents: vertex_bytes,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            }));
            self.vertex_capacity = vertex_bytes.len() as u64;
        } else if let Some(b) = &self.vertices {
            queue.write_buffer(b, 0, vertex_bytes);
        }

        if self.indices.is_none() || self.index_capacity < index_bytes.len() as u64 {
            self.indices = Some(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("egui_indices"),
                contents: index_bytes,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            }));
            self.index_capacity = index_bytes.len() as u64;
        } else if let Some(b) = &self.indices {
            queue.write_buffer(b, 0, index_bytes);
        }
    }

    /// Pixel dimensions of a texture egui gave us, for tests and diagnostics.
    pub fn texture_size(&self, id: TextureId) -> Option<(u32, u32)> {
        self.textures.get(&id).map(|t| (t.width, t.height))
    }

    pub fn texture_count(&self) -> usize {
        self.textures.len()
    }
}

/// egui's clip rectangle, in points, as a scissor rectangle in physical pixels.
///
/// Returns `None` when nothing of the rectangle is on the target. Clamping is
/// not defensive tidiness: wgpu rejects a scissor rectangle that leaves the
/// attachment and kills the whole frame, and egui produces one whenever a panel
/// is dragged past the window edge.
fn scissor_rect(
    clip_rect: &egui::Rect,
    pixels_per_point: f32,
    size_in_pixels: [u32; 2],
) -> Option<[u32; 4]> {
    let min_x = (clip_rect.min.x * pixels_per_point).round().max(0.0) as u32;
    let min_y = (clip_rect.min.y * pixels_per_point).round().max(0.0) as u32;
    let max_x = (clip_rect.max.x * pixels_per_point).round().max(0.0) as u32;
    let max_y = (clip_rect.max.y * pixels_per_point).round().max(0.0) as u32;

    let min_x = min_x.min(size_in_pixels[0]);
    let min_y = min_y.min(size_in_pixels[1]);
    let max_x = max_x.clamp(min_x, size_in_pixels[0]);
    let max_y = max_y.clamp(min_y, size_in_pixels[1]);

    let (width, height) = (max_x - min_x, max_y - min_y);
    // A zero-width scissor is also a validation error, and is what an entirely
    // off-screen panel collapses to.
    (width > 0 && height > 0).then_some([min_x, min_y, width, height])
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, Rect};

    #[test]
    fn a_full_screen_clip_covers_the_target() {
        let r = Rect::from_min_max(pos2(0.0, 0.0), pos2(800.0, 600.0));
        assert_eq!(scissor_rect(&r, 1.0, [800, 600]), Some([0, 0, 800, 600]));
    }

    #[test]
    fn scissor_scales_by_pixels_per_point() {
        let r = Rect::from_min_max(pos2(0.0, 0.0), pos2(400.0, 300.0));
        assert_eq!(scissor_rect(&r, 2.0, [800, 600]), Some([0, 0, 800, 600]));
    }

    #[test]
    fn a_rect_hanging_off_the_edge_is_clamped_not_rejected() {
        // Dragging a panel past the right edge. Unclamped this is a scissor
        // wider than the attachment, which wgpu refuses and the frame dies.
        let r = Rect::from_min_max(pos2(700.0, -50.0), pos2(900.0, 700.0));
        let s = scissor_rect(&r, 1.0, [800, 600]).expect("partly on screen");
        assert_eq!(s, [700, 0, 100, 600]);
        assert!(s[0] + s[2] <= 800 && s[1] + s[3] <= 600);
    }

    #[test]
    fn a_fully_offscreen_rect_is_dropped() {
        let right = Rect::from_min_max(pos2(900.0, 10.0), pos2(1000.0, 20.0));
        assert_eq!(scissor_rect(&right, 1.0, [800, 600]), None);
        // And off the left, where the coordinates go negative.
        let left = Rect::from_min_max(pos2(-200.0, 10.0), pos2(-10.0, 20.0));
        assert_eq!(scissor_rect(&left, 1.0, [800, 600]), None);
    }

    #[test]
    fn an_empty_rect_is_dropped() {
        let r = Rect::from_min_max(pos2(100.0, 100.0), pos2(100.0, 400.0));
        assert_eq!(scissor_rect(&r, 1.0, [800, 600]), None);
    }

    #[test]
    fn an_inverted_rect_does_not_underflow() {
        // egui should not produce one, but the arithmetic is unsigned and a
        // max below min would wrap to a scissor of about four billion.
        let r = Rect::from_min_max(pos2(400.0, 300.0), pos2(100.0, 100.0));
        assert_eq!(scissor_rect(&r, 1.0, [800, 600]), None);
    }

    #[test]
    fn the_vertex_matches_what_the_pipeline_declares() {
        // Unorm8x4 for the colour and two Float32x2 before it: 20 bytes. If
        // this drifts, every vertex reads its neighbour's data.
        assert_eq!(std::mem::size_of::<Vertex>(), 20);
        assert_eq!(std::mem::offset_of!(Vertex, position), 0);
        assert_eq!(std::mem::offset_of!(Vertex, uv), 8);
        assert_eq!(std::mem::offset_of!(Vertex, color), 16);
    }
}
