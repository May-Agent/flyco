//! The control-plane-backed Claude Code transcript synchronizer.
//!
//! Claude Code writes its session journal to disk under
//! `<home>/.claude/projects/<project>/<session-id>.jsonl` and subagent
//! sidechains under `<session-id>/subagents/agent-*.jsonl`. On an ephemeral
//! VM or container those files vanish when the machine stops or is reclaimed.
//!
//! [`R2TranscriptSync`] implements [`claude_acp_sdk::TranscriptSync`] on top
//! of [`ControlApi`], pulling the main transcript and any recorded subagent
//! transcripts before `session/resume` or `session/load` inspects disk, and
//! pushing newly appended byte ranges as immutable sequential batches after
//! every turn, on `flush`, and during session shutdown.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use claude_acp_sdk::{SdkError, TranscriptSync, encode_project_dir, find_transcript_path};
use sha2::Digest as _;
use tokio::sync::Mutex;

use crate::control::rest::ControlApi;
use crate::privilege::{AgentUser, chown_path};

/// Longest stream key the control plane accepts as one path segment
/// (`crates/api/src/transcripts.rs`).
pub const MAX_STREAM_LEN: usize = 128;

/// Maximum bytes uploaded in a single immutable batch (safely under the
/// control plane's 1 MiB per-batch ceiling).
const MAX_BATCH_BYTES: usize = 512 * 1024;

/// Reserved subpath marker for the per-session subagent manifest stream.
const SUBAGENT_INDEX_SUBPATH: &str = "__flyco_subagents_index";

/// Length of the hex-encoded SHA-256 digest suffix used when folding long or
/// subpath-bearing keys into a bounded control-plane stream key.
const HASH_HEX_LEN: usize = 32;

/// Why a transcript key cannot be mapped to a control-plane stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptKeyError {
    /// A key component was empty or contained path-traversal characters.
    #[error("transcript key field `{field}` is invalid: `{value}`")]
    InvalidSegment {
        /// Which part of the key was invalid.
        field: &'static str,
        /// The offending value.
        value: String,
    },
}

/// Validates a relative subagent path such as `subagents/agent-1.jsonl`.
///
/// Rejects empty strings, absolute paths, `..` / `.` segments, backslashes,
/// NUL bytes, and anything outside `subagents/<file>.jsonl`.
pub(crate) fn validate_subagent_relpath(relpath: &str) -> Result<PathBuf, TranscriptKeyError> {
    let invalid = || TranscriptKeyError::InvalidSegment {
        field: "subpath",
        value: relpath.to_owned(),
    };
    if relpath.is_empty() || relpath.contains('\\') || relpath.contains('\0') {
        return Err(invalid());
    }
    let path = Path::new(relpath);
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Err(invalid());
    };
    let Some(Component::Normal(file)) = components.next() else {
        return Err(invalid());
    };
    if components.next().is_some() {
        return Err(invalid());
    }
    let Some(first_str) = first.to_str() else {
        return Err(invalid());
    };
    let Some(file_str) = file.to_str() else {
        return Err(invalid());
    };
    if first_str != "subagents"
        || !Path::new(file_str)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        || file_str == ".jsonl"
    {
        return Err(invalid());
    }
    Ok(PathBuf::from(first_str).join(file_str))
}

