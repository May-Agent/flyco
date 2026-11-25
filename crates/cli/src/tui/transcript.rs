//! Structured conversation state and event reducer for the `flyco` TUI.
//!
//! Translates [`SessionDetail`], [`ClientEvent`]s, [`HarnessEvent`]s, pending
//! [`ApprovalView`]s, and read-only [`SwarmSnapshot`] updates into typed visual
//! entries consumed by [`super::render`].

use std::collections::{HashMap, HashSet, VecDeque};

use flyco_core::wire::{
    ApprovalPayload, ClientEvent, DesktopStatus, HarnessCommand, MessageOrigin, ProvisioningStage,
    ShellOutcome, ShellStream,
};
use flyco_core::{
    ApprovalId, ApprovalState, ApprovalView, BudgetView, ContextUsage, HarnessEvent, HarnessKind,
    InterruptedReason, ModelChoice, PausedReason, PermissionMode, SessionActivity, SessionDetail,
    SessionId, SessionRepo, SessionState, ShellRunId, SwarmSnapshot, UsageLimitPause, UsageReport,
};

use crate::follow::Item;

/// Maximum inline characters shown for a generic tool input summary.
const MAX_TOOL_SUMMARY_CHARS: usize = 140;

/// Maximum inline diff lines shown for an edit tool call.
const MAX_DIFF_LINES: usize = 12;

/// One pending approval waiting for user action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// Approval id passed to `POST /v1/approvals/{id}/decision`.
    pub id: ApprovalId,
    /// Structured approval request payload.
    pub payload: ApprovalPayload,
}

/// One rendered entry in the session timeline.
#[derive(Debug, Clone)]
pub enum Entry {
    /// A user prompt.
    User {
        /// Prompt text.
        text: String,
    },
    /// Completed assistant prose block.
    Assistant {
        /// Markdown/text body.
        text: String,
    },
    /// A harness tool invocation with optional result and diff lines.
    Tool {
        /// Tool call id for pairing with its result.
        call_id: String,
        /// Tool name (e.g. `Bash`, `Edit`, `Read`).
        name: String,
        /// Concise argument summary.
        summary: String,
        /// Extra detail lines (e.g. unified diff snippets).
        detail_lines: Vec<String>,
        /// Completion status: `None` while running, `Some(true)` ok, `Some(false)` error.
        ok: Option<bool>,
        /// Optional result excerpt on error or notable output.
        result_excerpt: Option<String>,
    },
    /// A `!cmd` shell execution on the session machine.
    Shell {
        /// Shell run identifier.
        id: ShellRunId,
        /// Command string.
        command: String,
        /// Combined stdout/stderr output buffer.
        output: String,
        /// Terminal outcome once finished.
        outcome: Option<ShellOutcome>,
    },
    /// Read-only swarm board message or snapshot milestone.
    Swarm {
        /// Sender or summary header.
        header: String,
        /// Body text.
        body: String,
    },
    /// Provisioning stage or system notice.
    Notice {
        /// Notice text.
        text: String,
    },
    /// Warning message (e.g. budget warning, pause notice).
    Warning {
        /// Warning text.
        text: String,
    },
    /// Error message.
    Error {
        /// Error text.
        text: String,
    },
}

