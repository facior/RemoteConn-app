//! USB HID keyboard usages (page 0x07) to PC/AT set 1 scancodes.
//!
//! Extended keys carry a `0xE0` prefix in the high byte (`0xE0_48` = Up).
//! Viewers send physical keys as HID usages so the host's layout decides
//! which character a key produces, exactly like a local keyboard.

pub const EXTENDED: u16 = 0xE000;

pub fn hid_to_scancode(usage: u32) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];
    const KEYPAD_DIGITS: [u16; 10] = [0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49, 0x52];

    let code = match usage {
        0x04..=0x1D => LETTERS[(usage - 0x04) as usize],
        // 1..9 then 0
        0x1E..=0x27 => 0x02 + (usage - 0x1E) as u16,
        0x28 => 0x1C,                                // Enter
        0x29 => 0x01,                                // Escape
        0x2A => 0x0E,                                // Backspace
        0x2B => 0x0F,                                // Tab
        0x2C => 0x39,                                // Space
        0x2D => 0x0C,                                // - _
        0x2E => 0x0D,                                // = +
        0x2F => 0x1A,                                // [ {
        0x30 => 0x1B,                                // ] }
        0x31 => 0x2B,                                // \ |
        0x32 => 0x2B,                                // Non-US # ~
        0x33 => 0x27,                                // ; :
        0x34 => 0x28,                                // ' "
        0x35 => 0x29,                                // ` ~
        0x36 => 0x33,                                // , <
        0x37 => 0x34,                                // . >
        0x38 => 0x35,                                // / ?
        0x39 => 0x3A,                                // Caps Lock
        0x3A..=0x43 => 0x3B + (usage - 0x3A) as u16, // F1..F10
        0x44 => 0x57,                                // F11
        0x45 => 0x58,                                // F12
        0x46 => EXTENDED | 0x37,                     // Print Screen
        0x47 => 0x46,                                // Scroll Lock
        0x49 => EXTENDED | 0x52,                     // Insert
        0x4A => EXTENDED | 0x47,                     // Home
        0x4B => EXTENDED | 0x49,                     // Page Up
        0x4C => EXTENDED | 0x53,                     // Delete
        0x4D => EXTENDED | 0x4F,                     // End
        0x4E => EXTENDED | 0x51,                     // Page Down
        0x4F => EXTENDED | 0x4D,                     // Right
        0x50 => EXTENDED | 0x4B,                     // Left
        0x51 => EXTENDED | 0x50,                     // Down
        0x52 => EXTENDED | 0x48,                     // Up
        0x53 => 0x45,                                // Num Lock
        0x54 => EXTENDED | 0x35,                     // Keypad /
        0x55 => 0x37,                                // Keypad *
        0x56 => 0x4A,                                // Keypad -
        0x57 => 0x4E,                                // Keypad +
        0x58 => EXTENDED | 0x1C,                     // Keypad Enter
        0x59..=0x62 => KEYPAD_DIGITS[(usage - 0x59) as usize],
        0x63 => 0x53,            // Keypad .
        0x64 => 0x56,            // Non-US \ |
        0x65 => EXTENDED | 0x5D, // Application / context menu
        0xE0 => 0x1D,            // Left Ctrl
        0xE1 => 0x2A,            // Left Shift
        0xE2 => 0x38,            // Left Alt
        0xE3 => EXTENDED | 0x5B, // Left GUI (Win)
        0xE4 => EXTENDED | 0x1D, // Right Ctrl
        0xE5 => 0x36,            // Right Shift
        0xE6 => EXTENDED | 0x38, // Right Alt (AltGr)
        0xE7 => EXTENDED | 0x5C, // Right GUI
        _ => return None,
    };
    Some(code)
}

/// HID usage of the Pause key; it has no plain scancode and is sent as a
/// virtual key instead.
pub const HID_PAUSE: u32 = 0x48;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys() {
        assert_eq!(hid_to_scancode(0x04), Some(0x1E)); // A
        assert_eq!(hid_to_scancode(0x1D), Some(0x2C)); // Z
        assert_eq!(hid_to_scancode(0x1E), Some(0x02)); // 1
        assert_eq!(hid_to_scancode(0x27), Some(0x0B)); // 0
        assert_eq!(hid_to_scancode(0x43), Some(0x44)); // F10
        assert_eq!(hid_to_scancode(0x52), Some(EXTENDED | 0x48)); // Up
        assert_eq!(hid_to_scancode(0x59), Some(0x4F)); // Keypad 1
        assert_eq!(hid_to_scancode(0x62), Some(0x52)); // Keypad 0
        assert_eq!(hid_to_scancode(0xE6), Some(EXTENDED | 0x38)); // AltGr
        assert_eq!(hid_to_scancode(HID_PAUSE), None);
        assert_eq!(hid_to_scancode(0xFFFF), None);
    }
}
