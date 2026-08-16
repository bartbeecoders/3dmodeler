//! The app's vector maths, on glam.
//!
//! This module exists because the app used to get its maths from `three_d`,
//! which re-exports cgmath, while `modeler-core` and Aether are both glam. Every
//! value crossing that line needed converting, and there were 645 places where
//! it could go wrong.
//!
//! Most of the migration was free: glam's `Vec3` already has `dot`, `cross`,
//! `normalize`, `extend`, `truncate`, `distance` and `lerp` with the same names
//! and the same meanings cgmath gives them. What follows is only the handful of
//! places the two libraries genuinely disagree.

pub use modeler_core::glam::{
    vec2, vec3, vec4, Mat3, Mat4, Quat, Vec2, Vec3, Vec4,
};

/// cgmath's `Point3::to_vec`, kept as a no-op.
///
/// cgmath distinguishes a point from a vector at the type level; glam does not,
/// and `Vec3` is both. Rather than delete 27 call sites that each say something
/// true about intent — *this position is being used as an offset* — the
/// conversion stays and does nothing.
pub trait ToVec {
    fn to_vec(self) -> Self;
}

impl ToVec for Vec3 {
    #[inline]
    fn to_vec(self) -> Self {
        self
    }
}

impl ToVec for Vec2 {
    #[inline]
    fn to_vec(self) -> Self {
        self
    }
}

/// cgmath's `magnitude` family, spelled the way glam spells it.
///
/// Provided so ported code reads consistently rather than mixing the two
/// vocabularies; new code should prefer `length`.
pub trait Magnitude {
    fn magnitude(self) -> f32;
    fn magnitude2(self) -> f32;
}

impl Magnitude for Vec3 {
    #[inline]
    fn magnitude(self) -> f32 {
        self.length()
    }
    #[inline]
    fn magnitude2(self) -> f32 {
        self.length_squared()
    }
}

impl Magnitude for Vec2 {
    #[inline]
    fn magnitude(self) -> f32 {
        self.length()
    }
    #[inline]
    fn magnitude2(self) -> f32 {
        self.length_squared()
    }
}

/// An angle, stored in radians.
///
/// cgmath has `Deg` and `Rad` as distinct types so a function taking an angle
/// cannot be handed the wrong unit. glam takes bare `f32` radians everywhere,
/// which is a real loss: `perspective(60.0, ..)` compiles and gives a scene
/// viewed through a pinhole. Keeping one wrapper type preserves the property
/// that mattered, without the arithmetic-trait weight cgmath carries.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Default)]
pub struct Angle(f32);

impl Angle {
    #[inline]
    pub fn from_degrees(d: f32) -> Self {
        Self(d.to_radians())
    }

    #[inline]
    pub fn from_radians(r: f32) -> Self {
        Self(r)
    }

    #[inline]
    pub fn radians(self) -> f32 {
        self.0
    }

    #[inline]
    pub fn degrees(self) -> f32 {
        self.0.to_degrees()
    }
}

/// An angle in degrees. Mirrors `three_d::degrees`.
#[inline]
pub fn degrees(d: f32) -> Angle {
    Angle::from_degrees(d)
}

/// An angle in radians. Mirrors `three_d::radians`.
#[inline]
pub fn radians(r: f32) -> Angle {
    Angle::from_radians(r)
}

/// An 8-bit sRGB colour with alpha, as three-d spells it.
///
/// The renderer wants linear `Vec4`, and [`Self::to_linear`] is the only
/// sanctioned way there — see its note on why the conversion is not `x / 255`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Srgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Srgba {
    pub const WHITE: Self = Self::new(255, 255, 255, 255);
    pub const BLACK: Self = Self::new(0, 0, 0, 255);

    #[inline]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    #[inline]
    pub const fn new_opaque(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 255)
    }

    /// Linear RGBA, which is what every shader in the engine wants.
    ///
    /// The RGB channels go through the sRGB transfer function; alpha does not,
    /// because alpha is coverage rather than colour and was never encoded.
    /// Dividing by 255 and stopping — the tempting version — leaves every
    /// mid-tone about 40% too bright, which reads as "the overlay colours look
    /// washed out" rather than as a maths error.
    pub fn to_linear(self) -> Vec4 {
        #[inline]
        fn channel(v: u8) -> f32 {
            let s = v as f32 / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        Vec4::new(channel(self.r), channel(self.g), channel(self.b), self.a as f32 / 255.0)
    }
}

/// A rectangle of the window, in physical pixels. Mirrors `three_d::Viewport`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Viewport {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    #[inline]
    pub fn new_at_origo(width: u32, height: u32) -> Self {
        Self { x: 0, y: 0, width, height }
    }

    /// Width over height, guarding the zero-sized window a minimise produces.
    #[inline]
    pub fn aspect(&self) -> f32 {
        if self.height == 0 {
            1.0
        } else {
            self.width as f32 / self.height as f32
        }
    }
}

/// A point in physical pixels. Mirrors `three_d::PhysicalPoint`.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct PhysicalPoint {
    pub x: f32,
    pub y: f32,
}

impl From<PhysicalPoint> for Vec2 {
    #[inline]
    fn from(p: PhysicalPoint) -> Self {
        Vec2::new(p.x, p.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_vec_is_the_identity() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert_eq!(v.to_vec(), v);
    }

    #[test]
    fn angles_round_trip_through_both_units() {
        let a = degrees(90.0);
        assert!((a.radians() - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert!((a.degrees() - 90.0).abs() < 1e-4);
        let r = radians(std::f32::consts::PI);
        assert!((r.degrees() - 180.0).abs() < 1e-4);
    }

    #[test]
    fn srgb_endpoints_are_exact() {
        // The transfer function must pin both ends, or white stops being white.
        let w = Srgba::WHITE.to_linear();
        assert!((w.x - 1.0).abs() < 1e-6 && (w.w - 1.0).abs() < 1e-6);
        let b = Srgba::BLACK.to_linear();
        assert!(b.x.abs() < 1e-6 && (b.w - 1.0).abs() < 1e-6);
    }

    #[test]
    fn srgb_midtones_are_decoded_not_just_scaled() {
        // The bug this guards: `v / 255` looks plausible and leaves mid-grey far
        // too bright. 128/255 is 0.502 encoded and ~0.216 linear.
        let mid = Srgba::new(128, 128, 128, 255).to_linear();
        assert!(
            (mid.x - 0.2159).abs() < 1e-3,
            "mid grey decoded to {}, which is the un-decoded value",
            mid.x
        );
        assert!(mid.x < 0.502 - 0.1, "the value was not decoded at all");
    }

    #[test]
    fn srgb_alpha_is_never_decoded() {
        // Alpha is coverage. Running it through the curve makes every
        // half-transparent overlay too faint.
        let half = Srgba::new(255, 255, 255, 128);
        assert!((half.to_linear().w - 128.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn a_zero_height_viewport_does_not_divide_by_zero() {
        // Minimising a window reports 0x0, and an aspect of NaN poisons the
        // projection matrix for every frame after it comes back.
        let v = Viewport { x: 0, y: 0, width: 800, height: 0 };
        assert!(v.aspect().is_finite());
        assert_eq!(Viewport::new_at_origo(1600, 800).aspect(), 2.0);
    }
}
