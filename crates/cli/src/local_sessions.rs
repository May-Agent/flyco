//! Unified local harness session discovery and native payload snapshotting.
//!
//! Used by both `flyco handoff` and the interactive TUI's `/resume-claude`,
//! `/resume-codex`, `/resume-agy`, and `/resume-devin` commands. Every local
//! session resumes on a cloud session running the **same** harness, with its
//! native transcript (or single-session SQLite database) uploaded verbatim for
//! `flycod` to materialize into that harness's native store.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flyco_core::{HandoffManifest, HarnessKind, LocalHandoff, SessionDetail, SessionSource};
use sha2::{Digest as _, Sha256};
use sqlx::Row as _;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::io::{AsyncReadExt as _, BufReader};
use tokio::process::Command;

use crate::cli::SessionSpec;
use crate::client::Api;
use crate::{Exit, Failure, Outcome};

/// One local harness session discovered on this machine.
#[derive(Debug, Clone)]
pub struct LocalSession {
    /// Harness that owns this local session.
    pub harness: HarnessKind,
    /// Harness-native conversation/session identifier.
    pub session_id: String,
    /// Backing transcript or database file on disk.
    pub source_path: PathBuf,
    /// Recorded working directory when available.
    pub cwd: Option<PathBuf>,
    /// Last modification timestamp for recency sorting.
    pub modified: SystemTime,
    /// Human-readable title (custom title, summary title, or first user prompt).
    pub title: String,
}

/// Resolved local git worktree context for optional patch upload.
#[derive(Debug, Clone)]
pub struct LocalRepo {
    /// `git rev-parse --show-toplevel`.
    pub root: PathBuf,
    /// `owner/name` parsed from `origin`.
    pub slug: String,
    /// Remote branch name tracked on `origin`.
    pub branch: String,
    /// `git merge-base HEAD origin/<branch>`.
    pub base_commit: String,
}

impl LocalRepo {
    /// Produces the binary diff against [`Self::base_commit`].
    ///
    /// # Errors
    /// Returns [`Failure`] when `git` fails.
    pub async fn patch(&self, include_untracked: bool) -> Outcome<Vec<u8>> {
        if !include_untracked {
            return Ok(
                git(&self.root, &["diff", "--binary", &self.base_commit, "--"])
                    .await?
                    .stdout,
            );
        }
        let index = std::env::temp_dir().join(format!("flyco-resume-{}.index", std::process::id()));
        let staged = async {
            git_env(
                &self.root,
                &[OsStr::new("read-tree"), OsStr::new(&self.base_commit)],
                &index,
            )
            .await?;
            git_env(&self.root, &[OsStr::new("add"), OsStr::new("-A")], &index).await?;
            git_env(
                &self.root,
                &[
                    OsStr::new("diff"),
                    OsStr::new("--cached"),
                    OsStr::new("--binary"),
                    OsStr::new(&self.base_commit),
                    OsStr::new("--"),
                ],
                &index,
            )
            .await
        }
        .await;
        let _ = std::fs::remove_file(&index);
        Ok(staged?.stdout)
    }
}

/// Human label for a harness.
#[must_use]
pub const fn harness_label(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::ClaudeCode => "claude",
        HarnessKind::Codex => "codex",
        HarnessKind::Devin => "devin",
        HarnessKind::Antigravity => "agy",
    }
}

/// Discovers all local sessions for `harness`, sorted newest first.
///
/// When `worktree_filter` is `Some(root)`, only sessions whose recorded cwd
/// sits inside `root` are returned.
///
/// # Errors
/// Returns [`Failure`] on unexpected filesystem or database read failures.
pub async fn discover(
    harness: HarnessKind,
    worktree_filter: Option<&Path>,
) -> Outcome<Vec<LocalSession>> {
    let mut found = match harness {
        HarnessKind::ClaudeCode => discover_claude(worktree_filter).await?,
        HarnessKind::Codex => discover_codex(worktree_filter).await?,
        HarnessKind::Devin => discover_devin(worktree_filter).await?,
        HarnessKind::Antigravity => discover_antigravity(worktree_filter).await?,
    };
    found.sort_by_key(|session| std::cmp::Reverse(session.modified));
    Ok(found)
}

