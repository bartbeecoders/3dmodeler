//! Snapshot-based undo/redo.
//!
//! Rather than a command pattern, the whole (small) scene document is
//! snapshotted. A version watcher batches bursts of changes (a modal drag, a
//! slider scrub) into a single undo step: a checkpoint is committed once the
//! scene has been quiet for a few frames and no editing tool is active.

use modeler_core::{Scene, SceneData};

const QUIET_FRAMES: u32 = 15;
const MAX_UNDO: usize = 64;

pub struct UndoStack {
    /// Last committed state — what undo returns to.
    stable: SceneData,
    past: Vec<SceneData>,
    future: Vec<SceneData>,
    last_version: u64,
    dirty: bool,
    quiet: u32,
}

impl UndoStack {
    pub fn new(scene: &Scene) -> Self {
        Self {
            stable: scene.snapshot(),
            past: Vec::new(),
            future: Vec::new(),
            last_version: scene.version(),
            dirty: false,
            quiet: 0,
        }
    }

    /// Call once per frame. `editing_active` suppresses checkpoints while a
    /// modal drag or the physics simulation owns the scene.
    pub fn on_frame(&mut self, scene: &Scene, editing_active: bool) {
        if scene.version() != self.last_version {
            self.last_version = scene.version();
            self.dirty = true;
            self.quiet = 0;
            return;
        }
        if self.dirty && !editing_active {
            self.quiet += 1;
            if self.quiet >= QUIET_FRAMES {
                self.commit(scene);
            }
        }
    }

    fn commit(&mut self, scene: &Scene) {
        self.dirty = false;
        self.quiet = 0;
        let current = scene.snapshot();
        if current == self.stable {
            return; // e.g. a cancelled modal: version moved, content didn't
        }
        self.past.push(std::mem::replace(&mut self.stable, current));
        self.future.clear();
        if self.past.len() > MAX_UNDO {
            self.past.remove(0);
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty() || self.dirty
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    pub fn undo(&mut self, scene: &mut Scene) {
        if self.dirty {
            self.commit(scene); // capture in-flight edits so they're redoable
        }
        let Some(previous) = self.past.pop() else { return };
        self.future.push(self.stable.clone());
        scene.restore(&previous);
        self.stable = previous;
        self.last_version = scene.version();
        self.dirty = false;
    }

    pub fn redo(&mut self, scene: &mut Scene) {
        let Some(next) = self.future.pop() else { return };
        self.past.push(self.stable.clone());
        scene.restore(&next);
        self.stable = next;
        self.last_version = scene.version();
        self.dirty = false;
    }

    /// After load/new: the current scene becomes the baseline.
    pub fn reset(&mut self, scene: &Scene) {
        self.stable = scene.snapshot();
        self.past.clear();
        self.future.clear();
        self.last_version = scene.version();
        self.dirty = false;
        self.quiet = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::{glam::Vec3, Primitive, Transform};

    fn settle(undo: &mut UndoStack, scene: &Scene) {
        for _ in 0..QUIET_FRAMES + 1 {
            undo.on_frame(scene, false);
        }
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut scene = Scene::default_scene();
        let mut undo = UndoStack::new(&scene);

        let id = scene.add_object(Primitive::Plane { size: 2.0 }, Transform::default());
        settle(&mut undo, &scene);
        assert_eq!(scene.objects().len(), 2);

        scene.object_mut(id).unwrap().transform.location = Vec3::new(5.0, 0.0, 0.0);
        settle(&mut undo, &scene);

        undo.undo(&mut scene);
        assert_eq!(scene.object(id).unwrap().transform.location, Vec3::ZERO);

        undo.undo(&mut scene);
        assert_eq!(scene.objects().len(), 1, "add undone");

        undo.redo(&mut scene);
        assert_eq!(scene.objects().len(), 2);
        undo.redo(&mut scene);
        assert_eq!(
            scene.object(id).unwrap().transform.location,
            Vec3::new(5.0, 0.0, 0.0)
        );
        assert!(!undo.can_redo());
    }

    #[test]
    fn burst_of_changes_is_one_step() {
        let mut scene = Scene::default_scene();
        let id = scene.objects()[0].id;
        let mut undo = UndoStack::new(&scene);

        // simulate a drag: many mutations, no quiet frames between them
        for i in 1..=20 {
            scene.object_mut(id).unwrap().transform.location.x = i as f32;
            undo.on_frame(&scene, true);
        }
        settle(&mut undo, &scene);

        undo.undo(&mut scene);
        assert_eq!(
            scene.object(id).unwrap().transform.location.x,
            0.0,
            "whole drag must undo as one step"
        );
        assert!(!undo.can_undo());
    }

    #[test]
    fn cancelled_change_creates_no_step() {
        let mut scene = Scene::default_scene();
        let id = scene.objects()[0].id;
        let mut undo = UndoStack::new(&scene);

        scene.object_mut(id).unwrap().transform.location.x = 3.0;
        undo.on_frame(&scene, true);
        scene.object_mut(id).unwrap().transform.location.x = 0.0; // cancel
        settle(&mut undo, &scene);

        assert!(!undo.can_undo());
    }
}
