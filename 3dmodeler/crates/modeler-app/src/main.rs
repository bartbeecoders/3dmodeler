//! 3D Modeler application.
//!
//! Blender-style modeler: box3d picking, modal G/R/S transforms, menu bar,
//! outliner and properties sidebar. Every object has a static body in a
//! b3World; clicks select via b3World_CastRayClosest.

#![recursion_limit = "256"]

mod add_menu;
mod ai;
mod axis_widget;
#[cfg(not(target_arch = "wasm32"))]
mod blend;
mod clipboard;
mod commands;
#[cfg(not(target_arch = "wasm32"))]
mod control;
mod camera;
mod camera_render;
mod context_menu;
#[cfg(not(target_arch = "wasm32"))]
mod render_preview;
mod cutout_handles;
mod force_handles;
mod rope_handles;
mod cloth_handles;
mod edit_mode;
mod gfx;
mod grid;
mod library;
mod pbr_library;
mod mesh_edit;
mod modal;
mod modifiers;
mod net;
mod object_ops;
mod io;
mod overlay;
mod pdf;
mod physics;
mod pie;
mod poke;
mod preview;
mod ref_image;
mod ref_setup;
mod roof_tool;
mod scene_render;
mod selection;
mod settings;
mod theme;
mod ui;
mod undo;
mod terrain_sculpt;
mod texture_bridge;
mod wall_tool;
mod wire_render;

use crate::gfx::*;
use camera::BlenderCamera;
use modeler_core::glam;
use modeler_core::Scene;
use selection::Selection;

fn info(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&msg.into());
    #[cfg(not(target_arch = "wasm32"))]
    println!("{msg}");
}

/// box3d's printf output lands here on wasm (see box3d-sys/shims/wasm_shims.c).
#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn js_log(ptr: *const u8, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    info(&format!("[box3d] {}", String::from_utf8_lossy(bytes)));
}

fn cg(v: glam::Vec3) -> Vec3 {
    vec3(v.x, v.y, v.z)
}

/// The renderer's readback, as the screenshot encoders want it.
///
/// `read_output` hands back tightly packed bytes; everything that encodes a
/// screenshot takes pixels.
fn rgba_pixels(bytes: &[u8]) -> Vec<[u8; 4]> {
    bytes.chunks_exact(4).map(|c| [c[0], c[1], c[2], 255]).collect()
}

/// Bounding sphere of the current selection (center, radius).
pub fn selection_bounds(scene: &Scene, selection: &Selection) -> Option<(glam::Vec3, f32)> {
    let objects: Vec<_> = scene
        .objects()
        .iter()
        .filter(|o| selection.is_selected(o.id))
        .collect();
    if objects.is_empty() {
        return None;
    }
    let center =
        objects.iter().map(|o| o.transform.location).sum::<glam::Vec3>() / objects.len() as f32;
    let radius = objects
        .iter()
        .map(|o| {
            let max_scale = o.transform.scale.abs().max_element().max(1e-6);
            (o.transform.location - center).length() + o.bounding_radius() * max_scale
        })
        .fold(0.0f32, f32::max);
    Some((center, radius))
}

