//! Local Unix-domain-socket broker for `flycod mcp`.
//!
//! Under dual-UID isolation, `flycod run` owns `0600 /etc/flycod/config.toml`
//! and holds the session's `fd_` daemon token, while `flycod mcp` is spawned by
//! the coding harness under the unprivileged agent UID. `flycod mcp` never
//! reads `config.toml` and never touches `daemon_token`; instead it connects to
//! this broker over a local Unix socket to retrieve its non-secret session
//! metadata and proxy [`AgentApi`] / [`ApprovalRaiser`] calls to the control
//! plane.
//!
//! Structured control-plane errors — specifically `429 Retry-After` and
//! `request-budget-exhausted` / `row-budget-exceeded` problem types — are
//! preserved verbatim across the socket, and [`RateLimitGuard`] enforces local
//! cooldowns so neither the MCP server nor a restarted MCP process hammers the
//! control plane during a rate-limit or budget-exhausted window.

use std::num::NonZeroU32;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flyco_core::wire::ApprovalPayload;
use flyco_core::{
    AgentMachineView, ApprovalId, BudgetView, MachineCatalogEntry, MachineOrigin, SessionId,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use crate::control::rest::{AgentApi, ApprovalRaiser, ControlApiError};
use crate::privilege::{AgentUser, chown_path, current_euid, ensure_dir};

/// Maximum size of one JSON request or response line over the broker socket.
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;

/// Maximum time a broker client waits for a proxied control-plane reply.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Fallback wait when a `429` omits `Retry-After`.
const DEFAULT_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// Active rate-limit or daily-budget refusal tracked locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitStatus {
    /// Temporary per-minute rate limit (`429 rate-limited` or bare `429`).
    CoolingDown {
        /// Remaining seconds before the next request may be sent.
        remaining_secs: u64,
    },
    /// Daily account/principal budget spent until reset
    /// (`request-budget-exhausted` or `row-budget-exceeded`).
    BudgetExhausted {
        /// Problem slug (`request-budget-exhausted` or `row-budget-exceeded`).
        kind: String,
        /// Remaining seconds before the window resets.
        remaining_secs: u64,
        /// Human-readable explanation from the control plane.
        detail: String,
    },
}

impl RateLimitStatus {
    /// Human-readable refusal sentence for the model or caller.
    #[must_use]
    pub fn refusal_message(&self) -> String {
        match self {
            Self::CoolingDown { remaining_secs } => format!(
                "Refused: the control plane rate-limited this session (`429`). \
                 Wait at least {remaining_secs}s before retrying this tool."
            ),
            Self::BudgetExhausted {
                kind,
                remaining_secs,
                detail,
            } => format!(
                "Refused: the control plane rejected this request with `{kind}` \
                 ({detail}). Do not retry this tool — the budget will not reset \
                 for approximately {remaining_secs}s; inform the user."
            ),
        }
    }
}

#[derive(Debug, Clone)]
enum CooldownEntry {
    Clear,
    CoolingDown {
        until: Instant,
    },
    BudgetExhausted {
        until: Instant,
        kind: String,
        detail: String,
    },
}

/// Shared local `429 Retry-After` and `request-budget-exhausted` guard.
///
/// Checked before any control-plane request and updated on any `429` response,
/// ensuring repeated model tool calls or reconnects cannot bypass `Retry-After`
/// (obeying `AGENTS.md` client rule #2).
#[derive(Debug, Default)]
pub struct RateLimitGuard {
    state: Mutex<Option<CooldownEntry>>,
}

