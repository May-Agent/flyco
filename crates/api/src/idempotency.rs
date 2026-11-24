//! `Idempotency-Key` deduplication for `POST /v1/sessions`.
//!
//! A session create is not naturally idempotent: retrying one whose
//! response was lost provisions a second billed machine. A client that
//! presents a key gets the create claimed under `(user_id, key)` before any
//! session row exists — the claim is the `PRIMARY KEY` insert itself, so
//! two concurrent requests under one key cannot both pass — and the claim
//! is bound to both a canonical SHA-256 digest of the request payload and the
//! session the moment one exists, so a replay of the same request reads that
//! session back while reusing the key with different parameters is refused.
//!
//! A row whose `session_id` is still `NULL` marks a create in flight:
//! another request under the same key is a conflict the caller must
//! reconcile (its session may exist any moment). Both committed and in-flight
//! rows older than [`TTL_SECONDS`] stop counting — the documented dedup
//! window — and are deleted lazily on reuse as well as by [`prune_expired`].

use std::collections::BTreeMap;

use flyco_core::{SessionId, UserId};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use skyzen::sql;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;

/// How long a presented key deduplicates, in seconds: one day.
pub const TTL_SECONDS: u64 = 24 * 60 * 60;

/// The most a caller may put in the header.
///
/// Unbounded keys would let a request write an unbounded row; the cap is
/// comfortably past anything a UUID, a ULID, or a request id generator
/// produces.
pub const MAX_KEY_CHARS: usize = 255;

/// What claiming a key found.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// Nobody else holds this key: the caller performs the create, then
    /// binds the outcome — [`FreshClaim::record`] on success,
    /// [`FreshClaim::release`] on failure.
    Fresh(FreshClaim),
    /// A create under this key already produced this session; the caller
    /// reads it back and answers with it.
    Committed(SessionId),
    /// A create under this key is in flight and has not named its session
    /// yet. The caller refuses with a conflict.
    InFlight,
    /// An active (non-expired) row under this key was recorded for a
    /// different request payload.
    PayloadMismatch,
}

/// A claim this request holds: the row exists and its `session_id` is still
/// `NULL` until [`Self::record`] writes it.
#[derive(Debug, PartialEq, Eq)]
pub struct FreshClaim {
    user: UserId,
    key: String,
}

/// Computes a deterministic SHA-256 hex digest of a serializable payload.
///
/// Object keys are recursively sorted before encoding so two semantically
/// identical JSON requests produce the same digest regardless of field order.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the payload cannot be serialized.
pub fn payload_hash<T: Serialize>(payload: &T) -> Result<String, ApiError> {
    let value = serde_json::to_value(payload)
        .map_err(|_| ApiError::CorruptRecord("create session payload could not be encoded"))?;
    let canonical = canonicalize(value);
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|_| ApiError::CorruptRecord("canonical payload could not be encoded"))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<String, serde_json::Value> = map
                .into_iter()
                .map(|(key, val)| (key, canonicalize(val)))
                .collect();
            let mut out = serde_json::Map::with_capacity(sorted.len());
            for (key, val) in sorted {
                out.insert(key, val);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize).collect())
        }
        other => other,
    }
}

/// Claims `key` for `user` bound to `request_hash`, or reports what the key
/// already names.
///
/// # Errors
///
/// Returns [`ApiError`] if the key is invalid or the database fails.
pub async fn claim(
    db: &Db,
    user: UserId,
    key: &str,
    request_hash: &str,
) -> Result<Claim, ApiError> {
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS {
        return Err(ApiError::InvalidIdempotencyKey { max: MAX_KEY_CHARS });
    }
    let now = now_unix();
    let cutoff = now.saturating_sub(TTL_SECONDS);
    loop {
        let claimed = sql!(
            db,
            "INSERT OR IGNORE INTO idempotency_keys \
             (user_id, key, request_hash, created_at_unix) \
             VALUES ({user}, {key}, {request_hash}, {now})"
        )
        .execute()
        .await?;
        if claimed.rows_written == 1 {
            return Ok(Claim::Fresh(FreshClaim {
                user,
                key: key.to_owned(),
            }));
        }

        let row: Option<HeldRow> = sql!(
            db,
            "SELECT session_id, request_hash, created_at_unix FROM idempotency_keys \
             WHERE user_id = {user} AND key = {key}"
        )
        .fetch_optional()
        .await?;
        // The row exists by definition — the `INSERT OR IGNORE` above just
        // collided with it — so an absent row means a concurrent release
        // removed an in-flight claim whose create then failed. That is a
        // key nobody holds: retry the claim.
        let Some(held) = row else { continue };

        // Both committed and in-flight entries expire after `TTL_SECONDS`:
        // once past the documented deduplication window, delete the stale row
        // and re-attempt the claim so the key can be reused cleanly.
        if held.created_at_unix < cutoff {
            sql!(
                db,
                "DELETE FROM idempotency_keys \
                 WHERE user_id = {user} AND key = {key} AND created_at_unix < {cutoff}"
            )
            .execute()
            .await?;
            continue;
        }

        if held
            .request_hash
            .as_deref()
            .is_some_and(|stored| stored != request_hash)
        {
            return Ok(Claim::PayloadMismatch);
        }

        return Ok(held.session_id.map_or(Claim::InFlight, Claim::Committed));
    }
}

/// Removes `idempotency_keys` rows older than [`TTL_SECONDS`] as of `at_unix`.
///
/// Designed to run from an hourly cron pass using the `created_at_unix` index;
/// returns the number of expired rows deleted.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn prune_expired(db: &Db, at_unix: u64) -> Result<u64, ApiError> {
    let cutoff = at_unix.saturating_sub(TTL_SECONDS);
    let deleted = sql!(
        db,
        "DELETE FROM idempotency_keys WHERE created_at_unix < {cutoff}"
    )
    .execute()
    .await?;
    Ok(deleted.rows_written)
}

impl FreshClaim {
    /// Binds the claim to the session its create produced.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] if the database fails.
    pub async fn record(self, db: &Db, session: SessionId) -> Result<(), ApiError> {
        sql!(
            db,
            "UPDATE idempotency_keys SET session_id = {session} \
             WHERE user_id = {self.user} AND key = {self.key}"
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Gives the key back because the create it guarded failed: a retry
    /// under the same key must not be locked out by a request that never
    /// produced a session.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] if the database fails.
    pub async fn release(self, db: &Db) -> Result<(), ApiError> {
        sql!(
            db,
            "DELETE FROM idempotency_keys \
             WHERE user_id = {self.user} AND key = {self.key} AND session_id IS NULL"
        )
        .execute()
        .await?;
        Ok(())
    }
}

#[derive(Debug, skyzen::FromRow)]
struct HeldRow {
    session_id: Option<SessionId>,
    request_hash: Option<String>,
    created_at_unix: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::payload_hash;

    #[test]
    fn canonical_payload_hash_is_order_independent_and_content_sensitive() {
        let first =
            payload_hash(&json!({ "b": 2, "a": { "y": true, "x": "hello" } })).expect("hash first");
        let reordered = payload_hash(&json!({ "a": { "x": "hello", "y": true }, "b": 2 }))
            .expect("hash reordered");
        let different = payload_hash(&json!({ "a": { "x": "world", "y": true }, "b": 2 }))
            .expect("hash different");

        assert_eq!(first, reordered);
        assert_eq!(first.len(), 64);
        assert_ne!(first, different);
    }
}
