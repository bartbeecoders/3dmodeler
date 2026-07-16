//! Mesh booleans (CSG): union, subtract and intersect of two triangle
//! meshes. Both meshes must be expressed in ONE common space —
//! [`mesh_to_frame`] brings a mesh from one object's local frame into
//! another's.
//!
//! Prefer **boolmesh** (Manifold-style mesh boolean with a Morton BVH and
//! optional rayon parallelism) for closed manifold solids — typically
//! orders of magnitude faster than BSP on anything beyond a few hundred
//! triangles. Fall back to BSP-tree clipping (the csg.js algorithm) for
//! open / non-manifold inputs that boolmesh rejects. Inputs should be
//! closed (watertight) solids for well-defined results; open meshes still
//! clip via BSP, but the result can be open too.

use crate::mesh::MeshData;
use crate::Transform;
use glam::Vec3;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which boolean to apply (A = target mesh, B = tool mesh).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BooleanOp {
    /// A ∪ B: one solid covering both volumes.
    Union,
    /// A − B: carve B out of A.
    Subtract,
    /// A ∩ B: keep only the shared volume.
    Intersect,
}

impl BooleanOp {
    pub const ALL: [BooleanOp; 3] =
        [BooleanOp::Union, BooleanOp::Subtract, BooleanOp::Intersect];

    pub fn label(self) -> &'static str {
        match self {
            BooleanOp::Union => "Union",
            BooleanOp::Subtract => "Subtract",
            BooleanOp::Intersect => "Intersect",
        }
    }

    pub fn from_name(name: &str) -> Option<BooleanOp> {
        match name.trim().to_ascii_lowercase().as_str() {
            "union" | "add" | "merge" | "join" => Some(BooleanOp::Union),
            "subtract" | "sub" | "difference" | "cut" => Some(BooleanOp::Subtract),
            "intersect" | "intersection" => Some(BooleanOp::Intersect),
            _ => None,
        }
    }
}

/// Boolean of two triangle meshes in the same space.
pub fn mesh_boolean(a_mesh: &MeshData, b_mesh: &MeshData, op: BooleanOp) -> MeshData {
    if a_mesh.indices.is_empty() {
        return match op {
            BooleanOp::Union => b_mesh.clone(),
            BooleanOp::Subtract | BooleanOp::Intersect => MeshData::default(),
        };
    }
    if b_mesh.indices.is_empty() {
        return match op {
            BooleanOp::Union | BooleanOp::Subtract => a_mesh.clone(),
            BooleanOp::Intersect => MeshData::default(),
        };
    }

    // Disjoint AABBs: exact CSG without building trees or Manifolds.
    if let Some(fast) = disjoint_aabb_result(a_mesh, b_mesh, op) {
        return fast;
    }

    // Primary path: Manifold-style mesh boolean (boolmesh).
    if let Some(result) = try_boolmesh(a_mesh, b_mesh, op) {
        return result;
    }

    // Fallback: BSP clipping for open / non-manifold meshes.
    mesh_boolean_bsp(a_mesh, b_mesh, op)
}

/// Axis-aligned bounds of a mesh (min, max). None if empty.
fn mesh_aabb(mesh: &MeshData) -> Option<(Vec3, Vec3)> {
    if mesh.positions.is_empty() {
        return None;
    }
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for &p in &mesh.positions {
        min = min.min(p);
        max = max.max(p);
    }
    Some((min, max))
}

/// Exact results when the AABBs do not overlap (with a tiny padding so
/// coplanar-touching cases still go through the full engine).
fn disjoint_aabb_result(a: &MeshData, b: &MeshData, op: BooleanOp) -> Option<MeshData> {
    let (amin, amax) = mesh_aabb(a)?;
    let (bmin, bmax) = mesh_aabb(b)?;
    let pad = EPSILON;
    let overlap = amin.x <= bmax.x + pad
        && amax.x + pad >= bmin.x
        && amin.y <= bmax.y + pad
        && amax.y + pad >= bmin.y
        && amin.z <= bmax.z + pad
        && amax.z + pad >= bmin.z;
    if overlap {
        return None;
    }
    Some(match op {
        BooleanOp::Union => concat_meshes(a, b),
        BooleanOp::Subtract => a.clone(),
        BooleanOp::Intersect => MeshData::default(),
    })
}

