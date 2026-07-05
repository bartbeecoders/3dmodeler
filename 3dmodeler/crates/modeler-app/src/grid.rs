//! Reference grid on the XY ground plane (Z up, Blender convention).
//!
//! Built from thin quads so it needs no custom shader. A shader-based
//! infinite grid with distance fade is a polish item for later.

use three_d::*;

const EXTENT: f32 = 50.0;
const MAJOR_EVERY: i32 = 10;

const X_AXIS_COLOR: Srgba = Srgba::new(174, 66, 55, 255); // Blender-ish red
const Y_AXIS_COLOR: Srgba = Srgba::new(96, 148, 58, 255); // Blender-ish green

struct GridBuilder {
    positions: Vec<Vec3>,
    colors: Vec<Srgba>,
    indices: Vec<u32>,
}

impl GridBuilder {
    fn quad(&mut self, corners: [Vec3; 4], color: Srgba) {
        let base = self.positions.len() as u32;
        self.positions.extend_from_slice(&corners);
        self.colors.extend_from_slice(&[color; 4]);
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    /// Line parallel to the X axis at the given y.
    fn line_x(&mut self, y: f32, half_width: f32, color: Srgba) {
        self.quad(
            [
                vec3(-EXTENT, y - half_width, 0.0),
                vec3(EXTENT, y - half_width, 0.0),
                vec3(EXTENT, y + half_width, 0.0),
                vec3(-EXTENT, y + half_width, 0.0),
            ],
            color,
        );
    }

    /// Line parallel to the Y axis at the given x.
    fn line_y(&mut self, x: f32, half_width: f32, color: Srgba) {
        self.quad(
            [
                vec3(x - half_width, -EXTENT, 0.0),
                vec3(x + half_width, -EXTENT, 0.0),
                vec3(x + half_width, EXTENT, 0.0),
                vec3(x - half_width, EXTENT, 0.0),
            ],
            color,
        );
    }
}

pub fn build_grid(
    context: &Context,
    spacing: f32,
    minor: [u8; 3],
    major: [u8; 3],
) -> Gm<Mesh, ColorMaterial> {
    let spacing = spacing.clamp(0.05, 10.0);
    let minor = Srgba::new(minor[0], minor[1], minor[2], 255);
    let major = Srgba::new(major[0], major[1], major[2], 255);
    let mut builder = GridBuilder {
        positions: Vec::new(),
        colors: Vec::new(),
        indices: Vec::new(),
    };

    let count = (EXTENT / spacing) as i32;
    for i in -count..=count {
        if i == 0 {
            continue; // axis lines drawn separately
        }
        let offset = i as f32 * spacing;
        let (half_width, color) = if i % MAJOR_EVERY == 0 {
            (0.014, major)
        } else {
            (0.010, minor)
        };
        builder.line_x(offset, half_width, color);
        builder.line_y(offset, half_width, color);
    }
    builder.line_x(0.0, 0.022, X_AXIS_COLOR); // X axis
    builder.line_y(0.0, 0.022, Y_AXIS_COLOR); // Y axis

    let cpu_mesh = CpuMesh {
        positions: Positions::F32(builder.positions),
        colors: Some(builder.colors),
        indices: Indices::U32(builder.indices),
        ..Default::default()
    };

    Gm::new(Mesh::new(context, &cpu_mesh), ColorMaterial::default())
}
