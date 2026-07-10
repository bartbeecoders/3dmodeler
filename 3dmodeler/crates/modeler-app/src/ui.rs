//! Blender-style editor UI: top menu bar, outliner, properties sidebar
//! (N panel), bottom status bar and the keymap overlay.
//!
//! Menus are hand-rolled egui Areas rather than `menu_button` — the built-in
//! popups misbehave inside the deprecated panel API (see plan.md, Phase 3).

use crate::camera::BlenderCamera;
use crate::context_menu::ContextMenu;
use crate::edit_mode::{EditMode, SelectMode};
use crate::library::LibraryPanel;
use crate::modal::{self, ModalTransform};
use crate::object_ops;
use crate::physics::{PhysicsMirror, SimState};
use crate::undo::UndoStack;
use crate::io;
use crate::overlay::MeasureTool;
use crate::ref_image::{self, CalibrateTool};
use crate::ref_setup::RefSetupDialog;
use crate::scene_render::{LightingMode, ShadeMode};
use crate::selection::Selection;
use crate::settings::{Settings, SettingsWindow};
use crate::theme::{self, Theme};
use modeler_core::ImagePlane;
use modeler_core::glam::{EulerRot, Quat, Vec3};
use modeler_core::{Library, ObjectId, Primitive, Scene, Transform};
use three_d::egui;
use three_d::Event;

/// Pending outliner mutations, collected while the rows draw and applied
/// afterwards (the rows only hold shared borrows of the scene).
#[derive(Default)]
struct OutlinerActions {
    visibility_toggle: Option<ObjectId>,
    clicked: Option<ObjectId>,
    start_rename: Option<(ObjectId, String)>,
    commit_rename: Option<(ObjectId, String)>,
    reparent: Option<(ObjectId, Option<ObjectId>)>,
    /// File an object under a folder; None sends it to the scene root
    /// (clearing its parent too).
    move_to_folder: Option<(ObjectId, Option<u64>)>,
    folder_eye: Option<u64>,
    delete_folder: Option<u64>,
    commit_folder_rename: Option<(u64, String)>,
    /// Rebuild the wall stored in this bricks folder.
    rebuild_wall: Option<u64>,
    /// Every object row drawn this frame, top to bottom (collapsed folder
    /// members excluded) — the order Shift+Click range selection walks.
    row_order: Vec<ObjectId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    File,
    Edit,
    Add,
    Object,
    View,
    Help,
}

pub struct UiState {
    open_menu: Option<Menu>,
    menu_origin: egui::Pos2,
    rename: Option<(ObjectId, String)>,
    rename_folder: Option<(u64, String)>,
    /// Focus the rename field on its FIRST frame only — re-requesting focus
    /// every frame would cancel the focus loss that commits the rename.
    rename_needs_focus: bool,
    collapsed_folders: std::collections::HashSet<u64>,
    /// Objects whose children are hidden in the outliner (collapse triangle
    /// on rows that have parented children).
    collapsed_objects: std::collections::HashSet<ObjectId>,
    /// Bricks folders already auto-collapsed once: they start closed so
    /// hundreds of brick rows don't flood the outliner, but re-expanding
    /// one by hand sticks (it is never auto-collapsed again).
    seen_brick_folders: std::collections::HashSet<u64>,
    pub show_sidebar: bool,
    show_keymap: bool,
    show_about: bool,
    /// Decoded-once GPU texture of the embedded About banner.
    about_texture: Option<egui::TextureHandle>,
    import_open: bool,
    import_buffer: String,
    /// Break-into-Bricks dialog: Some(target count) while open. Set by the
    /// Object menu, the properties panel and the context wheel; the window
    /// itself lives in `brick_dialog_window`.
    brick_dialog: Option<i32>,
    pub status_message: Option<String>,
    current_file: Option<io::FileHandle>,
    settings_window: SettingsWindow,
    ref_setup: RefSetupDialog,
    pub library_panel: LibraryPanel,
    pub context_menu: ContextMenu,
    pub chat_panel: crate::ai::ChatPanel,
    applied_theme: Option<Theme>,
    #[cfg(target_arch = "wasm32")]
    save_as_open: bool,
    #[cfg(target_arch = "wasm32")]
    save_as_buffer: String,
}

/// State of the MCP control API, shown in the status bar (native only).
/// None = web build (no control server). Some(None inside) = port taken.
#[derive(Clone, Copy)]
pub struct McpStatus {
    pub port: u16,
    pub commands_handled: u64,
    pub seconds_since_last: Option<f32>,
}

/// Layout info the viewport overlays need to avoid the UI chrome.
pub struct UiLayout {
    pub top_offset: f32,     // menu bar height (logical)
    pub right_offset: f32,   // sidebar width (logical)
    pub bottom_offset: f32,  // status bar height (logical)
    pub left_offset: f32,    // AI chat panel width (logical)
}

impl UiState {
    pub fn new() -> Self {
        Self {
            open_menu: None,
            menu_origin: egui::Pos2::ZERO,
            rename: None,
            rename_folder: None,
            rename_needs_focus: false,
            collapsed_folders: std::collections::HashSet::new(),
            collapsed_objects: std::collections::HashSet::new(),
            seen_brick_folders: std::collections::HashSet::new(),
            show_sidebar: true,
            show_keymap: false,
            show_about: false,
            about_texture: None,
            import_open: false,
            import_buffer: String::new(),
            brick_dialog: None,
            status_message: None,
            current_file: None,
            settings_window: SettingsWindow::new(),
            ref_setup: RefSetupDialog::new(),
            library_panel: LibraryPanel::new(),
            context_menu: ContextMenu::new(),
            chat_panel: crate::ai::ChatPanel::new(),
            applied_theme: None,
            #[cfg(target_arch = "wasm32")]
            save_as_open: false,
            #[cfg(target_arch = "wasm32")]
            save_as_buffer: String::new(),
        }
    }