/// Resolves an explicit user query (filesystem path, exact session id, or
/// unambiguous prefix) to one local session of `harness`.
///
/// # Errors
/// Returns [`Failure`] when no session matches or the query is ambiguous.
pub async fn resolve_query(harness: HarnessKind, query: &str) -> Outcome<LocalSession> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Failure::usage(
            "expected a session id, prefix, or file path",
        ));
    }

    let as_path = Path::new(query);
    if as_path.is_file()
        && let Some(session) = session_from_path(harness, as_path).await?
    {
        return Ok(session);
    }

    let all = discover(harness, None).await?;
    if let Some(exact) = all.iter().find(|s| s.session_id == query) {
        return Ok(exact.clone());
    }
    let mut iter = all
        .into_iter()
        .filter(|s| s.session_id.starts_with(query) || s.session_id.contains(query));
    match (iter.next(), iter.next()) {
        (None, _) => Err(Failure::problem(
            Exit::NotFoundOrConflict,
            format!(
                "no local {} session matches `{query}`",
                harness_label(harness)
            ),
        )),
        (Some(one), None) => Ok(one),
        (Some(_), Some(_)) => {
            let count = iter.count() + 2;
            Err(Failure::usage(format!(
                "ambiguous {} session query `{query}` ({count} matches) — pass a longer id",
                harness_label(harness)
            )))
        }
    }
}

/// Attempts to resolve local git worktree context from `dir`.
///
/// Returns `Ok(None)` when `dir` is not a git worktree or has no pushed
/// `origin/<branch>` merge-base (e.g., resuming a non-git or clean cloud repo
/// session directly from the TUI).
pub async fn try_local_repo(
    dir: &Path,
    repo_override: Option<&str>,
    branch_override: Option<&str>,
) -> Option<LocalRepo> {
    let root = PathBuf::from(
        git_text(dir, &["rev-parse", "--show-toplevel"])
            .await
            .ok()?,
    );
    let slug = if let Some(slug) = repo_override {
        slug.to_owned()
    } else {
        let url = git_text(&root, &["remote", "get-url", "origin"])
            .await
            .ok()?;
        repo_slug(&url)?
    };
    let branch = match branch_override {
        Some(branch) => branch.to_owned(),
        None => git_text(&root, &["branch", "--show-current"]).await.ok()?,
    };
    let remote_ref = format!("refs/remotes/origin/{branch}");
    git(&root, &["rev-parse", "--verify", "--quiet", &remote_ref])
        .await
        .ok()?;
    let base_commit = git_text(&root, &["merge-base", "HEAD", &remote_ref])
        .await
        .ok()?;
    Some(LocalRepo {
        root,
        slug,
        branch,
        base_commit,
    })
}

/// Strict git worktree resolution used by `flyco handoff`.
///
/// # Errors
/// Returns [`Failure`] with an actionable usage error when `dir` is not a
/// pushed git checkout.
pub async fn require_local_repo(
    dir: &Path,
    repo: Option<&str>,
    branch: Option<&str>,
) -> Outcome<LocalRepo> {
    let root = PathBuf::from(
        git_text(dir, &["rev-parse", "--show-toplevel"])
            .await
            .map_err(|_| Failure::usage("`flyco handoff` runs inside a git worktree"))?,
    );
    let slug = if let Some(slug) = repo {
        slug.to_owned()
    } else {
        let url = git_text(&root, &["remote", "get-url", "origin"])
            .await
            .map_err(|_| {
                Failure::usage("no `origin` remote to infer the repo from — pass `--repo`")
            })?;
        repo_slug(&url).ok_or_else(|| {
            Failure::usage(format!(
                "cannot read owner/name out of `origin` ({url}) — pass `--repo`"
            ))
        })?
    };
    let branch = match branch {
        Some(branch) => branch.to_owned(),
        None => git_text(&root, &["branch", "--show-current"])
            .await
            .map_err(|_| {
                Failure::usage("HEAD is detached — name the remote branch with `--branch`")
            })?,
    };
    let remote_ref = format!("refs/remotes/origin/{branch}");
    let exists = git(&root, &["rev-parse", "--verify", "--quiet", &remote_ref])
        .await
        .is_ok();
    if !exists {
        return Err(Failure::usage(format!(
            "`origin/{branch}` is not known locally — `git fetch`, or the branch is local-only \
             and `--branch` should name a pushed one"
        )));
    }
    let base_commit = git_text(&root, &["merge-base", "HEAD", &remote_ref])
        .await
        .map_err(|_| {
            Failure::usage(format!(
                "HEAD and `origin/{branch}` share no commit — nothing to base a patch on"
            ))
        })?;
    Ok(LocalRepo {
        root,
        slug,
        branch,
        base_commit,
    })
}

