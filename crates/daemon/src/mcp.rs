//! `flycod`'s local MCP server: the only way an agent acts on its session.
//!
//! An agent on a flyco machine cannot configure MCP for itself — the harness
//! config is root-owned and the server allowlist is enforced on the machine
//! (docs/ARCHITECTURE.md) — so everything it is allowed to do beyond editing
//! files comes through here. This module carries the machine half of that
//! surface: what machine the session is on, what the budget has left, and
//! moving to another machine.
//!
//! # A second process, on purpose
//!
//! The harness launches this over stdio (`flycod mcp --config …`), so it is
//! a different process from the `flycod run` that supervises the harness and
//! holds the relay. It therefore shares no memory with it, and every fact it
//! reports comes from the control plane rather than from the daemon's own
//! state. That is the right shape rather than a limitation: a resize
//! restarts the machine, so the *config file* on disk describes the machine
//! the session booted on, not the one it is on now. Asking the control plane
//! is the only answer that is still true after a resize.
//!
//! What the config does carry, and what nothing else could tell this
//! process, is
//! [`machine_origin`](crate::config::DaemonConfig::machine_origin): who
//! chose the machine. It is a fact about the session and does not move.
//!
//! # What is enforced here and what is enforced by flyco
//!
//! The dirty-tree refusal is enforced here, because the working tree is on
//! *this* machine and the control plane cannot see it. The licence gate is
//! not: the tool raises an approval rather than resizing, and the control
//! plane refuses a license-bound resize on a daemon's authority anyway
//! ([`ApiError::LicenseBoundResizeNeedsApproval`]). A rule an agent could
//! talk its way past is not a rule.
//!
//! [`ApiError::LicenseBoundResizeNeedsApproval`]: https://github.com/May-Agent/flyco/blob/main/crates/api/src/error.rs

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use askama::Template as _;
use flyco_core::wire::{ApprovalPayload, DesktopButton, DesktopInputEvent};
use flyco_core::{MachineCatalogEntry, MachineOrigin, SessionMachine};
use rmcp::handler::server::common::{schema_for_empty_input, schema_for_input};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::{Peer, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;

use crate::control::rest::{AgentApi, ControlApiError};
use crate::desktop::ipc::{self, AgentReply, AgentRequest, IpcError};
use crate::git::{GitError, GitWorkdir};
use crate::mcp_broker::{RateLimitGuard, RateLimitStatus};
use crate::notice::{
    BudgetStatus, MachineLine, MachineStatus, RepoPending, ResizeAccepted, ResizeDescription,
    ResizePending, ResizeRefusedDirty,
};

/// Name of the tool that reports the machine.
pub const MACHINE_STATUS: &str = "machine_status";

/// Name of the tool that reports the compute budget.
pub const BUDGET_STATUS: &str = "budget_status";

/// Name of the tool that moves the session onto another machine.
pub const MACHINE_RESIZE: &str = "machine_resize";

/// Name of the tool that reports the desktop's `DISPLAY`, geometry, and
/// whose hands are on it.
pub const COMPUTER_STATE: &str = "computer_state";

/// Name of the tool that reads the screen.
pub const COMPUTER_SCREENSHOT: &str = "computer_screenshot";

/// Name of the tool that moves the pointer.
pub const COMPUTER_MOVE: &str = "computer_move";

/// Name of the tool that clicks a spot.
pub const COMPUTER_CLICK: &str = "computer_click";

/// Name of the tool that drags between two spots.
pub const COMPUTER_DRAG: &str = "computer_drag";

/// Name of the tool that types text.
pub const COMPUTER_TYPE: &str = "computer_type";

/// Name of the tool that presses one key, optionally under modifiers.
pub const COMPUTER_KEY: &str = "computer_key";

/// Name of the tool that scrolls at a spot.
pub const COMPUTER_SCROLL: &str = "computer_scroll";

/// Name of the tool that waits — the desktop's answer to animations and
/// loads that only time can settle.
pub const COMPUTER_WAIT: &str = "computer_wait";

/// The desktop tools, in the order they are listed.
///
/// A session's flyco server exposes them only while its `computer_use`
/// flag is on, so [`crate::mount`]'s required set learns them
/// conditionally rather than advertising tools that would refuse.
pub const COMPUTER_TOOLS: [&str; 9] = [
    COMPUTER_STATE,
    COMPUTER_SCREENSHOT,
    COMPUTER_MOVE,
    COMPUTER_CLICK,
    COMPUTER_DRAG,
    COMPUTER_TYPE,
    COMPUTER_KEY,
    COMPUTER_SCROLL,
    COMPUTER_WAIT,
];

/// The longest `computer_wait` may hold a tool call.
///
/// A wait is how the agent lets a screen settle — minutes of it is a
/// stalled turn wearing a tool call's clothes, so past this the tool
/// says so rather than sleeping.
const WAIT_CAP_SECONDS: u16 = 120;

/// Name of the tool that asks for another repository in the workspace.
pub const REPO_ADD: &str = "repo_add";

/// The working trees, as the resize tool needs to see them.
///
/// Narrower than [`WorkingSet`](crate::git::WorkingSet), which is a
/// long-lived watcher with a snapshot handle: this process asks once,
/// answers one tool call, and exits. Stated as a trait so the refusal can be
/// tested without a checkout on disk.
pub trait TreeStatus: Send + Sync {
    /// `git status --short` per checkout, as it stands right now.
    ///
    /// Each pair is the checkout's workspace directory — `None` for the
    /// developer-machine shape, where the workspace root is the checkout —
    /// and its summary; an empty summary is a clean tree.
    fn status(
        &self,
    ) -> impl Future<Output = Result<Vec<(Option<String>, String)>, GitError>> + Send;
}

/// How long a fetched machine catalog stays cached before being refreshed.
///
/// The curated catalog for one session's account and region is effectively
/// static over a session's lifetime; caching it avoids hitting the control
/// plane on every `tools/list` and `machine_resize`.
const CATALOG_TTL: Duration = Duration::from_secs(600);

/// The session's checkouts, discovered dynamically under `workdir`.
///
/// Discovering checkouts on demand rather than freezing boot-time config
/// ensures repositories cloned mid-session via `AddRepo` are included in
/// `machine_resize` dirty-tree checks.
#[derive(Debug, Clone)]
pub struct RepoTrees {
    workdir: PathBuf,
}

impl RepoTrees {
    /// Discovers checkouts under `workdir` whenever [`TreeStatus::status`] is
    /// called: either `workdir` itself (the developer-machine shape, when
    /// `workdir/.git` exists) or every direct child directory containing `.git`.
    #[must_use]
    pub const fn discover(workdir: PathBuf) -> Self {
        Self { workdir }
    }

    async fn checkouts(&self) -> Result<Vec<(Option<String>, GitWorkdir)>, GitError> {
        if crate::git::has_checkout(&self.workdir).await {
            return Ok(vec![(None, GitWorkdir::new(self.workdir.clone()))]);
        }
        let mut dirs = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.workdir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(GitError::Spawn(error)),
        };
        while let Some(entry) = entries.next_entry().await.map_err(GitError::Spawn)? {
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name == ".git" {
                continue;
            }
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if meta.is_dir() && crate::git::has_checkout(&path).await {
                dirs.push((name, path));
            }
        }
        dirs.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(dirs
            .into_iter()
            .map(|(name, path)| (Some(name), GitWorkdir::new(path)))
            .collect())
    }
}

impl TreeStatus for RepoTrees {
    async fn status(&self) -> Result<Vec<(Option<String>, String)>, GitError> {
        let checkouts = self.checkouts().await?;
        let mut statuses = Vec::with_capacity(checkouts.len());
        for (dir, checkout) in checkouts {
            statuses.push((dir, checkout.read_status().await?));
        }
        Ok(statuses)
    }
}

/// What `machine_resize` takes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResizeInput {
    /// Provider-native machine type to move to, from the list in this
    /// tool's description.
    pub machine_type: String,
    /// Why this session needs that machine.
    ///
    /// Required rather than optional, and not decoration: it is what the
    /// user reads on the approval card for a license-bound type, and what
    /// justifies resizing over a dirty tree.
    pub reason: String,
    /// Resize even though the working tree has uncommitted changes.
    ///
    /// The changes survive — they are on the disk, and the disk survives —
    /// but every running process dies with the machine.
    #[serde(default)]
    pub force: bool,
}

/// The default button a click or a drag uses.
const fn left() -> DesktopButton {
    DesktopButton::Left
}

/// What `computer_move` takes: a spot on the display, in its own
/// pixels.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct MoveInput {
    /// Display x.
    pub x: u16,
    /// Display y.
    pub y: u16,
}

/// What `computer_click` takes.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct ClickInput {
    /// Display x.
    pub x: u16,
    /// Display y.
    pub y: u16,
    /// The button to click.
    #[serde(default = "left")]
    pub button: DesktopButton,
}

