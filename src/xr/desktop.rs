//! Desktop input over X11: keyboard, and mouse when the window is mirroring.
//!
//! OpenXR has no text input of any kind, and this is a native XR application
//! rather than a Wayland client, so a VR compositor's own keyboard cannot reach
//! it either. What *can* reach it is an ordinary X11 key event — which is what
//! any keyboard producing normal system input generates, whether that is a
//! physical keyboard, a VR overlay keyboard injecting through uinput, or a
//! keyboard forwarded from the headset.
//!
//! The application already holds an X connection and a window for GLX, so this
//! maps that window and reads key events from it. The window must hold input
//! focus for events to arrive; it is deliberately small and unobtrusive.

use std::os::raw::{c_char, c_int, c_ulong};

use x11_dl::xlib::{self, Xlib};

/// Something that happened in the desktop window.
#[derive(Debug, Clone)]
pub enum DesktopEvent {
    Key(KeyInput),
    /// Pointer position in window pixels, origin top-left.
    MouseMoved { x: f32, y: f32 },
    /// Primary button transition.
    MouseButton { pressed: bool },
}

/// A key event translated into something egui understands.
#[derive(Debug, Clone)]
pub enum KeyInput {
    /// Printable text that was typed.
    Text(String),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Escape,
    Enter,
}

/// Reads keyboard and mouse events from the GLX window.
pub struct DesktopInput {
    display: *mut xlib::Display,
    window: c_ulong,
}

impl DesktopInput {
    /// Maps the window and starts listening for key events.
    ///
    /// Mapping is required: an unmapped window can never take focus, and an
    /// unfocused window receives no key events.
    pub fn new(xlib: &Xlib, display: *mut xlib::Display, window: c_ulong) -> Self {
        unsafe {
            (xlib.XSelectInput)(
                display,
                window,
                xlib::KeyPressMask
                    | xlib::ButtonPressMask
                    | xlib::ButtonReleaseMask
                    | xlib::PointerMotionMask
                    | xlib::FocusChangeMask
                    | xlib::StructureNotifyMask,
            );
            (xlib.XMapWindow)(display, window);

            // Name it so it is identifiable in a window list, since the viewer
            // may have to click it to give it focus.
            let title = b"vrmp keyboard input\0";
            (xlib.XStoreName)(display, window, title.as_ptr() as *const c_char);
            (xlib.XFlush)(display);
        }
        DesktopInput { display, window }
    }

    /// Drains any key events queued since the last call.
    ///
    /// Non-blocking: only events already waiting are consumed, so the render
    /// loop is never held up.
    pub fn poll(&mut self, xlib: &Xlib) -> Vec<DesktopEvent> {
        let mut out = Vec::new();
        unsafe {
            while (xlib.XPending)(self.display) > 0 {
                let mut event: xlib::XEvent = std::mem::zeroed();
                (xlib.XNextEvent)(self.display, &mut event);

                match event.get_type() {
                    xlib::MotionNotify => {
                        let m = event.motion;
                        out.push(DesktopEvent::MouseMoved {
                            x: m.x as f32,
                            y: m.y as f32,
                        });
                        continue;
                    }
                    // Only the primary button. The scroll wheel arrives as
                    // buttons 4 and 5, which are deliberately ignored: the
                    // library already scrolls from the thumbstick, and a
                    // half-working wheel is worse than none.
                    xlib::ButtonPress | xlib::ButtonRelease if event.button.button == 1 => {
                        let pressed = event.get_type() == xlib::ButtonPress;
                        let b = event.button;
                        // Carry the position too: a click can arrive without a
                        // preceding motion event if the pointer entered the
                        // window already over the target.
                        out.push(DesktopEvent::MouseMoved {
                            x: b.x as f32,
                            y: b.y as f32,
                        });
                        out.push(DesktopEvent::MouseButton { pressed });
                        continue;
                    }
                    xlib::KeyPress => {}
                    _ => continue,
                }

                let mut key_event: xlib::XKeyEvent = event.key;
                let mut buffer = [0u8; 32];
                let mut keysym: xlib::KeySym = 0;
                let len = (xlib.XLookupString)(
                    &mut key_event,
                    buffer.as_mut_ptr() as *mut c_char,
                    buffer.len() as c_int,
                    &mut keysym,
                    std::ptr::null_mut(),
                );

                if let Some(named) = named_key(keysym) {
                    out.push(DesktopEvent::Key(named));
                    continue;
                }

                if len > 0 {
                    let text = String::from_utf8_lossy(&buffer[..len as usize]).into_owned();
                    // Control characters are handled above or ignored; only
                    // printable text should reach the field.
                    let printable: String =
                        text.chars().filter(|c| !c.is_control()).collect();
                    if !printable.is_empty() {
                        out.push(DesktopEvent::Key(KeyInput::Text(printable)));
                    }
                }
            }
        }
        out
    }

    pub fn window(&self) -> c_ulong {
        self.window
    }
}

/// Maps the editing and navigation keysyms egui needs as discrete keys.
fn named_key(keysym: xlib::KeySym) -> Option<KeyInput> {
    // Values from X11/keysymdef.h.
    const XK_BACKSPACE: xlib::KeySym = 0xff08;
    const XK_RETURN: xlib::KeySym = 0xff0d;
    const XK_ESCAPE: xlib::KeySym = 0xff1b;
    const XK_DELETE: xlib::KeySym = 0xffff;
    const XK_HOME: xlib::KeySym = 0xff50;
    const XK_LEFT: xlib::KeySym = 0xff51;
    const XK_RIGHT: xlib::KeySym = 0xff53;
    const XK_END: xlib::KeySym = 0xff57;
    const XK_KP_ENTER: xlib::KeySym = 0xff8d;

    match keysym {
        XK_BACKSPACE => Some(KeyInput::Backspace),
        XK_DELETE => Some(KeyInput::Delete),
        XK_LEFT => Some(KeyInput::Left),
        XK_RIGHT => Some(KeyInput::Right),
        XK_HOME => Some(KeyInput::Home),
        XK_END => Some(KeyInput::End),
        XK_ESCAPE => Some(KeyInput::Escape),
        XK_RETURN | XK_KP_ENTER => Some(KeyInput::Enter),
        _ => None,
    }
}

impl KeyInput {
    /// Converts to the egui event that expresses the same thing.
    pub fn to_egui(&self) -> egui::Event {
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        };
        match self {
            KeyInput::Text(text) => egui::Event::Text(text.clone()),
            KeyInput::Backspace => key(egui::Key::Backspace),
            KeyInput::Delete => key(egui::Key::Delete),
            KeyInput::Left => key(egui::Key::ArrowLeft),
            KeyInput::Right => key(egui::Key::ArrowRight),
            KeyInput::Home => key(egui::Key::Home),
            KeyInput::End => key(egui::Key::End),
            KeyInput::Escape => key(egui::Key::Escape),
            KeyInput::Enter => key(egui::Key::Enter),
        }
    }
}
