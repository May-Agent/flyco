//! The unified ACP driver — Claude Code, Codex, Devin, and any external ACP
//! agent.
//!
//! Claude Code and Codex run in-process on [`aither_acp`]'s zero-serde direct
//! lane via [`claude_acp::ClaudeAgent::into_server`] and
//! [`codex_acp::CodexAgent::into_server`], spawning their native CLIs under
//! [`AgentUser`] privilege isolation through custom connectors in
//! [`transport`]. External ACP agents such as Devin run as child processes over
//! newline-delimited JSON-RPC.
//!
//! # Mounting
//!
//! MCP servers reach the agent on the session-lifecycle request itself (or
//! through Claude Code's managed policy directory when configured). Proving it
//! differs by agent: where [`AcpMethods::mcp_status`] names an introspection
//! method the driver asks with bounded pagination and deadline and refuses a
//! session without flyco's tools; where it does not, [`observe_mount`] watches
//! the agent's notifications for a bounded window and fails on evidence of
//! failure.

pub mod normalize;
mod transport;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aither_acp::SessionEvent;
use aither_acp::client::{Client, ClientError, ClientSession, SessionEvents, TurnOutcome};
use aither_acp::domain::{
    CancelledOutcome, ConfigValue, ConfigValueById, ExtMethod, Implementation, McpServer, Open,
    PermissionOption, PermissionOptionKind, PermissionOutcome, PermissionRequest,
    PermissionSubject, SelectedOutcome, SessionId,
};
use aither_acp::rpc::Notification;
use claude_acp::{ClaudeAcpConfig, ClaudeAgent, SessionConnector};
use claude_acp_sdk::{AnyTranscriptSync, SpawnConfig as ClaudeSpawnConfig, TranscriptSync};
use codex_acp::{AnyAppServerConnector, AppServerConnector, CodexAcpConfig, CodexAgent};
use codex_sdk::SpawnConfig as CodexSpawnConfig;
use flyco_core::{ApprovalId, ContextUsage, HarnessEvent, ModelChoice, PermissionMode};
use futures_lite::StreamExt as _;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use self::normalize::Normalizer;
use self::transport::{ClaudeProcessConnector, CodexProcessConnector, TokioChildTransport};
use super::{Harness, HarnessSession, SessionOutput, StartRequest, Started, ToolApproval};
use crate::config::{
    AcpConfig, AcpMethodCall, AcpMethods, AcpMode, ClaudeAuth, ClaudeConfig, InProcessAgent,
};
use crate::mount::{Mount, MountState, MountedServer};
use crate::privilege::{AgentUser, chown_path};

/// Capacity of the driver's command and output channels.
const CHANNEL_DEPTH: usize = 256;

/// How long a clean shutdown may take before the driver gives up on it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// How long the driver waits for an agent's MCP servers to stop dialling.
const MOUNT_SETTLE: Duration = Duration::from_secs(10);

/// How often a still-dialling mount is re-asked.
const MOUNT_POLL: Duration = Duration::from_millis(250);

/// Hard ceiling on cursor pages walked during a single MCP mount query.
const MAX_MOUNT_PAGES: usize = 32;

/// How long [`observe_mount`] watches a mount-reportless agent for evidence.
const MOUNT_OBSERVE: Duration = Duration::from_secs(15);

/// How long a best-effort `_claude/context_usage` query may take before the
/// driver falls back to the cached window summary.
const CONTEXT_USAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// The unified ACP driver failed.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// An `[acp.files]` entry or managed policy file could not be materialized.
    #[error("could not write the agent's file at {path}")]
    File {
        /// Where the write was aimed.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The session working directory could not be created.
    #[error("could not create the session workdir at {path}")]
    Workdir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The agent process could not be spawned.
    #[error("could not run the ACP agent `{program}`: {detail}")]
    Spawn {
        /// The configured program.
        program: String,
        /// Why the spawn failed.
        detail: String,
    },
    /// A request to the agent failed.
    #[error("the ACP agent {context}")]
    Agent {
        /// What was being asked.
        context: String,
        /// The failure.
        #[source]
        source: ClientError,
    },
    /// A configured extension method name does not start with `_`.
    #[error("extension method `{method}` must start with `_`")]
    InvalidMethod {
        /// The configured method name.
        method: String,
    },
    /// The agent closed its connection or exited.
    #[error("the ACP agent's connection closed")]
    Closed,
    /// A registered MCP server needs a transport the agent does not speak.
    #[error(
        "the MCP server `{server}` is reached over HTTP, which the configured agent's \
         capabilities say it cannot speak"
    )]
    UnsupportedTransport {
        /// The registered server.
        server: String,
    },
    /// A permission mode was asked for that the `[acp.modes]` table does
    /// not express.
    #[error(
        "permission mode `{mode:?}` has no `[acp.modes]` entry for this agent — it is refused \
         rather than approximated"
    )]
    ModeUnsupported {
        /// The mode that cannot be expressed.
        mode: PermissionMode,
    },
    /// An effort was configured on an agent with no effort option.
    #[error(
        "an effort was configured but this agent's `[acp]` names no `effort_option` to carry it"
    )]
    EffortUnsupported,
    /// A feature was asked for that the `[acp.methods]` table has no
    /// method for.
    #[error("this agent's `[acp.methods]` names no method for {feature}")]
    NoMethod {
        /// What was asked for.
        feature: &'static str,
    },
    /// The session's task is gone, so no command can be delivered.
    #[error("the ACP session has stopped")]
    Stopped,
    /// A second prompt or compaction was asked for while a turn was still running.
    #[error("a turn is already in flight")]
    TurnInFlight,
    /// The agent's mount report could not be read as a server list.
    #[error("the agent's mount report was not a readable server list: {detail}")]
    MountReport {
        /// What the answer looked like.
        detail: String,
    },
    /// The mount check refused the session.
    #[error(transparent)]
    Mount(#[from] crate::mount::NotMounted),
    /// An approval decision arrived for a request nobody is waiting on.
    #[error("no agent permission request is waiting on {0}")]
    UnknownApproval(ApprovalId),
}

/// Optional custom Claude connector erased behind a closure for tests.
type ClaudeConnectorOverride =
    Arc<dyn Fn(ClaudeAcpConfig) -> ClaudeAcpConfig + Send + Sync + 'static>;

/// Which backend [`AcpHarness`] drives.
enum Backend {
    /// Generic external ACP subprocess or in-process Codex server configured
    /// via `[acp]`.
    Acp {
        config: Box<AcpConfig>,
        codex_connector: Option<AnyAppServerConnector>,
    },
    /// In-process Claude Code server configured via `[claude]`.
    Claude {
        config: Box<ClaudeConfig>,
        transcript_dir: PathBuf,
        transcript_sync: AnyTranscriptSync,
        connector: Option<ClaudeConnectorOverride>,
    },
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acp { config, .. } => f
                .debug_struct("Backend::Acp")
                .field("agent", &config.agent)
                .field("in_process", &config.in_process)
                .finish_non_exhaustive(),
            Self::Claude {
                config,
                transcript_dir,
                ..
            } => f
                .debug_struct("Backend::Claude")
                .field("config", config)
                .field("transcript_dir", transcript_dir)
                .finish_non_exhaustive(),
        }
    }
}

/// A configured, not-yet-started ACP harness.
#[derive(Debug)]
pub struct AcpHarness {
    backend: Backend,
    mount: Mount,
    agent: Option<AgentUser>,
    swarm_root: Option<PathBuf>,
    swarm_timings: crate::swarm::WatchTimings,
}

impl AcpHarness {
    /// Builds a harness from an `[acp]` configuration (external subprocess or
    /// in-process Codex) and the MCP servers the session may reach.
    #[must_use]
    pub fn new(config: AcpConfig, mount: Mount) -> Self {
        Self {
            backend: Backend::Acp {
                config: Box::new(config),
                codex_connector: None,
            },
            mount,
            agent: None,
            swarm_root: None,
            swarm_timings: crate::swarm::WatchTimings::default(),
        }
    }

    /// Builds an in-process Claude Code harness hosted via `claude-acp` and
    /// synchronized against `transcript_sync`.
    #[must_use]
    pub fn claude<S: TranscriptSync>(
        config: ClaudeConfig,
        mount: Mount,
        transcript_dir: PathBuf,
        transcript_sync: S,
    ) -> Self {
        Self {
            backend: Backend::Claude {
                config: Box::new(config),
                transcript_dir,
                transcript_sync: AnyTranscriptSync::new(transcript_sync),
                connector: None,
            },
            mount,
            agent: None,
            swarm_root: None,
            swarm_timings: crate::swarm::WatchTimings::default(),
        }
    }

    /// Configures the unprivileged agent user the harness spawns its native
    /// CLI processes as.
    #[must_use]
    pub fn with_agent(mut self, agent: Option<AgentUser>) -> Self {
        self.agent = agent;
        self
    }

    /// Overrides the Antigravity app-data root watched read-only for swarm
    /// files (`<root>/brain/<session_id>/.system_generated/swarm`).
    #[must_use]
    pub fn with_swarm_root(mut self, root: PathBuf) -> Self {
        self.swarm_root = Some(root);
        self
    }

    /// Overrides poll and coalescing timings for the read-only swarm watcher.
    #[must_use]
    pub const fn with_swarm_timings(mut self, timings: crate::swarm::WatchTimings) -> Self {
        self.swarm_timings = timings;
        self
    }

    /// Overrides the `claude-acp` [`SessionConnector`] (used by tests to drive
    /// an in-memory mock Claude CLI).
    #[must_use]
    pub fn with_claude_connector<C: SessionConnector + Clone>(mut self, connector: C) -> Self {
        if let Backend::Claude {
            connector: slot, ..
        } = &mut self.backend
        {
            *slot = Some(Arc::new(move |cfg| cfg.connector(connector.clone())));
        }
        self
    }

