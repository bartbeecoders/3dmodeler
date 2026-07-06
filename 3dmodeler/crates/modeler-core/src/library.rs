//! Object library: named, reusable collections of scene objects.
//!
//! A library asset stores full `Object`s (including hierarchy and edited
//! meshes) normalized around a placement pivot: root objects carry world
//! transforms re-based so the group's footprint center sits at the origin
//! with its lowest point at z = 0 — dropping an asset at a picked point puts
//! it ON that point. Children keep their local transforms.
//!
//! This module owns the document model and the capture/instantiate logic;
//! persistence (config dir / localStorage) and preview rendering live in
//! `modeler-app`.

use crate::{glam::Vec3, Object, ObjectId, Scene};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One reusable asset: a named group of objects plus a small preview image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryAsset {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The objects, pivot-normalized. Ids/parents are only meaningful within
    /// this list; they are remapped to fresh scene ids on placement.
    pub objects: Vec<Object>,
    /// Small preview image (PNG, base64) rendered from the objects.
    #[serde(default)]
    pub preview_png_base64: Option<String>,
    /// Pivot point (asset space): placed on the drop point when the asset
    /// lands on empty ground, and the reference for rotating the placed
    /// group. (0,0,0) = the normalized footprint-center / lowest point.
    #[serde(default)]
    pub pivot: Vec3,
    /// Anchor point (asset space): when the asset is dropped ONTO another
    /// object it attaches there — the anchor lands on the hit point.
    #[serde(default)]
    pub anchor: Vec3,
}

/// The whole library document (serialized as JSON).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Library {
    assets: Vec<LibraryAsset>,
    next_id: u64,
    /// Bumped on every mutation so callers know when to persist / re-cache.
    #[serde(skip)]
    revision: u64,
}

