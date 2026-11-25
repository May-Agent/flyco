//! Async state machine and event loop for the interactive `flyco` TUI.
//!
//! Multiplexes terminal events (`crossterm::event::EventStream`), at most one
//! live control-plane SSE stream ([`Follow`]), and background async REST /
//! local-session tasks without ever polling on a timer.

use std::io::stdout;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use flyco_core::wire::{ApprovalDecision, ClientEvent};
use flyco_core::{
    ApprovalId, ApprovalView, CreateSession, DecideApproval, HarnessAccountView, HarnessKind,
    MachineCatalog, MachineCatalogEntry, MachineChoice, MachineDefault, ModelChoice, ModelOption,
    RepoSelection, RepoSummary, RunShell, SendMessage, SessionDetail, SessionId, SessionState,
    SessionSummary, UpdateSession, Usd,
};
use futures_util::StreamExt as _;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::cli::SessionSpec;
use crate::client::Api;
use crate::follow::{Follow, Item};
use crate::local_sessions::{self, LocalSession, harness_label};
use crate::tui::commands::{CommandSpec, SlashCommand, parse_slash};
use crate::tui::input::Composer;
use crate::tui::picker::{Picker, PickerEntry};
use crate::tui::render;
use crate::tui::transcript::{Entry, PendingApproval, SessionTranscript, format_model_choice};
use crate::{Exit, Failure, Outcome};

/// Optional picker to open immediately upon entering the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPicker {
    /// Open the active cloud sessions picker (`/sessions`).
    Sessions,
    /// Open the inactive cloud sessions picker (`/resume`).
    Resume,
}

/// Entry options for [`run`].
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Pre-selected harness for lobby mode.
    pub initial_harness: Option<HarnessKind>,
    /// Pre-attached session (e.g. `flyco resume <id>`, `flyco handoff`, or `--prompt`).
    pub initial_session: Option<SessionDetail>,
    /// Whether to run bounded history catch-up when attaching to [`Self::initial_session`].
    pub initial_resume_history: bool,
    /// Optional modal picker opened immediately after startup.
    pub open_picker: Option<StartPicker>,
    /// CLI flags forwarded into lobby defaults.
    pub spec: SessionSpec,
}

/// Lobby configuration for the next cloud session to be created.
#[derive(Debug, Clone)]
pub struct LobbyConfig {
    /// Selected harness.
    pub harness: HarnessKind,
    /// Selected model (None = harness default).
    pub model: Option<ModelChoice>,
    /// Target `owner/repo`.
    pub repo: Option<String>,
    /// Optional explicit branch (`None` = repo default).
    pub branch: Option<String>,
    /// Explicit machine choice (`None` = control-plane automatic choice).
    pub machine: Option<MachineChoice>,
    /// Human display label for [`Self::machine`].
    pub machine_label: Option<String>,
    /// Whether spot capacity is preferred when auto-selecting.
    pub spot: bool,
    /// Spend limit in USD.
    pub budget: Usd,
    /// Extra environment variables (`KEY=VALUE`) applied on create.
    pub env: Vec<String>,
    /// Optional permission mode override.
    pub permission_mode: Option<flyco_core::PermissionMode>,
}

/// Active modal overlay, if any.
#[derive(Debug)]
pub enum Overlay {
    /// Commands and keybindings sheet (`/help`).
    Help,
    /// Active cloud sessions picker (`/sessions`).
    Sessions(Picker<SessionSummary>),
    /// Inactive cloud sessions picker (`/resume`).
    Resume(Picker<SessionSummary>),
    /// Model selector for the active harness (`/model`).
    Models(Picker<ModelSelection>),
    /// Harness selector (`/provider`).
    Providers(Picker<HarnessKind>),
    /// Local harness transcript picker (`/resume-{codex,claude,agy,devin}`).
    LocalSessions(Picker<LocalSession>),
    /// Repository selector (`/repo`).
    Repos(Picker<RepoSummary>),
    /// Machine selector (`/machine`).
    Machines(Picker<MachineCatalogEntry>),
}

/// Value returned by the model picker.
#[derive(Debug, Clone)]
pub struct ModelSelection {
    /// Model identifier.
    pub model: String,
    /// Optional default effort level.
    pub effort: Option<String>,
}

/// Complete mutable UI state rendered every frame.
#[derive(Debug)]
pub struct AppState {
    /// Active attached session, or `None` when in lobby mode.
    pub session: Option<SessionTranscript>,
    /// Launch defaults for the lobby.
    pub lobby: LobbyConfig,
    /// Cached linked harness accounts from `GET /v1/harness-accounts`.
    pub harness_accounts: Vec<HarnessAccountView>,
    /// Composer text editor.
    pub composer: Composer,
    /// Selected row index inside the live `/` command palette.
    pub palette_selected: usize,
    /// Active modal overlay.
    pub overlay: Option<Overlay>,
    /// Whether timeline view follows the latest line.
    pub follow_tail: bool,
    /// Lines scrolled upward from bottom when `follow_tail` is false.
    pub scroll_from_bottom: usize,
    /// Inline notices/errors shown in lobby mode.
    pub lobby_log: Vec<Entry>,
    /// Non-empty while a background REST or upload operation is in flight.
    pub busy_label: Option<String>,
    /// Spinner animation frame index.
    pub spinner_tick: usize,
    /// Set to exit the main loop cleanly.
    pub should_quit: bool,
}

impl AppState {
    fn new(opts: &LaunchOptions) -> Self {
        let harness = opts.initial_harness.unwrap_or(HarnessKind::ClaudeCode);
        let model = opts.spec.model.clone().map(|m| ModelChoice {
            model: m,
            effort: opts.spec.effort.clone(),
        });
        let repo = opts.spec.repo.first().cloned();
        let machine = opts.spec.machine_choice().ok().flatten();
        let machine_label = machine
            .as_ref()
            .map(|m| format!("{} ({})", m.machine_type, m.region));
        let lobby = LobbyConfig {
            harness,
            model,
            repo,
            branch: opts.spec.branch.clone(),
            machine,
            machine_label,
            spot: !opts.spec.on_demand,
            budget: opts.spec.budget.unwrap_or_else(|| Usd::from_dollars(10)),
            env: opts.spec.env.clone(),
            permission_mode: opts.spec.permission_mode,
        };
        Self {
            session: opts.initial_session.clone().map(SessionTranscript::new),
            lobby,
            harness_accounts: Vec::new(),
            composer: Composer::default(),
            palette_selected: 0,
            overlay: None,
            follow_tail: true,
            scroll_from_bottom: 0,
            lobby_log: Vec::new(),
            busy_label: None,
            spinner_tick: 0,
            should_quit: false,
        }
    }

    /// Pending approval for the currently attached session, if any.
    #[must_use]
    pub fn pending_approval(&self) -> Option<&PendingApproval> {
        self.session
            .as_ref()
            .and_then(SessionTranscript::front_approval)
    }

    /// Matching slash commands for the current composer input (empty unless
    /// input begins with `/` and has no whitespace arguments yet).
    #[must_use]
    pub fn active_palette(&self) -> Vec<CommandSpec> {
        let text = self.composer.text();
        if !text.starts_with('/') || text.contains(char::is_whitespace) {
            return Vec::new();
        }
        SlashCommand::filter(text)
    }

