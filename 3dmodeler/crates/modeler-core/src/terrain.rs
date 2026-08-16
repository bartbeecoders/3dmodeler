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
fn hash_u32(mut x: u32) -> u32 {
    x = x.wrapping_mul(0x2c1b_3c6d).rotate_right(15);
    x = x.wrapping_mul(0x297a_2d39);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2c1b_3c6d);
    x ^ (x >> 16)
}

/// Lattice hash → [0, 1).
fn hash2(ix: i32, iy: i32, seed: u32) -> f32 {
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
                TerrainData {
                    layers: vec![
                        layer(LayerKind::DomainWarp { scale: 120.0, strength: 18.0, octaves: 3 }, BlendMode::Add, 1.0),
                        layer(LayerKind::Ridged { scale: 130.0, octaves: 6, gain: 0.52, lacunarity: 2.1, sharpness: 2.0 }, BlendMode::Add, 0.85),
                        layer(LayerKind::Fbm { scale: 30.0, octaves: 4, gain: 0.5, lacunarity: 2.0, erosion: 0.6, warp: 0.0 }, BlendMode::Add, 0.12),
                    ],
                },
            ),
            (
                "Dunes",
                TerrainData {
                    layers: vec![
                        layer(LayerKind::Billow { scale: 120.0, octaves: 3, gain: 0.5, lacunarity: 2.0 }, BlendMode::Add, 0.25),
                        layer(LayerKind::DomainWarp { scale: 60.0, strength: 6.0, octaves: 2 }, BlendMode::Add, 1.0),
                        layer(LayerKind::Dune { scale: 16.0, direction_deg: 30.0, sharpness: 1.8 }, BlendMode::Add, 0.35),
                    ],
                },
            ),
            (
                "Archipelago",
                TerrainData {
                    layers: vec![
                        layer(LayerKind::Constant { value: 1.0 }, BlendMode::Subtract, 0.35),
                        layer(LayerKind::DomainWarp { scale: 100.0, strength: 20.0, octaves: 3 }, BlendMode::Add, 1.0),
                        layer(LayerKind::Fbm { scale: 90.0, octaves: 5, gain: 0.5, lacunarity: 2.0, erosion: 0.3, warp: 0.0 }, BlendMode::Add, 0.9),
                    ],
                },
            ),
            (
                "Canyon",
                TerrainData {
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
                TerrainData {
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
                TerrainData {
                    layers: vec![
                        layer(LayerKind::DomainWarp { scale: 80.0, strength: 10.0, octaves: 2 }, BlendMode::Add, 1.0),
                        layer(LayerKind::Fbm { scale: 60.0, octaves: 5, gain: 0.45, lacunarity: 2.0, erosion: 0.5, warp: 0.0 }, BlendMode::Add, 0.4),
                    ],
                },
            ),
            (
                "Craters",
                TerrainData {
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

    /// Evaluate the stack over an `(n+1)²` vertex grid covering
    /// `[-size/2, size/2]²`, row-major, in meters (already scaled by
    /// `height`). `height` also calibrates the slope masks.
    pub fn eval_grid(&self, seed: u32, resolution: u32, size: f32, height: f32) -> Vec<f32> {
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
                    let mut pw = Vec2::new(world(ix), world(iy));
                    if let Some(w) = warp.get(idx) {
                        pw += *w;
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
        LayerKind::DomainWarp { .. } | LayerKind::Terrace { .. } => 0.0,
    }
}

/// Build the terrain mesh: a smooth-shaded grid with analytic-difference
/// normals and world-meter UVs (matching the box-projection convention).
pub fn generate_mesh(data: &TerrainData, size: f32, resolution: u32, height: f32, seed: u32) -> MeshData {
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
            mesh.uvs.push(Vec2::new(x, y));
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
    mesh
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
        let data = TerrainData { layers: Vec::new() };
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
        let data = TerrainData {
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
}
