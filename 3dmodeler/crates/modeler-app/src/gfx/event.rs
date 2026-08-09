//! Input events.
//!
//! These mirror `three_d`'s event types field for field. That is deliberate:
//! the app matches on them in 32 files and roughly a hundred places, and the
//! shapes were never the problem — the problem was that they arrived from a
//! renderer the app is leaving. Keeping the shapes means the port is an import
//! change in those files rather than a rewrite of every tool's input handling.
//!
//! `handled` is the part worth understanding. Events are delivered as a mutable
//! slice and each consumer sets `handled` when it takes one, so the tools form a
//! priority chain: the UI looks first, then whichever modal tool is active, then
//! the viewport. A tool that reads an event without setting `handled` is saying
//! "I looked, but someone else may still want this", which is how a drag can
//! update a readout and still orbit the camera.

use super::math::PhysicalPoint;

/// Keyboard modifiers held during an event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    /// The Windows/Command key.
    pub command: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// The keys the app actually binds.
///
/// Deliberately not every key a keyboard has: this is the set the tools name,
/// and an exhaustive enum would be a list of variants no `match` ever mentions.
/// Anything unbound arrives as [`Event::Text`] instead, which is also how
/// layout-dependent bindings work — see the note there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    Backspace,
    Delete,
    End,
    Enter,
    Escape,
    Home,
    Insert,
    PageDown,
    PageUp,
    Space,
    Tab,
    /// Ctrl+V, surfaced as its own key because the browser delivers a paste as
    /// a clipboard event rather than as a keystroke.
    Paste,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Period,
    Comma,
    Minus,
    Equals,
}

/// One input event.
///
/// Field-compatible with `three_d::Event`.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    MousePress {
        button: MouseButton,
        position: PhysicalPoint,
        modifiers: Modifiers,
        handled: bool,
    },
    MouseRelease {
        button: MouseButton,
        position: PhysicalPoint,
        modifiers: Modifiers,
        handled: bool,
    },
    MouseMotion {
        /// The button held while moving, if any. A drag and a hover are the
        /// same event with this set or not.
        button: Option<MouseButton>,
        delta: (f32, f32),
        position: PhysicalPoint,
        modifiers: Modifiers,
        handled: bool,
    },
    MouseWheel {
        delta: (f32, f32),
        position: PhysicalPoint,
        modifiers: Modifiers,
        handled: bool,
    },
    /// A trackpad pinch, which is a zoom rather than a scroll.
    PinchGesture {
        delta: f32,
        position: PhysicalPoint,
        modifiers: Modifiers,
        handled: bool,
    },
    KeyPress {
        kind: Key,
        modifiers: Modifiers,
        handled: bool,
    },
    KeyRelease {
        kind: Key,
        modifiers: Modifiers,
        handled: bool,
    },
    /// A typed character.
    ///
    /// The app binds some shortcuts to this rather than to [`Event::KeyPress`]
    /// on purpose: a key *position* is not a letter. On AZERTY the physical `Q`
    /// types an `A`, so a binding read from the position fires under the user's
    /// finger in the wrong place. Text is what the user meant to type.
    Text(String),
    /// A file dropped onto the window from the desktop.
    DroppedFile(std::path::PathBuf),
}

impl Event {
    /// Whether some earlier consumer has claimed this event.
    ///
    /// [`Event::Text`] and [`Event::DroppedFile`] carry no flag and always
    /// report `false`: nothing in the app contests them.
    pub fn handled(&self) -> bool {
        match self {
            Self::MousePress { handled, .. }
            | Self::MouseRelease { handled, .. }
            | Self::MouseMotion { handled, .. }
            | Self::MouseWheel { handled, .. }
            | Self::PinchGesture { handled, .. }
            | Self::KeyPress { handled, .. }
            | Self::KeyRelease { handled, .. } => *handled,
            Self::Text(_) | Self::DroppedFile(_) => false,
        }
    }

    /// Claims the event so later consumers in the chain skip it.
    pub fn set_handled(&mut self, value: bool) {
        match self {
            Self::MousePress { handled, .. }
            | Self::MouseRelease { handled, .. }
            | Self::MouseMotion { handled, .. }
            | Self::MouseWheel { handled, .. }
            | Self::PinchGesture { handled, .. }
            | Self::KeyPress { handled, .. }
            | Self::KeyRelease { handled, .. } => *handled = value,
            Self::Text(_) | Self::DroppedFile(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(handled: bool) -> Event {
        Event::KeyPress { kind: Key::A, modifiers: Modifiers::default(), handled }
    }

    #[test]
    fn handled_round_trips() {
        let mut e = press(false);
        assert!(!e.handled());
        e.set_handled(true);
        assert!(e.handled());
    }

    #[test]
    fn text_and_drops_are_never_handled() {
        // They carry no flag, so the accessor must not claim otherwise — a
        // consumer skipping "handled" events would drop every keystroke.
        let mut t = Event::Text("a".into());
        assert!(!t.handled());
        t.set_handled(true);
        assert!(!t.handled(), "setting handled on Text must stay a no-op");

        let mut d = Event::DroppedFile("/tmp/x.png".into());
        d.set_handled(true);
        assert!(!d.handled());
    }

    #[test]
    fn modifiers_default_to_nothing_held() {
        let m = Modifiers::default();
        assert!(!m.alt && !m.ctrl && !m.shift && !m.command);
    }
}