fn concat_meshes(a: &MeshData, b: &MeshData) -> MeshData {
    let mut out = a.clone();
    let base = out.positions.len() as u32;
    out.positions.extend_from_slice(&b.positions);
    out.normals.extend_from_slice(&b.normals);
    out.indices
        .extend(b.indices.iter().map(|&i| i.saturating_add(base)));
    out
}

/// Weld co-located vertices so flat-shaded (per-face duplicated) meshes
/// become topologically manifold, then run boolmesh.
fn try_boolmesh(a_mesh: &MeshData, b_mesh: &MeshData, op: BooleanOp) -> Option<MeshData> {
    use boolmesh::prelude::{compute_boolean, Manifold, OpType};

    let (a_pos, a_idx) = weld_positions(a_mesh);
    let (b_pos, b_idx) = weld_positions(b_mesh);
    let a = Manifold::new(&a_pos, &a_idx).ok()?;
    let b = Manifold::new(&b_pos, &b_idx).ok()?;
    let op = match op {
        BooleanOp::Union => OpType::Add,
        BooleanOp::Subtract => OpType::Subtract,
        BooleanOp::Intersect => OpType::Intersect,
    };
    match compute_boolean(&a, &b, op) {
        Ok(m) => Some(manifold_to_mesh(m)),
        // Empty solid (e.g. A entirely inside B for subtract/intersect).
        Err(msg) if msg.contains("empty") => Some(MeshData::default()),
        Err(_) => None,
    }
}

/// Merge vertices that share the same position (within quantisation),
/// producing flat f64 positions + usize indices for boolmesh.
fn weld_positions(mesh: &MeshData) -> (Vec<f64>, Vec<usize>) {
    // Quantise so floating noise from mesh_to_frame still welds; 1e-5 m
    // matches our BSP EPSILON scale.
    const Q: f32 = 1e5;
    let mut map: HashMap<(i32, i32, i32), usize> =
        HashMap::with_capacity(mesh.positions.len());
    let mut pos = Vec::with_capacity(mesh.positions.len() * 3);
    let mut indices = Vec::with_capacity(mesh.indices.len());
    for &i in &mesh.indices {
        let p = mesh.positions[i as usize];
        let key = (
            (p.x * Q).round() as i32,
            (p.y * Q).round() as i32,
            (p.z * Q).round() as i32,
        );
        let n = map.len();
        let id = *map.entry(key).or_insert_with(|| {
            pos.extend([p.x as f64, p.y as f64, p.z as f64]);
            n
        });
        indices.push(id);
    }
    (pos, indices)
}

fn manifold_to_mesh(m: boolmesh::prelude::Manifold) -> MeshData {
    let mut out = MeshData::default();
    out.positions.reserve(m.ps.len());
    for p in &m.ps {
        out.positions
            .push(Vec3::new(p.x as f32, p.y as f32, p.z as f32));
    }
    if m.vert_normals.len() == m.ps.len() {
        out.normals.reserve(m.vert_normals.len());
        for n in &m.vert_normals {
            let v = Vec3::new(n.x as f32, n.y as f32, n.z as f32);
            out.normals.push(v.normalize_or_zero());
        }
    }
    let tris = m.get_indices();
    out.indices.reserve(tris.len() * 3);
    for t in tris {
        out.indices
            .extend([t.x as u32, t.y as u32, t.z as u32]);
    }
    if out.normals.len() != out.positions.len() {
        out.recompute_normals();
    } else {
        // Replace any zero normals left by the library.
        for n in &mut out.normals {
            if *n == Vec3::ZERO {
                out.recompute_normals();
                break;
            }
        }
    }
    out
}