    /// Active harness (the attached session's harness, or the lobby's).
    #[must_use]
    pub fn active_harness(&self) -> HarnessKind {
        self.session
            .as_ref()
            .map_or(self.lobby.harness, |s| s.harness)
    }

    fn push_notice(&mut self, text: impl Into<String>) {
        if let Some(session) = &mut self.session {
            session.push_notice(text);
        } else {
            self.lobby_log.push(Entry::Notice { text: text.into() });
        }
    }

    fn push_warning(&mut self, text: impl Into<String>) {
        if let Some(session) = &mut self.session {
            session.push_warning(text);
        } else {
            self.lobby_log.push(Entry::Warning { text: text.into() });
        }
    }

    fn push_error(&mut self, text: impl Into<String>) {
        if let Some(session) = &mut self.session {
            session.push_error(text);
        } else {
            self.lobby_log.push(Entry::Error { text: text.into() });
        }
    }

    fn is_spinning(&self) -> bool {
        self.busy_label.is_some()
            || self
                .session
                .as_ref()
                .is_some_and(|s| s.turn_active || s.state == SessionState::Provisioning)
    }
}

/// Completed background task delivered back to the main TUI event loop.
enum BgResult {
    Bootstrap {
        accounts: Option<Vec<HarnessAccountView>>,
        default_machine: Option<MachineDefault>,
        local_repo: Option<(String, String)>,
    },
    ActiveSessions(Outcome<Vec<SessionSummary>>),
    InactiveSessions(Outcome<Vec<SessionSummary>>),
    ModelsLoaded {
        harness: HarnessKind,
        query: String,
        result: Outcome<Vec<HarnessAccountView>>,
    },
    ReposLoaded {
        query: String,
        result: Outcome<Vec<RepoSummary>>,
    },
    MachinesLoaded(Outcome<(MachineCatalog, Option<MachineDefault>)>),
    LocalDiscovered {
        harness: HarnessKind,
        query: String,
        result: Outcome<Vec<LocalSession>>,
    },
    AttachReady {
        detail: SessionDetail,
        replayed: Vec<Item>,
        pending: Vec<ApprovalView>,
        cursor: u64,
    },
    AttachFailed(Failure),
    ModelUpdated {
        session_id: SessionId,
        result: Outcome<SessionDetail>,
    },
    BudgetUpdated {
        session_id: SessionId,
        result: Outcome<SessionDetail>,
    },
    OutboundDone {
        action: &'static str,
        draft: Option<String>,
        result: Outcome<()>,
    },
    DetailRefreshed {
        session_id: SessionId,
        result: Outcome<SessionDetail>,
    },
}

/// RAII guard restoring terminal mode on exit or panic.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Outcome<Self> {
        enable_raw_mode().map_err(|e| Failure::transport(format!("raw mode: {e}")))?;
        let mut out = stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )
        .map_err(|e| {
            let _ = disable_raw_mode();
            Failure::transport(format!("terminal setup: {e}"))
        })?;
        let backend = CrosstermBackend::new(out);
        let terminal = Terminal::new(backend).map_err(|e| {
            let _ = disable_raw_mode();
            Failure::transport(format!("terminal init: {e}"))
        })?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

/// Launches the full-screen interactive `flyco` TUI and runs until `/quit` or `Ctrl+Q`.
///
/// # Errors
/// Returns [`Failure`] on terminal setup or fatal unrecoverable stream errors.
pub async fn run(api: &Api, opts: LaunchOptions) -> Outcome<Exit> {
    let mut guard = TerminalGuard::enter()?;
    let mut app = AppState::new(&opts);
    let (bg_tx, mut bg_rx) = mpsc::unbounded_channel::<BgResult>();

    // Kick off non-blocking bootstrap (linked harnesses, default machine, local git repo).
    spawn_bootstrap(api.clone(), bg_tx.clone());

    // If launched directly onto a session, run bounded catch-up & attach now.
    if let Some(detail) = opts.initial_session.clone() {
        app.busy_label = Some("connecting to session…".to_owned());
        spawn_attach(
            api.clone(),
            detail,
            opts.initial_resume_history,
            bg_tx.clone(),
        );
    } else if let Some(start_picker) = opts.open_picker {
        match start_picker {
            StartPicker::Sessions => trigger_sessions_picker(api, &mut app, &bg_tx),
            StartPicker::Resume => trigger_resume_picker(api, &mut app, &bg_tx),
        }
    }

    // Strictly ONE active SSE stream at a time (AGENTS.md rule #5).
    let mut active_follow: Option<(SessionId, Follow<'_>)> = None;
    let mut events = EventStream::new();
    let mut spinner_interval = tokio::time::interval(Duration::from_millis(120));
    spinner_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    while !app.should_quit {
        guard
            .terminal
            .draw(|frame| render::draw(frame, &app))
            .map_err(|e| Failure::transport(format!("draw: {e}")))?;

        let spinning = app.is_spinning();
        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(event)) => handle_terminal_event(
                        api,
                        &mut app,
                        &mut active_follow,
                        &bg_tx,
                        event,
                    ),
                    Some(Err(err)) => {
                        app.push_error(format!("terminal input error: {err}"));
                    }
                    None => break,
                }
            }
            maybe_bg = bg_rx.recv() => {
                if let Some(bg) = maybe_bg {
                    handle_bg_result(api, &mut app, &mut active_follow, &bg_tx, bg).await;
                }
            }
            stream_res = poll_follow(&mut active_follow) => {
                handle_stream_event(api, &mut app, &mut active_follow, &bg_tx, stream_res);
            }
            _ = spinner_interval.tick(), if spinning => {
                app.spinner_tick = app.spinner_tick.wrapping_add(1);
            }
        }
    }

    Ok(Exit::Ok)
}