    /// Overrides the `codex-acp` [`AppServerConnector`] (used by tests to drive
    /// an in-memory mock `codex app-server`).
    #[must_use]
    pub fn with_codex_connector<C: AppServerConnector>(mut self, connector: C) -> Self {
        if let Backend::Acp {
            codex_connector: slot,
            ..
        } = &mut self.backend
        {
            *slot = Some(AnyAppServerConnector::new(connector));
        }
        self
    }
}

/// Optional Claude-specific transcript sync context held by [`Driver`] so
/// `DriverCommand::Flush` pushes pending JSONL bytes before spot/SIGTERM exit.
struct ClaudeSyncContext {
    sync: AnyTranscriptSync,
    workdir: PathBuf,
    transcript_home: PathBuf,
}

impl Harness for AcpHarness {
    type Session = AcpSession;
    type Error = AcpError;

    #[expect(
        clippy::too_many_lines,
        reason = "startup orchestrates connect, session open, mode/model setup, mount proof, and driver spawn in one sequence"
    )]
    async fn start(self, request: StartRequest) -> Result<Started<Self::Session>, AcpError> {
        let agent = self.agent;
        let mount = self.mount;
        tokio::fs::create_dir_all(&request.workdir)
            .await
            .map_err(|source| AcpError::Workdir {
                path: request.workdir.clone(),
                source,
            })?;

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();

        let (
            config,
            connected,
            pass_mcp_to_session,
            context_usage_method,
            claude_compact,
            claude_sync,
        ) = match self.backend {
            Backend::Acp {
                config,
                codex_connector,
            } => {
                let config = *config;
                materialize_files(&config, agent.as_ref()).await?;
                let connected = connect_acp(
                    &config,
                    &request,
                    events_tx.clone(),
                    &mount,
                    agent.as_ref(),
                    codex_connector,
                )
                .await?;
                let pass_mcp_to_session = !config.mcp_via_files;
                (config, connected, pass_mcp_to_session, None, false, None)
            }
            Backend::Claude {
                config: claude_cfg,
                transcript_dir,
                transcript_sync,
                connector,
            } => {
                let (acp_cfg, connected, pass_mcp, transcript_home) = connect_claude(
                    &claude_cfg,
                    &transcript_dir,
                    transcript_sync.clone(),
                    connector,
                    &request,
                    events_tx.clone(),
                    &mount,
                    agent.as_ref(),
                )
                .await?;
                let context_usage_method = Some(AcpMethodCall {
                    call: claude_acp::EXT_CONTEXT_USAGE.to_owned(),
                    params: json!({ "sessionId": "<session>" }),
                });
                let sync_ctx = ClaudeSyncContext {
                    sync: transcript_sync,
                    workdir: request.workdir.clone(),
                    transcript_home,
                };
                (
                    acp_cfg,
                    connected,
                    pass_mcp,
                    context_usage_method,
                    true,
                    Some(sync_ctx),
                )
            }
        };

        let mcp_servers = if pass_mcp_to_session {
            mount.acp_servers()
        } else {
            Vec::new()
        };

        let mut opened =
            match open_session(&connected.client, &config, &mcp_servers, &request).await {
                Ok(opened) => opened,
                Err(error) => {
                    connected.connection_task.abort();
                    return Err(error);
                }
            };
        let session = opened.session.clone();
        let session_id = opened.session_id.clone();

        if let Err(error) = apply_permission_mode(&session, config.permission_mode, &config).await {
            connected.connection_task.abort();
            return Err(error);
        }
        if let Err(error) = apply_model(&session, &config).await {
            connected.connection_task.abort();
            return Err(error);
        }

        // Prove or observe the mount, exactly once, before the session can
        // produce anything.
        let mut prelude = Vec::new();
        let mount_outcome = match &config.methods.mcp_status {
            Some(method) => verify_mount(&connected.client, &session_id, method, &mount).await,
            None => observe_mount(&mut events_rx, &mut prelude).await,
        };
        if let Err(error) = mount_outcome {
            connected.connection_task.abort();
            return Err(error);
        }

        // Subscribe after startup setup and mount verification so the snapshot
        // includes any early `available_commands_update` / `config_option_update`
        // notifications without duplicating them in the live event stream.
        let session_events = session.subscribe();
        refresh_opened_from_state(&mut opened, &session_events.snapshot);

        let swarm_dir = match self.swarm_root {
            Some(root) => crate::swarm::swarm_dir_for_session(&root, &session_id),
            None if config.agent == "antigravity" => crate::swarm::resolve_antigravity_swarm_dir(
                &config.args,
                agent.as_ref().map(|u| u.home.as_path()),
                &session_id,
            ),
            None => None,
        };

        let (commands, inbox) = mpsc::channel(CHANNEL_DEPTH);
        let (outputs, output_rx) = mpsc::channel(CHANNEL_DEPTH);
        tokio::spawn(
            Driver {
                client: connected.client,
                session,
                connection_task: connected.connection_task,
                session_id,
                config,
                context_usage_method,
                claude_compact,
                claude_sync,
                swarm_dir,
                swarm_timings: self.swarm_timings,
                swarm_task: None,
                normalizer: Normalizer::default(),
                outputs,
                events: events_tx,
                turn_seq: 0,
                active_turn: None,
                queued_messages: VecDeque::new(),
                pending_permissions: BTreeMap::new(),
                pending_calls: BTreeMap::new(),
                call_seq: 0,
                close_supported: opened.close_supported,
                stopped: false,
                mode: opened.mode.clone(),
                model: opened.model.clone(),
            }
            .run(inbox, events_rx, session_events, prelude, opened),
        );

        Ok(Started {
            session: AcpSession { commands },
            outputs: output_rx,
        })
    }
}

/// A connected, handshaken ACP client paired with its connection driver task.
struct Connected {
    client: Client,
    connection_task: tokio::task::JoinHandle<()>,
}

/// Maps a flyco [`PermissionMode`] to `claude-acp`'s canonical mode identifier.
const fn claude_mode_id(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "agent",
        PermissionMode::AcceptEdits => "workspace-write",
        PermissionMode::Plan => "read-only",
        PermissionMode::Auto => "auto",
        PermissionMode::DontAsk => "dont-ask",
        PermissionMode::BypassPermissions => "agent-full-access",
    }
}

/// Builds the `[acp]` view that drives an in-process Claude Code session.
fn claude_acp_driver_config(claude: &ClaudeConfig) -> AcpConfig {
    let modes = [
        PermissionMode::Default,
        PermissionMode::AcceptEdits,
        PermissionMode::Plan,
        PermissionMode::Auto,
        PermissionMode::DontAsk,
        PermissionMode::BypassPermissions,
    ]
    .into_iter()
    .map(|mode| {
        (
            mode,
            AcpMode {
                set_mode: Some(claude_mode_id(mode).to_owned()),
                options: Vec::new(),
            },
        )
    })
    .collect();

    AcpConfig {
        agent: "claude".to_owned(),
        in_process: None,
        program: claude
            .claude_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(flyco_provider::flycod::CLAUDE_BIN)),
        args: Vec::new(),
        auth_method: None,
        mcp_via_files: false,
        env: BTreeMap::new(),
        files: Vec::new(),
        model: claude.model.clone(),
        effort: claude.effort.clone(),
        model_option: "model".to_owned(),
        effort_option: Some("reasoning_effort".to_owned()),
        permission_mode: claude.permission_mode,
        modes,
        methods: AcpMethods {
            compact: None,
            usage: Some(AcpMethodCall {
                call: claude_acp::EXT_RATE_LIMITS.to_owned(),
                params: json!({ "sessionId": "<session>" }),
            }),
            mcp_status: Some(AcpMethodCall {
                call: claude_acp::EXT_MCP_STATUS.to_owned(),
                params: json!({ "sessionId": "<session>" }),
            }),
        },
        tui: None,
    }
}

/// Prepares a transcript home directory whose `.claude` entry points at
/// `isolation_config_dir` so `claude-acp-sdk`'s [`TranscriptStore`] shares the
/// exact directory tree `CLAUDE_CONFIG_DIR` writes into.
async fn prepare_claude_transcript_home(
    transcript_dir: &Path,
    isolation_config_dir: &Path,
    agent: Option<&AgentUser>,
) -> Result<PathBuf, AcpError> {
    let home = transcript_dir.join("claude-home");
    tokio::fs::create_dir_all(&home)
        .await
        .map_err(|source| AcpError::File {
            path: home.clone(),
            source,
        })?;
    tokio::fs::create_dir_all(isolation_config_dir)
        .await
        .map_err(|source| AcpError::File {
            path: isolation_config_dir.to_path_buf(),
            source,
        })?;
    if let Some(user) = agent
        && user.needs_uid_switch()
    {
        chown_path(isolation_config_dir, Some(user.uid), Some(user.gid)).map_err(|error| {
            AcpError::File {
                path: isolation_config_dir.to_path_buf(),
                source: std::io::Error::other(error),
            }
        })?;
    }
    let link = home.join(".claude");
    match tokio::fs::symlink_metadata(&link).await {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                let _ = tokio::fs::remove_file(&link).await;
            } else {
                return Err(AcpError::File {
                    path: link,
                    source: std::io::Error::other(
                        "expected `.claude` under transcript home to be a symlink",
                    ),
                });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(AcpError::File { path: link, source }),
    }
    tokio::fs::symlink(isolation_config_dir, &link)
        .await
        .map_err(|source| AcpError::File { path: link, source })?;
    Ok(home)
}

/// Builds a [`aither_acp::ClientBuilder`] wired with flycod's identity,
/// permission handler, and vendor notification forwarder.
fn client_builder(events: mpsc::UnboundedSender<AgentEvent>) -> aither_acp::ClientBuilder {
    let perm_tx = events.clone();
    let notif_tx = events;
    Client::builder(Implementation::new("flycod", env!("CARGO_PKG_VERSION")))
        .permissions(move |request: PermissionRequest| {
            let tx = perm_tx.clone();
            async move {
                let (respond, answer) = oneshot::channel();
                if tx
                    .send(AgentEvent::Permission {
                        request: Box::new(request),
                        respond,
                    })
                    .is_err()
                {
                    return PermissionOutcome::Cancelled(CancelledOutcome { meta: None });
                }
                answer
                    .await
                    .unwrap_or(PermissionOutcome::Cancelled(CancelledOutcome {
                        meta: None,
                    }))
            }
        })
        .notification(move |notification: Notification| {
            let tx = notif_tx.clone();
            async move {
                let _ = tx.send(AgentEvent::Vendor(notification));
            }
        })
}

