//! Procedural terrain: a layered noise stack evaluated on a height grid.
//!
//! A `Primitive::Terrain` carries its size / resolution / height / seed;
//! the layer stack lives on the OBJECT (`Object::terrain`), like wall
//! cutouts, so the primitive stays `Copy`. Height is a pure function of
//! `(x, y, seed, stack)` — no baked heightmap is stored in the scene file,
//! the mesh regenerates from the parameters. Editors must bump
//! `Object::mesh_revision` when they change the stack so the render and
//! physics caches resync.
//!
//! All noise is integer-lattice hashed (PCG-style), so the same seed gives
//! the same terrain on every platform.

use crate::mesh::MeshData;
use glam::{Vec2, Vec3};
use serde::{Deserialize, Serialize};

/// Grid quads per side, keeping vertex counts sane (513² max).
pub const MIN_RESOLUTION: u32 = 8;
pub const MAX_RESOLUTION: u32 = 512;

// --- deterministic hashed value noise ----------------------------------

/// 32-bit avalanche hash (PCG output permutation).
pub(crate) fn hash_u32(mut x: u32) -> u32 {
    x = x.wrapping_mul(0x2c1b_3c6d).rotate_right(15);
    x = x.wrapping_mul(0x297a_2d39);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2c1b_3c6d);
    x ^ (x >> 16)
}

/// Lattice hash → [0, 1).
pub(crate) fn hash2(ix: i32, iy: i32, seed: u32) -> f32 {
    let h = hash_u32(
        (ix as u32)
            .wrapping_mul(0x8da6_b343)
            .wrapping_add((iy as u32).wrapping_mul(0xd816_3841))
            .wrapping_add(seed.wrapping_mul(0xcb1a_b31f)),
    );
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Lattice hash → [-1, 1).
fn shash2(ix: i32, iy: i32, seed: u32) -> f32 {
    hash2(ix, iy, seed) * 2.0 - 1.0
}

/// Value noise with its analytic gradient: `(value in [-1,1], d/dx, d/dy)`.
/// Quintic interpolation, so the gradient is continuous across cells.
fn vnoise_d(p: Vec2, seed: u32) -> (f32, Vec2) {
    let ix = p.x.floor() as i32;
    let iy = p.y.floor() as i32;
    let f = Vec2::new(p.x - p.x.floor(), p.y - p.y.floor());

    let a = shash2(ix, iy, seed);
    let b = shash2(ix + 1, iy, seed);
    let c = shash2(ix, iy + 1, seed);
    let d = shash2(ix + 1, iy + 1, seed);

    // quintic fade and its derivative
    let u = f * f * f * (f * (f * 6.0 - Vec2::splat(15.0)) + Vec2::splat(10.0));
    let du = 30.0 * f * f * (f - Vec2::ONE) * (f - Vec2::ONE);

    let k = a - b - c + d;
    let value = a + (b - a) * u.x + (c - a) * u.y + k * u.x * u.y;
    let grad = Vec2::new(
        ((b - a) + k * u.y) * du.x,
        ((c - a) + k * u.x) * du.y,
    );
    (value, grad)
}

/// Fixed rotation between octaves (kills axis-aligned artifacts).
const ROT: [Vec2; 2] = [Vec2::new(0.8, -0.6), Vec2::new(0.6, 0.8)];

fn rot(p: Vec2) -> Vec2 {
    Vec2::new(ROT[0].dot(p), ROT[1].dot(p))
}

/// Fractal Brownian motion in [0, 1], derivative-aware.
///
/// `erosion` suppresses octave amplitude on accumulated slopes (valleys stay
/// smooth, ridges keep detail — the cheap "eroded" look); `warp` bends each
/// octave's domain by the accumulated gradient (flow-like streaks).
fn fbm(p: Vec2, seed: u32, octaves: u32, gain: f32, lacunarity: f32, erosion: f32, warp: f32) -> f32 {
    let mut q = p;
    let mut amp = 1.0f32;
    let mut sum = 0.0f32;
    let mut norm = 0.0f32;
    let mut dsum = Vec2::ZERO;
    for i in 0..octaves.clamp(1, 10) {
        let (n, g) = vnoise_d(q + dsum * warp, seed.wrapping_add(i));
        dsum += g * amp;
        let damp = 1.0 / (1.0 + erosion * 4.0 * dsum.length_squared());
        sum += amp * n * damp;
        norm += amp * damp;
        amp *= gain;
        q = rot(q) * lacunarity;
    }
    // octave normalization compresses value noise into the mid-range;
    // the 1.1 stretch restores a practical 0..1 span (clamped extremes
    // are rare)
    (0.5 + 1.1 * (sum / norm.max(1e-6))).clamp(0.0, 1.0)
}

/// Ridged multifractal in [0, 1]: folded noise with spectral weighting.
fn ridged(p: Vec2, seed: u32, octaves: u32, gain: f32, lacunarity: f32, sharpness: f32) -> f32 {
    let mut q = p;
    let mut amp = 1.0f32;
    let mut sum = 0.0f32;
    let mut norm = 0.0f32;
    let mut carry = 1.0f32;
    for i in 0..octaves.clamp(1, 10) {
        let (n, _) = vnoise_d(q, seed.wrapping_add(i));
        let r = (1.0 - n.abs()).clamp(0.0, 1.0).powf(sharpness.max(0.2));
        sum += amp * r * carry;
        norm += amp;
        carry = (r * 1.4).clamp(0.0, 1.0); // peaks breed detail, valleys stay calm
        amp *= gain;
        q = rot(q) * lacunarity;
    }
    // same mid-range stretch story as fbm
    (1.3 * sum / norm.max(1e-6)).clamp(0.0, 1.0)
}

/// Billowy (cloud/dune bellies) fractal in [0, 1].
fn billow(p: Vec2, seed: u32, octaves: u32, gain: f32, lacunarity: f32) -> f32 {
    let mut q = p;
    let mut amp = 1.0f32;
    let mut sum = 0.0f32;
    let mut norm = 0.0f32;
    for i in 0..octaves.clamp(1, 10) {
        let (n, _) = vnoise_d(q, seed.wrapping_add(i));
        sum += amp * n.abs();
        norm += amp;
        amp *= gain;
        q = rot(q) * lacunarity;
    }
    // same mid-range stretch story as fbm
    (1.3 * sum / norm.max(1e-6)).clamp(0.0, 1.0)
}

/// Position of a Voronoi feature point in its cell.
fn cell_point(ix: i32, iy: i32, seed: u32, jitter: f32) -> Vec2 {
    Vec2::new(
        ix as f32 + 0.5 + (hash2(ix, iy, seed) - 0.5) * jitter,
        iy as f32 + 0.5 + (hash2(ix, iy, seed.wrapping_add(77)) - 0.5) * jitter,
    )
}

// --- the layer stack ----------------------------------------------------

/// How a Voronoi layer turns cell geometry into height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoronoiOutput {
    /// Flat random value per cell (plateaus / patchwork).
    CellValue,
    /// Distance to the nearest feature point (bubbly domes).
    Distance,
    /// Distance to the nearest cell border (ridged cell walls).
    Edge,
}

impl VoronoiOutput {
    pub const ALL: [VoronoiOutput; 3] =
        [VoronoiOutput::CellValue, VoronoiOutput::Distance, VoronoiOutput::Edge];

    pub fn label(self) -> &'static str {
        match self {
            VoronoiOutput::CellValue => "Cell value",
            VoronoiOutput::Distance => "Distance",
            VoronoiOutput::Edge => "Edge",
        }
    }
}

/// One layer's generator. `scale` is always the feature size in meters.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayerKind {
    /// Rolling fractal hills. `erosion` smooths valleys, `warp` streaks flow.
    Fbm { scale: f32, octaves: u32, gain: f32, lacunarity: f32, erosion: f32, warp: f32 },
    /// Sharp mountain ridges.
    Ridged { scale: f32, octaves: u32, gain: f32, lacunarity: f32, sharpness: f32 },
    /// Soft puffy mounds.
    Billow { scale: f32, octaves: u32, gain: f32, lacunarity: f32 },
    /// Single-octave value noise.
    Value { scale: f32 },
    /// Cellular pattern (plateaus, cracked ground, cell walls).
    Voronoi { scale: f32, jitter: f32, output: VoronoiOutput },
    /// Impact craters: signed bowls with raised rims. `density` 0..1 is the
    /// chance each `scale`-sized cell holds one.
    Crater { scale: f32, density: f32, depth: f32, rim: f32 },
    /// Transverse sand dunes marching along `direction_deg`.
    Dune { scale: f32, direction_deg: f32, sharpness: f32 },
    /// A meandering river channel (pairs with the Carve blend).
    Flow { scale: f32, direction_deg: f32, width: f32, meander: f32 },
    /// Flat offset (raise / lower everything the mask lets through).
    Constant { value: f32 },
    /// A single stamped landform placed at `(x, y)` in terrain-local meters:
    /// mountain, ridge, valley, plateau or crater. `rotation_deg`/`aspect`
    /// orient and stretch the footprint (ridges); `detail` roughens the
    /// profile with noise.
    Shape {
        shape: ShapeKind,
        x: f32,
        y: f32,
        radius: f32,
        rotation_deg: f32,
        aspect: f32,
        falloff: f32,
        detail: f32,
    },
    /// MODIFIER: bends the sample position of every layer BELOW it.
    DomainWarp { scale: f32, strength: f32, octaves: u32 },
    /// MODIFIER: quantizes the accumulated height into steps.
    Terrace { steps: u32, smoothness: f32 },
}

impl LayerKind {
    pub fn label(&self) -> &'static str {
        match self {
            LayerKind::Fbm { .. } => "Hills (fBm)",
            LayerKind::Ridged { .. } => "Ridges",
            LayerKind::Billow { .. } => "Billow",
            LayerKind::Value { .. } => "Value noise",
            LayerKind::Voronoi { .. } => "Voronoi",
            LayerKind::Crater { .. } => "Craters",
            LayerKind::Dune { .. } => "Dunes",
            LayerKind::Flow { .. } => "River",
            LayerKind::Constant { .. } => "Constant",
            LayerKind::Shape { shape, .. } => shape.label(),
            LayerKind::DomainWarp { .. } => "Domain warp",
            LayerKind::Terrace { .. } => "Terrace",
        }
    }

    /// Modifiers transform the stack instead of adding a height field.
    pub fn is_modifier(&self) -> bool {
        matches!(self, LayerKind::DomainWarp { .. } | LayerKind::Terrace { .. })
    }

    /// Catalog of one default instance per kind, in Add-layer menu order.
    pub fn catalog() -> Vec<LayerKind> {
        vec![
            LayerKind::Fbm { scale: 60.0, octaves: 5, gain: 0.5, lacunarity: 2.0, erosion: 0.4, warp: 0.0 },
            LayerKind::Ridged { scale: 90.0, octaves: 5, gain: 0.5, lacunarity: 2.1, sharpness: 1.6 },
            LayerKind::Billow { scale: 50.0, octaves: 4, gain: 0.5, lacunarity: 2.0 },
            LayerKind::Value { scale: 30.0 },
            LayerKind::Voronoi { scale: 40.0, jitter: 0.9, output: VoronoiOutput::Edge },
            LayerKind::Crater { scale: 30.0, density: 0.5, depth: 0.6, rim: 0.25 },
            LayerKind::Dune { scale: 18.0, direction_deg: 30.0, sharpness: 1.8 },
            LayerKind::Flow { scale: 80.0, direction_deg: 0.0, width: 4.0, meander: 8.0 },
            LayerKind::Constant { value: 0.2 },
            LayerKind::DomainWarp { scale: 70.0, strength: 12.0, octaves: 3 },
            LayerKind::Terrace { steps: 8, smoothness: 0.4 },
        ]
    }

    /// A default stamp of the given shape, centered on the terrain.
    pub fn shape(shape: ShapeKind) -> LayerKind {
        LayerKind::Shape {
            shape,
            x: 0.0,
            y: 0.0,
            radius: 25.0,
            rotation_deg: 0.0,
            aspect: if shape == ShapeKind::Ridge { 3.0 } else { 1.0 },
            falloff: 0.5,
            detail: 0.3,
        }
    }
}

/// The stampable landforms of `LayerKind::Shape`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeKind {
    Mountain,
    Ridge,
    /// Negative mountain — defaults to the Subtract blend when added.
    Valley,
    Plateau,
    Crater,
}

impl ShapeKind {
    pub const ALL: [ShapeKind; 5] = [
        ShapeKind::Mountain,
        ShapeKind::Ridge,
        ShapeKind::Valley,
        ShapeKind::Plateau,
        ShapeKind::Crater,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ShapeKind::Mountain => "Mountain",
            ShapeKind::Ridge => "Ridge",
            ShapeKind::Valley => "Valley",
            ShapeKind::Plateau => "Plateau",
            ShapeKind::Crater => "Crater (stamp)",
        }
    }
}

/// How a layer's field combines with the accumulated height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlendMode {
    Add,
    Subtract,
    Multiply,
    Max,
    Min,
    Replace,
    /// Subtract only the positive part — dig channels without raising banks.
    Carve,
    /// Ease the accumulated height toward the layer value (plateau maker).
    Flatten,
}