/// `git@github.com:owner/name`, `https://github.com/owner/name`, or
/// `ssh://git@github.com/owner/name` -> `owner/name`.
#[must_use]
pub fn repo_slug(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches(".git").trim_end_matches('/');
    let path = if let Some(scp) = url.strip_prefix("git@") {
        scp.split(':').nth(1)?
    } else {
        let path = url.split_once("://").map_or(url, |(_, rest)| rest);
        path.split_once('/')?.1
    };
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// Creates a same-harness cloud session seeded with `session`'s native
/// transcript payload and optional local git working-tree patch.
///
/// Completes the upload manifest so provisioning starts and returns the
/// created [`SessionDetail`].
///
/// # Errors
/// Returns [`Failure`] on snapshot, session creation, or upload errors.
pub async fn resume_to_cloud(
    api: &Api,
    session: &LocalSession,
    local_repo: Option<&LocalRepo>,
    mut spec: SessionSpec,
    extra_message: Option<String>,
    include_untracked: bool,
) -> Outcome<SessionDetail> {
    let (patch, base_commit, local_workdir) = if let Some(repo) = local_repo {
        let patch = repo.patch(include_untracked).await?;
        (
            patch,
            repo.base_commit.clone(),
            repo.root.display().to_string(),
        )
    } else {
        let workdir = session
            .cwd
            .as_ref()
            .map_or_default(|p| p.display().to_string());
        (Vec::new(), String::new(), workdir)
    };

    let env = std::mem::take(&mut spec.env);
    if spec.repo.is_empty()
        && let Some(repo) = local_repo
    {
        spec.repo.push(repo.slug.clone());
    }
    spec.resolve_runtime(api).await?;

    let prompt = extra_message.unwrap_or_default();
    let mut request = spec.to_request(session.harness, prompt)?;
    if let Some(repo) = local_repo
        && let Some(first) = request.repos.first_mut()
    {
        *first = flyco_core::RepoSelection {
            repo: repo.slug.clone(),
            branch: Some(repo.branch.clone()),
        };
    }
    request.source = Some(SessionSource::LocalHandoff(LocalHandoff {
        harness: session.harness,
        session_id: session.session_id.clone(),
        base_commit,
        local_workdir,
    }));

    let payload_file = snapshot_payload(session).await?;
    let headers: &[(&str, &str)] = match &spec.idempotency_key {
        Some(key) => &[("Idempotency-Key", key.as_str())],
        None => &[],
    };
    let created: SessionDetail = match api
        .post_with_headers("/v1/sessions", &request, headers)
        .await
    {
        Ok(detail) => detail,
        Err(error) => {
            let _ = tokio::fs::remove_file(&payload_file).await;
            return Err(error);
        }
    };
    let id = created.summary.id;
    if let Err(error) = crate::session::set_env(api, &id, &env).await {
        let _ = tokio::fs::remove_file(&payload_file).await;
        let _ = archive_on_abort(api, id).await;
        return Err(error);
    }

    let uploaded = upload(api, id, &patch, &payload_file).await;
    let _ = tokio::fs::remove_file(&payload_file).await;
    if let Err(failure) = uploaded {
        let _ = archive_on_abort(api, id).await;
        return Err(failure);
    }
    Ok(created)
}

async fn archive_on_abort(api: &Api, id: flyco_core::SessionId) -> Outcome<()> {
    api.post_json_empty(
        &format!("/v1/sessions/{id}/archive"),
        &serde_json::json!({}),
    )
    .await
}

async fn upload(api: &Api, id: flyco_core::SessionId, patch: &[u8], payload: &Path) -> Outcome<()> {
    api.put_bytes(&format!("/v1/sessions/{id}/handoff/patch"), patch.to_vec())
        .await?;
    api.put_file(&format!("/v1/sessions/{id}/handoff/transcript"), payload)
        .await?;
    let transcript_sha256 = sha256_file(payload).await?;
    let transcript_bytes = tokio::fs::metadata(payload)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", payload.display())))?
        .len();
    api.post_json_empty(
        &format!("/v1/sessions/{id}/handoff/complete"),
        &HandoffManifest {
            patch_sha256: hex::encode(Sha256::digest(patch)),
            patch_bytes: patch.len() as u64,
            transcript_sha256,
            transcript_bytes,
        },
    )
    .await?;
    Ok(())
}

/// Produces a self-contained snapshot file of `session`'s native payload.
///
/// - Claude Code & Codex: verbatim copy of the JSONL file.
/// - Antigravity: SQLite `VACUUM INTO` snapshot of `conversations/<id>.db` so
///   uncheckpointed WAL frames are merged cleanly into one `.db` file.
/// - Devin: extracts a single-session standalone SQLite `sessions.db` with
///   only `session.session_id`'s rows so the user's multi-gigabyte local
///   database is never uploaded whole.
///
/// # Errors
/// Returns [`Failure`] on I/O or SQLite snapshot failures.
pub async fn snapshot_payload(session: &LocalSession) -> Outcome<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dest = std::env::temp_dir().join(format!(
        "flyco-native-resume-{}-{stamp}.payload",
        std::process::id()
    ));
    match session.harness {
        HarnessKind::ClaudeCode | HarnessKind::Codex => {
            tokio::fs::copy(&session.source_path, &dest)
                .await
                .map_err(|error| {
                    Failure::transport(format!("{}: {error}", session.source_path.display()))
                })?;
        }
        HarnessKind::Antigravity => {
            snapshot_sqlite_single_db(&session.source_path, &dest).await?;
        }
        HarnessKind::Devin => {
            extract_devin_single_session(&session.source_path, &session.session_id, &dest).await?;
        }
    }
    Ok(dest)
}