/// BSP-tree clipping (csg.js). Used when boolmesh cannot accept the mesh
/// (open surfaces, non-manifold after weld, etc.).
fn mesh_boolean_bsp(a_mesh: &MeshData, b_mesh: &MeshData, op: BooleanOp) -> MeshData {
    let mut a = Bsp::new(to_polygons(a_mesh));
    let mut b = Bsp::new(to_polygons(b_mesh));
    // csg.js sequences. The invert-clip-invert on b also removes b's faces
    // coplanar with a's, which would otherwise come out doubled. Where
    // csg.js ends with `a.build(b.all); a.invert()`, the trees' polygons are
    // concatenated and flipped directly — the same faces, fewer splits.
    let flip_result = match op {
        BooleanOp::Union => {
            a.clip_to(&b);
            b.clip_to(&a);
            b.invert();
            b.clip_to(&a);
            b.invert();
            false
        }
        BooleanOp::Subtract => {
            a.invert();
            a.clip_to(&b);
            b.clip_to(&a);
            b.invert();
            b.clip_to(&a);
            b.invert();
            true
        }
        BooleanOp::Intersect => {
            a.invert();
            b.clip_to(&a);
            b.invert();
            a.clip_to(&b);
            b.clip_to(&a);
            true
        }
    };
    let mut polygons = a.into_polygons();
    polygons.extend(b.into_polygons());
    if flip_result {
        for polygon in &mut polygons {
            polygon.flip();
        }
    }
    to_mesh(polygons)
}

/// Re-express `mesh` (living in `from`'s local space) in `to`'s local
/// space, mapping through world coordinates. Normals follow the
/// inverse-transpose rule (correct under non-uniform scale); when the
/// combined map is a mirror (an odd number of negative scale components)
/// the triangle winding is reversed so faces stay outward.
pub fn mesh_to_frame(mesh: &MeshData, from: &Transform, to: &Transform) -> MeshData {
    let safe = |v: Vec3| {
        Vec3::new(
            if v.x.abs() < 1e-9 { 1.0 } else { v.x },
            if v.y.abs() < 1e-9 { 1.0 } else { v.y },
            if v.z.abs() < 1e-9 { 1.0 } else { v.z },
        )
    };
    let from_scale = safe(from.scale);
    let to_scale = safe(to.scale);
    let mut out = mesh.clone();
    for p in &mut out.positions {
        *p = to.inverse_transform_point(from.transform_point(*p));
    }
    for n in &mut out.normals {
        let world = from.rotation * (*n / from_scale);
        *n = ((to.rotation.inverse() * world) * to_scale).normalize_or_zero();
    }
    let mirror = (from.scale.x * from.scale.y * from.scale.z)
        .signum()
        * (to.scale.x * to.scale.y * to.scale.z).signum()
        < 0.0;
    if mirror {
        for tri in out.indices.chunks_exact_mut(3) {
            tri.swap(1, 2);
        }
    }
    out
}

// --- the BSP machinery -------------------------------------------------------

const EPSILON: f32 = 1e-5;

const COPLANAR: u8 = 0;
const FRONT: u8 = 1;
const BACK: u8 = 2;
const SPANNING: u8 = 3;

#[derive(Clone, Copy)]
struct Vertex {
    pos: Vec3,
    normal: Vec3,
}

impl Vertex {
    fn interpolated(self, other: Vertex, t: f32) -> Vertex {
        Vertex {
            pos: self.pos.lerp(other.pos, t),
            normal: self.normal.lerp(other.normal, t),
        }
    }
}

#[derive(Clone, Copy)]
struct Plane {
    normal: Vec3,
    w: f32,
}

impl Plane {
    /// None for degenerate (zero-area) triangles.
    fn from_points(a: Vec3, b: Vec3, c: Vec3) -> Option<Plane> {
        let n = (b - a).cross(c - a);
        (n.length_squared() > 1e-16).then(|| {
            let normal = n.normalize();
            Plane { normal, w: normal.dot(a) }
        })
    }

    fn flip(&mut self) {
        self.normal = -self.normal;
        self.w = -self.w;
    }
}