async fn poll_follow(active: &mut Option<(SessionId, Follow<'_>)>) -> (SessionId, Outcome<Item>) {
    match active.as_mut() {
        Some((id, follow)) => (*id, follow.next().await),
        None => core::future::pending().await,
    }
}

fn spawn_bootstrap(api: Api, tx: mpsc::UnboundedSender<BgResult>) {
    tokio::spawn(async move {
        let accounts = api
            .get::<Vec<HarnessAccountView>>("/v1/harness-accounts")
            .await
            .ok();
        let default_machine = api.get::<MachineDefault>("/v1/machines/default").await.ok();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let local_repo = local_sessions::try_local_repo(&cwd, None, None)
            .await
            .map(|r| (r.slug, r.branch));
        let _ = tx.send(BgResult::Bootstrap {
            accounts,
            default_machine,
            local_repo,
        });
    });
}

fn spawn_attach(
    api: Api,
    detail: SessionDetail,
    catch_up_history: bool,
    tx: mpsc::UnboundedSender<BgResult>,
) {
    tokio::spawn(async move {
        let id = detail.summary.id;
        let mut cursor = 0_u64;
        let mut replayed = Vec::new();
        if catch_up_history {
            let mut pages = 0_u32;
            loop {
                let page = match crate::follow::events(&api, id, cursor).await {
                    Ok(page) => page,
                    Err(error) => {
                        let _ = tx.send(BgResult::AttachFailed(error));
                        return;
                    }
                };
                let advanced = page.events.iter().any(|stored| stored.seq > cursor);
                for stored in page.events {
                    if stored.seq <= cursor {
                        continue;
                    }
                    cursor = stored.seq;
                    replayed.push(Item::Replayed(stored));
                }
                if !page.more {
                    break;
                }
                if !advanced {
                    let _ = tx.send(BgResult::AttachFailed(crate::follow::paging_stalled()));
                    return;
                }
                pages += 1;
                if pages >= crate::follow::MAX_PAGES {
                    let _ = tx.send(BgResult::AttachFailed(crate::follow::paging_stalled()));
                    return;
                }
            }
        }
        let pending = api
            .get::<Vec<ApprovalView>>(&format!("/v1/approvals?session={id}&state=pending"))
            .await
            .unwrap_or_default();
        let _ = tx.send(BgResult::AttachReady {
            detail,
            replayed,
            pending,
            cursor,
        });
    });
}

#[expect(
    clippy::too_many_lines,
    reason = "handles all background task result variants in one state-update match"
)]
async fn handle_bg_result<'a>(
    api: &'a Api,
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'a>)>,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
    bg: BgResult,
) {
    match bg {
        BgResult::Bootstrap {
            accounts,
            default_machine,
            local_repo,
        } => {
            if let Some(accounts) = accounts {
                // If the initial harness has no linked account, select the first linked one.
                if !accounts.iter().any(|a| a.harness == app.lobby.harness)
                    && let Some(first) = accounts.first()
                {
                    app.lobby.harness = first.harness;
                }
                if app.lobby.model.is_none()
                    && let Some(account) = accounts.iter().find(|a| a.harness == app.lobby.harness)
                    && !account.models.is_empty()
                {
                    app.lobby.model = Some(ModelChoice::default_of(&account.models));
                }
                app.harness_accounts = accounts;
            }
            if app.lobby.machine.is_none()
                && let Some(default) = default_machine
            {
                app.lobby.machine_label = Some(format!(
                    "{} ({}) [default]",
                    default.choice.machine_type, default.choice.region
                ));
                app.lobby.machine = Some(default.choice);
            }
            if app.lobby.repo.is_none()
                && let Some((slug, branch)) = local_repo
            {
                app.lobby.repo = Some(slug);
                if app.lobby.branch.is_none() {
                    app.lobby.branch = Some(branch);
                }
            }
        }
        BgResult::ActiveSessions(result) => {
            app.busy_label = None;
            match result {
                Ok(sessions) => {
                    let entries = sessions
                        .into_iter()
                        .filter(|s| {
                            matches!(s.state, SessionState::Provisioning | SessionState::Active)
                        })
                        .map(session_picker_entry)
                        .collect();
                    app.overlay = Some(Overlay::Sessions(Picker::new(
                        "Active sessions (/sessions)",
                        entries,
                        "",
                        "No active cloud sessions",
                    )));
                }
                Err(error) => app.push_error(error.text),
            }
        }
        BgResult::InactiveSessions(result) => {
            app.busy_label = None;
            match result {
                Ok(sessions) => {
                    let entries = sessions
                        .into_iter()
                        .filter(|s| {
                            !matches!(s.state, SessionState::Provisioning | SessionState::Active)
                        })
                        .map(session_picker_entry)
                        .collect();
                    app.overlay = Some(Overlay::Resume(Picker::new(
                        "Inactive sessions (/resume)",
                        entries,
                        "",
                        "No inactive sessions to resume",
                    )));
                }
                Err(error) => app.push_error(error.text),
            }
        }
        BgResult::ModelsLoaded {
            harness,
            query,
            result,
        } => {
            app.busy_label = None;
            match result {
                Ok(accounts) => {
                    app.harness_accounts = accounts;
                    let models = models_for_harness(&app.harness_accounts, harness);
                    if !query.is_empty()
                        && let Some(exact) =
                            models.iter().find(|m| m.id.eq_ignore_ascii_case(&query))
                    {
                        apply_model_selection(
                            api,
                            app,
                            ModelSelection {
                                model: exact.id.clone(),
                                effort: exact.default_effort.clone(),
                            },
                            bg_tx,
                        );
                        return;
                    }
                    let entries = models
                        .into_iter()
                        .map(|m| PickerEntry {
                            value: ModelSelection {
                                model: m.id.clone(),
                                effort: m.default_effort.clone(),
                            },
                            badge: if m.is_default { "DEFAULT" } else { "MODEL" }.to_owned(),
                            primary: m.id.clone(),
                            secondary: m.label.clone(),
                            trailing: m.default_effort.unwrap_or_default(),
                            search_extra: format!("{} {}", m.id, m.label),
                        })
                        .collect();
                    app.overlay = Some(Overlay::Models(Picker::new(
                        format!("Models — {}", harness_label(harness)),
                        entries,
                        &query,
                        "No models advertised for this harness",
                    )));
                }
                Err(error) => app.push_error(error.text),
            }
        }
        BgResult::ReposLoaded { query, result } => {
            app.busy_label = None;
            match result {
                Ok(repos) => {
                    if !query.is_empty()
                        && let Some(exact) = repos
                            .iter()
                            .find(|r| r.slug.as_str().eq_ignore_ascii_case(&query))
                    {
                        app.lobby.repo = Some(exact.slug.to_string());
                        app.lobby.branch = None;
                        app.push_notice(format!("repo -> {}", exact.slug));
                        return;
                    }
                    let entries = repos
                        .into_iter()
                        .map(|r| PickerEntry {
                            badge: if r.private { "PRIVATE" } else { "PUBLIC" }.to_owned(),
                            primary: r.slug.to_string(),
                            secondary: r.description.clone().unwrap_or_default(),
                            trailing: r.default_branch.to_string(),
                            search_extra: r.slug.to_string(),
                            value: r,
                        })
                        .collect();
                    app.overlay = Some(Overlay::Repos(Picker::new(
                        "Repositories (/repo)",
                        entries,
                        &query,
                        "No repositories available",
                    )));
                }
                Err(error) => app.push_error(error.text),
            }
        }
        BgResult::MachinesLoaded(result) => {
            app.busy_label = None;
            match result {
                Ok((catalog, default)) => {
                    let entries = catalog
                        .entries
                        .into_iter()
                        .filter(|e| e.account.is_some())
                        .map(|entry| {
                            let is_def = default.as_ref().is_some_and(|d| {
                                entry.account == Some(d.choice.provider_account)
                                    && entry.machine_type == d.choice.machine_type
                                    && entry.region == d.choice.region
                            });
                            let size = entry.capacity.as_ref().map_or_else(
                                || "-".to_owned(),
                                |c| format!("{}c/{}GiB", c.vcpus, c.memory_mib / 1024),
                            );
                            PickerEntry {
                                badge: if is_def {
                                    "DEFAULT".to_owned()
                                } else {
                                    format!("{:?}", entry.provider).to_ascii_uppercase()
                                },
                                primary: entry.machine_type.clone(),
                                secondary: format!("{}  {size}", entry.region),
                                trailing: crate::discover::pricing(&entry),
                                search_extra: format!("{} {}", entry.machine_type, entry.region),
                                value: entry,
                            }
                        })
                        .collect();
                    app.overlay = Some(Overlay::Machines(Picker::new(
                        "Cloud machines (/machine)",
                        entries,
                        "",
                        "No machines in catalog",
                    )));
                }
                Err(error) => app.push_error(error.text),
            }
        }
        BgResult::LocalDiscovered {
            harness,
            query,
            result,
        } => {
            app.busy_label = None;
            match result {
                Ok(sessions) => {
                    if !query.is_empty() {
                        let hits: Vec<&LocalSession> = sessions
                            .iter()
                            .filter(|s| {
                                s.session_id == query
                                    || s.session_id.starts_with(&query)
                                    || s.session_id.contains(&query)
                            })
                            .collect();
                        if hits.len() == 1 {
                            start_local_native_resume(api, app, hits[0].clone(), bg_tx);
                            return;
                        }
                    }
                    let entries = sessions
                        .into_iter()
                        .map(|s| PickerEntry {
                            badge: harness_label(s.harness).to_ascii_uppercase(),
                            primary: s.title.clone(),
                            secondary: s.cwd.as_ref().map_or_default(|p| shorten_home(p)),
                            trailing: format_age(s.modified),
                            search_extra: s.session_id.clone(),
                            value: s,
                        })
                        .collect();
                    app.overlay = Some(Overlay::LocalSessions(Picker::new(
                        format!(
                            "Local {} sessions (same-harness resume)",
                            harness_label(harness)
                        ),
                        entries,
                        &query,
                        format!("No local {} sessions found", harness_label(harness)),
                    )));
                }
                Err(error) => app.push_error(error.text),
            }
        }
        BgResult::AttachReady {
            detail,
            replayed,
            pending,
            cursor,
        } => {
            // Drop any previous stream BEFORE opening the new one (AGENTS.md rule #5).
            *active_follow = None;
            let id = detail.summary.id;
            let mut transcript = SessionTranscript::new(detail);
            for item in &replayed {
                transcript.apply_item(item, true);
            }
            transcript.reconcile_approvals(pending);
            app.session = Some(transcript);
            app.follow_tail = true;
            app.scroll_from_bottom = 0;

            let mut follow = Follow::new(api, id).after(cursor);
            match follow.connect().await {
                Ok(()) => {
                    *active_follow = Some((id, follow));
                    app.busy_label = None;
                }
                Err(error) => {
                    app.busy_label = None;
                    app.push_error(format!("stream connect failed: {}", error.text));
                }
            }
        }
        BgResult::AttachFailed(error) => {
            app.busy_label = None;
            app.push_error(error.text);
        }
        BgResult::ModelUpdated { session_id, result } => {
            app.busy_label = None;
            match result {
                Ok(detail) => {
                    let label = format_model_choice(&detail.summary.model);
                    if let Some(session) = &mut app.session
                        && session.id == session_id
                    {
                        session.sync_detail(detail);
                    }
                    app.push_notice(format!("model -> {label}"));
                }
                Err(error) => app.push_error(error.text),
            }
        }
        BgResult::BudgetUpdated { session_id, result } => {
            app.busy_label = None;
            match result {
                Ok(detail) => {
                    let limit = detail.budget.limit;
                    if let Some(session) = &mut app.session
                        && session.id == session_id
                    {
                        session.sync_detail(detail);
                    }
                    app.push_notice(format!("budget limit -> {limit}"));
                }
                Err(error) => app.push_error(error.text),
            }
        }
        BgResult::OutboundDone {
            action,
            draft,
            result,
        } => {
            app.busy_label = None;
            if let Err(error) = result {
                app.push_error(format!("{action} failed: {}", error.text));
                if let Some(draft) = draft
                    && app.composer.is_empty()
                {
                    app.composer.set_text(draft);
                }
            }
        }
        BgResult::DetailRefreshed { session_id, result } => {
            if let Ok(detail) = result
                && let Some(session) = &mut app.session
                && session.id == session_id
            {
                let notice = detail.failure.clone();
                session.sync_detail(detail);
                if let Some(failure) = notice {
                    session.push_error(failure);
                }
            }
        }
    }
}

