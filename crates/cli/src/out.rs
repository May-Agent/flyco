//! How a command's answer reaches stdout.
//!
//! The contract is one line in the issue and one flag in the parser:
//! **`--json` forces JSON; a non-TTY stdout is JSON anyway; a TTY gets
//! the human rendering.** An agent that forgets the flag on a pipe never
//! parses a table, and a person at a terminal never reads a document they
//! did not ask for.

use std::io::{IsTerminal as _, Write as _};

use serde::Serialize;

use crate::Failure;

/// Whether stdout renders for a person or a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// API DTOs, serialized. Forced by `--json`, and the default whenever
    /// stdout is not a terminal.
    Json,
    /// Tables and prose. Only ever on a TTY without `--json`.
    Human,
}

impl Mode {
    /// Resolves the flag against what stdout is.
    #[must_use]
    pub fn resolve(json: bool) -> Self {
        if json || !std::io::stdout().is_terminal() {
            Self::Json
        } else {
            Self::Human
        }
    }
}

/// Writes one value as a JSON object followed by a newline.
///
/// The single-shot commands' whole output — an agent pipes it to `jq`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the value does not serialize or the write fails.
pub fn emit<T: Serialize>(value: &T) -> crate::Outcome<()> {
    let line = serde_json::to_string(value)
        .map_err(|error| Failure::usage(format!("a result failed to serialize: {error}")))?;
    line_out(&line)
}

/// Writes one value as one line of a JSONL stream.
///
/// `session events --follow` and `flyco run` emit a stream; each element is
/// self-contained, so a consumer may cut the stream anywhere.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the value does not serialize or the write fails.
pub fn emit_line<T: Serialize>(value: &T) -> crate::Outcome<()> {
    emit(value)
}

/// Writes one already-serialized line.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the value does not serialize or the write fails.
pub fn line_out(text: &str) -> crate::Outcome<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
        .map_err(|error| Failure::transport(format!("stdout: {error}")))
}

/// Writes raw bytes to stderr — relayed shell output, where stdout is
/// reserved for the data stream.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the value does not serialize or the write fails.
pub fn raw_err(bytes: &[u8]) -> crate::Outcome<()> {
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    err.write_all(bytes)
        .and_then(|()| err.flush())
        .map_err(|error| Failure::transport(format!("stderr: {error}")))
}

/// Writes raw bytes without adding a newline or altering encoding — relayed
/// shell output and file contents.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the value does not serialize or the write fails.
pub fn raw_out(bytes: &[u8]) -> crate::Outcome<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(bytes)
        .and_then(|()| out.flush())
        .map_err(|error| Failure::transport(format!("stdout: {error}")))
}

/// Writes human-facing text, exactly as given.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the value does not serialize or the write fails.
pub fn print(text: &str) -> crate::Outcome<()> {
    line_out(text)
}

/// A simple aligned table for the human renderings.
///
/// Deliberately plain — spaces, no borders — because the human surface is
/// the TTY's own typography and anything drawn here would only get in the
/// way of it.
#[must_use]
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    let mut out = String::new();
    let header = headers
        .iter()
        .enumerate()
        .map(|(index, head)| pad(head, widths[index]))
        .collect::<Vec<_>>()
        .join(" ");
    out.push_str(header.trim_end());
    out.push('\n');
    for row in rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(index, cell)| pad(cell, widths[index]))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim_end_matches('\n').to_owned()
}

fn pad(cell: &str, width: usize) -> String {
    let len = cell.chars().count();
    if len >= width {
        cell.to_owned()
    } else {
        format!("{cell}{}", " ".repeat(width - len))
    }
}