/// Live state for one attached cloud session inside the TUI.
#[derive(Debug)]
pub struct SessionTranscript {
    /// Session identifier.
    pub id: SessionId,
    /// Session title.
    pub title: String,
    /// Driving harness.
    pub harness: HarnessKind,
    /// Attached repositories.
    pub repos: Vec<SessionRepo>,
    /// Lifecycle state.
    pub state: SessionState,
    /// Fine-grained activity state.
    pub activity: SessionActivity,
    /// Pause reason when [`Self::state`] is `Paused`.
    pub paused_reason: Option<PausedReason>,
    /// Interrupt reason when [`Self::state`] is `Interrupted`.
    pub interrupted_reason: Option<InterruptedReason>,
    /// Usage limit pause metadata if rate-limited.
    pub usage_limit: Option<UsageLimitPause>,
    /// Budget telemetry.
    pub budget: BudgetView,
    /// Failure description when `Failed`.
    pub failure: Option<String>,
    /// Active model and effort.
    pub model: ModelChoice,
    /// Active permission mode.
    pub permission_mode: PermissionMode,
    /// Whether the daemon relay is currently connected.
    pub machine_connected: Option<bool>,
    /// Whether a turn is actively streaming.
    pub turn_active: bool,
    /// Currently running `!cmd` shell if any.
    pub active_shell: Option<ShellRunId>,
    /// Latest reported token/cost usage.
    pub latest_usage: Option<UsageReport>,
    /// Latest context window usage snapshot.
    pub latest_context: Option<ContextUsage>,
    /// Latest read-only swarm snapshot.
    pub latest_swarm: Option<SwarmSnapshot>,
    /// Count of unique swarm board messages observed.
    pub swarm_message_count: usize,
    /// Highest swarm board sequence already appended to [`Self::entries`].
    last_swarm_seq: u64,
    /// Harness-advertised slash commands.
    pub commands: Vec<HarnessCommand>,
    /// Queue of unresolved approvals.
    pub pending_approvals: VecDeque<PendingApproval>,
    /// Set of approvals already decided (deduplicates replayed events).
    decided_approvals: HashSet<ApprovalId>,
    /// Timeline entries in chronological order.
    pub entries: Vec<Entry>,
    /// In-progress streaming assistant prose not yet flushed into [`Self::entries`].
    pub streaming_text: String,
    /// Index of tool call id -> position in [`Self::entries`].
    tool_index: HashMap<String, usize>,
    /// Index of shell run id -> position in [`Self::entries`].
    shell_index: HashMap<ShellRunId, usize>,
    /// Latest provisioning stage shown.
    pub provisioning_stage: Option<ProvisioningStage>,
    /// Latest virtual desktop status.
    pub desktop_status: Option<DesktopStatus>,
}

impl SessionTranscript {
    /// Initializes timeline state from a [`SessionDetail`] snapshot.
    #[must_use]
    pub fn new(detail: SessionDetail) -> Self {
        let s = detail.summary;
        Self {
            id: s.id,
            title: s.title,
            harness: s.harness,
            repos: s.repos,
            state: s.state,
            activity: s.activity,
            paused_reason: s.paused_reason,
            interrupted_reason: s.interrupted_reason,
            usage_limit: detail.usage_limit,
            budget: detail.budget,
            failure: detail.failure,
            model: s.model,
            permission_mode: s.permission_mode,
            machine_connected: None,
            turn_active: false,
            active_shell: None,
            latest_usage: None,
            latest_context: None,
            latest_swarm: None,
            swarm_message_count: 0,
            last_swarm_seq: 0,
            commands: Vec::new(),
            pending_approvals: VecDeque::new(),
            decided_approvals: HashSet::new(),
            entries: Vec::new(),
            streaming_text: String::new(),
            tool_index: HashMap::new(),
            shell_index: HashMap::new(),
            provisioning_stage: None,
            desktop_status: None,
        }
    }

    /// Synchronizes metadata from a refreshed [`SessionDetail`].
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

    /// Reconciles pending approvals fetched from `GET /v1/approvals?state=pending`.
    pub fn reconcile_approvals(&mut self, approvals: Vec<ApprovalView>) {
        for view in approvals {
            if view.state != ApprovalState::Pending || self.decided_approvals.contains(&view.id) {
                continue;
            }
            if !self.pending_approvals.iter().any(|p| p.id == view.id) {
                self.pending_approvals.push_back(PendingApproval {
                    id: view.id,
                    payload: view.payload,
                });
            }
        }
    }

    /// Front pending approval awaiting user decision, if any.
    #[must_use]
    pub fn front_approval(&self) -> Option<&PendingApproval> {
        self.pending_approvals.front()
    }

