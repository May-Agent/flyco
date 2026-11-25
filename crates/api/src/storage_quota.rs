//! Per-user R2 storage accounting, hard quota enforcement, and runtime operator
//! settings.
//!
//! Every user-owned R2 object (`transcripts/`, `handoffs/`, `workdirs/`,
//! `skills/`) is indexed in `user_storage_objects` by its exact R2 object key.
//! Checking a write costs one indexed D1 read (never an R2 list), overwrites
//! account for the exact byte delta, and deletes reclaim space immediately.
//!
//! Default and per-user quotas are stored in D1 (`operator_settings` and
//! `users.storage_quota_bytes`) so operators adjust them at runtime without
//! code changes or redeploys.

use flyco_core::{CurrentUser, SessionId, StorageUsageBreakdown, StorageUsageView, UserId};
use skyzen::routing::{CreateRouteNode as _, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::problem::Outcome;

/// Runtime operator-setting key for the deployment-wide default per-user
/// storage quota in bytes.
pub const DEFAULT_STORAGE_QUOTA_KEY: &str = "default_storage_quota_bytes";

/// Which user-owned R2 object class an entry belongs to.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::Column,
)]
#[serde(rename_all = "snake_case")]
pub enum StorageCategory {
    /// Session transcript batches (`transcripts/`).
    Transcript,
    /// Handoff / native local-resume transcript payloads (`handoffs/`).
    Handoff,
    /// Uncommitted checkout patches (`workdirs/`).
    WorkdirPatch,
    /// Uploaded skill zip bundles (`skills/`).
    Skill,
}

impl StorageCategory {
    /// SQLite column value stored in `user_storage_objects.category`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::Handoff => "handoff",
            Self::WorkdirPatch => "workdir_patch",
            Self::Skill => "skill",
        }
    }
}

#[derive(Debug, skyzen::FromRow)]
struct CategorySumRow {
    category: String,
    total_bytes: i64,
}

#[derive(Debug, skyzen::FromRow)]
struct WriteCheckRow {
    used_bytes: i64,
    existing_bytes: i64,
    user_quota: Option<i64>,
    default_quota: Option<String>,
}

/// Resolves the effective storage quota for `user`: the user's explicit
/// override when set, otherwise the runtime `operator_settings` default.
///
/// Costs 1 D1 read.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the runtime default setting is
/// absent or malformed, or a database error otherwise.
pub async fn effective_quota_bytes(db: &Db, user: UserId) -> Result<u64, ApiError> {
    #[derive(Debug, skyzen::FromRow)]
    struct QuotaRow {
        user_quota: Option<i64>,
        default_quota: Option<String>,
    }

    let key = DEFAULT_STORAGE_QUOTA_KEY;
    let row: Option<QuotaRow> = sql!(
        db,
        "SELECT u.storage_quota_bytes AS user_quota, \
         (SELECT value FROM operator_settings WHERE key = {key}) AS default_quota \
         FROM users u WHERE u.id = {user}"
    )
    .fetch_optional()
    .await?;

    let row = row.ok_or(ApiError::CorruptRecord(
        "storage quota queried for a missing user",
    ))?;
    parse_effective_quota(row.user_quota, row.default_quota.as_deref())
}

fn parse_effective_quota(
    user_quota: Option<i64>,
    default_quota: Option<&str>,
) -> Result<u64, ApiError> {
    if let Some(override_bytes) = user_quota {
        return u64::try_from(override_bytes).map_err(|_| {
            ApiError::CorruptRecord("users.storage_quota_bytes must be non-negative")
        });
    }
    let raw = default_quota.ok_or(ApiError::CorruptRecord(
        "operator_settings is missing default_storage_quota_bytes",
    ))?;
    raw.parse::<u64>().map_err(|_| {
        ApiError::CorruptRecord(
            "operator_settings.default_storage_quota_bytes is not a valid byte count",
        )
    })
}

/// Updates the deployment-wide default per-user storage quota at runtime.
///
/// Operators are trusted: any non-negative byte count is accepted with no
/// artificial ceiling.
///
/// # Errors
///
/// Returns [`ApiError`] if the database write fails.
pub async fn set_default_quota_bytes(db: &Db, bytes: u64) -> Result<(), ApiError> {
    let key = DEFAULT_STORAGE_QUOTA_KEY;
    let value = bytes.to_string();
    let now = now_unix();
    sql!(
        db,
        "INSERT INTO operator_settings (key, value, updated_at_unix) \
         VALUES ({key}, {value}, {now}) \
         ON CONFLICT (key) DO UPDATE SET \
         value = excluded.value, updated_at_unix = excluded.updated_at_unix"
    )
    .execute()
    .await?;
    tracing::info!(bytes, "updated runtime default user storage quota");
    Ok(())
}

/// Sets or clears one user's explicit storage quota override at runtime.
///
/// Passing `None` restores inheritance from `default_storage_quota_bytes`.
///
/// # Errors
///
/// Returns [`ApiError`] if `bytes` exceeds `i64::MAX` or the database write
/// fails.
pub async fn set_user_quota_bytes(
    db: &Db,
    user: UserId,
    bytes: Option<u64>,
) -> Result<(), ApiError> {
    let stored = bytes
        .map(|value| {
            i64::try_from(value)
                .map_err(|_| ApiError::CorruptRecord("storage quota exceeds SQLite integer range"))
        })
        .transpose()?;
    sql!(
        db,
        "UPDATE users SET storage_quota_bytes = {stored} WHERE id = {user}"
    )
    .execute()
    .await?;
    tracing::info!(%user, quota_bytes = ?bytes, "updated per-user storage quota override");
    Ok(())
}