/// Verifies that every HTTP MCP server in `servers` is supported by the
/// negotiated agent capabilities.
fn verify_mcp_transports(client: &Client, servers: &[McpServer]) -> Result<(), AcpError> {
    let http_supported = client
        .agent_capabilities()
        .session
        .as_ref()
        .and_then(|s| s.mcp.as_ref())
        .is_some_and(|m| m.http.is_some());
    for server in servers {
        if let McpServer::Http(http) = server
            && !http_supported
        {
            return Err(AcpError::UnsupportedTransport {
                server: http.name.clone(),
            });
        }
    }
    Ok(())
}

/// Connects an in-process `ClaudeAgent` hosted via `ClientBuilder::in_process_server`.
#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "assembles Claude managed policy, auth isolation, transcript home, and in-process ACP server"
)]
async fn connect_claude(
    claude: &ClaudeConfig,
    transcript_dir: &Path,
    transcript_sync: AnyTranscriptSync,
    connector_override: Option<ClaudeConnectorOverride>,
    request: &StartRequest,
    events: mpsc::UnboundedSender<AgentEvent>,
    mount: &Mount,
    agent: Option<&AgentUser>,
) -> Result<(AcpConfig, Connected, bool, PathBuf), AcpError> {
    let pass_mcp_to_session = if let Some(managed_dir) = &claude.managed_dir {
        mount
            .write_claude_managed(managed_dir)
            .await
            .map_err(|error| AcpError::File {
                path: managed_dir.clone(),
                source: std::io::Error::other(error),
            })?;
        false
    } else {
        true
    };

    let mut spawn_cfg = ClaudeSpawnConfig::new()
        .cwd(request.workdir.clone())
        .include_partial_messages(true);
    if let Some(path) = &claude.claude_path {
        spawn_cfg = spawn_cfg.claude_path(path.clone());
    } else if Path::new(flyco_provider::flycod::CLAUDE_BIN).is_file() {
        spawn_cfg = spawn_cfg.claude_path(flyco_provider::flycod::CLAUDE_BIN);
    }
    if let Some(model) = &claude.model {
        spawn_cfg = spawn_cfg.model(model.clone());
    }
    if pass_mcp_to_session {
        spawn_cfg.extra_args.push("--strict-mcp-config".to_owned());
    }

    let transcript_home = match &claude.auth {
        ClaudeAuth::OauthToken { token, isolation } => {
            let dir = isolation.config_dir.clone();
            let token_owned = token.clone();
            let agent_owned = agent.cloned();
            tokio::task::spawn_blocking(move || {
                crate::tui::provision_claude_oauth(&dir, &token_owned, agent_owned.as_ref())
            })
            .await
            .map_err(|error| AcpError::File {
                path: isolation.config_dir.clone(),
                source: std::io::Error::other(error),
            })?
            .map_err(|source| AcpError::File {
                path: isolation.config_dir.clone(),
                source,
            })?;
            spawn_cfg.env.insert(
                "CLAUDE_CONFIG_DIR".to_owned(),
                isolation.config_dir.to_string_lossy().into_owned(),
            );
            spawn_cfg.env.insert(
                "CLAUDE_CODE_PROJECT_DIR_NAME".to_owned(),
                isolation.project_dir_name.clone(),
            );
            prepare_claude_transcript_home(transcript_dir, &isolation.config_dir, agent).await?
        }
        ClaudeAuth::ApiKey { key, isolation } => {
            spawn_cfg
                .env
                .insert("ANTHROPIC_API_KEY".to_owned(), key.clone());
            spawn_cfg.env.insert(
                "CLAUDE_CONFIG_DIR".to_owned(),
                isolation.config_dir.to_string_lossy().into_owned(),
            );
            spawn_cfg.env.insert(
                "CLAUDE_CODE_PROJECT_DIR_NAME".to_owned(),
                isolation.project_dir_name.clone(),
            );
            prepare_claude_transcript_home(transcript_dir, &isolation.config_dir, agent).await?
        }
        ClaudeAuth::Inherit => agent
            .map(|u| u.home.clone())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| transcript_dir.to_path_buf()),
    };

    let mut host_cfg = ClaudeAcpConfig::new()
        .spawn(spawn_cfg)
        .default_mode(claude_mode_id(claude.permission_mode))
        .include_partial_messages(true)
        .transcript_home(transcript_home.clone())
        .connector(ClaudeProcessConnector::new(agent.cloned()));
    host_cfg.transcript_sync = transcript_sync;
    if let Some(apply) = connector_override {
        host_cfg = apply(host_cfg);
    }

    let claude_agent = ClaudeAgent::new(host_cfg);
    let shutdown_agent = claude_agent.clone();
    let server = claude_agent.into_server();

    let (client, connection) = client_builder(events.clone())
        .in_process_server(server)
        .await
        .map_err(|source| AcpError::Agent {
            context: "rejected `initialize`".to_owned(),
            source,
        })?;
    let closed = events;
    let connection_task = tokio::spawn(async move {
        connection.await;
        shutdown_agent.shutdown_all().await;
        let _ = closed.send(AgentEvent::Closed);
    });

    tracing::info!(
        agent = "claude",
        version = ?client.agent_info().map(|info| &info.version),
        "in-process Claude ACP host completed its handshake"
    );

    if pass_mcp_to_session && let Err(error) = verify_mcp_transports(&client, &mount.acp_servers())
    {
        connection_task.abort();
        return Err(error);
    }

    Ok((
        claude_acp_driver_config(claude),
        Connected {
            client,
            connection_task,
        },
        pass_mcp_to_session,
        transcript_home,
    ))
}

/// Connects either an in-process `CodexAgent` or an external ACP subprocess.
#[expect(
    clippy::too_many_lines,
    reason = "handles both in-process Codex and privilege-dropped subprocess ACP connection setup"
)]
async fn connect_acp(
    config: &AcpConfig,
    request: &StartRequest,
    events: mpsc::UnboundedSender<AgentEvent>,
    mount: &Mount,
    agent: Option<&AgentUser>,
    codex_connector: Option<AnyAppServerConnector>,
) -> Result<Connected, AcpError> {
    let builder = client_builder(events.clone());

    let (client, connection_task) = match config.in_process {
        Some(InProcessAgent::Codex) => {
            let mut spawn_cfg = CodexSpawnConfig::new()
                .codex_path(config.program.clone())
                .cwd(request.workdir.clone())
                .client_info("flycod", env!("CARGO_PKG_VERSION"))
                .experimental_api(true);
            spawn_cfg
                .env
                .extend(config.env.iter().map(|(k, v)| (k.clone(), v.clone())));
            let mut host_cfg = CodexAcpConfig::new().spawn(spawn_cfg);
            if let Some(mode_id) = config
                .modes
                .get(&config.permission_mode)
                .and_then(|m| m.set_mode.as_deref())
            {
                host_cfg = host_cfg.default_mode(mode_id);
            }
            host_cfg = match codex_connector {
                Some(override_connector) => host_cfg.connector(override_connector),
                None => host_cfg.connector(CodexProcessConnector::new(agent.cloned())),
            };

            let codex_agent = CodexAgent::new(host_cfg);
            let bg_pump = codex_agent.drive_background();
            let shutdown_agent = codex_agent.clone();
            let server = codex_agent.into_server();

            let (client, connection) =
                builder
                    .in_process_server(server)
                    .await
                    .map_err(|source| AcpError::Agent {
                        context: "rejected `initialize`".to_owned(),
                        source,
                    })?;
            let closed = events;
            let task = tokio::spawn(async move {
                futures_lite::future::or(connection, async move {
                    bg_pump.await;
                    std::future::pending::<()>().await;
                })
                .await;
                shutdown_agent.shutdown_all().await;
                let _ = closed.send(AgentEvent::Closed);
            });
            (client, task)
        }
        None => {
            let program = config.program.to_string_lossy().into_owned();
            let (client, connection) = if let Some(user) = agent {
                let transport = TokioChildTransport::spawn(
                    &program,
                    &config.args,
                    &config.env,
                    &request.workdir,
                    user,
                )
                .map_err(|error| AcpError::Spawn {
                    program: program.clone(),
                    detail: error.to_string(),
                })?;
                builder
                    .connect(transport)
                    .await
                    .map_err(|source| AcpError::Agent {
                        context: "rejected `initialize`".to_owned(),
                        source,
                    })?
            } else {
                let env: Vec<(String, String)> = config
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                builder
                    .spawn(&program, &config.args, &env, &request.workdir)
                    .await
                    .map_err(|source| match source {
                        ClientError::Transport(rpc) => AcpError::Spawn {
                            program: program.clone(),
                            detail: rpc.to_string(),
                        },
                        other => AcpError::Agent {
                            context: "rejected `initialize`".to_owned(),
                            source: other,
                        },
                    })?
            };
            let closed = events;
            let task = tokio::spawn(async move {
                connection.await;
                let _ = closed.send(AgentEvent::Closed);
            });
            (client, task)
        }
    };

    tracing::info!(
        agent = %config.agent,
        version = ?client.agent_info().map(|info| &info.version),
        protocol = client.protocol_version(),
        "the ACP agent completed its handshake"
    );

    if let Some(method) = &config.auth_method
        && let Err(source) = client.authenticate(method.as_str()).await
    {
        connection_task.abort();
        return Err(AcpError::Agent {
            context: format!("rejected `authenticate` ({method})"),
            source,
        });
    }

    if !config.mcp_via_files
        && let Err(error) = verify_mcp_transports(&client, &mount.acp_servers())
    {
        connection_task.abort();
        return Err(error);
    }
    Ok(Connected {
        client,
        connection_task,
    })
}

