//! OS clipboard → egui paste bridge.
//!
//! three-d's egui integration forwards typed characters (`Event::Text`) but
//! never reads the system clipboard, so Ctrl+V did nothing in text fields
//! (API keys, names, endpoints, …). This bridge turns a paste into an
//! `Event::Text` carrying the clipboard contents, which egui inserts exactly
//! like typed input.
//!
//! Native: paste chords (Ctrl+V, Shift+Insert, the dedicated Paste key) read
//! the clipboard via arboard. Browser: a document-level `paste` listener
//! captures the text (the clipboard API is async there, so the chord itself
//! can't read it synchronously).
//!
//! Copying OUT of fields still needs upstream integration support (egui's
//! copy command is consumed inside three-d) — paste is the important half.

use three_d::Event;

/// Call once at startup. Registers the browser paste listener on wasm;
/// a no-op natively.
pub fn init() {
    #[cfg(target_arch = "wasm32")]
    web::init();
}

/// Turn this frame's paste into `Event::Text` with the clipboard contents.
/// Call right before `gui.update`. `field_focused` must be egui's
/// `wants_keyboard_input()` — without a focused text field the injected text
/// would leak into the viewport's single-key shortcuts.
pub fn inject_paste(events: &mut Vec<Event>, field_focused: bool) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if !take_paste_chord(events, field_focused) {
            return;
        }
        let Ok(mut clipboard) = arboard::Clipboard::new() else { return };
        if let Ok(text) = clipboard.get_text() {
            if !text.is_empty() {
                events.push(Event::Text(text));
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        // the browser event is the source of truth; chords arrive separately
        // as (harmless) key presses
        if let Some(text) = web::take_pasted() {
            if field_focused {
                events.push(Event::Text(text));
            }
        }
    }
}

/// Detect an unhandled paste chord and claim it (mark handled) so neither
/// egui nor the app shortcuts see the raw key press. Returns true when a
/// paste should happen.
#[cfg(not(target_arch = "wasm32"))]
fn take_paste_chord(events: &mut [Event], field_focused: bool) -> bool {
    use three_d::Key;
    let mut requested = false;
    for event in events.iter_mut() {
        if let Event::KeyPress { kind, modifiers, handled } = event {
            let chord = (*kind == Key::V && (modifiers.ctrl || modifiers.command))
                || (*kind == Key::Insert && modifiers.shift)
                || *kind == Key::Paste;
            if chord && !*handled && field_focused {
                *handled = true;
                requested = true;
            }
        }
    }
    requested
}

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::RefCell;

    thread_local! {
        static PASTED: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub fn init() {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let closure = Closure::<dyn FnMut(web_sys::ClipboardEvent)>::new(
            |event: web_sys::ClipboardEvent| {
                let text = event
                    .clipboard_data()
                    .and_then(|data| data.get_data("text").ok())
                    .unwrap_or_default();
                if !text.is_empty() {
                    PASTED.with(|p| *p.borrow_mut() = Some(text));
                }
                event.prevent_default();
            },
        );
        let _ = document
            .add_event_listener_with_callback("paste", closure.as_ref().unchecked_ref());
        closure.forget(); // lives for the page's lifetime
    }

    pub fn take_pasted() -> Option<String> {
        PASTED.with(|p| p.borrow_mut().take())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use three_d::{Key, Modifiers};

    fn ctrl_v(handled: bool) -> Event {
        Event::KeyPress {
            kind: Key::V,
            modifiers: Modifiers { ctrl: true, ..Default::default() },
            handled,
        }
    }

    #[test]
    fn chord_claimed_only_when_field_focused() {
        // focused: the chord is claimed and marked handled
        let mut events = vec![ctrl_v(false)];
        assert!(take_paste_chord(&mut events, true));
        assert!(matches!(events[0], Event::KeyPress { handled: true, .. }));

        // not focused: left alone (the viewport may own the keyboard)
        let mut events = vec![ctrl_v(false)];
        assert!(!take_paste_chord(&mut events, false));
        assert!(matches!(events[0], Event::KeyPress { handled: false, .. }));

        // already handled (e.g. by a previous claimer): not a paste
        let mut events = vec![ctrl_v(true)];
        assert!(!take_paste_chord(&mut events, true));
    }

    #[test]
    fn shift_insert_and_plain_v_behave() {
        let mut events = vec![Event::KeyPress {
            kind: Key::Insert,
            modifiers: Modifiers { shift: true, ..Default::default() },
            handled: false,
        }];
        assert!(take_paste_chord(&mut events, true));

        // plain V (no modifier) is typing, not pasting
        let mut events = vec![Event::KeyPress {
            kind: Key::V,
            modifiers: Modifiers::default(),
            handled: false,
        }];
        assert!(!take_paste_chord(&mut events, true));
    }
}