    /// Appends a system notice entry.
    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.flush_streaming();
        self.entries.push(Entry::Notice { text: text.into() });
    }

    /// Appends a warning entry.
    pub fn push_warning(&mut self, text: impl Into<String>) {
        self.flush_streaming();
        self.entries.push(Entry::Warning { text: text.into() });
    }

    /// Appends an error entry.
    pub fn push_error(&mut self, text: impl Into<String>) {
        self.flush_streaming();
        self.entries.push(Entry::Error { text: text.into() });
    }

    /// Primary repository display label (`owner/repo [branch]`).
    #[must_use]
    pub fn primary_repo_label(&self) -> String {
        self.repos.first().map_or_else(
            || "(no repo)".to_owned(),
            |repo| {
                repo.branch.as_ref().map_or_else(
                    || repo.slug.to_string(),
                    |branch| format!("{} [{branch}]", repo.slug),
                )
            },
        )
    }

    /// Consumes one SSE or replayed [`Item`].
    #[expect(
        clippy::too_many_lines,
        reason = "exhaustive ClientEvent reducer keeps all state transitions in one place"
    )]
    pub fn apply_item(&mut self, item: &Item, replayed: bool) {
        let Some(event) = item.event() else {
            return;
        };
        match event {
            ClientEvent::UserMessage { text, origin } => {
                self.flush_streaming();
                if origin == MessageOrigin::User {
                    self.entries.push(Entry::User { text });
                    self.turn_active = true;
                } else {
                    self.entries.push(Entry::Notice {
                        text: format!("[flyco] {text}"),
                    });
                }
            }
            ClientEvent::Harness { event } => self.apply_harness(&event),
            ClientEvent::SessionStateChanged { state } => {
                self.state = state;
                if matches!(
                    state,
                    SessionState::Paused
                        | SessionState::Interrupted
                        | SessionState::Archived
                        | SessionState::Failed
                ) {
                    self.turn_active = false;
                    self.flush_streaming();
                }
                if !replayed {
                    self.push_notice(format!("session state -> {}", state_label(state)));
                }
            }
            ClientEvent::ProvisioningStage { stage, .. } => {
                self.provisioning_stage = Some(stage);
                if !replayed {
                    self.push_notice(format!("provisioning: {}", stage_label(stage)));
                }
            }
            ClientEvent::MachineConnection { connected } => {
                self.machine_connected = Some(connected);
                if !connected {
                    self.turn_active = false;
                    self.flush_streaming();
                }
                if !replayed {
                    if connected {
                        self.push_notice("machine connected");
                    } else {
                        self.push_warning("machine disconnected");
                    }
                }
            }
            ClientEvent::ApprovalPending { id, payload } => {
                if !self.decided_approvals.contains(&id)
                    && !self.pending_approvals.iter().any(|p| p.id == id)
                {
                    self.pending_approvals
                        .push_back(PendingApproval { id, payload });
                }
            }
            ClientEvent::ApprovalDecided { id, decision } => {
                self.decided_approvals.insert(id);
                self.pending_approvals.retain(|p| p.id != id);
                if !replayed {
                    self.push_notice(format!("approval {decision:?}").to_ascii_lowercase());
                }
            }
            ClientEvent::ShellCommand { run, command } => {
                self.flush_streaming();
                self.active_shell = Some(run);
                let idx = self.entries.len();
                self.entries.push(Entry::Shell {
                    id: run,
                    command,
                    output: String::new(),
                    outcome: None,
                });
                self.shell_index.insert(run, idx);
            }
            ClientEvent::ShellOutput { run, stream, data } => {
                if let Some(&idx) = self.shell_index.get(&run)
                    && let Some(Entry::Shell { output, .. }) = self.entries.get_mut(idx)
                {
                    if stream == ShellStream::Stderr
                        && !output.ends_with('\n')
                        && !output.is_empty()
                    {
                        output.push('\n');
                    }
                    output.push_str(&data);
                }
            }
            ClientEvent::ShellExited { run, outcome, .. } => {
                if self.active_shell == Some(run) {
                    self.active_shell = None;
                }
                if let Some(&idx) = self.shell_index.get(&run)
                    && let Some(Entry::Shell { outcome: slot, .. }) = self.entries.get_mut(idx)
                {
                    *slot = Some(outcome);
                }
            }
            ClientEvent::Commands { commands } => {
                self.commands = commands;
            }
            ClientEvent::SwarmUpdated {
                snapshot,
                board_messages,
            } => {
                if let Some(snap) = snapshot {
                    self.latest_swarm = Some(snap);
                }
                for msg in board_messages {
                    if msg.seq > self.last_swarm_seq {
                        self.last_swarm_seq = msg.seq;
                        self.swarm_message_count += 1;
                        self.entries.push(Entry::Swarm {
                            header: format!("swarm #{} {}", msg.seq, msg.agent),
                            body: msg.text,
                        });
                    }
                }
            }
            ClientEvent::DesktopState { status, .. } => {
                self.desktop_status = Some(status);
            }
            ClientEvent::Usage { usage } => {
                self.latest_usage = Some(usage);
            }
            ClientEvent::ModelChanged { model } => {
                let label = format_model_choice(&model);
                self.model = model;
                if !replayed {
                    self.push_notice(format!("model switched -> {label}"));
                }
            }
            ClientEvent::PermissionModeChanged { mode } => {
                self.permission_mode = mode;
                if !replayed {
                    self.push_notice(format!("permission mode -> {mode:?}"));
                }
            }
            ClientEvent::MachineChanged { machine_type, .. } => {
                if !replayed {
                    self.push_notice(format!("machine changed -> {machine_type}"));
                }
            }
            ClientEvent::SpotNotice { seconds_remaining } if !replayed => {
                self.push_warning(format!(
                    "spot reclamation in {seconds_remaining}s — flushing workspace"
                ));
            }
            _ => {}
        }
    }

    fn apply_harness(&mut self, event: &HarnessEvent) {
        match event {
            HarnessEvent::TurnStarted { .. } => {
                self.turn_active = true;
            }
            HarnessEvent::AssistantDelta { text, .. } => {
                self.turn_active = true;
                self.streaming_text.push_str(text);
            }
            HarnessEvent::ToolStarted {
                call_id,
                tool,
                input,
                ..
            } => {
                self.turn_active = true;
                self.flush_streaming();
                let (summary, detail_lines) = summarize_tool_input(tool, input);
                let idx = self.entries.len();
                self.entries.push(Entry::Tool {
                    call_id: call_id.clone(),
                    name: tool.clone(),
                    summary,
                    detail_lines,
                    ok: None,
                    result_excerpt: None,
                });
                self.tool_index.insert(call_id.clone(), idx);
            }
            HarnessEvent::ToolCompleted { call_id, ok, .. } => {
                if let Some(&idx) = self.tool_index.get(call_id)
                    && let Some(Entry::Tool { ok: slot, .. }) = self.entries.get_mut(idx)
                {
                    *slot = Some(*ok);
                }
            }
            HarnessEvent::TurnCompleted { usage, .. } => {
                self.turn_active = false;
                self.latest_usage = Some(*usage);
                self.flush_streaming();
            }
            HarnessEvent::TurnFailed { error, .. } => {
                self.turn_active = false;
                self.push_error(error.clone());
            }
            HarnessEvent::UsageLimited { window } => {
                self.turn_active = false;
                self.push_warning(format!("usage limit reached ({})", window.label));
            }
            HarnessEvent::ContextCompacted => {
                self.push_notice("conversation compacted");
            }
            HarnessEvent::ContextCompactionFailed { error } => {
                self.push_warning(format!("compaction failed: {error}"));
            }
            _ => {}
        }
    }

    fn flush_streaming(&mut self) {
        if !self.streaming_text.is_empty() {
            let text = std::mem::take(&mut self.streaming_text);
            self.entries.push(Entry::Assistant { text });
        }
    }
}

