//! Child-process transports and privilege-dropping connectors for the unified
//! ACP driver.
//!
//! `aither-acp`, `claude-acp-sdk`, and `codex-sdk` are runtime-agnostic and
//! speak `futures_lite::io::{AsyncRead, AsyncWrite}`. On provisioned machines
//! `flycod` spawns every agent-controlled process under [`AgentUser`] (dual-UID
//! isolation + sanitized environment via `tokio::process::Command`). This module
//! bridges `tokio` pipes into those crates while preserving `kill_on_drop` and
//! `pre_exec` privilege drop across Claude Code, Codex, and subprocess ACP
//! agents.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::task::{Context, Poll};

use aither_acp::Transport;
use aither_acp::rpc::{Inbound, Lines, Outbound, RpcError};
use claude_acp::{ConnectedSession, SessionConnector};
use claude_acp_sdk::{
    AnyControlHandler, Client as ClaudeSdkClient, SpawnConfig as ClaudeSpawnConfig,
};
use codex_acp::{AppServerConnector, ConnectedAppServer};
use codex_sdk::{AnyServerRequestHandler, AppServer, SpawnConfig as CodexSpawnConfig};
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::privilege::AgentUser;

/// Bridges a [`tokio::io::AsyncRead`] into [`futures_lite::io::AsyncRead`].
pub struct TokioRead<R>(pub R);

impl<R: TokioAsyncRead + Unpin> futures_lite::io::AsyncRead for TokioRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut read_buf = ReadBuf::new(buf);
        match Pin::new(&mut self.0).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Bridges a [`tokio::io::AsyncWrite`] into [`futures_lite::io::AsyncWrite`].
pub struct TokioWrite<W>(pub W);

impl<W: TokioAsyncWrite + Unpin> futures_lite::io::AsyncWrite for TokioWrite<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Newline-delimited ACP [`Transport`] backed by a privilege-dropped
/// [`tokio::process::Child`].
///
/// Used for external ACP subprocesses (such as `devin acp`) when dual-UID
/// isolation (`AgentUser`) is active. Ensures the child is killed on drop.
pub struct TokioChildTransport {
    child: Child,
    inner: Lines<TokioRead<ChildStdout>, TokioWrite<ChildStdin>>,
    status: Option<ExitStatus>,
}

impl std::fmt::Debug for TokioChildTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioChildTransport")
            .field("id", &self.child.id())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl TokioChildTransport {
    /// Spawns `program` as `agent` with piped stdin/stdout, inherited stderr,
    /// sanitized environment, and `kill_on_drop(true)`.
    pub(crate) fn spawn(
        program: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
        agent: &AgentUser,
    ) -> io::Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        agent.apply_tokio_command(&mut cmd, env.clone());
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("ACP child stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("ACP child stdout was not piped"))?;
        Ok(Self {
            child,
            inner: Lines::new(TokioRead(stdout), TokioWrite(stdin)),
            status: None,
        })
    }
}

impl Transport for TokioChildTransport {
    async fn recv(&mut self) -> Result<Option<Inbound>, RpcError> {
        self.inner.recv().await
    }

    async fn send(&mut self, msg: &Outbound) -> Result<(), RpcError> {
        self.inner.send(msg).await
    }

    async fn close(&mut self) -> Result<(), RpcError> {
        let _ = self.inner.close().await;
        if self.status.is_none() {
            let _ = self.child.start_kill();
            self.status = self.child.wait().await.ok();
        }
        Ok(())
    }

    async fn exit_status(&mut self) -> Option<ExitStatus> {
        if let Some(status) = self.status {
            return Some(status);
        }
        let status = if let Ok(Some(status)) = self.child.try_wait() {
            Some(status)
        } else {
            let _ = self.child.start_kill();
            self.child.wait().await.ok()
        };
        self.status = status;
        status
    }
}