/// Checks a v1 session capability flag folded into
/// `agent_capabilities().session.extra` by `aither-acp`'s v1 codec.
fn v1_session_cap(client: &Client, key: &str) -> bool {
    client
        .agent_capabilities()
        .session
        .as_ref()
        .and_then(|s| s.extra.get(key))
        .is_some_and(|v| v.is_object() || v.as_bool().unwrap_or(false))
}

/// Whether the agent supports `additionalDirectories` on session lifecycle calls.
fn supports_additional_directories(client: &Client) -> bool {
    client.protocol_version() >= 2
        || client
            .agent_capabilities()
            .session
            .as_ref()
            .is_some_and(|s| s.additional_directories.is_some())
        || v1_session_cap(client, "additionalDirectories")
}

/// What a session-open call settled, before the driver task exists.
struct Opened {
    session: ClientSession,
    session_id: String,
    close_supported: bool,
    mode: Option<String>,
    model: Option<String>,
    capabilities: Vec<String>,
    models: Vec<flyco_core::ModelOption>,
    commands: Vec<flyco_core::HarnessCommand>,
    notice: Option<String>,
}

/// Opens the ACP session — new, resumed, or loaded.
async fn open_session(
    client: &Client,
    config: &AcpConfig,
    mcp_servers: &[McpServer],
    request: &StartRequest,
) -> Result<Opened, AcpError> {
    let is_v2 = client.protocol_version() >= 2;
    let can_resume = is_v2 || v1_session_cap(client, "resume");
    let can_load = is_v2 || v1_session_cap(client, "loadSession");
    let can_add_dirs = supports_additional_directories(client);

    let (session, notice) = match &request.resume_session_id {
        Some(id) if can_resume => {
            let mut builder = client
                .resume_session(SessionId::from(id.clone()), &request.workdir)
                .no_replay();
            for server in mcp_servers {
                builder = builder.mcp_server(server.clone());
            }
            if can_add_dirs && !request.additional_directories.is_empty() {
                builder =
                    builder.additional_directories(request.additional_directories.iter().cloned());
            }
            match builder.open().await {
                Ok(session) => (session, None),
                Err(source) => {
                    let fresh =
                        new_conversation(client, mcp_servers, request, can_add_dirs).await?;
                    (
                        fresh,
                        Some(restarted_fresh(config, "session/resume", &source)),
                    )
                }
            }
        }
        Some(id) if can_load => {
            let mut builder = client
                .resume_session(SessionId::from(id.clone()), &request.workdir)
                .replay_from_start();
            for server in mcp_servers {
                builder = builder.mcp_server(server.clone());
            }
            if can_add_dirs && !request.additional_directories.is_empty() {
                builder =
                    builder.additional_directories(request.additional_directories.iter().cloned());
            }
            match builder.open().await {
                Ok(session) => (session, None),
                Err(source) => {
                    let fresh =
                        new_conversation(client, mcp_servers, request, can_add_dirs).await?;
                    (
                        fresh,
                        Some(restarted_fresh(config, "session/load", &source)),
                    )
                }
            }
        }
        Some(_) => {
            let fresh = new_conversation(client, mcp_servers, request, can_add_dirs).await?;
            (fresh, Some(uncontinuable(config)))
        }
        None => {
            let fresh = new_conversation(client, mcp_servers, request, can_add_dirs).await?;
            (fresh, None)
        }
    };

    Ok(describe_opened(client, session, notice))
}

/// `session/new` — where a fresh conversation starts.
async fn new_conversation(
    client: &Client,
    mcp_servers: &[McpServer],
    request: &StartRequest,
    can_add_dirs: bool,
) -> Result<ClientSession, AcpError> {
    let mut builder = client
        .new_session(&request.workdir)
        .mcp_servers(mcp_servers.iter().cloned());
    if can_add_dirs && !request.additional_directories.is_empty() {
        builder = builder.additional_directories(request.additional_directories.iter().cloned());
    }
    builder.open().await.map_err(|source| AcpError::Agent {
        context: "rejected `session/new`".to_owned(),
        source,
    })
}

fn restarted_fresh(config: &AcpConfig, method: &str, source: &ClientError) -> String {
    tracing::warn!(agent = %config.agent, %source, "{method} was rejected; the session opens fresh");
    format!(
        "The previous conversation could not be continued — {} rejected `{method}` — so this \
         session started fresh. The workspace still holds all of its work.",
        config.agent
    )
}

fn uncontinuable(config: &AcpConfig) -> String {
    tracing::warn!(
        agent = %config.agent,
        "the agent supports neither session/resume nor session/load; the session opens fresh"
    );
    format!(
        "{} can neither resume nor load a stored conversation, so this session started fresh. \
         The workspace still holds all of the previous work.",
        config.agent
    )
}

/// Translates the opened [`ClientSession`] snapshot and negotiated capabilities
/// into an [`Opened`] descriptor.
fn describe_opened(client: &Client, session: ClientSession, notice: Option<String>) -> Opened {
    let is_v2 = client.protocol_version() >= 2;
    let caps = client.agent_capabilities();
    let state = session.state();

    let load_session = is_v2 || v1_session_cap(client, "loadSession");
    let resume = is_v2 || v1_session_cap(client, "resume");
    let close = is_v2 || v1_session_cap(client, "close");
    let list = is_v2 || v1_session_cap(client, "list");
    let additional_directories = supports_additional_directories(client);
    let mcp_http = caps
        .session
        .as_ref()
        .and_then(|s| s.mcp.as_ref())
        .is_some_and(|m| m.http.is_some());
    let has_modes = state.current_mode.is_some()
        || state
            .config_options
            .iter()
            .any(aither_acp::domain::ConfigOption::is_mode);

    let mut tokens = vec!["acp".to_owned()];
    for (supported, token) in [
        (load_session, "acp:load_session"),
        (resume, "acp:resume"),
        (close, "acp:close"),
        (list, "acp:list"),
        (additional_directories, "acp:additional_directories"),
        (has_modes, "acp:modes"),
        (!state.config_options.is_empty(), "acp:config_options"),
        (mcp_http, "acp:mcp_http"),
    ] {
        if supported {
            tokens.push(token.to_owned());
        }
    }

    let (model_list, current_model) = normalize::read_model_option(&state.config_options);
    let current_mode = state
        .current_mode
        .map(|id| id.as_str().to_owned())
        .or_else(|| {
            state
                .config_options
                .iter()
                .find(|o| o.is_mode())
                .and_then(|o| match &o.kind {
                    aither_acp::domain::ConfigKind::Select(select) => {
                        Some(select.current_value.as_str().to_owned())
                    }
                    _ => None,
                })
        });

    Opened {
        session_id: session.id().as_str().to_owned(),
        close_supported: close,
        mode: current_mode,
        model: current_model,
        capabilities: tokens,
        models: model_list.unwrap_or_default(),
        commands: normalize::convert_commands(&state.available_commands),
        notice,
        session,
    }
}

/// Refreshes [`Opened`] metadata from the folded [`aither_acp::SessionState`]
/// snapshot captured right after startup setup and mount verification.
fn refresh_opened_from_state(opened: &mut Opened, state: &aither_acp::SessionState) {
    let (model_list, current_model) = normalize::read_model_option(&state.config_options);
    if let Some(models) = model_list
        && !models.is_empty()
    {
        opened.models = models;
    }
    if current_model.is_some() {
        opened.model = current_model;
    }
    if let Some(mode) = state.current_mode.as_ref() {
        opened.mode = Some(mode.as_str().to_owned());
    }
    let commands = normalize::convert_commands(&state.available_commands);
    if !commands.is_empty() {
        opened.commands = commands;
    }
}

/// Applies the configured flyco [`PermissionMode`] to `session`.
async fn apply_permission_mode(
    session: &ClientSession,
    mode: PermissionMode,
    config: &AcpConfig,
) -> Result<(), AcpError> {
    let Some(mapping) = config.modes.get(&mode) else {
        return Err(AcpError::ModeUnsupported { mode });
    };
    if let Some(mode_id) = &mapping.set_mode {
        session
            .set_mode(mode_id.as_str())
            .await
            .map_err(|source| AcpError::Agent {
                context: format!("rejected `session/set_mode {mode_id}`"),
                source,
            })?;
    }
    for write in &mapping.options {
        session
            .set_config(
                write.option.as_str(),
                ConfigValue::Id(ConfigValueById {
                    value: write.value.as_str().into(),
                }),
            )
            .await
            .map_err(|source| AcpError::Agent {
                context: format!("rejected `session/set_config_option {}`", write.option),
                source,
            })?;
    }
    Ok(())
}

/// Selects the configured model and effort through the session's config options.
async fn apply_model(session: &ClientSession, config: &AcpConfig) -> Result<(), AcpError> {
    if let Some(model) = &config.model {
        session
            .set_config(
                config.model_option.as_str(),
                ConfigValue::Id(ConfigValueById {
                    value: model.as_str().into(),
                }),
            )
            .await
            .map_err(|source| AcpError::Agent {
                context: format!(
                    "rejected `session/set_config_option {}`",
                    config.model_option
                ),
                source,
            })?;
    }
    if let Some(effort) = &config.effort {
        let Some(option) = &config.effort_option else {
            return Err(AcpError::EffortUnsupported);
        };
        session
            .set_config(
                option.as_str(),
                ConfigValue::Id(ConfigValueById {
                    value: effort.as_str().into(),
                }),
            )
            .await
            .map_err(|source| AcpError::Agent {
                context: format!("rejected `session/set_config_option {option}`"),
                source,
            })?;
    }
    Ok(())
}

/// Whether an `[acp.files]` entry is a managed policy document (non-secret
/// MCP registry) rather than a credential file.
fn is_managed_policy_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("config.toml" | "mcp_config.json")
    )
}

