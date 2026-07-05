//! Selection state with Blender's click rules. This is editor state, not
//! document state — it lives outside the scene and has its own change stamp.

use modeler_core::ObjectId;

#[derive(Default)]
pub struct Selection {
    selected: Vec<ObjectId>,
    active: Option<ObjectId>,
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
    pub fn click(&mut self, hit: Option<ObjectId>, shift: bool) {
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

    /// Replace the whole selection (used by duplicate).
    pub fn set(&mut self, ids: Vec<ObjectId>, active: Option<ObjectId>) {
        self.selected = ids;
        self.active = active;
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
}
