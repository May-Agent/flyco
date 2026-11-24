//! Interactive ACP/SSE session driver.
//!
//! Replaces the legacy PTY harness bridge: every interactive harness
//! (`claude`, `codex`, `devin`, `antigravity`, and `resume`) runs through the
//! same control-plane REST + SSE conversation protocol used by the Web PWA:
//!
//! * one shared SSE stream (`GET /v1/events?session={id}` via [`Follow`]) with
//!   chunk-level `: ping` silence detection, capped exponential full-jitter
//!   backoff, `429 Retry-After` handling, and immediate stop on budget
//!   exhaustion;
//! * bounded history catch-up on `resume` (`GET /v1/sessions/{id}/events`,
//!   capped at [`crate::follow::MAX_PAGES`] with cursor-advance enforcement);
//! * local line editing with batched single-request message/shell submission
//!   (`POST /v1/sessions/{id}/messages`, `POST /v1/sessions/{id}/shell`),
//!   in-band approval decisions (`POST /v1/approvals/{id}/decision`), and
//!   turn interrupts (`POST /v1/sessions/{id}/interrupt`).

pub mod editor;
pub mod render;
mod term;

use std::collections::VecDeque;
use std::io::Write;
use std::pin::Pin;

use flyco_core::wire::{ApprovalDecision, ClientEvent};
use flyco_core::{
    ApprovalId, ApprovalView, DecideApproval, RunShell, SendMessage, SessionDetail, SessionId,
    SessionState,
};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::client::Api;
use crate::follow::{Follow, Item};
use crate::interactive::editor::{Key, KeyDecoder, LineEditor};
use crate::interactive::render::Renderer;
use crate::{Exit, Failure, Outcome, out};

/// Read buffer size for stdin keystroke chunks.
const STDIN_CHUNK: usize = 1024;

/// How an interactive session ended locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The user detached (`Ctrl-D`, `Ctrl-]`, `/detach`); the session keeps
    /// running on its machine.
    Detached,
    /// Stdin closed (EOF); detached cleanly.
    InputEnded,
    /// The session transitioned to [`SessionState::Failed`].
    SessionFailed,
    /// The session transitioned to [`SessionState::Archived`].
    SessionArchived,
}

/// One outbound REST command queued from the interactive prompt.
#[derive(Debug, Clone)]
enum Outbound {
    Message {
        text: String,
        draft: String,
    },
    Shell {
        command: String,
        draft: String,
    },
    Interrupt,
    Compact,
    ContextUsage,
    Approve {
        id: ApprovalId,
        decision: ApprovalDecision,
    },
}

impl Outbound {
    const fn draft(&self) -> Option<&str> {
        match self {
            Self::Message { draft, .. } | Self::Shell { draft, .. } => Some(draft.as_str()),
            Self::Interrupt | Self::Compact | Self::ContextUsage | Self::Approve { .. } => None,
        }
    }
}

/// Attaches the local terminal to a session over the control-plane ACP/SSE
/// interface and runs until detach, EOF, session failure, or fatal API error.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on terminal setup failure, catch-up
/// failure, stream failure, or control-plane budget exhaustion.
pub async fn attach(
    api: &Api,
    detail: SessionDetail,
    resume: bool,
    mode: out::Mode,
) -> Outcome<Exit> {
    let id = detail.summary.id;
    let color = term::use_color();
    let raw = term::RawMode::enter()?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = std::io::stdout();
    let outcome = attach_io(api, detail, resume, color, &mut stdin, &mut stdout).await;
    // Drop raw mode before emitting the final note on stderr.
    drop(raw);
    match outcome? {
        Ended::Detached => {
            if mode == out::Mode::Human {
                let _ =
                    out::raw_err(format!("detached — `flyco resume {id}` re-enters\n").as_bytes());
            }
            Ok(Exit::Ok)
        }
        Ended::InputEnded | Ended::SessionArchived => Ok(Exit::Ok),
        Ended::SessionFailed => Ok(Exit::Failed),
    }
}

/// Testable core of [`attach`], generic over stdin/stdout streams.
///
/// Opens strictly one [`Follow`] SSE stream after bounded history catch-up (on
/// `resume`) and initial pending-approval reconciliation.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on catch-up, stream, I/O, or fatal
/// control-plane errors (including `request-budget-exhausted` and
/// `row-budget-exhausted`).
pub async fn attach_io<R, W>(
    api: &Api,
    detail: SessionDetail,
    resume: bool,
    color: bool,
    stdin: &mut R,
    stdout: &mut W,
) -> Outcome<Ended>
where
    R: AsyncRead + Unpin,
    W: Write,
{
    let id = detail.summary.id;
    let mut renderer = Renderer::new(detail, color);
    write_all(stdout, renderer.render_header().as_bytes())?;

    // On resume, page recorded history under AGENTS.md rule #3 (MAX_PAGES cap
    // and strict cursor advance) before opening the single live SSE stream.
    let cursor = if resume {
        catch_up(api, id, &mut renderer, stdout).await?
    } else {
        0
    };

    // Reconcile any approvals still pending on the control plane.
    let pending: Vec<ApprovalView> = api
        .get(&format!("/v1/approvals?session={id}&state=pending"))
        .await?;
    let cards = renderer.render_pending_approvals(pending);
    if !cards.is_empty() {
        write_all(stdout, cards.as_bytes())?;
    }

    // Single shared SSE stream (AGENTS.md rules #1, #2, #4, #5).
    let mut follow = Follow::new(api, id).after(cursor);
    follow.connect().await?;

    drive(api, id, &mut follow, &mut renderer, stdin, stdout).await
}

