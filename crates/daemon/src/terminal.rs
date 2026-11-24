//! A PTY-backed interactive terminal for the session.
//!
//! Flycod owns the pane on the session VM. Its foreground starts as the
//! configured shell; a
//! [`TerminalHarness`](flyco_core::ControlToDaemon::TerminalHarness)
//! command replaces it with the session's harness TUI, and when that
//! process exits the shell comes back — the pane always has a foreground.
//! Browser keystrokes arrive as
//! [`TerminalInput`](flyco_core::ControlToDaemon::TerminalInput); bytes the
//! child writes leave as
//! [`TerminalOutput`](flyco_core::DaemonToControl::TerminalOutput), and a
//! harness's natural exit leaves as
//! [`TerminalExited`](flyco_core::DaemonToControl::TerminalExited).
//!
//! Reading and lifecycle share one supervisor thread, and every
//! foreground gets a *fresh* PTY pair. Reusing a slave across sessions is
//! not portable: a BSD master reports `EIO` once the child's session ends
//! even while the slave fd stays open, and a revoked slave cannot become
//! another session's controlling terminal. Keystrokes and resizes are
//! therefore sent down the control channel too — the supervisor owns
//! whichever master is current, so a byte written mid-replacement lands
//! on the new foreground rather than a dying one.
//!
//! The same ordering gives [`TerminalEvent::Exited`] its guarantee: it is
//! sent only after the last byte the harness wrote has been read.

use std::future::Future;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem as _,
};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fd::{AsFd, BorrowedFd};
use rustix::io::fcntl_dupfd_cloexec;
use tokio::sync::mpsc;

use crate::privilege::{AgentUser, chown_path, drop_in_pre_exec};

/// How many output chunks may wait for a room that is not there.
const OUTPUT_DEPTH: usize = 256;

/// How often the supervisor wakes to check the foreground child and the
/// control channel when no output is arriving.
///
/// Output itself is read the moment `poll` reports it — this bounds only
/// the idle waits: how long a control message or a process exit can go
/// unnoticed.
const IDLE_POLL: Duration = Duration::from_millis(25);

/// How long the supervisor waits for a dying child's last bytes.
///
/// When `try_wait` reports an exit the kernel may still hold output the
/// child wrote; this is the drain deadline after which whatever remains is
/// abandoned. A hundred milliseconds is far longer than the flush takes in
/// practice and short enough that a harness's goodbye reaches the screen
/// before its exit does.
const DRAIN: Duration = Duration::from_millis(100);

/// How long a killed child may take to be reapable before the supervisor
/// stops waiting on it.
///
/// `wait` is never called: a session leader whose session still has
/// living members can sit in the kernel's exiting state far longer than
/// a signal takes to land, and a blocking reap would freeze the pane.
/// Children that outstay the deadline move to the reap-pending list,
/// which the loop keeps polling.
const REAP: Duration = Duration::from_secs(1);

/// How often a killed child is polled for reaping during the deadline.
const REAP_POLL: Duration = Duration::from_millis(10);

/// What the terminal's foreground produced.
#[derive(Debug)]
pub enum TerminalEvent {
    /// Bytes the foreground process wrote, UTF-8 lossy.
    Output(String),
    /// The harness exited on its own.
    ///
    /// Sent only for a harness's natural exit — the shell is ambient and
    /// its exits just respawn it, and a child killed to make room for
    /// the harness says nothing: the pane did not *lose* its foreground,
    /// it was given another.
    Exited {
        /// The exit code; `None` when a signal ended the process.
        code: Option<i32>,
    },
}

/// The control handle of a live web terminal.
pub trait TerminalSession: Send {
    /// Writes one chunk of keystrokes.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the terminal has stopped.
    fn write(&mut self, data: &str) -> Result<(), TerminalError>;