/// Snapshots a standalone SQLite database (Antigravity `conversations/<id>.db`)
/// into `dest` via `VACUUM INTO`, folding any active WAL frames safely without
/// mutating the source file.
async fn snapshot_sqlite_single_db(source: &Path, dest: &Path) -> Outcome<()> {
    let _ = tokio::fs::remove_file(dest).await;
    let opts = SqliteConnectOptions::new()
        .filename(source)
        .read_only(true)
        .busy_timeout(Duration::from_secs(5));
    if let Ok(pool) = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
    {
        let result = sqlx::query("VACUUM INTO ?")
            .bind(dest.display().to_string())
            .execute(&pool)
            .await;
        pool.close().await;
        if result.is_ok() && dest.is_file() {
            return Ok(());
        }
        let _ = tokio::fs::remove_file(dest).await;
    }
    // Fallback when SQLite cannot VACUUM INTO (e.g., immutable snapshot): copy bytes directly.
    tokio::fs::copy(source, dest)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", source.display())))?;
    Ok(())
}

/// Extracts one Devin session (`session_id`) out of `source_db` into `dest_db`.
///
/// Preserves the full schema and migration metadata (`refinery_schema_history`,
/// `app_state`, and all `session_id`-scoped tables).
///
/// # Errors
/// Returns [`Failure`] when the source database cannot be read or the session
/// is absent.
#[expect(
    clippy::too_many_lines,
    reason = "copies sqlite schema and session-scoped rows in one pass"
)]
pub async fn extract_devin_single_session(
    source_db: &Path,
    session_id: &str,
    dest_db: &Path,
) -> Outcome<()> {
    let _ = tokio::fs::remove_file(dest_db).await;

    let src_opts = SqliteConnectOptions::new()
        .filename(source_db)
        .read_only(true)
        .busy_timeout(Duration::from_secs(5));
    let src_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(src_opts)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", source_db.display())))?;

    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM sessions WHERE id = ?")
        .bind(session_id)
        .fetch_optional(&src_pool)
        .await
        .map_err(|error| Failure::transport(format!("reading devin sessions.db: {error}")))?;
    if exists.is_none() {
        src_pool.close().await;
        return Err(Failure::problem(
            Exit::NotFoundOrConflict,
            format!("no Devin session `{session_id}` in {}", source_db.display()),
        ));
    }

    // Read DDL definitions in deterministic order: tables -> virtual tables -> indexes -> triggers.
    let ddl_rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT type, name, sql FROM sqlite_master \
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
         ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 WHEN 'trigger' THEN 2 ELSE 3 END, name",
    )
    .fetch_all(&src_pool)
    .await
    .map_err(|error| Failure::transport(format!("reading devin schema: {error}")))?;

    let dst_opts = SqliteConnectOptions::new()
        .filename(dest_db)
        .create_if_missing(true);
    let dst_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(dst_opts)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", dest_db.display())))?;

    // Skip FTS5 shadow tables (created automatically by CREATE VIRTUAL TABLE).
    let fts_bases: Vec<String> = ddl_rows
        .iter()
        .filter(|(kind, _, sql)| {
            kind == "table"
                && sql
                    .as_deref()
                    .is_some_and(|s| s.to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE"))
        })
        .map(|(_, name, _)| name.clone())
        .collect();

    let is_fts_shadow = |name: &str| {
        fts_bases.iter().any(|base| {
            name.strip_prefix(base).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "_data" | "_idx" | "_content" | "_docsize" | "_config"
                )
            })
        })
    };

    for (_, name, sql) in &ddl_rows {
        if is_fts_shadow(name) {
            continue;
        }
        if let Some(stmt) = sql {
            sqlx::query(sqlx::AssertSqlSafe(stmt.clone()))
                .execute(&dst_pool)
                .await
                .map_err(|error| {
                    Failure::transport(format!("replaying devin DDL `{name}`: {error}"))
                })?;
        }
    }

    // Attach source read-only to copy rows efficiently inside SQLite without
    // marshaling large blobs through Rust.
    sqlx::query("ATTACH DATABASE ? AS src")
        .bind(format!("file:{}?mode=ro", source_db.display()))
        .execute(&dst_pool)
        .await
        .map_err(|error| Failure::transport(format!("attaching devin source db: {error}")))?;

    let tables: Vec<String> = ddl_rows
        .iter()
        .filter(|(kind, name, sql)| {
            kind == "table"
                && !is_fts_shadow(name)
                && !sql
                    .as_deref()
                    .is_some_and(|s| s.to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE"))
        })
        .map(|(_, name, _)| name.clone())
        .collect();

    for table in &tables {
        // Inspect table columns so forward-compatible schemas copy cleanly.
        let pragma = format!("PRAGMA src.table_info(\"{table}\")");
        let col_rows = sqlx::query(sqlx::AssertSqlSafe(pragma))
            .fetch_all(&dst_pool)
            .await
            .map_err(|error| Failure::transport(format!("inspecting `{table}`: {error}")))?;
        let cols: Vec<String> = col_rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("name").ok())
            .collect();
        if cols.is_empty() {
            continue;
        }
        let col_list = cols
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");

        let copy_res = if table == "sessions" {
            let q = format!(
                "INSERT INTO main.\"{table}\" ({col_list}) \
                 SELECT {col_list} FROM src.\"{table}\" WHERE id = ?"
            );
            sqlx::query(sqlx::AssertSqlSafe(q))
                .bind(session_id)
                .execute(&dst_pool)
                .await
        } else if cols.iter().any(|c| c == "session_id") {
            let q = format!(
                "INSERT INTO main.\"{table}\" ({col_list}) \
                 SELECT {col_list} FROM src.\"{table}\" WHERE session_id = ?"
            );
            sqlx::query(sqlx::AssertSqlSafe(q))
                .bind(session_id)
                .execute(&dst_pool)
                .await
        } else if matches!(table.as_str(), "refinery_schema_history" | "app_state") {
            let q = format!(
                "INSERT INTO main.\"{table}\" ({col_list}) SELECT {col_list} FROM src.\"{table}\""
            );
            sqlx::query(sqlx::AssertSqlSafe(q)).execute(&dst_pool).await
        } else {
            continue;
        };
        copy_res.map_err(|error| {
            Failure::transport(format!("copying devin table `{table}`: {error}"))
        })?;
    }

    let _ = sqlx::query("DETACH DATABASE src").execute(&dst_pool).await;
    dst_pool.close().await;
    src_pool.close().await;
    Ok(())
}

