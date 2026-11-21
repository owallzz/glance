//! Room and contacts state machines and the session controller (no GUI, no I/O).
//!
//! So far: the view-model types the UI renders (M2) and demo data. The room
//! and contacts state machines that produce them arrive with M3/M3b.

pub mod contacts;
pub mod demo;
pub mod format;
pub mod model;
pub mod voice;
pub mod rooms;

pub use model::*;
