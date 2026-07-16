//! Production-oriented material system inspired by the top features from
//! the Blender / Unreal comparison:
//!
//! 1. **Master materials + instances** — one master, many cheap variants
//! 2. **Principled-style PBR** — expanded metal/rough lobes + authoring fields
//! 3. **Material functions** — reusable presets / operators
//! 4. **World-position effects** — height snow, gradients, world-up tint
//! 5. **Material Parameter Collections (MPC)** — global wetness / snow / tint

use glam::Vec3;
use serde::{Deserialize, Serialize};

/// Stable id for a master material asset in the scene library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MaterialId(pub u64);

fn default_specular() -> f32 {
    0.5
}
fn one() -> f32 {
    1.0
}
fn default_occlusion() -> f32 {
    1.0
}

/// Principled-style PBR parameters (metal/rough workflow).
///
/// Fields that three-d can shade today are applied at render time; coat /
/// sheen / specular are first-class authoring data and are approximated into
/// the GPU material where possible.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Material {
    pub base_color: [f32; 3],
    pub roughness: f32,
    pub metallic: f32,
    /// Dielectric specular level (UE-style, default 0.5). Approximated.
    #[serde(default = "default_specular")]
    pub specular: f32,
    /// Emissive RGB (linear).
    #[serde(default)]
    pub emissive: [f32; 3],
    #[serde(default = "one")]
    pub emissive_strength: f32,
    /// Opacity 0..1 (1 = opaque). Values < 1 use transparent shading.
    #[serde(default = "one")]
    pub alpha: f32,
    /// Clear-coat weight 0..1 (cars, lacquer). Approximated into roughness.
    #[serde(default)]
    pub coat: f32,
    #[serde(default)]
    pub coat_roughness: f32,
    /// Cloth/fabric sheen 0..1. Approximated as a slight albedo lift.
    #[serde(default)]
    pub sheen: f32,
    /// Ambient occlusion strength 0..1 (1 = full AO contribution if present).
    #[serde(default = "default_occlusion")]
    pub occlusion: f32,
    /// Per-material world-space shading effect.
    #[serde(default)]
    pub world_effect: WorldPositionEffect,
}

impl Default for Material {
    fn default() -> Self {
        // Blender's default material gray
        Self {
            base_color: [0.8, 0.8, 0.8],
            roughness: 0.7,
            metallic: 0.0,
            specular: 0.5,
            emissive: [0.0, 0.0, 0.0],
            emissive_strength: 1.0,
            alpha: 1.0,
            coat: 0.0,
            coat_roughness: 0.0,
            sheen: 0.0,
            occlusion: 1.0,
            world_effect: WorldPositionEffect::None,
        }
    }
}

impl Material {
    /// Clamp every channel into a sane authoring range.
    pub fn clamped(mut self) -> Self {
        for c in &mut self.base_color {
            *c = c.clamp(0.0, 1.0);
        }
        self.roughness = self.roughness.clamp(0.0, 1.0);
        self.metallic = self.metallic.clamp(0.0, 1.0);
        self.specular = self.specular.clamp(0.0, 1.0);
        for c in &mut self.emissive {
            *c = (*c).max(0.0);
        }
        self.emissive_strength = self.emissive_strength.max(0.0);
        self.alpha = self.alpha.clamp(0.0, 1.0);
        self.coat = self.coat.clamp(0.0, 1.0);
        self.coat_roughness = self.coat_roughness.clamp(0.0, 1.0);
        self.sheen = self.sheen.clamp(0.0, 1.0);
        self.occlusion = self.occlusion.clamp(0.0, 1.0);
        self
    }