impl BlendMode {
    pub const ALL: [BlendMode; 8] = [
        BlendMode::Add,
        BlendMode::Subtract,
        BlendMode::Multiply,
        BlendMode::Max,
        BlendMode::Min,
        BlendMode::Replace,
        BlendMode::Carve,
        BlendMode::Flatten,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BlendMode::Add => "Add",
            BlendMode::Subtract => "Subtract",
            BlendMode::Multiply => "Multiply",
            BlendMode::Max => "Max",
            BlendMode::Min => "Min",
            BlendMode::Replace => "Replace",
            BlendMode::Carve => "Carve",
            BlendMode::Flatten => "Flatten",
        }
    }
}

/// Band with soft edges: full weight inside [min, max], easing to zero over
/// `falloff` outside it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub min: f32,
    pub max: f32,
    pub falloff: f32,
    #[serde(default)]
    pub invert: bool,
}

impl Band {
    fn weight(&self, v: f32) -> f32 {
        let f = self.falloff.max(1e-4);
        let lo = smoothstep((self.min - f, self.min), v);
        let hi = 1.0 - smoothstep((self.max, self.max + f), v);
        let w = lo * hi;
        if self.invert { 1.0 - w } else { w }
    }
}

fn smoothstep((e0, e1): (f32, f32), x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Where a layer applies. All present masks multiply together.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct LayerMask {
    /// Band on the height accumulated so far (0..1 of the terrain height).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<Band>,
    /// Band on the slope accumulated so far (rise/run; 0 flat, 1 = 45°).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slope: Option<Band>,
    /// Broken-up coverage: fBm at `scale` thresholded softly at `threshold`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise: Option<NoiseMask>,
}

impl LayerMask {
    pub fn is_empty(&self) -> bool {
        self.height.is_none() && self.slope.is_none() && self.noise.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NoiseMask {
    pub scale: f32,
    pub threshold: f32,
    pub softness: f32,
    #[serde(default)]
    pub invert: bool,
}

/// One entry in the terrain's layer stack, applied top to bottom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerrainLayer {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub name: String,
    pub kind: LayerKind,
    pub blend: BlendMode,
    /// Layer amplitude, in units of the terrain height (1 = full height).
    pub amount: f32,
    /// Decorrelates this layer from others of the same kind.
    #[serde(default)]
    pub seed_offset: u32,
    #[serde(default, skip_serializing_if = "LayerMask::is_empty")]
    pub mask: LayerMask,
}

fn default_true() -> bool {
    true
}

impl TerrainLayer {
    pub fn new(kind: LayerKind) -> Self {
        let blend = match kind {
            LayerKind::Flow { .. } => BlendMode::Carve,
            // valleys dig; carving keeps the surrounding terrain untouched
            LayerKind::Shape { shape: ShapeKind::Valley, .. } => BlendMode::Carve,
            _ => BlendMode::Add,
        };
        Self {
            enabled: true,
            name: kind.label().to_string(),
            kind,
            blend,
            amount: match kind {
                LayerKind::DomainWarp { .. } | LayerKind::Terrace { .. } => 1.0,
                _ => 0.5,
            },
            seed_offset: 0,
            mask: LayerMask::default(),
        }
    }
}

/// The terrain's generator state, stored on the object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerrainData {
    #[serde(default)]
    pub layers: Vec<TerrainLayer>,
    /// Hand-sculpted height offsets (meters), added on top of the stack.
    /// Non-destructive: clearing it restores the pure procedural surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sculpt: Option<SculptLayer>,
    /// Procedural biome coloring, baked to an albedo texture by the
    /// renderer. `None` = plain material color (the pre-phase-4 look).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<TerrainColor>,
    /// Hand-painted biome patches, blended over the procedural coloring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paint: Option<PaintLayer>,
    /// Baked erosion offsets (meters), applied after the sculpt:
    /// `final = stack + sculpt + erosion.delta × strength`. Baked once on
    /// demand (`bake_erosion`), toggleable, and stale-checkable when the
    /// surface it was baked against has changed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erosion: Option<ErosionLayer>,
    /// Still water table: a surface at a fixed height filling every basin
    /// and carved channel below it. Purely visual — no height or collision
    /// impact — so it stays out of `BaseKey`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub water: Option<WaterLayer>,
    /// Memoized stack evaluation, so sculpt strokes don't re-run the noise
    /// layers every frame. Never serialized, never cloned, always equal.
    #[serde(skip)]
    cache: BaseCache,
}

/// Interior-mutable memo of the last `eval_base_grid` run. Deliberately
/// inert for Clone/PartialEq so undo snapshots and change detection see
/// two logically-equal terrains as equal regardless of cache state.
#[derive(Default)]
struct BaseCache(std::cell::RefCell<Option<(BaseKey, Vec<f32>)>>);

#[derive(PartialEq, Clone)]
struct BaseKey {
    layers: Vec<TerrainLayer>,
    seed: u32,
    resolution: u32,
    size: f32,
    height: f32,
}

impl Clone for BaseCache {
    fn clone(&self) -> Self {
        Self::default() // caches don't travel with copies
    }
}

impl PartialEq for BaseCache {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for BaseCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BaseCache")
    }
}

// --- hand sculpting ------------------------------------------------------

/// Which way a sculpt brush pushes the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrushMode {
    Raise,
    Lower,
    /// Blur the surface toward the local average.
    Smooth,
    /// Pull the surface toward the height under the cursor at stroke start.
    Flatten,
}

impl BrushMode {
    pub const ALL: [BrushMode; 4] =
        [BrushMode::Raise, BrushMode::Lower, BrushMode::Smooth, BrushMode::Flatten];

    pub fn label(self) -> &'static str {
        match self {
            BrushMode::Raise => "Raise",
            BrushMode::Lower => "Lower",
            BrushMode::Smooth => "Smooth",
            BrushMode::Flatten => "Flatten",
        }
    }
}

/// Hand-sculpted signed height offsets in meters, on its own `(res+1)²`
/// vertex grid over the terrain footprint. Stored losslessly in scene files
/// as base64 of the raw little-endian f32 bytes (see the serde impls).
#[derive(Debug, Clone, PartialEq)]
pub struct SculptLayer {
    /// Grid quads per side (vertices = resolution + 1 per side).
    pub resolution: u32,
    /// Row-major `(resolution+1)²` offsets, meters.
    pub deltas: Vec<f32>,
}

impl SculptLayer {
    pub fn new(resolution: u32) -> Self {
        let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        let n = res as usize + 1;
        Self { resolution: res, deltas: vec![0.0; n * n] }
    }

    pub fn is_empty(&self) -> bool {
        self.deltas.iter().all(|d| *d == 0.0)
    }

    /// Bilinear sample at normalized coordinates (0..1 across the grid).
    pub fn sample_normalized(&self, u: f32, v: f32) -> f32 {
        sample_grid_normalized(&self.deltas, self.resolution as usize, u, v)
    }

    /// Re-grid to a new resolution (bilinear), keeping the sculpt intact
    /// when the terrain's mesh resolution changes.
    pub fn resample(&self, resolution: u32) -> Self {
        let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        if res == self.resolution {
            return self.clone();
        }
        let n = res as usize + 1;
        let mut out = Self::new(res);
        for iy in 0..n {
            for ix in 0..n {
                let u = ix as f32 / res as f32;
                let v = iy as f32 / res as f32;
                out.deltas[iy * n + ix] = self.sample_normalized(u, v);
            }
        }
        out
    }