impl RateLimitGuard {
    /// Creates an initially clear guard.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    /// Checks whether requests are currently blocked at `now`.
    #[must_use]
    pub fn check_at(&self, now: Instant) -> Option<RateLimitStatus> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.as_ref()?;
        match entry {
            CooldownEntry::Clear => None,
            CooldownEntry::CoolingDown { until } => {
                if now < *until {
                    let remaining_secs = ceil_secs(until.saturating_duration_since(now));
                    Some(RateLimitStatus::CoolingDown { remaining_secs })
                } else {
                    *guard = Some(CooldownEntry::Clear);
                    None
                }
            }
            CooldownEntry::BudgetExhausted {
                until,
                kind,
                detail,
            } => {
                if now < *until {
                    let remaining_secs = ceil_secs(until.saturating_duration_since(now));
                    Some(RateLimitStatus::BudgetExhausted {
                        kind: kind.clone(),
                        remaining_secs,
                        detail: detail.clone(),
                    })
                } else {
                    *guard = Some(CooldownEntry::Clear);
                    None
                }
            }
        }
    }

    /// Checks whether requests are currently blocked right now.
    #[must_use]
    pub fn check(&self) -> Option<RateLimitStatus> {
        self.check_at(Instant::now())
    }

    /// Inspects a [`ControlApiError`]; if it is a `429`, records the cooldown
    /// at `now` and returns the corresponding [`RateLimitStatus`].
    pub fn record_error_at(
        &self,
        error: &ControlApiError,
        now: Instant,
    ) -> Option<RateLimitStatus> {
        let status_code = match error {
            ControlApiError::Refused { status, .. } | ControlApiError::Status { status, .. } => {
                *status
            }
            ControlApiError::Transport(_) | ControlApiError::Unaddressable(_) => return None,
        };
        if status_code != 429 {
            return None;
        }
        let wait = error.retry_after().unwrap_or(DEFAULT_RATE_LIMIT_WAIT);
        let until = now + wait;
        let remaining_secs = ceil_secs(wait);
        let kind = error.kind().map(str::to_owned);
        let is_budget = matches!(
            kind.as_deref(),
            Some("request-budget-exhausted" | "row-budget-exceeded")
        );

        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if is_budget {
            let kind = kind.unwrap_or_else(|| "request-budget-exhausted".to_owned());
            let detail = match error {
                ControlApiError::Refused { detail, .. } if !detail.is_empty() => detail.clone(),
                _ => "daily control-plane request budget exhausted".to_owned(),
            };
            *guard = Some(CooldownEntry::BudgetExhausted {
                until,
                kind: kind.clone(),
                detail: detail.clone(),
            });
            drop(guard);
            Some(RateLimitStatus::BudgetExhausted {
                kind,
                remaining_secs,
                detail,
            })
        } else {
            // Never downgrade an active daily budget-exhausted block to a
            // shorter per-minute cooldown.
            if let Some(CooldownEntry::BudgetExhausted {
                until: existing_until,
                kind,
                detail,
            }) = guard.as_ref()
                && now < *existing_until
            {
                let status = RateLimitStatus::BudgetExhausted {
                    kind: kind.clone(),
                    remaining_secs: ceil_secs(existing_until.saturating_duration_since(now)),
                    detail: detail.clone(),
                };
                drop(guard);
                return Some(status);
            }
            *guard = Some(CooldownEntry::CoolingDown { until });
            drop(guard);
            Some(RateLimitStatus::CoolingDown { remaining_secs })
        }
    }

    /// Inspects a [`ControlApiError`] at `Instant::now()`.
    pub fn record_error(&self, error: &ControlApiError) -> Option<RateLimitStatus> {
        self.record_error_at(error, Instant::now())
    }
}

fn ceil_secs(duration: Duration) -> u64 {
    let secs = duration.as_secs();
    if duration.subsec_nanos() > 0 {
        secs.saturating_add(1)
    } else {
        secs.max(1)
    }
}

/// Non-secret session metadata handed to `flycod mcp` over the broker socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpBootstrap {
    /// Session identifier (for logging).
    pub session: SessionId,
    /// Workspace root under which checkouts are dynamically discovered.
    pub workdir: PathBuf,
    /// Who chose the session's machine (`auto` or `user`).
    pub machine_origin: MachineOrigin,
    /// Path where the desktop IPC socket lives whenever computer use is active.
    pub desktop_socket: PathBuf,
}

/// One JSON-line request from `flycod mcp` to `flycod run`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum BrokerRequest {
    Bootstrap,
    AgentMachine,
    AgentMachineCatalog,
    ResizeMachine { machine_type: String },
    AgentBudget,
    RaiseApproval { payload: ApprovalPayload },
}