/// Writes every `[acp.files]` entry with dual-UID permissions when `agent` is
/// present.
async fn materialize_files(config: &AcpConfig, agent: Option<&AgentUser>) -> Result<(), AcpError> {
    for file in &config.files {
        let managed_policy = is_managed_policy_file(&file.path);
        if let Some(parent) = file.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| AcpError::File {
                    path: parent.to_owned(),
                    source,
                })?;
            if managed_policy && agent.is_some() {
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))
                    .await
                    .map_err(|source| AcpError::File {
                        path: parent.to_owned(),
                        source,
                    })?;
            } else if !managed_policy
                && let Some(user) = agent
                && user.needs_uid_switch()
                && (parent.starts_with(flyco_provider::flycod::DEVIN_DATA_HOME)
                    || parent.starts_with(flyco_provider::flycod::ANTIGRAVITY_GEMINI_DIR))
            {
                chown_path(parent, Some(user.uid), Some(user.gid)).map_err(|error| {
                    AcpError::File {
                        path: parent.to_owned(),
                        source: std::io::Error::other(error),
                    }
                })?;
            }
        }
        tokio::fs::write(&file.path, &file.contents)
            .await
            .map_err(|source| AcpError::File {
                path: file.path.clone(),
                source,
            })?;
        let mode = match (agent, managed_policy) {
            (Some(_), true) => 0o644,
            (Some(_), false) => 0o640,
            (None, _) => 0o600,
        };
        tokio::fs::set_permissions(&file.path, std::fs::Permissions::from_mode(mode))
            .await
            .map_err(|source| AcpError::File {
                path: file.path.clone(),
                source,
            })?;
        if !managed_policy
            && let Some(user) = agent
            && user.needs_uid_switch()
        {
            chown_path(&file.path, None, Some(user.gid)).map_err(|error| AcpError::File {
                path: file.path.clone(),
                source: std::io::Error::other(error),
            })?;
        }
    }
    Ok(())
}

/// Fills `<session>` placeholders inside an [`AcpMethodCall`]'s params JSON.
fn render_params(call: &AcpMethodCall, session_id: &str) -> Value {
    fn fill(value: &Value, session_id: &str) -> Value {
        match value {
            Value::String(text) if text == "<session>" => Value::String(session_id.to_owned()),
            Value::Array(items) => {
                Value::Array(items.iter().map(|i| fill(i, session_id)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), fill(value, session_id)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    fill(&call.params, session_id)
}

/// Invokes a configured `_`-prefixed extension method on `client`.
async fn call_ext(
    client: &Client,
    call: &AcpMethodCall,
    session_id: &str,
) -> Result<Value, AcpError> {
    let method = ExtMethod::try_new(&call.call).map_err(|_| AcpError::InvalidMethod {
        method: call.call.clone(),
    })?;
    let params = render_params(call, session_id);
    client
        .ext_request(&method, &params)
        .await
        .map_err(|source| AcpError::Agent {
            context: format!("rejected `{}`", call.call),
            source,
        })
}

/// Asks the agent what it mounted, until nothing is still dialling or
/// [`MOUNT_SETTLE`] expires.
async fn verify_mount(
    client: &Client,
    session_id: &str,
    method: &AcpMethodCall,
    mount: &Mount,
) -> Result<(), AcpError> {
    let deadline = tokio::time::Instant::now() + MOUNT_SETTLE;
    loop {
        let servers = mounted_servers(client, session_id, method, deadline).await?;
        if crate::mount::settled(&servers) || tokio::time::Instant::now() >= deadline {
            return crate::mount::verify(&servers, &mount.required_tools())
                .map_err(AcpError::Mount);
        }
        let now = tokio::time::Instant::now();
        let wait = MOUNT_POLL.min(deadline.saturating_duration_since(now));
        if wait.is_zero() {
            return crate::mount::verify(&servers, &mount.required_tools())
                .map_err(AcpError::Mount);
        }
        tokio::time::sleep(wait).await;
    }
}

/// Walks the agent's mount-introspection method with a strict page cap,
/// non-advancing cursor guard, and overall deadline.
async fn mounted_servers(
    client: &Client,
    session_id: &str,
    method: &AcpMethodCall,
    deadline: tokio::time::Instant,
) -> Result<Vec<MountedServer>, AcpError> {
    let ext_method = ExtMethod::try_new(&method.call).map_err(|_| AcpError::InvalidMethod {
        method: method.call.clone(),
    })?;
    let mut mounted = Vec::new();
    let mut params = render_params(method, session_id);
    let mut seen_cursors = BTreeSet::new();
    let mut pages = 0usize;

    loop {
        pages += 1;
        let result: Value =
            tokio::time::timeout_at(deadline, client.ext_request(&ext_method, &params))
                .await
                .map_err(|_| {
                    AcpError::Mount(crate::mount::NotMounted::NotConnected {
                        status: format!(
                            "`{}` did not finish within {}s",
                            method.call,
                            MOUNT_SETTLE.as_secs()
                        ),
                    })
                })?
                .map_err(|source| AcpError::Agent {
                    context: format!("rejected `{}`", method.call),
                    source,
                })?;

        let list = result
            .get("mcpServers")
            .or_else(|| result.get("data"))
            .or_else(|| result.get("servers"))
            .and_then(Value::as_array)
            .or_else(|| result.as_array())
            .ok_or_else(|| AcpError::MountReport {
                detail: format!("no server list in {}", truncate(&result)),
            })?;
        mounted.extend(list.iter().filter_map(mounted_server));

        let Some(cursor) = result
            .get("nextCursor")
            .or_else(|| result.get("next_cursor"))
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        else {
            return Ok(mounted);
        };
        if pages >= MAX_MOUNT_PAGES {
            tracing::warn!(
                method = %method.call,
                pages,
                "mount report pagination reached page cap; stopping walk"
            );
            return Ok(mounted);
        }
        if !seen_cursors.insert(cursor.to_owned()) {
            tracing::warn!(
                method = %method.call,
                cursor,
                "mount report pagination returned a repeating cursor; stopping walk"
            );
            return Ok(mounted);
        }
        if let Value::Object(map) = &mut params {
            map.insert("cursor".to_owned(), Value::String(cursor.to_owned()));
        } else {
            tracing::warn!("the mount report paged but its params take no cursor");
            return Ok(mounted);
        }
    }
}

/// One entry of a mount report, read across Claude (`mcpServers`/`status`) and
/// Codex (`data`/`runtimeStatus`) shapes.
fn mounted_server(entry: &Value) -> Option<MountedServer> {
    let name = entry.get("name")?.as_str()?.to_owned();
    let status = entry
        .get("runtimeStatus")
        .or_else(|| entry.get("status"))
        .or_else(|| entry.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("notStarted")
        .to_owned();
    let tools = entry.get("tools").map_or_default(|tools| match tools {
        Value::Object(map) => map.keys().cloned().collect(),
        Value::Array(list) => list
            .iter()
            .filter_map(|tool| {
                tool.as_str()
                    .map(str::to_owned)
                    .or_else(|| tool.get("name").and_then(Value::as_str).map(str::to_owned))
            })
            .collect(),
        _ => Vec::new(),
    });
    Some(MountedServer {
        name,
        state: match status.as_str() {
            "connected" | "running" | "ready" => MountState::Connected,
            "notStarted" | "starting" | "pending" | "connecting" => MountState::Pending,
            _ => MountState::Failed,
        },
        status,
        tools,
    })
}

/// Watches early agent notifications when no `mcp_status` method exists.
async fn observe_mount(
    events: &mut mpsc::UnboundedReceiver<AgentEvent>,
    prelude: &mut Vec<AgentEvent>,
) -> Result<(), AcpError> {
    let deadline = tokio::time::sleep(MOUNT_OBSERVE);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => {
                tracing::warn!(
                    "this agent reports no MCP status method and said nothing about the mount; \
                     flyco's server is provisioned but unproven"
                );
                return Ok(());
            }
            event = events.recv() => {
                let Some(event) = event else { return Ok(()) };
                match event {
                    AgentEvent::Closed => return Err(AcpError::Closed),
                    AgentEvent::Vendor(notification) => {
                        match mount_evidence(&notification) {
                            Some(Evidence::Failed(detail)) => {
                                return Err(AcpError::Mount(crate::mount::NotMounted::NotConnected {
                                    status: detail,
                                }));
                            }
                            Some(Evidence::Connected) => {
                                tracing::info!("the agent confirmed flyco's MCP server connected");
                                return Ok(());
                            }
                            None => prelude.push(AgentEvent::Vendor(notification)),
                        }
                    }
                    other => prelude.push(other),
                }
            }
        }
    }
}

/// What a notification says about flyco's MCP server, when it says anything.
#[derive(Debug, PartialEq, Eq)]
enum Evidence {
    Connected,
    Failed(String),
}

/// Structurally parses an unmodeled agent notification for flyco MCP server
/// status.
///
/// Rejects false positives: failure/error payloads or in-progress (`starting`,
/// `connecting`, `pending`) notifications never register as [`Evidence::Connected`].
fn mount_evidence(notification: &Notification) -> Option<Evidence> {
    let method = notification.method.as_str();
    let params = notification.params.as_ref()?;

    if method.contains("serversChanged") || method.ends_with("/mcp") {
        let list = params
            .get("mcpServers")
            .or_else(|| params.get("servers"))
            .or_else(|| params.get("data"))
            .and_then(Value::as_array)
            .or_else(|| params.as_array())?;

        for entry in list {
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                continue;
            };
            if name != crate::mount::FLYCO {
                continue;
            }
            if let Some(error) = entry.get("error").and_then(Value::as_str)
                && !error.is_empty()
            {
                return Some(Evidence::Failed(error.to_owned()));
            }
            let status = entry
                .get("runtimeStatus")
                .or_else(|| entry.get("status"))
                .or_else(|| entry.get("state"))
                .and_then(Value::as_str);
            return match status {
                Some("connected" | "running" | "ready") => Some(Evidence::Connected),
                Some("notStarted" | "starting" | "pending" | "connecting") | None => None,
                Some(other) => Some(Evidence::Failed(other.to_owned())),
            };
        }
        return None;
    }

    if let Some(message) = params.get("message").and_then(Value::as_str)
        && params
            .get("channel")
            .and_then(Value::as_str)
            .is_some_and(|channel| channel.ends_with(crate::mount::FLYCO))
    {
        let lower = message.to_lowercase();
        if [
            "fail",
            "error",
            "refus",
            "denied",
            "unable",
            "disconnect",
            "crash",
            "timeout",
            "exited",
        ]
        .iter()
        .any(|word| lower.contains(word))
        {
            return Some(Evidence::Failed(message.to_owned()));
        }
        if [
            "starting",
            "connecting",
            "pending",
            "initializing",
            "loading",
        ]
        .iter()
        .any(|word| lower.contains(word))
        {
            return None;
        }
        if has_word(&lower, "connected")
            || has_word(&lower, "ready")
            || has_word(&lower, "listening")
            || has_word(&lower, "started")
        {
            return Some(Evidence::Connected);
        }
    }
    None
}

/// Checks whether `word` appears as a whole ASCII-alphanumeric word in `text`.
fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token == word)
}

