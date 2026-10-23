//! `SendInput` backend.

use rc_proto::mouse_event::Button;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::keymap::{hid_to_scancode, EXTENDED, HID_PAUSE};
use crate::{InputError, Result};

const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

fn send(inputs: &[INPUT]) -> Result<()> {
    // Input only reaches the desktop this thread is attached to; the service
    // agent (SYSTEM) follows the login screen and UAC prompts this way.
    rc_capture::desktop::follow_input_desktop();
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(InputError::Blocked)
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Maps a virtual-desktop pixel to the 0..=65535 range `SendInput` expects.
fn normalize(x: i32, y: i32) -> (i32, i32) {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let nx = ((x - vx) as i64 * 65535 / (vw - 1) as i64) as i32;
    let ny = ((y - vy) as i64 * 65535 / (vh - 1) as i64) as i32;
    (nx, ny)
}

const ABS_MOVE: MOUSE_EVENT_FLAGS =
    MOUSE_EVENT_FLAGS(MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0);

pub fn mouse_move(x: i32, y: i32) -> Result<()> {
    let (nx, ny) = normalize(x, y);
    send(&[mouse_input(nx, ny, 0, ABS_MOVE)])
}

fn button_flags(button: Button, down: bool) -> Option<(MOUSE_EVENT_FLAGS, u32)> {
    Some(match (button, down) {
        (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (Button::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (Button::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (Button::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (Button::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        (Button::None, _) => return None,
    })
}

pub fn mouse_button(x: i32, y: i32, button: Button, down: bool) -> Result<()> {
    let Some((flags, data)) = button_flags(button, down) else {
        return Ok(());
    };
    let (nx, ny) = normalize(x, y);
    // Move and click in one input so the click lands where the viewer clicked.
    send(&[mouse_input(
        nx,
        ny,
        data,
        MOUSE_EVENT_FLAGS(ABS_MOVE.0 | flags.0),
    )])
}

pub fn mouse_button_up(button: Button) -> Result<()> {
    let Some((flags, data)) = button_flags(button, false) else {
        return Ok(());
    };
    send(&[mouse_input(0, 0, data, flags)])
}

/// Deltas are in Windows wheel units (120 per notch); positive `dy` scrolls up.
pub fn mouse_wheel(dx: i32, dy: i32) -> Result<()> {
    let mut inputs = Vec::with_capacity(2);
    if dy != 0 {
        inputs.push(mouse_input(0, 0, dy as u32, MOUSEEVENTF_WHEEL));
    }
    if dx != 0 {
        inputs.push(mouse_input(0, 0, dx as u32, MOUSEEVENTF_HWHEEL));
    }
    if inputs.is_empty() {
        return Ok(());
    }
    send(&inputs)
}

pub fn key(hid_usage: u32, down: bool) -> Result<()> {
    let up = if down {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    let ki = if hid_usage == HID_PAUSE {
        KEYBDINPUT {
            wVk: VK_PAUSE,
            wScan: 0,
            dwFlags: up,
            time: 0,
            dwExtraInfo: 0,
        }
    } else {
        let Some(code) = hid_to_scancode(hid_usage) else {
            return Ok(());
        };
        let mut flags = KEYEVENTF_SCANCODE.0 | up.0;
        if code & EXTENDED != 0 {
            flags |= KEYEVENTF_EXTENDEDKEY.0;
        }
        KEYBDINPUT {
            wVk: VIRTUAL_KEY(0),
            wScan: code & 0xFF,
            dwFlags: KEYBD_EVENT_FLAGS(flags),
            time: 0,
            dwExtraInfo: 0,
        }
    };
    send(&[INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    }])
}