fn home() -> PathBuf {
    std::env::home_dir().expect("a home directory")
}

async fn session_from_path(harness: HarnessKind, path: &Path) -> Outcome<Option<LocalSession>> {
    match harness {
        HarnessKind::ClaudeCode => claude_session(path, None).await,
        HarnessKind::Codex => codex_session(path, None).await,
        HarnessKind::Antigravity => {
            let id = path
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned();
            if id.is_empty() {
                return Ok(None);
            }
            Ok(Some(LocalSession {
                harness: HarnessKind::Antigravity,
                title: id.clone(),
                session_id: id,
                source_path: path.to_path_buf(),
                cwd: None,
                modified: mtime(path).await,
            }))
        }
        HarnessKind::Devin => Ok(None),
    }
}

async fn discover_claude(worktree_filter: Option<&Path>) -> Outcome<Vec<LocalSession>> {
    let dir = home().join(".claude/projects");
    let Some(mut projects) = read_dir(&dir).await else {
        return Ok(Vec::new());
    };
    let encoded_prefix = worktree_filter.map(claude_project_dir);
    let mut found = Vec::new();
    while let Some(project) = entries_next(&mut projects).await {
        if let Some(prefix) = &encoded_prefix
            && !project.file_name().to_string_lossy().starts_with(prefix)
        {
            continue;
        }
        let Some(mut files) = read_dir(&project.path()).await else {
            continue;
        };
        while let Some(file) = entries_next(&mut files).await {
            let path = file.path();
            if path.extension() != Some(OsStr::new("jsonl")) {
                continue;
            }
            if let Some(session) = claude_session(&path, worktree_filter).await? {
                found.push(session);
            }
        }
    }
    Ok(found)
}

fn claude_project_dir(dir: &Path) -> String {
    dir.display()
        .to_string()
        .chars()
        .map(|c| match c {
            '/' | '.' | '_' => '-',
            other => other,
        })
        .collect()
}