/// What `computer_drag` takes.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct DragInput {
    /// Where the drag starts, display x.
    pub from_x: u16,
    /// Display y of the start.
    pub from_y: u16,
    /// Where the drag ends, display x.
    pub to_x: u16,
    /// Display y of the end.
    pub to_y: u16,
    /// The button held through it.
    #[serde(default = "left")]
    pub button: DesktopButton,
}

/// What `computer_type` takes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TypeInput {
    /// The text to type — one keystroke pair per character.
    pub text: String,
}

/// A modifier `computer_key` holds through a keystroke.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyModifier {
    /// Shift.
    Shift,
    /// Control.
    Control,
    /// Alt.
    Alt,
    /// The command or windows key.
    Meta,
}

impl KeyModifier {
    /// The DOM `key` name the display resolves it by — the same name a
    /// browser's `KeyboardEvent.key` reports for the modifier.
    const fn key(self) -> &'static str {
        match self {
            Self::Shift => "Shift",
            Self::Control => "Control",
            Self::Alt => "Alt",
            Self::Meta => "Meta",
        }
    }
}

/// What `computer_key` takes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KeyInput {
    /// The DOM `key` name of the key to press: a character or a named
    /// key — `Enter`, `Tab`, `Escape`, `Backspace`, `F5`, `ArrowDown`.
    pub key: String,
    /// Modifiers held through the keystroke — `["control", "c"]` style
    /// chords are spelled `key: "c", modifiers: ["control"]`.
    #[serde(default)]
    pub modifiers: Vec<KeyModifier>,
}

/// What `computer_scroll` takes.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct ScrollInput {
    /// Display x where the wheel lands.
    pub x: u16,
    /// Display y where the wheel lands.
    pub y: u16,
    /// Horizontal delta, DOM convention — positive scrolls right.
    #[serde(default)]
    pub delta_x: i32,
    /// Vertical delta, DOM convention — positive scrolls down.
    pub delta_y: i32,
}

/// What `computer_wait` takes.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct WaitInput {
    /// How long to wait, in seconds — at most
    /// [`WAIT_CAP_SECONDS`].
    pub seconds: u16,
}

/// What `repo_add` takes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RepoAddInput {
    /// Repository in `owner/name` form.
    pub repo: String,
    /// Branch to check out — the repository's default when absent.
    #[serde(default)]
    pub branch: Option<String>,
    /// Why this session needs the repository.
    ///
    /// Required rather than optional: it is what the user reads on the
    /// approval card, and a clone they never picked is their call.
    pub reason: String,
}

