//! The session's harness, as a terminal application.
//!
//! `flyco claude`, `flyco codex` and `flyco resume` ask the control plane
//! for [`ControlToDaemon::TerminalHarness`](flyco_core::ControlToDaemon),
//! and the answer lands here: the daemon builds the invocation from its
//! own configuration, so credentials reach the TUI through its environment
//! and nothing a client sends ever carries them.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use flyco_core::DriverKind;
use portable_pty::CommandBuilder;
use serde::Serialize;

use crate::config::{ClaudeAuth, DaemonConfig};
use crate::privilege::{AgentUser, chown_path};

/// Why a harness TUI cannot be launched.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TuiError {
    /// The configured or PATH-resolved native `claude` binary was not found.
    #[error(
        "no claude binary at {0}: install the native Claude Code CLI or set [claude].claude_path"
    )]
    ClaudeMissing(PathBuf),
    /// The agent's `[acp]` table declares no TUI to bridge to.
    #[error("{0} has no TUI: its [acp.tui] table is empty, so there is nothing to launch")]
    NoTui(String),
    /// The OAuth credential file could not be written.
    #[error("the OAuth credential could not be written: {0}")]
    CredentialWrite(String),
}

/// The OAuth grant scopes a `claude setup-token` is minted with.
///
/// The CLI's own OAuth client requests this set at login; the token itself
/// carries them server-side, and the credentials file repeats them so the
/// resolved credential's `scopes` satisfies the rate-limit gate.
const SETUP_TOKEN_SCOPES: &[&str] = &[
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// The `.credentials.json` the CLI resolves an OAuth login from.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CredentialFile<'a> {
    claude_ai_oauth: Credential<'a>,
}

/// One OAuth credential entry, as the CLI's credentials store spells it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Credential<'a> {
    access_token: &'a str,
    scopes: &'static [&'static str],
}

