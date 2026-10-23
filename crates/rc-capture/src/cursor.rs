//! Conversion of DXGI pointer shapes to RGBA.

use std::hash::{Hash, Hasher};

use crate::CursorShape;

pub const TYPE_MONOCHROME: u32 = 1;
pub const TYPE_COLOR: u32 = 2;
pub const TYPE_MASKED_COLOR: u32 = 4;

/// Converts a pointer shape buffer. Screen-inverting pixels cannot be shown
/// as a plain image; they become black (monochrome) or keep their colour.
pub fn to_rgba(
    kind: u32,
    width: u32,
    height: u32,
    pitch: u32,
    hot_x: i32,
    hot_y: i32,
    buf: &[u8],
) -> Option<CursorShape> {
    let (w, pitch) = (width as usize, pitch as usize);
    let h = if kind == TYPE_MONOCHROME {
        height / 2
    } else {
        height
    } as usize;
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    match kind {
        TYPE_COLOR | TYPE_MASKED_COLOR => {
            if buf.len() < pitch * (h - 1) + w * 4 {
                return None;
            }
            for y in 0..h {
                for x in 0..w {
                    let p = &buf[y * pitch + x * 4..][..4];
                    let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
                    let px = match kind {
                        TYPE_COLOR => [r, g, b, a],
                        // Alpha is a mask: 0 = opaque colour, 0xFF = XOR.
                        _ if a == 0 => [r, g, b, 255],
                        _ if (r, g, b) == (0, 0, 0) => [0, 0, 0, 0],
                        _ => [r, g, b, 255],
                    };
                    rgba.extend_from_slice(&px);
                }
            }
        }
        TYPE_MONOCHROME => {
            if buf.len() < pitch * (2 * h - 1) + w.div_ceil(8) {
                return None;
            }
            for y in 0..h {
                for x in 0..w {
                    let bit = 0x80u8 >> (x % 8);
                    let and = buf[y * pitch + x / 8] & bit != 0;
                    let xor = buf[(y + h) * pitch + x / 8] & bit != 0;
                    let px = match (and, xor) {
                        (false, false) => [0, 0, 0, 255],
                        (false, true) => [255, 255, 255, 255],
                        (true, false) => [0, 0, 0, 0],
                        (true, true) => [0, 0, 0, 255],
                    };
                    rgba.extend_from_slice(&px);
                }
            }
        }
        _ => return None,
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (w, h, hot_x, hot_y, &rgba).hash(&mut hasher);
    Some(CursorShape {
        id: hasher.finish(),
        width: w as u32,
        height: h as u32,
        hot_x: hot_x.clamp(0, w as i32 - 1) as u32,
        hot_y: hot_y.clamp(0, h as i32 - 1) as u32,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_shape_swaps_channels() {
        // 2x1 BGRA: blue opaque, red half transparent.
        let buf = [255, 0, 0, 255, 0, 0, 255, 128];
        let s = to_rgba(TYPE_COLOR, 2, 1, 8, 0, 0, &buf).unwrap();
        assert_eq!(s.rgba, vec![0, 0, 255, 255, 255, 0, 0, 128]);
    }

    #[test]
    fn monochrome_masks() {
        // 4x1 cursor, pitch 1: AND row then XOR row.
        // pixels: (and,xor) = (0,0) black, (0,1) white, (1,0) clear, (1,1) invert
        let and = 0b0011_0000u8;
        let xor = 0b0101_0000u8;
        let s = to_rgba(TYPE_MONOCHROME, 4, 2, 1, 0, 0, &[and, xor]).unwrap();
        assert_eq!((s.width, s.height), (4, 1));
        assert_eq!(
            s.rgba,
            vec![0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 255]
        );
    }

    #[test]
    fn same_shape_same_id_and_bad_input_rejected() {
        let buf = [1, 2, 3, 255];
        let a = to_rgba(TYPE_COLOR, 1, 1, 4, 0, 0, &buf).unwrap();
        let b = to_rgba(TYPE_COLOR, 1, 1, 4, 0, 0, &buf).unwrap();
        assert_eq!(a.id, b.id);
        assert!(to_rgba(TYPE_COLOR, 2, 2, 8, 0, 0, &buf).is_none());
        assert!(to_rgba(99, 1, 1, 4, 0, 0, &buf).is_none());
    }
}