/// Structured control-plane failure forwarded across the broker socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BrokerError {
    Transport {
        message: String,
    },
    Refused {
        method: String,
        path: String,
        status: u16,
        title: String,
        detail: String,
        problem_kind: String,
        retry_after_secs: Option<u32>,
    },
    Status {
        method: String,
        path: String,
        status: u16,
        retry_after_secs: Option<u32>,
    },
    Unaddressable {
        path: String,
    },
}

impl From<ControlApiError> for BrokerError {
    fn from(error: ControlApiError) -> Self {
        match error {
            ControlApiError::Transport(message) => Self::Transport { message },
            ControlApiError::Refused {
                method,
                path,
                status,
                title,
                detail,
                kind,
                retry_after_secs,
            } => Self::Refused {
                method: method.to_owned(),
                path,
                status,
                title,
                detail,
                problem_kind: kind,
                retry_after_secs: retry_after_secs.map(NonZeroU32::get),
            },
            ControlApiError::Status {
                method,
                path,
                status,
                retry_after_secs,
            } => Self::Status {
                method: method.to_owned(),
                path,
                status,
                retry_after_secs: retry_after_secs.map(NonZeroU32::get),
            },
            ControlApiError::Unaddressable(path) => Self::Unaddressable { path },
        }
    }
}

impl From<BrokerError> for ControlApiError {
    fn from(error: BrokerError) -> Self {
        match error {
            BrokerError::Transport { message } => Self::Transport(message),
            BrokerError::Refused {
                method,
                path,
                status,
                title,
                detail,
                problem_kind,
                retry_after_secs,
            } => Self::Refused {
                method: static_method(&method),
                path,
                status,
                title,
                detail,
                kind: problem_kind,
                retry_after_secs: retry_after_secs.and_then(NonZeroU32::new),
            },
            BrokerError::Status {
                method,
                path,
                status,
                retry_after_secs,
            } => Self::Status {
                method: static_method(&method),
                path,
                status,
                retry_after_secs: retry_after_secs.and_then(NonZeroU32::new),
            },
            BrokerError::Unaddressable { path } => Self::Unaddressable(path),
        }
    }
}

fn static_method(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        _ => "REQUEST",
    }
}

/// One JSON-line reply from `flycod run` to `flycod mcp`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BrokerResponse {
    Bootstrap { bootstrap: McpBootstrap },
    Machine { view: AgentMachineView },
    Catalog { entries: Vec<MachineCatalogEntry> },
    Ok,
    Budget { view: BudgetView },
    Approval { id: ApprovalId },
    Err { error: BrokerError },
}

