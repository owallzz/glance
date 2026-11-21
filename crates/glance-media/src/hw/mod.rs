//! Hardware video encoders, loaded at run time so glance still starts (and
//! falls back to OpenH264) on machines without them.

#[cfg(target_os = "linux")]
mod h264_headers;
#[cfg(target_os = "windows")]
pub mod mediafoundation;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod nv12;
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod nvenc;
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unsafe_op_in_unsafe_fn,
    unused_imports,
    unnecessary_transmutes,
    clippy::all
)]
mod nvenc_sys;
#[cfg(target_os = "linux")]
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unsafe_op_in_unsafe_fn,
    unused_imports,
    unnecessary_transmutes,
    clippy::all
)]
mod va_sys;
#[cfg(target_os = "linux")]
pub mod vaapi;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

/// The hardware backends `GLANCE_ENCODER` can name.
pub(crate) const BACKENDS: [&str; 4] = ["nvenc", "vaapi", "videotoolbox", "mediafoundation"];

/// `GLANCE_ENCODER=nvenc|vaapi|videotoolbox|mediafoundation` limits
/// hardware encoding to that one backend (testing aid).
pub(crate) fn allowed(name: &str) -> bool {
    match std::env::var("GLANCE_ENCODER") {
        Ok(v) if BACKENDS.contains(&v.as_str()) => v == name,
        _ => true,
    }
}
