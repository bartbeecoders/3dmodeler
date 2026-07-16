//! Core document model for the 3D modeler.
//!
//! This crate holds the scene graph, mesh primitives, and (in later phases)
//! the command/undo system and serialization. It knows nothing about
//! rendering or physics — those live in `modeler-app`.

pub mod boolean;
pub mod library;
pub mod material;
pub mod mesh;

use glam::{Quat, Vec2, Vec3};
pub use boolean::{mesh_boolean, mesh_to_frame, BooleanOp};
pub use glam;
pub use library::{Library, LibraryAsset};
pub use material::{
    resolve_authored, resolve_for_render, MasterMaterial, Material, MaterialFunction,
    MaterialId, MaterialOverrides, MaterialParameterCollection, WorldPositionEffect,
};
pub use mesh::{MeshData, WallCutout};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Stable identifier for an object in the scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectId(pub u64);

/// Location / rotation / scale, Blender-style.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub location: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            location: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }
}

impl Transform {
    /// Map a local-space point to this transform's space.
    pub fn transform_point(&self, p: Vec3) -> Vec3 {
        self.location + self.rotation * (p * self.scale)
    }

    /// Map a point from this transform's space back to local space (the
    /// inverse of `transform_point`, with zero-scale guarded).
    pub fn inverse_transform_point(&self, p: Vec3) -> Vec3 {
        let safe_scale = Vec3::new(
            if self.scale.x.abs() < 1e-9 { 1.0 } else { self.scale.x },
            if self.scale.y.abs() < 1e-9 { 1.0 } else { self.scale.y },
            if self.scale.z.abs() < 1e-9 { 1.0 } else { self.scale.z },
        );
        (self.rotation.inverse() * (p - self.location)) / safe_scale
    }

    /// Change the rotation while keeping the given LOCAL point fixed —
    /// the object rotates around that point instead of its origin.
    pub fn set_rotation_about(&mut self, rotation: Quat, local_point: Vec3) {
        let fixed = self.transform_point(local_point);
        self.rotation = rotation.normalize();
        self.location = fixed - self.rotation * (local_point * self.scale);
    }

    /// Compose parent ∘ child (child expressed in parent space).
    /// Exact for uniform scales; the usual SRT approximation otherwise.
    pub fn compose(parent: &Transform, child: &Transform) -> Transform {
        Transform {
            location: parent.location + parent.rotation * (parent.scale * child.location),
            rotation: (parent.rotation * child.rotation).normalize(),
            scale: parent.scale * child.scale,
        }
    }

    /// Express a world transform in this (parent) transform's local space:
    /// the inverse of `compose(self, result) == world`.
    pub fn to_local(&self, world: &Transform) -> Transform {
        let inv_rot = self.rotation.inverse();
        let safe_scale = Vec3::new(
            if self.scale.x.abs() < 1e-9 { 1.0 } else { self.scale.x },
            if self.scale.y.abs() < 1e-9 { 1.0 } else { self.scale.y },
            if self.scale.z.abs() < 1e-9 { 1.0 } else { self.scale.z },
        );
        Transform {
            location: (inv_rot * (world.location - self.location)) / safe_scale,
            rotation: (inv_rot * world.rotation).normalize(),
            scale: world.scale / safe_scale,
        }
    }
}

/// Kinds of light sources (Add ▸ Light). Position and orientation come from
/// the object transform; Sun and Spot shine along the object's local -Z axis
/// (Blender's convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightKind {
    /// Shines in all directions from a point; intensity falls off with
    /// distance.
    Point,
    /// Parallel rays from infinitely far away (direction only, no falloff).
    Sun,
    /// A cone of light along -Z with an adjustable angle.
    Spot,
}

impl LightKind {
    pub const ALL: [LightKind; 3] = [LightKind::Point, LightKind::Sun, LightKind::Spot];

    pub fn label(self) -> &'static str {
        match self {
            LightKind::Point => "Point",
            LightKind::Sun => "Sun",
            LightKind::Spot => "Spot",
        }
    }
}

/// Roof shapes for `Primitive::Roof` (Add ▸ Roof).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoofKind {
    /// Pyramid: four slopes meeting in a single point over the center.
    Point,
    /// Two slopes meeting at a ridge; vertical triangular gable ends.
    Gable,
    /// Four slopes: the ridge is pulled in from the ends so all sides pitch.
    Hip,
    /// A plain slab (the height is its thickness).
    Flat,
    /// One slope across the whole footprint, rising toward the high eave.
    Shed,
    /// Barn roof: each side breaks into a steep lower and a shallow upper
    /// slope; vertical gable ends.
    Gambrel,
    /// Hip version of the gambrel: steep lower slopes all around, shallow
    /// upper slopes, and a small flat top.
    Mansard,
}

impl RoofKind {
    pub const ALL: [RoofKind; 7] = [
        RoofKind::Point,
        RoofKind::Gable,
        RoofKind::Hip,
        RoofKind::Flat,
        RoofKind::Shed,
        RoofKind::Gambrel,
        RoofKind::Mansard,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RoofKind::Point => "Point",
            RoofKind::Gable => "Gable",
            RoofKind::Hip => "Hip",
            RoofKind::Flat => "Flat",
            RoofKind::Shed => "Shed",
            RoofKind::Gambrel => "Gambrel",
            RoofKind::Mansard => "Mansard",
        }
    }

    pub fn from_name(name: &str) -> Option<RoofKind> {
        Self::ALL
            .into_iter()
            .find(|k| k.label().eq_ignore_ascii_case(name.trim()))
    }

    /// Sensible rise for a footprint whose shorter side is `span` — used
    /// when a roof is created (the sidebar can change it afterwards).
    pub fn default_height(self, span: f32) -> f32 {
        match self {
            RoofKind::Flat => 0.2,
            RoofKind::Shed => (0.25 * span).max(0.3),
            RoofKind::Gable | RoofKind::Hip => (0.35 * span).max(0.3),
            RoofKind::Point | RoofKind::Gambrel | RoofKind::Mansard => {
                (0.45 * span).max(0.3)
            }
        }
    }

    /// Whether the ridge / slope direction matters for this kind (Flat,
    /// Point and Mansard are the same in both orientations).
    pub fn oriented(self) -> bool {
        matches!(
            self,
            RoofKind::Shed | RoofKind::Gable | RoofKind::Hip | RoofKind::Gambrel
        )
    }
}

/// Primitive shapes with their creation parameters. Defaults match Blender's
/// Add > Mesh entries.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Primitive {
    Plane { size: f32 },
    Cube { size: f32 },
    UvSphere { segments: u32, rings: u32, radius: f32 },
    IcoSphere { subdivisions: u32, radius: f32 },
    Cylinder { vertices: u32, radius: f32, depth: f32 },
    Cone { vertices: u32, radius_bottom: f32, radius_top: f32, depth: f32 },
    Torus { major_segments: u32, minor_segments: u32, major_radius: f32, minor_radius: f32 },
    /// Building wall segment: origin at its start, running along local +X,
    /// standing on z = 0, thickness centered on the X axis. Door/window
    /// openings live on the OBJECT (`Object::cutouts`), not here.
    Wall { length: f32, height: f32, thickness: f32 },
    /// Floor slab: a thin box centered on the origin in XY, standing on
    /// z = 0 (top face at z = thickness), so walls at z = 0 sit in it like
    /// a poured slab. Add ▸ Floor sizes it to the selected walls; when they
    /// close a loop the slab follows their shape via the footprint polygon
    /// on the OBJECT (`Object::floor_outline`), not here — width/depth then
    /// only mirror the outline's bounds.
    Floor { width: f32, depth: f32, thickness: f32 },
    /// Roof lid: a watertight solid standing on z = 0, centered on the
    /// origin in XY, rising to z = `height`. `width` × `depth` is the
    /// footprint it covers (the wall rectangle); `overhang` extends the
    /// eaves past it on all four sides. For the oriented kinds the ridge
    /// (shed: the high eave) runs along local X when `ridge_x`, else Y.
    Roof {
        kind: RoofKind,
        width: f32,
        depth: f32,
        height: f32,
        overhang: f32,
        ridge_x: bool,
    },
    /// Empty point (Blender's plain-axes empty): three thin axis lines
    /// crossing at the origin, ±`size` long. A marker / grouping parent —
    /// it never collides or simulates.
    Empty { size: f32 },
    /// Light source. The mesh is only a viewport gizmo (emissive, pickable);
    /// like `Empty` it never collides or simulates. `spot_angle_deg` is the
    /// full cone angle, used by `LightKind::Spot` only; `shadows` applies to
    /// Sun and Spot (point lights cannot cast shadows in the renderer).
    Light {
        kind: LightKind,
        color: [f32; 3],
        intensity: f32,
        spot_angle_deg: f32,
        shadows: bool,
    },
    /// Physical rope: a flexible chain of segment bodies between two ends.
    /// In local space the rope runs along +X from the origin to
    /// `(length, 0, 0)`. Each end can be anchored to another object via
    /// `Object::rope_start` / `Object::rope_end`. When the simulation plays,
    /// the rope sags, swings and collides under gravity.
    Rope {
        /// Rest / maximum length in meters (sum of segment max lengths).
        length: f32,
        /// Visual and collision radius of the cord.
        radius: f32,
        /// Number of physics links (nodes = segments + 1). Clamped 2..=64.
        segments: u32,
    },
}

/// One end of a `Primitive::Rope`: free, or pinned to a point on another object.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct RopeEnd {
    /// Object this end is pinned to. `None` = free (the end sits at the
    /// rope's own local start/end and moves with the rope body).
    #[serde(default)]
    pub object: Option<ObjectId>,
    /// Attachment point in the target object's local space. Ignored when
    /// `object` is `None`. Defaults to the origin; set to the target's
    /// `anchor` when attaching "to its anchor point".
    #[serde(default)]
    pub local_point: Vec3,
}

impl Primitive {
    /// All primitives with Blender-default parameters, in Add-menu order.
    pub fn catalog() -> [Primitive; 8] {
        [
            Primitive::Plane { size: 2.0 },
            Primitive::Cube { size: 2.0 },
            Primitive::UvSphere { segments: 32, rings: 16, radius: 1.0 },
            Primitive::IcoSphere { subdivisions: 2, radius: 1.0 },
            Primitive::Cylinder { vertices: 32, radius: 1.0, depth: 2.0 },
            Primitive::Cone { vertices: 32, radius_bottom: 1.0, radius_top: 0.0, depth: 2.0 },
            Primitive::Torus { major_segments: 48, minor_segments: 12, major_radius: 1.0, minor_radius: 0.25 },
            Primitive::Empty { size: 1.0 },
        ]
    }

    /// The three light kinds with sensible defaults, in Add-menu order.
    pub fn light_catalog() -> [Primitive; 3] {
        let light = |kind, intensity| Primitive::Light {
            kind,
            color: [1.0, 1.0, 1.0],
            intensity,
            spot_angle_deg: 45.0,
            shadows: true,
        };
        [
            light(LightKind::Point, 3.0),
            light(LightKind::Sun, 1.5),
            light(LightKind::Spot, 5.0),
        ]
    }

    pub fn is_light(&self) -> bool {
        matches!(self, Primitive::Light { .. })
    }

    pub fn is_rope(&self) -> bool {
        matches!(self, Primitive::Rope { .. })
    }

