//! Camera format negotiation: pick the best format a V4L2 device offers for a
//! recorder.
//!
//! The rule upstream's AVCapture preset stands for, made explicit: the largest
//! frame no taller (or wider) than 1080p that still runs at 30 fps, preferring
//! uncompressed YUYV, then NV12, then MJPEG. Webcams only offer 1080p30 in
//! MJPEG usually (YUYV at 1080p often tops out at 5 fps), so the candidates
//! carry the fastest frame rate each format/size combination supports and the
//! frame-rate floor rules the slow ones out before resolution is compared.
//!
//! The selection is a pure function over the candidate list so it can be unit
//! tested without a camera; `v4l2.rs` builds the list from the driver's
//! enumeration.
//!
//! Adapted from rec-rs src/camera.rs (MIT OR Apache-2.0): the AVCapture session
//! preset (720p, else High) becomes an explicit choice from the device's caps.

use v4l::FourCC;

/// The largest camera frame the recorder asks for, like upstream's 720p/HIGH
/// presets stay below the screen size: a webcam overlay never needs more.
pub const MAX_WIDTH: u32 = 1920;
pub const MAX_HEIGHT: u32 = 1080;
/// The frame rate a format must reach to be considered.
pub const MIN_FPS: f64 = 30.0;
/// The frame rate requested when the format supports it (upstream's 30 fps).
pub const PREFERRED_FPS: u32 = 30;

pub fn fourcc_yuyv() -> FourCC {
    FourCC::new(b"YUYV")
}
pub fn fourcc_nv12() -> FourCC {
    FourCC::new(b"NV12")
}
pub fn fourcc_mjpeg() -> FourCC {
    FourCC::new(b"MJPG")
}

/// How the device packs a frame, for the converter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// Packed 4:2:2 YUYV (aka YUY2).
    Yuyv,
    /// Semi-planar 4:2:0 NV12.
    Nv12,
    /// One JPEG per frame.
    Mjpeg,
}

impl Encoding {
    /// The contract format the encoding converts to (BGRA, what the compositor
    /// consumes and what AVCapture hands upstream).
    pub fn pixel_format(self) -> crate::media_types::PixelFormat {
        crate::media_types::PixelFormat::Bgra
    }

    /// Preference among formats at the same resolution: uncompressed first.
    fn preference(self) -> u8 {
        match self {
            Encoding::Yuyv => 3,
            Encoding::Nv12 => 2,
            Encoding::Mjpeg => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Encoding::Yuyv => "YUYV",
            Encoding::Nv12 => "NV12",
            Encoding::Mjpeg => "MJPEG",
        }
    }

    pub fn from_fourcc(fourcc: FourCC) -> Option<Encoding> {
        if fourcc == fourcc_yuyv() {
            Some(Encoding::Yuyv)
        } else if fourcc == fourcc_nv12() {
            Some(Encoding::Nv12)
        } else if fourcc == fourcc_mjpeg() {
            Some(Encoding::Mjpeg)
        } else {
            None
        }
    }
}

/// One format/size combination the device supports, with its fastest frame
/// rate. The negotiated frame rate is [`PREFERRED_FPS`] when `max_fps` allows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub fourcc: FourCC,
    pub width: u32,
    pub height: u32,
    pub max_fps: f64,
}

impl Candidate {
    pub fn encoding(&self) -> Encoding {
        Encoding::from_fourcc(self.fourcc).expect("candidates only carry supported formats")
    }
}

/// The chosen format and frame rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Choice {
    pub fourcc: FourCC,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Choice {
    pub fn encoding(&self) -> Encoding {
        Encoding::from_fourcc(self.fourcc).expect("a choice is a supported format")
    }

    pub fn label(&self) -> String {
        format!(
            "{}x{} {} @ {} fps",
            self.width,
            self.height,
            self.encoding().label(),
            self.fps
        )
    }
}

