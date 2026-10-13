//! rg's message classes, process-wide (rg keeps the same three flags in
//! its own `messages` module).
//!
//! - [`error`]: a per-file or per-directory problem (unreadable file or
//!   directory, a dangling link under -L, a failing --pre command, a
//!   corrupt -z stream). The search goes on, but the run has errored: the
//!   exit code becomes 2 unless -q found a match. --no-messages hides the
//!   text, never the exit code.
//! - [`ignore_warning`]: a problem reading or parsing an ignore file (a
//!   missing --ignore-file, say). Printed unless --no-messages or
//!   --no-ignore-messages; never changes the exit code.

use std::sync::atomic::{AtomicBool, Ordering};

static MESSAGES: AtomicBool = AtomicBool::new(true);
static IGNORE_MESSAGES: AtomicBool = AtomicBool::new(true);
static ERRORED: AtomicBool = AtomicBool::new(false);

/// Apply --no-messages / --no-ignore-messages (called once from `run`).
pub fn configure(messages: bool, ignore_messages: bool) {
    MESSAGES.store(messages, Ordering::Relaxed);
    IGNORE_MESSAGES.store(ignore_messages, Ordering::Relaxed);
}

/// True once any [`error`] was reported during this run.
pub fn errored() -> bool {
    ERRORED.load(Ordering::Relaxed)
}

/// Report a per-file or per-walk error: sets the errored flag, prints
/// `glep: <msg>` unless --no-messages.
pub fn error(msg: impl std::fmt::Display) {
    ERRORED.store(true, Ordering::Relaxed);
    if MESSAGES.load(Ordering::Relaxed) {
        eprintln!("glep: {msg}");
    }
}

/// Report an ignore-file problem: prints `glep: <msg>` unless
/// --no-messages or --no-ignore-messages, and leaves the exit code alone.
pub fn ignore_warning(msg: impl std::fmt::Display) {
    if MESSAGES.load(Ordering::Relaxed) && IGNORE_MESSAGES.load(Ordering::Relaxed) {
        eprintln!("glep: {msg}");
    }
}

/// The operating system's own text for a missing path, as rg prints it:
/// "No such file or directory (os error 2)" on Unix, "The system cannot
/// find the file specified. (os error 2)" on Windows.
pub fn not_found() -> std::io::Error {
    std::io::Error::from_raw_os_error(2)
}