    /// Base object name, matching Blender's naming.
    pub fn base_name(&self) -> &'static str {
        match self {
            Primitive::Plane { .. } => "Plane",
            Primitive::Cube { .. } => "Cube",
            Primitive::UvSphere { .. } => "Sphere",
            Primitive::IcoSphere { .. } => "Icosphere",
            Primitive::Cylinder { .. } => "Cylinder",
            Primitive::Cone { .. } => "Cone",
            Primitive::Torus { .. } => "Torus",
            Primitive::Wall { .. } => "Wall",
            Primitive::Floor { .. } => "Floor",
            Primitive::Roof { .. } => "Roof",
            Primitive::Empty { .. } => "Empty",
            Primitive::Light { kind, .. } => kind.label(),
            Primitive::Rope { .. } => "Rope",
        }
    }

    /// Radius of the bounding sphere around the local origin.
    pub fn bounding_radius(&self) -> f32 {
        match *self {
            Primitive::Plane { size } => size * std::f32::consts::FRAC_1_SQRT_2,
            Primitive::Cube { size } => 0.5 * size * 3f32.sqrt(),
            Primitive::UvSphere { radius, .. } | Primitive::IcoSphere { radius, .. } => radius,
            Primitive::Cylinder { radius, depth, .. } => (radius * radius + 0.25 * depth * depth).sqrt(),
            Primitive::Cone { radius_bottom, radius_top, depth, .. } => {
                let r = radius_bottom.max(radius_top);
                (r * r + 0.25 * depth * depth).sqrt()
            }
            Primitive::Torus { major_radius, minor_radius, .. } => major_radius + minor_radius,
            // origin at the start-bottom corner: the far top corner is the
            // most distant point
            Primitive::Wall { length, height, thickness } => {
                (length * length + 0.25 * thickness * thickness + height * height).sqrt()
            }
            // origin at the bottom center: a top corner is farthest
            Primitive::Floor { width, depth, thickness } => {
                (0.25 * (width * width + depth * depth) + thickness * thickness).sqrt()
            }
            // ditto, with the eaves extended by the overhang
            Primitive::Roof { width, depth, height, overhang, .. } => {
                let hx = 0.5 * width + overhang.max(0.0);
                let hy = 0.5 * depth + overhang.max(0.0);
                (hx * hx + hy * hy + height * height).sqrt()
            }
            Primitive::Empty { size } => size,
            // + 0.01: spoke corners stick out past the nominal extents
            Primitive::Light { kind, spot_angle_deg, .. } => match kind {
                LightKind::Point => mesh::POINT_GIZMO_EXTENT + 0.01,
                LightKind::Sun => mesh::SUN_GIZMO_EXTENT + 0.01,
                LightKind::Spot => {
                    let r = mesh::spot_gizmo_radius(spot_angle_deg);
                    (mesh::SPOT_GIZMO_LENGTH * mesh::SPOT_GIZMO_LENGTH + r * r).sqrt() + 0.01
                }
            },
            // origin at the start; far end + radius is the farthest point
            Primitive::Rope { length, radius, .. } => {
                ((length * length) + 2.0 * radius * radius).sqrt()
            }
        }
    }

    /// Full extents (width, depth, height) of the primitive, unscaled.
    pub fn dimensions(&self) -> Vec3 {
        match *self {
            Primitive::Plane { size } => Vec3::new(size, size, 0.0),
            Primitive::Cube { size } => Vec3::splat(size),
            Primitive::UvSphere { radius, .. } | Primitive::IcoSphere { radius, .. } => {
                Vec3::splat(2.0 * radius)
            }
            Primitive::Cylinder { radius, depth, .. } => {
                Vec3::new(2.0 * radius, 2.0 * radius, depth)
            }
            Primitive::Cone { radius_bottom, radius_top, depth, .. } => {
                let r = 2.0 * radius_bottom.max(radius_top);
                Vec3::new(r, r, depth)
            }
            Primitive::Torus { major_radius, minor_radius, .. } => {
                let d = 2.0 * (major_radius + minor_radius);
                Vec3::new(d, d, 2.0 * minor_radius)
            }
            Primitive::Wall { length, height, thickness } => {
                Vec3::new(length, thickness, height)
            }
            Primitive::Floor { width, depth, thickness } => {
                Vec3::new(width, depth, thickness)
            }
            Primitive::Roof { width, depth, height, overhang, .. } => Vec3::new(
                width + 2.0 * overhang.max(0.0),
                depth + 2.0 * overhang.max(0.0),
                height,
            ),
            Primitive::Empty { size } => Vec3::splat(2.0 * size),
            Primitive::Light { kind, spot_angle_deg, .. } => match kind {
                LightKind::Point => Vec3::splat(2.0 * mesh::POINT_GIZMO_EXTENT),
                LightKind::Sun => Vec3::new(0.9, 0.9, mesh::SUN_GIZMO_EXTENT + 0.45),
                LightKind::Spot => {
                    let r = mesh::spot_gizmo_radius(spot_angle_deg);
                    Vec3::new(2.0 * r, 2.0 * r, mesh::SPOT_GIZMO_LENGTH)
                }
            },
            Primitive::Rope { length, radius, .. } => {
                Vec3::new(length, 2.0 * radius, 2.0 * radius)
            }
        }
    }

    /// Distance from the local origin to the lowest point (unscaled).
    pub fn bottom_offset(&self) -> f32 {
        match *self {
            Primitive::Plane { .. } => 0.0,
            Primitive::Cube { size } => 0.5 * size,
            Primitive::UvSphere { radius, .. } | Primitive::IcoSphere { radius, .. } => radius,
            Primitive::Cylinder { depth, .. } | Primitive::Cone { depth, .. } => 0.5 * depth,
            Primitive::Torus { minor_radius, .. } => minor_radius,
            Primitive::Wall { .. } => 0.0, // stands on its own floor line
            Primitive::Floor { .. } => 0.0, // ditto
            Primitive::Roof { .. } => 0.0, // sits on the wall tops
            Primitive::Empty { size } => size,
            Primitive::Light { kind, .. } => match kind {
                LightKind::Point => mesh::POINT_GIZMO_EXTENT,
                LightKind::Sun => mesh::SUN_GIZMO_EXTENT,
                LightKind::Spot => mesh::SPOT_GIZMO_LENGTH,
            },
            Primitive::Rope { radius, .. } => radius,
        }
    }

    /// Generate the triangle mesh, flat- or smooth-shaded.
    pub fn generate(&self, smooth: bool) -> MeshData {
        // light gizmos come with their own normals; the smooth flag is moot
        if let Primitive::Light { kind, spot_angle_deg, .. } = *self {
            return mesh::light_gizmo(kind, spot_angle_deg);
        }
        let m = match *self {
            Primitive::Plane { size } => mesh::plane(size),
            Primitive::Cube { size } => mesh::cube(size),
            Primitive::UvSphere { segments, rings, radius } => mesh::uv_sphere(segments, rings, radius),
            Primitive::IcoSphere { subdivisions, radius } => mesh::ico_sphere(subdivisions, radius),
            Primitive::Cylinder { vertices, radius, depth } => mesh::frustum(vertices, radius, radius, depth),
            Primitive::Cone { vertices, radius_bottom, radius_top, depth } => {
                mesh::frustum(vertices, radius_bottom, radius_top, depth)
            }
            Primitive::Torus { major_segments, minor_segments, major_radius, minor_radius } => {
                mesh::torus(major_segments, minor_segments, major_radius, minor_radius)
            }
            // solid wall; cutouts need the object and go through render_mesh
            Primitive::Wall { length, height, thickness } => {
                mesh::wall(length, height, thickness, &[])
            }
            Primitive::Floor { width, depth, thickness } => {
                mesh::floor(width, depth, thickness)
            }
            Primitive::Roof { kind, width, depth, height, overhang, ridge_x } => {
                mesh::roof(kind, width, depth, height, overhang, ridge_x)
            }
            Primitive::Empty { size } => mesh::empty_axes(size),
            Primitive::Light { .. } => unreachable!("handled above"),
            Primitive::Rope { length, radius, .. } => mesh::rope(length, radius),
        };
        if smooth {
            m
        } else {
            m.into_flat()
        }
    }
}

/// One entry in an object's modifier stack: a non-destructive mesh effect,
/// applied top to bottom at DISPLAY time (Blender's modifier system). The
/// base mesh stays the editing cage and the collision shape until the user
/// applies the stack, which bakes the result into `Object::edited_mesh`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Modifier {
    /// Live in the viewport preview and included when applying. Disabled
    /// modifiers stay in the stack but have no effect.
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub kind: ModifierKind,
}

fn default_true() -> bool {
    true
}

impl Modifier {
    pub fn new(kind: ModifierKind) -> Self {
        Self { enabled: true, kind }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ModifierKind {
    /// Catmull-Clark subdivision surface (Blender's subsurf).
    Subdivision { levels: u8 },
    /// CSG against another scene object (the tool). The tool object stays
    /// in the scene — it is usually hidden so the result is visible — and
    /// the effect follows it live as it moves or is edited. A missing tool
    /// (deleted, or a library asset placed into another scene) makes the
    /// modifier a no-op.
    Boolean { op: BooleanOp, object: ObjectId },
}

impl ModifierKind {
    pub fn label(&self) -> &'static str {
        match self {
            ModifierKind::Subdivision { .. } => "Subdivision",
            ModifierKind::Boolean { .. } => "Boolean",
        }
    }
}

/// One object in the scene.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Object {
    pub id: ObjectId,
    pub name: String,
    pub transform: Transform,
    pub primitive: Primitive,
    pub smooth: bool,
    pub visible: bool,
    /// Inline material, or snapshot used when a master is missing.
    pub material: Material,
    /// When set, this object is a **material instance** of the named master
    /// (UE-style MI). Overrides layer on top; see `material_overrides`.
    #[serde(default)]
    pub material_master: Option<MaterialId>,
    /// Per-instance parameter overrides (only fields that diverge from master).
    #[serde(default)]
    pub material_overrides: MaterialOverrides,
    /// Physics simulation: dynamic bodies fall and collide when playing.
    pub dynamic: bool,
    pub density: f32,
    /// World-space linear impulse (N·s) applied once when simulation starts.
    /// Only used for dynamic bodies; ignored (and not drawn) when static.
    #[serde(default)]
    pub initial_force: Vec3,
    /// Hierarchy: this object follows its parent's transform.
    #[serde(default)]
    pub parent: Option<ObjectId>,
    /// Outliner folder this object is filed under (root objects only —
    /// children display under their parent). Purely organizational: no
    /// effect on transforms, rendering or physics.
    #[serde(default)]
    pub folder: Option<u64>,
    /// Viewport adornments.
    #[serde(default)]
    pub show_label: bool,
    #[serde(default)]
    pub show_dimensions: bool,
    /// Pivot point (local space): interactive rotations (R) spin the object
    /// around this point instead of its origin.
    #[serde(default)]
    pub pivot: Vec3,
    /// Anchor point (local space): where this object attaches to another
    /// object (Object ▸ Attach to Active, MCP attach_object, library drops).
    #[serde(default)]
    pub anchor: Vec3,
    /// Group root: this object and its descendants act as ONE unit —
    /// clicking any part in the viewport selects the whole subtree. Placed
    /// library assets are grouped by default; Ungroup clears the flag.
    #[serde(default)]
    pub group: bool,
    /// Door/window openings, for `Primitive::Wall` objects only (ignored
    /// elsewhere). Editors must bump `mesh_revision` when they change these
    /// so the render/physics caches resync.
    #[serde(default)]
    pub cutouts: Vec<WallCutout>,
    /// Footprint polygon (local XY, implicitly closed), for
    /// `Primitive::Floor` objects only (ignored elsewhere): when non-empty
    /// the slab follows this outline instead of the width × depth rectangle
    /// (Add ▸ Floor with a closed run of walls). Editors must bump
    /// `mesh_revision` when they change it.
    #[serde(default)]
    pub floor_outline: Vec<Vec2>,
    /// Result of mesh editing (Tab edit mode): when present it replaces the
    /// primitive's generated mesh. Local space, saved with the scene.
    #[serde(default)]
    pub edited_mesh: Option<MeshData>,
    /// LEGACY subdivision-surface levels: superseded by a
    /// `ModifierKind::Subdivision` entry in `modifiers`. Old scene files
    /// still carry it; `Scene::restore` migrates it into the stack and
    /// zeroes it. Nothing reads it at display time anymore.
    #[serde(default)]
    pub subdivision: u8,
    /// Modifier stack (Blender-style): non-destructive mesh effects applied
    /// in order at display time. Editors must go through `object_mut` (or
    /// bump the version another way) when changing it so previews resync.
    #[serde(default)]
    pub modifiers: Vec<Modifier>,
    /// Bumped on every mesh edit so caches (renderer, physics) resync.
    /// Not saved: a fresh session starts with fresh caches anyway.
    #[serde(skip)]
    pub mesh_revision: u64,
    /// Rope start end (pin target). Only meaningful for `Primitive::Rope`.
    #[serde(default)]
    pub rope_start: RopeEnd,
    /// Rope end end (pin target). Only meaningful for `Primitive::Rope`.
    #[serde(default)]
    pub rope_end: RopeEnd,
    /// Live world-space node positions while the rope is simulating.
    /// Not saved; the renderer uses these to draw the draped cord.
    #[serde(skip)]
    pub rope_nodes: Option<Vec<Vec3>>,
}