/// Live handle to the Unix-socket broker hosted by `flycod run`.
///
/// Dropping the handle aborts the accept task and unlinks the socket file.
#[derive(Debug)]
pub struct McpBrokerHandle {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl McpBrokerHandle {
    /// Path clients (`flycod mcp --socket <PATH>`) connect to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for McpBrokerHandle {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Starts the Unix-socket MCP broker for one session.
///
/// When `agent` is `Some`, the socket is created mode `0660` with group
/// `agent.gid` (and parent directory `0750` group `agent.gid`), and every
/// incoming connection is checked via OS peer credentials (`SO_PEERCRED` /
/// `LOCAL_PEERCRED`) to permit only `agent.uid` and the daemon's own `euid`.
///
/// # Errors
///
/// Returns an `io::Error` if the socket directory cannot be created, the
/// socket cannot be bound, or permissions/ownership cannot be applied.
pub fn spawn<A: AgentApi + 'static>(
    path: PathBuf,
    bootstrap: McpBootstrap,
    api: A,
    agent: Option<&AgentUser>,
) -> std::io::Result<McpBrokerHandle> {
    if let Some(parent) = path.parent() {
        let dir_mode = if agent.is_some() { 0o750 } else { 0o700 };
        ensure_dir(parent, dir_mode, agent, true)?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let sock_mode = if agent.is_some() { 0o660 } else { 0o600 };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(sock_mode))?;
    if let Some(user) = agent
        && user.needs_uid_switch()
    {
        chown_path(&path, None, Some(user.gid)).map_err(std::io::Error::other)?;
    }

    let daemon_uid = current_euid();
    let allowed_agent_uid = agent.map(|user| user.uid);
    let state = Arc::new(BrokerState {
        bootstrap,
        api,
        rate_limit: RateLimitGuard::new(),
    });
    let task = tokio::spawn(accept_loop(listener, state, daemon_uid, allowed_agent_uid));
    tracing::info!(socket = %path.display(), "started local MCP broker");
    Ok(McpBrokerHandle { path, task })
}

struct BrokerState<A> {
    bootstrap: McpBootstrap,
    api: A,
    rate_limit: RateLimitGuard,
}

async fn accept_loop<A: AgentApi + 'static>(
    listener: UnixListener,
    state: Arc<BrokerState<A>>,
    daemon_uid: u32,
    allowed_agent_uid: Option<u32>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                match stream.peer_cred() {
                    Ok(cred) => {
                        let peer_uid = cred.uid();
                        let allowed = peer_uid == daemon_uid
                            || allowed_agent_uid.is_some_and(|uid| peer_uid == uid);
                        if !allowed {
                            tracing::warn!(
                                peer_uid,
                                daemon_uid,
                                ?allowed_agent_uid,
                                "rejected unauthorized UID on MCP broker socket"
                            );
                            continue;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not read peer credentials on MCP broker socket");
                        continue;
                    }
                }
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, state).await {
                        tracing::debug!(%error, "MCP broker connection ended");
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error, "MCP broker accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn handle_connection<A: AgentApi>(
    stream: UnixStream,
    state: Arc<BrokerState<A>>,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(reader.take(MAX_LINE_BYTES)).read_line(&mut line),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "broker request timed out"))??;
    if read == 0 {
        return Ok(());
    }
    let response = match serde_json::from_str::<BrokerRequest>(&line) {
        Ok(request) => dispatch(&state, request).await,
        Err(error) => BrokerResponse::Err {
            error: BrokerError::Transport {
                message: format!("malformed broker request: {error}"),
            },
        },
    };
    let mut encoded = serde_json::to_vec(&response)
        .map_err(|error| std::io::Error::other(format!("encode broker reply: {error}")))?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.shutdown().await?;
    Ok(())
}

async fn dispatch<A: AgentApi>(state: &BrokerState<A>, request: BrokerRequest) -> BrokerResponse {
    if matches!(request, BrokerRequest::Bootstrap) {
        return BrokerResponse::Bootstrap {
            bootstrap: state.bootstrap.clone(),
        };
    }

    if let Some(blocked) = state.rate_limit.check() {
        return BrokerResponse::Err {
            error: rate_limit_to_broker_error(&blocked),
        };
    }

    match request {
        BrokerRequest::Bootstrap => unreachable!("handled above"),
        BrokerRequest::AgentMachine => match state.api.agent_machine().await {
            Ok(view) => BrokerResponse::Machine { view },
            Err(error) => {
                let _ = state.rate_limit.record_error(&error);
                BrokerResponse::Err {
                    error: error.into(),
                }
            }
        },
        BrokerRequest::AgentMachineCatalog => match state.api.agent_machine_catalog().await {
            Ok(entries) => BrokerResponse::Catalog { entries },
            Err(error) => {
                let _ = state.rate_limit.record_error(&error);
                BrokerResponse::Err {
                    error: error.into(),
                }
            }
        },
        BrokerRequest::ResizeMachine { machine_type } => {
            match state.api.resize_machine(&machine_type).await {
                Ok(()) => BrokerResponse::Ok,
                Err(error) => {
                    let _ = state.rate_limit.record_error(&error);
                    BrokerResponse::Err {
                        error: error.into(),
                    }
                }
            }
        }
        BrokerRequest::AgentBudget => match state.api.agent_budget().await {
            Ok(view) => BrokerResponse::Budget { view },
            Err(error) => {
                let _ = state.rate_limit.record_error(&error);
                BrokerResponse::Err {
                    error: error.into(),
                }
            }
        },
        BrokerRequest::RaiseApproval { payload } => match state.api.raise_approval(payload).await {
            Ok(id) => BrokerResponse::Approval { id },
            Err(error) => {
                let _ = state.rate_limit.record_error(&error);
                BrokerResponse::Err {
                    error: error.into(),
                }
            }
        },
    }
}

