//! Screen capture.
//!
//! Windows: DXGI Desktop Duplication. macOS (ScreenCaptureKit) comes in stage 3;
//! until then other platforms report [`CaptureError::Unsupported`].

use std::time::Duration;

#[cfg_attr(not(windows), allow(dead_code))]
mod cursor;
pub mod desktop;
#[cfg(windows)]
mod dxgi;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// Index in enumeration order; stable while the display setup is unchanged.
    pub id: u32,
    pub name: String,
    /// Position on the virtual desktop, in physical pixels.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// A captured frame in BGRA8, rows tightly packed (`stride == width * 4`).
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
}

impl Frame<'_> {
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }
}

/// Pointer image in RGBA8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    /// Content hash; equal shapes have equal ids.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub rgba: Vec<u8>,
}

/// Pointer change since the last [`Capturer::take_cursor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorUpdate {
    pub visible: bool,
    /// Hotspot position relative to the captured monitor.
    pub x: i32,
    pub y: i32,
    /// Set when the pointer image changed.
    pub shape: Option<CursorShape>,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("screen capture is not supported on this platform yet")]
    Unsupported,
    #[error("monitor {0} not found")]
    MonitorNotFound(u32),
    #[error("capture failed: {0}")]
    Os(String),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

pub fn monitors() -> Result<Vec<MonitorInfo>> {
    #[cfg(windows)]
    return dxgi::monitors();
    #[cfg(not(windows))]
    Err(CaptureError::Unsupported)
}

pub struct Capturer {
    #[cfg(windows)]
    inner: dxgi::DxgiCapturer,
    monitor: MonitorInfo,
}

impl Capturer {
    pub fn new(monitor_id: u32) -> Result<Self> {
        let monitor = monitors()?
            .into_iter()
            .find(|m| m.id == monitor_id)
            .ok_or(CaptureError::MonitorNotFound(monitor_id))?;
        Ok(Self {
            #[cfg(windows)]
            inner: dxgi::DxgiCapturer::new(monitor_id)?,
            monitor,
        })
    }

    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// Waits up to `timeout` for the screen to change.
    ///
    /// Returns `Ok(None)` when nothing changed (or capture is temporarily
    /// unavailable, e.g. during a desktop switch); the first call after
    /// creation always yields a full frame once one is available.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>> {
        #[cfg(windows)]
        return self.inner.next_frame(timeout);
        #[cfg(not(windows))]
        {
            let _ = timeout;
            Err(CaptureError::Unsupported)
        }
    }

    /// Pointer changes observed by [`Capturer::next_frame`] since last call.
    pub fn take_cursor(&mut self) -> Option<CursorUpdate> {
        #[cfg(windows)]
        return self.inner.take_cursor();
        #[cfg(not(windows))]
        None
    }

    /// The most recent frame, used to answer keyframe requests while the
    /// screen is static.
    pub fn last_frame(&self) -> Option<Frame<'_>> {
        #[cfg(windows)]
        return self.inner.last_frame();
        #[cfg(not(windows))]
        None
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn captures_primary_monitor() {
        let monitors = monitors().unwrap();
        let Some(primary) = monitors.iter().find(|m| m.primary).or(monitors.first()) else {
            eprintln!("no monitor attached, skipping");
            return;
        };
        let mut capturer = Capturer::new(primary.id).unwrap();
        let mut frame = None;
        for _ in 0..20 {
            if let Some(f) = capturer.next_frame(Duration::from_millis(100)).unwrap() {
                frame = Some((f.width, f.height, f.data.len()));
                break;
            }
        }
        let (w, h, len) = frame.expect("no frame within 2s");
        assert_eq!((w, h), (primary.width, primary.height));
        assert_eq!(len, (w * h * 4) as usize);
    }
}