/// Why a tool call could not be answered at all.
///
/// Distinct from a *refusal*, which is an answer: a refusal is a
/// [`CallToolResult`] the agent can read and act on, and this is the control
/// plane being unreachable or a schema the agent did not honour.
#[derive(Debug, thiserror::Error)]
enum ToolFailure {
    /// The control plane could not be reached or refused the call.
    #[error(transparent)]
    ControlApi(#[from] ControlApiError),
    /// The control plane rate-limited or exhausted this session's budget (`429`).
    #[error("{}", .0.refusal_message())]
    RateLimited(RateLimitStatus),
    /// The working tree could not be read.
    #[error(transparent)]
    Git(#[from] GitError),
    /// The desktop socket answered late, wrongly, or not at all.
    ///
    /// `NoDesktop` never reaches here — a session without a screen is an
    /// expected condition the model reads as a refusal, not this
    /// process's failure to reach one.
    #[error(transparent)]
    Desktop(#[from] IpcError),
    /// The arguments did not match the tool's schema.
    #[error("`{tool}` was called with arguments it does not accept: {detail}")]
    Arguments {
        /// Tool that was called.
        tool: &'static str,
        /// What was wrong with them.
        detail: String,
    },
    /// A tool nobody defined.
    #[error("this server has no tool called `{0}`")]
    UnknownTool(String),
    /// A notice could not be rendered, which is this daemon's own bug.
    #[error("a flyco notice could not be rendered: {0}")]
    Render(#[from] askama::Error),
}

impl From<ToolFailure> for ErrorData {
    fn from(failure: ToolFailure) -> Self {
        match failure {
            // Arguments the schema does not admit, and a tool that does not
            // exist, are both the caller's mistake and are answered as such;
            // everything else is flyco failing to answer.
            argued @ (ToolFailure::Arguments { .. } | ToolFailure::UnknownTool(_)) => {
                Self::invalid_params(argued.to_string(), None)
            }
            other => Self::internal_error(other.to_string(), None),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedCatalog {
    entries: Vec<MachineCatalogEntry>,
    spot: bool,
    fetched_at: Instant,
}

/// Probes whether the desktop IPC socket at `path` currently has a live
/// listener.
pub async fn probe_desktop_socket(path: &Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

/// flyco's tools, over one session's stdio.
///
/// Generic over the control plane and the working tree so both can be
/// replaced in a test; the shipped server is
/// `FlycoTools<BrokerClient, RepoTrees>`.
pub struct FlycoTools<A, T> {
    api: A,
    tree: T,
    origin: MachineOrigin,
    /// Path where the session's desktop IPC socket lives whenever computer
    /// use is active. Probed dynamically so toggling `SetComputerUse`
    /// mid-session exposes or hides the `computer_*` tools without restarting
    /// `flycod mcp`.
    desktop: Option<PathBuf>,
    /// Last desktop availability observed, used to emit
    /// `notifications/tools/list_changed` on transitions.
    last_desktop_live: Arc<AtomicBool>,
    /// Cached machine catalog + boot-time spot flag.
    catalog_cache: AsyncMutex<Option<CachedCatalog>>,
    /// Local `429 Retry-After` and `request-budget-exhausted` cooldown guard.
    rate_limit: RateLimitGuard,
}

impl<A, T> core::fmt::Debug for FlycoTools<A, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FlycoTools")
            .field("origin", &self.origin)
            .field("desktop", &self.desktop.is_some())
            .finish_non_exhaustive()
    }
}

impl<A: AgentApi, T: TreeStatus + 'static> FlycoTools<A, T> {
    /// Builds the server for one session.
    ///
    /// `origin` comes from the daemon's broker bootstrap because nothing else
    /// on the machine knows it and it never changes: it says who decided
    /// this session would run on a machine of its own choosing.
    ///
    /// `desktop` is the path where the session's desktop IPC socket lives
    /// whenever `computer_use` is enabled; `tools/list` probes it dynamically
    /// so mid-session `SetComputerUse` toggles take effect immediately.
    pub fn new(api: A, tree: T, origin: MachineOrigin, desktop: Option<PathBuf>) -> Self {
        Self {
            api,
            tree,
            origin,
            desktop,
            last_desktop_live: Arc::new(AtomicBool::new(false)),
            catalog_cache: AsyncMutex::new(None),
            rate_limit: RateLimitGuard::new(),
        }
    }

    /// Returns the desktop socket path and shared availability flag for
    /// spawning [`spawn_desktop_watcher`].
    #[must_use]
    pub fn desktop_watcher_state(&self) -> Option<(PathBuf, Arc<AtomicBool>)> {
        self.desktop
            .as_ref()
            .map(|path| (path.clone(), Arc::clone(&self.last_desktop_live)))
    }

    /// Checks whether the desktop IPC socket is currently reachable.
    async fn desktop_available(&self) -> bool {
        let Some(path) = &self.desktop else {
            return false;
        };
        let live = probe_desktop_socket(path).await;
        self.last_desktop_live.store(live, Ordering::Release);
        live
    }

    /// Runs a control-plane call through the local `429` cooldown guard.
    ///
    /// Returns `Ok(Ok(value))` on success, `Ok(Err(refusal))` when blocked by
    /// an active or newly received `429` (`Retry-After` /
    /// `request-budget-exhausted`), and `Err(ToolFailure::ControlApi(_))` for
    /// non-429 control-plane errors.
    async fn guarded_call<R, F>(
        &self,
        call: impl FnOnce() -> F,
    ) -> Result<Result<R, CallToolResult>, ToolFailure>
    where
        F: Future<Output = Result<R, ControlApiError>>,
    {
        if let Some(blocked) = self.rate_limit.check() {
            return Ok(Err(refusal(&blocked.refusal_message())));
        }
        match call().await {
            Ok(value) => Ok(Ok(value)),
            Err(error) => self.rate_limit.record_error(&error).map_or_else(
                || Err(ToolFailure::ControlApi(error)),
                |status| Ok(Err(refusal(&status.refusal_message()))),
            ),
        }
    }

    /// Returns the cached machine catalog and spot flag, fetching them once
    /// per [`CATALOG_TTL`] window under the local `429` guard.
    async fn cached_catalog(&self) -> Result<(Vec<MachineCatalogEntry>, bool), ToolFailure> {
        let mut cache = self.catalog_cache.lock().await;
        if let Some(cached) = cache.as_ref()
            && cached.fetched_at.elapsed() < CATALOG_TTL
        {
            return Ok((cached.entries.clone(), cached.spot));
        }
        if let Some(blocked) = self.rate_limit.check() {
            return Err(ToolFailure::RateLimited(blocked));
        }
        let machine = match self.api.agent_machine().await {
            Ok(machine) => machine,
            Err(error) => {
                if let Some(status) = self.rate_limit.record_error(&error) {
                    return Err(ToolFailure::RateLimited(status));
                }
                return Err(ToolFailure::ControlApi(error));
            }
        };
        let entries = match self.api.agent_machine_catalog().await {
            Ok(entries) => entries,
            Err(error) => {
                if let Some(status) = self.rate_limit.record_error(&error) {
                    return Err(ToolFailure::RateLimited(status));
                }
                return Err(ToolFailure::ControlApi(error));
            }
        };
        let spot = machine.machine.spot;
        *cache = Some(CachedCatalog {
            entries: entries.clone(),
            spot,
            fetched_at: Instant::now(),
        });
        drop(cache);
        Ok((entries, spot))
    }

    /// The tools, described against this session's cached catalog and live
    /// desktop availability.
    async fn tools(&self) -> Result<Vec<Tool>, ToolFailure> {
        let (entries, spot) = self.cached_catalog().await?;
        let catalog: Vec<SessionMachine> = entries
            .iter()
            .map(|entry| SessionMachine::of(entry, spot))
            .collect();

        let mut tools = vec![
            tool(
                MACHINE_STATUS,
                include_str!("../templates/tool_machine_status.md"),
                schema_for_empty_input(),
            ),
            tool(
                BUDGET_STATUS,
                include_str!("../templates/tool_budget_status.md"),
                schema_for_empty_input(),
            ),
            tool(
                MACHINE_RESIZE,
                &ResizeDescription::new(&catalog, self.origin).render()?,
                input_schema::<ResizeInput>(MACHINE_RESIZE)?,
            ),
            tool(
                REPO_ADD,
                include_str!("../templates/tool_repo_add.md"),
                input_schema::<RepoAddInput>(REPO_ADD)?,
            ),
        ];
        if self.desktop_available().await {
            tools.extend([
                tool(
                    COMPUTER_STATE,
                    include_str!("../templates/tool_computer_state.md"),
                    schema_for_empty_input(),
                ),
                tool(
                    COMPUTER_SCREENSHOT,
                    include_str!("../templates/tool_computer_screenshot.md"),
                    schema_for_empty_input(),
                ),
                tool(
                    COMPUTER_MOVE,
                    include_str!("../templates/tool_computer_move.md"),
                    input_schema::<MoveInput>(COMPUTER_MOVE)?,
                ),
                tool(
                    COMPUTER_CLICK,
                    include_str!("../templates/tool_computer_click.md"),
                    input_schema::<ClickInput>(COMPUTER_CLICK)?,
                ),
                tool(
                    COMPUTER_DRAG,
                    include_str!("../templates/tool_computer_drag.md"),
                    input_schema::<DragInput>(COMPUTER_DRAG)?,
                ),
                tool(
                    COMPUTER_TYPE,
                    include_str!("../templates/tool_computer_type.md"),
                    input_schema::<TypeInput>(COMPUTER_TYPE)?,
                ),
                tool(
                    COMPUTER_KEY,
                    include_str!("../templates/tool_computer_key.md"),
                    input_schema::<KeyInput>(COMPUTER_KEY)?,
                ),
                tool(
                    COMPUTER_SCROLL,
                    include_str!("../templates/tool_computer_scroll.md"),
                    input_schema::<ScrollInput>(COMPUTER_SCROLL)?,
                ),
                tool(
                    COMPUTER_WAIT,
                    include_str!("../templates/tool_computer_wait.md"),
                    input_schema::<WaitInput>(COMPUTER_WAIT)?,
                ),
            ]);
        }
        Ok(tools)
    }

    /// Answers one tool call.
    async fn call(&self, request: CallToolRequestParams) -> Result<CallToolResult, ToolFailure> {
        match request.name.as_ref() {
            MACHINE_STATUS => self.machine_status().await,
            BUDGET_STATUS => self.budget_status().await,
            MACHINE_RESIZE => {
                self.machine_resize(arguments(MACHINE_RESIZE, request.arguments)?)
                    .await
            }
            REPO_ADD => self.repo_add(arguments(REPO_ADD, request.arguments)?).await,
            name if COMPUTER_TOOLS.contains(&name) && self.desktop.is_some() => {
                self.computer(name, request.arguments).await
            }
            other => Err(ToolFailure::UnknownTool(other.to_owned())),
        }
    }

    async fn machine_status(&self) -> Result<CallToolResult, ToolFailure> {
        let view = match self.guarded_call(|| self.api.agent_machine()).await? {
            Ok(view) => view,
            Err(refused) => return Ok(refused),
        };
        Ok(answer(&MachineStatus::of(&view).render()?))
    }

    async fn budget_status(&self) -> Result<CallToolResult, ToolFailure> {
        let budget = match self.guarded_call(|| self.api.agent_budget()).await? {
            Ok(budget) => budget,
            Err(refused) => return Ok(refused),
        };
        Ok(answer(&BudgetStatus::of(&budget).render()?))
    }

    /// Moves the session onto another machine, or explains why it did not.
    ///
    /// Three answers, in the order the checks have to happen: the type has
    /// to be one this session can become, the work has to be safe to
    /// restart on, and a type that bills a minimum on boot is the user's
    /// decision rather than the agent's.
    async fn machine_resize(&self, input: ResizeInput) -> Result<CallToolResult, ToolFailure> {
        let (catalog, _cached_spot) = match self.cached_catalog().await {
            Ok(pair) => pair,
            Err(ToolFailure::RateLimited(status)) => return Ok(refusal(&status.refusal_message())),
            Err(other) => return Err(other),
        };
        let current = match self.guarded_call(|| self.api.agent_machine()).await? {
            Ok(current) => current,
            Err(refused) => return Ok(refused),
        };
        let Some(entry) = catalog
            .iter()
            .find(|entry| entry.machine_type == input.machine_type)
        else {
            return Ok(refusal(&format!(
                "`{}` is not one of the machine types this session can move to. \
                 The types it can move to are listed in this tool's description.",
                input.machine_type
            )));
        };
        let wanted = SessionMachine::of(entry, current.machine.spot);

        // The working trees are on this machine and the control plane
        // cannot see them, so this check lives here and nowhere else.
        let dirty: Vec<(String, String)> = self
            .tree
            .status()
            .await?
            .into_iter()
            .filter(|(_, summary)| !summary.trim().is_empty())
            .map(|(dir, summary)| (dir.unwrap_or_else(|| ".".to_owned()), summary))
            .collect();
        if !dirty.is_empty() && !input.force {
            return Ok(refusal(&ResizeRefusedDirty { checkouts: dirty }.render()?));
        }

        if let Some(minimum) = wanted.minimum {
            let id = match self
                .guarded_call(|| {
                    self.api
                        .raise_approval(ApprovalPayload::MachineResizeLicenseBound {
                            machine_type: wanted.machine_type.clone(),
                            minimum,
                            reason: input.reason,
                        })
                })
                .await?
            {
                Ok(id) => id,
                Err(refused) => return Ok(refused),
            };
            tracing::info!(
                approval = %id,
                machine_type = %wanted.machine_type,
                "raised an approval for a license-bound resize"
            );
            return Ok(answer(
                &ResizePending {
                    machine_type: wanted.machine_type.clone(),
                    minimum: MachineLine::of(&wanted)
                        .minimum
                        .unwrap_or_else(|| minimum.charge.to_string()),
                    current_type: current.machine.machine_type,
                }
                .render()?,
            ));
        }

        if let Err(refused) = self
            .guarded_call(|| self.api.resize_machine(&wanted.machine_type))
            .await?
        {
            return Ok(refused);
        }
        tracing::info!(
            machine_type = %wanted.machine_type,
            reason = %input.reason,
            forced = input.force,
            "the agent resized this session's machine"
        );
        Ok(answer(
            &ResizeAccepted {
                line: MachineLine::of(&wanted),
            }
            .render()?,
        ))
    }

    /// Answers one `computer_*` call.
    ///
    /// Every tool but `computer_wait` is a round-trip to the session's
    /// desktop over its agent socket; `wait` is time, not a display
    /// request, so it answers from here.
    async fn computer(
        &self,
        name: &str,
        args: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolFailure> {
        match name {
            COMPUTER_STATE => self.computer_state().await,
            COMPUTER_SCREENSHOT => self.computer_screenshot().await,
            COMPUTER_MOVE => {
                let input: MoveInput = arguments(COMPUTER_MOVE, args)?;
                self.inject(vec![DesktopInputEvent::Move {
                    x: input.x,
                    y: input.y,
                }])
                .await
            }
            COMPUTER_CLICK => {
                let input: ClickInput = arguments(COMPUTER_CLICK, args)?;
                self.inject(click_events(input)).await
            }
            COMPUTER_DRAG => {
                let input: DragInput = arguments(COMPUTER_DRAG, args)?;
                self.inject(drag_events(input)).await
            }
            COMPUTER_TYPE => {
                let input: TypeInput = arguments(COMPUTER_TYPE, args)?;
                self.inject(type_events(&input.text)).await
            }
            COMPUTER_KEY => {
                let input: KeyInput = arguments(COMPUTER_KEY, args)?;
                self.inject(key_events(&input)).await
            }
            COMPUTER_SCROLL => {
                let input: ScrollInput = arguments(COMPUTER_SCROLL, args)?;
                self.inject(vec![DesktopInputEvent::Scroll {
                    x: input.x,
                    y: input.y,
                    delta_x: input.delta_x,
                    delta_y: input.delta_y,
                }])
                .await
            }
            COMPUTER_WAIT => {
                let input: WaitInput = arguments(COMPUTER_WAIT, args)?;
                if input.seconds > WAIT_CAP_SECONDS {
                    return Ok(refusal(&format!(
                        "a wait may be at most {WAIT_CAP_SECONDS} seconds; \
                         longer is a stalled turn, not a settled screen"
                    )));
                }
                tokio::time::sleep(Duration::from_secs(u64::from(input.seconds))).await;
                Ok(answer("the wait is over — the screen has had its time"))
            }
            other => Err(ToolFailure::UnknownTool(other.to_owned())),
        }
    }

    /// What the desktop is — `DISPLAY`, geometry, and whose hands are
    /// on it — in the sentence the model reads before driving.
    async fn computer_state(&self) -> Result<CallToolResult, ToolFailure> {
        match self.ask(AgentRequest::State).await? {
            AgentReply::State {
                takeover,
                display,
                width,
                height,
            } => Ok(answer(&format!(
                "the desktop is {width}x{height} pixels on DISPLAY={display}; {}",
                if takeover {
                    "the user is driving it — input you send now is refused until control is handed back"
                } else {
                    "you may drive it; launch GUI apps with DISPLAY set to that value"
                }
            ))),
            AgentReply::Refused { reason } => Ok(refusal(&reason)),
            _ => Err(ToolFailure::Desktop(IpcError::Reply(
                "state was answered with the wrong kind of reply".to_owned(),
            ))),
        }
    }

    /// The screen, as the model sees it: a PNG image block at display
    /// resolution.
    async fn computer_screenshot(&self) -> Result<CallToolResult, ToolFailure> {
        match self.ask(AgentRequest::Screenshot).await? {
            AgentReply::Screenshot { png_base64 } => {
                Ok(CallToolResult::success(vec![ContentBlock::image(
                    png_base64,
                    "image/png",
                )]))
            }
            AgentReply::Refused { reason } => Ok(refusal(&reason)),
            _ => Err(ToolFailure::Desktop(IpcError::Reply(
                "a screenshot was answered with the wrong kind of reply".to_owned(),
            ))),
        }
    }

    /// One input batch to the session's desktop.
    ///
    /// The wire shape is the same [`DesktopInputEvent`] the user's
    /// takeover input takes — one event language for both drivers.
    async fn inject(&self, events: Vec<DesktopInputEvent>) -> Result<CallToolResult, ToolFailure> {
        match self.ask(AgentRequest::Input { events }).await? {
            AgentReply::Done => Ok(answer("the input landed on the desktop")),
            AgentReply::Refused { reason } => Ok(refusal(&reason)),
            _ => Err(ToolFailure::Desktop(IpcError::Reply(
                "input was answered with the wrong kind of reply".to_owned(),
            ))),
        }
    }

    /// One round-trip to the session's desktop.
    ///
    /// An absent socket is a session whose flag is off — an expected
    /// condition phrased as the refusal the model reads, not a
    /// transport failure this process reports as its own.
    async fn ask(&self, request: AgentRequest) -> Result<AgentReply, ToolFailure> {
        let Some(socket) = &self.desktop else {
            return Ok(AgentReply::Refused {
                reason: "this session has no desktop".to_owned(),
            });
        };
        match ipc::ask(socket, &request).await {
            Ok(reply) => Ok(reply),
            Err(IpcError::NoDesktop) => Ok(AgentReply::Refused {
                reason: "this session's desktop is not running".to_owned(),
            }),
            Err(error) => Err(ToolFailure::Desktop(error)),
        }
    }

    /// Asks the user to add a repository to this session's workspace.
    ///
    /// The clone is never this tool's to perform: a repository the user
    /// did not pick is code they have not agreed to put on the machine, so
    /// the tool raises an approval and the control plane is what performs
    /// the `AddRepo` once — and only if — it is allowed. The agent's answer
    /// is therefore "asked", never "added": the checkout landing is
    /// announced separately, when the clone finishes.
    ///
    /// The slug and branch are parsed here rather than left to the control
    /// plane because a malformed one is an argument error the agent can fix
    /// and retry — it should not be a card the user reads, decides on, and
    /// only then learns could never have been cloned.
    async fn repo_add(&self, input: RepoAddInput) -> Result<CallToolResult, ToolFailure> {
        let slug =
            input
                .repo
                .parse::<flyco_core::RepoSlug>()
                .map_err(|_| ToolFailure::Arguments {
                    tool: REPO_ADD,
                    detail: format!("`{}` is not a repository in `owner/name` form", input.repo),
                })?;
        let branch = input
            .branch
            .map(|branch| {
                branch
                    .parse::<flyco_core::BranchName>()
                    .map_err(|_| ToolFailure::Arguments {
                        tool: REPO_ADD,
                        detail: format!("`{branch}` is not a branch name git accepts"),
                    })
            })
            .transpose()?;
        let id = match self
            .guarded_call(|| {
                self.api.raise_approval(ApprovalPayload::RepoAdd {
                    repo: slug.as_str().to_owned(),
                    branch: branch.map(|branch| branch.as_str().to_owned()),
                    reason: input.reason,
                })
            })
            .await?
        {
            Ok(id) => id,
            Err(refused) => return Ok(refused),
        };
        tracing::info!(approval = %id, repo = %slug, "raised an approval to add a repository");
        Ok(answer(
            &RepoPending {
                repo: slug.to_string(),
            }
            .render()?,
        ))
    }
}

/// Checks whether the desktop IPC socket availability changed since
/// `last_live`, updating `last_live` and sending
/// `notifications/tools/list_changed` when it toggled.
pub async fn sync_desktop_list_changed(
    desktop_socket: &Path,
    last_live: &AtomicBool,
    peer: &Peer<RoleServer>,
) -> bool {
    let live = probe_desktop_socket(desktop_socket).await;
    let previous = last_live.swap(live, Ordering::AcqRel);
    if live != previous {
        tracing::info!(
            desktop_live = live,
            "desktop availability toggled; emitting notifications/tools/list_changed"
        );
        if let Err(error) = peer.notify_tool_list_changed().await {
            tracing::debug!(%error, "could not notify peer of tool list change");
        }
        return true;
    }
    false
}

/// Watches local desktop IPC socket availability (no control-plane traffic)
/// and emits `notifications/tools/list_changed` whenever computer use toggles.
#[must_use]
pub fn spawn_desktop_watcher(
    desktop_socket: PathBuf,
    last_live: Arc<AtomicBool>,
    peer: Peer<RoleServer>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // First tick fires immediately; seed initial state without spurious
        // notification before checking for subsequent transitions.
        let initial = probe_desktop_socket(&desktop_socket).await;
        last_live.store(initial, Ordering::Release);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let _ = sync_desktop_list_changed(&desktop_socket, &last_live, &peer).await;
        }
    })
}

/// One tool definition.
///
/// The description is trimmed because it comes out of a template file, which
/// ends with a newline the way every text file does, and a tool listing is
/// not the place to argue about trailing whitespace.
fn tool(name: &'static str, description: &str, input_schema: Arc<JsonObject>) -> Tool {
    Tool::new(name, description.trim_end().to_owned(), input_schema)
}

/// A tool's input schema, built from its argument type.
fn input_schema<I: JsonSchema + 'static>(
    name: &'static str,
) -> Result<Arc<JsonObject>, ToolFailure> {
    schema_for_input::<I>().map_err(|detail| ToolFailure::Arguments { tool: name, detail })
}

/// Reads a call's arguments, or says what was wrong with them.
fn arguments<I: for<'de> Deserialize<'de>>(
    tool: &'static str,
    arguments: Option<JsonObject>,
) -> Result<I, ToolFailure> {
    serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default())).map_err(
        |error| ToolFailure::Arguments {
            tool,
            detail: error.to_string(),
        },
    )
}

