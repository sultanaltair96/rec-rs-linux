//! Recording sinks and inputs: the file writer, the probe, the pause timeline.
//!
//! Phase 0 owns the writer v0 (GStreamer appsrc → H.264/AAC → fragmented
//! MP4/MOV) and the probe. Phase 1C extends this module with the full writer
//! (hardware encoders, track grouping, the mix track) and the GPU compositor
//! output; anything in here must keep working for the CLI's self-tests while
//! that happens.

pub mod pauses;
pub mod probe;
pub mod writer;

pub use writer::{AudioLayout, Codec, Stats, VideoSpec, Writer};