/// Derives a control-plane-valid stream key (`1..=128` bytes of
/// `[A-Za-z0-9._-]`, never `.` or `..`) for a transcript file.
///
/// Short main-stream keys keep their readable `<project>.<session>` spelling.
/// Any key that would exceed [`MAX_STREAM_LEN`] or carries a subpath is folded
/// deterministically with a 128-bit SHA-256 suffix so long project paths or
/// nested subagent filenames never fail control-plane validation and never
/// collide across sanitization boundaries.
///
/// # Errors
///
/// Returns [`TranscriptKeyError::InvalidSegment`] if `project_key` or
/// `session_id` is empty, or if `subpath` is empty.
pub fn stream_key(
    project_key: &str,
    session_id: &str,
    subpath: Option<&str>,
) -> Result<String, TranscriptKeyError> {
    if project_key.is_empty() {
        return Err(TranscriptKeyError::InvalidSegment {
            field: "project_key",
            value: String::new(),
        });
    }
    if session_id.is_empty()
        || session_id == "."
        || session_id == ".."
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains('\0')
    {
        return Err(TranscriptKeyError::InvalidSegment {
            field: "session_id",
            value: session_id.to_owned(),
        });
    }
    if let Some(sub) = subpath
        && sub.is_empty()
    {
        return Err(TranscriptKeyError::InvalidSegment {
            field: "subpath",
            value: String::new(),
        });
    }

    let sanitize = |raw: &str| -> String {
        raw.chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                    ch
                } else {
                    '_'
                }
            })
            .collect()
    };

    let clean_project = sanitize(project_key);
    let clean_session = sanitize(session_id);

    if subpath.is_none() {
        let plain = format!("{clean_project}.{clean_session}");
        if plain.len() <= MAX_STREAM_LEN && plain != "." && plain != ".." {
            return Ok(plain);
        }
    }

    let mut hasher = sha2::Sha256::new();
    hasher.update(project_key.as_bytes());
    hasher.update([0]);
    hasher.update(session_id.as_bytes());
    hasher.update([0]);
    if let Some(sub) = subpath {
        hasher.update(sub.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    let short_hash = &digest[..HASH_HEX_LEN];

    let mut prefix = format!("{clean_project}.{clean_session}");
    if let Some(sub) = subpath {
        prefix.push('.');
        prefix.push_str(&sanitize(sub));
    }
    // `.h` (2) + 32 hex chars = 34 bytes reserved for the collision-free suffix.
    let max_prefix = MAX_STREAM_LEN - 2 - HASH_HEX_LEN;
    if prefix.len() > max_prefix {
        prefix.truncate(max_prefix);
    }
    while prefix.ends_with('.') {
        prefix.pop();
    }
    if prefix.is_empty() || prefix == "." || prefix == ".." {
        Ok(format!("t.h{short_hash}"))
    } else {
        Ok(format!("{prefix}.h{short_hash}"))
    }
}

/// Sync cursor tracked per control-plane transcript stream.
#[derive(Debug, Default, Clone)]
struct StreamCursor {
    /// Sequence number for the next batch `PUT`.
    next_seq: u64,
    /// Number of bytes already mirrored to the control plane.
    synced_bytes: usize,
}

/// Mutable state synchronized across pull/push calls for one session.
#[derive(Debug, Default)]
struct SyncState {
    streams: HashMap<String, StreamCursor>,
    known_subagents: BTreeSet<String>,
}

/// Control-plane-backed [`TranscriptSync`] mirroring Claude Code's main
/// `<session-id>.jsonl` and `subagents/agent-*.jsonl` files to R2.
#[derive(Debug, Clone)]
pub struct R2TranscriptSync<A> {
    api: A,
    /// Explicit `CLAUDE_CODE_PROJECT_DIR_NAME` when running under isolated
    /// credentials; `None` falls back to `encode_project_dir(cwd)`.
    project_dir_name: Option<String>,
    /// Unprivileged agent user that must own transcript files pulled to disk
    /// so the `claude` CLI can read and append to them.
    agent: Option<AgentUser>,
    state: Arc<Mutex<SyncState>>,
}

impl<A: ControlApi + Clone + Send + Sync + 'static> R2TranscriptSync<A> {
    /// Creates a new synchronizer backed by `api`.
    #[must_use]
    pub fn new(api: A, project_dir_name: Option<String>, agent: Option<AgentUser>) -> Self {
        Self {
            api,
            project_dir_name,
            agent,
            state: Arc::new(Mutex::new(SyncState::default())),
        }
    }

    fn resolve_project_key(&self, cwd: &Path) -> String {
        self.project_dir_name
            .clone()
            .unwrap_or_else(|| encode_project_dir(cwd))
    }

    fn project_dir(&self, home: &Path, cwd: &Path) -> PathBuf {
        home.join(".claude")
            .join("projects")
            .join(self.resolve_project_key(cwd))
    }

    async fn seed_stream(&self, stream: &str) -> Result<StreamCursor, SdkError> {
        {
            let guard = self.state.lock().await;
            if let Some(cursor) = guard.streams.get(stream) {
                return Ok(cursor.clone());
            }
        }
        let read = self
            .api
            .get_transcript(stream)
            .await
            .map_err(|error| SdkError::Protocol(format!("transcript read failed: {error}")))?;
        let cursor = StreamCursor {
            next_seq: read.batches,
            synced_bytes: read.body.len(),
        };
        self.state
            .lock()
            .await
            .streams
            .insert(stream.to_owned(), cursor.clone());
        Ok(cursor)
    }

    async fn push_file(&self, stream: &str, path: &Path) -> Result<(), SdkError> {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(SdkError::from(error)),
        };
        if bytes.is_empty() {
            return Ok(());
        }

        let mut cursor = self.seed_stream(stream).await?;
        let start = if bytes.len() < cursor.synced_bytes {
            // The local file shrank (e.g. rewritten); mirror from offset 0 so
            // no lines are lost.
            0
        } else {
            cursor.synced_bytes
        };
        if start >= bytes.len() {
            return Ok(());
        }

        let mut offset = start;
        while offset < bytes.len() {
            let end = next_chunk_end(&bytes, offset, MAX_BATCH_BYTES);
            let chunk = bytes[offset..end].to_vec();
            self.api
                .put_transcript_batch(stream, cursor.next_seq, chunk)
                .await
                .map_err(|error| {
                    SdkError::Protocol(format!("transcript push failed for `{stream}`: {error}"))
                })?;
            cursor.next_seq += 1;
            offset = end;
        }
        cursor.synced_bytes = bytes.len();

        self.state
            .lock()
            .await
            .streams
            .insert(stream.to_owned(), cursor);
        tracing::debug!(
            stream,
            bytes = bytes.len(),
            "mirrored transcript updates to the control plane"
        );
        Ok(())
    }

    async fn write_pulled_file(
        &self,
        path: &Path,
        bytes: &[u8],
        chown_roots: &[&Path],
    ) -> Result<(), SdkError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, bytes).await?;
        if let Some(user) = &self.agent
            && user.needs_uid_switch()
        {
            for dir in chown_roots {
                if dir.exists() {
                    chown_path(dir, Some(user.uid), Some(user.gid))
                        .map_err(|error| SdkError::Io(std::io::Error::other(error).into()))?;
                }
            }
            chown_path(path, Some(user.uid), Some(user.gid))
                .map_err(|error| SdkError::Io(std::io::Error::other(error).into()))?;
        }
        Ok(())
    }

    async fn record_subagents(
        &self,
        index_stream: &str,
        new_paths: &[String],
    ) -> Result<(), SdkError> {
        if new_paths.is_empty() {
            return Ok(());
        }
        let mut cursor = self.seed_stream(index_stream).await?;
        let mut body = Vec::new();
        for path in new_paths {
            body.extend_from_slice(path.as_bytes());
            body.push(b'\n');
        }
        self.api
            .put_transcript_batch(index_stream, cursor.next_seq, body)
            .await
            .map_err(|error| {
                SdkError::Protocol(format!(
                    "subagent index push failed for `{index_stream}`: {error}"
                ))
            })?;
        cursor.next_seq += 1;
        cursor.synced_bytes += new_paths.iter().map(|p| p.len() + 1).sum::<usize>();
        let mut guard = self.state.lock().await;
        guard.streams.insert(index_stream.to_owned(), cursor);
        for path in new_paths {
            guard.known_subagents.insert(path.clone());
        }
        drop(guard);
        Ok(())
    }

    /// Seeds the main Claude transcript stream in object storage when no
    /// batches exist yet, so a locally resumed session is immediately durable
    /// across future machine restarts and resizes.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] if stream key derivation or any control-plane I/O
    /// fails.
    pub async fn seed_if_empty(
        &self,
        session_id: &str,
        cwd: &Path,
        bytes: &[u8],
    ) -> Result<(), SdkError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let project_key = self.resolve_project_key(cwd);
        let main_stream = stream_key(&project_key, session_id, None)
            .map_err(|error| SdkError::Protocol(error.to_string()))?;
        let existing = self
            .api
            .get_transcript(&main_stream)
            .await
            .map_err(|error| {
                SdkError::Protocol(format!(
                    "transcript read failed for `{main_stream}`: {error}"
                ))
            })?;
        if existing.batches > 0 {
            return Ok(());
        }
        let mut seq = 0_u64;
        let mut offset = 0_usize;
        while offset < bytes.len() {
            let end = next_chunk_end(bytes, offset, MAX_BATCH_BYTES);
            self.api
                .put_transcript_batch(&main_stream, seq, bytes[offset..end].to_vec())
                .await
                .map_err(|error| {
                    SdkError::Protocol(format!(
                        "transcript seed push failed for `{main_stream}`: {error}"
                    ))
                })?;
            seq += 1;
            offset = end;
        }
        Ok(())
    }
}