    /// Tells the PTY how many columns and rows the browser's pane shows.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the terminal has stopped.
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), TerminalError>;

    /// Puts `command` — the session's harness TUI — in the foreground,
    /// replacing the shell.
    ///
    /// An *ensure*: called while a harness already runs in the foreground,
    /// it does nothing, so re-attaching to a session cannot kill the turn
    /// on its screen. When the harness exits the shell returns.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the terminal has stopped.
    fn launch_harness(&mut self, command: CommandBuilder) -> Result<(), TerminalError>;

    /// Writes a line into the terminal's output stream, as if the
    /// foreground had printed it.
    ///
    /// How a launch failure reaches the user: the pane and a bridged CLI
    /// both render output, so "could not start claude" belongs there, not
    /// in a log only an operator reads.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the terminal has stopped.
    fn print(&mut self, text: &str) -> Result<(), TerminalError>;

    /// Stops the foreground child and closes the PTY.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the child could not be stopped.
    fn shutdown(&mut self) -> impl Future<Output = Result<(), TerminalError>> + Send;
}

/// The PTY could not be used.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// Opening or spawning the PTY failed.
    #[error("could not start the web terminal: {0}")]
    Pty(String),
    /// Writing keystrokes to the PTY failed.
    #[error("could not write to the web terminal")]
    Write(#[source] std::io::Error),
    /// The terminal has already been shut down.
    #[error("the web terminal has stopped")]
    Stopped,
}

/// What the supervisor's control channel carries.
enum Supervision {
    /// Keystrokes for the current foreground.
    Input(Vec<u8>),
    /// A new pane size; remembered for foregrounds spawned later.
    Resize(PtySize),
    /// Replace the foreground with the session's harness TUI.
    Harness(CommandBuilder),
    /// Kill the foreground child and end the supervisor.
    Stop,
}

/// A live PTY and the thread that supervises its foreground.
pub struct Terminal {
    /// Reports the line a failed launch prints — shares the supervisor's
    /// channel so the line lands in order with real output.
    events: mpsc::Sender<TerminalEvent>,
    control: std::sync::mpsc::SyncSender<Supervision>,
    /// Signals the supervisor thread to exit even while `events` is full.
    stop: Arc<AtomicBool>,
    /// Joined on shutdown, so the child's reap is done before the PTY
    /// goes away.
    thread: Option<thread::JoinHandle<()>>,
}

/// The size a PTY opens at, until the browser says what its pane shows.
///
/// Also what the shell sees if no browser ever opens the pane, so it is a
/// size a shell is comfortable at rather than a placeholder.
const INITIAL_SIZE: PtySize = PtySize {
    rows: 32,
    cols: 120,
    pixel_width: 0,
    pixel_height: 0,
};

/// What the foreground child is told it is running in.
///
/// Without `TERM` fish opens with a warning and falls back to plain
/// `xterm`; xterm.js in the browser renders 256 colours and truecolour,
/// so the shell — and a harness TUI — is told so.
const TERM: &str = "xterm-256color";
const COLORTERM: &str = "truecolor";

impl core::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Terminal").finish_non_exhaustive()
    }
}

impl Terminal {
    /// Spawns `shell` in `workdir` on a new PTY as the current user.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError::Pty`] if the PTY cannot be opened or the
    /// shell cannot be spawned.
    pub fn spawn(
        shell: &Path,
        workdir: &Path,
    ) -> Result<(Self, mpsc::Receiver<TerminalEvent>), TerminalError> {
        Self::spawn_as(shell, workdir, None)
    }