/// A convex polygon (triangles on input; splitting keeps them convex).
#[derive(Clone)]
struct Polygon {
    vertices: Vec<Vertex>,
    plane: Plane,
}

impl Polygon {
    fn flip(&mut self) {
        self.vertices.reverse();
        for v in &mut self.vertices {
            v.normal = -v.normal;
        }
        self.plane.flip();
    }
}

/// Split `polygon` by `plane`, distributing the piece(s) into the four
/// lists (csg.js `splitPolygon`).
fn split_polygon(
    plane: &Plane,
    polygon: &Polygon,
    coplanar_front: &mut Vec<Polygon>,
    coplanar_back: &mut Vec<Polygon>,
    front: &mut Vec<Polygon>,
    back: &mut Vec<Polygon>,
) {
    let mut polygon_type = COPLANAR;
    let mut types = [COPLANAR; 8]; // splits of a triangle stay small
    let mut types_vec;
    let types: &mut [u8] = if polygon.vertices.len() <= types.len() {
        &mut types[..polygon.vertices.len()]
    } else {
        types_vec = vec![COPLANAR; polygon.vertices.len()];
        &mut types_vec
    };
    for (v, ty) in polygon.vertices.iter().zip(types.iter_mut()) {
        let t = plane.normal.dot(v.pos) - plane.w;
        *ty = if t < -EPSILON {
            BACK
        } else if t > EPSILON {
            FRONT
        } else {
            COPLANAR
        };
        polygon_type |= *ty;
    }
    match polygon_type {
        COPLANAR => {
            if plane.normal.dot(polygon.plane.normal) > 0.0 {
                coplanar_front.push(polygon.clone());
            } else {
                coplanar_back.push(polygon.clone());
            }
        }
        FRONT => front.push(polygon.clone()),
        BACK => back.push(polygon.clone()),
        _ => {
            let n = polygon.vertices.len();
            let mut f = Vec::with_capacity(n + 1);
            let mut b = Vec::with_capacity(n + 1);
            for i in 0..n {
                let j = (i + 1) % n;
                let (ti, tj) = (types[i], types[j]);
                let (vi, vj) = (polygon.vertices[i], polygon.vertices[j]);
                if ti != BACK {
                    f.push(vi);
                }
                if ti != FRONT {
                    b.push(vi);
                }
                if (ti | tj) == SPANNING {
                    let t = (plane.w - plane.normal.dot(vi.pos))
                        / plane.normal.dot(vj.pos - vi.pos);
                    let v = vi.interpolated(vj, t);
                    f.push(v);
                    b.push(v);
                }
            }
            if f.len() >= 3 {
                front.push(Polygon { vertices: f, plane: polygon.plane });
            }
            if b.len() >= 3 {
                back.push(Polygon { vertices: b, plane: polygon.plane });
            }
        }
    }
}

/// BSP tree over polygons, arena-allocated so building and clipping run
/// iteratively — a recursive tree walk would overflow the stack on convex
/// meshes, whose first-polygon-plane trees degenerate into an O(n) chain.
struct Bsp {
    nodes: Vec<BspNode>,
}

struct BspNode {
    plane: Plane,
    front: Option<usize>,
    back: Option<usize>,
    polygons: Vec<Polygon>,
}

impl Bsp {
    fn new(polygons: Vec<Polygon>) -> Bsp {
        let mut bsp = Bsp { nodes: Vec::new() };
        bsp.add(polygons);
        bsp
    }

    fn push_node(&mut self, plane: Plane) -> usize {
        self.nodes.push(BspNode { plane, front: None, back: None, polygons: Vec::new() });
        self.nodes.len() - 1
    }