fn rate_limit_to_broker_error(status: &RateLimitStatus) -> BrokerError {
    match status {
        RateLimitStatus::CoolingDown { remaining_secs } => BrokerError::Refused {
            method: "REQUEST".to_owned(),
            path: "broker".to_owned(),
            status: 429,
            title: "Too Many Requests".to_owned(),
            detail: format!("local rate-limit cooldown active for {remaining_secs}s"),
            problem_kind: "rate-limited".to_owned(),
            retry_after_secs: Some(u32::try_from(*remaining_secs).unwrap_or(u32::MAX)),
        },
        RateLimitStatus::BudgetExhausted {
            kind,
            remaining_secs,
            detail,
        } => BrokerError::Refused {
            method: "REQUEST".to_owned(),
            path: "broker".to_owned(),
            status: 429,
            title: "Request Budget Exhausted".to_owned(),
            detail: detail.clone(),
            problem_kind: kind.clone(),
            retry_after_secs: Some(u32::try_from(*remaining_secs).unwrap_or(u32::MAX)),
        },
    }
}

/// Client used by `flycod mcp` to talk to `flycod run`'s Unix-socket broker.
#[derive(Debug, Clone)]
pub struct BrokerClient {
    socket: PathBuf,
}

impl BrokerClient {
    /// Creates a client targeting `socket`.
    #[must_use]
    pub const fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Fetches the session's non-secret bootstrap metadata from `flycod run`.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the broker socket is unreachable or
    /// returns an unexpected reply.
    pub async fn bootstrap(&self) -> Result<McpBootstrap, ControlApiError> {
        match self.ask(&BrokerRequest::Bootstrap).await? {
            BrokerResponse::Bootstrap { bootstrap } => Ok(bootstrap),
            BrokerResponse::Err { error } => Err(error.into()),
            other => Err(ControlApiError::Transport(format!(
                "unexpected broker reply to bootstrap: {other:?}"
            ))),
        }
    }

    async fn ask(&self, request: &BrokerRequest) -> Result<BrokerResponse, ControlApiError> {
        let stream = tokio::time::timeout(CLIENT_TIMEOUT, UnixStream::connect(&self.socket))
            .await
            .map_err(|_| ControlApiError::Transport("MCP broker connect timed out".to_owned()))?
            .map_err(|error| {
                ControlApiError::Transport(format!(
                    "could not connect to MCP broker at {}: {error}",
                    self.socket.display()
                ))
            })?;
        let (reader, mut writer) = stream.into_split();
        let mut encoded = serde_json::to_vec(request)
            .map_err(|error| ControlApiError::Transport(format!("encode request: {error}")))?;
        encoded.push(b'\n');
        tokio::time::timeout(CLIENT_TIMEOUT, writer.write_all(&encoded))
            .await
            .map_err(|_| ControlApiError::Transport("MCP broker write timed out".to_owned()))?
            .map_err(|error| ControlApiError::Transport(format!("MCP broker write: {error}")))?;

        let mut line = String::new();
        let read = tokio::time::timeout(
            CLIENT_TIMEOUT,
            BufReader::new(reader.take(MAX_LINE_BYTES)).read_line(&mut line),
        )
        .await
        .map_err(|_| ControlApiError::Transport("MCP broker reply timed out".to_owned()))?
        .map_err(|error| ControlApiError::Transport(format!("MCP broker read: {error}")))?;
        if read == 0 {
            return Err(ControlApiError::Transport(
                "MCP broker closed the connection without replying".to_owned(),
            ));
        }
        serde_json::from_str::<BrokerResponse>(&line)
            .map_err(|error| ControlApiError::Transport(format!("invalid broker reply: {error}")))
    }
}