    /// Spawns `shell` in `workdir` on a new PTY, dropping privileges to `agent`
    /// (for both the shell and any harness TUI launched later) when `Some`.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError::Pty`] if the PTY cannot be opened or the
    /// shell cannot be spawned.
    pub fn spawn_as(
        shell: &Path,
        workdir: &Path,
        agent: Option<AgentUser>,
    ) -> Result<(Self, mpsc::Receiver<TerminalEvent>), TerminalError> {
        let shell = foreground_command(shell, workdir);
        let pane = Pane::open(&shell, INITIAL_SIZE, agent.as_ref())?;

        let (events, terminal_out) = mpsc::channel(OUTPUT_DEPTH);
        let (control, supervised) = std::sync::mpsc::sync_channel(64);
        let stop = Arc::new(AtomicBool::new(false));
        let supervised_events = events.clone();
        let supervised_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("flycod-terminal".to_owned())
            .spawn(move || {
                supervise(
                    pane,
                    &shell,
                    agent.as_ref(),
                    &supervised_events,
                    &supervised,
                    &supervised_stop,
                );
            })
            .map_err(|error| TerminalError::Pty(error.to_string()))?;

        Ok((
            Self {
                events,
                control,
                stop,
                thread: Some(thread),
            },
            terminal_out,
        ))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.control.try_send(Supervision::Stop);
    }
}

impl Terminal {
    fn send_control(&self, message: Supervision, label: &'static str) -> Result<(), TerminalError> {
        match self.control.try_send(message) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                tracing::warn!(
                    kind = label,
                    "terminal control queue is full; dropping message"
                );
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(TerminalError::Stopped),
        }
    }
}

impl TerminalSession for Terminal {
    fn write(&mut self, data: &str) -> Result<(), TerminalError> {
        self.send_control(Supervision::Input(data.as_bytes().to_vec()), "input")
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), TerminalError> {
        self.send_control(
            Supervision::Resize(PtySize {
                rows,
                cols,
                ..INITIAL_SIZE
            }),
            "resize",
        )
    }

    fn launch_harness(&mut self, command: CommandBuilder) -> Result<(), TerminalError> {
        self.send_control(Supervision::Harness(command), "harness")
    }

    fn print(&mut self, text: &str) -> Result<(), TerminalError> {
        match self.events.try_send(TerminalEvent::Output(text.to_owned())) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("terminal output buffer is full; dropping diagnostic line");
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TerminalError::Stopped),
        }
    }

    async fn shutdown(&mut self) -> Result<(), TerminalError> {
        self.stop.store(true, Ordering::Release);
        let _ = self.control.try_send(Supervision::Stop);
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || thread.join())
                .await
                .map_err(|_| {
                    TerminalError::Pty("the terminal shutdown task was cancelled".to_owned())
                })?
                .map_err(|_| TerminalError::Pty("the supervisor thread panicked".to_owned()))?;
        }
        Ok(())
    }
}

/// The command a PTY foreground runs, with the environment every child
/// shares.
fn foreground_command(program: &Path, workdir: &Path) -> CommandBuilder {
    let mut command = CommandBuilder::new(program);
    command.cwd(workdir);
    command.env("TERM", TERM);
    command.env("COLORTERM", COLORTERM);
    command
}

/// Non-blocking wait result for a [`PaneChild`].
enum ChildExit {
    Running,
    Exited(Option<i32>),
}

/// A child living on a PTY slave: either spawned directly through
/// `portable_pty` (developer-machine mode) or via `std::process::Command` with
/// an explicit `pre_exec` privilege drop to [`AgentUser`].
enum PaneChild {
    Pty(Box<dyn Child + Send + Sync>),
    Std(std::process::Child),
}

impl PaneChild {
    fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Pty(child) => child.kill(),
            Self::Std(child) => child.kill(),
        }
    }

    fn try_wait(&mut self) -> std::io::Result<ChildExit> {
        match self {
            Self::Pty(child) => Ok(child.try_wait()?.map_or(ChildExit::Running, |status| {
                ChildExit::Exited(exit_code(&status))
            })),
            Self::Std(child) => Ok(child.try_wait()?.map_or(ChildExit::Running, |status| {
                ChildExit::Exited(std_exit_code(status))
            })),
        }
    }
}

fn std_exit_code(status: std::process::ExitStatus) -> Option<i32> {
    if status.signal().is_some() {
        None
    } else {
        status.code()
    }
}

