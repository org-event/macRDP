//! Remote input injected with `CGEventPost` (needs the Accessibility permission).
//!
//! Keys arrive as PC scan codes (set 1). The Windows key maps to Command, Alt to
//! Option, so a client set to send Command as the Windows key gets real Command.

use std::time::{Duration, Instant};

use ironrdp_server::{KeyboardEvent, MouseButton, MouseEvent, RdpServerInputHandler};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton,
    CGScrollEventUnit,
};
use tracing::{debug, trace};

/// Clicks closer than this in time and distance count as a multi-click.
const MULTI_CLICK_TIME: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 4.0;

pub struct MacInput {
    /// Desktop pixels to display points.
    scale: f64,
    modifiers: CGEventFlags,
    position: CGPoint,
    pressed: [bool; 5],
    last_click: Option<(MouseButton, Instant, CGPoint, i64)>,
}

impl MacInput {
    pub fn new(scale: f64) -> Self {
        Self {
            scale,
            modifiers: CGEventFlags::empty(),
            position: CGPoint::ZERO,
            pressed: [false; 5],
            last_click: None,
        }
    }

    fn point(&self, x: u16, y: u16) -> CGPoint {
        CGPoint::new(f64::from(x) * self.scale, f64::from(y) * self.scale)
    }

    fn post(&self, event: Option<objc2_core_foundation::CFRetained<CGEvent>>) {
        let Some(event) = event else { return };
        CGEvent::set_flags(Some(&event), self.modifiers);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }

    fn key(&mut self, code: u8, extended: bool, down: bool) {
        let Some(key) = mac_keycode(code, extended) else {
            debug!(code, extended, "unmapped scan code");
            return;
        };
        trace!(code, extended, key, down, "key");
        let mask = modifier_mask(key);
        if !mask.is_empty() {
            if down {
                self.modifiers |= mask;
            } else {
                self.modifiers &= !mask;
            }
        }
        self.post(CGEvent::new_keyboard_event(None, key, down));
    }

    fn unicode(&self, ch: u16, down: bool) {
        let event = CGEvent::new_keyboard_event(None, 0, down);
        // SAFETY: one UTF-16 unit from a valid local.
        unsafe { CGEvent::keyboard_set_unicode_string(event.as_deref(), 1, &ch) };
        self.post(event);
    }

