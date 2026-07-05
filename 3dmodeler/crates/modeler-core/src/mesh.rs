//! Mesh primitive generators. All primitives are Z-up and centered at the
//! origin, matching Blender's conventions (cylinder/cone axis along Z, plane
//! and torus in the XY plane).

use glam::Vec3;
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
}

impl MeshData {
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
    let h = 0.5 * size;
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
            m.positions.push((n + u * su + v * sv) * h);
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