/// Verifies that writing `incoming_bytes` to `object_key` for `user` stays
/// within their effective storage quota.
///
/// Overwriting an existing `object_key` subtracts its currently accounted size
/// before checking the new total. Costs 1 D1 read and 0 R2 calls.
///
/// # Errors
///
/// Returns [`ApiError::StorageQuotaExceeded`] when the write would exceed the
/// user's effective quota, or [`ApiError`] on database failure.
pub async fn check_write(
    db: &Db,
    user: UserId,
    object_key: &str,
    incoming_bytes: u64,
) -> Result<(), ApiError> {
    let setting_key = DEFAULT_STORAGE_QUOTA_KEY;
    let row: Option<WriteCheckRow> = sql!(
        db,
        "SELECT \
         COALESCE((SELECT SUM(size_bytes) FROM user_storage_objects WHERE user_id = {user}), 0) AS used_bytes, \
         COALESCE((SELECT size_bytes FROM user_storage_objects WHERE object_key = {object_key} AND user_id = {user}), 0) AS existing_bytes, \
         u.storage_quota_bytes AS user_quota, \
         (SELECT value FROM operator_settings WHERE key = {setting_key}) AS default_quota \
         FROM users u WHERE u.id = {user}"
    )
    .fetch_optional()
    .await?;

    let row = row.ok_or(ApiError::CorruptRecord(
        "storage write checked for a missing user",
    ))?;
    let quota_bytes = parse_effective_quota(row.user_quota, row.default_quota.as_deref())?;
    let used_bytes = u64::try_from(row.used_bytes.max(0)).unwrap_or(0);
    let existing_bytes = u64::try_from(row.existing_bytes.max(0)).unwrap_or(0);
    let projected = used_bytes
        .saturating_sub(existing_bytes)
        .saturating_add(incoming_bytes);
    if projected > quota_bytes {
        return Err(ApiError::StorageQuotaExceeded {
            used_bytes,
            quota_bytes,
            incoming_bytes,
        });
    }
    Ok(())
}

/// Records (or updates) one stored R2 object in the user's ledger.
///
/// Called immediately after `storage.put` succeeds so a failed upload never
/// charges the user. Costs 1 D1 write.
///
/// # Errors
///
/// Returns [`ApiError`] if the database write fails.
pub async fn record_write(
    db: &Db,
    user: UserId,
    session: Option<SessionId>,
    category: StorageCategory,
    object_key: &str,
    size_bytes: u64,
) -> Result<(), ApiError> {
    let stored_size = i64::try_from(size_bytes).unwrap_or(i64::MAX);
    let category_str = category.as_str();
    let now = now_unix();
    sql!(
        db,
        "INSERT INTO user_storage_objects \
         (object_key, user_id, session_id, category, size_bytes, updated_at_unix) \
         VALUES ({object_key}, {user}, {session}, {category_str}, {stored_size}, {now}) \
         ON CONFLICT (object_key) DO UPDATE SET \
         size_bytes = excluded.size_bytes, updated_at_unix = excluded.updated_at_unix"
    )
    .execute()
    .await?;
    Ok(())
}

/// Removes one R2 object from the user's ledger, reclaiming its quota
/// immediately. Costs 1 D1 write.
///
/// # Errors
///
/// Returns [`ApiError`] if the database write fails.
pub async fn record_delete(db: &Db, object_key: &str) -> Result<(), ApiError> {
    sql!(
        db,
        "DELETE FROM user_storage_objects WHERE object_key = {object_key}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Computes `user`'s current R2 storage usage, per-category breakdown, and
/// effective quota.
///
/// Costs 2 D1 reads, 0 D1 writes, 0 R2 calls.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or the runtime quota setting is
/// corrupt.
pub async fn usage(db: &Db, user: UserId) -> Result<StorageUsageView, ApiError> {
    let quota_bytes = effective_quota_bytes(db, user).await?;
    let rows: Vec<CategorySumRow> = sql!(
        db,
        "SELECT category, COALESCE(SUM(size_bytes), 0) AS total_bytes \
         FROM user_storage_objects WHERE user_id = {user} GROUP BY category"
    )
    .fetch_all()
    .await?;

    let mut breakdown = StorageUsageBreakdown::default();
    for row in rows {
        let bytes = u64::try_from(row.total_bytes.max(0)).unwrap_or(0);
        match row.category.as_str() {
            "transcript" => breakdown.transcripts_bytes = bytes,
            "handoff" => breakdown.handoffs_bytes = bytes,
            "workdir_patch" => breakdown.workdir_patches_bytes = bytes,
            "skill" => breakdown.skills_bytes = bytes,
            _ => {}
        }
    }

    Ok(StorageUsageView {
        used_bytes: breakdown.total_bytes(),
        quota_bytes,
        breakdown,
    })
}

/// `GET /v1/usage/storage` — the caller's current R2 storage usage and quota.
///
/// Costs 2 D1 reads (quota lookup + grouped category sum), 0 writes, 0 KV,
/// 0 Durable Object calls, 0 R2 calls, 0 subrequests.
#[skyzen::openapi]
async fn get_storage_usage(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<StorageUsageView>> {
    usage(&db, user.id).await.map(Json).into()
}

/// Authenticated routes owned by this module.
pub fn routes() -> Vec<RouteNode> {
    Route::new(("/v1/usage/storage".at(get_storage_usage),)).into_route_nodes()
}