impl ApprovalRaiser for BrokerClient {
    async fn raise_approval(
        &self,
        payload: ApprovalPayload,
    ) -> Result<ApprovalId, ControlApiError> {
        match self.ask(&BrokerRequest::RaiseApproval { payload }).await? {
            BrokerResponse::Approval { id } => Ok(id),
            BrokerResponse::Err { error } => Err(error.into()),
            other => Err(ControlApiError::Transport(format!(
                "unexpected broker reply to raise_approval: {other:?}"
            ))),
        }
    }
}

impl AgentApi for BrokerClient {
    async fn agent_machine(&self) -> Result<AgentMachineView, ControlApiError> {
        match self.ask(&BrokerRequest::AgentMachine).await? {
            BrokerResponse::Machine { view } => Ok(view),
            BrokerResponse::Err { error } => Err(error.into()),
            other => Err(ControlApiError::Transport(format!(
                "unexpected broker reply to agent_machine: {other:?}"
            ))),
        }
    }

    async fn agent_machine_catalog(&self) -> Result<Vec<MachineCatalogEntry>, ControlApiError> {
        match self.ask(&BrokerRequest::AgentMachineCatalog).await? {
            BrokerResponse::Catalog { entries } => Ok(entries),
            BrokerResponse::Err { error } => Err(error.into()),
            other => Err(ControlApiError::Transport(format!(
                "unexpected broker reply to agent_machine_catalog: {other:?}"
            ))),
        }
    }

    async fn resize_machine(&self, machine_type: &str) -> Result<(), ControlApiError> {
        match self
            .ask(&BrokerRequest::ResizeMachine {
                machine_type: machine_type.to_owned(),
            })
            .await?
        {
            BrokerResponse::Ok => Ok(()),
            BrokerResponse::Err { error } => Err(error.into()),
            other => Err(ControlApiError::Transport(format!(
                "unexpected broker reply to resize_machine: {other:?}"
            ))),
        }
    }