impl Object {
    /// The mesh to draw: the edited mesh if any, else the primitive (walls
    /// include their door/window cutouts). While a rope is simulating,
    /// `rope_nodes` (world space) overrides the straight rest pose — callers
    /// that apply the object transform should use `render_mesh_world` or
    /// draw nodes in world space.
    pub fn render_mesh(&self) -> MeshData {
        match (&self.edited_mesh, self.primitive) {
            (Some(mesh), _) => mesh.clone(),
            (None, Primitive::Wall { length, height, thickness }) => {
                mesh::wall(length, height, thickness, &self.cutouts)
            }
            (None, Primitive::Floor { thickness, .. })
                if !self.floor_outline.is_empty() =>
            {
                mesh::floor_polygon(&self.floor_outline, thickness)
            }
            // live draped rope: nodes are world-space; return a LOCAL mesh
            // centered on the first node by expressing points relative to it
            // so the usual object transform (updated to that first node)
            // places them correctly
            (None, Primitive::Rope { radius, .. }) => {
                if let Some(nodes) = self.rope_nodes.as_ref().filter(|n| n.len() >= 2) {
                    let origin = nodes[0];
                    let local: Vec<Vec3> = nodes.iter().map(|p| *p - origin).collect();
                    return mesh::rope_polyline(&local, radius);
                }
                self.primitive.generate(self.smooth)
            }
            (None, primitive) => primitive.generate(self.smooth),
        }
    }

    /// The mesh for collision building (shared-vertex topology preferred;
    /// walls keep their cutouts so rays pass through doors and windows).
    pub fn collision_mesh(&self) -> MeshData {
        match (&self.edited_mesh, self.primitive) {
            (Some(mesh), _) => mesh.clone(),
            (None, Primitive::Wall { length, height, thickness }) => {
                mesh::wall(length, height, thickness, &self.cutouts)
            }
            (None, Primitive::Floor { thickness, .. })
                if !self.floor_outline.is_empty() =>
            {
                mesh::floor_polygon(&self.floor_outline, thickness)
            }
            (None, primitive) => primitive.generate(true),
        }
    }

    /// Radius of the bounding sphere around the local origin.
    pub fn bounding_radius(&self) -> f32 {
        match &self.edited_mesh {
            Some(mesh) => mesh
                .positions
                .iter()
                .map(|p| p.length())
                .fold(0.0f32, f32::max),
            None => self.primitive.bounding_radius(),
        }
    }

    /// Total enabled subdivision levels when the modifier stack contains
    /// NOTHING but subdivision modifiers (Some(0) for an empty stack) —
    /// such objects can still be drawn instanced. None when any other
    /// enabled modifier is present (the mesh depends on other objects).
    pub fn subdivision_only_levels(&self) -> Option<u8> {
        let mut total: u8 = 0;
        for modifier in self.modifiers.iter().filter(|m| m.enabled) {
            match modifier.kind {
                ModifierKind::Subdivision { levels } => {
                    total = total.saturating_add(levels).min(6)
                }
                _ => return None,
            }
        }
        Some(total)
    }

    /// True when any enabled modifier changes the displayed mesh.
    pub fn has_enabled_modifiers(&self) -> bool {
        self.modifiers.iter().any(|m| {
            m.enabled && !matches!(m.kind, ModifierKind::Subdivision { levels: 0 })
        })
    }

    /// Distance from the local origin to the lowest point (unscaled),
    /// following the edited mesh when there is one.
    pub fn bottom_offset(&self) -> f32 {
        match &self.edited_mesh {
            Some(mesh) => {
                let min_z = mesh
                    .positions
                    .iter()
                    .map(|p| p.z)
                    .fold(f32::INFINITY, f32::min);
                if min_z.is_finite() {
                    -min_z
                } else {
                    self.primitive.bottom_offset()
                }
            }
            None => self.primitive.bottom_offset(),
        }
    }
}

/// A ruler measurement between two world-space points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub a: Vec3,
    pub b: Vec3,
}

impl Measurement {
    pub fn length(&self) -> f32 {
        (self.b - self.a).length()
    }
}

/// The axis a reference image's plane is perpendicular to (its normal):
/// X = side view (YZ plane), Y = front view (XZ plane), Z = floor (XY plane).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImagePlane {
    X,
    Y,
    Z,
}

impl ImagePlane {
    pub const ALL: [ImagePlane; 3] = [ImagePlane::X, ImagePlane::Y, ImagePlane::Z];

    pub fn label(self) -> &'static str {
        match self {
            ImagePlane::X => "X (side)",
            ImagePlane::Y => "Y (front)",
            ImagePlane::Z => "Z (floor)",
        }
    }

    /// Plane basis: (u = image right, v = image up, normal), right-handed.
    /// The Y normal points toward the front view (-Y) so "right" stays +X.
    pub fn basis(self) -> (Vec3, Vec3, Vec3) {
        match self {
            ImagePlane::X => (Vec3::Y, Vec3::Z, Vec3::X),
            ImagePlane::Y => (Vec3::X, Vec3::Z, Vec3::NEG_Y),
            ImagePlane::Z => (Vec3::X, Vec3::Y, Vec3::Z),
        }
    }
}

/// Shape of an AI marker drawn on a reference image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkerKind {
    /// A single spot (1 point).
    Point,
    /// An open polyline (≥ 2 points).
    Line,
    /// A closed polygon (≥ 3 points; the closing edge is implicit).
    Area,
}

impl MarkerKind {
    pub fn label(self) -> &'static str {
        match self {
            MarkerKind::Point => "Point",
            MarkerKind::Line => "Line",
            MarkerKind::Area => "Area",
        }
    }

    /// Fewest points that make this kind of marker well-formed.
    pub fn min_points(self) -> usize {
        match self {
            MarkerKind::Point => 1,
            MarkerKind::Line => 2,
            MarkerKind::Area => 3,
        }
    }
}

/// An AI marker: a point, line or area the user draws ON a reference image,
/// together with a free-text note. The AI assistant and MCP clients read
/// markers to anchor instructions to spots on the image ("build a 2.2 m wall
/// along this line"). Points are normalized image coordinates — u right,
/// v down, both 0..1 with the origin at the image's top-left, like pixels —
/// so markers stay glued to the image through moves, rescales, calibration
/// and plane changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageMarker {
    pub id: u64,
    pub name: String,
    pub kind: MarkerKind,
    pub points: Vec<Vec2>,
    /// User-provided context/instructions for the AI ("front door here").
    pub note: String,
}

/// A reference image shown in the viewport as a flat, optionally transparent
/// picture locked to an axis plane. The image bytes (PNG/JPEG) are embedded
/// base64 so scenes stay self-contained across save/load and platforms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceImage {
    pub id: u64,
    pub name: String,
    pub plane: ImagePlane,
    /// Center of the image, world space.
    pub location: Vec3,
    /// In-plane rotation around the plane normal, degrees.
    pub rotation_deg: f32,
    /// World width in meters; height follows from the pixel aspect ratio.
    pub width_m: f32,
    /// height / width of the source image, cached at import.
    pub aspect: f32,
    /// 0 = invisible, 1 = opaque.
    pub opacity: f32,
    pub visible: bool,
    /// Mirror the image horizontally. Back/left elevations are drawn as seen
    /// from behind/left, so they must be mirrored to read correctly from
    /// their viewing direction.
    #[serde(default)]
    pub flip_h: bool,
    /// Mirror the image vertically (e.g. scans that came in upside down,
    /// or a floor plan meant to be viewed from below).
    #[serde(default)]
    pub flip_v: bool,
    /// Original file bytes (PNG or JPEG), base64-encoded.
    pub data_base64: String,
    /// AI markers the user drew on this image (see [`ImageMarker`]).
    #[serde(default)]
    pub markers: Vec<ImageMarker>,
}

impl ReferenceImage {
    pub fn height_m(&self) -> f32 {
        self.width_m * self.aspect.max(1e-6)
    }

    /// Basis with the horizontal/vertical flips and in-plane rotation
    /// applied: (right, up, normal). The flips negate "right"/"up", so
    /// rendering, picking and calibration all see the mirrored image
    /// consistently.
    pub fn oriented_basis(&self) -> (Vec3, Vec3, Vec3) {
        let (mut u, mut v, n) = self.plane.basis();
        if self.flip_h {
            u = -u;
        }
        if self.flip_v {
            v = -v;
        }
        let (s, c) = self.rotation_deg.to_radians().sin_cos();
        (u * c + v * s, v * c - u * s, n)
    }

    /// World-space corners of the image rectangle (counter-clockwise).
    pub fn corners(&self) -> [Vec3; 4] {
        let (u, v, _) = self.oriented_basis();
        let half_w = u * (0.5 * self.width_m);
        let half_h = v * (0.5 * self.height_m());
        [
            self.location - half_w - half_h,
            self.location + half_w - half_h,
            self.location + half_w + half_h,
            self.location - half_w + half_h,
        ]
    }

    /// Map a normalized image coordinate (u right, v down, 0..1, origin
    /// top-left — the marker convention) to its world-space position on the
    /// image plane.
    pub fn uv_to_world(&self, uv: Vec2) -> Vec3 {
        let (u_axis, v_axis, _) = self.oriented_basis();
        self.location
            + u_axis * ((uv.x - 0.5) * self.width_m)
            + v_axis * ((0.5 - uv.y) * self.height_m())
    }

    /// Inverse of `uv_to_world`: project a world point onto the image plane
    /// and express it in normalized image coordinates (may exceed 0..1 for
    /// points beside the image).
    pub fn world_to_uv(&self, p: Vec3) -> Vec2 {
        let (u_axis, v_axis, _) = self.oriented_basis();
        let rel = p - self.location;
        Vec2::new(
            rel.dot(u_axis) / self.width_m.max(1e-9) + 0.5,
            0.5 - rel.dot(v_axis) / self.height_m().max(1e-9),
        )
    }

