//! Gerstner wave field for the terrain water simulation.
//!
//! Adapted from the swell component of the OceanThreejs reference
//! (https://github.com/achrefelouafi/OceanThreejs): a small set of
//! directional trochoid waves — `xy += Q·A·D·cos(k·D·p − ωt)`,
//! `z += A·sin(k·D·p − ωt)` — with the deep-water dispersion relation
//! `ω = √(g·k)`, so long waves genuinely travel faster than short ones.
//! The reference layers these over a GPU FFT spectrum; here the Gerstner
//! set alone animates the (already meshed) water sheet on the CPU, which
//! keeps shadows, SSR and picking consistent for free.
//!
//! Everything is deterministic per seed: the same seed and parameters
//! rebuild the same wave set, so a paused simulation resumes seamlessly
//! and headless renders reproduce.

use crate::glam::{Vec2, Vec3};
use serde::{Deserialize, Serialize};

/// Standard gravity, the only physical constant the dispersion needs.
const G: f32 = 9.81;

/// Number of trochoid waves in a set (the reference uses up to six).
pub const WAVE_COUNT: usize = 6;

/// User-facing wave parameters (stored on the water layer).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaveParams {
    /// Crest-to-rest amplitude budget in meters, shared by the whole set.
    pub amplitude: f32,
    /// Dominant wavelength in meters; the set spreads geometrically
    /// around it (half to double).
    pub wavelength: f32,
    /// Mean travel direction in degrees (0 = +X, CCW).
    pub direction_deg: f32,
    /// Half-angle of the directional spread in degrees.
    pub spread_deg: f32,
    /// 0 = pure sine swell, 1 = maximally trochoid (sharp crests). Kept
    /// below the self-intersection bound per wave.
    pub choppiness: f32,
    /// Time multiplier: 1 = physically dispersive speed.
    pub speed: f32,
}