/// Choose the format to open, or None when the device offers nothing usable
/// (then the caller reports what the camera can do).
pub fn choose(candidates: &[Candidate]) -> Option<Choice> {
    candidates
        .iter()
        .filter(|c| Encoding::from_fourcc(c.fourcc).is_some())
        .filter(|c| c.width % 2 == 0 && c.height % 2 == 0)
        .filter(|c| c.width > 0 && c.height > 0)
        .filter(|c| c.width <= MAX_WIDTH && c.height <= MAX_HEIGHT)
        .filter(|c| c.max_fps + 0.01 >= MIN_FPS)
        .max_by(|a, b| {
            rank(a)
                .partial_cmp(&rank(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|c| Choice {
            fourcc: c.fourcc,
            width: c.width,
            height: c.height,
            // 30 fps when the format reaches it; the fastest rate otherwise is
            // still 30 by the filter above.
            fps: PREFERRED_FPS,
        })
}

/// The sort key: resolution first, then a 16:9 frame at equal size (upstream's
/// 720p preset is 16:9), then the format, then the fastest rate.
fn rank(c: &Candidate) -> (u64, u8, u8, f64) {
    let pixels = u64::from(c.width) * u64::from(c.height);
    let wide = u8::from(u64::from(c.width) * 9 == u64::from(c.height) * 16);
    let pref = Encoding::from_fourcc(c.fourcc).map_or(0, Encoding::preference);
    (pixels, wide, pref, c.max_fps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(fourcc: &[u8; 4], width: u32, height: u32, max_fps: f64) -> Candidate {
        Candidate {
            fourcc: FourCC::new(fourcc),
            width,
            height,
            max_fps,
        }
    }

    #[test]
    fn highest_resolution_at_30fps_wins_over_uncompressed_slower_formats() {
        let candidates = [
            c(b"YUYV", 1920, 1080, 5.0),  // too slow at 1080p
            c(b"MJPG", 1920, 1080, 30.0), // usable
            c(b"YUYV", 1280, 720, 30.0),
        ];
        let chosen = choose(&candidates).unwrap();
        assert_eq!(chosen.fourcc, fourcc_mjpeg());
        assert_eq!((chosen.width, chosen.height), (1920, 1080));
        assert_eq!(chosen.fps, 30);
    }

    #[test]
    fn uncompressed_beats_mjpeg_at_the_same_size() {
        let candidates = [
            c(b"MJPG", 1920, 1080, 60.0),
            c(b"YUYV", 1920, 1080, 30.0),
            c(b"NV12", 1920, 1080, 30.0),
        ];
        assert_eq!(choose(&candidates).unwrap().fourcc, fourcc_yuyv());
        let candidates = [c(b"MJPG", 1920, 1080, 60.0), c(b"NV12", 1920, 1080, 30.0)];
        assert_eq!(choose(&candidates).unwrap().fourcc, fourcc_nv12());
    }

    #[test]
    fn never_more_than_1080p_and_never_odd() {
        let candidates = [c(b"MJPG", 3840, 2160, 30.0), c(b"YUYV", 1281, 721, 30.0)];
        assert_eq!(choose(&candidates), None);
        let candidates = [c(b"MJPG", 3840, 2160, 30.0), c(b"YUYV", 1280, 720, 30.0)];
        let chosen = choose(&candidates).unwrap();
        assert_eq!((chosen.width, chosen.height), (1280, 720));
    }

    #[test]
    fn sixteen_by_nine_at_the_same_pixel_count() {
        // 1024x900 and 1280x720 are both 921 600 pixels; the 16:9 frame is the
        // camera shape the overlay wants.
        let candidates = [c(b"YUYV", 1024, 900, 30.0), c(b"YUYV", 1280, 720, 30.0)];
        let chosen = choose(&candidates).unwrap();
        assert_eq!((chosen.width, chosen.height), (1280, 720));
    }

    #[test]
    fn unknown_formats_and_slow_cameras_are_rejected() {
        let candidates = [c(b"RGB3", 1920, 1080, 30.0), c(b"YUYV", 1920, 1080, 25.0)];
        assert_eq!(choose(&candidates), None);
        assert_eq!(choose(&[]), None);
    }

    #[test]
    fn encodings_map_to_the_contract_format() {
        for enc in [Encoding::Yuyv, Encoding::Nv12, Encoding::Mjpeg] {
            assert_eq!(enc.pixel_format(), crate::media_types::PixelFormat::Bgra);
            let fourcc = match enc {
                Encoding::Yuyv => b"YUYV",
                Encoding::Nv12 => b"NV12",
                Encoding::Mjpeg => b"MJPG",
            };
            assert_eq!(Encoding::from_fourcc(FourCC::new(fourcc)), Some(enc));
        }
        assert_eq!(Encoding::from_fourcc(FourCC::new(b"RGB3")), None);
    }
}
