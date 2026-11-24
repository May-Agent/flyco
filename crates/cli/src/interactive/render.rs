//! Pure event-to-terminal renderer for the interactive ACP/SSE client.
//!
//! Translates [`SessionDetail`], [`ClientEvent`]s, [`HarnessEvent`]s, pending
//! [`ApprovalView`]s, and future/unknown JSON event payloads (such as read-only
//! swarm events) into clean terminal output while maintaining live session
//! state for the prompt bar and `/status`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

use flyco_core::wire::{
    ApprovalDecision, ApprovalPayload, ClientEvent, DesktopStatus, HarnessCommand, MessageOrigin,
    ProvisioningStage, ShellOutcome, ShellStream, UsageWindow,
};
use flyco_core::{
    ApprovalId, ApprovalState, ApprovalView, BudgetStage, BudgetView, ContextCost, ContextUsage,
    HarnessEvent, HarnessKind, InterruptedReason, ModelChoice, PausedReason, PermissionMode,
    SessionActivity, SessionDetail, SessionId, SessionRepo, SessionState, ShellRunId,
    UsageLimitPause, UsageReport,
};

use crate::follow::Item;

/// Maximum inline characters shown for a generic tool input JSON summary.
const MAX_TOOL_SUMMARY_CHARS: usize = 160;

/// Maximum diff lines printed inline for an edit tool call or approval card.
const MAX_DIFF_LINES: usize = 12;

/// One pending approval waiting for the user's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// Approval identifier sent to `POST /v1/approvals/{id}/decision`.
    pub id: ApprovalId,
    /// What the session is asking to do.
    pub payload: ApprovalPayload,
}

/// Optional ANSI styling controlled by a boolean so tests assert plain text.
#[derive(Debug, Clone, Copy)]
struct Style {
    color: bool,
}

impl Style {
    const fn new(color: bool) -> Self {
        Self { color }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }

    fn cyan(self, text: &str) -> String {
        self.paint("36", text)
    }

    fn bold_yellow(self, text: &str) -> String {
        self.paint("1;33", text)
    }

    fn bold_cyan(self, text: &str) -> String {
        self.paint("1;36", text)
    }

    fn bold_green(self, text: &str) -> String {
        self.paint("1;32", text)
    }
}

/// Stateful ACP/SSE transcript renderer for one interactive session.
#[derive(Debug)]
pub struct Renderer {
    style: Style,
    id: SessionId,
    title: String,
    harness: HarnessKind,
    repos: Vec<SessionRepo>,
    state: SessionState,
    activity: SessionActivity,
    paused_reason: Option<PausedReason>,
    interrupted_reason: Option<InterruptedReason>,
    usage_limit: Option<UsageLimitPause>,
    budget: BudgetView,
    failure: Option<String>,
    model: ModelChoice,
    permission_mode: PermissionMode,
    machine_connected: Option<bool>,
    turn_active: bool,
    active_shell: Option<ShellRunId>,
    latest_usage: Option<UsageReport>,
    commands: Vec<HarnessCommand>,
    pending_approvals: VecDeque<PendingApproval>,
    decided_approvals: HashSet<ApprovalId>,
    announced_decisions: HashSet<ApprovalId>,
    active_tools: HashMap<String, String>,
    streaming_assistant: bool,
    at_line_start: bool,
}

impl Renderer {
    /// Creates a renderer seeded from the session's current [`SessionDetail`].
    #[must_use]
    pub fn new(detail: SessionDetail, color: bool) -> Self {
        let turn_active = detail.summary.state == SessionState::Active
            && detail.summary.activity == SessionActivity::Working;
        Self {
            style: Style::new(color),
            id: detail.summary.id,
            title: detail.summary.title,
            harness: detail.summary.harness,
            repos: detail.summary.repos,
            state: detail.summary.state,
            activity: detail.summary.activity,
            paused_reason: detail.summary.paused_reason,
            interrupted_reason: detail.summary.interrupted_reason,
            usage_limit: detail.usage_limit,
            budget: detail.budget,
            failure: detail.failure,
            model: detail.summary.model,
            permission_mode: detail.summary.permission_mode,
            machine_connected: None,
            turn_active,
            active_shell: None,
            latest_usage: None,
            commands: Vec::new(),
            pending_approvals: VecDeque::new(),
            decided_approvals: HashSet::new(),
            announced_decisions: HashSet::new(),
            active_tools: HashMap::new(),
            streaming_assistant: false,
            at_line_start: true,
        }
    }

    /// Updates cached session metadata from a refreshed [`SessionDetail`].
    pub fn sync_detail(&mut self, detail: SessionDetail) {
        self.title = detail.summary.title;
        self.repos = detail.summary.repos;
        self.state = detail.summary.state;
        self.activity = detail.summary.activity;
        self.paused_reason = detail.summary.paused_reason;
        self.interrupted_reason = detail.summary.interrupted_reason;
        self.usage_limit = detail.usage_limit;
        self.budget = detail.budget;
        self.failure = detail.failure;
        self.model = detail.summary.model;
        self.permission_mode = detail.summary.permission_mode;
    }

    /// Current session lifecycle state.
    #[must_use]
    pub const fn session_state(&self) -> SessionState {
        self.state
    }

    /// Current session failure message, if known.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Whether a harness turn is currently in progress.
    #[must_use]
    pub const fn turn_active(&self) -> bool {
        self.turn_active
    }

    /// Active `!` shell command run id, if one is running.
    #[must_use]
    pub const fn active_shell(&self) -> Option<ShellRunId> {
        self.active_shell
    }

    /// Whether the renderer is mid-line on streamed output (not at column 0).
    #[must_use]
    pub const fn at_line_start(&self) -> bool {
        self.at_line_start
    }

    /// Whether assistant delta text is actively mid-line without a trailing newline.
    #[must_use]
    pub const fn is_mid_stream(&self) -> bool {
        self.streaming_assistant && !self.at_line_start
    }

    /// The next pending approval awaiting the user's decision, if any.
    #[must_use]
    pub fn front_approval(&self) -> Option<&PendingApproval> {
        self.pending_approvals.front()
    }