    /// Apply one brush dab at `center` (terrain-local meters, footprint
    /// `[-size/2, size/2]²`). `amount` is meters of push for Raise/Lower and
    /// a 0..1 lerp factor for Smooth/Flatten; `falloff` (0..1) is the soft
    /// fraction of the radius; `current` is the current TOTAL height grid at
    /// this layer's resolution (Smooth/Flatten read it; Raise/Lower ignore
    /// it); `flatten_target` is the height Flatten pulls toward.
    #[allow(clippy::too_many_arguments)]
    pub fn brush(
        &mut self,
        mode: BrushMode,
        center: Vec2,
        radius: f32,
        amount: f32,
        falloff: f32,
        size: f32,
        current: &[f32],
        flatten_target: f32,
    ) {
        let res = self.resolution as usize;
        let n = res + 1;
        let step = size / res as f32;
        let radius = radius.max(step * 0.5);
        // touched vertex range
        let to_idx = |w: f32| ((w / size + 0.5) * res as f32).round() as i32;
        let x0 = (to_idx(center.x - radius)).clamp(0, res as i32) as usize;
        let x1 = (to_idx(center.x + radius)).clamp(0, res as i32) as usize;
        let y0 = (to_idx(center.y - radius)).clamp(0, res as i32) as usize;
        let y1 = (to_idx(center.y + radius)).clamp(0, res as i32) as usize;
        let inner = 1.0 - falloff.clamp(0.05, 1.0);
        let have_current = current.len() == n * n;

        for iy in y0..=y1 {
            for ix in x0..=x1 {
                let w = Vec2::new(
                    (ix as f32 / res as f32 - 0.5) * size,
                    (iy as f32 / res as f32 - 0.5) * size,
                );
                let t = (w - center).length() / radius;
                if t >= 1.0 {
                    continue;
                }
                // 1 inside the hard core, easing to 0 at the rim
                let weight = 1.0 - smoothstep((inner, 1.0), t);
                let idx = iy * n + ix;
                match mode {
                    BrushMode::Raise => self.deltas[idx] += amount * weight,
                    BrushMode::Lower => self.deltas[idx] -= amount * weight,
                    BrushMode::Smooth if have_current => {
                        let avg = {
                            let mut sum = 0.0;
                            let mut count = 0.0;
                            for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1), (0, 0)] {
                                let jx = ix as i32 + dx;
                                let jy = iy as i32 + dy;
                                if jx >= 0 && jx <= res as i32 && jy >= 0 && jy <= res as i32 {
                                    sum += current[jy as usize * n + jx as usize];
                                    count += 1.0;
                                }
                            }
                            sum / count
                        };
                        let lerp = (amount * weight).clamp(0.0, 1.0);
                        self.deltas[idx] += (avg - current[idx]) * lerp;
                    }
                    BrushMode::Flatten if have_current => {
                        let lerp = (amount * weight).clamp(0.0, 1.0);
                        self.deltas[idx] += (flatten_target - current[idx]) * lerp;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Serialized form: raw little-endian f32 deltas, base64 (lossless — the
/// grid survives any number of save/load cycles bit-exactly).
#[derive(Serialize, Deserialize)]
struct SculptLayerRepr {
    resolution: u32,
    data: String,
}

impl Serialize for SculptLayer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut bytes = Vec::with_capacity(self.deltas.len() * 4);
        for d in &self.deltas {
            bytes.extend_from_slice(&d.to_le_bytes());
        }
        SculptLayerRepr {
            resolution: self.resolution,
            data: base64_encode(&bytes),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for SculptLayer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let repr = SculptLayerRepr::deserialize(d)?;
        let bytes = base64_decode(&repr.data)
            .ok_or_else(|| serde::de::Error::custom("bad sculpt base64"))?;
        let res = repr.resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        let n = res as usize + 1;
        if bytes.len() != n * n * 4 {
            return Err(serde::de::Error::custom("sculpt data size mismatch"));
        }
        let deltas = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .map(|v| if v.is_finite() { v } else { 0.0 })
            .collect();
        Ok(Self { resolution: res, deltas })
    }
}

/// A baked erosion result: signed offsets (meters) on an `(res+1)²` grid,
/// blended in as `delta × strength`. `bake_stamp` fingerprints the surface
/// it was computed against, so the UI can flag a stale bake after stack or
/// sculpt edits. Serialized like the sculpt layer (lossless base64 f32).
#[derive(Debug, Clone, PartialEq)]
pub struct ErosionLayer {
    pub enabled: bool,
    /// Blend factor: 0 = off, 1 = the full baked result. Live-adjustable.
    pub strength: f32,
    /// The recipe used for the bake (re-bakes reuse it).
    pub settings: crate::erosion::ErosionSettings,
    pub resolution: u32,
    /// Row-major `(resolution+1)²` offsets, meters.
    pub delta: Vec<f32>,
    /// `grid_stamp` of the pre-erosion surface at bake time.
    pub bake_stamp: u64,
}

#[derive(Serialize, Deserialize)]
struct ErosionLayerRepr {
    enabled: bool,
    strength: f32,
    settings: crate::erosion::ErosionSettings,
    resolution: u32,
    bake_stamp: u64,
    data: String,
}

impl Serialize for ErosionLayer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut bytes = Vec::with_capacity(self.delta.len() * 4);
        for d in &self.delta {
            bytes.extend_from_slice(&d.to_le_bytes());
        }
        ErosionLayerRepr {
            enabled: self.enabled,
            strength: self.strength,
            settings: self.settings,
            resolution: self.resolution,
            bake_stamp: self.bake_stamp,
            data: base64_encode(&bytes),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for ErosionLayer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let repr = ErosionLayerRepr::deserialize(d)?;
        let bytes = base64_decode(&repr.data)
            .ok_or_else(|| serde::de::Error::custom("bad erosion base64"))?;
        let res = repr.resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        let n = res as usize + 1;
        if bytes.len() != n * n * 4 {
            return Err(serde::de::Error::custom("erosion data size mismatch"));
        }
        let delta = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .map(|v| if v.is_finite() { v } else { 0.0 })
            .collect();
        Ok(Self {
            enabled: repr.enabled,
            strength: repr.strength,
            settings: repr.settings,
            resolution: res,
            delta,
            bake_stamp: repr.bake_stamp,
        })
    }
}

// --- biome coloring ------------------------------------------------------

/// The paintable biome channels, indexing into [`TerrainColor`]'s palette.
pub const PAINT_CHANNELS: [&str; 6] = ["Grass", "Dry grass", "Rock", "Cliff", "Snow", "Sand"];

/// Hand-painted biome override: per-vertex palette slot + blend weight on
/// its own grid. Baked into the albedo AFTER the procedural rules, so
/// painted patches sit on top of (and blend into) the automatic coloring.
#[derive(Debug, Clone, PartialEq)]
pub struct PaintLayer {
    /// Grid quads per side (vertices = resolution + 1 per side).
    pub resolution: u32,
    /// Palette slot per vertex (index into `PAINT_CHANNELS`).
    pub slots: Vec<u8>,
    /// Blend weight per vertex, 0..1.
    pub weights: Vec<f32>,
}

impl PaintLayer {
    pub fn new(resolution: u32) -> Self {
        let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        let n = res as usize + 1;
        Self {
            resolution: res,
            slots: vec![0; n * n],
            weights: vec![0.0; n * n],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.weights.iter().all(|w| *w == 0.0)
    }

    /// One paint dab: positive `amount` paints `channel` in (and claims the
    /// vertex's slot), negative erases weight. Same footprint/falloff
    /// semantics as the sculpt brush.
    pub fn brush(
        &mut self,
        center: Vec2,
        radius: f32,
        amount: f32,
        falloff: f32,
        size: f32,
        channel: u8,
    ) {
        let res = self.resolution as usize;
        let n = res + 1;
        let step = size / res as f32;
        let radius = radius.max(step * 0.5);
        let to_idx = |w: f32| ((w / size + 0.5) * res as f32).round() as i32;
        let x0 = to_idx(center.x - radius).clamp(0, res as i32) as usize;
        let x1 = to_idx(center.x + radius).clamp(0, res as i32) as usize;
        let y0 = to_idx(center.y - radius).clamp(0, res as i32) as usize;
        let y1 = to_idx(center.y + radius).clamp(0, res as i32) as usize;
        let inner = 1.0 - falloff.clamp(0.05, 1.0);
        for iy in y0..=y1 {
            for ix in x0..=x1 {
                let w = Vec2::new(
                    (ix as f32 / res as f32 - 0.5) * size,
                    (iy as f32 / res as f32 - 0.5) * size,
                );
                let t = (w - center).length() / radius;
                if t >= 1.0 {
                    continue;
                }
                let weight = 1.0 - smoothstep((inner, 1.0), t);
                let idx = iy * n + ix;
                if amount > 0.0 {
                    // painting claims the slot; an existing different color
                    // fades out before the new one fades in
                    if self.slots[idx] != channel {
                        let drop = amount * weight;
                        if self.weights[idx] <= drop {
                            self.slots[idx] = channel;
                            self.weights[idx] = drop - self.weights[idx];
                        } else {
                            self.weights[idx] -= drop;
                        }
                    } else {
                        self.weights[idx] = (self.weights[idx] + amount * weight).min(1.0);
                    }
                } else {
                    self.weights[idx] = (self.weights[idx] + amount * weight).max(0.0);
                }
            }
        }
    }

    /// Nearest-vertex slot and bilinear weight at normalized coordinates.
    fn sample(&self, u: f32, v: f32) -> (u8, f32) {
        let res = self.resolution as usize;
        let n = res + 1;
        let x = ((u.clamp(0.0, 1.0)) * res as f32).round() as usize;
        let y = ((v.clamp(0.0, 1.0)) * res as f32).round() as usize;
        let slot = self.slots[y.min(res) * n + x.min(res)];
        let weight = sample_grid_normalized(&self.weights, res, u, v);
        (slot, weight.clamp(0.0, 1.0))
    }
}

/// Serialized form: slots as base64 bytes, weights as base64 f32 LE.
#[derive(Serialize, Deserialize)]
struct PaintLayerRepr {
    resolution: u32,
    slots: String,
    weights: String,
}

impl Serialize for PaintLayer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut wbytes = Vec::with_capacity(self.weights.len() * 4);
        for w in &self.weights {
            wbytes.extend_from_slice(&w.to_le_bytes());
        }
        PaintLayerRepr {
            resolution: self.resolution,
            slots: base64_encode(&self.slots),
            weights: base64_encode(&wbytes),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for PaintLayer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let repr = PaintLayerRepr::deserialize(d)?;
        let res = repr.resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        let n = res as usize + 1;
        let slots = base64_decode(&repr.slots)
            .ok_or_else(|| serde::de::Error::custom("bad paint slots base64"))?;
        let wbytes = base64_decode(&repr.weights)
            .ok_or_else(|| serde::de::Error::custom("bad paint weights base64"))?;
        if slots.len() != n * n || wbytes.len() != n * n * 4 {
            return Err(serde::de::Error::custom("paint data size mismatch"));
        }
        let weights = wbytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .map(|v| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 })
            .collect();
        Ok(Self { resolution: res, slots, weights })
    }
}

/// The terrain's procedural coloring: a small rule chain over height and
/// slope (grass → rock by steepness, snow above the line, sand near the
/// base, noise mottling), baked to an albedo texture by the renderer.
/// Colors are sRGB 0..1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TerrainColor {
    pub grass: [f32; 3],
    /// Mottled into the grass by `variation` noise.
    pub dry_grass: [f32; 3],
    pub rock: [f32; 3],
    /// Steepest faces (a darker/greyer rock).
    pub cliff: [f32; 3],
    pub snow: [f32; 3],
    pub sand: [f32; 3],
    /// Meters above the base plane below which sand takes over.
    pub sand_height: f32,
    /// Slope (rise/run) where rock starts winning over vegetation.
    pub rock_slope: f32,
    /// Fraction of the terrain height (0..1) where snow begins on flat ground.
    pub snow_line: f32,
    /// Slope above which snow cannot stick.
    pub snow_slope_max: f32,
    /// Patchiness of the grass mottling, 0..1.
    pub variation: f32,
}

impl Default for TerrainColor {
    /// Temperate meadow-and-rock ("Meadow").
    fn default() -> Self {
        Self {
            grass: [0.29, 0.42, 0.19],
            dry_grass: [0.48, 0.45, 0.22],
            rock: [0.42, 0.38, 0.33],
            cliff: [0.32, 0.30, 0.28],
            snow: [0.92, 0.93, 0.95],
            sand: [0.65, 0.58, 0.42],
            sand_height: 0.6,
            rock_slope: 0.55,
            snow_line: 0.75,
            snow_slope_max: 0.9,
            variation: 0.5,
        }
    }
}

impl TerrainColor {
    /// Named looks (UI combo and the `terrain_color` command parameter).
    pub fn presets() -> Vec<(&'static str, TerrainColor)> {
        let meadow = TerrainColor::default();
        vec![
            ("Meadow", meadow),
            (
                "Autumn",
                TerrainColor {
                    grass: [0.42, 0.34, 0.14],
                    dry_grass: [0.55, 0.35, 0.16],
                    rock: [0.40, 0.35, 0.30],
                    snow_line: 0.8,
                    ..meadow
                },
            ),
            (
                "Desert",
                TerrainColor {
                    grass: [0.70, 0.58, 0.38],
                    dry_grass: [0.62, 0.48, 0.30],
                    rock: [0.55, 0.38, 0.26],
                    cliff: [0.45, 0.28, 0.20],
                    sand: [0.76, 0.65, 0.45],
                    sand_height: 2.5,
                    snow_line: 2.0, // never
                    variation: 0.6,
                    ..meadow
                },
            ),
            (
                "Arctic",
                TerrainColor {
                    grass: [0.55, 0.58, 0.55],
                    dry_grass: [0.45, 0.48, 0.46],
                    rock: [0.35, 0.36, 0.38],
                    cliff: [0.25, 0.26, 0.29],
                    sand: [0.5, 0.52, 0.54],
                    snow_line: 0.25,
                    snow_slope_max: 1.2,
                    ..meadow
                },
            ),
            (
                "Volcanic",
                TerrainColor {
                    grass: [0.25, 0.22, 0.20],
                    dry_grass: [0.35, 0.28, 0.22],
                    rock: [0.20, 0.18, 0.17],
                    cliff: [0.12, 0.11, 0.11],
                    snow: [0.85, 0.83, 0.80], // ash
                    sand: [0.30, 0.26, 0.22],
                    snow_line: 2.0, // never
                    variation: 0.7,
                    ..meadow
                },
            ),
            (
                "Alien",
                TerrainColor {
                    grass: [0.30, 0.20, 0.42],
                    dry_grass: [0.45, 0.25, 0.50],
                    rock: [0.25, 0.28, 0.38],
                    cliff: [0.16, 0.18, 0.28],
                    snow: [0.75, 0.95, 0.90],
                    sand: [0.40, 0.32, 0.50],
                    variation: 0.65,
                    ..meadow
                },
            ),
        ]
    }

    pub fn preset(name: &str) -> Option<TerrainColor> {
        Self::presets()
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, c)| c)
    }

    /// Palette entry for a paint channel (see [`PAINT_CHANNELS`]).
    pub fn entry(&self, slot: u8) -> [f32; 3] {
        match slot {
            0 => self.grass,
            1 => self.dry_grass,
            2 => self.rock,
            3 => self.cliff,
            4 => self.snow,
            _ => self.sand,
        }
    }

    /// Hash of every field, for render-cache stamping.
    pub fn stamp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for c in [self.grass, self.dry_grass, self.rock, self.cliff, self.snow, self.sand] {
            for v in c {
                v.to_bits().hash(&mut h);
            }
        }
        for v in [
            self.sand_height,
            self.rock_slope,
            self.snow_line,
            self.snow_slope_max,
            self.variation,
        ] {
            v.to_bits().hash(&mut h);
        }
        h.finish()
    }

    /// Evaluate the rule chain at one point. `h` is the height in meters,
    /// `h01` height / terrain height, `slope` rise/run, `jitter` a noise
    /// value in [-1, 1] that breaks up the thresholds.
    fn shade(&self, h: f32, h01: f32, slope: f32, jitter: f32, mottle: f32) -> [f32; 3] {
        let mix3 = |a: [f32; 3], b: [f32; 3], t: f32| {
            let t = t.clamp(0.0, 1.0);
            [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
            ]
        };
        // vegetation with noise mottling
        let mut color = mix3(
            self.grass,
            self.dry_grass,
            (0.5 + mottle * 1.2 * self.variation).clamp(0.0, 1.0),
        );
        // sand band near the base plane
        let sand_w = 1.0 - smoothstep(
            (self.sand_height, self.sand_height + 0.8),
            h + jitter * 0.4,
        );
        color = mix3(color, self.sand, sand_w);
        // rock by steepness, hard cliffs above that
        let rock_w = smoothstep(
            (self.rock_slope, self.rock_slope + 0.35),
            slope + jitter * 0.08,
        );
        color = mix3(color, self.rock, rock_w);
        let cliff_w = smoothstep(
            (self.rock_slope + 0.55, self.rock_slope + 1.1),
            slope + jitter * 0.08,
        );
        color = mix3(color, self.cliff, cliff_w);
        // snow above the line, only where it can stick
        let snow_w = smoothstep(
            (self.snow_line, self.snow_line + 0.12),
            h01 + jitter * 0.05,
        ) * (1.0 - smoothstep((self.snow_slope_max, self.snow_slope_max + 0.4), slope));
        color = mix3(color, self.snow, snow_w);
        // micro grain so large faces don't read flat
        let grain = 1.0 + mottle * 0.06;
        [
            (color[0] * grain).clamp(0.0, 1.0),
            (color[1] * grain).clamp(0.0, 1.0),
            (color[2] * grain).clamp(0.0, 1.0),
        ]
    }
}

impl TerrainData {
    /// Bake the biome color rules into an sRGB RGBA8 image covering the
    /// terrain footprint (texel (0,0) at the -X/-Y corner, matching the
    /// mesh's 0..1 UVs). Returns `None` when coloring is disabled.
    pub fn bake_color(
        &self,
        seed: u32,
        resolution: u32,
        size: f32,
        height: f32,
        tex_res: u32,
    ) -> Option<(u32, u32, Vec<u8>)> {
        let color = self.color?;
        let grid = self.eval_grid(seed, resolution, size, height);
        let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        let n = res as usize + 1;
        let tex = tex_res.clamp(64, 4096) as usize;
        let step = size / res as f32;
        let h_max = height.max(1e-3);

        let mut out = vec![0u8; tex * tex * 4];
        for ty in 0..tex {
            let v = (ty as f32 + 0.5) / tex as f32;
            for tx in 0..tex {
                let u = (tx as f32 + 0.5) / tex as f32;
                let h = sample_grid_normalized(&grid, res as usize, u, v);
                // slope from the grid around this texel
                let gx = (u * res as f32) as usize;
                let gy = (v * res as f32) as usize;
                let (gx, gy) = (gx.min(res as usize - 1), gy.min(res as usize - 1));
                let idx = gy * n + gx;
                let dx = (grid[idx + 1] - grid[idx]) / step;
                let dy = (grid[idx + n] - grid[idx]) / step;
                let slope = (dx * dx + dy * dy).sqrt();
                // two noise fields: threshold jitter and broad mottling
                let p = Vec2::new((u - 0.5) * size, (v - 0.5) * size);
                let jitter =
                    vnoise_d(p / 7.0, seed.wrapping_add(0x0c01)).0;
                let mottle = fbm(p / 24.0, seed.wrapping_add(0x0c02), 3, 0.5, 2.0, 0.0, 0.0)
                    - 0.5;
                let mut rgb = color.shade(h, h / h_max, slope, jitter, mottle);
                if let Some(paint) = &self.paint {
                    let (slot, w) = paint.sample(u, v);
                    if w > 0.0 {
                        let target = color.entry(slot);
                        for (c, t) in rgb.iter_mut().zip(target) {
                            *c += (t - *c) * w;
                        }
                    }
                }
                let o = (ty * tex + tx) * 4;
                out[o] = (rgb[0] * 255.0 + 0.5) as u8;
                out[o + 1] = (rgb[1] * 255.0 + 0.5) as u8;
                out[o + 2] = (rgb[2] * 255.0 + 0.5) as u8;
                out[o + 3] = 255;
            }
        }
        Some((tex as u32, tex as u32, out))
    }