impl Default for WaveParams {
    fn default() -> Self {
        Self {
            amplitude: 0.35,
            wavelength: 14.0,
            direction_deg: 25.0,
            spread_deg: 35.0,
            choppiness: 0.7,
            speed: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Wave {
    dir: Vec2,
    /// Angular wavenumber 2π/λ.
    k: f32,
    /// Temporal frequency from deep-water dispersion, pre-multiplied by
    /// the user's speed factor.
    omega: f32,
    amp: f32,
    /// Horizontal (choppy) displacement factor for this wave.
    q: f32,
    phase: f32,
}

/// A ready-to-evaluate set of [`WAVE_COUNT`] Gerstner waves.
#[derive(Debug, Clone)]
pub struct WaveSet {
    waves: [Wave; WAVE_COUNT],
}

/// Deterministic [0,1) hash (splitmix-style, same spirit as terrain.rs).
fn hash01(seed: u32, i: u32) -> f32 {
    let mut x = seed
        .wrapping_mul(0x9e37_79b9)
        .wrapping_add(i.wrapping_mul(0x85eb_ca6b))
        .wrapping_add(0x27d4_eb2f);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2c1b_3c6d);
    x ^= x >> 12;
    x = x.wrapping_mul(0x297a_2d39);
    x ^= x >> 15;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

impl WaveSet {
    pub fn new(params: &WaveParams, seed: u32) -> Self {
        let amplitude = params.amplitude.max(0.0);
        let base_len = params.wavelength.clamp(0.5, 4000.0);
        let choppy = params.choppiness.clamp(0.0, 1.0);
        let dir0 = params.direction_deg.to_radians();
        let spread = params.spread_deg.clamp(0.0, 180.0).to_radians();

        // Wavelengths spread λ/2..2λ; weight longer waves heavier
        // (∝ √λ) so the swell reads as one sea, not six ripples.
        let mut lens = [0f32; WAVE_COUNT];
        let mut weights = [0f32; WAVE_COUNT];
        let mut total = 0.0;
        for i in 0..WAVE_COUNT {
            // stratified: one wave per band, jittered inside it
            let u = (i as f32 + hash01(seed, i as u32)) / WAVE_COUNT as f32;
            lens[i] = base_len * 0.5 * 4f32.powf(u);
            weights[i] = (lens[i] / base_len).sqrt();
            total += weights[i];
        }

        let waves = std::array::from_fn(|i| {
            let angle = dir0 + (hash01(seed, 100 + i as u32) * 2.0 - 1.0) * spread;
            let k = std::f32::consts::TAU / lens[i];
            let amp = amplitude * weights[i] / total;
            // per-wave choppiness, capped at the classic 1/(k·A·N)
            // self-intersection bound so crests never loop over
            let q = if amp > 1e-6 {
                choppy / (k * amp * WAVE_COUNT as f32)
            } else {
                0.0
            };
            Wave {
                dir: Vec2::new(angle.cos(), angle.sin()),
                k,
                omega: (G * k).sqrt() * params.speed.max(0.0),
                amp,
                q: q.min(1.0 / (k * amp.max(1e-6) * WAVE_COUNT as f32)),
                phase: hash01(seed, 200 + i as u32) * std::f32::consts::TAU,
            }
        });
        Self { waves }
    }

    /// Displacement of the rest-position `p` (terrain-local meters) at
    /// time `t`, and the surface normal there. The offset's xy is the
    /// trochoid choppiness, z the heave; add it to the flat sheet vertex.
    pub fn displace(&self, p: Vec2, t: f32) -> (Vec3, Vec3) {
        let mut offset = Vec3::ZERO;
        // accumulated slope/compression terms for the analytic normal
        let mut nx = 0.0;
        let mut ny = 0.0;
        let mut nz = 0.0;
        for w in &self.waves {
            let theta = w.k * w.dir.dot(p) - w.omega * t + w.phase;
            let (sin, cos) = theta.sin_cos();
            offset.x += w.q * w.amp * w.dir.x * cos;
            offset.y += w.q * w.amp * w.dir.y * cos;
            offset.z += w.amp * sin;
            let ka = w.k * w.amp;
            nx += w.dir.x * ka * cos;
            ny += w.dir.y * ka * cos;
            nz += w.q * ka * sin;
        }
        (offset, Vec3::new(-nx, -ny, 1.0 - nz).normalize())
    }

    /// Vertical displacement only, at the rest position — what buoyancy
    /// wants (the horizontal trochoid shift matters visually, not for a
    /// floating body's water line).
    pub fn height(&self, p: Vec2, t: f32) -> f32 {
        self.waves
            .iter()
            .map(|w| w.amp * (w.k * w.dir.dot(p) - w.omega * t + w.phase).sin())
            .sum()
    }

    /// The set's total amplitude budget (max |heave|).
    pub fn amplitude(&self) -> f32 {
        self.waves.iter().map(|w| w.amp).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_per_seed() {
        let params = WaveParams::default();
        let a = WaveSet::new(&params, 7);
        let b = WaveSet::new(&params, 7);
        let c = WaveSet::new(&params, 8);
        let p = Vec2::new(3.2, -1.7);
        assert_eq!(a.displace(p, 1.25), b.displace(p, 1.25));
        assert_ne!(a.displace(p, 1.25), c.displace(p, 1.25));
    }

    #[test]
    fn heave_stays_inside_the_amplitude_budget() {
        let params = WaveParams { amplitude: 0.5, ..Default::default() };
        let set = WaveSet::new(&params, 3);
        assert!((set.amplitude() - 0.5).abs() < 1e-4, "weights must normalize");
        for i in 0..500 {
            let p = Vec2::new(i as f32 * 0.37, i as f32 * -0.61);
            let (offset, normal) = set.displace(p, i as f32 * 0.11);
            assert!(offset.z.abs() <= 0.5 + 1e-4);
            assert!(normal.z > 0.0, "water never overhangs");
            assert!((normal.length() - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    fn long_waves_travel_faster() {
        // deep-water dispersion: phase speed c = ω/k = √(g/k) grows with λ
        let long = WaveSet::new(
            &WaveParams { wavelength: 60.0, spread_deg: 0.0, ..Default::default() },
            1,
        );
        let short = WaveSet::new(
            &WaveParams { wavelength: 4.0, spread_deg: 0.0, ..Default::default() },
            1,
        );
        let speed = |s: &WaveSet| {
            s.waves.iter().map(|w| w.omega / w.k).sum::<f32>() / WAVE_COUNT as f32
        };
        assert!(speed(&long) > speed(&short) * 2.0);
    }

    #[test]
    fn flat_when_amplitude_zero() {
        let set = WaveSet::new(&WaveParams { amplitude: 0.0, ..Default::default() }, 1);
        let (offset, normal) = set.displace(Vec2::new(5.0, 5.0), 2.0);
        assert!(offset.length() < 1e-6);
        assert!((normal - Vec3::Z).length() < 1e-6);
    }

    #[test]
    fn surface_moves_over_time() {
        let set = WaveSet::new(&WaveParams::default(), 1);
        let p = Vec2::new(1.0, 2.0);
        let h0 = set.height(p, 0.0);
        let h1 = set.height(p, 0.5);
        assert!((h0 - h1).abs() > 1e-4, "waves must actually travel");
    }
}
