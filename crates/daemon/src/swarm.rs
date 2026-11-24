//! Strictly read-only watcher for Antigravity on-disk Swarm persistence.
//!
//! When an Antigravity (`agy acp`) session orchestrates a swarm, its runtime
//! persists files under
//! `<app_data_dir>/brain/<session_id>/.system_generated/swarm/`:
//!
//! - `swarm.json` — swarm identifier and backend metadata.
//! - `members.json` — swarm title and roster entries (roles, models, workspace
//!   modes, desired states, runtime phases, read cursors, killed flags).
//! - `plan.json` — top-level goal, per-member task summaries, and thread groups.
//! - `board.jsonl` — append-only NDJSON message board.
//!
//! This module **never** creates, modifies, truncates, or deletes any file in
//! that directory. Every read is best-effort and defensive: missing files,
//! partial writes during a concurrent update, truncated logs, and unknown JSON
//! fields are tolerated gracefully without disturbing the running session.
//!
//! Updates are coalesced over a debounce window and emitted in bounded batches
//! through [`SessionOutput::SwarmUpdated`], honouring the control-plane
//! free-plan quota rules (batched frames, bounded payloads, zero per-item
//! requests).

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use flyco_core::swarm::{
    SWARM_BOARD_BATCH_MAX, SWARM_MEMBERS_MAX, SWARM_MESSAGE_TEXT_BYTES_MAX, SWARM_TASKS_MAX,
    SWARM_TEXT_FIELD_BYTES_MAX, SWARM_THREADS_MAX, SwarmBoardMessage, SwarmGoal, SwarmMember,
    SwarmSnapshot, SwarmThread, truncate_utf8,
};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::harness::SessionOutput;

/// How often the watcher checks for the swarm directory to appear before any
/// swarm exists for the session.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How often the watcher checks swarm files once the swarm directory exists.
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Coalescing window applied once a change is observed so rapid bursts of
/// writes across `members.json`, `plan.json`, and `board.jsonl` collapse into
/// a single [`SessionOutput::SwarmUpdated`] frame.
const DEFAULT_COALESCE: Duration = Duration::from_millis(300);

/// Maximum bytes read from `board.jsonl` in a single poll tick so a massive
/// backlog drains smoothly in bounded chunks without spiking memory.
const MAX_BOARD_READ_BYTES: usize = 256 * 1024;

/// Maximum bytes read from any single snapshot JSON file (`swarm.json`,
/// `members.json`, `plan.json`) to guard against runaway files.
const MAX_SNAPSHOT_FILE_BYTES: u64 = 1024 * 1024;

/// Timing knobs for [`watch`], configurable so unit tests can run briskly.
#[derive(Debug, Clone, Copy)]
pub struct WatchTimings {
    /// Poll interval before the swarm directory exists.
    pub idle_poll: Duration,
    /// Poll interval once the swarm directory exists.
    pub active_poll: Duration,
    /// Coalescing window after a change is detected before emitting.
    pub coalesce: Duration,
}

impl Default for WatchTimings {
    fn default() -> Self {
        Self {
            idle_poll: IDLE_POLL_INTERVAL,
            active_poll: ACTIVE_POLL_INTERVAL,
            coalesce: DEFAULT_COALESCE,
        }
    }
}

/// Resolves the Antigravity swarm directory for `session_id` given the ACP
/// process's CLI arguments (and an optional home directory fallback).
///
/// Matches Antigravity's `--gemini-dir` and `--app-data-dir` flags:
/// - If `--app-data-dir` is absolute, uses it directly.
/// - Otherwise joins `--app-data-dir` (defaulting to `"antigravity-cli"`) onto
///   `--gemini-dir` (defaulting to `<home>/.gemini`).
/// - Rejects any `session_id` that is not a single normal path component so
///   crafted ids cannot escape the `brain/` directory.
#[must_use]
pub fn resolve_antigravity_swarm_dir(
    args: &[String],
    home: Option<&Path>,
    session_id: &str,
) -> Option<PathBuf> {
    let app_data_dir = resolve_antigravity_app_data_dir(args, home)?;
    swarm_dir_for_session(&app_data_dir, session_id)
}