    /// Distance along a ray to the image rectangle, if it hits (viewport
    /// picking — reference images are not physics bodies).
    pub fn intersect_ray(&self, origin: Vec3, direction: Vec3) -> Option<f32> {
        let (u, v, n) = self.oriented_basis();
        let denom = direction.dot(n);
        if denom.abs() < 1e-9 {
            return None; // ray parallel to the image plane
        }
        let t = (self.location - origin).dot(n) / denom;
        if t <= 1e-6 {
            return None;
        }
        let p = origin + direction * t - self.location;
        (p.dot(u).abs() <= 0.5 * self.width_m && p.dot(v).abs() <= 0.5 * self.height_m())
            .then_some(t)
    }
}

/// An outliner folder: an organizational bucket for root objects (children
/// display under their parent regardless). Folders never affect transforms,
/// rendering or physics — deleting one keeps its objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub id: u64,
    pub name: String,
    /// The original wall, stored by "Break Wall into Bricks" so the wall
    /// can be rebuilt from the folder later. None for ordinary folders.
    #[serde(default)]
    pub source_wall: Option<Box<Object>>,
}

/// The scene document — the single source of truth that the renderer and the
/// physics mirror derive their state from.
#[derive(Debug)]
pub struct Scene {
    objects: Vec<Object>,
    /// id → position in `objects`, kept in step with every membership change
    /// so `object()` / `object_mut()` are O(1) instead of a linear scan.
    index: HashMap<ObjectId, usize>,
    measurements: Vec<Measurement>,
    reference_images: Vec<ReferenceImage>,
    folders: Vec<Folder>,
    /// Master material library (UE-style parent materials).
    masters: Vec<MasterMaterial>,
    /// Global material parameters (wetness, snow, tint, …).
    mpc: MaterialParameterCollection,
    next_id: u64,
    next_material_id: u64,
    version: u64,
    /// Process-unique id of this Scene value. Editors use it to notice the
    /// document being REPLACED (File ▸ New, control new_scene) — object ids
    /// restart there, so an id alone can silently match a different object.
    instance: u64,
}

