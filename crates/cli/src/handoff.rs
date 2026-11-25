//! `flyco handoff` — same-harness native resume of a local session onto a
//! cloud machine.
//!
//! Shares the exact same native-resume pipeline as the interactive TUI's
//! `/resume-claude`, `/resume-codex`, `/resume-agy`, and `/resume-devin`
//! commands: discovers a local session in the current git worktree, packages
//! the working tree's binary git diff against `origin/<branch>`'s merge-base,
//! snapshots the harness-native transcript (or single-session SQLite database),
//! and creates a same-harness cloud session seeded with that native session id.
//! Cross-harness handoff is not supported.

use std::io::IsTerminal as _;
use std::path::Path;

use flyco_core::HarnessKind;

use crate::cli::SessionSpec;
use crate::client::Api;
use crate::local_sessions::{self, LocalSession, harness_label};
use crate::{Exit, Failure, Outcome, out, pick, tui};

/// Parsed arguments for `flyco handoff`.
#[derive(Debug)]
pub struct Args {
    /// Restrict the local session search to one harness; absent scans all four.
    pub from: Option<HarnessKind>,
    /// Explicit local session id (or unambiguous prefix).
    pub session: Option<String>,
    /// Optional follow-up message sent after resuming natively.
    pub message: Option<String>,
    /// Include untracked-but-unignored files in the binary git patch.
    pub include_untracked: bool,
    /// Shared session-creation flags (`--budget`, `--machine`, …).
    pub spec: SessionSpec,
}

/// Runs `flyco handoff` through the unified same-harness native resume pipeline.
///
/// On a TTY in human mode, attaches straight into the interactive `flyco` TUI
/// once the payloads have uploaded. Under `--json` or on a pipe, prints the
/// created session and exits cleanly.
///
/// # Errors
/// Returns [`Failure`] on usage, discovery, git, snapshot, or API errors.
pub async fn handoff(api: &Api, args: Args, mode: out::Mode) -> Outcome<Exit> {
    if args.spec.prompt.is_some() || args.spec.prompt_file.is_some() {
        return Err(Failure::usage(
            "`flyco handoff` resumes natively — pass an optional follow-up instruction with `--message`",
        ));
    }

    let repo = local_sessions::require_local_repo(
        Path::new("."),
        args.spec.repo.first().map(String::as_str),
        args.spec.branch.as_deref(),
    )
    .await?;
    let session = pick_session(args.from, args.session.as_deref(), &repo.root).await?;
    note(
        mode,
        &format!(
            "resuming local {} session `{}` onto cloud {}…",
            harness_label(session.harness),
            session.session_id,
            harness_label(session.harness)
        ),
    );

    let mut spec = args.spec;
    if interactive() && mode == out::Mode::Human {
        fill_spec(api, &mut spec).await?;
    }
    let created = local_sessions::resume_to_cloud(
        api,
        &session,
        Some(&repo),
        spec.clone(),
        args.message,
        args.include_untracked,
    )
    .await?;

    match mode {
        out::Mode::Json => {
            out::emit(&created)?;
            Ok(Exit::Ok)
        }
        out::Mode::Human if interactive() => {
            tui::run(
                api,
                tui::LaunchOptions {
                    initial_harness: Some(created.summary.harness),
                    initial_session: Some(created),
                    initial_resume_history: false,
                    open_picker: None,
                    spec,
                },
            )
            .await
        }
        out::Mode::Human => {
            out::print(&format!(
                "{}  resumed natively on {} — provisioning",
                created.summary.id,
                harness_label(created.summary.harness)
            ))?;
            Ok(Exit::Ok)
        }
    }
}

async fn pick_session(
    from: Option<HarnessKind>,
    named: Option<&str>,
    root: &Path,
) -> Outcome<LocalSession> {
    let harnesses = [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Devin,
        HarnessKind::Antigravity,
    ];
    if let Some(id) = named {
        for harness in harnesses {
            if from.is_some_and(|wanted| wanted != harness) {
                continue;
            }
            if let Ok(found) = local_sessions::resolve_query(harness, id).await {
                return Ok(found);
            }
        }
        return Err(Failure::problem(
            Exit::NotFoundOrConflict,
            format!("no local session `{id}` found for the selected harness"),
        ));
    }

    let mut candidates = Vec::new();
    for harness in harnesses {
        if from.is_some_and(|wanted| wanted != harness) {
            continue;
        }
        candidates.extend(local_sessions::discover(harness, Some(root)).await?);
    }
    candidates.sort_by_key(|s| std::cmp::Reverse(s.modified));
    match candidates.len() {
        0 => Err(Failure::problem(
            Exit::NotFoundOrConflict,
            "no local claude, codex, devin or antigravity session found in this worktree",
        )),
        1 => Ok(candidates.into_iter().next().expect("len == 1")),
        _ if interactive() => {
            let chosen = pick::pick("Resume which local session", &candidates, |s| {
                format!(
                    "{}  {}  {}",
                    harness_label(s.harness),
                    s.session_id,
                    s.title
                )
            })?;
            Ok(candidates.into_iter().nth(chosen).expect("picked in range"))
        }
        count => Err(Failure::usage(format!(
            "{count} local sessions found — pass `--session` or narrow with `--from`"
        ))),
    }
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

async fn fill_spec(api: &Api, spec: &mut SessionSpec) -> Outcome<()> {
    if spec.machine.is_none() {
        let machine = crate::human::pick_machine(api, spec).await?;
        spec.account = Some(machine.provider_account.to_string());
        spec.machine = Some(machine.machine_type);
        spec.region = Some(machine.region);
        spec.runtime = Some(machine.runtime);
        spec.disk = Some(machine.disk_gib);
        if !machine.spot {
            spec.on_demand = true;
        }
    }
    if spec.budget.is_none() {
        spec.budget = Some(flyco_core::Usd::from_dollars(10));
    }
    Ok(())
}

fn note(mode: out::Mode, text: &str) {
    if mode == out::Mode::Human {
        let _ = out::raw_err(format!("{text}\n").as_bytes());
    }
}
