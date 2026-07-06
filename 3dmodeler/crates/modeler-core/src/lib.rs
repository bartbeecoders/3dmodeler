//! Core document model for the 3D modeler.
//!
//! This crate holds the scene graph, mesh primitives, and (in later phases)
//! the command/undo system and serialization. It knows nothing about
//! rendering or physics — those live in `modeler-app`.

pub mod library;
pub mod mesh;

use glam::{Quat, Vec3};
pub use glam;
pub use library::{Library, LibraryAsset};
pub use mesh::MeshData;
use serde::{Deserialize, Serialize};

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
}

impl Primitive {
    /// All primitives with Blender-default parameters, in Add-menu order.
    pub fn catalog() -> [Primitive; 7] {
        [
            Primitive::Plane { size: 2.0 },
            Primitive::Cube { size: 2.0 },
            Primitive::UvSphere { segments: 32, rings: 16, radius: 1.0 },
            Primitive::IcoSphere { subdivisions: 2, radius: 1.0 },
            Primitive::Cylinder { vertices: 32, radius: 1.0, depth: 2.0 },
            Primitive::Cone { vertices: 32, radius_bottom: 1.0, radius_top: 0.0, depth: 2.0 },
            Primitive::Torus { major_segments: 48, minor_segments: 12, major_radius: 1.0, minor_radius: 0.25 },
        ]
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
        }
    }

    /// Generate the triangle mesh, flat- or smooth-shaded.
    pub fn generate(&self, smooth: bool) -> MeshData {
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
        };
        if smooth {
            m
        } else {
            m.into_flat()
        }
    }
}

/// Simple PBR-ish material.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Material {
    pub base_color: [f32; 3],
    pub roughness: f32,
    pub metallic: f32,
}

impl Default for Material {
    fn default() -> Self {
        // Blender's default material gray
        Self {
            base_color: [0.8, 0.8, 0.8],
            roughness: 0.7,
            metallic: 0.0,
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
    pub material: Material,
    /// Physics simulation: dynamic bodies fall and collide when playing.
    pub dynamic: bool,
    pub density: f32,
    /// Hierarchy: this object follows its parent's transform.
    #[serde(default)]
    pub parent: Option<ObjectId>,
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
    /// Result of mesh editing (Tab edit mode): when present it replaces the
    /// primitive's generated mesh. Local space, saved with the scene.
    #[serde(default)]
    pub edited_mesh: Option<MeshData>,
    /// Bumped on every mesh edit so caches (renderer, physics) resync.
    /// Not saved: a fresh session starts with fresh caches anyway.
    #[serde(skip)]
    pub mesh_revision: u64,
}

impl Object {
    /// The mesh to draw: the edited mesh if any, else the primitive.
    pub fn render_mesh(&self) -> MeshData {
        match &self.edited_mesh {
            Some(mesh) => mesh.clone(),
            None => self.primitive.generate(self.smooth),
        }
    }

    /// The mesh for collision building (shared-vertex topology preferred).
    pub fn collision_mesh(&self) -> MeshData {
        match &self.edited_mesh {
            Some(mesh) => mesh.clone(),
            None => self.primitive.generate(true),
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
    /// Original file bytes (PNG or JPEG), base64-encoded.
    pub data_base64: String,
}

impl ReferenceImage {
    pub fn height_m(&self) -> f32 {
        self.width_m * self.aspect.max(1e-6)
    }

    /// Basis with the in-plane rotation applied: (right, up, normal).
    pub fn oriented_basis(&self) -> (Vec3, Vec3, Vec3) {
        let (u, v, n) = self.plane.basis();
        let (s, c) = self.rotation_deg.to_radians().sin_cos();
        (u * c + v * s, v * c - u * s, n)
    }
}

/// The scene document — the single source of truth that the renderer and the
/// physics mirror derive their state from.
#[derive(Debug)]
pub struct Scene {
    objects: Vec<Object>,
    measurements: Vec<Measurement>,
    reference_images: Vec<ReferenceImage>,
    next_id: u64,
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
            measurements: Vec::new(),
            reference_images: Vec::new(),
            next_id: 0,
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
            dynamic: false,
            density: 1.0,
            parent: None,
            show_label: false,
            show_dimensions: false,
            pivot: Vec3::ZERO,
            anchor: Vec3::ZERO,
            edited_mesh: None,
            mesh_revision: 0,
        });
        id
    }

    /// Insert a pre-built object (e.g. from a library asset), assigning a
    /// fresh id and a unique name derived from the object's current name.
    /// Everything else (transform, material, edited mesh, …) is kept; the
    /// caller is responsible for the parent link being valid.
    pub fn insert_object(&mut self, mut object: Object) -> ObjectId {
        self.next_id += 1;
        self.version += 1;
        object.id = ObjectId(self.next_id);
        object.name = self.unique_name(&object.name);
        object.mesh_revision = 0;
        self.objects.push(object);
        ObjectId(self.next_id)
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
        self.objects.iter().find(|o| o.id == id)
    }

    /// Mutable access; bumps the version (callers are expected to change
    /// something).
    pub fn object_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        self.version += 1;
        self.objects.iter_mut().find(|o| o.id == id)
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
        Some(self.objects.remove(index))
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
    pub next_id: u64,
}

impl Scene {
    pub fn snapshot(&self) -> SceneData {
        SceneData {
            objects: self.objects.clone(),
            measurements: self.measurements.clone(),
            reference_images: self.reference_images.clone(),
            next_id: self.next_id,
        }
    }

    pub fn restore(&mut self, data: &SceneData) {
        self.objects = data.objects.clone();
        self.measurements = data.measurements.clone();
        self.reference_images = data.reference_images.clone();
        self.next_id = data.next_id;
        self.version += 1;
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.snapshot()).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Result<SceneData, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }
}

/// Export all visible objects as a Wavefront OBJ string (world space,
/// triangulated, with normals).
pub fn export_obj(scene: &Scene) -> String {
    let mut out = String::from("# exported by 3dmodeler (box3d)\n");
    let mut vertex_offset: u32 = 1; // OBJ indices are 1-based
    for object in scene.objects() {
        if !object.visible {
            continue;
        }
        let mesh = object.render_mesh();
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
            data_base64: "aGVsbG8=".into(),
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