/// Fetches recorded events from `after = 0` with bounded pagination
/// (`MAX_PAGES` cap and advancing-cursor check per `AGENTS.md` rule #3) and
/// renders them into `stdout`.
async fn catch_up<W: Write>(
    api: &Api,
    session: SessionId,
    renderer: &mut Renderer,
    stdout: &mut W,
) -> Outcome<u64> {
    let mut cursor = 0_u64;
    let mut pages = 0_u32;
    loop {
        let page = crate::follow::events(api, session, cursor).await?;
        let advanced = page.events.iter().any(|stored| stored.seq > cursor);
        for stored in page.events {
            if stored.seq <= cursor {
                continue;
            }
            cursor = stored.seq;
            let text = renderer.apply_item(&Item::Replayed(stored), true);
            if !text.is_empty() {
                write_all(stdout, text.as_bytes())?;
            }
        }
        if !page.more {
            break;
        }
        if !advanced {
            return Err(crate::follow::paging_stalled());
        }
        pages += 1;
        if pages >= crate::follow::MAX_PAGES {
            return Err(crate::follow::paging_stalled());
        }
    }
    Ok(cursor)
}

type CommandFuture<'a> =
    Pin<Box<dyn core::future::Future<Output = (Outbound, Outcome<()>)> + Send + 'a>>;

async fn poll_in_flight(in_flight: &mut Option<CommandFuture<'_>>) -> (Outbound, Outcome<()>) {
    match in_flight.as_mut() {
        Some(fut) => fut.await,
        None => core::future::pending().await,
    }
}

/// Runs the live interactive loop over one SSE stream, stdin keystrokes, and
/// at most one in-flight outbound REST command.
async fn drive<R, W>(
    api: &Api,
    session: SessionId,
    follow: &mut Follow<'_>,
    renderer: &mut Renderer,
    stdin: &mut R,
    stdout: &mut W,
) -> Outcome<Ended>
where
    R: AsyncRead + Unpin,
    W: Write,
{
    let mut decoder = KeyDecoder::default();
    let mut editor = LineEditor::default();
    let mut queue: VecDeque<Outbound> = VecDeque::new();
    let mut in_flight: Option<CommandFuture<'_>> = None;
    let mut prompt_visible = false;
    let mut stdin_buf = [0_u8; STDIN_CHUNK];

    draw_prompt(stdout, renderer, &editor, false, &mut prompt_visible)?;

    loop {
        tokio::select! {
            item = follow.next() => {
                let item = item?;
                let text = renderer.apply_item(&item, false);
                if !text.is_empty() {
                    clear_prompt(stdout, &mut prompt_visible)?;
                    write_all(stdout, text.as_bytes())?;
                }
                if let Some(ClientEvent::SessionStateChanged { state }) = item.event()
                    && let Some(ended) = handle_state_change(
                        api, session, state, renderer, stdout, &mut prompt_visible,
                    ).await?
                {
                    return Ok(ended);
                }
            }
            (cmd, result) = poll_in_flight(&mut in_flight) => {
                in_flight = None;
                handle_command_result(
                    stdout, renderer, &mut editor, &mut queue, &mut prompt_visible, &cmd, result,
                )?;
                if let Some(next) = queue.pop_front() {
                    in_flight = Some(start_command(api, session, next));
                }
            }
            read = stdin.read(&mut stdin_buf) => match read {
                Ok(0) => {
                    if let Some(fut) = in_flight.take() {
                        let (cmd, result) = fut.await;
                        handle_command_result(
                            stdout, renderer, &mut editor, &mut queue, &mut prompt_visible, &cmd, result,
                        )?;
                    }
                    clear_prompt(stdout, &mut prompt_visible)?;
                    return Ok(Ended::InputEnded);
                }
                Ok(n) => {
                    for key in decoder.push(&stdin_buf[..n]) {
                        if let Some(ended) = handle_key(
                            key, renderer, &mut editor, &mut queue, in_flight.is_some(), stdout, &mut prompt_visible,
                        )? {
                            clear_prompt(stdout, &mut prompt_visible)?;
                            return Ok(ended);
                        }
                        if in_flight.is_none()
                            && let Some(next) = queue.pop_front()
                        {
                            in_flight = Some(start_command(api, session, next));
                        }
                    }
                }
                Err(error) => {
                    clear_prompt(stdout, &mut prompt_visible)?;
                    return Err(Failure::transport(format!("stdin: {error}")));
                }
            },
        }
        if !renderer.is_mid_stream() {
            draw_prompt(
                stdout,
                renderer,
                &editor,
                in_flight.is_some(),
                &mut prompt_visible,
            )?;
        }
    }
}