async fn claude_session(
    path: &Path,
    worktree_filter: Option<&Path>,
) -> Outcome<Option<LocalSession>> {
    let mut cwd: Option<PathBuf> = None;
    let mut custom_title = String::new();
    let mut first_prompt = String::new();

    for line in head_lines(path, 64 * 1024).await? {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if cwd.is_none()
            && let Some(dir) = entry.get("cwd").and_then(|value| value.as_str())
        {
            cwd = Some(PathBuf::from(dir));
        }
        if custom_title.is_empty()
            && let Some(name) = entry.get("customTitle").and_then(|value| value.as_str())
        {
            name.clone_into(&mut custom_title);
        }
        if first_prompt.is_empty()
            && entry.get("type").and_then(|v| v.as_str()) == Some("user")
            && !entry
                .get("isMeta")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            && let Some(text) = extract_claude_user_text(&entry["message"]["content"])
            && is_title_prompt(&text)
        {
            first_prompt = first_line_preview(&text, 96);
        }
        if cwd.is_some() && !custom_title.is_empty() {
            break;
        }
    }

    if let Some(root) = worktree_filter {
        let Some(ref dir) = cwd else {
            return Ok(None);
        };
        if !dir.starts_with(root) {
            return Ok(None);
        }
    }

    let Some(session_id) = path.file_stem().and_then(OsStr::to_str).map(str::to_owned) else {
        return Ok(None);
    };
    let title = if !custom_title.is_empty() {
        custom_title
    } else if !first_prompt.is_empty() {
        first_prompt
    } else {
        session_id.clone()
    };
    Ok(Some(LocalSession {
        harness: HarnessKind::ClaudeCode,
        session_id,
        source_path: path.to_path_buf(),
        cwd,
        modified: mtime(path).await,
        title,
    }))
}

fn extract_claude_user_text(content: &serde_json::Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        let trimmed = text.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }
    if let Some(blocks) = content.as_array() {
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) == Some("text")
                && let Some(text) = block.get("text").and_then(|v| v.as_str())
            {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
        }
    }
    None
}

async fn discover_codex(worktree_filter: Option<&Path>) -> Outcome<Vec<LocalSession>> {
    let dir = home().join(".codex/sessions");
    let Some(mut years) = read_dir(&dir).await else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    while let Some(year) = entries_next(&mut years).await {
        let Some(mut months) = read_dir(&year.path()).await else {
            continue;
        };
        while let Some(month) = entries_next(&mut months).await {
            let Some(mut days) = read_dir(&month.path()).await else {
                continue;
            };
            while let Some(day) = entries_next(&mut days).await {
                let Some(mut files) = read_dir(&day.path()).await else {
                    continue;
                };
                while let Some(file) = entries_next(&mut files).await {
                    let path = file.path();
                    if path.extension() != Some(OsStr::new("jsonl")) {
                        continue;
                    }
                    if let Some(session) = codex_session(&path, worktree_filter).await? {
                        found.push(session);
                    }
                }
            }
        }
    }
    Ok(found)
}

async fn codex_session(
    path: &Path,
    worktree_filter: Option<&Path>,
) -> Outcome<Option<LocalSession>> {
    let lines = head_lines(path, 64 * 1024).await?;
    let Some(first) = lines.first() else {
        return Ok(None);
    };
    let Ok(meta_line) = serde_json::from_str::<serde_json::Value>(first) else {
        return Ok(None);
    };
    let meta = &meta_line["payload"];
    let (Some(id), Some(cwd)) = (
        meta["id"].as_str().map(str::to_owned),
        meta["cwd"].as_str().map(PathBuf::from),
    ) else {
        return Ok(None);
    };
    if let Some(root) = worktree_filter
        && !cwd.starts_with(root)
    {
        return Ok(None);
    }

    let mut title = String::new();
    for line in lines.iter().skip(1) {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let candidate =
            if entry["type"] == "event_msg" && entry["payload"]["type"] == "user_message" {
                entry["payload"]["message"].as_str().map(str::to_owned)
            } else if entry["type"] == "response_item"
                && entry["payload"]["type"] == "message"
                && entry["payload"]["role"] == "user"
            {
                entry["payload"]["content"].as_array().and_then(|parts| {
                    parts.iter().find_map(|part| {
                        part.get("text").and_then(|t| t.as_str()).map(str::to_owned)
                    })
                })
            } else {
                None
            };
        if let Some(text) = candidate
            && is_title_prompt(&text)
        {
            title = first_line_preview(&text, 96);
            break;
        }
    }
    if title.is_empty() {
        title.clone_from(&id);
    }

    Ok(Some(LocalSession {
        harness: HarnessKind::Codex,
        session_id: id,
        source_path: path.to_path_buf(),
        cwd: Some(cwd),
        modified: mtime(path).await,
        title,
    }))
}

async fn discover_devin(worktree_filter: Option<&Path>) -> Outcome<Vec<LocalSession>> {
    let db_path = home().join(".local/share/devin/cli/sessions.db");
    if !db_path.is_file() {
        return Ok(Vec::new());
    }
    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .read_only(true)
        .busy_timeout(Duration::from_secs(3));
    let Ok(pool) = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
    else {
        return Ok(Vec::new());
    };

    let rows: Vec<(String, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT id, working_directory, title, last_activity_at \
         FROM sessions WHERE hidden = 0 ORDER BY last_activity_at DESC LIMIT 250",
    )
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    pool.close().await;

    let mut found = Vec::new();
    for (id, working_directory, title, last_activity_at) in rows {
        let cwd = PathBuf::from(&working_directory);
        if let Some(root) = worktree_filter
            && !cwd.starts_with(root)
        {
            continue;
        }
        let modified = if last_activity_at > 0 {
            UNIX_EPOCH + Duration::from_secs(last_activity_at.cast_unsigned())
        } else {
            UNIX_EPOCH
        };
        let label = title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map_or_else(|| id.clone(), |t| first_line_preview(t, 96));
        found.push(LocalSession {
            harness: HarnessKind::Devin,
            session_id: id,
            source_path: db_path.clone(),
            cwd: Some(cwd),
            modified,
            title: label,
        });
    }
    Ok(found)
}