    /// Marks an approval as decided locally after the REST decision succeeds
    /// (or conflicts because another client decided it first).
    pub fn resolve_approval_local(
        &mut self,
        id: ApprovalId,
        decision: Option<ApprovalDecision>,
    ) -> String {
        self.pending_approvals.retain(|entry| entry.id != id);
        self.decided_approvals.insert(id);
        if !self.announced_decisions.insert(id) {
            return String::new();
        }
        let mut out = String::new();
        self.ensure_line_start(&mut out);
        match decision {
            Some(ApprovalDecision::Approved) => {
                let _ = writeln!(out, "{}", self.style.green("✓ approval allowed"));
            }
            Some(ApprovalDecision::Denied) => {
                let _ = writeln!(out, "{}", self.style.red("✗ approval denied"));
            }
            None => {
                let _ = writeln!(out, "{}", self.style.dim("─ approval already decided"));
            }
        }
        self.at_line_start = true;
        out
    }

    /// Renders the opening session banner.
    #[must_use]
    pub fn render_header(&mut self) -> String {
        let mut out = String::new();
        let harness = harness_name(self.harness);
        let state = state_label(self.state, self.paused_reason, self.interrupted_reason);
        let _ = writeln!(
            out,
            "{}  {}  ({}, {})",
            self.style.bold("Session"),
            self.id,
            harness,
            state
        );
        if !self.repos.is_empty() {
            let repos = self
                .repos
                .iter()
                .map(|repo| {
                    repo.branch.as_ref().map_or_else(
                        || repo.slug.to_string(),
                        |branch| format!("{} [{branch}]", repo.slug),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "{}     {repos}", self.style.bold("Repo"));
        }
        let model = format_model(&self.model);
        let mode = permission_mode_label(self.permission_mode);
        let _ = writeln!(
            out,
            "{}   {model}   {mode}   budget {}/{}",
            self.style.bold("Config"),
            self.budget.spent,
            self.budget.limit
        );
        if let Some(notice) = self.render_pause_or_failure_notice() {
            out.push_str(&notice);
        }
        let _ = writeln!(
            out,
            "{}",
            self.style
                .dim("Commands: !cmd  /interrupt  /compact  /context  /status  /detach (Ctrl-D)")
        );
        self.at_line_start = true;
        out
    }

    /// Renders a pause or failure notice from the current session metadata.
    #[must_use]
    pub fn render_pause_or_failure_notice(&self) -> Option<String> {
        let mut out = String::new();
        if self.state == SessionState::Paused {
            match self.paused_reason {
                Some(PausedReason::Budget) => {
                    let _ = writeln!(
                        out,
                        "{}",
                        self.style.yellow(&format!(
                            "⚠ paused: compute budget exhausted ({}/{}) — raise with `flyco session set {} --budget <USD>`",
                            self.budget.spent, self.budget.limit, self.id
                        ))
                    );
                }
                Some(PausedReason::UsageLimit) => {
                    let detail = self.usage_limit.as_ref().map_or_else(
                        || "plan usage limit reached".to_owned(),
                        |pause| {
                            let queued = pause
                                .queued_message
                                .as_ref()
                                .map_or_else(String::new, |msg| {
                                    format!(" (queued: {})", truncate_chars(msg, 60))
                                });
                            format!(
                                "{} window reached — resumes automatically at reset{queued}",
                                pause.window
                            )
                        },
                    );
                    let _ = writeln!(out, "{}", self.style.yellow(&format!("⚠ paused: {detail}")));
                }
                None => {
                    let _ = writeln!(out, "{}", self.style.yellow("⚠ session paused"));
                }
            }
        } else if self.budget.stage == BudgetStage::Final90 {
            let _ = writeln!(
                out,
                "{}",
                self.style.yellow(&format!(
                    "⚠ budget warning: 90% spent ({}/{})",
                    self.budget.spent, self.budget.limit
                ))
            );
        } else if self.budget.stage == BudgetStage::Warn80 {
            let _ = writeln!(
                out,
                "{}",
                self.style.yellow(&format!(
                    "⚠ budget warning: 80% spent ({}/{})",
                    self.budget.spent, self.budget.limit
                ))
            );
        }
        if let Some(failure) = &self.failure {
            let _ = writeln!(
                out,
                "{}",
                self.style.red(&format!("✗ session failure: {failure}"))
            );
        }
        (!out.is_empty()).then_some(out)
    }

    /// Reconciles server-side pending approvals (fetched on attach/resume)
    /// against those already seen in history and renders cards for any still
    /// waiting.
    #[must_use]
    pub fn render_pending_approvals(&mut self, server_pending: Vec<ApprovalView>) -> String {
        for approval in server_pending {
            if approval.state != ApprovalState::Pending {
                continue;
            }
            if self.decided_approvals.contains(&approval.id)
                || self
                    .pending_approvals
                    .iter()
                    .any(|existing| existing.id == approval.id)
            {
                continue;
            }
            self.pending_approvals.push_back(PendingApproval {
                id: approval.id,
                payload: approval.payload,
            });
        }
        if self.pending_approvals.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        self.ensure_line_start(&mut out);
        let snapshot: Vec<PendingApproval> = self.pending_approvals.iter().cloned().collect();
        for pending in &snapshot {
            out.push_str(&self.format_approval_card(pending));
        }
        self.at_line_start = true;
        out
    }

    /// Formats the local prompt prefix based on current session & send state.
    #[must_use]
    pub fn prompt_prefix(&self, sending: bool) -> String {
        if let Some(approval) = self.pending_approvals.front() {
            let extra = self.pending_approvals.len().saturating_sub(1);
            let label = if extra > 0 {
                format!("approve (+{extra}) [a/d] > ")
            } else {
                let _ = approval.id;
                "approve [a/d] > ".to_owned()
            };
            return self.style.bold_yellow(&label);
        }
        if sending {
            return self.style.dim("sending… > ");
        }
        if self.turn_active {
            return self.style.cyan("working > ");
        }
        match self.state {
            SessionState::Provisioning => self.style.dim("provisioning > "),
            SessionState::Paused => self.style.yellow("paused > "),
            SessionState::Interrupted => self.style.yellow("reconnecting > "),
            SessionState::Active | SessionState::Archived | SessionState::Failed => {
                self.style.bold_green("> ")
            }
        }
    }

    /// Renders a local `/status` summary without issuing any API requests.
    #[must_use]
    pub fn format_status(&self) -> String {
        let mut out = String::new();
        let state = state_label(self.state, self.paused_reason, self.interrupted_reason);
        let _ = writeln!(out, "Session  {} ({})", self.id, harness_name(self.harness));
        let _ = writeln!(out, "State    {state} (activity: {:?})", self.activity);
        let _ = writeln!(
            out,
            "Model    {}  mode: {}",
            format_model(&self.model),
            permission_mode_label(self.permission_mode)
        );
        let _ = writeln!(
            out,
            "Budget   {}/{} (remaining: {})",
            self.budget.spent, self.budget.limit, self.budget.remaining
        );
        if let Some(usage) = self.latest_usage {
            let _ = writeln!(out, "Usage    {}", format_usage(usage));
        }
        if !self.pending_approvals.is_empty() {
            let _ = writeln!(out, "Pending  {} approval(s)", self.pending_approvals.len());
        }
        if let Some(failure) = &self.failure {
            let _ = writeln!(out, "Failure  {failure}");
        }
        out
    }

    /// Renders local `/help`, including harness-advertised slash commands if any.
    #[must_use]
    pub fn format_help(&self) -> String {
        let mut out = String::from(
            "Local commands:\n  \
             !<cmd>       run a shell command on the session machine\n  \
             /interrupt   interrupt the active turn (also Ctrl-C)\n  \
             /compact     compact conversation context\n  \
             /context     show context-window breakdown\n  \
             /status      show session state, model, budget, and usage\n  \
             /approve     allow the active pending approval\n  \
             /deny        deny the active pending approval\n  \
             /detach      detach and leave the session running (also Ctrl-D / Ctrl-])\n  \
             /help        show this help\n",
        );
        if !self.commands.is_empty() {
            out.push_str("Harness commands (forwarded to the agent):\n");
            for cmd in &self.commands {
                let hint = cmd
                    .argument_hint
                    .as_deref()
                    .map_or(String::new(), |h| format!(" {h}"));
                let _ = writeln!(out, "  /{}{}  {}", cmd.name, hint, cmd.description);
            }
        }
        out
    }

    /// Formats a status/notice line styled consistently with the renderer.
    #[must_use]
    pub fn format_notice(&mut self, text: &str) -> String {
        let mut out = String::new();
        self.ensure_line_start(&mut out);
        let _ = writeln!(out, "{}", self.style.dim(text));
        self.at_line_start = true;
        out
    }

    /// Formats a warning line styled consistently with the renderer.
    #[must_use]
    pub fn format_warning(&mut self, text: &str) -> String {
        let mut out = String::new();
        self.ensure_line_start(&mut out);
        let _ = writeln!(out, "{}", self.style.yellow(text));
        self.at_line_start = true;
        out
    }

    /// Formats an error line styled consistently with the renderer.
    #[must_use]
    pub fn format_error(&mut self, text: &str) -> String {
        let mut out = String::new();
        self.ensure_line_start(&mut out);
        let _ = writeln!(out, "{}", self.style.red(text));
        self.at_line_start = true;
        out
    }

    /// Applies one [`follow::Item`](crate::follow::Item) and returns any
    /// terminal text to emit.
    #[must_use]
    pub fn apply_item(&mut self, item: &Item, replay: bool) -> String {
        if let Some(event) = item.event() {
            return self.apply_event(&event, replay);
        }
        if let Item::Replayed(stored) = item {
            return self.apply_raw_event(&stored.event);
        }
        String::new()
    }

    /// Applies a typed [`ClientEvent`] and returns any terminal text to emit.
    #[expect(
        clippy::too_many_lines,
        reason = "exhaustive rendering of every ClientEvent variant in one place keeps protocol coverage visible"
    )]
    #[must_use]
    pub fn apply_event(&mut self, event: &ClientEvent, replay: bool) -> String {
        let mut out = String::new();
        match event {
            ClientEvent::Harness { event } => {
                self.render_harness_event(event, &mut out);
            }
            ClientEvent::UserMessage { text, origin } => {
                self.streaming_assistant = false;
                self.ensure_line_start(&mut out);
                match origin {
                    MessageOrigin::User => {
                        let _ = writeln!(out, "{} {text}", self.style.bold_cyan("you>"));
                    }
                    MessageOrigin::Flyco => {
                        let _ = writeln!(out, "{} {text}", self.style.yellow("[flyco]"));
                    }
                }
                self.at_line_start = true;
            }
            ClientEvent::ShellCommand { run, command } => {
                self.streaming_assistant = false;
                self.active_shell = Some(*run);
                self.ensure_line_start(&mut out);
                let _ = writeln!(out, "{} {command}", self.style.bold("$"));
                self.at_line_start = true;
            }
            ClientEvent::ShellOutput { stream, data, .. } => {
                if !data.is_empty() {
                    match stream {
                        ShellStream::Stdout => out.push_str(data),
                        ShellStream::Stderr => out.push_str(&self.style.yellow(data)),
                    }
                    self.at_line_start = data.ends_with('\n');
                }
            }
            ClientEvent::ShellExited {
                run,
                outcome,
                truncated,
            } => {
                if self.active_shell == Some(*run) {
                    self.active_shell = None;
                }
                self.ensure_line_start(&mut out);
                let suffix = if *truncated { " (truncated)" } else { "" };
                let line = match outcome {
                    ShellOutcome::Exited { code: 0 } => {
                        self.style.dim(&format!("─ shell exited (0){suffix}"))
                    }
                    ShellOutcome::Exited { code } => self
                        .style
                        .red(&format!("✗ shell exited (code {code}){suffix}")),
                    ShellOutcome::Signalled => {
                        self.style.red(&format!("✗ shell killed by signal{suffix}"))
                    }
                    ShellOutcome::TimedOut { after_seconds } => self
                        .style
                        .red(&format!("✗ shell timed out after {after_seconds}s{suffix}")),
                    ShellOutcome::Cancelled => {
                        self.style.dim(&format!("─ shell cancelled{suffix}"))
                    }
                    ShellOutcome::Offline => {
                        self.style.red("✗ shell unavailable (machine offline)")
                    }
                    ShellOutcome::Busy => self
                        .style
                        .yellow("⚠ another shell command is already running"),
                    ShellOutcome::Refused => self
                        .style
                        .yellow("⚠ shell refused (session paused or reclaiming)"),
                    ShellOutcome::Failed { error } => {
                        self.style.red(&format!("✗ shell failed: {error}"))
                    }
                };
                let _ = writeln!(out, "{line}");
                self.at_line_start = true;
            }
            ClientEvent::ApprovalPending { id, payload } => {
                if !self.decided_approvals.contains(id)
                    && !self.pending_approvals.iter().any(|entry| entry.id == *id)
                {
                    let pending = PendingApproval {
                        id: *id,
                        payload: payload.clone(),
                    };
                    self.pending_approvals.push_back(pending.clone());
                    if !replay {
                        self.streaming_assistant = false;
                        self.ensure_line_start(&mut out);
                        out.push_str(&self.format_approval_card(&pending));
                        self.at_line_start = true;
                    }
                }
            }
            ClientEvent::ApprovalDecided { id, decision } => {
                self.pending_approvals.retain(|entry| entry.id != *id);
                self.decided_approvals.insert(*id);
                if self.announced_decisions.insert(*id) {
                    self.streaming_assistant = false;
                    self.ensure_line_start(&mut out);
                    match decision {
                        ApprovalDecision::Approved => {
                            let _ = writeln!(out, "{}", self.style.green("✓ approval allowed"));
                        }
                        ApprovalDecision::Denied => {
                            let _ = writeln!(out, "{}", self.style.red("✗ approval denied"));
                        }
                    }
                    self.at_line_start = true;
                }
            }
            ClientEvent::SessionStateChanged { state } => {
                self.state = *state;
                self.streaming_assistant = false;
                if *state != SessionState::Active {
                    self.turn_active = false;
                }
                self.ensure_line_start(&mut out);
                let line = match state {
                    SessionState::Provisioning => self.style.dim("─ session provisioning"),
                    SessionState::Active => self.style.green("─ session active"),
                    SessionState::Paused => self.style.yellow("⚠ session paused"),
                    SessionState::Interrupted => self
                        .style
                        .yellow("⚠ machine interrupted — waiting for recovery"),
                    SessionState::Archived => self.style.dim("─ session archived"),
                    SessionState::Failed => self.style.red("✗ session failed"),
                };
                let _ = writeln!(out, "{line}");
                self.at_line_start = true;
            }
            ClientEvent::MachineConnection { connected } => {
                let changed = self.machine_connected != Some(*connected);
                self.machine_connected = Some(*connected);
                if changed && !replay {
                    self.streaming_assistant = false;
                    self.ensure_line_start(&mut out);
                    let line = if *connected {
                        self.style.dim("─ machine connected")
                    } else {
                        self.style
                            .yellow("⚠ machine disconnected — waiting for it back")
                    };
                    let _ = writeln!(out, "{line}");
                    self.at_line_start = true;
                }
            }
            ClientEvent::Usage { usage } => {
                self.latest_usage = Some(*usage);
            }
            ClientEvent::ProvisioningStage { stage, .. } => {
                if !replay {
                    self.streaming_assistant = false;
                    self.ensure_line_start(&mut out);
                    let label = match stage {
                        ProvisioningStage::Reserving => "reserving",
                        ProvisioningStage::Booting => "booting",
                        ProvisioningStage::Cloning => "cloning",
                        ProvisioningStage::Ready => "ready",
                    };
                    let _ = writeln!(
                        out,
                        "{}",
                        self.style.dim(&format!("─ provisioning: {label}"))
                    );
                    self.at_line_start = true;
                }
            }
            ClientEvent::SpotNotice { seconds_remaining } => {
                self.streaming_assistant = false;
                self.ensure_line_start(&mut out);
                let _ = writeln!(
                    out,
                    "{}",
                    self.style
                        .yellow(&format!("⚠ spot reclamation in {seconds_remaining}s"))
                );
                self.at_line_start = true;
            }
            ClientEvent::RepoDirty { dir, summary } => {
                self.streaming_assistant = false;
                self.ensure_line_start(&mut out);
                let target = dir
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" ({d})"));
                let _ = writeln!(
                    out,
                    "{}",
                    self.style.yellow(&format!(
                        "⚠ uncommitted changes{target}: {}",
                        first_line(summary)
                    ))
                );
                self.at_line_start = true;
            }
            ClientEvent::RepoAdded { slug, branch, dir } => {
                self.streaming_assistant = false;
                self.ensure_line_start(&mut out);
                let _ = writeln!(
                    out,
                    "{}",
                    self.style
                        .dim(&format!("─ repo added: {slug} [{branch}] -> {dir}"))
                );
                self.at_line_start = true;
            }
            ClientEvent::MachineChanged {
                machine_type,
                hourly,
                spot,
                restarted,
            } => {
                self.streaming_assistant = false;
                self.ensure_line_start(&mut out);
                let mut parts = vec![format!("machine: {machine_type}")];
                if *spot {
                    parts.push("spot".to_owned());
                }
                if let Some(rate) = hourly {
                    parts.push(format!("{rate}/h"));
                }
                if *restarted {
                    parts.push("restarted".to_owned());
                }
                let _ = writeln!(
                    out,
                    "{}",
                    self.style.dim(&format!("─ {}", parts.join("  ")))
                );
                self.at_line_start = true;
            }
            ClientEvent::ModelChanged { model } => {
                self.model = model.clone();
                self.streaming_assistant = false;
                self.ensure_line_start(&mut out);
                let _ = writeln!(
                    out,
                    "{}",
                    self.style.dim(&format!("─ model: {}", format_model(model)))
                );
                self.at_line_start = true;
            }
            ClientEvent::PermissionModeChanged { mode } => {
                self.permission_mode = *mode;
                self.streaming_assistant = false;
                self.ensure_line_start(&mut out);
                let _ = writeln!(
                    out,
                    "{}",
                    self.style.dim(&format!(
                        "─ permission mode: {}",
                        permission_mode_label(*mode)
                    ))
                );
                self.at_line_start = true;
            }
            ClientEvent::Commands { commands } => {
                self.commands.clone_from(commands);
            }
            ClientEvent::DesktopState { status, detail } => {
                if !replay {
                    self.streaming_assistant = false;
                    self.ensure_line_start(&mut out);
                    let status_text = match status {
                        DesktopStatus::Starting => "starting",
                        DesktopStatus::Ready => "ready",
                        DesktopStatus::Active => "active",
                        DesktopStatus::Failed => "failed",
                    };
                    let extra = detail
                        .as_deref()
                        .map_or(String::new(), |d| format!(": {d}"));
                    let _ = writeln!(
                        out,
                        "{}",
                        self.style.dim(&format!("─ desktop: {status_text}{extra}"))
                    );
                    self.at_line_start = true;
                }
            }
            ClientEvent::DesktopActive => {
                if !replay {
                    self.streaming_assistant = false;
                    self.ensure_line_start(&mut out);
                    let _ = writeln!(out, "{}", self.style.dim("─ desktop active"));
                    self.at_line_start = true;
                }
            }
            ClientEvent::DesktopTakeover { active } => {
                self.streaming_assistant = false;
                self.ensure_line_start(&mut out);
                let label = if *active {
                    "─ desktop takeover active"
                } else {
                    "─ desktop takeover released"
                };
                let _ = writeln!(out, "{}", self.style.dim(label));
                self.at_line_start = true;
            }
            ClientEvent::ComputerUseChanged { enabled } => {
                self.streaming_assistant = false;
                self.ensure_line_start(&mut out);
                let label = if *enabled {
                    "─ computer use enabled"
                } else {
                    "─ computer use disabled"
                };
                let _ = writeln!(out, "{}", self.style.dim(label));
                self.at_line_start = true;
            }
            // Metadata-only or independent shell-pane PTY frames are never
            // mixed into the ACP conversation transcript.
            ClientEvent::Started { .. }
            | ClientEvent::Capabilities { .. }
            | ClientEvent::Models { .. }
            | ClientEvent::TerminalOutput { .. }
            | ClientEvent::TerminalExited { .. } => {}
        }
        out
    }

    /// Renders an unrecognized raw JSON event from a newer control plane
    /// (including read-only `swarm_*` events introduced in parallel).
    #[must_use]
    pub fn apply_raw_event(&mut self, value: &serde_json::Value) -> String {
        let Some(kind) = value.get("type").and_then(serde_json::Value::as_str) else {
            return String::new();
        };
        if let Some(swarm_kind) = kind.strip_prefix("swarm_") {
            let mut out = String::new();
            self.streaming_assistant = false;
            self.ensure_line_start(&mut out);
            let detail = summarize_swarm_payload(swarm_kind, value);
            let _ = writeln!(out, "{}", self.style.cyan(&format!("[swarm] {detail}")));
            self.at_line_start = true;
            return out;
        }
        String::new()
    }

    #[expect(
        clippy::too_many_lines,
        reason = "exhaustive rendering of every HarnessEvent variant in one place"
    )]
    fn render_harness_event(&mut self, event: &HarnessEvent, out: &mut String) {
        match event {
            HarnessEvent::TurnStarted { .. } => {
                self.turn_active = true;
                self.streaming_assistant = false;
                self.ensure_line_start(out);
                let _ = writeln!(out, "{}", self.style.dim("● working…"));
                self.at_line_start = true;
            }
            HarnessEvent::AssistantDelta { text, .. } => {
                if !text.is_empty() {
                    self.streaming_assistant = true;
                    out.push_str(text);
                    self.at_line_start = text.ends_with('\n');
                }
            }
            HarnessEvent::ToolStarted {
                call_id,
                tool,
                input,
                ..
            } => {
                self.streaming_assistant = false;
                self.active_tools.insert(call_id.clone(), tool.clone());
                self.ensure_line_start(out);
                let summary = summarize_tool_input(input);
                if summary.is_empty() {
                    let _ = writeln!(out, "{}", self.style.cyan(&format!("▸ {tool}")));
                } else {
                    let _ = writeln!(
                        out,
                        "{} {}",
                        self.style.cyan(&format!("▸ {tool}")),
                        self.style.dim(&summary)
                    );
                }
                if let Some(diff) = render_inline_diff(input, self.style) {
                    out.push_str(&diff);
                }
                self.at_line_start = true;
            }
            HarnessEvent::ToolCompleted { call_id, ok, .. } => {
                self.streaming_assistant = false;
                let tool = self
                    .active_tools
                    .remove(call_id)
                    .unwrap_or_else(|| "tool".to_owned());
                self.ensure_line_start(out);
                if *ok {
                    let _ = writeln!(out, "{}", self.style.dim(&format!("✓ {tool}")));
                } else {
                    let _ = writeln!(out, "{}", self.style.red(&format!("✗ {tool} (failed)")));
                }
                self.at_line_start = true;
            }
            HarnessEvent::TurnCompleted { usage, .. } => {
                self.turn_active = false;
                self.streaming_assistant = false;
                self.active_tools.clear();
                self.latest_usage = Some(*usage);
                self.ensure_line_start(out);
                let _ = writeln!(
                    out,
                    "{}",
                    self.style
                        .dim(&format!("─ turn complete  {}", format_usage(*usage)))
                );
                self.at_line_start = true;
            }
            HarnessEvent::TurnFailed { error, .. } => {
                self.turn_active = false;
                self.streaming_assistant = false;
                self.active_tools.clear();
                self.ensure_line_start(out);
                let _ = writeln!(
                    out,
                    "{}",
                    self.style.red(&format!("✗ turn failed: {error}"))
                );
                self.at_line_start = true;
            }
            HarnessEvent::UsageLimited { window } => {
                self.streaming_assistant = false;
                self.ensure_line_start(out);
                let _ = writeln!(out, "{}", self.style.yellow(&format_usage_limit(window)));
                self.at_line_start = true;
            }
            HarnessEvent::ContextCompacted => {
                self.streaming_assistant = false;
                self.ensure_line_start(out);
                let _ = writeln!(out, "{}", self.style.dim("─ context compacted"));
                self.at_line_start = true;
            }
            HarnessEvent::ContextCompactionFailed { error } => {
                self.streaming_assistant = false;
                self.ensure_line_start(out);
                let _ = writeln!(
                    out,
                    "{}",
                    self.style
                        .red(&format!("✗ context compaction failed: {error}"))
                );
                self.at_line_start = true;
            }
            HarnessEvent::LocalCommandOutput { content } => {
                self.streaming_assistant = false;
                self.ensure_line_start(out);
                out.push_str(content);
                if !content.ends_with('\n') {
                    out.push('\n');
                }
                self.at_line_start = true;
            }
            HarnessEvent::ContextUsage { usage } => {
                self.streaming_assistant = false;
                self.ensure_line_start(out);
                out.push_str(&format_context_usage(usage, self.style));
                self.at_line_start = true;
            }
        }
    }

    fn format_approval_card(&self, pending: &PendingApproval) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{}",
            self.style
                .bold_yellow(&format!("⚠ approval required ({})", pending.id))
        );
        match &pending.payload {
            ApprovalPayload::ToolUse { tool, input } => {
                let summary = summarize_tool_input(input);
                if summary.is_empty() {
                    let _ = writeln!(out, "  tool: {tool}");
                } else {
                    let _ = writeln!(out, "  tool: {tool} — {summary}");
                }
                if let Some(diff) = render_inline_diff(input, self.style) {
                    out.push_str(&diff);
                }
            }
            ApprovalPayload::Merge {
                repo,
                from_branch,
                into_branch,
            } => {
                let _ = writeln!(out, "  merge {repo}: {from_branch} -> {into_branch}");
            }
            ApprovalPayload::HistoryRewrite {
                repo,
                branch,
                description,
            } => {
                let _ = writeln!(out, "  history rewrite {repo} [{branch}]: {description}");
            }
            ApprovalPayload::AgentsMdChange { find, replace } => {
                let _ = writeln!(out, "  AGENTS.md update:");
                for line in find.lines().take(MAX_DIFF_LINES / 2) {
                    let _ = writeln!(out, "    {}", self.style.red(&format!("- {line}")));
                }
                for line in replace.lines().take(MAX_DIFF_LINES / 2) {
                    let _ = writeln!(out, "    {}", self.style.green(&format!("+ {line}")));
                }
            }
            ApprovalPayload::MachineResizeLicenseBound {
                machine_type,
                minimum,
                reason,
            } => {
                let _ = writeln!(
                    out,
                    "  resize to {machine_type} (minimum {}h / {}) — {reason}",
                    minimum.hours, minimum.charge
                );
            }
            ApprovalPayload::RepoAdd {
                repo,
                branch,
                reason,
            } => {
                let branch_part = branch
                    .as_deref()
                    .map_or(String::new(), |b| format!(" [{b}]"));
                let _ = writeln!(out, "  add repo {repo}{branch_part} — {reason}");
            }
        }
        let _ = writeln!(
            out,
            "  {}",
            self.style.dim("enter 'a'/'y' to allow or 'd'/'n' to deny")
        );
        out
    }

    fn ensure_line_start(&mut self, out: &mut String) {
        if !self.at_line_start {
            out.push('\n');
            self.at_line_start = true;
        }
    }
}