async fn handle_state_change<W: Write>(
    api: &Api,
    session: SessionId,
    state: SessionState,
    renderer: &mut Renderer,
    stdout: &mut W,
    prompt_visible: &mut bool,
) -> Outcome<Option<Ended>> {
    match state {
        SessionState::Paused => {
            if let Ok(detail) = api
                .get::<SessionDetail>(&format!("/v1/sessions/{session}"))
                .await
            {
                renderer.sync_detail(detail);
                if let Some(notice) = renderer.render_pause_or_failure_notice() {
                    clear_prompt(stdout, prompt_visible)?;
                    write_all(stdout, notice.as_bytes())?;
                }
            }
            Ok(None)
        }
        SessionState::Failed => {
            if let Ok(detail) = api
                .get::<SessionDetail>(&format!("/v1/sessions/{session}"))
                .await
            {
                renderer.sync_detail(detail);
                if let Some(failure) = renderer.failure().map(str::to_owned) {
                    clear_prompt(stdout, prompt_visible)?;
                    let msg = renderer.format_error(&format!("✗ {failure}"));
                    write_all(stdout, msg.as_bytes())?;
                }
            }
            clear_prompt(stdout, prompt_visible)?;
            Ok(Some(Ended::SessionFailed))
        }
        SessionState::Archived => {
            clear_prompt(stdout, prompt_visible)?;
            Ok(Some(Ended::SessionArchived))
        }
        SessionState::Provisioning | SessionState::Active | SessionState::Interrupted => Ok(None),
    }
}

fn handle_key<W: Write>(
    key: Key,
    renderer: &mut Renderer,
    editor: &mut LineEditor,
    queue: &mut VecDeque<Outbound>,
    sending: bool,
    stdout: &mut W,
    prompt_visible: &mut bool,
) -> Outcome<Option<Ended>> {
    match key {
        Key::CtrlDetach => return Ok(Some(Ended::Detached)),
        Key::CtrlD => {
            if editor.is_empty() {
                return Ok(Some(Ended::Detached));
            }
            editor.delete();
        }
        Key::CtrlC => {
            if !editor.is_empty() {
                editor.clear();
            } else if renderer.session_state() == SessionState::Active
                || renderer.turn_active()
                || renderer.active_shell().is_some()
            {
                clear_prompt(stdout, prompt_visible)?;
                let note = renderer.format_notice("─ interrupting…");
                write_all(stdout, note.as_bytes())?;
                queue.push_back(Outbound::Interrupt);
            } else {
                clear_prompt(stdout, prompt_visible)?;
                let note =
                    renderer.format_notice("─ press Ctrl-D or type /detach to leave the session");
                write_all(stdout, note.as_bytes())?;
            }
        }
        Key::Enter => {
            if sending {
                // Keep the user's draft intact while awaiting the active POST.
                return Ok(None);
            }
            let raw = editor.take_line();
            clear_prompt(stdout, prompt_visible)?;
            if raw.trim().is_empty() {
                return Ok(None);
            }
            if let Some(ended) = dispatch_line(raw, renderer, queue, stdout)? {
                return Ok(Some(ended));
            }
        }
        Key::Char(ch) => editor.insert_char(ch),
        Key::Backspace => editor.backspace(),
        Key::Delete => editor.delete(),
        Key::Left => editor.move_left(),
        Key::Right => editor.move_right(),
        Key::Up => editor.history_prev(),
        Key::Down => editor.history_next(),
        Key::Home => editor.move_home(),
        Key::End => editor.move_end(),
        Key::CtrlU => editor.kill_to_start(),
        Key::CtrlK => editor.kill_to_end(),
        Key::CtrlW => editor.delete_prev_word(),
        Key::Escape => {}
    }
    Ok(None)
}

fn dispatch_line<W: Write>(
    raw: String,
    renderer: &mut Renderer,
    queue: &mut VecDeque<Outbound>,
    stdout: &mut W,
) -> Outcome<Option<Ended>> {
    let trimmed = raw.trim();
    // Commands that always work, even while an approval prompt is active.
    match trimmed {
        "/detach" | "/quit" => return Ok(Some(Ended::Detached)),
        "/interrupt" => {
            let note = renderer.format_notice("─ interrupting…");
            write_all(stdout, note.as_bytes())?;
            queue.push_back(Outbound::Interrupt);
            return Ok(None);
        }
        "/status" => {
            let status = renderer.format_status();
            write_all(stdout, status.as_bytes())?;
            return Ok(None);
        }
        "/help" => {
            let help = renderer.format_help();
            write_all(stdout, help.as_bytes())?;
            return Ok(None);
        }
        _ => {}
    }

    if let Some(pending) = renderer.front_approval() {
        let lower = trimmed.to_ascii_lowercase();
        let decision = match lower.as_str() {
            "y" | "yes" | "a" | "allow" | "/approve" => Some(ApprovalDecision::Approved),
            "n" | "no" | "d" | "deny" | "/deny" => Some(ApprovalDecision::Denied),
            _ => None,
        };
        if let Some(decision) = decision {
            queue.push_back(Outbound::Approve {
                id: pending.id,
                decision,
            });
        } else {
            let msg = renderer.format_warning(
                "⚠ approval pending — enter 'a'/'y' to allow, 'd'/'n' to deny, or /detach",
            );
            write_all(stdout, msg.as_bytes())?;
        }
        return Ok(None);
    }

    match trimmed {
        "/compact" => {
            queue.push_back(Outbound::Compact);
            return Ok(None);
        }
        "/context" => {
            queue.push_back(Outbound::ContextUsage);
            return Ok(None);
        }
        "/approve" | "/deny" => {
            let note = renderer.format_notice("─ no pending approval");
            write_all(stdout, note.as_bytes())?;
            return Ok(None);
        }
        _ => {}
    }

    if let Some(shell) = trimmed.strip_prefix('!') {
        let command = shell.trim();
        if !command.is_empty() {
            queue.push_back(Outbound::Shell {
                command: command.to_owned(),
                draft: raw,
            });
        }
        return Ok(None);
    }

    queue.push_back(Outbound::Message {
        text: trimmed.to_owned(),
        draft: raw,
    });
    Ok(None)
}