    /// Shader-facing approximation of advanced lobes for engines without
    /// native clear-coat / sheen (e.g. three-d PhysicalMaterial).
    ///
    /// - Coat mixes roughness toward `coat_roughness` and slightly brightens
    /// - Sheen lifts albedo
    /// - Specular > 0.5 slightly lowers roughness on dielectrics
    /// - Occlusion multiplies albedo toward a contact-shadow look
    pub fn for_shading(self) -> Material {
        let mut m = self.clamped();
        // Clear coat: smooth, reflective top layer
        if m.coat > 1e-4 {
            let coat_r = m.coat_roughness;
            m.roughness = m.roughness * (1.0 - m.coat * 0.65) + coat_r * m.coat * 0.35;
            for c in &mut m.base_color {
                *c = (*c * (1.0 - m.coat * 0.08) + m.coat * 0.08).clamp(0.0, 1.0);
            }
        }
        // Sheen: fabric edge lift → slight albedo brighten
        if m.sheen > 1e-4 {
            for c in &mut m.base_color {
                *c = (*c + (1.0 - *c) * m.sheen * 0.18).clamp(0.0, 1.0);
            }
            m.roughness = (m.roughness + m.sheen * 0.05).clamp(0.0, 1.0);
        }
        // Specular level (dielectrics): higher specular ≈ tighter highlight
        if m.metallic < 0.5 {
            let s = (m.specular - 0.5) * 2.0; // -1..1 around default
            m.roughness = (m.roughness - s * 0.08).clamp(0.0, 1.0);
        }
        // AO: darken albedo when occlusion < 1
        if m.occlusion < 0.999 {
            let k = 0.35 + 0.65 * m.occlusion;
            for c in &mut m.base_color {
                *c *= k;
            }
        }
        m
    }

    /// Effective emissive color after strength.
    pub fn emissive_rgb(&self) -> [f32; 3] {
        [
            self.emissive[0] * self.emissive_strength,
            self.emissive[1] * self.emissive_strength,
            self.emissive[2] * self.emissive_strength,
        ]
    }
}

/// World-space material effects (feature #4 from the comparison).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum WorldPositionEffect {
    #[default]
    None,
    /// Blend toward snow color as world Z rises from `start` to `end`.
    HeightSnow {
        start: f32,
        end: f32,
        #[serde(default = "default_snow_color")]
        color: [f32; 3],
    },
    /// Vertical color gradient in world Z between `min_z` and `max_z`.
    HeightGradient {
        bottom: [f32; 3],
        top: [f32; 3],
        min_z: f32,
        max_z: f32,
    },
    /// Tint toward `color` on surfaces facing world +Z (uses object up axis).
    WorldUpTint {
        amount: f32,
        #[serde(default = "default_snow_color")]
        color: [f32; 3],
    },
    /// Soft distance fade of albedo toward `color` from origin (world XY).
    RadialFade {
        inner: f32,
        outer: f32,
        #[serde(default)]
        color: [f32; 3],
    },
}

fn default_snow_color() -> [f32; 3] {
    [0.92, 0.95, 1.0]
}