const fn harness_name(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::ClaudeCode => "Claude Code",
        HarnessKind::Codex => "Codex",
        HarnessKind::Devin => "Devin",
        HarnessKind::Antigravity => "Antigravity",
    }
}

const fn permission_mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::BypassPermissions => "bypass-permissions",
        PermissionMode::Plan => "plan",
        PermissionMode::DontAsk => "dont-ask",
        PermissionMode::Auto => "auto",
    }
}

fn state_label(
    state: SessionState,
    paused_reason: Option<PausedReason>,
    interrupted_reason: Option<InterruptedReason>,
) -> String {
    let base = match state {
        SessionState::Provisioning => "provisioning",
        SessionState::Active => "active",
        SessionState::Paused => "paused",
        SessionState::Interrupted => "interrupted",
        SessionState::Archived => "archived",
        SessionState::Failed => "failed",
    };
    if state == SessionState::Paused
        && let Some(reason) = paused_reason
    {
        return match reason {
            PausedReason::Budget => format!("{base} (budget)"),
            PausedReason::UsageLimit => format!("{base} (plan window)"),
        };
    }
    if state == SessionState::Interrupted
        && let Some(reason) = interrupted_reason
    {
        let why = match reason {
            InterruptedReason::SpotReclaimed => "spot reclaimed",
            InterruptedReason::Suspended => "suspended",
            InterruptedReason::MachineLost => "machine lost",
        };
        return format!("{base} ({why})");
    }
    base.to_owned()
}