    /// Bake the water surface tint into an sRGB RGBA8 image covering the
    /// terrain footprint (same texel convention as [`Self::bake_color`]).
    /// Shallow fades to deep with depth below the level; a noise-broken
    /// foam band hugs the shoreline. Returns `None` when water is off.
    pub fn bake_water_color(
        &self,
        seed: u32,
        resolution: u32,
        size: f32,
        height: f32,
        tex_res: u32,
    ) -> Option<(u32, u32, Vec<u8>)> {
        let water = self.water.filter(|w| w.enabled)?;
        let grid = self.eval_grid(seed, resolution, size, height);
        let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        let tex = tex_res.clamp(64, 4096) as usize;

        let mix3 = |a: [f32; 3], b: [f32; 3], t: f32| {
            let t = t.clamp(0.0, 1.0);
            [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
            ]
        };
        let falloff = water.depth_falloff.max(0.05);
        let mut out = vec![0u8; tex * tex * 4];
        for ty in 0..tex {
            let v = (ty as f32 + 0.5) / tex as f32;
            for tx in 0..tex {
                let u = (tx as f32 + 0.5) / tex as f32;
                let depth = water.level - sample_grid_normalized(&grid, res as usize, u, v);
                let mut rgb =
                    mix3(water.shallow, water.deep, smoothstep((0.0, falloff), depth));
                if water.foam_width > 1e-3 {
                    // the band waves in and out with noise so the shoreline
                    // doesn't read as a hard contour line
                    let p = Vec2::new((u - 0.5) * size, (v - 0.5) * size);
                    let ripple = vnoise_d(p / 3.0, seed.wrapping_add(0x0aa0)).0;
                    let w = water.foam_width * (0.55 + 0.45 * ripple);
                    let foam = 1.0 - smoothstep((0.0, w.max(0.02)), depth);
                    rgb = mix3(rgb, [0.93, 0.96, 0.97], foam * 0.85);
                }
                let o = (ty * tex + tx) * 4;
                out[o] = (rgb[0] * 255.0 + 0.5) as u8;
                out[o + 1] = (rgb[1] * 255.0 + 0.5) as u8;
                out[o + 2] = (rgb[2] * 255.0 + 0.5) as u8;
                out[o + 3] = 255;
            }
        }
        Some((tex as u32, tex as u32, out))
    }
}

/// A still water table over the terrain: a flat (gently rippled) surface at
/// `level` meters above the base plane, meshed only over ground that sits
/// below it — so it fills lakes, basins and carved river beds. Rendered as
/// its own translucent submesh; never collides and never moves heights.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaterLayer {
    pub enabled: bool,
    /// Surface height, meters above the terrain base plane (z = 0).
    /// Negative reaches into channels carved below the base.
    pub level: f32,
    /// sRGB tint where the water is shallow.
    pub shallow: [f32; 3],
    /// sRGB tint at `depth_falloff` meters and deeper.
    pub deep: [f32; 3],
    /// Meters of depth over which shallow fades to deep.
    pub depth_falloff: f32,
    /// Width of the foam band along the shoreline, meters (0 = none).
    pub foam_width: f32,
    /// Surface opacity 0..1 (the renderer dithers it; TAA resolves).
    pub opacity: f32,
    /// Micro-roughness: low values give mirror-like reflections.
    pub roughness: f32,
    /// Static ripple amplitude in meters (0 = glass flat). Used while the
    /// simulation is stopped; playback animates the wave field instead.
    pub ripple: f32,
    /// Gerstner wave parameters for the physics-play water simulation
    /// (a terrain with its physics flag set animates these while ▶).
    pub waves: crate::water::WaveParams,
}

impl Default for WaterLayer {
    fn default() -> Self {
        Self {
            enabled: true,
            level: 0.4,
            shallow: [0.22, 0.55, 0.60],
            deep: [0.03, 0.15, 0.28],
            depth_falloff: 3.0,
            foam_width: 0.6,
            opacity: 0.55,
            roughness: 0.08,
            ripple: 0.06,
            waves: crate::water::WaveParams::default(),
        }
    }
}

impl WaterLayer {
    /// Hash of every field, for render-cache stamping (same idea as
    /// [`TerrainColor::stamp`]).
    pub fn stamp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.enabled.hash(&mut h);
        for c in [self.shallow, self.deep] {
            for v in c {
                v.to_bits().hash(&mut h);
            }
        }
        for v in [
            self.level,
            self.depth_falloff,
            self.foam_width,
            self.opacity,
            self.roughness,
            self.ripple,
        ] {
            v.to_bits().hash(&mut h);
        }
        h.finish()
    }
}

// --- prop scattering -----------------------------------------------------

/// One scatter run's rules. Candidates sit on a hash-jittered grid of
/// `cell_size`-meter cells; each passes through slope/height/paint gates,
/// macro patchiness and a density lottery — all deterministic per seed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScatterParams {
    /// Candidate spacing in meters (the minimum distance floor).
    pub cell_size: f32,
    /// 0..1 acceptance chance per candidate.
    pub density: f32,
    /// Scatter seed — same seed re-places the same props.
    pub seed: u32,
    /// Reject ground steeper than this (rise/run).
    pub max_slope: f32,
    /// Only place between these heights (meters above the base plane).
    pub height_min: f32,
    pub height_max: f32,
    /// Per-prop uniform scale range.
    pub scale_min: f32,
    pub scale_max: f32,
    /// Local-max spacing test (trees): a candidate only wins if its hash
    /// beats all 8 neighbours', spreading placements apart.
    pub spacing: bool,
    /// 0 = even coverage, 1 = strong clustering by low-frequency noise.
    pub patchiness: f32,
    /// Skip ground hand-painted rock/cliff/snow/sand (vegetation avoids
    /// painted clearings; rocks ignore this).
    pub avoid_paint: bool,
}

impl Default for ScatterParams {
    fn default() -> Self {
        Self {
            cell_size: 6.0,
            density: 0.5,
            seed: 1,
            max_slope: 0.7,
            height_min: -1000.0,
            height_max: 1000.0,
            scale_min: 0.8,
            scale_max: 1.4,
            spacing: true,
            patchiness: 0.5,
            avoid_paint: true,
        }
    }
}

/// One placement, in terrain-LOCAL space (apply the terrain's transform —
/// or parent the prop to the terrain — to get world coordinates).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub position: Vec3,
    /// Random yaw in radians.
    pub yaw: f32,
    pub scale: f32,
}

impl TerrainData {
    /// Deterministically scatter prop placements over this terrain.
    /// Capped at `max` (nearest-the-center candidates win beyond it, by
    /// simple truncation of the row-major sweep).
    pub fn scatter(
        &self,
        terrain_seed: u32,
        resolution: u32,
        size: f32,
        height: f32,
        params: &ScatterParams,
        max: usize,
    ) -> Vec<Placement> {
        let grid = self.eval_grid(terrain_seed, resolution, size, height);
        let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        let n = res as usize + 1;
        let step = size / res as f32;
        let cell = params.cell_size.max(0.5);
        let cells = ((size / cell).floor() as i32).max(1);
        let seed = params.seed.wrapping_mul(0x9e37_79b9).wrapping_add(0x5ca7);
        let mut out = Vec::new();

        'rows: for cy in 0..cells {
            'candidates: for cx in 0..cells {
                if out.len() >= max {
                    break 'rows;
                }
                let priority = hash2(cx, cy, seed);
                // anti-clumping: only the locally strongest candidate places
                if params.spacing {
                    for (dx, dy) in [
                        (-1, -1), (0, -1), (1, -1),
                        (-1, 0), (1, 0),
                        (-1, 1), (0, 1), (1, 1),
                    ] {
                        if hash2(cx + dx, cy + dy, seed) > priority {
                            continue 'candidates;
                        }
                    }
                }
                // jittered position inside the cell, kept off the rim
                let jx = (hash2(cx, cy, seed.wrapping_add(11)) - 0.5) * 0.84;
                let jy = (hash2(cx, cy, seed.wrapping_add(12)) - 0.5) * 0.84;
                let x = ((cx as f32 + 0.5 + jx) * cell - 0.5 * size)
                    .clamp(-0.49 * size, 0.49 * size);
                let y = ((cy as f32 + 0.5 + jy) * cell - 0.5 * size)
                    .clamp(-0.49 * size, 0.49 * size);

                // macro patchiness: clustered coverage instead of confetti
                let patch = if params.patchiness > 0.0 {
                    let p = fbm(
                        Vec2::new(x, y) / (size * 0.25).max(1.0),
                        seed.wrapping_add(0xbead),
                        3,
                        0.5,
                        2.0,
                        0.0,
                        0.0,
                    );
                    (1.0 - params.patchiness) + params.patchiness * (p * 1.6 - 0.2)
                } else {
                    1.0
                };
                // the lottery uses its OWN hash: the spacing test above
                // keeps locally-maximal `priority` values, which would lose
                // every `priority < density` draw
                let lottery = hash2(cx, cy, seed.wrapping_add(15));
                if lottery >= (params.density * patch).clamp(0.0, 1.0) {
                    continue;
                }

                // ground rules
                let u = x / size + 0.5;
                let v = y / size + 0.5;
                let h = sample_grid_normalized(&grid, res as usize, u, v);
                if h < params.height_min || h > params.height_max {
                    continue;
                }
                let gx = ((u * res as f32) as usize).min(res as usize - 1);
                let gy = ((v * res as f32) as usize).min(res as usize - 1);
                let idx = gy * n + gx;
                let dzdx = (grid[idx + 1] - grid[idx]) / step;
                let dzdy = (grid[idx + n] - grid[idx]) / step;
                if (dzdx * dzdx + dzdy * dzdy).sqrt() > params.max_slope {
                    continue;
                }
                if params.avoid_paint {
                    if let Some(paint) = &self.paint {
                        let (slot, w) = paint.sample(u, v);
                        // channels 2..=5: rock, cliff, snow, sand
                        if slot >= 2 && w > 0.5 {
                            continue;
                        }
                    }
                }

                out.push(Placement {
                    position: Vec3::new(x, y, h),
                    yaw: hash2(cx, cy, seed.wrapping_add(13)) * std::f32::consts::TAU,
                    scale: params.scale_min
                        + (params.scale_max - params.scale_min)
                            * hash2(cx, cy, seed.wrapping_add(14)),
                });
            }
        }
        out
    }
}