pub fn main() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();
    clipboard::init();

    #[cfg(target_arch = "wasm32")]
    let window = Window::new(WindowSettings {
        title: "3D Modeler".to_string(),
        ..Default::default()
    })
    .unwrap();
    #[cfg(target_arch = "wasm32")]
    let context = window.gl();

    let mut camera = BlenderCamera::new();
    let mut scene = Scene::default_scene();
    let mut scene_render = scene_render::SceneRender::new();
    let mut physics = physics::PhysicsMirror::new();
    let mut sel = Selection::default();
    let mut add_menu = add_menu::AddMenu::new();
    let mut modal = modal::ModalTransform::new();
    let mut delete_tool = object_ops::DeleteTool::new();
    let mut cutout_handles = cutout_handles::CutoutHandles::new();
    let mut force_handles = force_handles::ForceHandles::new();
    let mut rope_handles = rope_handles::RopeHandles::new();
    let mut cloth_handles = cloth_handles::ClothHandles::new();
    let mut poke_tool = poke::PokeTool::new();
    let mut ui_state = ui::UiState::new();
    // dev/test hook: start with the AI panel open (used by UI verification)
    #[cfg(not(target_arch = "wasm32"))]
    if std::env::var("MODELER_AI_PANEL").is_ok() {
        ui_state.chat_panel.open = true;
    }
    let mut undo = undo::UndoStack::new(&scene);
    let mut measure = overlay::MeasureTool::new();
    let mut wall_tool = wall_tool::WallTool::new();
    let mut roof_tool = roof_tool::RoofTool::new();
    let mut sculpt_tool = terrain_sculpt::SculptTool::new();
    let mut texture_bridge = texture_bridge::TextureBridge::new();
    let mut edit_mode = edit_mode::EditMode::new();
    let mut ref_render = ref_image::RefImageRender::new();
    let mut calibrate = ref_image::CalibrateTool::new();
    let mut marker_tool = ref_image::MarkerTool::new();
    let mut image_move = ref_image::ImageMoveTool::new();
    let mut settings = settings::Settings::load();
    let mut saved_settings = settings.clone();
    let mut library = library::load();
    let mut library_saved_revision = library.revision();
    let mut snap_to_grid = false;
    let mut snap_to_vertex = false;
    let mut shade_mode = scene_render::ShadeMode::MaterialPreview;
    let mut xray = false;
    // F12 toggles live camera preview (native: separate OS window; wasm: egui).
    let mut camera_live = false;
    // True after the live preview window has been shown at least once this
    // session — used to detect the user closing the OS window.
    #[cfg(not(target_arch = "wasm32"))]
    let mut live_window_started = false;
    #[cfg(not(target_arch = "wasm32"))]
    let mut render_preview = render_preview::RenderPreview::new();
    // Reused off-screen targets for live camera rendering.
    let mut camera_rt: Option<camera_render::CameraRenderTarget> = None;
    // Separate target for MCP `render` calls so agent-chosen resolutions
    // don't thrash the live preview's.
    #[cfg(not(target_arch = "wasm32"))]
    let mut mcp_camera_rt: Option<camera_render::CameraRenderTarget> = None;
    let mut wire_render = wire_render::WireRender::new();
    #[cfg(not(target_arch = "wasm32"))]
    let mut control = control::ControlServer::start();
    let mut chat = ai::ChatSession::new();

    info("box3d physics mirror created");

    // studio rig + scene lights, switched by the lighting mode
    let mut lights = scene_render::SceneLights::new();

    let mut egui_kb_last_frame = false;

    // The interface and the drawing surface are passed in rather than captured:
    // both are created after the window exists, which on winit 0.30 is inside
    // the event loop, while everything above is ready before it starts.
    //
    // Moved into the loop, which owns it and calls it once per frame.
    let on_frame = move |mut frame_input: FrameInput,
                         gui: &mut Gui,
                         viewport: &mut Viewport3d,
                         paint: &mut FramePaint|
     -> FrameOutput {
        edit_mode.sync(&mut scene);
        // claim Tab for edit mode BEFORE egui grabs it for widget-focus
        // traversal; when a text field had focus last frame, egui keeps it.
        // Dialogs keep Tab too — there it walks the dialog's fields.
        let dialog_open = ui_state.any_dialog_open()
            || calibrate.measured().is_some()
            || marker_tool.active()
            || sculpt_tool.active();
        let mut tab_pressed = false;
        if !egui_kb_last_frame && !dialog_open {
            for event in frame_input.events.iter_mut() {
                if let Event::KeyPress { kind: Key::Tab, handled, .. } = event {
                    if !*handled {
                        tab_pressed = true;
                        *handled = true;
                    }
                }
            }
        }
        let modal_status = edit_mode
            .status_line()
            .or_else(|| modal.status_line())
            .or_else(|| image_move.status_line())
            .or_else(|| wall_tool.status_line(settings.unit))
            .or_else(|| roof_tool.status_line(settings.unit))
            .or_else(|| sculpt_tool.status_line(settings.unit));
        let modal_guides = modal.guides();
        let edit_overlay = edit_mode.overlay(&scene);
        // edit-mode element selection, for "set pivot/anchor to selection"
        let edit_point = edit_mode.active_object().zip(edit_mode.selected_point());
        if shade_mode == scene_render::ShadeMode::Wireframe {
            wire_render.sync(&scene, &sel);
        }
        let fps = 1000.0 / frame_input.elapsed_time.max(0.001) as f32;
        #[cfg(not(target_arch = "wasm32"))]
        let mcp_status = Some(control.as_ref().map(|c| c.status()));
        #[cfg(target_arch = "wasm32")]
        let mcp_status: Option<Option<ui::McpStatus>> = None;
        // paste (Ctrl+V) into focused text fields: three-d never reads the
        // OS clipboard, so bridge it into a Text event before egui runs
        clipboard::inject_paste(
            &mut frame_input.events,
            gui.context().egui_wants_keyboard_input(),
        );
        let mut pointer_over_ui = false;
        gui.update(
            &mut frame_input.events,
            frame_input.accumulated_time,
            frame_input.viewport,
            frame_input.device_pixel_ratio,
            |gui_context| {
                let layout = ui_state.draw(
                    gui_context,
                    &mut scene,
                    &mut sel,
                    &mut camera,
                    &mut modal,
                    &mut physics,
                    &mut undo,
                    &mut measure,
                    &mut calibrate,
                    &mut marker_tool,
                    &mut settings,
                    &mut library,
                    edit_point,
                    edit_mode.active().then_some(&mut edit_mode),
                    &mut wall_tool,
                    &mut roof_tool,
                    &mut sculpt_tool,
                    &mut snap_to_grid,
                    &mut snap_to_vertex,
                    &mut shade_mode,
                    &mut xray,
                    &modal_status,
                    fps,
                    mcp_status,
                    &mut chat,
                );

                // overlays never draw over the menu bar / sidebar / status bar
                let overlay_clip = overlay::viewport_clip(gui_context, &layout);
                overlay::draw(
                    gui_context,
                    &camera,
                    frame_input.viewport,
                    frame_input.device_pixel_ratio,
                    overlay_clip,
                    &scene,
                    &sel,
                    &measure,
                    &calibrate,
                    &marker_tool,
                    settings.unit,
                );
                // grab handles on the openings of selected walls
                if physics.is_stopped()
                    && !modal.active()
                    && !edit_mode.active()
                    && !wall_tool.active()
                    && !roof_tool.active()
                {
                    cutout_handles.draw(
                        gui_context,
                        &scene,
                        &sel,
                        &camera,
                        frame_input.viewport,
                        frame_input.device_pixel_ratio,
                        settings.unit,
                    );
                }
                // initial-force arrows (edit-time; hidden while simulating)
                if physics.is_stopped() && !edit_mode.active() {
                    force_handles.draw(
                        gui_context,
                        &scene,
                        &sel,
                        &camera,
                        frame_input.viewport,
                        frame_input.device_pixel_ratio,
                        overlay_clip,
                    );
                    rope_handles.draw(
                        gui_context,
                        &scene,
                        &sel,
                        &camera,
                        frame_input.viewport,
                        frame_input.device_pixel_ratio,
                        overlay_clip,
                    );
                    cloth_handles.draw(
                        gui_context,
                        &scene,
                        &sel,
                        &camera,
                        frame_input.viewport,
                        frame_input.device_pixel_ratio,
                        overlay_clip,
                    );
                }
                if let Some(guides) = &modal_guides {
                    overlay::draw_modal_guides(
                        gui_context,
                        &camera,
                        frame_input.viewport,
                        frame_input.device_pixel_ratio,
                        overlay_clip,
                        guides,
                    );
                }
                if let Some(edit) = &edit_overlay {
                    overlay::draw_edit_mode(
                        gui_context,
                        &camera,
                        frame_input.viewport,
                        frame_input.device_pixel_ratio,
                        overlay_clip,
                        edit,
                    );
                }
                if let Some(message) = add_menu.ui(
                    gui_context,
                    &mut scene,
                    &mut sel,
                    &mut wall_tool,
                    &mut roof_tool,
                    &settings,
                ) {
                    ui_state.status_message = Some(message);
                }
                delete_tool.ui(gui_context, &mut scene, &mut sel);
                sculpt_tool.panel(gui_context, &mut scene, layout.top_offset);
                axis_widget::axis_widget(
                    gui_context,
                    &mut camera,
                    layout.right_offset,
                    layout.top_offset,
                );
                axis_widget::view_label(gui_context, &camera, layout.left_offset, layout.top_offset);
                poke_tool.draw(gui_context);

                // Blender-style operator status while transforming
                if let Some(status) = &modal_status {
                    let screen = gui_context.content_rect();
                    egui::Area::new(egui::Id::new("modal-status"))
                        .fixed_pos(egui::pos2(
                            screen.left() + layout.left_offset + 12.0,
                            screen.top() + layout.top_offset + 30.0,
                        ))
                        .order(egui::Order::Foreground)
                        .interactable(false)
                        .show(gui_context, |ui| {
                            let color = ui.visuals().warn_fg_color;
                            ui.label(
                                egui::RichText::new(status).size(13.0).color(color),
                            );
                        });
                }

                // plain clicks on egui widgets are NOT flagged handled by
                // three-d (only drags are), so track hover ourselves.
                // is_pointer_over_egui() misses the (deprecated-API) panels
                // in egui 0.34, so also test against the chrome rects —
                // otherwise clicks in the sidebar leak through to the
                // viewport and clear the selection the sidebar just made.
                pointer_over_ui = gui_context.is_pointer_over_egui();
                if let Some(pos) = gui_context.input(|i| i.pointer.latest_pos()) {
                    let screen = gui_context.content_rect();
                    pointer_over_ui |= pos.x > screen.right() - layout.right_offset
                        || pos.x < screen.left() + layout.left_offset
                        || pos.y < screen.top() + layout.top_offset
                        || pos.y > screen.bottom() - layout.bottom_offset;
                }
            },
        );

        if settings != saved_settings {
            settings.save();
            saved_settings = settings.clone();
        }

        // a library asset dragged into the viewport lands here: place it on
        // the picked surface (or the z=0 grid plane) under the cursor
        if let Some(drop) = ui_state.library_panel.take_drop() {
            if !physics.is_stopped() {
                ui_state.status_message =
                    Some("stop the simulation before placing library items".into());
            } else if let Some(asset) = library.asset(drop.asset_id).cloned() {
                // egui gives logical top-left coords; pick rays want physical
                // bottom-left (see camera::pick_ray)
                let dpr = frame_input.device_pixel_ratio;
                let x_px = drop.pos.x * dpr;
                let y_px = frame_input.viewport.height as f32 - drop.pos.y * dpr;
                physics.sync(&scene); // ray needs a current mirror
                let (origin, direction) =
                    camera.pick_ray(frame_input.viewport, x_px, y_px);
                let ray_origin = glam::Vec3::new(origin.x, origin.y, origin.z);
                let ray_dir = glam::Vec3::new(direction.x, direction.y, direction.z);
                let point = physics
                    .pick_point(ray_origin, ray_dir)
                    .unwrap_or(glam::Vec3::ZERO);
                // dropped ONTO an object: the asset's anchor lands on the hit
                // point and the asset attaches (parents) there; dropped on
                // empty ground: the pivot lands on the drop point
                let hit_object = physics.pick(ray_origin, ray_dir);
                let reference = if hit_object.is_some() { asset.anchor } else { asset.pivot };
                let new_ids = modeler_core::library::instantiate(
                    &mut scene,
                    &asset,
                    point - reference,
                );
                if let Some(target) = hit_object {
                    let roots: Vec<_> = new_ids
                        .iter()
                        .copied()
                        .filter(|&id| {
                            scene.object(id).is_some_and(|o| o.parent.is_none())
                        })
                        .collect();
                    for root in roots {
                        scene.set_parent(root, Some(target));
                    }
                }
                let active = new_ids.first().copied();
                sel.set(new_ids, active);
                ui_state.status_message = Some(match hit_object {
                    Some(target) => format!(
                        "placed '{}' on '{}'",
                        asset.name,
                        scene.object(target).map(|o| o.name.as_str()).unwrap_or("?")
                    ),
                    None => format!("placed '{}'", asset.name),
                });
            }
        }

        // persist library changes (sidebar edits or MCP commands)
        if library.revision() != library_saved_revision {
            library::save(&library);
            library_saved_revision = library.revision();
        }

        // PBR material library: finish downloads, then apply to selection
        ui_state.pbr_library_panel.poll();
        ui_state.pbr_library_panel.expand_poly_files();
        if let Some(msg) = ui_state.pbr_library_panel.take_status() {
            ui_state.status_message = Some(msg);
        }
        if let Some((material, name)) = ui_state.pbr_library_panel.take_apply() {
            // Edit mode with a face selected: the material goes to that one
            // face instead of the whole object.
            if edit_mode.active() {
                ui_state.status_message = Some(if edit_mode.selected_face().is_none() {
                    "select a face to apply a PBR material in edit mode".into()
                } else if edit_mode.apply_material_to_selected_face(&mut scene, material) {
                    format!("applied PBR material '{name}' to the selected face")
                } else {
                    "couldn't apply the material to the selected face".into()
                });
            } else {
                let targets: Vec<_> = sel.selected().to_vec();
                if targets.is_empty() {
                    ui_state.status_message =
                        Some("select an object before applying a PBR material".into());
                } else {
                    for id in &targets {
                        if scene.object(*id).is_some_and(|o| o.primitive.is_gizmo()) {
                            continue;
                        }
                        // Textured PBR is stored on the inline material; break any
                        // master link so maps aren't lost under master resolve.
                        let _ = scene.make_material_unique(*id);
                        scene.set_object_material(*id, material.clone());
                    }
                    ui_state.status_message =
                        Some(format!("applied PBR material '{name}' to selection"));
                }
            }
        }

        // did egui consume the keyboard this frame (e.g. focused text field)?
        // (Tab was pre-claimed above, so exclude it from the heuristic)
        let egui_owns_keyboard = frame_input.events.iter().any(|e| {
            matches!(e, Event::KeyPress { handled: true, kind, .. } if *kind != Key::Tab)
        });
        egui_kb_last_frame = egui_owns_keyboard;

        // Ctrl+S save / Ctrl+O open / Ctrl+N new / Ctrl+Z undo /
        // Ctrl+Shift+Z or Ctrl+Y redo (note: physical key position on web —
        // AZERTY users can use the File/Edit menus instead)
        if physics.is_stopped() && !modal.active() {
            for event in frame_input.events.iter_mut() {
                if let Event::KeyPress { kind, modifiers, handled } = event {
                    if *handled || !modifiers.ctrl || egui_owns_keyboard {
                        continue;
                    }
                    match kind {
                        Key::Z if modifiers.shift => {
                            undo.redo(&mut scene);
                            *handled = true;
                        }
                        Key::Z => {
                            undo.undo(&mut scene);
                            *handled = true;
                        }
                        Key::Y => {
                            undo.redo(&mut scene);
                            *handled = true;
                        }
                        Key::S => {
                            ui_state.action_save(&scene, &settings);
                            *handled = true;
                        }
                        Key::O => {
                            ui_state.action_open(&settings);
                            *handled = true;
                        }
                        Key::N => {
                            ui_state.action_new_scene(&mut scene, &mut sel, &mut undo);
                            *handled = true;
                        }
                        _ => {}
                    }
                }
            }
        }

        // parenting shortcuts (Ctrl+P / Alt+P)
        if physics.is_stopped() && !modal.active() {
            for event in frame_input.events.iter_mut() {
                if let Event::KeyPress { kind: Key::P, modifiers, handled } = event {
                    if !*handled && !egui_owns_keyboard {
                        if modifiers.ctrl {
                            ui::parent_selected_to_active(&mut scene, &sel);
                            *handled = true;
                        } else if modifiers.alt {
                            for id in sel.selected().to_vec() {
                                scene.set_parent(id, None);
                            }
                            *handled = true;
                        }
                    }
                }
            }
        }

        // measure tool: consume clicks and Escape while active
        if measure.active {
            for event in frame_input.events.iter_mut() {
                match event {
                    Event::MousePress {
                        button: MouseButton::Left,
                        position,
                        handled,
                        ..
                    } if !*handled && !pointer_over_ui => {
                        physics.sync(&scene); // ray needs a current mirror
                        let (origin, direction) =
                            camera.pick_ray(frame_input.viewport, position.x, position.y);
                        if let Some(point) = physics.pick_point(
                            glam::Vec3::new(origin.x, origin.y, origin.z),
                            glam::Vec3::new(direction.x, direction.y, direction.z),
                        ) {
                            measure.add_point(point, &mut scene);
                        }
                        *handled = true;
                    }
                    Event::KeyPress { kind: Key::Escape, handled, .. } if !*handled => {
                        measure.cancel();
                        *handled = true;
                    }
                    _ => {}
                }
            }
        }

        // reference-image scale calibration: pick 2 points on the image plane
        if calibrate.picking() {
            for event in frame_input.events.iter_mut() {
                match event {
                    Event::MousePress {
                        button: MouseButton::Left,
                        position,
                        handled,
                        ..
                    } if !*handled && !pointer_over_ui => {
                        let (origin, direction) =
                            camera.pick_ray(frame_input.viewport, position.x, position.y);
                        calibrate.add_ray(
                            &scene,
                            glam::Vec3::new(origin.x, origin.y, origin.z),
                            glam::Vec3::new(direction.x, direction.y, direction.z),
                        );
                        *handled = true;
                    }
                    Event::KeyPress { kind: Key::Escape, handled, .. } if !*handled => {
                        calibrate.cancel();
                        *handled = true;
                    }
                    _ => {}
                }
            }
        }

        // AI marker drawing: pick points on the reference image plane;
        // Enter finishes a line/area, Esc cancels
        if marker_tool.picking() {
            for event in frame_input.events.iter_mut() {
                match event {
                    Event::MousePress {
                        button: MouseButton::Left,
                        position,
                        handled,
                        ..
                    } if !*handled && !pointer_over_ui => {
                        let (origin, direction) =
                            camera.pick_ray(frame_input.viewport, position.x, position.y);
                        marker_tool.add_ray(
                            &scene,
                            glam::Vec3::new(origin.x, origin.y, origin.z),
                            glam::Vec3::new(direction.x, direction.y, direction.z),
                        );
                        *handled = true;
                    }
                    Event::KeyPress { kind: Key::Enter, handled, .. }
                        if !*handled && !egui_owns_keyboard =>
                    {
                        marker_tool.finish();
                        *handled = true;
                    }
                    Event::KeyPress { kind: Key::Escape, handled, .. } if !*handled => {
                        marker_tool.cancel();
                        *handled = true;
                    }
                    _ => {}
                }
            }
        }

        // wall tool: click wall segments onto the floor. It owns the mouse
        // and typed input while active, so it runs before the other tools.
        if !physics.is_stopped() && wall_tool.active() {
            wall_tool.abort(&mut scene); // simulation took over mid-draw
        }
        if wall_tool.active() && !edit_mode.active() && !modal.active() {
            wall_tool.handle_events(
                &mut frame_input.events,
                &camera,
                frame_input.viewport,
                &mut scene,
                &mut sel,
                egui_owns_keyboard,
                pointer_over_ui,
                snap_to_grid,
                settings.grid_spacing,
            );
        }

        // roof tool: same contract, drawing a roof footprint rectangle
        if !physics.is_stopped() && roof_tool.active() {
            roof_tool.abort(&mut scene); // simulation took over mid-draw
        }
        if roof_tool.active() && !edit_mode.active() && !modal.active() && !wall_tool.active()
        {
            roof_tool.handle_events(
                &mut frame_input.events,
                &camera,
                frame_input.viewport,
                &mut scene,
                &mut sel,
                egui_owns_keyboard,
                pointer_over_ui,
                snap_to_grid,
                settings.grid_spacing,
            );
        }

        // terrain sculpt brush: owns LMB while active (dabs must never
        // fall through to click-selection)
        if !physics.is_stopped() && sculpt_tool.active() {
            sculpt_tool.abort(&mut scene); // simulation took over mid-stroke
        }
        if sculpt_tool.active()
            && !edit_mode.active()
            && !modal.active()
            && !wall_tool.active()
            && !roof_tool.active()
        {
            sculpt_tool.handle_events(
                &mut frame_input.events,
                &camera,
                frame_input.viewport,
                &mut scene,
                egui_owns_keyboard,
                pointer_over_ui,
                frame_input.elapsed_time as f32 / 1000.0,
            );
        }

        // right-click: context menu on the object (object mode) or the
        // vertex/edge/face (edit mode) under the cursor — set pivot/anchor
        // and common actions. On empty canvas (object mode) it opens the
        // Add wheel instead. Cancel-RMB during modal/grab stays theirs.
        if physics.is_stopped()
            && !modal.active()
            && !edit_mode.grabbing()
            && !wall_tool.active()
            && !roof_tool.active()
            && !sculpt_tool.active()
        {
            for event in frame_input.events.iter_mut() {
                if let Event::MousePress {
                    button: MouseButton::Right,
                    position,
                    handled,
                    ..
                } = event
                {
                    if *handled || pointer_over_ui {
                        continue;
                    }
                    // event coords are physical bottom-left; egui wants
                    // logical top-left
                    let menu_pos = egui::pos2(
                        position.x / frame_input.device_pixel_ratio,
                        (frame_input.viewport.height as f32 - position.y)
                            / frame_input.device_pixel_ratio,
                    );
                    let target = if edit_mode.active() {
                        edit_mode
                            .context_pick(
                                &scene,
                                &camera,
                                frame_input.viewport,
                                position.x,
                                position.y,
                            )
                            .map(|(id, point, label)| context_menu::Target::Element {
                                id,
                                point,
                                label,
                            })
                    } else {
                        physics.sync(&scene); // ray needs a current mirror
                        let (origin, direction) =
                            camera.pick_ray(frame_input.viewport, position.x, position.y);
                        let ray_o = glam::Vec3::new(origin.x, origin.y, origin.z);
                        let ray_d = glam::Vec3::new(direction.x, direction.y, direction.z);
                        physics.pick(ray_o, ray_d).map(|id| {
                            // a grouped assembly is addressed via its root
                            let id = scene.group_root(id).unwrap_or(id);
                            // clicking inside the current selection keeps it
                            // (menu actions apply to the whole selection)
                            if !sel.is_selected(id) {
                                sel.click_expanded(&scene, Some(id), false);
                            }
                            let hit = physics.pick_point(ray_o, ray_d).unwrap_or_default();
                            let hit_local =
                                scene.world_transform(id).inverse_transform_point(hit);
                            context_menu::Target::Object { id, hit_local }
                        })
                    };
                    match target {
                        Some(target) => ui_state.context_menu.open(menu_pos, target),
                        None => {
                            ui_state.context_menu.close();
                            // empty canvas (object mode): offer the Add wheel
                            if !edit_mode.active() {
                                add_menu.open_at(menu_pos);
                            }
                        }
                    }
                    *handled = true;
                }
            }
        }

        // edit mode (Tab): element selection & moves on the active object
        edit_mode.handle_events(
            &mut frame_input.events,
            &camera,
            frame_input.viewport,
            &mut scene,
            &sel,
            egui_owns_keyboard,
            pointer_over_ui,
            tab_pressed,
            physics.is_stopped(),
            settings.unit,
        );

        // Space = play/pause, Esc = stop (when not editing)
        if !modal.active() && !edit_mode.active() && !wall_tool.active() && !roof_tool.active()
        {
            for event in frame_input.events.iter_mut() {
                if let Event::KeyPress { kind, handled, .. } = event {
                    match kind {
                        Key::Space if !*handled && !egui_owns_keyboard => {
                            match physics.sim_state() {
                                physics::SimState::Playing => physics.pause(),
                                _ => physics.play(&scene),
                            }
                            *handled = true;
                        }
                        Key::Escape
                            if !*handled && physics.sim_state() != physics::SimState::Stopped =>
                        {
                            physics.stop(&mut scene);
                            *handled = true;
                        }
                        _ => {}
                    }
                }
            }
        }

        // G on a selected reference image: move it (same gestures as objects)
        if physics.is_stopped()
            && !edit_mode.active()
            && !modal.active()
            && !wall_tool.active()
            && !roof_tool.active()
            && !sculpt_tool.active()
        {
            image_move.handle_events(
                &mut frame_input.events,
                &camera,
                frame_input.viewport,
                &mut scene,
                sel.image(),
                egui_owns_keyboard,
                settings.unit,
            );
        }

        // editing tools are disabled while the simulation owns the transforms
        // and while edit mode owns the object
        if physics.is_stopped()
            && !edit_mode.active()
            && !image_move.active()
            && !wall_tool.active()
            && !roof_tool.active()
            && !sculpt_tool.active()
        {
            // modal transform operators get first claim on input after the UI
            modal.handle_events(
                &mut frame_input.events,
                &camera,
                frame_input.viewport,
                &mut scene,
                &mut sel,
                egui_owns_keyboard,
                snap_to_grid,
                snap_to_vertex,
                settings.grid_spacing,
                settings.unit,
            );
        }

        ui_state.handle_events(&mut frame_input.events, egui_owns_keyboard, pointer_over_ui);

        if !modal.active()
            && physics.is_stopped()
            && !edit_mode.active()
            && !wall_tool.active()
            && !roof_tool.active()
            && !sculpt_tool.active()
        {
            delete_tool.handle_events(
                &mut frame_input.events,
                frame_input.viewport,
                frame_input.device_pixel_ratio,
                egui_owns_keyboard,
                &mut scene,
                &mut sel,
            );
            add_menu.handle_events(
                &mut frame_input.events,
                frame_input.viewport,
                frame_input.device_pixel_ratio,
            );
        }
        // context wheel (also available in edit mode): consume clicks/Esc so
        // a commit click never falls through to the picking below
        ui_state.context_menu.handle_events(&mut frame_input.events);

        // physics mode: hold LMB to charge, release to kick the object under
        // the cursor (consumes the click so it never changes the selection)
        if let Some(message) = poke_tool.handle_events(
            &mut frame_input.events,
            &mut physics,
            &camera,
            frame_input.viewport,
            pointer_over_ui,
        ) {
            ui_state.status_message = Some(message);
        }
        poke_tool.update(frame_input.elapsed_time as f32 / 1000.0, &physics);

        // wall opening handles: grab/drag doors & windows of selected walls
        if !modal.active()
            && physics.is_stopped()
            && !edit_mode.active()
            && !wall_tool.active()
            && !roof_tool.active()
            && !sculpt_tool.active()
        {
            cutout_handles.handle_events(
                &mut frame_input.events,
                &mut scene,
                &sel,
                &camera,
                frame_input.viewport,
                frame_input.device_pixel_ratio,
                pointer_over_ui,
            );
            force_handles.handle_events(
                &mut frame_input.events,
                &mut scene,
                &sel,
                &camera,
                frame_input.viewport,
                frame_input.device_pixel_ratio,
                pointer_over_ui,
            );
            rope_handles.handle_events(
                &mut frame_input.events,
                &mut scene,
                &sel,
                &physics,
                &camera,
                frame_input.viewport,
                frame_input.device_pixel_ratio,
                pointer_over_ui,
            );
            cloth_handles.handle_events(
                &mut frame_input.events,
                &mut scene,
                &sel,
                &physics,
                &camera,
                frame_input.viewport,
                frame_input.device_pixel_ratio,
                pointer_over_ui,
            );
        } else {
            cutout_handles.cancel();
            force_handles.cancel();
            rope_handles.cancel();
            cloth_handles.cancel();
        }

        // external control API (MCP): execute queued agent commands
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(control) = control.as_mut() {
            control.poll(
                &mut scene,
                &mut sel,
                &mut physics,
                &mut library,
                &mut shade_mode,
                &mut camera,
            );
        }

        // AI assistant: deliver finished responses, run requested tools
        chat.poll(
            &mut settings,
            ai::ToolContext {
                scene: &mut scene,
                selection: &mut sel,
                physics: &mut physics,
                library: &mut library,
                shade_mode: &mut shade_mode,
            },
        );

        // step the simulation (writes transforms back into the scene)
        physics.update(&mut scene, frame_input.elapsed_time as f32 / 1000.0);

        // design-mode: keep attached rope ends on their targets when those
        // objects move (skip while dragging a rope handle or simulating)
        if physics.is_stopped()
            && !rope_handles.dragging()
            && !cloth_handles.dragging()
            && !modal.active()
        {
            rope_handles::sync_attached_ropes(&mut scene);
            // Keep pinned cloth near their pin targets in design mode
            cloth_handles::sync_pinned_cloths(&mut scene);
        }

        // physics mirror must be current before picking (no-op while playing)
        physics.sync(&scene);
        sel.retain_existing(|id| scene.object(id).is_some());
        if sel
            .image()
            .is_some_and(|id| !scene.reference_images().iter().any(|r| r.id == id))
        {
            sel.clear_image();
        }

        // batch this frame's edits into undo checkpoints once things go quiet
        undo.on_frame(
            &scene,
            modal.active()
                || edit_mode.grabbing()
                || wall_tool.drawing()
                || roof_tool.drawing()
                || sculpt_tool.stroking()
                || cutout_handles.dragging()
                || force_handles.dragging()
                || rope_handles.dragging()
                || cloth_handles.dragging()
                || !physics.is_stopped(),
        );

        // overlap warning while placing (grab/rotate/scale active)
        let overlaps = if modal.active() {
            physics.overlapping(sel.selected())
        } else {
            std::collections::HashSet::new()
        };

        // boolean eyedropper: Esc disarms it, and it never outlives the
        // modifier it was armed on (deleted object, removed modifier)
        if let Some((target, index)) = ui_state.pick_boolean_tool {
            let still_valid = scene
                .object(target)
                .and_then(|o| o.modifiers.get(index))
                .is_some_and(|m| matches!(m.kind, modeler_core::ModifierKind::Boolean { .. }));
            if !still_valid {
                ui_state.pick_boolean_tool = None;
            } else {
                for event in frame_input.events.iter_mut() {
                    if let Event::KeyPress { kind: Key::Escape, handled, .. } = event {
                        if !*handled {
                            ui_state.pick_boolean_tool = None;
                            ui_state.status_message = Some("tool pick cancelled".to_string());
                            *handled = true;
                        }
                    }
                }
            }
        }

        // viewport click selection (box3d ray cast) — object mode only
        for event in frame_input.events.iter_mut() {
            if edit_mode.active() {
                break;
            }
            if let Event::MousePress {
                button: MouseButton::Left,
                position,
                modifiers,
                handled,
            } = event
            {
                if !*handled && !pointer_over_ui {
                    let (origin, direction) =
                        camera.pick_ray(frame_input.viewport, position.x, position.y);
                    let ray_o = glam::Vec3::new(origin.x, origin.y, origin.z);
                    let ray_d = glam::Vec3::new(direction.x, direction.y, direction.z);
                    let hit = physics.pick(ray_o, ray_d);
                    // boolean eyedropper armed in the Modifiers panel: this
                    // click assigns the tool object instead of selecting
                    if let Some((target, index)) = ui_state.pick_boolean_tool {
                        *handled = true;
                        match modifiers::pick_boolean_tool(&mut scene, target, index, hit) {
                            Ok(message) => {
                                ui_state.pick_boolean_tool = None;
                                ui_state.status_message = Some(message);
                            }
                            // stay armed on a miss or an invalid pick
                            Err(message) => ui_state.status_message = Some(message),
                        }
                        continue;
                    }
                    // reference images are not physics bodies: intersect
                    // them analytically and let the nearest hit win
                    let object_t = hit
                        .and_then(|_| physics.pick_point(ray_o, ray_d))
                        .map(|p| (p - ray_o).length());
                    let image_hit = scene
                        .reference_images()
                        .iter()
                        .filter(|r| r.visible)
                        .filter_map(|r| r.intersect_ray(ray_o, ray_d).map(|t| (t, r.id)))
                        .min_by(|a, b| a.0.total_cmp(&b.0));
                    let image_in_front = match (object_t, image_hit) {
                        (Some(ot), Some((it, _))) => it < ot,
                        (None, Some(_)) => true,
                        _ => false,
                    };
                    if image_in_front && !modifiers.shift {
                        sel.select_image(image_hit.unwrap().1);
                    } else {
                        // grouped assemblies (placed library objects) select as one
                        sel.click_expanded(&scene, hit, modifiers.shift);
                    }
                    *handled = true;
                }
            }
        }

        let logical_height = frame_input.viewport.height as f32 / frame_input.device_pixel_ratio;
        camera.handle_events(&mut frame_input.events, logical_height, pointer_over_ui);

        // '.' frames the selection (and re-pivots the orbit on it); Home
        // frames all; End drops the selection onto the ground (z = 0) or
        // the objects below it, whichever is higher. F12 renders from a
        // scene camera (Blender convention).
        for event in frame_input.events.iter_mut() {
            if let Event::KeyPress {
                kind,
                handled,
                ..
            } = event
            {
                if *handled {
                    continue;
                }
                match kind {
                    Key::Period => {
                        let bounds =
                            selection_bounds(&scene, &sel).or_else(|| scene.bounds());
                        if let Some((center, radius)) = bounds {
                            camera.frame(cg(center), radius);
                        }
                    }
                    Key::Home => {
                        if let Some((center, radius)) = scene.bounds() {
                            camera.frame(cg(center), radius);
                        }
                    }
                    Key::End
                        if physics.is_stopped()
                            && !edit_mode.active()
                            && !egui_owns_keyboard =>
                    {
                        if let Some(image_id) = sel.image() {
                            // ground the reference image: lowest corner to z=0
                            // (locked images stay put)
                            let min_z = scene
                                .reference_images()
                                .iter()
                                .find(|r| r.id == image_id && !r.locked)
                                .map(|r| {
                                    r.corners()
                                        .iter()
                                        .map(|c| c.z)
                                        .fold(f32::INFINITY, f32::min)
                                });
                            if let Some(min_z) = min_z.filter(|z| z.is_finite()) {
                                if let Some(image) = scene.reference_image_mut(image_id) {
                                    image.location.z -= min_z;
                                }
                            }
                        } else {
                            physics.sync(&scene); // rays need a current mirror
                            physics.drop_to_floor(&mut scene, &sel);
                        }
                    }
                    Key::F12 if !egui_owns_keyboard => {
                        camera_live = !camera_live;
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            if !camera_live {
                                render_preview.close();
                                live_window_started = false;
                            }
                        }
                        #[cfg(target_arch = "wasm32")]
                        if !camera_live {
                            ui_state.clear_render_result();
                        }
                        ui_state.status_message = Some(if camera_live {
                            "Live camera view ON — updates in real time (F12 to close)"
                                .into()
                        } else {
                            "Live camera view OFF".into()
                        });
                        *handled = true;
                    }
                    _ => {}
                }
            }
        }

        let gpu = viewport.renderer().gpu.clone();
        scene_render.sync(
            &gpu,
            viewport.renderer_mut(),
            &mut texture_bridge,
            &scene,
            &sel,
            &overlaps,
            shade_mode,
            xray,
        );
        lights.sync(&mut scene_render.scene, &scene, shade_mode);
        gfx::viewport::sync_exposure(viewport.renderer_mut(), lights.scene_active());

        // Live camera view: re-render every frame while active so the
        // preview tracks camera/scene changes. Closing the OS window (✕),
        // or F12/Esc in that window, stops the stream and must not reopen.
        #[cfg(not(target_arch = "wasm32"))]
        if camera_live && live_window_started && !render_preview.is_open() {
            camera_live = false;
            live_window_started = false;
            ui_state.status_message = Some("Live camera view closed".into());
        }

        if camera_live {
            match camera_render::resolve_camera(
                &scene,
                sel.selected().iter().copied(),
                sel.active(),
            ) {
                Some(cam_id) => {
                    let (live_w, live_h) = (
                        camera_render::LIVE_WIDTH,
                        camera_render::LIVE_HEIGHT,
                    );
                    let rt = camera_rt.get_or_insert_with(|| {
                        camera_render::CameraRenderTarget::new(&gpu, live_w, live_h)
                    });
                    rt.ensure_size(live_w, live_h);
                    rt.sync_exposure(lights.scene_active());
                    match rt.render(&scene, cam_id, &mut scene_render) {
                        Ok((w, h, rgba)) => {
                            let name = scene
                                .object(cam_id)
                                .map(|o| o.name.clone())
                                .unwrap_or_else(|| "Camera".into());
                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                if !live_window_started {
                                    // First frame after F12: create the OS window.
                                    render_preview.open(name, w, h, rgba);
                                    live_window_started = true;
                                } else if !render_preview.push_frame(name, w, h, rgba) {
                                    // User closed the window (✕ / F12 / Esc).
                                    camera_live = false;
                                    live_window_started = false;
                                    ui_state.status_message =
                                        Some("Live camera view closed".into());
                                }
                            }
                            #[cfg(target_arch = "wasm32")]
                            {
                                ui_state.set_render_result(name, w, h, rgba.to_vec());
                            }
                        }
                        Err(e) => {
                            camera_live = false;
                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                render_preview.close();
                                live_window_started = false;
                            }
                            ui_state.status_message =
                                Some(format!("Camera view failed: {e}"));
                        }
                    }
                }
                None => {
                    camera_live = false;
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        render_preview.close();
                        live_window_started = false;
                    }
                    #[cfg(target_arch = "wasm32")]
                    ui_state.clear_render_result();
                    ui_state.status_message = Some(
                        "No camera in the scene — Add ▸ Camera, then press F12".into(),
                    );
                }
            }
        }

        let cam = camera.camera(frame_input.viewport);
        scene_render.scene.camera = gfx::viewport::aether_camera(&cam);

        // -- editor chrome ----------------------------------------------------
        //
        // Rebuilt every frame: the overlay's draw list is not retained, which is
        // what lets the grid follow a settings change and the wireframe follow a
        // drag without either of them owning a cache.
        {
            let overlay = viewport.renderer_mut().overlay_mut();
            overlay.draws.clear();
            ref_render.sync(&scene, &gpu, overlay);

            grid::draw(
                &mut overlay.draws,
                settings.grid_spacing,
                settings.grid_minor_color,
                settings.grid_major_color,
            );
            if let Some(plane) = camera.vertical_axis_plane() {
                grid::draw_zero_lines(&mut overlay.draws, plane);
            }

            if shade_mode == scene_render::ShadeMode::Wireframe {
                let (positions, ranges) = wire_render.lines();
                for (tier, &(first, count)) in ranges.iter().enumerate() {
                    let c = wire_render::WireRender::tier_color(tier);
                    let color = aether_math::Vec4::new(c[0], c[1], c[2], c[3]);
                    for pair in positions[first as usize..(first + count) as usize].chunks_exact(2)
                    {
                        overlay.draws.line(pair[0], pair[1], color);
                    }
                }
            }

            // sculpt brush ring, conforming to the terrain under the cursor
            sculpt_tool.overlay(&scene, &mut overlay.draws);

            // Reference images last: they blend over the grid and the meshes.
            for quad in ref_render.quads() {
                overlay.draws.image(
                    quad.handle,
                    quad.corners,
                    aether_math::Vec4::new(1.0, 1.0, 1.0, quad.opacity),
                );
            }

            overlay.set_selection(scene_render.selection());
            let outline = scene_render::SceneRender::outline_color(&scene, &sel, &overlaps);
            let c = outline.to_linear();
            overlay.outline.color = aether_math::Vec4::new(c.x, c.y, c.z, 1.0);
        }

        viewport.render(&scene_render.scene, frame_input.elapsed_time as f32 * 0.001);

        // The scene fills the window; the interface goes over it.
        viewport.blit(paint.encoder, paint.view);
        gui.render(paint.device, paint.queue, paint.encoder, paint.view);

        // the AI assistant's screenshot tool sees the frame just rendered
        if chat.wants_screenshot() {
            let (w, h) = (frame_input.viewport.width, frame_input.viewport.height);
            let pixels = rgba_pixels(&viewport.read_output());
            chat.deliver_screenshot(&pixels, w, h, &settings);
        }

        // deliver any pending image requests from the control API: viewport
        // screenshots read the frame just drawn, `render` draws off-screen
        // from a scene camera (the F12 view, at an agent-chosen resolution)
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(control) = control.as_mut() {
            let requests: Vec<(serde_json::Value, _)> =
                control.pending_screenshots.drain(..).collect();
            // the frame is read at most once, however many requests queued
            let mut viewport_shot: Option<serde_json::Value> = None;
            for (mut command, reply) in requests {
                // a request that moved the viewport camera waits a frame so
                // the UI overlay matches the 3D view it captures
                if let Some(left) = command["settle_frames"].as_u64() {
                    if left > 0 {
                        command["settle_frames"] = serde_json::json!(left - 1);
                        control.pending_screenshots.push((command, reply));
                        continue;
                    }
                }
                let response = if command["cmd"] == "render" {
                    let width = command["width"].as_u64().unwrap_or(camera_render::LIVE_WIDTH as u64)
                        .clamp(16, 4096) as u32;
                    let height = command["height"]
                        .as_u64()
                        .unwrap_or(camera_render::LIVE_HEIGHT as u64)
                        .clamp(16, 4096) as u32;
                    let camera_id = if command["camera"].is_null() {
                        camera_render::resolve_camera(
                            &scene,
                            sel.selected().iter().copied(),
                            sel.active(),
                        )
                        .ok_or_else(|| {
                            "no camera in the scene — add_object {\"primitive\":\"camera\"} first"
                                .to_string()
                        })
                    } else {
                        commands::resolve(&scene, &command["camera"])
                    };
                    match camera_id {
                        Err(e) => serde_json::json!({"ok": false, "error": e}),
                        Ok(camera_id) => {
                            let rt = mcp_camera_rt.get_or_insert_with(|| {
                                camera_render::CameraRenderTarget::new(&gpu, width, height)
                            });
                            rt.ensure_size(width, height);
                            rt.sync_exposure(lights.scene_active());
                            match rt.render(&scene, camera_id, &mut scene_render) {
                                Err(e) => serde_json::json!({"ok": false, "error": e}),
                                Ok((w, h, rgba)) => match commands::encode_png_rgba(rgba, w, h) {
                                    Err(e) => serde_json::json!({"ok": false, "error": e}),
                                    Ok(png_base64) => serde_json::json!({
                                        "ok": true,
                                        "png_base64": png_base64,
                                        "width": w,
                                        "height": h,
                                        "camera": scene
                                            .object(camera_id)
                                            .map(|o| o.name.clone())
                                            .unwrap_or_default(),
                                    }),
                                },
                            }
                        }
                    }
                } else {
                    viewport_shot
                        .get_or_insert_with(|| {
                            let pixels = rgba_pixels(&viewport.read_output());
                            let (w, h) =
                                (frame_input.viewport.width, frame_input.viewport.height);
                            match commands::encode_screenshot(&pixels, w, h) {
                                Ok(png_base64) => serde_json::json!({
                                    "ok": true,
                                    "png_base64": png_base64,
                                    "width": w,
                                    "height": h,
                                    "view": camera.view_name(),
                                }),
                                Err(e) => serde_json::json!({"ok": false, "error": e}),
                            }
                        })
                        .clone()
                };
                let _ = reply.send(response);
            }
        }

        FrameOutput::default()
    };

    #[cfg(target_arch = "wasm32")]
    window.render_loop(on_frame);

    // native main loop: winit 0.30's application handler, plus OS file drops.
    //
    // The app owns the loop rather than using a render_loop helper because the
    // reference-setup dialog accepts files dropped from the file manager, and
    // that event has to reach the app.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let event_loop = winit::event_loop::EventLoop::new().expect("an event loop");
        let mut app = NativeApp::new(on_frame);
        event_loop.run_app(&mut app).expect("the event loop ran");
    }
}