/// Builds `<app_data_dir>/brain/<session_id>/.system_generated/swarm` after
/// verifying `session_id` is a single normal path segment.
#[must_use]
pub fn swarm_dir_for_session(app_data_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let mut components = Path::new(session_id).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Some(
            app_data_dir
                .join("brain")
                .join(session_id)
                .join(".system_generated")
                .join("swarm"),
        ),
        _ => None,
    }
}

/// Parses `--gemini-dir` and `--app-data-dir` out of `agy acp` CLI args.
fn resolve_antigravity_app_data_dir(args: &[String], home: Option<&Path>) -> Option<PathBuf> {
    let mut gemini_dir: Option<PathBuf> = None;
    let mut app_data_dir: Option<PathBuf> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--gemini-dir" {
            if let Some(value) = iter.next() {
                gemini_dir = Some(PathBuf::from(value));
            }
        } else if let Some(value) = arg.strip_prefix("--gemini-dir=") {
            gemini_dir = Some(PathBuf::from(value));
        } else if arg == "--app-data-dir" {
            if let Some(value) = iter.next() {
                app_data_dir = Some(PathBuf::from(value));
            }
        } else if let Some(value) = arg.strip_prefix("--app-data-dir=") {
            app_data_dir = Some(PathBuf::from(value));
        }
    }

    if let Some(dir) = &app_data_dir
        && dir.is_absolute()
    {
        return Some(dir.clone());
    }

    let base = gemini_dir.or_else(|| {
        home.map(|h| h.join(".gemini"))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gemini")))
    })?;
    let leaf = app_data_dir.unwrap_or_else(|| PathBuf::from("antigravity-cli"));
    Some(base.join(leaf))
}

/// Spawns a read-only watcher task for `swarm_dir`, emitting coalesced
/// [`SessionOutput::SwarmUpdated`] events on `outputs`.
///
/// The task stops cleanly as soon as `outputs` closes or the returned
/// [`tokio::task::JoinHandle`] is aborted.
#[must_use]
pub fn spawn(
    swarm_dir: PathBuf,
    outputs: mpsc::Sender<SessionOutput>,
    timings: WatchTimings,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        watch(swarm_dir, outputs, timings).await;
    })
}

/// Drives the read-only poll + debounce loop until `outputs` closes.
pub async fn watch(
    swarm_dir: PathBuf,
    outputs: mpsc::Sender<SessionOutput>,
    timings: WatchTimings,
) {
    let mut tracker = SwarmTracker::new(swarm_dir);
    let mut pending_snapshot: Option<SwarmSnapshot> = None;
    let mut pending_messages: Vec<SwarmBoardMessage> = Vec::new();
    let mut flush_at: Option<tokio::time::Instant> = None;

    loop {
        if outputs.is_closed() {
            return;
        }

        let delta = tokio::task::spawn_blocking(move || {
            let delta = tracker.poll();
            (tracker, delta)
        })
        .await;

        let delta = match delta {
            Ok((next_tracker, delta)) => {
                tracker = next_tracker;
                delta
            }
            Err(error) => {
                tracing::debug!(%error, "swarm watcher poll task cancelled");
                return;
            }
        };

        let snapshot_changed = delta.snapshot.is_some();
        if let Some(snapshot) = delta.snapshot {
            pending_snapshot = Some(snapshot);
        }
        let board_changed = !delta.board_messages.is_empty();
        if board_changed {
            pending_messages.extend(delta.board_messages);
        }
        let changed = snapshot_changed || board_changed;

        let has_pending = pending_snapshot.is_some() || !pending_messages.is_empty();
        if changed && flush_at.is_none() {
            flush_at = Some(tokio::time::Instant::now() + timings.coalesce);
        }

        let should_flush = has_pending
            && (pending_messages.len() >= SWARM_BOARD_BATCH_MAX
                || flush_at.is_some_and(|due| tokio::time::Instant::now() >= due));

        if should_flush {
            if !flush_pending(&outputs, &mut pending_snapshot, &mut pending_messages).await {
                return;
            }
            flush_at = None;
            if delta.board_more {
                continue;
            }
        }

        let base_interval = if tracker.dir_exists {
            timings.active_poll
        } else {
            timings.idle_poll
        };
        let sleep_for = flush_at.map_or(base_interval, |due| {
            let remaining = due.saturating_duration_since(tokio::time::Instant::now());
            remaining.min(base_interval)
        });

        tokio::select! {
            () = outputs.closed() => return,
            () = tokio::time::sleep(sleep_for) => {}
        }
    }
}

