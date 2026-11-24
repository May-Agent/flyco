//! ACP `SessionEvent` and extension payload normalization.
//!
//! [`aither_acp::SessionReducer`] folds raw `session/update` frames into a
//! [`SessionState`] snapshot and emits [`SessionEvent`]s. This module translates
//! those events, turn outcomes, context-usage breakdowns, and plan-usage
//! readings into [`flyco_core::HarnessEvent`]s and session metadata reports.

use std::collections::BTreeSet;

use aither_acp::client::{MessageKind, TurnOutcome};
use aither_acp::domain::{
    AvailableCommand, CommandInput, CompactionStatus, ConfigCategory, ConfigKind, ConfigOption,
    ContentBlock, DiffOperation, Open, Patch, Plan, PlanEntryStatus, ResourceContents,
    SelectOptions, StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolKind, Usage,
    UsageUpdate,
};
use aither_acp::{SessionEvent, SessionState};
use claude_acp_sdk::ContextUsageResponse;
use flyco_core::{
    ContextCost, ContextUsage, ContextWindow, HarnessCommand, HarnessEvent, ModelOption,
    UsageReport, UsageWindow, Usd,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// What one [`SessionEvent`] became, in flyco's vocabulary.
#[derive(Debug, Default)]
pub struct Normalized {
    /// Transcript events.
    pub events: Vec<HarnessEvent>,
    /// A fresh command palette, when the event announced one.
    pub commands: Option<Vec<HarnessCommand>>,
    /// A fresh model list, when config options changed.
    pub models: Option<Vec<ModelOption>>,
    /// The model the session now reports running on, when it changed.
    pub model: Option<String>,
    /// The agent's current mode id, when it changed.
    pub mode: Option<String>,
}

/// The stateful half of ACP event normalization.
#[derive(Debug, Default)]
pub struct Normalizer {
    /// The last rendered plan, for deduplication.
    plan: Option<String>,
    /// The window fill the agent last reported.
    usage: Option<UsageUpdate>,
    /// Tool call ids already announced via [`HarnessEvent::ToolStarted`].
    started_tools: BTreeSet<String>,
    /// Tool call ids already finished via [`HarnessEvent::ToolCompleted`].
    completed_tools: BTreeSet<String>,
    /// Plan-usage windows already announced as exhausted.
    announced_limits: BTreeSet<String>,
}

impl Normalizer {
    /// Translates one [`SessionEvent`] against the session's current snapshot.
    pub fn on_event(
        &mut self,
        event: &SessionEvent,
        state: &SessionState,
        turn_id: &str,
    ) -> Normalized {
        let mut out = Normalized::default();
        match event {
            SessionEvent::MessageDelta {
                kind: MessageKind::Agent,
                delta,
                text,
                ..
            } => {
                let emitted = delta.as_ref().and_then(block_text).or_else(|| {
                    text.as_deref()
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                });
                if let Some(chunk) = emitted {
                    out.events.push(HarnessEvent::AssistantDelta {
                        turn_id: turn_id.to_owned(),
                        text: chunk,
                    });
                }
            }
            SessionEvent::ToolCall(call) => {
                let call_id = call.tool_call_id.as_str().to_owned();
                if self.started_tools.insert(call_id.clone()) {
                    out.events.push(HarnessEvent::ToolStarted {
                        turn_id: turn_id.to_owned(),
                        call_id: call_id.clone(),
                        tool: tool_name(
                            call.name.as_deref(),
                            call.title.as_str(),
                            call.kind.as_ref(),
                        ),
                        input: call
                            .raw_input
                            .clone()
                            .unwrap_or_else(|| tool_call_input(call)),
                    });
                }
                if let Some(ok) = call.status.as_ref().and_then(finished)
                    && self.completed_tools.insert(call_id.clone())
                {
                    out.events.push(HarnessEvent::ToolCompleted {
                        turn_id: turn_id.to_owned(),
                        call_id,
                        ok,
                    });
                }
            }
            SessionEvent::Plan {
                plan: Plan::Items(items),
                ..
            } => {
                let rendered = render_plan(&items.entries);
                if self.plan.as_deref() != Some(rendered.as_str()) {
                    self.plan = Some(rendered.clone());
                    out.events
                        .push(HarnessEvent::LocalCommandOutput { content: rendered });
                }
            }
            SessionEvent::CommandsChanged => {
                out.commands = Some(convert_commands(&state.available_commands));
            }
            SessionEvent::ConfigChanged => {
                let (models, model) = read_model_option(&state.config_options);
                out.models = models;
                out.model = model;
            }
            SessionEvent::ModeChanged(mode_id) => {
                out.mode = Some(mode_id.as_str().to_owned());
            }
            SessionEvent::Usage(usage) => {
                self.usage = Some(usage.clone());
            }
            SessionEvent::CompactionStarted { update, .. } => match &update.status {
                Patch::Set(Open::Known(CompactionStatus::Completed)) => {
                    out.events.push(HarnessEvent::ContextCompacted);
                }
                Patch::Set(Open::Known(CompactionStatus::Failed)) => {
                    let error = update
                        .error
                        .as_option()
                        .cloned()
                        .unwrap_or_else(|| "context compaction failed".to_owned());
                    out.events
                        .push(HarnessEvent::ContextCompactionFailed { error });
                }
                _ => {}
            },
            SessionEvent::MessageDelta { .. }
            | SessionEvent::ForegroundChanged { .. }
            | SessionEvent::TurnEnded { .. }
            | SessionEvent::TerminalUpsert(_)
            | SessionEvent::TerminalOutput { .. }
            | SessionEvent::Subagent(_)
            | SessionEvent::InfoChanged
            | SessionEvent::Notice(_)
            | SessionEvent::CompactionSummary { .. }
            | SessionEvent::ReplayComplete
            | SessionEvent::Other(_) => {}
        }
        out
    }

    /// Translates a completed [`TurnOutcome`] into the closing turn event plus
    /// whether the turn ended by cancellation.
    #[must_use]
    pub fn on_turn_outcome(&self, outcome: TurnOutcome, turn_id: &str) -> (HarnessEvent, bool) {
        match outcome {
            TurnOutcome::Cancelled => (
                HarnessEvent::TurnCompleted {
                    turn_id: turn_id.to_owned(),
                    usage: self.usage_report(None),
                },
                true,
            ),
            TurnOutcome::Finished {
                stop_reason,
                usage,
                error,
                ..
            } => match stop_reason {
                None | Some(Open::Known(StopReason::EndTurn)) => (
                    HarnessEvent::TurnCompleted {
                        turn_id: turn_id.to_owned(),
                        usage: self.usage_report(usage.as_deref()),
                    },
                    false,
                ),
                Some(Open::Known(StopReason::Cancelled)) => (
                    HarnessEvent::TurnCompleted {
                        turn_id: turn_id.to_owned(),
                        usage: self.usage_report(usage.as_deref()),
                    },
                    true,
                ),
                Some(reason) => {
                    let message = error.map_or_else(
                        || format!("the agent ended the turn with stop reason `{reason:?}`"),
                        |rpc| rpc.to_string(),
                    );
                    (
                        HarnessEvent::TurnFailed {
                            turn_id: turn_id.to_owned(),
                            error: message,
                        },
                        false,
                    )
                }
            },
            TurnOutcome::Rejected(rpc) => (
                HarnessEvent::TurnFailed {
                    turn_id: turn_id.to_owned(),
                    error: rpc.to_string(),
                },
                false,
            ),
        }
    }

    /// Builds the [`UsageReport`] attached to a finished turn.
    fn usage_report(&self, turn_usage: Option<&Usage>) -> UsageReport {
        UsageReport {
            input_tokens: turn_usage.map_or(0, |u| u.input_tokens),
            output_tokens: turn_usage.map_or(0, |u| u.output_tokens),
            context: self.window(),
            estimated_cost: self
                .usage
                .as_ref()
                .and_then(|usage| usage.cost.as_ref())
                .and_then(cost_to_usd),
        }
    }

    /// The context-window fill the agent last reported, when both halves are
    /// positive/known.
    #[must_use]
    pub fn window(&self) -> Option<ContextWindow> {
        let usage = self.usage.as_ref()?;
        let size_tokens = usage.size.filter(|&size| size > 0)?;
        Some(ContextWindow {
            used_tokens: usage.used,
            size_tokens,
        })
    }

    /// Reads plan-usage snapshots from either Codex (`_codex/rate_limits` /
    /// `usedPercent`) or Claude (`_claude/rate_limits` / `_claude/rate_limit`),
    /// emitting [`HarnessEvent::UsageLimited`] once per newly exhausted window.
    pub fn on_plan_usage(&mut self, result: &Value) -> Vec<HarnessEvent> {
        let windows = usage_windows(result);
        let mut events = Vec::new();
        for window in &windows {
            if window.is_exhausted() {
                if self.announced_limits.insert(window.label.clone()) {
                    events.push(HarnessEvent::UsageLimited {
                        window: window.clone(),
                    });
                }
            } else {
                self.announced_limits.remove(&window.label);
            }
        }
        events
    }
}

/// Converts a Claude `_claude/context_usage` response payload into
/// [`ContextUsage`].
#[must_use]
pub(super) fn parse_claude_context_usage(
    value: &Value,
    fallback_model: Option<&str>,
) -> Option<ContextUsage> {
    let parsed: ContextUsageResponse = serde_json::from_value(value.clone()).ok()?;
    let model = (!parsed.model.is_empty())
        .then_some(parsed.model)
        .or_else(|| fallback_model.map(str::to_owned));
    let size_tokens = parsed
        .raw_max_tokens
        .filter(|&size| size > 0)
        .or_else(|| (parsed.max_tokens > 0).then_some(parsed.max_tokens));
    let window = size_tokens.map(|size_tokens| ContextWindow {
        used_tokens: parsed.total_tokens,
        size_tokens,
    });
    let auto_compact = parsed
        .is_auto_compact_enabled
        .then_some(parsed.auto_compact_threshold)
        .flatten();
    let categories = parsed
        .categories
        .into_iter()
        .map(|row| ContextCost {
            name: row.name,
            tokens: row.tokens,
            deferred: row.is_deferred.unwrap_or(false),
        })
        .collect();
    let mcp_tools = parsed
        .mcp_tools
        .into_iter()
        .map(|row| ContextCost {
            name: row.name,
            tokens: row.tokens,
            deferred: row.is_loaded == Some(false),
        })
        .collect();
    let memory_files = parsed
        .memory_files
        .into_iter()
        .map(|row| ContextCost {
            name: row.path,
            tokens: row.tokens,
            deferred: false,
        })
        .collect();
    let agents = parsed
        .agents
        .into_iter()
        .map(|row| ContextCost {
            name: row.agent_type,
            tokens: row.tokens,
            deferred: false,
        })
        .collect();
    let skills = parsed.skills.map_or_default(|skills| {
        skills
            .skill_frontmatter
            .into_iter()
            .map(|row| ContextCost {
                name: row.name,
                tokens: row.tokens,
                deferred: false,
            })
            .collect()
    });

    Some(ContextUsage {
        model,
        window,
        auto_compact,
        categories,
        mcp_tools,
        memory_files,
        agents,
        skills,
    })
}

/// Converts ACP [`AvailableCommand`] entries into flyco's [`HarnessCommand`].
pub(super) fn convert_commands(commands: &[AvailableCommand]) -> Vec<HarnessCommand> {
    commands
        .iter()
        .filter_map(|entry| match entry {
            AvailableCommand::Command(command) => Some(HarnessCommand {
                name: command.name.clone(),
                description: command.description.clone(),
                argument_hint: command.input.as_ref().and_then(|input| match input {
                    Open::Known(CommandInput::Text(text)) => text.hint.clone(),
                    Open::Custom(_, _) | Open::Unknown(_, _) => None,
                }),
            }),
            AvailableCommand::Custom(_, _) | AvailableCommand::Unknown(_, _) => None,
        })
        .collect()
}

/// Whether a tool-call status is terminal, and if so whether it succeeded.
const fn finished(status: &Open<ToolCallStatus>) -> Option<bool> {
    match status {
        Open::Known(ToolCallStatus::Completed) => Some(true),
        Open::Known(ToolCallStatus::Failed | ToolCallStatus::Cancelled) => Some(false),
        Open::Known(
            ToolCallStatus::Pending | ToolCallStatus::InProgress | ToolCallStatus::Paused,
        )
        | Open::Custom(_, _)
        | Open::Unknown(_, _) => None,
    }
}

/// Converts an ACP [`aither_acp::domain::Cost`] to [`Usd`].
fn cost_to_usd(cost: &aither_acp::domain::Cost) -> Option<Usd> {
    if !cost.currency.eq_ignore_ascii_case("USD") {
        return None;
    }
    let amount = cost
        .amount
        .as_f64()
        .or_else(|| cost.amount.as_str().and_then(|s| s.parse::<f64>().ok()))?;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a turn's spend in cents is non-negative and far inside u64"
    )]
    Some(Usd::from_cents((amount * 100.0).round().max(0.0) as u64))
}