fn start_command(api: &Api, session: SessionId, cmd: Outbound) -> CommandFuture<'_> {
    Box::pin(async move {
        let result = match &cmd {
            Outbound::Message { text, .. } => {
                api.post_json_empty(
                    &format!("/v1/sessions/{session}/messages"),
                    &SendMessage { text: text.clone() },
                )
                .await
            }
            Outbound::Shell { command, .. } => {
                api.post_json_empty(
                    &format!("/v1/sessions/{session}/shell"),
                    &RunShell {
                        command: command.clone(),
                    },
                )
                .await
            }
            Outbound::Interrupt => {
                api.post_empty(&format!("/v1/sessions/{session}/interrupt"))
                    .await
            }
            Outbound::Compact => {
                api.post_empty(&format!("/v1/sessions/{session}/compact"))
                    .await
            }
            Outbound::ContextUsage => {
                api.post_empty(&format!("/v1/sessions/{session}/context"))
                    .await
            }
            Outbound::Approve { id, decision } => api
                .post::<DecideApproval, ApprovalView>(
                    &format!("/v1/approvals/{id}/decision"),
                    &DecideApproval {
                        decision: *decision,
                    },
                )
                .await
                .map(|_| ()),
        };
        (cmd, result)
    })
}

fn handle_command_result<W: Write>(
    stdout: &mut W,
    renderer: &mut Renderer,
    editor: &mut LineEditor,
    queue: &mut VecDeque<Outbound>,
    prompt_visible: &mut bool,
    cmd: &Outbound,
    result: Outcome<()>,
) -> Outcome<()> {
    match result {
        Ok(()) => match cmd {
            Outbound::Approve { id, decision } => {
                let text = renderer.resolve_approval_local(*id, Some(*decision));
                if !text.is_empty() {
                    clear_prompt(stdout, prompt_visible)?;
                    write_all(stdout, text.as_bytes())?;
                }
            }
            Outbound::Interrupt => {
                clear_prompt(stdout, prompt_visible)?;
                let text = renderer.format_notice("─ interrupt sent");
                write_all(stdout, text.as_bytes())?;
            }
            Outbound::Compact => {
                clear_prompt(stdout, prompt_visible)?;
                let text = renderer.format_notice("─ compact requested");
                write_all(stdout, text.as_bytes())?;
            }
            Outbound::ContextUsage => {
                clear_prompt(stdout, prompt_visible)?;
                let text = renderer.format_notice("─ requesting context usage…");
                write_all(stdout, text.as_bytes())?;
            }
            Outbound::Message { .. } | Outbound::Shell { .. } => {}
        },
        Err(failure) => {
            // Daily/row budget exhaustion stops immediately (AGENTS.md rule #2).
            if failure.is_budget_exhausted() {
                clear_prompt(stdout, prompt_visible)?;
                return Err(failure);
            }
            // Another client (e.g. Web PWA) already decided this approval.
            if let Outbound::Approve { id, .. } = cmd
                && failure.code == Exit::NotFoundOrConflict
            {
                let text = renderer.resolve_approval_local(*id, None);
                if !text.is_empty() {
                    clear_prompt(stdout, prompt_visible)?;
                    write_all(stdout, text.as_bytes())?;
                }
                return Ok(());
            }
            clear_prompt(stdout, prompt_visible)?;
            if let Some(wait) = failure.retry_after {
                // Never fire queued mutations into a 429 window.
                queue.clear();
                let msg = renderer.format_warning(&format!(
                    "⚠ rate limited — wait {}s before retrying ({})",
                    wait.as_secs(),
                    failure.text
                ));
                write_all(stdout, msg.as_bytes())?;
            } else {
                let msg = renderer.format_error(&format!("✗ {}", failure.text));
                write_all(stdout, msg.as_bytes())?;
            }
            // Restore the unsent draft so user text is never lost on failure.
            if let Some(draft) = cmd.draft() {
                if editor.is_empty() {
                    editor.set_text(draft.to_owned());
                } else {
                    editor.push_history(draft.to_owned());
                }
            }
        }
    }
    Ok(())
}

fn clear_prompt<W: Write>(stdout: &mut W, prompt_visible: &mut bool) -> Outcome<()> {
    if *prompt_visible {
        write_all(stdout, b"\r\x1b[2K")?;
        *prompt_visible = false;
    }
    Ok(())
}

fn draw_prompt<W: Write>(
    stdout: &mut W,
    renderer: &Renderer,
    editor: &LineEditor,
    sending: bool,
    prompt_visible: &mut bool,
) -> Outcome<()> {
    let prefix = renderer.prompt_prefix(sending);
    let mut buf = format!("\r\x1b[2K{prefix}{}", editor.text());
    let back = editor.chars_after_cursor();
    if back > 0 {
        use std::fmt::Write as _;
        let _ = write!(buf, "\x1b[{back}D");
    }
    write_all(stdout, buf.as_bytes())?;
    *prompt_visible = true;
    Ok(())
}