fn handle_stream_event(
    api: &Api,
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'_>)>,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
    (session_id, result): (SessionId, Outcome<Item>),
) {
    match result {
        Ok(item) => {
            let state_changed = match item.event() {
                Some(ClientEvent::SessionStateChanged { state }) => Some(state),
                _ => None,
            };
            if let Some(session) = &mut app.session
                && session.id == session_id
            {
                session.apply_item(&item, false);
            }
            if matches!(
                state_changed,
                Some(SessionState::Paused | SessionState::Failed)
            ) {
                let api = api.clone();
                let tx = bg_tx.clone();
                tokio::spawn(async move {
                    let result = api
                        .get::<SessionDetail>(&format!("/v1/sessions/{session_id}"))
                        .await;
                    let _ = tx.send(BgResult::DetailRefreshed { session_id, result });
                });
            }
        }
        Err(error) => {
            // Stop the stream cleanly on fatal error or budget exhaustion (AGENTS.md rule #2).
            *active_follow = None;
            if error.is_budget_exhausted() {
                app.push_error(format!(
                    "control-plane daily quota exhausted (stream stopped): {}",
                    error.text
                ));
            } else {
                app.push_error(format!("session stream closed: {}", error.text));
            }
        }
    }
}

fn handle_terminal_event(
    api: &Api,
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'_>)>,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
    event: Event,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
                app.should_quit = true;
                return;
            }
            if app.overlay.is_some() {
                handle_overlay_key(api, app, active_follow, bg_tx, key);
            } else {
                handle_main_key(api, app, active_follow, bg_tx, key);
            }
        }
        Event::Paste(text) => {
            if app.overlay.is_none() {
                app.composer.insert_str(&text);
            }
        }
        Event::Mouse(mouse) if app.overlay.is_none() && app.session.is_some() => match mouse.kind {
            MouseEventKind::ScrollUp => {
                app.follow_tail = false;
                app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(3);
            }
            MouseEventKind::ScrollDown => {
                app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(3);
                if app.scroll_from_bottom == 0 {
                    app.follow_tail = true;
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn handle_overlay_key(
    api: &Api,
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'_>)>,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
    key: KeyEvent,
) {
    if matches!(app.overlay, Some(Overlay::Help)) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
            app.overlay = None;
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            app.overlay = None;
        }
        KeyCode::Up => with_overlay_picker_mut(app, |p| p.move_up()),
        KeyCode::Down => with_overlay_picker_mut(app, |p| p.move_down()),
        KeyCode::PageUp => with_overlay_picker_mut(app, |p| p.page_up(10)),
        KeyCode::PageDown => with_overlay_picker_mut(app, |p| p.page_down(10)),
        KeyCode::Backspace => with_overlay_picker_mut(app, |p| p.pop_char()),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            with_overlay_picker_mut(app, |p| p.clear_query());
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            with_overlay_picker_mut(app, |p| p.push_char(ch));
        }
        KeyCode::Enter => {
            let overlay = app.overlay.take();
            if let Some(overlay) = overlay {
                confirm_overlay_selection(api, app, active_follow, bg_tx, overlay);
            }
        }
        _ => {}
    }
}