/// A click as the display hears it: a motion to the spot, then the
/// button down and up where it lands.
fn click_events(input: ClickInput) -> Vec<DesktopInputEvent> {
    vec![
        DesktopInputEvent::Move {
            x: input.x,
            y: input.y,
        },
        DesktopInputEvent::Button {
            x: input.x,
            y: input.y,
            button: input.button,
            pressed: true,
        },
        DesktopInputEvent::Button {
            x: input.x,
            y: input.y,
            button: input.button,
            pressed: false,
        },
    ]
}

/// A drag: the button goes down at the start and up at the end, with
/// the motion between.
fn drag_events(input: DragInput) -> Vec<DesktopInputEvent> {
    vec![
        DesktopInputEvent::Move {
            x: input.from_x,
            y: input.from_y,
        },
        DesktopInputEvent::Button {
            x: input.from_x,
            y: input.from_y,
            button: input.button,
            pressed: true,
        },
        DesktopInputEvent::Move {
            x: input.to_x,
            y: input.to_y,
        },
        DesktopInputEvent::Button {
            x: input.to_x,
            y: input.to_y,
            button: input.button,
            pressed: false,
        },
    ]
}

/// Text as keystrokes — a press and release per character, addressed by
/// the produced `key` so the display picks the keymap level that yields
/// it, shift and all.
fn type_events(text: &str) -> Vec<DesktopInputEvent> {
    text.chars()
        .flat_map(|ch| {
            let key = ch.to_string();
            [
                DesktopInputEvent::Key {
                    code: String::new(),
                    key: key.clone(),
                    pressed: true,
                },
                DesktopInputEvent::Key {
                    code: String::new(),
                    key,
                    pressed: false,
                },
            ]
        })
        .collect()
}

