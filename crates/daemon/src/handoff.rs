//! Native same-harness materialization of locally resumed session transcripts.
//!
//! When a user resumes a local Claude Code, Codex, Antigravity, or Devin
//! session onto a cloud machine (`flyco handoff` or `/resume-*` in the TUI),
//! the CLI uploads the raw harness-native transcript (or single-session SQLite
//! database) to the control plane. Before starting the ACP harness with
//! `resume_session_id`, `flycod` places that payload directly into the
//! matching harness's native session store so the harness continues the
//! conversation natively without any cross-harness translation.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use flyco_core::{HandoffView, HarnessKind};

use crate::config::DaemonConfig;
use crate::control::rest::ControlApi;
use crate::control::store::R2TranscriptSync;
use crate::control::wire::WireError;
use crate::privilege::{AgentUser, chown_path};

/// Validates a harness-native session id before embedding it in a filesystem
/// path, rejecting traversal or path separators.
fn validate_session_id(session_id: &str) -> Result<(), WireError> {
    if session_id.is_empty()
        || session_id == "."
        || session_id == ".."
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains('\0')
        || session_id.contains("..")
    {
        return Err(WireError::Handoff(format!(
            "invalid native session id `{session_id}`"
        )));
    }
    Ok(())
}

/// Resolves the flag value following `flag` in an argument vector (e.g.
/// `["--gemini-dir", "/var/lib/flyco/antigravity"]` or `--gemini-dir=...`).
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().map(String::as_str);
        }
        if let Some(rest) = arg.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            return Some(rest);
        }
    }
    None
}

/// Computes the native on-disk destination where `harness` reads `session_id`.
///
/// # Errors
///
/// Returns [`WireError::Handoff`] when `session_id` is invalid.
pub fn native_transcript_path(
    config: &DaemonConfig,
    harness: HarnessKind,
    session_id: &str,
) -> Result<PathBuf, WireError> {
    validate_session_id(session_id)?;
    match harness {
        HarnessKind::ClaudeCode => config
            .claude
            .as_ref()
            .and_then(|claude| claude.auth.isolation())
            .map_or_else(
                || {
                    let project_key = claude_acp_sdk::encode_project_dir(&config.workdir);
                    Ok(config
                        .transcript_dir
                        .join("claude-home")
                        .join(".claude")
                        .join("projects")
                        .join(project_key)
                        .join(format!("{session_id}.jsonl")))
                },
                |isolation| {
                    Ok(isolation
                        .config_dir
                        .join("projects")
                        .join(&isolation.project_dir_name)
                        .join(format!("{session_id}.jsonl")))
                },
            ),
        HarnessKind::Codex => {
            let codex_home = config
                .acp
                .as_ref()
                .and_then(|acp| acp.env.get("CODEX_HOME"))
                .map_or_else(
                    || PathBuf::from(flyco_provider::flycod::CODEX_HOME),
                    PathBuf::from,
                );
            Ok(codex_home
                .join("sessions")
                .join("2026")
                .join("01")
                .join("01")
                .join(format!("rollout-2026-01-01T00-00-00-{session_id}.jsonl")))
        }
        HarnessKind::Antigravity => {
            let args = config
                .acp
                .as_ref()
                .map_or(&[][..], |acp| acp.args.as_slice());
            let gemini_dir = arg_value(args, "--gemini-dir").map_or_else(
                || PathBuf::from(flyco_provider::flycod::ANTIGRAVITY_GEMINI_DIR),
                PathBuf::from,
            );
            let app_data_dir = arg_value(args, "--app-data-dir")
                .unwrap_or(flyco_provider::flycod::ANTIGRAVITY_APP_DATA_DIR);
            Ok(gemini_dir
                .join(app_data_dir)
                .join("conversations")
                .join(format!("{session_id}.db")))
        }
        HarnessKind::Devin => {
            let data_home = config
                .acp
                .as_ref()
                .and_then(|acp| acp.env.get("XDG_DATA_HOME"))
                .map_or_else(
                    || PathBuf::from(flyco_provider::flycod::DEVIN_DATA_HOME),
                    PathBuf::from,
                );
            Ok(data_home.join("devin").join("cli").join("sessions.db"))
        }
    }
}

