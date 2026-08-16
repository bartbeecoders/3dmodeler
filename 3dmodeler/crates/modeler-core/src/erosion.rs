//! Hydraulic + thermal erosion on a height grid (Beyer/Lague droplet model).
//!
//! The simulation runs on NORMALIZED heights (terrain height 1.0 == the
//! terrain's `height` parameter) over an `(res+1)²` vertex grid, so the
//! same settings behave the same on a 5 m garden mound and a 200 m massif.
//! It is deterministic: the same grid + settings + seed always produce the
//! same delta, on every platform (integer-hashed RNG, no `fastrand`).
//!
//! The result is a signed offset grid (`eroded - input`), stored by the
//! caller as a non-destructive layer — the procedural base is never touched.

use serde::{Deserialize, Serialize};

/// Tuning of one erosion bake. All fields have sensible ranges enforced by
/// `sanitized()`; presets port the reference implementation's recipes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ErosionSettings {
    /// Number of simulated rain droplets.
    pub droplets: u32,
    /// Steps a droplet lives (cells traveled).
    pub lifetime: u32,
    /// 0 = follows the gradient exactly, 1 = keeps its direction.
    pub inertia: f32,
    /// Sediment capacity multiplier — higher digs deeper channels.
    pub capacity: f32,
    /// Minimum slope for capacity (keeps flats from stalling droplets).
    pub min_slope: f32,
    /// Fraction of surplus sediment dropped per step.
    pub deposition: f32,
    /// Fraction of the capacity deficit eroded per step.
    pub erosion_rate: f32,
    /// Erode-brush radius in cells (spreads carving; keeps channels smooth).
    pub brush_radius: u32,
    /// Water lost per step.
    pub evaporation: f32,
    /// Downhill acceleration.
    pub gravity: f32,
    /// Thermal (talus) relaxation sweeps after the hydraulic pass.
    pub thermal_iterations: u32,
    /// Fraction of the over-talus difference moved per sweep.
    pub thermal_strength: f32,
    /// Angle of repose in degrees: steeper slopes shed material.
    pub talus_angle_deg: f32,
    /// Final 5-tap smoothing blend (0 = off).
    pub smoothing: f32,
}

impl Default for ErosionSettings {
    /// The "Natural" recipe.
    fn default() -> Self {
        Self {
            droplets: 60_000,
            lifetime: 30,
            inertia: 0.05,
            capacity: 4.0,
            min_slope: 0.01,
            deposition: 0.3,
            erosion_rate: 0.3,
            brush_radius: 3,
            evaporation: 0.02,
            gravity: 4.0,
            thermal_iterations: 30,
            thermal_strength: 0.4,
            talus_angle_deg: 33.0,
            smoothing: 0.1,
        }
    }
}

impl ErosionSettings {
    /// Named recipes (UI preset menu and the `erode` command parameter).
    pub fn presets() -> Vec<(&'static str, ErosionSettings)> {
        let natural = ErosionSettings::default();
        vec![
            (
                "Lite",
                ErosionSettings {
                    droplets: 25_000,
                    thermal_iterations: 15,
                    ..natural
                },
            ),
            ("Natural", natural),
            (
                "Mountain",
                ErosionSettings {
                    droplets: 80_000,
                    capacity: 5.0,
                    erosion_rate: 0.35,
                    talus_angle_deg: 38.0,
                    ..natural
                },
            ),
            (
                "Canyon",
                ErosionSettings {
                    droplets: 110_000,
                    brush_radius: 2,
                    capacity: 6.0,
                    deposition: 0.15,
                    ..natural
                },
            ),
            (
                "Heavy Rain",
                ErosionSettings {
                    droplets: 150_000,
                    evaporation: 0.015,
                    ..natural
                },
            ),
            (
                "Dry Thermal",
                ErosionSettings {
                    droplets: 15_000,
                    thermal_iterations: 70,
                    thermal_strength: 0.5,
                    talus_angle_deg: 30.0,
                    ..natural
                },
            ),
        ]
    }