async fn discover_antigravity(worktree_filter: Option<&Path>) -> Outcome<Vec<LocalSession>> {
    let base = home().join(".gemini/antigravity-cli");
    let conv_dir = base.join("conversations");
    if !conv_dir.is_dir() {
        return Ok(Vec::new());
    }

    let summaries_db = base.join("conversation_summaries.db");
    if summaries_db.is_file()
        && let Ok(sessions) =
            discover_antigravity_from_summaries(&summaries_db, &conv_dir, worktree_filter).await
        && !sessions.is_empty()
    {
        return Ok(sessions);
    }

    // Fallback: scan `conversations/*.db`.
    let Some(mut entries) = read_dir(&conv_dir).await else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    while let Some(entry) = entries_next(&mut entries).await {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("db")) {
            continue;
        }
        let Some(id) = path.file_stem().and_then(OsStr::to_str).map(str::to_owned) else {
            continue;
        };
        if worktree_filter.is_some() {
            // Without summary metadata, we cannot match a worktree path reliably.
            continue;
        }
        let modified = mtime(&path).await;
        found.push(LocalSession {
            harness: HarnessKind::Antigravity,
            title: id.clone(),
            session_id: id,
            source_path: path,
            cwd: None,
            modified,
        });
    }
    Ok(found)
}

async fn discover_antigravity_from_summaries(
    summaries_db: &Path,
    conv_dir: &Path,
    worktree_filter: Option<&Path>,
) -> Result<Vec<LocalSession>, sqlx::Error> {
    let opts = SqliteConnectOptions::new()
        .filename(summaries_db)
        .read_only(true)
        .busy_timeout(Duration::from_secs(3));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;

    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT conversation_id, title, preview, workspace_uris \
         FROM conversation_summaries \
         WHERE parent_conversation_id = '' \
         ORDER BY last_modified_time DESC LIMIT 250",
    )
    .fetch_all(&pool)
    .await?;
    pool.close().await;

    let mut found = Vec::new();
    for (id, title, preview, workspace_uris) in rows {
        let db_path = conv_dir.join(format!("{id}.db"));
        if !db_path.is_file() {
            continue;
        }
        let cwd = parse_workspace_uri(&workspace_uris);
        if let Some(root) = worktree_filter {
            let Some(ref dir) = cwd else {
                continue;
            };
            if !dir.starts_with(root) {
                continue;
            }
        }
        let display_title = if !title.trim().is_empty() {
            first_line_preview(&title, 96)
        } else if !preview.trim().is_empty() {
            first_line_preview(&preview, 96)
        } else {
            id.clone()
        };
        found.push(LocalSession {
            harness: HarnessKind::Antigravity,
            session_id: id,
            source_path: db_path.clone(),
            cwd,
            modified: mtime(&db_path).await,
            title: display_title,
        });
    }
    Ok(found)
}

fn parse_workspace_uri(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first = if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(trimmed) {
        items
            .into_iter()
            .find_map(|v| v.as_str().map(str::to_owned))
    } else {
        trimmed
            .split([',', '\n'])
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_owned)
    }?;
    let path_str = first.strip_prefix("file://").unwrap_or(&first);
    let path = PathBuf::from(path_str);
    (!path_str.is_empty()).then_some(path)
}

fn is_title_prompt(text: &str) -> bool {
    const SKIP: &[&str] = &[
        "This session is being continued from a previous conversation",
        "<task-notification",
        "<local-command",
        "<command-name",
        "<command-message",
        "<command-args",
        "# AGENTS.md",
        "<environment_context",
    ];
    let trimmed = text.trim();
    !trimmed.is_empty() && !SKIP.iter().any(|prefix| trimmed.starts_with(prefix))
}

