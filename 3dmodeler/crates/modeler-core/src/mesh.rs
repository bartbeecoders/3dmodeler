//! Mesh primitive generators. All primitives are Z-up and centered at the
//! origin, matching Blender's conventions (cylinder/cone axis along Z, plane
//! and torus in the XY plane).

use glam::{Vec2, Vec3};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::f32::consts::{PI, TAU};

/// Triangle mesh with per-vertex normals, ready for upload by the renderer.
/// Serializable so edited meshes can live in scene files.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeshData {
    pub positions: Vec<Vec3>,
    pub normals: Vec<Vec3>,
    pub indices: Vec<u32>,
    /// Edges cut by the user (loop cut), as position-index pairs. Purely
    /// topological: the welded edit-mode view keeps coplanar faces separated
    /// across these edges, which would otherwise merge back into one face
    /// group and make the cut unselectable. Renderers ignore them.
    #[serde(default)]
    pub seams: Vec<(u32, u32)>,
}

impl MeshData {
    /// Axis-aligned extents (width, depth, height) of the vertices.
    pub fn extents(&self) -> Vec3 {
        if self.positions.is_empty() {
            return Vec3::ZERO;
        }
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for p in &self.positions {
            min = min.min(*p);
            max = max.max(*p);
        }
        max - min
    }

    /// Re-expand into per-face vertices with face normals (flat shading).
    pub fn into_flat(self) -> MeshData {
        let mut out = MeshData::default();
        out.positions.reserve(self.indices.len());
        out.normals.reserve(self.indices.len());
        out.indices.reserve(self.indices.len());
        for tri in self.indices.chunks_exact(3) {
            let a = self.positions[tri[0] as usize];
            let b = self.positions[tri[1] as usize];
            let c = self.positions[tri[2] as usize];
            let n = (b - a).cross(c - a).normalize_or_zero();
            let base = out.positions.len() as u32;
            out.positions.extend_from_slice(&[a, b, c]);
            out.normals.extend_from_slice(&[n, n, n]);
            out.indices.extend_from_slice(&[base, base + 1, base + 2]);
        }
        out
    }

    /// Recompute normals from the current positions: every vertex gets the
    /// average of its triangles' face normals. Flat-shaded meshes keep flat
    /// faces (their vertices are unshared, so the average is one triangle);
    /// smooth meshes stay smooth. Call after moving vertices.
    pub fn recompute_normals(&mut self) {
        self.normals.clear();
        self.normals.resize(self.positions.len(), Vec3::ZERO);
        for tri in self.indices.chunks_exact(3) {
            let a = self.positions[tri[0] as usize];
            let b = self.positions[tri[1] as usize];
            let c = self.positions[tri[2] as usize];
            let n = (b - a).cross(c - a); // area-weighted
            for &i in tri {
                self.normals[i as usize] += n;
            }
        }
        for n in &mut self.normals {
            *n = n.normalize_or_zero();
        }
    }

    fn quad(&mut self, a: u32, b: u32, c: u32, d: u32) {
        self.indices.extend_from_slice(&[a, b, c, a, c, d]);
    }
}

/// A rectangular opening (door or window) cut through a wall, in the wall's
/// local frame: `offset` is the distance from the wall's start along its
/// length to the opening's left edge, `bottom` the sill height above the
/// floor (0 for doors).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WallCutout {
    pub offset: f32,
    pub width: f32,
    pub bottom: f32,
    pub height: f32,
}

impl WallCutout {
    /// Standard door opening (0.9 × 2.1 m), horizontally centered on
    /// `center_x` and clamped inside the wall.
    pub fn door(center_x: f32, wall_length: f32, wall_height: f32) -> Self {
        let width = 0.9f32.min(wall_length);
        Self {
            offset: (center_x - 0.5 * width).clamp(0.0, (wall_length - width).max(0.0)),
            width,
            bottom: 0.0,
            height: 2.1f32.min(wall_height),
        }
    }

    /// Standard window opening (1.2 × 1.2 m) centered on `(center_x,
    /// center_z)` and clamped inside the wall.
    pub fn window(center_x: f32, center_z: f32, wall_length: f32, wall_height: f32) -> Self {
        let width = 1.2f32.min(wall_length);
        let height = 1.2f32.min(wall_height);
        Self {
            offset: (center_x - 0.5 * width).clamp(0.0, (wall_length - width).max(0.0)),
            width,
            bottom: (center_z - 0.5 * height).clamp(0.0, (wall_height - height).max(0.0)),
            height,
        }
    }

    pub fn is_door(&self) -> bool {
        self.bottom <= 1e-4
    }
}

/// Empty point (plain axes): three thin boxes crossing at the origin, one
/// per axis, ±`size` long — reads as three lines in the viewport but stays
/// a regular pickable mesh.
pub fn empty_axes(size: f32) -> MeshData {
    let s = size.max(0.01);
    let t = (0.02 * s).max(0.004); // line half-thickness
    let mut m = MeshData::default();
    axis_box(&mut m, Vec3::new(-s, -t, -t), Vec3::new(s, t, t));
    axis_box(&mut m, Vec3::new(-t, -s, -t), Vec3::new(t, s, t));
    axis_box(&mut m, Vec3::new(-t, -t, -s), Vec3::new(t, t, s));
    m
}

// Light gizmo geometry (viewport markers, meters). Spot cones grow with the
// angle; the extents feed bounding radii / dimensions in lib.rs.
pub const POINT_GIZMO_EXTENT: f32 = 0.38;
pub const SUN_GIZMO_EXTENT: f32 = 0.85; // shaft reach along -Z
pub const SPOT_GIZMO_LENGTH: f32 = 0.7;

/// Base radius of the spot gizmo cone for a full cone angle in degrees.
pub fn spot_gizmo_radius(spot_angle_deg: f32) -> f32 {
    ((0.5 * spot_angle_deg.clamp(1.0, 160.0)).to_radians().tan() * SPOT_GIZMO_LENGTH)
        .clamp(0.02, 4.0)
}

