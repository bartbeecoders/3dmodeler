//! Topology-editing operators for edit mode: loop cut (Ctrl+R), edge bevel
//! (Ctrl+B), face extrude (E) and face inset (I).
//!
//! All take the welded `Topology` view plus the underlying triangle mesh
//! and return a rebuilt mesh, leaving the inputs untouched — the modal tools
//! re-run the operator from the pre-tool mesh every time a parameter changes
//! (wheel notch, drag), so scrubbing stays non-destructive until the user
//! confirms.

use crate::edit_mode::{build_topology, Topology};
use modeler_core::glam::{Vec2, Vec3};
use modeler_core::MeshData;
use std::collections::{BTreeMap, HashMap, HashSet};

fn ekey(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}

/// Order a face's outline into one closed loop of welded vertices, oriented
/// counter-clockwise seen from outside (matching the face normal). None if
/// the outline is not a single simple loop (e.g. a wall face with a window
/// hole) — such faces don't support cut/bevel surgery.
pub fn face_loop(topo: &Topology, face: usize) -> Option<Vec<usize>> {
    let group = topo.faces.get(face)?;
    let mut neighbors: HashMap<usize, Vec<usize>> = HashMap::new();
    for &(a, b) in &group.outline {
        neighbors.entry(a).or_default().push(b);
        neighbors.entry(b).or_default().push(a);
    }
    if neighbors.values().any(|n| n.len() != 2) {
        return None;
    }
    let start = group.outline.first()?.0;
    let mut ordered = vec![start];
    let (mut prev, mut cur) = (start, neighbors[&start][0]);
    while cur != start {
        ordered.push(cur);
        let n = &neighbors[&cur];
        let next = if n[0] == prev { n[1] } else { n[0] };
        prev = cur;
        cur = next;
    }
    if ordered.len() != group.outline.len() {
        return None; // outline splits into several loops (holes)
    }
    // orient CCW from outside: polygon area vector vs. a member triangle
    let t = topo.tris[group.tris[0]];
    let tri_n =
        (topo.verts[t[1]] - topo.verts[t[0]]).cross(topo.verts[t[2]] - topo.verts[t[0]]);
    if polygon_area_vector(&ordered, |w| topo.verts[w]).dot(tri_n) < 0.0 {
        ordered.reverse();
    }
    Some(ordered)
}

/// Twice the signed area vector of a closed polygon (origin-independent).
fn polygon_area_vector<F: Fn(usize) -> Vec3>(loop_: &[usize], pos: F) -> Vec3 {
    let mut n = Vec3::ZERO;
    for i in 0..loop_.len() {
        n += pos(loop_[i]).cross(pos(loop_[(i + 1) % loop_.len()]));
    }
    n
}

/// Welded edge -> faces whose outline contains it.
fn edge_face_map(topo: &Topology) -> HashMap<(usize, usize), Vec<usize>> {
    let mut map: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    for (fi, f) in topo.faces.iter().enumerate() {
        for &(a, b) in &f.outline {
            map.entry(ekey(a, b)).or_default().push(fi);
        }
    }
    map
}

/// One quad the loop cut passes through: the cut enters across `entry` and
/// leaves across `exit`, both oriented so equal interpolation parameters
/// connect (the point at t on `entry` joins the point at t on `exit`).
/// `aligned` says whether `entry` runs along the face's CCW loop — needed to
/// wind the replacement triangles outward.
struct RingFace {
    face: usize,
    entry: (usize, usize),
    exit: (usize, usize),
    aligned: bool,
}

/// Cross a quad entering over the oriented edge (u, v): the exit is the
/// opposite loop edge, oriented so the endpoint sharing a side with `u`
/// comes first.
fn cross_quad(loop4: &[usize], u: usize, v: usize) -> Option<((usize, usize), bool)> {
    if loop4.len() != 4 {
        return None;
    }
    for i in 0..4 {
        let (p, q) = (loop4[i], loop4[(i + 1) % 4]);
        let (c, d) = (loop4[(i + 2) % 4], loop4[(i + 3) % 4]);
        if (p, q) == (u, v) {
            return Some(((d, c), true));
        }
        if (p, q) == (v, u) {
            return Some(((c, d), false));
        }
    }
    None
}

/// Walk the ring of quads perpendicular to `start` (Blender's edge ring):
/// out of both sides of the edge, through opposite edges of each quad, until
/// the ring closes or hits a non-quad / boundary.
fn walk_ring(
    loops: &[Option<Vec<usize>>],
    ef: &HashMap<(usize, usize), Vec<usize>>,
    start: (usize, usize),
) -> Vec<RingFace> {
    let mut ring = Vec::new();
    let mut visited: HashSet<usize> = HashSet::new();
    for _direction in 0..2 {
        let mut edge = start;
        loop {
            let next = ef
                .get(&ekey(edge.0, edge.1))
                .and_then(|fs| fs.iter().copied().find(|f| !visited.contains(f)));
            let Some(face) = next else { break };
            // visited even on failure, so the second pass tries the edge's
            // other face instead of giving up on the same non-quad again
            visited.insert(face);
            let Some(Some(loop4)) = loops.get(face) else { break };
            let Some((exit, aligned)) = cross_quad(loop4, edge.0, edge.1) else { break };
            ring.push(RingFace { face, entry: edge, exit, aligned });
            edge = exit;
        }
    }
    ring
}