    async fn agent_budget(&self) -> Result<BudgetView, ControlApiError> {
        match self.ask(&BrokerRequest::AgentBudget).await? {
            BrokerResponse::Budget { view } => Ok(view),
            BrokerResponse::Err { error } => Err(error.into()),
            other => Err(ControlApiError::Transport(format!(
                "unexpected broker reply to agent_budget: {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use flyco_core::wire::ApprovalPayload;
    use flyco_core::{
        AgentMachineView, ApprovalId, BudgetStage, BudgetView, MachineCapacity,
        MachineCatalogEntry, MachineOrigin, MachineState, SessionId, SessionMachine, Usd,
    };

    use super::{BrokerClient, McpBootstrap, RateLimitGuard, RateLimitStatus, spawn};
    use crate::control::rest::{AgentApi, ApprovalRaiser, ControlApiError};

    #[derive(Clone)]
    struct MockApi {
        calls: Arc<AtomicUsize>,
        fail_with_429: Arc<std::sync::atomic::AtomicBool>,
        budget_exhausted: bool,
    }

    impl ApprovalRaiser for MockApi {
        fn raise_approval(
            &self,
            _payload: ApprovalPayload,
        ) -> impl core::future::Future<Output = Result<ApprovalId, ControlApiError>> + Send
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            core::future::ready(Ok(ApprovalId::generate()))
        }
    }

    impl AgentApi for MockApi {
        fn agent_machine(
            &self,
        ) -> impl core::future::Future<Output = Result<AgentMachineView, ControlApiError>> + Send
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_with_429.load(Ordering::SeqCst) {
                return core::future::ready(Err(ControlApiError::Refused {
                    method: "GET",
                    path: "/v1/sessions/s/agent/machine".to_owned(),
                    status: 429,
                    title: "Too Many Requests".to_owned(),
                    detail: if self.budget_exhausted {
                        "daily limit reached".to_owned()
                    } else {
                        "minute rate limit".to_owned()
                    },
                    kind: if self.budget_exhausted {
                        "https://flyco.dev/problems/request-budget-exhausted".to_owned()
                    } else {
                        "https://flyco.dev/problems/rate-limited".to_owned()
                    },
                    retry_after_secs: NonZeroU32::new(if self.budget_exhausted {
                        3600
                    } else {
                        45
                    }),
                }));
            }
            core::future::ready(Ok(AgentMachineView {
                origin: MachineOrigin::Auto,
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
            }))
        }

        fn agent_machine_catalog(
            &self,
        ) -> impl core::future::Future<Output = Result<Vec<MachineCatalogEntry>, ControlApiError>> + Send
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            core::future::ready(Ok(Vec::new()))
        }

        fn resize_machine(
            &self,
            _machine_type: &str,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            core::future::ready(Ok(()))
        }

        fn agent_budget(
            &self,
        ) -> impl core::future::Future<Output = Result<BudgetView, ControlApiError>> + Send
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            core::future::ready(Ok(BudgetView {
                limit: Usd::from_dollars(10),
                spent: Usd::from_dollars(1),
                remaining: Usd::from_dollars(9),
                stage: BudgetStage::Ok,
            }))
        }
    }

    #[tokio::test]
    async fn broker_proxies_bootstrap_and_agent_api_over_unix_socket() {
        let dir = std::env::temp_dir().join(format!("fb-{}", rand::random::<u32>()));
        let socket = dir.join("mcp.sock");
        let bootstrap = McpBootstrap {
            session: SessionId::generate(),
            workdir: PathBuf::from("/srv/flyco/work"),
            machine_origin: MachineOrigin::User,
            desktop_socket: dir.join("desktop.sock"),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let _handle = spawn(
            socket.clone(),
            bootstrap.clone(),
            MockApi {
                calls: Arc::clone(&calls),
                fail_with_429: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                budget_exhausted: false,
            },
            None,
        )
        .expect("broker binds");

        let client = BrokerClient::new(socket);
        assert_eq!(client.bootstrap().await.expect("bootstrap"), bootstrap);
        let machine = client.agent_machine().await.expect("machine");
        assert_eq!(machine.machine.machine_type, "m7i.xlarge");
        let budget = client.agent_budget().await.expect("budget");
        assert_eq!(budget.remaining, Usd::from_dollars(9));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn broker_preserves_429_and_enforces_local_cooldown() {
        let dir = std::env::temp_dir().join(format!("fb429-{}", rand::random::<u32>()));
        let socket = dir.join("mcp.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let _handle = spawn(
            socket.clone(),
            McpBootstrap {
                session: SessionId::generate(),
                workdir: PathBuf::from("/srv/flyco/work"),
                machine_origin: MachineOrigin::Auto,
                desktop_socket: dir.join("desktop.sock"),
            },
            MockApi {
                calls: Arc::clone(&calls),
                fail_with_429: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                budget_exhausted: true,
            },
            None,
        )
        .expect("broker binds");

        let client = BrokerClient::new(socket);
        let first = client.agent_machine().await.expect_err("first 429");
        assert_eq!(first.kind(), Some("request-budget-exhausted"));
        assert_eq!(first.retry_after(), Some(Duration::from_secs(3600)));

        // Second call hits the broker's local cooldown without calling upstream again.
        let second = client
            .agent_machine()
            .await
            .expect_err("second blocked locally");
        assert_eq!(second.kind(), Some("request-budget-exhausted"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "broker must not forward requests while cooldown is active"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rate_limit_guard_tracks_expiry_and_budget_exhaustion() {
        let guard = RateLimitGuard::new();
        let t0 = Instant::now();
        let err = ControlApiError::Refused {
            method: "GET",
            path: "/agent/machine".to_owned(),
            status: 429,
            title: "Too Many Requests".to_owned(),
            detail: "slow down".to_owned(),
            kind: "https://flyco.dev/problems/rate-limited".to_owned(),
            retry_after_secs: NonZeroU32::new(30),
        };

        let recorded = guard
            .record_error_at(&err, t0)
            .expect("429 records cooldown");
        assert!(matches!(
            recorded,
            RateLimitStatus::CoolingDown { remaining_secs: 30 }
        ));
        assert!(guard.check_at(t0 + Duration::from_secs(10)).is_some());
        assert!(guard.check_at(t0 + Duration::from_secs(31)).is_none());
    }
}