/// One key under its modifiers: modifiers down first and up last, the
/// key's press and release between them — the same order a human's
/// hands produce a chord in.
fn key_events(input: &KeyInput) -> Vec<DesktopInputEvent> {
    let modifiers = input
        .modifiers
        .iter()
        .map(|modifier| modifier.key())
        .collect::<Vec<_>>();
    let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
    for key in &modifiers {
        events.push(DesktopInputEvent::Key {
            code: String::new(),
            key: (*key).to_owned(),
            pressed: true,
        });
    }
    for pressed in [true, false] {
        events.push(DesktopInputEvent::Key {
            code: String::new(),
            key: input.key.clone(),
            pressed,
        });
    }
    for key in modifiers.iter().rev() {
        events.push(DesktopInputEvent::Key {
            code: String::new(),
            key: (*key).to_owned(),
            pressed: false,
        });
    }
    events
}

/// A tool call that answered.
fn answer(text: &str) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.trim_end())])
}

/// A tool call that refused, in terms the agent can act on.
fn refusal(text: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text.trim_end())])
}

impl<A: AgentApi, T: TreeStatus + 'static> ServerHandler for FlycoTools<A, T> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("flyco", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools().await?))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(self.call(request).await?.into())
    }
}

#[cfg(test)]
mod tests {
    use core::future::{Future, ready};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use flyco_core::wire::{ApprovalPayload, DesktopButton, DesktopInputEvent};
    use flyco_core::{
        AgentMachineView, ApprovalId, BillingMinimum, BudgetStage, BudgetView, CloudProviderKind,
        MachineCapacity, MachineCatalogEntry, MachineOrigin, MachinePricing, MachineState,
        OsFamily, Runtime, SessionMachine, StoragePricing, Usd,
    };
    use rmcp::model::CallToolRequestParams;
    use serde_json::json;

    use super::{
        BUDGET_STATUS, COMPUTER_CLICK, COMPUTER_KEY, COMPUTER_MOVE, COMPUTER_SCREENSHOT,
        COMPUTER_STATE, COMPUTER_TOOLS, COMPUTER_TYPE, COMPUTER_WAIT, CallToolResult, ContentBlock,
        FlycoTools, MACHINE_RESIZE, MACHINE_STATUS, REPO_ADD, RepoAddInput, ResizeInput,
        TreeStatus,
    };
    use crate::control::rest::{AgentApi, ApprovalRaiser, ControlApiError};
    use crate::desktop::ipc::{AgentReply, AgentRequest};
    use crate::git::GitError;

    /// A control plane that answers from memory and remembers what it was
    /// asked to do.
    ///
    /// A `Mutex` around two `Vec`s rather than channels: these are
    /// assertions read *after* the call under test finished, and a channel
    /// would be a queue nobody is draining.
    #[derive(Debug)]
    struct FakePlane {
        catalog: Vec<MachineCatalogEntry>,
        machine: AgentMachineView,
        approvals: Mutex<Vec<ApprovalPayload>>,
        resizes: Mutex<Vec<String>>,
    }

    impl ApprovalRaiser for FakePlane {
        fn raise_approval(
            &self,
            payload: ApprovalPayload,
        ) -> impl Future<Output = Result<ApprovalId, ControlApiError>> + Send {
            self.approvals.lock().expect("not poisoned").push(payload);
            ready(Ok(ApprovalId::generate()))
        }
    }

    impl AgentApi for FakePlane {
        fn agent_machine(
            &self,
        ) -> impl Future<Output = Result<AgentMachineView, ControlApiError>> + Send {
            ready(Ok(self.machine.clone()))
        }

        fn agent_machine_catalog(
            &self,
        ) -> impl Future<Output = Result<Vec<MachineCatalogEntry>, ControlApiError>> + Send
        {
            ready(Ok(self.catalog.clone()))
        }

        fn resize_machine(
            &self,
            machine_type: &str,
        ) -> impl Future<Output = Result<(), ControlApiError>> + Send {
            self.resizes
                .lock()
                .expect("not poisoned")
                .push(machine_type.to_owned());
            ready(Ok(()))
        }

        fn agent_budget(&self) -> impl Future<Output = Result<BudgetView, ControlApiError>> + Send {
            ready(Ok(BudgetView {
                limit: Usd::from_dollars(10),
                spent: Usd::from_dollars(2),
                remaining: Usd::from_dollars(8),
                stage: BudgetStage::Ok,
            }))
        }
    }