    /// Non-egui inputs: N toggles the sidebar, clicks into the viewport close
    /// any open menu.
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        egui_owns_keyboard: bool,
        _pointer_over_ui: bool,
    ) {
        for event in events.iter_mut() {
            match event {
                Event::Text(text) if text == "n" && !egui_owns_keyboard => {
                    self.show_sidebar = !self.show_sidebar;
                    text.clear();
                }
                _ => {}
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        selection: &mut Selection,
        camera: &mut BlenderCamera,
        modal: &mut ModalTransform,
        physics: &mut PhysicsMirror,
        undo: &mut UndoStack,
        measure: &mut MeasureTool,
        calibrate: &mut CalibrateTool,
        settings: &mut Settings,
        library: &mut Library,
        edit_point: Option<(ObjectId, Vec3)>,
        edit: Option<&mut EditMode>,
        wall_tool: &mut crate::wall_tool::WallTool,
        snap_to_grid: &mut bool,
        snap_to_vertex: &mut bool,
        shade_mode: &mut ShadeMode,
        lighting_mode: &mut LightingMode,
        xray: &mut bool,
        modal_status: &Option<String>,
        fps: f32,
        mcp: Option<Option<McpStatus>>,
        chat: &mut crate::ai::ChatSession,
    ) -> UiLayout {
        // restyle egui when the theme changed (and on the first frame)
        if self.applied_theme != Some(settings.theme) {
            settings.theme.apply(ctx);
            self.applied_theme = Some(settings.theme);
        }
        // finished reference-image picks arrive here
        if let Some((name, bytes)) = ref_image::poll_image() {
            match ref_image::make_reference(name, &bytes) {
                Ok(image) => {
                    scene.add_reference_image(image);
                    self.status_message = Some("reference image added".into());
                }
                Err(e) => self.status_message = Some(format!("image load failed: {e}")),
            }
        }
        if let Some(result) = io::poll_open() {
            match result {
                Ok((handle, data)) => {
                    scene.restore(&data);
                    selection.set(Vec::new(), None);
                    undo.reset(scene);
                    io::add_recent(&handle, scene);
                    self.status_message = Some(format!("loaded {}", io::display_name(&handle)));
                    self.current_file = Some(handle);
                }
                Err(e) => self.status_message = Some(format!("open failed: {e}")),
            }
        }
        if let Some(result) = io::poll_save() {
            match result {
                Ok(handle) => {
                    io::add_recent(&handle, scene);
                    self.status_message = Some(format!("saved {}", io::display_name(&handle)));
                    self.current_file = Some(handle);
                }
                Err(e) => self.status_message = Some(format!("save failed: {e}")),
            }
        }
        let menu_offset = self.menu_bar(
            ctx, scene, selection, camera, modal, physics, undo, measure, settings,
            wall_tool, snap_to_grid, shade_mode, lighting_mode, xray,
        );
        let top_offset = menu_offset
            + self.toolbar(
                ctx, scene, selection, modal, physics, undo, settings, snap_to_grid,
                snap_to_vertex, shade_mode, lighting_mode, xray, edit,
            );
        let bottom_offset = self.status_bar(
            ctx, scene, physics, measure, calibrate, snap_to_grid, settings, modal_status, fps,
            mcp,
        );
        let right_offset = if self.show_sidebar {
            self.sidebar(ctx, scene, selection, settings, calibrate, library, edit_point)
        } else {
            0.0
        };
        let left_offset = self.chat_panel.ui(ctx, chat, settings);
        self.keymap_window(ctx);
        self.about_window(ctx);
        self.import_window(ctx, scene, undo);
        self.save_as_window(ctx, scene, settings);
        self.brick_dialog_window(ctx, scene, selection);
        self.settings_window.ui(ctx, settings);
        calibrate_window(ctx, scene, calibrate, settings);
        if let Some(message) = self.ref_setup.window(ctx, scene, settings) {
            self.status_message = Some(message);
        }
        if let Some(message) = self.library_panel.dialog_window(ctx, scene, selection, library) {
            self.status_message = Some(message);
        }
        if let Some(message) = self.context_menu.ui(
            ctx,
            scene,
            selection,
            modal,
            &mut self.library_panel,
            &mut self.brick_dialog,
        ) {
            self.status_message = Some(message);
        }
        self.library_panel
            .detect_viewport_drop(ctx, top_offset, right_offset, bottom_offset, left_offset);
        UiLayout {
            top_offset,
            right_offset,
            bottom_offset,
            left_offset,
        }
    }

    // --- menu bar --------------------------------------------------------

    fn menu_bar(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        selection: &mut Selection,
        camera: &mut BlenderCamera,
        modal: &mut ModalTransform,
        physics: &mut PhysicsMirror,
        undo: &mut UndoStack,
        measure: &mut MeasureTool,
        settings: &mut Settings,
        wall_tool: &mut crate::wall_tool::WallTool,
        snap_to_grid: &mut bool,
        shade_mode: &mut ShadeMode,
        lighting_mode: &mut LightingMode,
        xray: &mut bool,
    ) -> f32 {
        let mut bar_height = 24.0;
        let mut opened_this_frame = false;
        #[allow(deprecated)]
        let response = egui::Panel::top("menu_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                for (menu, label) in [
                    (Menu::File, "File"),
                    (Menu::Edit, "Edit"),
                    (Menu::Add, "Add"),
                    (Menu::Object, "Object"),
                    (Menu::View, "View"),
                    (Menu::Help, "Help"),
                ] {
                    let button = ui.selectable_label(self.open_menu == Some(menu), label);
                    if button.clicked() {
                        self.open_menu = if self.open_menu == Some(menu) {
                            None
                        } else {
                            opened_this_frame = true;
                            Some(menu)
                        };
                        self.menu_origin = button.rect.left_bottom();
                    } else if button.hovered() && self.open_menu.is_some() {
                        // Blender-style: hovering neighbors switches menus
                        if self.open_menu != Some(menu) {
                            opened_this_frame = true;
                        }
                        self.open_menu = Some(menu);
                        self.menu_origin = button.rect.left_bottom();
                    }
                }
            });
        });
        bar_height = response.response.rect.height().max(bar_height);

        if let Some(menu) = self.open_menu {
            let mut close = false;
            let area = egui::Area::new(egui::Id::new("menu-dropdown"))
                .fixed_pos(self.menu_origin + egui::vec2(0.0, 2.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::menu(ui.style()).show(ui, |ui| {
                        ui.set_min_width(180.0);
                        // full-width entries: the hover highlight and click
                        // area span the whole menu, not just the label text
                        let justified =
                            egui::Layout::top_down_justified(egui::Align::Min);
                        ui.with_layout(justified, |ui| {
                            close = match menu {
                                Menu::File => {
                                    self.file_menu(ui, scene, selection, undo, settings)
                                }
                                Menu::Edit => {
                                    edit_menu(ui, scene, undo, &mut self.settings_window)
                                }
                                Menu::Add => add_menu_items(
                                    ui, scene, selection, measure, wall_tool, settings,
                                    &mut self.ref_setup, &mut self.status_message,
                                ),
                                Menu::Object => object_menu(
                                    ui, scene, selection, modal, physics,
                                    &mut self.library_panel, &mut self.status_message,
                                    &mut self.brick_dialog,
                                ),
                                Menu::View => view_menu(
                                    ui, camera, scene, selection, settings, snap_to_grid,
                                    shade_mode, lighting_mode, xray,
                                    &mut self.chat_panel.open,
                                ),
                                Menu::Help => {
                                    let mut close = false;
                                    if ui.button("Keymap").clicked() {
                                        self.show_keymap = !self.show_keymap;
                                        close = true;
                                    }
                                    if ui.button("About…").clicked() {
                                        self.show_about = true;
                                        close = true;
                                    }
                                    close
                                }
                            };
                        });
                    });
                });
            // close when clicking anywhere outside the dropdown (egui's own
            // hit-testing — event-space based closing proved unreliable),
            // except on the very frame a menu button opened it
            if !opened_this_frame && area.response.clicked_elsewhere() {
                close = true;
            }
            if close {
                self.open_menu = None;
            }
        }
        bar_height
    }

    /// Top toolbar: the main actions as icon buttons with tooltips.
    #[allow(clippy::too_many_arguments)]
    fn toolbar(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        selection: &mut Selection,
        modal: &mut ModalTransform,
        physics: &mut PhysicsMirror,
        undo: &mut UndoStack,
        settings: &Settings,
        snap_to_grid: &mut bool,
        snap_to_vertex: &mut bool,
        shade_mode: &mut ShadeMode,
        lighting_mode: &mut LightingMode,
        xray: &mut bool,
        edit: Option<&mut EditMode>,
    ) -> f32 {
        #[allow(deprecated)]
        let response = egui::Panel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let icon = |ui: &mut egui::Ui, glyph: &str, tip: &str| {
                    ui.add(egui::Button::new(egui::RichText::new(glyph).size(16.0)))
                        .on_hover_text(tip)
                        .clicked()
                };
                let toggle = |ui: &mut egui::Ui, on: &mut bool, glyph: &str, tip: &str| {
                    if ui
                        .add(egui::SelectableLabel::new(
                            *on,
                            egui::RichText::new(glyph).size(16.0),
                        ))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        *on = !*on;
                    }
                };

                // "AI" as text: egui's built-in fonts have no robot emoji
                toggle(ui, &mut self.chat_panel.open, "AI", "AI Assistant chat");
                ui.separator();

                if icon(ui, "🗋", "New scene (Ctrl+N)") {
                    self.action_new_scene(scene, selection, undo);
                }
                if icon(ui, "📂", "Open… (Ctrl+O)") {
                    self.action_open(settings);
                }
                if icon(ui, "💾", "Save (Ctrl+S)") {
                    self.action_save(scene, settings);
                }
                ui.separator();

                if ui
                    .add_enabled(
                        undo.can_undo(),
                        egui::Button::new(egui::RichText::new("↩").size(16.0)),
                    )
                    .on_hover_text("Undo (Ctrl+Z)")
                    .clicked()
                {
                    undo.undo(scene);
                }
                if ui
                    .add_enabled(
                        undo.can_redo(),
                        egui::Button::new(egui::RichText::new("↪").size(16.0)),
                    )
                    .on_hover_text("Redo (Ctrl+Shift+Z)")
                    .clicked()
                {
                    undo.redo(scene);
                }
                ui.separator();

                // transform operators on the current selection
                let has_selection = !selection.is_empty();
                let op = |ui: &mut egui::Ui, label: &str, tip: &str| {
                    ui.add_enabled(
                        has_selection,
                        egui::Button::new(egui::RichText::new(label).size(14.0)),
                    )
                    .on_hover_text(tip)
                    .clicked()
                };
                if op(ui, "↔ G", "Move selection (G)") {
                    modal.begin_grab(scene, selection);
                }
                if op(ui, "⟳ R", "Rotate selection (R)") {
                    modal.begin_rotate(scene, selection);
                }
                if op(ui, "↗ S", "Scale selection (S)") {
                    modal.begin_scale(scene, selection);
                }
                ui.separator();

                // edit mode: vertex / edge / face element select (1/2/3)
                if let Some(edit) = edit {
                    for (mode, tip) in [
                        (SelectMode::Vertex, "Vertex select (1)"),
                        (SelectMode::Edge, "Edge select (2)"),
                        (SelectMode::Face, "Face select (3)"),
                    ] {
                        if select_mode_button(ui, edit.mode == mode, mode)
                            .on_hover_text(tip)
                            .clicked()
                        {
                            edit.set_mode(mode);
                        }
                    }
                    ui.separator();
                }

                // snapping toggles
                toggle(
                    ui,
                    snap_to_grid,
                    "#",
                    "Snap to grid: moves land on grid positions (Ctrl inverts while dragging)",
                );
                toggle(
                    ui,
                    snap_to_vertex,
                    "⌖",
                    "Snap to vertex: while moving, the selection snaps so its closest                      vertex lands on the vertex nearest the cursor",
                );
                ui.separator();

                // viewport shading modes + X-ray
                for (mode, label, tip) in [
                    (ShadeMode::Wireframe, "Wire", "Wireframe: only object edges"),
                    (ShadeMode::Solid, "Solid", "Solid: neutral studio shading, materials ignored"),
                    (ShadeMode::Shaded, "Shaded", "Shaded: full materials and lights"),
                ] {
                    if ui
                        .selectable_label(*shade_mode == mode, label)
                        .on_hover_text(tip)
                        .clicked()
                    {
                        *shade_mode = mode;
                    }
                }
                toggle(
                    ui,
                    xray,
                    "X-ray",
                    "X-ray: see through objects (solid and shaded modes)",
                );
                // lighting mode (shaded only): studio rig or scene lights
                if *shade_mode == ShadeMode::Shaded {
                    for (mode, label, tip) in [
                        (
                            LightingMode::Studio,
                            "Studio",
                            "Studio lighting: built-in key + fill rig, scene lights ignored",
                        ),
                        (
                            LightingMode::Scene,
                            "Lights",
                            "Scene lights: the scene's light objects illuminate the \
                             viewport, with shadows (Add ▸ Light)",
                        ),
                    ] {
                        if ui
                            .selectable_label(*lighting_mode == mode, label)
                            .on_hover_text(tip)
                            .clicked()
                        {
                            *lighting_mode = mode;
                        }
                    }
                }
                ui.separator();

                // simulation controls
                match physics.sim_state() {
                    SimState::Stopped => {
                        if icon(ui, "▶", "Play physics (Space)") {
                            physics.play(scene);
                        }
                    }
                    SimState::Playing => {
                        if icon(ui, "⏸", "Pause (Space)") {
                            physics.pause();
                        }
                        if icon(ui, "⏹", "Stop & reset (Esc)") {
                            physics.stop(scene);
                        }
                    }
                    SimState::Paused => {
                        if icon(ui, "▶", "Resume (Space)") {
                            physics.play(scene);
                        }
                        if icon(ui, "⏹", "Stop & reset (Esc)") {
                            physics.stop(scene);
                        }
                    }
                }
            });
        });
        response.response.rect.height()
    }

    fn file_menu(
        &mut self,
        ui: &mut egui::Ui,
        scene: &mut Scene,
        selection: &mut Selection,
        undo: &mut UndoStack,
        settings: &Settings,
    ) -> bool {
        let mut close = false;
        if ui.button("New scene  (Ctrl+N)").clicked() {
            self.action_new_scene(scene, selection, undo);
            close = true;
        }
        ui.separator();
        if ui.button("Open…  (Ctrl+O)").clicked() {
            self.action_open(settings);
            close = true;
        }
        if ui.button("Save  (Ctrl+S)").clicked() {
            self.action_save(scene, settings);
            close = true;
        }
        if ui.button("Save As…").clicked() {
            let default_name = self
                .current_file
                .as_ref()
                .map(io::display_name)
                .unwrap_or_else(|| io::DEFAULT_NAME.to_string());
            #[cfg(not(target_arch = "wasm32"))]
            io::request_save(scene.to_json(), default_name, settings.save_dir());
            #[cfg(target_arch = "wasm32")]
            {
                self.save_as_buffer = default_name;
                self.save_as_open = true;
            }
            close = true;
        }

        let recents = io::recent_entries();
        if !recents.is_empty() {
            ui.separator();
            ui.label(egui::RichText::new("Recent").weak());
            for entry in recents {
                if ui.button(&entry.label).clicked() {
                    match io::load(&entry.handle) {
                        Ok(data) => {
                            scene.restore(&data);
                            selection.set(Vec::new(), None);
                            undo.reset(scene);
                            self.status_message = Some(format!("loaded {}", entry.label));
                            self.current_file = Some(entry.handle);
                        }
                        Err(e) => self.status_message = Some(format!("load failed: {e}")),
                    }
                    close = true;
                }
            }
        }

        ui.separator();
        if ui.button("Export .obj").clicked() {
            let obj = modeler_core::export_obj(scene);
            self.status_message =
                Some(io::export_file("export.obj", &obj).unwrap_or_else(|e| e));
            close = true;
        }
        if ui.button("Import .json (paste)…").clicked() {
            self.import_open = true;
            self.import_buffer.clear();
            close = true;
        }
        close
    }

    fn do_save(&mut self, scene: &Scene, handle: io::FileHandle) {
        match io::save(scene, &handle) {
            Ok(()) => {
                io::add_recent(&handle, scene);
                self.status_message = Some(format!("saved {}", io::display_name(&handle)));
                self.current_file = Some(handle);
            }
            Err(e) => self.status_message = Some(format!("save failed: {e}")),
        }
    }

    /// File > New scene, also reachable via Ctrl+N.
    pub fn action_new_scene(&mut self, scene: &mut Scene, selection: &mut Selection, undo: &mut UndoStack) {
        *scene = Scene::default_scene();
        selection.set(Vec::new(), None);
        self.rename = None;
        undo.reset(scene);
        self.current_file = None;
    }

    /// File > Save, also reachable via Ctrl+S: writes to the current file
    /// directly, or opens a Save dialog (async — the result arrives via
    /// `io::poll_save` on a later frame) if there isn't one yet.
    pub fn action_save(&mut self, scene: &Scene, settings: &Settings) {
        if let Some(handle) = self.current_file.clone() {
            self.do_save(scene, handle);
        } else {
            io::request_save(
                scene.to_json(),
                io::DEFAULT_NAME.to_string(),
                settings.save_dir(),
            );
        }
    }

    /// File > Open…, also reachable via Ctrl+O.
    pub fn action_open(&mut self, settings: &Settings) {
        io::request_open(settings.save_dir());
    }

    #[cfg(target_arch = "wasm32")]
    fn save_as_window(&mut self, ctx: &egui::Context, scene: &Scene, _settings: &Settings) {
        if !self.save_as_open {
            return;
        }
        let mut open = true;
        let mut do_save = false;
        egui::Window::new("Save As")
            .open(&mut open)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("File name:");
                let response = ui.text_edit_singleline(&mut self.save_as_buffer);
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    do_save = true;
                }
                if ui.button("Save").clicked() {
                    do_save = true;
                }
            });
        if do_save {
            let mut name = self.save_as_buffer.trim().to_string();
            if name.is_empty() {
                name = io::DEFAULT_NAME.to_string();
            }
            if !name.ends_with(&format!(".{}", io::EXTENSION)) {
                name.push('.');
                name.push_str(io::EXTENSION);
            }
            io::request_save(scene.to_json(), name, None);
            self.save_as_open = false;
        } else if !open {
            self.save_as_open = false;
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn save_as_window(&mut self, _ctx: &egui::Context, _scene: &Scene, _settings: &Settings) {}

    /// Break-into-Bricks dialog: slider for the target brick count (100 to
    /// 5000 — 100 floor, then steps of 200 — default 1000), then break the
    /// active object on confirm.
    fn brick_dialog_window(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        selection: &mut Selection,
    ) {
        let Some(mut value) = self.brick_dialog else { return };
        let mut open = true;
        let mut do_break = false;
        let mut cancel = false;
        egui::Window::new("Break into Bricks")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let name = selection
                    .active()
                    .and_then(|id| scene.object(id))
                    .map(|o| o.name.clone())
                    .unwrap_or_default();
                ui.label(format!("Break '{name}' into approximately:"));
                ui.add(
                    egui::Slider::new(
                        &mut value,
                        object_ops::MIN_BRICKS as i32..=object_ops::MAX_BRICKS as i32,
                    )
                    .text("bricks"),
                );
                // snap: 100 at the floor, multiples of 200 above it
                value = if value < 150 { 100 } else { ( value + 100 ) / 200 * 200 };
                ui.small("Openings, curvature and course rounding vary the exact count.");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Break").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Enter))
                    {
                        do_break = true;
                    }
                    if ui.button("Cancel").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Escape))
                    {
                        cancel = true;
                    }
                });
            });

        if do_break {
            if let Some(id) = selection.active() {
                if let Some(bricks) =
                    object_ops::break_into_bricks(scene, id, value.max(1) as usize)
                {
                    self.status_message = Some(format!(
                        "broken into {} bricks — Space simulates",
                        bricks.len()
                    ));
                    let active = bricks.first().copied();
                    selection.set(bricks, active);
                }
            }
            self.brick_dialog = None;
        } else if cancel || !open {
            self.brick_dialog = None;
        } else {
            self.brick_dialog = Some(value);
        }
    }

    fn import_window(&mut self, ctx: &egui::Context, scene: &mut Scene, undo: &mut UndoStack) {
        if !self.import_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Import scene JSON")
            .open(&mut open)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Paste a scene .json below:");
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.import_buffer)
                            .desired_rows(10)
                            .desired_width(360.0)
                            .code_editor(),
                    );
                });
                if ui.button("Load").clicked() {
                    match Scene::from_json(&self.import_buffer) {
                        Ok(data) => {
                            scene.restore(&data);
                            undo.reset(scene);
                            self.status_message = Some("scene imported".into());
                            self.import_open = false;
                        }
                        Err(e) => {
                            self.status_message = Some(format!("import failed: {e}"));
                        }
                    }
                }
            });
        if !open {
            self.import_open = false;
        }
    }

    // --- status bar ------------------------------------------------------

    fn status_bar(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        physics: &mut PhysicsMirror,
        measure: &MeasureTool,
        calibrate: &CalibrateTool,
        snap_to_grid: &mut bool,
        settings: &Settings,
        modal_status: &Option<String>,
        fps: f32,
        mcp: Option<Option<McpStatus>>,
    ) -> f32 {
        #[allow(deprecated)]
        let response = egui::Panel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // physics playback controls (Space toggles, Esc stops)
                match physics.sim_state() {
                    SimState::Stopped => {
                        if ui.button("▶").on_hover_text("Play physics (Space)").clicked() {
                            physics.play(scene);
                        }
                    }
                    SimState::Playing => {
                        if ui.button("⏸").on_hover_text("Pause (Space)").clicked() {
                            physics.pause();
                        }
                        if ui.button("⏹").on_hover_text("Stop & reset (Esc)").clicked() {
                            physics.stop(scene);
                        }
                    }
                    SimState::Paused => {
                        if ui.button("▶").on_hover_text("Resume (Space)").clicked() {
                            physics.play(scene);
                        }
                        if ui.button("⏹").on_hover_text("Stop & reset (Esc)").clicked() {
                            physics.stop(scene);
                        }
                    }
                }
                ui.checkbox(&mut physics.ground_plane, "ground")
                    .on_hover_text("Static ground plane at z=0 during simulation");
                ui.checkbox(snap_to_grid, "snap")
                    .on_hover_text("Snap moves to absolute grid positions (Ctrl inverts)");
                ui.label(
                    egui::RichText::new(format!(
                        "grid {}",
                        settings.unit.format(settings.grid_spacing)
                    ))
                    .size(12.0)
                    .weak(),
                );
                ui.separator();

                let hint = if calibrate.picking() {
                    format!(
                        "Scale image: click point {} of 2 on the image · Esc cancel",
                        calibrate.points.len() + 1
                    )
                } else if measure.active {
                    if measure.first.is_some() {
                        "Measure: click the second point · Esc cancel".to_string()
                    } else {
                        "Measure: click the first point · Esc cancel".to_string()
                    }
                } else {
                    match physics.sim_state() {
                    SimState::Playing => {
                        "SIMULATING · hold LMB to charge a poke, release to kick · \
                         Space pause · Esc stop"
                            .to_string()
                    }
                    SimState::Paused => "PAUSED · Space resume · Esc stop".to_string(),
                    SimState::Stopped => match modal_status {
                        // every modal status carries its own confirm/cancel hint
                        Some(status) => status.clone(),
                        None => "LMB Select · MMB Orbit · Shift+A Add · G/R/S Transform · N Sidebar"
                            .to_string(),
                    },
                }
                };
                ui.label(egui::RichText::new(hint).size(12.0));
                if let Some(message) = &self.status_message {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(message)
                            .size(12.0)
                            .color(theme::accent(ui)),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // app version = modeler-app crate version; the patch digit
                    // is bumped on every build+commit
                    ui.label(
                        egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                            .size(12.0)
                            .weak(),
                    );
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!(
                            "{} objects · {fps:.0} fps",
                            scene.objects().len()
                        ))
                        .size(12.0)
                        .weak(),
                    );

                    // MCP control API indicator (native builds)
                    if let Some(mcp) = mcp {
                        let palette = settings.theme.palette();
                        ui.separator();
                        match mcp {
                            None => {
                                ui.label(
                                    egui::RichText::new("● MCP off")
                                        .size(12.0)
                                        .color(palette.err),
                                )
                                .on_hover_text(
                                    "Control port already in use — agents cannot connect                                      to this instance",
                                );
                            }
                            Some(status) => {
                                let active =
                                    status.seconds_since_last.is_some_and(|s| s < 3.0);
                                let (color, text) = if active {
                                    (palette.ok, "● MCP active".to_string())
                                } else {
                                    (
                                        palette.ok.gamma_multiply(0.55),
                                        format!("● MCP :{}", status.port),
                                    )
                                };
                                let hover = if status.commands_handled == 0 {
                                    format!(
                                        "MCP control API listening on 127.0.0.1:{} —                                          no agent commands yet. Register with:
                                         claude mcp add modeler -- <repo>/3dmodeler/target/release/modeler-mcp",
                                        status.port
                                    )
                                } else {
                                    format!(
                                        "MCP control API on 127.0.0.1:{} — {} agent command{} handled",
                                        status.port,
                                        status.commands_handled,
                                        if status.commands_handled == 1 { "" } else { "s" }
                                    )
                                };
                                ui.label(egui::RichText::new(text).size(12.0).color(color))
                                    .on_hover_text(hover);
                                if active {
                                    ui.ctx().request_repaint(); // keep the glow fresh
                                }
                            }
                        }
                    }
                });
            });
        });
        response.response.rect.height()
    }

    // --- sidebar: outliner + properties -----------------------------------

    fn sidebar(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        selection: &mut Selection,
        settings: &Settings,
        calibrate: &mut CalibrateTool,
        library: &mut Library,
        edit_point: Option<(ObjectId, Vec3)>,
    ) -> f32 {
        #[allow(deprecated)]
        let response = egui::Panel::right("sidebar")
            .default_size(250.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    theme::section_header(ui, "Outliner");
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui
                                .small_button("+ 📂")
                                .on_hover_text(
                                    "New folder — drag outliner objects onto it",
                                )
                                .clicked()
                            {
                                let id = scene.add_folder("Folder");
                                let name = scene
                                    .folder(id)
                                    .map(|f| f.name.clone())
                                    .unwrap_or_default();
                                self.rename_folder = Some((id, name));
                                self.rename_needs_focus = true;
                            }
                        },
                    );
                });
                egui::ScrollArea::vertical()
                    .id_salt("outliner-scroll")
                    .max_height(320.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        self.outliner(ui, scene, selection);
                    });
                ui.separator();
                if let Some(message) = self.library_panel.section(ui, library) {
                    self.status_message = Some(message);
                }
                ui.separator();
                if !scene.reference_images().is_empty() {
                    theme::section_header(ui, "Reference Images");
                    reference_image_rows(ui, scene, selection, settings, calibrate);
                    ui.separator();
                }
                if !scene.measurements().is_empty() {
                    theme::section_header(ui, "Measurements");
                    let mut remove: Option<usize> = None;
                    for (i, m) in scene.measurements().iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.small_button("✖").on_hover_text("Delete measurement").clicked() {
                                remove = Some(i);
                            }
                            ui.label(settings.unit.format(m.length()));
                        });
                    }
                    if let Some(i) = remove {
                        scene.remove_measurement(i);
                    }
                    ui.separator();
                }
                if let Some(message) =
                    properties(ui, scene, selection, settings, edit_point, &mut self.brick_dialog)
                {
                    self.status_message = Some(message);
                }
            });
        response.response.rect.width()
    }

    fn outliner(&mut self, ui: &mut egui::Ui, scene: &mut Scene, selection: &mut Selection) {
        let mut acts = OutlinerActions::default();

        // a fresh Break-into-Bricks folder starts collapsed, whichever path
        // created it (menu, wheel, properties, MCP) — hundreds of brick rows
        // would drown the outliner. Prune first: a new scene may reuse ids.
        self.seen_brick_folders.retain(|&id| scene.folder(id).is_some());
        for folder in scene.folders() {
            if folder.source_wall.is_some() && self.seen_brick_folders.insert(folder.id) {
                self.collapsed_folders.insert(folder.id);
            }
        }

        self.collapsed_objects.retain(|&id| scene.object(id).is_some());

        // children indented beneath their parent; collapsed parents keep
        // their subtree hidden
        fn push_children(
            scene: &Scene,
            parent: ObjectId,
            depth: u32,
            collapsed: &std::collections::HashSet<ObjectId>,
            out: &mut Vec<(ObjectId, u32)>,
        ) {
            if collapsed.contains(&parent) {
                return;
            }
            for object in scene.objects() {
                if object.parent == Some(parent) {
                    out.push((object.id, depth));
                    if depth < 32 {
                        push_children(scene, object.id, depth + 1, collapsed, out);
                    }
                }
            }
        }
        // a root shows at the top level (or under its folder)
        fn is_root(scene: &Scene, object: &modeler_core::Object) -> bool {
            object.parent.is_none()
                || object.parent.is_some_and(|p| scene.object(p).is_none())
        }

        let folder_ids: Vec<u64> = scene.folders().iter().map(|f| f.id).collect();

        // --- folders, each with its member roots -------------------------
        for &fid in &folder_ids {
            let Some(folder) = scene.folder(fid) else { continue };
            let folder_name = folder.name.clone();
            let open = !self.collapsed_folders.contains(&fid);
            self.folder_row(ui, scene, fid, &folder_name, open, &mut acts);
            if !open {
                continue;
            }
            let mut display: Vec<(ObjectId, u32)> = Vec::new();
            for object in scene.objects() {
                if object.folder == Some(fid) && is_root(scene, object) {
                    display.push((object.id, 1));
                    push_children(scene, object.id, 2, &self.collapsed_objects, &mut display);
                }
            }
            if display.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.weak("empty — drag objects onto the folder");
                });
            }
            for (id, depth) in display {
                acts.row_order.push(id);
                self.object_row(ui, scene, selection, id, depth, &mut acts);
            }
        }

        // --- objects outside any folder -----------------------------------
        let mut display: Vec<(ObjectId, u32)> = Vec::new();
        for object in scene.objects() {
            let unfoldered = object
                .folder
                .is_none_or(|f| !folder_ids.contains(&f));
            if is_root(scene, object) && unfoldered {
                display.push((object.id, 0));
                push_children(scene, object.id, 1, &self.collapsed_objects, &mut display);
            }
        }
        for (id, depth) in display {
            acts.row_order.push(id);
            self.object_row(ui, scene, selection, id, depth, &mut acts);
        }

        // drop here (or on empty space below the tree) to leave parents and
        // folders behind
        let any_filed = scene
            .objects()
            .iter()
            .any(|o| o.parent.is_some() || o.folder.is_some());
        let drag_active =
            egui::DragAndDrop::has_payload_of_type::<ObjectId>(ui.ctx());
        if any_filed || drag_active {
            let frame = egui::Frame::default()
                .stroke(egui::Stroke::new(1.0, ui.visuals().window_stroke.color))
                .inner_margin(4.0)
                .corner_radius(3.0);
            let (_, dropped) = ui.dnd_drop_zone::<ObjectId, ()>(frame, |ui| {
                ui.weak("⤒ drop here: no parent, no folder");
            });
            if let Some(dragged) = dropped {
                acts.move_to_folder = Some((*dragged, None));
            }
        }

        self.apply_outliner_actions(ui, scene, selection, acts);
    }

    /// One folder header: collapse triangle, eye, name (drop target,
    /// double-click renames), member count and delete.
    fn folder_row(
        &mut self,
        ui: &mut egui::Ui,
        scene: &Scene,
        fid: u64,
        name: &str,
        open: bool,
        acts: &mut OutlinerActions,
    ) {
        ui.horizontal(|ui| {
            let icon_resp = ui.allocate_response(
                egui::vec2(12.0, 14.0),
                egui::Sense::click(),
            );
            egui::collapsing_header::paint_default_icon(
                ui,
                if open { 1.0 } else { 0.0 },
                &icon_resp,
            );
            if icon_resp.clicked() {
                if open {
                    self.collapsed_folders.insert(fid);
                } else {
                    self.collapsed_folders.remove(&fid);
                }
            }

            let members: Vec<ObjectId> = scene
                .objects()
                .iter()
                .filter(|o| o.folder == Some(fid))
                .map(|o| o.id)
                .collect();
            let all_visible = members
                .iter()
                .all(|&id| scene.object(id).is_some_and(|o| o.visible));
            let eye = if all_visible { "●" } else { "○" };
            if ui
                .small_button(eye)
                .on_hover_text("Show / hide everything in this folder")
                .clicked()
            {
                acts.folder_eye = Some(fid);
            }

            if let Some((rename_id, buffer)) = &mut self.rename_folder {
                if *rename_id == fid {
                    let edit = ui.text_edit_singleline(buffer);
                    if self.rename_needs_focus {
                        edit.request_focus();
                        self.rename_needs_focus = false;
                    }
                    if edit.lost_focus() {
                        acts.commit_folder_rename = Some((fid, buffer.clone()));
                    }
                    return;
                }
            }

            let row = ui
                .selectable_label(
                    false,
                    egui::RichText::new(format!("📂 {name}")).strong(),
                )
                .interact(egui::Sense::click());
            ui.label(
                egui::RichText::new(format!("{}", members.len()))
                    .weak()
                    .size(10.5),
            );
            if row.double_clicked() {
                self.rename_folder = Some((fid, name.to_string()));
                self.rename_needs_focus = true;
            } else if row.clicked() {
                if open {
                    self.collapsed_folders.insert(fid);
                } else {
                    self.collapsed_folders.remove(&fid);
                }
            }
            // dropping an object on the folder files it here
            if let Some(dragged) = row.dnd_release_payload::<ObjectId>() {
                acts.move_to_folder = Some((*dragged, Some(fid)));
            }
            if row.dnd_hover_payload::<ObjectId>().is_some() {
                ui.painter().rect_stroke(
                    row.rect.expand(2.0),
                    3.0,
                    egui::Stroke::new(1.5, theme::accent(ui)),
                    egui::StrokeKind::Outside,
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("✖")
                    .on_hover_text("Delete the folder (its objects are kept)")
                    .clicked()
                {
                    acts.delete_folder = Some(fid);
                }
                // bricks folders can restore their original wall
                if scene.folder(fid).is_some_and(|f| f.source_wall.is_some())
                    && ui
                        .small_button("⟳")
                        .on_hover_text(
                            "Rebuild the wall: remove these bricks and restore \
                             the original wall object",
                        )
                        .clicked()
                {
                    acts.rebuild_wall = Some(fid);
                }
            });
        });
    }

    /// One object row: eye, selectable name (drag source & drop target for
    /// parenting), double-click renames.
    fn object_row(
        &mut self,
        ui: &mut egui::Ui,
        scene: &Scene,
        selection: &Selection,
        id: ObjectId,
        depth: u32,
        acts: &mut OutlinerActions,
    ) {
        let Some(object) = scene.object(id) else { return };
        let has_children = scene.objects().iter().any(|o| o.parent == Some(id));
        ui.horizontal(|ui| {
            ui.add_space(12.0 * depth as f32);
                // collapse triangle for objects with children; the slot is
                // always allocated so leaf rows line up with parent rows
                let icon_resp = ui.allocate_response(
                    egui::vec2(12.0, 14.0),
                    egui::Sense::click(),
                );
                if has_children {
                    let open = !self.collapsed_objects.contains(&id);
                    egui::collapsing_header::paint_default_icon(
                        ui,
                        if open { 1.0 } else { 0.0 },
                        &icon_resp,
                    );
                    if icon_resp.clicked() {
                        if open {
                            self.collapsed_objects.insert(id);
                        } else {
                            self.collapsed_objects.remove(&id);
                        }
                    }
                }
                // visibility "eye"
                let eye = if object.visible { "●" } else { "○" };
                if ui
                    .small_button(eye)
                    .on_hover_text("Show / hide (hidden objects are not selectable)")
                    .clicked()
                {
                    acts.visibility_toggle = Some(object.id);
                }

                if let Some((rename_id, buffer)) = &mut self.rename {
                    if *rename_id == object.id {
                        let edit = ui.text_edit_singleline(buffer);
                        if self.rename_needs_focus {
                            edit.request_focus();
                            self.rename_needs_focus = false;
                        }
                        if edit.lost_focus() {
                            acts.commit_rename = Some((object.id, buffer.clone()));
                        }
                        return;
                    }
                }

                let is_selected = selection.is_selected(object.id);
                // group roots select as one unit in the viewport — mark them
                let display_name = if object.group {
                    format!("❐ {}", object.name)
                } else {
                    object.name.clone()
                };
                let label = if selection.active() == Some(object.id) {
                    egui::RichText::new(&display_name).strong()
                } else {
                    egui::RichText::new(&display_name)
                };

                // click selects, double-click renames, dragging a row onto
                // another row parents it there. Senses are handled on the
                // row itself — egui's dnd_drag_source wrapper swallows
                // clicks (its drag overlay sits on top of the label).
                let row = ui
                    .selectable_label(is_selected, label)
                    .interact(egui::Sense::click_and_drag());
                if row.double_clicked() {
                    acts.start_rename = Some((object.id, object.name.clone()));
                } else if row.clicked() {
                    acts.clicked = Some(object.id);
                }
                if row.drag_started() {
                    egui::DragAndDrop::set_payload(ui.ctx(), object.id);
                }
                // floating name at the cursor while this row is dragged
                if row.dragged()
                    && egui::DragAndDrop::has_payload_of_type::<ObjectId>(ui.ctx())
                {
                    if let Some(pos) = ui.ctx().pointer_interact_pos() {
                        let painter = ui.ctx().layer_painter(egui::LayerId::new(
                            egui::Order::Tooltip,
                            egui::Id::new("outliner-drag-preview"),
                        ));
                        painter.text(
                            pos + egui::vec2(12.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            &object.name,
                            egui::FontId::proportional(13.0),
                            theme::accent(ui),
                        );
                    }
                }

                // drop target: another object released on this row
                if let Some(dragged) = row.dnd_release_payload::<ObjectId>() {
                    if *dragged != object.id {
                        acts.reparent = Some((*dragged, Some(object.id)));
                    }
                }
                // highlight while a compatible drag hovers this row
                if let Some(dragged) = row.dnd_hover_payload::<ObjectId>() {
                    if *dragged != object.id {
                        ui.painter().rect_stroke(
                            row.rect.expand(2.0),
                            3.0,
                            egui::Stroke::new(1.5, theme::accent(ui)),
                            egui::StrokeKind::Outside,
                        );
                    }
                }
            });
    }

    fn apply_outliner_actions(
        &mut self,
        ui: &egui::Ui,
        scene: &mut Scene,
        selection: &mut Selection,
        acts: OutlinerActions,
    ) {
        if let Some((child, parent)) = acts.reparent {
            if !scene.set_parent(child, parent) {
                self.status_message =
                    Some("can't parent an object to its own child".to_string());
            }
        }
        if let Some((id, folder)) = acts.move_to_folder {
            scene.set_folder(id, folder);
        }
        if let Some(fid) = acts.folder_eye {
            // toggle the whole folder content (members and their subtrees)
            let mut all: Vec<ObjectId> = Vec::new();
            let members: Vec<ObjectId> = scene
                .objects()
                .iter()
                .filter(|o| o.folder == Some(fid))
                .map(|o| o.id)
                .collect();
            for id in members {
                all.extend(scene.subtree(id));
            }
            let all_visible = all
                .iter()
                .all(|&id| scene.object(id).is_some_and(|o| o.visible));
            for id in all {
                if let Some(object) = scene.object_mut(id) {
                    object.visible = !all_visible;
                }
            }
        }
        if let Some(fid) = acts.delete_folder {
            scene.remove_folder(fid);
            self.collapsed_folders.remove(&fid);
            if self.rename_folder.as_ref().is_some_and(|(id, _)| *id == fid) {
                self.rename_folder = None;
            }
        }
        if let Some((fid, name)) = acts.commit_folder_rename {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                scene.rename_folder(fid, trimmed.to_string());
            }
            self.rename_folder = None;
        }
        if let Some(fid) = acts.rebuild_wall {
            if let Some(wall) = object_ops::rebuild_wall_from_folder(scene, fid) {
                selection.set(vec![wall], Some(wall));
                let name = scene
                    .object(wall)
                    .map(|o| o.name.clone())
                    .unwrap_or_default();
                self.status_message = Some(format!("bricks rebuilt into '{name}'"));
                self.collapsed_folders.remove(&fid);
            }
        }

        if let Some(id) = acts.visibility_toggle {
            if let Some(object) = scene.object_mut(id) {
                object.visible = !object.visible;
            }
        }
        if let Some(id) = acts.clicked {
            let (shift, ctrl) = ui.input(|i| (i.modifiers.shift, i.modifiers.command));
            if shift {
                // range select (Blender/file-manager): everything between
                // the active object and the clicked row, in displayed order
                let anchor = selection
                    .active()
                    .and_then(|a| acts.row_order.iter().position(|&r| r == a));
                let clicked = acts.row_order.iter().position(|&r| r == id);
                match (anchor, clicked) {
                    (Some(a), Some(c)) => {
                        let (lo, hi) = if a <= c { (a, c) } else { (c, a) };
                        selection.extend(&acts.row_order[lo..=hi], id);
                    }
                    // no anchor (or a hidden one): extend with the row alone
                    _ => selection.click(Some(id), true),
                }
            } else {
                // Ctrl+Click toggles one object, a plain click selects it
                selection.click(Some(id), ctrl);
            }
        }
        if let Some((id, name)) = acts.start_rename {
            self.rename = Some((id, name));
            self.rename_needs_focus = true;
        }
        if let Some((id, name)) = acts.commit_rename {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                if let Some(object) = scene.object_mut(id) {
                    object.name = trimmed.to_string();
                }
            }
            self.rename = None;
        }
    }

    fn keymap_window(&mut self, ctx: &egui::Context) {
        if !self.show_keymap {
            return;
        }
        let mut open = self.show_keymap;
        egui::Window::new("Keymap")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("keymap-grid").striped(true).show(ui, |ui| {
                    for (keys, action) in [
                        ("LMB / Shift+LMB", "Select / extend selection"),
                        ("Outliner Shift/Ctrl+Click", "Select range to active / toggle one"),
                        ("RMB (viewport)", "Context menu: pivot/anchor & object actions"),
                        ("MMB drag", "Orbit"),
                        ("Shift+MMB", "Pan"),
                        ("Wheel / Ctrl+MMB", "Zoom"),
                        ("1 / 3 / 7 (+Ctrl)", "Front / Right / Top views"),
                        ("4 / 6 / 8 / 2", "Step-rotate view"),
                        ("5", "Orthographic / perspective"),
                        (". / Home", "Frame selection / scene"),
                        ("End", "Drop selection onto the ground or the objects below it"),
                        ("G (image selected)", "Move the selected reference image"),
                        ("Shift+A", "Add wheel: flick towards an item, click to add"),
                        ("G / R / S", "Move / Rotate / Scale"),
                        ("X / Y / Z (modal)", "Axis constraint (Shift: plane)"),
                        ("digits (modal)", "Exact value, Enter applies"),
                        ("Ctrl (modal)", "Snap 1 m / 5° / 0.1"),
                        ("Shift+D", "Duplicate"),
                        ("X / Delete", "Delete (confirm / immediate)"),
                        ("Tab", "Object edit mode (active object)"),
                        ("1 / 2 / 3 (edit)", "Vertex / Edge / Face select"),
                        ("G / R / S (edit)", "Move / rotate / scale element (X/Y/Z axis)"),
                        ("P / A (edit)", "Selected element becomes pivot / anchor"),
                        ("N", "Toggle sidebar"),
                        ("Space", "Play / pause physics"),
                        ("LMB hold (simulating)", "Charge a poke; release kicks the object under the cursor"),
                        ("Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y", "Undo / redo"),
                        ("Ctrl+S / Ctrl+O / Ctrl+N", "Save / Open / New scene"),
                        ("Ctrl+P / Alt+P", "Parent to active / clear parent"),
                        ("Add ▸ Wall", "Draw walls on the floor: click corners, Esc ends"),
                        ("Add ▸ Floor", "Floor slab encompassing the selected walls"),
                        ("Drag ⊕ handle (wall)", "Move a door/window opening; Esc cancels the drag"),
                        ("Add ▸ Measure", "Ruler: click two points"),
                        ("Esc", "Stop physics (restore)"),
                    ] {
                        ui.label(egui::RichText::new(keys).monospace());
                        ui.label(action);
                        ui.end_row();
                    }
                });
            });
        self.show_keymap = open;
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        // Decode the embedded banner once, then reuse the GPU texture.
        let texture = self
            .about_texture
            .get_or_insert_with(|| {
                let bytes = include_bytes!("../assets/about.jpg");
                let rgba = image::load_from_memory(bytes)
                    .expect("embedded about banner decodes")
                    .to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let pixels = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                ctx.load_texture("about-banner", pixels, egui::TextureOptions::LINEAR)
            })
            .clone();

        let mut open = self.show_about;
        egui::Window::new("About")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let width = 440.0;
                ui.set_width(width);
                ui.add(
                    egui::Image::new(&texture)
                        .fit_to_exact_size(egui::vec2(width, width / texture.aspect_ratio())),
                );
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.heading("3D Modeler");
                    ui.label(
                        egui::RichText::new(concat!("version ", env!("CARGO_PKG_VERSION"))).weak(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        "A Blender-style 3D modeler with real physics at its core: \
                         every object lives in a box3d world — stack it, poke it, \
                         knock it over.",
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Rust · egui · three-d — runs natively and in the browser (WebAssembly)",
                        )
                        .size(12.0)
                        .weak(),
                    );
                    ui.add_space(6.0);
                    ui.hyperlink_to(
                        "bartbeecoders/3dmodeler on GitHub",
                        "https://github.com/bartbeecoders/3dmodeler",
                    );
                    ui.add_space(4.0);
                });
            });
        self.show_about = open;
    }
}