/// The window, its surface and the interface — everything that cannot exist
/// until winit has handed over a window.
#[cfg(not(target_arch = "wasm32"))]
struct Running {
    window: std::sync::Arc<winit::window::Window>,
    gfx: GfxWindow,
    gui: Gui,
    viewport: Viewport3d,
    input: FrameInputGenerator,
}

/// The native event loop, as winit 0.30 wants it: a handler rather than a
/// closure, because a window may only be created once the platform says so.
#[cfg(not(target_arch = "wasm32"))]
struct NativeApp<F> {
    on_frame: F,
    running: Option<Running>,
    /// Without vsync the GPU never blocks us — the loop paces itself.
    frame_budget: std::time::Duration,
    last_frame: std::time::Instant,
}

#[cfg(not(target_arch = "wasm32"))]
impl<F> NativeApp<F>
where
    F: FnMut(FrameInput, &mut Gui, &mut Viewport3d, &mut FramePaint) -> FrameOutput,
{
    fn new(on_frame: F) -> Self {
        Self {
            on_frame,
            running: None,
            frame_budget: std::time::Duration::from_micros(16_600),
            last_frame: std::time::Instant::now(),
        }
    }

    /// Draws one frame, or skips it when the swapchain is momentarily gone.
    fn redraw(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let Some(running) = self.running.as_mut() else { return };

        let frame_input = running.input.generate();
        let Some(surface_texture) = running.gfx.acquire() else { return };
        let view = surface_texture.texture.create_view(&Default::default());
        let mut encoder = running
            .gfx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });

        let frame_output = {
            let mut paint = FramePaint {
                device: running.gfx.device(),
                queue: running.gfx.queue(),
                encoder: &mut encoder,
                view: &view,
            };
            (self.on_frame)(frame_input, &mut running.gui, &mut running.viewport, &mut paint)
        };
        running.gfx.queue().submit([encoder.finish()]);

        if frame_output.exit {
            event_loop.exit();
            return;
        }
        if frame_output.swap_buffers {
            surface_texture.present();
        }
        if frame_output.wait_next_event {
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
        } else {
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
            running.window.request_redraw();
        }

        if !running.gfx.vsync {
            let elapsed = self.last_frame.elapsed();
            if elapsed < self.frame_budget {
                std::thread::sleep(self.frame_budget - elapsed);
            }
            self.last_frame = std::time::Instant::now();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<F> winit::application::ApplicationHandler for NativeApp<F>
where
    F: FnMut(FrameInput, &mut Gui, &mut Viewport3d, &mut FramePaint) -> FrameOutput,
{
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        // Fired again when a suspended app comes back. The window it already
        // has is still good, so this is only ever a first-run initialisation.
        if self.running.is_some() {
            return;
        }
        let attributes = winit::window::Window::default_attributes()
            .with_title("3D Modeler")
            .with_min_inner_size(winit::dpi::LogicalSize::new(2.0, 2.0))
            .with_maximized(true);
        let window = std::sync::Arc::new(
            event_loop.create_window(attributes).expect("a window"),
        );
        window.focus_window();

        let gfx = GfxWindow::new(window.clone()).unwrap_or_else(|e| {
            // A real dialog: this is where a machine with no usable GPU stops,
            // and someone who double-clicked the executable would never see a
            // console panic.
            let message = format!(
                "The 3D modeler could not start:\n{e}.\n\n\
                 The renderer needs Vulkan, Direct3D 12 or Metal.\n\n\
                 In a virtual machine (VirtualBox, VMware):\n\
                 • enable 3D acceleration in the VM display settings\n\
                 • install the guest additions / VMware tools\n\n\
                 Alternative (software rendering): install Mesa's lavapipe,\n\
                 a software Vulkan driver."
            );
            eprintln!("{message}");
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("3D Modeler — cannot start")
                .set_description(&message)
                .show();
            std::process::exit(1);
        });
        if !gfx.vsync {
            println!("vsync unavailable (VM or remote desktop?) — limiting to ~60 fps");
        }

        let gui = Gui::new(gfx.device(), gfx.format());
        let size = window.inner_size();
        let viewport =
            Viewport3d::new(gfx.gpu().clone(), size.width, size.height, gfx.format());
        let input = FrameInputGenerator::from_winit_window(&window);
        self.running = Some(Running { window, gfx, gui, viewport, input });
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        use winit::event::WindowEvent;

        if let Some(running) = self.running.as_mut() {
            running.input.handle_winit_window_event(&event);
        }
        match event {
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            WindowEvent::Resized(size) => {
                if let Some(running) = self.running.as_mut() {
                    running.gfx.resize(size);
                    // The renderer's targets are its own; the swapchain resizing
                    // does not carry them along.
                    running.viewport.resize(size.width, size.height);
                }
            }
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::DroppedFile(path) => {
                // .blend drops import as scene objects; everything else goes
                // to the reference-image setup as before
                if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("blend")) {
                    blend::import_path(path);
                } else {
                    ref_image::push_setup_file(&path);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
        if let Some(running) = self.running.as_ref() {
            running.window.request_redraw();
        }
    }
}
