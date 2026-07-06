//! Selection state with Blender's click rules. This is editor state, not
//! document state — it lives outside the scene and has its own change stamp.

use modeler_core::{ObjectId, Scene};

#[derive(Default)]
pub struct Selection {
    selected: Vec<ObjectId>,
    active: Option<ObjectId>,
    /// A selected reference image (viewport click) — mutually exclusive
    /// with the object selection.
    image: Option<u64>,
    stamp: u64,
}

impl Selection {
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    #[allow(dead_code)] // used by tests now, by the transform tools in Phase 5
    pub fn selected(&self) -> &[ObjectId] {
        &self.selected
    }

    #[allow(dead_code)] // used by the transform tools in Phase 5
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub fn active(&self) -> Option<ObjectId> {
        self.active
    }

    pub fn is_selected(&self, id: ObjectId) -> bool {
        self.selected.contains(&id)
    }

    /// Blender click rules: plain click selects only the hit object (or
    /// clears on empty space); shift+click extends / toggles.
    /// The selected reference image, if any.
    pub fn image(&self) -> Option<u64> {
        self.image
    }

    /// Select a reference image (clears the object selection).
    pub fn select_image(&mut self, id: u64) {
        self.selected.clear();
        self.active = None;
        self.image = Some(id);
        self.stamp += 1;
    }

    pub fn clear_image(&mut self) {
        if self.image.take().is_some() {
            self.stamp += 1;
        }
    }

    pub fn click(&mut self, hit: Option<ObjectId>, shift: bool) {
        self.image = None;
        self.stamp += 1;
        match (hit, shift) {
            (Some(id), false) => {
                self.selected = vec![id];
                self.active = Some(id);
            }
            (None, false) => {
                self.selected.clear();
                self.active = None;
            }
            (Some(id), true) => {
                if !self.is_selected(id) {
                    self.selected.push(id);
                    self.active = Some(id);
                } else if self.active == Some(id) {
                    // shift-click on the active object deselects it
                    self.selected.retain(|&s| s != id);
                    self.active = self.selected.last().copied();
                } else {
                    // shift-click on a selected (non-active) object activates it
                    self.active = Some(id);
                }
            }
            (None, true) => {} // shift-click on empty space keeps the selection
        }
    }

    /// Viewport click with GROUP expansion: hitting any part of a grouped
    /// assembly (placed library objects) selects the whole group, root
    /// active. Ungrouped objects fall through to the plain click rules.
    pub fn click_expanded(&mut self, scene: &Scene, hit: Option<ObjectId>, shift: bool) {
        let Some(root) = hit.and_then(|id| scene.group_root(id)) else {
            self.click(hit, shift);
            return;
        };
        let members = scene.subtree(root);
        self.image = None;
        self.stamp += 1;
        if !shift {
            self.selected = members;
            self.active = Some(root);
        } else if self.is_selected(root) {
            // shift-click on a selected group removes the whole group
            self.selected.retain(|id| !members.contains(id));
            if self.active.is_some_and(|a| members.contains(&a)) {
                self.active = self.selected.last().copied();
            }
        } else {
            self.selected.retain(|id| !members.contains(id));
            self.selected.extend(members);
            self.active = Some(root);
        }
    }

    /// Replace the whole selection (used by duplicate).
    pub fn set(&mut self, ids: Vec<ObjectId>, active: Option<ObjectId>) {
        self.selected = ids;
        self.active = active;
        self.image = None;
        self.stamp += 1;
    }

    /// Drop references to objects that no longer exist.
    pub fn retain_existing(&mut self, exists: impl Fn(ObjectId) -> bool) {
        let before = (self.selected.len(), self.active);
        self.selected.retain(|&id| exists(id));
        if let Some(active) = self.active {
            if !exists(active) {
                self.active = self.selected.last().copied();
            }
        }
        if before != (self.selected.len(), self.active) {
            self.stamp += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blender_click_rules() {
        let a = ObjectId(1);
        let b = ObjectId(2);
        let mut sel = Selection::default();

        sel.click(Some(a), false);
        assert_eq!(sel.selected(), &[a]);
        assert_eq!(sel.active(), Some(a));

        // shift-click extends and activates
        sel.click(Some(b), true);
        assert_eq!(sel.selected(), &[a, b]);
        assert_eq!(sel.active(), Some(b));

        // shift-click selected non-active object activates it
        sel.click(Some(a), true);
        assert_eq!(sel.active(), Some(a));
        assert_eq!(sel.selected().len(), 2);

        // shift-click active deselects it
        sel.click(Some(a), true);
        assert!(!sel.is_selected(a));
        assert_eq!(sel.active(), Some(b));

        // plain click on empty clears
        sel.click(None, false);
        assert!(sel.is_empty());
        assert_eq!(sel.active(), None);
    }

    #[test]
    fn image_and_object_selection_are_mutually_exclusive() {
        let a = ObjectId(1);
        let mut sel = Selection::default();

        sel.click(Some(a), false);
        sel.select_image(7);
        assert_eq!(sel.image(), Some(7));
        assert!(sel.is_empty(), "selecting an image clears objects");
        assert_eq!(sel.active(), None);

        sel.click(Some(a), false);
        assert_eq!(sel.image(), None, "selecting an object clears the image");
        sel.select_image(7);
        sel.click(None, false);
        assert_eq!(sel.image(), None, "clicking empty space clears the image");

        sel.select_image(7);
        sel.clear_image();
        assert_eq!(sel.image(), None);
    }

    #[test]
    fn clicking_a_group_part_selects_the_whole_group() {
        use modeler_core::{Primitive, Transform};
        let mut scene = Scene::new();
        let root = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let part = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        let loose = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        scene.set_parent(part, Some(root));
        scene.object_mut(root).unwrap().group = true;

        let mut sel = Selection::default();
        // hitting the PART selects root + part, root active
        sel.click_expanded(&scene, Some(part), false);
        assert!(sel.is_selected(root) && sel.is_selected(part));
        assert_eq!(sel.active(), Some(root));

        // shift-click on a loose object extends; shift-click the group again
        // removes the whole group
        sel.click_expanded(&scene, Some(loose), true);
        assert_eq!(sel.selected().len(), 3);
        sel.click_expanded(&scene, Some(part), true);
        assert_eq!(sel.selected(), &[loose]);

        // ungrouped: plain per-object click rules apply again
        scene.object_mut(root).unwrap().group = false;
        sel.click_expanded(&scene, Some(part), false);
        assert_eq!(sel.selected(), &[part]);
        assert_eq!(sel.active(), Some(part));
    }
}