/// Order-and-bit-exact fingerprint of an evaluated grid (stale detection).
pub fn grid_stamp(grid: &[f32]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    grid.len().hash(&mut h);
    for v in grid {
        v.to_bits().hash(&mut h);
    }
    h.finish()
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let v = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64_ALPHABET[(v >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in text.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = BASE64_ALPHABET.iter().position(|&a| a == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Bilinear sample of a row-major `(res+1)²` grid at normalized (0..1)
/// coordinates, clamped at the borders.
pub fn sample_grid_normalized(grid: &[f32], resolution: usize, u: f32, v: f32) -> f32 {
    let n = resolution + 1;
    if grid.len() != n * n {
        return 0.0;
    }
    let fx = (u.clamp(0.0, 1.0)) * resolution as f32;
    let fy = (v.clamp(0.0, 1.0)) * resolution as f32;
    let x0 = (fx.floor() as usize).min(resolution - 1 + 1).min(resolution);
    let y0 = (fy.floor() as usize).min(resolution);
    let x1 = (x0 + 1).min(resolution);
    let y1 = (y0 + 1).min(resolution);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;
    let a = grid[y0 * n + x0];
    let b = grid[y0 * n + x1];
    let c = grid[y1 * n + x0];
    let d = grid[y1 * n + x1];
    a + (b - a) * tx + (c - a) * ty + (a - b - c + d) * tx * ty
}

/// Bilinear height at terrain-local `(x, y)` meters (footprint
/// `[-size/2, size/2]²`) of a full evaluated grid.
pub fn sample_height(grid: &[f32], resolution: u32, size: f32, x: f32, y: f32) -> f32 {
    sample_grid_normalized(
        grid,
        resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION) as usize,
        x / size + 0.5,
        y / size + 0.5,
    )
}

/// Intersect a local-space ray with the evaluated height grid: coarse march
/// at half-cell steps, then bisection. Returns the local-space hit point.
pub fn raycast_grid(
    grid: &[f32],
    resolution: u32,
    size: f32,
    origin: Vec3,
    dir: Vec3,
) -> Option<Vec3> {
    let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
    let dir = dir.normalize_or_zero();
    if dir == Vec3::ZERO {
        return None;
    }
    let half = 0.5 * size;
    let (zmin, zmax) = grid
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    // clip the ray to the grid's bounding box (padded a little)
    let mut t0 = 0.0f32;
    let mut t1 = f32::INFINITY;
    for (o, d, lo, hi) in [
        (origin.x, dir.x, -half, half),
        (origin.y, dir.y, -half, half),
        (origin.z, dir.z, zmin - 0.1, zmax + 0.1),
    ] {
        if d.abs() < 1e-9 {
            if o < lo || o > hi {
                return None;
            }
            continue;
        }
        let (ta, tb) = ((lo - o) / d, (hi - o) / d);
        t0 = t0.max(ta.min(tb));
        t1 = t1.min(ta.max(tb));
    }
    if t0 > t1 {
        return None;
    }
    let above = |t: f32| {
        let p = origin + dir * t;
        p.z - sample_height(grid, res, size, p.x, p.y)
    };
    let step = (size / res as f32) * 0.5;
    let mut t_prev = t0;
    let mut d_prev = above(t0);
    if d_prev < 0.0 {
        // starting under the surface: report the entry point
        let p = origin + dir * t0;
        return Some(Vec3::new(p.x, p.y, sample_height(grid, res, size, p.x, p.y)));
    }
    let mut t = t0 + step;
    while t <= t1 + step {
        let tc = t.min(t1);
        let d = above(tc);
        if d <= 0.0 {
            // bracketed: bisect
            let (mut lo, mut hi) = (t_prev, tc);
            for _ in 0..12 {
                let mid = 0.5 * (lo + hi);
                if above(mid) > 0.0 {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let p = origin + dir * hi;
            return Some(Vec3::new(p.x, p.y, sample_height(grid, res, size, p.x, p.y)));
        }
        t_prev = tc;
        d_prev = d;
        if tc >= t1 {
            break;
        }
        t += step;
    }
    let _ = d_prev;
    None
}

impl Default for TerrainData {
    /// The out-of-the-box terrain: warped rolling hills with ridged peaks
    /// on the upper slopes.
    fn default() -> Self {
        let mut ridges = TerrainLayer::new(LayerKind::Ridged {
            scale: 110.0,
            octaves: 5,
            gain: 0.5,
            lacunarity: 2.1,
            sharpness: 1.6,
        });
        ridges.amount = 0.55;
        ridges.mask.height = Some(Band { min: 0.25, max: 1.5, falloff: 0.2, invert: false });
        Self {
            sculpt: None,
            erosion: None,
            color: Some(TerrainColor::default()),
            paint: None,
            water: None,
            cache: Default::default(),
            layers: vec![
                TerrainLayer::new(LayerKind::DomainWarp { scale: 90.0, strength: 14.0, octaves: 3 }),
                TerrainLayer {
                    amount: 0.5,
                    ..TerrainLayer::new(LayerKind::Fbm {
                        scale: 70.0,
                        octaves: 6,
                        gain: 0.5,
                        lacunarity: 2.0,
                        erosion: 0.45,
                        warp: 0.0,
                    })
                },
                ridges,
            ],
        }
    }
}

impl TerrainData {
    /// Named starter stacks (Add ▸ Terrain presets and the `terrain_preset`
    /// command parameter).
    pub fn presets() -> Vec<(&'static str, TerrainData)> {
        let layer = |kind, blend, amount| TerrainLayer {
            blend,
            amount,
            ..TerrainLayer::new(kind)
        };
        let masked = |mut l: TerrainLayer, mask: LayerMask| {
            l.mask = mask;
            l
        };
        let height_band = |min, max, falloff| LayerMask {
            height: Some(Band { min, max, falloff, invert: false }),
            ..LayerMask::default()
        };

        vec![
            ("Hills", TerrainData::default()),
            (
                "Alpine",
                TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(),
                    layers: vec![
                        layer(LayerKind::DomainWarp { scale: 120.0, strength: 18.0, octaves: 3 }, BlendMode::Add, 1.0),
                        layer(LayerKind::Ridged { scale: 130.0, octaves: 6, gain: 0.52, lacunarity: 2.1, sharpness: 2.0 }, BlendMode::Add, 0.85),
                        layer(LayerKind::Fbm { scale: 30.0, octaves: 4, gain: 0.5, lacunarity: 2.0, erosion: 0.6, warp: 0.0 }, BlendMode::Add, 0.12),
                    ],
                },
            ),
            (
                "Dunes",
                TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(),
                    layers: vec![
                        layer(LayerKind::Billow { scale: 120.0, octaves: 3, gain: 0.5, lacunarity: 2.0 }, BlendMode::Add, 0.25),
                        layer(LayerKind::DomainWarp { scale: 60.0, strength: 6.0, octaves: 2 }, BlendMode::Add, 1.0),
                        layer(LayerKind::Dune { scale: 16.0, direction_deg: 30.0, sharpness: 1.8 }, BlendMode::Add, 0.35),
                    ],
                },
            ),
            (
                "Archipelago",
                TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(),
                    layers: vec![
                        layer(LayerKind::Constant { value: 1.0 }, BlendMode::Subtract, 0.35),
                        layer(LayerKind::DomainWarp { scale: 100.0, strength: 20.0, octaves: 3 }, BlendMode::Add, 1.0),
                        layer(LayerKind::Fbm { scale: 90.0, octaves: 5, gain: 0.5, lacunarity: 2.0, erosion: 0.3, warp: 0.0 }, BlendMode::Add, 0.9),
                    ],
                },
            ),
            (
                "Canyon",
                TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(),
                    layers: vec![
                        // high base plateau, stepped strata, then the river
                        // digs deep through all of it
                        layer(LayerKind::Constant { value: 1.0 }, BlendMode::Add, 0.55),
                        layer(LayerKind::Fbm { scale: 90.0, octaves: 4, gain: 0.5, lacunarity: 2.0, erosion: 0.2, warp: 0.0 }, BlendMode::Add, 0.35),
                        layer(LayerKind::Terrace { steps: 8, smoothness: 0.25 }, BlendMode::Add, 1.0),
                        layer(LayerKind::Flow { scale: 100.0, direction_deg: 20.0, width: 6.0, meander: 14.0 }, BlendMode::Carve, 0.9),
                    ],
                },
            ),
            (
                "Volcanic",
                TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(),
                    layers: vec![
                        layer(LayerKind::Ridged { scale: 80.0, octaves: 5, gain: 0.5, lacunarity: 2.2, sharpness: 2.4 }, BlendMode::Add, 0.6),
                        masked(
                            layer(LayerKind::Crater { scale: 45.0, density: 0.45, depth: 0.8, rim: 0.35 }, BlendMode::Add, 0.5),
                            height_band(0.2, 1.5, 0.15),
                        ),
                    ],
                },
            ),
            (
                "Rolling",
                TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(),
                    layers: vec![
                        layer(LayerKind::DomainWarp { scale: 80.0, strength: 10.0, octaves: 2 }, BlendMode::Add, 1.0),
                        layer(LayerKind::Fbm { scale: 60.0, octaves: 5, gain: 0.45, lacunarity: 2.0, erosion: 0.5, warp: 0.0 }, BlendMode::Add, 0.4),
                    ],
                },
            ),
            (
                "Craters",
                TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(),
                    layers: vec![
                        layer(LayerKind::Fbm { scale: 70.0, octaves: 4, gain: 0.5, lacunarity: 2.0, erosion: 0.0, warp: 0.0 }, BlendMode::Add, 0.2),
                        layer(LayerKind::Crater { scale: 35.0, density: 0.6, depth: 0.7, rim: 0.3 }, BlendMode::Add, 0.6),
                        layer(LayerKind::Crater { scale: 12.0, density: 0.4, depth: 0.5, rim: 0.25 }, BlendMode::Add, 0.15),
                    ],
                },
            ),
        ]
    }

    pub fn preset(name: &str) -> Option<TerrainData> {
        Self::presets()
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, d)| d)
    }

    /// Evaluate the terrain over an `(n+1)²` vertex grid covering
    /// `[-size/2, size/2]²`, row-major, in meters: the layer stack (memoized
    /// — repeat calls with unchanged layers reuse the last run, which is
    /// what keeps sculpt strokes cheap) plus the sculpt offsets, plus the
    /// baked erosion delta scaled by its strength.
    pub fn eval_grid(&self, seed: u32, resolution: u32, size: f32, height: f32) -> Vec<f32> {
        let mut out = self.eval_pre_erosion(seed, resolution, size, height);
        if let Some(erosion) = self.erosion.as_ref().filter(|e| e.enabled && e.strength != 0.0)
        {
            let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION) as usize;
            let n = res + 1;
            let s = erosion.strength;
            if erosion.resolution as usize == res {
                for (o, d) in out.iter_mut().zip(&erosion.delta) {
                    *o += d * s;
                }
            } else {
                // bake at another resolution: bilinear resample on the fly
                let er = erosion.resolution as usize;
                for iy in 0..n {
                    for ix in 0..n {
                        let u = ix as f32 / res as f32;
                        let v = iy as f32 / res as f32;
                        out[iy * n + ix] +=
                            sample_grid_normalized(&erosion.delta, er, u, v) * s;
                    }
                }
            }
        }
        out
    }

    /// The surface erosion runs on and is compared against for staleness:
    /// layer stack (memoized) + sculpt, without any erosion applied.
    pub fn eval_pre_erosion(&self, seed: u32, resolution: u32, size: f32, height: f32) -> Vec<f32> {
        let key = BaseKey {
            layers: self.layers.clone(),
            seed,
            resolution,
            size,
            height,
        };
        let mut out = {
            let mut slot = self.cache.0.borrow_mut();
            match slot.as_ref() {
                Some((cached_key, grid)) if *cached_key == key => grid.clone(),
                _ => {
                    let grid = self.eval_base_grid(seed, resolution, size, height);
                    *slot = Some((key, grid.clone()));
                    grid
                }
            }
        };
        if let Some(sculpt) = &self.sculpt {
            let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION) as usize;
            let n = res + 1;
            for iy in 0..n {
                for ix in 0..n {
                    let u = ix as f32 / res as f32;
                    let v = iy as f32 / res as f32;
                    out[iy * n + ix] += sculpt.sample_normalized(u, v);
                }
            }
        }
        out
    }

    /// Run the erosion simulation against the current stack + sculpt and
    /// store the result as the (non-destructive) erosion layer. A previous
    /// layer's enabled/strength survive the re-bake.
    pub fn bake_erosion(
        &mut self,
        seed: u32,
        resolution: u32,
        size: f32,
        height: f32,
        settings: crate::erosion::ErosionSettings,
    ) {
        let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION);
        let pre = self.eval_pre_erosion(seed, res, size, height);
        let stamp = grid_stamp(&pre);
        // the sim runs on normalized heights so presets behave identically
        // at every terrain height
        let h = height.max(1e-3);
        let normalized: Vec<f32> = pre.iter().map(|v| v / h).collect();
        let cell_norm = (size / res as f32) / h;
        let delta_norm =
            crate::erosion::erode_grid(&normalized, res, cell_norm, &settings, seed);
        let (enabled, strength) = self
            .erosion
            .as_ref()
            .map(|e| (e.enabled, e.strength))
            .unwrap_or((true, 1.0));
        self.erosion = Some(ErosionLayer {
            enabled,
            strength,
            settings,
            resolution: res,
            delta: delta_norm.into_iter().map(|d| d * h).collect(),
            bake_stamp: stamp,
        });
    }

    /// True when an erosion bake exists but the surface under it (stack,
    /// sculpt, seed, resolution or size) has changed since — the carved
    /// channels no longer match the terrain they were carved into.
    pub fn erosion_stale(&self, seed: u32, resolution: u32, size: f32, height: f32) -> bool {
        let Some(erosion) = &self.erosion else {
            return false;
        };
        if erosion.resolution != resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION) {
            return true;
        }
        grid_stamp(&self.eval_pre_erosion(seed, resolution, size, height)) != erosion.bake_stamp
    }

    /// The pure layer-stack evaluation (no sculpt, no cache).
    fn eval_base_grid(&self, seed: u32, resolution: u32, size: f32, height: f32) -> Vec<f32> {
        let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION) as usize;
        let n = res + 1;
        let step = size / res as f32;
        let world = |i: usize| (i as f32 / res as f32 - 0.5) * size;

        let mut acc = vec![0.0f32; n * n];
        // domain-warp offset per vertex, in meters, applied to layers below
        let mut warp: Vec<Vec2> = Vec::new();

        // slope of the accumulated height (rise/run), for slope masks —
        // recomputed lazily only for layers that ask for it
        let slope_of = |acc: &[f32], idx: usize, ix: usize, iy: usize| -> f32 {
            let xm = acc[idx - if ix > 0 { 1 } else { 0 }];
            let xp = acc[idx + if ix < res { 1 } else { 0 }];
            let ym = acc[idx - if iy > 0 { n } else { 0 }];
            let yp = acc[idx + if iy < res { n } else { 0 }];
            let denom = |lo: usize, hi: usize| ((hi - lo).max(1) as f32) * step;
            let dx = (xp - xm) * height / denom(ix.saturating_sub(1), (ix + 1).min(res));
            let dy = (yp - ym) * height / denom(iy.saturating_sub(1), (iy + 1).min(res));
            (dx * dx + dy * dy).sqrt()
        };

        for layer in self.layers.iter().filter(|l| l.enabled) {
            let seed = seed
                .wrapping_add(hash_u32(layer.seed_offset.wrapping_add(0x9e37_79b9)));

            match layer.kind {
                LayerKind::DomainWarp { scale, strength, octaves } => {
                    let s = scale.max(0.01);
                    if warp.is_empty() {
                        warp = vec![Vec2::ZERO; n * n];
                    }
                    for iy in 0..n {
                        for ix in 0..n {
                            let p = Vec2::new(world(ix), world(iy)) / s;
                            let wx = fbm(p + Vec2::new(13.7, 41.3), seed, octaves, 0.5, 2.0, 0.0, 0.0);
                            let wy = fbm(p + Vec2::new(87.2, 9.1), seed, octaves, 0.5, 2.0, 0.0, 0.0);
                            warp[iy * n + ix] +=
                                Vec2::new(wx - 0.5, wy - 0.5) * 2.0 * strength * layer.amount;
                        }
                    }
                    continue;
                }
                LayerKind::Terrace { steps, smoothness } => {
                    let steps = steps.clamp(2, 64) as f32;
                    let sm = smoothness.clamp(0.0, 1.0);
                    for v in acc.iter_mut() {
                        let t = *v * steps;
                        let base = t.floor();
                        let frac = t - base;
                        // sharpen the transition between steps, keep a soft lip
                        let shaped = smoothstep((0.5 - 0.5 * sm.max(1e-3), 0.5 + 0.5 * sm.max(1e-3)), frac);
                        let stepped = (base + shaped) / steps;
                        *v = *v + (stepped - *v) * layer.amount.clamp(0.0, 1.0);
                    }
                    continue;
                }
                _ => {}
            }

            let needs_slope = layer.mask.slope.is_some();
            for iy in 0..n {
                for ix in 0..n {
                    let idx = iy * n + ix;
                    let raw = Vec2::new(world(ix), world(iy));
                    let mut pw = raw;
                    // hand-placed stamps stay where the user put them —
                    // domain warp must not slide them around
                    if !matches!(layer.kind, LayerKind::Shape { .. }) {
                        if let Some(w) = warp.get(idx) {
                            pw += *w;
                        }
                    }

                    let v = sample_kind(&layer.kind, pw, seed);

                    // masks read the terrain built so far
                    let mut mask = 1.0f32;
                    if let Some(band) = layer.mask.height {
                        mask *= band.weight(acc[idx]);
                    }
                    if needs_slope {
                        if let Some(band) = layer.mask.slope {
                            mask *= band.weight(slope_of(&acc, idx, ix, iy));
                        }
                    }
                    if let Some(nm) = layer.mask.noise {
                        let np = pw / nm.scale.max(0.01);
                        let field = fbm(np, seed.wrapping_add(0x5f5f), 3, 0.5, 2.0, 0.0, 0.0);
                        let s = nm.softness.max(1e-3);
                        let w = smoothstep((nm.threshold - s, nm.threshold + s), field);
                        mask *= if nm.invert { 1.0 - w } else { w };
                    }
                    if mask <= 0.0 {
                        continue;
                    }

                    let a = layer.amount;
                    let acc_v = acc[idx];
                    let blended = match layer.blend {
                        BlendMode::Add => acc_v + v * a,
                        BlendMode::Subtract => acc_v - v * a,
                        BlendMode::Multiply => acc_v * (1.0 + (v - 0.5) * 2.0 * a),
                        BlendMode::Max => acc_v.max(v * a),
                        BlendMode::Min => acc_v.min(v * a),
                        BlendMode::Replace => v * a,
                        BlendMode::Carve => acc_v - (v * a).max(0.0),
                        BlendMode::Flatten => acc_v + (v - acc_v) * a.clamp(0.0, 1.0),
                    };
                    acc[idx] = acc_v + (blended - acc_v) * mask;
                }
            }
        }

        // final height in meters; a little negative room lets rivers dig
        // below the base plane
        for v in acc.iter_mut() {
            *v = v.clamp(-0.5, 1.5) * height;
        }
        acc
    }
}

