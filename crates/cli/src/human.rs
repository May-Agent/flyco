//! The human path: `flyco claude`, `flyco codex`, `flyco devin`,
//! `flyco antigravity`, `flyco resume`, and the bare command's harness picker.
//!
//! One orchestration for all four harnesses: resolve any missing flags via
//! interactive pickers, create or resume the session, and attach the local
//! terminal to the control-plane ACP/SSE conversation interface via
//! [`crate::interactive`].
//!
//! The path is TTY-only by definition: interactive pickers and the live prompt
//! require a terminal. The agent surface — `flyco run`, `flyco session …` — is
//! the non-interactive one.

use std::io::IsTerminal as _;

use flyco_core::{
    CreateSession, HarnessKind, MachineCatalog, MachineChoice, MachineDefault, RepoSummary,
    SessionDetail, SessionState, SessionSummary,
};

use crate::cli::SessionSpec;
use crate::client::Api;
use crate::interactive;
use crate::{Exit, Failure, Outcome, out, pick};

/// `flyco claude` / `flyco codex` / `flyco devin` / `flyco antigravity`:
/// create a session and attach to its control-plane ACP/SSE stream.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when stdin/stdout is not a terminal, a
/// request fails, or the interactive stream terminates with a fatal error.
pub async fn launch(
    api: &Api,
    harness: HarnessKind,
    repo_arg: Option<String>,
    spec: &SessionSpec,
    mode: out::Mode,
) -> Outcome<Exit> {
    require_terminal()?;
    let (request, env) = resolve(api, harness, repo_arg, spec).await?;
    let headers: &[(&str, &str)] = match &spec.idempotency_key {
        Some(key) => &[("Idempotency-Key", key.as_str())],
        None => &[],
    };
    let session: SessionDetail = api
        .post_with_headers("/v1/sessions", &request, headers)
        .await?;
    let id = session.summary.id;
    crate::session::set_env(api, &id, &env).await?;
    interactive::attach(api, session, false, mode).await
}

/// `flyco resume [<id>|--last]`: re-attach to an existing session over the
/// control-plane ACP/SSE stream, re-provisioning its machine first when away.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when stdin/stdout is not a terminal,
/// the session cannot be found or resumed, or the interactive stream fails.
pub async fn resume(api: &Api, id: Option<String>, last: bool, mode: out::Mode) -> Outcome<Exit> {
    require_terminal()?;
    let session = find_session(api, id, last).await?;
    let detail = if session.state.is_resumable() {
        note(mode, "putting the session back on a machine");
        api.post::<serde_json::Value, SessionDetail>(
            &format!("/v1/sessions/{}/resume", session.id),
            &serde_json::json!({}),
        )
        .await?
    } else {
        api.get(&format!("/v1/sessions/{}", session.id)).await?
    };
    interactive::attach(api, detail, true, mode).await
}

/// Bare `flyco`: the harness picker, then the launch flow.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when stdin/stdout is not a terminal, a
/// request fails, or the interactive stream terminates with a fatal error.
pub async fn pick_harness(api: &Api, mode: out::Mode) -> Outcome<Exit> {
    require_terminal()?;
    let harnesses = [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Devin,
        HarnessKind::Antigravity,
    ];
    let chosen = pick::pick("Harness", &harnesses, |harness| match harness {
        HarnessKind::ClaudeCode => "Claude Code".to_owned(),
        HarnessKind::Codex => "Codex".to_owned(),
        HarnessKind::Devin => "Devin".to_owned(),
        HarnessKind::Antigravity => "Antigravity".to_owned(),
    })?;
    launch(api, harnesses[chosen], None, &SessionSpec::default(), mode).await
}

/// An interactive command with no terminal is a category error, not a missing
/// flag: point at the non-interactive surface rather than asking a pipe
/// questions.
fn require_terminal() -> Outcome<()> {
    require_terminal_streams(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    )
}

pub(crate) fn require_terminal_streams(stdin_is_tty: bool, stdout_is_tty: bool) -> Outcome<()> {
    if stdin_is_tty && stdout_is_tty {
        return Ok(());
    }
    Err(Failure::usage(
        "interactive sessions need a terminal — `flyco run` and `flyco session …` are the non-interactive surface",
    ))
}