/// The text one content block contributes to the transcript.
fn block_text(block: &ContentBlock) -> Option<String> {
    let text = match block {
        ContentBlock::Text(content) => content.text.clone(),
        ContentBlock::Image(image) => {
            format!(
                "[image: {}]",
                image.uri.as_deref().unwrap_or(&image.mime_type)
            )
        }
        ContentBlock::Audio(audio) => format!("[audio: {}]", audio.mime_type),
        ContentBlock::ResourceLink(link) => {
            let title = link.title.as_deref().unwrap_or(&link.name);
            format!("[{title}]({})", link.uri)
        }
        ContentBlock::Resource(resource) => match &resource.resource {
            ResourceContents::Text(text) => text.text.clone(),
            ResourceContents::Blob(blob) => format!("[resource: {}]", blob.uri),
        },
        ContentBlock::Custom(_, _) | ContentBlock::Unknown(_, _) => return None,
    };
    if text.is_empty() { None } else { Some(text) }
}

/// What flyco calls the tool a `ToolCall` describes.
pub(super) fn tool_name(name: Option<&str>, title: &str, kind: Option<&Open<ToolKind>>) -> String {
    if !title.is_empty() {
        return title.to_owned();
    }
    if let Some(name) = name
        && !name.is_empty()
    {
        return name.to_owned();
    }
    let label = match kind {
        Some(Open::Known(ToolKind::Read)) => "read",
        Some(Open::Known(ToolKind::Edit)) => "edit",
        Some(Open::Known(ToolKind::Delete)) => "delete",
        Some(Open::Known(ToolKind::Move)) => "move",
        Some(Open::Known(ToolKind::Search)) => "search",
        Some(Open::Known(ToolKind::Execute)) => "execute",
        Some(Open::Known(ToolKind::Think)) => "think",
        Some(Open::Known(ToolKind::Fetch)) => "fetch",
        Some(Open::Known(ToolKind::SwitchMode)) => "switch mode",
        Some(Open::Known(ToolKind::Other)) | None => "tool",
        Some(Open::Custom(ext, _)) => ext.as_str(),
        Some(Open::Unknown(raw, _)) => raw.as_str(),
    };
    label.to_owned()
}

