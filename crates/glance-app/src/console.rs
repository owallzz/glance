//! Terminal output for the Windows GUI build.
//!
//! glance.exe is a GUI-subsystem program (see `main.rs`), so Windows gives it
//! no console: nothing pops up when it starts, but `glance --version`,
//! `glance update` or `glance bot` run from a terminal would print nothing.
//! [`attach`] borrows the console of the terminal that started us, when there
//! is one. The terminal doesn't wait for a GUI program, so the prompt may come
//! back before the output (`start /wait glance ...` in cmd, or piping through
//! `| Out-Host` in PowerShell, waits for it).
//!
//! Elsewhere this does nothing: macOS and Linux never open a terminal for an
//! app started from a bundle or a desktop entry (`Terminal=false`).

/// Attach to the parent process's console if our output goes nowhere.
/// Output already redirected (a pipe, a file, `cargo run`) is left alone.
pub fn attach() {
    #[cfg(windows)]
    windows::attach();
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;

    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
    }

    fn missing(id: u32) -> bool {
        // SAFETY: GetStdHandle only reads the process's handle table.
        let handle = unsafe { GetStdHandle(id) };
        handle.is_null() || handle as isize == -1
    }

    pub fn attach() {
        if missing(STD_OUTPUT_HANDLE) && missing(STD_ERROR_HANDLE) {
            // SAFETY: no arguments to get wrong; it fails harmlessly when the
            // parent has no console (started from Explorer).
            unsafe {
                AttachConsole(ATTACH_PARENT_PROCESS);
            }
        }
    }
}