/// Insert `cuts` evenly spaced edge loops perpendicular to `edge`, splitting
/// every quad of its edge ring into strips. Faces outside the ring keep
/// their exact triangles and vertex sharing (smooth stays smooth, flat stays
/// flat). None when the edge borders no quad.
pub fn loop_cut(
    mesh: &MeshData,
    topo: &Topology,
    edge: (usize, usize),
    cuts: usize,
) -> Option<MeshData> {
    if cuts == 0 {
        return None;
    }
    let ef = edge_face_map(topo);
    let loops: Vec<Option<Vec<usize>>> =
        (0..topo.faces.len()).map(|f| face_loop(topo, f)).collect();
    let ring = walk_ring(&loops, &ef, ekey(edge.0, edge.1));
    if ring.is_empty() {
        return None;
    }
    let ring_faces: HashSet<usize> = ring.iter().map(|r| r.face).collect();

    // per-face corner lookup (weld -> original mesh vertex) so rebuilt strips
    // reuse the original corners and keep the mesh's sharing structure
    let mut corner: HashMap<(usize, usize), u32> = HashMap::new();
    for &fi in &ring_faces {
        for &ti in &topo.faces[fi].tris {
            for k in 0..3 {
                let idx = mesh.indices[3 * ti + k];
                corner.insert((fi, topo.weld_of[idx as usize]), idx);
            }
        }
    }

    // an edge whose two ring faces share mesh vertices is smooth-shaded:
    // the inserted cut vertices must be shared across it too
    let mut cut_edge_faces: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    for r in &ring {
        for e in [r.entry, r.exit] {
            let faces = cut_edge_faces.entry(ekey(e.0, e.1)).or_default();
            if !faces.contains(&r.face) {
                faces.push(r.face);
            }
        }
    }
    let smooth: HashSet<(usize, usize)> = cut_edge_faces
        .iter()
        .filter(|(key, fs)| {
            let shared = |w: usize| match (corner.get(&(fs[0], w)), corner.get(&(fs[1], w))) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            };
            fs.len() == 2 && shared(key.0) && shared(key.1)
        })
        .map(|(key, _)| *key)
        .collect();

    // keep every triangle outside the ring
    let mut face_of_tri = vec![usize::MAX; topo.tris.len()];
    for (fi, f) in topo.faces.iter().enumerate() {
        for &ti in &f.tris {
            face_of_tri[ti] = fi;
        }
    }
    let mut out = MeshData {
        positions: mesh.positions.clone(),
        normals: Vec::new(), // recomputed after compaction
        indices: Vec::new(),
        seams: mesh.seams.clone(),
    };
    for (ti, tri) in mesh.indices.chunks_exact(3).enumerate() {
        if !ring_faces.contains(&face_of_tri[ti]) {
            out.indices.extend_from_slice(tri);
        }
    }

    // cut-vertex factory: one vertex per (edge, cut) on smooth edges, one
    // per (edge, cut, face) on sharp ones; the cut index is canonicalized to
    // the sorted edge direction so both orientations meet the same vertex
    let mut created: HashMap<(usize, usize, usize, usize), u32> = HashMap::new();
    let mut cut_vertex = |out: &mut MeshData, face: usize, e: (usize, usize), j: usize| -> u32 {
        let key = ekey(e.0, e.1);
        let jc = if (e.0, e.1) == key { j } else { cuts + 1 - j };
        let share = if smooth.contains(&key) { usize::MAX } else { face };
        *created.entry((key.0, key.1, jc, share)).or_insert_with(|| {
            let t = j as f32 / (cuts + 1) as f32;
            out.positions.push(topo.verts[e.0].lerp(topo.verts[e.1], t));
            (out.positions.len() - 1) as u32
        })
    };

    for r in &ring {
        let mut point = |out: &mut MeshData, e: (usize, usize), j: usize| -> u32 {
            if j == 0 {
                corner[&(r.face, e.0)]
            } else if j == cuts + 1 {
                corner[&(r.face, e.1)]
            } else {
                cut_vertex(out, r.face, e, j)
            }
        };
        for j in 0..=cuts {
            let a0 = point(&mut out, r.entry, j);
            let a1 = point(&mut out, r.entry, j + 1);
            let b0 = point(&mut out, r.exit, j);
            let b1 = point(&mut out, r.exit, j + 1);
            // strip quad [a0, a1, b1, b0]; CCW when entry runs with the loop
            if r.aligned {
                out.indices.extend_from_slice(&[a0, a1, b1, a0, b1, b0]);
            } else {
                out.indices.extend_from_slice(&[a0, b1, a1, a0, b0, b1]);
            }
            // the strip boundary is a user cut: seam it so the coplanar
            // strips stay separate faces in the welded topology
            if j > 0 {
                out.seams.push((a0, b0));
            }
        }
    }

    compact(&mut out);
    Some(out)
}

/// Everything `bevel_edge` needs, resolved once: the two side faces, the end
/// face(s) at each vertex, the slide targets, and the geometric width limit.
struct BevelPlan {
    side_faces: [usize; 2],
    /// Loops of the side faces (CCW).
    side_loops: [Vec<usize>; 2],
    /// End faces at vertex a / b (may be the same face) with their loops.
    end_faces: Vec<(usize, Vec<usize>)>,
    /// Slide targets: in side face 0 / 1, for endpoint a then b.
    targets: [[usize; 2]; 2],
    limit: f32,
}