/// A `ToolCall` without `rawInput`, restated as input JSON.
pub(super) fn tool_call_input(call: &ToolCall) -> Value {
    json!({
        "locations": call.locations.iter().map(|location| {
            location.line.map_or_else(
                || location.path.display().to_string(),
                |line| format!("{}:{line}", location.path.display()),
            )
        }).collect::<Vec<_>>(),
        "content": call.content.iter().map(|content| match content {
            ToolCallContent::Content(content) => block_text(&content.content)
                .unwrap_or_else(|| "[content]".to_owned()),
            ToolCallContent::Diff(diff) => {
                let paths: Vec<String> = diff.changes.iter().filter_map(|change| {
                    match &change.operation {
                        Open::Known(
                            DiffOperation::Add(p)
                            | DiffOperation::Delete(p)
                            | DiffOperation::Modify(p),
                        ) => Some(p.path.display().to_string()),
                        Open::Known(DiffOperation::Move(p) | DiffOperation::Copy(p)) => {
                            Some(p.path.display().to_string())
                        }
                        Open::Custom(_, _) | Open::Unknown(_, _) => None,
                    }
                }).collect();
                if paths.is_empty() {
                    "diff".to_owned()
                } else {
                    format!("diff {}", paths.join(", "))
                }
            }
            ToolCallContent::Terminal(term) => format!("terminal {}", term.terminal_id),
            ToolCallContent::Custom(ext, _) => format!("[{ext}]"),
            ToolCallContent::Unknown(tag, _) => format!("[{tag}]"),
        }).collect::<Vec<_>>(),
    })
}