impl Library {
    pub fn assets(&self) -> &[LibraryAsset] {
        &self.assets
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn asset(&self, id: u64) -> Option<&LibraryAsset> {
        self.assets.iter().find(|a| a.id == id)
    }

    /// Mutable access; bumps the revision (callers are expected to change
    /// something).
    pub fn asset_mut(&mut self, id: u64) -> Option<&mut LibraryAsset> {
        self.revision += 1;
        self.assets.iter_mut().find(|a| a.id == id)
    }

    /// Add an asset with a unique name (Table, Table.001, …). Returns the id.
    pub fn add_asset(
        &mut self,
        name: &str,
        description: &str,
        objects: Vec<Object>,
        preview_png_base64: Option<String>,
    ) -> u64 {
        self.next_id += 1;
        self.revision += 1;
        let id = self.next_id;
        self.assets.push(LibraryAsset {
            id,
            name: self.unique_name(name),
            description: description.to_string(),
            objects,
            preview_png_base64,
            pivot: Vec3::ZERO,
            anchor: Vec3::ZERO,
        });
        id
    }

    pub fn remove_asset(&mut self, id: u64) -> bool {
        let before = self.assets.len();
        self.assets.retain(|a| a.id != id);
        let removed = self.assets.len() != before;
        if removed {
            self.revision += 1;
        }
        removed
    }

    /// Rename keeping names unique (no-op suffixing if taken by another).
    pub fn rename_asset(&mut self, id: u64, name: &str) {
        let name = name.trim();
        if name.is_empty() || self.asset(id).is_none() {
            return;
        }
        let unique = if self.assets.iter().any(|a| a.id != id && a.name == name) {
            self.unique_name(name)
        } else {
            name.to_string()
        };
        if let Some(asset) = self.asset_mut(id) {
            asset.name = unique;
        }
    }

    fn unique_name(&self, base: &str) -> String {
        let base = base.trim();
        let base = if base.is_empty() { "Asset" } else { base };
        if !self.assets.iter().any(|a| a.name == base) {
            return base.to_string();
        }
        for i in 1..1000 {
            let candidate = format!("{base}.{i:03}");
            if !self.assets.iter().any(|a| a.name == candidate) {
                return candidate;
            }
        }
        format!("{base}.{}", self.next_id)
    }
}


/// Capture the given objects (plus all their descendants) as a
/// pivot-normalized object list ready to store in a [`LibraryAsset`].
///
/// Roots (objects whose parent is not captured) get their WORLD transform
/// re-based so the group's center-of-footprint is at x=y=0 and its lowest
/// point at z=0; children keep their local transforms untouched.
pub fn capture_objects(scene: &Scene, ids: &[ObjectId]) -> Vec<Object> {
    let included: Vec<&Object> = scene
        .objects()
        .iter()
        .filter(|o| ids.iter().any(|&sel| scene.is_ancestor(sel, o.id)))
        .collect();
    if included.is_empty() {
        return Vec::new();
    }
    let in_set = |id: ObjectId| included.iter().any(|o| o.id == id);
    let roots: Vec<&&Object> = included
        .iter()
        .filter(|o| o.parent.map_or(true, |p| !in_set(p)))
        .collect();

    let center = roots
        .iter()
        .map(|o| scene.world_transform(o.id).location)
        .sum::<Vec3>()
        / roots.len().max(1) as f32;
    let bottom = included
        .iter()
        .map(|o| scene.lowest_point_z(o.id))
        .fold(f32::INFINITY, f32::min);
    let pivot = Vec3::new(center.x, center.y, if bottom.is_finite() { bottom } else { 0.0 });

    included
        .iter()
        .map(|source| {
            let mut object = (*source).clone();
            let is_root = object.parent.map_or(true, |p| !in_set(p));
            if is_root {
                object.parent = None;
                object.transform = scene.world_transform(object.id);
                object.transform.location -= pivot;
            }
            object.mesh_revision = 0;
            object
        })
        .collect()
}

/// Place an asset's objects into the scene at `at` (world space). Objects get
/// fresh ids and unique names; the internal hierarchy is preserved. The
/// placed instance becomes ONE group: a multi-root asset is unified under
/// its first root, and that root gets the `group` flag so viewport clicks
/// select the whole assembly (Ungroup breaks it apart). Returns the new ids
/// (same order as the asset's object list).
pub fn instantiate(scene: &mut Scene, asset: &LibraryAsset, at: Vec3) -> Vec<ObjectId> {
    let stored: Vec<ObjectId> = asset.objects.iter().map(|o| o.id).collect();
    let mut id_map: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut new_ids = Vec::with_capacity(asset.objects.len());

    for source in &asset.objects {
        let mut object = source.clone();
        let is_root = object.parent.map_or(true, |p| !stored.contains(&p));
        if is_root {
            object.transform.location += at;
        }
        object.parent = None; // linked in the second pass, after all ids exist
        let new_id = scene.insert_object(object);
        id_map.insert(source.id, new_id);
        new_ids.push(new_id);
    }

    // second pass: re-link the hierarchy. Direct assignment (not set_parent)
    // because the stored child transforms are already parent-local.
    for source in &asset.objects {
        let Some(old_parent) = source.parent else { continue };
        let Some(&new_parent) = id_map.get(&old_parent) else { continue };
        if let Some(object) = scene.object_mut(id_map[&source.id]) {
            object.parent = Some(new_parent);
        }
    }

    // one group per placement: extra roots slide under the first root
    // (world-preserving), which becomes the group root
    let roots: Vec<ObjectId> = new_ids
        .iter()
        .copied()
        .filter(|&id| scene.object(id).is_some_and(|o| o.parent.is_none()))
        .collect();
    if let Some((&group_root, rest)) = roots.split_first() {
        for &other in rest {
            scene.set_parent(other, Some(group_root));
        }
        if let Some(object) = scene.object_mut(group_root) {
            object.group = true;
        }
    }
    new_ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Primitive, Transform};

    fn cube_at(scene: &mut Scene, location: Vec3) -> ObjectId {
        let mut t = Transform::default();
        t.location = location;
        scene.add_object(Primitive::Cube { size: 2.0 }, t)
    }

    #[test]
    fn capture_normalizes_pivot_to_footprint() {
        let mut scene = Scene::new();
        // two cubes side by side, resting on z=1 (cube bottom offset is 1)
        let a = cube_at(&mut scene, Vec3::new(2.0, 0.0, 2.0));
        let b = cube_at(&mut scene, Vec3::new(4.0, 0.0, 2.0));

        let objects = capture_objects(&scene, &[a, b]);
        assert_eq!(objects.len(), 2);
        // centered in x, lowest point (z = 2 - 1 = 1) moved to z = 0
        assert!((objects[0].transform.location - Vec3::new(-1.0, 0.0, 1.0)).length() < 1e-5);
        assert!((objects[1].transform.location - Vec3::new(1.0, 0.0, 1.0)).length() < 1e-5);
    }

    #[test]
    fn capture_includes_descendants_and_keeps_local_transforms() {
        let mut scene = Scene::new();
        let parent = cube_at(&mut scene, Vec3::new(5.0, 0.0, 1.0));
        let child = cube_at(&mut scene, Vec3::new(5.0, 3.0, 1.0));
        scene.set_parent(child, Some(parent));
        let child_local = scene.object(child).unwrap().transform;

        // selecting only the parent captures the child too
        let objects = capture_objects(&scene, &[parent]);
        assert_eq!(objects.len(), 2);
        let captured_child = objects.iter().find(|o| o.id == child).unwrap();
        assert_eq!(captured_child.parent, Some(parent));
        assert_eq!(captured_child.transform, child_local);
    }