fn bevel_plan(topo: &Topology, edge: (usize, usize)) -> Option<BevelPlan> {
    let (a, b) = ekey(edge.0, edge.1);
    let ef = edge_face_map(topo);
    let faces = ef.get(&(a, b))?;
    if faces.len() != 2 {
        return None;
    }
    let (f1, f2) = (faces[0], faces[1]);
    let loop1 = face_loop(topo, f1)?;
    let loop2 = face_loop(topo, f2)?;

    // the loop neighbor of `w` on the other side of `other` — the edge the
    // new corner slides along
    let slide_target = |lp: &[usize], w: usize, other: usize| -> Option<usize> {
        let i = lp.iter().position(|&x| x == w)?;
        let l = lp.len();
        let (prev, next) = (lp[(i + l - 1) % l], lp[(i + 1) % l]);
        if prev == other {
            Some(next)
        } else if next == other {
            Some(prev)
        } else {
            None
        }
    };
    let p_a = slide_target(&loop1, a, b)?;
    let q_a = slide_target(&loop2, a, b)?;
    let p_b = slide_target(&loop1, b, a)?;
    let q_b = slide_target(&loop2, b, a)?;

    // each endpoint must be a simple 3-edge corner: exactly one more face
    // besides the two sides (a box corner) — anything else is unsupported
    let other_faces = |w: usize| -> Vec<usize> {
        topo.faces
            .iter()
            .enumerate()
            .filter(|&(fi, f)| {
                fi != f1 && fi != f2 && f.outline.iter().any(|&(x, y)| x == w || y == w)
            })
            .map(|(fi, _)| fi)
            .collect()
    };
    let (ea, eb) = (other_faces(a), other_faces(b));
    if ea.len() != 1 || eb.len() != 1 {
        return None;
    }
    let (ea, eb) = (ea[0], eb[0]);
    let mut end_faces = vec![(ea, face_loop(topo, ea)?)];
    if eb != ea {
        end_faces.push((eb, face_loop(topo, eb)?));
    }
    // in its end face, each endpoint must sit between its two slide targets
    for (w, t1, t2) in [(a, p_a, q_a), (b, p_b, q_b)] {
        let lp = &end_faces.iter().find(|(f, _)| topo.faces[*f].outline.iter().any(|&(x, y)| x == w || y == w))?.1;
        let i = lp.iter().position(|&x| x == w)?;
        let l = lp.len();
        let ns = [lp[(i + l - 1) % l], lp[(i + 1) % l]];
        if !(ns.contains(&t1) && ns.contains(&t2)) {
            return None;
        }
    }

    let mut limit = f32::INFINITY;
    for (w, t) in [(a, p_a), (a, q_a), (b, p_b), (b, q_b)] {
        limit = limit.min((topo.verts[t] - topo.verts[w]).length());
    }
    if !limit.is_finite() || limit < 1e-5 {
        return None;
    }
    Some(BevelPlan {
        side_faces: [f1, f2],
        side_loops: [loop1, loop2],
        end_faces,
        targets: [[p_a, p_b], [q_a, q_b]],
        limit: 0.9 * limit,
    })
}

/// Largest usable bevel width for the edge (just under the shortest edge the
/// new corners slide along), or None when the edge can't be beveled.
pub fn bevel_limit(topo: &Topology, edge: (usize, usize)) -> Option<f32> {
    Some(bevel_plan(topo, edge)?.limit)
}

/// Bevel the edge: its endpoints slide `width` along the adjacent face
/// edges, the two side faces shrink, the end faces gain the profile's
/// corners, and `segments` strips bridge the gap along a quadratic curve
/// tangent to both side faces (1 segment = flat chamfer). Only simple
/// 3-edge (box-like) corners are supported. Rebuilt faces come out
/// flat-shaded.
pub fn bevel_edge(
    mesh: &MeshData,
    topo: &Topology,
    edge: (usize, usize),
    width: f32,
    segments: usize,
) -> Option<MeshData> {
    let (a, b) = ekey(edge.0, edge.1);
    let plan = bevel_plan(topo, (a, b))?;
    let w = width.clamp(1e-4, plan.limit);
    let segments = segments.clamp(1, 64);

    let slide = |from: usize, toward: usize| -> Vec3 {
        let d = (topo.verts[toward] - topo.verts[from]).normalize_or_zero();
        topo.verts[from] + d * w
    };
    // rounded profile per endpoint: a quadratic Bézier from the side-0
    // corner over the original vertex to the side-1 corner — tangent to
    // both side faces, so the bevel meets them without a crease
    let arc = |v: usize, endpoint: usize| -> Vec<Vec3> {
        let c0 = slide(v, plan.targets[0][endpoint]);
        let c1 = slide(v, plan.targets[1][endpoint]);
        let p = topo.verts[v];
        (0..=segments)
            .map(|j| {
                let t = j as f32 / segments as f32;
                let s = 1.0 - t;
                c0 * (s * s) + p * (2.0 * s * t) + c1 * (t * t)
            })
            .collect()
    };
    // profile rows per endpoint: arcs[a / b][0 ..= segments]
    let arcs = [arc(a, 0), arc(b, 1)];
    // profile ends: [side face 0 / 1][endpoint a / b]
    let corners = [
        [arcs[0][0], arcs[1][0]],
        [arcs[0][segments], arcs[1][segments]],
    ];

    // rebuilt polygons, as position loops (CCW seen from outside)
    let mut polys: Vec<Vec<Vec3>> = Vec::new();
    // side faces: the edge endpoints move to that face's new corners
    for side in 0..2 {
        polys.push(
            plan.side_loops[side]
                .iter()
                .map(|&v| {
                    if v == a {
                        corners[side][0]
                    } else if v == b {
                        corners[side][1]
                    } else {
                        topo.verts[v]
                    }
                })
                .collect(),
        );
    }
    // end faces: the endpoint splits into the full profile, ordered so it
    // starts on the slide edge the loop walk arrives from
    for (_, lp) in &plan.end_faces {
        let mut poly = Vec::with_capacity(lp.len() + segments + 1);
        for (i, &v) in lp.iter().enumerate() {
            let endpoint = if v == a { 0 } else if v == b { 1 } else { usize::MAX };
            if endpoint == usize::MAX {
                poly.push(topo.verts[v]);
                continue;
            }
            let prev = lp[(i + lp.len() - 1) % lp.len()];
            // the profile runs side 0 -> side 1
            if prev == plan.targets[0][endpoint] {
                poly.extend(arcs[endpoint].iter().copied());
            } else {
                poly.extend(arcs[endpoint].iter().rev().copied());
            }
        }
        polys.push(poly);
    }
    // the bevel strips, wound outward (against the mean of the side faces)
    let outward: Vec3 = plan
        .side_faces
        .iter()
        .map(|&f| {
            let t = topo.tris[topo.faces[f].tris[0]];
            (topo.verts[t[1]] - topo.verts[t[0]])
                .cross(topo.verts[t[2]] - topo.verts[t[0]])
                .normalize_or_zero()
        })
        .sum();
    let coarse = [corners[0][0], corners[0][1], corners[1][1], corners[1][0]];
    let area = {
        let mut n = Vec3::ZERO;
        for i in 0..coarse.len() {
            n += coarse[i].cross(coarse[(i + 1) % coarse.len()]);
        }
        n
    };
    let flip = area.dot(outward) < 0.0;
    for j in 0..segments {
        let mut strip = vec![arcs[0][j], arcs[1][j], arcs[1][j + 1], arcs[0][j + 1]];
        if flip {
            strip.reverse();
        }
        polys.push(strip);
    }

    // keep everything outside the rebuilt faces
    let rebuilt: HashSet<usize> = plan
        .side_faces
        .iter()
        .copied()
        .chain(plan.end_faces.iter().map(|(f, _)| *f))
        .collect();
    let mut face_of_tri = vec![usize::MAX; topo.tris.len()];
    for (fi, f) in topo.faces.iter().enumerate() {
        for &ti in &f.tris {
            face_of_tri[ti] = fi;
        }
    }
    let mut out = MeshData {
        positions: mesh.positions.clone(),
        normals: Vec::new(),
        indices: Vec::new(),
        seams: mesh.seams.clone(),
    };
    for (ti, tri) in mesh.indices.chunks_exact(3).enumerate() {
        if !rebuilt.contains(&face_of_tri[ti]) {
            out.indices.extend_from_slice(tri);
        }
    }
    for poly in polys {
        emit_polygon(&mut out, &poly)?;
    }

    compact(&mut out);
    Some(out)
}

