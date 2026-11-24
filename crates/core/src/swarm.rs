//! Read-only Antigravity Swarm observation types.
//!
//! When an Antigravity (`agy acp`) session orchestrates a swarm, its runtime
//! persists swarm metadata, roster state, thread/goal plan, and an append-only
//! message board on disk under
//! `<app_data_dir>/brain/<session_id>/.system_generated/swarm/`. `flycod`
//! watches those files strictly read-only and forwards coalesced, size-bounded
//! updates through [`crate::wire::DaemonToControl::SwarmUpdated`] and
//! [`crate::wire::ClientEvent::SwarmUpdated`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Maximum UTF-8 byte length kept for one board message's body on the wire.
///
/// Individual board posts can carry pasted command logs or long summaries;
/// capping each entry bounds frame payloads so one post cannot bloat the
/// room's SQLite `events` table or a browser catch-up page.
pub const SWARM_MESSAGE_TEXT_BYTES_MAX: usize = 4 * 1024;

/// Maximum UTF-8 byte length kept for short snapshot metadata fields (title,
/// goal text, thread titles, task summaries, model/state labels).
pub const SWARM_TEXT_FIELD_BYTES_MAX: usize = 512;

/// Maximum number of board messages carried in a single `SwarmUpdated` frame.
pub const SWARM_BOARD_BATCH_MAX: usize = 64;

/// Maximum number of members included in a [`SwarmSnapshot`].
pub const SWARM_MEMBERS_MAX: usize = 64;

/// Maximum number of thread groups included in a [`SwarmSnapshot`].
pub const SWARM_THREADS_MAX: usize = 64;

/// Maximum number of per-member task summaries included in a [`SwarmSnapshot`].
pub const SWARM_TASKS_MAX: usize = 128;

/// High-level swarm goal read from `plan.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SwarmGoal {
    /// Short heading describing the swarm's objective.
    pub title: String,
    /// Optional one-line detail beneath the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

/// One logical work thread grouping tasks/members in `plan.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SwarmThread {
    /// Stable thread identifier as recorded in `plan.json`.
    pub id: String,
    /// Display heading for the thread.
    pub title: String,
    /// Optional objective sentence for the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// Member or task names grouped under this thread.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<String>,
}

/// One member in the swarm roster, read from `members.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SwarmMember {
    /// Roster name (e.g. `coordinator`, `parser`, `verifier`).
    pub name: String,
    /// Role label reported by Antigravity (e.g. `coordinator`, `worker`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Active or configured model display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Workspace isolation mode (`inherit`, `share`, `branch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mode: Option<String>,
    /// Desired lifecycle state (e.g. `running`, `waiting`, `stopped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Current runtime phase (e.g. `working`, `waiting`, `idle`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Highest message board sequence number this member has read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_seq: Option<u64>,
    /// Whether this member has been permanently ended.
    #[serde(default, skip_serializing_if = "crate::wire::is_false")]
    pub killed: bool,
}

/// Point-in-time snapshot of a session's swarm state.
///
/// Combines `swarm.json`, `members.json`, and `plan.json` into one compact
/// document. Emitted only when the snapshot changes; board-only updates omit it
/// so unchanged roster state is never repeated on every message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SwarmSnapshot {
    /// Swarm identifier from `swarm.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_id: Option<String>,
    /// Human-readable swarm title from `members.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Top-level goal from `plan.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<SwarmGoal>,
    /// Thread groupings from `plan.json`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<SwarmThread>,
    /// Per-member task summaries keyed by roster name from `plan.json`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tasks: BTreeMap<String, String>,
    /// Roster entries from `members.json`, ordered deterministically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<SwarmMember>,
}

impl SwarmSnapshot {
    /// Whether the snapshot carries no observable swarm state at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.swarm_id.is_none()
            && self.title.is_none()
            && self.goal.is_none()
            && self.threads.is_empty()
            && self.tasks.is_empty()
            && self.members.is_empty()
    }
}

/// One message appended to the swarm's shared `board.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SwarmBoardMessage {
    /// Monotonic sequence number on the swarm message board.
    pub seq: u64,
    /// Roster name of the agent that posted the message.
    pub agent: String,
    /// Message text, truncated at [`SWARM_MESSAGE_TEXT_BYTES_MAX`] bytes when oversized.
    pub text: String,
    /// Timestamp string as recorded on the board (typically RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    /// Optional message kind tag recorded by Antigravity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Whether `text` was truncated to fit [`SWARM_MESSAGE_TEXT_BYTES_MAX`].
    #[serde(default, skip_serializing_if = "crate::wire::is_false")]
    pub truncated: bool,
}

/// Truncates `text` to at most `max_bytes` bytes on a valid UTF-8 character
/// boundary, returning the resulting string and whether truncation occurred.
#[must_use]
pub fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_utf8_respects_multibyte_boundaries() {
        // CJK characters (Chinese, Japanese, Korean) are 3 bytes each in UTF-8.
        let sample = "中文日本語한국어";
        assert_eq!(sample.len(), 24);
        let (cut, truncated) = truncate_utf8(sample, 10);
        assert!(truncated);
        assert_eq!(cut, "中文日");
        assert!(cut.len() <= 10);

        let (full, truncated) = truncate_utf8(sample, 24);
        assert!(!truncated);
        assert_eq!(full, sample);
    }

    #[test]
    fn empty_snapshot_is_detected() {
        assert!(SwarmSnapshot::default().is_empty());
        let non_empty = SwarmSnapshot {
            title: Some("Release prep".to_owned()),
            ..SwarmSnapshot::default()
        };
        assert!(!non_empty.is_empty());
    }
}
