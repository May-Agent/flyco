//! A session's turn history, projected in its [`SessionRoom`](crate::room::SessionRoom).
//!
//! A turn is a shape in the session's normalized event stream — a
//! `user_message`, then `turn_started`, then whatever the agent did, then
//! `turn_completed` or `turn_failed`. The room maintains a lightweight
//! `turns` projection table beside `events` (and backfills pre-existing
//! rooms incrementally on demand) so listing turns reads one row per turn
//! rather than scanning thousands of streaming deltas.
//!
//! # Why the room's stream and not the R2 transcript
//!
//! The frozen contract says the history is read from what survives a
//! machine, and what survives a machine is the control plane's copy. Two of
//! those exist. R2 holds the *harness's own* store: for Claude Code it is
//! the Agent SDK's `SessionStore` entries, opaque values whose shape is
//! Anthropic's to change, keyed by a stream name the Worker never learns;
//! Codex has no such store at all. Folding turns out of it would put
//! harness-specific interpretation in the control plane, which is exactly
//! what `flyco_core::HarnessEvent` exists to prevent — every interpretation
//! happens in flycod — and would answer nothing for half the harnesses.
//!
//! The room's `events` table holds the normalized stream instead: the same
//! events, harness-neutral, each stamped with when the control plane
//! recorded it, and it is what a browser replays. Projecting `turns`
//! directly from that stream inside the room keeps the turn list exact and
//! consistent with the conversation beside it while letting single turns
//! that span >500 stream events paginate cleanly.
//!
//! # Pagination
//!
//! The cursor is the `seq` of the last returned turn row, so every page
//! strictly advances even when a single turn produced thousands of stream
//! events. `next_cursor` is `None` once the read reaches the end of what is
//! recorded: a client following a live session watches the relay for what
//! happens next and re-lists when it wants the history again.

use flyco_core::{SessionId, TurnPage};

use crate::error::ApiError;
use crate::rooms::Rooms;

/// Most turns one page returns.
pub const MAX_TURNS_PER_PAGE: u32 = 50;

/// Reads one page of a session's turns.
///
/// # Errors
///
/// Returns [`ApiError::InvalidCursor`] if the cursor is not one this API
/// issued, or [`ApiError`] if the room could not be reached or refused the
/// read.
pub async fn page(
    rooms: &Rooms,
    session: SessionId,
    cursor: Option<&str>,
    limit: Option<u32>,
) -> Result<TurnPage, ApiError> {
    let after = match cursor {
        Some(cursor) => cursor
            .parse::<u64>()
            .map_err(|_| ApiError::InvalidCursor(cursor.to_owned()))?,
        None => 0,
    };
    let limit = limit
        .unwrap_or(MAX_TURNS_PER_PAGE)
        .clamp(1, MAX_TURNS_PER_PAGE);

    rooms.turns(session, after, limit).await
}

#[cfg(test)]
mod tests {
    use flyco_core::SessionId;

    use super::page;
    use crate::error::ApiError;
    use crate::testing::test_rooms;

    #[skyzen::test]
    async fn a_non_numeric_cursor_is_refused() {
        let error = page(&test_rooms(), SessionId::generate(), Some("not-a-seq"), None)
            .await
            .expect_err("non-numeric cursor");
        assert!(matches!(error, ApiError::InvalidCursor(_)));
    }
}
