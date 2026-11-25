//! The `flyco` command line, as `clap` sees it.
//!
//! Two surfaces share the one binary: the human path (`claude`, `codex`,
//! `resume`, and the bare command's harness picker) and the agent path —
//! `session …`, `run`, and the discovery commands, which are a faithful
//! projection of `/v1` routes onto flags.

use clap::{Args, Parser, Subcommand};

/// `flyco` — run Claude Code, Codex, Devin and Antigravity sessions on flyco from a terminal.
#[derive(Debug, Parser)]
#[command(name = "flyco", version, about)]
pub struct Cli {
    /// Emit API documents rather than human renderings. Forced whenever
    /// stdout is not a terminal — an agent on a pipe never parses a table.
    #[arg(long, global = true)]
    pub json: bool,

    /// Answer confirmations without asking. Meaningless off a TTY, where
    /// nothing is ever asked.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// The command to run; bare `flyco` is the human harness picker.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Sign this machine in: open the approval page in a browser, wait for
    /// the user to approve it there, and store the issued `fk_` key.
    Login {
        /// Store an existing `fk_` key instead of running the browser
        /// approval flow — the headless path.
        #[arg(long, value_name = "KEY")]
        token: Option<String>,
    },
    /// Sign out: revoke the stored API key and remove the credentials file.
    Logout,
    /// Who the stored credential authenticates as.
    Auth {
        /// The auth subcommand.
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// The machine catalog every linked provider offers.
    Catalog,
    /// Repositories the linked GitHub account can open sessions on.
    Repos,
    /// A repository's branches.
    Branches {
        /// `owner/name`.
        repo: String,
    },
    /// Linked harness accounts and the verified feature matrix.
    Harnesses,
    /// Everything about one session, or the list of them.
    Session {
        /// The session subcommand.
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// One prompt, one session, one turn: create the session, stream its
    /// events as JSONL, and end when the turn does.
    Run {
        /// Which harness drives the session.
        #[arg(long, value_parser = parse_harness)]
        harness: flyco_core::HarnessKind,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
        /// Report the session id and return at once, without watching it.
        #[arg(long)]
        detach: bool,
        /// Stop the session's machine when the turn ends.
        #[arg(long, conflicts_with = "archive")]
        stop: bool,
        /// Archive the session when the turn ends.
        #[arg(long)]
        archive: bool,
        /// Give up waiting after this long (`300`, `5m`, `1h30m`).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        timeout: Option<std::time::Duration>,
    },
    /// Start an interactive Claude Code session over the control-plane ACP/SSE stream.
    Claude {
        /// `owner/name`; asked for when omitted on a TTY.
        #[arg(id = "target_repo", value_name = "REPO")]
        repo: Option<String>,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// Start an interactive Codex session over the control-plane ACP/SSE stream.
    Codex {
        /// `owner/name`; asked for when omitted on a TTY.
        #[arg(id = "target_repo", value_name = "REPO")]
        repo: Option<String>,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// Start an interactive Devin session over the control-plane ACP/SSE stream.
    Devin {
        /// `owner/name`; asked for when omitted on a TTY.
        #[arg(id = "target_repo", value_name = "REPO")]
        repo: Option<String>,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// Start an interactive Antigravity session over the control-plane ACP/SSE stream.
    Antigravity {
        /// `owner/name`; asked for when omitted on a TTY.
        #[arg(id = "target_repo", value_name = "REPO")]
        repo: Option<String>,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// Resume a local harness session natively on a cloud machine running the
    /// same harness: uploads the working tree's binary git diff and the
    /// harness-native session transcript/database directly into the cloud
    /// harness's store. Cross-harness handoff is not supported.
    Handoff {
        /// Restrict discovery to this harness; the default scans all four.
        #[arg(long, value_parser = parse_harness)]
        from: Option<flyco_core::HarnessKind>,
        /// The local session id to resume — Claude's session UUID, a Codex
        /// thread id, a Devin session id, or an Antigravity conversation UUID.
        #[arg(long)]
        session: Option<String>,
        /// An optional follow-up message sent after the session resumes.
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Include untracked (but not ignored) files in the patch.
        #[arg(long)]
        include_untracked: bool,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// Re-attach to an existing session over the control-plane ACP/SSE stream,
    /// re-provisioning its machine when it was away.
    Resume {
        /// The session to re-enter; picked interactively when omitted.
        id: Option<String>,
        /// Re-enter the most recently touched session.
        #[arg(long, conflicts_with = "id")]
        last: bool,
    },
}

/// The `auth` subcommands.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// `GET /v1/me`: the user the credential belongs to.
    Status,
}

/// The `session` subcommands.
#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// `GET /v1/sessions`: every session, newest first.
    List {
        /// Only sessions in this state (`provisioning`, `active`,
        /// `paused`, `interrupted`, `archived`, `failed`).
        #[arg(long, value_parser = parse_state)]
        state: Option<flyco_core::SessionState>,
    },
    /// `GET /v1/sessions/{id}`.
    Get {
        /// The session.
        id: String,
    },
    /// `POST /v1/sessions` — a session is born with a goal; `--prompt` (or
    /// piped stdin) is required.
    Create {
        /// Which harness drives the session.
        #[arg(long, value_parser = parse_harness)]
        harness: flyco_core::HarnessKind,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// `POST …/messages` — say something to the session's agent. A leading
    /// `!` in the text is the shell escape.
    Send {
        /// The session.
        id: String,
        /// The message, inline.
        #[arg(long, short = 'm', conflicts_with_all = ["file", "stdin_flag"])]
        text: Option<String>,
        /// The message, from a file.
        #[arg(long, short = 'f', value_name = "PATH")]
        file: Option<String>,
        /// The message, from stdin (default when stdin is piped).
        #[arg(long = "stdin")]
        stdin_flag: bool,
    },
    /// Run a command on the session's machine — `!cmd` — and relay its
    /// output until it exits.
    Exec {
        /// The session.
        id: String,
        /// The command and its arguments, after `--`.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// The session's recorded event stream, as JSONL.
    Events {
        /// The session.
        id: String,
        /// Start strictly after this sequence position.
        #[arg(long)]
        after: Option<u64>,
        /// Keep emitting as events arrive — riding `/v1/events` and
        /// catching gaps up through `events?after=`.
        #[arg(long, short = 'f')]
        follow: bool,
    },
    /// Block until the session reaches one of the named conditions.
    Wait {
        /// The session.
        id: String,
        /// Comma-separated: `active`, `idle`, `approval`, `paused`,
        /// `stopped`, `failed`, `archived`. The first to occur wins.
        #[arg(long = "for", value_delimiter = ',', required = true)]
        conditions: Vec<String>,
        /// Give up after this long (`300`, `5m`, `1h30m`).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        timeout: Option<std::time::Duration>,
    },
    /// `PATCH /v1/sessions/{id}` — rename, re-budget, re-model, re-mode.
    Set {
        /// The session.
        id: String,
        /// What to call it.
        #[arg(long)]
        title: Option<String>,
        /// New spend limit, in dollars.
        #[arg(long, value_parser = parse_usd)]
        budget: Option<flyco_core::Usd>,
        /// The model to run on.
        #[arg(long)]
        model: Option<String>,
        /// The effort level, meaningful only with `--model`.
        #[arg(long)]
        effort: Option<String>,
        /// The permission mode: `default`, `accept-edits`, `plan`,
        /// `dont-ask`, `auto`, `bypass-permissions`.
        #[arg(long, value_parser = parse_permission_mode)]
        permission_mode: Option<flyco_core::PermissionMode>,
        /// Give the session a desktop the agent can see and drive —
        /// `--computer-use false` takes it away.
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        computer_use: Option<bool>,
    },
    /// Stop the session's machine; disk and session survive.
    Stop {
        /// The session.
        id: String,
    },
    /// Put the session back on a machine (interrupted, paused, archived
    /// and failed sessions resume; an active one is already there).
    Resume {
        /// The session.
        id: String,
    },
    /// Interrupt the turn in flight; the machine stays.
    Interrupt {
        /// The session.
        id: String,
    },
    /// Archive the session — turns kept, environment released.
    Archive {
        /// The session.
        id: String,
        /// Discard uncommitted work. Required when the tree is dirty;
        /// without it a dirty archive is refused.
        #[arg(long)]
        discard_uncommitted: bool,
    },
    /// `GET /v1/approvals?session={id}` — approvals the session raised.
    Approvals {
        /// The session.
        id: String,
        /// Only the ones still waiting.
        #[arg(long)]
        pending: bool,
    },
    /// Decide one of the session's approvals.
    Approve {
        /// The session the approval belongs to (kept for command symmetry;
        /// the route names the approval alone).
        id: String,
        /// The approval.
        approval: String,
        /// Allow it.
        #[arg(long, conflicts_with = "deny")]
        allow: bool,
        /// Refuse it.
        #[arg(long)]
        deny: bool,
    },
    /// `GET …/diff` — the session's working tree against its base branch.
    Diff {
        /// The session.
        id: String,
    },
    /// `GET …/files` — list a directory of the session's checkout.
    Files {
        /// The session.
        id: String,
        /// The directory, relative to the checkout root (default: root).
        path: Option<String>,
    },
    /// `GET …/files/content` — print one file.
    Read {
        /// The session.
        id: String,
        /// The file, relative to the checkout root.
        path: String,
    },
    /// `GET`/`PUT …/env` — the session's environment variables.
    Env {
        /// The session.
        id: String,
        /// `KEY=VALUE` to set (repeatable); without any, prints the set.
        #[arg(long, value_name = "KEY=VALUE")]
        set: Vec<String>,
    },
}

/// The flags every session-creating command shares.
///
/// A flag per picker step: each is asked for interactively only when it is
/// missing *and* stdin is a TTY — on a pipe the missing one is exit 2
/// naming the flag.
#[derive(Debug, Clone, Default, Args)]
pub struct SessionSpec {
    /// `owner/name`; repeatable — each is checked out side by side.
    #[arg(long)]
    pub repo: Vec<String>,
    /// Branch to check out on the first `--repo` (default: the
    /// repository's).
    #[arg(long)]
    pub branch: Option<String>,
    /// The goal the session is born with. Required: a session with nothing
    /// to do is a machine nobody asked for. `-` or piped stdin reads it.
    #[arg(long, short = 'p', conflicts_with = "prompt_file")]
    pub prompt: Option<String>,
    /// The goal, from a file.
    #[arg(long, value_name = "PATH")]
    pub prompt_file: Option<String>,
    /// Spend limit for the whole session, in dollars (`5.00`).
    #[arg(long, value_parser = parse_usd)]
    pub budget: Option<flyco_core::Usd>,
    /// Catalog machine type; `--account` and `--region` belong with it.
    #[arg(long, requires_all = ["account", "region"])]
    pub machine: Option<String>,
    /// The linked provider account to provision through.
    #[arg(long, requires = "machine")]
    pub account: Option<String>,
    /// The catalog region; `--machine` and `--account` belong with it.
    #[arg(long, requires = "machine")]
    pub region: Option<String>,
    /// Machine runtime (`vm` or `container`), resolved from the catalog when
    /// omitted beside `--machine`.
    #[arg(long, requires = "machine", value_parser = parse_runtime)]
    pub runtime: Option<flyco_core::Runtime>,
    /// Ask for interruptible spot capacity (the default).
    #[arg(long, conflicts_with = "on_demand")]
    pub spot: bool,
    /// Ask for on-demand capacity.
    #[arg(long)]
    pub on_demand: bool,
    /// Disk size in GiB.
    #[arg(long, value_name = "GIB")]
    pub disk: Option<u32>,
    /// The model to run on.
    #[arg(long)]
    pub model: Option<String>,
    /// The effort level, meaningful only with `--model`.
    #[arg(long)]
    pub effort: Option<String>,
    /// The permission mode: `default`, `accept-edits`, `plan`,
    /// `dont-ask`, `auto`, `bypass-permissions`.
    #[arg(long, value_parser = parse_permission_mode)]
    pub permission_mode: Option<flyco_core::PermissionMode>,
    /// `KEY=VALUE` environment variable (repeatable).
    #[arg(long, value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Reuse a prior attempt's outcome within its 24-hour window: a retried
    /// `run` or `create` names the same key and gets the same session back
    /// rather than provisioning a second machine.
    #[arg(long, value_name = "KEY")]
    pub idempotency_key: Option<String>,
}

impl SessionSpec {
    /// Builds a [`flyco_core::MachineChoice`] from the machine flags, returning
    /// `None` when none of the machine flags are set and a usage failure on
    /// partial combinations.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when a machine flag is malformed or
    /// only part of the `--machine`/`--account`/`--region` triple was passed.
    pub(crate) fn machine_choice(&self) -> crate::Outcome<Option<flyco_core::MachineChoice>> {
        match (&self.machine, &self.account, &self.region) {
            (Some(machine_type), Some(account), Some(region)) => {
                Ok(Some(flyco_core::MachineChoice {
                    provider_account: account
                        .parse()
                        .map_err(|_| crate::Failure::usage("`--account` is not an account id"))?,
                    machine_type: machine_type.clone(),
                    runtime: self.runtime.unwrap_or(flyco_core::Runtime::Vm),
                    region: region.clone(),
                    spot: !self.on_demand,
                    disk_gib: self.disk.unwrap_or(flyco_core::DEFAULT_DISK_GIB),
                }))
            }
            (None, None, None) => {
                if self.runtime.is_some() {
                    Err(crate::Failure::usage(
                        "`--runtime` needs `--machine`, `--account`, and `--region`",
                    ))
                } else {
                    Ok(None)
                }
            }
            _ => Err(crate::Failure::usage(
                "`--machine`, `--account`, and `--region` go together",
            )),
        }
    }

    /// Resolves `self.runtime` from `GET /v1/machines/catalog` when `--machine`
    /// is specified without `--runtime`.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the machine flags are partial/malformed
    /// or the catalog request fails.
    pub(crate) async fn resolve_runtime(&mut self, api: &crate::client::Api) -> crate::Outcome<()> {
        if self.machine.is_none() || self.runtime.is_some() {
            return Ok(());
        }
        let (Some(machine_type), Some(account), Some(region)) =
            (&self.machine, &self.account, &self.region)
        else {
            return Err(crate::Failure::usage(
                "`--machine`, `--account`, and `--region` go together",
            ));
        };
        let account_id: flyco_core::ProviderAccountId = account
            .parse()
            .map_err(|_| crate::Failure::usage("`--account` is not an account id"))?;
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("account", &account_id.to_string())
            .append_pair("region", region)
            .finish();
        let catalog: flyco_core::MachineCatalog =
            api.get(&format!("/v1/machines/catalog?{query}")).await?;
        if let Some(entry) = catalog.entries.into_iter().find(|entry| {
            entry.account == Some(account_id)
                && entry.machine_type == *machine_type
                && entry.region.eq_ignore_ascii_case(region)
        }) {
            self.runtime = Some(entry.runtime);
        }
        Ok(())
    }

    /// The agent path's `CreateSession` body.
    ///
    /// The spec's fields are `Option` because the human picker fills them
    /// interactively; a non-interactive caller names them all by flag, and
    /// a missing one is usage — never a guess.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when a required flag is missing or malformed.
    pub fn to_request(
        &self,
        harness: flyco_core::HarnessKind,
        prompt: String,
    ) -> Result<flyco_core::CreateSession, crate::Failure> {
        if self.repo.is_empty() {
            return Err(crate::Failure::usage("a session needs --repo owner/name"));
        }
        let repos: Vec<flyco_core::RepoSelection> = self
            .repo
            .iter()
            .enumerate()
            .map(|(index, repo)| flyco_core::RepoSelection {
                repo: repo.clone(),
                branch: if index == 0 {
                    self.branch.clone()
                } else {
                    None
                },
            })
            .collect();
        let budget_limit = self
            .budget
            .ok_or_else(|| crate::Failure::usage("a session needs --budget, in dollars"))?;
        let machine = self.machine_choice()?;
        Ok(flyco_core::CreateSession {
            prompt,
            harness,
            repos,
            budget_limit,
            machine,
            spot: !self.on_demand,
            model: self.model.clone().map(|model| flyco_core::ModelChoice {
                model,
                effort: self.effort.clone(),
            }),
            permission_mode: self.permission_mode,
            source: None,
        })
    }
}

/// `vm`/`container` machine runtimes.
fn parse_runtime(text: &str) -> Result<flyco_core::Runtime, String> {
    match text {
        "vm" => Ok(flyco_core::Runtime::Vm),
        "container" => Ok(flyco_core::Runtime::Container),
        other => Err(format!("`{other}` is not a runtime — `vm` or `container`")),
    }
}

/// `claude`/`codex`/`devin`/`antigravity` spellings of the supported harnesses.
fn parse_harness(text: &str) -> Result<flyco_core::HarnessKind, String> {
    match text {
        "claude" | "claude_code" | "claude-code" => Ok(flyco_core::HarnessKind::ClaudeCode),
        "codex" => Ok(flyco_core::HarnessKind::Codex),
        "devin" => Ok(flyco_core::HarnessKind::Devin),
        "antigravity" | "agy" => Ok(flyco_core::HarnessKind::Antigravity),
        other => Err(format!(
            "`{other}` is not a harness — `claude`, `codex`, `devin` or `antigravity`"
        )),
    }
}

/// `provisioning`/`active`/`paused`/`interrupted`/`archived`/`failed`.
fn parse_state(text: &str) -> Result<flyco_core::SessionState, String> {
    use flyco_core::SessionState as S;
    match text {
        "provisioning" => Ok(S::Provisioning),
        "active" => Ok(S::Active),
        "paused" => Ok(S::Paused),
        "interrupted" => Ok(S::Interrupted),
        "archived" => Ok(S::Archived),
        "failed" => Ok(S::Failed),
        other => Err(format!("`{other}` is not a session state")),
    }
}

/// The permission-mode spellings the SDK uses (`acceptEdits` and friends),
/// with the hyphenated ones a flag would suggest.
fn parse_permission_mode(text: &str) -> Result<flyco_core::PermissionMode, String> {
    use flyco_core::PermissionMode as M;
    match text {
        "default" => Ok(M::Default),
        "accept-edits" | "acceptEdits" | "accept_edits" => Ok(M::AcceptEdits),
        "bypass-permissions" | "bypassPermissions" | "bypass_permissions" => {
            Ok(M::BypassPermissions)
        }
        "plan" => Ok(M::Plan),
        "dont-ask" | "dontAsk" | "dont_ask" => Ok(M::DontAsk),
        "auto" => Ok(M::Auto),
        other => Err(format!("`{other}` is not a permission mode")),
    }
}

/// `5.00` → `Usd` microdollars. A leading `$` is stripped; anything past
/// six decimal places is more precise than the unit itself and refused.
pub(crate) fn parse_usd(text: &str) -> Result<flyco_core::Usd, String> {
    let text = text.strip_prefix('$').unwrap_or(text);
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if fraction.len() > 6 {
        return Err(format!("`{text}` is finer than a microdollar"));
    }
    let dollars: u64 = whole
        .parse()
        .map_err(|_| format!("`{text}` is not a dollar amount"))?;
    let micros = fraction
        .chars()
        .chain("000000".chars())
        .take(6)
        .collect::<String>()
        .parse::<u64>()
        .map_err(|_| format!("`{text}` is not a dollar amount"))?;
    dollars
        .checked_mul(1_000_000)
        .and_then(|d| d.checked_add(micros))
        .map(flyco_core::Usd::from_micros)
        .ok_or_else(|| format!("`{text}` is too large"))
}

/// `300`, `90s`, `5m`, `1h30m` — a duration in seconds, minutes, hours.
fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
    let mut seconds = 0u64;
    let mut digits = String::new();
    for ch in text.chars() {
        match ch {
            '0'..='9' => digits.push(ch),
            's' | 'm' | 'h' => {
                let value: u64 = digits
                    .parse()
                    .map_err(|_| format!("`{text}` is not a duration"))?;
                digits.clear();
                let unit = match ch {
                    's' => 1,
                    'm' => 60,
                    'h' => 3600,
                    _ => unreachable!(),
                };
                seconds += value * unit;
            }
            _ => {
                return Err(format!(
                    "`{text}` is not a duration — try `300`, `5m`, `1h`"
                ));
            }
        }
    }
    if !digits.is_empty() {
        seconds += digits
            .parse::<u64>()
            .map_err(|_| format!("`{text}` is not a duration"))?;
    }
    if seconds == 0 {
        return Err("`0` is not a duration".to_owned());
    }
    Ok(std::time::Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dollar_amount_parses_to_microdollars() {
        assert_eq!(
            parse_usd("5").expect("valid"),
            flyco_core::Usd::from_dollars(5)
        );
        assert_eq!(
            parse_usd("5.00").expect("valid"),
            flyco_core::Usd::from_dollars(5)
        );
        assert_eq!(
            parse_usd("0.50").expect("valid"),
            flyco_core::Usd::from_cents(50)
        );
        assert_eq!(
            parse_usd("$1.25").expect("valid"),
            flyco_core::Usd::from_micros(1_250_000)
        );
        assert!(parse_usd("0.0000001").is_err());
        assert!(parse_usd("later").is_err());
    }

    #[test]
    fn a_duration_parses() {
        assert_eq!(
            parse_duration("300").expect("valid"),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(
            parse_duration("5m").expect("valid"),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(
            parse_duration("1h30m").expect("valid"),
            std::time::Duration::from_mins(90)
        );
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("soon").is_err());
    }

    #[test]
    fn region_and_runtime_require_machine() {
        let region_only = Cli::try_parse_from([
            "flyco",
            "session",
            "create",
            "--harness",
            "claude",
            "--repo",
            "o/r",
            "--budget",
            "5",
            "--prompt",
            "hi",
            "--region",
            "eastus",
        ]);
        assert!(region_only.is_err(), "--region without --machine must fail");

        let runtime_only = Cli::try_parse_from([
            "flyco",
            "session",
            "create",
            "--harness",
            "claude",
            "--repo",
            "o/r",
            "--budget",
            "5",
            "--prompt",
            "hi",
            "--runtime",
            "container",
        ]);
        assert!(
            runtime_only.is_err(),
            "--runtime without --machine must fail"
        );
    }

    #[test]
    fn partial_machine_spec_returns_usage_instead_of_panicking() {
        let spec = SessionSpec {
            repo: vec!["o/r".to_owned()],
            budget: Some(flyco_core::Usd::from_dollars(5)),
            region: Some("eastus".to_owned()),
            ..SessionSpec::default()
        };
        let err = spec
            .to_request(flyco_core::HarnessKind::ClaudeCode, "hi".to_owned())
            .expect_err("partial machine flags must return Failure::usage");
        assert_eq!(err.code, crate::Exit::Usage);

        let runtime_only = SessionSpec {
            repo: vec!["o/r".to_owned()],
            budget: Some(flyco_core::Usd::from_dollars(5)),
            runtime: Some(flyco_core::Runtime::Container),
            ..SessionSpec::default()
        };
        let err = runtime_only
            .to_request(flyco_core::HarnessKind::ClaudeCode, "hi".to_owned())
            .expect_err("runtime without machine must return Failure::usage");
        assert_eq!(err.code, crate::Exit::Usage);
    }

    #[test]
    fn explicit_runtime_flag_populates_machine_choice() {
        let parsed = Cli::try_parse_from([
            "flyco",
            "session",
            "create",
            "--harness",
            "claude",
            "--repo",
            "o/r",
            "--budget",
            "5",
            "--prompt",
            "hi",
            "--machine",
            "Standard_D2s_v5",
            "--account",
            &flyco_core::ProviderAccountId::generate().to_string(),
            "--region",
            "eastus",
            "--runtime",
            "container",
        ])
        .expect("valid flags");
        let Command::Session {
            command: SessionCommand::Create { spec, .. },
        } = parsed.command.expect("subcommand")
        else {
            panic!("expected session create");
        };
        let req = spec
            .to_request(flyco_core::HarnessKind::ClaudeCode, "hi".to_owned())
            .expect("valid request");
        assert_eq!(
            req.machine.expect("machine").runtime,
            flyco_core::Runtime::Container
        );
    }

    #[tokio::test]
    async fn resolve_runtime_picks_catalog_runtime_for_machine() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let account = flyco_core::ProviderAccountId::generate();
        let catalog = flyco_core::MachineCatalog {
            entries: vec![flyco_core::MachineCatalogEntry {
                provider: flyco_core::CloudProviderKind::Azure,
                account: Some(account),
                region: "eastus".to_owned(),
                location: None,
                machine_type: "aca-1c2g".to_owned(),
                runtime: flyco_core::Runtime::Container,
                free_grant: None,
                os: flyco_core::OsFamily::Linux,
                capacity: Some(flyco_core::MachineCapacity {
                    vcpus: 1,
                    memory_mib: 2048,
                }),
                lineage: None,
                pricing: flyco_core::MachinePricing::Metered {
                    on_demand_hourly: flyco_core::Usd::from_cents(5),
                    spot_hourly: None,
                    minimum: None,
                    storage: flyco_core::StoragePricing::PerGibHourly {
                        rate: flyco_core::Usd::from_micros(0),
                    },
                },
            }],
            pending_accounts: Vec::new(),
        };
        let body = serde_json::to_string(&catalog).expect("serialize");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("port").port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut req = Vec::new();
                let mut buf = [0_u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let base = url::Url::parse(&format!("http://127.0.0.1:{port}")).expect("url");
        let api = crate::client::Api::new(base, None);
        let mut spec = SessionSpec {
            repo: vec!["o/r".to_owned()],
            budget: Some(flyco_core::Usd::from_dollars(5)),
            machine: Some("aca-1c2g".to_owned()),
            account: Some(account.to_string()),
            region: Some("eastus".to_owned()),
            ..SessionSpec::default()
        };
        spec.resolve_runtime(&api).await.expect("resolve");
        assert_eq!(spec.runtime, Some(flyco_core::Runtime::Container));
        let req = spec
            .to_request(flyco_core::HarnessKind::ClaudeCode, "hi".to_owned())
            .expect("request");
        assert_eq!(
            req.machine.expect("machine").runtime,
            flyco_core::Runtime::Container
        );
    }

    #[test]
    fn antigravity_harness_and_subcommand_parse() {
        assert_eq!(
            parse_harness("antigravity").expect("antigravity"),
            flyco_core::HarnessKind::Antigravity
        );
        assert_eq!(
            parse_harness("agy").expect("agy"),
            flyco_core::HarnessKind::Antigravity
        );
        let parsed = Cli::try_parse_from(["flyco", "antigravity", "lexoliu/flyco"])
            .expect("flyco antigravity parses");
        assert!(matches!(
            parsed.command,
            Some(Command::Antigravity { repo: Some(ref slug), .. }) if slug == "lexoliu/flyco"
        ));
    }
}