/// Copy of the mesh without one welded face group's triangles (normals
/// left empty — callers run `compact`, which recomputes them).
fn strip_face(mesh: &MeshData, topo: &Topology, face: usize) -> MeshData {
    let skip: HashSet<usize> = topo.faces[face].tris.iter().copied().collect();
    let mut out = MeshData {
        positions: mesh.positions.clone(),
        normals: Vec::new(),
        indices: Vec::new(),
        seams: mesh.seams.clone(),
    };
    for (ti, tri) in mesh.indices.chunks_exact(3).enumerate() {
        if !skip.contains(&ti) {
            out.indices.extend_from_slice(tri);
        }
    }
    out
}

/// Extrude the face along its outward normal by `amount` (negative pushes
/// inward): the face translates and a wall quad bridges each boundary edge.
/// The base loop is seamed so walls coplanar with their neighbors (extruding
/// a box top leaves them flush with the sides) stay separate faces. Zero
/// amount returns the mesh unchanged — the modal tool starts there. None
/// when the outline isn't a single simple loop (holes).
pub fn extrude_face(
    mesh: &MeshData,
    topo: &Topology,
    face: usize,
    amount: f32,
) -> Option<MeshData> {
    let lp = face_loop(topo, face)?;
    if amount.abs() < 1e-4 {
        return Some(mesh.clone());
    }
    let normal = polygon_area_vector(&lp, |w| topo.verts[w]).normalize_or_zero();
    if normal.length_squared() < 0.5 {
        return None; // degenerate face
    }
    let offset = normal * amount;

    let mut out = strip_face(mesh, topo, face);
    // the moved face, same CCW orientation
    let top: Vec<Vec3> = lp.iter().map(|&v| topo.verts[v] + offset).collect();
    emit_polygon(&mut out, &top)?;
    for i in 0..lp.len() {
        let a = topo.verts[lp[i]];
        let b = topo.verts[lp[(i + 1) % lp.len()]];
        let base = out.positions.len() as u32;
        // wall [a, b, b+off, a+off]: faces outward on a pull and inward on
        // a push — the boundary of the swept slab flips with the sign
        emit_polygon(&mut out, &[a, b, b + offset, a + offset])?;
        // the base edge is a user cut: without it a wall flush with its
        // neighbor face would weld back into one group
        out.seams.push((base, base + 1));
    }
    compact(&mut out);
    Some(out)
}

/// Largest usable inset thickness: just under the centroid's distance to
/// the nearest outline edge (the border collapses around there). None when
/// the face has no simple loop or is degenerate.
pub fn inset_limit(topo: &Topology, face: usize) -> Option<f32> {
    let lp = face_loop(topo, face)?;
    let c = lp.iter().map(|&v| topo.verts[v]).sum::<Vec3>() / lp.len() as f32;
    let mut min_d = f32::INFINITY;
    for i in 0..lp.len() {
        let a = topo.verts[lp[i]];
        let e = (topo.verts[lp[(i + 1) % lp.len()]] - a).normalize_or_zero();
        let to_c = c - a;
        min_d = min_d.min((to_c - e * to_c.dot(e)).length());
    }
    (min_d.is_finite() && min_d > 1e-4).then_some(0.9 * min_d)
}