/// Emits buffered snapshot and board messages in bounded chunks of at most
/// [`SWARM_BOARD_BATCH_MAX`] messages per frame.
async fn flush_pending(
    outputs: &mpsc::Sender<SessionOutput>,
    pending_snapshot: &mut Option<SwarmSnapshot>,
    pending_messages: &mut Vec<SwarmBoardMessage>,
) -> bool {
    let mut snapshot = pending_snapshot.take();
    if pending_messages.is_empty() {
        if let Some(snap) = snapshot {
            return outputs
                .send(SessionOutput::SwarmUpdated {
                    snapshot: Some(snap),
                    board_messages: Vec::new(),
                })
                .await
                .is_ok();
        }
        return true;
    }

    let messages = std::mem::take(pending_messages);
    let mut iter = messages.into_iter().peekable();
    while iter.peek().is_some() {
        let batch: Vec<SwarmBoardMessage> = iter.by_ref().take(SWARM_BOARD_BATCH_MAX).collect();
        let frame_snapshot = snapshot.take();
        if outputs
            .send(SessionOutput::SwarmUpdated {
                snapshot: frame_snapshot,
                board_messages: batch,
            })
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

/// Cached `(len, modified)` stamp used to skip re-reading unchanged snapshot files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// Output of one synchronous read-only poll cycle.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PollDelta {
    /// Updated snapshot, present only when the snapshot changed.
    pub snapshot: Option<SwarmSnapshot>,
    /// Newly tailed board messages, in ascending `seq` order.
    pub board_messages: Vec<SwarmBoardMessage>,
    /// True when `board.jsonl` had more unread bytes past [`MAX_BOARD_READ_BYTES`].
    pub board_more: bool,
}

/// Stateful, strictly read-only reader for one Antigravity swarm directory.
#[derive(Debug)]
pub struct SwarmTracker {
    dir: PathBuf,
    dir_exists: bool,
    swarm_stamp: Option<FileStamp>,
    members_stamp: Option<FileStamp>,
    plan_stamp: Option<FileStamp>,
    cached_swarm: Option<RawSwarmFile>,
    cached_members: Option<RawMembersFile>,
    cached_plan: Option<RawPlanFile>,
    last_snapshot: Option<SwarmSnapshot>,
    board_offset: u64,
    last_board_seq: u64,
}

impl SwarmTracker {
    /// Creates a new tracker pointed at `dir` without touching the filesystem.
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            dir_exists: false,
            swarm_stamp: None,
            members_stamp: None,
            plan_stamp: None,
            cached_swarm: None,
            cached_members: None,
            cached_plan: None,
            last_snapshot: None,
            board_offset: 0,
            last_board_seq: 0,
        }
    }

    /// Reads any updated swarm files and newly appended `board.jsonl` lines.
    pub fn poll(&mut self) -> PollDelta {
        let Ok(meta) = std::fs::metadata(&self.dir) else {
            self.dir_exists = false;
            return PollDelta::default();
        };
        if !meta.is_dir() {
            self.dir_exists = false;
            return PollDelta::default();
        }
        self.dir_exists = true;

        let mut snapshot_dirty = false;
        snapshot_dirty |= refresh_json_file(
            &self.dir.join("swarm.json"),
            &mut self.swarm_stamp,
            &mut self.cached_swarm,
        );
        snapshot_dirty |= refresh_json_file(
            &self.dir.join("members.json"),
            &mut self.members_stamp,
            &mut self.cached_members,
        );
        snapshot_dirty |= refresh_json_file(
            &self.dir.join("plan.json"),
            &mut self.plan_stamp,
            &mut self.cached_plan,
        );

        let snapshot = if snapshot_dirty {
            let built = build_snapshot(
                self.cached_swarm.as_ref(),
                self.cached_members.as_ref(),
                self.cached_plan.as_ref(),
            );
            if built.is_empty() || self.last_snapshot.as_ref() == Some(&built) {
                None
            } else {
                self.last_snapshot = Some(built.clone());
                Some(built)
            }
        } else {
            None
        };

        let (board_messages, board_more) = self.tail_board();

        PollDelta {
            snapshot,
            board_messages,
            board_more,
        }
    }

    /// Incrementally reads newly appended complete NDJSON lines from `board.jsonl`.
    fn tail_board(&mut self) -> (Vec<SwarmBoardMessage>, bool) {
        let path = self.dir.join("board.jsonl");
        let Ok(meta) = std::fs::metadata(&path) else {
            return (Vec::new(), false);
        };
        if !meta.is_file() {
            return (Vec::new(), false);
        }
        let len = meta.len();
        if len < self.board_offset {
            // File was truncated or rotated; restart from the beginning.
            self.board_offset = 0;
            self.last_board_seq = 0;
        }
        if len == self.board_offset {
            return (Vec::new(), false);
        }

        let Ok(mut file) = std::fs::File::open(&path) else {
            return (Vec::new(), false);
        };
        if file.seek(SeekFrom::Start(self.board_offset)).is_err() {
            return (Vec::new(), false);
        }

        let remaining =
            usize::try_from(len.saturating_sub(self.board_offset)).unwrap_or(usize::MAX);
        let to_read = remaining.min(MAX_BOARD_READ_BYTES);
        let mut buf = vec![0_u8; to_read];
        let Ok(read) = file.read(&mut buf) else {
            return (Vec::new(), false);
        };
        if read == 0 {
            return (Vec::new(), false);
        }
        buf.truncate(read);

        // Only consume through the last newline so a writer mid-line is retried
        // on the next poll once the line completes.
        let Some(last_nl) = buf.iter().rposition(|byte| *byte == b'\n') else {
            // If a single line exceeds MAX_BOARD_READ_BYTES without a newline,
            // advance past the oversized chunk to avoid wedging forever.
            if read >= MAX_BOARD_READ_BYTES {
                self.board_offset = self
                    .board_offset
                    .saturating_add(u64::try_from(read).unwrap_or(0));
            }
            return (Vec::new(), false);
        };

        let consumed = last_nl + 1;
        self.board_offset = self
            .board_offset
            .saturating_add(u64::try_from(consumed).unwrap_or(0));

        let slice = &buf[..consumed];
        let mut messages = Vec::new();
        for raw_line in slice.split(|byte| *byte == b'\n') {
            if let Some(message) = self.parse_board_line(raw_line) {
                messages.push(message);
            }
        }

        let more = self.board_offset < len;
        (messages, more)
    }

    /// Parses one complete line from `board.jsonl`, skipping blank, corrupt,
    /// or already-emitted sequence numbers.
    fn parse_board_line(&mut self, raw_line: &[u8]) -> Option<SwarmBoardMessage> {
        if raw_line.is_empty() {
            return None;
        }
        let trimmed = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if trimmed.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
        let parsed = match serde_json::from_slice::<RawBoardLine>(trimmed) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::debug!(%error, "ignoring unparseable line in swarm board.jsonl");
                return None;
            }
        };
        let seq = parsed.seq?;
        if seq <= self.last_board_seq {
            return None;
        }
        let agent = parsed
            .agent
            .as_deref()
            .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX))?;
        let text_raw = parsed.text.unwrap_or_default();
        let (text, truncated) = truncate_utf8(&text_raw, SWARM_MESSAGE_TEXT_BYTES_MAX);
        let time = parsed
            .time
            .as_deref()
            .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX));
        let kind = parsed
            .kind
            .as_deref()
            .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX));
        self.last_board_seq = seq;
        Some(SwarmBoardMessage {
            seq,
            agent,
            text,
            time,
            kind,
            truncated,
        })
    }
}