impl Default for Scene {
    fn default() -> Self {
        static NEXT_INSTANCE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        Self {
            objects: Vec::new(),
            index: HashMap::new(),
            measurements: Vec::new(),
            reference_images: Vec::new(),
            folders: Vec::new(),
            masters: Vec::new(),
            mpc: MaterialParameterCollection::default(),
            next_id: 0,
            next_material_id: 0,
            version: 0,
            instance: NEXT_INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    /// See the `instance` field: changes when the whole document is swapped.
    pub fn instance(&self) -> u64 {
        self.instance
    }

    /// Blender-like startup scene: a default cube.
    pub fn default_scene() -> Self {
        let mut scene = Self::new();
        scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        scene
    }

    /// Monotonic counter, bumped on every mutation. Derived state (renderer,
    /// physics mirror) uses it to know when to resync.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Add an object with a Blender-style unique name (Cube, Cube.001, …).
    pub fn add_object(&mut self, primitive: Primitive, transform: Transform) -> ObjectId {
        self.next_id += 1;
        self.version += 1;
        let id = ObjectId(self.next_id);
        let name = self.unique_name(primitive.base_name());
        self.objects.push(Object {
            id,
            name,
            transform,
            primitive,
            smooth: false, // Blender adds meshes flat-shaded
            visible: true,
            material: Material::default(),
            material_master: None,
            material_overrides: MaterialOverrides::default(),
            dynamic: false,
            density: 1.0,
            initial_force: Vec3::ZERO,
            parent: None,
            folder: None,
            show_label: false,
            show_dimensions: false,
            pivot: Vec3::ZERO,
            anchor: Vec3::ZERO,
            group: false,
            cutouts: Vec::new(),
            floor_outline: Vec::new(),
            edited_mesh: None,
            subdivision: 0,
            modifiers: Vec::new(),
            mesh_revision: 0,
            rope_start: RopeEnd::default(),
            rope_end: RopeEnd::default(),
            rope_nodes: None,
        });
        // Ropes are physical by default — they only do something useful under gravity.
        if primitive.is_rope() {
            if let Some(object) = self.objects.last_mut() {
                object.dynamic = true;
                object.smooth = true;
                // brown cord
                object.material.base_color = [0.45, 0.28, 0.12];
            }
        }
        self.index.insert(id, self.objects.len() - 1);
        id
    }

    /// Insert a pre-built object (e.g. from a library asset), assigning a
    /// fresh id and a unique name derived from the object's current name.
    /// Everything else (transform, material, edited mesh, …) is kept; the
    /// caller is responsible for the parent link being valid. Boolean
    /// modifiers are stripped — their tool ids belong to the scene the
    /// object was captured in and would alias unrelated objects here.
    pub fn insert_object(&mut self, mut object: Object) -> ObjectId {
        self.next_id += 1;
        self.version += 1;
        object.id = ObjectId(self.next_id);
        object.name = self.unique_name(&object.name);
        object.mesh_revision = 0;
        migrate_subdivision(&mut object);
        object
            .modifiers
            .retain(|m| !matches!(m.kind, ModifierKind::Boolean { .. }));
        self.objects.push(object);
        self.index.insert(ObjectId(self.next_id), self.objects.len() - 1);
        ObjectId(self.next_id)
    }

    /// Recompute the id → position map after anything that shifts `objects`.
    fn rebuild_index(&mut self) {
        self.index = self
            .objects
            .iter()
            .enumerate()
            .map(|(i, o)| (o.id, i))
            .collect();
    }

    fn unique_name(&self, base: &str) -> String {
        if !self.objects.iter().any(|o| o.name == base) {
            return base.to_string();
        }
        for i in 1..1000 {
            let candidate = format!("{base}.{i:03}");
            if !self.objects.iter().any(|o| o.name == candidate) {
                return candidate;
            }
        }
        format!("{base}.{}", self.next_id)
    }

    pub fn objects(&self) -> &[Object] {
        &self.objects
    }

    pub fn object(&self, id: ObjectId) -> Option<&Object> {
        self.index.get(&id).map(|&i| &self.objects[i])
    }

    /// Mutable access; bumps the version (callers are expected to change
    /// something).
    pub fn object_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        self.version += 1;
        self.index.get(&id).map(|&i| &mut self.objects[i])
    }

    pub fn remove_object(&mut self, id: ObjectId) -> Option<Object> {
        let index = self.objects.iter().position(|o| o.id == id)?;
        self.version += 1;
        // children stay where they are in the world, just unparented
        let child_ids: Vec<ObjectId> = self
            .objects
            .iter()
            .filter(|o| o.parent == Some(id))
            .map(|o| o.id)
            .collect();
        for child in child_ids {
            let world = self.world_transform(child);
            if let Some(object) = self.objects.iter_mut().find(|o| o.id == child) {
                object.parent = None;
                object.transform = world;
            }
        }
        let removed = self.objects.remove(index);
        self.rebuild_index();
        Some(removed)
    }

    // --- materials (masters, MPC, resolve) --------------------------------

    pub fn masters(&self) -> &[MasterMaterial] {
        &self.masters
    }

    pub fn master(&self, id: MaterialId) -> Option<&MasterMaterial> {
        self.masters.iter().find(|m| m.id == id)
    }

    pub fn master_mut(&mut self, id: MaterialId) -> Option<&mut MasterMaterial> {
        self.version += 1;
        self.masters.iter_mut().find(|m| m.id == id)
    }

    pub fn mpc(&self) -> &MaterialParameterCollection {
        &self.mpc
    }

    pub fn mpc_mut(&mut self) -> &mut MaterialParameterCollection {
        self.version += 1;
        &mut self.mpc
    }

    pub fn set_mpc(&mut self, mpc: MaterialParameterCollection) {
        self.mpc = mpc;
        self.version += 1;
    }

    /// Create a master material from a template (copies `material`).
    pub fn add_master(&mut self, name: &str, material: Material) -> MaterialId {
        self.next_material_id += 1;
        self.version += 1;
        let id = MaterialId(self.next_material_id);
        let name = self.unique_master_name(name);
        self.masters.push(MasterMaterial {
            id,
            name,
            material: material.clamped(),
        });
        id
    }

    /// Promote an object's current resolved material into a new master and
    /// rebind the object as an instance (no overrides).
    pub fn create_master_from_object(&mut self, object_id: ObjectId, name: &str) -> Option<MaterialId> {
        let mat = self.object_material(object_id)?;
        let id = self.add_master(name, mat);
        if let Some(object) = self.object_mut(object_id) {
            object.material = mat;
            object.material_master = Some(id);
            object.material_overrides = MaterialOverrides::default();
        }
        Some(id)
    }

    /// Bind `object_id` as an instance of `master_id`, clearing overrides.
    pub fn assign_master(&mut self, object_id: ObjectId, master_id: MaterialId) -> bool {
        let Some(master) = self.master(master_id).map(|m| m.material) else {
            return false;
        };
        let Some(object) = self.object_mut(object_id) else {
            return false;
        };
        object.material = master;
        object.material_master = Some(master_id);
        object.material_overrides = MaterialOverrides::default();
        true
    }

    /// Make the object's material fully local (break instance link).
    pub fn make_material_unique(&mut self, object_id: ObjectId) -> bool {
        let mat = match self.object_material(object_id) {
            Some(m) => m,
            None => return false,
        };
        let Some(object) = self.object_mut(object_id) else {
            return false;
        };
        object.material = mat;
        object.material_master = None;
        object.material_overrides = MaterialOverrides::default();
        true
    }

    /// Authored material (master + overrides), no MPC / world effects.
    pub fn object_material(&self, id: ObjectId) -> Option<Material> {
        let object = self.object(id)?;
        Some(resolve_authored(
            &object.material,
            object.material_master,
            &object.material_overrides,
            &self.masters,
        ))
    }

    /// Full render-ready material for an object at its current world transform.
    pub fn object_material_for_render(&self, id: ObjectId) -> Option<Material> {
        let object = self.object(id)?;
        let world = self.world_transform(id);
        let world_up = world.rotation * Vec3::Z;
        Some(resolve_for_render(
            &object.material,
            object.material_master,
            &object.material_overrides,
            &self.masters,
            &self.mpc,
            world.location,
            world_up,
        ))
    }

    /// Write an edited material back onto the object. If the object is an
    /// instance, only the differing fields become overrides; the inline
    /// snapshot is kept in sync for orphan fallback / library capture.
    pub fn set_object_material(&mut self, id: ObjectId, edited: Material) -> bool {
        let edited = edited.clamped();
        let master_id = self.object(id).and_then(|o| o.material_master);
        let overrides = if let Some(mid) = master_id {
            if let Some(master) = self.master(mid) {
                MaterialOverrides::from_diff(&master.material, &edited)
            } else {
                MaterialOverrides::default()
            }
        } else {
            MaterialOverrides::default()
        };
        let Some(object) = self.object_mut(id) else {
            return false;
        };
        object.material = edited;
        if object.material_master.is_some() {
            object.material_overrides = overrides;
        }
        true
    }

    /// Apply a material function to an object (preserves master link via overrides).
    pub fn apply_material_function(&mut self, id: ObjectId, func: MaterialFunction) -> bool {
        let Some(base) = self.object_material(id) else {
            return false;
        };
        self.set_object_material(id, func.apply(&base))
    }

    fn unique_master_name(&self, base: &str) -> String {
        if !self.masters.iter().any(|m| m.name == base) {
            return base.to_string();
        }
        for i in 1..1000 {
            let candidate = format!("{base}.{i:03}");
            if !self.masters.iter().any(|m| m.name == candidate) {
                return candidate;
            }
        }
        format!("{base}.{}", self.next_material_id)
    }

    // --- outliner folders ---------------------------------------------------

    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    pub fn folder(&self, id: u64) -> Option<&Folder> {
        self.folders.iter().find(|f| f.id == id)
    }

    /// Create a folder with a unique name derived from `base`.
    pub fn add_folder(&mut self, base: &str) -> u64 {
        self.next_id += 1;
        self.version += 1;
        let id = self.next_id;
        let name = if !self.folders.iter().any(|f| f.name == base) {
            base.to_string()
        } else {
            let mut candidate = format!("{base}.{id}");
            for i in 1..1000 {
                let n = format!("{base}.{i:03}");
                if !self.folders.iter().any(|f| f.name == n) {
                    candidate = n;
                    break;
                }
            }
            candidate
        };
        self.folders.push(Folder { id, name, source_wall: None });
        id
    }

    /// Mutable access; bumps the version (callers are expected to change
    /// something).
    pub fn folder_mut(&mut self, id: u64) -> Option<&mut Folder> {
        self.version += 1;
        self.folders.iter_mut().find(|f| f.id == id)
    }

    pub fn rename_folder(&mut self, id: u64, name: String) {
        if let Some(folder) = self.folders.iter_mut().find(|f| f.id == id) {
            folder.name = name;
            self.version += 1;
        }
    }

    /// Delete a folder; its objects are kept and drop back to the scene root.
    pub fn remove_folder(&mut self, id: u64) {
        let Some(index) = self.folders.iter().position(|f| f.id == id) else {
            return;
        };
        self.version += 1;
        self.folders.remove(index);
        for object in &mut self.objects {
            if object.folder == Some(id) {
                object.folder = None;
            }
        }
    }

    /// File an object under a folder (None = scene root). Folder membership
    /// is a root-object property, so a parented object is unparented first,
    /// keeping its world transform.
    pub fn set_folder(&mut self, id: ObjectId, folder: Option<u64>) {
        if self.object(id).is_some_and(|o| o.parent.is_some()) {
            let world = self.world_transform(id);
            if let Some(object) = self.objects.iter_mut().find(|o| o.id == id) {
                object.parent = None;
                object.transform = world;
            }
        }
        if let Some(object) = self.objects.iter_mut().find(|o| o.id == id) {
            object.folder = folder;
            self.version += 1;
        }
    }

    /// Transform of an object in world space, composed through its parents.
    pub fn world_transform(&self, id: ObjectId) -> Transform {
        let Some(object) = self.object(id) else {
            return Transform::default();
        };
        match object.parent {
            Some(parent) if self.object(parent).is_some() => {
                Transform::compose(&self.world_transform(parent), &object.transform)
            }
            _ => object.transform,
        }
    }

    /// World transforms of ALL objects in one memoized pass — O(N) total.
    /// Per-frame consumers (renderer, physics mirror, wireframe) use this
    /// instead of calling `world_transform` once per object, which walks the
    /// parent chain per call.
    pub fn world_transforms(&self) -> HashMap<ObjectId, Transform> {
        let mut memo: HashMap<ObjectId, Transform> =
            HashMap::with_capacity(self.objects.len());
        let mut chain: Vec<ObjectId> = Vec::new();
        for object in &self.objects {
            if memo.contains_key(&object.id) {
                continue;
            }
            // walk up to a memoized ancestor or a root…
            chain.clear();
            let mut parent_world: Option<Transform> = None;
            let mut cur = Some(object.id);
            while let Some(c) = cur {
                if let Some(&t) = memo.get(&c) {
                    parent_world = Some(t);
                    break;
                }
                let Some(o) = self.object(c) else { break };
                chain.push(c);
                cur = o.parent.filter(|p| self.object(*p).is_some());
                if chain.len() > 1000 {
                    break; // corrupted-cycle guard, mirrors world_transform
                }
            }
            // …then compose back down, memoizing every link.
            let mut world = parent_world;
            for &c in chain.iter().rev() {
                let o = self.object(c).expect("chained ids exist");
                let w = match world {
                    Some(pw) => Transform::compose(&pw, &o.transform),
                    None => o.transform,
                };
                memo.insert(c, w);
                world = Some(w);
            }
        }
        memo
    }

    /// Set an object's local transform so that its WORLD transform matches.
    pub fn set_world_transform(&mut self, id: ObjectId, world: Transform) {
        let parent_world = match self.object(id).and_then(|o| o.parent) {
            Some(parent) if self.object(parent).is_some() => Some(self.world_transform(parent)),
            _ => None,
        };
        let local = match parent_world {
            Some(pw) => pw.to_local(&world),
            None => world,
        };
        if let Some(object) = self.object_mut(id) {
            object.transform = local;
        }
    }

    /// True if `ancestor` is on `id`'s parent chain (or is `id` itself).
    pub fn is_ancestor(&self, ancestor: ObjectId, id: ObjectId) -> bool {
        let mut current = Some(id);
        let mut hops = 0;
        while let Some(cur) = current {
            if cur == ancestor {
                return true;
            }
            current = self.object(cur).and_then(|o| o.parent);
            hops += 1;
            if hops > 1000 {
                return false; // corrupted cycle guard
            }
        }
        false
    }

    /// Parent `child` to `parent` (or clear with None), preserving the
    /// child's world transform. Rejects cycles and self-parenting.
    pub fn set_parent(&mut self, child: ObjectId, parent: Option<ObjectId>) -> bool {
        if let Some(p) = parent {
            if p == child || self.is_ancestor(child, p) || self.object(p).is_none() {
                return false;
            }
        }
        if self.object(child).is_none() {
            return false;
        }
        let world = self.world_transform(child);
        let parent_world = parent.map(|p| self.world_transform(p));
        if let Some(object) = self.object_mut(child) {
            object.parent = parent;
            object.transform = match parent_world {
                Some(pw) => pw.to_local(&world),
                None => world,
            };
            true
        } else {
            false
        }
    }

    /// Lowest world-space z of an object, estimated from its bottom extent
    /// along z (rotation is ignored — a placement approximation).
    pub fn lowest_point_z(&self, id: ObjectId) -> f32 {
        let world = self.world_transform(id);
        let bottom = self.object(id).map(|o| o.bottom_offset()).unwrap_or(0.0);
        world.location.z - bottom * world.scale.z.abs()
    }

    /// World-space position of an object's pivot point.
    pub fn world_pivot(&self, id: ObjectId) -> Vec3 {
        let pivot = self.object(id).map(|o| o.pivot).unwrap_or(Vec3::ZERO);
        self.world_transform(id).transform_point(pivot)
    }

    /// World-space position of an object's anchor point.
    pub fn world_anchor(&self, id: ObjectId) -> Vec3 {
        let anchor = self.object(id).map(|o| o.anchor).unwrap_or(Vec3::ZERO);
        self.world_transform(id).transform_point(anchor)
    }

    /// World-space position of a rope end. When the end is attached, uses the
    /// target object's transform × local_point; otherwise uses the rope's
    /// own local start (origin) or end `(length, 0, 0)`.
    pub fn rope_end_world(&self, rope_id: ObjectId, start: bool) -> Vec3 {
        let Some(object) = self.object(rope_id) else {
            return Vec3::ZERO;
        };
        let end = if start {
            object.rope_start
        } else {
            object.rope_end
        };
        if let Some(target) = end.object {
            if self.object(target).is_some() {
                return self
                    .world_transform(target)
                    .transform_point(end.local_point);
            }
        }
        let length = match object.primitive {
            Primitive::Rope { length, .. } => length.max(0.01),
            _ => 1.0,
        };
        let local = if start {
            Vec3::ZERO
        } else {
            Vec3::new(length, 0.0, 0.0)
        };
        self.world_transform(rope_id).transform_point(local)
    }

    /// Attach `child` to `parent`: move the child so its anchor point lands
    /// on `at` (world space; defaults to the parent's anchor point), then
    /// parent it there. Rejects cycles like `set_parent`.
    pub fn attach(&mut self, child: ObjectId, parent: ObjectId, at: Option<Vec3>) -> bool {
        if child == parent
            || self.object(child).is_none()
            || self.object(parent).is_none()
            || self.is_ancestor(child, parent)
        {
            return false;
        }
        let target = at.unwrap_or_else(|| self.world_anchor(parent));
        let mut world = self.world_transform(child);
        world.location += target - self.world_anchor(child);
        self.set_world_transform(child, world);
        self.set_parent(child, Some(parent))
    }

    /// The OUTERMOST group root above `id` (or `id` itself), if any part of
    /// its parent chain is flagged as a group.
    pub fn group_root(&self, id: ObjectId) -> Option<ObjectId> {
        let mut result = None;
        let mut current = Some(id);
        let mut hops = 0;
        while let Some(cur) = current {
            if self.object(cur).is_some_and(|o| o.group) {
                result = Some(cur);
            }
            current = self.object(cur).and_then(|o| o.parent);
            hops += 1;
            if hops > 1000 {
                break;
            }
        }
        result
    }

    /// `root` plus all its descendants (any depth).
    pub fn subtree(&self, root: ObjectId) -> Vec<ObjectId> {
        self.objects
            .iter()
            .filter(|o| self.is_ancestor(root, o.id))
            .map(|o| o.id)
            .collect()
    }

    /// Nesting depth (roots are 0) — used for hierarchy-ordered updates.
    pub fn depth(&self, id: ObjectId) -> u32 {
        let mut depth = 0;
        let mut current = self.object(id).and_then(|o| o.parent);
        while let Some(cur) = current {
            depth += 1;
            current = self.object(cur).and_then(|o| o.parent);
            if depth > 1000 {
                break;
            }
        }
        depth
    }

    // --- reference images --------------------------------------------------

    pub fn reference_images(&self) -> &[ReferenceImage] {
        &self.reference_images
    }

    /// Add a reference image; assigns a fresh id and a unique name from the
    /// one provided. Returns the id.
    pub fn add_reference_image(&mut self, mut image: ReferenceImage) -> u64 {
        self.next_id += 1;
        self.version += 1;
        image.id = self.next_id;
        let base = if image.name.trim().is_empty() { "Image" } else { image.name.trim() };
        let mut name = base.to_string();
        let mut i = 1;
        while self.reference_images.iter().any(|r| r.name == name) {
            name = format!("{base}.{i:03}");
            i += 1;
        }
        image.name = name;
        self.reference_images.push(image);
        self.next_id
    }

    /// Mutable access; bumps the version (callers are expected to change
    /// something).
    pub fn reference_image_mut(&mut self, id: u64) -> Option<&mut ReferenceImage> {
        self.version += 1;
        self.reference_images.iter_mut().find(|r| r.id == id)
    }

    pub fn remove_reference_image(&mut self, id: u64) {
        let before = self.reference_images.len();
        self.reference_images.retain(|r| r.id != id);
        if self.reference_images.len() != before {
            self.version += 1;
        }
    }

    /// Add an AI marker to a reference image; assigns a fresh id and a
    /// unique name within that image (empty name defaults to the kind).
    /// Returns the marker id, or None when the image does not exist.
    pub fn add_image_marker(&mut self, image_id: u64, mut marker: ImageMarker) -> Option<u64> {
        let image = self.reference_images.iter_mut().find(|r| r.id == image_id)?;
        self.next_id += 1;
        self.version += 1;
        marker.id = self.next_id;
        let base = if marker.name.trim().is_empty() {
            marker.kind.label()
        } else {
            marker.name.trim()
        };
        let mut name = base.to_string();
        let mut i = 1;
        while image.markers.iter().any(|m| m.name == name) {
            name = format!("{base}.{i:03}");
            i += 1;
        }
        marker.name = name;
        image.markers.push(marker);
        Some(self.next_id)
    }

    /// Remove a marker from a reference image; true when something was
    /// actually removed.
    pub fn remove_image_marker(&mut self, image_id: u64, marker_id: u64) -> bool {
        let Some(image) = self.reference_images.iter_mut().find(|r| r.id == image_id) else {
            return false;
        };
        let before = image.markers.len();
        image.markers.retain(|m| m.id != marker_id);
        let removed = image.markers.len() != before;
        if removed {
            self.version += 1;
        }
        removed
    }

    // --- measurements ----------------------------------------------------

    pub fn measurements(&self) -> &[Measurement] {
        &self.measurements
    }

    pub fn add_measurement(&mut self, a: Vec3, b: Vec3) {
        self.version += 1;
        self.measurements.push(Measurement { a, b });
    }

    pub fn remove_measurement(&mut self, index: usize) {
        if index < self.measurements.len() {
            self.version += 1;
            self.measurements.remove(index);
        }
    }

    /// Bounding sphere of the whole scene (center, radius).
    pub fn bounds(&self) -> Option<(Vec3, f32)> {
        if self.objects.is_empty() {
            return None;
        }
        let worlds: Vec<(Transform, f32)> = self
            .objects
            .iter()
            .map(|o| (self.world_transform(o.id), o.bounding_radius()))
            .collect();
        let center =
            worlds.iter().map(|(t, _)| t.location).sum::<Vec3>() / worlds.len() as f32;
        let radius = worlds
            .iter()
            .map(|(t, r)| {
                let max_scale = t.scale.abs().max_element().max(1e-6);
                (t.location - center).length() + r * max_scale
            })
            .fold(0.0f32, f32::max);
        Some((center, radius))
    }
}

/// Serializable scene state: used for save files, undo snapshots and the
/// physics reset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneData {
    pub objects: Vec<Object>,
    #[serde(default)]
    pub measurements: Vec<Measurement>,
    #[serde(default)]
    pub reference_images: Vec<ReferenceImage>,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub masters: Vec<MasterMaterial>,
    #[serde(default)]
    pub mpc: MaterialParameterCollection,
    pub next_id: u64,
    #[serde(default)]
    pub next_material_id: u64,
}