impl Drop for TokioChildTransport {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// [`SessionConnector`] that spawns the native `claude` CLI with `tokio`,
/// optionally dropping privileges to [`AgentUser`] and sanitizing the
/// environment.
#[derive(Debug, Clone)]
pub struct ClaudeProcessConnector {
    agent: Option<AgentUser>,
}

impl ClaudeProcessConnector {
    /// Creates a connector for the native `claude` binary.
    #[must_use]
    pub const fn new(agent: Option<AgentUser>) -> Self {
        Self { agent }
    }

    fn connect_sync(
        &self,
        config: &ClaudeSpawnConfig,
        handler: AnyControlHandler,
    ) -> claude_acp_sdk::Result<ConnectedSession> {
        let exe = config.resolve_executable();
        let args = config.build_args()?;

        let mut cmd = Command::new(&exe);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }
        if let Some(user) = &self.agent {
            user.apply_tokio_command(&mut cmd, config.env.clone());
        } else {
            cmd.envs(&config.env);
        }

        let mut child = cmd.spawn().map_err(claude_acp_sdk::SdkError::from)?;
        let stdin = child.stdin.take().ok_or_else(|| {
            claude_acp_sdk::SdkError::Io(
                io::Error::other("claude child stdin was not piped").into(),
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            claude_acp_sdk::SdkError::Io(
                io::Error::other("claude child stdout was not piped").into(),
            )
        })?;

        let (client, driver) =
            ClaudeSdkClient::attach(TokioRead(stdout), TokioWrite(stdin), handler);
        let supervised_driver = async move {
            let outcome = driver.await;
            let _ = child.start_kill();
            let _ = child.wait().await;
            outcome
        };
        Ok(ConnectedSession::new(client, supervised_driver))
    }
}

impl SessionConnector for ClaudeProcessConnector {
    fn connect(
        &self,
        config: ClaudeSpawnConfig,
        handler: AnyControlHandler,
    ) -> impl std::future::Future<Output = claude_acp_sdk::Result<ConnectedSession>> + Send {
        std::future::ready(self.connect_sync(&config, handler))
    }
}

/// [`AppServerConnector`] that spawns `codex app-server` with `tokio`,
/// optionally dropping privileges to [`AgentUser`] and sanitizing the
/// environment.
#[derive(Debug, Clone)]
pub struct CodexProcessConnector {
    agent: Option<AgentUser>,
}

impl CodexProcessConnector {
    /// Creates a connector for the native `codex` binary.
    #[must_use]
    pub const fn new(agent: Option<AgentUser>) -> Self {
        Self { agent }
    }

    fn connect_sync(
        &self,
        config: &CodexSpawnConfig,
        handler: AnyServerRequestHandler,
    ) -> codex_sdk::Result<ConnectedAppServer> {
        let exe = config.resolve_executable();
        let args = config.build_args();

        let mut cmd = Command::new(&exe);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }
        if let Some(user) = &self.agent {
            user.apply_tokio_command(&mut cmd, config.env.clone());
        } else {
            cmd.envs(&config.env);
        }

        let mut child = cmd.spawn().map_err(codex_sdk::SdkError::from)?;
        let stdin = child.stdin.take().ok_or_else(|| {
            codex_sdk::SdkError::Io(io::Error::other("codex child stdin was not piped"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            codex_sdk::SdkError::Io(io::Error::other("codex child stdout was not piped"))
        })?;

        let (server, driver) = AppServer::attach(TokioRead(stdout), TokioWrite(stdin), handler);
        let supervised_driver = async move {
            driver.await;
            let _ = child.start_kill();
            let _ = child.wait().await;
        };
        Ok(ConnectedAppServer::new(server, supervised_driver))
    }
}

impl AppServerConnector for CodexProcessConnector {
    fn connect(
        &self,
        config: CodexSpawnConfig,
        handler: AnyServerRequestHandler,
    ) -> impl std::future::Future<Output = codex_sdk::Result<ConnectedAppServer>> + Send {
        std::future::ready(self.connect_sync(&config, handler))
    }
}