    pub fn preset(name: &str) -> Option<ErosionSettings> {
        Self::presets()
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, s)| s)
    }

    /// Clamp every field into its sane range (protects the sim from wild
    /// command input; also what the UI sliders enforce).
    pub fn sanitized(mut self) -> Self {
        self.droplets = self.droplets.clamp(1_000, 400_000);
        self.lifetime = self.lifetime.clamp(4, 120);
        self.inertia = self.inertia.clamp(0.0, 0.95);
        self.capacity = self.capacity.clamp(0.1, 32.0);
        self.min_slope = self.min_slope.clamp(0.0001, 0.5);
        self.deposition = self.deposition.clamp(0.0, 1.0);
        self.erosion_rate = self.erosion_rate.clamp(0.0, 1.0);
        self.brush_radius = self.brush_radius.clamp(1, 8);
        self.evaporation = self.evaporation.clamp(0.0, 0.5);
        self.gravity = self.gravity.clamp(0.1, 20.0);
        self.thermal_iterations = self.thermal_iterations.clamp(0, 200);
        self.thermal_strength = self.thermal_strength.clamp(0.0, 1.0);
        self.talus_angle_deg = self.talus_angle_deg.clamp(5.0, 75.0);
        self.smoothing = self.smoothing.clamp(0.0, 1.0);
        self
    }
}

/// Deterministic 32-bit RNG (mulberry32).
struct Rng(u32);

impl Rng {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x6d2b_79f5);
        let mut z = self.0;
        z = (z ^ (z >> 15)).wrapping_mul(z | 1);
        z ^= z.wrapping_add((z ^ (z >> 7)).wrapping_mul(z | 61));
        ((z ^ (z >> 14)) >> 8) as f32 / (1u32 << 24) as f32
    }
}

/// Bilinear height and gradient (d/dx, d/dy in height-units per cell) at a
/// continuous grid position.
fn height_and_gradient(map: &[f32], n: usize, x: f32, y: f32) -> (f32, f32, f32) {
    let xi = (x.floor() as usize).min(n - 2);
    let yi = (y.floor() as usize).min(n - 2);
    let fx = (x - xi as f32).clamp(0.0, 1.0);
    let fy = (y - yi as f32).clamp(0.0, 1.0);
    let idx = yi * n + xi;
    let (a, b, c, d) = (map[idx], map[idx + 1], map[idx + n], map[idx + n + 1]);
    let gx = (b - a) * (1.0 - fy) + (d - c) * fy;
    let gy = (c - a) * (1.0 - fx) + (d - b) * fx;
    let h = a + (b - a) * fx + (c - a) * fy + (a - b - c + d) * fx * fy;
    (h, gx, gy)
}

/// Radial erode brush: cell offsets with normalized `1 - dist/r` weights.
fn build_brush(radius: u32) -> Vec<(i32, i32, f32)> {
    let r = radius.max(1) as i32;
    let mut brush = Vec::new();
    let mut total = 0.0f32;
    for dy in -r..=r {
        for dx in -r..=r {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            if d < r as f32 {
                let w = 1.0 - d / r as f32;
                brush.push((dx, dy, w));
                total += w;
            }
        }
    }
    for (_, _, w) in &mut brush {
        *w /= total.max(1e-6);
    }
    brush
}

