//! 3D Modeler application.
//!
//! Blender-style modeler: box3d picking, modal G/R/S transforms, menu bar,
//! outliner and properties sidebar. Every object has a static body in a
//! b3World; clicks select via b3World_CastRayClosest.

mod add_menu;
mod axis_widget;
#[cfg(not(target_arch = "wasm32"))]
mod control;
mod camera;
mod edit_mode;
mod grid;
mod library;
mod modal;
mod object_ops;
mod io;
mod overlay;
mod physics;
mod preview;
mod ref_image;
mod scene_render;
mod selection;
mod settings;
mod ui;
mod undo;

use camera::BlenderCamera;
use modeler_core::glam;
use modeler_core::Scene;
use selection::Selection;
use three_d::*;

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

    let window = Window::new(WindowSettings {
        title: "3D Modeler".to_string(),
        ..Default::default()
    })
    .unwrap();
    let context = window.gl();

    let mut camera = BlenderCamera::new();
    let mut scene = Scene::default_scene();
    let mut scene_render = scene_render::SceneRender::new();
    let mut physics = physics::PhysicsMirror::new();
    let mut sel = Selection::default();
    let mut add_menu = add_menu::AddMenu::new();
    let mut modal = modal::ModalTransform::new();
    let mut delete_tool = object_ops::DeleteTool::new();
    let mut ui_state = ui::UiState::new();
    let mut undo = undo::UndoStack::new(&scene);
    let mut measure = overlay::MeasureTool::new();
    let mut edit_mode = edit_mode::EditMode::new();
    let mut ref_render = ref_image::RefImageRender::new();
    let mut calibrate = ref_image::CalibrateTool::new();
    let mut settings = settings::Settings::load();
    let mut saved_settings = settings.clone();
    let mut library = library::load();
    let mut library_saved_revision = library.revision();
    let mut snap_to_grid = false;
    let mut snap_to_vertex = false;
    let mut shade_mode = scene_render::ShadeMode::Shaded;
    let mut xray = false;
    let mut wire_cache = scene_render::WireframeCache::new();
    let mut grid_built =
        (settings.grid_spacing, settings.grid_minor_color, settings.grid_major_color);
    #[cfg(not(target_arch = "wasm32"))]
    let mut control = control::ControlServer::start();

    info("box3d physics mirror created");

    let mut grid = grid::build_grid(
        &context,
        settings.grid_spacing,
        settings.grid_minor_color,
        settings.grid_major_color,
    );

    // Z-up lighting: key light from above-left, cool fill from the opposite side.
    let ambient = AmbientLight::new(&context, 0.35, Srgba::WHITE);
    let key = DirectionalLight::new(&context, 1.4, Srgba::WHITE, vec3(-0.4, 0.35, -0.85));
    let fill = DirectionalLight::new(
        &context,
        0.5,
        Srgba::new(180, 190, 210, 255),
        vec3(0.6, -0.5, -0.2),
    );

    let mut gui = three_d::GUI::new(&context);
    let mut egui_kb_last_frame = false;

    window.render_loop(move |mut frame_input| {
        edit_mode.sync(&mut scene);
        // claim Tab for edit mode BEFORE egui grabs it for widget-focus
        // traversal; when a text field had focus last frame, egui keeps it
        let mut tab_pressed = false;
        if !egui_kb_last_frame {
            for event in frame_input.events.iter_mut() {
                if let Event::KeyPress { kind: Key::Tab, handled, .. } = event {
                    if !*handled {
                        tab_pressed = true;
                        *handled = true;
                    }
                }
            }
        }
        let modal_status = edit_mode.status_line().or_else(|| modal.status_line());
        let modal_guides = modal.guides();
        let edit_overlay = edit_mode.overlay(&scene);
        let wire_segments = (shade_mode == scene_render::ShadeMode::Wireframe)
            .then(|| wire_cache.segments(&scene, &sel));
        let fps = 1000.0 / frame_input.elapsed_time.max(0.001) as f32;
        #[cfg(not(target_arch = "wasm32"))]
        let mcp_status = Some(control.as_ref().map(|c| c.status()));
        #[cfg(target_arch = "wasm32")]
        let mcp_status: Option<Option<ui::McpStatus>> = None;
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
                    &mut settings,
                    &mut library,
                    &mut snap_to_grid,
                    &mut snap_to_vertex,
                    &mut shade_mode,
                    &mut xray,
                    &modal_status,
                    fps,
                    mcp_status,
                );

                overlay::draw(
                    gui_context,
                    &camera,
                    frame_input.viewport,
                    frame_input.device_pixel_ratio,
                    &scene,
                    &measure,
                    &calibrate,
                    settings.unit,
                );
                if let Some(guides) = &modal_guides {
                    overlay::draw_modal_guides(
                        gui_context,
                        &camera,
                        frame_input.viewport,
                        frame_input.device_pixel_ratio,
                        guides,
                    );
                }
                if let Some(segments) = &wire_segments {
                    overlay::draw_wireframe(
                        gui_context,
                        &camera,
                        frame_input.viewport,
                        frame_input.device_pixel_ratio,
                        segments,
                    );
                }
                if let Some(edit) = &edit_overlay {
                    overlay::draw_edit_mode(
                        gui_context,
                        &camera,
                        frame_input.viewport,
                        frame_input.device_pixel_ratio,
                        edit,
                    );
                }
                add_menu.ui(gui_context, &mut scene);
                delete_tool.ui(gui_context, &mut scene, &mut sel);
                axis_widget::axis_widget(
                    gui_context,
                    &mut camera,
                    layout.right_offset,
                    layout.top_offset,
                );
                axis_widget::view_label(gui_context, &camera, 0.0, layout.top_offset);

                // Blender-style operator status while transforming
                if let Some(status) = &modal_status {
                    let screen = gui_context.content_rect();
                    egui::Area::new(egui::Id::new("modal-status"))
                        .fixed_pos(egui::pos2(
                            screen.left() + 12.0,
                            screen.top() + layout.top_offset + 30.0,
                        ))
                        .order(egui::Order::Foreground)
                        .interactable(false)
                        .show(gui_context, |ui| {
                            ui.label(
                                egui::RichText::new(status)
                                    .size(13.0)
                                    .color(egui::Color32::from_rgb(255, 200, 120)),
                            );
                        });
                }

                // plain clicks on egui widgets are NOT flagged handled by
                // three-d (only drags are), so track hover ourselves
                pointer_over_ui = gui_context.is_pointer_over_egui();
            },
        );

        // rebuild the grid when its settings change; persist settings edits
        let grid_wanted =
            (settings.grid_spacing, settings.grid_minor_color, settings.grid_major_color);
        if grid_built != grid_wanted {
            grid = grid::build_grid(
                &context,
                settings.grid_spacing,
                settings.grid_minor_color,
                settings.grid_major_color,
            );
            grid_built = grid_wanted;
        }
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
                let at = physics
                    .pick_point(
                        glam::Vec3::new(origin.x, origin.y, origin.z),
                        glam::Vec3::new(direction.x, direction.y, direction.z),
                    )
                    .unwrap_or(glam::Vec3::ZERO);
                let new_ids = modeler_core::library::instantiate(&mut scene, &asset, at);
                let active = new_ids.first().copied();
                sel.set(new_ids, active);
                ui_state.status_message = Some(format!("placed '{}'", asset.name));
            }
        }

        // persist library changes (sidebar edits or MCP commands)
        if library.revision() != library_saved_revision {
            library::save(&library);
            library_saved_revision = library.revision();
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

        // edit mode (Tab): element selection & moves on the active object
        edit_mode.handle_events(
            &mut frame_input.events,
            &camera,
            frame_input.viewport,
            &mut scene,
            &sel,
            egui_owns_keyboard,
            tab_pressed,
            physics.is_stopped(),
            settings.unit,
        );

        // Space = play/pause, Esc = stop (when not editing)
        if !modal.active() && !edit_mode.active() {
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

        // editing tools are disabled while the simulation owns the transforms
        // and while edit mode owns the object
        if physics.is_stopped() && !edit_mode.active() {
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

        if !modal.active() && physics.is_stopped() && !edit_mode.active() {
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

        // external control API (MCP): execute queued agent commands
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(control) = control.as_mut() {
            control.poll(&mut scene, &mut sel, &mut physics, &mut library);
        }

        // step the simulation (writes transforms back into the scene)
        physics.update(&mut scene, frame_input.elapsed_time as f32 / 1000.0);

        // physics mirror must be current before picking (no-op while playing)
        physics.sync(&scene);
        sel.retain_existing(|id| scene.object(id).is_some());

        // batch this frame's edits into undo checkpoints once things go quiet
        undo.on_frame(&scene, modal.active() || edit_mode.grabbing() || !physics.is_stopped());

        // overlap warning while placing (grab/rotate/scale active)
        let overlaps = if modal.active() {
            physics.overlapping(sel.selected())
        } else {
            std::collections::HashSet::new()
        };

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
                    let hit = physics.pick(
                        glam::Vec3::new(origin.x, origin.y, origin.z),
                        glam::Vec3::new(direction.x, direction.y, direction.z),
                    );
                    sel.click(hit, modifiers.shift);
                    *handled = true;
                }
            }
        }

        let logical_height = frame_input.viewport.height as f32 / frame_input.device_pixel_ratio;
        camera.handle_events(&mut frame_input.events, logical_height);

        // '.' frames the selection (and re-pivots the orbit on it); Home frames all
        for event in frame_input.events.iter() {
            if let Event::KeyPress { kind, handled: false, .. } = event {
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
                    _ => {}
                }
            }
        }

        scene_render.sync(&scene, &sel, &overlaps, &context, shade_mode, xray);
        ref_render.sync(&scene, &context);

        let cam = camera.camera(frame_input.viewport);

        let mut render_objects: Vec<&dyn Object> =
            scene_render.models().map(|m| m as &dyn Object).collect();
        render_objects.extend(scene_render.outlines().map(|m| m as &dyn Object));
        render_objects.push(&grid);
        // reference images last: they blend over the grid and the meshes
        render_objects.extend(ref_render.models().map(|m| m as &dyn Object));

        frame_input
            .screen()
            .clear(ClearState::color_and_depth(0.12, 0.13, 0.16, 1.0, 1.0))
            .render(&cam, render_objects, &[&ambient, &key, &fill])
            .write(|| gui.render())
            .unwrap();

        // deliver any pending screenshot requests from the control API
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(control) = control.as_mut() {
            if !control.pending_screenshots.is_empty() {
                let pixels: Vec<[u8; 4]> = frame_input.screen().read_color();
                let response = match control::encode_screenshot(
                    &pixels,
                    frame_input.viewport.width,
                    frame_input.viewport.height,
                ) {
                    Ok(png_base64) => serde_json::json!({"ok": true, "png_base64": png_base64}),
                    Err(e) => serde_json::json!({"ok": false, "error": e}),
                };
                for reply in control.pending_screenshots.drain(..) {
                    let _ = reply.send(response.clone());
                }
            }
        }

        FrameOutput::default()
    });
}