/// Fetches the uploaded native transcript payload from the control plane and
/// writes it into the matching harness's native session store.
///
/// Idempotent across restarts on a persistent disk: if the target file already
/// exists, the live disk state is preserved rather than overwritten by the
/// initial upload. For Claude Code, the payload is also seeded into
/// [`R2TranscriptSync`] when its stream is still empty so future machine
/// restarts and resizes retain it.
///
/// # Errors
///
/// Returns [`WireError::Handoff`] when the manifest names a missing transcript,
/// the session id is invalid, or disk/R2 materialization fails.
pub async fn materialize_native_transcript<A>(
    config: &DaemonConfig,
    api: &A,
    view: &HandoffView,
    agent: Option<&AgentUser>,
) -> Result<PathBuf, WireError>
where
    A: ControlApi + Clone + Send + Sync + 'static,
{
    let target = native_transcript_path(config, view.harness, &view.session_id)?;
    let bytes = if target.exists() {
        tracing::info!(
            harness = ?view.harness,
            path = %target.display(),
            "native transcript already present on disk; preserving live session state"
        );
        None
    } else {
        let Some(transcript) = api.get_handoff_transcript().await? else {
            return Err(WireError::Handoff(
                "the manifest announces a transcript that is not stored".to_owned(),
            ));
        };
        write_owned_file(&target, &transcript, agent).await?;
        tracing::info!(
            harness = ?view.harness,
            bytes = transcript.len(),
            path = %target.display(),
            "materialized native transcript for local resume"
        );
        Some(transcript)
    };

    if view.harness == HarnessKind::ClaudeCode {
        let payload = match bytes {
            Some(b) => Some(b),
            None => tokio::fs::read(&target).await.ok(),
        };
        if let Some(payload) = payload {
            let project_dir_name = config
                .claude
                .as_ref()
                .and_then(|claude| claude.auth.isolation())
                .map(|iso| iso.project_dir_name.clone());
            let sync = R2TranscriptSync::new(api.clone(), project_dir_name, agent.cloned());
            sync.seed_if_empty(&view.session_id, &config.workdir, &payload)
                .await
                .map_err(|error| WireError::Handoff(error.to_string()))?;
        }
    }

    Ok(target)
}

