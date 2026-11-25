//! Recording sinks and inputs: the file writer, the probe, the pause timeline,
//! the MP4 box surgery and the synthetic test sources.
//!
//! The writer (GStreamer appsrc -> H.264/HEVC/ProRes + AAC -> fragmented
//! MP4/MOV, finalised into a regular movie) and the probe live here; anything
//! in this module must keep working for the CLI's self-tests.

pub mod disk;
pub mod mp4;
pub mod pauses;
pub mod probe;
pub mod testsrc;
pub mod writer;

pub use writer::{AudioLayout, Codec, Stats, VideoSpec, Writer};