fn truncate(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 200 {
        format!("{}…", &text[..200])
    } else {
        text
    }
}

/// The control handle of a running ACP session.
#[derive(Debug, Clone)]
pub struct AcpSession {
    commands: mpsc::Sender<DriverCommand>,
}

impl AcpSession {
    async fn ask(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), AcpError>>) -> DriverCommand,
    ) -> Result<(), AcpError> {
        let (ack, answer) = oneshot::channel();
        self.commands
            .send(make(ack))
            .await
            .map_err(|_| AcpError::Stopped)?;
        answer.await.map_err(|_| AcpError::Stopped)?
    }
}

impl HarnessSession for AcpSession {
    type Error = AcpError;

    async fn send_user_message(&self, text: String) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::UserMessage { text, ack })
            .await
    }

    async fn interrupt(&self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Interrupt { ack }).await
    }

    async fn flush(&self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Flush { ack }).await
    }

    async fn compact(&self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Compact { ack }).await
    }

    async fn context_usage(&self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::ContextUsage { ack }).await
    }

    async fn set_model(&self, model: ModelChoice) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::SetModel { model, ack }).await
    }

    async fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::SetPermissionMode { mode, ack })
            .await
    }

    async fn decide_approval(&self, approval: ToolApproval) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Approval { approval, ack })
            .await
    }

    async fn shutdown(self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Shutdown { ack }).await
    }
}