fn format_model(model: &ModelChoice) -> String {
    model.effort.as_deref().map_or_else(
        || model.model.clone(),
        |effort| format!("{} ({effort})", model.model),
    )
}

fn format_usage(usage: UsageReport) -> String {
    let mut parts = vec![
        format!("in {}", format_tokens(usage.input_tokens)),
        format!("out {}", format_tokens(usage.output_tokens)),
    ];
    if let Some(ctx) = usage.context
        && ctx.size_tokens > 0
    {
        let pct = ctx.fill_basis_points() / 100;
        parts.push(format!("ctx {pct}%"));
    }
    if let Some(cost) = usage.estimated_cost {
        parts.push(cost.to_string());
    }
    parts.join("  ")
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}.{}m", tokens / 1_000_000, (tokens % 1_000_000) / 100_000)
    } else if tokens >= 1_000 {
        format!("{}.{}k", tokens / 1_000, (tokens % 1_000) / 100)
    } else {
        tokens.to_string()
    }
}

fn format_usage_limit(window: &UsageWindow) -> String {
    let base = format!(
        "⚠ usage limit reached ({} at {}%)",
        window.label, window.used_percent
    );
    let Some(resets_at) = window.resets_at_unix else {
        return base;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0);
    let remaining = resets_at.saturating_sub(now);
    if remaining <= 0 {
        return format!("{base} — resetting now");
    }
    let mins = remaining / 60;
    if mins >= 60 {
        format!("{base} — resets in {}h {}m", mins / 60, mins % 60)
    } else if mins > 0 {
        format!("{base} — resets in {mins}m")
    } else {
        format!("{base} — resets in {remaining}s")
    }
}

