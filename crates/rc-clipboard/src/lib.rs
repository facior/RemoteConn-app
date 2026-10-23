//! Clipboard synchronization (text and images).
//!
//! A background thread watches the local clipboard and reports changes; content
//! received from the peer is written locally. Whatever was last sent or
//! received is remembered by hash, so a change never bounces back and forth.

use std::borrow::Cow;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;

use arboard::{Clipboard, ImageData};

const POLL: Duration = Duration::from_millis(400);
/// Larger content is not synchronized (it would block the control stream).
pub const MAX_TEXT: usize = 1024 * 1024;
pub const MAX_PNG: usize = 3 * 1024 * 1024;
/// Decoding allocates width*height*4 bytes; a tiny PNG from the peer could
/// otherwise claim an enormous size (16M pixels = 64 MB).
const MAX_PIXELS: usize = 16 * 1024 * 1024;

/// One clipboard access at a time in this process. Windows lets every thread
/// of a process use the clipboard opened by another one, and two watchers
/// (hosting and viewing at once) reading an image concurrently crashed in
/// clipboard-win (memcpy from a freed global handle).
static ACCESS: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipContent {
    Text(String),
    /// RGBA image encoded as PNG.
    Png(Vec<u8>),
}

impl ClipContent {
    fn hash(&self) -> blake3::Hash {
        match self {
            ClipContent::Text(t) => blake3::hash(t.as_bytes()),
            ClipContent::Png(p) => blake3::hash(p),
        }
    }
}

enum Cmd {
    Set(ClipContent),
}

/// Running synchronization; stops when dropped.
pub struct ClipboardSync {
    tx: mpsc::Sender<Cmd>,
}

impl ClipboardSync {
    /// Starts watching; `on_change` runs on the watcher thread for every local
    /// change (not for content applied through [`ClipboardSync::set`]).
    pub fn start(on_change: impl Fn(ClipContent) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("rc-clipboard".into())
            .spawn(move || watch(rx, on_change))
            .expect("spawn clipboard thread");
        Self { tx }
    }

    /// Applies content received from the peer.
    pub fn set(&self, content: ClipContent) {
        let _ = self.tx.send(Cmd::Set(content));
    }
}

fn watch(rx: mpsc::Receiver<Cmd>, on_change: impl Fn(ClipContent)) {
    let mut clipboard = match Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("clipboard unavailable: {e}");
            return;
        }
    };
    let mut seq = sequence_number();
    // Content already on the clipboard when the session starts is not sent.
    let mut last = read(&mut clipboard).map(|c| c.hash());

    loop {
        match rx.recv_timeout(POLL) {
            Ok(Cmd::Set(content)) => {
                last = Some(content.hash());
                if let Err(e) = write(&mut clipboard, &content) {
                    tracing::debug!("clipboard write failed: {e}");
                }
                seq = sequence_number();
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let now = sequence_number();
        if now.is_some() && now == seq {
            continue; // cheap path: nothing changed (Windows)
        }
        seq = now;
        if let Some(content) = read(&mut clipboard) {
            let hash = content.hash();
            if last != Some(hash) {
                last = Some(hash);
                on_change(content);
            }
        }
    }
}

#[cfg(windows)]
fn sequence_number() -> Option<u32> {
    Some(unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() })
}

#[cfg(not(windows))]
fn sequence_number() -> Option<u32> {
    None
}

fn read(clipboard: &mut Clipboard) -> Option<ClipContent> {
    let _guard = ACCESS.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(text) = clipboard.get_text() {
        if !text.is_empty() && text.len() <= MAX_TEXT {
            return Some(ClipContent::Text(text));
        }
    }
    let image = clipboard.get_image().ok()?;
    if image.width * image.height > MAX_PIXELS {
        return None;
    }
    let png = encode_png(image.width as u32, image.height as u32, &image.bytes).ok()?;
    (png.len() <= MAX_PNG).then_some(ClipContent::Png(png))
}

fn write(clipboard: &mut Clipboard, content: &ClipContent) -> Result<(), String> {
    let _guard = ACCESS.lock().unwrap_or_else(|e| e.into_inner());
    match content {
        ClipContent::Text(text) => clipboard.set_text(text.clone()).map_err(|e| e.to_string()),
        ClipContent::Png(png) => {
            let (width, height, rgba) = decode_png(png)?;
            clipboard
                .set_image(ImageData {
                    width,
                    height,
                    bytes: Cow::Owned(rgba),
                })
                .map_err(|e| e.to_string())
        }
    }
}

pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Decodes an 8-bit RGBA PNG (the only kind peers send).
pub fn decode_png(png: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let decoder = png::Decoder::new(png);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let info = reader.info();
    let (width, height) = (info.width as usize, info.height as usize);
    if info.color_type != png::ColorType::Rgba
        || info.bit_depth != png::BitDepth::Eight
        || width * height > MAX_PIXELS
    {
        return Err("unsupported image".into());
    }
    let mut buf = vec![0; reader.output_buffer_size()];
    reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    Ok((width, height, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip() {
        let rgba: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8).collect();
        let png = encode_png(3, 2, &rgba).unwrap();
        assert_eq!(decode_png(&png).unwrap(), (3, 2, rgba));
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_png(b"not a png").is_err());
    }
}