impl Scene {
    pub fn snapshot(&self) -> SceneData {
        SceneData {
            objects: self.objects.clone(),
            measurements: self.measurements.clone(),
            reference_images: self.reference_images.clone(),
            folders: self.folders.clone(),
            masters: self.masters.clone(),
            mpc: self.mpc,
            next_id: self.next_id,
            next_material_id: self.next_material_id,
        }
    }

    pub fn restore(&mut self, data: &SceneData) {
        self.objects = data.objects.clone();
        for object in &mut self.objects {
            migrate_subdivision(object);
        }
        self.measurements = data.measurements.clone();
        self.reference_images = data.reference_images.clone();
        self.folders = data.folders.clone();
        self.masters = data.masters.clone();
        self.mpc = data.mpc;
        self.next_id = data.next_id;
        self.next_material_id = data.next_material_id;
        self.version += 1;
        self.rebuild_index();
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.snapshot()).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Result<SceneData, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }
}

/// Pre-modifier scene files carry subdivision levels on the object; move
/// them into the modifier stack so there is one source of truth.
fn migrate_subdivision(object: &mut Object) {
    if object.subdivision > 0 {
        object.modifiers.push(Modifier::new(ModifierKind::Subdivision {
            levels: object.subdivision.min(4),
        }));
        object.subdivision = 0;
    }
}

/// Export all visible objects as a Wavefront OBJ string (world space,
/// triangulated, with normals), using each object's base mesh. Callers
/// that can evaluate modifier stacks should use [`export_obj_with`].
pub fn export_obj(scene: &Scene) -> String {
    export_obj_with(scene, |_, object| object.render_mesh())
}