impl<A: ControlApi + Clone + Send + Sync + 'static> TranscriptSync for R2TranscriptSync<A> {
    async fn pull(&self, session_id: &str, cwd: &Path, home: &Path) -> Result<(), SdkError> {
        let project_key = self.resolve_project_key(cwd);
        let main_stream = stream_key(&project_key, session_id, None)
            .map_err(|error| SdkError::Protocol(error.to_string()))?;
        let index_stream = stream_key(&project_key, session_id, Some(SUBAGENT_INDEX_SUBPATH))
            .map_err(|error| SdkError::Protocol(error.to_string()))?;

        let projects_root = home.join(".claude").join("projects");
        let project_dir = self.project_dir(home, cwd);
        let main_path = project_dir.join(format!("{session_id}.jsonl"));

        let main_read = self
            .api
            .get_transcript(&main_stream)
            .await
            .map_err(|error| {
                SdkError::Protocol(format!(
                    "transcript pull failed for `{main_stream}`: {error}"
                ))
            })?;
        {
            let mut guard = self.state.lock().await;
            guard.streams.insert(
                main_stream.clone(),
                StreamCursor {
                    next_seq: main_read.batches,
                    synced_bytes: main_read.body.len(),
                },
            );
        }
        if !main_read.body.is_empty() {
            self.write_pulled_file(&main_path, &main_read.body, &[&projects_root, &project_dir])
                .await?;
        }

        let index_read = self
            .api
            .get_transcript(&index_stream)
            .await
            .map_err(|error| {
                SdkError::Protocol(format!(
                    "subagent index pull failed for `{index_stream}`: {error}"
                ))
            })?;
        {
            let mut guard = self.state.lock().await;
            guard.streams.insert(
                index_stream.clone(),
                StreamCursor {
                    next_seq: index_read.batches,
                    synced_bytes: index_read.body.len(),
                },
            );
        }

        let index_text = String::from_utf8(index_read.body).map_err(|error| {
            SdkError::Protocol(format!("subagent index is not valid UTF-8: {error}"))
        })?;
        let mut subagent_paths = BTreeSet::new();
        for line in index_text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let validated = validate_subagent_relpath(line)
                .map_err(|error| SdkError::Protocol(error.to_string()))?;
            subagent_paths.insert(validated.to_string_lossy().into_owned());
        }

        let session_dir = project_dir.join(session_id);
        let subagents_dir = session_dir.join("subagents");
        for relpath in subagent_paths {
            let sub_stream = stream_key(&project_key, session_id, Some(&relpath))
                .map_err(|error| SdkError::Protocol(error.to_string()))?;
            let sub_read = self
                .api
                .get_transcript(&sub_stream)
                .await
                .map_err(|error| {
                    SdkError::Protocol(format!(
                        "subagent transcript pull failed for `{sub_stream}`: {error}"
                    ))
                })?;
            {
                let mut guard = self.state.lock().await;
                guard.streams.insert(
                    sub_stream.clone(),
                    StreamCursor {
                        next_seq: sub_read.batches,
                        synced_bytes: sub_read.body.len(),
                    },
                );
                guard.known_subagents.insert(relpath.clone());
            }
            if !sub_read.body.is_empty() {
                let target = session_dir.join(&relpath);
                self.write_pulled_file(
                    &target,
                    &sub_read.body,
                    &[&projects_root, &project_dir, &session_dir, &subagents_dir],
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn push(&self, session_id: &str, cwd: &Path, home: &Path) -> Result<(), SdkError> {
        let project_key = self.resolve_project_key(cwd);
        let main_stream = stream_key(&project_key, session_id, None)
            .map_err(|error| SdkError::Protocol(error.to_string()))?;
        let index_stream = stream_key(&project_key, session_id, Some(SUBAGENT_INDEX_SUBPATH))
            .map_err(|error| SdkError::Protocol(error.to_string()))?;

        let preferred_project_dir = self.project_dir(home, cwd);
        let preferred_main = preferred_project_dir.join(format!("{session_id}.jsonl"));
        let (actual_project_dir, main_path) = if tokio::fs::metadata(&preferred_main).await.is_ok()
        {
            (preferred_project_dir, preferred_main)
        } else if let Some(found) = find_transcript_path(home, session_id, Some(cwd)).await? {
            let dir = found
                .parent()
                .map_or(preferred_project_dir, Path::to_path_buf);
            (dir, found)
        } else {
            return Ok(());
        };

        self.push_file(&main_stream, &main_path).await?;

        let subagents_dir = actual_project_dir.join(session_id).join("subagents");
        let mut entries = match tokio::fs::read_dir(&subagents_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(SdkError::from(error)),
        };

        let mut discovered = BTreeSet::new();
        while let Some(entry) = entries.next_entry().await? {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let relpath = format!("subagents/{name}");
            if validate_subagent_relpath(&relpath).is_ok() {
                discovered.insert(relpath);
            }
        }

        let new_subagents: Vec<String> = {
            let guard = self.state.lock().await;
            discovered
                .iter()
                .filter(|rel| !guard.known_subagents.contains(*rel))
                .cloned()
                .collect()
        };
        self.record_subagents(&index_stream, &new_subagents).await?;

        for relpath in discovered {
            let sub_stream = stream_key(&project_key, session_id, Some(&relpath))
                .map_err(|error| SdkError::Protocol(error.to_string()))?;
            let sub_path = actual_project_dir.join(session_id).join(&relpath);
            self.push_file(&sub_stream, &sub_path).await?;
        }

        Ok(())
    }
}

/// Picks the end offset for the next batch chunk starting at `offset`,
/// preferring a newline boundary within `max_bytes` so batches stay aligned on
/// JSONL records whenever possible.
fn next_chunk_end(bytes: &[u8], offset: usize, max_bytes: usize) -> usize {
    let limit = (offset + max_bytes).min(bytes.len());
    if limit == bytes.len() {
        return limit;
    }
    bytes[offset..limit]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(limit, |pos| offset + pos + 1)
}