// --- menu contents ---------------------------------------------------------

fn edit_menu(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    undo: &mut UndoStack,
    settings_window: &mut SettingsWindow,
) -> bool {
    let mut close = false;
    if ui
        .add_enabled(undo.can_undo(), egui::Button::new("Undo  (Ctrl+Z)"))
        .clicked()
    {
        undo.undo(scene);
        close = true;
    }
    if ui
        .add_enabled(
            undo.can_redo(),
            egui::Button::new("Redo  (Ctrl+Shift+Z / Ctrl+Y)"),
        )
        .clicked()
    {
        undo.redo(scene);
        close = true;
    }
    ui.separator();
    if ui.button("Preferences…").clicked() {
        settings_window.open = true;
        close = true;
    }
    close
}

#[allow(clippy::too_many_arguments)]
fn add_menu_items(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    selection: &mut Selection,
    measure: &mut MeasureTool,
    wall_tool: &mut crate::wall_tool::WallTool,
    settings: &Settings,
    ref_setup: &mut RefSetupDialog,
    status: &mut Option<String>,
) -> bool {
    if let Some(primitive) = crate::add_menu::mesh_menu_buttons(ui) {
        let id = scene.add_object(primitive, Transform::default());
        selection.set(vec![id], Some(id));
        return true;
    }
    ui.separator();
    if crate::pie::icon_menu_button(ui, &crate::pie::PieIcon::Wall, "Wall")
        .on_hover_text(
            "Draw wall segments on the floor: click the start point, then each \
             corner; Enter keeps the current segment, Esc/RMB ends the tool",
        )
        .clicked()
    {
        wall_tool.start(settings);
        return true;
    }
    if crate::pie::icon_menu_button(ui, &crate::pie::PieIcon::Floor, "Floor")
        .on_hover_text(
            "Add a floor slab sized to encompass the selected walls \
             (all walls when none are selected)",
        )
        .clicked()
    {
        *status = Some(crate::object_ops::add_floor(scene, selection));
        return true;
    }
    ui.separator();
    // lights (Blender's Add ▸ Light)
    for (light, tip) in Primitive::light_catalog().iter().zip([
        "Point light: shines in all directions, falls off with distance",
        "Sun: parallel light from infinitely far away — rotate to aim (-Z)",
        "Spot: a cone of light along -Z with an adjustable angle",
    ]) {
        let label = format!("{} Light", light.base_name());
        if crate::pie::icon_menu_button(ui, &crate::pie::primitive_icon(light), &label)
            .on_hover_text(tip)
            .clicked()
        {
            let id = scene.add_object(*light, Transform::default());
            selection.set(vec![id], Some(id));
            return true;
        }
    }
    ui.separator();
    if ui
        .button("Reference Image…")
        .on_hover_text("Place a PNG/JPEG on an axis plane as a modeling reference")
        .clicked()
    {
        ref_image::request_image();
        return true;
    }
    if ui
        .button("Reference Setup…")
        .on_hover_text(
            "Load a whole drawing set at once and drag each picture onto the \
             view it shows (front, side, floor plan…) — all images are placed \
             around the origin, oriented and scaled consistently",
        )
        .clicked()
    {
        ref_setup.open();
        return true;
    }
    ui.separator();
    if ui
        .button("Measure (ruler)")
        .on_hover_text("Click two points in the viewport to measure the distance")
        .clicked()
    {
        measure.start();
        return true;
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn object_menu(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    selection: &mut Selection,
    modal: &mut ModalTransform,
    physics: &mut PhysicsMirror,
    library_panel: &mut LibraryPanel,
    status: &mut Option<String>,
    brick_dialog: &mut Option<i32>,
) -> bool {
    let has_selection = !selection.is_empty();
    let mut close = false;

    if ui
        .add_enabled(has_selection, egui::Button::new("Duplicate  (Shift+D)"))
        .clicked()
    {
        if modal::duplicate_selection(scene, selection) {
            modal.begin_grab(scene, selection);
        }
        close = true;
    }
    if ui
        .add_enabled(has_selection, egui::Button::new("Drop to Floor  (End)"))
        .on_hover_text(
            "Drop onto the ground or the objects below, whichever is higher",
        )
        .clicked()
    {
        physics.sync(scene); // mirror must match before ray casting
        physics.drop_to_floor(scene, selection);
        close = true;
    }
    if ui
        .add_enabled(has_selection, egui::Button::new("Place on Ground"))
        .on_hover_text(
            "Move the selection vertically so its lowest point sits at z = 0, \
             ignoring objects below",
        )
        .clicked()
    {
        object_ops::place_on_ground(scene, selection);
        close = true;
    }
    if ui
        .add_enabled(has_selection, egui::Button::new("Apply Scale"))
        .on_hover_text(
            "Bake the selection's scale into its geometry and reset the scale \
             to 1 (Blender's Ctrl+A ▸ Scale); children keep their placement",
        )
        .clicked()
    {
        *status = Some(object_ops::apply_scale(scene, selection));
        close = true;
    }
    ui.separator();
    if ui
        .add_enabled(has_selection, egui::Button::new("Shade Smooth"))
        .clicked()
    {
        set_selected_smooth(scene, selection, true);
        close = true;
    }
    if ui
        .add_enabled(has_selection, egui::Button::new("Shade Flat"))
        .clicked()
    {
        set_selected_smooth(scene, selection, false);
        close = true;
    }
    ui.separator();
    let breakable_active = selection
        .active()
        .and_then(|id| scene.object(id))
        .is_some_and(|o| {
            !o.primitive.is_light() && !matches!(o.primitive, Primitive::Empty { .. })
        });
    if ui
        .add_enabled(breakable_active, egui::Button::new("Break into Bricks…"))
        .on_hover_text(
            "Replace the active object with individual dynamic bricks \
             (running bond; walls keep their openings, curved shapes get a \
             stepped approximation) that collide and tumble when the \
             simulation plays (Space). Opens a dialog to pick the brick count",
        )
        .clicked()
    {
        *brick_dialog = Some(object_ops::DEFAULT_BRICKS as i32);
        close = true;
    }
    let rebuild_folder = selection
        .active()
        .and_then(|id| object_ops::rebuildable_folder(scene, id));
    if ui
        .add_enabled(
            rebuild_folder.is_some(),
            egui::Button::new("Rebuild from Bricks"),
        )
        .on_hover_text(
            "Remove the bricks (wherever they tumbled) and restore the \
             original wall object",
        )
        .clicked()
    {
        if let Some(folder) = rebuild_folder {
            if let Some(wall) = object_ops::rebuild_wall_from_folder(scene, folder) {
                selection.set(vec![wall], Some(wall));
                let name = scene
                    .object(wall)
                    .map(|o| o.name.clone())
                    .unwrap_or_default();
                *status = Some(format!("bricks rebuilt into '{name}'"));
            }
        }
        close = true;
    }
    ui.separator();
    let can_parent = selection.selected().len() >= 2 && selection.active().is_some();
    if ui
        .add_enabled(can_parent, egui::Button::new("Group Selection"))
        .on_hover_text(
            "Parent the selection to the active object and select it as one \
             unit from then on (placed library objects come pre-grouped)",
        )
        .clicked()
    {
        group_selection(scene, selection);
        close = true;
    }
    let has_group = selection
        .selected()
        .iter()
        .any(|&id| scene.object(id).is_some_and(|o| o.group));
    if ui
        .add_enabled(has_group, egui::Button::new("Ungroup"))
        .on_hover_text(
            "Break the selected group(s) into parts: clicks select parts \
             individually again (the hierarchy is kept)",
        )
        .clicked()
    {
        ungroup_selection(scene, selection);
        close = true;
    }
    ui.separator();
    if ui
        .add_enabled(can_parent, egui::Button::new("Parent to Active  (Ctrl+P)"))
        .clicked()
    {
        parent_selected_to_active(scene, selection);
        close = true;
    }
    if ui
        .add_enabled(can_parent, egui::Button::new("Attach to Active"))
        .on_hover_text(
            "Parent to the active object AND move each selected object so its \
             anchor point lands on the active object's anchor point",
        )
        .clicked()
    {
        if let Some(active) = selection.active() {
            for id in selection.selected().to_vec() {
                if id != active {
                    scene.attach(id, active, None);
                }
            }
        }
        close = true;
    }
    if ui
        .add_enabled(has_selection, egui::Button::new("Clear Parent  (Alt+P)"))
        .clicked()
    {
        for id in selection.selected().to_vec() {
            scene.set_parent(id, None);
        }
        close = true;
    }
    ui.separator();
    if ui
        .add_enabled(
            has_selection,
            egui::Button::new("Save Selection to Library…"),
        )
        .on_hover_text("Store the selected objects (children included) as a reusable library item")
        .clicked()
    {
        library_panel.open_create_dialog(scene, selection);
        close = true;
    }
    ui.separator();
    if ui
        .add_enabled(has_selection, egui::Button::new("Delete  (X)"))
        .clicked()
    {
        object_ops::delete_selected(scene, selection);
        close = true;
    }
    close
}

/// Blender Ctrl+P: every selected object (except the active one) becomes a
/// child of the active object, keeping its world transform.
pub fn parent_selected_to_active(scene: &mut Scene, selection: &Selection) {
    let Some(active) = selection.active() else { return };
    for id in selection.selected().to_vec() {
        if id != active {
            scene.set_parent(id, Some(active));
        }
    }
}

/// Make the selection ONE group: parent everything to the active object
/// (world transforms preserved) and flag it as the group root — viewport
/// clicks then select the whole assembly.
pub fn group_selection(scene: &mut Scene, selection: &Selection) {
    let Some(active) = selection.active() else { return };
    parent_selected_to_active(scene, selection);
    if let Some(object) = scene.object_mut(active) {
        object.group = true;
    }
}

/// Clear the group flag on every selected group root: parts become
/// individually selectable again (the parent hierarchy is kept).
pub fn ungroup_selection(scene: &mut Scene, selection: &Selection) {
    for id in selection.selected().to_vec() {
        if scene.object(id).is_some_and(|o| o.group) {
            if let Some(object) = scene.object_mut(id) {
                object.group = false;
            }
        }
    }
}

fn set_selected_smooth(scene: &mut Scene, selection: &Selection, smooth: bool) {
    let ids: Vec<ObjectId> = selection.selected().to_vec();
    for id in ids {
        if let Some(object) = scene.object_mut(id) {
            object.smooth = smooth;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn view_menu(
    ui: &mut egui::Ui,
    camera: &mut BlenderCamera,
    scene: &Scene,
    selection: &Selection,
    settings: &mut Settings,
    snap_to_grid: &mut bool,
    shade_mode: &mut ShadeMode,
    lighting_mode: &mut LightingMode,
    xray: &mut bool,
    chat_open: &mut bool,
) -> bool {
    let mut close = false;
    if ui
        .selectable_label(*chat_open, "AI Assistant")
        .on_hover_text("Chat with an AI that models alongside you")
        .clicked()
    {
        *chat_open = !*chat_open;
        close = true;
    }
    ui.separator();
    ui.label(egui::RichText::new("Viewport shading").weak().size(11.0));
    ui.horizontal(|ui| {
        for (mode, label) in [
            (ShadeMode::Wireframe, "Wireframe"),
            (ShadeMode::Solid, "Solid"),
            (ShadeMode::Shaded, "Shaded"),
        ] {
            if ui.selectable_label(*shade_mode == mode, label).clicked() {
                *shade_mode = mode;
            }
        }
    });
    ui.checkbox(xray, "X-ray");
    ui.label(egui::RichText::new("Lighting (shaded)").weak().size(11.0));
    ui.horizontal(|ui| {
        for (mode, label) in [
            (LightingMode::Studio, "Studio"),
            (LightingMode::Scene, "Scene lights"),
        ] {
            if ui
                .selectable_label(*lighting_mode == mode, label)
                .clicked()
            {
                *lighting_mode = mode;
                *shade_mode = ShadeMode::Shaded;
            }
        }
    });
    ui.separator();
    ui.label(egui::RichText::new("Color theme").weak().size(11.0));
    ui.horizontal(|ui| {
        for theme in Theme::ALL {
            if ui
                .selectable_label(settings.theme == theme, theme.label())
                .on_hover_text(theme.description())
                .clicked()
            {
                settings.theme = theme;
            }
        }
    });
    ui.separator();
    ui.label(egui::RichText::new("Grid spacing").weak().size(11.0));
    ui.horizontal(|ui| {
        let unit = settings.unit;
        for spacing in [0.1f32, 0.25, 0.5, 1.0, 2.0] {
            let selected = (settings.grid_spacing - spacing).abs() < 1e-6;
            let label = format!("{}", unit.from_meters(spacing));
            if ui.selectable_label(selected, label).clicked() {
                settings.grid_spacing = spacing;
            }
        }
        ui.label(egui::RichText::new(settings.unit.suffix()).weak());
    });
    ui.checkbox(snap_to_grid, "Snap to grid");
    ui.separator();
    for (label, yaw, pitch) in [
        ("Front  (1)", 0.0, 0.0),
        ("Right  (3)", 90.0, 0.0),
        ("Top  (7)", 0.0, 90.0),
    ] {
        if ui.button(label).clicked() {
            camera.set_view(yaw, pitch);
            close = true;
        }
    }
    ui.separator();
    let projection = if camera.ortho {
        "Perspective  (5)"
    } else {
        "Orthographic  (5)"
    };
    if ui.button(projection).clicked() {
        camera.toggle_ortho();
        close = true;
    }
    ui.separator();
    if ui.button("Frame Selection  (.)").clicked() {
        if let Some((center, radius)) = crate::selection_bounds(scene, selection) {
            camera.frame(three_d::vec3(center.x, center.y, center.z), radius);
        }
        close = true;
    }
    if ui.button("Frame All  (Home)").clicked() {
        if let Some((center, radius)) = scene.bounds() {
            camera.frame(three_d::vec3(center.x, center.y, center.z), radius);
        }
        close = true;
    }
    close
}

// --- reference images ---------------------------------------------------------

/// Sidebar rows for every reference image: visibility, plane, placement,
/// size, opacity, calibration and delete. The name row selects the image
/// (shared `Selection`, so the viewport outline and the row highlight stay
/// in sync in both directions).
fn reference_image_rows(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    selection: &mut Selection,
    settings: &Settings,
    calibrate: &mut CalibrateTool,
) {
    let unit = settings.unit;
    let ids: Vec<u64> = scene.reference_images().iter().map(|r| r.id).collect();
    let mut delete: Option<u64> = None;

    for id in ids {
        let Some(image) = scene.reference_images().iter().find(|r| r.id == id) else {
            continue;
        };
        // edit a copy; write back only on change (writes bump the version)
        let mut edited = image.clone();
        let mut changed = false;

        // custom collapsing row: the triangle toggles the details, the name
        // itself is a selectable label (like outliner object rows)
        egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            ui.make_persistent_id(("ref-image", id)),
            false,
        )
        .show_header(ui, |ui| {
            let is_selected = selection.image() == Some(id);
            if ui
                .selectable_label(is_selected, &edited.name)
                .on_hover_text("Select this reference image (G moves it in the viewport)")
                .clicked()
            {
                selection.select_image(id);
            }
        })
        .body(|ui| {
                ui.horizontal(|ui| {
                    let eye = if edited.visible { "●" } else { "○" };
                    if ui.small_button(eye).on_hover_text("Show / hide").clicked() {
                        edited.visible = !edited.visible;
                        changed = true;
                    }
                    if ui.small_button("✖").on_hover_text("Delete image").clicked() {
                        delete = Some(id);
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "{} × {}",
                            unit.format(edited.width_m),
                            unit.format(edited.height_m()),
                        ))
                        .weak()
                        .size(11.0),
                    );
                });

                ui.horizontal(|ui| {
                    ui.label("Plane");
                    for plane in ImagePlane::ALL {
                        if ui
                            .selectable_label(edited.plane == plane, plane.label())
                            .clicked()
                            && edited.plane != plane
                        {
                            edited.plane = plane;
                            changed = true;
                        }
                    }
                });

                ui.label("Location");
                ui.horizontal(|ui| {
                    for value in [&mut edited.location.x, &mut edited.location.y, &mut edited.location.z] {
                        changed |= ui
                            .add(egui::DragValue::new(value).speed(0.05))
                            .changed();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Rotation");
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut edited.rotation_deg)
                                .speed(1.0)
                                .suffix("°"),
                        )
                        .changed();
                });

                ui.horizontal(|ui| {
                    ui.label("Width");
                    let mut width = unit.from_meters(edited.width_m);
                    if ui
                        .add(
                            egui::DragValue::new(&mut width)
                                .speed(0.02 * unit.per_meter() as f64)
                                .range(unit.from_meters(0.01)..=unit.from_meters(1000.0))
                                .suffix(format!(" {}", unit.suffix())),
                        )
                        .changed()
                    {
                        edited.width_m = unit.to_meters(width).max(0.01);
                        changed = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Opacity");
                    changed |= ui
                        .add(egui::Slider::new(&mut edited.opacity, 0.0..=1.0))
                        .changed();
                });

                changed |= ui
                    .checkbox(&mut edited.flip_h, "Mirror horizontally")
                    .on_hover_text(
                        "Back/left elevations are drawn as seen from behind/left — \
                         mirror them so they read correctly from that side",
                    )
                    .changed();

                let calibrating = calibrate.target == Some(id);
                if calibrating {
                    if ui.button("Cancel scale picking").clicked() {
                        calibrate.cancel();
                    }
                } else if ui
                    .button("Scale from 2 points…")
                    .on_hover_text(
                        "Click two points on the image in the viewport, then enter \
                         the real distance between them",
                    )
                    .clicked()
                {
                    calibrate.start(id);
                }
            });

        if changed {
            if let Some(target) = scene.reference_image_mut(id) {
                *target = edited;
            }
        }
    }

    if let Some(id) = delete {
        if calibrate.target == Some(id) {
            calibrate.cancel();
        }
        scene.remove_reference_image(id);
    }
}

/// After two points are picked: ask for the real-world distance and rescale.
fn calibrate_window(
    ctx: &egui::Context,
    scene: &mut Scene,
    calibrate: &mut CalibrateTool,
    settings: &Settings,
) {
    let Some(measured) = calibrate.measured() else { return };
    let Some(id) = calibrate.target else { return };
    let unit = settings.unit;

    let mut open = true;
    let mut done = false;
    egui::Window::new("Scale reference image")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(format!(
                "Picked distance on the image: {}",
                unit.format(measured)
            ));
            ui.horizontal(|ui| {
                ui.label(format!("Real distance ({}):", unit.suffix()));
                let response = ui.text_edit_singleline(&mut calibrate.distance_input);
                response.request_focus();
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = true;
                }
            });
            let parsed = calibrate.distance_input.trim().replace(',', ".").parse::<f32>();
            let valid = parsed.as_ref().is_ok_and(|v| *v > 0.0);
            if ui.add_enabled(valid, egui::Button::new("Apply")).clicked() {
                done = true;
            }
            if done && valid {
                let real_m = unit.to_meters(parsed.unwrap());
                if let Some(image) = scene.reference_image_mut(id) {
                    CalibrateTool::apply_scale(image, measured, real_m);
                }
                calibrate.cancel();
            }
        });
    if !open {
        calibrate.cancel();
    }
}