/// One field sample for the non-modifier layer kinds, in the layer's
/// documented range (fractals [0,1], craters signed, constant as-is).
fn sample_kind(kind: &LayerKind, pw: Vec2, seed: u32) -> f32 {
    match *kind {
        LayerKind::Fbm { scale, octaves, gain, lacunarity, erosion, warp } => {
            fbm(pw / scale.max(0.01), seed, octaves, gain, lacunarity, erosion, warp)
        }
        LayerKind::Ridged { scale, octaves, gain, lacunarity, sharpness } => {
            ridged(pw / scale.max(0.01), seed, octaves, gain, lacunarity, sharpness)
        }
        LayerKind::Billow { scale, octaves, gain, lacunarity } => {
            billow(pw / scale.max(0.01), seed, octaves, gain, lacunarity)
        }
        LayerKind::Value { scale } => {
            0.5 + 0.5 * vnoise_d(pw / scale.max(0.01), seed).0
        }
        LayerKind::Voronoi { scale, jitter, output } => {
            let p = pw / scale.max(0.01);
            let ix = p.x.floor() as i32;
            let iy = p.y.floor() as i32;
            let mut f1 = f32::INFINITY;
            let mut f2 = f32::INFINITY;
            let mut nearest = (ix, iy);
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let c = (ix + dx, iy + dy);
                    let d = (cell_point(c.0, c.1, seed, jitter) - p).length();
                    if d < f1 {
                        f2 = f1;
                        f1 = d;
                        nearest = c;
                    } else if d < f2 {
                        f2 = d;
                    }
                }
            }
            match output {
                VoronoiOutput::CellValue => hash2(nearest.0, nearest.1, seed.wrapping_add(31)),
                VoronoiOutput::Distance => f1.clamp(0.0, 1.0),
                VoronoiOutput::Edge => (f2 - f1).clamp(0.0, 1.0),
            }
        }
        LayerKind::Crater { scale, density, depth, rim } => {
            let p = pw / scale.max(0.01);
            let ix = p.x.floor() as i32;
            let iy = p.y.floor() as i32;
            let mut h = 0.0f32;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (cx, cy) = (ix + dx, iy + dy);
                    if hash2(cx, cy, seed.wrapping_add(5)) > density.clamp(0.0, 1.0) {
                        continue;
                    }
                    let radius = 0.25 + 0.25 * hash2(cx, cy, seed.wrapping_add(9));
                    let d = (cell_point(cx, cy, seed, 0.8) - p).length() / radius;
                    if d < 1.0 {
                        h += (d * d - 1.0) * depth; // parabolic bowl
                    }
                    // raised rim just outside the bowl edge
                    let rim_d = (d - 1.0) / 0.35;
                    h += rim * (-rim_d * rim_d).exp() * smoothstep((0.4, 0.9), d);
                }
            }
            h
        }
        LayerKind::Dune { scale, direction_deg, sharpness } => {
            let dir = Vec2::from_angle(direction_deg.to_radians());
            // wiggle the crest lines so they read as sand, not corrugation
            let wiggle = fbm(pw / (scale.max(0.01) * 4.0), seed, 2, 0.5, 2.0, 0.0, 0.0) - 0.5;
            let t = (pw.dot(dir) / scale.max(0.01) + wiggle * 0.8).fract();
            let t = if t < 0.0 { t + 1.0 } else { t };
            // asymmetric profile: long windward slope, short slip face
            let profile = if t < 0.7 { t / 0.7 } else { (1.0 - t) / 0.3 };
            profile.clamp(0.0, 1.0).powf(sharpness.max(0.2))
        }
        LayerKind::Flow { scale, direction_deg, width, meander } => {
            let dir = Vec2::from_angle(direction_deg.to_radians());
            let perp = Vec2::new(-dir.y, dir.x);
            let along = pw.dot(dir) / scale.max(0.01);
            let across = pw.dot(perp);
            let center = (fbm(Vec2::new(along, 7.3), seed, 3, 0.5, 2.0, 0.0, 0.0) - 0.5) * 2.0 * meander;
            let d = (across - center).abs() / width.max(0.01);
            (1.0 - smoothstep((0.4, 1.0), d)) * (1.0 - 0.35 * d * d).max(0.0)
        }
        LayerKind::Constant { value } => value,
        LayerKind::Shape { shape, x, y, radius, rotation_deg, aspect, falloff, detail } => {
            // footprint distance, elongated along the rotated local X
            let rel = pw - Vec2::new(x, y);
            let dir = Vec2::from_angle(-rotation_deg.to_radians());
            let local = Vec2::new(
                rel.x * dir.x - rel.y * dir.y,
                rel.x * dir.y + rel.y * dir.x,
            );
            let aspect = aspect.max(0.2);
            let d = (Vec2::new(local.x / aspect, local.y) / radius.max(0.1)).length();
            if d >= 1.5 {
                return 0.0;
            }
            // noise roughening: wobble the distance so edges aren't perfect
            let rough = if detail > 0.0 {
                (fbm(pw / (radius.max(0.1) * 0.5), seed, 3, 0.5, 2.0, 0.0, 0.0) - 0.5)
                    * detail
                    * 0.6
            } else {
                0.0
            };
            let d = (d + rough).max(0.0);
            let soft = falloff.clamp(0.05, 1.0);
            match shape {
                // rounded peak easing to 0 at the rim
                ShapeKind::Mountain => (1.0 - smoothstep((1.0 - soft, 1.0), d))
                    * (1.0 - d * d * 0.35).max(0.0),
                // tent profile across the elongated footprint
                ShapeKind::Ridge => (1.0 - smoothstep((1.0 - soft, 1.0), d))
                    * (1.0 - d).clamp(0.0, 1.0).powf(0.75),
                // same dome as Mountain; the sign comes from the blend
                // (TerrainLayer::new defaults valleys to Subtract)
                ShapeKind::Valley => (1.0 - smoothstep((1.0 - soft, 1.0), d))
                    * (1.0 - d * d * 0.35).max(0.0),
                // flat top, all the shaping in the rim
                ShapeKind::Plateau => 1.0 - smoothstep((1.0 - soft, 1.0), d),
                // parabolic bowl with a raised rim
                ShapeKind::Crater => {
                    let bowl = if d < 1.0 { (d * d - 1.0) * 0.8 } else { 0.0 };
                    let rim_d = (d - 1.0) / (0.35 * soft.max(0.2));
                    bowl + 0.45 * (-rim_d * rim_d).exp()
                }
            }
        }
        LayerKind::DomainWarp { .. } | LayerKind::Terrace { .. } => 0.0,
    }
}

/// Build the terrain mesh: a smooth-shaded grid with analytic-difference
/// normals and world-meter UVs (matching the box-projection convention).
pub fn generate_mesh(data: &TerrainData, size: f32, resolution: u32, height: f32, seed: u32) -> MeshData {
    generate_mesh_ex(data, size, resolution, height, seed, true)
}

/// [`generate_mesh`] with the water surface optional: collision meshing
/// passes `include_water = false` so rays and physics reach the ground
/// through the water.
pub fn generate_mesh_ex(
    data: &TerrainData,
    size: f32,
    resolution: u32,
    height: f32,
    seed: u32,
    include_water: bool,
) -> MeshData {
    generate_mesh_at(data, size, resolution, height, seed, include_water, None)
}

/// [`generate_mesh_ex`] at a simulation time: `Some(t)` animates the water
/// sheet with the layer's Gerstner wave field (physics playback); `None`
/// keeps the static rippled surface.
pub fn generate_mesh_at(
    data: &TerrainData,
    size: f32,
    resolution: u32,
    height: f32,
    seed: u32,
    include_water: bool,
    water_time: Option<f32>,
) -> MeshData {
    let res = resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION) as usize;
    let n = res + 1;
    let step = size / res as f32;
    let heights = data.eval_grid(seed, resolution, size, height);

    let mut mesh = MeshData::default();
    mesh.positions.reserve(n * n);
    mesh.normals.reserve(n * n);
    mesh.uvs.reserve(n * n);
    for iy in 0..n {
        let y = (iy as f32 / res as f32 - 0.5) * size;
        for ix in 0..n {
            let x = (ix as f32 / res as f32 - 0.5) * size;
            let idx = iy * n + ix;
            mesh.positions.push(Vec3::new(x, y, heights[idx]));

            // central differences (one-sided on the border)
            let xm = heights[idx - if ix > 0 { 1 } else { 0 }];
            let xp = heights[idx + if ix < res { 1 } else { 0 }];
            let ym = heights[idx - if iy > 0 { n } else { 0 }];
            let yp = heights[idx + if iy < res { n } else { 0 }];
            let span = |lo: usize, hi: usize| ((hi - lo).max(1) as f32) * step;
            let dzdx = (xp - xm) / span(ix.saturating_sub(1), (ix + 1).min(res));
            let dzdy = (yp - ym) / span(iy.saturating_sub(1), (iy + 1).min(res));
            mesh.normals.push(Vec3::new(-dzdx, -dzdy, 1.0).normalize());
            // 0..1 across the footprint, aligned with the baked color
            // texture (tiling materials can still repeat via UV scale)
            mesh.uvs.push(Vec2::new(
                ix as f32 / res as f32,
                iy as f32 / res as f32,
            ));
        }
    }
    mesh.indices.reserve(res * res * 6);
    for iy in 0..res {
        for ix in 0..res {
            let a = (iy * n + ix) as u32;
            let b = a + 1;
            let c = a + n as u32 + 1;
            let d = a + n as u32;
            // winding: +Z up with X right / Y forward wants CCW seen from above
            mesh.indices.extend_from_slice(&[a, b, c, a, c, d]);
        }
    }
    if include_water {
        if let Some(water) = data.water.filter(|w| w.enabled) {
            append_water_surface(&mut mesh, &heights, &water, size, res, seed, water_time);
        }
    }
    mesh
}

