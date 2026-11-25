//! Slash command registry for the `flyco` interactive TUI.
//!
//! One authoritative table drives the live `/` completion popover, argument
//! parsing, command dispatch, and the `/help` overlay.

/// One slash command supported by the interactive `flyco` TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    /// Browse and attach to an active cloud session (`provisioning`, `active`).
    Sessions,
    /// Browse and resume an inactive cloud session (`paused`, `interrupted`, `archived`, `failed`).
    Resume,
    /// Switch the model (and effort) for the active harness or live session.
    Model,
    /// Switch the active harness (`claude`, `codex`, `devin`, `antigravity`).
    Provider,
    /// Discover and natively resume a local Codex conversation on cloud Codex.
    ResumeCodex,
    /// Discover and natively resume a local Claude Code conversation on cloud Claude.
    ResumeClaude,
    /// Discover and natively resume a local Antigravity conversation on cloud Antigravity.
    ResumeAgy,
    /// Discover and natively resume a local Devin conversation on cloud Devin.
    ResumeDevin,
    /// Pick target repository and branch for the next session.
    Repo,
    /// Pick cloud machine, region, and spot mode for the next session.
    Machine,
    /// Set budget limit in USD (updates live session or next session).
    Budget,
    /// Detach into a fresh lobby draft without stopping the cloud session.
    New,
    /// Interrupt the currently running turn.
    Interrupt,
    /// Compact conversation history on the active session.
    Compact,
    /// Query context window usage on the active session.
    Context,
    /// Print detailed session, machine, budget, and swarm status.
    Status,
    /// Detach from the current session back to the lobby.
    Detach,
    /// Toggle the help overlay.
    Help,
    /// Exit the `flyco` CLI (cloud sessions keep running).
    Quit,
}

/// Display metadata for one [`SlashCommand`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// The command variant.
    pub command: SlashCommand,
    /// Canonical slash name including leading `/`.
    pub name: &'static str,
    /// Optional argument signature hint.
    pub args: &'static str,
    /// Concise action label.
    pub summary: &'static str,
}

impl SlashCommand {
    /// Ordered table of all TUI slash commands.
    pub const ALL: [Self; 19] = [
        Self::Sessions,
        Self::Resume,
        Self::Model,
        Self::Provider,
        Self::ResumeCodex,
        Self::ResumeClaude,
        Self::ResumeAgy,
        Self::ResumeDevin,
        Self::Repo,
        Self::Machine,
        Self::Budget,
        Self::New,
        Self::Interrupt,
        Self::Compact,
        Self::Context,
        Self::Status,
        Self::Detach,
        Self::Help,
        Self::Quit,
    ];

    /// Specification row for this command.
    #[must_use]
    pub const fn spec(self) -> CommandSpec {
        let (name, args, summary) = match self {
            Self::Sessions => ("/sessions", "", "Active cloud sessions"),
            Self::Resume => ("/resume", "", "Inactive cloud sessions"),
            Self::Model => ("/model", "[query]", "Switch model"),
            Self::Provider => ("/provider", "[harness]", "Switch harness"),
            Self::ResumeCodex => ("/resume-codex", "[query]", "Resume local Codex session"),
            Self::ResumeClaude => ("/resume-claude", "[query]", "Resume local Claude session"),
            Self::ResumeAgy => ("/resume-agy", "[query]", "Resume local Antigravity session"),
            Self::ResumeDevin => ("/resume-devin", "[query]", "Resume local Devin session"),
            Self::Repo => ("/repo", "[owner/repo]", "Select repository"),
            Self::Machine => ("/machine", "", "Select cloud machine"),
            Self::Budget => ("/budget", "<usd>", "Set budget limit"),
            Self::New => ("/new", "", "New session lobby"),
            Self::Interrupt => ("/interrupt", "", "Interrupt current turn"),
            Self::Compact => ("/compact", "", "Compact conversation"),
            Self::Context => ("/context", "", "Context window usage"),
            Self::Status => ("/status", "", "Session & swarm status"),
            Self::Detach => ("/detach", "", "Detach to lobby"),
            Self::Help => ("/help", "", "Commands & keybindings"),
            Self::Quit => ("/quit", "", "Exit flyco"),
        };
        CommandSpec {
            command: self,
            name,
            args,
            summary,
        }
    }

    /// Canonical command name including leading `/`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.spec().name
    }

    /// Exact lookup by slash command name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|cmd| cmd.name() == name)
    }

    /// Commands matching `query` (with or without leading `/`), prefix hits
    /// ranked ahead of substring hits.
    #[must_use]
    pub fn filter(query: &str) -> Vec<CommandSpec> {
        let needle = query.trim().trim_start_matches('/').to_ascii_lowercase();
        let mut prefix_hits = Vec::new();
        let mut other_hits = Vec::new();
        for cmd in Self::ALL {
            let spec = cmd.spec();
            let bare = spec.name.trim_start_matches('/');
            if needle.is_empty() || bare.starts_with(&needle) {
                prefix_hits.push(spec);
            } else if bare.contains(&needle) || spec.summary.to_ascii_lowercase().contains(&needle)
            {
                other_hits.push(spec);
            }
        }
        prefix_hits.extend(other_hits);
        prefix_hits
    }
}

/// Parses a submitted composer line as a slash command plus trimmed arguments.
#[must_use]
pub fn parse_slash(line: &str) -> Option<(SlashCommand, &str)> {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let (head, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.trim()),
        None => (trimmed, ""),
    };
    let cmd = SlashCommand::from_name(&head.to_ascii_lowercase())?;
    Some((cmd, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_commands_have_unique_slash_names() {
        for (idx, cmd) in SlashCommand::ALL.into_iter().enumerate() {
            assert!(cmd.name().starts_with('/'));
            assert_eq!(SlashCommand::from_name(cmd.name()), Some(cmd));
            assert_eq!(SlashCommand::ALL[idx], cmd);
        }
    }

    #[test]
    fn filter_ranks_prefix_matches_first() {
        let matches = SlashCommand::filter("/resume");
        assert_eq!(matches[0].command, SlashCommand::Resume);
        assert!(
            matches
                .iter()
                .any(|m| m.command == SlashCommand::ResumeCodex)
        );
        assert!(
            matches
                .iter()
                .any(|m| m.command == SlashCommand::ResumeClaude)
        );
        assert!(matches.iter().any(|m| m.command == SlashCommand::ResumeAgy));
        assert!(
            matches
                .iter()
                .any(|m| m.command == SlashCommand::ResumeDevin)
        );
    }

    #[test]
    fn parse_slash_extracts_command_and_arguments() {
        assert_eq!(
            parse_slash("/model sonnet-4.6"),
            Some((SlashCommand::Model, "sonnet-4.6"))
        );
        assert_eq!(
            parse_slash("/resume-codex 01a1a85c"),
            Some((SlashCommand::ResumeCodex, "01a1a85c"))
        );
        assert_eq!(parse_slash("plain prompt"), None);
    }
}