fn with_overlay_picker_mut(app: &mut AppState, f: impl FnOnce(&mut dyn PickerControl)) {
    match &mut app.overlay {
        Some(Overlay::Sessions(p) | Overlay::Resume(p)) => f(p),
        Some(Overlay::Models(p)) => f(p),
        Some(Overlay::Providers(p)) => f(p),
        Some(Overlay::LocalSessions(p)) => f(p),
        Some(Overlay::Repos(p)) => f(p),
        Some(Overlay::Machines(p)) => f(p),
        Some(Overlay::Help) | None => {}
    }
}

trait PickerControl {
    fn move_up(&mut self);
    fn move_down(&mut self);
    fn page_up(&mut self, step: usize);
    fn page_down(&mut self, step: usize);
    fn push_char(&mut self, ch: char);
    fn pop_char(&mut self);
    fn clear_query(&mut self);
}

impl<T> PickerControl for Picker<T> {
    fn move_up(&mut self) {
        Self::move_up(self);
    }
    fn move_down(&mut self) {
        Self::move_down(self);
    }
    fn page_up(&mut self, step: usize) {
        Self::page_up(self, step);
    }
    fn page_down(&mut self, step: usize) {
        Self::page_down(self, step);
    }
    fn push_char(&mut self, ch: char) {
        Self::push_char(self, ch);
    }
    fn pop_char(&mut self) {
        Self::pop_char(self);
    }
    fn clear_query(&mut self) {
        Self::clear_query(self);
    }
}

fn confirm_overlay_selection(
    api: &Api,
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'_>)>,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
    overlay: Overlay,
) {
    match overlay {
        Overlay::Help => {}
        Overlay::Sessions(mut picker) => {
            if let Some(summary) = picker.take_selected() {
                app.busy_label = Some(format!("attaching to {}…", summary.title));
                let api = api.clone();
                let tx = bg_tx.clone();
                tokio::spawn(async move {
                    match api
                        .get::<SessionDetail>(&format!("/v1/sessions/{}", summary.id))
                        .await
                    {
                        Ok(detail) => spawn_attach(api, detail, true, tx),
                        Err(error) => {
                            let _ = tx.send(BgResult::AttachFailed(error));
                        }
                    }
                });
            }
        }
        Overlay::Resume(mut picker) => {
            if let Some(summary) = picker.take_selected() {
                app.busy_label = Some(format!("resuming {}…", summary.title));
                let api = api.clone();
                let tx = bg_tx.clone();
                tokio::spawn(async move {
                    let res = if summary.state.is_resumable() {
                        api.post::<serde_json::Value, SessionDetail>(
                            &format!("/v1/sessions/{}/resume", summary.id),
                            &serde_json::json!({}),
                        )
                        .await
                    } else {
                        api.get(&format!("/v1/sessions/{}", summary.id)).await
                    };
                    match res {
                        Ok(detail) => spawn_attach(api, detail, true, tx),
                        Err(error) => {
                            let _ = tx.send(BgResult::AttachFailed(error));
                        }
                    }
                });
            }
        }
        Overlay::Models(mut picker) => {
            if let Some(selection) = picker.take_selected() {
                apply_model_selection(api, app, selection, bg_tx);
            }
        }
        Overlay::Providers(mut picker) => {
            if let Some(harness) = picker.take_selected() {
                apply_provider_selection(app, active_follow, harness);
            }
        }
        Overlay::LocalSessions(mut picker) => {
            if let Some(local) = picker.take_selected() {
                start_local_native_resume(api, app, local, bg_tx);
            }
        }
        Overlay::Repos(mut picker) => {
            if let Some(repo) = picker.take_selected() {
                app.lobby.repo = Some(repo.slug.to_string());
                app.lobby.branch = None;
                app.push_notice(format!("repo -> {}", repo.slug));
            }
        }
        Overlay::Machines(mut picker) => {
            if let Some(entry) = picker.take_selected()
                && let Some(account) = entry.account
            {
                let choice = MachineChoice {
                    provider_account: account,
                    machine_type: entry.machine_type.clone(),
                    runtime: entry.runtime,
                    region: entry.region.clone(),
                    spot: entry.pricing.offers_spot() && app.lobby.spot,
                    disk_gib: flyco_core::DEFAULT_DISK_GIB,
                };
                app.lobby.machine_label =
                    Some(format!("{} ({})", choice.machine_type, choice.region));
                app.lobby.machine = Some(choice);
                app.push_notice(format!(
                    "machine -> {} ({})",
                    entry.machine_type, entry.region
                ));
            }
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "dispatches all main-view keybindings and composer shortcuts in one match"
)]
fn handle_main_key(
    api: &Api,
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'_>)>,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
    key: KeyEvent,
) {
    let palette = app.active_palette();
    let palette_open = !palette.is_empty();

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => {
                if !app.composer.is_empty() {
                    app.composer.clear();
                    app.palette_selected = 0;
                } else if let Some(session) = &app.session
                    && (session.turn_active
                        || session.active_shell.is_some()
                        || session.state == SessionState::Active)
                {
                    let id = session.id;
                    app.push_notice("interrupting…");
                    spawn_simple_post(
                        api.clone(),
                        format!("/v1/sessions/{id}/interrupt"),
                        "interrupt",
                        bg_tx.clone(),
                    );
                } else {
                    app.push_notice("press Ctrl+Q or type /quit to exit");
                }
                return;
            }
            KeyCode::Char('d') => {
                if !app.composer.is_empty() {
                    app.composer.delete();
                } else if app.session.is_some() {
                    detach_to_lobby(app, active_follow);
                } else {
                    app.should_quit = true;
                }
                return;
            }
            KeyCode::Char('u') => {
                app.composer.kill_to_start();
                app.palette_selected = 0;
                return;
            }
            KeyCode::Char('k') => {
                app.composer.kill_to_end();
                return;
            }
            KeyCode::Char('w') => {
                app.composer.delete_prev_word();
                app.palette_selected = 0;
                return;
            }
            KeyCode::Char('a') => {
                app.composer.move_home();
                return;
            }
            KeyCode::Char('e') => {
                app.composer.move_end();
                return;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Up => {
            if palette_open {
                app.palette_selected = if app.palette_selected == 0 {
                    palette.len() - 1
                } else {
                    app.palette_selected - 1
                };
            } else {
                app.composer.history_prev();
            }
        }
        KeyCode::Down => {
            if palette_open {
                app.palette_selected = (app.palette_selected + 1) % palette.len();
            } else {
                app.composer.history_next();
            }
        }
        KeyCode::Tab => {
            if palette_open {
                let idx = app.palette_selected.min(palette.len() - 1);
                let spec = palette[idx];
                let mut text = spec.name.to_owned();
                if !spec.args.is_empty() {
                    text.push(' ');
                }
                app.composer.set_text(text);
                app.palette_selected = 0;
            }
        }
        KeyCode::PageUp => {
            if app.session.is_some() {
                app.follow_tail = false;
                app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(10);
            }
        }
        KeyCode::PageDown => {
            if app.session.is_some() {
                app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(10);
                if app.scroll_from_bottom == 0 {
                    app.follow_tail = true;
                }
            }
        }
        KeyCode::End => {
            if app.composer.is_empty() && app.session.is_some() {
                app.follow_tail = true;
                app.scroll_from_bottom = 0;
            } else {
                app.composer.move_end();
            }
        }
        KeyCode::Home => app.composer.move_home(),
        KeyCode::Left => app.composer.move_left(),
        KeyCode::Right => app.composer.move_right(),
        KeyCode::Backspace => {
            app.composer.backspace();
            app.palette_selected = 0;
        }
        KeyCode::Delete => app.composer.delete(),
        KeyCode::Esc => {
            if !app.composer.is_empty() {
                app.composer.clear();
                app.palette_selected = 0;
            }
        }
        KeyCode::Enter => {
            if palette_open {
                let idx = app.palette_selected.min(palette.len() - 1);
                let chosen = palette[idx].command;
                app.composer.clear();
                app.palette_selected = 0;
                execute_slash(api, app, active_follow, bg_tx, chosen, "");
                return;
            }
            let raw = app.composer.take();
            app.palette_selected = 0;
            submit_line(api, app, active_follow, bg_tx, raw);
        }
        KeyCode::Char(ch) => {
            app.composer.insert_char(ch);
            app.palette_selected = 0;
        }
        _ => {}
    }
}

fn submit_line(
    api: &Api,
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'_>)>,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
    raw: String,
) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }

    if let Some((cmd, args)) = parse_slash(trimmed) {
        execute_slash(api, app, active_follow, bg_tx, cmd, args);
        return;
    }
    if trimmed.starts_with('/') {
        app.push_warning(format!("unknown command `{trimmed}` — type /help"));
        return;
    }

    // Inline approval response when an approval card is active.
    if let Some(pending) = app.pending_approval().cloned() {
        let decision = match trimmed.to_ascii_lowercase().as_str() {
            "a" | "allow" | "y" | "yes" => Some(ApprovalDecision::Approved),
            "d" | "deny" | "n" | "no" => Some(ApprovalDecision::Denied),
            _ => None,
        };
        if let Some(decision) = decision {
            spawn_approval(api.clone(), pending.id, decision, bg_tx.clone());
        } else {
            app.push_warning("approval pending — enter `a` to allow, `d` to deny, or a /command");
        }
        return;
    }

    // Shell escape `!cmd` inside an active session.
    if let Some(shell) = trimmed.strip_prefix('!') {
        let cmd = shell.trim();
        let Some(session) = &app.session else {
            app.push_warning("attach or start a session before running `!` shell commands");
            return;
        };
        if !cmd.is_empty() {
            let id = session.id;
            let api = api.clone();
            let tx = bg_tx.clone();
            let command = cmd.to_owned();
            let draft = raw;
            app.busy_label = Some("sending shell command…".to_owned());
            tokio::spawn(async move {
                let result = api
                    .post_json_empty(&format!("/v1/sessions/{id}/shell"), &RunShell { command })
                    .await;
                let _ = tx.send(BgResult::OutboundDone {
                    action: "shell",
                    draft: Some(draft),
                    result,
                });
            });
        }
        return;
    }

    // Plain message: either send to attached session or create a new cloud session from lobby!
    if let Some(session) = &app.session {
        let id = session.id;
        let api = api.clone();
        let tx = bg_tx.clone();
        let text = trimmed.to_owned();
        let draft = raw;
        app.follow_tail = true;
        app.scroll_from_bottom = 0;
        app.busy_label = Some("sending message…".to_owned());
        tokio::spawn(async move {
            let result = api
                .post_json_empty(
                    &format!("/v1/sessions/{id}/messages"),
                    &SendMessage { text },
                )
                .await;
            let _ = tx.send(BgResult::OutboundDone {
                action: "message",
                draft: Some(draft),
                result,
            });
        });
    } else {
        create_session_from_lobby(api, app, trimmed.to_owned(), bg_tx);
    }
}