fn write_all<W: Write>(stdout: &mut W, bytes: &[u8]) -> Outcome<()> {
    stdout
        .write_all(bytes)
        .and_then(|()| stdout.flush())
        .map_err(|error| Failure::transport(format!("stdout: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt as _;

    use flyco_core::wire::{ApprovalPayload, EventPage, SessionEvent, StoredEvent};
    use flyco_core::{
        ApprovalState, BudgetStage, BudgetView, HarnessEvent, HarnessKind, MachineOrigin,
        ModelChoice, PermissionMode, SessionActivity, SessionRepo, SessionSummary, Usd,
    };

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        head: String,
        body: String,
    }

    type RequestLog = Arc<Mutex<Vec<RecordedRequest>>>;

    fn sample_detail(id: SessionId, harness: HarnessKind) -> SessionDetail {
        SessionDetail {
            summary: SessionSummary {
                id,
                title: "Test interactive session".to_owned(),
                machine_origin: MachineOrigin::Auto,
                harness,
                repos: vec![SessionRepo {
                    slug: "lexoliu/flyco".parse().expect("slug"),
                    branch: Some("main".parse().expect("branch")),
                    dir: "flyco".to_owned(),
                    added_by: flyco_core::RepoAddedBy::User,
                }],
                state: SessionState::Active,
                activity: SessionActivity::Idle,
                interrupted_reason: None,
                paused_reason: None,
                created_at_unix: 0,
                last_active_unix: 0,
                model: ModelChoice {
                    model: "sonnet".to_owned(),
                    effort: None,
                },
                permission_mode: PermissionMode::Auto,
                awake_until_unix: None,
                computer_use: false,
            },
            budget: BudgetView {
                limit: Usd::from_dollars(10),
                spent: Usd::ZERO,
                remaining: Usd::from_dollars(10),
                stage: BudgetStage::Ok,
            },
            failure: None,
            usage_limit: None,
        }
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> RecordedRequest {
        let mut raw = Vec::new();
        let mut chunk = [0_u8; 2048];
        let mut header_end = None;
        while header_end.is_none() {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    raw.extend_from_slice(&chunk[..n]);
                    header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                }
            }
        }
        let end = header_end.unwrap_or(raw.len());
        let header_text = String::from_utf8_lossy(&raw[..end]).into_owned();
        let content_len = header_text
            .lines()
            .find_map(|line| {
                let (name, val) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| val.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        while raw.len() < end + content_len {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&chunk[..n]),
            }
        }
        let head = header_text.lines().next().unwrap_or_default().to_owned();
        let body =
            String::from_utf8_lossy(&raw[end..raw.len().min(end + content_len)]).into_owned();
        RecordedRequest { head, body }
    }

    async fn serve_mock(
        handler: impl Fn(RecordedRequest) -> String + Send + Sync + 'static,
    ) -> (url::Url, RequestLog) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let log: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let log_clone = Arc::clone(&log);
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let log_clone = Arc::clone(&log_clone);
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let req = read_http_request(&mut socket).await;
                    log_clone.lock().expect("log").push(req.clone());
                    let resp = handler(req);
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });
        (
            url::Url::parse(&format!("http://127.0.0.1:{port}")).expect("url"),
            log,
        )
    }

    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buf").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end mock server test exercising catch-up, live SSE, approvals, messages, shell, interrupt, and detach in one flow"
    )]
    #[tokio::test]
    async fn resume_catches_up_streams_live_and_never_calls_terminal_harness() {
        let session = SessionId::generate();
        let approval_id = ApprovalId::generate();
        let history = EventPage {
            events: vec![
                StoredEvent {
                    seq: 1,
                    at_unix: 10,
                    event: serde_json::to_value(ClientEvent::UserMessage {
                        text: "check status".to_owned(),
                        origin: flyco_core::wire::MessageOrigin::User,
                    })
                    .expect("json"),
                },
                StoredEvent {
                    seq: 2,
                    at_unix: 11,
                    event: serde_json::to_value(ClientEvent::Harness {
                        event: HarnessEvent::AssistantDelta {
                            turn_id: "t1".to_owned(),
                            text: "Everything looks clean.\n".to_owned(),
                        },
                    })
                    .expect("json"),
                },
            ],
            more: false,
        };
        let history_json = serde_json::to_string(&history).expect("history json");
        let pending_json = serde_json::to_string(&vec![ApprovalView {
            id: approval_id,
            session,
            payload: ApprovalPayload::ToolUse {
                tool: "Bash".to_owned(),
                input: serde_json::json!({"command": "cargo test"}),
            },
            state: ApprovalState::Pending,
            created_at_unix: 12,
        }])
        .expect("pending json");
        let decided_json = serde_json::to_string(&ApprovalView {
            id: approval_id,
            session,
            payload: ApprovalPayload::ToolUse {
                tool: "Bash".to_owned(),
                input: serde_json::json!({"command": "cargo test"}),
            },
            state: ApprovalState::Approved,
            created_at_unix: 12,
        })
        .expect("decided json");

        let live_event = SessionEvent {
            session,
            seq: Some(3),
            at_unix: 15,
            event: ClientEvent::Harness {
                event: HarnessEvent::AssistantDelta {
                    turn_id: "t2".to_owned(),
                    text: "Tests passed!\n".to_owned(),
                },
            },
        };
        let sse_body = format!(
            "id: 3\ndata: {}\n\n",
            serde_json::to_string(&live_event).expect("sse json")
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let log: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let log_clone = Arc::clone(&log);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let log_clone = Arc::clone(&log_clone);
                let history_json = history_json.clone();
                let pending_json = pending_json.clone();
                let decided_json = decided_json.clone();
                let sse_body = sse_body.clone();
                tokio::spawn(async move {
                    let req = read_http_request(&mut socket).await;
                    log_clone.lock().expect("log").push(req.clone());
                    if req.head.starts_with("GET /v1/events") {
                        let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\n";
                        let _ = socket.write_all(headers.as_bytes()).await;
                        let _ = socket.write_all(sse_body.as_bytes()).await;
                        // Hold SSE open until client disconnects.
                        tokio::time::sleep(core::time::Duration::from_secs(30)).await;
                        return;
                    }
                    let resp = if req.head.contains("/events?after=") {
                        json_response("200 OK", &history_json)
                    } else if req.head.starts_with("GET /v1/approvals") {
                        json_response("200 OK", &pending_json)
                    } else if req.head.contains("/decision") {
                        json_response("200 OK", &decided_json)
                    } else {
                        "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_owned()
                    };
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });

        let api = Api::new(
            url::Url::parse(&format!("http://127.0.0.1:{port}")).expect("url"),
            None,
        );
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        let mut output = Vec::new();

        // Feed: allow pending approval -> send message -> run !shell -> interrupt -> detach.
        let feeder = tokio::spawn(async move {
            tokio::time::sleep(core::time::Duration::from_millis(40)).await;
            writer.write_all(b"a\n").await.expect("approve");
            tokio::time::sleep(core::time::Duration::from_millis(40)).await;
            writer.write_all(b"follow up prompt\n").await.expect("msg");
            tokio::time::sleep(core::time::Duration::from_millis(40)).await;
            writer.write_all(b"!git status\n").await.expect("shell");
            tokio::time::sleep(core::time::Duration::from_millis(40)).await;
            writer.write_all(b"\x03").await.expect("ctrl-c");
            tokio::time::sleep(core::time::Duration::from_millis(40)).await;
            writer.write_all(b"/detach\n").await.expect("detach");
        });

        let ended = attach_io(
            &api,
            sample_detail(session, HarnessKind::ClaudeCode),
            true,
            false,
            &mut reader,
            &mut output,
        )
        .await
        .expect("interactive session detaches cleanly");
        feeder.await.expect("feeder");
        assert_eq!(ended, Ended::Detached);

        let rendered = String::from_utf8_lossy(&output);
        assert!(rendered.contains("Everything looks clean."), "{rendered}");
        assert!(rendered.contains("Tests passed!"), "{rendered}");
        assert!(rendered.contains("cargo test"), "{rendered}");
        assert!(rendered.contains("approval allowed"), "{rendered}");

        let requests = log.lock().expect("log").clone();
        // Never touches the legacy PTY harness endpoint.
        assert!(
            requests
                .iter()
                .all(|req| !req.head.contains("/terminal/harness")),
            "must never call /terminal/harness: {requests:?}"
        );
        // Verify approval decision, user message, shell command, and interrupt.
        assert!(
            requests
                .iter()
                .any(|req| req.head.starts_with("POST /v1/approvals/")
                    && req.head.contains("/decision")
                    && req.body.contains("\"approved\"")),
            "expected approval decision POST: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|req| req.head.contains("/messages") && req.body.contains("follow up prompt")),
            "expected user message POST: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|req| req.head.contains("/shell") && req.body.contains("git status")),
            "expected shell POST: {requests:?}"
        );
        assert!(
            requests.iter().any(|req| req.head.contains("/interrupt")),
            "expected interrupt POST: {requests:?}"
        );
        // Strictly one SSE stream opened.
        assert_eq!(
            requests
                .iter()
                .filter(|req| req.head.starts_with("GET /v1/events"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn approval_deny_sends_denied_decision() {
        let session = SessionId::generate();
        let approval_id = ApprovalId::generate();
        let pending_json = serde_json::to_string(&vec![ApprovalView {
            id: approval_id,
            session,
            payload: ApprovalPayload::RepoAdd {
                repo: "lexoliu/other".to_owned(),
                branch: None,
                reason: "inspect crate".to_owned(),
            },
            state: ApprovalState::Pending,
            created_at_unix: 1,
        }])
        .expect("json");
        let decided_json = serde_json::to_string(&ApprovalView {
            id: approval_id,
            session,
            payload: ApprovalPayload::RepoAdd {
                repo: "lexoliu/other".to_owned(),
                branch: None,
                reason: "inspect crate".to_owned(),
            },
            state: ApprovalState::Denied,
            created_at_unix: 1,
        })
        .expect("json");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let log: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let log_clone = Arc::clone(&log);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let log_clone = Arc::clone(&log_clone);
                let pending_json = pending_json.clone();
                let decided_json = decided_json.clone();
                tokio::spawn(async move {
                    let req = read_http_request(&mut socket).await;
                    log_clone.lock().expect("log").push(req.clone());
                    if req.head.starts_with("GET /v1/events") {
                        let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
                        let _ = socket.write_all(headers.as_bytes()).await;
                        tokio::time::sleep(core::time::Duration::from_secs(30)).await;
                        return;
                    }
                    let resp = if req.head.starts_with("GET /v1/approvals") {
                        json_response("200 OK", &pending_json)
                    } else {
                        json_response("200 OK", &decided_json)
                    };
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });

        let api = Api::new(
            url::Url::parse(&format!("http://127.0.0.1:{port}")).expect("url"),
            None,
        );
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let mut output = Vec::new();
        tokio::spawn(async move {
            tokio::time::sleep(core::time::Duration::from_millis(30)).await;
            writer.write_all(b"d\n").await.expect("deny");
            tokio::time::sleep(core::time::Duration::from_millis(30)).await;
            writer.write_all(b"\x04").await.expect("ctrl-d");
        });

        let ended = attach_io(
            &api,
            sample_detail(session, HarnessKind::Devin),
            false,
            false,
            &mut reader,
            &mut output,
        )
        .await
        .expect("clean detach");
        assert_eq!(ended, Ended::Detached);

        let requests = log.lock().expect("log").clone();
        assert!(
            requests
                .iter()
                .any(|req| req.head.contains("/decision") && req.body.contains("\"denied\"")),
            "expected denied decision: {requests:?}"
        );
    }

    #[tokio::test]
    async fn stops_immediately_on_request_budget_exhausted() {
        let session = SessionId::generate();
        let body = serde_json::json!({
            "type": "https://flyco.dev/problems/request-budget-exhausted",
            "title": "request budget exhausted",
            "status": 429,
            "detail": "daily quota spent"
        })
        .to_string();
        let exhausted = format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/problem+json\r\nretry-after: 3600\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let (base, log) = serve_mock(move |req| {
            if req.head.starts_with("GET /v1/approvals") {
                json_response("200 OK", "[]")
            } else {
                exhausted.clone()
            }
        })
        .await;

        let api = Api::new(base, None);
        let (_writer, mut reader) = tokio::io::duplex(256);
        let mut output = Vec::new();
        let err = attach_io(
            &api,
            sample_detail(session, HarnessKind::Codex),
            false,
            false,
            &mut reader,
            &mut output,
        )
        .await
        .expect_err("budget exhaustion must stop immediately");
        assert_eq!(err.code, Exit::Problem);
        assert!(err.is_budget_exhausted());
        let sse_attempts = log
            .lock()
            .expect("log")
            .iter()
            .filter(|r| r.head.starts_with("GET /v1/events"))
            .count();
        assert_eq!(sse_attempts, 1);
    }

    #[tokio::test]
    async fn catch_up_stops_when_server_pages_without_advancing() {
        let session = SessionId::generate();
        let stuck = serde_json::to_string(&EventPage {
            events: vec![],
            more: true,
        })
        .expect("json");
        let (base, log) = serve_mock(move |_| json_response("200 OK", &stuck)).await;
        let api = Api::new(base, None);
        let (_writer, mut reader) = tokio::io::duplex(256);
        let mut output = Vec::new();
        let err = attach_io(
            &api,
            sample_detail(session, HarnessKind::Antigravity),
            true,
            false,
            &mut reader,
            &mut output,
        )
        .await
        .expect_err("stalled pagination must fail fast");
        assert_eq!(err.code, Exit::Transport);
        assert_eq!(log.lock().expect("log").len(), 1);
    }

    #[tokio::test]
    async fn draft_is_restored_when_message_post_fails() {
        let session = SessionId::generate();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_clone = Arc::clone(&attempts);
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_clone = Arc::clone(&bodies);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let attempts_clone = Arc::clone(&attempts_clone);
                let bodies_clone = Arc::clone(&bodies_clone);
                tokio::spawn(async move {
                    let req = read_http_request(&mut socket).await;
                    if req.head.starts_with("GET /v1/events") {
                        let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
                        let _ = socket.write_all(headers.as_bytes()).await;
                        tokio::time::sleep(core::time::Duration::from_secs(30)).await;
                        return;
                    }
                    let resp = if req.head.starts_with("GET /v1/approvals") {
                        json_response("200 OK", "[]")
                    } else if req.head.contains("/messages") {
                        bodies_clone.lock().expect("bodies").push(req.body);
                        let n = attempts_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if n == 0 {
                            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
                        } else {
                            "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
                        }
                    } else {
                        json_response("200 OK", "{}")
                    };
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });

        let api = Api::new(
            url::Url::parse(&format!("http://127.0.0.1:{port}")).expect("url"),
            None,
        );
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let mut output = Vec::new();
        tokio::spawn(async move {
            tokio::time::sleep(core::time::Duration::from_millis(30)).await;
            writer.write_all(b"important draft\n").await.expect("first");
            // Wait for 503 to restore the draft, then press Enter again without retyping.
            tokio::time::sleep(core::time::Duration::from_millis(80)).await;
            writer.write_all(b"\n").await.expect("retry enter");
            tokio::time::sleep(core::time::Duration::from_millis(50)).await;
            writer.write_all(b"\x1d").await.expect("ctrl-]");
        });

        let ended = attach_io(
            &api,
            sample_detail(session, HarnessKind::ClaudeCode),
            false,
            false,
            &mut reader,
            &mut output,
        )
        .await
        .expect("detach");
        assert_eq!(ended, Ended::Detached);
        let sent = bodies.lock().expect("bodies").clone();
        assert_eq!(
            sent.len(),
            2,
            "expected initial attempt plus retry of restored draft"
        );
        assert!(sent[0].contains("important draft"));
        assert!(sent[1].contains("important draft"));
    }

    #[expect(
        clippy::too_many_lines,
        reason = "mock server + virtual-clock driver verifying SSE reconnect with backoff"
    )]
    #[tokio::test(start_paused = true)]
    async fn sse_reconnects_with_backoff_and_delivers_subsequent_events() {
        let session = SessionId::generate();
        let e1 = SessionEvent {
            session,
            seq: Some(1),
            at_unix: 1,
            event: ClientEvent::Harness {
                event: HarnessEvent::AssistantDelta {
                    turn_id: "t1".to_owned(),
                    text: "part-one\n".to_owned(),
                },
            },
        };
        let e2 = SessionEvent {
            session,
            seq: Some(2),
            at_unix: 2,
            event: ClientEvent::Harness {
                event: HarnessEvent::AssistantDelta {
                    turn_id: "t1".to_owned(),
                    text: "part-two\n".to_owned(),
                },
            },
        };
        let sse1 = format!(
            "id: 1\ndata: {}\n\n",
            serde_json::to_string(&e1).expect("e1 json")
        );
        let sse2 = format!(
            "id: 2\ndata: {}\n\n",
            serde_json::to_string(&e2).expect("e2 json")
        );
        let sse_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sse_calls_srv = Arc::clone(&sse_calls);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let sse_calls_srv = Arc::clone(&sse_calls_srv);
                let sse1 = sse1.clone();
                let sse2 = sse2.clone();
                tokio::spawn(async move {
                    let req = read_http_request(&mut socket).await;
                    if req.head.starts_with("GET /v1/approvals") {
                        let resp = json_response("200 OK", "[]");
                        let _ = socket.write_all(resp.as_bytes()).await;
                        return;
                    }
                    if req.head.starts_with("GET /v1/events") {
                        let n = sse_calls_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if n == 0 {
                            // Emit first event then drop stream immediately to force backoff reconnect.
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse1}",
                                sse1.len()
                            );
                            let _ = socket.write_all(resp.as_bytes()).await;
                        } else {
                            let headers =
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
                            let _ = socket.write_all(headers.as_bytes()).await;
                            let _ = socket.write_all(sse2.as_bytes()).await;
                            tokio::time::sleep(core::time::Duration::from_secs(60)).await;
                        }
                    }
                });
            }
        });

        let api = Api::new(
            url::Url::parse(&format!("http://127.0.0.1:{port}")).expect("url"),
            None,
        );
        let (mut writer, mut reader) = tokio::io::duplex(512);
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let out_handle = Arc::clone(&output);

        let driver = tokio::spawn(async move {
            let mut sink = SharedBuf(out_handle);
            attach_io(
                &api,
                sample_detail(session, HarnessKind::Codex),
                false,
                false,
                &mut reader,
                &mut sink,
            )
            .await
        });

        for _ in 0..200 {
            let text = String::from_utf8_lossy(&output.lock().expect("buf")).into_owned();
            if text.contains("part-two") {
                break;
            }
            tokio::time::advance(core::time::Duration::from_millis(50)).await;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        }
        writer.write_all(b"/detach\n").await.expect("detach");
        for _ in 0..20 {
            if driver.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let ended = driver.await.expect("join").expect("clean detach");
        assert_eq!(ended, Ended::Detached);
        assert_eq!(sse_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let rendered = String::from_utf8_lossy(&output.lock().expect("buf")).into_owned();
        assert!(rendered.contains("part-one"), "{rendered}");
        assert!(rendered.contains("part-two"), "{rendered}");
    }

    #[tokio::test]
    async fn session_failed_event_fetches_failure_reason_and_exits() {
        let session = SessionId::generate();
        let mut failed_detail = sample_detail(session, HarnessKind::Antigravity);
        failed_detail.summary.state = SessionState::Failed;
        failed_detail.failure = Some("provider quota exceeded".to_owned());
        let failed_detail_json = serde_json::to_string(&failed_detail).expect("json");
        let failed_event = SessionEvent {
            session,
            seq: Some(1),
            at_unix: 1,
            event: ClientEvent::SessionStateChanged {
                state: SessionState::Failed,
            },
        };
        let sse_body = format!(
            "id: 1\ndata: {}\n\n",
            serde_json::to_string(&failed_event).expect("sse json")
        );

        let (base, _) = serve_mock(move |req| {
            if req.head.starts_with("GET /v1/approvals") {
                json_response("200 OK", "[]")
            } else if req.head.starts_with("GET /v1/events") {
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse_body}",
                    sse_body.len()
                )
            } else {
                json_response("200 OK", &failed_detail_json)
            }
        })
        .await;

        let api = Api::new(base, None);
        let (_writer, mut reader) = tokio::io::duplex(256);
        let mut output = Vec::new();
        let ended = attach_io(
            &api,
            sample_detail(session, HarnessKind::Antigravity),
            false,
            false,
            &mut reader,
            &mut output,
        )
        .await
        .expect("returns Ended::SessionFailed");
        assert_eq!(ended, Ended::SessionFailed);
        let rendered = String::from_utf8_lossy(&output);
        assert!(rendered.contains("provider quota exceeded"), "{rendered}");
    }
}
