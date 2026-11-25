//! Same-harness native resume of a local session onto a cloud machine.
//!
//! Used by both `flyco handoff` and the interactive TUI's `/resume-claude`,
//! `/resume-codex`, `/resume-agy`, and `/resume-devin` commands. The local
//! session's native transcript (or single-session database) and optional
//! binary git diff patch are uploaded to object storage, the session's
//! `harness_session_id` is seeded with the local native id, and `flycod`
//! materializes the payload straight into the matching harness's native
//! store before opening the conversation via ACP `session/resume` /
//! `session/load`. Cross-harness handoff is not supported.
//!
//! # Wire flow
//!
//! 1. `POST /v1/sessions` with [`SessionSource::LocalHandoff`] validates that
//!    `source.harness == harness`, seeds `harness_session_id`, and records a
//!    pending `handoffs` row; the machine is reserved but its provisioning
//!    job is not yet queued — a daemon that booted before the payload
//!    landed would start without the transcript or patch.
//! 2. `PUT /v1/sessions/{id}/handoff/patch` and
//!    `PUT /v1/sessions/{id}/handoff/transcript` upload the two payloads
//!    to object storage.
//! 3. `POST /v1/sessions/{id}/handoff/complete` carries the
//!    [`HandoffManifest`] of what was uploaded; the control plane checks
//!    the objects exist at the declared sizes, marks the row complete, and
//!    only then enqueues provisioning.
//!
//! On the machine, `flycod` asks `GET /v1/sessions/{id}/handoff` after
//! cloning: the [`HandoffView`] names the harness and native session id,
//! the optional base commit to rewind to before applying the patch, and
//! whether a transcript payload waits to be materialized into the harness's
//! native store.

use serde::{Deserialize, Serialize};

/// The workdir every provisioned machine checks the session's repository
/// out into.
pub const SESSION_WORKDIR: &str = "/srv/flyco/work";

/// Largest handoff patch accepted, in bytes.
///
/// Generous rather than tight — a handoff can carry months of branch
/// divergence — but bounded so a malformed client cannot stream unbounded
/// data into object storage through the API.
pub const HANDOFF_PATCH_BYTES_MAX: u64 = 256 * 1024 * 1024;

/// Largest handoff transcript accepted, in bytes. Transcripts are JSONL
/// or single-session SQLite databases; a gigabyte is well past any real session.
pub const HANDOFF_TRANSCRIPT_BYTES_MAX: u64 = 1024 * 1024 * 1024;

/// Where a new session's context comes from, as `CreateSession::source`.
///
/// Absent means a fresh session — the overwhelmingly common case, so the
/// field is an `Option` rather than a required tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionSource {
    /// Same-harness native resume of a local session onto a cloud machine.
    LocalHandoff(LocalHandoff),
}

/// The provenance a local native resume declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LocalHandoff {
    /// Which harness the local session ran under. Must match the cloud
    /// session's `harness`; cross-harness handoff is rejected.
    pub harness: crate::harness::HarnessKind,
    /// The harness-native session id to resume — Claude's session UUID,
    /// Codex's rollout/thread id, Devin's session id, or Antigravity's
    /// conversation UUID.
    pub session_id: String,
    /// The commit the local work was based on — `git merge-base HEAD
    /// origin/<branch>` — or empty when no local working-tree patch is
    /// being replayed.
    pub base_commit: String,
    /// The sender's local workdir as an absolute path, recorded for forensics.
    pub local_workdir: String,
}

/// `POST /v1/sessions/{id}/handoff/complete` body — the integrity record
/// of the two uploaded objects.
///
/// Checksums, not just sizes: the daemon verifies `patch_sha256` before
/// `git apply`, so a corrupted object is a clear startup failure rather
/// than a mysteriously malformed diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HandoffManifest {
    /// Lowercase hex SHA-256 of the patch object.
    pub patch_sha256: String,
    /// Byte length of the patch object.
    pub patch_bytes: u64,
    /// Lowercase hex SHA-256 of the transcript object.
    pub transcript_sha256: String,
    /// Byte length of the transcript object.
    pub transcript_bytes: u64,
}

/// `GET /v1/sessions/{id}/handoff`, daemon-scoped — what the daemon
/// materializes after cloning, or `404` for a session that was never a
/// local resume/handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HandoffView {
    /// The harness whose native store receives the uploaded transcript payload.
    pub harness: crate::harness::HarnessKind,
    /// The harness-native session id being resumed.
    pub session_id: String,
    /// The commit to check out before applying the stored patch, or empty
    /// when no local patch needs to be replayed.
    pub base_commit: String,
    /// SHA-256 the applied patch must hash to, verified before `git
    /// apply` so a corrupt object fails loudly.
    pub patch_sha256: String,
    /// Whether a transcript object exists for
    /// `GET /v1/sessions/{id}/handoff/transcript` to fetch.
    pub has_transcript: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_handoff_source_serializes_tagged() {
        let source = SessionSource::LocalHandoff(LocalHandoff {
            harness: crate::harness::HarnessKind::Codex,
            session_id: "s-1".to_owned(),
            base_commit: "abc123".to_owned(),
            local_workdir: "/home/u/repo".to_owned(),
        });
        let json = serde_json::to_value(&source).expect("serialize");
        assert_eq!(json["kind"], "local_handoff");
        assert_eq!(json["session_id"], "s-1");
        let back: SessionSource = serde_json::from_value(json).expect("roundtrip");
        assert_eq!(back, source);
    }
}