/// One foreground: a PTY pair, the child living on its slave, and a dup
/// of the master the supervisor polls.
///
/// The slave is dropped the moment the child has its stdio — the pair's
/// only long-lived half is the master, so the child's session owns the
/// terminal outright and its death is visible as a quiet master.
struct Pane {
    /// Resizes; also the fd `poll` is a dup of.
    master: Box<dyn MasterPty + Send>,
    /// Keystrokes go here.
    writer: Box<dyn Write + Send>,
    /// Output is read here.
    reader: Box<dyn Read + Send>,
    /// A pollable dup of the master's fd.
    poll: rustix::fd::OwnedFd,
    child: PaneChild,
}

impl Pane {
    /// Opens a PTY at `size` and spawns `command` on it, dropping privileges
    /// to `agent` when present.
    fn open(
        command: &CommandBuilder,
        size: PtySize,
        agent: Option<&AgentUser>,
    ) -> Result<Self, TerminalError> {
        let pair = NativePtySystem::default()
            .openpty(size)
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        let child = if let Some(user) = agent {
            let tty_name = pair.master.tty_name().ok_or_else(|| {
                TerminalError::Pty("the PTY master reported no slave path".to_owned())
            })?;
            if user.needs_uid_switch() {
                chown_path(&tty_name, Some(user.uid), Some(user.gid))
                    .map_err(|error| TerminalError::Pty(error.to_string()))?;
                std::fs::set_permissions(&tty_name, std::fs::Permissions::from_mode(0o600))
                    .map_err(|error| TerminalError::Pty(error.to_string()))?;
            }
            spawn_pty_as_agent(&tty_name, command, user)?
        } else {
            let child = pair
                .slave
                .spawn_command(command.clone())
                .map_err(|error| TerminalError::Pty(error.to_string()))?;
            PaneChild::Pty(child)
        };
        // The child holds the slave as its stdio and controlling tty;
        // our copy would only delay the master's hangup.
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        // The supervisor polls its own dup of the master: readable means
        // the shared file description has bytes, whoever holds the fd.
        // SAFETY: `master` owns the fd and outlives the dup call.
        let poll = pair
            .master
            .as_raw_fd()
            .map(|raw| unsafe { BorrowedFd::borrow_raw(raw) })
            .and_then(|borrowed| fcntl_dupfd_cloexec(borrowed, 0).ok())
            .ok_or_else(|| TerminalError::Pty("the PTY master has no fd to poll".to_owned()))?;
        Ok(Self {
            master: pair.master,
            writer,
            reader,
            poll,
            child,
        })
    }
}