/// Writes the OAuth credential the launched `claude` CLI resolves.
///
/// An env-injected `CLAUDE_CODE_OAUTH_TOKEN` resolves as env-quad auth,
/// which the CLI's rate-limit gate treats as ineligible for plan limits —
/// `/usage` would show nothing on every OAuth session. The credentials
/// store — `CLAUDE_CONFIG_DIR/.credentials.json` — is the shape a real
/// `claude login` leaves behind, so the gate passes and the plan windows
/// read true. `claude setup-token` mints a long-lived grant with no
/// refresh token, so the entry cannot expire or refresh from the CLI's
/// side.
pub(crate) fn provision_claude_oauth(
    dir: &Path,
    token: &str,
    agent: Option<&AgentUser>,
) -> std::io::Result<()> {
    use std::io::Write as _;

    std::fs::create_dir_all(dir)?;
    if let Some(user) = agent
        && user.needs_uid_switch()
    {
        chown_path(dir, Some(user.uid), Some(user.gid)).map_err(std::io::Error::other)?;
    }
    let file = CredentialFile {
        claude_ai_oauth: Credential {
            access_token: token,
            scopes: SETUP_TOKEN_SCOPES,
        },
    };
    let body = serde_json::to_string(&file).expect("a credential file serializes");
    let path = dir.join(".credentials.json");
    let tmp_path = dir.join(format!(".credentials.json.{}.tmp", uuid::Uuid::new_v4()));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let write_result = (|| -> std::io::Result<()> {
        let mut tmp = options.open(&tmp_path)?;
        tmp.write_all(body.as_bytes())?;
        tmp.sync_all()?;
        drop(tmp);
        if let Some(user) = agent
            && user.needs_uid_switch()
        {
            chown_path(&tmp_path, Some(user.uid), Some(user.gid)).map_err(std::io::Error::other)?;
        }
        std::fs::rename(&tmp_path, &path)?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    Ok(())
}

fn provision_oauth(dir: &Path, token: &str, agent: Option<&AgentUser>) -> Result<(), TuiError> {
    provision_claude_oauth(dir, token, agent)
        .map_err(|error| TuiError::CredentialWrite(error.to_string()))
}

/// How the session's harness TUI is launched.
///
/// Resolved once from the daemon's configuration: the program, the
/// arguments a fresh launch takes, the arguments a re-entry takes, and
/// the environment — credentials included — the child is spawned with.
pub struct HarnessTui {
    program: Program,
    args: Vec<OsString>,
    resume_args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    /// Where an OAuth credential is provisioned, and the token it carries —
    /// written fresh at every launch because the CLI removes the file on
    /// some failures.
    oauth_credential: Option<(PathBuf, String)>,
    /// Unprivileged user that will own any freshly provisioned credential file.
    agent: Option<AgentUser>,
}

impl core::fmt::Debug for HarnessTui {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `env` carries the session's credentials — a Debug that printed
        // it would put them in every trace the value touches, so only the
        // variable *names* are shown.
        f.debug_struct("HarnessTui")
            .field("args", &self.args)
            .field("resume_args", &self.resume_args)
            .field(
                "env",
                &self.env.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// The executable half of a [`HarnessTui`].
enum Program {
    /// An executable the configuration names — an `[acp.tui]` program.
    Bin(PathBuf),
    /// The agent has no TUI; the error is answered at launch instead.
    None(String),
    /// The native `claude` CLI binary resolved from `[claude].claude_path` or
    /// `PATH` at startup. A session whose host lacks it still runs headless;
    /// the error is answered at launch instead.
    Claude(Result<PathBuf, TuiError>),
}

impl HarnessTui {
    /// Builds the launch description from the daemon's configuration.
    ///
    /// The driver the session runs decides which configuration speaks:
    /// for Claude Code the native `claude` binary, for ACP the `[acp.tui]`
    /// table — which an agent without a TUI simply does not have, and says
    /// so at launch.
    pub async fn resolve(config: &DaemonConfig, agent: Option<AgentUser>) -> Self {
        match config.harness {
            DriverKind::ClaudeCode => Self::claude(config, agent).await,
            DriverKind::Acp => Self::acp(config, agent),
        }
    }

    /// The invocation for one launch, fresh or re-entering.
    ///
    /// # Errors
    ///
    /// Returns [`TuiError`] when the program itself could not be resolved —
    /// a missing `claude` binary, or an agent with no TUI at all.
    pub async fn command(&self, resume: bool) -> Result<CommandBuilder, TuiError> {
        let program = match &self.program {
            Program::Bin(bin) => bin.clone(),
            Program::None(agent) => return Err(TuiError::NoTui(agent.clone())),
            Program::Claude(resolved) => resolved.clone()?,
        };
        if let Some((dir, token)) = self.oauth_credential.clone() {
            let agent = self.agent.clone();
            tokio::task::spawn_blocking(move || provision_oauth(&dir, &token, agent.as_ref()))
                .await
                .map_err(|error| TuiError::CredentialWrite(error.to_string()))??;
        }
        let mut command = CommandBuilder::new(program);
        command.args(if resume {
            &self.resume_args
        } else {
            &self.args
        });
        for (name, value) in &self.env {
            command.env(name, value);
        }
        Ok(command)
    }

    /// The Claude Code TUI: the native `claude` binary, the session's
    /// isolated config tree and credential, and the session's model and
    /// permission mode as flags.
    async fn claude(config: &DaemonConfig, agent: Option<AgentUser>) -> Self {
        let claude = config.claude();

        let mut env: Vec<(OsString, OsString)> = Vec::new();
        if let Some(isolation) = claude.auth.isolation() {
            env.push((
                OsStr::new("CLAUDE_CONFIG_DIR").to_owned(),
                isolation.config_dir.clone().into_os_string(),
            ));
            env.push((
                OsStr::new("CLAUDE_CODE_PROJECT_DIR_NAME").to_owned(),
                isolation.project_dir_name.clone().into(),
            ));
        }
        // The OAuth token reaches the CLI as `.credentials.json` inside the
        // isolated config tree, written at launch — env injection resolves
        // as env-quad auth, which is ineligible for plan rate limits.
        let mut oauth_credential = None;
        match &claude.auth {
            ClaudeAuth::Inherit => {}
            ClaudeAuth::OauthToken { token, isolation } => {
                oauth_credential = Some((isolation.config_dir.clone(), token.clone()));
            }
            ClaudeAuth::ApiKey { key, .. } => {
                env.push((OsStr::new("ANTHROPIC_API_KEY").to_owned(), key.into()));
            }
        }

        let mut args: Vec<OsString> = Vec::new();
        if let Some(model) = &claude.model {
            args.push("--model".into());
            args.push(model.into());
        }
        // The camelCase spelling matches Claude CLI's `--permission-mode`.
        let mode = serde_json::to_value(claude.permission_mode)
            .expect("a permission mode serializes")
            .as_str()
            .expect("a permission mode serializes to a string")
            .to_owned();
        args.push("--permission-mode".into());
        args.push(mode.into());

        // Re-entry names the harness session this Flyco session continues
        // when the configuration knows it, and the latest conversation
        // otherwise.
        let mut resume_args = args.clone();
        match &config.resume_session_id {
            Some(id) => {
                resume_args.push("--resume".into());
                resume_args.push(id.into());
            }
            None => resume_args.push("--continue".into()),
        }

        Self {
            program: Program::Claude(resolve_claude_binary(claude.claude_path.as_deref()).await),
            args,
            resume_args,
            env,
            oauth_credential,
            agent,
        }
    }

    /// The ACP agent's TUI: the program `[acp.tui]` names, under the same
    /// environment the driver spawns the agent with plus the TUI's own —
    /// a `CODEX_HOME` or `XDG_DATA_HOME` the agent's credentials live
    /// under reaches its TUI the same way it reaches the agent.
    ///
    /// Resume arguments are spelled by the provisioner rather than built
    /// here: each agent re-enters a conversation its own way, and a
    /// `{session}` placeholder in `resume_args` is filled with the
    /// harness-native session id when one is recorded.
    fn acp(config: &DaemonConfig, agent: Option<AgentUser>) -> Self {
        let acp = config.acp();
        let Some(tui) = &acp.tui else {
            return Self {
                program: Program::None(acp.agent.clone()),
                args: Vec::new(),
                resume_args: Vec::new(),
                env: Vec::new(),
                oauth_credential: None,
                agent,
            };
        };

        let env = acp
            .env
            .iter()
            .chain(tui.env.iter())
            .map(|(name, value)| (name.clone().into(), value.clone().into()))
            .collect();

        let fill = |args: &[String]| -> Vec<OsString> {
            args.iter()
                .filter_map(|arg| {
                    config.resume_session_id.as_ref().map_or_else(
                        // With nothing recorded the placeholder has no
                        // value: `codex resume` and `devin --resume`
                        // without it land on the agent's own picker, which
                        // is the resume the session can offer.
                        || (!arg.contains("{session}")).then(|| OsString::from(arg)),
                        |id| Some(OsString::from(arg.replace("{session}", id))),
                    )
                })
                .collect()
        };

        Self {
            program: Program::Bin(tui.program.clone()),
            args: fill(&tui.args),
            resume_args: fill(&tui.resume_args),
            env,
            oauth_credential: None,
            agent,
        }
    }
}

#[cfg(test)]
impl HarnessTui {
    /// A spec whose command exists and runs nothing — tests record the
    /// launch rather than spawn it.
    pub(crate) fn fixture() -> Self {
        Self {
            program: Program::Bin(PathBuf::from("/bin/true")),
            args: Vec::new(),
            resume_args: Vec::new(),
            env: Vec::new(),
            oauth_credential: None,
            agent: None,
        }
    }
}

/// Resolves the native `claude` CLI binary from `[claude].claude_path` or `PATH`.
async fn resolve_claude_binary(configured: Option<&Path>) -> Result<PathBuf, TuiError> {
    if let Some(path) = configured {
        if matches!(tokio::fs::metadata(path).await, Ok(meta) if meta.is_file()) {
            return Ok(path.to_path_buf());
        }
        return Err(TuiError::ClaudeMissing(path.to_path_buf()));
    }
    if let Some(path_env) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join("claude");
            if matches!(tokio::fs::metadata(&candidate).await, Ok(meta) if meta.is_file()) {
                return Ok(candidate);
            }
        }
    }
    Err(TuiError::ClaudeMissing(PathBuf::from("claude")))
}