/// Inset the face: a shrunk copy sits `amount` inside the boundary with
/// uniform border thickness (each edge shifts inward by `amount`; the new
/// corners are the shifted edges' intersections), ringed by border quads.
/// The inner rim and the corner spokes are seamed — the ring is coplanar
/// with the inner face and would weld back into one group without them.
/// Zero amount returns the mesh unchanged. None when the outline isn't a
/// single simple loop (holes).
pub fn inset_face(
    mesh: &MeshData,
    topo: &Topology,
    face: usize,
    amount: f32,
) -> Option<MeshData> {
    let lp = face_loop(topo, face)?;
    if amount < 1e-4 {
        return Some(mesh.clone());
    }
    let normal = polygon_area_vector(&lp, |w| topo.verts[w]).normalize_or_zero();
    if normal.length_squared() < 0.5 {
        return None;
    }
    let n = lp.len();
    let pts: Vec<Vec3> = lp.iter().map(|&v| topo.verts[v]).collect();
    let inner: Vec<Vec3> = (0..n)
        .map(|i| {
            // in-plane inward normals of the two edges meeting at pts[i];
            // their offset lines intersect at v + amount·(m1+m2)/(1+m1·m2)
            let m1 = normal.cross((pts[i] - pts[(i + n - 1) % n]).normalize_or_zero());
            let m2 = normal.cross((pts[(i + 1) % n] - pts[i]).normalize_or_zero());
            // reflex-spike guard: clamp the denominator so a near-180°
            // corner can't shoot its new vertex across the face
            pts[i] + (m1 + m2) * (amount / (1.0 + m1.dot(m2)).max(0.1))
        })
        .collect();

    let mut out = strip_face(mesh, topo, face);
    emit_polygon(&mut out, &inner)?;
    for i in 0..n {
        let j = (i + 1) % n;
        let base = out.positions.len() as u32;
        emit_polygon(&mut out, &[pts[i], pts[j], inner[j], inner[i]])?;
        out.seams.push((base + 2, base + 3)); // inner rim
        out.seams.push((base + 3, base)); // corner spoke
    }
    compact(&mut out);
    Some(out)
}

/// Subdivision surface (Blender's subsurf): `levels` rounds of Catmull-
/// Clark over the mesh. Smooth objects keep shared vertices and averaged
/// normals; flat ones come out faceted. A triangle-count valve stops
/// runaway growth on dense meshes.
pub fn subdivide(mesh: &MeshData, levels: u8, smooth: bool) -> MeshData {
    let mut current = mesh.clone();
    for _ in 0..levels {
        if current.indices.len() > 3 * 200_000 {
            break;
        }
        current = catmull_clark(&current);
    }
    if smooth {
        current
    } else {
        current.into_flat()
    }
}

/// One Catmull-Clark round. Faces are the welded coplanar polygon groups
/// (their loops keep cube sides as quads, matching Blender's cage); groups
/// that don't form a simple loop (holes) fall back to their raw triangles.
/// Boundary edges/vertices of open meshes follow the standard B-spline
/// boundary rules. BTreeMaps keep the vertex order deterministic so the
/// renderer's in-place buffer updates stay effective during edit drags.
fn catmull_clark(mesh: &MeshData) -> MeshData {
    let topo = build_topology(mesh);
    let mut faces: Vec<Vec<usize>> = Vec::new();
    for f in 0..topo.faces.len() {
        if let Some(lp) = face_loop(&topo, f) {
            faces.push(lp);
        } else {
            for &ti in &topo.faces[f].tris {
                let t = topo.tris[ti];
                faces.push(vec![t[0], t[1], t[2]]);
            }
        }
    }

    let nv = topo.verts.len();
    let face_point: Vec<Vec3> = faces
        .iter()
        .map(|lp| lp.iter().map(|&v| topo.verts[v]).sum::<Vec3>() / lp.len() as f32)
        .collect();

    let mut edge_faces: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
    for (fi, lp) in faces.iter().enumerate() {
        for i in 0..lp.len() {
            edge_faces.entry(ekey(lp[i], lp[(i + 1) % lp.len()])).or_default().push(fi);
        }
    }
    let mut v_faces: Vec<Vec<usize>> = vec![Vec::new(); nv];
    for (fi, lp) in faces.iter().enumerate() {
        for &v in lp {
            v_faces[v].push(fi);
        }
    }
    let mut v_edges: Vec<Vec<(usize, usize)>> = vec![Vec::new(); nv];
    for &e in edge_faces.keys() {
        v_edges[e.0].push(e);
        v_edges[e.1].push(e);
    }

    let mut out = MeshData::default();
    // output layout: moved original vertices, then face points, then edge
    // points — quads index into these three blocks
    for v in 0..nv {
        let p = topo.verts[v];
        let boundary: Vec<(usize, usize)> = v_edges[v]
            .iter()
            .copied()
            .filter(|e| edge_faces[e].len() != 2)
            .collect();
        let moved = if boundary.len() == 2 {
            // open-mesh rim: smooth along the boundary curve
            let other = |e: (usize, usize)| if e.0 == v { e.1 } else { e.0 };
            (topo.verts[other(boundary[0])] + 6.0 * p + topo.verts[other(boundary[1])]) / 8.0
        } else if !boundary.is_empty() || v_faces[v].is_empty() {
            p // non-manifold or isolated: pinned
        } else {
            let n = v_edges[v].len() as f32;
            let q = v_faces[v].iter().map(|&f| face_point[f]).sum::<Vec3>()
                / v_faces[v].len() as f32;
            let r = v_edges[v]
                .iter()
                .map(|e| 0.5 * (topo.verts[e.0] + topo.verts[e.1]))
                .sum::<Vec3>()
                / n;
            (q + 2.0 * r + (n - 3.0) * p) / n
        };
        out.positions.push(moved);
    }
    let face_base = out.positions.len();
    out.positions.extend(face_point.iter().copied());
    let mut edge_index: BTreeMap<(usize, usize), u32> = BTreeMap::new();
    for (&e, fs) in &edge_faces {
        let mid = 0.5 * (topo.verts[e.0] + topo.verts[e.1]);
        let p = if fs.len() == 2 {
            0.5 * mid + 0.25 * (face_point[fs[0]] + face_point[fs[1]])
        } else {
            mid // boundary edge
        };
        edge_index.insert(e, out.positions.len() as u32);
        out.positions.push(p);
    }

    for (fi, lp) in faces.iter().enumerate() {
        let n = lp.len();
        let f_idx = (face_base + fi) as u32;
        for i in 0..n {
            let v = lp[i] as u32;
            let e_next = edge_index[&ekey(lp[i], lp[(i + 1) % n])];
            let e_prev = edge_index[&ekey(lp[(i + n - 1) % n], lp[i])];
            // corner quad (v, e_next, face, e_prev) keeps the CCW winding
            out.indices.extend_from_slice(&[v, e_next, f_idx]);
            out.indices.extend_from_slice(&[v, f_idx, e_prev]);
        }
    }
    // merged coplanar regions can leave interior vertices unreferenced
    compact(&mut out);
    out
}

