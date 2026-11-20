//! Hardware video encoders, loaded at run time so glance still starts (and
//! falls back to OpenH264) on machines without them.

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