/// [`export_obj`] with the displayed mesh supplied per object — the app
/// passes its modifier evaluation here so exports match the viewport.
pub fn export_obj_with(
    scene: &Scene,
    mesh_for: impl Fn(&Scene, &Object) -> MeshData,
) -> String {
    let mut out = String::from("# exported by 3dmodeler (box3d)\n");
    let mut vertex_offset: u32 = 1; // OBJ indices are 1-based
    for object in scene.objects() {
        if !object.visible {
            continue;
        }
        let mesh = mesh_for(scene, object);
        let t = scene.world_transform(object.id);
        out.push_str(&format!("o {}\n", object.name.replace(' ', "_")));
        for p in &mesh.positions {
            let world = t.location + t.rotation * (*p * t.scale);
            out.push_str(&format!("v {} {} {}\n", world.x, world.y, world.z));
        }
        for n in &mesh.normals {
            let world = (t.rotation * *n).normalize_or_zero();
            out.push_str(&format!("vn {} {} {}\n", world.x, world.y, world.z));
        }
        for tri in mesh.indices.chunks_exact(3) {
            let (a, b, c) = (
                tri[0] + vertex_offset,
                tri[1] + vertex_offset,
                tri[2] + vertex_offset,
            );
            out.push_str(&format!("f {a}//{a} {b}//{b} {c}//{c}\n"));
        }
        vertex_offset += mesh.positions.len() as u32;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_lookup() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        assert_eq!(scene.objects().len(), 1);
        assert_eq!(scene.object(id).unwrap().name, "Cube");
        scene.remove_object(id);
        assert!(scene.object(id).is_none());
    }

    #[test]
    fn master_instance_and_mpc_resolve() {
        let mut scene = Scene::new();
        let a = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        let b = scene.add_object(
            Primitive::Cube { size: 1.0 },
            Transform {
                location: Vec3::new(0.0, 0.0, 5.0),
                ..Default::default()
            },
        );
        scene.object_mut(a).unwrap().material.base_color = [1.0, 0.0, 0.0];
        let mid = scene.create_master_from_object(a, "RedPlastic").unwrap();
        assert!(scene.assign_master(b, mid));

        // Instance inherits master color
        assert_eq!(scene.object_material(b).unwrap().base_color, [1.0, 0.0, 0.0]);

        // Override roughness on b only
        scene.set_object_material(
            b,
            Material {
                base_color: [1.0, 0.0, 0.0],
                roughness: 0.1,
                ..Default::default()
            },
        );
        assert!((scene.object_material(b).unwrap().roughness - 0.1).abs() < 1e-5);
        assert!((scene.object_material(a).unwrap().roughness - 0.7).abs() < 1e-5);

        // Edit master → instance without that override field still follows
        scene.master_mut(mid).unwrap().material.base_color = [0.0, 1.0, 0.0];
        assert_eq!(scene.object_material(a).unwrap().base_color, [0.0, 1.0, 0.0]);
        // b overrode only roughness, so base_color follows master
        assert_eq!(scene.object_material(b).unwrap().base_color, [0.0, 1.0, 0.0]);
        assert!((scene.object_material(b).unwrap().roughness - 0.1).abs() < 1e-5);

        // MPC wetness affects render resolve
        scene.set_mpc(MaterialParameterCollection {
            wetness: 1.0,
            ..Default::default()
        });
        let dry_r = 0.1;
        let wet = scene.object_material_for_render(b).unwrap();
        assert!(wet.roughness < dry_r);

        // Material function
        assert!(scene.apply_material_function(a, MaterialFunction::Metal));
        assert!((scene.object_material(a).unwrap().metallic - 1.0).abs() < 1e-5);
    }

    #[test]
    fn scene_json_roundtrip_keeps_masters_and_mpc() {
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        let mid = scene.create_master_from_object(id, "MasterA").unwrap();
        scene.set_mpc(MaterialParameterCollection {
            wetness: 0.4,
            snow_amount: 0.2,
            snow_height: 3.0,
            global_tint: [0.9, 0.95, 1.0],
            emissive_boost: 1.5,
        });
        let json = scene.to_json();
        let data = Scene::from_json(&json).unwrap();
        let mut restored = Scene::new();
        restored.restore(&data);
        assert_eq!(restored.masters().len(), 1);
        assert_eq!(restored.masters()[0].id, mid);
        assert!((restored.mpc().wetness - 0.4).abs() < 1e-5);
        assert_eq!(restored.object(id).unwrap().material_master, Some(mid));
    }

    #[test]
    fn blender_style_naming() {
        let mut scene = Scene::new();
        let cube = Primitive::Cube { size: 2.0 };
        scene.add_object(cube, Transform::default());
        scene.add_object(cube, Transform::default());
        let third = scene.add_object(cube, Transform::default());
        assert_eq!(scene.object(third).unwrap().name, "Cube.002");
    }

    #[test]
    fn version_bumps_on_mutation() {
        let mut scene = Scene::default_scene();
        let v0 = scene.version();
        let id = scene.objects()[0].id;
        scene.object_mut(id).unwrap().smooth = true;
        assert!(scene.version() > v0);
    }

    #[test]
    fn json_roundtrip_preserves_scene() {
        let mut scene = Scene::default_scene();
        let id = scene.objects()[0].id;
        scene.object_mut(id).unwrap().transform.location = Vec3::new(1.0, 2.0, 3.0);
        scene.object_mut(id).unwrap().dynamic = true;

        let json = scene.to_json();
        let data = Scene::from_json(&json).expect("parse");
        let mut restored = Scene::new();
        restored.restore(&data);

        assert_eq!(restored.snapshot(), scene.snapshot());
        // ids keep working after restore
        let new_id = restored.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        assert!(new_id.0 > id.0, "next_id must survive the roundtrip");
    }

    #[test]
    fn legacy_subdivision_migrates_into_the_modifier_stack() {
        let mut scene = Scene::default_scene();
        let id = scene.objects()[0].id;
        scene.object_mut(id).unwrap().subdivision = 2;

        // a save/load roundtrip turns the legacy field into a modifier
        let data = Scene::from_json(&scene.to_json()).expect("parse");
        let mut restored = Scene::new();
        restored.restore(&data);
        let object = restored.object(id).unwrap();
        assert_eq!(object.subdivision, 0, "legacy field cleared");
        assert_eq!(
            object.modifiers,
            vec![Modifier::new(ModifierKind::Subdivision { levels: 2 })]
        );
        // idempotent: restoring the migrated snapshot adds nothing
        let again = restored.snapshot();
        restored.restore(&again);
        assert_eq!(restored.object(id).unwrap().modifiers.len(), 1);
    }

    #[test]
    fn modifiers_survive_json_and_gate_instancing() {
        let mut scene = Scene::new();
        let cube = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let tool = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        {
            let object = scene.object_mut(cube).unwrap();
            object.modifiers.push(Modifier::new(ModifierKind::Subdivision { levels: 2 }));
            object.modifiers.push(Modifier::new(ModifierKind::Boolean {
                op: BooleanOp::Subtract,
                object: tool,
            }));
        }
        let object = scene.object(cube).unwrap();
        assert!(object.has_enabled_modifiers());
        assert_eq!(object.subdivision_only_levels(), None, "boolean blocks instancing");

        // disabling the boolean leaves a subdivision-only stack
        let mut scene2 = Scene::new();
        let id = scene2.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let o = scene2.object_mut(id).unwrap();
        o.modifiers.push(Modifier::new(ModifierKind::Subdivision { levels: 1 }));
        o.modifiers.push(Modifier {
            enabled: false,
            kind: ModifierKind::Boolean { op: BooleanOp::Union, object: ObjectId(99) },
        });
        assert_eq!(scene2.object(id).unwrap().subdivision_only_levels(), Some(1));

        // the stack survives save/load
        let data = Scene::from_json(&scene.to_json()).expect("parse");
        let mut restored = Scene::new();
        restored.restore(&data);
        assert_eq!(restored.object(cube).unwrap().modifiers.len(), 2);
        assert_eq!(restored.snapshot(), scene.snapshot());
    }

    #[test]
    fn insert_object_strips_foreign_boolean_modifiers() {
        let mut scene = Scene::new();
        let mut object = Scene::default_scene().objects()[0].clone();
        object.subdivision = 3; // legacy field on a library asset
        object.modifiers.push(Modifier::new(ModifierKind::Boolean {
            op: BooleanOp::Union,
            object: ObjectId(42), // an id from another scene
        }));
        let id = scene.insert_object(object);
        let inserted = scene.object(id).unwrap();
        assert_eq!(inserted.subdivision, 0);
        assert_eq!(
            inserted.modifiers,
            vec![Modifier::new(ModifierKind::Subdivision { levels: 3 })],
            "subdivision migrated, boolean stripped"
        );
    }

    #[test]
    fn obj_export_with_supplies_the_displayed_mesh() {
        let scene = Scene::default_scene();
        // stand-in for modifier evaluation: export a plane instead
        let obj = export_obj_with(&scene, |_, _| mesh::plane(2.0));
        assert_eq!(obj.matches("\nf ").count(), 2, "two triangles of the plane");
    }

    #[test]
    fn obj_export_contains_all_pieces() {
        let scene = Scene::default_scene();
        let obj = export_obj(&scene);
        assert!(obj.contains("o Cube"));
        assert_eq!(obj.matches("\nv ").count(), 36); // flat cube: 12 tris expanded
        assert_eq!(obj.matches("\nf ").count(), 12); // 12 triangles
    }

    #[test]
    fn parenting_keeps_world_transform_and_follows() {
        let mut scene = Scene::new();
        let parent = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mut t = Transform::default();
        t.location = Vec3::new(3.0, 0.0, 0.0);
        let child = scene.add_object(Primitive::Cube { size: 2.0 }, t);

        assert!(scene.set_parent(child, Some(parent)));
        // world position unchanged by parenting
        let w = scene.world_transform(child);
        assert!((w.location - Vec3::new(3.0, 0.0, 0.0)).length() < 1e-5);

        // moving the parent carries the child
        scene.object_mut(parent).unwrap().transform.location = Vec3::new(0.0, 0.0, 5.0);
        let w = scene.world_transform(child);
        assert!((w.location - Vec3::new(3.0, 0.0, 5.0)).length() < 1e-5);

        // rotating the parent 90° about Z swings the child to +Y
        scene.object_mut(parent).unwrap().transform.rotation =
            Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let w = scene.world_transform(child);
        assert!((w.location - Vec3::new(0.0, 3.0, 5.0)).length() < 1e-4, "{:?}", w.location);

        // set_world_transform roundtrip under a rotated parent
        let target = Transform {
            location: Vec3::new(1.0, 2.0, 3.0),
            rotation: Quat::from_rotation_x(0.3),
            scale: Vec3::splat(2.0),
        };
        scene.set_world_transform(child, target);
        let w = scene.world_transform(child);
        assert!((w.location - target.location).length() < 1e-4);
        assert!((w.scale - target.scale).length() < 1e-4);

        // cycles rejected
        assert!(!scene.set_parent(parent, Some(child)));
        assert!(!scene.set_parent(parent, Some(parent)));

        // deleting the parent unparents the child in place
        let world_before = scene.world_transform(child);
        scene.remove_object(parent);
        let object = scene.object(child).unwrap();
        assert_eq!(object.parent, None);
        assert!((object.transform.location - world_before.location).length() < 1e-4);
    }

    #[test]
    fn world_transforms_pass_matches_per_object_recursion() {
        let mut scene = Scene::new();
        // three-deep chain plus a sibling and a loose root
        let root = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mut t = Transform::default();
        t.location = Vec3::new(3.0, 0.0, 0.0);
        t.rotation = Quat::from_rotation_z(0.7);
        t.scale = Vec3::new(2.0, 1.0, 0.5);
        let child = scene.add_object(Primitive::Cube { size: 1.0 }, t);
        let grandchild = scene.add_object(Primitive::Plane { size: 1.0 }, t);
        let sibling = scene.add_object(Primitive::Cube { size: 1.0 }, t);
        let loose = scene.add_object(Primitive::Empty { size: 1.0 }, t);
        scene.set_parent(child, Some(root));
        scene.set_parent(grandchild, Some(child));
        scene.set_parent(sibling, Some(root));
        scene.object_mut(root).unwrap().transform.rotation = Quat::from_rotation_x(0.3);

        let worlds = scene.world_transforms();
        assert_eq!(worlds.len(), scene.objects().len());
        for id in [root, child, grandchild, sibling, loose] {
            let expected = scene.world_transform(id);
            let got = worlds[&id];
            assert!((got.location - expected.location).length() < 1e-6, "{id:?}");
            assert!(
                got.rotation.dot(expected.rotation).abs() > 1.0 - 1e-6,
                "{id:?}"
            );
            assert!((got.scale - expected.scale).length() < 1e-6, "{id:?}");
        }

        // index stays correct through removal and restore
        scene.remove_object(child);
        assert!(scene.object(child).is_none());
        assert!(scene.object(grandchild).is_some());
        let data = scene.snapshot();
        let mut restored = Scene::new();
        restored.restore(&data);
        assert_eq!(restored.object(grandchild).unwrap().id, grandchild);
        assert_eq!(restored.world_transforms().len(), restored.objects().len());
    }

    #[test]
    fn transform_point_roundtrips_through_inverse() {
        let t = Transform {
            location: Vec3::new(1.0, -2.0, 3.0),
            rotation: Quat::from_euler(glam::EulerRot::XYZ, 0.4, -0.2, 1.1),
            scale: Vec3::new(2.0, 0.5, 3.0),
        };
        let p = Vec3::new(0.3, -1.7, 2.2);
        let there = t.transform_point(p);
        assert!((t.inverse_transform_point(there) - p).length() < 1e-5);
    }

    #[test]
    fn rotation_about_pivot_keeps_the_pivot_fixed() {
        let mut t = Transform::default();
        t.location = Vec3::new(2.0, 0.0, 0.0);
        t.scale = Vec3::splat(2.0);
        let local_pivot = Vec3::new(1.0, 0.0, 0.0); // world (4, 0, 0)
        let before = t.transform_point(local_pivot);

        t.set_rotation_about(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2), local_pivot);
        let after = t.transform_point(local_pivot);
        assert!((before - after).length() < 1e-5, "{after:?}");
        // the origin swung around the pivot: (2,0,0) -> (4,-2,0)
        assert!((t.location - Vec3::new(4.0, -2.0, 0.0)).length() < 1e-5, "{:?}", t.location);
    }

    #[test]
    fn attach_lands_anchor_on_anchor_and_parents() {
        let mut scene = Scene::new();
        let table = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let mut t = Transform::default();
        t.location = Vec3::new(10.0, 0.0, 0.0);
        let cup = scene.add_object(Primitive::Cylinder { vertices: 8, radius: 0.5, depth: 1.0 }, t);

        // table's attachment site is the tabletop center; the cup attaches
        // by the bottom of its base
        scene.object_mut(table).unwrap().anchor = Vec3::new(0.0, 0.0, 1.0);
        scene.object_mut(cup).unwrap().anchor = Vec3::new(0.0, 0.0, -0.5);

        assert!(scene.attach(cup, table, None));
        assert_eq!(scene.object(cup).unwrap().parent, Some(table));
        // cup bottom sits on the tabletop: cup center at z = 1.5
        let w = scene.world_transform(cup);
        assert!((w.location - Vec3::new(0.0, 0.0, 1.5)).length() < 1e-5, "{:?}", w.location);

        // explicit attachment point wins
        assert!(scene.attach(cup, table, Some(Vec3::new(0.5, 0.5, 1.0))));
        assert!((scene.world_anchor(cup) - Vec3::new(0.5, 0.5, 1.0)).length() < 1e-5);

        // cycles and self-attach rejected
        assert!(!scene.attach(table, cup, None));
        assert!(!scene.attach(table, table, None));
    }

    #[test]
    fn empty_primitive_is_three_axis_lines() {
        let empty = Primitive::Empty { size: 1.0 };
        assert_eq!(empty.base_name(), "Empty");
        let mesh = empty.generate(false);
        // three thin boxes, six faces each
        assert_eq!(mesh.indices.len(), 3 * 6 * 6);
        // spans ±size on every axis
        let max = mesh
            .positions
            .iter()
            .fold(Vec3::ZERO, |m, p| m.max(p.abs()));
        assert!((max - Vec3::ONE).length() < 1e-5, "{max:?}");
        assert_eq!(empty.dimensions(), Vec3::splat(2.0));

        // survives save/load
        let mut scene = Scene::new();
        scene.add_object(empty, Transform::default());
        let data = Scene::from_json(&scene.to_json()).unwrap();
        assert_eq!(data.objects[0].primitive, empty);
    }

    #[test]
    fn rope_primitive_has_mesh_and_anchors() {
        let mut scene = Scene::new();
        let id = scene.add_object(
            Primitive::Rope {
                length: 3.0,
                radius: 0.05,
                segments: 8,
            },
            Transform::default(),
        );
        let object = scene.object(id).unwrap();
        assert!(object.dynamic, "ropes are dynamic by default");
        assert_eq!(object.primitive.base_name(), "Rope");
        let mesh = object.render_mesh();
        assert!(mesh.positions.len() >= 8);
        assert!(mesh.indices.len() >= 6);
        // anchor both ends to a cube
        let cube = scene.add_object(Primitive::Cube { size: 1.0 }, Transform {
            location: Vec3::new(0.0, 0.0, 2.0),
            ..Default::default()
        });
        let weight = scene.add_object(Primitive::Cube { size: 0.5 }, Transform {
            location: Vec3::new(2.0, 0.0, 0.0),
            ..Default::default()
        });
        {
            let o = scene.object_mut(id).unwrap();
            o.rope_start = RopeEnd {
                object: Some(cube),
                local_point: Vec3::ZERO,
            };
            o.rope_end = RopeEnd {
                object: Some(weight),
                local_point: Vec3::ZERO,
            };
        }
        let start = scene.rope_end_world(id, true);
        let end = scene.rope_end_world(id, false);
        assert!((start - Vec3::new(0.0, 0.0, 2.0)).length() < 1e-4);
        assert!((end - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-4);
        // roundtrip
        let data = Scene::from_json(&scene.to_json()).unwrap();
        let o = data
            .objects
            .iter()
            .find(|o| o.id == id)
            .expect("rope restored");
        assert!(matches!(o.primitive, Primitive::Rope { length, .. } if (length - 3.0).abs() < 1e-5));
        assert_eq!(o.rope_start.object, Some(cube));
        assert_eq!(o.rope_end.object, Some(weight));
    }


    #[test]
    fn lights_have_gizmos_and_survive_json() {
        let mut scene = Scene::new();
        for light in Primitive::light_catalog() {
            scene.add_object(light, Transform::default());
        }
        let names: Vec<_> = scene.objects().iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["Point", "Sun", "Spot"]);

        // gizmos are real, pickable meshes with sane bounds
        for object in scene.objects() {
            assert!(object.primitive.is_light());
            let mesh = object.render_mesh();
            assert!(!mesh.indices.is_empty());
            assert_eq!(mesh.positions.len(), mesh.normals.len());
            let extent = mesh
                .positions
                .iter()
                .map(|p| p.length())
                .fold(0.0f32, f32::max);
            assert!(
                extent <= object.bounding_radius() + 1e-4,
                "{}: {extent} > {}",
                object.name,
                object.bounding_radius()
            );
        }

        // the spot cone widens with the angle
        assert!(mesh::spot_gizmo_radius(90.0) > mesh::spot_gizmo_radius(30.0));

        // parameters survive save/load
        let data = Scene::from_json(&scene.to_json()).unwrap();
        assert_eq!(data.objects.len(), 3);
        assert_eq!(data.objects[1].primitive, Primitive::light_catalog()[1]);
        match data.objects[2].primitive {
            Primitive::Light { kind, intensity, shadows, .. } => {
                assert_eq!(kind, LightKind::Spot);
                assert_eq!(intensity, 5.0);
                assert!(shadows);
            }
            other => panic!("expected a spot light, got {other:?}"),
        }
    }

    #[test]
    fn folders_organize_roots_and_survive_json() {
        let mut scene = Scene::new();
        let a = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        let b = scene.add_object(Primitive::Cube { size: 1.0 }, Transform::default());
        let folder = scene.add_folder("Bricks");
        assert_eq!(scene.folder(folder).unwrap().name, "Bricks");
        // duplicate base names get suffixed
        let other = scene.add_folder("Bricks");
        assert_ne!(scene.folder(other).unwrap().name, "Bricks");

        // filing a parented object unparents it, keeping the world transform
        scene.object_mut(a).unwrap().transform.location = Vec3::new(1.0, 2.0, 3.0);
        scene.set_parent(b, Some(a));
        let world_before = scene.world_transform(b);
        scene.set_folder(b, Some(folder));
        let object = scene.object(b).unwrap();
        assert_eq!(object.parent, None);
        assert_eq!(object.folder, Some(folder));
        assert!((scene.world_transform(b).location - world_before.location).length() < 1e-5);

        // save/load keeps folders; old files (no field) still load
        let data = Scene::from_json(&scene.to_json()).unwrap();
        assert_eq!(data.folders.len(), 2);
        assert_eq!(data.objects[1].folder, Some(folder));
        let old: SceneData = serde_json::from_str(r#"{"objects": [], "next_id": 1}"#).unwrap();
        assert!(old.folders.is_empty());

        // deleting the folder keeps the objects
        scene.remove_folder(folder);
        assert_eq!(scene.folders().len(), 1);
        assert_eq!(scene.object(b).unwrap().folder, None);
    }

    #[test]
    fn wall_cutouts_survive_json_and_reach_the_meshes() {
        let mut scene = Scene::new();
        let wall = Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 };
        let id = scene.add_object(wall, Transform::default());
        assert_eq!(scene.object(id).unwrap().name, "Wall");

        let solid_tris = scene.object(id).unwrap().render_mesh().indices.len();
        scene.object_mut(id).unwrap().cutouts.push(WallCutout::door(1.0, 4.0, 2.5));
        let object = scene.object(id).unwrap();
        assert_ne!(object.render_mesh().indices.len(), solid_tris);
        assert_eq!(
            object.render_mesh().indices.len(),
            object.collision_mesh().indices.len(),
            "render and collision meshes must both carry the cutouts"
        );

        // dimensions / bottom line: stands on z = 0
        assert_eq!(wall.dimensions(), Vec3::new(4.0, 0.2, 2.5));
        assert_eq!(wall.bottom_offset(), 0.0);
        assert_eq!(scene.lowest_point_z(id), 0.0);

        // save/load keeps the openings; old files (no field) still load
        let data = Scene::from_json(&scene.to_json()).unwrap();
        assert_eq!(data.objects[0].cutouts.len(), 1);
        let old: SceneData = serde_json::from_str(r#"{"objects": [], "next_id": 1}"#).unwrap();
        assert!(old.objects.is_empty());
    }

    #[test]
    fn measurements_survive_json() {
        let mut scene = Scene::default_scene();
        scene.add_measurement(Vec3::ZERO, Vec3::new(3.0, 4.0, 0.0));
        assert!((scene.measurements()[0].length() - 5.0).abs() < 1e-6);
        let data = Scene::from_json(&scene.to_json()).unwrap();
        assert_eq!(data.measurements.len(), 1);

        scene.remove_measurement(0);
        assert!(scene.measurements().is_empty());
    }

    #[test]
    fn reference_images_roundtrip_and_unique_names() {
        let mut scene = Scene::default_scene();
        let image = ReferenceImage {
            id: 0,
            name: "blueprint".into(),
            plane: ImagePlane::Y,
            location: Vec3::new(0.0, 0.0, 1.0),
            rotation_deg: 15.0,
            width_m: 4.0,
            aspect: 0.5,
            opacity: 0.6,
            visible: true,
            flip_h: false,
            flip_v: false,
            data_base64: "aGVsbG8=".into(),
            markers: Vec::new(),
        };
        let a = scene.add_reference_image(image.clone());
        let b = scene.add_reference_image(image);
        assert_ne!(a, b);
        assert_eq!(scene.reference_images()[1].name, "blueprint.001");
        assert!((scene.reference_images()[0].height_m() - 2.0).abs() < 1e-6);

        // survives save/load
        let data = Scene::from_json(&scene.to_json()).unwrap();
        assert_eq!(data.reference_images.len(), 2);
        // old scene files without the field still load
        let old: SceneData =
            serde_json::from_str(r#"{"objects": [], "next_id": 1}"#).unwrap();
        assert!(old.reference_images.is_empty());

        scene.remove_reference_image(a);
        assert_eq!(scene.reference_images().len(), 1);

        // in-plane rotation keeps the basis orthonormal
        let (u, v, n) = scene.reference_images()[0].oriented_basis();
        assert!(u.dot(v).abs() < 1e-6);
        assert!(u.cross(v).dot(n) > 0.99);
    }

    #[test]
    fn reference_image_ray_picking_and_corners() {
        // 4 m wide, aspect 0.5 -> 2 m tall, on the front plane (Y), at z = 1
        let image = ReferenceImage {
            id: 1,
            name: "front".into(),
            plane: ImagePlane::Y,
            location: Vec3::new(0.0, 0.0, 1.0),
            rotation_deg: 0.0,
            width_m: 4.0,
            aspect: 0.5,
            opacity: 0.5,
            visible: true,
            flip_h: false,
            flip_v: false,
            data_base64: String::new(),
            markers: Vec::new(),
        };
        // corners span x -2..2, z 0..2 at y = 0
        let corners = image.corners();
        let min_z = corners.iter().map(|c| c.z).fold(f32::INFINITY, f32::min);
        let max_x = corners.iter().map(|c| c.x).fold(f32::NEG_INFINITY, f32::max);
        assert!((min_z - 0.0).abs() < 1e-5 && (max_x - 2.0).abs() < 1e-5);

        // ray from the front hits the middle at t = 5
        let t = image
            .intersect_ray(Vec3::new(1.0, -5.0, 1.5), Vec3::new(0.0, 1.0, 0.0))
            .expect("hit");
        assert!((t - 5.0).abs() < 1e-5);
        // misses beside the rectangle; parallel rays never hit
        assert!(image
            .intersect_ray(Vec3::new(3.0, -5.0, 1.0), Vec3::new(0.0, 1.0, 0.0))
            .is_none());
        assert!(image
            .intersect_ray(Vec3::new(0.0, -5.0, 1.0), Vec3::new(1.0, 0.0, 0.0))
            .is_none());
    }

    #[test]
    fn image_markers_roundtrip_uv_world_and_json() {
        let mut scene = Scene::default_scene();
        let image_id = scene.add_reference_image(ReferenceImage {
            id: 0,
            name: "plan".into(),
            plane: ImagePlane::Z,
            location: Vec3::new(1.0, 2.0, 0.0),
            rotation_deg: 30.0,
            width_m: 4.0,
            aspect: 0.5,
            opacity: 0.5,
            visible: true,
            flip_h: true,
            flip_v: true,
            data_base64: String::new(),
            markers: Vec::new(),
        });

        // uv -> world -> uv is the identity, flips and rotation included
        let image = &scene.reference_images()[0];
        for uv in [Vec2::new(0.5, 0.5), Vec2::new(0.0, 0.0), Vec2::new(0.9, 0.2)] {
            let roundtrip = image.world_to_uv(image.uv_to_world(uv));
            assert!((roundtrip - uv).length() < 1e-5, "{uv:?} -> {roundtrip:?}");
        }
        // the image center maps to its world location
        assert!((image.uv_to_world(Vec2::new(0.5, 0.5)) - image.location).length() < 1e-6);
        // v = 1 is the bottom edge: below the center along the plane's "up"
        let (_, v_axis, _) = image.oriented_basis();
        let bottom = image.uv_to_world(Vec2::new(0.5, 1.0));
        assert!((bottom - image.location + v_axis * (0.5 * image.height_m())).length() < 1e-5);

        // add: fresh ids, unique names, kind label as the default name
        let marker = |kind: MarkerKind, points: Vec<Vec2>| ImageMarker {
            id: 0,
            name: String::new(),
            kind,
            points,
            note: "the front door".into(),
        };
        let a = scene
            .add_image_marker(image_id, marker(MarkerKind::Point, vec![Vec2::new(0.5, 0.5)]))
            .unwrap();
        let b = scene
            .add_image_marker(
                image_id,
                marker(MarkerKind::Line, vec![Vec2::new(0.1, 0.5), Vec2::new(0.9, 0.5)]),
            )
            .unwrap();
        let c = scene
            .add_image_marker(image_id, marker(MarkerKind::Point, vec![Vec2::new(0.2, 0.2)]))
            .unwrap();
        assert!(a != b && b != c);
        assert!(scene.add_image_marker(9999, marker(MarkerKind::Point, vec![])).is_none());
        let names: Vec<_> = scene.reference_images()[0]
            .markers
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(names, ["Point", "Line", "Point.001"]);

        // markers survive save/load; old files (no field) still load
        let data = Scene::from_json(&scene.to_json()).unwrap();
        assert_eq!(data.reference_images[0].markers.len(), 3);
        assert_eq!(data.reference_images[0].markers[1].kind, MarkerKind::Line);
        assert_eq!(data.reference_images[0].markers[1].note, "the front door");
        let old: ReferenceImage = serde_json::from_str(
            r#"{"id": 1, "name": "x", "plane": "Y", "location": [0,0,0],
                "rotation_deg": 0, "width_m": 2, "aspect": 1, "opacity": 0.5,
                "visible": true, "data_base64": ""}"#,
        )
        .unwrap();
        assert!(old.markers.is_empty());

        // remove: only the targeted marker goes; bad ids are a no-op
        assert!(scene.remove_image_marker(image_id, b));
        assert!(!scene.remove_image_marker(image_id, b));
        assert!(!scene.remove_image_marker(9999, a));
        assert_eq!(scene.reference_images()[0].markers.len(), 2);
    }

    #[test]
    fn scene_bounds_cover_objects() {
        let mut scene = Scene::new();
        let mut t = Transform::default();
        t.location = Vec3::new(10.0, 0.0, 0.0);
        scene.add_object(Primitive::UvSphere { segments: 8, rings: 4, radius: 2.0 }, t);
        let (center, radius) = scene.bounds().unwrap();
        assert!((center - Vec3::new(10.0, 0.0, 0.0)).length() < 1e-5);
        assert!((radius - 2.0).abs() < 1e-5);
    }
}