/// Formats a [`ModelChoice`] concisely (`model` or `model (effort)`).
#[must_use]
pub fn format_model_choice(choice: &ModelChoice) -> String {
    match &choice.effort {
        Some(effort) if !effort.is_empty() => format!("{} ({effort})", choice.model),
        _ => choice.model.clone(),
    }
}

/// Formats a [`SessionState`] as an uppercase badge string.
#[must_use]
pub const fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Provisioning => "PROVISIONING",
        SessionState::Active => "ACTIVE",
        SessionState::Paused => "PAUSED",
        SessionState::Interrupted => "INTERRUPTED",
        SessionState::Archived => "ARCHIVED",
        SessionState::Failed => "FAILED",
    }
}

const fn stage_label(stage: ProvisioningStage) -> &'static str {
    match stage {
        ProvisioningStage::Reserving => "reserving machine",
        ProvisioningStage::Booting => "booting and installing daemon",
        ProvisioningStage::Cloning => "cloning repository",
        ProvisioningStage::Ready => "ready",
    }
}

/// Formats an approval payload into `(headline, detail)` for the approval dock.
#[must_use]
pub fn format_approval(payload: &ApprovalPayload) -> (String, String) {
    match payload {
        ApprovalPayload::ToolUse { tool, input } => {
            let (summary, _) = summarize_tool_input(tool, input);
            (format!("Tool approval: {tool}"), summary)
        }
        ApprovalPayload::Merge {
            repo,
            from_branch,
            into_branch,
        } => (
            format!("Merge in {repo}"),
            format!("{from_branch} -> {into_branch}"),
        ),
        ApprovalPayload::HistoryRewrite {
            repo,
            branch,
            description,
        } => (
            format!("History rewrite: {repo} [{branch}]"),
            description.clone(),
        ),
        ApprovalPayload::AgentsMdChange { find, replace } => (
            "Update shared AGENTS.md".to_owned(),
            format!(
                "{} -> {}",
                truncate_chars(find, 48),
                truncate_chars(replace, 48)
            ),
        ),
        ApprovalPayload::MachineResizeLicenseBound {
            machine_type,
            reason,
            ..
        } => (format!("Resize machine -> {machine_type}"), reason.clone()),
        ApprovalPayload::RepoAdd {
            repo,
            branch,
            reason,
        } => (
            branch.as_ref().map_or_else(
                || format!("Add repository: {repo}"),
                |b| format!("Add repository: {repo} [{b}]"),
            ),
            reason.clone(),
        ),
    }
}