impl WorldPositionEffect {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::HeightSnow { .. } => "Height snow",
            Self::HeightGradient { .. } => "Height gradient",
            Self::WorldUpTint { .. } => "World-up tint",
            Self::RadialFade { .. } => "Radial fade",
        }
    }

    /// Apply this effect given the object's world-space origin and +Z axis.
    pub fn apply(self, mut material: Material, world_origin: Vec3, world_up: Vec3) -> Material {
        match self {
            Self::None => material,
            Self::HeightSnow { start, end, color } => {
                let t = if (end - start).abs() < 1e-5 {
                    if world_origin.z >= end {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    ((world_origin.z - start) / (end - start)).clamp(0.0, 1.0)
                };
                if t > 0.0 {
                    for i in 0..3 {
                        material.base_color[i] =
                            material.base_color[i] * (1.0 - t) + color[i] * t;
                    }
                    // snow is rougher and non-metal
                    material.roughness = (material.roughness * (1.0 - t) + 0.95 * t).clamp(0.0, 1.0);
                    material.metallic *= 1.0 - t;
                }
                material
            }
            Self::HeightGradient {
                bottom,
                top,
                min_z,
                max_z,
            } => {
                let t = if (max_z - min_z).abs() < 1e-5 {
                    0.5
                } else {
                    ((world_origin.z - min_z) / (max_z - min_z)).clamp(0.0, 1.0)
                };
                for i in 0..3 {
                    material.base_color[i] = bottom[i] * (1.0 - t) + top[i] * t;
                }
                material
            }
            Self::WorldUpTint { amount, color } => {
                let up = world_up.normalize_or_zero();
                let facing = up.z.clamp(0.0, 1.0); // +Z world
                let t = (facing * amount.clamp(0.0, 1.0)).clamp(0.0, 1.0);
                for i in 0..3 {
                    material.base_color[i] =
                        material.base_color[i] * (1.0 - t) + color[i] * t;
                }
                material
            }
            Self::RadialFade { inner, outer, color } => {
                let d = (world_origin.x * world_origin.x + world_origin.y * world_origin.y).sqrt();
                let t = if (outer - inner).abs() < 1e-5 {
                    if d >= outer {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    ((d - inner) / (outer - inner)).clamp(0.0, 1.0)
                };
                for i in 0..3 {
                    material.base_color[i] =
                        material.base_color[i] * (1.0 - t) + color[i] * t;
                }
                material
            }
        }
    }
}

/// Optional per-field overrides for a material instance (feature #1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct MaterialOverrides {
    pub base_color: Option<[f32; 3]>,
    pub roughness: Option<f32>,
    pub metallic: Option<f32>,
    pub specular: Option<f32>,
    pub emissive: Option<[f32; 3]>,
    pub emissive_strength: Option<f32>,
    pub alpha: Option<f32>,
    pub coat: Option<f32>,
    pub coat_roughness: Option<f32>,
    pub sheen: Option<f32>,
    pub occlusion: Option<f32>,
    pub world_effect: Option<WorldPositionEffect>,
}

impl MaterialOverrides {
    pub fn is_empty(&self) -> bool {
        self.base_color.is_none()
            && self.roughness.is_none()
            && self.metallic.is_none()
            && self.specular.is_none()
            && self.emissive.is_none()
            && self.emissive_strength.is_none()
            && self.alpha.is_none()
            && self.coat.is_none()
            && self.coat_roughness.is_none()
            && self.sheen.is_none()
            && self.occlusion.is_none()
            && self.world_effect.is_none()
    }

    /// Layer overrides on top of a master (or any base) material.
    pub fn apply(&self, base: &Material) -> Material {
        Material {
            base_color: self.base_color.unwrap_or(base.base_color),
            roughness: self.roughness.unwrap_or(base.roughness),
            metallic: self.metallic.unwrap_or(base.metallic),
            specular: self.specular.unwrap_or(base.specular),
            emissive: self.emissive.unwrap_or(base.emissive),
            emissive_strength: self.emissive_strength.unwrap_or(base.emissive_strength),
            alpha: self.alpha.unwrap_or(base.alpha),
            coat: self.coat.unwrap_or(base.coat),
            coat_roughness: self.coat_roughness.unwrap_or(base.coat_roughness),
            sheen: self.sheen.unwrap_or(base.sheen),
            occlusion: self.occlusion.unwrap_or(base.occlusion),
            world_effect: self.world_effect.unwrap_or(base.world_effect),
        }
    }

    /// Capture the difference between an edited material and its master.
    pub fn from_diff(master: &Material, edited: &Material) -> Self {
        let mut o = Self::default();
        if edited.base_color != master.base_color {
            o.base_color = Some(edited.base_color);
        }
        if (edited.roughness - master.roughness).abs() > 1e-5 {
            o.roughness = Some(edited.roughness);
        }
        if (edited.metallic - master.metallic).abs() > 1e-5 {
            o.metallic = Some(edited.metallic);
        }
        if (edited.specular - master.specular).abs() > 1e-5 {
            o.specular = Some(edited.specular);
        }
        if edited.emissive != master.emissive {
            o.emissive = Some(edited.emissive);
        }
        if (edited.emissive_strength - master.emissive_strength).abs() > 1e-5 {
            o.emissive_strength = Some(edited.emissive_strength);
        }
        if (edited.alpha - master.alpha).abs() > 1e-5 {
            o.alpha = Some(edited.alpha);
        }
        if (edited.coat - master.coat).abs() > 1e-5 {
            o.coat = Some(edited.coat);
        }
        if (edited.coat_roughness - master.coat_roughness).abs() > 1e-5 {
            o.coat_roughness = Some(edited.coat_roughness);
        }
        if (edited.sheen - master.sheen).abs() > 1e-5 {
            o.sheen = Some(edited.sheen);
        }
        if (edited.occlusion - master.occlusion).abs() > 1e-5 {
            o.occlusion = Some(edited.occlusion);
        }
        if edited.world_effect != master.world_effect {
            o.world_effect = Some(edited.world_effect);
        }
        o
    }
}