/// Refreshes one cached JSON file if its `(len, modified)` stamp changed.
///
/// On partial/corrupt JSON (e.g. mid-write), the stamp is NOT updated and the
/// previous cached value is kept so the next poll retries automatically.
fn refresh_json_file<T: for<'de> Deserialize<'de>>(
    path: &Path,
    stamp: &mut Option<FileStamp>,
    cached: &mut Option<T>,
) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        if stamp.is_some() || cached.is_some() {
            *stamp = None;
            *cached = None;
            return true;
        }
        return false;
    };
    if !meta.is_file() || meta.len() > MAX_SNAPSHOT_FILE_BYTES {
        return false;
    }
    let next_stamp = FileStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    };
    if *stamp == Some(next_stamp) {
        return false;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    match serde_json::from_slice::<T>(&bytes) {
        Ok(parsed) => {
            *stamp = Some(next_stamp);
            *cached = Some(parsed);
            true
        }
        Err(error) => {
            tracing::debug!(
                path = %path.display(),
                %error,
                "deferring partial or invalid swarm JSON file"
            );
            false
        }
    }
}

/// Combines the parsed raw files into a normalized, bounded [`SwarmSnapshot`].
fn build_snapshot(
    swarm: Option<&RawSwarmFile>,
    members_file: Option<&RawMembersFile>,
    plan: Option<&RawPlanFile>,
) -> SwarmSnapshot {
    SwarmSnapshot {
        swarm_id: swarm
            .and_then(|s| s.swarm_id.as_deref())
            .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX)),
        title: members_file
            .and_then(|m| m.title.as_deref())
            .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX)),
        goal: plan.and_then(build_goal),
        threads: plan.map_or_default(build_threads),
        tasks: plan.map_or_default(build_tasks),
        members: members_file.map_or_default(build_members),
    }
}

