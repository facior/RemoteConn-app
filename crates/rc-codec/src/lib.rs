//! Video encoding.
//!
//! Stage 1 ships the software OpenH264 encoder (works everywhere). Hardware
//! encoders (Media Foundation / VideoToolbox) plug in behind [`VideoEncoder`].

use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;
use openh264_sys2::{SBitrateInfo, ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL};

/// WebCodecs codec string for the stream we produce (constrained baseline,
/// level 5.1 so any desktop resolution is accepted).
pub const H264_CODEC_STRING: &str = "avc1.42E033";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSettings {
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            fps: 30,
            bitrate_kbps: 8000,
        }
    }
}

/// One encoded access unit in Annex B format.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("openh264: {0}")]
    OpenH264(#[from] openh264::Error),
    #[error("frame buffer has {got} bytes, expected {expected}")]
    BadFrame { got: usize, expected: usize },
}

pub type Result<T> = std::result::Result<T, CodecError>;

pub trait VideoEncoder: Send {
    /// Codec name as announced in `VideoConfig.codec`.
    fn codec(&self) -> &'static str;

    /// Encodes a tightly packed BGRA frame. Returns `None` when the encoder
    /// decided to skip the frame (rate control).
    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>>;

    /// Changes fps or bitrate; may rebuild the encoder (next frame is a keyframe).
    fn set_settings(&mut self, settings: EncoderSettings);

    /// Adjusts the target bitrate in place, without a keyframe.
    fn set_bitrate(&mut self, kbps: u32);

    fn settings(&self) -> EncoderSettings;
}

pub struct H264Encoder {
    settings: EncoderSettings,
    state: Option<State>,
    crop: Vec<u8>,
}

struct State {
    encoder: Encoder,
    yuv: YUVBuffer,
    width: u32,
    height: u32,
}

impl H264Encoder {
    pub fn new(settings: EncoderSettings) -> Self {
        Self {
            settings,
            state: None,
            crop: Vec::new(),
        }
    }

    fn create(&self, width: u32, height: u32) -> Result<State> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(self.settings.bitrate_kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(self.settings.fps as f32))
            .skip_frames(true)
            // Not supported for screen content; set explicitly to avoid warnings.
            .adaptive_quantization(false)
            .background_detection(false)
            // Viewers ask for keyframes after loss; the periodic one is a safety net.
            .intra_frame_period(IntraFramePeriod::from_num_frames(self.settings.fps * 30));
        Ok(State {
            encoder: Encoder::with_api_config(OpenH264API::from_source(), config)?,
            yuv: YUVBuffer::new(width as usize, height as usize),
            width,
            height,
        })
    }
}

impl VideoEncoder for H264Encoder {
    fn codec(&self) -> &'static str {
        "h264"
    }

    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>> {
        let expected = width as usize * height as usize * 4;
        if bgra.len() != expected {
            return Err(CodecError::BadFrame {
                got: bgra.len(),
                expected,
            });
        }
        // I420 needs even dimensions; drop the last row/column if odd.
        let (w, h) = (width & !1, height & !1);
        let src = if (w, h) == (width, height) {
            bgra
        } else {
            self.crop.clear();
            for row in bgra.chunks(width as usize * 4).take(h as usize) {
                self.crop.extend_from_slice(&row[..w as usize * 4]);
            }
            &self.crop
        };

        let mut force = force_keyframe;
        if !matches!(&self.state, Some(s) if s.width == w && s.height == h) {
            self.state = Some(self.create(w, h)?);
            force = true;
        }
        let state = self.state.as_mut().expect("state created above");
        state
            .yuv
            .read_bgra8(BgraSliceU8::new(src, (w as usize, h as usize)));
        if force {
            state.encoder.force_intra_frame();
        }
        let bitstream = state.encoder.encode(&state.yuv)?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame {
            data,
            keyframe,
            width: w,
            height: h,
        }))
    }

    fn set_settings(&mut self, settings: EncoderSettings) {
        if settings.fps != self.settings.fps {
            self.settings = settings;
            // Rebuilt (with a keyframe) on the next frame.
            self.state = None;
        } else if settings.bitrate_kbps != self.settings.bitrate_kbps {
            self.set_bitrate(settings.bitrate_kbps);
        }
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.settings.bitrate_kbps = kbps;
        let Some(state) = self.state.as_mut() else {
            return; // applied when the encoder is created
        };
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: (kbps.saturating_mul(1000)).min(i32::MAX as u32) as i32,
        };
        // SAFETY: the option id matches the pointed-to struct type.
        let rc = unsafe {
            state.encoder.raw_api().set_option(
                ENCODER_OPTION_BITRATE,
                (&mut info as *mut SBitrateInfo).cast(),
            )
        };
        if rc != 0 {
            // Fall back to rebuilding with the new bitrate.
            self.state = None;
        }
    }

    fn settings(&self) -> EncoderSettings {
        self.settings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::decoder::Decoder;

    fn gradient(w: u32, h: u32, shift: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                out.extend_from_slice(&[(x + shift) as u8, y as u8, (x ^ y) as u8, 255]);
            }
        }
        out
    }

    #[test]
    fn encodes_decodable_stream_with_keyframes() {
        let (w, h) = (640, 360);
        let mut enc = H264Encoder::new(EncoderSettings::default());
        let mut dec = Decoder::new().unwrap();
        let mut frames = Vec::new();
        for i in 0..5 {
            let frame = enc
                .encode(&gradient(w, h, i * 7), w, h, i == 3)
                .unwrap()
                .expect("frame not skipped");
            assert!(frame.data.starts_with(&[0, 0, 0, 1]) || frame.data.starts_with(&[0, 0, 1]));
            frames.push(frame.keyframe);
            let decoded = dec.decode(&frame.data).unwrap();
            if i == 0 {
                assert!(decoded.is_some(), "first IDR must decode");
            }
        }
        assert!(frames[0], "first frame is a keyframe");
        assert!(!frames[1]);
        assert!(frames[3], "forced keyframe");
    }

    #[test]
    fn bitrate_changes_without_keyframe() {
        let (w, h) = (320, 240);
        let mut enc = H264Encoder::new(EncoderSettings::default());
        assert!(
            enc.encode(&gradient(w, h, 0), w, h, false)
                .unwrap()
                .unwrap()
                .keyframe
        );
        enc.set_bitrate(1500);
        assert_eq!(enc.settings().bitrate_kbps, 1500);
        let next = enc
            .encode(&gradient(w, h, 5), w, h, false)
            .unwrap()
            .unwrap();
        assert!(
            !next.keyframe,
            "live bitrate change must not force a keyframe"
        );
    }

    #[test]
    fn odd_dimensions_are_cropped() {
        let mut enc = H264Encoder::new(EncoderSettings::default());
        let frame = enc
            .encode(&gradient(321, 241, 0), 321, 241, false)
            .unwrap()
            .unwrap();
        assert_eq!((frame.width, frame.height), (320, 240));
    }

    #[test]
    fn rejects_wrong_buffer_size() {
        let mut enc = H264Encoder::new(EncoderSettings::default());
        assert!(matches!(
            enc.encode(&[0; 10], 16, 16, false),
            Err(CodecError::BadFrame { .. })
        ));
    }
}
