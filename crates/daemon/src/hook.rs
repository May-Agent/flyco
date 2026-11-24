//! `flycod hook` — managed `PreToolUse` backstop for Claude Code.
//!
//! Claude Code's `/etc/claude-code/managed-settings.json` pairs absolute-path
//! `permissions.deny` rules with a `PreToolUse` command hook pointing at
//! `flycod hook pre-tool-use`. Even in `auto` or `bypassPermissions` mode,
//! Claude Code invokes this hook with the tool payload on stdin; exiting with
//! code `2` blocks the tool call and hands stderr back to the model as the
//! refusal reason.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// Exit code Claude Code treats as a blocking hook refusal.
pub const BLOCK_EXIT_CODE: u8 = 2;

/// Paths the agent may neither read nor write (daemon secrets and state).
const NO_ACCESS_PREFIXES: &[&str] = &["/etc/flycod", "/var/lib/flyco/transcripts"];

/// Paths the agent may read (policy/runtime) but must never modify.
const READ_ONLY_PREFIXES: &[&str] = &[
    "/etc/claude-code",
    "/var/lib/flyco/codex",
    "/var/lib/flyco/devin-config",
];

/// Partial Claude Code `PreToolUse` hook stdin payload.
#[derive(Debug, Deserialize)]
struct HookInput {
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: serde_json::Value,
}

/// Decision made by [`evaluate_pre_tool_use`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// Permit the tool call (exit 0).
    Allow,
    /// Block the tool call (exit 2) with the given reason on stderr.
    Block(String),
}

/// Evaluates one JSON `PreToolUse` payload from stdin.
#[must_use]
pub fn evaluate_pre_tool_use(payload: &str) -> HookDecision {
    let Ok(input) = serde_json::from_str::<HookInput>(payload) else {
        return HookDecision::Block(
            "flyco managed PreToolUse backstop rejected a malformed hook payload".to_owned(),
        );
    };

    match input.tool_name.as_str() {
        "Read" => check_file_tool(&input.tool_input, false),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => check_file_tool(&input.tool_input, true),
        "Bash" => check_bash_tool(&input.tool_input),
        _ => HookDecision::Allow,
    }
}

/// Reads stdin and runs the `PreToolUse` backstop, returning the process exit
/// code (`0` to allow, `2` to block).
#[must_use]
pub fn run_pre_tool_use() -> u8 {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        eprintln!("flyco managed PreToolUse backstop could not read hook stdin");
        return BLOCK_EXIT_CODE;
    }
    match evaluate_pre_tool_use(&buf) {
        HookDecision::Allow => 0,
        HookDecision::Block(reason) => {
            eprintln!("{reason}");
            BLOCK_EXIT_CODE
        }
    }
}

fn check_file_tool(tool_input: &serde_json::Value, mutating: bool) -> HookDecision {
    let candidate = tool_input
        .get("file_path")
        .or_else(|| tool_input.get("path"))
        .or_else(|| tool_input.get("notebook_path"))
        .and_then(serde_json::Value::as_str);

    let Some(raw) = candidate else {
        return HookDecision::Allow;
    };
    let normalized = normalize_path(Path::new(raw));
    for prefix in NO_ACCESS_PREFIXES {
        if normalized.starts_with(prefix) {
            return HookDecision::Block(format!(
                "blocked by flyco managed policy: `{prefix}` is daemon-only and not accessible to the agent"
            ));
        }
    }
    if mutating {
        for prefix in READ_ONLY_PREFIXES {
            if normalized.starts_with(prefix) {
                return HookDecision::Block(format!(
                    "blocked by flyco managed policy: `{prefix}` is managed read-only configuration"
                ));
            }
        }
    }
    HookDecision::Allow
}

fn check_bash_tool(tool_input: &serde_json::Value) -> HookDecision {
    let Some(command) = tool_input
        .get("command")
        .and_then(serde_json::Value::as_str)
    else {
        return HookDecision::Allow;
    };
    for prefix in NO_ACCESS_PREFIXES {
        if command.contains(prefix) {
            return HookDecision::Block(format!(
                "blocked by flyco managed policy: shell commands may not access `{prefix}`"
            ));
        }
    }
    for prefix in READ_ONLY_PREFIXES {
        if command.contains(prefix) && looks_mutating(command) {
            return HookDecision::Block(format!(
                "blocked by flyco managed policy: shell commands may not modify `{prefix}`"
            ));
        }
    }
    HookDecision::Allow
}

/// Conservative check for shell commands that attempt to write, unlink, move,
/// or re-permission a path.
fn looks_mutating(command: &str) -> bool {
    const MUTATORS: &[&str] = &[
        ">",
        "tee ",
        "rm ",
        "mv ",
        "cp ",
        "chmod ",
        "chown ",
        "ln ",
        "sed -i",
        "perl -i",
        "truncate ",
        "install ",
        "mkdir ",
        "rmdir ",
        "touch ",
        "dd ",
        "python",
        "node ",
        "bun ",
    ];
    MUTATORS.iter().any(|token| command.contains(token))
}

/// Resolves `.` and `..` lexically (and symlinks when the path exists) so
/// `/tmp/../etc/flycod/config.toml` cannot slip past prefix checks.
fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{HookDecision, evaluate_pre_tool_use};

    #[test]
    fn blocks_reads_and_writes_to_daemon_config_including_traversal() {
        let read = json!({
            "tool_name": "Read",
            "tool_input": { "file_path": "/etc/flycod/config.toml" }
        });
        assert!(matches!(
            evaluate_pre_tool_use(&read.to_string()),
            HookDecision::Block(_)
        ));

        let traversal = json!({
            "tool_name": "Read",
            "tool_input": { "file_path": "/srv/flyco/work/../../../etc/flycod/config.toml" }
        });
        assert!(matches!(
            evaluate_pre_tool_use(&traversal.to_string()),
            HookDecision::Block(_)
        ));

        let bash = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cat /etc/flycod/config.toml" }
        });
        assert!(matches!(
            evaluate_pre_tool_use(&bash.to_string()),
            HookDecision::Block(_)
        ));
    }

    #[test]
    fn blocks_mutations_to_managed_policy_dirs_while_allowing_workdir() {
        let edit_policy = json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": "/etc/claude-code/managed-settings.json" }
        });
        assert!(matches!(
            evaluate_pre_tool_use(&edit_policy.to_string()),
            HookDecision::Block(_)
        ));

        let write_codex = json!({
            "tool_name": "Write",
            "tool_input": { "file_path": "/var/lib/flyco/codex/config.toml" }
        });
        assert!(matches!(
            evaluate_pre_tool_use(&write_codex.to_string()),
            HookDecision::Block(_)
        ));

        let bash_rm = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "rm -f /var/lib/flyco/devin-config/devin/mcp_config.json" }
        });
        assert!(matches!(
            evaluate_pre_tool_use(&bash_rm.to_string()),
            HookDecision::Block(_)
        ));

        let workdir_edit = json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": "/srv/flyco/work/flyco/src/lib.rs" }
        });
        assert_eq!(
            evaluate_pre_tool_use(&workdir_edit.to_string()),
            HookDecision::Allow
        );
    }

    #[test]
    fn malformed_payload_fails_closed() {
        assert!(matches!(
            evaluate_pre_tool_use("not json"),
            HookDecision::Block(_)
        ));
    }
}
