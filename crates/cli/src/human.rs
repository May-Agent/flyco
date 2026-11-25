//! The human interactive entry points: bare `flyco`, `flyco claude`,
//! `flyco codex`, `flyco devin`, `flyco antigravity`, and `flyco resume`.
//!
//! Every human interactive command enters the full-screen `flyco` TUI
//! ([`crate::tui`]). Non-interactive automation uses `flyco run` and
//! `flyco session …`.

use std::io::IsTerminal as _;

use flyco_core::{
    CreateSession, HarnessKind, MachineCatalog, MachineChoice, MachineDefault, SessionDetail,
    SessionState, SessionSummary,
};

use crate::cli::SessionSpec;
use crate::client::Api;
use crate::tui::{self, LaunchOptions, StartPicker};
use crate::{Exit, Failure, Outcome, out, pick};

/// Bare `flyco`: enters the interactive `flyco` TUI directly in lobby mode.
///
/// From the lobby users can switch harnesses (`/provider`), switch models
/// (`/model`), browse active sessions (`/sessions`), resume inactive sessions
/// (`/resume`), or natively resume local sessions (`/resume-codex`,
/// `/resume-claude`, `/resume-agy`, `/resume-devin`).
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when stdin/stdout is not a terminal or
/// the TUI fails.
pub async fn enter_tui(api: &Api, _mode: out::Mode) -> Outcome<Exit> {
    require_terminal()?;
    tui::run(
        api,
        LaunchOptions {
            initial_harness: None,
            initial_session: None,
            initial_resume_history: false,
            open_picker: None,
            spec: SessionSpec::default(),
        },
    )
    .await
}

/// `flyco claude` / `flyco codex` / `flyco devin` / `flyco antigravity`:
/// opens the interactive TUI configured for `harness`.
///
/// When `--prompt` (or `--prompt-file`) is given alongside a repository, the
/// session is created immediately and opened attached in the TUI; otherwise
/// the TUI opens in lobby mode pre-configured with `harness` and any supplied
/// flags.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when stdin/stdout is not a terminal or
/// session creation fails.
pub async fn launch(
    api: &Api,
    harness: HarnessKind,
    repo_arg: Option<String>,
    spec: &SessionSpec,
    _mode: out::Mode,
) -> Outcome<Exit> {
    require_terminal()?;
    let mut spec = spec.clone();
    if let Some(repo) = repo_arg {
        spec.repo.insert(0, repo);
    }

    if let Some(prompt_result) = crate::session::resolve_prompt(&spec) {
        let prompt = prompt_result?;
        spec.resolve_runtime(api).await?;
        let env = std::mem::take(&mut spec.env);
        let request: CreateSession = spec.to_request(harness, prompt)?;
        let headers: &[(&str, &str)] = match &spec.idempotency_key {
            Some(key) => &[("Idempotency-Key", key.as_str())],
            None => &[],
        };
        let session: SessionDetail = api
            .post_with_headers("/v1/sessions", &request, headers)
            .await?;
        let id = session.summary.id;
        crate::session::set_env(api, &id, &env).await?;
        return tui::run(
            api,
            LaunchOptions {
                initial_harness: Some(harness),
                initial_session: Some(session),
                initial_resume_history: false,
                open_picker: None,
                spec,
            },
        )
        .await;
    }

    tui::run(
        api,
        LaunchOptions {
            initial_harness: Some(harness),
            initial_session: None,
            initial_resume_history: false,
            open_picker: None,
            spec,
        },
    )
    .await
}

/// `flyco resume [<id>|--last]`: re-attaches to an existing cloud session in
/// the interactive TUI, or opens the TUI directly on the `/resume` picker when
/// no session is named.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when stdin/stdout is not a terminal or
/// the named session cannot be found or resumed.
pub async fn resume(api: &Api, id: Option<String>, last: bool, _mode: out::Mode) -> Outcome<Exit> {
    require_terminal()?;
    if id.is_none() && !last {
        return tui::run(
            api,
            LaunchOptions {
                initial_harness: None,
                initial_session: None,
                initial_resume_history: false,
                open_picker: Some(StartPicker::Resume),
                spec: SessionSpec::default(),
            },
        )
        .await;
    }

    let sessions: Vec<SessionSummary> = api.get("/v1/sessions").await?;
    let target = if let Some(id) = id {
        sessions
            .into_iter()
            .find(|s| s.id.to_string() == id)
            .ok_or_else(|| Failure::problem(Exit::NotFoundOrConflict, format!("no session {id}")))?
    } else {
        sessions
            .into_iter()
            .find(|s| s.state != SessionState::Archived)
            .ok_or_else(|| Failure::problem(Exit::NotFoundOrConflict, "no sessions to resume"))?
    };

    let detail = if target.state.is_resumable() {
        api.post::<serde_json::Value, SessionDetail>(
            &format!("/v1/sessions/{}/resume", target.id),
            &serde_json::json!({}),
        )
        .await?
    } else {
        api.get(&format!("/v1/sessions/{}", target.id)).await?
    };

    tui::run(
        api,
        LaunchOptions {
            initial_harness: Some(detail.summary.harness),
            initial_session: Some(detail),
            initial_resume_history: true,
            open_picker: None,
            spec: SessionSpec::default(),
        },
    )
    .await
}

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

/// Machine picker over `GET /v1/machines/catalog`, used by `flyco handoff`
/// when `--machine` is omitted on a TTY.
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
    } else {
        entry.pricing.offers_spot()
    };
    let disk_gib = spec.disk.unwrap_or(flyco_core::DEFAULT_DISK_GIB);
    Ok(MachineChoice {
        provider_account: entry.account.expect("filtered to named accounts"),
        machine_type: entry.machine_type.clone(),
        runtime: entry.runtime,
        region: entry.region.clone(),
        spot,
        disk_gib,
    })
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