fn build_goal(plan: &RawPlanFile) -> Option<SwarmGoal> {
    let raw = plan.goal.as_ref()?;
    let goal_title = raw
        .title
        .as_deref()
        .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX));
    let line = raw
        .line
        .as_deref()
        .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX));
    match (goal_title, line) {
        (Some(title), line) => Some(SwarmGoal { title, line }),
        (None, Some(line)) => Some(SwarmGoal {
            title: line,
            line: None,
        }),
        (None, None) => None,
    }
}

fn build_threads(plan: &RawPlanFile) -> Vec<SwarmThread> {
    plan.threads
        .iter()
        .take(SWARM_THREADS_MAX)
        .filter_map(|t| {
            let id =
                t.id.as_deref()
                    .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX))?;
            let title = t
                .title
                .as_deref()
                .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX))
                .unwrap_or_else(|| id.clone());
            let goal = t
                .goal
                .as_deref()
                .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX));
            let tasks = t
                .tasks
                .iter()
                .take(SWARM_TASKS_MAX)
                .filter_map(|name| non_empty_trimmed(name, SWARM_TEXT_FIELD_BYTES_MAX))
                .collect();
            Some(SwarmThread {
                id,
                title,
                goal,
                tasks,
            })
        })
        .collect()
}

fn build_tasks(plan: &RawPlanFile) -> BTreeMap<String, String> {
    plan.tasks
        .iter()
        .take(SWARM_TASKS_MAX)
        .filter_map(|(name, summary)| {
            let key = non_empty_trimmed(name, SWARM_TEXT_FIELD_BYTES_MAX)?;
            let val = non_empty_trimmed(summary, SWARM_TEXT_FIELD_BYTES_MAX)?;
            Some((key, val))
        })
        .collect()
}