/// Light gizmo: an emissive viewport marker (bulb + rays / cone). Like the
/// empty it is a regular pickable mesh; the renderer draws it emissive and
/// excludes it from shadow casting. Sun and Spot point along local -Z.
pub fn light_gizmo(kind: crate::LightKind, spot_angle_deg: f32) -> MeshData {
    let mut m = MeshData::default();
    let t = 0.015; // spoke half-thickness
    match kind {
        crate::LightKind::Point => {
            append(&mut m, uv_sphere(16, 8, 0.12));
            // six short rays leaving the bulb
            axis_box(&mut m, Vec3::new(0.18, -t, -t), Vec3::new(0.38, t, t));
            axis_box(&mut m, Vec3::new(-0.38, -t, -t), Vec3::new(-0.18, t, t));
            axis_box(&mut m, Vec3::new(-t, 0.18, -t), Vec3::new(t, 0.38, t));
            axis_box(&mut m, Vec3::new(-t, -0.38, -t), Vec3::new(t, -0.18, t));
            axis_box(&mut m, Vec3::new(-t, -t, 0.18), Vec3::new(t, t, 0.38));
            axis_box(&mut m, Vec3::new(-t, -t, -0.38), Vec3::new(t, t, -0.18));
        }
        crate::LightKind::Sun => {
            append(&mut m, uv_sphere(16, 8, 0.15));
            // rays sideways and up; a longer shaft marks the light direction
            axis_box(&mut m, Vec3::new(0.22, -t, -t), Vec3::new(0.45, t, t));
            axis_box(&mut m, Vec3::new(-0.45, -t, -t), Vec3::new(-0.22, t, t));
            axis_box(&mut m, Vec3::new(-t, 0.22, -t), Vec3::new(t, 0.45, t));
            axis_box(&mut m, Vec3::new(-t, -0.45, -t), Vec3::new(t, -0.22, t));
            axis_box(&mut m, Vec3::new(-t, -t, 0.22), Vec3::new(t, t, 0.45));
            let s = 0.025;
            axis_box(
                &mut m,
                Vec3::new(-s, -s, -SUN_GIZMO_EXTENT),
                Vec3::new(s, s, -0.2),
            );
        }
        crate::LightKind::Spot => {
            append(&mut m, uv_sphere(16, 8, 0.1));
            // open cone: apex at the origin, spreading along -Z
            let segments = 24;
            let r = spot_gizmo_radius(spot_angle_deg);
            let l = SPOT_GIZMO_LENGTH;
            let apex = Vec3::ZERO;
            let ring: Vec<Vec3> = (0..segments)
                .map(|i| {
                    let a = TAU * i as f32 / segments as f32;
                    Vec3::new(r * a.cos(), r * a.sin(), -l)
                })
                .collect();
            // per-face vertices so the cone stays flat-shaded like the rest
            for i in 0..segments {
                let p0 = ring[i];
                let p1 = ring[(i + 1) % segments];
                let n = (p0 - apex).cross(p1 - apex).normalize_or_zero();
                let v = m.positions.len() as u32;
                m.positions.extend_from_slice(&[apex, p0, p1]);
                m.normals.extend_from_slice(&[n, n, n]);
                m.indices.extend_from_slice(&[v, v + 1, v + 2]);
            }
            // base cap so the cone reads as a solid lamp head from below
            let center = Vec3::new(0.0, 0.0, -l);
            let down = -Vec3::Z;
            let v0 = m.positions.len() as u32;
            m.positions.push(center);
            m.normals.push(down);
            for p in &ring {
                m.positions.push(*p);
                m.normals.push(down);
            }
            for i in 0..segments as u32 {
                let a = v0 + 1 + i;
                let b = v0 + 1 + (i + 1) % segments as u32;
                m.indices.extend_from_slice(&[v0, b, a]);
            }
        }
    }
    m
}

/// Append `other` onto `m`, remapping its indices.
fn append(m: &mut MeshData, other: MeshData) {
    let base = m.positions.len() as u32;
    m.positions.extend(other.positions);
    m.normals.extend(other.normals);
    m.indices.extend(other.indices.iter().map(|i| i + base));
}

/// Axis-aligned box between `min` and `max` as six faces.
fn axis_box(m: &mut MeshData, min: Vec3, max: Vec3) {
    let v = Vec3::new;
    let (a, b) = (min, max);
    // +X / -X
    face(m, [v(b.x, a.y, a.z), v(b.x, b.y, a.z), v(b.x, b.y, b.z), v(b.x, a.y, b.z)], Vec3::X);
    face(m, [v(a.x, b.y, a.z), v(a.x, a.y, a.z), v(a.x, a.y, b.z), v(a.x, b.y, b.z)], -Vec3::X);
    // +Y / -Y
    face(m, [v(b.x, b.y, a.z), v(a.x, b.y, a.z), v(a.x, b.y, b.z), v(b.x, b.y, b.z)], Vec3::Y);
    face(m, [v(a.x, a.y, a.z), v(b.x, a.y, a.z), v(b.x, a.y, b.z), v(a.x, a.y, b.z)], -Vec3::Y);
    // +Z / -Z
    face(m, [v(a.x, a.y, b.z), v(b.x, a.y, b.z), v(b.x, b.y, b.z), v(a.x, b.y, b.z)], Vec3::Z);
    face(m, [v(a.x, b.y, a.z), v(b.x, b.y, a.z), v(b.x, a.y, a.z), v(a.x, a.y, a.z)], -Vec3::Z);
}