/// Spawns `builder` on `tty_name` after establishing a new session, dropping
/// privileges to `agent`, and taking `tty_name` as the controlling terminal.
fn spawn_pty_as_agent(
    tty_name: &Path,
    builder: &CommandBuilder,
    agent: &AgentUser,
) -> Result<PaneChild, TerminalError> {
    let command_line = builder.get_argv();
    let Some((program, args)) = command_line.split_first() else {
        return Err(TerminalError::Pty("empty terminal command".to_owned()));
    };
    let slave_in = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(tty_name)
        .map_err(|error| TerminalError::Pty(error.to_string()))?;
    let slave_out = slave_in
        .try_clone()
        .map_err(|error| TerminalError::Pty(error.to_string()))?;
    let slave_err = slave_in
        .try_clone()
        .map_err(|error| TerminalError::Pty(error.to_string()))?;

    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    if let Some(cwd) = builder.get_cwd() {
        cmd.current_dir(cwd);
    }
    let extra_env: Vec<(String, String)> = builder
        .iter_extra_env_as_str()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let env = agent.sanitized_env(std::env::vars_os(), extra_env);
    cmd.env_clear().envs(env);
    cmd.stdin(Stdio::from(slave_in))
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(slave_err));

    let controlling_tty = builder.get_controlling_tty();
    let drop_spec = agent.drop_spec();
    // SAFETY: `pre_exec` invokes only async-signal-safe POSIX/Linux syscalls
    // (`signal`, `sigprocmask`, `setsid`, `setgroups`/`setgid`/`setuid`,
    // `prctl`, `ioctl(TIOCSCTTY)`).
    unsafe {
        cmd.pre_exec(move || {
            for signo in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
            ] {
                libc::signal(signo, libc::SIG_DFL);
            }
            let empty_set: libc::sigset_t = std::mem::zeroed();
            libc::sigprocmask(
                libc::SIG_SETMASK,
                &raw const empty_set,
                std::ptr::null_mut(),
            );
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            drop_in_pre_exec(drop_spec)?;
            if controlling_tty {
                #[allow(clippy::cast_lossless)]
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .map_err(|error| TerminalError::Pty(error.to_string()))?;
    Ok(PaneChild::Std(child))
}

/// The `poll` timeout as a kernel `timespec`.
fn timeout(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(duration.subsec_nanos()),
    }
}

/// Whether the master is readable within `within`.
fn readable<Fd: AsFd>(fd: &Fd, within: &Timespec) -> bool {
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    matches!(rustix::event::poll(&mut fds, Some(within)), Ok(n) if n > 0)
}

/// What one read of the master found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// Bytes were read and passed on.
    Chunk,
    /// Nothing there. `Ok(0)` means the child's session ended and no
    /// slave remains open — a genuine EOF — but it is still not a
    /// reason for the supervisor to die: exits are detected by
    /// `try_wait`, and the next foreground gets a fresh master anyway.
    /// Non-`Ok(0)` errors land here too — the poll loop paces retries
    /// and the control channel still ends the thread.
    Quiet,
    /// The output channel is gone; nobody will read again.
    Gone,
}

/// Sends one terminal event without blocking past a shutdown signal.
fn send_event(
    events: &mpsc::Sender<TerminalEvent>,
    stop: &AtomicBool,
    mut event: TerminalEvent,
) -> bool {
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match events.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(returned)) => {
                event = returned;
                thread::sleep(IDLE_POLL);
            }
        }
    }
}

/// Reads one chunk into `events`.
fn read_out(
    reader: &mut dyn Read,
    buf: &mut [u8],
    events: &mpsc::Sender<TerminalEvent>,
    stop: &AtomicBool,
) -> Reading {
    match reader.read(buf) {
        Ok(0) | Err(_) => Reading::Quiet,
        Ok(n) => {
            let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
            if send_event(events, stop, TerminalEvent::Output(chunk)) {
                Reading::Chunk
            } else {
                Reading::Gone
            }
        }
    }
}

/// What the foreground is, for the supervisor's two policies: a second
/// harness launch is dropped rather than restarting a live TUI, and only
/// a harness's exit is reported — the shell's just respawns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Foreground {
    /// The configured shell — what the pane starts on and returns to.
    Shell,
    /// The session's harness TUI.
    Harness,
}

/// Kills `child` and gives it [`REAP`] to die; `false` if it is still not
/// reapable and must join the pending list instead.
fn reaped(child: &mut PaneChild) -> bool {
    let _ = child.kill();
    let deadline = Instant::now() + REAP;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(ChildExit::Exited(_)) | Err(_) => return true,
            Ok(ChildExit::Running) => thread::sleep(REAP_POLL),
        }
    }
    false
}

/// Opens the harness pane `command` asks for, or prints the failure into
/// the terminal and opens the shell back. `None` when even the shell
/// refused to start — the pane is dead either way.
fn foreground_pane(
    command: &CommandBuilder,
    shell: &CommandBuilder,
    agent: Option<&AgentUser>,
    size: PtySize,
    events: &mpsc::Sender<TerminalEvent>,
    stop: &AtomicBool,
) -> Option<(Pane, Foreground)> {
    match Pane::open(command, size, agent) {
        Ok(pane) => Some((pane, Foreground::Harness)),
        Err(error) => {
            let _ = send_event(
                events,
                stop,
                TerminalEvent::Output(format!(
                    "\r\nflycod: could not start the harness: {error}\r\n"
                )),
            );
            Pane::open(shell, size, agent)
                .ok()
                .map(|pane| (pane, Foreground::Shell))
        }
    }
}