    #[test]
    fn instantiate_roundtrip_preserves_layout() {
        let mut scene = Scene::new();
        let parent = cube_at(&mut scene, Vec3::new(2.0, 0.0, 1.0));
        let child = cube_at(&mut scene, Vec3::new(2.0, 3.0, 1.0));
        scene.set_parent(child, Some(parent));

        let mut library = Library::default();
        let id = library.add_asset(
            "Pair",
            "two cubes",
            capture_objects(&scene, &[parent]),
            None,
        );

        let mut target = Scene::default_scene(); // has the default cube already
        let at = Vec3::new(10.0, 20.0, 0.0);
        let new_ids = instantiate(&mut target, library.asset(id).unwrap(), at);
        assert_eq!(new_ids.len(), 2);

        // the placed instance is ONE group, rooted at the (single) root
        let new_parent = new_ids[0];
        let new_child = new_ids[1];
        assert!(target.object(new_parent).unwrap().group, "placed root must be a group");
        assert!(!target.object(new_child).unwrap().group);
        assert_eq!(target.group_root(new_child), Some(new_parent));

        // parent sits on the drop point (cube bottom at z=0)
        let pw = target.world_transform(new_parent);
        assert!((pw.location - Vec3::new(10.0, 20.0, 1.0)).length() < 1e-5);
        // child kept its +3 y offset relative to the parent
        let cw = target.world_transform(new_child);
        assert!((cw.location - Vec3::new(10.0, 23.0, 1.0)).length() < 1e-4);
        assert_eq!(target.object(new_child).unwrap().parent, Some(new_parent));
        // fresh unique names next to the existing "Cube"
        assert_eq!(target.object(new_parent).unwrap().name, "Cube.001");
    }

    #[test]
    fn multi_root_assets_unify_into_one_group() {
        let mut scene = Scene::new();
        // two UNPARENTED cubes captured together
        let a = cube_at(&mut scene, Vec3::new(0.0, 0.0, 1.0));
        let b = cube_at(&mut scene, Vec3::new(3.0, 0.0, 1.0));
        let mut library = Library::default();
        let id = library.add_asset("Pair", "", capture_objects(&scene, &[a, b]), None);

        let mut target = Scene::new();
        let new_ids = instantiate(&mut target, library.asset(id).unwrap(), Vec3::ZERO);
        // one root carries the group flag; the other became its child,
        // keeping its world placement
        let roots: Vec<_> = new_ids
            .iter()
            .filter(|&&i| target.object(i).unwrap().parent.is_none())
            .collect();
        assert_eq!(roots.len(), 1);
        let root = *roots[0];
        assert!(target.object(root).unwrap().group);
        let other = *new_ids.iter().find(|&&i| i != root).unwrap();
        assert_eq!(target.group_root(other), Some(root));
        let spread = (target.world_transform(new_ids[0]).location
            - target.world_transform(new_ids[1]).location)
            .length();
        assert!((spread - 3.0).abs() < 1e-4, "world layout preserved, got {spread}");
    }

    #[test]
    fn library_names_stay_unique_and_assets_delete() {
        let mut library = Library::default();
        let scene = Scene::default_scene();
        let objects = capture_objects(&scene, &[scene.objects()[0].id]);
        let a = library.add_asset("Table", "", objects.clone(), None);
        let b = library.add_asset("Table", "", objects, None);
        assert_eq!(library.asset(b).unwrap().name, "Table.001");

        library.rename_asset(b, "Chair");
        assert_eq!(library.asset(b).unwrap().name, "Chair");
        library.rename_asset(a, "Chair"); // taken -> suffixed
        assert_eq!(library.asset(a).unwrap().name, "Chair.001");

        assert!(library.remove_asset(a));
        assert!(!library.remove_asset(a));
        assert_eq!(library.assets().len(), 1);
    }

    #[test]
    fn library_json_roundtrip() {
        let mut library = Library::default();
        let scene = Scene::default_scene();
        library.add_asset(
            "Cube kit",
            "a cube",
            capture_objects(&scene, &[scene.objects()[0].id]),
            Some("cHJldmlldw==".into()),
        );
        let json = serde_json::to_string(&library).unwrap();
        let restored: Library = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.assets(), library.assets());
        // next_id survives so new assets don't collide
        let mut restored = restored;
        let id = restored.add_asset("Next", "", Vec::new(), None);
        assert_eq!(id, 2);
    }
}