/// Appends the water sheet as material-slot-1 triangles: one flat (gently
/// rippled) cell per terrain cell whose lowest corner sits below the water
/// level. Vertices are emitted only where used and share the terrain's
/// 0..1 UV mapping so the baked water tint lines up texel-for-texel.
fn append_water_surface(
    mesh: &mut MeshData,
    heights: &[f32],
    water: &WaterLayer,
    size: f32,
    res: usize,
    seed: u32,
    water_time: Option<f32>,
) {
    let n = res + 1;
    let wet = |ix: usize, iy: usize| heights[iy * n + ix] < water.level;
    // a cell carries water when any corner is submerged: reaching one cell
    // past the exact shoreline lets the sheet run into the bank instead of
    // stopping short of it
    let cell_wet = |ix: usize, iy: usize| {
        wet(ix, iy) || wet(ix + 1, iy) || wet(ix, iy + 1) || wet(ix + 1, iy + 1)
    };
    if !(0..res).any(|iy| (0..res).any(|ix| cell_wet(ix, iy))) {
        return; // level below every point: no sheet at all
    }

    let ripple_z = |ix: usize, iy: usize| {
        if water.ripple <= 1.0e-4 {
            return water.level;
        }
        let p = Vec2::new(ix as f32, iy as f32) * (size / res as f32);
        // two octaves of value noise (each in [-1, 1]), wavelengths ~2.5 m
        // and ~1 m: reads as gentle chop at any terrain size, no animation
        let a = vnoise_d(p / 2.5, seed.wrapping_add(0x0aa1)).0;
        let b = vnoise_d(p / 1.1, seed.wrapping_add(0x0aa2)).0;
        water.level + (a + 0.5 * b) * (water.ripple / 1.5)
    };
    // simulation playback: the static ripple gives way to the travelling
    // Gerstner field (see water.rs)
    let wave_set = water_time.map(|_| crate::water::WaveSet::new(&water.waves, seed));

    // emit only the vertices wet cells actually reference
    let mut remap: Vec<u32> = vec![u32::MAX; n * n];
    let emit = |mesh: &mut MeshData, remap: &mut Vec<u32>, ix: usize, iy: usize| -> u32 {
        let slot = iy * n + ix;
        if remap[slot] != u32::MAX {
            return remap[slot];
        }
        let x = (ix as f32 / res as f32 - 0.5) * size;
        let y = (iy as f32 / res as f32 - 0.5) * size;
        let (position, normal) = match (&wave_set, water_time) {
            (Some(set), Some(t)) => {
                // trochoid: crests pull vertices horizontally too
                let (offset, normal) = set.displace(Vec2::new(x, y), t);
                (Vec3::new(x, y, water.level) + offset, normal)
            }
            _ => {
                let z = ripple_z(ix, iy);
                // ripple slope for the normal (finite differences)
                let step = size / res as f32;
                let dzdx = (ripple_z((ix + 1).min(res), iy)
                    - ripple_z(ix.saturating_sub(1), iy))
                    / (2.0 * step);
                let dzdy = (ripple_z(ix, (iy + 1).min(res))
                    - ripple_z(ix, iy.saturating_sub(1)))
                    / (2.0 * step);
                (Vec3::new(x, y, z), Vec3::new(-dzdx, -dzdy, 1.0).normalize())
            }
        };
        mesh.positions.push(position);
        mesh.normals.push(normal);
        mesh.uvs.push(Vec2::new(ix as f32 / res as f32, iy as f32 / res as f32));
        let index = mesh.positions.len() as u32 - 1;
        remap[slot] = index;
        index
    };

    // the terrain triangles so far are all slot 0; the sheet is slot 1
    let ground_tris = mesh.indices.len() / 3;
    mesh.tri_materials = vec![0; ground_tris];
    for iy in 0..res {
        for ix in 0..res {
            if !cell_wet(ix, iy) {
                continue;
            }
            let a = emit(mesh, &mut remap, ix, iy);
            let b = emit(mesh, &mut remap, ix + 1, iy);
            let c = emit(mesh, &mut remap, ix + 1, iy + 1);
            let d = emit(mesh, &mut remap, ix, iy + 1);
            mesh.indices.extend_from_slice(&[a, b, c, a, c, d]);
            mesh.tri_materials.extend_from_slice(&[1, 1]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against the mid-range compression that made early stacks
    /// nearly flat: every preset must actually use its height budget.
    #[test]
    fn presets_have_useful_relief() {
        for (name, data) in TerrainData::presets() {
            let g = data.eval_grid(1, 128, 100.0, 1.0);
            let n = g.len() as f32;
            let mean = g.iter().sum::<f32>() / n;
            let min = g.iter().cloned().fold(f32::INFINITY, f32::min);
            let max = g.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            let std = (g.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n).sqrt();
            assert!(std > 0.05, "preset {name} too flat: std {std:.3}");
            assert!(max - min > 0.3, "preset {name} span too small: {:.3}", max - min);
        }
    }

    #[test]
    fn deterministic_across_calls() {
        let data = TerrainData::default();
        let a = data.eval_grid(42, 64, 100.0, 12.0);
        let b = data.eval_grid(42, 64, 100.0, 12.0);
        assert_eq!(a, b);
        let c = data.eval_grid(43, 64, 100.0, 12.0);
        assert_ne!(a, c, "different seeds must differ");
    }

    #[test]
    fn grid_and_mesh_sizes() {
        let data = TerrainData::default();
        let mesh = generate_mesh(&data, 50.0, 32, 8.0, 1);
        assert_eq!(mesh.positions.len(), 33 * 33);
        assert_eq!(mesh.normals.len(), 33 * 33);
        assert_eq!(mesh.uvs.len(), 33 * 33);
        assert_eq!(mesh.indices.len(), 32 * 32 * 6);
    }

    #[test]
    fn resolution_is_clamped() {
        let data = TerrainData::default();
        let mesh = generate_mesh(&data, 50.0, 2, 8.0, 1); // below MIN
        let n = MIN_RESOLUTION as usize + 1;
        assert_eq!(mesh.positions.len(), n * n);
    }

    #[test]
    fn heights_bounded_by_height_param() {
        let data = TerrainData::default();
        let h = 10.0;
        for v in data.eval_grid(7, 64, 120.0, h) {
            assert!(v >= -0.5 * h - 1e-4 && v <= 1.5 * h + 1e-4, "height {v} out of range");
        }
    }

    #[test]
    fn normals_point_up() {
        let data = TerrainData::default();
        let mesh = generate_mesh(&data, 80.0, 48, 10.0, 3);
        for n in &mesh.normals {
            assert!(n.z > 0.0, "terrain normals must face up, got {n:?}");
        }
    }

    #[test]
    fn empty_stack_is_flat() {
        let data = TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(), layers: Vec::new() };
        for v in data.eval_grid(1, 16, 10.0, 5.0) {
            assert_eq!(v, 0.0);
        }
    }

    #[test]
    fn masked_constant_respects_height_band() {
        // raise everything to 0.5, then a constant that only applies above
        // 0.75 must change nothing
        let mut top_up = TerrainLayer::new(LayerKind::Constant { value: 1.0 });
        top_up.blend = BlendMode::Add;
        top_up.amount = 1.0;
        top_up.mask.height = Some(Band { min: 0.75, max: 2.0, falloff: 0.01, invert: false });
        let data = TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(),
            layers: vec![
                TerrainLayer {
                    amount: 0.5,
                    ..TerrainLayer::new(LayerKind::Constant { value: 1.0 })
                },
                top_up,
            ],
        };
        for v in data.eval_grid(1, 16, 10.0, 1.0) {
            assert!((v - 0.5).abs() < 1e-4, "masked layer leaked: {v}");
        }
    }

    #[test]
    fn base64_roundtrip() {
        for len in [0usize, 1, 2, 3, 4, 5, 100] {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes, "len {len}");
        }
    }

    #[test]
    fn sculpt_survives_serde_bit_exactly() {
        let mut sculpt = SculptLayer::new(16);
        for (i, d) in sculpt.deltas.iter_mut().enumerate() {
            *d = (i as f32 * 0.37).sin() * 5.0;
        }
        let mut data = TerrainData::default();
        data.sculpt = Some(sculpt.clone());
        let json = serde_json::to_string(&data).unwrap();
        let back: TerrainData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sculpt.as_ref().unwrap().deltas, sculpt.deltas);
    }

    #[test]
    fn raise_brush_lifts_the_surface_inside_the_radius_only() {
        let mut data = TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(), layers: Vec::new() };
        let mut sculpt = SculptLayer::new(32);
        sculpt.brush(
            BrushMode::Raise,
            Vec2::new(10.0, -5.0),
            8.0,
            2.0,
            0.5,
            100.0,
            &[],
            0.0,
        );
        data.sculpt = Some(sculpt);
        let grid = data.eval_grid(1, 32, 100.0, 10.0);
        let center = sample_height(&grid, 32, 100.0, 10.0, -5.0);
        let far = sample_height(&grid, 32, 100.0, -40.0, 40.0);
        assert!(center > 1.5, "center must lift ~2 m, got {center}");
        assert_eq!(far, 0.0, "outside the radius stays untouched");
    }

    #[test]
    fn flatten_brush_pulls_toward_the_target() {
        let mut data =
            TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(), layers: Vec::new() };
        // constant 0.5 → flat 5 m surface at height 10
        data.layers.push(TerrainLayer {
            amount: 0.5,
            ..TerrainLayer::new(LayerKind::Constant { value: 1.0 })
        });
        let current = data.eval_grid(1, 32, 100.0, 10.0);
        let mut sculpt = SculptLayer::new(32);
        sculpt.brush(
            BrushMode::Flatten,
            Vec2::ZERO,
            10.0,
            1.0, // full lerp
            0.3,
            100.0,
            &current,
            8.0, // pull to 8 m
        );
        data.sculpt = Some(sculpt);
        let grid = data.eval_grid(1, 32, 100.0, 10.0);
        let center = sample_height(&grid, 32, 100.0, 0.0, 0.0);
        assert!((center - 8.0).abs() < 0.05, "flattened to target, got {center}");
    }

    #[test]
    fn sculpt_resample_keeps_the_shape() {
        let mut sculpt = SculptLayer::new(32);
        sculpt.brush(BrushMode::Raise, Vec2::ZERO, 20.0, 3.0, 0.5, 100.0, &[], 0.0);
        let fine = sculpt.resample(64);
        assert!((fine.sample_normalized(0.5, 0.5) - sculpt.sample_normalized(0.5, 0.5)).abs() < 0.05);
        assert_eq!(fine.resolution, 64);
    }

    #[test]
    fn raycast_hits_the_surface_from_above() {
        let data = TerrainData::default();
        let grid = data.eval_grid(1, 64, 100.0, 10.0);
        let hit = raycast_grid(
            &grid,
            64,
            100.0,
            Vec3::new(5.0, 5.0, 50.0),
            Vec3::new(0.0, 0.0, -1.0),
        )
        .expect("straight-down ray must hit");
        let expected = sample_height(&grid, 64, 100.0, 5.0, 5.0);
        assert!((hit.z - expected).abs() < 0.05, "hit {hit:?} vs surface {expected}");
        assert!((hit.x - 5.0).abs() < 1e-3 && (hit.y - 5.0).abs() < 1e-3);
        // a ray that misses the footprint entirely
        assert!(raycast_grid(
            &grid,
            64,
            100.0,
            Vec3::new(200.0, 0.0, 50.0),
            Vec3::new(0.0, 0.0, -1.0),
        )
        .is_none());
        // a shallow diagonal ray from outside the box
        let diag = raycast_grid(
            &grid,
            64,
            100.0,
            Vec3::new(-80.0, 0.0, 30.0),
            Vec3::new(1.0, 0.1, -0.35).normalize(),
        );
        assert!(diag.is_some(), "diagonal ray should land on the terrain");
    }

    #[test]
    fn shape_stamp_lands_where_placed() {
        let mut data =
            TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(), layers: Vec::new() };
        let mut layer = TerrainLayer::new(LayerKind::Shape {
            shape: ShapeKind::Mountain,
            x: 20.0,
            y: -10.0,
            radius: 15.0,
            rotation_deg: 0.0,
            aspect: 1.0,
            falloff: 0.5,
            detail: 0.0,
        });
        layer.amount = 1.0;
        data.layers.push(layer);
        let grid = data.eval_grid(1, 64, 100.0, 10.0);
        let peak = sample_height(&grid, 64, 100.0, 20.0, -10.0);
        let away = sample_height(&grid, 64, 100.0, -30.0, 30.0);
        assert!(peak > 7.0, "mountain peak at the stamp center, got {peak}");
        assert!(away.abs() < 0.01, "far field flat, got {away}");
    }

    #[test]
    fn base_cache_makes_sculpt_only_changes_cheap_and_correct() {
        let mut data = TerrainData::default();
        let a = data.eval_grid(1, 64, 100.0, 10.0);
        // second call hits the cache — must be identical
        let b = data.eval_grid(1, 64, 100.0, 10.0);
        assert_eq!(a, b);
        // sculpt on top: base unchanged, delta added exactly
        let mut sculpt = SculptLayer::new(64);
        sculpt.brush(BrushMode::Raise, Vec2::ZERO, 10.0, 1.0, 0.5, 100.0, &[], 0.0);
        data.sculpt = Some(sculpt);
        let c = data.eval_grid(1, 64, 100.0, 10.0);
        let mid = (64 / 2) * 65 + 64 / 2;
        assert!((c[mid] - a[mid] - 1.0).abs() < 1e-4);
        // changing a layer invalidates the cache
        data.layers.push(TerrainLayer {
            amount: 0.3,
            ..TerrainLayer::new(LayerKind::Constant { value: 1.0 })
        });
        let d = data.eval_grid(1, 64, 100.0, 10.0);
        assert!((d[0] - c[0] - 3.0).abs() < 0.2, "new layer must take effect");
    }

    #[test]
    fn erosion_bake_applies_and_scales_with_strength() {
        let mut data = TerrainData::default();
        let before = data.eval_grid(1, 64, 100.0, 12.0);
        let settings = crate::erosion::ErosionSettings {
            droplets: 8_000,
            ..Default::default()
        };
        data.bake_erosion(1, 64, 100.0, 12.0, settings);
        let full = data.eval_grid(1, 64, 100.0, 12.0);
        assert_ne!(before, full, "erosion must change the surface");
        // half strength = half the delta at every vertex
        data.erosion.as_mut().unwrap().strength = 0.5;
        let half = data.eval_grid(1, 64, 100.0, 12.0);
        for ((b, f), h) in before.iter().zip(&full).zip(&half) {
            assert!((h - b) - (f - b) * 0.5 < 1e-4, "strength must scale linearly");
        }
        // disabled = the un-eroded surface
        data.erosion.as_mut().unwrap().enabled = false;
        assert_eq!(data.eval_grid(1, 64, 100.0, 12.0), before);
    }

    #[test]
    fn erosion_stale_detection() {
        let mut data = TerrainData::default();
        let settings = crate::erosion::ErosionSettings {
            droplets: 2_000,
            ..Default::default()
        };
        data.bake_erosion(1, 64, 100.0, 12.0, settings);
        assert!(!data.erosion_stale(1, 64, 100.0, 12.0), "fresh bake is not stale");
        assert!(data.erosion_stale(2, 64, 100.0, 12.0), "seed change → stale");
        assert!(data.erosion_stale(1, 128, 100.0, 12.0), "resolution change → stale");
        // sculpting after the bake also invalidates it
        let mut sculpt = SculptLayer::new(64);
        sculpt.brush(BrushMode::Raise, Vec2::ZERO, 10.0, 2.0, 0.5, 100.0, &[], 0.0);
        data.sculpt = Some(sculpt);
        assert!(data.erosion_stale(1, 64, 100.0, 12.0), "sculpt change → stale");
    }

    #[test]
    fn erosion_survives_serde_bit_exactly() {
        let mut data = TerrainData::default();
        data.bake_erosion(
            1,
            32,
            100.0,
            12.0,
            crate::erosion::ErosionSettings {
                droplets: 2_000,
                ..Default::default()
            },
        );
        let json = serde_json::to_string(&data).unwrap();
        let back: TerrainData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.erosion, data.erosion);
        assert_eq!(
            back.eval_grid(1, 32, 100.0, 12.0),
            data.eval_grid(1, 32, 100.0, 12.0)
        );
    }

    #[test]
    fn color_rules_pick_the_expected_biome() {
        let c = TerrainColor::default();
        // flat mid-height ground → vegetation (greenish)
        let veg = c.shade(5.0, 0.4, 0.1, 0.0, 0.0);
        assert!(veg[1] > veg[0] && veg[1] > veg[2], "flat ground should be grassy: {veg:?}");
        // steep face → rock/cliff (desaturated)
        let rock = c.shade(5.0, 0.4, 1.8, 0.0, 0.0);
        assert!((rock[0] - rock[1]).abs() < 0.08, "steep face should be grey: {rock:?}");
        // high flat ground → snow (bright)
        let snow = c.shade(11.0, 0.95, 0.1, 0.0, 0.0);
        assert!(snow.iter().all(|v| *v > 0.8), "peak should be snowy: {snow:?}");
        // base plane → sand
        let sand = c.shade(0.1, 0.01, 0.05, 0.0, 0.0);
        assert!(sand[0] > sand[2], "base should be sandy: {sand:?}");
    }

    #[test]
    fn color_bake_produces_a_full_opaque_image() {
        let data = TerrainData::default(); // color on by default
        let (w, h, rgba) = data.bake_color(1, 64, 100.0, 12.0, 128).expect("color on");
        assert_eq!((w, h), (128, 128));
        assert_eq!(rgba.len(), 128 * 128 * 4);
        assert!(rgba.chunks_exact(4).all(|p| p[3] == 255), "opaque");
        // not a flat color: biome variation must show up
        let first = &rgba[0..3];
        assert!(
            rgba.chunks_exact(4).any(|p| {
                (p[0] as i32 - first[0] as i32).abs() > 12
                    || (p[1] as i32 - first[1] as i32).abs() > 12
            }),
            "bake should vary across the terrain"
        );
        // disabled = no bake
        let mut off = data.clone();
        off.color = None;
        assert!(off.bake_color(1, 64, 100.0, 12.0, 128).is_none());
    }

    #[test]
    fn color_presets_and_stamp() {
        for (name, c) in TerrainColor::presets() {
            let json = serde_json::to_string(&c).unwrap();
            let back: TerrainColor = serde_json::from_str(&json).unwrap();
            assert_eq!(c, back, "preset {name} serde roundtrip");
        }
        let a = TerrainColor::default();
        let mut b = a;
        b.snow_line = 0.5;
        assert_ne!(a.stamp(), b.stamp(), "stamp must follow edits");
        assert!(TerrainColor::preset("desert").is_some());
    }

    #[test]
    fn paint_brush_claims_channel_and_shows_in_the_bake() {
        let mut data = TerrainData { sculpt: None, erosion: None, color: None, paint: None, water: None, cache: Default::default(), layers: Vec::new() };
        data.color = Some(TerrainColor::default());
        let mut paint = PaintLayer::new(32);
        // paint snow (channel 4) into a patch until fully opaque
        for _ in 0..40 {
            paint.brush(Vec2::new(20.0, 20.0), 12.0, 0.2, 0.3, 100.0, 4);
        }
        assert!(!paint.is_empty());
        let (slot, w) = paint.sample(0.7, 0.7);
        assert_eq!(slot, 4);
        assert!(w > 0.9, "repeated dabs should saturate, got {w}");
        data.paint = Some(paint.clone());
        let (_, _, rgba) = data.bake_color(1, 32, 100.0, 10.0, 128).unwrap();
        // texel inside the patch is snow-bright; far corner is grass-dark
        let px = |u: f32, v: f32| {
            let (x, y) = ((u * 127.0) as usize, (v * 127.0) as usize);
            let o = (y * 128 + x) * 4;
            [rgba[o], rgba[o + 1], rgba[o + 2]]
        };
        let painted = px(0.7, 0.7);
        let unpainted = px(0.1, 0.1);
        assert!(painted[0] > 200, "snow patch should be bright: {painted:?}");
        // the flat zero-height test terrain is sand elsewhere — the point is
        // that the paint stayed inside its patch
        assert!(unpainted[0] < 200, "outside the patch is not snow: {unpainted:?}");
        // erasing brings the weight back down
        paint.brush(Vec2::new(20.0, 20.0), 12.0, -1.0, 0.3, 100.0, 4);
        let (_, w) = paint.sample(0.7, 0.7);
        assert!(w < 0.1, "erase should clear, got {w}");
        // serde roundtrip
        let json = serde_json::to_string(&paint).unwrap();
        let back: PaintLayer = serde_json::from_str(&json).unwrap();
        assert_eq!(back, paint);
    }

    #[test]
    fn scatter_is_deterministic_and_respects_the_rules() {
        let data = TerrainData::default();
        let params = ScatterParams {
            density: 0.8,
            max_slope: 0.6,
            height_min: 1.0,
            height_max: 9.0,
            ..Default::default()
        };
        let a = data.scatter(1, 128, 100.0, 12.0, &params, 5000);
        let b = data.scatter(1, 128, 100.0, 12.0, &params, 5000);
        assert_eq!(a, b, "same seeds must reproduce");
        assert!(!a.is_empty(), "dense scatter on hills must place props");
        let grid = data.eval_grid(1, 128, 100.0, 12.0);
        for p in &a {
            assert!(p.position.x.abs() <= 50.0 && p.position.y.abs() <= 50.0);
            assert!(p.position.z >= 1.0 - 1e-3 && p.position.z <= 9.0 + 1e-3);
            let h = sample_height(&grid, 128, 100.0, p.position.x, p.position.y);
            assert!((h - p.position.z).abs() < 0.6, "sits on the surface");
            assert!(p.scale >= 0.8 && p.scale <= 1.4);
        }
        // different scatter seed → different layout
        let c = data.scatter(
            1,
            128,
            100.0,
            12.0,
            &ScatterParams { seed: 9, ..params },
            5000,
        );
        assert_ne!(a, c);
        // spacing: no two placements share a cell-size neighbourhood
        for (i, p) in a.iter().enumerate() {
            for q in &a[i + 1..] {
                let d = (p.position - q.position).truncate().length();
                assert!(d > 3.0, "spacing keeps props apart, got {d}");
            }
        }
        // the cap truncates
        assert_eq!(data.scatter(1, 128, 100.0, 12.0, &params, 3).len(), 3);
    }

    #[test]
    fn prop_meshes_are_sane() {
        use crate::mesh;
        for (mesh, two_slots) in [
            (mesh::prop_rock(3, 1.5), false),
            (mesh::prop_conifer(3, 4.0), true),
            (mesh::prop_broadleaf(3, 5.0), true),
            (mesh::prop_bush(3, 1.0), true),
        ] {
            assert!(!mesh.indices.is_empty());
            assert!(mesh.positions.len() == mesh.normals.len());
            let min_z = mesh.positions.iter().map(|p| p.z).fold(f32::INFINITY, f32::min);
            assert!(min_z > -0.5, "props stand near z=0, got {min_z}");
            if two_slots {
                assert!(
                    mesh.tri_materials.iter().any(|s| *s == 1),
                    "foliage triangles tagged slot 1"
                );
                assert_eq!(mesh.tri_materials.len(), mesh.indices.len() / 3);
            }
            // determinism
        }
        assert_eq!(mesh::prop_rock(7, 2.0), mesh::prop_rock(7, 2.0));
    }

    #[test]
    fn serde_roundtrip() {
        let data = TerrainData::presets()
            .into_iter()
            .find(|(n, _)| *n == "Canyon")
            .unwrap()
            .1;
        let json = serde_json::to_string(&data).unwrap();
        let back: TerrainData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, back);
    }

    #[test]
    fn all_presets_evaluate() {
        for (name, data) in TerrainData::presets() {
            let grid = data.eval_grid(5, 32, 60.0, 8.0);
            assert!(grid.iter().all(|v| v.is_finite()), "preset {name} produced NaN");
        }
    }

    /// A partially submerged terrain grows a slot-1 water sheet in the
    /// render mesh — and never in the collision variant.
    #[test]
    fn water_sheet_is_render_only_slot_1() {
        let mut data = TerrainData::default();
        let dry = generate_mesh(&data, 50.0, 32, 8.0, 1);
        // put the level mid-relief so some cells are wet and some dry
        let grid = data.eval_grid(1, 32, 50.0, 8.0);
        let (lo, hi) = grid.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &v| {
            (a.min(v), b.max(v))
        });
        data.water = Some(WaterLayer { level: (lo + hi) * 0.5, ..Default::default() });

        let wet = generate_mesh(&data, 50.0, 32, 8.0, 1);
        assert!(wet.indices.len() > dry.indices.len(), "no sheet appended");
        assert_eq!(wet.tri_materials.len(), wet.indices.len() / 3);
        let water_tris = wet.tri_materials.iter().filter(|&&m| m == 1).count();
        assert!(water_tris > 0, "sheet triangles must be slot 1");
        assert!(
            water_tris < wet.tri_materials.len(),
            "a mid-relief level must leave dry ground"
        );
        // every sheet vertex is used and sits near the level (± ripple)
        let ripple = data.water.unwrap().ripple + 1.0e-4;
        let level = data.water.unwrap().level;
        for p in &wet.positions[dry.positions.len()..] {
            assert!((p.z - level).abs() <= ripple, "sheet vertex at z {}", p.z);
        }

        let collision = generate_mesh_ex(&data, 50.0, 32, 8.0, 1, false);
        assert_eq!(collision.indices.len(), dry.indices.len());
        assert!(collision.tri_materials.iter().all(|&m| m == 0) || collision.tri_materials.is_empty());
    }

    #[test]
    fn water_below_everything_adds_nothing() {
        let mut data = TerrainData::default();
        data.water = Some(WaterLayer { level: -1000.0, ..Default::default() });
        let mesh = generate_mesh(&data, 50.0, 16, 8.0, 1);
        let dry = generate_mesh_ex(&data, 50.0, 16, 8.0, 1, false);
        assert_eq!(mesh.indices.len(), dry.indices.len());
    }

    #[test]
    fn water_serde_roundtrip() {
        let mut data = TerrainData::default();
        data.water = Some(WaterLayer { level: 1.25, opacity: 0.7, ..Default::default() });
        let json = serde_json::to_string(&data).unwrap();
        let back: TerrainData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, back);
        // absent field stays None for old files
        let old: TerrainData = serde_json::from_str("{\"layers\":[]}").unwrap();
        assert!(old.water.is_none());
    }

    #[test]
    fn water_bake_dimensions_and_gate() {
        let mut data = TerrainData::default();
        assert!(data.bake_water_color(1, 32, 50.0, 8.0, 128).is_none());
        data.water = Some(WaterLayer::default());
        let (w, h, px) = data.bake_water_color(1, 32, 50.0, 8.0, 128).unwrap();
        assert_eq!((w, h), (128, 128));
        assert_eq!(px.len(), 128 * 128 * 4);
        data.water.as_mut().unwrap().enabled = false;
        assert!(data.bake_water_color(1, 32, 50.0, 8.0, 128).is_none());
    }
}