/// Scene-level master material (feature #1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MasterMaterial {
    pub id: MaterialId,
    pub name: String,
    pub material: Material,
}

/// Global material parameters shared by every material at resolve time
/// (feature #5 — UE Material Parameter Collection).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MaterialParameterCollection {
    /// 0..1 — lowers roughness, slight darken (wet roads / rain).
    pub wetness: f32,
    /// 0..1 — global snow blend amount (multiplies height-snow effects).
    pub snow_amount: f32,
    /// World Z where global snow starts when `snow_amount` > 0.
    pub snow_height: f32,
    /// Multiplicative albedo tint (white = no change).
    pub global_tint: [f32; 3],
    /// Extra emissive boost for night / cinematic look.
    pub emissive_boost: f32,
}

impl Default for MaterialParameterCollection {
    fn default() -> Self {
        Self {
            wetness: 0.0,
            snow_amount: 0.0,
            snow_height: 2.0,
            global_tint: [1.0, 1.0, 1.0],
            emissive_boost: 1.0,
        }
    }
}

impl MaterialParameterCollection {
    /// Apply global modulators to a resolved material (before world effects
    /// that also read snow_amount, or after — caller decides). Here we apply
    /// wetness, tint, and emissive boost; snow height is applied separately
    /// in [`resolve_for_render`] so it can use world position.
    pub fn modulate(&self, mut m: Material) -> Material {
        let w = self.wetness.clamp(0.0, 1.0);
        if w > 1e-4 {
            m.roughness = (m.roughness * (1.0 - w * 0.75)).clamp(0.02, 1.0);
            for c in &mut m.base_color {
                *c *= 1.0 - w * 0.12;
            }
            // wet surfaces pick up a bit of clear-coat look
            m.coat = (m.coat + w * 0.35).clamp(0.0, 1.0);
            m.coat_roughness = (m.coat_roughness * (1.0 - w) + 0.05 * w).clamp(0.0, 1.0);
        }
        for i in 0..3 {
            m.base_color[i] = (m.base_color[i] * self.global_tint[i].max(0.0)).clamp(0.0, 1.0);
        }
        m.emissive_strength *= self.emissive_boost.max(0.0);
        m
    }
}

/// Built-in reusable material operators (feature #3 — Material Functions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MaterialFunction {
    Plastic,
    RoughPlastic,
    Metal,
    BrushedMetal,
    Rubber,
    Glass,
    Concrete,
    Wood,
    Ceramic,
    CarPaint,
    Fabric,
    EmissiveGlow,
    /// Modifier: apply wetness on top of the current material.
    MakeWet,
    /// Modifier: push metallic + low roughness.
    MakeMetal,
}