/// Renders plan items as a Markdown task list.
fn render_plan(entries: &[aither_acp::domain::PlanEntry]) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("**Plan**\n");
    for entry in entries {
        let mark = match entry.status {
            Some(Open::Known(PlanEntryStatus::Completed)) => "[x]",
            _ => "[ ]",
        };
        let _ = writeln!(out, "- {mark} {}", entry.content);
    }
    out
}

/// Reads a config-option set for the model picker.
pub(super) fn read_model_option(
    options: &[ConfigOption],
) -> (Option<Vec<ModelOption>>, Option<String>) {
    let mut models: Option<Vec<ModelOption>> = None;
    let mut current = None;
    let mut efforts = Vec::new();
    let mut default_effort = None;
    for option in options {
        match &option.category {
            Some(Open::Known(ConfigCategory::Model)) => {
                if let ConfigKind::Select(select) = &option.kind {
                    let current_id = select.current_value.as_str().to_owned();
                    current = Some(current_id.clone());
                    models = Some(
                        select_values(&select.options)
                            .into_iter()
                            .map(|(value, name, description)| ModelOption {
                                is_default: current_id == value,
                                id: value,
                                label: name,
                                description,
                                efforts: Vec::new(),
                                default_effort: None,
                            })
                            .collect(),
                    );
                }
            }
            Some(Open::Known(ConfigCategory::ThoughtLevel)) => {
                if let ConfigKind::Select(select) = &option.kind {
                    efforts = select_values(&select.options)
                        .into_iter()
                        .map(|(value, _, _)| value)
                        .collect();
                    default_effort = Some(select.current_value.as_str().to_owned());
                }
            }
            _ => {}
        }
    }
    if let Some(list) = models.as_mut()
        && !efforts.is_empty()
    {
        for model in list.iter_mut() {
            model.efforts.clone_from(&efforts);
            model.default_effort.clone_from(&default_effort);
        }
    }
    (models, current)
}

