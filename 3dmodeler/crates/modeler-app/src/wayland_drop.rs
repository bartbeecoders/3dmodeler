//! Wayland drag-and-drop: OS file drops on the native window.
//!
//! winit 0.30 implements `WindowEvent::DroppedFile` on X11, macOS and
//! Windows but not on Wayland — under Hyprland & co the event never fires.
//! So this module speaks the core `wl_data_device` protocol itself: it joins
//! winit's existing libwayland connection as a guest backend (the raw
//! display pointer from raw-window-handle, with its own event queue —
//! libwayland's supported multi-queue embedding) and binds a second data
//! device on the seat, which the compositor feeds drag-and-drop offers
//! alongside winit's own. Offers advertising `text/uri-list` are accepted,
//! received through a pipe on drop, and the `file://` URIs land in
//! [`crate::drop_target::handle_path`] exactly like a winit drop would.
//!
//! Everything runs on one background thread; the event handlers only ever
//! run there. Clipboard offers arrive on the same data device and are
//! discarded — winit's own device keeps handling the clipboard.

#![cfg(target_os = "linux")]

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::AsFd;

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, Proxy, QueueHandle,
};

const URI_LIST: &str = "text/uri-list";

/// Start the drop listener for `window`. A no-op away from Wayland (X11:
/// winit delivers drops itself) or when anything about the setup fails —
/// drag-and-drop is a convenience, never worth failing startup over.
pub fn init(window: &winit::window::Window) {
    use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    let Ok(handle) = window.display_handle() else { return };
    let RawDisplayHandle::Wayland(wayland) = handle.as_raw() else {
        return;
    };
    // SAFETY: the pointer is winit's live wl_display, and the window — and
    // with it the display connection — lives for the rest of the process.
    let backend = unsafe { Backend::from_foreign_display(wayland.display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    std::thread::spawn(move || run(conn));
}

fn run(conn: Connection) {
    let mut queue = conn.new_event_queue();
    let _registry = conn.display().get_registry(&queue.handle(), ());
    let mut state = State {
        conn: conn.clone(),
        seat: None,
        manager: None,
        device: None,
        mimes: HashMap::new(),
        current: None,
    };
    loop {
        if let Err(e) = queue.blocking_dispatch(&mut state) {
            eprintln!("Wayland file-drop listener stopped: {e}");
            return;
        }
    }
}

struct State {
    conn: Connection,
    seat: Option<WlSeat>,
    manager: Option<WlDataDeviceManager>,
    device: Option<WlDataDevice>,
    /// Mime types each live offer has advertised, by protocol object id.
    mimes: HashMap<ObjectId, HashSet<String>>,
    /// The drag currently over the window.
    current: Option<WlDataOffer>,
}

impl State {
    fn offers_uris(&self, offer: &WlDataOffer) -> bool {
        self.mimes.get(&offer.id()).is_some_and(|m| m.contains(URI_LIST))
    }

    fn forget(&mut self, offer: &WlDataOffer) {
        self.mimes.remove(&offer.id());
        offer.destroy();
    }

    /// Ask the source for the URI list and hand every `file://` entry to the
    /// drop dispatch. Runs on the listener thread; blocking on the pipe only
    /// blocks drag-and-drop itself.
    fn receive_uris(&self, offer: &WlDataOffer) {
        let Ok((read_end, write_end)) = rustix::pipe::pipe() else { return };
        offer.receive(URI_LIST.into(), write_end.as_fd());
        drop(write_end); // the source holds the only writer now
        let _ = self.conn.flush();
        let mut text = String::new();
        let _ = std::fs::File::from(read_end).read_to_string(&mut text);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some(rest) = line.strip_prefix("file://") else { continue };
            // an authority may sit before the path (file://localhost/…)
            let Some(slash) = rest.find('/') else { continue };
            let path = crate::gltf_import::percent_decode(&rest[slash..]);
            crate::drop_target::handle_path(path.into());
        }
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, 1, qh, ()));
            }
            "wl_data_device_manager" if state.manager.is_none() => {
                // v3 for the accept/set_actions/finish handshake
                state.manager = Some(registry.bind(name, version.min(3), qh, ()));
            }
            _ => {}
        }
        if state.device.is_none() {
            if let (Some(seat), Some(manager)) = (&state.seat, &state.manager) {
                state.device = Some(manager.get_data_device(seat, qh, ()));
            }
        }
    }
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.mimes.entry(offer.id()).or_default().insert(mime_type);
        }
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    // the data_offer event introduces a new wl_data_offer object
    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);

    fn event(
        state: &mut Self,
        _device: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Enter { serial, id: Some(offer), .. } => {
                // accept (and pick the copy action) up front; a v3 source
                // cancels the drop on anything left unaccepted
                if state.offers_uris(&offer) {
                    offer.accept(serial, Some(URI_LIST.into()));
                    if offer.version() >= 3 {
                        offer.set_actions(DndAction::Copy, DndAction::Copy);
                    }
                } else {
                    offer.accept(serial, None);
                }
                if let Some(stale) = state.current.replace(offer) {
                    state.forget(&stale);
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(offer) = state.current.take() {
                    state.forget(&offer);
                }
            }
            wl_data_device::Event::Drop => {
                let Some(offer) = state.current.take() else { return };
                if state.offers_uris(&offer) {
                    state.receive_uris(&offer);
                    if offer.version() >= 3 {
                        offer.finish();
                    }
                }
                state.forget(&offer);
            }
            // clipboard offers arrive on this device too; not ours to serve
            wl_data_device::Event::Selection { id: Some(offer) } => state.forget(&offer),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: WlDataDeviceManager);