    /// Insert polygons, extending the tree where they spill past leaves
    /// (csg.js `Node.build`). Each node's plane is the first polygon that
    /// reached it, which consumes that polygon as coplanar — insertion
    /// always terminates.
    fn add(&mut self, polygons: Vec<Polygon>) {
        if polygons.is_empty() {
            return;
        }
        if self.nodes.is_empty() {
            self.push_node(polygons[0].plane);
        }
        let mut stack = vec![(0usize, polygons)];
        while let Some((index, polygons)) = stack.pop() {
            let plane = self.nodes[index].plane;
            let mut coplanar_front = Vec::new();
            let mut coplanar_back = Vec::new();
            let mut front = Vec::new();
            let mut back = Vec::new();
            for polygon in &polygons {
                split_polygon(
                    &plane,
                    polygon,
                    &mut coplanar_front,
                    &mut coplanar_back,
                    &mut front,
                    &mut back,
                );
            }
            // both coplanar sides stay on this node
            self.nodes[index].polygons.append(&mut coplanar_front);
            self.nodes[index].polygons.append(&mut coplanar_back);
            if !front.is_empty() {
                let child = match self.nodes[index].front {
                    Some(child) => child,
                    None => {
                        let child = self.push_node(front[0].plane);
                        self.nodes[index].front = Some(child);
                        child
                    }
                };
                stack.push((child, front));
            }
            if !back.is_empty() {
                let child = match self.nodes[index].back {
                    Some(child) => child,
                    None => {
                        let child = self.push_node(back[0].plane);
                        self.nodes[index].back = Some(child);
                        child
                    }
                };
                stack.push((child, back));
            }
        }
    }

    /// Convert solid space ↔ empty space.
    fn invert(&mut self) {
        for node in &mut self.nodes {
            for polygon in &mut node.polygons {
                polygon.flip();
            }
            node.plane.flip();
            std::mem::swap(&mut node.front, &mut node.back);
        }
    }

    /// Remove the parts of `polygons` inside this BSP's solid.
    fn clip_polygons(&self, polygons: Vec<Polygon>) -> Vec<Polygon> {
        if self.nodes.is_empty() {
            return polygons;
        }
        let mut result = Vec::new();
        let mut stack = vec![(0usize, polygons)];
        while let Some((index, polygons)) = stack.pop() {
            let node = &self.nodes[index];
            let mut coplanar_front = Vec::new();
            let mut coplanar_back = Vec::new();
            let mut front = Vec::new();
            let mut back = Vec::new();
            for polygon in &polygons {
                split_polygon(
                    &node.plane,
                    polygon,
                    &mut coplanar_front,
                    &mut coplanar_back,
                    &mut front,
                    &mut back,
                );
            }
            // coplanar polygons ride along with their facing side
            front.append(&mut coplanar_front);
            back.append(&mut coplanar_back);
            match node.front {
                Some(child) if !front.is_empty() => stack.push((child, front)),
                // reaching open space in front of every plane: outside, keep
                _ => result.append(&mut front),
            }
            if let Some(child) = node.back {
                if !back.is_empty() {
                    stack.push((child, back));
                }
            }
            // no back child: solid space — `back` is dropped
        }
        result
    }

    /// Clip every polygon stored in this tree to the outside of `other`.
    fn clip_to(&mut self, other: &Bsp) {
        for index in 0..self.nodes.len() {
            let polygons = std::mem::take(&mut self.nodes[index].polygons);
            self.nodes[index].polygons = other.clip_polygons(polygons);
        }
    }

    fn into_polygons(self) -> Vec<Polygon> {
        self.nodes.into_iter().flat_map(|n| n.polygons).collect()
    }
}

// --- MeshData conversion ------------------------------------------------------

fn to_polygons(mesh: &MeshData) -> Vec<Polygon> {
    let mut polygons = Vec::with_capacity(mesh.indices.len() / 3);
    for tri in mesh.indices.chunks_exact(3) {
        let vertex = |i: u32| Vertex {
            pos: mesh.positions[i as usize],
            normal: mesh
                .normals
                .get(i as usize)
                .copied()
                .unwrap_or(Vec3::ZERO),
        };
        let (a, b, c) = (vertex(tri[0]), vertex(tri[1]), vertex(tri[2]));
        if let Some(plane) = Plane::from_points(a.pos, b.pos, c.pos) {
            polygons.push(Polygon { vertices: vec![a, b, c], plane });
        }
    }
    polygons
}