/// Append a planar CCW polygon as flat-shaded triangles (ear clipping, so
/// non-convex faces like L-shaped floors triangulate correctly).
fn emit_polygon(out: &mut MeshData, poly: &[Vec3]) -> Option<()> {
    if poly.len() < 3 {
        return None;
    }
    // project onto the polygon plane, preserving orientation
    let normal = {
        let mut n = Vec3::ZERO;
        for i in 0..poly.len() {
            n += poly[i].cross(poly[(i + 1) % poly.len()]);
        }
        n.normalize_or_zero()
    };
    let u = if normal.x.abs() < 0.9 { Vec3::X } else { Vec3::Y };
    let u = (u - normal * u.dot(normal)).normalize_or_zero();
    let v = normal.cross(u);
    let flat: Vec<Vec2> = poly.iter().map(|p| Vec2::new(p.dot(u), p.dot(v))).collect();
    let tris = modeler_core::mesh::triangulate(&flat);
    if tris.is_empty() {
        return None;
    }
    let base = out.positions.len() as u32;
    out.positions.extend_from_slice(poly);
    for [x, y, z] in tris {
        out.indices.extend_from_slice(&[base + x, base + y, base + z]);
    }
    Some(())
}

/// Drop unreferenced vertices and recompute the normals. Seams follow the
/// remap; a seam whose vertices were rebuilt away is dropped.
fn compact(mesh: &mut MeshData) {
    let mut remap = vec![u32::MAX; mesh.positions.len()];
    let mut kept = Vec::new();
    for index in &mut mesh.indices {
        let old = *index as usize;
        if remap[old] == u32::MAX {
            remap[old] = kept.len() as u32;
            kept.push(mesh.positions[old]);
        }
        *index = remap[old];
    }
    mesh.positions = kept;
    mesh.seams = mesh
        .seams
        .iter()
        .filter_map(|&(a, b)| {
            let (a, b) = (remap[a as usize], remap[b as usize]);
            (a != u32::MAX && b != u32::MAX).then_some((a, b))
        })
        .collect();
    mesh.recompute_normals();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit_mode::build_topology;
    use modeler_core::Primitive;

    fn cube() -> (MeshData, Topology) {
        let mesh = Primitive::Cube { size: 2.0 }.generate(false);
        let topo = build_topology(&mesh);
        (mesh, topo)
    }

    /// A welded edge parallel to `axis_dir`, e.g. a top edge of the cube.
    fn find_edge(topo: &Topology, predicate: impl Fn(Vec3, Vec3) -> bool) -> (usize, usize) {
        *topo
            .edges
            .iter()
            .find(|&&(a, b)| predicate(topo.verts[a], topo.verts[b]))
            .expect("edge")
    }

    fn validate(m: &MeshData) {
        assert_eq!(m.positions.len(), m.normals.len());
        assert!(m.indices.len() % 3 == 0 && !m.indices.is_empty());
        for &i in &m.indices {
            assert!((i as usize) < m.positions.len());
        }
        for n in &m.normals {
            assert!((n.length() - 1.0).abs() < 1e-3, "bad normal {n:?}");
        }
    }

    /// Volume via the divergence theorem — the surgeries must keep the mesh
    /// closed and consistently wound.
    fn volume(m: &MeshData) -> f32 {
        m.indices
            .chunks_exact(3)
            .map(|t| {
                let (a, b, c) = (
                    m.positions[t[0] as usize],
                    m.positions[t[1] as usize],
                    m.positions[t[2] as usize],
                );
                a.dot(b.cross(c)) / 6.0
            })
            .sum()
    }

    #[test]
    fn cube_face_loops_are_ccw_quads() {
        let (_, topo) = cube();
        for f in 0..topo.faces.len() {
            let lp = face_loop(&topo, f).expect("cube faces are simple quads");
            assert_eq!(lp.len(), 4);
            let t = topo.tris[topo.faces[f].tris[0]];
            let n = (topo.verts[t[1]] - topo.verts[t[0]])
                .cross(topo.verts[t[2]] - topo.verts[t[0]]);
            assert!(polygon_area_vector(&lp, |w| topo.verts[w]).dot(n) > 0.0);
        }
    }

    #[test]
    fn loop_cut_on_a_cube_closes_the_ring() {
        let (mesh, topo) = cube();
        // a top edge along X: the ring runs around the 4 side faces
        let edge = find_edge(&topo, |a, b| {
            (a.z - 1.0).abs() < 1e-4 && (b.z - 1.0).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4
        });
        let cut = loop_cut(&mesh, &topo, edge, 1).expect("ring of quads");
        validate(&cut);
        assert!((volume(&cut) - 8.0).abs() < 1e-3, "volume {}", volume(&cut));

        let new_topo = build_topology(&cut);
        // the loop adds one welded vertex on each of the 4 cut edges
        assert_eq!(new_topo.verts.len(), 12);
        // cube edges: 12 + (4 ring edges split into 2) + 4 new loop segments
        assert_eq!(new_topo.edges.len(), 20);
        assert_eq!(new_topo.faces.len(), 10, "4 side quads split in two");
    }

    #[test]
    fn loop_cut_counts_scale_with_cuts() {
        let (mesh, topo) = cube();
        let edge = find_edge(&topo, |a, b| {
            (a.z - 1.0).abs() < 1e-4 && (b.z - 1.0).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4
        });
        for cuts in [2usize, 5] {
            let cut = loop_cut(&mesh, &topo, edge, cuts).expect("ring");
            validate(&cut);
            assert!((volume(&cut) - 8.0).abs() < 1e-3);
            let t = build_topology(&cut);
            assert_eq!(t.verts.len(), 8 + 4 * cuts);
            assert_eq!(t.faces.len(), 6 - 4 + 4 * (cuts + 1));
        }
    }

    #[test]
    fn loop_cut_keeps_smooth_spheres_smooth() {
        let mesh = Primitive::UvSphere { segments: 8, rings: 4, radius: 1.0 }.generate(true);
        let shared_before = mesh.positions.len();
        let topo = build_topology(&mesh);
        // an equator-adjacent edge going around a ring of quads
        let edge = find_edge(&topo, |a, b| {
            a.z.abs() > 1e-3 && (a.z - b.z).abs() < 1e-4 // horizontal edge off the poles
        });
        let cut = loop_cut(&mesh, &topo, edge, 1).expect("sphere quads form a ring");
        validate(&cut);
        // smooth sharing preserved: the rebuilt band reuses shared vertices,
        // so the count grows only by the newly inserted loop
        assert!(cut.positions.len() < shared_before + 2 * 8 + 2);
    }

    #[test]
    fn loop_cut_needs_a_quad() {
        // an icosphere has no quads at all
        let mesh = Primitive::IcoSphere { subdivisions: 1, radius: 1.0 }.generate(false);
        let topo = build_topology(&mesh);
        assert!(loop_cut(&mesh, &topo, topo.edges[0], 1).is_none());
    }

    #[test]
    fn bevel_chamfers_a_cube_edge() {
        let (mesh, topo) = cube();
        let edge = find_edge(&topo, |a, b| {
            (a.z - 1.0).abs() < 1e-4
                && (b.z - 1.0).abs() < 1e-4
                && (a.y - 1.0).abs() < 1e-4
                && (b.y - 1.0).abs() < 1e-4
        });
        let beveled = bevel_edge(&mesh, &topo, edge, 0.4, 1).expect("box corner");
        validate(&beveled);

        let t = build_topology(&beveled);
        // the two edge vertices split into four
        assert_eq!(t.verts.len(), 10);
        // one new chamfer face
        assert_eq!(t.faces.len(), 7);
        // the chamfer removes a 45° prism of material along the 2-long edge:
        // V = 8 - (0.4²/2) · 2
        let expect = 8.0 - 0.4 * 0.4 * 0.5 * 2.0;
        assert!((volume(&beveled) - expect).abs() < 1e-3, "volume {}", volume(&beveled));
        // width stays clamped to the geometric limit
        let wide = bevel_edge(&mesh, &topo, edge, 100.0, 1).expect("clamped");
        validate(&wide);
    }

    #[test]
    fn bevel_segments_round_the_edge() {
        let (mesh, topo) = cube();
        let edge = find_edge(&topo, |a, b| {
            (a.z - 1.0).abs() < 1e-4
                && (b.z - 1.0).abs() < 1e-4
                && (a.y - 1.0).abs() < 1e-4
                && (b.y - 1.0).abs() < 1e-4
        });
        for segments in [2usize, 4, 8] {
            let m = bevel_edge(&mesh, &topo, edge, 0.4, segments).expect("box corner");
            validate(&m);
            let t = build_topology(&m);
            // each endpoint splits into the profile's segments+1 vertices
            assert_eq!(t.verts.len(), 2 * segments + 8);
            assert_eq!(t.faces.len(), 6 + segments);
        }
        // rounding removes less material than the flat chamfer, approaching
        // the parabolic profile (w²/6 per unit of edge length)
        let flat = volume(&bevel_edge(&mesh, &topo, edge, 0.4, 1).unwrap());
        let s2 = volume(&bevel_edge(&mesh, &topo, edge, 0.4, 2).unwrap());
        let s8 = volume(&bevel_edge(&mesh, &topo, edge, 0.4, 8).unwrap());
        assert!(flat < s2 && s2 < s8 && s8 < 8.0, "{flat} < {s2} < {s8} < 8");
        assert!((s2 - (8.0 - 0.4 * 0.4 / 4.0 * 2.0)).abs() < 1e-3, "s2 {s2}");
        assert!((s8 - (8.0 - 0.4 * 0.4 / 6.0 * 2.0)).abs() < 0.01, "s8 {s8}");
    }

    #[test]
    fn bevel_limit_matches_short_adjacent_edges() {
        let (_, topo) = cube();
        let edge = find_edge(&topo, |a, b| {
            (a.z - 1.0).abs() < 1e-4 && (b.z - 1.0).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4
        });
        let limit = bevel_limit(&topo, edge).expect("cube edge");
        assert!((limit - 0.9 * 2.0).abs() < 1e-4);
    }

    #[test]
    fn subdivision_rounds_a_cube() {
        let (mesh, _) = cube();
        let sub = subdivide(&mesh, 1, true);
        validate(&sub);
        let t = build_topology(&sub);
        // V + F + E = 8 + 6 + 12 shared vertices, 6 faces × 4 corner quads
        assert_eq!(t.verts.len(), 26);
        assert_eq!(sub.indices.len() / 3, 48);
        // one Catmull-Clark round of a 2³ cube encloses exactly 10/3
        let v1 = volume(&sub);
        assert!((v1 - 10.0 / 3.0).abs() < 1e-3, "rounded cube volume {v1}");
        // more levels keep shrinking toward the smooth limit surface
        let v2 = volume(&subdivide(&mesh, 2, true));
        assert!(v2 < v1 && v2 > 2.5, "level 2 volume {v2}");
        // flat shading re-expands to per-face vertices
        let flat = subdivide(&mesh, 1, false);
        validate(&flat);
        assert_eq!(flat.positions.len(), flat.indices.len());
    }

    #[test]
    fn subdivision_respects_open_boundaries() {
        // a plane stays flat; its rim smooths inward like Blender's subsurf
        let mesh = Primitive::Plane { size: 2.0 }.generate(false);
        let sub = subdivide(&mesh, 1, true);
        validate(&sub);
        for p in &sub.positions {
            assert!(p.z.abs() < 1e-5, "stays planar: {p:?}");
        }
        // boundary edge midpoints stay on the old rim (extents hold), while
        // the corners pull in to (±0.75, ±0.75)
        let e = sub.extents();
        assert!((e.x - 2.0).abs() < 1e-4, "rim midpoints hold: {e:?}");
        let reach = sub
            .positions
            .iter()
            .map(|p| p.x.abs() + p.y.abs())
            .fold(0.0f32, f32::max);
        assert!((reach - 1.5).abs() < 1e-4, "corners round off: {reach}");
        // deterministic output: same input, same vertex order
        let again = subdivide(&mesh, 1, true);
        assert_eq!(sub, again);
    }

    /// The welded face whose vertices all sit at z = 1 (the cube top).
    fn top_face(topo: &Topology) -> usize {
        topo.faces
            .iter()
            .position(|f| f.verts.iter().all(|&v| (topo.verts[v].z - 1.0).abs() < 1e-4))
            .expect("top face")
    }

    #[test]
    fn extrude_pulls_a_cube_face_out() {
        let (mesh, topo) = cube();
        let out = extrude_face(&mesh, &topo, top_face(&topo), 0.5).expect("simple loop");
        validate(&out);
        // the 2×2 top face sweeps a 2×2×0.5 slab
        assert!((volume(&out) - 10.0).abs() < 1e-3, "volume {}", volume(&out));
        assert!((out.extents().z - 2.5).abs() < 1e-4);
        let t = build_topology(&out);
        assert_eq!(t.verts.len(), 12, "4 new top corners");
        assert_eq!(t.faces.len(), 10, "walls stay separate from the flush sides");
        assert_eq!(t.edges.len(), 20);
    }

    #[test]
    fn extrude_negative_pushes_the_face_in() {
        let (mesh, topo) = cube();
        let out = extrude_face(&mesh, &topo, top_face(&topo), -0.5).expect("simple loop");
        validate(&out);
        // the top sinks to z = 0.5; the rim (side faces) stays at z = 1
        assert!((volume(&out) - 6.0).abs() < 1e-3, "volume {}", volume(&out));
        assert!((out.extents().z - 2.0).abs() < 1e-4);
    }

    #[test]
    fn extrude_and_inset_at_zero_keep_the_mesh() {
        let (mesh, topo) = cube();
        let f = top_face(&topo);
        assert_eq!(extrude_face(&mesh, &topo, f, 0.0).as_ref(), Some(&mesh));
        assert_eq!(inset_face(&mesh, &topo, f, 0.0).as_ref(), Some(&mesh));
    }

    #[test]
    fn inset_rings_a_cube_face() {
        let (mesh, topo) = cube();
        let out = inset_face(&mesh, &topo, top_face(&topo), 0.4).expect("simple loop");
        validate(&out);
        assert!((volume(&out) - 8.0).abs() < 1e-3, "inset leaves the volume alone");
        let t = build_topology(&out);
        assert_eq!(t.verts.len(), 12, "4 inner corners");
        assert_eq!(t.faces.len(), 10, "5 untouched + 4 ring quads + inner");
        // uniform border thickness: inner corners at (±0.6, ±0.6, 1)
        for sx in [-1.0f32, 1.0] {
            for sy in [-1.0f32, 1.0] {
                let q = Vec3::new(0.6 * sx, 0.6 * sy, 1.0);
                assert!(
                    t.verts.iter().any(|v| (*v - q).length() < 1e-4),
                    "missing inner corner {q:?}"
                );
            }
        }
    }

    #[test]
    fn inset_limit_is_short_of_the_centroid_edge_distance() {
        let (_, topo) = cube();
        let limit = inset_limit(&topo, top_face(&topo)).expect("cube face");
        assert!((limit - 0.9).abs() < 1e-4, "0.9 × the 1 m centroid-edge gap, got {limit}");
        // the limit stays usable: the inner face survives at full width
        let (mesh, _) = cube();
        let out = inset_face(&mesh, &topo, top_face(&topo), limit).expect("valid at limit");
        validate(&out);
        assert_eq!(build_topology(&out).faces.len(), 10);
    }

    #[test]
    fn bevel_refuses_unsupported_corners() {
        // sphere vertices have more than three incident faces
        let mesh = Primitive::UvSphere { segments: 8, rings: 4, radius: 1.0 }.generate(false);
        let topo = build_topology(&mesh);
        assert!(bevel_edge(&mesh, &topo, topo.edges[0], 0.1, 1).is_none());
    }
}