// --- properties (N panel) ----------------------------------------------------

fn properties(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    selection: &mut Selection,
    settings: &Settings,
    edit_point: Option<(ObjectId, Vec3)>,
    brick_dialog: &mut Option<i32>,
) -> Option<String> {
    let Some(active_id) = selection.active() else {
        ui.weak("No active object");
        return None;
    };
    let Some(object) = scene.object(active_id) else {
        return None;
    };

    // editable object name (also renamable by double-click in the outliner)
    let mut name = object.name.clone();
    ui.horizontal(|ui| {
        ui.label("Name");
        ui.text_edit_singleline(&mut name);
    });
    if name != object.name && !name.trim().is_empty() {
        if let Some(object) = scene.object_mut(active_id) {
            object.name = name;
        }
    }
    let Some(object) = scene.object(active_id) else { return None };
    if let Some(parent) = object.parent {
        if let Some(parent_object) = scene.object(parent) {
            ui.label(
                egui::RichText::new(format!("child of {} (transform is local)", parent_object.name))
                    .weak()
                    .size(11.0),
            );
        }
    }

    // edit copies; write back only when something changed (writes bump the
    // scene version and trigger rebuilds)
    let mut transform = object.transform;
    let mut primitive = object.primitive;
    let mut material = object.material;
    let mut smooth = object.smooth;
    let mut phys = (object.dynamic, object.density);
    let mut adorn = (object.show_label, object.show_dimensions);
    let mut pivot = object.pivot;
    let mut anchor = object.anchor;
    let mut changed = false;
    let mut edited_cutouts: Option<Vec<modeler_core::WallCutout>> = None;
    let mut break_bricks = false;

    egui::CollapsingHeader::new("Transform")
        .default_open(true)
        .show(ui, |ui| {
            changed |= vec3_row(ui, "Location", &mut transform.location, 0.05);

            // display rotation as XYZ Euler degrees like Blender
            let (rx, ry, rz) = transform.rotation.to_euler(EulerRot::XYZ);
            let mut degrees = [rx.to_degrees(), ry.to_degrees(), rz.to_degrees()];
            let mut rot_changed = false;
            ui.label("Rotation");
            ui.horizontal(|ui| {
                for value in &mut degrees {
                    rot_changed |= ui
                        .add(egui::DragValue::new(value).speed(1.0).suffix("°"))
                        .changed();
                }
            });
            if rot_changed {
                transform.rotation = Quat::from_euler(
                    EulerRot::XYZ,
                    degrees[0].to_radians(),
                    degrees[1].to_radians(),
                    degrees[2].to_radians(),
                );
                changed = true;
            }

            changed |= vec3_row(ui, "Scale", &mut transform.scale, 0.02);
        });

    egui::CollapsingHeader::new("Pivot & Anchor")
        .default_open(false)
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("Local-space points; markers show in the viewport.")
                    .weak()
                    .size(11.0),
            );
            changed |= vec3_row(ui, "Pivot (rotation center, R)", &mut pivot, 0.05);
            changed |= vec3_row(ui, "Anchor (attachment point)", &mut anchor, 0.05);

            // edit mode (Tab) with an element selected on THIS object: one
            // click makes that vertex/edge/face the pivot or anchor
            match edit_point {
                Some((edit_id, point)) if edit_id == active_id => {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button("Pivot = selection")
                            .on_hover_text(
                                "Set the pivot to the selected vertex (edge midpoint / \
                                 face center) — also: press P in edit mode",
                            )
                            .clicked()
                        {
                            pivot = point;
                            changed = true;
                        }
                        if ui
                            .button("Anchor = selection")
                            .on_hover_text(
                                "Set the anchor to the selected vertex (edge midpoint / \
                                 face center) — also: press A in edit mode",
                            )
                            .clicked()
                        {
                            anchor = point;
                            changed = true;
                        }
                    });
                }
                _ => {
                    ui.label(
                        egui::RichText::new(
                            "Tab into edit mode and select a vertex to set these \
                             from the geometry (P = pivot, A = anchor).",
                        )
                        .weak()
                        .size(11.0),
                    );
                }
            }
        });

    let mut revert_mesh = false;
    if let Some(mesh) = &object.edited_mesh {
        let (verts, tris) = (mesh.positions.len(), mesh.indices.len() / 3);
        egui::CollapsingHeader::new("Mesh (edited)")
            .default_open(true)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("{verts} vertices · {tris} triangles"))
                        .weak()
                        .size(11.0),
                );
                revert_mesh = ui
                    .button("Revert to primitive")
                    .on_hover_text("Discard all mesh edits and restore the parametric shape")
                    .clicked();
            });
    } else {
        let header = if primitive.is_light() { "Light" } else { "Primitive" };
        egui::CollapsingHeader::new(header)
            .default_open(true)
            .show(ui, |ui| {
                changed |=
                    primitive_params(ui, &mut primitive, !object.floor_outline.is_empty());
                if !primitive.is_light() {
                    changed |= ui.checkbox(&mut smooth, "Shade smooth").changed();
                }
            });

        // wall openings (doors & windows) — cutout edits regenerate the mesh
        if let Primitive::Wall { length, height, .. } = primitive {
            let mut cutouts = object.cutouts.clone();
            let mut cut_changed = false;
            egui::CollapsingHeader::new("Openings (doors & windows)")
                .default_open(true)
                .show(ui, |ui| {
                    cut_changed = wall_cutout_rows(ui, &mut cutouts, length, height);
                });
            if cut_changed {
                edited_cutouts = Some(cutouts);
            }
        }
    }

    // subdivision surface (Blender's subsurf modifier): render-time
    // Catmull-Clark on the base mesh; edit mode keeps editing the cage
    let mut subdivision_change = None;
    if !primitive.is_light() && !matches!(primitive, Primitive::Empty { .. }) {
        egui::CollapsingHeader::new("Subdivision Surface")
            .default_open(object.subdivision > 0)
            .show(ui, |ui| {
                let mut levels = object.subdivision as u32;
                if int_row(ui, "Levels", &mut levels, 0..=4) {
                    subdivision_change = Some(levels as u8);
                }
                if object.subdivision > 0 {
                    ui.label(
                        egui::RichText::new(
                            "smooths the viewport mesh; editing and physics \
                             use the base shape",
                        )
                        .weak()
                        .size(11.0),
                    );
                }
            });
    }

    // any solid object can shatter; lights and empties have no volume
    let breakable = !object.primitive.is_light()
        && !matches!(object.primitive, Primitive::Empty { .. });
    if breakable
        && ui
            .button("Break into bricks")
            .on_hover_text(
                "Replace this object with individual dynamic bricks (running \
                 bond; walls keep their openings, curved shapes get a stepped \
                 approximation). They collide and can tumble when the \
                 simulation plays (Space)",
            )
            .clicked()
    {
        break_bricks = true;
    }

    egui::CollapsingHeader::new("Adornments")
        .default_open(false)
        .show(ui, |ui| {
            changed |= ui.checkbox(&mut adorn.0, "Show name label").changed();
            changed |= ui
                .checkbox(
                    &mut adorn.1,
                    format!("Show dimensions ({})", settings.unit.suffix()),
                )
                .changed();
        });

    // lights neither simulate nor have a surface material
    if !primitive.is_light() {
        egui::CollapsingHeader::new("Physics")
            .default_open(true)
            .show(ui, |ui| {
                let mut dynamic = object.dynamic;
                let mut density = object.density;
                if ui
                    .checkbox(&mut dynamic, "Dynamic")
                    .on_hover_text("Falls and collides during simulation (▶)")
                    .changed()
                {
                    changed = true;
                }
                ui.add_enabled_ui(dynamic, |ui| {
                    if slider_row(ui, "Density", &mut density, 0.1..=20.0) {
                        changed = true;
                    }
                });
                phys = (dynamic, density);
            });

        egui::CollapsingHeader::new("Material")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Base color");
                    changed |= ui.color_edit_button_rgb(&mut material.base_color).changed();
                });
                changed |= slider_row(ui, "Roughness", &mut material.roughness, 0.0..=1.0);
                changed |= slider_row(ui, "Metallic", &mut material.metallic, 0.0..=1.0);
            });
    }

    if changed {
        if let Some(object) = scene.object_mut(active_id) {
            object.transform = transform;
            object.primitive = primitive;
            object.material = material;
            object.smooth = smooth;
            object.dynamic = phys.0;
            object.density = phys.1;
            object.show_label = adorn.0;
            object.show_dimensions = adorn.1;
            object.pivot = pivot;
            object.anchor = anchor;
        }
    }
    if let Some(cutouts) = edited_cutouts {
        if let Some(object) = scene.object_mut(active_id) {
            object.cutouts = cutouts;
            object.mesh_revision += 1; // caches key on it (primitive unchanged)
        }
    }
    if let Some(levels) = subdivision_change {
        if let Some(object) = scene.object_mut(active_id) {
            object.subdivision = levels;
            object.mesh_revision += 1;
        }
    }
    if revert_mesh {
        if let Some(object) = scene.object_mut(active_id) {
            object.edited_mesh = None;
            object.mesh_revision += 1;
        }
    }
    if break_bricks {
        *brick_dialog = Some(object_ops::DEFAULT_BRICKS as i32);
    }
    None
}