fn execute_slash(
    api: &Api,
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'_>)>,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
    cmd: SlashCommand,
    args: &str,
) {
    match cmd {
        SlashCommand::Sessions => trigger_sessions_picker(api, app, bg_tx),
        SlashCommand::Resume => trigger_resume_picker(api, app, bg_tx),
        SlashCommand::Model => trigger_model_picker(api, app, args, bg_tx),
        SlashCommand::Provider => trigger_provider_switch(app, active_follow, args),
        SlashCommand::ResumeCodex => {
            trigger_local_resume(api, app, HarnessKind::Codex, args, bg_tx);
        }
        SlashCommand::ResumeClaude => {
            trigger_local_resume(api, app, HarnessKind::ClaudeCode, args, bg_tx);
        }
        SlashCommand::ResumeAgy => {
            trigger_local_resume(api, app, HarnessKind::Antigravity, args, bg_tx);
        }
        SlashCommand::ResumeDevin => {
            trigger_local_resume(api, app, HarnessKind::Devin, args, bg_tx);
        }
        SlashCommand::Repo => trigger_repo_picker(api, app, args, bg_tx),
        SlashCommand::Machine => trigger_machine_picker(api, app, bg_tx),
        SlashCommand::Budget => apply_budget(api, app, args, bg_tx),
        SlashCommand::New | SlashCommand::Detach => detach_to_lobby(app, active_follow),
        SlashCommand::Interrupt => {
            if let Some(session) = &app.session {
                let id = session.id;
                app.push_notice("interrupting…");
                spawn_simple_post(
                    api.clone(),
                    format!("/v1/sessions/{id}/interrupt"),
                    "interrupt",
                    bg_tx.clone(),
                );
            } else {
                app.push_warning("not attached to a session");
            }
        }
        SlashCommand::Compact => {
            if let Some(session) = &app.session {
                let id = session.id;
                app.push_notice("requesting compaction…");
                spawn_simple_post(
                    api.clone(),
                    format!("/v1/sessions/{id}/compact"),
                    "compact",
                    bg_tx.clone(),
                );
            } else {
                app.push_warning("not attached to a session");
            }
        }
        SlashCommand::Context => {
            if let Some(session) = &app.session {
                let id = session.id;
                spawn_simple_post(
                    api.clone(),
                    format!("/v1/sessions/{id}/context"),
                    "context",
                    bg_tx.clone(),
                );
            } else {
                app.push_warning("not attached to a session");
            }
        }
        SlashCommand::Status => push_status_summary(app),
        SlashCommand::Help => {
            app.overlay = Some(Overlay::Help);
        }
        SlashCommand::Quit => {
            app.should_quit = true;
        }
    }
}

fn trigger_sessions_picker(api: &Api, app: &mut AppState, bg_tx: &mpsc::UnboundedSender<BgResult>) {
    app.busy_label = Some("loading active sessions…".to_owned());
    let api = api.clone();
    let tx = bg_tx.clone();
    tokio::spawn(async move {
        let result = api.get::<Vec<SessionSummary>>("/v1/sessions").await;
        let _ = tx.send(BgResult::ActiveSessions(result));
    });
}

fn trigger_resume_picker(api: &Api, app: &mut AppState, bg_tx: &mpsc::UnboundedSender<BgResult>) {
    app.busy_label = Some("loading inactive sessions…".to_owned());
    let api = api.clone();
    let tx = bg_tx.clone();
    tokio::spawn(async move {
        let result = api.get::<Vec<SessionSummary>>("/v1/sessions").await;
        let _ = tx.send(BgResult::InactiveSessions(result));
    });
}