/// One quad with per-face vertices; `corners` counter-clockwise seen from
/// the normal side.
fn face(m: &mut MeshData, corners: [Vec3; 4], n: Vec3) {
    let base = m.positions.len() as u32;
    m.positions.extend_from_slice(&corners);
    m.normals.extend_from_slice(&[n; 4]);
    m.indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// Wall segment: runs along +X from the origin to `length`, straddling the
/// X axis in Y (`thickness`), floor at z = 0, top at z = `height`. Cutouts
/// are rectangular holes through the thickness (doors, windows); a wall
/// whose cutouts cover it completely falls back to the solid shape so the
/// mesh never comes out empty.
pub fn wall(length: f32, height: f32, thickness: f32, cutouts: &[WallCutout]) -> MeshData {
    let length = length.max(0.01);
    let height = height.max(0.01);
    let ht = 0.5 * thickness.max(0.002);

    // clamp the openings into the wall rectangle; drop degenerate ones
    let holes: Vec<(f32, f32, f32, f32)> = cutouts
        .iter()
        .map(|c| {
            (
                c.offset.clamp(0.0, length),
                (c.offset + c.width).clamp(0.0, length),
                c.bottom.clamp(0.0, height),
                (c.bottom + c.height).clamp(0.0, height),
            )
        })
        .filter(|(x0, x1, z0, z1)| x1 - x0 > 1e-4 && z1 - z0 > 1e-4)
        .collect();

    // grid decomposition: cell edges at the wall and cutout boundaries
    let mut xs = vec![0.0, length];
    let mut zs = vec![0.0, height];
    for &(x0, x1, z0, z1) in &holes {
        xs.extend_from_slice(&[x0, x1]);
        zs.extend_from_slice(&[z0, z1]);
    }
    for edges in [&mut xs, &mut zs] {
        edges.sort_by(f32::total_cmp);
        edges.dedup_by(|a, b| (*a - *b).abs() < 1e-5);
    }

    let (nx, nz) = (xs.len() - 1, zs.len() - 1);
    // outside the wall counts as open, so the rim faces come out of the
    // same solid/open-boundary rule as the jambs inside the holes
    let open = |i: isize, j: isize| -> bool {
        if i < 0 || j < 0 || i >= nx as isize || j >= nz as isize {
            return true;
        }
        let cx = 0.5 * (xs[i as usize] + xs[i as usize + 1]);
        let cz = 0.5 * (zs[j as usize] + zs[j as usize + 1]);
        holes
            .iter()
            .any(|&(x0, x1, z0, z1)| cx > x0 && cx < x1 && cz > z0 && cz < z1)
    };

    let mut m = MeshData::default();
    let v = Vec3::new;
    for i in 0..nx {
        for j in 0..nz {
            if open(i as isize, j as isize) {
                continue;
            }
            let (x0, x1, z0, z1) = (xs[i], xs[i + 1], zs[j], zs[j + 1]);
            // the two big wall faces of this solid cell
            face(&mut m, [v(x0, ht, z0), v(x0, ht, z1), v(x1, ht, z1), v(x1, ht, z0)], Vec3::Y);
            face(&mut m, [v(x0, -ht, z0), v(x1, -ht, z0), v(x1, -ht, z1), v(x0, -ht, z1)], Vec3::NEG_Y);
            // faces through the thickness wherever the neighbor is open
            if open(i as isize - 1, j as isize) {
                face(&mut m, [v(x0, -ht, z0), v(x0, -ht, z1), v(x0, ht, z1), v(x0, ht, z0)], Vec3::NEG_X);
            }
            if open(i as isize + 1, j as isize) {
                face(&mut m, [v(x1, -ht, z0), v(x1, ht, z0), v(x1, ht, z1), v(x1, -ht, z1)], Vec3::X);
            }
            if open(i as isize, j as isize - 1) {
                face(&mut m, [v(x0, -ht, z0), v(x0, ht, z0), v(x1, ht, z0), v(x1, -ht, z0)], Vec3::NEG_Z);
            }
            if open(i as isize, j as isize + 1) {
                face(&mut m, [v(x0, -ht, z1), v(x1, -ht, z1), v(x1, ht, z1), v(x0, ht, z1)], Vec3::Z);
            }
        }
    }
    if m.indices.is_empty() {
        return wall(length, height, thickness, &[]);
    }
    m
}

pub fn plane(size: f32) -> MeshData {
    let h = 0.5 * size;
    let mut m = MeshData::default();
    m.positions = vec![
        Vec3::new(-h, -h, 0.0),
        Vec3::new(h, -h, 0.0),
        Vec3::new(h, h, 0.0),
        Vec3::new(-h, h, 0.0),
    ];
    m.normals = vec![Vec3::Z; 4];
    m.quad(0, 1, 2, 3);
    m
}

pub fn cube(size: f32) -> MeshData {
    box_mesh(Vec3::splat(0.5 * size), Vec3::ZERO)
}

/// Floor slab: centered on the origin in XY, spanning z ∈ [0, thickness].
pub fn floor(width: f32, depth: f32, thickness: f32) -> MeshData {
    box_mesh(
        Vec3::new(0.5 * width, 0.5 * depth, 0.5 * thickness),
        Vec3::new(0.0, 0.0, 0.5 * thickness),
    )
}

/// Floor slab following a footprint polygon (simple, convex or concave, any
/// winding), spanning z ∈ [0, thickness]: flat caps plus perimeter sides.
pub fn floor_polygon(outline: &[Vec2], thickness: f32) -> MeshData {
    let mut m = MeshData::default();
    // work in CCW order so cap triangles face +Z and edge normals point out
    let doubled_area: f32 = outline
        .iter()
        .zip(outline.iter().cycle().skip(1))
        .take(outline.len())
        .map(|(a, b)| a.perp_dot(*b))
        .sum();
    let ccw: Vec<Vec2> = if doubled_area < 0.0 {
        outline.iter().rev().copied().collect()
    } else {
        outline.to_vec()
    };
    let tris = triangulate(&ccw);

    // top & bottom caps
    for (z, normal) in [(thickness, Vec3::Z), (0.0, Vec3::NEG_Z)] {
        let base = m.positions.len() as u32;
        for p in &ccw {
            m.positions.push(Vec3::new(p.x, p.y, z));
            m.normals.push(normal);
        }
        for &[a, b, c] in &tris {
            if normal == Vec3::Z {
                m.indices.extend_from_slice(&[base + a, base + b, base + c]);
            } else {
                m.indices.extend_from_slice(&[base + a, base + c, base + b]);
            }
        }
    }

    // perimeter sides; for a CCW polygon the outward normal of edge a→b is
    // its direction rotated -90°
    for i in 0..ccw.len() {
        let a = ccw[i];
        let b = ccw[(i + 1) % ccw.len()];
        let n = Vec3::new(b.y - a.y, a.x - b.x, 0.0).normalize_or_zero();
        let base = m.positions.len() as u32;
        m.positions.extend_from_slice(&[
            Vec3::new(a.x, a.y, 0.0),
            Vec3::new(b.x, b.y, 0.0),
            Vec3::new(b.x, b.y, thickness),
            Vec3::new(a.x, a.y, thickness),
        ]);
        m.normals.extend_from_slice(&[n, n, n, n]);
        m.quad(base, base + 1, base + 2, base + 3);
    }
    m
}

/// Roof solid (see `Primitive::Roof`): footprint `width` × `depth` plus
/// `overhang` on all four sides, centered on the origin in XY, base plane at
/// z = 0, rising to z = `height`. Every kind is a watertight solid with a
/// flat bottom cap, so the roof sits on the wall tops as a closed lid. For
/// the oriented kinds the ridge (shed: the high eave) runs along X when
/// `ridge_x`, else Y.
pub fn roof(
    kind: crate::RoofKind,
    width: f32,
    depth: f32,
    height: f32,
    overhang: f32,
    ridge_x: bool,
) -> MeshData {
    use crate::RoofKind;
    let w = (width + 2.0 * overhang.max(0.0)).max(0.02);
    let d = (depth + 2.0 * overhang.max(0.0)).max(0.02);
    let h = height.max(0.02);
    // the oriented kinds are generated with the ridge along X in a
    // length × span frame, then rotated into place when it runs along Y
    let (len, span) = if ridge_x { (w, d) } else { (d, w) };
    let mut m = match kind {
        RoofKind::Flat => {
            return box_mesh(
                Vec3::new(0.5 * w, 0.5 * d, 0.5 * h),
                Vec3::new(0.0, 0.0, 0.5 * h),
            )
        }
        RoofKind::Point => return roof_point(w, d, h),
        RoofKind::Mansard => return roof_mansard(w, d, h),
        RoofKind::Shed => roof_shed(len, span, h),
        RoofKind::Gable => roof_gable(len, span, h),
        RoofKind::Hip => roof_hip(len, span, h),
        RoofKind::Gambrel => roof_gambrel(len, span, h),
    };
    if !ridge_x {
        // rotate +90° around Z: (x, y) → (-y, x)
        for p in &mut m.positions {
            *p = Vec3::new(-p.y, p.x, p.z);
        }
        for n in &mut m.normals {
            *n = Vec3::new(-n.y, n.x, n.z);
        }
    }
    m
}

/// One flat convex polygon face with per-face vertices; `pts` counter-
/// clockwise seen from outside (the normal comes from the winding).
fn poly_face(m: &mut MeshData, pts: &[Vec3]) {
    let mut n = Vec3::ZERO;
    for i in 0..pts.len() {
        n += pts[i].cross(pts[(i + 1) % pts.len()]); // 2× area vector
    }
    let n = n.normalize_or_zero();
    let base = m.positions.len() as u32;
    for p in pts {
        m.positions.push(*p);
        m.normals.push(n);
    }
    for i in 1..pts.len() as u32 - 1 {
        m.indices.extend_from_slice(&[base, base + i, base + i + 1]);
    }
}

/// The four base corners (CCW from above) and the bottom cap they close.
fn roof_base(m: &mut MeshData, hx: f32, hy: f32) -> [Vec3; 4] {
    let corners = [
        Vec3::new(-hx, -hy, 0.0),
        Vec3::new(hx, -hy, 0.0),
        Vec3::new(hx, hy, 0.0),
        Vec3::new(-hx, hy, 0.0),
    ];
    poly_face(m, &[corners[0], corners[3], corners[2], corners[1]]);
    corners
}

/// Pyramid: four slopes from the eaves to an apex over the center.
fn roof_point(w: f32, d: f32, h: f32) -> MeshData {
    let mut m = MeshData::default();
    let base = roof_base(&mut m, 0.5 * w, 0.5 * d);
    let apex = Vec3::new(0.0, 0.0, h);
    for i in 0..4 {
        poly_face(&mut m, &[base[i], base[(i + 1) % 4], apex]);
    }
    m
}

/// Wedge: eave at y = -span/2 on the base plane, high eave at y = +span/2.
fn roof_shed(len: f32, span: f32, h: f32) -> MeshData {
    let (hl, hs) = (0.5 * len, 0.5 * span);
    let mut m = MeshData::default();
    let [a, b, c, e] = roof_base(&mut m, hl, hs);
    let f = Vec3::new(hl, hs, h); // high eave corners
    let g = Vec3::new(-hl, hs, h);
    poly_face(&mut m, &[a, b, f, g]); // the slope
    poly_face(&mut m, &[c, e, g, f]); // vertical high side
    poly_face(&mut m, &[b, c, f]); // triangular ends
    poly_face(&mut m, &[e, a, g]);
    m
}

/// Triangular prism: two slopes to a full-length ridge, vertical gable ends.
fn roof_gable(len: f32, span: f32, h: f32) -> MeshData {
    let (hl, hs) = (0.5 * len, 0.5 * span);
    let mut m = MeshData::default();
    let [a, b, c, e] = roof_base(&mut m, hl, hs);
    let r0 = Vec3::new(-hl, 0.0, h); // ridge ends
    let r1 = Vec3::new(hl, 0.0, h);
    poly_face(&mut m, &[a, b, r1, r0]); // slopes
    poly_face(&mut m, &[c, e, r0, r1]);
    poly_face(&mut m, &[b, c, r1]); // gable ends
    poly_face(&mut m, &[e, a, r0]);
    m
}

/// Hip: the ridge is pulled in from each end by the half-span so all four
/// slopes share the pitch; short footprints degenerate into a pyramid.
fn roof_hip(len: f32, span: f32, h: f32) -> MeshData {
    let (hl, hs) = (0.5 * len, 0.5 * span);
    let rx = hl - hs;
    if rx <= 1e-5 {
        return roof_point(len, span, h);
    }
    let mut m = MeshData::default();
    let [a, b, c, e] = roof_base(&mut m, hl, hs);
    let r0 = Vec3::new(-rx, 0.0, h);
    let r1 = Vec3::new(rx, 0.0, h);
    poly_face(&mut m, &[a, b, r1, r0]); // long slopes (trapezoids)
    poly_face(&mut m, &[c, e, r0, r1]);
    poly_face(&mut m, &[b, c, r1]); // hip ends
    poly_face(&mut m, &[e, a, r0]);
    m
}

/// Gambrel: prism over a pentagon section — steep lower slopes breaking
/// into shallow upper ones — with vertical gable-end caps.
fn roof_gambrel(len: f32, span: f32, h: f32) -> MeshData {
    let (hl, hs) = (0.5 * len, 0.5 * span);
    let (by, bz) = (0.5 * hs, 0.75 * h); // the pitch break
    let v = Vec3::new;
    let mut m = MeshData::default();
    roof_base(&mut m, hl, hs);
    // slope quads, walking the section from eave to eave over the ridge
    for (y0, z0, y1, z1) in [
        (-hs, 0.0, -by, bz),
        (-by, bz, 0.0, h),
        (0.0, h, by, bz),
        (by, bz, hs, 0.0),
    ] {
        poly_face(
            &mut m,
            &[v(-hl, y0, z0), v(hl, y0, z0), v(hl, y1, z1), v(-hl, y1, z1)],
        );
    }
    // pentagon end caps (this order faces -X; reversed for +X)
    let cap = |x: f32| {
        [v(x, -hs, 0.0), v(x, -by, bz), v(x, 0.0, h), v(x, by, bz), v(x, hs, 0.0)]
    };
    poly_face(&mut m, &cap(-hl));
    let mut positive = cap(hl);
    positive.reverse();
    poly_face(&mut m, &positive);
    m
}

/// Mansard: two stacked truncated pyramids — steep below the pitch break,
/// shallow above it — closed by a small flat top.
fn roof_mansard(w: f32, d: f32, h: f32) -> MeshData {
    let (hx, hy) = (0.5 * w, 0.5 * d);
    let short = hx.min(hy);
    let ring = |inset: f32, z: f32| {
        [
            Vec3::new(-(hx - inset), -(hy - inset), z),
            Vec3::new(hx - inset, -(hy - inset), z),
            Vec3::new(hx - inset, hy - inset, z),
            Vec3::new(-(hx - inset), hy - inset, z),
        ]
    };
    let mut m = MeshData::default();
    roof_base(&mut m, hx, hy);
    let rings = [
        ring(0.0, 0.0),
        ring(0.3 * short, 0.75 * h), // steep lower stage
        ring(0.6 * short, h),        // shallow upper stage
    ];
    for stage in rings.windows(2) {
        let (lo, hi) = (stage[0], stage[1]);
        for i in 0..4 {
            let j = (i + 1) % 4;
            poly_face(&mut m, &[lo[i], lo[j], hi[j], hi[i]]);
        }
    }
    poly_face(&mut m, &rings[2]); // flat top
    m
}

/// Ear-clipping triangulation of a simple CCW polygon; returns index triples
/// into `points`. Degenerate input yields a partial (possibly empty) result
/// rather than looping forever.
pub fn triangulate(points: &[Vec2]) -> Vec<[u32; 3]> {
    let mut idx: Vec<u32> = (0..points.len() as u32).collect();
    let mut tris = Vec::new();
    let cross = |o: Vec2, a: Vec2, b: Vec2| (a - o).perp_dot(b - o);
    while idx.len() > 3 {
        let m = idx.len();
        let mut clipped = false;
        for i in 0..m {
            let (pi, ci, ni) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            let (p, c, n) =
                (points[pi as usize], points[ci as usize], points[ni as usize]);
            if cross(p, c, n) <= 1e-9 {
                continue; // reflex or collinear corner: not an ear
            }
            // an ear must not contain any other polygon vertex
            let blocked = idx.iter().any(|&j| {
                if j == pi || j == ci || j == ni {
                    return false;
                }
                let q = points[j as usize];
                cross(p, c, q) >= -1e-9
                    && cross(c, n, q) >= -1e-9
                    && cross(n, p, q) >= -1e-9
            });
            if blocked {
                continue;
            }
            tris.push([pi, ci, ni]);
            idx.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            return tris; // numerically degenerate: keep what we have
        }
    }
    if idx.len() == 3 {
        tris.push([idx[0], idx[1], idx[2]]);
    }
    tris
}

/// Axis-aligned box with the given half-extents, centered on `center`.
fn box_mesh(half: Vec3, center: Vec3) -> MeshData {
    let mut m = MeshData::default();
    // (normal, u, v) per face, CCW seen from outside
    let faces = [
        (Vec3::X, Vec3::Y, Vec3::Z),
        (Vec3::NEG_X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::Z, Vec3::X),
        (Vec3::NEG_Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (Vec3::NEG_Z, Vec3::Y, Vec3::X),
    ];
    for (n, u, v) in faces {
        let base = m.positions.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            m.positions.push((n + u * su + v * sv) * half + center);
            m.normals.push(n);
        }
        m.quad(base, base + 1, base + 2, base + 3);
    }
    m
}

pub fn uv_sphere(segments: u32, rings: u32, radius: f32) -> MeshData {
    let segments = segments.max(3);
    let rings = rings.max(2);
    let mut m = MeshData::default();

    // poles on the Z axis, like Blender
    m.positions.push(Vec3::new(0.0, 0.0, radius));
    m.normals.push(Vec3::Z);
    for ring in 1..rings {
        let phi = PI * ring as f32 / rings as f32;
        let (sp, cp) = (phi.sin(), phi.cos());
        for seg in 0..segments {
            let theta = TAU * seg as f32 / segments as f32;
            let n = Vec3::new(sp * theta.cos(), sp * theta.sin(), cp);
            m.positions.push(n * radius);
            m.normals.push(n);
        }
    }
    m.positions.push(Vec3::new(0.0, 0.0, -radius));
    m.normals.push(Vec3::NEG_Z);

    let ring_start = |ring: u32| 1 + (ring - 1) * segments;
    let bottom = m.positions.len() as u32 - 1;

    // top fan
    for i in 0..segments {
        let a = ring_start(1) + i;
        let b = ring_start(1) + (i + 1) % segments;
        m.indices.extend_from_slice(&[0, a, b]);
    }
    // quads between rings
    for ring in 1..rings - 1 {
        for i in 0..segments {
            let i1 = (i + 1) % segments;
            let a = ring_start(ring) + i;
            let b = ring_start(ring) + i1;
            let c = ring_start(ring + 1) + i1;
            let d = ring_start(ring + 1) + i;
            m.quad(a, d, c, b);
        }
    }
    // bottom fan
    for i in 0..segments {
        let a = ring_start(rings - 1) + i;
        let b = ring_start(rings - 1) + (i + 1) % segments;
        m.indices.extend_from_slice(&[bottom, b, a]);
    }
    m
}

pub fn ico_sphere(subdivisions: u32, radius: f32) -> MeshData {
    let t = (1.0 + 5.0f32.sqrt()) / 2.0;
    let base_positions = [
        Vec3::new(-1.0, t, 0.0),
        Vec3::new(1.0, t, 0.0),
        Vec3::new(-1.0, -t, 0.0),
        Vec3::new(1.0, -t, 0.0),
        Vec3::new(0.0, -1.0, t),
        Vec3::new(0.0, 1.0, t),
        Vec3::new(0.0, -1.0, -t),
        Vec3::new(0.0, 1.0, -t),
        Vec3::new(t, 0.0, -1.0),
        Vec3::new(t, 0.0, 1.0),
        Vec3::new(-t, 0.0, -1.0),
        Vec3::new(-t, 0.0, 1.0),
    ];
    #[rustfmt::skip]
    let mut faces: Vec<[u32; 3]> = vec![
        [0, 11, 5], [0, 5, 1], [0, 1, 7], [0, 7, 10], [0, 10, 11],
        [1, 5, 9], [5, 11, 4], [11, 10, 2], [10, 7, 6], [7, 1, 8],
        [3, 9, 4], [3, 4, 2], [3, 2, 6], [3, 6, 8], [3, 8, 9],
        [4, 9, 5], [2, 4, 11], [6, 2, 10], [8, 6, 7], [9, 8, 1],
    ];

    let mut positions: Vec<Vec3> = base_positions.iter().map(|p| p.normalize()).collect();

    for _ in 0..subdivisions.min(6) {
        let mut midpoints: HashMap<(u32, u32), u32> = HashMap::new();
        let mut next = Vec::with_capacity(faces.len() * 4);
        let mut midpoint = |a: u32, b: u32, positions: &mut Vec<Vec3>| -> u32 {
            let key = (a.min(b), a.max(b));
            *midpoints.entry(key).or_insert_with(|| {
                let p = ((positions[a as usize] + positions[b as usize]) * 0.5).normalize();
                positions.push(p);
                positions.len() as u32 - 1
            })
        };
        for [a, b, c] in faces {
            let ab = midpoint(a, b, &mut positions);
            let bc = midpoint(b, c, &mut positions);
            let ca = midpoint(c, a, &mut positions);
            next.extend_from_slice(&[[a, ab, ca], [b, bc, ab], [c, ca, bc], [ab, bc, ca]]);
        }
        faces = next;
    }

    MeshData {
        normals: positions.clone(),
        positions: positions.into_iter().map(|p| p * radius).collect(),
        indices: faces.into_flattened(),
        seams: Vec::new(),
    }
}

/// Shared generator for cylinders (radius_top == radius_bottom), cones
/// (radius_top == 0) and frustums.
pub fn frustum(vertices: u32, radius_bottom: f32, radius_top: f32, depth: f32) -> MeshData {
    let n = vertices.max(3);
    let h = 0.5 * depth;
    let mut m = MeshData::default();
    let dir = |i: u32| {
        let theta = TAU * i as f32 / n as f32;
        (theta.cos(), theta.sin())
    };
    // side normal: radial component scaled by height, z by radius difference
    let side_normal = |c: f32, s: f32| Vec3::new(c * depth, s * depth, radius_bottom - radius_top).normalize();

    let apex = radius_top <= f32::EPSILON;

    // bottom side ring
    let bottom_ring = m.positions.len() as u32;
    for i in 0..n {
        let (c, s) = dir(i);
        m.positions.push(Vec3::new(c * radius_bottom, s * radius_bottom, -h));
        m.normals.push(side_normal(c, s));
    }

    if apex {
        // one apex vertex per segment so flat and smooth both look right
        for i in 0..n {
            let i1 = (i + 1) % n;
            let theta_mid = TAU * (i as f32 + 0.5) / n as f32;
            let apex_index = m.positions.len() as u32;
            m.positions.push(Vec3::new(0.0, 0.0, h));
            m.normals.push(side_normal(theta_mid.cos(), theta_mid.sin()));
            m.indices
                .extend_from_slice(&[bottom_ring + i, bottom_ring + i1, apex_index]);
        }
    } else {
        let top_ring = m.positions.len() as u32;
        for i in 0..n {
            let (c, s) = dir(i);
            m.positions.push(Vec3::new(c * radius_top, s * radius_top, h));
            m.normals.push(side_normal(c, s));
        }
        for i in 0..n {
            let i1 = (i + 1) % n;
            m.quad(bottom_ring + i, bottom_ring + i1, top_ring + i1, top_ring + i);
        }
    }

    // caps (flat, own vertices)
    let mut cap = |z: f32, radius: f32, normal: Vec3| {
        if radius <= f32::EPSILON {
            return;
        }
        let center = m.positions.len() as u32;
        m.positions.push(Vec3::new(0.0, 0.0, z));
        m.normals.push(normal);
        let ring = m.positions.len() as u32;
        for i in 0..n {
            let (c, s) = dir(i);
            m.positions.push(Vec3::new(c * radius, s * radius, z));
            m.normals.push(normal);
        }
        for i in 0..n {
            let i1 = (i + 1) % n;
            if normal.z > 0.0 {
                m.indices.extend_from_slice(&[center, ring + i, ring + i1]);
            } else {
                m.indices.extend_from_slice(&[center, ring + i1, ring + i]);
            }
        }
    };
    cap(-h, radius_bottom, Vec3::NEG_Z);
    cap(h, radius_top, Vec3::Z);

    m
}

pub fn torus(major_segments: u32, minor_segments: u32, major_radius: f32, minor_radius: f32) -> MeshData {
    let maj = major_segments.max(3);
    let min = minor_segments.max(3);
    let mut m = MeshData::default();
    for i in 0..maj {
        let u = TAU * i as f32 / maj as f32;
        let (su, cu) = u.sin_cos();
        for j in 0..min {
            let v = TAU * j as f32 / min as f32;
            let (sv, cv) = v.sin_cos();
            let ring = major_radius + minor_radius * cv;
            m.positions.push(Vec3::new(ring * cu, ring * su, minor_radius * sv));
            m.normals.push(Vec3::new(cv * cu, cv * su, sv));
        }
    }
    let at = |i: u32, j: u32| (i % maj) * min + (j % min);
    for i in 0..maj {
        for j in 0..min {
            m.quad(at(i, j), at(i + 1, j), at(i + 1, j + 1), at(i, j + 1));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(m: &MeshData) {
        assert_eq!(m.positions.len(), m.normals.len());
        assert!(m.indices.len() % 3 == 0);
        assert!(!m.indices.is_empty());
        for &i in &m.indices {
            assert!((i as usize) < m.positions.len(), "index out of range");
        }
        for n in &m.normals {
            assert!((n.length() - 1.0).abs() < 1e-3, "normal not unit length: {n:?}");
        }
    }

    #[test]
    fn generators_produce_valid_meshes() {
        validate(&plane(2.0));
        validate(&cube(2.0));
        validate(&uv_sphere(32, 16, 1.0));
        validate(&ico_sphere(2, 1.0));
        validate(&frustum(32, 1.0, 1.0, 2.0)); // cylinder
        validate(&frustum(32, 1.0, 0.0, 2.0)); // cone
        validate(&torus(48, 12, 1.0, 0.25));
        validate(&cube(2.0).into_flat());
        validate(&uv_sphere(16, 8, 1.0).into_flat());
        validate(&wall(4.0, 2.5, 0.2, &[]));
        validate(&wall(
            4.0,
            2.5,
            0.2,
            &[WallCutout::door(1.0, 4.0, 2.5), WallCutout::window(3.0, 1.5, 4.0, 2.5)],
        ));
    }

    #[test]
    fn roofs_are_watertight_solids_with_the_right_extents() {
        use crate::RoofKind;
        for kind in RoofKind::ALL {
            for ridge_x in [true, false] {
                let m = roof(kind, 4.0, 3.0, 1.2, 0.3, ridge_x);
                validate(&m);

                // footprint + overhang all around, base at z = 0, top at h
                let e = m.extents();
                assert!((e.x - 4.6).abs() < 1e-4, "{kind:?} extents {e:?}");
                assert!((e.y - 3.6).abs() < 1e-4, "{kind:?} extents {e:?}");
                assert!((e.z - 1.2).abs() < 1e-4, "{kind:?} extents {e:?}");
                let min_z = m.positions.iter().map(|p| p.z).fold(f32::INFINITY, f32::min);
                assert!(min_z.abs() < 1e-5, "{kind:?} must stand on z = 0");

                // closed 2-manifold: every welded edge is walked once in
                // each direction (consistent winding, no open boundary)
                let key = |p: Vec3| {
                    (
                        (p.x * 1e4).round() as i64,
                        (p.y * 1e4).round() as i64,
                        (p.z * 1e4).round() as i64,
                    )
                };
                let mut ids: HashMap<(i64, i64, i64), usize> = HashMap::new();
                let mut edges: HashMap<(usize, usize), i32> = HashMap::new();
                for tri in m.indices.chunks_exact(3) {
                    let welded: Vec<usize> = tri
                        .iter()
                        .map(|&i| {
                            let next = ids.len();
                            *ids.entry(key(m.positions[i as usize])).or_insert(next)
                        })
                        .collect();
                    for i in 0..3 {
                        let (a, b) = (welded[i], welded[(i + 1) % 3]);
                        assert_ne!(a, b, "{kind:?} has a degenerate edge");
                        let sign = if a < b { 1 } else { -1 };
                        *edges.entry((a.min(b), a.max(b))).or_insert(0) += sign;
                    }
                }
                for ((a, b), count) in edges {
                    assert_eq!(count, 0, "{kind:?} edge {a}-{b} is unbalanced");
                }
            }
        }
    }

    #[test]
    fn roof_ridge_follows_the_requested_axis() {
        // gable, 4 × 3, no overhang: the ridge (z = h) spans the full length
        let ridge_of = |m: &MeshData| -> Vec<Vec3> {
            m.positions.iter().copied().filter(|p| (p.z - 1.0).abs() < 1e-5).collect()
        };
        let along_x = roof(crate::RoofKind::Gable, 4.0, 3.0, 1.0, 0.0, true);
        for p in ridge_of(&along_x) {
            assert!(p.y.abs() < 1e-5 && p.x.abs() > 1.9, "ridge point {p:?}");
        }
        // ridge along Y spans the 3 m depth: endpoints at y = ±1.5
        let along_y = roof(crate::RoofKind::Gable, 4.0, 3.0, 1.0, 0.0, false);
        for p in ridge_of(&along_y) {
            assert!(p.x.abs() < 1e-5 && (p.y.abs() - 1.5).abs() < 1e-4, "ridge point {p:?}");
        }
        // hip with ridge along X on a 6 × 2 footprint: pulled in by the
        // half-span so the end slopes share the side pitch
        let hip = roof(crate::RoofKind::Hip, 6.0, 2.0, 1.0, 0.0, true);
        let ridge = ridge_of(&hip);
        assert!(!ridge.is_empty());
        for p in &ridge {
            assert!(p.y.abs() < 1e-5 && (p.x.abs() - 2.0).abs() < 1e-4, "{p:?}");
        }
        // a square hip degenerates into a pyramid (single apex)
        let pyramid = roof(crate::RoofKind::Hip, 2.0, 2.0, 1.0, 0.0, true);
        for p in ridge_of(&pyramid) {
            assert!(p.x.abs() < 1e-5 && p.y.abs() < 1e-5, "{p:?}");
        }
    }

    #[test]
    fn wall_cutouts_open_real_holes() {
        // a point ray through the middle of the door opening must not cross
        // any triangle; through solid wall it must cross front and back
        let door = WallCutout::door(1.0, 4.0, 2.5);
        let m = wall(4.0, 2.5, 0.2, &[door]);

        let crossings = |x: f32, z: f32| -> usize {
            // count triangles whose XZ projection contains (x, z) — the wall
            // is a prism along Y, so the ±Y faces are the only ones with
            // nonzero XZ... use a y-directed segment against all triangles
            let (o, d) = (Vec3::new(x, -5.0, z), Vec3::new(0.0, 1.0, 0.0));
            m.indices
                .chunks_exact(3)
                .filter(|tri| {
                    let (a, b, c) = (
                        m.positions[tri[0] as usize],
                        m.positions[tri[1] as usize],
                        m.positions[tri[2] as usize],
                    );
                    // Möller–Trumbore
                    let (e1, e2) = (b - a, c - a);
                    let p = d.cross(e2);
                    let det = e1.dot(p);
                    if det.abs() < 1e-9 {
                        return false;
                    }
                    let t = o - a;
                    let u = t.dot(p) / det;
                    let q = t.cross(e1);
                    let vv = d.dot(q) / det;
                    u >= -1e-6 && vv >= -1e-6 && u + vv <= 1.0 + 1e-6 && e2.dot(q) / det > 0.0
                })
                .count()
        };
        assert_eq!(crossings(1.0, 1.0), 0, "ray through the door must be free");
        assert!(crossings(3.5, 1.0) >= 2, "solid wall must block the ray");
        assert!(crossings(1.0, 2.3) >= 2, "lintel above the door must block");

        // cutouts covering the whole wall fall back to the solid shape
        let all = WallCutout { offset: -1.0, width: 10.0, bottom: -1.0, height: 10.0 };
        let solid = wall(4.0, 2.5, 0.2, &[all]);
        assert_eq!(solid.indices.len(), wall(4.0, 2.5, 0.2, &[]).indices.len());
    }

    #[test]
    fn wall_cutout_constructors_clamp_into_the_wall() {
        let door = WallCutout::door(0.0, 4.0, 2.5); // centered past the start
        assert_eq!(door.offset, 0.0);
        assert!(door.is_door());
        let door = WallCutout::door(1.0, 0.5, 2.0); // wall shorter than a door
        assert!(door.width <= 0.5 && door.offset >= 0.0);
        let win = WallCutout::window(3.9, 2.4, 4.0, 2.5); // near the top corner
        assert!(win.offset + win.width <= 4.0 + 1e-6);
        assert!(win.bottom + win.height <= 2.5 + 1e-6);
        assert!(!win.is_door());
    }

    #[test]
    fn flat_shading_expands_vertices() {
        let smooth = uv_sphere(8, 4, 1.0);
        let flat = smooth.clone().into_flat();
        assert_eq!(flat.positions.len(), smooth.indices.len());
    }

    /// Flat normals (from winding) must agree with the analytic smooth
    /// normals, otherwise a generator has inverted winding somewhere.
    #[test]
    fn winding_matches_normals() {
        for mesh in [
            plane(2.0),
            cube(2.0),
            uv_sphere(16, 8, 1.0),
            ico_sphere(1, 1.0),
            frustum(16, 1.0, 1.0, 2.0),
            frustum(16, 1.0, 0.5, 2.0),
            frustum(16, 1.0, 0.0, 2.0),
            torus(24, 8, 1.0, 0.25),
            wall(4.0, 2.5, 0.2, &[]),
            wall(4.0, 2.5, 0.2, &[WallCutout::door(1.0, 4.0, 2.5), WallCutout::window(3.0, 1.5, 4.0, 2.5)]),
            roof(crate::RoofKind::Gable, 4.0, 3.0, 1.2, 0.3, false),
            roof(crate::RoofKind::Hip, 4.0, 3.0, 1.2, 0.3, true),
            roof(crate::RoofKind::Gambrel, 4.0, 3.0, 1.5, 0.3, false),
            roof(crate::RoofKind::Mansard, 4.0, 3.0, 1.5, 0.3, true),
        ] {
            for tri in mesh.indices.chunks_exact(3) {
                let a = mesh.positions[tri[0] as usize];
                let b = mesh.positions[tri[1] as usize];
                let c = mesh.positions[tri[2] as usize];
                let face = (b - a).cross(c - a);
                if face.length() < 1e-6 {
                    continue; // degenerate (e.g. pole caps)
                }
                let avg = (mesh.normals[tri[0] as usize]
                    + mesh.normals[tri[1] as usize]
                    + mesh.normals[tri[2] as usize])
                    / 3.0;
                assert!(
                    face.normalize().dot(avg.normalize_or_zero()) > 0.0,
                    "winding disagrees with normals for triangle {tri:?}"
                );
            }
        }
    }
}