fn to_mesh(polygons: Vec<Polygon>) -> MeshData {
    let mut m = MeshData::default();
    for polygon in polygons {
        if polygon.vertices.len() < 3 {
            continue;
        }
        // drop sliver polygons the splits shaved off (area vector ≈ 0)
        let mut area = Vec3::ZERO;
        for i in 1..polygon.vertices.len() - 1 {
            let a = polygon.vertices[0].pos;
            let b = polygon.vertices[i].pos;
            let c = polygon.vertices[i + 1].pos;
            area += (b - a).cross(c - a);
        }
        if area.length_squared() < 1e-16 {
            continue;
        }
        let base = m.positions.len() as u32;
        for v in &polygon.vertices {
            let n = v.normal.normalize_or_zero();
            m.positions.push(v.pos);
            // opposing normals can cancel out under interpolation
            m.normals.push(if n == Vec3::ZERO { polygon.plane.normal } else { n });
        }
        for i in 1..polygon.vertices.len() as u32 - 1 {
            m.indices.extend_from_slice(&[base, base + i, base + i + 1]);
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh;
    use crate::Transform;
    use glam::Quat;

    /// Signed volume via the divergence theorem — outward winding gives the
    /// true volume, so this doubles as an orientation check.
    fn volume(m: &MeshData) -> f32 {
        m.indices
            .chunks_exact(3)
            .map(|tri| {
                let a = m.positions[tri[0] as usize];
                let b = m.positions[tri[1] as usize];
                let c = m.positions[tri[2] as usize];
                a.dot(b.cross(c)) / 6.0
            })
            .sum()
    }

    fn validate(m: &MeshData) {
        assert_eq!(m.positions.len(), m.normals.len());
        assert!(m.indices.len() % 3 == 0);
        for &i in &m.indices {
            assert!((i as usize) < m.positions.len(), "index out of range");
        }
        for n in &m.normals {
            assert!((n.length() - 1.0).abs() < 1e-3, "normal not unit: {n:?}");
        }
    }

    /// A unit cube shifted by `offset`.
    fn cube_at(offset: Vec3) -> MeshData {
        let mut m = mesh::cube(1.0);
        for p in &mut m.positions {
            *p += offset;
        }
        m
    }

    #[test]
    fn booleans_of_overlapping_cubes_have_the_right_volumes() {
        // diagonal offset: no coplanar faces, overlap = 0.75³
        let a = cube_at(Vec3::ZERO);
        let b = cube_at(Vec3::splat(0.25));
        let overlap = 0.75f32.powi(3);

        let union = mesh_boolean(&a, &b, BooleanOp::Union);
        validate(&union);
        assert!((volume(&union) - (2.0 - overlap)).abs() < 1e-3, "{}", volume(&union));

        let cut = mesh_boolean(&a, &b, BooleanOp::Subtract);
        validate(&cut);
        assert!((volume(&cut) - (1.0 - overlap)).abs() < 1e-3, "{}", volume(&cut));

        let both = mesh_boolean(&a, &b, BooleanOp::Intersect);
        validate(&both);
        assert!((volume(&both) - overlap).abs() < 1e-3, "{}", volume(&both));
    }

    #[test]
    fn coplanar_faces_do_not_double_up() {
        // identical cubes: every face coplanar — union must stay volume 1
        let a = cube_at(Vec3::ZERO);
        let union = mesh_boolean(&a, &a, BooleanOp::Union);
        validate(&union);
        assert!((volume(&union) - 1.0).abs() < 1e-3, "{}", volume(&union));

        // axis offset: the four side faces stay pairwise coplanar
        let b = cube_at(Vec3::new(0.5, 0.0, 0.0));
        let union = mesh_boolean(&a, &b, BooleanOp::Union);
        validate(&union);
        assert!((volume(&union) - 1.5).abs() < 1e-3, "{}", volume(&union));
    }

    #[test]
    fn disjoint_and_enclosing_cases() {
        let a = cube_at(Vec3::ZERO);
        let far = cube_at(Vec3::new(5.0, 0.0, 0.0));

        // disjoint union keeps both bodies; subtract changes nothing
        let union = mesh_boolean(&a, &far, BooleanOp::Union);
        assert!((volume(&union) - 2.0).abs() < 1e-3);
        let cut = mesh_boolean(&a, &far, BooleanOp::Subtract);
        assert!((volume(&cut) - 1.0).abs() < 1e-3);
        let both = mesh_boolean(&a, &far, BooleanOp::Intersect);
        assert_eq!(volume(&both), 0.0);
        assert!(both.indices.is_empty(), "disjoint intersection is empty");

        // subtracting an enclosing cube leaves nothing
        let big = mesh::cube(3.0);
        let gone = mesh_boolean(&a, &big, BooleanOp::Subtract);
        assert!(gone.indices.is_empty(), "{} tris left", gone.indices.len() / 3);

        // a hollow: subtracting a smaller cube from a bigger one — volume
        // is the shell, and the result has twice the face count region-wise
        let hollow = mesh_boolean(&big, &a, BooleanOp::Subtract);
        validate(&hollow);
        assert!((volume(&hollow) - 26.0).abs() < 0.01, "{}", volume(&hollow));
    }

    #[test]
    fn curved_meshes_survive_and_keep_smooth_normals() {
        let sphere = mesh::uv_sphere(24, 12, 0.75);
        let cube = cube_at(Vec3::new(0.5, 0.0, 0.0));
        let cut = mesh_boolean(&sphere, &cube, BooleanOp::Subtract);
        validate(&cut);
        let sphere_volume = volume(&sphere);
        let v = volume(&cut);
        assert!(v > 0.2 * sphere_volume && v < 0.9 * sphere_volume, "{v}");
        // vertices still on the sphere keep their smooth (radial) normals
        let mut checked = 0;
        for (p, n) in cut.positions.iter().zip(cut.normals.iter()) {
            if (p.length() - 0.75).abs() < 1e-4 && p.x < -0.1 {
                assert!(
                    n.dot(p.normalize()) > 0.95,
                    "normal {n:?} not radial at {p:?}"
                );
                checked += 1;
            }
        }
        assert!(checked > 20, "only {checked} spherical vertices found");
    }

    #[test]
    fn winding_matches_normals_after_booleans() {
        let a = mesh::uv_sphere(16, 8, 0.8);
        let b = cube_at(Vec3::splat(0.4));
        for op in BooleanOp::ALL {
            let m = mesh_boolean(&a, &b, op);
            for tri in m.indices.chunks_exact(3) {
                let p0 = m.positions[tri[0] as usize];
                let p1 = m.positions[tri[1] as usize];
                let p2 = m.positions[tri[2] as usize];
                let face = (p1 - p0).cross(p2 - p0);
                if face.length() < 1e-8 {
                    continue;
                }
                let avg = (m.normals[tri[0] as usize]
                    + m.normals[tri[1] as usize]
                    + m.normals[tri[2] as usize])
                    / 3.0;
                assert!(
                    face.normalize().dot(avg.normalize_or_zero()) > 0.0,
                    "{op:?}: winding disagrees with normals"
                );
            }
        }
    }

    #[test]
    fn mesh_to_frame_maps_between_object_spaces() {
        let from = Transform {
            location: Vec3::new(2.0, 0.0, 1.0),
            rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            scale: Vec3::new(2.0, 1.0, 1.0),
        };
        let to = Transform {
            location: Vec3::new(1.0, 0.0, 0.0),
            ..Transform::default()
        };
        let m = mesh_to_frame(&mesh::cube(1.0), &from, &to);
        validate(&m);
        // the cube's world center (2, 0, 1) lands at (1, 0, 1) in `to`
        let center: Vec3 =
            m.positions.iter().sum::<Vec3>() / m.positions.len() as f32;
        assert!((center - Vec3::new(1.0, 0.0, 1.0)).length() < 1e-4, "{center:?}");
        // scaled 2× along local X then rotated: extents (1, 2, 1) in `to`
        let e = m.extents();
        assert!((e - Vec3::new(1.0, 2.0, 1.0)).length() < 1e-4, "{e:?}");
        // orientation survives: winding still agrees with the normals
        for tri in m.indices.chunks_exact(3) {
            let face = (m.positions[tri[1] as usize] - m.positions[tri[0] as usize])
                .cross(m.positions[tri[2] as usize] - m.positions[tri[0] as usize]);
            assert!(face.dot(m.normals[tri[0] as usize]) > 0.0);
        }
    }

    #[test]
    fn mesh_to_frame_mirror_scale_keeps_outward_winding() {
        let from = Transform {
            scale: Vec3::new(-1.0, 1.0, 1.0),
            ..Transform::default()
        };
        let m = mesh_to_frame(&mesh::cube(1.0), &from, &Transform::default());
        validate(&m);
        assert!((volume(&m) - 1.0).abs() < 1e-4, "{}", volume(&m));
    }

    #[test]
    fn boolean_ops_parse_from_names() {
        assert_eq!(BooleanOp::from_name("union"), Some(BooleanOp::Union));
        assert_eq!(BooleanOp::from_name("Add"), Some(BooleanOp::Union));
        assert_eq!(BooleanOp::from_name("SUBTRACT"), Some(BooleanOp::Subtract));
        assert_eq!(BooleanOp::from_name("difference"), Some(BooleanOp::Subtract));
        assert_eq!(BooleanOp::from_name("intersect"), Some(BooleanOp::Intersect));
        assert_eq!(BooleanOp::from_name("nope"), None);
    }

    #[test]
    fn boolmesh_beats_bsp_on_sphere_cube() {
        use std::time::Instant;
        let sphere = mesh::uv_sphere(48, 24, 0.75);
        let cube = cube_at(Vec3::new(0.5, 0.0, 0.0));
        // warm + measure primary path
        let _ = mesh_boolean(&sphere, &cube, BooleanOp::Subtract);
        let t0 = Instant::now();
        for _ in 0..10 {
            let m = mesh_boolean(&sphere, &cube, BooleanOp::Subtract);
            assert!(!m.indices.is_empty());
        }
        let primary = t0.elapsed();
        // measure BSP fallback alone
        let t1 = Instant::now();
        for _ in 0..10 {
            let m = mesh_boolean_bsp(&sphere, &cube, BooleanOp::Subtract);
            assert!(!m.indices.is_empty());
        }
        let bsp = t1.elapsed();
        eprintln!(
            "sphere−cube ×10: primary={primary:?} bsp={bsp:?} speedup={:.1}×",
            bsp.as_secs_f64() / primary.as_secs_f64().max(1e-9)
        );
        // boolmesh should be clearly faster; allow some CI noise
        assert!(
            primary < bsp,
            "expected boolmesh primary path faster than BSP: {primary:?} vs {bsp:?}"
        );
    }

    #[test]
    fn disjoint_aabb_is_near_instant() {
        use std::time::Instant;
        let a = mesh::uv_sphere(32, 16, 1.0);
        let b = {
            let mut m = mesh::uv_sphere(32, 16, 1.0);
            for p in &mut m.positions {
                *p += Vec3::new(10.0, 0.0, 0.0);
            }
            m
        };
        let t0 = Instant::now();
        for _ in 0..100 {
            let u = mesh_boolean(&a, &b, BooleanOp::Union);
            assert!((volume(&u) - 2.0 * volume(&a)).abs() < 0.05);
            let c = mesh_boolean(&a, &b, BooleanOp::Subtract);
            assert!((volume(&c) - volume(&a)).abs() < 0.05);
            assert!(mesh_boolean(&a, &b, BooleanOp::Intersect).indices.is_empty());
        }
        let elapsed = t0.elapsed();
        eprintln!("100×3 disjoint sphere ops: {elapsed:?}");
        assert!(elapsed.as_millis() < 50, "disjoint path should be cheap: {elapsed:?}");
    }
}