fn trigger_model_picker(
    api: &Api,
    app: &mut AppState,
    query: &str,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
) {
    let harness = app.active_harness();
    app.busy_label = Some("loading models…".to_owned());
    let api = api.clone();
    let tx = bg_tx.clone();
    let query = query.trim().to_owned();
    tokio::spawn(async move {
        let result = api
            .get::<Vec<HarnessAccountView>>("/v1/harness-accounts")
            .await;
        let _ = tx.send(BgResult::ModelsLoaded {
            harness,
            query,
            result,
        });
    });
}

fn trigger_provider_switch(
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'_>)>,
    args: &str,
) {
    if !args.is_empty() {
        let parsed = match args.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude_code" | "claude-code" => Some(HarnessKind::ClaudeCode),
            "codex" => Some(HarnessKind::Codex),
            "devin" => Some(HarnessKind::Devin),
            "agy" | "antigravity" => Some(HarnessKind::Antigravity),
            _ => None,
        };
        if let Some(harness) = parsed {
            apply_provider_selection(app, active_follow, harness);
            return;
        }
    }
    let current = app.active_harness();
    let all = [
        (HarnessKind::ClaudeCode, "Claude Code"),
        (HarnessKind::Codex, "Codex"),
        (HarnessKind::Devin, "Devin"),
        (HarnessKind::Antigravity, "Antigravity"),
    ];
    let entries = all
        .into_iter()
        .map(|(harness, display)| {
            let linked = app.harness_accounts.iter().find(|a| a.harness == harness);
            let badge = if harness == current {
                "CURRENT".to_owned()
            } else if linked.is_some() {
                "LINKED".to_owned()
            } else {
                "UNLINKED".to_owned()
            };
            let secondary = linked.map_or_else(
                || "not linked".to_owned(),
                |a| format!("{} ({} models)", a.label, a.models.len()),
            );
            PickerEntry {
                value: harness,
                primary: display.to_owned(),
                secondary,
                badge,
                trailing: harness_label(harness).to_owned(),
                search_extra: format!("{display} {}", harness_label(harness)),
            }
        })
        .collect();
    app.overlay = Some(Overlay::Providers(Picker::new(
        "Harnesses (/provider)",
        entries,
        args,
        "No harnesses available",
    )));
}

fn trigger_repo_picker(
    api: &Api,
    app: &mut AppState,
    query: &str,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
) {
    app.busy_label = Some("loading repositories…".to_owned());
    let api = api.clone();
    let tx = bg_tx.clone();
    let query = query.trim().to_owned();
    tokio::spawn(async move {
        let result = api.get::<Vec<RepoSummary>>("/v1/github/repos").await;
        let _ = tx.send(BgResult::ReposLoaded { query, result });
    });
}

fn trigger_machine_picker(api: &Api, app: &mut AppState, bg_tx: &mpsc::UnboundedSender<BgResult>) {
    app.busy_label = Some("loading machine catalog…".to_owned());
    let api = api.clone();
    let tx = bg_tx.clone();
    tokio::spawn(async move {
        let catalog = api.get::<MachineCatalog>("/v1/machines/catalog").await;
        let default = api.get::<MachineDefault>("/v1/machines/default").await.ok();
        let _ = tx.send(BgResult::MachinesLoaded(catalog.map(|c| (c, default))));
    });
}

fn trigger_local_resume(
    api: &Api,
    app: &mut AppState,
    harness: HarnessKind,
    query: &str,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
) {
    let query = query.trim().to_owned();
    if !query.is_empty() && std::path::Path::new(&query).is_file() {
        app.busy_label = Some(format!(
            "resolving local {} session…",
            harness_label(harness)
        ));
        let api = api.clone();
        let tx = bg_tx.clone();
        let lobby = app.lobby.clone();
        tokio::spawn(async move {
            match local_sessions::resolve_query(harness, &query).await {
                Ok(local) => run_local_resume_upload(api, local, lobby, tx).await,
                Err(error) => {
                    let _ = tx.send(BgResult::AttachFailed(error));
                }
            }
        });
        return;
    }

    app.busy_label = Some(format!(
        "scanning local {} sessions…",
        harness_label(harness)
    ));
    let tx = bg_tx.clone();
    tokio::spawn(async move {
        let result = local_sessions::discover(harness, None).await;
        let _ = tx.send(BgResult::LocalDiscovered {
            harness,
            query,
            result,
        });
    });
}

fn start_local_native_resume(
    api: &Api,
    app: &mut AppState,
    local: LocalSession,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
) {
    app.busy_label = Some(format!(
        "uploading {} session {}…",
        harness_label(local.harness),
        local.session_id
    ));
    let api = api.clone();
    let lobby = app.lobby.clone();
    let tx = bg_tx.clone();
    tokio::spawn(async move {
        run_local_resume_upload(api, local, lobby, tx).await;
    });
}

async fn run_local_resume_upload(
    api: Api,
    local: LocalSession,
    lobby: LobbyConfig,
    tx: mpsc::UnboundedSender<BgResult>,
) {
    let probe_dir = local
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let local_repo =
        local_sessions::try_local_repo(&probe_dir, lobby.repo.as_deref(), lobby.branch.as_deref())
            .await;

    let mut spec = SessionSpec {
        budget: Some(lobby.budget),
        env: lobby.env.clone(),
        permission_mode: lobby.permission_mode,
        on_demand: !lobby.spot,
        ..SessionSpec::default()
    };
    if let Some(repo) = &lobby.repo {
        spec.repo.push(repo.clone());
    }
    spec.branch = lobby.branch.clone();
    if let Some(machine) = &lobby.machine {
        spec.account = Some(machine.provider_account.to_string());
        spec.machine = Some(machine.machine_type.clone());
        spec.region = Some(machine.region.clone());
        spec.runtime = Some(machine.runtime);
        spec.disk = Some(machine.disk_gib);
        spec.on_demand = !machine.spot;
    }
    if lobby.harness == local.harness
        && let Some(model) = &lobby.model
    {
        spec.model = Some(model.model.clone());
        spec.effort = model.effort.clone();
    }

    if spec.repo.is_empty() && local_repo.is_none() {
        let _ = tx.send(BgResult::AttachFailed(Failure::usage(
            "could not infer a GitHub repository for that local session — pick one with `/repo` first",
        )));
        return;
    }

    match local_sessions::resume_to_cloud(&api, &local, local_repo.as_ref(), spec, None, false)
        .await
    {
        Ok(detail) => spawn_attach(api, detail, false, tx),
        Err(error) => {
            let _ = tx.send(BgResult::AttachFailed(error));
        }
    }
}

fn create_session_from_lobby(
    api: &Api,
    app: &mut AppState,
    prompt: String,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
) {
    let Some(repo) = app.lobby.repo.clone() else {
        app.push_warning("select a repository first with `/repo`");
        app.composer.set_text(prompt);
        return;
    };
    let lobby = app.lobby.clone();
    let api = api.clone();
    let tx = bg_tx.clone();
    app.busy_label = Some(format!(
        "creating {} session on {repo}…",
        harness_label(lobby.harness)
    ));
    tokio::spawn(async move {
        let request = CreateSession {
            prompt,
            harness: lobby.harness,
            repos: vec![RepoSelection {
                repo,
                branch: lobby.branch,
            }],
            budget_limit: lobby.budget,
            machine: lobby.machine,
            spot: lobby.spot,
            model: lobby.model,
            permission_mode: lobby.permission_mode,
            source: None,
        };
        match api
            .post::<CreateSession, SessionDetail>("/v1/sessions", &request)
            .await
        {
            Ok(detail) => {
                if let Err(error) =
                    crate::session::set_env(&api, &detail.summary.id, &lobby.env).await
                {
                    let _ = tx.send(BgResult::AttachFailed(error));
                    return;
                }
                spawn_attach(api, detail, false, tx);
            }
            Err(error) => {
                let _ = tx.send(BgResult::AttachFailed(error));
            }
        }
    });
}