/// Creates parent directories, writes `bytes` with `0600` permissions, and
/// transfers ownership to `agent` when running under dual-UID isolation.
async fn write_owned_file(
    path: &Path,
    bytes: &[u8],
    agent: Option<&AgentUser>,
) -> Result<(), WireError> {
    let mut created_dirs = Vec::new();
    if let Some(parent) = path.parent() {
        let mut probe = parent.to_path_buf();
        while !probe.exists() {
            created_dirs.push(probe.clone());
            let Some(next) = probe.parent() else {
                break;
            };
            probe = next.to_path_buf();
        }
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| WireError::Handoff(error.to_string()))?;
    }
    tokio::fs::write(path, bytes)
        .await
        .map_err(|error| WireError::Handoff(error.to_string()))?;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|error| WireError::Handoff(error.to_string()))?;

    if let Some(user) = agent
        && user.needs_uid_switch()
    {
        for dir in created_dirs.iter().rev() {
            chown_path(dir, Some(user.uid), Some(user.gid))
                .map_err(|error| WireError::Handoff(error.to_string()))?;
        }
        chown_path(path, Some(user.uid), Some(user.gid))
            .map_err(|error| WireError::Handoff(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use flyco_core::{DriverKind, HarnessKind, PermissionMode, SessionId};

    use super::*;
    use crate::config::{
        AcpConfig, AcpMethods, ClaudeAuth, ClaudeConfig, ComputerConfig, Isolation, ShellConfig,
        TerminalConfig,
    };

    fn base_config(root: &Path) -> DaemonConfig {
        DaemonConfig {
            session: SessionId::generate(),
            workdir: root.join("work"),
            transcript_dir: root.join("transcripts"),
            harness: DriverKind::ClaudeCode,
            claude: Some(ClaudeConfig {
                model: None,
                effort: None,
                permission_mode: PermissionMode::Default,
                claude_path: None,
                managed_dir: None,
                auth: ClaudeAuth::ApiKey {
                    key: "sk-ant-test".to_owned(),
                    isolation: Isolation {
                        config_dir: root.join("claude-cfg"),
                        project_dir_name: "flyco-work".to_owned(),
                    },
                },
            }),
            acp: None,
            resume_session_id: None,
            control_plane: None,
            github: None,
            repos: Vec::new(),
            mcp_servers: Vec::new(),
            machine: flyco_provider::testing::session_machine(),
            machine_origin: flyco_core::MachineOrigin::Auto,
            runtime: flyco_core::Runtime::Vm,
            spot_provider: None,
            terminal: TerminalConfig::default(),
            shell: ShellConfig::default(),
            computer: ComputerConfig::default(),
        }
    }

    fn acp_config(agent: &str, args: Vec<String>, env: BTreeMap<String, String>) -> AcpConfig {
        AcpConfig {
            agent: agent.to_owned(),
            in_process: None,
            program: PathBuf::from(agent),
            args,
            auth_method: None,
            mcp_via_files: true,
            env,
            files: Vec::new(),
            model: None,
            effort: None,
            model_option: "model".to_owned(),
            effort_option: None,
            permission_mode: PermissionMode::Default,
            modes: BTreeMap::new(),
            methods: AcpMethods::default(),
            tui: None,
        }
    }

    #[test]
    fn resolves_native_paths_for_all_four_harnesses() {
        let root = PathBuf::from("/tmp/flyco-handoff-paths");
        let mut cfg = base_config(&root);

        let claude =
            native_transcript_path(&cfg, HarnessKind::ClaudeCode, "sess-1").expect("claude path");
        assert_eq!(
            claude,
            root.join("claude-cfg/projects/flyco-work/sess-1.jsonl")
        );

        cfg.acp = Some(acp_config(
            "codex",
            Vec::new(),
            BTreeMap::from([(
                "CODEX_HOME".to_owned(),
                root.join("codex").display().to_string(),
            )]),
        ));
        let codex =
            native_transcript_path(&cfg, HarnessKind::Codex, "thread-9").expect("codex path");
        assert_eq!(
            codex,
            root.join("codex/sessions/2026/01/01/rollout-2026-01-01T00-00-00-thread-9.jsonl")
        );

        cfg.acp = Some(acp_config(
            "antigravity",
            vec![
                "acp".to_owned(),
                "--gemini-dir".to_owned(),
                root.join("ag").display().to_string(),
                "--app-data-dir".to_owned(),
                "antigravity-cli".to_owned(),
            ],
            BTreeMap::new(),
        ));
        let agy =
            native_transcript_path(&cfg, HarnessKind::Antigravity, "conv-7").expect("agy path");
        assert_eq!(agy, root.join("ag/antigravity-cli/conversations/conv-7.db"));

        cfg.acp = Some(acp_config(
            "devin",
            Vec::new(),
            BTreeMap::from([(
                "XDG_DATA_HOME".to_owned(),
                root.join("devin-data").display().to_string(),
            )]),
        ));
        let devin =
            native_transcript_path(&cfg, HarnessKind::Devin, "brave-otter").expect("devin path");
        assert_eq!(devin, root.join("devin-data/devin/cli/sessions.db"));
    }

    #[test]
    fn rejects_path_traversal_in_session_id() {
        let root = PathBuf::from("/tmp/flyco-handoff-traversal");
        let cfg = base_config(&root);
        for bad in ["", ".", "..", "../escape", "a/b", "a\\b", "x\0y"] {
            assert!(
                native_transcript_path(&cfg, HarnessKind::ClaudeCode, bad).is_err(),
                "must reject {bad:?}"
            );
        }
    }
}