#[derive(Debug)]
enum DriverCommand {
    UserMessage {
        text: String,
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    Interrupt {
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    Flush {
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    Compact {
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    ContextUsage {
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    SetModel {
        model: ModelChoice,
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    SetPermissionMode {
        mode: PermissionMode,
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    Approval {
        approval: ToolApproval,
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    Shutdown {
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
}

#[derive(Debug)]
enum AgentEvent {
    Permission {
        request: Box<PermissionRequest>,
        respond: oneshot::Sender<PermissionOutcome>,
    },
    Vendor(Notification),
    PromptFinished {
        turn_id: String,
        outcome: TurnOutcome,
    },
    CompactFinished {
        outcome: TurnOutcome,
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    CallFinished {
        id: u64,
        result: Result<Value, AcpError>,
    },
    Closed,
}

struct PendingPermission {
    respond: oneshot::Sender<PermissionOutcome>,
    options: Vec<PermissionOption>,
}

enum PendingCall {
    Compact(oneshot::Sender<Result<(), AcpError>>),
    Usage,
}

struct Driver {
    client: Client,
    session: ClientSession,
    connection_task: tokio::task::JoinHandle<()>,
    session_id: String,
    config: AcpConfig,
    context_usage_method: Option<AcpMethodCall>,
    claude_compact: bool,
    claude_sync: Option<ClaudeSyncContext>,
    swarm_dir: Option<PathBuf>,
    swarm_timings: crate::swarm::WatchTimings,
    swarm_task: Option<tokio::task::JoinHandle<()>>,
    normalizer: Normalizer,
    outputs: mpsc::Sender<SessionOutput>,
    events: mpsc::UnboundedSender<AgentEvent>,
    turn_seq: u64,
    active_turn: Option<String>,
    queued_messages: VecDeque<String>,
    pending_permissions: BTreeMap<ApprovalId, PendingPermission>,
    pending_calls: BTreeMap<u64, PendingCall>,
    call_seq: u64,
    close_supported: bool,
    stopped: bool,
    mode: Option<String>,
    model: Option<String>,
}

impl Driver {
    async fn run(
        mut self,
        mut inbox: mpsc::Receiver<DriverCommand>,
        mut events: mpsc::UnboundedReceiver<AgentEvent>,
        mut session_events: SessionEvents,
        prelude: Vec<AgentEvent>,
        opened: Opened,
    ) {
        if !emit(
            &self.outputs,
            SessionOutput::Started {
                session_id: self.session_id.clone(),
            },
        )
        .await
        {
            return;
        }
        if !emit(
            &self.outputs,
            SessionOutput::Capabilities {
                capabilities: opened.capabilities,
            },
        )
        .await
        {
            return;
        }
        if !opened.models.is_empty()
            && !emit(
                &self.outputs,
                SessionOutput::Models {
                    models: opened.models,
                },
            )
            .await
        {
            return;
        }
        if !opened.commands.is_empty()
            && !emit(
                &self.outputs,
                SessionOutput::Commands {
                    commands: opened.commands,
                },
            )
            .await
        {
            return;
        }
        if let Some(notice) = opened.notice
            && !emit(
                &self.outputs,
                SessionOutput::Event {
                    event: HarnessEvent::LocalCommandOutput { content: notice },
                },
            )
            .await
        {
            return;
        }
        self.request_usage();
        self.start_swarm_watcher();

        for event in prelude {
            if !self.on_agent(event, &mut session_events).await {
                let _ = self.stop().await;
                return;
            }
        }

        loop {
            if inbox.is_closed() {
                break;
            }
            let input = tokio::select! {
                command = inbox.recv() => command.map(LoopInput::Command),
                agent = events.recv() => agent.map(|ev| LoopInput::Agent(Box::new(ev))),
                ev = session_events.next() => match ev {
                    Some(session_event) => Some(LoopInput::SessionEvent(Box::new(session_event))),
                    None => events
                        .recv()
                        .await
                        .map(|agent| LoopInput::Agent(Box::new(agent))),
                },
            };
            match input {
                Some(LoopInput::Command(command)) => {
                    if !self.handle(command, &mut session_events).await {
                        break;
                    }
                }
                Some(LoopInput::Agent(event)) => {
                    if !self.on_agent(*event, &mut session_events).await {
                        break;
                    }
                }
                Some(LoopInput::SessionEvent(event)) => {
                    if !self.on_session_event(&event).await {
                        break;
                    }
                }
                None => break,
            }
        }
        let _ = self.stop().await;
        tracing::debug!(agent = %self.config.agent, "the ACP driver task finished");
    }

    /// Drains all immediately-ready [`SessionEvent`]s so deltas, tool
    /// completions, and usage updates emitted by a turn always reach the
    /// output stream before `TurnCompleted` / `Flush`.
    async fn drain_session_events(&mut self, session_events: &mut SessionEvents) -> bool {
        while let Some(Some(event)) = futures_lite::future::poll_once(session_events.next()).await {
            if !self.on_session_event(&event).await {
                return false;
            }
        }
        true
    }

    #[expect(
        clippy::too_many_lines,
        reason = "command dispatch handles every HarnessSession verb in one match"
    )]
    async fn handle(&mut self, command: DriverCommand, session_events: &mut SessionEvents) -> bool {
        match command {
            DriverCommand::UserMessage { text, ack } => {
                if self.active_turn.is_some() {
                    let _ = ack.send(Ok(()));
                    self.queued_messages.push_back(text);
                    return true;
                }
                let result = self.start_turn(text).await;
                let ok = result.is_ok();
                let _ = ack.send(result);
                ok
            }
            DriverCommand::Interrupt { ack } => {
                // Clear queued follow-ups immediately so cancelling the active
                // turn does not unexpectedly kick off buffered user messages.
                self.queued_messages.clear();
                let result = self
                    .session
                    .cancel()
                    .await
                    .map_err(|source| AcpError::Agent {
                        context: "rejected `session/cancel`".to_owned(),
                        source,
                    });
                let _ = ack.send(result);
                true
            }
            DriverCommand::Flush { ack } => {
                if !self.drain_session_events(session_events).await {
                    let _ = ack.send(Err(AcpError::Stopped));
                    return false;
                }
                if let Some(ctx) = &self.claude_sync
                    && let Err(error) = ctx
                        .sync
                        .push(&self.session_id, &ctx.workdir, &ctx.transcript_home)
                        .await
                {
                    tracing::warn!(%error, "transcript push during flush failed");
                }
                let _ = ack.send(Ok(()));
                true
            }
            DriverCommand::Compact { ack } => {
                if let Some(call) = self.config.methods.compact.clone() {
                    self.spawn_call(call, PendingCall::Compact(ack));
                    return true;
                }
                if self.claude_compact {
                    if self.active_turn.is_some() {
                        let _ = ack.send(Err(AcpError::TurnInFlight));
                        return true;
                    }
                    let session = self.session.clone();
                    let events = self.events.clone();
                    match session.prompt("/compact") {
                        Ok(mut turn) => {
                            tokio::spawn(async move {
                                let outcome = turn.finish().await;
                                let _ = events.send(AgentEvent::CompactFinished { outcome, ack });
                            });
                        }
                        Err(source) => {
                            let _ = ack.send(Err(AcpError::Agent {
                                context: "rejected `/compact`".to_owned(),
                                source,
                            }));
                        }
                    }
                    return true;
                }
                let _ = ack.send(Err(AcpError::NoMethod {
                    feature: "context compaction",
                }));
                true
            }
            DriverCommand::ContextUsage { ack } => {
                let _ = ack.send(Ok(()));
                self.report_context_usage().await
            }
            DriverCommand::SetModel { model, ack } => {
                let result = self.apply_model_choice(model).await;
                let _ = ack.send(result);
                true
            }
            DriverCommand::SetPermissionMode { mode, ack } => {
                let result = apply_permission_mode(&self.session, mode, &self.config).await;
                let _ = ack.send(result);
                true
            }
            DriverCommand::Approval { approval, ack } => {
                let result = self.decide(&approval);
                let _ = ack.send(result);
                true
            }
            DriverCommand::Shutdown { ack } => {
                let stopped = self.stop().await;
                let _ = ack.send(stopped);
                false
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "agent event dispatch handles permissions, vendor notifications, turn/compact outcomes, and close"
    )]
    async fn on_agent(&mut self, event: AgentEvent, session_events: &mut SessionEvents) -> bool {
        match event {
            AgentEvent::Permission { request, respond } => {
                if !self.drain_session_events(session_events).await {
                    return false;
                }
                self.on_permission(*request, respond).await
            }
            AgentEvent::Vendor(notification) => {
                if notification.method == claude_acp::NOTIFY_RATE_LIMIT
                    && let Some(params) = notification.params.as_ref()
                {
                    for event in self.normalizer.on_plan_usage(params) {
                        if !emit(&self.outputs, SessionOutput::Event { event }).await {
                            return false;
                        }
                    }
                    return true;
                }
                tracing::debug!(
                    method = %notification.method,
                    "an unmodeled agent notification arrived"
                );
                true
            }
            AgentEvent::PromptFinished { turn_id, outcome } => {
                if !self.drain_session_events(session_events).await {
                    return false;
                }
                if self.active_turn.as_deref() == Some(turn_id.as_str()) {
                    self.active_turn = None;
                }
                let (event, cancelled) = self.normalizer.on_turn_outcome(outcome, &turn_id);
                if cancelled {
                    self.queued_messages.clear();
                }
                if !emit(&self.outputs, SessionOutput::Event { event }).await {
                    return false;
                }
                self.request_usage();
                if !self.report_context_usage().await {
                    return false;
                }
                if !cancelled
                    && let Some(text) = self.queued_messages.pop_front()
                    && let Err(error) = self.start_turn(text).await
                {
                    tracing::warn!(%error, "a queued message could not open its turn");
                }
                true
            }
            AgentEvent::CompactFinished { outcome, ack } => {
                if !self.drain_session_events(session_events).await {
                    let _ = ack.send(Err(AcpError::Stopped));
                    return false;
                }
                let (event, reply) = match outcome {
                    TurnOutcome::Finished {
                        stop_reason:
                            None | Some(Open::Known(aither_acp::domain::StopReason::EndTurn)),
                        ..
                    } => (HarnessEvent::ContextCompacted, Ok(())),
                    TurnOutcome::Cancelled
                    | TurnOutcome::Finished {
                        stop_reason: Some(Open::Known(aither_acp::domain::StopReason::Cancelled)),
                        ..
                    } => (
                        HarnessEvent::ContextCompactionFailed {
                            error: "context compaction was cancelled".to_owned(),
                        },
                        Ok(()),
                    ),
                    TurnOutcome::Finished {
                        stop_reason, error, ..
                    } => {
                        let message = error.map_or_else(
                            || format!("compaction stopped with `{stop_reason:?}`"),
                            |rpc| rpc.to_string(),
                        );
                        (
                            HarnessEvent::ContextCompactionFailed {
                                error: message.clone(),
                            },
                            Err(AcpError::Agent {
                                context: "rejected `/compact`".to_owned(),
                                source: ClientError::Protocol(message),
                            }),
                        )
                    }
                    TurnOutcome::Rejected(rpc) => (
                        HarnessEvent::ContextCompactionFailed {
                            error: rpc.to_string(),
                        },
                        Err(AcpError::Agent {
                            context: "rejected `/compact`".to_owned(),
                            source: ClientError::Rejected(rpc),
                        }),
                    ),
                };
                let _ = ack.send(reply);
                emit(&self.outputs, SessionOutput::Event { event }).await
            }
            AgentEvent::CallFinished { id, result } => self.on_call_finished(id, result).await,
            AgentEvent::Closed => {
                let _ = self.drain_session_events(session_events).await;
                if self.stopped {
                    tracing::info!("the ACP agent's connection closed");
                } else {
                    let message = format!(
                        "the {} ACP agent connection closed unexpectedly",
                        self.config.agent
                    );
                    tracing::error!("{message}");
                    if let Some(turn_id) = self.active_turn.take() {
                        let _ = emit(
                            &self.outputs,
                            SessionOutput::Event {
                                event: HarnessEvent::TurnFailed {
                                    turn_id,
                                    error: message.clone(),
                                },
                            },
                        )
                        .await;
                    }
                    self.queued_messages.clear();
                    let _ = emit(&self.outputs, SessionOutput::Fatal { error: message }).await;
                }
                false
            }
        }
    }

    async fn on_session_event(&mut self, event: &SessionEvent) -> bool {
        let turn_id = self
            .active_turn
            .clone()
            .unwrap_or_else(|| "turn-0".to_owned());
        let state = self.session.state();
        let normalized = self.normalizer.on_event(event, &state, &turn_id);
        if let Some(commands) = normalized.commands
            && !emit(&self.outputs, SessionOutput::Commands { commands }).await
        {
            return false;
        }
        if let Some(models) = normalized.models
            && !models.is_empty()
            && !emit(&self.outputs, SessionOutput::Models { models }).await
        {
            return false;
        }
        if let Some(model) = normalized.model {
            self.model = Some(model);
        }
        if let Some(mode) = normalized.mode {
            self.mode = Some(mode);
        }
        for event in normalized.events {
            if !emit(&self.outputs, SessionOutput::Event { event }).await {
                return false;
            }
        }
        true
    }

    async fn on_permission(
        &mut self,
        request: PermissionRequest,
        respond: oneshot::Sender<PermissionOutcome>,
    ) -> bool {
        let id = ApprovalId::generate();
        let (tool, input) = permission_tool_and_input(&request);
        self.pending_permissions.insert(
            id,
            PendingPermission {
                respond,
                options: request.options,
            },
        );
        emit(
            &self.outputs,
            SessionOutput::ApprovalRequest {
                id,
                tool,
                input,
                suggestions: None,
            },
        )
        .await
    }

    async fn on_call_finished(&mut self, id: u64, result: Result<Value, AcpError>) -> bool {
        let Some(call) = self.pending_calls.remove(&id) else {
            return true;
        };
        match call {
            PendingCall::Compact(ack) => {
                let event = match &result {
                    Ok(_) => HarnessEvent::ContextCompacted,
                    Err(error) => HarnessEvent::ContextCompactionFailed {
                        error: super::describe(error),
                    },
                };
                let _ = ack.send(result.map(|_| ()));
                emit(&self.outputs, SessionOutput::Event { event }).await
            }
            PendingCall::Usage => match result {
                Ok(value) => {
                    for event in self.normalizer.on_plan_usage(&value) {
                        if !emit(&self.outputs, SessionOutput::Event { event }).await {
                            return false;
                        }
                    }
                    true
                }
                Err(error) => {
                    tracing::warn!(%error, "the agent refused to state the plan's limits");
                    true
                }
            },
        }
    }

    async fn start_turn(&mut self, text: String) -> Result<(), AcpError> {
        if self.active_turn.is_some() {
            return Err(AcpError::TurnInFlight);
        }
        self.turn_seq += 1;
        let turn_id = format!("turn-{}", self.turn_seq);
        self.active_turn = Some(turn_id.clone());
        if !emit(
            &self.outputs,
            SessionOutput::Event {
                event: HarnessEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                },
            },
        )
        .await
        {
            return Err(AcpError::Stopped);
        }
        let mut turn = self
            .session
            .prompt(text)
            .map_err(|source| AcpError::Agent {
                context: "rejected `session/prompt`".to_owned(),
                source,
            })?;
        let events = self.events.clone();
        let id = turn_id;
        tokio::spawn(async move {
            let outcome = turn.finish().await;
            let _ = events.send(AgentEvent::PromptFinished {
                turn_id: id,
                outcome,
            });
        });
        Ok(())
    }

    async fn report_context_usage(&self) -> bool {
        let usage = if let Some(call) = &self.context_usage_method {
            match tokio::time::timeout(
                CONTEXT_USAGE_TIMEOUT,
                call_ext(&self.client, call, &self.session_id),
            )
            .await
            {
                Ok(Ok(value)) => {
                    normalize::parse_claude_context_usage(&value, self.model.as_deref())
                }
                Ok(Err(error)) => {
                    tracing::debug!(%error, "context usage extension call failed; using window");
                    None
                }
                Err(_) => {
                    tracing::debug!("context usage extension call timed out; using window");
                    None
                }
            }
        } else {
            None
        }
        .unwrap_or_else(|| ContextUsage {
            model: self.model.clone(),
            window: self.normalizer.window(),
            auto_compact: None,
            categories: Vec::new(),
            mcp_tools: Vec::new(),
            memory_files: Vec::new(),
            agents: Vec::new(),
            skills: Vec::new(),
        });

        emit(
            &self.outputs,
            SessionOutput::Event {
                event: HarnessEvent::ContextUsage { usage },
            },
        )
        .await
    }

    fn request_usage(&mut self) {
        let Some(call) = self.config.methods.usage.clone() else {
            return;
        };
        self.spawn_call(call, PendingCall::Usage);
    }

    fn start_swarm_watcher(&mut self) {
        if let Some(swarm_dir) = self.swarm_dir.take() {
            tracing::debug!(path = %swarm_dir.display(), "starting read-only swarm watcher");
            self.swarm_task = Some(crate::swarm::spawn(
                swarm_dir,
                self.outputs.clone(),
                self.swarm_timings,
            ));
        }
    }

    fn spawn_call(&mut self, call: AcpMethodCall, pending: PendingCall) {
        self.call_seq += 1;
        let id = self.call_seq;
        self.pending_calls.insert(id, pending);
        let client = self.client.clone();
        let events = self.events.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            let result = call_ext(&client, &call, &session_id).await;
            let _ = events.send(AgentEvent::CallFinished { id, result });
        });
    }

    async fn apply_model_choice(&mut self, model: ModelChoice) -> Result<(), AcpError> {
        self.session
            .set_config(
                self.config.model_option.as_str(),
                ConfigValue::Id(ConfigValueById {
                    value: model.model.as_str().into(),
                }),
            )
            .await
            .map_err(|source| AcpError::Agent {
                context: format!(
                    "rejected `session/set_config_option {}`",
                    self.config.model_option
                ),
                source,
            })?;
        if let Some(effort) = model.effort {
            let Some(option) = &self.config.effort_option else {
                return Err(AcpError::EffortUnsupported);
            };
            self.session
                .set_config(
                    option.as_str(),
                    ConfigValue::Id(ConfigValueById {
                        value: effort.as_str().into(),
                    }),
                )
                .await
                .map_err(|source| AcpError::Agent {
                    context: format!("rejected `session/set_config_option {option}`"),
                    source,
                })?;
        }
        self.model = Some(model.model);
        Ok(())
    }

    fn decide(&mut self, approval: &ToolApproval) -> Result<(), AcpError> {
        let id = approval.id();
        let Some(pending) = self.pending_permissions.remove(&id) else {
            return Err(AcpError::UnknownApproval(id));
        };
        let outcome = match &approval {
            ToolApproval::Allow { .. } => pick(&pending.options, true),
            ToolApproval::Deny { message, .. } => {
                tracing::info!(%message, "a denial's reason has no ACP channel");
                pick(&pending.options, false)
            }
        };
        let outcome = outcome.unwrap_or(PermissionOutcome::Cancelled(CancelledOutcome {
            meta: None,
        }));
        let _ = pending.respond.send(outcome);
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), AcpError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        if let Some(task) = self.swarm_task.take() {
            task.abort();
        }
        self.queued_messages.clear();
        if self.close_supported
            && let Err(error) = self.session.close().await
        {
            tracing::debug!(%error, "the agent refused `session/close`");
        }
        self.client.close().await;
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut self.connection_task)
            .await
            .is_err()
        {
            self.connection_task.abort();
            return Err(AcpError::Agent {
                context: format!(
                    "did not stop within {}s and was killed",
                    SHUTDOWN_GRACE.as_secs()
                ),
                source: ClientError::PeerClosed { status: None },
            });
        }
        Ok(())
    }
}

/// Extracts a human-readable tool name and structured input payload from an
/// ACP [`PermissionRequest`].
fn permission_tool_and_input(request: &PermissionRequest) -> (String, Value) {
    match &request.subject {
        Some(Open::Known(PermissionSubject::ToolCall(subject))) => {
            let call = &subject.tool_call;
            let title = call
                .title
                .as_option()
                .map(String::as_str)
                .filter(|t| !t.is_empty())
                .unwrap_or(request.title.as_str());
            let name = call.name.as_option().map(String::as_str);
            let tool = normalize::tool_name(name, title, call.kind.as_option());
            let input = call.raw_input.as_option().cloned().unwrap_or_else(|| {
                json!({
                    "title": title,
                    "description": request.description,
                })
            });
            (tool, input)
        }
        Some(Open::Known(PermissionSubject::Command(cmd))) => {
            let tool = if request.title.is_empty() {
                "execute".to_owned()
            } else {
                request.title.clone()
            };
            let input = json!({
                "command": cmd.command,
                "cwd": cmd.cwd,
            });
            (tool, input)
        }
        Some(Open::Custom(_, raw) | Open::Unknown(_, raw)) => {
            let tool = if request.title.is_empty() {
                "tool".to_owned()
            } else {
                request.title.clone()
            };
            (tool, raw.clone())
        }
        None => {
            let tool = if request.title.is_empty() {
                "tool".to_owned()
            } else {
                request.title.clone()
            };
            let input = json!({
                "title": request.title,
                "description": request.description,
            });
            (tool, input)
        }
    }
}

fn pick(options: &[PermissionOption], allow: bool) -> Option<PermissionOutcome> {
    let preferred = if allow {
        [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    } else {
        [
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]
    };
    preferred
        .iter()
        .find_map(|kind| {
            options
                .iter()
                .find(|option| option.kind.as_known() == Some(kind))
                .map(|option| option.option_id.clone())
        })
        .map(|option_id| {
            PermissionOutcome::Selected(SelectedOutcome {
                option_id,
                meta: None,
            })
        })
}

enum LoopInput {
    Command(DriverCommand),
    Agent(Box<AgentEvent>),
    SessionEvent(Box<SessionEvent>),
}

async fn emit(outputs: &mpsc::Sender<SessionOutput>, output: SessionOutput) -> bool {
    if outputs.send(output).await.is_err() {
        tracing::debug!("nothing is consuming the session's output stream");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use aither_acp::Client;
    use aither_acp::domain::{Implementation, SessionSetupParams};
    use aither_acp::rpc::Notification;
    use aither_acp::server::{
        PendingPrompt, PromptError, Server, ServerError, Session, SessionContext, SessionHost,
    };
    use serde_json::json;

    use super::{Evidence, MAX_MOUNT_PAGES, mount_evidence, mounted_servers};
    use crate::config::AcpMethodCall;

    #[test]
    fn mount_evidence_rejects_failure_and_starting_notifications() {
        let starting_server = Notification {
            method: "_vendor/mcp".to_owned(),
            params: Some(json!({
                "servers": [{"name": "flyco", "status": "starting"}]
            })),
        };
        assert_eq!(mount_evidence(&starting_server), None);

        let failed_server = Notification {
            method: "_vendor/serversChanged".to_owned(),
            params: Some(json!({
                "servers": [{"name": "flyco", "status": "failed", "error": "connection refused"}]
            })),
        };
        assert_eq!(
            mount_evidence(&failed_server),
            Some(Evidence::Failed("connection refused".to_owned()))
        );

        let connected_server = Notification {
            method: "_vendor/serversChanged".to_owned(),
            params: Some(json!({
                "mcpServers": [{"name": "flyco", "status": "connected"}]
            })),
        };
        assert_eq!(mount_evidence(&connected_server), Some(Evidence::Connected));

        let starting_log = Notification {
            method: "_cognition.ai/output".to_owned(),
            params: Some(json!({
                "channel": "MCP: flyco",
                "message": "starting flyco MCP server..."
            })),
        };
        assert_eq!(mount_evidence(&starting_log), None);

        let connecting_log = Notification {
            method: "_cognition.ai/output".to_owned(),
            params: Some(json!({
                "channel": "MCP: flyco",
                "message": "connecting to flyco"
            })),
        };
        assert_eq!(mount_evidence(&connecting_log), None);

        let disconnected_log = Notification {
            method: "_cognition.ai/output".to_owned(),
            params: Some(json!({
                "channel": "MCP: flyco",
                "message": "server disconnected unexpectedly"
            })),
        };
        assert!(matches!(
            mount_evidence(&disconnected_log),
            Some(Evidence::Failed(_))
        ));

        let ready_log = Notification {
            method: "_cognition.ai/output".to_owned(),
            params: Some(json!({
                "channel": "MCP: flyco",
                "message": "MCP server connected and ready"
            })),
        };
        assert_eq!(mount_evidence(&ready_log), Some(Evidence::Connected));
    }

    struct DummySession;
    impl Session for DummySession {
        fn prompt(
            &self,
            _pending: PendingPrompt,
            _cx: &SessionContext,
        ) -> impl std::future::Future<Output = Result<aither_acp::domain::StopReason, PromptError>>
        {
            std::future::ready(Ok(aither_acp::domain::StopReason::EndTurn))
        }
    }

    struct DummyHost;
    impl SessionHost for DummyHost {
        type Session = DummySession;
        fn new_session(
            &self,
            _params: SessionSetupParams,
            _cx: &SessionContext,
        ) -> impl std::future::Future<Output = Result<DummySession, ServerError>> {
            std::future::ready(Ok(DummySession))
        }
    }

    #[tokio::test]
    async fn mounted_servers_stops_on_repeating_cursor() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = calls.clone();
        let server = Server::builder(DummyHost, Implementation::new("dummy", "0.1.0"))
            .ext(move |_method, _params| {
                let calls = calls_ref.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({
                        "data": [{"name": "flyco", "runtimeStatus": "connected", "tools": ["machine_status"]}],
                        "nextCursor": "stuck-cursor"
                    }))
                }
            })
            .build();
        let (client, conn) = Client::builder(Implementation::new("test", "0.1.0"))
            .in_process_server(server)
            .await
            .expect("connect");
        let task = tokio::spawn(conn);

        let method = AcpMethodCall {
            call: "_test/mcp_status".to_owned(),
            params: json!({}),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let servers = mounted_servers(&client, "s1", &method, deadline)
            .await
            .expect("bounded walk succeeds");
        assert_eq!(servers.len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        client.close().await;
        let _ = task.await;
    }

    #[tokio::test]
    async fn mounted_servers_enforces_page_cap() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = calls.clone();
        let server = Server::builder(DummyHost, Implementation::new("dummy", "0.1.0"))
            .ext(move |_method, _params| {
                let calls = calls_ref.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(json!({
                        "data": [{"name": format!("srv-{n}"), "runtimeStatus": "connected", "tools": []}],
                        "nextCursor": format!("cursor-{n}")
                    }))
                }
            })
            .build();
        let (client, conn) = Client::builder(Implementation::new("test", "0.1.0"))
            .in_process_server(server)
            .await
            .expect("connect");
        let task = tokio::spawn(conn);

        let method = AcpMethodCall {
            call: "_test/mcp_status".to_owned(),
            params: json!({}),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let servers = mounted_servers(&client, "s1", &method, deadline)
            .await
            .expect("capped walk succeeds");
        assert_eq!(calls.load(Ordering::SeqCst), MAX_MOUNT_PAGES);
        assert_eq!(servers.len(), MAX_MOUNT_PAGES);
        client.close().await;
        let _ = task.await;
    }
}