fn format_context_usage(usage: &ContextUsage, style: Style) -> String {
    let mut out = String::new();
    let model = usage.model.as_deref().unwrap_or("unknown");
    let window = usage.window.map_or_else(
        || "unavailable".to_owned(),
        |w| {
            let pct = if w.size_tokens > 0 {
                w.fill_basis_points() / 100
            } else {
                0
            };
            format!(
                "{} / {} ({pct}%)",
                format_tokens(w.used_tokens),
                format_tokens(w.size_tokens)
            )
        },
    );
    let _ = writeln!(out, "{}  {model}  {window}", style.bold("Context"));
    for (heading, items) in [
        ("Categories", &usage.categories),
        ("MCP tools", &usage.mcp_tools),
        ("Memory", &usage.memory_files),
        ("Agents", &usage.agents),
        ("Skills", &usage.skills),
    ] {
        append_cost_section(&mut out, heading, items, style);
    }
    out
}

fn append_cost_section(out: &mut String, heading: &str, items: &[ContextCost], style: Style) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "  {}", style.dim(heading));
    for item in items {
        let deferred = if item.deferred { " (deferred)" } else { "" };
        let _ = writeln!(
            out,
            "    {:24} {}{deferred}",
            item.name,
            format_tokens(item.tokens)
        );
    }
}