fn build_members(members_file: &RawMembersFile) -> Vec<SwarmMember> {
    let mut members: Vec<SwarmMember> = members_file
        .members
        .iter()
        .filter_map(|(name, raw)| {
            let member_name = non_empty_trimmed(name, SWARM_TEXT_FIELD_BYTES_MAX)?;
            let model = raw
                .applied_model
                .as_deref()
                .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX))
                .or_else(|| {
                    raw.model
                        .as_deref()
                        .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX))
                })
                .or_else(|| {
                    raw.desired_model
                        .as_deref()
                        .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX))
                });
            Some(SwarmMember {
                name: member_name,
                role: raw
                    .role
                    .as_deref()
                    .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX)),
                model,
                workspace_mode: raw
                    .workspace_mode
                    .as_deref()
                    .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX)),
                state: raw
                    .desired_state
                    .as_deref()
                    .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX)),
                phase: raw
                    .phase
                    .as_deref()
                    .and_then(|s| non_empty_trimmed(s, SWARM_TEXT_FIELD_BYTES_MAX)),
                last_read_seq: raw.last_read_seq,
                killed: raw.killed,
            })
        })
        .collect();

    // Put the coordinator first, then sort the rest alphabetically by name.
    members.sort_by(|left, right| {
        let left_coord = left.role.as_deref() == Some("coordinator") || left.name == "coordinator";
        let right_coord =
            right.role.as_deref() == Some("coordinator") || right.name == "coordinator";
        right_coord
            .cmp(&left_coord)
            .then_with(|| left.name.cmp(&right.name))
    });
    members.truncate(SWARM_MEMBERS_MAX);
    members
}

fn non_empty_trimmed(text: &str, max_bytes: usize) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_utf8(trimmed, max_bytes).0)
}