/// Toolbar toggle for the edit-mode element select, with a painted
/// vertex / edge / face pictogram (text glyphs don't cover these shapes).
fn select_mode_button(ui: &mut egui::Ui, active: bool, mode: SelectMode) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(26.0, 20.0), egui::Sense::click());
    let visuals = ui.style().interact_selectable(&response, active);
    if active || response.hovered() {
        ui.painter().rect_filled(rect, 3.0, visuals.bg_fill);
    }
    let color = visuals.fg_stroke.color;
    let r = rect.shrink2(egui::vec2(8.0, 5.5));
    let painter = ui.painter();
    let corners = [r.left_bottom(), r.center_top(), r.right_bottom()];
    match mode {
        SelectMode::Vertex => {
            for c in corners {
                painter.circle_filled(c, 2.0, color);
            }
        }
        SelectMode::Edge => {
            painter.line_segment(
                [corners[0], corners[1]],
                egui::Stroke::new(2.0, color),
            );
            painter.circle_filled(corners[0], 2.0, color);
            painter.circle_filled(corners[1], 2.0, color);
        }
        SelectMode::Face => {
            painter.add(egui::Shape::convex_polygon(
                corners.to_vec(),
                color,
                egui::Stroke::NONE,
            ));
        }
    }
    response
}