fn apply_model_selection(
    api: &Api,
    app: &mut AppState,
    selection: ModelSelection,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
) {
    let choice = ModelChoice {
        model: selection.model,
        effort: selection.effort,
    };
    if let Some(session) = &app.session {
        let session_id = session.id;
        app.busy_label = Some(format!(
            "switching model to {}…",
            format_model_choice(&choice)
        ));
        let api = api.clone();
        let tx = bg_tx.clone();
        tokio::spawn(async move {
            let result = api
                .patch::<UpdateSession, SessionDetail>(
                    &format!("/v1/sessions/{session_id}"),
                    &UpdateSession {
                        title: None,
                        budget_limit: None,
                        model: Some(choice),
                        permission_mode: None,
                        computer_use: None,
                    },
                )
                .await;
            let _ = tx.send(BgResult::ModelUpdated { session_id, result });
        });
    } else {
        let label = format_model_choice(&choice);
        app.lobby.model = Some(choice);
        app.push_notice(format!("lobby model -> {label}"));
    }
}

fn apply_provider_selection(
    app: &mut AppState,
    active_follow: &mut Option<(SessionId, Follow<'_>)>,
    harness: HarnessKind,
) {
    let was_attached_other = app.session.as_ref().is_some_and(|s| s.harness != harness);
    app.lobby.harness = harness;
    let models = models_for_harness(&app.harness_accounts, harness);
    app.lobby.model = (!models.is_empty()).then(|| ModelChoice::default_of(&models));
    if was_attached_other {
        *active_follow = None;
        app.session = None;
        app.follow_tail = true;
        app.scroll_from_bottom = 0;
        app.push_notice(format!(
            "detached to lobby — harness switched to {}",
            harness_label(harness)
        ));
    } else {
        app.push_notice(format!("harness -> {}", harness_label(harness)));
    }
}

fn apply_budget(
    api: &Api,
    app: &mut AppState,
    args: &str,
    bg_tx: &mpsc::UnboundedSender<BgResult>,
) {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        app.push_warning("usage: /budget <usd> (e.g. /budget 15)");
        return;
    }
    let usd = match crate::cli::parse_usd(trimmed) {
        Ok(usd) => usd,
        Err(msg) => {
            app.push_error(msg);
            return;
        }
    };
    if let Some(session) = &app.session {
        let session_id = session.id;
        app.busy_label = Some(format!("updating budget to {usd}…"));
        let api = api.clone();
        let tx = bg_tx.clone();
        tokio::spawn(async move {
            let result = api
                .patch::<UpdateSession, SessionDetail>(
                    &format!("/v1/sessions/{session_id}"),
                    &UpdateSession {
                        title: None,
                        budget_limit: Some(usd),
                        model: None,
                        permission_mode: None,
                        computer_use: None,
                    },
                )
                .await;
            let _ = tx.send(BgResult::BudgetUpdated { session_id, result });
        });
    } else {
        app.lobby.budget = usd;
        app.push_notice(format!("lobby budget -> {usd}"));
    }
}

fn detach_to_lobby(app: &mut AppState, active_follow: &mut Option<(SessionId, Follow<'_>)>) {
    *active_follow = None;
    if let Some(old) = app.session.take() {
        app.follow_tail = true;
        app.scroll_from_bottom = 0;
        app.push_notice(format!(
            "detached from {} (still running in cloud — `/sessions` re-attaches)",
            old.id
        ));
    }
}

fn push_status_summary(app: &mut AppState) {
    if let Some(session) = &app.session {
        let swarm_info = session.latest_swarm.as_ref().map_or_else(
            || "none".to_owned(),
            |s| {
                format!(
                    "{} members, {} messages",
                    s.members.len(),
                    session.swarm_message_count
                )
            },
        );
        let summary = format!(
            "session {}  harness={}  model={}  state={:?}  budget={}/{}  swarm={swarm_info}",
            session.id,
            harness_label(session.harness),
            format_model_choice(&session.model),
            session.state,
            session.budget.spent,
            session.budget.limit
        );
        app.push_notice(summary);
    } else {
        let summary = format!(
            "lobby  harness={}  model={}  repo={}  budget={}",
            harness_label(app.lobby.harness),
            app.lobby
                .model
                .as_ref()
                .map_or_else(|| "default".to_owned(), format_model_choice),
            app.lobby.repo.as_deref().unwrap_or("(unset)"),
            app.lobby.budget
        );
        app.push_notice(summary);
    }
}

fn spawn_simple_post(
    api: Api,
    path: String,
    action: &'static str,
    tx: mpsc::UnboundedSender<BgResult>,
) {
    tokio::spawn(async move {
        let result = api.post_json_empty(&path, &serde_json::json!({})).await;
        let _ = tx.send(BgResult::OutboundDone {
            action,
            draft: None,
            result,
        });
    });
}

fn spawn_approval(
    api: Api,
    id: ApprovalId,
    decision: ApprovalDecision,
    tx: mpsc::UnboundedSender<BgResult>,
) {
    tokio::spawn(async move {
        let result = api
            .post_json_empty(
                &format!("/v1/approvals/{id}/decision"),
                &DecideApproval { decision },
            )
            .await;
        let _ = tx.send(BgResult::OutboundDone {
            action: "approval",
            draft: None,
            result,
        });
    });
}

fn models_for_harness(accounts: &[HarnessAccountView], harness: HarnessKind) -> Vec<ModelOption> {
    if let Some(account) = accounts.iter().find(|a| a.harness == harness)
        && !account.models.is_empty()
    {
        return account.models.clone();
    }
    flyco_core::builtin_models(harness)
}

fn session_picker_entry(session: SessionSummary) -> PickerEntry<SessionSummary> {
    let badge = format!(
        "{} {}",
        harness_label(session.harness),
        crate::tui::transcript::state_label(session.state)
    );
    let secondary = crate::session::repos_label(&session.repos);
    let trailing = format_unix_age(session.last_active_unix);
    let search_extra = format!("{} {}", session.id, format_model_choice(&session.model));
    PickerEntry {
        primary: session.title.clone(),
        secondary,
        badge,
        trailing,
        search_extra,
        value: session,
    }
}

fn shorten_home(path: &std::path::Path) -> String {
    if let Some(home) = std::env::home_dir()
        && let Ok(rel) = path.strip_prefix(&home)
    {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

fn format_age(modified: SystemTime) -> String {
    let Ok(elapsed) = SystemTime::now().duration_since(modified) else {
        return "now".to_owned();
    };
    format_secs_age(elapsed.as_secs())
}

fn format_unix_age(unix: u64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_secs_age(now.saturating_sub(unix))
}

fn format_secs_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}