/// Every selectable value of a [`SelectOptions`], flattened.
fn select_values(options: &SelectOptions) -> Vec<(String, String, String)> {
    let values = |option: &aither_acp::domain::SelectOption| {
        (
            option.value.as_str().to_owned(),
            option.name.clone(),
            option.description.clone().unwrap_or_default(),
        )
    };
    match options {
        SelectOptions::Ungrouped(list) => list.iter().map(values).collect(),
        SelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(values))
            .collect(),
    }
}

/// Claude rate-limit status payload (`rateLimitInfo`).
#[derive(Debug, Deserialize)]
struct ClaudeRateLimitInfo {
    status: ClaudeRateLimitStatus,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<i64>,
    #[serde(default, rename = "rateLimitType")]
    kind: Option<ClaudeRateLimitKind>,
    #[serde(default)]
    utilization: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClaudeRateLimitKind {
    FiveHour,
    SevenDay,
    SevenDayOpus,
    SevenDaySonnet,
    SevenDayOverageIncluded,
    Overage,
    #[serde(other)]
    Other,
}

const SEVEN_DAY_MINUTES: u32 = 7 * 24 * 60;

impl ClaudeRateLimitKind {
    const fn window(self) -> (Option<u32>, Option<&'static str>) {
        match self {
            Self::FiveHour => (Some(5 * 60), None),
            Self::SevenDay => (Some(SEVEN_DAY_MINUTES), None),
            Self::SevenDayOpus => (Some(SEVEN_DAY_MINUTES), Some("Opus")),
            Self::SevenDaySonnet => (Some(SEVEN_DAY_MINUTES), Some("Sonnet")),
            Self::SevenDayOverageIncluded => (Some(SEVEN_DAY_MINUTES), Some("overage included")),
            Self::Overage => (None, Some("overage")),
            Self::Other => (None, None),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClaudeRateLimitStatus {
    Allowed,
    AllowedWarning,
    Rejected,
    #[serde(other)]
    Other,
}

impl ClaudeRateLimitInfo {
    fn to_usage_window(&self) -> UsageWindow {
        let (minutes, scope) = self.kind.map_or((None, None), ClaudeRateLimitKind::window);
        let used = if self.status == ClaudeRateLimitStatus::Rejected {
            100
        } else {
            self.utilization.map_or(0, |value| {
                // Claude CLI may report utilization as 0.0..=1.0 or 0..=100.
                let scaled = if value <= 1.0 && value > 0.0 {
                    value * 100.0
                } else {
                    value
                };
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "clamped to percentage range before cast"
                )]
                let percent = scaled.clamp(0.0, 100.0).round() as u8;
                percent
            })
        };
        UsageWindow::new(minutes, scope, used, self.resets_at)
    }
}

/// Every usage window a JSON answer describes, supporting both Codex
/// (`usedPercent`) and Claude (`rateLimitInfo` / `windows`) shapes.
fn usage_windows(result: &Value) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    collect_windows(result, None, &mut windows);
    windows
}

fn collect_windows(value: &Value, scope: Option<&str>, windows: &mut Vec<UsageWindow>) {
    match value {
        Value::Object(map) => {
            if let Some(percent) = map
                .get("usedPercent")
                .or_else(|| map.get("used_percent"))
                .and_then(Value::as_f64)
            {
                let minutes = map
                    .get("windowDurationMins")
                    .or_else(|| map.get("window_duration_mins"))
                    .and_then(Value::as_u64)
                    .map(|mins| u32::try_from(mins).unwrap_or(u32::MAX));
                let resets = map
                    .get("resetsAt")
                    .or_else(|| map.get("resets_at"))
                    .and_then(|value| {
                        value.as_i64().or_else(|| {
                            value.as_str().and_then(|text| {
                                time::OffsetDateTime::parse(
                                    text,
                                    &time::format_description::well_known::Rfc3339,
                                )
                                .ok()
                                .map(time::OffsetDateTime::unix_timestamp)
                            })
                        })
                    });
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a reported fill is a percentage; clamped before cast"
                )]
                windows.push(UsageWindow::new(
                    minutes,
                    scope,
                    percent.round().clamp(0.0, 100.0) as u8,
                    resets,
                ));
                return;
            }
            if map.get("status").and_then(Value::as_str).is_some()
                && (map.contains_key("rateLimitType")
                    || map.contains_key("utilization")
                    || map.contains_key("resetsAt"))
                && let Ok(info) = serde_json::from_value::<ClaudeRateLimitInfo>(value.clone())
            {
                windows.push(info.to_usage_window());
                return;
            }
            for (key, child) in map {
                collect_windows(child, Some(key), windows);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_windows(item, scope, windows);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aither_acp::client::{MessageKind, TurnOutcome};
    use aither_acp::domain::{ContentBlock, Open, StopReason, UsageUpdate};
    use aither_acp::{MessageId, SessionEvent, SessionState};
    use flyco_core::HarnessEvent;
    use serde_json::json;

    use super::{Normalizer, parse_claude_context_usage};

    #[test]
    fn a_text_chunk_is_an_assistant_delta() {
        let mut normalizer = Normalizer::default();
        let state = SessionState::default();
        let out = normalizer.on_event(
            &SessionEvent::MessageDelta {
                message_id: MessageId::from("m1"),
                kind: MessageKind::Agent,
                delta: Some(ContentBlock::text("hello")),
                text: Some("hello".to_owned()),
                full: vec![ContentBlock::text("hello")],
            },
            &state,
            "turn-1",
        );
        assert_eq!(
            out.events,
            [HarnessEvent::AssistantDelta {
                turn_id: "turn-1".to_owned(),
                text: "hello".to_owned(),
            }]
        );
    }

    #[test]
    fn an_end_turn_completes_the_turn_with_the_known_fill() {
        let mut normalizer = Normalizer::default();
        let state = SessionState::default();
        normalizer.on_event(
            &SessionEvent::Usage(UsageUpdate {
                used: 10_000,
                size: Some(200_000),
                cost: None,
                meta: None,
                extra: BTreeMap::new(),
            }),
            &state,
            "turn-1",
        );
        let (event, cancelled) = normalizer.on_turn_outcome(
            TurnOutcome::Finished {
                stop_reason: Some(Open::Known(StopReason::EndTurn)),
                text: String::new(),
                usage: None,
                error: None,
            },
            "turn-1",
        );
        assert!(!cancelled);
        let HarnessEvent::TurnCompleted { usage, .. } = event else {
            panic!("expected TurnCompleted, got {event:?}");
        };
        assert_eq!(
            usage.context.expect("a fill was reported").size_tokens,
            200_000
        );
    }

    #[test]
    fn a_refusal_fails_the_turn() {
        let normalizer = Normalizer::default();
        let (event, cancelled) = normalizer.on_turn_outcome(
            TurnOutcome::Finished {
                stop_reason: Some(Open::Known(StopReason::Refusal)),
                text: String::new(),
                usage: None,
                error: None,
            },
            "turn-1",
        );
        assert!(!cancelled);
        assert!(matches!(event, HarnessEvent::TurnFailed { .. }));
    }

    #[test]
    fn a_spent_window_is_announced_once_for_codex_and_claude_shapes() {
        let mut normalizer = Normalizer::default();
        let codex = json!({
            "rateLimits": {
                "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1_787_100_000},
                "secondary": {"usedPercent": 12, "windowDurationMins": 10080},
            }
        });
        let events = normalizer.on_plan_usage(&codex);
        assert_eq!(events.len(), 1, "only the exhausted window announces");
        assert_eq!(normalizer.on_plan_usage(&codex), [] as [HarnessEvent; 0]);

        let claude = json!({
            "sessionId": "s1",
            "rateLimitInfo": {
                "status": "rejected",
                "rateLimitType": "seven_day_opus",
                "resetsAt": 1_789_000_000
            }
        });
        let claude_events = normalizer.on_plan_usage(&claude);
        assert_eq!(claude_events.len(), 1);
        let HarnessEvent::UsageLimited { window } = &claude_events[0] else {
            panic!("expected UsageLimited");
        };
        assert_eq!(window.label, "Weekly (Opus)");
        assert_eq!(window.used_percent, 100);
        assert_eq!(normalizer.on_plan_usage(&claude), [] as [HarnessEvent; 0]);
    }

    #[test]
    fn claude_context_usage_converts_all_sections() {
        let payload = json!({
            "model": "claude-sonnet-4-6",
            "totalTokens": 12_345,
            "maxTokens": 200_000,
            "isAutoCompactEnabled": true,
            "autoCompactThreshold": 160_000,
            "categories": [{"name": "System prompt", "tokens": 2000, "isDeferred": false}],
            "mcpTools": [{"name": "mcp__flyco__budget_status", "tokens": 450, "isLoaded": true}],
            "memoryFiles": [{"path": "CLAUDE.md", "tokens": 800}],
            "agents": [{"agentType": "Explore", "tokens": 300}],
            "skills": {"skillFrontmatter": [{"name": "rust", "tokens": 120}]}
        });
        let usage = parse_claude_context_usage(&payload, None).expect("parses");
        assert_eq!(usage.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(usage.window.expect("window").used_tokens, 12_345);
        assert_eq!(usage.auto_compact, Some(160_000));
        assert_eq!(usage.categories.len(), 1);
        assert_eq!(usage.mcp_tools.len(), 1);
        assert_eq!(usage.memory_files.len(), 1);
        assert_eq!(usage.agents.len(), 1);
        assert_eq!(usage.skills.len(), 1);
    }
}