fn summarize_tool_input(name: &str, input: &serde_json::Value) -> (String, Vec<String>) {
    let mut details = Vec::new();
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        return (truncate_chars(cmd, MAX_TOOL_SUMMARY_CHARS), details);
    }
    if let Some(path) = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())
    {
        if let (Some(old), Some(new)) = (
            input.get("old_string").and_then(|v| v.as_str()),
            input.get("new_string").and_then(|v| v.as_str()),
        ) {
            for line in old.lines().take(MAX_DIFF_LINES / 2) {
                details.push(format!("- {line}"));
            }
            for line in new.lines().take(MAX_DIFF_LINES / 2) {
                details.push(format!("+ {line}"));
            }
        }
        return (truncate_chars(path, MAX_TOOL_SUMMARY_CHARS), details);
    }
    if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
        return (truncate_chars(pattern, MAX_TOOL_SUMMARY_CHARS), details);
    }
    let compact = input.to_string();
    if compact == "{}" || compact == "null" {
        (name.to_owned(), details)
    } else {
        (truncate_chars(&compact, MAX_TOOL_SUMMARY_CHARS), details)
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    let first_line = text.lines().next().unwrap_or(text);
    let mut out = String::new();
    for (idx, ch) in first_line.chars().enumerate() {
        if idx >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}