impl MaterialFunction {
    pub const ALL: &'static [MaterialFunction] = &[
        Self::Plastic,
        Self::RoughPlastic,
        Self::Metal,
        Self::BrushedMetal,
        Self::Rubber,
        Self::Glass,
        Self::Concrete,
        Self::Wood,
        Self::Ceramic,
        Self::CarPaint,
        Self::Fabric,
        Self::EmissiveGlow,
        Self::MakeWet,
        Self::MakeMetal,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Plastic => "MF Plastic",
            Self::RoughPlastic => "MF Rough plastic",
            Self::Metal => "MF Metal",
            Self::BrushedMetal => "MF Brushed metal",
            Self::Rubber => "MF Rubber",
            Self::Glass => "MF Glass",
            Self::Concrete => "MF Concrete",
            Self::Wood => "MF Wood",
            Self::Ceramic => "MF Ceramic",
            Self::CarPaint => "MF Car paint",
            Self::Fabric => "MF Fabric",
            Self::EmissiveGlow => "MF Emissive glow",
            Self::MakeWet => "MF Make wet",
            Self::MakeMetal => "MF Make metal",
        }
    }

    /// Apply the function. Presets replace most fields (keep base color when
    /// sensible); modifiers layer onto `base`.
    pub fn apply(self, base: &Material) -> Material {
        let mut m = *base;
        match self {
            Self::Plastic => {
                m.metallic = 0.0;
                m.roughness = 0.35;
                m.specular = 0.5;
                m.coat = 0.0;
                m.sheen = 0.0;
                m.alpha = 1.0;
            }
            Self::RoughPlastic => {
                m.metallic = 0.0;
                m.roughness = 0.75;
                m.specular = 0.4;
                m.coat = 0.0;
                m.sheen = 0.0;
                m.alpha = 1.0;
            }
            Self::Metal => {
                m.metallic = 1.0;
                m.roughness = 0.25;
                m.specular = 0.5;
                m.coat = 0.0;
                m.sheen = 0.0;
                m.alpha = 1.0;
            }
            Self::BrushedMetal => {
                m.metallic = 1.0;
                m.roughness = 0.45;
                m.specular = 0.5;
                m.coat = 0.0;
                m.sheen = 0.05;
                m.alpha = 1.0;
            }
            Self::Rubber => {
                m.metallic = 0.0;
                m.roughness = 0.9;
                m.specular = 0.25;
                m.coat = 0.0;
                m.sheen = 0.0;
                m.alpha = 1.0;
            }
            Self::Glass => {
                m.metallic = 0.0;
                m.roughness = 0.05;
                m.specular = 0.9;
                m.alpha = 0.25;
                m.coat = 0.0;
                m.sheen = 0.0;
            }
            Self::Concrete => {
                m.base_color = [0.55, 0.54, 0.52];
                m.metallic = 0.0;
                m.roughness = 0.92;
                m.specular = 0.35;
                m.occlusion = 0.85;
                m.coat = 0.0;
                m.sheen = 0.0;
                m.alpha = 1.0;
            }
            Self::Wood => {
                m.base_color = [0.45, 0.28, 0.14];
                m.metallic = 0.0;
                m.roughness = 0.65;
                m.specular = 0.4;
                m.coat = 0.15;
                m.coat_roughness = 0.4;
                m.sheen = 0.0;
                m.alpha = 1.0;
            }
            Self::Ceramic => {
                m.base_color = [0.9, 0.9, 0.88];
                m.metallic = 0.0;
                m.roughness = 0.2;
                m.specular = 0.55;
                m.coat = 0.4;
                m.coat_roughness = 0.1;
                m.alpha = 1.0;
            }
            Self::CarPaint => {
                m.metallic = 0.6;
                m.roughness = 0.35;
                m.coat = 1.0;
                m.coat_roughness = 0.08;
                m.specular = 0.5;
                m.sheen = 0.0;
                m.alpha = 1.0;
            }
            Self::Fabric => {
                m.metallic = 0.0;
                m.roughness = 0.85;
                m.sheen = 0.7;
                m.specular = 0.2;
                m.coat = 0.0;
                m.alpha = 1.0;
            }
            Self::EmissiveGlow => {
                m.emissive = m.base_color;
                m.emissive_strength = 2.5;
                m.roughness = 0.5;
                m.metallic = 0.0;
            }
            Self::MakeWet => {
                m.roughness = (m.roughness * 0.35).clamp(0.02, 1.0);
                m.coat = (m.coat + 0.4).clamp(0.0, 1.0);
                m.coat_roughness = 0.08;
                for c in &mut m.base_color {
                    *c *= 0.9;
                }
            }
            Self::MakeMetal => {
                m.metallic = 1.0;
                m.roughness = m.roughness.min(0.3);
            }
        }
        m.clamped()
    }
}