/// The session `resume` names: explicit id, `--last`, or a picker over
/// the account's sessions.
async fn find_session(api: &Api, id: Option<String>, last: bool) -> Outcome<SessionSummary> {
    let sessions: Vec<SessionSummary> = api.get("/v1/sessions").await?;
    if let Some(id) = id {
        return sessions
            .into_iter()
            .find(|session| session.id.to_string() == id)
            .ok_or_else(|| Failure::problem(Exit::NotFoundOrConflict, format!("no session {id}")));
    }
    let resumable: Vec<SessionSummary> = sessions
        .into_iter()
        .filter(|session| session.state != SessionState::Archived)
        .collect();
    if resumable.is_empty() {
        return Err(Failure::problem(
            Exit::NotFoundOrConflict,
            "no sessions to resume",
        ));
    }
    if last {
        // The list answers newest-first.
        return Ok(resumable.into_iter().next().expect("non-empty"));
    }
    let chosen = pick::pick("Resume which session", &resumable, |session| {
        format!(
            "{}  {}  {}",
            state_of(session),
            crate::session::repos_label(&session.repos),
            session.title
        )
    })?;
    Ok(resumable.into_iter().nth(chosen).expect("picked in range"))
}

/// A session's state as the picker shows it.
fn state_of(session: &SessionSummary) -> String {
    serde_json::to_value(session.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The picker chain for a new session — every step a flag can bypass.
///
/// Order is repo → branch → machine → spot → disk → budget → env → goal.
/// Each answer comes from `spec` when the flag was given and is asked for
/// otherwise; nothing here ever runs off a TTY.
async fn resolve(
    api: &Api,
    harness: HarnessKind,
    repo_arg: Option<String>,
    spec: &SessionSpec,
) -> Outcome<(CreateSession, Vec<String>)> {
    let mut slugs = spec.repo.clone();
    if let Some(repo) = repo_arg {
        slugs.insert(0, repo);
    }
    if slugs.is_empty() {
        slugs.push(pick_repo(api).await?);
    }
    let branch = match &spec.branch {
        Some(branch) => Some(branch.clone()),
        None => pick_branch(api, &slugs[0]).await?,
    };
    let repos = slugs
        .into_iter()
        .enumerate()
        .map(|(index, repo)| flyco_core::RepoSelection {
            repo,
            branch: if index == 0 { branch.clone() } else { None },
        })
        .collect();
    let mut spec = spec.clone();
    spec.resolve_runtime(api).await?;
    let machine = match spec.machine_choice()? {
        Some(machine) => Some(machine),
        None => Some(pick_machine(api, &spec).await?),
    };
    let budget_limit = if let Some(budget) = spec.budget {
        budget
    } else {
        let text = pick::input("Budget for the session, in dollars", Some("10"))?;
        crate::cli::parse_usd(&text).map_err(Failure::usage)?
    };
    let mut env = spec.env.clone();
    if env.is_empty() {
        let text = pick::input(
            "Extra env (KEY=VALUE, comma-separated; empty for none)",
            None,
        )?;
        if !text.trim().is_empty() {
            env = text.split(',').map(|pair| pair.trim().to_owned()).collect();
        }
    }
    let prompt = match crate::session::resolve_prompt(&spec) {
        Some(prompt) => prompt?,
        None => pick::input("What should it do", None)?,
    };
    Ok((
        CreateSession {
            prompt,
            harness,
            repos,
            budget_limit,
            machine,
            spot: !spec.on_demand,
            model: spec.model.clone().map(|model| flyco_core::ModelChoice {
                model,
                effort: spec.effort.clone(),
            }),
            permission_mode: spec.permission_mode,
            source: None,
        },
        env,
    ))
}

/// Repo picker over `GET /v1/github/repos`.
async fn pick_repo(api: &Api) -> Outcome<String> {
    let repos: Vec<RepoSummary> = api.get("/v1/github/repos").await?;
    if repos.is_empty() {
        return Err(Failure::problem(
            Exit::Problem,
            "no repositories — link a GitHub account in flyco first",
        ));
    }
    let chosen = pick::pick("Repository", &repos, |repo| {
        format!(
            "{}{}{}",
            repo.slug,
            if repo.private { " (private)" } else { "" },
            repo.description
                .as_ref()
                .map_or(String::new(), |d| format!(" — {d}")),
        )
    })?;
    Ok(repos[chosen].slug.to_string())
}

/// Branch picker; the repository's default is the preselected row.
///
/// `None` means "the default" — the request omits the branch and the
/// control plane resolves it, so a picker answer of "the usual one" never
/// freezes a name the repo might change.
async fn pick_branch(api: &Api, repo: &str) -> Outcome<Option<String>> {
    let page: flyco_core::BranchPage = api
        .get(&format!("/v1/github/repos/{repo}/branches"))
        .await?;
    if page.branches.is_empty() {
        return Ok(None);
    }
    let mut names: Vec<String> = vec!["(default)".to_owned()];
    names.extend(page.branches.iter().map(|branch| branch.name.to_string()));
    let chosen = pick::pick("Branch", &names, std::clone::Clone::clone)?;
    Ok((chosen > 0).then(|| names[chosen].clone()))
}

/// Machine picker over `GET /v1/machines/catalog`, with
/// `GET /v1/machines/default`'s answer preselected.
///
/// Also answers the spot and disk questions, since they belong to the
/// machine choice: spot only exists where the entry offers it, and the
/// disk default is the platform's.
pub(crate) async fn pick_machine(api: &Api, spec: &SessionSpec) -> Outcome<MachineChoice> {
    let catalog: MachineCatalog = api.get("/v1/machines/catalog").await?;
    let default: Option<MachineDefault> = api.get("/v1/machines/default").await.ok();
    let entries: Vec<&flyco_core::MachineCatalogEntry> = catalog
        .entries
        .iter()
        .filter(|entry| entry.account.is_some())
        .collect();
    if entries.is_empty() {
        return Err(Failure::problem(
            Exit::Problem,
            "no machines in the catalog — link a provider account in flyco first",
        ));
    }
    let is_default = |entry: &flyco_core::MachineCatalogEntry| {
        default.as_ref().is_some_and(|d| {
            entry.account == Some(d.choice.provider_account)
                && entry.machine_type == d.choice.machine_type
                && entry.region == d.choice.region
        })
    };
    let mut ordered: Vec<&flyco_core::MachineCatalogEntry> = Vec::new();
    ordered.extend(entries.iter().copied().filter(|entry| is_default(entry)));
    ordered.extend(entries.iter().copied().filter(|entry| !is_default(entry)));
    let chosen = pick::pick("Machine", &ordered, |entry| {
        let size = entry.capacity.as_ref().map_or_else(
            || "?".to_owned(),
            |capacity| format!("{}c/{}GiB", capacity.vcpus, capacity.memory_mib / 1024),
        );
        let price = crate::discover::pricing(entry);
        format!(
            "{}  {}  {}  {}",
            entry.machine_type, entry.region, size, price
        )
    })?;
    let entry = ordered[chosen];

    let spot = if spec.spot {
        true
    } else if spec.on_demand {
        false
    } else if entry.pricing.offers_spot() {
        pick::confirm("Spot capacity", true)?
    } else {
        false
    };
    let disk_gib = if let Some(disk) = spec.disk {
        disk
    } else {
        let text = pick::input("Disk, GiB", Some(&flyco_core::DEFAULT_DISK_GIB.to_string()))?;
        text.trim()
            .parse::<u32>()
            .map_err(|_| Failure::usage(format!("`{text}` is not a disk size")))?
    };
    Ok(MachineChoice {
        provider_account: entry.account.expect("filtered to named accounts"),
        machine_type: entry.machine_type.clone(),
        runtime: entry.runtime,
        region: entry.region.clone(),
        spot,
        disk_gib,
    })
}

/// A progress line on stderr — JSON mode keeps stdout for documents alone.
fn note(mode: out::Mode, text: &str) {
    if mode == out::Mode::Human {
        let _ = out::raw_err(format!("{text}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_tty_streams_are_rejected_with_usage_exit_code() {
        assert!(require_terminal_streams(true, true).is_ok());
        for (stdin, stdout) in [(false, true), (true, false), (false, false)] {
            let err = require_terminal_streams(stdin, stdout)
                .expect_err("non-TTY stream must fail with usage");
            assert_eq!(err.code, Exit::Usage);
            assert!(err.text.contains("flyco run"));
        }
    }
}