fn summarize_tool_input(input: &serde_json::Value) -> String {
    if let Some(obj) = input.as_object() {
        for key in [
            "command",
            "cmd",
            "file_path",
            "path",
            "pattern",
            "query",
            "url",
        ] {
            if let Some(val) = obj.get(key).and_then(serde_json::Value::as_str) {
                return truncate_chars(first_line(val), MAX_TOOL_SUMMARY_CHARS);
            }
        }
        if obj.is_empty() {
            return String::new();
        }
    }
    if input.is_null() {
        return String::new();
    }
    truncate_chars(&input.to_string(), MAX_TOOL_SUMMARY_CHARS)
}

fn render_inline_diff(input: &serde_json::Value, style: Style) -> Option<String> {
    let obj = input.as_object()?;
    if let (Some(old), Some(new)) = (
        obj.get("old_string").and_then(serde_json::Value::as_str),
        obj.get("new_string").and_then(serde_json::Value::as_str),
    ) {
        let mut out = String::new();
        let half = MAX_DIFF_LINES / 2;
        for line in old.lines().take(half) {
            let _ = writeln!(out, "  {}", style.red(&format!("- {line}")));
        }
        for line in new.lines().take(half) {
            let _ = writeln!(out, "  {}", style.green(&format!("+ {line}")));
        }
        return (!out.is_empty()).then_some(out);
    }
    if let Some(patch) = obj
        .get("diff")
        .or_else(|| obj.get("patch"))
        .and_then(serde_json::Value::as_str)
    {
        let mut out = String::new();
        for line in patch.lines().take(MAX_DIFF_LINES) {
            let styled = if line.starts_with('+') && !line.starts_with("+++") {
                style.green(line)
            } else if line.starts_with('-') && !line.starts_with("---") {
                style.red(line)
            } else {
                style.dim(line)
            };
            let _ = writeln!(out, "  {styled}");
        }
        return (!out.is_empty()).then_some(out);
    }
    None
}