/// The exit status as the wire reports it: `None` when a signal ended the
/// process, its code otherwise.
fn exit_code(status: &ExitStatus) -> Option<i32> {
    if status.signal().is_some() {
        None
    } else {
        Some(i32::try_from(status.exit_code()).unwrap_or(-1))
    }
}

/// Reads the pane and watches its foreground child, one thread for both.
///
/// A blocking `read` could never report an exit — the pane's lifecycle
/// is `try_wait` — and two threads racing the same channel could let
/// `Exited` overtake the child's last bytes. One thread that polls is
/// the shape that has neither problem.
fn supervise(
    mut pane: Pane,
    shell: &CommandBuilder,
    agent: Option<&AgentUser>,
    events: &mpsc::Sender<TerminalEvent>,
    control: &std::sync::mpsc::Receiver<Supervision>,
    stop_flag: &AtomicBool,
) {
    let mut foreground = Foreground::Shell;
    let mut size = INITIAL_SIZE;
    // Killed children that outlived their reap deadline; still polled so
    // the process table stays clean.
    let mut pending: Vec<PaneChild> = Vec::new();
    let mut buf = [0_u8; 4096];
    let idle = timeout(IDLE_POLL);
    let drain = timeout(DRAIN);

    loop {
        if stop_flag.load(Ordering::Acquire) {
            let _ = pane.child.kill();
            return;
        }
        if readable(&pane.poll, &idle) {
            match read_out(&mut *pane.reader, &mut buf, events, stop_flag) {
                Reading::Chunk => {}
                Reading::Quiet => {
                    // A dead session's master can stay pollable (HUP
                    // counts as an event) with nothing to read; without
                    // a pause that state is a busy loop.
                    thread::sleep(IDLE_POLL);
                }
                Reading::Gone => {
                    let _ = pane.child.kill();
                    return;
                }
            }
        }

        let mut stop = stop_flag.load(Ordering::Acquire);
        for message in control.try_iter() {
            match message {
                Supervision::Input(bytes) => {
                    if let Err(error) = pane.writer.write_all(&bytes) {
                        tracing::warn!(%error, "terminal input dropped: the pane is gone");
                    }
                }
                Supervision::Resize(next) => {
                    size = next;
                    if let Err(error) = pane.master.resize(next) {
                        tracing::warn!(%error, "terminal resize refused by the pane");
                    }
                }
                Supervision::Harness(command) => {
                    if foreground == Foreground::Harness {
                        continue;
                    }
                    // The pane is dead with its child: destructuring drops
                    // the master and writer along with it rather than
                    // leaving a revoked pair half-held.
                    let Pane { mut child, .. } = pane;
                    if !reaped(&mut child) {
                        pending.push(child);
                    }
                    match foreground_pane(&command, shell, agent, size, events, stop_flag) {
                        Some((next, became)) => {
                            pane = next;
                            foreground = became;
                        }
                        None => return,
                    }
                }
                Supervision::Stop => stop = true,
            }
        }
        if stop {
            let _ = pane.child.kill();
            return;
        }

        pending.retain_mut(|child| matches!(child.try_wait(), Ok(ChildExit::Running)));

        // A wait that fails is a child we can no longer see — reported as
        // a lost foreground rather than silently ignored.
        let exited = match pane.child.try_wait() {
            Ok(ChildExit::Exited(code)) => Some(code),
            Err(_) => Some(None),
            Ok(ChildExit::Running) => None,
        };
        if let Some(code) = exited {
            // Drain what the child left before saying so: `Exited` must
            // not overtake the last bytes it wrote. A quiet read — the
            // session ending — ends the drain as surely as the deadline.
            while readable(&pane.poll, &drain)
                && read_out(&mut *pane.reader, &mut buf, events, stop_flag) == Reading::Chunk
            {
            }
            if foreground == Foreground::Harness
                && !send_event(events, stop_flag, TerminalEvent::Exited { code })
            {
                return;
            }
            if stop_flag.load(Ordering::Acquire) {
                return;
            }
            match Pane::open(shell, size, agent) {
                Ok(next) => {
                    pane = next;
                    foreground = Foreground::Shell;
                }
                Err(error) => {
                    tracing::error!(%error, "the terminal's shell could not be restarted");
                    return;
                }
            }
        }
    }
}