    /// The session's checkouts as one status string apiece.
    ///
    /// A single `&'static str` is the developer-machine shape — one
    /// checkout, the root, keyed `None` — which is what the resize tests
    /// exercise.
    #[derive(Debug)]
    struct FakeTree(&'static str);

    impl TreeStatus for FakeTree {
        fn status(
            &self,
        ) -> impl Future<Output = Result<Vec<(Option<String>, String)>, GitError>> + Send {
            ready(Ok(vec![(None, self.0.to_owned())]))
        }
    }

    /// A desktop socket that answers every request with one canned reply
    /// and keeps what it heard.
    ///
    /// A real unix listener on a scratch path, because the IPC's contract
    /// is the wire: a test that never serialized a request would prove the
    /// tools agree with themselves rather than with the desktop.
    #[derive(Debug)]
    struct FakeDesktop {
        path: PathBuf,
        heard: Arc<Mutex<Vec<AgentRequest>>>,
    }

    /// Distinct socket names per test process.
    static SOCKETS: AtomicUsize = AtomicUsize::new(0);

    impl FakeDesktop {
        /// Binds a scratch socket and answers `reply` to every request,
        /// on a thread so `ask`'s own runtime is the one under test.
        fn answering(reply: AgentReply) -> Self {
            let path = std::env::temp_dir().join(format!(
                "flycod-test-{}-{}.desktop.sock",
                std::process::id(),
                SOCKETS.fetch_add(1, Ordering::Relaxed),
            ));
            let listener = UnixListener::bind(&path).expect("a scratch socket binds");
            let heard = Arc::new(Mutex::new(Vec::new()));
            let keeping = heard.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(stream) = conn else {
                        continue;
                    };
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    if let Ok(request) = serde_json::from_str::<AgentRequest>(&line) {
                        keeping.lock().expect("not poisoned").push(request);
                    }
                    let mut body = serde_json::to_vec(&reply).expect("the reply serializes");
                    body.push(b'\n');
                    let _ = reader.get_mut().write_all(&body);
                }
            });
            Self { path, heard }
        }

        /// The requests the socket heard.
        fn heard(&self) -> Vec<AgentRequest> {
            self.heard.lock().expect("not poisoned").clone()
        }
    }

    impl Drop for FakeDesktop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A tool call as the harness spells it.
    fn calling(name: &'static str, arguments: serde_json::Value) -> CallToolRequestParams {
        let mut params = CallToolRequestParams::new(name);
        params.arguments = match arguments {
            serde_json::Value::Object(args) => Some(args),
            _ => None,
        };
        params
    }

    fn entry(machine_type: &str, cents: u64, minimum_hours: Option<u32>) -> MachineCatalogEntry {
        MachineCatalogEntry {
            provider: CloudProviderKind::Aws,
            account: None,
            region: "us-east-1".to_owned(),
            location: None,
            machine_type: machine_type.to_owned(),
            runtime: Runtime::Vm,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity {
                vcpus: 8,
                memory_mib: 32 * 1024,
            }),
            lineage: None,
            pricing: MachinePricing::Metered {
                on_demand_hourly: Usd::from_cents(cents),
                spot_hourly: None,
                minimum: minimum_hours
                    .map(|hours| BillingMinimum::new(hours, Usd::from_cents(cents))),
                storage: StoragePricing::PerGibHourly {
                    rate: Usd::from_micros(100),
                },
            },
        }
    }

    fn tools(tree: &'static str, origin: MachineOrigin) -> FlycoTools<FakePlane, FakeTree> {
        tools_with_desktop(tree, origin, None)
    }

    fn tools_with_desktop(
        tree: &'static str,
        origin: MachineOrigin,
        desktop: Option<std::path::PathBuf>,
    ) -> FlycoTools<FakePlane, FakeTree> {
        FlycoTools::new(
            FakePlane {
                catalog: vec![
                    entry("m7i.2xlarge", 40, None),
                    entry("mac2.metal", 65, Some(24)),
                ],
                machine: AgentMachineView {
                    origin,
                    machine: SessionMachine {
                        machine_type: "m7i.xlarge".to_owned(),
                        hourly: Some(Usd::from_cents(20)),
                        spot: false,
                        capacity: Some(MachineCapacity {
                            vcpus: 4,
                            memory_mib: 16 * 1024,
                        }),
                        minimum: None,
                    },
                    state: MachineState::Running,
                    region: "us-east-1".to_owned(),
                },
                approvals: Mutex::new(Vec::new()),
                resizes: Mutex::new(Vec::new()),
            },
            FakeTree(tree),
            origin,
            desktop,
        )
    }

    fn resize(machine_type: &str, force: bool) -> ResizeInput {
        ResizeInput {
            machine_type: machine_type.to_owned(),
            reason: "the test suite needs more cores".to_owned(),
            force,
        }
    }

    fn text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn the_tools_are_named_and_described_as_the_contract_says() {
        let listed = tools("", MachineOrigin::Auto).tools().await.expect("list");
        let names: Vec<&str> = listed.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            [MACHINE_STATUS, BUDGET_STATUS, MACHINE_RESIZE, REPO_ADD]
        );

        let described = listed
            .iter()
            .find(|tool| tool.name == MACHINE_RESIZE)
            .and_then(|tool| tool.description.clone())
            .expect("the resize tool describes itself");

        // The three facts a resize costs, in the tool that costs them.
        assert!(described.contains("Resizing restarts the machine."));
        assert!(described.contains("The disk survives untouched"));
        assert!(described.contains("refuses while the working tree has uncommitted changes"));
        assert!(described.contains("`force: true`"));
        // The curated catalog, priced, with the licence minimum in dollars.
        assert!(described.contains("- m7i.2xlarge · 8 vCPU / 32 GiB · $0.40/hr"));
        assert!(described.contains(
            "- mac2.metal · 8 vCPU / 32 GiB · $0.65/hr · starts a 24-hour minimum charge of \
             $15.60 the moment it boots"
        ));
    }

    #[tokio::test]
    async fn the_resize_schema_takes_a_type_a_reason_and_an_optional_force() {
        let listed = tools("", MachineOrigin::Auto).tools().await.expect("list");
        let schema = listed
            .iter()
            .find(|tool| tool.name == MACHINE_RESIZE)
            .map(|tool| tool.input_schema.clone())
            .expect("the resize tool");

        let properties = schema["properties"].as_object().expect("an object schema");
        assert!(properties.contains_key("machine_type"));
        assert!(properties.contains_key("reason"));
        assert!(properties.contains_key("force"));

        let mut required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required names")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["machine_type", "reason"]);
    }

    #[tokio::test]
    async fn a_tool_listing_says_the_user_chose_the_machine_when_they_did() {
        let chosen = tools("", MachineOrigin::User).tools().await.expect("list");
        let described = chosen
            .iter()
            .find(|tool| tool.name == MACHINE_RESIZE)
            .and_then(|tool| tool.description.clone())
            .expect("a description");
        assert!(described.contains(
            "The user chose this machine themselves; do not switch it unless the task cannot \
             proceed on it, and say why when you do."
        ));
    }

    #[tokio::test]
    async fn a_resize_over_uncommitted_work_is_refused_until_it_is_forced() {
        let dirty = tools(" M crates/daemon/src/mcp.rs\n", MachineOrigin::Auto);

        let refused = dirty
            .machine_resize(resize("m7i.2xlarge", false))
            .await
            .expect("a refusal is an answer");
        assert_eq!(refused.is_error, Some(true));
        let said = text(&refused);
        assert!(said.starts_with("Refused: a working tree has uncommitted changes."));
        assert!(said.contains(" M crates/daemon/src/mcp.rs"));
        assert!(dirty.api.resizes.lock().expect("not poisoned").is_empty());

        let forced = dirty
            .machine_resize(resize("m7i.2xlarge", true))
            .await
            .expect("a forced resize is performed");
        assert_ne!(forced.is_error, Some(true));
        assert_eq!(
            dirty.api.resizes.lock().expect("not poisoned").as_slice(),
            ["m7i.2xlarge"]
        );
    }

    #[tokio::test]
    async fn a_clean_tree_resizes_and_says_what_the_restart_costs() {
        let clean = tools("", MachineOrigin::Auto);
        let done = clean
            .machine_resize(resize("m7i.2xlarge", false))
            .await
            .expect("resize");

        assert_ne!(done.is_error, Some(true));
        let said = text(&done);
        assert!(said.contains("Resizing this session onto m7i.2xlarge"));
        assert!(said.contains("The machine restarts now"));
        assert!(said.contains("The disk is kept"));
        assert_eq!(
            clean.api.resizes.lock().expect("not poisoned").as_slice(),
            ["m7i.2xlarge"]
        );
    }

    #[tokio::test]
    async fn a_license_bound_type_becomes_an_approval_rather_than_a_resize() {
        let clean = tools("", MachineOrigin::Auto);
        let pending = clean
            .machine_resize(resize("mac2.metal", false))
            .await
            .expect("an approval is an answer");

        // Nothing was resized: the money is the user's to spend.
        assert!(clean.api.resizes.lock().expect("not poisoned").is_empty());
        let raised = clean.api.approvals.lock().expect("not poisoned").clone();
        assert_eq!(
            raised,
            [ApprovalPayload::MachineResizeLicenseBound {
                machine_type: "mac2.metal".to_owned(),
                minimum: BillingMinimum::new(24, Usd::from_cents(65)),
                reason: "the test suite needs more cores".to_owned(),
            }]
        );

        let said = text(&pending);
        assert!(said.starts_with("Pending the user's approval."));
        assert!(said.contains("$15.60"));
        assert!(said.contains("m7i.xlarge"));
    }

    #[tokio::test]
    async fn a_type_outside_the_catalog_is_refused_rather_than_attempted() {
        let clean = tools("", MachineOrigin::Auto);
        let refused = clean
            .machine_resize(resize("x1e.32xlarge", false))
            .await
            .expect("a refusal is an answer");

        assert_eq!(refused.is_error, Some(true));
        assert!(text(&refused).contains("`x1e.32xlarge` is not one of the machine types"));
        assert!(clean.api.resizes.lock().expect("not poisoned").is_empty());
    }

    #[tokio::test]
    async fn machine_status_reports_the_live_machine_and_who_chose_it() {
        let said = text(
            &tools("", MachineOrigin::User)
                .machine_status()
                .await
                .expect("status"),
        );
        assert!(said.contains("m7i.xlarge · 4 vCPU / 16 GiB · $0.20/hr"));
        assert!(said.contains("in us-east-1"));
        assert!(said.contains("The user chose this machine themselves"));
    }

    #[tokio::test]
    async fn budget_status_reports_what_is_left() {
        let said = text(
            &tools("", MachineOrigin::Auto)
                .budget_status()
                .await
                .expect("budget"),
        );
        assert!(said.contains("spent $2.00 of its $10.00 compute budget, leaving $8.00"));
    }

    #[tokio::test]
    async fn a_session_with_a_desktop_lists_the_computer_tools() {
        let desktop = FakeDesktop::answering(AgentReply::Done);
        let listed = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()))
            .tools()
            .await
            .expect("list");
        let names: Vec<&str> = listed.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names.len(), 4 + COMPUTER_TOOLS.len());
        for name in COMPUTER_TOOLS {
            assert!(names.contains(&name), "{name} is listed");
        }
    }

    fn repo_add(repo: &str, branch: Option<&str>) -> RepoAddInput {
        RepoAddInput {
            repo: repo.to_owned(),
            branch: branch.map(str::to_owned),
            reason: "the task needs the fixture repository".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_click_moves_presses_and_releases_over_the_socket() {
        let desktop = FakeDesktop::answering(AgentReply::Done);
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        let result = server
            .call(calling(COMPUTER_CLICK, json!({"x": 100, "y": 50})))
            .await
            .expect("the call answers");

        assert_eq!(result.is_error, Some(false));
        let heard = desktop.heard();
        assert_eq!(heard.len(), 1);
        let AgentRequest::Input { events } = &heard[0] else {
            panic!("a click is an input request");
        };
        assert_eq!(
            events.as_slice(),
            [
                DesktopInputEvent::Move { x: 100, y: 50 },
                DesktopInputEvent::Button {
                    x: 100,
                    y: 50,
                    button: DesktopButton::Left,
                    pressed: true,
                },
                DesktopInputEvent::Button {
                    x: 100,
                    y: 50,
                    button: DesktopButton::Left,
                    pressed: false,
                },
            ]
        );
    }

    #[tokio::test]
    async fn repo_add_raises_an_approval_and_says_it_is_pending() {
        let tools = tools("", MachineOrigin::Auto);
        let asked = tools
            .repo_add(repo_add("lexoliu/aither", Some("main")))
            .await
            .expect("an approval is an answer");

        assert_ne!(asked.is_error, Some(true));
        let said = text(&asked);
        assert!(said.contains("Asked the user to add `lexoliu/aither`"));
        assert!(said.contains("approval card"));

        let raised = tools.api.approvals.lock().expect("not poisoned").clone();
        assert_eq!(
            raised,
            [ApprovalPayload::RepoAdd {
                repo: "lexoliu/aither".to_owned(),
                branch: Some("main".to_owned()),
                reason: "the task needs the fixture repository".to_owned(),
            }]
        );
    }

    #[tokio::test]
    async fn typing_sends_one_press_release_pair_per_character() {
        let desktop = FakeDesktop::answering(AgentReply::Done);
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        server
            .call(calling(COMPUTER_TYPE, json!({"text": "Hi"})))
            .await
            .expect("the call answers");

        let AgentRequest::Input { events } = &desktop.heard()[0] else {
            panic!("typing is an input request");
        };
        assert_eq!(
            events.as_slice(),
            ["H", "i"]
                .iter()
                .flat_map(|key| [
                    DesktopInputEvent::Key {
                        code: String::new(),
                        key: (*key).to_owned(),
                        pressed: true,
                    },
                    DesktopInputEvent::Key {
                        code: String::new(),
                        key: (*key).to_owned(),
                        pressed: false,
                    },
                ])
                .collect::<Vec<_>>()
                .as_slice()
        );
    }

    #[tokio::test]
    async fn a_key_chord_holds_its_modifiers_around_the_key() {
        let desktop = FakeDesktop::answering(AgentReply::Done);
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        server
            .call(calling(
                COMPUTER_KEY,
                json!({"key": "c", "modifiers": ["control"]}),
            ))
            .await
            .expect("the call answers");

        let AgentRequest::Input { events } = &desktop.heard()[0] else {
            panic!("a chord is an input request");
        };
        let keys: Vec<(&str, bool)> = events
            .iter()
            .map(|event| match event {
                DesktopInputEvent::Key { key, pressed, .. } => (key.as_str(), *pressed),
                other => panic!("a chord is all key events, not {other:?}"),
            })
            .collect();
        assert_eq!(
            keys,
            [
                ("Control", true),
                ("c", true),
                ("c", false),
                ("Control", false),
            ]
        );
    }

    #[tokio::test]
    async fn a_screenshot_comes_back_as_an_image_block() {
        let desktop = FakeDesktop::answering(AgentReply::Screenshot {
            png_base64: "aGk=".to_owned(),
        });
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        let result = server
            .call(calling(COMPUTER_SCREENSHOT, json!({})))
            .await
            .expect("the call answers");

        let Some(ContentBlock::Image(image)) = result.content.first() else {
            panic!("a screenshot is an image, not {:?}", result.content);
        };
        assert_eq!(image.data, "aGk=");
        assert_eq!(image.mime_type, "image/png");
    }

    #[tokio::test]
    async fn computer_state_names_the_display_and_whose_hands_are_on_it() {
        let desktop = FakeDesktop::answering(AgentReply::State {
            takeover: true,
            display: ":99".to_owned(),
            width: 1280,
            height: 800,
        });
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        let said = text(
            &server
                .call(calling(COMPUTER_STATE, json!({})))
                .await
                .expect("the call answers"),
        );
        assert!(said.contains("1280x800"));
        assert!(said.contains("DISPLAY=:99"));
        assert!(said.contains("the user is driving"));
    }

    #[tokio::test]
    async fn a_desktop_under_takeover_refuses_the_agent_in_its_own_words() {
        let desktop = FakeDesktop::answering(AgentReply::Refused {
            reason: "the user is driving the desktop".to_owned(),
        });
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        let refused = server
            .call(calling(COMPUTER_MOVE, json!({"x": 1, "y": 1})))
            .await
            .expect("a refusal is an answer");
        assert_eq!(refused.is_error, Some(true));
        assert!(text(&refused).contains("the user is driving the desktop"));
    }

    #[tokio::test]
    async fn an_unbound_desktop_socket_is_a_refusal_not_an_error() {
        let path = std::env::temp_dir().join(format!(
            "flycod-test-{}-never.desktop.sock",
            std::process::id()
        ));
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(path));
        let refused = server
            .call(calling(COMPUTER_MOVE, json!({"x": 1, "y": 1})))
            .await
            .expect("a refusal is an answer");
        assert_eq!(refused.is_error, Some(true));
        assert!(text(&refused).contains("desktop is not running"));
    }

    #[tokio::test]
    async fn computer_wait_refuses_to_hold_past_its_cap() {
        let server = tools_with_desktop(
            "",
            MachineOrigin::Auto,
            Some(PathBuf::from("/tmp/flycod-unbound.sock")),
        );
        let refused = server
            .call(calling(COMPUTER_WAIT, json!({"seconds": 121})))
            .await
            .expect("a refusal is an answer");
        assert_eq!(refused.is_error, Some(true));
        assert!(text(&refused).contains("at most 120 seconds"));
    }

    #[tokio::test]
    async fn repo_add_refuses_arguments_no_card_should_carry() {
        let tools = tools("", MachineOrigin::Auto);

        let bad_slug = tools.repo_add(repo_add("not-a-slug", None)).await;
        assert!(
            matches!(
                bad_slug,
                Err(crate::mcp::ToolFailure::Arguments { tool: REPO_ADD, .. })
            ),
            "a malformed slug is an argument error, not an approval card: {bad_slug:?}"
        );
        let bad_branch = tools.repo_add(repo_add("lexoliu/aither", Some("-f"))).await;
        assert!(
            matches!(
                bad_branch,
                Err(crate::mcp::ToolFailure::Arguments { tool: REPO_ADD, .. })
            ),
            "a branch git would read as a flag is an argument error: {bad_branch:?}"
        );
        assert!(tools.api.approvals.lock().expect("not poisoned").is_empty());
    }

    #[tokio::test]
    async fn repo_trees_dynamically_discovers_mid_session_add_repo_checkouts() {
        let workdir =
            std::env::temp_dir().join(format!("flycod-mcp-discover-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workdir).expect("create workdir");
        let trees = super::RepoTrees::discover(workdir.clone());

        // Initially empty workspace -> no checkouts.
        assert_eq!(
            trees.status().await.expect("empty status"),
            Vec::<(Option<String>, String)>::new()
        );

        // Simulate mid-session `AddRepo` creating a git checkout under workdir.
        let repo_a = workdir.join("repo-a");
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo_a)
            .status()
            .expect("git init");
        assert!(init.success());
        std::fs::write(repo_a.join("dirty.txt"), "uncommitted\n").expect("write dirty file");

        let statuses = trees.status().await.expect("discovered status");
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].0.as_deref(), Some("repo-a"));
        assert!(statuses[0].1.contains("dirty.txt"));

        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[tokio::test]
    async fn desktop_tools_appear_and_disappear_dynamically_with_socket_availability() {
        let socket_path = std::env::temp_dir().join(format!(
            "flycod-dyn-{}-{}.sock",
            std::process::id(),
            SOCKETS.fetch_add(1, Ordering::Relaxed),
        ));
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(socket_path.clone()));

        // Before SetComputerUse binds the socket, computer_* tools are absent.
        let before = server.tools().await.expect("list before desktop");
        assert_eq!(before.len(), 4);

        // Bind desktop socket -> computer_* tools appear on the very next tools/list.
        let listener = UnixListener::bind(&socket_path).expect("bind desktop socket");
        let during = server.tools().await.expect("list with desktop");
        assert_eq!(during.len(), 4 + COMPUTER_TOOLS.len());

        // Remove socket -> computer_* tools disappear again.
        drop(listener);
        let _ = std::fs::remove_file(&socket_path);
        let after = server.tools().await.expect("list after desktop stops");
        assert_eq!(after.len(), 4);
    }

    #[tokio::test]
    async fn catalog_is_cached_across_repeated_tools_list_and_resize() {
        #[derive(Debug)]
        struct CountingPlane {
            machine_calls: AtomicUsize,
            catalog_calls: AtomicUsize,
        }

        impl ApprovalRaiser for CountingPlane {
            fn raise_approval(
                &self,
                _payload: ApprovalPayload,
            ) -> impl Future<Output = Result<ApprovalId, ControlApiError>> + Send {
                ready(Ok(ApprovalId::generate()))
            }
        }

        impl AgentApi for CountingPlane {
            fn agent_machine(
                &self,
            ) -> impl Future<Output = Result<AgentMachineView, ControlApiError>> + Send
            {
                self.machine_calls.fetch_add(1, Ordering::SeqCst);
                ready(Ok(AgentMachineView {
                    origin: MachineOrigin::Auto,
                    machine: SessionMachine {
                        machine_type: "m7i.xlarge".to_owned(),
                        hourly: Some(Usd::from_cents(20)),
                        spot: false,
                        capacity: None,
                        minimum: None,
                    },
                    state: MachineState::Running,
                    region: "us-east-1".to_owned(),
                }))
            }

            fn agent_machine_catalog(
                &self,
            ) -> impl Future<Output = Result<Vec<MachineCatalogEntry>, ControlApiError>> + Send
            {
                self.catalog_calls.fetch_add(1, Ordering::SeqCst);
                ready(Ok(vec![entry("m7i.2xlarge", 40, None)]))
            }

            fn resize_machine(
                &self,
                _machine_type: &str,
            ) -> impl Future<Output = Result<(), ControlApiError>> + Send {
                ready(Ok(()))
            }

            fn agent_budget(
                &self,
            ) -> impl Future<Output = Result<BudgetView, ControlApiError>> + Send {
                ready(Ok(BudgetView {
                    limit: Usd::from_dollars(10),
                    spent: Usd::from_dollars(1),
                    remaining: Usd::from_dollars(9),
                    stage: BudgetStage::Ok,
                }))
            }
        }

        let server = FlycoTools::new(
            CountingPlane {
                machine_calls: AtomicUsize::new(0),
                catalog_calls: AtomicUsize::new(0),
            },
            FakeTree(""),
            MachineOrigin::Auto,
            None,
        );

        let _ = server.tools().await.expect("first tools/list");
        let _ = server.tools().await.expect("second tools/list");
        let _ = server
            .machine_resize(resize("m7i.2xlarge", false))
            .await
            .expect("resize");

        assert_eq!(
            server.api.catalog_calls.load(Ordering::SeqCst),
            1,
            "static machine catalog must be fetched once and reused across tools/list and resize"
        );
    }

    #[tokio::test]
    async fn rate_limit_429_returns_explicit_refusal_and_enforces_local_cooldown() {
        use std::num::NonZeroU32;

        #[derive(Debug)]
        struct RateLimitedPlane {
            calls: AtomicUsize,
            kind: &'static str,
            retry_after: u32,
        }

        impl ApprovalRaiser for RateLimitedPlane {
            fn raise_approval(
                &self,
                _payload: ApprovalPayload,
            ) -> impl Future<Output = Result<ApprovalId, ControlApiError>> + Send {
                self.calls.fetch_add(1, Ordering::SeqCst);
                ready(Err(self.err()))
            }
        }

        impl RateLimitedPlane {
            fn err(&self) -> ControlApiError {
                ControlApiError::Refused {
                    method: "GET",
                    path: "/v1/sessions/s/agent/budget".to_owned(),
                    status: 429,
                    title: "Too Many Requests".to_owned(),
                    detail: "daily quota spent".to_owned(),
                    kind: format!("https://flyco.dev/problems/{}", self.kind),
                    retry_after_secs: NonZeroU32::new(self.retry_after),
                }
            }
        }

        impl AgentApi for RateLimitedPlane {
            fn agent_machine(
                &self,
            ) -> impl Future<Output = Result<AgentMachineView, ControlApiError>> + Send
            {
                self.calls.fetch_add(1, Ordering::SeqCst);
                ready(Err(self.err()))
            }

            fn agent_machine_catalog(
                &self,
            ) -> impl Future<Output = Result<Vec<MachineCatalogEntry>, ControlApiError>> + Send
            {
                self.calls.fetch_add(1, Ordering::SeqCst);
                ready(Err(self.err()))
            }

            fn resize_machine(
                &self,
                _machine_type: &str,
            ) -> impl Future<Output = Result<(), ControlApiError>> + Send {
                self.calls.fetch_add(1, Ordering::SeqCst);
                ready(Err(self.err()))
            }

            fn agent_budget(
                &self,
            ) -> impl Future<Output = Result<BudgetView, ControlApiError>> + Send {
                self.calls.fetch_add(1, Ordering::SeqCst);
                ready(Err(self.err()))
            }
        }

        let exhausted = FlycoTools::new(
            RateLimitedPlane {
                calls: AtomicUsize::new(0),
                kind: "request-budget-exhausted",
                retry_after: 3600,
            },
            FakeTree(""),
            MachineOrigin::Auto,
            None,
        );

        let first = exhausted
            .budget_status()
            .await
            .expect("429 is returned as a structured refusal, not internal_error");
        assert_eq!(first.is_error, Some(true));
        let first_text = text(&first);
        assert!(
            first_text.contains("request-budget-exhausted"),
            "{first_text}"
        );
        assert!(first_text.contains("Do not retry"), "{first_text}");

        // Second call is refused locally without hitting the control plane again.
        let second = exhausted
            .machine_status()
            .await
            .expect("local cooldown refusal");
        assert_eq!(second.is_error, Some(true));
        assert!(text(&second).contains("request-budget-exhausted"));
        assert_eq!(
            exhausted.api.calls.load(Ordering::SeqCst),
            1,
            "local cooldown must block follow-up calls without hitting control plane"
        );
    }

    #[tokio::test]
    async fn desktop_toggle_emits_tools_list_changed_notification_over_mcp() {
        use rmcp::ServiceExt as _;
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

        let socket_path = std::env::temp_dir().join(format!(
            "flycod-notify-{}-{}.sock",
            std::process::id(),
            SOCKETS.fetch_add(1, Ordering::Relaxed),
        ));
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(socket_path.clone()));
        let (desktop_socket, last_live) = server
            .desktop_watcher_state()
            .expect("watcher state present");

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move { server.serve(server_io).await });

        let (client_read, mut client_write) = tokio::io::split(client_io);
        let mut lines = BufReader::new(client_read).lines();

        // Standard MCP initialize handshake (declaring listChanged support).
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1.0" }
            }
        });
        client_write
            .write_all(format!("{init_req}\n").as_bytes())
            .await
            .expect("write init");
        let init_resp = lines
            .next_line()
            .await
            .expect("read init resp")
            .expect("init line");
        let init_json: serde_json::Value =
            serde_json::from_str(&init_resp).expect("init response json");
        assert_eq!(
            init_json["result"]["capabilities"]["tools"]["listChanged"],
            json!(true),
            "server must advertise tools.listChanged capability: {init_json}"
        );

        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        client_write
            .write_all(format!("{initialized}\n").as_bytes())
            .await
            .expect("write initialized");

        let running = server_task
            .await
            .expect("serve task")
            .expect("server initialized");

        // Bind desktop socket -> sync emits notifications/tools/list_changed.
        let listener = UnixListener::bind(&socket_path).expect("bind desktop socket");
        let toggled =
            super::sync_desktop_list_changed(&desktop_socket, &last_live, running.peer()).await;
        assert!(toggled, "binding desktop socket must trigger toggle");

        let note = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .expect("notification arrived in time")
            .expect("read notification")
            .expect("notification line");
        let note_json: serde_json::Value = serde_json::from_str(&note).expect("notification json");
        assert_eq!(
            note_json["method"],
            json!("notifications/tools/list_changed")
        );

        // Drop desktop socket -> sync emits notifications/tools/list_changed again.
        drop(listener);
        let _ = std::fs::remove_file(&socket_path);
        let toggled_off =
            super::sync_desktop_list_changed(&desktop_socket, &last_live, running.peer()).await;
        assert!(toggled_off, "removing desktop socket must trigger toggle");

        let note_off = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .expect("off notification arrived")
            .expect("read off notification")
            .expect("off notification line");
        let note_off_json: serde_json::Value =
            serde_json::from_str(&note_off).expect("off notification json");
        assert_eq!(
            note_off_json["method"],
            json!("notifications/tools/list_changed")
        );

        drop(lines);
        drop(client_write);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), running.waiting())
            .await
            .expect("server shuts down on client EOF");
    }
}
