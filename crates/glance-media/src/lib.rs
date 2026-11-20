//! Capture, encode/decode and audio pipelines with per-OS backends.
//!
//! Frames and display-size scaling, H.264 encode/decode (NVENC or OpenH264
//! to encode, OpenH264 to decode), and
//! screen capture (zed-scap: portal+PipeWire on Wayland, X11, ScreenCaptureKit,
//! Windows Graphics Capture).

pub mod audio;
pub mod camera;
pub mod capture;
pub mod codec;
pub mod frame;
pub mod hw;
pub mod pattern;
pub mod pipeline;
pub mod stream_audio;

pub use frame::{FrameScaler, TargetSize, VideoFrame, fit_within};