/// What the relay asked a [`FakeTerminal`] to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalCall {
    /// Keystrokes written to the foreground.
    Write(String),
    /// The pane's size, columns then rows.
    Resize(u16, u16),
    /// The harness TUI was asked into the foreground.
    LaunchHarness,
    /// A line was printed into the output stream.
    Print(String),
}

/// A stand-in terminal for relay tests.
pub struct FakeTerminal {
    calls: mpsc::UnboundedSender<TerminalCall>,
}

impl core::fmt::Debug for FakeTerminal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FakeTerminal").finish_non_exhaustive()
    }
}

impl FakeTerminal {
    /// A pair: the handle the relay owns, and the stream of calls it made.
    #[must_use]
    pub fn pair() -> (
        Self,
        mpsc::UnboundedReceiver<TerminalCall>,
        mpsc::Sender<TerminalEvent>,
        mpsc::Receiver<TerminalEvent>,
    ) {
        let (calls, received) = mpsc::unbounded_channel();
        let (output_tx, outputs) = mpsc::channel(OUTPUT_DEPTH);
        (Self { calls }, received, output_tx, outputs)
    }
}

impl TerminalSession for FakeTerminal {
    fn write(&mut self, data: &str) -> Result<(), TerminalError> {
        self.calls
            .send(TerminalCall::Write(data.to_owned()))
            .map_err(|_| TerminalError::Stopped)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), TerminalError> {
        self.calls
            .send(TerminalCall::Resize(cols, rows))
            .map_err(|_| TerminalError::Stopped)
    }

    fn launch_harness(&mut self, _command: CommandBuilder) -> Result<(), TerminalError> {
        self.calls
            .send(TerminalCall::LaunchHarness)
            .map_err(|_| TerminalError::Stopped)
    }

    fn print(&mut self, text: &str) -> Result<(), TerminalError> {
        self.calls
            .send(TerminalCall::Print(text.to_owned()))
            .map_err(|_| TerminalError::Stopped)
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<(), TerminalError>> {
        core::future::ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{Terminal, TerminalEvent, TerminalSession as _};
    use crate::privilege::{AgentUser, current_egid, current_euid};

    #[tokio::test]
    async fn spawn_as_sanitizes_pty_environment_and_runs_shell() {
        let agent = AgentUser {
            name: "flyco-pty-test".to_owned(),
            uid: current_euid(),
            gid: current_egid(),
            home: PathBuf::from("/tmp/flyco-pty-home"),
            shell: PathBuf::from("/bin/sh"),
        };
        let (mut term, mut events) =
            Terminal::spawn_as(Path::new("/bin/sh"), &std::env::temp_dir(), Some(agent))
                .expect("spawn PTY with agent user");
        term.write("printf 'PTY_HOME=%s\\n' \"$HOME\"\n")
            .expect("write command");

        let mut output = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !output.contains("PTY_HOME=/tmp/flyco-pty-home") {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for PTY output: {output:?}"
            );
            if let Ok(Some(TerminalEvent::Output(chunk))) =
                tokio::time::timeout(remaining, events.recv()).await
            {
                output.push_str(&chunk);
            }
        }
        term.shutdown().await.expect("clean shutdown");
    }
}
