//! Capture, encode/decode and audio pipelines with per-OS backends.
//!
//! So far: the decoded-frame type and display-size scaling (M1).

pub mod frame;
pub mod pattern;

pub use frame::{FrameScaler, TargetSize, VideoFrame, fit_within};
