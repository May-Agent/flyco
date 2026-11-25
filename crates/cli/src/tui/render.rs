//! Original `ratatui` presentation layer for the `flyco` terminal client.
//!
//! Layout hierarchy:
//! - Top bar (2 rows): brand mark, session/repo identity, state pill, budget,
//!   and separate harness/model chips.
//! - Main viewport: either the lobby launch dashboard (when detached) or the
//!   scrollable live session timeline (when attached).
//! - Optional approval dock (when an approval is pending) or live `/` command
//!   palette popover (when composing a slash command).
//! - Bottom composer dock (3 rows): UTF-8/CJK-aware input box with keybinding
//!   footer.
//! - Optional centered floating overlay for searchable pickers and `/help`.

use flyco_core::{HarnessKind, SessionState};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr as _;

use crate::local_sessions::harness_label;
use crate::tui::app::{AppState, Overlay};
use crate::tui::commands::SlashCommand;
use crate::tui::picker::Picker;
use crate::tui::transcript::{Entry, format_approval, format_model_choice, state_label};

/// Spinner frames shown while a turn, provision, or background REST call is in flight.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Draws the entire TUI frame from `app`.
pub fn draw(frame: &mut Frame<'_>, app: &AppState) {
    let root = frame.area();
    let approval_height = if app.pending_approval().is_some() {
        4
    } else {
        0
    };
    let palette_items = app.active_palette();
    let palette_height = if app.overlay.is_none() && !palette_items.is_empty() {
        u16::try_from(palette_items.len().min(8) + 2).unwrap_or(10)
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(approval_height),
            Constraint::Length(palette_height),
            Constraint::Length(3),
        ])
        .split(root);

    draw_header(frame, chunks[0], app);
    if app.session.is_some() {
        draw_timeline(frame, chunks[1], app);
    } else {
        draw_lobby(frame, chunks[1], app);
    }
    if approval_height > 0 {
        draw_approval_dock(frame, chunks[2], app);
    }
    if palette_height > 0 {
        draw_palette(frame, chunks[3], &palette_items, app.palette_selected);
    }
    draw_composer(frame, chunks[4], app);

    if let Some(overlay) = &app.overlay {
        draw_overlay(frame, root, overlay);
    }
}

struct HeaderFields {
    title: String,
    repo: String,
    state_badge: &'static str,
    state_color: Color,
    budget_str: String,
    harness: HarnessKind,
    model_str: String,
}

