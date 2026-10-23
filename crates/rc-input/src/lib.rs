//! Input injection on the host.
//!
//! Windows: `SendInput` with scancodes. macOS (CGEvent) comes in stage 3.

pub mod keymap;

#[cfg(windows)]
mod win;

use std::collections::HashSet;

use rc_proto::{mouse_event, KeyEvent, MouseEvent};

/// Target monitor on the virtual desktop, in physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("input injection is not supported on this platform yet")]
    Unsupported,
    #[error("input was blocked by the system (UIPI or secure desktop)")]
    Blocked,
}

pub type Result<T> = std::result::Result<T, InputError>;

/// Injects viewer input and remembers what is held down, so nothing stays
/// stuck when the session ends.
#[derive(Default)]
pub struct Injector {
    keys_down: HashSet<u32>,
    buttons_down: HashSet<i32>,
}

impl Injector {
    pub fn new() -> Result<Self> {
        if cfg!(windows) {
            Ok(Self::default())
        } else {
            Err(InputError::Unsupported)
        }
    }

    pub fn mouse(&mut self, ev: &MouseEvent, monitor: Rect) -> Result<()> {
        let x = monitor.x + ev.x.clamp(0, monitor.width as i32 - 1);
        let y = monitor.y + ev.y.clamp(0, monitor.height as i32 - 1);
        let button = ev.button();
        match ev.kind() {
            mouse_event::Kind::Move => platform::mouse_move(x, y),
            mouse_event::Kind::Down => {
                self.buttons_down.insert(button as i32);
                platform::mouse_button(x, y, button, true)
            }
            mouse_event::Kind::Up => {
                self.buttons_down.remove(&(button as i32));
                platform::mouse_button(x, y, button, false)
            }
            mouse_event::Kind::Wheel => platform::mouse_wheel(ev.wheel_dx, ev.wheel_dy),
            mouse_event::Kind::Unspecified => Ok(()),
        }
    }

    pub fn key(&mut self, ev: &KeyEvent) -> Result<()> {
        if ev.down {
            self.keys_down.insert(ev.hid_usage);
        } else {
            self.keys_down.remove(&ev.hid_usage);
        }
        platform::key(ev.hid_usage, ev.down)
    }

    /// Releases every key and button the viewer left pressed.
    pub fn release_all(&mut self) {
        for usage in self.keys_down.drain() {
            let _ = platform::key(usage, false);
        }
        for button in self.buttons_down.drain() {
            if let Ok(button) = mouse_event::Button::try_from(button) {
                let _ = platform::mouse_button_up(button);
            }
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(windows)]
use self::win as platform;

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn mouse_move(_: i32, _: i32) -> Result<()> {
        Err(InputError::Unsupported)
    }
    pub fn mouse_button(_: i32, _: i32, _: mouse_event::Button, _: bool) -> Result<()> {
        Err(InputError::Unsupported)
    }
    pub fn mouse_button_up(_: mouse_event::Button) -> Result<()> {
        Err(InputError::Unsupported)
    }
    pub fn mouse_wheel(_: i32, _: i32) -> Result<()> {
        Err(InputError::Unsupported)
    }
    pub fn key(_: u32, _: bool) -> Result<()> {
        Err(InputError::Unsupported)
    }
}
