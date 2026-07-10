//! HTTP control API for external tools (the MCP server in `modeler-mcp`).
//! Native builds only — the browser can't host a TCP listener.
//!
//! A background thread accepts POST requests with a JSON command body and
//! forwards them to the render loop through a channel; the render loop
//! executes them against the live scene (via `commands::execute`, shared
//! with the AI assistant) and replies. Screenshots are captured right after
//! the frame is rendered.

use crate::commands;
use crate::physics::PhysicsMirror;
use crate::selection::Selection;
use modeler_core::Library;
use modeler_core::Scene;
use serde_json::{json, Value};
use std::sync::mpsc::{channel, Receiver, Sender};

pub const DEFAULT_PORT: u16 = 8323;

type Reply = Sender<Value>;

pub struct ControlServer {
    requests: Receiver<(Value, Reply)>,
    /// Screenshot requests wait until after the next render.
    pub pending_screenshots: Vec<Reply>,
    port: u16,
    commands_handled: u64,
    last_command: Option<std::time::Instant>,
}

impl ControlServer {
    /// Spawn the HTTP listener thread. Returns None if the port is taken
    /// (e.g. a second app instance) — the app just runs without control.
    pub fn start() -> Option<Self> {
        let port = std::env::var("MODELER_CONTROL_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PORT);
        let server = tiny_http::Server::http(("127.0.0.1", port)).ok()?;
        let (tx, rx) = channel::<(Value, Reply)>();

        std::thread::spawn(move || {
            for mut http_request in server.incoming_requests() {
                let mut body = String::new();
                let _ = http_request.as_reader().read_to_string(&mut body);

                let response_json = match serde_json::from_str::<Value>(&body) {
                    Err(e) => json!({"ok": false, "error": format!("invalid JSON: {e}")}),
                    Ok(command) => {
                        let (reply_tx, reply_rx) = channel();
                        if tx.send((command, reply_tx)).is_err() {
                            json!({"ok": false, "error": "app is shutting down"})
                        } else {
                            // screenshots wait for the next frame; allow time
                            reply_rx
                                .recv_timeout(std::time::Duration::from_secs(10))
                                .unwrap_or_else(|_| {
                                    json!({"ok": false, "error": "timed out waiting for the app"})
                                })
                        }
                    }
                };
                let data = response_json.to_string();
                let response = tiny_http::Response::from_string(data).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap(),
                );
                let _ = http_request.respond(response);
            }
        });

        println!("control API listening on http://127.0.0.1:{port} (for modeler-mcp)");
        Some(Self {
            requests: rx,
            pending_screenshots: Vec::new(),
            port,
            commands_handled: 0,
            last_command: None,
        })
    }

    /// Status for the UI indicator.
    pub fn status(&self) -> crate::ui::McpStatus {
        crate::ui::McpStatus {
            port: self.port,
            commands_handled: self.commands_handled,
            seconds_since_last: self
                .last_command
                .map(|t| t.elapsed().as_secs_f32()),
        }
    }

    /// Execute queued commands. Call once per frame from the render loop.
    #[allow(clippy::too_many_arguments)]
    pub fn poll(
        &mut self,
        scene: &mut Scene,
        selection: &mut Selection,
        physics: &mut PhysicsMirror,
        library_doc: &mut Library,
        shade_mode: &mut crate::scene_render::ShadeMode,
        lighting_mode: &mut crate::scene_render::LightingMode,
    ) {
        while let Ok((command, reply)) = self.requests.try_recv() {
            self.commands_handled += 1;
            self.last_command = Some(std::time::Instant::now());
            if command["cmd"] == "screenshot" {
                self.pending_screenshots.push(reply);
                continue;
            }
            // viewport view state lives in the render loop, not the scene
            if command["cmd"] == "set_view" {
                let _ = reply.send(commands::set_view(&command, shade_mode, lighting_mode));
                continue;
            }
            let response = commands::execute(&command, scene, selection, physics, library_doc);
            let _ = reply.send(response);
        }
    }
}