fn header_fields(app: &AppState) -> HeaderFields {
    app.session.as_ref().map_or_else(
        || {
            let repo = app.lobby.repo.as_ref().map_or_else(
                || "(no repo)".to_owned(),
                |r| {
                    app.lobby
                        .branch
                        .as_ref()
                        .map_or_else(|| r.clone(), |b| format!("{r} [{b}]"))
                },
            );
            let model_str = app
                .lobby
                .model
                .as_ref()
                .map_or_else(|| "default".to_owned(), format_model_choice);
            HeaderFields {
                title: "new session".to_owned(),
                repo,
                state_badge: "LOBBY",
                state_color: Color::Cyan,
                budget_str: format!("{}", app.lobby.budget),
                harness: app.lobby.harness,
                model_str,
            }
        },
        |session| {
            let state_color = match session.state {
                SessionState::Active => Color::Green,
                SessionState::Provisioning => Color::Yellow,
                SessionState::Paused | SessionState::Interrupted => Color::Magenta,
                SessionState::Failed => Color::Red,
                SessionState::Archived => Color::DarkGray,
            };
            HeaderFields {
                title: truncate_width(&session.title, 36),
                repo: session.primary_repo_label(),
                state_badge: state_label(session.state),
                state_color,
                budget_str: format!("{} / {}", session.budget.spent, session.budget.limit),
                harness: session.harness,
                model_str: format_model_choice(&session.model),
            }
        },
    )
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let HeaderFields {
        title,
        repo,
        state_badge,
        state_color,
        budget_str,
        harness,
        model_str,
    } = header_fields(app);

    let left = vec![
        Span::styled(
            " FLYCO ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(repo, Style::default().fg(Color::DarkGray)),
    ];

    let right = vec![
        Span::styled(
            format!(" {state_badge} "),
            Style::default()
                .fg(Color::Black)
                .bg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(budget_str, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(
            format!(" {} ", harness_label(harness)),
            Style::default()
                .fg(Color::Black)
                .bg(harness_color(harness))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!(" {model_str} "),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
    ];

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(20),
            Constraint::Length(spans_width(&right) + 1),
        ])
        .split(rows[0]);

    frame.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(ratatui::layout::Alignment::Right),
        cols[1],
    );

    let rule = "─".repeat(usize::from(area.width));
    frame.render_widget(
        Paragraph::new(Span::styled(rule, Style::default().fg(Color::DarkGray))),
        rows[1],
    );
}

fn draw_lobby(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let padded = Rect {
        x: area.x.saturating_add(2),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(1),
    };
    if padded.width < 20 || padded.height < 4 {
        return;
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(1)])
        .split(padded);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(sections[0]);

    // Left pane: current launch configuration.
    let model_text = app
        .lobby
        .model
        .as_ref()
        .map_or_else(|| "(harness default)".to_owned(), format_model_choice);
    let repo_text = app.lobby.repo.as_deref().unwrap_or("(choose with /repo)");
    let branch_text = app.lobby.branch.as_deref().unwrap_or("(default)");
    let machine_text = app.lobby.machine_label.as_deref().unwrap_or("auto");
    let budget_text = format!("{}", app.lobby.budget);

    let cfg_lines = vec![
        kv_line(
            "Harness",
            harness_label(app.lobby.harness),
            harness_color(app.lobby.harness),
        ),
        kv_line("Model", &model_text, Color::White),
        kv_line("Repo", repo_text, Color::Cyan),
        kv_line("Branch", branch_text, Color::White),
        kv_line("Machine", machine_text, Color::White),
        kv_line("Budget", &budget_text, Color::Green),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Type a prompt", Style::default().fg(Color::Cyan)),
            Span::styled(
                " to start a cloud session",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];
    let left_block = Block::default()
        .title(" Launch ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    frame.render_widget(Paragraph::new(cfg_lines).block(left_block), panes[0]);

    // Right pane: primary commands index.
    let quick = [
        ("/sessions", "Active cloud sessions"),
        ("/resume", "Inactive cloud sessions"),
        ("/model", "Switch model"),
        ("/provider", "Switch harness"),
        ("/resume-codex", "Resume local Codex"),
        ("/resume-claude", "Resume local Claude"),
        ("/resume-agy", "Resume local Antigravity"),
        ("/resume-devin", "Resume local Devin"),
    ];
    let cmd_lines: Vec<Line<'_>> = quick
        .into_iter()
        .map(|(cmd, label)| {
            Line::from(vec![
                Span::styled(
                    format!("{cmd:<16}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(label, Style::default().fg(Color::White)),
            ])
        })
        .collect();
    let right_block = Block::default()
        .title(" Commands ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    frame.render_widget(Paragraph::new(cmd_lines).block(right_block), panes[1]);

    // Bottom lobby log (status / errors / activity).
    let mut log_lines = Vec::new();
    if let Some(busy) = &app.busy_label {
        let frame_ch = SPINNER[app.spinner_tick % SPINNER.len()];
        log_lines.push(Line::from(vec![
            Span::styled(format!("{frame_ch} "), Style::default().fg(Color::Yellow)),
            Span::styled(busy.as_str(), Style::default().fg(Color::Yellow)),
        ]));
    }
    for entry in app.lobby_log.iter().rev().take(12).rev() {
        render_entry(&mut log_lines, entry);
    }
    frame.render_widget(
        Paragraph::new(log_lines).wrap(Wrap { trim: false }),
        sections[1],
    );
}

fn draw_timeline(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let Some(session) = &app.session else {
        return;
    };

    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    let mut lines: Vec<Line<'_>> = Vec::new();

    if let Some(swarm) = &session.latest_swarm
        && !swarm.members.is_empty()
    {
        let mut spans = vec![Span::styled(
            " SWARM ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )];
        for member in &swarm.members {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                member.name.as_str(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" [{}]", member.state.as_deref().unwrap_or("idle")),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::raw(""));
    }

    for entry in &session.entries {
        render_entry(&mut lines, entry);
    }

    if !session.streaming_text.is_empty() {
        for (idx, part) in session.streaming_text.lines().enumerate() {
            let prefix = if idx == 0 { "● " } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Green)),
                Span::raw(part),
            ]));
        }
    }

    if let Some(busy) = &app.busy_label {
        let frame_ch = SPINNER[app.spinner_tick % SPINNER.len()];
        lines.push(Line::from(vec![
            Span::styled(format!("{frame_ch} "), Style::default().fg(Color::Yellow)),
            Span::styled(busy.as_str(), Style::default().fg(Color::Yellow)),
        ]));
    } else if session.turn_active || session.state == SessionState::Provisioning {
        let frame_ch = SPINNER[app.spinner_tick % SPINNER.len()];
        let label = if session.state == SessionState::Provisioning {
            "provisioning cloud machine…"
        } else {
            "working…"
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{frame_ch} "), Style::default().fg(Color::Cyan)),
            Span::styled(label, Style::default().fg(Color::DarkGray)),
        ]));
    }

    let viewport_height = usize::from(inner.height);
    let total_lines = estimate_wrapped_lines(&lines, usize::from(inner.width.max(1)));
    let max_scroll = total_lines.saturating_sub(viewport_height);
    let scroll_top = if app.follow_tail {
        max_scroll
    } else {
        max_scroll.saturating_sub(app.scroll_from_bottom)
    };

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(scroll_top).unwrap_or(u16::MAX), 0));
    frame.render_widget(paragraph, inner);

    if !app.follow_tail && inner.width > 18 && inner.height > 1 {
        let pill = " ↓ End: live ";
        let pill_w = u16::try_from(pill.width()).unwrap_or(14);
        let badge_area = Rect {
            x: inner.x + inner.width.saturating_sub(pill_w),
            y: inner.y + inner.height.saturating_sub(1),
            width: pill_w,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                pill,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            badge_area,
        );
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "renders all Entry variants into styled lines in one place"
)]
fn render_entry<'a>(lines: &mut Vec<Line<'a>>, entry: &'a Entry) {
    match entry {
        Entry::User { text } => {
            lines.push(Line::raw(""));
            for part in text.lines() {
                lines.push(Line::from(vec![
                    Span::styled("▌ ", Style::default().fg(Color::Cyan)),
                    Span::styled(part, Style::default().add_modifier(Modifier::BOLD)),
                ]));
            }
            lines.push(Line::raw(""));
        }
        Entry::Assistant { text } => {
            for (idx, part) in text.lines().enumerate() {
                let prefix = if idx == 0 { "● " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Green)),
                    Span::raw(part),
                ]));
            }
            lines.push(Line::raw(""));
        }
        Entry::Tool {
            name,
            summary,
            detail_lines,
            ok,
            result_excerpt,
            ..
        } => {
            let (icon, color) = match ok {
                None => ("▸", Color::Yellow),
                Some(true) => ("✓", Color::Green),
                Some(false) => ("✗", Color::Red),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {icon} "), Style::default().fg(color)),
                Span::styled(
                    format!("{name} "),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(summary.as_str(), Style::default().fg(Color::DarkGray)),
            ]));
            for detail in detail_lines {
                let style = if detail.starts_with('+') {
                    Style::default().fg(Color::Green)
                } else if detail.starts_with('-') {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                lines.push(Line::from(Span::styled(format!("    {detail}"), style)));
            }
            if let Some(excerpt) = result_excerpt {
                lines.push(Line::from(Span::styled(
                    format!("    {excerpt}"),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        Entry::Shell {
            command,
            output,
            outcome,
            ..
        } => {
            let suffix = outcome
                .as_ref()
                .map_or_else(|| " (running)".to_owned(), |out| format!(" ({out:?})"));
            lines.push(Line::from(vec![
                Span::styled("  $ ", Style::default().fg(Color::Magenta)),
                Span::styled(
                    command.as_str(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(suffix, Style::default().fg(Color::DarkGray)),
            ]));
            for part in output.lines().take(24) {
                lines.push(Line::from(Span::styled(
                    format!("    {part}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        Entry::Swarm { header, body } => {
            lines.push(Line::from(vec![
                Span::styled("  ◈ ", Style::default().fg(Color::Blue)),
                Span::styled(header.as_str(), Style::default().fg(Color::Blue)),
            ]));
            for part in body.lines().take(6) {
                lines.push(Line::from(Span::styled(
                    format!("    {part}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        Entry::Notice { text } => {
            lines.push(Line::from(Span::styled(
                format!("  ─ {text}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        Entry::Warning { text } => {
            lines.push(Line::from(Span::styled(
                format!("  ▲ {text}"),
                Style::default().fg(Color::Yellow),
            )));
        }
        Entry::Error { text } => {
            lines.push(Line::from(Span::styled(
                format!("  ✗ {text}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }
    }
}

fn draw_approval_dock(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let Some(pending) = app.pending_approval() else {
        return;
    };
    let (headline, detail) = format_approval(&pending.payload);
    let block = Block::default()
        .title(" Approval Required ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let lines = vec![
        Line::from(vec![
            Span::styled(
                headline,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                "[a] Allow",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                "[d] Deny",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(detail, Style::default().fg(Color::White))),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_palette(
    frame: &mut Frame<'_>,
    area: Rect,
    items: &[crate::tui::commands::CommandSpec],
    selected: usize,
) {
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let start = selected.saturating_sub(visible.saturating_sub(1));
    let rows: Vec<ListItem<'_>> = items
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(idx, spec)| {
            let is_sel = idx == selected;
            let name_style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            };
            let rest_style = if is_sel {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let arg_text = if spec.args.is_empty() {
                String::new()
            } else {
                format!(" {}", spec.args)
            };
            let left = format!("{}{arg_text}", spec.name);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {left:<24}"), name_style),
                Span::styled(format!(" {} ", spec.summary), rest_style),
            ]))
        })
        .collect();
    let block = Block::default()
        .title(" Commands ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(List::new(rows).block(block), area);
}

fn draw_composer(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let hints = if app.session.is_some() {
        " / commands   PgUp/Dn scroll   ^D lobby   ^Q quit "
    } else {
        " / commands   Enter send   ^Q quit "
    };
    let is_shell = app.composer.text().trim_start().starts_with('!');
    let border_color = if is_shell {
        Color::Magenta
    } else if app.overlay.is_some() {
        Color::DarkGray
    } else {
        Color::Cyan
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title_bottom(
            Line::from(Span::styled(hints, Style::default().fg(Color::DarkGray))).right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let prefix = if is_shell { "! " } else { "❯ " };
    let prefix_w = 2_usize;
    let avail = usize::from(inner.width).saturating_sub(prefix_w);
    let (visible, cursor_col) = app.composer.visible_window(avail);

    let line = if app.composer.is_empty() {
        let placeholder = if app.pending_approval().is_some() {
            "a to allow, d to deny, or /command"
        } else if app.session.is_some() {
            "Message agent, !command, or /command"
        } else {
            "Prompt to start a session, or /sessions  /resume  /model  /provider"
        };
        Line::from(vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(placeholder, Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(visible),
        ])
    };
    frame.render_widget(Paragraph::new(line), inner);

    if app.overlay.is_none() && inner.width > 2 && inner.height >= 1 {
        let cx = inner.x + u16::try_from(prefix_w + cursor_col).unwrap_or(0);
        let max_x = inner.x + inner.width.saturating_sub(1);
        frame.set_cursor_position((cx.min(max_x), inner.y));
    }
}

fn draw_overlay(frame: &mut Frame<'_>, root: Rect, overlay: &Overlay) {
    let area = centered_rect(82, 74, root);
    frame.render_widget(Clear, area);
    match overlay {
        Overlay::Help => draw_help_modal(frame, area),
        Overlay::Sessions(picker) | Overlay::Resume(picker) => {
            draw_picker_modal(frame, area, picker);
        }
        Overlay::Models(picker) => draw_picker_modal(frame, area, picker),
        Overlay::Providers(picker) => draw_picker_modal(frame, area, picker),
        Overlay::LocalSessions(picker) => draw_picker_modal(frame, area, picker),
        Overlay::Repos(picker) => draw_picker_modal(frame, area, picker),
        Overlay::Machines(picker) => draw_picker_modal(frame, area, picker),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "lays out modal frame, filter bar, and scrollable list rows together"
)]
fn draw_picker_modal<T>(frame: &mut Frame<'_>, area: Rect, picker: &Picker<T>) {
    let count_title = format!(
        " {} ({}/{}) ",
        picker.title,
        picker.visible_len(),
        picker.total_len()
    );
    let block = Block::default()
        .title(count_title)
        .title_bottom(
            Line::from(Span::styled(
                " ↑/↓ move   Enter select   Esc close ",
                Style::default().fg(Color::DarkGray),
            ))
            .right_aligned(),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(inner);

    let filter_line = Line::from(vec![
        Span::styled(
            " Filter: ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(picker.query.as_str()),
    ]);
    let filter_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(chunks[0]);
    frame.render_widget(Paragraph::new(filter_line), filter_rows[0]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(usize::from(chunks[0].width)),
            Style::default().fg(Color::DarkGray),
        )),
        filter_rows[1],
    );

    let filter_cursor_x = filter_rows[0].x + 9 + u16::try_from(picker.query.width()).unwrap_or(0);
    let max_x = filter_rows[0].x + filter_rows[0].width.saturating_sub(1);
    frame.set_cursor_position((filter_cursor_x.min(max_x), filter_rows[0].y));

    if picker.visible_len() == 0 {
        let msg = if picker.total_len() == 0 {
            picker.empty_label.as_str()
        } else {
            "No matches for current filter"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("  {msg}"),
                Style::default().fg(Color::DarkGray),
            )),
            chunks[1],
        );
        return;
    }

    let visible_rows = usize::from(chunks[1].height).max(1);
    let start = picker.selected.saturating_sub(visible_rows / 2);
    let total_w = usize::from(chunks[1].width);
    let items: Vec<ListItem<'_>> = picker
        .visible()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(idx, entry)| {
            let is_sel = idx == picker.selected;
            let row_style = if is_sel {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            let badge_w = 13_usize;
            let trail_w = 14_usize.min(total_w / 4);
            let remaining = total_w.saturating_sub(badge_w + trail_w + 6);
            let pri_w = (remaining * 55) / 100;
            let sec_w = remaining.saturating_sub(pri_w);

            let badge = pad_width(&entry.badge, badge_w);
            let primary = pad_width(&entry.primary, pri_w);
            let secondary = pad_width(&entry.secondary, sec_w);
            let trailing = pad_width(&entry.trailing, trail_w);

            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {badge} "),
                    if is_sel {
                        row_style.add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
                Span::styled(
                    format!("{primary} "),
                    if is_sel {
                        row_style.add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    },
                ),
                Span::styled(
                    format!("{secondary} "),
                    if is_sel {
                        row_style
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::styled(
                    trailing,
                    if is_sel {
                        row_style
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ]))
        })
        .collect();
    frame.render_widget(List::new(items), chunks[1]);
}

fn draw_help_modal(frame: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .title(" flyco Help ")
        .title_bottom(
            Line::from(Span::styled(
                " Esc / Enter close ",
                Style::default().fg(Color::DarkGray),
            ))
            .right_aligned(),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(inner);

    let cmd_lines: Vec<Line<'_>> = SlashCommand::ALL
        .into_iter()
        .map(|cmd| {
            let spec = cmd.spec();
            let left = if spec.args.is_empty() {
                spec.name.to_owned()
            } else {
                format!("{} {}", spec.name, spec.args)
            };
            Line::from(vec![
                Span::styled(
                    format!(" {left:<24}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(spec.summary),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(cmd_lines), cols[0]);

    let keys = [
        ("Enter", "Send prompt / run command"),
        ("Tab", "Accept palette suggestion"),
        ("Up / Down", "History or palette nav"),
        ("PgUp / PgDn", "Scroll timeline"),
        ("End", "Jump back to live tail"),
        ("a / d", "Allow / deny pending approval"),
        ("Ctrl+C", "Clear draft or interrupt turn"),
        ("Ctrl+D", "Detach to lobby / exit"),
        ("Ctrl+Q", "Exit flyco"),
        ("!cmd", "Run shell command on machine"),
    ];
    let key_lines: Vec<Line<'_>> = keys
        .into_iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(
                    format!(" {k:<12}"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(v),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(key_lines), cols[1]);
}

fn kv_line<'a>(label: &'a str, value: &'a str, color: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), Style::default().fg(Color::DarkGray)),
        Span::styled(
            value,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

const fn harness_color(harness: HarnessKind) -> Color {
    match harness {
        HarnessKind::ClaudeCode => Color::Magenta,
        HarnessKind::Codex => Color::Green,
        HarnessKind::Devin => Color::Blue,
        HarnessKind::Antigravity => Color::Yellow,
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}

fn spans_width(spans: &[Span<'_>]) -> u16 {
    let total: usize = spans.iter().map(|s| s.content.width()).sum();
    u16::try_from(total).unwrap_or(u16::MAX)
}

fn truncate_width(text: &str, max_cols: usize) -> String {
    if text.width() <= max_cols {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut cols = 0_usize;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cols + w + 1 > max_cols {
            out.push('…');
            break;
        }
        out.push(ch);
        cols += w;
    }
    out
}

fn pad_width(text: &str, width: usize) -> String {
    let clipped = truncate_width(text, width);
    let w = clipped.width();
    if w >= width {
        clipped
    } else {
        format!("{clipped}{}", " ".repeat(width - w))
    }
}

fn estimate_wrapped_lines(lines: &[Line<'_>], width: usize) -> usize {
    let w = width.max(1);
    lines
        .iter()
        .map(|line| {
            let line_w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            line_w.div_ceil(w).max(1)
        })
        .sum()
}