/// Formats a read-only swarm update (snapshot and/or message-board batch) for
/// terminal display.
#[must_use]
pub fn format_swarm_update(
    snapshot: Option<&flyco_core::SwarmSnapshot>,
    board_messages: &[flyco_core::SwarmBoardMessage],
) -> String {
    let mut sections = Vec::new();
    if let Some(snapshot) = snapshot {
        let mut lines = Vec::new();
        if let Some(title) = &snapshot.title {
            lines.push(format!("Swarm: {title}"));
        } else if let Some(id) = &snapshot.swarm_id {
            lines.push(format!("Swarm: {id}"));
        }
        if let Some(goal) = &snapshot.goal {
            match &goal.line {
                Some(line) => lines.push(format!("Goal: {} — {line}", goal.title)),
                None => lines.push(format!("Goal: {}", goal.title)),
            }
        }
        if !snapshot.threads.is_empty() {
            let mut thread_lines = Vec::new();
            for thread in &snapshot.threads {
                let members = thread.tasks.join(", ");
                if members.is_empty() {
                    thread_lines.push(format!("  [{}] {}", thread.id, thread.title));
                } else {
                    thread_lines.push(format!("  [{}] {} ({members})", thread.id, thread.title));
                }
            }
            lines.push(format!("Threads:\n{}", thread_lines.join("\n")));
        }
        if !snapshot.members.is_empty() {
            let rows: Vec<Vec<String>> = snapshot
                .members
                .iter()
                .map(|member| {
                    let status = if member.killed {
                        "killed".to_owned()
                    } else {
                        member
                            .phase
                            .as_deref()
                            .or(member.state.as_deref())
                            .unwrap_or("-")
                            .to_owned()
                    };
                    vec![
                        member.name.clone(),
                        member.role.clone().unwrap_or_else(|| "-".to_owned()),
                        member.model.clone().unwrap_or_else(|| "-".to_owned()),
                        status,
                    ]
                })
                .collect();
            lines.push(table(&["MEMBER", "ROLE", "MODEL", "STATE"], &rows));
        }
        if !lines.is_empty() {
            sections.push(lines.join("\n"));
        }
    }
    if !board_messages.is_empty() {
        let board: Vec<String> = board_messages
            .iter()
            .map(|msg| {
                let suffix = if msg.truncated { " [truncated]" } else { "" };
                format!("#{} {}: {}{suffix}", msg.seq, msg.agent, msg.text)
            })
            .collect();
        sections.push(board.join("\n"));
    }
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_aligns_its_columns() {
        let text = table(
            &["ID", "STATE"],
            &[
                vec!["abc".to_owned(), "active".to_owned()],
                vec!["longer-id".to_owned(), "paused".to_owned()],
            ],
        );
        assert_eq!(text, "ID        STATE\nabc       active\nlonger-id paused");
    }

    #[test]
    fn formats_swarm_snapshot_and_board_messages() {
        let snapshot = flyco_core::SwarmSnapshot {
            swarm_id: Some("sw-1".to_owned()),
            title: Some("Release".to_owned()),
            goal: Some(flyco_core::SwarmGoal {
                title: "Ship 412".to_owned(),
                line: Some("All gates green".to_owned()),
            }),
            threads: vec![flyco_core::SwarmThread {
                id: "t1".to_owned(),
                title: "Daemon".to_owned(),
                goal: None,
                tasks: vec!["worker".to_owned()],
            }],
            tasks: std::collections::BTreeMap::new(),
            members: vec![flyco_core::SwarmMember {
                name: "worker".to_owned(),
                role: Some("worker".to_owned()),
                model: Some("Gemini 4 Neon (High)".to_owned()),
                workspace_mode: Some("share".to_owned()),
                state: Some("running".to_owned()),
                phase: Some("working".to_owned()),
                last_read_seq: Some(3),
                killed: false,
            }],
        };
        let messages = vec![flyco_core::SwarmBoardMessage {
            seq: 4,
            agent: "worker".to_owned(),
            text: "@coordinator done".to_owned(),
            time: None,
            kind: None,
            truncated: true,
        }];
        let rendered = format_swarm_update(Some(&snapshot), &messages);
        assert!(rendered.contains("Swarm: Release"), "{rendered}");
        assert!(
            rendered.contains("Goal: Ship 412 — All gates green"),
            "{rendered}"
        );
        assert!(rendered.contains("[t1] Daemon (worker)"), "{rendered}");
        assert!(rendered.contains("worker"), "{rendered}");
        assert!(
            rendered.contains("#4 worker: @coordinator done [truncated]"),
            "{rendered}"
        );
    }
}