/// Resolve an object's authored material from master + overrides (no MPC,
/// no world effects).
pub fn resolve_authored(
    inline: &Material,
    master_id: Option<MaterialId>,
    overrides: &MaterialOverrides,
    masters: &[MasterMaterial],
) -> Material {
    if let Some(id) = master_id {
        if let Some(master) = masters.iter().find(|m| m.id == id) {
            return overrides.apply(&master.material).clamped();
        }
    }
    // orphaned master id → fall back to inline snapshot
    inline.clamped()
}

/// Full resolve for viewport / export: authored → MPC → world effects → shading approx.
pub fn resolve_for_render(
    inline: &Material,
    master_id: Option<MaterialId>,
    overrides: &MaterialOverrides,
    masters: &[MasterMaterial],
    mpc: &MaterialParameterCollection,
    world_origin: Vec3,
    world_up: Vec3,
) -> Material {
    let mut m = resolve_authored(inline, master_id, overrides, masters);
    m = mpc.modulate(m);

    // Global snow (MPC) as a HeightSnow layer on top of the material's own effect.
    let snow = mpc.snow_amount.clamp(0.0, 1.0);
    if snow > 1e-4 {
        let end = mpc.snow_height + 1.5;
        let layer = WorldPositionEffect::HeightSnow {
            start: mpc.snow_height,
            end,
            color: [0.92, 0.95, 1.0],
        };
        let snowed = layer.apply(m, world_origin, world_up);
        for i in 0..3 {
            m.base_color[i] =
                m.base_color[i] * (1.0 - snow) + snowed.base_color[i] * snow;
        }
        m.roughness = m.roughness * (1.0 - snow) + snowed.roughness * snow;
        m.metallic *= 1.0 - snow * 0.85;
    }

    m = m.world_effect.apply(m, world_origin, world_up);
    m.for_shading()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_layer_on_master() {
        let master = Material {
            base_color: [1.0, 0.0, 0.0],
            roughness: 0.5,
            metallic: 0.0,
            ..Default::default()
        };
        let o = MaterialOverrides {
            roughness: Some(0.1),
            ..Default::default()
        };
        let r = o.apply(&master);
        assert_eq!(r.base_color, [1.0, 0.0, 0.0]);
        assert!((r.roughness - 0.1).abs() < 1e-5);
    }

    #[test]
    fn mpc_wetness_lowers_roughness() {
        let m = Material {
            roughness: 0.8,
            ..Default::default()
        };
        let mpc = MaterialParameterCollection {
            wetness: 1.0,
            ..Default::default()
        };
        let out = mpc.modulate(m);
        assert!(out.roughness < 0.4);
        assert!(out.coat > 0.0);
    }

    #[test]
    fn material_function_glass_is_transparent() {
        let m = MaterialFunction::Glass.apply(&Material::default());
        assert!(m.alpha < 0.5);
        assert!(m.roughness < 0.2);
    }

    #[test]
    fn height_snow_at_peak_is_white() {
        let m = Material {
            base_color: [0.2, 0.4, 0.1],
            ..Default::default()
        };
        let effect = WorldPositionEffect::HeightSnow {
            start: 0.0,
            end: 1.0,
            color: [1.0, 1.0, 1.0],
        };
        let out = effect.apply(m, Vec3::new(0.0, 0.0, 1.0), Vec3::Z);
        assert!((out.base_color[0] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn resolve_falls_back_when_master_missing() {
        let inline = Material {
            base_color: [0.1, 0.2, 0.3],
            ..Default::default()
        };
        let r = resolve_authored(
            &inline,
            Some(MaterialId(99)),
            &MaterialOverrides::default(),
            &[],
        );
        assert_eq!(r.base_color, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn old_json_three_field_material_deserializes() {
        let json = r#"{"base_color":[0.5,0.5,0.5],"roughness":0.4,"metallic":0.2}"#;
        let m: Material = serde_json::from_str(json).unwrap();
        assert!((m.roughness - 0.4).abs() < 1e-5);
        assert!((m.specular - 0.5).abs() < 1e-5);
        assert!((m.alpha - 1.0).abs() < 1e-5);
        assert_eq!(m.world_effect, WorldPositionEffect::None);
    }
}
