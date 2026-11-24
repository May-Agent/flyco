//! Low-level terminal controls for the interactive ACP/SSE client.
//!
//! Puts stdin into raw mode (leaving `OPOST` enabled so `\n` translates to
//! `\r\n` cleanly) and restores the original `termios` on drop, including on
//! error or unwind.

use std::io::IsTerminal as _;

use rustix::termios::{
    ControlModes, InputModes, LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr,
    tcsetattr,
};

use crate::{Failure, Outcome};

/// Local terminal raw-mode guard, restoring the saved `termios` on drop.
///
/// Unlike a raw PTY bridge, output post-processing (`OPOST`) is preserved so
/// ordinary `\n` newlines emitted by the renderer translate cleanly to
/// `\r\n`. `ISIG` is disabled so `Ctrl-C` arrives on stdin as `0x03` and can
/// trigger `POST /v1/sessions/{id}/interrupt` instead of killing the client.
pub struct RawMode {
    fd: std::os::fd::BorrowedFd<'static>,
    saved: Termios,
}

impl core::fmt::Debug for RawMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RawMode").finish_non_exhaustive()
    }
}

impl RawMode {
    /// Enters raw input mode on stdin and returns a guard that restores it.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) if `tcgetattr` or `tcsetattr` fails.
    pub fn enter() -> Outcome<Self> {
        use std::os::fd::AsRawFd as _;
        // SAFETY: stdin stays open for the lifetime of the process, and this
        // guard borrows the raw descriptor strictly within that lifetime.
        let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(std::io::stdin().as_raw_fd()) };
        let saved =
            tcgetattr(fd).map_err(|error| Failure::transport(format!("tcgetattr: {error}")))?;
        let mut raw = saved.clone();
        raw.input_modes -= InputModes::BRKINT
            | InputModes::ICRNL
            | InputModes::INPCK
            | InputModes::ISTRIP
            | InputModes::IXON;
        raw.control_modes |= ControlModes::CS8;
        raw.local_modes -=
            LocalModes::ECHO | LocalModes::ICANON | LocalModes::IEXTEN | LocalModes::ISIG;
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        tcsetattr(fd, OptionalActions::Now, &raw)
            .map_err(|error| Failure::transport(format!("tcsetattr raw: {error}")))?;
        Ok(Self { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, OptionalActions::Now, &self.saved);
    }
}

/// Whether ANSI styling should be emitted to stdout.
#[must_use]
pub fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").ok().as_deref() != Some("dumb")
}
