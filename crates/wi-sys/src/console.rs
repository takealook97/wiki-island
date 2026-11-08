//! Hidden console input.

use std::io;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle,
    STD_INPUT_HANDLE, SetConsoleCtrlHandler, SetConsoleMode,
};
use windows_sys::core::BOOL;

/// The console input handle (as an address) and the mode to restore,
/// while an [`EchoOff`] is active.
static SAVED: Mutex<Option<(usize, u32)>> = Mutex::new(None);

/// Restore the saved console mode once; later calls do nothing.
fn restore() {
    let saved = match SAVED.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    if let Some((handle, mode)) = saved {
        // SAFETY: the handle is the process's console input handle, which
        // stays valid for the process lifetime; the mode was read from it.
        unsafe { SetConsoleMode(handle as HANDLE, mode) };
    }
}

unsafe extern "system" fn on_ctrl(ctrl: u32) -> BOOL {
    if ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT {
        restore();
    }
    // Not handled: continue with the next handler (default: exit).
    0
}

/// Turns off echo on the console attached to stdin until dropped.
///
/// Dropping restores the exact saved mode. A Ctrl+C or Ctrl+Break handler
/// restores it too before the default handling ends the process. Only one
/// may be active at a time.
#[derive(Debug)]
pub struct EchoOff {
    _private: (),
}

impl EchoOff {
    /// Disable echo on the stdin console. Fails when stdin is not a
    /// console (callers keep their piped-input path), when another
    /// `EchoOff` is active, or when the mode cannot be changed.
    pub fn new() -> io::Result<EchoOff> {
        // SAFETY: GetStdHandle has no pointer arguments.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdin is not a console",
            ));
        }
        let mut mode = 0u32;
        // SAFETY: `mode` is a writable output; a non-console handle makes
        // the call fail without side effects.
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdin is not a console",
            ));
        }
        {
            let mut saved = SAVED.lock().unwrap_or_else(|p| p.into_inner());
            if saved.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "console echo is already off",
                ));
            }
            *saved = Some((handle as usize, mode));
        }
        // SAFETY: `on_ctrl` is a valid handler for the process lifetime.
        if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) } == 0 {
            let e = io::Error::last_os_error();
            SAVED.lock().unwrap_or_else(|p| p.into_inner()).take();
            return Err(e);
        }
        // SAFETY: `handle` is the console input handle checked above.
        if unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) } == 0 {
            let e = io::Error::last_os_error();
            restore();
            // SAFETY: removes the handler registered above.
            unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 0) };
            return Err(e);
        }
        Ok(EchoOff { _private: () })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        restore();
        // SAFETY: removes the handler registered in `new`.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 0) };
    }
}