#[derive(Debug, Default, Deserialize)]
struct RawSwarmFile {
    #[serde(default)]
    swarm_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMembersFile {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    members: BTreeMap<String, RawMember>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMember {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    desired_state: Option<String>,
    #[serde(default)]
    desired_model: Option<String>,
    #[serde(default)]
    applied_model: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    workspace_mode: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    last_read_seq: Option<u64>,
    #[serde(default)]
    killed: bool,
}

#[derive(Debug, Default, Deserialize)]
struct RawPlanFile {
    #[serde(default)]
    goal: Option<RawGoal>,
    #[serde(default)]
    tasks: BTreeMap<String, String>,
    #[serde(default)]
    threads: Vec<RawThread>,
}

#[derive(Debug, Default, Deserialize)]
struct RawGoal {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    line: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawThread {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    tasks: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawBoardLine {
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "flycod-swarm-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_antigravity_swarm_dir_from_cli_args() {
        let args: Vec<String> = [
            "acp",
            "--stdio",
            "--gemini-dir",
            "/var/lib/flyco/antigravity",
            "--app-data-dir",
            "antigravity-cli",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let resolved = resolve_antigravity_swarm_dir(&args, None, "abc-123")
            .expect("valid session id resolves");
        assert_eq!(
            resolved,
            PathBuf::from(
                "/var/lib/flyco/antigravity/antigravity-cli/brain/abc-123/.system_generated/swarm"
            )
        );

        let eq_args = vec![
            "--gemini-dir=/custom/gemini".to_owned(),
            "--app-data-dir=/abs/app-data".to_owned(),
        ];
        assert_eq!(
            resolve_antigravity_swarm_dir(&eq_args, None, "s1"),
            Some(PathBuf::from(
                "/abs/app-data/brain/s1/.system_generated/swarm"
            ))
        );

        // Traversal session ids are rejected.
        assert_eq!(resolve_antigravity_swarm_dir(&args, None, "../etc"), None);
        assert_eq!(resolve_antigravity_swarm_dir(&args, None, "a/b"), None);
        assert_eq!(resolve_antigravity_swarm_dir(&args, None, ""), None);
    }

    #[test]
    fn parses_full_swarm_files_ignores_unknown_fields_and_deduplicates_snapshots() {
        let tmp = TempDir::new("full");
        let swarm_dir = tmp.path().join("swarm");
        let mut tracker = SwarmTracker::new(swarm_dir.clone());

        // Missing directory produces empty delta and does not create the directory.
        assert_eq!(tracker.poll(), PollDelta::default());
        assert!(!swarm_dir.exists());

        std::fs::create_dir_all(&swarm_dir).expect("mkdir");
        std::fs::write(
            swarm_dir.join("swarm.json"),
            r#"{"version":1,"backend":"local","swarm_id":"sw-99","created_at":"2026-11-24T13:55:00Z","future_flag":true}"#,
        )
        .expect("write swarm.json");
        std::fs::write(
            swarm_dir.join("members.json"),
            r#"{
                "title": "Flyco 412",
                "extra_meta": 42,
                "members": {
                    "verifier": {
                        "conversation_id": "c-2",
                        "role": "worker",
                        "desired_state": "running",
                        "desired_model": "Gemini 4 Neon (Medium)",
                        "applied_model": "Gemini 4 Neon (High)",
                        "workspace_mode": "inherit",
                        "phase": "waiting",
                        "wake_mode": "mentions",
                        "wait_since": 12,
                        "last_read_seq": 10,
                        "killed": false
                    },
                    "coordinator": {
                        "role": "coordinator",
                        "desired_state": "running",
                        "model": "Gemini 4 Neon (High)",
                        "workspace_mode": "inherit",
                        "phase": "working",
                        "last_read_seq": 12,
                        "killed": false
                    }
                }
            }"#,
        )
        .expect("write members.json");
        std::fs::write(
            swarm_dir.join("plan.json"),
            r#"{
                "goal": { "title": "Read-only swarm", "line": "Observe without writing", "unknown": 1 },
                "tasks": { "verifier": "Audit PR #412" },
                "threads": [
                    { "id": "t1", "title": "Backend", "goal": "Wire + daemon", "tasks": ["verifier"], "extra": [] }
                ]
            }"#,
        )
        .expect("write plan.json");

        let delta = tracker.poll();
        let snap = delta.snapshot.expect("initial snapshot emitted");
        assert_eq!(snap.swarm_id.as_deref(), Some("sw-99"));
        assert_eq!(snap.title.as_deref(), Some("Flyco 412"));
        assert_eq!(
            snap.goal,
            Some(SwarmGoal {
                title: "Read-only swarm".to_owned(),
                line: Some("Observe without writing".to_owned()),
            })
        );
        assert_eq!(snap.threads.len(), 1);
        assert_eq!(snap.threads[0].id, "t1");
        assert_eq!(
            snap.tasks.get("verifier").map(String::as_str),
            Some("Audit PR #412")
        );
        assert_eq!(snap.members.len(), 2);
        // Coordinator is ordered first.
        assert_eq!(snap.members[0].name, "coordinator");
        assert_eq!(snap.members[1].name, "verifier");
        assert_eq!(
            snap.members[1].model.as_deref(),
            Some("Gemini 4 Neon (High)")
        );
        assert_eq!(snap.members[1].last_read_seq, Some(10));

        // Polling again without changes produces no duplicate snapshot.
        assert_eq!(tracker.poll(), PollDelta::default());
    }

    #[test]
    fn tolerates_partial_json_and_recovers_when_write_completes() {
        let tmp = TempDir::new("partial");
        let swarm_dir = tmp.path().join("swarm");
        std::fs::create_dir_all(&swarm_dir).expect("mkdir");
        let mut tracker = SwarmTracker::new(swarm_dir.clone());

        // Truncated/in-flight write must not crash or advance stamp.
        std::fs::write(swarm_dir.join("members.json"), r#"{"title": "Half"#)
            .expect("write partial");
        assert_eq!(tracker.poll(), PollDelta::default());

        // Once the file finishes writing, the next poll parses and emits it.
        std::fs::write(
            swarm_dir.join("members.json"),
            r#"{"title": "Complete", "members": {}}"#,
        )
        .expect("write complete");
        let delta = tracker.poll();
        assert_eq!(
            delta.snapshot.and_then(|s| s.title).as_deref(),
            Some("Complete")
        );
    }

    #[test]
    fn incrementally_tails_board_handles_partial_lines_truncation_and_long_posts() {
        let tmp = TempDir::new("board");
        let swarm_dir = tmp.path().join("swarm");
        std::fs::create_dir_all(&swarm_dir).expect("mkdir");
        let board_path = swarm_dir.join("board.jsonl");
        let mut tracker = SwarmTracker::new(swarm_dir);

        // Write one complete line + one incomplete line (missing trailing newline).
        let mut file = std::fs::File::create(&board_path).expect("create board");
        writeln!(
            file,
            r#"{{"seq":1,"agent":"coordinator","text":"@worker start","time":"2026-11-24T13:55:00Z","channel":"main","control":false}}"#
        )
        .expect("line 1");
        write!(file, r#"{{"seq":2,"agent":"worker","text":"half"#).expect("partial line 2");
        file.flush().expect("flush");

        let delta1 = tracker.poll();
        assert_eq!(delta1.board_messages.len(), 1);
        assert_eq!(delta1.board_messages[0].seq, 1);
        assert_eq!(delta1.board_messages[0].agent, "coordinator");
        assert!(!delta1.board_messages[0].truncated);

        // Finish line 2 + add a corrupt line + add an oversized line.
        let huge = "a".repeat(SWARM_MESSAGE_TEXT_BYTES_MAX + 500);
        writeln!(file, r#"way there","kind":"status"}}"#).expect("finish line 2");
        writeln!(file, "not valid json").expect("corrupt line");
        writeln!(file, r#"{{"seq":3,"agent":"worker","text":"{huge}"}}"#).expect("line 3");
        file.flush().expect("flush");

        let delta2 = tracker.poll();
        assert_eq!(delta2.board_messages.len(), 2);
        assert_eq!(delta2.board_messages[0].seq, 2);
        assert_eq!(delta2.board_messages[0].text, "halfway there");
        assert_eq!(delta2.board_messages[0].kind.as_deref(), Some("status"));
        assert_eq!(delta2.board_messages[1].seq, 3);
        assert!(delta2.board_messages[1].truncated);
        assert_eq!(
            delta2.board_messages[1].text.len(),
            SWARM_MESSAGE_TEXT_BYTES_MAX
        );

        // Truncate/rotate the board and verify the tracker resets cleanly.
        std::fs::write(
            &board_path,
            "{\"seq\":1,\"agent\":\"coordinator\",\"text\":\"fresh\"}\n",
        )
        .expect("rotate board");
        let delta3 = tracker.poll();
        assert_eq!(delta3.board_messages.len(), 1);
        assert_eq!(delta3.board_messages[0].seq, 1);
        assert_eq!(delta3.board_messages[0].text, "fresh");
    }

    #[tokio::test]
    async fn watcher_coalesces_bursts_into_batched_session_output() {
        let tmp = TempDir::new("coalesce");
        let swarm_dir = tmp.path().join("swarm");
        std::fs::create_dir_all(&swarm_dir).expect("mkdir");
        std::fs::write(
            swarm_dir.join("members.json"),
            r#"{"title":"Coalesced","members":{"a":{"role":"worker"}}}"#,
        )
        .expect("write members");
        std::fs::write(
            swarm_dir.join("board.jsonl"),
            concat!(
                "{\"seq\":1,\"agent\":\"a\",\"text\":\"one\"}\n",
                "{\"seq\":2,\"agent\":\"a\",\"text\":\"two\"}\n",
                "{\"seq\":3,\"agent\":\"a\",\"text\":\"three\"}\n",
            ),
        )
        .expect("write board");

        let (tx, mut rx) = mpsc::channel(16);
        let handle = spawn(
            swarm_dir,
            tx,
            WatchTimings {
                idle_poll: Duration::from_millis(20),
                active_poll: Duration::from_millis(20),
                coalesce: Duration::from_millis(40),
            },
        );

        let output = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("swarm update arrives")
            .expect("channel open");
        let SessionOutput::SwarmUpdated {
            snapshot,
            board_messages,
        } = output
        else {
            panic!("expected SwarmUpdated, got {output:?}");
        };
        assert_eq!(
            snapshot.as_ref().and_then(|s| s.title.as_deref()),
            Some("Coalesced")
        );
        assert_eq!(board_messages.len(), 3);
        assert_eq!(board_messages[0].seq, 1);
        assert_eq!(board_messages[2].seq, 3);

        handle.abort();
    }
}