fn first_line_preview(text: &str, max_chars: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut out = String::new();
    for (idx, ch) in line.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

async fn read_dir(path: &Path) -> Option<tokio::fs::ReadDir> {
    tokio::fs::read_dir(path).await.ok()
}

async fn entries_next(dir: &mut tokio::fs::ReadDir) -> Option<tokio::fs::DirEntry> {
    dir.next_entry().await.ok().flatten()
}

async fn mtime(path: &Path) -> SystemTime {
    tokio::fs::metadata(path)
        .await
        .and_then(|meta| meta.modified())
        .unwrap_or(UNIX_EPOCH)
}

async fn head_lines(path: &Path, limit: usize) -> Outcome<Vec<String>> {
    let Ok(file) = tokio::fs::File::open(path).await else {
        return Ok(Vec::new());
    };
    let mut reader = BufReader::new(file).take(limit as u64);
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;
    let text = String::from_utf8_lossy(&buf);
    Ok(text.lines().map(str::to_owned).collect())
}

async fn sha256_file(path: &Path) -> Outcome<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|error| Failure::transport(format!("{}: {error}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

async fn git(dir: &Path, args: &[&str]) -> Outcome<std::process::Output> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|error| Failure::transport(format!("git: {error}")))?;
    if !output.status.success() {
        return Err(Failure::problem(
            Exit::Problem,
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(output)
}

async fn git_env(dir: &Path, args: &[&OsStr], index: &Path) -> Outcome<std::process::Output> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_INDEX_FILE", index)
        .output()
        .await
        .map_err(|error| Failure::transport(format!("git: {error}")))?;
    if !output.status.success() {
        return Err(Failure::problem(
            Exit::Problem,
            format!(
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(output)
}

async fn git_text(dir: &Path, args: &[&str]) -> Outcome<String> {
    let output = git(dir, args).await?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_slug_parses_https_and_ssh() {
        assert_eq!(
            repo_slug("https://github.com/May-Agent/flyco.git").as_deref(),
            Some("May-Agent/flyco")
        );
        assert_eq!(
            repo_slug("git@github.com:May-Agent/flyco.git").as_deref(),
            Some("May-Agent/flyco")
        );
        assert_eq!(
            repo_slug("ssh://git@github.com/May-Agent/flyco").as_deref(),
            Some("May-Agent/flyco")
        );
    }

    #[test]
    fn workspace_uri_handles_json_array_and_plain_uri() {
        assert_eq!(
            parse_workspace_uri(r#"["file:///Users/test/repo"]"#),
            Some(PathBuf::from("/Users/test/repo"))
        );
        assert_eq!(
            parse_workspace_uri("file:///srv/work"),
            Some(PathBuf::from("/srv/work"))
        );
    }

    #[tokio::test]
    async fn devin_single_session_extraction_isolates_only_target_session() {
        let dir =
            std::env::temp_dir().join(format!("flyco-devin-extract-test-{}", std::process::id()));
        let _ = tokio::fs::create_dir_all(&dir).await;
        let src = dir.join("sessions.db");
        let dst = dir.join("single.db");
        let _ = tokio::fs::remove_file(&src).await;
        let _ = tokio::fs::remove_file(&dst).await;

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&src)
                    .create_if_missing(true),
            )
            .await
            .expect("create src db");
        sqlx::query("CREATE TABLE refinery_schema_history(version INTEGER PRIMARY KEY, name TEXT)")
            .execute(&pool)
            .await
            .expect("ddl 1");
        sqlx::query(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, working_directory TEXT NOT NULL, title TEXT, last_activity_at INTEGER NOT NULL, hidden INTEGER NOT NULL DEFAULT 0)",
        )
        .execute(&pool)
        .await
        .expect("ddl 2");
        sqlx::query(
            "CREATE TABLE message_nodes(row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, node_id INTEGER NOT NULL, chat_message TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .expect("ddl 3");
        sqlx::query("INSERT INTO refinery_schema_history VALUES (1, 'init')")
            .execute(&pool)
            .await
            .expect("seed history");
        sqlx::query("INSERT INTO sessions VALUES ('s-keep', '/w/a', 'Keep me', 200, 0), ('s-drop', '/w/b', 'Drop me', 100, 0)")
            .execute(&pool)
            .await
            .expect("seed sessions");
        sqlx::query("INSERT INTO message_nodes(session_id, node_id, chat_message) VALUES ('s-keep', 1, 'hello'), ('s-drop', 1, 'secret')")
            .execute(&pool)
            .await
            .expect("seed msgs");
        pool.close().await;

        extract_devin_single_session(&src, "s-keep", &dst)
            .await
            .expect("extract");

        let check = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&dst).read_only(true))
            .await
            .expect("open dst");
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM sessions")
            .fetch_all(&check)
            .await
            .expect("read sessions");
        assert_eq!(ids, vec!["s-keep".to_owned()]);
        let msgs: Vec<String> = sqlx::query_scalar("SELECT chat_message FROM message_nodes")
            .fetch_all(&check)
            .await
            .expect("read msgs");
        assert_eq!(msgs, vec!["hello".to_owned()]);
        let hist: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM refinery_schema_history")
            .fetch_one(&check)
            .await
            .expect("read hist");
        assert_eq!(hist, 1);
        check.close().await;

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