fn vec3_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut modeler_core::glam::Vec3,
    speed: f64,
) -> bool {
    let mut changed = false;
    ui.label(label);
    ui.horizontal(|ui| {
        changed |= ui.add(egui::DragValue::new(&mut value.x).speed(speed)).changed();
        changed |= ui.add(egui::DragValue::new(&mut value.y).speed(speed)).changed();
        changed |= ui.add(egui::DragValue::new(&mut value.z).speed(speed)).changed();
    });
    changed
}

fn slider_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::Slider::new(value, range)).changed()
    })
    .inner
}

fn int_row(ui: &mut egui::Ui, label: &str, value: &mut u32, range: std::ops::RangeInclusive<u32>) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(value).speed(0.2).range(range)).changed()
    })
    .inner
}

fn float_row(ui: &mut egui::Ui, label: &str, value: &mut f32, speed: f64) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(value).speed(speed).range(0.001..=1000.0)).changed()
    })
    .inner
}

fn primitive_params(ui: &mut egui::Ui, primitive: &mut Primitive, shaped: bool) -> bool {
    let mut changed = false;
    match primitive {
        Primitive::Plane { size } | Primitive::Cube { size } => {
            changed |= float_row(ui, "Size", size, 0.02);
        }
        Primitive::UvSphere { segments, rings, radius } => {
            changed |= int_row(ui, "Segments", segments, 3..=128);
            changed |= int_row(ui, "Rings", rings, 2..=64);
            changed |= float_row(ui, "Radius", radius, 0.02);
        }
        Primitive::IcoSphere { subdivisions, radius } => {
            changed |= int_row(ui, "Subdivisions", subdivisions, 0..=5);
            changed |= float_row(ui, "Radius", radius, 0.02);
        }
        Primitive::Cylinder { vertices, radius, depth } => {
            changed |= int_row(ui, "Vertices", vertices, 3..=128);
            changed |= float_row(ui, "Radius", radius, 0.02);
            changed |= float_row(ui, "Depth", depth, 0.02);
        }
        Primitive::Cone { vertices, radius_bottom, radius_top, depth } => {
            changed |= int_row(ui, "Vertices", vertices, 3..=128);
            changed |= float_row(ui, "Radius bottom", radius_bottom, 0.02);
            ui.horizontal(|ui| {
                ui.label("Radius top");
                changed |= ui
                    .add(egui::DragValue::new(radius_top).speed(0.02).range(0.0..=1000.0))
                    .changed();
            });
            changed |= float_row(ui, "Depth", depth, 0.02);
        }
        Primitive::Torus { major_segments, minor_segments, major_radius, minor_radius } => {
            changed |= int_row(ui, "Major segments", major_segments, 3..=256);
            changed |= int_row(ui, "Minor segments", minor_segments, 3..=64);
            changed |= float_row(ui, "Major radius", major_radius, 0.02);
            changed |= float_row(ui, "Minor radius", minor_radius, 0.02);
        }
        Primitive::Wall { length, height, thickness } => {
            changed |= float_row(ui, "Length", length, 0.02);
            changed |= float_row(ui, "Height", height, 0.02);
            changed |= float_row(ui, "Thickness", thickness, 0.005);
        }
        Primitive::Floor { width, depth, thickness } => {
            if shaped {
                ui.label(
                    egui::RichText::new(
                        "Footprint follows the walls it was created from \
                         (scale with S).",
                    )
                    .weak()
                    .size(11.0),
                );
            } else {
                changed |= float_row(ui, "Width", width, 0.02);
                changed |= float_row(ui, "Depth", depth, 0.02);
            }
            changed |= float_row(ui, "Thickness", thickness, 0.005);
        }
        Primitive::Empty { size } => {
            changed |= float_row(ui, "Size", size, 0.02);
            ui.label(
                egui::RichText::new(
                    "A marker / grouping parent — never collides or simulates.",
                )
                .weak()
                .size(11.0),
            );
        }
        Primitive::Light { kind, color, intensity, spot_angle_deg, shadows } => {
            ui.horizontal(|ui| {
                ui.label("Type");
                for k in modeler_core::LightKind::ALL {
                    if ui.selectable_label(*kind == k, k.label()).clicked() && *kind != k {
                        *kind = k;
                        changed = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Color");
                changed |= ui.color_edit_button_rgb(color).changed();
            });
            changed |= slider_row(ui, "Intensity", intensity, 0.0..=20.0);
            if *kind == modeler_core::LightKind::Spot {
                changed |= slider_row(ui, "Spot angle °", spot_angle_deg, 5.0..=160.0);
            }
            match kind {
                modeler_core::LightKind::Point => {
                    ui.label(
                        egui::RichText::new("Point lights cannot cast shadows.")
                            .weak()
                            .size(11.0),
                    );
                }
                _ => {
                    changed |= ui.checkbox(shadows, "Cast shadows").changed();
                }
            }
            ui.label(
                egui::RichText::new(
                    "Sun and Spot shine along the object's -Z axis (rotate to \
                     aim). Lights show in Shaded mode with Scene lighting.",
                )
                .weak()
                .size(11.0),
            );
        }
    }
    changed
}

/// Door/window openings on a wall: per-cutout rows plus add buttons.
/// Returns true when anything changed (the caller writes back and bumps
/// `mesh_revision` so the render/physics caches rebuild).
fn wall_cutout_rows(
    ui: &mut egui::Ui,
    cutouts: &mut Vec<modeler_core::WallCutout>,
    length: f32,
    height: f32,
) -> bool {
    let mut changed = false;
    let mut remove: Option<usize> = None;
    for (i, cutout) in cutouts.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            if ui.small_button("✖").on_hover_text("Remove this opening").clicked() {
                remove = Some(i);
            }
            ui.label(
                egui::RichText::new(if cutout.is_door() {
                    format!("Door {}", i + 1)
                } else {
                    format!("Window {}", i + 1)
                })
                .weak()
                .size(11.0),
            );
        });
        for (label, value, speed) in [
            ("Offset", &mut cutout.offset, 0.02),
            ("Width", &mut cutout.width, 0.02),
            ("Bottom", &mut cutout.bottom, 0.02),
            ("Height", &mut cutout.height, 0.02),
        ] {
            ui.horizontal(|ui| {
                ui.label(label);
                changed |= ui
                    .add(egui::DragValue::new(value).speed(speed).range(0.0..=1000.0))
                    .changed();
            });
        }
    }
    if let Some(i) = remove {
        cutouts.remove(i);
        changed = true;
    }
    ui.horizontal(|ui| {
        if ui
            .button("+ Door")
            .on_hover_text("Add a 0.9 × 2.1 m door opening at the wall center")
            .clicked()
        {
            cutouts.push(modeler_core::WallCutout::door(0.5 * length, length, height));
            changed = true;
        }
        if ui
            .button("+ Window")
            .on_hover_text("Add a 1.2 × 1.2 m window opening at the wall center")
            .clicked()
        {
            cutouts.push(modeler_core::WallCutout::window(
                0.5 * length,
                1.5,
                length,
                height,
            ));
            changed = true;
        }
    });
    changed
}