fn summarize_swarm_payload(swarm_kind: &str, value: &serde_json::Value) -> String {
    let mut parts = vec![swarm_kind.replace('_', " ")];
    if let Some(obj) = value.as_object() {
        for key in [
            "member", "agent", "name", "status", "state", "title", "message", "summary",
        ] {
            if let Some(text) = obj.get(key).and_then(serde_json::Value::as_str)
                && !text.is_empty()
            {
                parts.push(truncate_chars(first_line(text), 80));
            }
        }
    }
    parts.join(" — ")
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let end = text
        .char_indices()
        .nth(max_chars.saturating_sub(1))
        .map_or(text.len(), |(idx, _)| idx);
    format!("{}\u{2026}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use flyco_core::wire::StoredEvent;
    use flyco_core::{ContextWindow, MachineOrigin, SessionRepo, SessionSummary, Usd};

    fn sample_detail(harness: HarnessKind, state: SessionState) -> SessionDetail {
        SessionDetail {
            summary: SessionSummary {
                id: SessionId::generate(),
                title: "Implement ACP CLI".to_owned(),
                machine_origin: MachineOrigin::Auto,
                harness,
                repos: vec![SessionRepo {
                    slug: "lexoliu/flyco".parse().expect("slug"),
                    branch: Some("main".parse().expect("branch")),
                    dir: "flyco".to_owned(),
                    added_by: flyco_core::RepoAddedBy::User,
                }],
                state,
                activity: SessionActivity::Idle,
                interrupted_reason: None,
                paused_reason: None,
                created_at_unix: 0,
                last_active_unix: 0,
                model: ModelChoice {
                    model: "sonnet".to_owned(),
                    effort: Some("high".to_owned()),
                },
                permission_mode: PermissionMode::Auto,
                awake_until_unix: None,
                computer_use: false,
            },
            budget: BudgetView {
                limit: Usd::from_dollars(10),
                spent: Usd::from_cents(125),
                remaining: Usd::from_cents(875),
                stage: BudgetStage::Ok,
            },
            failure: None,
            usage_limit: None,
        }
    }

    #[test]
    fn renders_assistant_stream_tools_diffs_and_turn_end() {
        let mut renderer = Renderer::new(
            sample_detail(HarnessKind::Antigravity, SessionState::Active),
            false,
        );
        let header = renderer.render_header();
        assert!(header.contains("Antigravity"));
        assert!(header.contains("lexoliu/flyco [main]"));

        assert!(
            renderer
                .apply_event(
                    &ClientEvent::Harness {
                        event: HarnessEvent::TurnStarted {
                            turn_id: "t1".to_owned(),
                        },
                    },
                    false
                )
                .contains("working")
        );
        assert!(renderer.turn_active());

        let d1 = renderer.apply_event(
            &ClientEvent::Harness {
                event: HarnessEvent::AssistantDelta {
                    turn_id: "t1".to_owned(),
                    text: "Hello ".to_owned(),
                },
            },
            false,
        );
        let d2 = renderer.apply_event(
            &ClientEvent::Harness {
                event: HarnessEvent::AssistantDelta {
                    turn_id: "t1".to_owned(),
                    text: "world".to_owned(),
                },
            },
            false,
        );
        assert_eq!(format!("{d1}{d2}"), "Hello world");
        assert!(renderer.is_mid_stream());

        let tool_start = renderer.apply_event(
            &ClientEvent::Harness {
                event: HarnessEvent::ToolStarted {
                    turn_id: "t1".to_owned(),
                    call_id: "c1".to_owned(),
                    tool: "Edit".to_owned(),
                    input: serde_json::json!({
                        "file_path": "src/lib.rs",
                        "old_string": "old()",
                        "new_string": "new()"
                    }),
                },
            },
            false,
        );
        assert!(tool_start.starts_with('\n'));
        assert!(tool_start.contains("▸ Edit src/lib.rs"));
        assert!(tool_start.contains("- old()"));
        assert!(tool_start.contains("+ new()"));

        let tool_done = renderer.apply_event(
            &ClientEvent::Harness {
                event: HarnessEvent::ToolCompleted {
                    turn_id: "t1".to_owned(),
                    call_id: "c1".to_owned(),
                    ok: true,
                },
            },
            false,
        );
        assert!(tool_done.contains("✓ Edit"));

        let turn_done = renderer.apply_event(
            &ClientEvent::Harness {
                event: HarnessEvent::TurnCompleted {
                    turn_id: "t1".to_owned(),
                    usage: UsageReport {
                        input_tokens: 1_500,
                        output_tokens: 420,
                        context: Some(ContextWindow {
                            used_tokens: 50_000,
                            size_tokens: 200_000,
                        }),
                        estimated_cost: Some(Usd::from_cents(12)),
                    },
                },
            },
            false,
        );
        assert!(!renderer.turn_active());
        assert!(turn_done.contains("turn complete"));
        assert!(turn_done.contains("in 1.5k"));
        assert!(turn_done.contains("ctx 25%"));
        assert!(turn_done.contains("$0.12"));
    }

    #[test]
    fn renders_pause_usage_limit_and_ignores_raw_pty_events() {
        let mut detail = sample_detail(HarnessKind::ClaudeCode, SessionState::Paused);
        detail.summary.paused_reason = Some(PausedReason::Budget);
        let mut renderer = Renderer::new(detail, false);
        let header = renderer.render_header();
        assert!(header.contains("paused: compute budget exhausted"));

        let limit = renderer.apply_event(
            &ClientEvent::Harness {
                event: HarnessEvent::UsageLimited {
                    window: UsageWindow::new(Some(300), None, 100, None),
                },
            },
            false,
        );
        assert!(limit.contains("usage limit reached (5-hour at 100%)"));

        // Independent shell-pane PTY events must never pollute the ACP view.
        assert_eq!(
            renderer.apply_event(
                &ClientEvent::TerminalOutput {
                    data: "\x1b[2Jraw pty".to_owned(),
                },
                false
            ),
            ""
        );
        assert_eq!(
            renderer.apply_event(&ClientEvent::TerminalExited { code: Some(0) }, false),
            ""
        );
    }

    #[test]
    fn renders_unknown_swarm_replayed_events_cleanly() {
        let mut renderer = Renderer::new(
            sample_detail(HarnessKind::Codex, SessionState::Active),
            false,
        );
        let item = Item::Replayed(StoredEvent {
            seq: 9,
            at_unix: 0,
            event: serde_json::json!({
                "type": "swarm_member_updated",
                "member": "verifier",
                "status": "working"
            }),
        });
        let text = renderer.apply_item(&item, false);
        assert!(text.contains("[swarm]"));
        assert!(text.contains("verifier"));
        assert!(text.contains("working"));
    }

    #[test]
    fn replay_hides_already_decided_approvals_and_keeps_pending_ones() {
        let mut renderer = Renderer::new(
            sample_detail(HarnessKind::Devin, SessionState::Active),
            false,
        );
        let decided_id = ApprovalId::generate();
        let open_id = ApprovalId::generate();

        let _ = renderer.apply_event(
            &ClientEvent::ApprovalPending {
                id: decided_id,
                payload: ApprovalPayload::ToolUse {
                    tool: "Bash".to_owned(),
                    input: serde_json::json!({"command": "ls"}),
                },
            },
            true,
        );
        let _ = renderer.apply_event(
            &ClientEvent::ApprovalDecided {
                id: decided_id,
                decision: ApprovalDecision::Approved,
            },
            true,
        );
        let _ = renderer.apply_event(
            &ClientEvent::ApprovalPending {
                id: open_id,
                payload: ApprovalPayload::ToolUse {
                    tool: "Bash".to_owned(),
                    input: serde_json::json!({"command": "cargo test"}),
                },
            },
            true,
        );

        let cards = renderer.render_pending_approvals(Vec::new());
        assert!(!cards.contains(&decided_id.to_string()));
        assert!(cards.contains(&open_id.to_string()));
        assert!(cards.contains("cargo test"));
        assert_eq!(renderer.front_approval().map(|a| a.id), Some(open_id));
    }
}