    fn mouse_move(&mut self, x: u16, y: u16) {
        self.position = self.point(x, y);
        let (kind, button) = if self.pressed[0] {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.pressed[1] {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if self.pressed[2..].iter().any(|p| *p) {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        self.post(CGEvent::new_mouse_event(None, kind, self.position, button));
    }

    fn button(&mut self, x: u16, y: u16, button: MouseButton, down: bool) {
        self.position = self.point(x, y);
        let (index, cg_button, number, kind) = match (button, down) {
            (MouseButton::Left, true) => (0, CGMouseButton::Left, 0, CGEventType::LeftMouseDown),
            (MouseButton::Left, false) => (0, CGMouseButton::Left, 0, CGEventType::LeftMouseUp),
            (MouseButton::Right, true) => (1, CGMouseButton::Right, 1, CGEventType::RightMouseDown),
            (MouseButton::Right, false) => (1, CGMouseButton::Right, 1, CGEventType::RightMouseUp),
            (MouseButton::Middle, true) => {
                (2, CGMouseButton::Center, 2, CGEventType::OtherMouseDown)
            }
            (MouseButton::Middle, false) => {
                (2, CGMouseButton::Center, 2, CGEventType::OtherMouseUp)
            }
            (MouseButton::X1, true) => (3, CGMouseButton(3), 3, CGEventType::OtherMouseDown),
            (MouseButton::X1, false) => (3, CGMouseButton(3), 3, CGEventType::OtherMouseUp),
            (MouseButton::X2, true) => (4, CGMouseButton(4), 4, CGEventType::OtherMouseDown),
            (MouseButton::X2, false) => (4, CGMouseButton(4), 4, CGEventType::OtherMouseUp),
            _ => return,
        };
        self.pressed[index] = down;

        // macOS does not count clicks of synthesized events; double-click
        // selection and similar need the click state set explicitly.
        let clicks = if down {
            let clicks = match self.last_click {
                Some((last, at, pos, n))
                    if last == button
                        && at.elapsed() < MULTI_CLICK_TIME
                        && (pos.x - self.position.x).abs() <= MULTI_CLICK_DISTANCE
                        && (pos.y - self.position.y).abs() <= MULTI_CLICK_DISTANCE =>
                {
                    n + 1
                }
                _ => 1,
            };
            self.last_click = Some((button, Instant::now(), self.position, clicks));
            clicks
        } else {
            self.last_click.map_or(1, |(_, _, _, n)| n)
        };

        let event = CGEvent::new_mouse_event(None, kind, self.position, cg_button);
        CGEvent::set_integer_value_field(
            event.as_deref(),
            CGEventField::MouseEventClickState,
            clicks,
        );
        CGEvent::set_integer_value_field(
            event.as_deref(),
            CGEventField::MouseEventButtonNumber,
            number,
        );
        self.post(event);
    }

    fn scroll(&self, vertical: i32, horizontal: i32) {
        // RDP wheel units are 120 per notch; a third of that in pixels feels like
        // a native notch and keeps trackpad deltas smooth.
        let event = CGEvent::new_scroll_wheel_event2(
            None,
            CGScrollEventUnit::Pixel,
            2,
            vertical / 3,
            horizontal / 3,
            0,
        );
        self.post(event);
    }
}

impl RdpServerInputHandler for MacInput {
    fn keyboard(&mut self, event: KeyboardEvent) {
        trace!(?event, "keyboard");
        match event {
            KeyboardEvent::Pressed { code, extended } => self.key(code, extended, true),
            KeyboardEvent::Released { code, extended } => self.key(code, extended, false),
            KeyboardEvent::UnicodePressed(ch) => self.unicode(ch, true),
            KeyboardEvent::UnicodeReleased(ch) => self.unicode(ch, false),
            // Focus came back to the client: forget held modifiers so none stick.
            KeyboardEvent::Synchronize(_) => self.modifiers = CGEventFlags::empty(),
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        trace!(?event, "mouse");
        match event {
            MouseEvent::Move { x, y } => self.mouse_move(x, y),
            MouseEvent::Button {
                x,
                y,
                button,
                pressed,
            } => self.button(x, y, button, pressed),
            MouseEvent::VerticalScroll { value } => self.scroll(i32::from(value), 0),
            MouseEvent::HorizontalScroll { value } => self.scroll(0, -i32::from(value)),
            MouseEvent::Scroll { x, y } => self.scroll(y, -x),
            other => debug!(?other, "unhandled mouse event"),
        }
    }
}

fn modifier_mask(key: u16) -> CGEventFlags {
    match key {
        0x37 | 0x36 => CGEventFlags::MaskCommand,
        0x38 | 0x3C => CGEventFlags::MaskShift,
        0x3A | 0x3D => CGEventFlags::MaskAlternate,
        0x3B | 0x3E => CGEventFlags::MaskControl,
        _ => CGEventFlags::empty(),
    }
}

/// PC scan code (set 1) to macOS virtual key code (`kVK_*`).
fn mac_keycode(code: u8, extended: bool) -> Option<u16> {
    let key = if extended {
        match code {
            0x1C => 0x4C, // keypad Enter
            0x1D => 0x3E, // right Control
            0x20 => 0x4A, // mute
            0x2E => 0x49, // volume down
            0x30 => 0x48, // volume up
            0x35 => 0x4B, // keypad /
            0x37 => 0x69, // Print Screen -> F13
            0x38 => 0x3D, // right Alt -> right Option
            0x47 => 0x73, // Home
            0x48 => 0x7E, // Up
            0x49 => 0x74, // Page Up
            0x4B => 0x7B, // Left
            0x4D => 0x7C, // Right
            0x4F => 0x77, // End
            0x50 => 0x7D, // Down
            0x51 => 0x79, // Page Down
            0x52 => 0x72, // Insert -> Help
            0x53 => 0x75, // Delete (forward)
            0x5B => 0x37, // left Windows -> Command
            0x5C => 0x36, // right Windows -> right Command
            0x5D => 0x6E, // Menu
            _ => return None,
        }
    } else {
        match code {
            0x01 => 0x35, // Escape
            0x02 => 0x12, // 1
            0x03 => 0x13,
            0x04 => 0x14,
            0x05 => 0x15,
            0x06 => 0x17,
            0x07 => 0x16,
            0x08 => 0x1A,
            0x09 => 0x1C,
            0x0A => 0x19,
            0x0B => 0x1D, // 0
            0x0C => 0x1B, // -
            0x0D => 0x18, // =
            0x0E => 0x33, // Backspace
            0x0F => 0x30, // Tab
            0x10 => 0x0C, // Q
            0x11 => 0x0D, // W
            0x12 => 0x0E, // E
            0x13 => 0x0F, // R
            0x14 => 0x11, // T
            0x15 => 0x10, // Y
            0x16 => 0x20, // U
            0x17 => 0x22, // I
            0x18 => 0x1F, // O
            0x19 => 0x23, // P
            0x1A => 0x21, // [
            0x1B => 0x1E, // ]
            0x1C => 0x24, // Return
            0x1D => 0x3B, // left Control
            0x1E => 0x00, // A
            0x1F => 0x01, // S
            0x20 => 0x02, // D
            0x21 => 0x03, // F
            0x22 => 0x05, // G
            0x23 => 0x04, // H
            0x24 => 0x26, // J
            0x25 => 0x28, // K
            0x26 => 0x25, // L
            0x27 => 0x29, // ;
            0x28 => 0x27, // '
            0x29 => 0x32, // `
            0x2A => 0x38, // left Shift
            0x2B => 0x2A, // backslash
            0x2C => 0x06, // Z
            0x2D => 0x07, // X
            0x2E => 0x08, // C
            0x2F => 0x09, // V
            0x30 => 0x0B, // B
            0x31 => 0x2D, // N
            0x32 => 0x2E, // M
            0x33 => 0x2B, // ,
            0x34 => 0x2F, // .
            0x35 => 0x2C, // /
            0x36 => 0x3C, // right Shift
            0x37 => 0x43, // keypad *
            0x38 => 0x3A, // left Alt -> Option
            0x39 => 0x31, // Space
            0x3A => 0x39, // Caps Lock
            0x3B => 0x7A, // F1
            0x3C => 0x78, // F2
            0x3D => 0x63, // F3
            0x3E => 0x76, // F4
            0x3F => 0x60, // F5
            0x40 => 0x61, // F6
            0x41 => 0x62, // F7
            0x42 => 0x64, // F8
            0x43 => 0x65, // F9
            0x44 => 0x6D, // F10
            0x45 => 0x47, // Num Lock -> keypad Clear
            0x46 => 0x6B, // Scroll Lock -> F14
            0x47 => 0x59, // keypad 7
            0x48 => 0x5B, // keypad 8
            0x49 => 0x5C, // keypad 9
            0x4A => 0x4E, // keypad -
            0x4B => 0x56, // keypad 4
            0x4C => 0x57, // keypad 5
            0x4D => 0x58, // keypad 6
            0x4E => 0x45, // keypad +
            0x4F => 0x53, // keypad 1
            0x50 => 0x54, // keypad 2
            0x51 => 0x55, // keypad 3
            0x52 => 0x52, // keypad 0
            0x53 => 0x41, // keypad .
            0x56 => 0x0A, // ISO section key
            0x57 => 0x67, // F11
            0x58 => 0x6F, // F12
            0x59 => 0x51, // keypad =
            0x64 => 0x69, // F13
            0x65 => 0x6B, // F14
            0x66 => 0x71, // F15
            0x67 => 0x6A, // F16
            0x68 => 0x40, // F17
            0x69 => 0x4F, // F18
            0x6A => 0x50, // F19
            0x6B => 0x5A, // F20
            0x70 => 0x68, // JIS Kana
            0x73 => 0x5E, // JIS Ro (underscore)
            0x7B => 0x66, // JIS Eisu (Muhenkan)
            0x7D => 0x5D, // JIS Yen
            0x7E => 0x5F, // JIS keypad comma
            _ => return None,
        }
    };
    Some(key)
}