/// Run the full erosion bake. `heights` are normalized (1.0 == terrain
/// height) on an `(resolution+1)²` grid; `cell_norm` is the width of one
/// cell in the same normalized units (`(size/res) / height`), which anchors
/// the talus angle to real-world geometry. Returns `eroded - heights`.
pub fn erode_grid(
    heights: &[f32],
    resolution: u32,
    cell_norm: f32,
    settings: &ErosionSettings,
    seed: u32,
) -> Vec<f32> {
    let s = settings.sanitized();
    let n = resolution as usize + 1;
    assert_eq!(heights.len(), n * n, "grid size mismatch");
    let mut map = heights.to_vec();
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9).wrapping_add(1));
    let brush = build_brush(s.brush_radius);
    let max_pos = (n - 1) as f32 - 1e-3;
    // Bedrock: nothing erodes below the input's lowest point. Without this
    // floor, a closed basin is a runaway — every droplet falling into the
    // pit sees a steeper drop, gains more capacity, and digs it deeper.
    let bedrock = map.iter().fold(f32::INFINITY, |m, v| m.min(*v)) - 0.02;

    // --- hydraulic droplets --------------------------------------------
    for _ in 0..s.droplets {
        let mut x = rng.next_f32() * max_pos;
        let mut y = rng.next_f32() * max_pos;
        let (mut dx, mut dy) = (0.0f32, 0.0f32);
        let mut speed = 1.0f32;
        let mut water = 1.0f32;
        let mut sediment = 0.0f32;

        for _ in 0..s.lifetime {
            let (h, gx, gy) = height_and_gradient(&map, n, x, y);
            // inertia carries the droplet; the gradient bends it downhill
            dx = dx * s.inertia - gx * (1.0 - s.inertia);
            dy = dy * s.inertia - gy * (1.0 - s.inertia);
            let len = (dx * dx + dy * dy).sqrt();
            if len < 1e-8 {
                // flat and stopped: kick it in a random direction
                let a = rng.next_f32() * std::f32::consts::TAU;
                dx = a.cos();
                dy = a.sin();
            } else {
                dx /= len;
                dy /= len;
            }
            let nx = x + dx;
            let ny = y + dy;
            if nx < 0.0 || nx > max_pos || ny < 0.0 || ny > max_pos {
                break; // ran off the terrain
            }
            let (nh, _, _) = height_and_gradient(&map, n, nx, ny);
            let dh = nh - h;

            let capacity = (-dh).max(s.min_slope) * speed * water * s.capacity;
            if sediment > capacity || dh > 0.0 {
                // drop sediment: fill the pit when moving uphill, else shed
                // the surplus gradually
                let deposit = if dh > 0.0 {
                    dh.min(sediment)
                } else {
                    (sediment - capacity) * s.deposition
                };
                sediment -= deposit;
                // bilinear-distribute at the OLD position's cell corners
                let xi = (x.floor() as usize).min(n - 2);
                let yi = (y.floor() as usize).min(n - 2);
                let fx = x - xi as f32;
                let fy = y - yi as f32;
                let idx = yi * n + xi;
                map[idx] += deposit * (1.0 - fx) * (1.0 - fy);
                map[idx + 1] += deposit * fx * (1.0 - fy);
                map[idx + n] += deposit * (1.0 - fx) * fy;
                map[idx + n + 1] += deposit * fx * fy;
            } else {
                // carve, spread over the radial brush so channels stay
                // smooth instead of pitted; never dig below the next step
                let erode = ((capacity - sediment) * s.erosion_rate).min(-dh);
                let cx = x.round() as i32;
                let cy = y.round() as i32;
                let mut taken = 0.0;
                for &(bx, by, w) in &brush {
                    let px = cx + bx;
                    let py = cy + by;
                    if px >= 0 && px < n as i32 && py >= 0 && py < n as i32 {
                        let idx = py as usize * n + px as usize;
                        // never dig below bedrock (see above)
                        let amount = (erode * w).min((map[idx] - bedrock).max(0.0));
                        map[idx] -= amount;
                        taken += amount;
                    }
                }
                sediment += taken;
            }

            // capped: a droplet circling a deep pit must not build
            // unbounded capacity
            speed = (speed * speed + (-dh) * s.gravity).max(0.0).sqrt().min(16.0);
            water *= 1.0 - s.evaporation;
            x = nx;
            y = ny;
        }
    }

    // --- thermal relaxation --------------------------------------------
    // material slides to the steepest lower 4-neighbour while the slope
    // exceeds the angle of repose
    let talus = s.talus_angle_deg.to_radians().tan() * cell_norm;
    let mut delta_buf = vec![0.0f32; n * n];
    for _ in 0..s.thermal_iterations {
        for v in delta_buf.iter_mut() {
            *v = 0.0;
        }
        for y in 1..n - 1 {
            for x in 1..n - 1 {
                let idx = y * n + x;
                let h = map[idx];
                let mut steepest = 0.0f32;
                let mut target = idx;
                for nidx in [idx - 1, idx + 1, idx - n, idx + n] {
                    let diff = h - map[nidx];
                    if diff > steepest {
                        steepest = diff;
                        target = nidx;
                    }
                }
                if steepest > talus {
                    let moved = (steepest - talus) * 0.5 * s.thermal_strength;
                    delta_buf[idx] -= moved;
                    delta_buf[target] += moved;
                }
            }
        }
        for (v, d) in map.iter_mut().zip(&delta_buf) {
            *v += d;
        }
    }

    // --- gentle smoothing, blended in ----------------------------------
    if s.smoothing > 0.0 {
        let src = map.clone();
        for y in 1..n - 1 {
            for x in 1..n - 1 {
                let idx = y * n + x;
                let avg = (src[idx] + src[idx - 1] + src[idx + 1] + src[idx - n] + src[idx + n])
                    / 5.0;
                map[idx] += (avg - src[idx]) * s.smoothing;
            }
        }
    }

    map.iter().zip(heights).map(|(e, h)| e - h).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple test hill: a cone in the grid center.
    fn cone(resolution: u32) -> Vec<f32> {
        let n = resolution as usize + 1;
        let c = resolution as f32 / 2.0;
        (0..n * n)
            .map(|i| {
                let x = (i % n) as f32;
                let y = (i / n) as f32;
                let d = ((x - c).powi(2) + (y - c).powi(2)).sqrt() / c;
                (1.0 - d).max(0.0)
            })
            .collect()
    }

    #[test]
    fn deterministic_and_bounded() {
        let h = cone(64);
        let s = ErosionSettings {
            droplets: 5_000,
            ..ErosionSettings::default()
        };
        let a = erode_grid(&h, 64, 0.02, &s, 7);
        let b = erode_grid(&h, 64, 0.02, &s, 7);
        assert_eq!(a, b, "same seed must reproduce the bake");
        let c = erode_grid(&h, 64, 0.02, &s, 8);
        assert_ne!(a, c, "different seed must differ");
        assert!(a.iter().all(|v| v.is_finite()));
        // erosion reshapes but must not explode
        let max = a.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(max > 1e-4, "erosion must actually change the surface");
        assert!(max < 1.0, "delta out of scale: {max}");
    }

    #[test]
    fn hydraulic_carves_channels_into_a_noisy_slope() {
        // a tilted plane with hash bumps: flow converges into rills, which
        // is what distinguishes erosion from a uniform blur
        let res = 64u32;
        let n = res as usize + 1;
        let mut rng = Rng(99);
        let h: Vec<f32> = (0..n * n)
            .map(|i| {
                let y = (i / n) as f32 / res as f32;
                y * 0.8 + rng.next_f32() * 0.04
            })
            .collect();
        let s = ErosionSettings {
            droplets: 20_000,
            thermal_iterations: 0,
            smoothing: 0.0,
            ..ErosionSettings::default()
        };
        let delta = erode_grid(&h, res, 0.02, &s, 3);
        // the slope must lose material overall...
        let mid: Vec<f32> = (0..n * n)
            .filter(|i| {
                let y = i / n;
                (20..45).contains(&y)
            })
            .map(|i| delta[i])
            .collect();
        let mean = mid.iter().sum::<f32>() / mid.len() as f32;
        assert!(mean < 0.0, "slope should erode, mean {mean}");
        // ...and the carving must be CHANNELED: streaks of deep cuts
        // between lightly-touched interfluves, not a uniform lowering
        let std = (mid.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / mid.len() as f32)
            .sqrt();
        // (threshold calibrated against the bedrock-clamped sim: the old
        // 0.35 passed only because runaway pits inflated the variance)
        assert!(
            std > mean.abs() * 0.1,
            "carving should be channeled, not uniform (mean {mean:.4}, std {std:.4})"
        );
    }

    #[test]
    fn thermal_relaxes_a_spike_toward_the_talus_angle() {
        let res = 32u32;
        let n = res as usize + 1;
        let mut h = vec![0.0f32; n * n];
        h[16 * n + 16] = 1.0; // a needle
        let s = ErosionSettings {
            droplets: 1_000, // minimum; the needle is what we watch
            lifetime: 4,
            erosion_rate: 0.0,
            deposition: 0.0,
            thermal_iterations: 80,
            thermal_strength: 0.8,
            talus_angle_deg: 33.0,
            smoothing: 0.0,
            ..ErosionSettings::default()
        };
        let delta = erode_grid(&h, res, 0.05, &s, 1);
        let peak_after = h[16 * n + 16] + delta[16 * n + 16];
        assert!(
            peak_after < 0.5,
            "thermal pass must shed the needle, still {peak_after}"
        );
        // the shed material lands next door
        assert!(delta[16 * n + 17] > 0.0);
    }

    /// Regression: a closed basin used to be a positive-feedback runaway
    /// (droplets dug a pit, the pit steepened the drop, capacity grew) that
    /// left kilometer-deep spikes. The bedrock floor pins it.
    #[test]
    fn closed_basins_stay_bounded() {
        let res = 64u32;
        let n = res as usize + 1;
        // an inverted cone: everything drains INTO the center
        let c = res as f32 / 2.0;
        let h: Vec<f32> = (0..n * n)
            .map(|i| {
                let x = (i % n) as f32;
                let y = (i / n) as f32;
                let d = ((x - c).powi(2) + (y - c).powi(2)).sqrt() / c;
                d.min(1.0) * 0.8
            })
            .collect();
        let s = ErosionSettings {
            droplets: 60_000,
            ..ErosionSettings::default()
        };
        let delta = erode_grid(&h, res, 0.02, &s, 5);
        let min = h.iter().zip(&delta).map(|(a, b)| a + b).fold(f32::INFINITY, f32::min);
        assert!(
            min > -0.1,
            "the basin floor must stay at bedrock, went to {min}"
        );
        let worst = delta.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(worst < 1.0, "delta blew up: {worst}");
    }

    #[test]
    fn all_presets_are_sane() {
        for (name, s) in ErosionSettings::presets() {
            assert_eq!(s, s.sanitized(), "preset {name} outside sane ranges");
        }
        assert!(ErosionSettings::preset("natural").is_some());
        assert!(ErosionSettings::preset("nope").is_none());
    }
}
