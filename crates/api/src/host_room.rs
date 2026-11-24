//! The host room: one Durable Object per enrolled machine.
//!
//! A host's `flycod host` attaches over REST, holds one SSE stream for the
//! room's [`ControlToHost`] commands, and posts its [`HostToControl`]
//! frames back in sequenced batches. The room is the only thing in flyco
//! that can see that attachment, which makes it two things: the way
//! container work reaches the machine, and the authority on whether the
//! machine is there at all.
//!
//! # What the room is, and is not, allowed to touch
//!
//! A Durable Object cannot reach D1 or the Worker's KV — the same rule the
//! session room lives under (see [`crate::room`]). So:
//!
//! * **Liveness** lives here and nowhere else. `hosts.state` in D1 is what
//!   the control plane last *recorded*; [`HostStatus`] is what is true now,
//!   and every Worker read of a host refreshes the row from it
//!   ([`crate::hosts::refresh`]).
//! * **Job results** arrive here as a frame *and* at the Worker over REST.
//!   The frame lets the room forget the job it was holding; the REST call
//!   is what completes the machine row, because only the Worker can write
//!   D1. Two routes for one fact, exactly as a session daemon's spot
//!   notice travels twice and for the same reason.
//!
//! # The mailbox
//!
//! A host is not always attached — the machine is rebooting, the unit is
//! restarting, somebody closed the lid — and container work that arrives
//! in that window is not a notification to drop: a `Run` lost is a
//! session that never gets its container, or a container nobody ever
//! removes. So every job is a row in `host_commands` before it is
//! anything else, and it stays a row until the host answers it — the
//! answer, not the delivery, is what retires a job, so a machine that
//! dies mid-`podman run` is asked again on its next attach.
//!
//! Delivery is therefore at-least-once, which is what a container job is
//! written to survive: creating one removes any container of that name
//! first, and stopping, starting or removing a container that is already
//! in that state is what `podman` does anyway. The stream cursor gives
//! the host something better than re-executing blindly: a job row carries
//! its `seq`, so a machine can tell a job it is already running from one
//! it has never seen.

use std::time::Duration;

use flyco_core::Problem;
use flyco_core::host::HostFacts;
use flyco_provider::host::{
    ContainerJob, ControlToHost, HostAttach, HostCommand, HostFrames, HostToControl, container_name,
};
use serde::{Deserialize, Serialize};
use skyzen::durable::{DurableObject, DurableObjectError};
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::responder::sse::Event;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::sql;
use skyzen::utils::{Bytes, Json, State};
use skyzen_services::durable::{DurableDb, DurableKv};

use crate::ApiError;
use crate::clock::now_unix;
use crate::extract::Headers;
use crate::problem::Outcome;
use crate::respond::NoContent;
use crate::room::{BatchGate, HEADER_INTERNAL, INTERNAL};
use crate::schema_version::Cache;
use crate::sse::Signal;

/// Names the host a Worker→room call belongs to.
pub const HEADER_HOST: &str = "x-flyco-host";

/// How long a host's presence marker survives its last contact.
///
/// The same contract as the session room's: attach, every frames POST,
/// and the command stream's periodic refresh renew it, so expiry means a
/// machine that has said nothing for this long.
const PRESENCE_TTL_SECONDS: u64 = 30;

/// How often the command stream renews the presence marker.
const PRESENCE_REFRESH_SECONDS: u64 = 10;

/// Most queued commands one SSE step reads at a time.
const COMMAND_BATCH_LIMIT: u64 = 256;

/// Most frame-log rows written in one atomic multi-row `INSERT` statement.
///
/// Four bound parameters per row keeps each statement at 80 bindings, safely
/// inside Cloudflare Durable Object SQLite's 100-parameter limit.
const MAX_FRAME_ROWS_PER_STATEMENT: usize = 20;

/// KV key holding the facts the machine last reported.
const KEY_FACTS: &str = "host:facts";

/// What a host's room knows right now.
///
/// Answered by `/internal/status`, and the only honest source for the
/// first field: an attachment that fell silent is visible to the Durable
/// Object holding its marker and to nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HostStatus {
    /// Whether an attached host is still inside its contact deadline.
    pub connected: bool,
    /// What the machine last said about itself, if it has ever attached.
    pub facts: Option<HostFacts>,
    /// How many container jobs are still waiting to be answered.
    pub pending_jobs: u32,
}

/// What an attach answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
pub struct HostAttachResponse {
    /// Generation of this attachment; increments per attach.
    pub epoch: u64,
}

/// Query of the room's command stream.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct HostCommandCursor {
    /// The attach this stream serves.
    pub epoch: u64,
}

/// The columns the `host_commands` table stores.
#[derive(Debug, skyzen::FromRow)]
struct CommandRow {
    seq: u64,
    /// The command, as a JSON document. Untyped for the same reason a
    /// stored session event is: the room hands the host what it was
    /// given, including a variant this build does not know how to read.
    #[row(json)]
    json: serde_json::Value,
}

/// The `host_presence` row.
#[derive(Debug, skyzen::FromRow)]
struct PresenceRow {
    epoch: u64,
    live_until: u64,
}

/// The relay room for one enrolled host.
#[derive(Debug, Default, Serialize, Deserialize)]
#[skyzen::durable_object]
pub struct HostRoom {
    /// The schema version this activation already verified — a memo shared
    /// across every request on this activation, never part of the state
    /// blob.
    #[serde(skip)]
    schema: Cache,
    /// Wakes open command streams on new mailbox rows or a superseding attach.
    #[serde(skip)]
    commands: Signal,
    /// Serializes concurrent `accept_frames` batches on this activation so
    /// overlapping retries never read the same `through` cursor.
    #[serde(skip)]
    batch_gate: BatchGate,
}

impl DurableObject for HostRoom {
    /// Every durable fact lives in the object's storage; there is no
    /// blob to load and save around an event.
    const PERSIST: bool = false;

    fn fetch(&self) -> Router {
        Route::new((
            "/internal/attach".post(attach_host),
            "/internal/commands".at(stream_commands),
            "/internal/frames".post(accept_frames),
            "/internal/command".post(run_command),
            "/internal/status".at(read_status),
        ))
        .with(State(self.schema.clone()))
        .with(State(self.commands.clone()))
        .with(State(self.batch_gate.clone()))
        .build()
    }
}

// ── The host's three routes ──

/// Attaches a machine to its room.
///
/// The attach is the greeting: the facts are re-reported here because
/// they change between connections — memory is added, a disk fills,
/// Podman is upgraded — and the epoch minted here names every later
/// frame batch and command stream.
async fn attach_host(
    headers: Headers,
    Json(attach): Json<HostAttach>,
    State(cache): State<Cache>,
    State(commands): State<Signal>,
    db: DurableDb,
    kv: DurableKv,
) -> Outcome<Json<HostAttachResponse>> {
    attach_inner(&headers, &attach, &db, &kv, &cache, &commands)
        .await
        .into()
}

async fn attach_inner(
    headers: &Headers,
    attach: &HostAttach,
    db: &DurableDb,
    kv: &DurableKv,
    cache: &Cache,
    commands: &Signal,
) -> Result<Json<HostAttachResponse>, ApiError> {
    internal(headers)?;
    ensure_schema(db, cache)
        .await
        .map_err(|error| room_failed(&error))?;

    let live_until = now_unix().saturating_add(PRESENCE_TTL_SECONDS);
    sql!(
        db,
        "INSERT INTO host_presence (id, epoch, live_until) VALUES (0, 1, {live_until}) \
         ON CONFLICT (id) DO UPDATE SET \
             epoch = host_presence.epoch + 1, \
             live_until = excluded.live_until"
    )
    .execute()
    .await
    .map_err(|error| room_failed(&error))?;
    let epoch: u64 = sql!(db, "SELECT epoch FROM host_presence WHERE id = 0")
        .fetch_scalar()
        .await
        .map_err(|error| room_failed(&error))?;
    sql!(db, "DELETE FROM host_frames WHERE epoch != {epoch}")
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;
    sql!(db, "DELETE FROM host_frame_log WHERE epoch != {epoch}")
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;

    put_facts(kv, &attach.facts)
        .await
        .map_err(|error| room_failed(&error))?;
    commands.notify();
    tracing::info!(hostname = %attach.facts.hostname, epoch, "a host attached to its room");
    Ok(Json(HostAttachResponse { epoch }))
}

/// Holds a machine's command stream open: a cursor over `host_commands`,
/// woken by in-memory signals when commands land or the attach is
/// superseded.
async fn stream_commands(
    headers: Headers,
    Query(cursor): Query<HostCommandCursor>,
    State(cache): State<Cache>,
    State(commands): State<Signal>,
    db: DurableDb,
) -> Outcome<Sse> {
    open_command_stream(&headers, cursor.epoch, db, &cache, commands)
        .await
        .into()
}

async fn open_command_stream(
    headers: &Headers,
    epoch: u64,
    db: DurableDb,
    cache: &Cache,
    signal: Signal,
) -> Result<Sse, ApiError> {
    internal(headers)?;
    ensure_schema(&db, cache)
        .await
        .map_err(|error| room_failed(&error))?;
    let presence = read_presence(&db)
        .await
        .map_err(|error| room_failed(&error))?;
    let Some(presence) = presence else {
        return Err(ApiError::Room(
            "a command stream was opened before any host attached".to_owned(),
        ));
    };
    if presence.epoch != epoch {
        return Err(ApiError::RelayEpochStale {
            current: presence.epoch,
            opened: epoch,
        });
    }
    Ok(crate::sse::serve(
        CommandFeed {
            db,
            signal,
            epoch,
            cursor: 0,
            superseded_announced: false,
            last_touch: 0,
        },
        poll_command_feed,
        crate::sse::HEARTBEAT,
    ))
}

/// The command stream's working state.
struct CommandFeed {
    db: DurableDb,
    /// Shared wakeup signal notified on new commands or a superseding attach.
    signal: Signal,
    /// The attach this stream serves; a newer one ends it.
    epoch: u64,
    /// How far down `host_commands` this stream has handed over.
    ///
    /// Starts at zero on every stream: a job is retired by its answer, not
    /// by having been delivered, so a fresh stream replays whatever the
    /// host has not yet answered — the mailbox semantic the room owes.
    cursor: u64,
    /// Whether the supersession notice has already been emitted — the next
    /// step ends the body.
    superseded_announced: bool,
    /// When presence was last renewed, seconds.
    last_touch: u64,
}

/// One step of the host's command stream.
fn poll_command_feed(feed: &mut CommandFeed) -> crate::sse::PollFn<'_> {
    Box::pin(command_feed_step(feed))
}

/// Helper to park a feed briefly after a transient storage failure.
fn retry_wait(signal: &Signal, seen: u64) -> crate::sse::Poll {
    crate::sse::Poll::Wait {
        signal: Some((signal.clone(), seen)),
        after: Some(crate::sse::RETRY_WAIT),
    }
}

/// Checks supersession and renews host presence when due.
///
/// Returns `Some(poll)` when the step should immediately emit or end/wait,
/// or `None` to proceed to reading `host_commands`.
async fn step_host_presence(
    feed: &mut CommandFeed,
    seen: u64,
    now: u64,
) -> Option<crate::sse::Poll> {
    let db = &feed.db;
    let presence = match read_presence(db).await {
        Ok(presence) => presence,
        Err(error) => {
            tracing::warn!(%error, "a host command stream could not read presence");
            return Some(retry_wait(&feed.signal, seen));
        }
    };
    if let Some(row) = presence
        && row.epoch != feed.epoch
    {
        if feed.superseded_announced {
            return Some(crate::sse::Poll::End);
        }
        feed.superseded_announced = true;
        let envelope = match serde_json::to_string(&HostCommand {
            seq: None,
            command: ControlToHost::Superseded,
        }) {
            Ok(envelope) => envelope,
            Err(error) => {
                tracing::warn!(%error, "a host command stream could not encode its supersession");
                return Some(crate::sse::Poll::End);
            }
        };
        return Some(crate::sse::Poll::Emit(vec![
            Event::data(envelope).event("command"),
        ]));
    }
    if now.saturating_sub(feed.last_touch) >= PRESENCE_REFRESH_SECONDS {
        let live_until = now.saturating_add(PRESENCE_TTL_SECONDS);
        let epoch = feed.epoch;
        if let Err(error) = sql!(
            db,
            "UPDATE host_presence SET live_until = {live_until} \
             WHERE id = 0 AND epoch = {epoch}"
        )
        .execute()
        .await
        {
            tracing::warn!(%error, "a host command stream could not renew presence");
            return Some(retry_wait(&feed.signal, seen));
        }
        feed.last_touch = now;
    }
    None
}

/// One step of the host's command stream: checks the row budget before
/// reading `host_commands`, debits rows returned, and closes cleanly on
/// [`ApiError::RowBudgetExceeded`].
async fn command_feed_step(feed: &mut CommandFeed) -> crate::sse::Poll {
    let seen = feed.signal.version();
    let now = now_unix();
    if let Some(poll) = step_host_presence(feed, seen, now).await {
        return poll;
    }

    let db = &feed.db;
    if let Err(error) = crate::row_budget::check_budget(db).await {
        if matches!(error, ApiError::RowBudgetExceeded) {
            tracing::warn!("a host command stream closed: row budget exhausted");
            return crate::sse::Poll::End;
        }
        tracing::warn!(%error, "a host command stream could not check the row budget");
        return retry_wait(&feed.signal, seen);
    }

    let cursor = feed.cursor;
    let limit = COMMAND_BATCH_LIMIT;
    let rows: Vec<CommandRow> = match sql!(
        db,
        "SELECT seq, json FROM host_commands WHERE seq > {cursor} ORDER BY seq LIMIT {limit}"
    )
    .fetch_all()
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "a host command stream could not poll its commands");
            return retry_wait(&feed.signal, seen);
        }
    };
    if rows.is_empty() {
        let refresh_in = PRESENCE_REFRESH_SECONDS
            .saturating_sub(now.saturating_sub(feed.last_touch))
            .max(1);
        return crate::sse::Poll::Wait {
            signal: Some((feed.signal.clone(), seen)),
            after: Some(Duration::from_secs(refresh_in)),
        };
    }
    if let Err(error) = crate::row_budget::debit_reads(db, rows.len() as u64).await {
        tracing::warn!(%error, "a host command stream could not debit the row budget");
        return retry_wait(&feed.signal, seen);
    }
    feed.cursor = rows.last().map_or(feed.cursor, |row| row.seq);
    crate::sse::Poll::Emit(
        rows.iter()
            .map(|row| {
                let envelope = serde_json::json!({
                    "seq": row.seq,
                    "command": row.json,
                });
                Event::data(envelope.to_string())
                    .event("command")
                    .id(row.seq.to_string())
            })
            .collect(),
    )
}

/// Accepts one batch of a machine's outbound frames.
///
/// The same ordering contract the session room enforces: a batch resumes
/// where the last stored one ended, a retransmission is answered without
/// reapplying, a gap is refused.
async fn accept_frames(
    headers: Headers,
    Json(batch): Json<HostFrames>,
    State(cache): State<Cache>,
    State(batch_gate): State<BatchGate>,
    db: DurableDb,
) -> Outcome<NoContent> {
    accept_batch(&headers, batch, &db, &cache, &batch_gate)
        .await
        .into()
}

async fn accept_batch(
    headers: &Headers,
    batch: HostFrames,
    db: &DurableDb,
    cache: &Cache,
    batch_gate: &BatchGate,
) -> Result<NoContent, ApiError> {
    internal(headers)?;
    let _guard = batch_gate.lock().await;
    ensure_schema(db, cache)
        .await
        .map_err(|error| room_failed(&error))?;
    let Some(presence) = read_presence(db)
        .await
        .map_err(|error| room_failed(&error))?
    else {
        return Err(ApiError::Room(
            "a host posted frames before it attached".to_owned(),
        ));
    };
    if presence.epoch != batch.epoch {
        return Err(ApiError::RelayEpochStale {
            current: presence.epoch,
            opened: batch.epoch,
        });
    }

    let epoch = batch.epoch;
    let through: u64 = sql!(db, "SELECT through FROM host_frames WHERE epoch = {epoch}")
        .fetch_scalar_optional()
        .await
        .map_err(|error| room_failed(&error))?
        .unwrap_or(0);
    let next = through.saturating_add(1);
    if batch.from_seq > next {
        return Err(ApiError::RelayFramesGap {
            next,
            got: batch.from_seq,
        });
    }
    let skip = usize::try_from(next.saturating_sub(batch.from_seq)).unwrap_or(usize::MAX);
    let fresh = batch.frames.get(skip..).unwrap_or(&[]);
    let first_fresh_seq = batch
        .from_seq
        .saturating_add(u64::try_from(skip).unwrap_or(0));

    // Apply frames and advance `host_frames.through` atomically in chunks via
    // `host_frame_log`'s `AFTER INSERT` trigger. A failure rolls back the
    // whole SQLite statement so `host_commands` and `host_frames.through`
    // never drift apart.
    for (chunk_idx, chunk) in fresh.chunks(MAX_FRAME_ROWS_PER_STATEMENT).enumerate() {
        let base_seq = first_fresh_seq.saturating_add(
            u64::try_from(chunk_idx * MAX_FRAME_ROWS_PER_STATEMENT).unwrap_or(0),
        );
        let placeholders = vec!["(?, ?, ?, ?)"; chunk.len()].join(", ");
        let statement = format!(
            "INSERT OR IGNORE INTO host_frame_log (epoch, frame_seq, machine, command_seq) \
             VALUES {placeholders}"
        );
        let mut query = db.query(&statement);
        for (offset, frame) in chunk.iter().enumerate() {
            let frame_seq = base_seq.saturating_add(u64::try_from(offset).unwrap_or(0));
            match frame {
                HostToControl::JobResult {
                    job_id,
                    outcome,
                    command_seq,
                } => {
                    tracing::info!(
                        machine = %job_id,
                        command_seq,
                        outcome = ?core::mem::discriminant(outcome),
                        "a host answered a container job"
                    );
                    let container = container_name(*job_id);
                    query = query
                        .bind(epoch)
                        .bind(frame_seq)
                        .bind(container)
                        .bind(*command_seq);
                }
            }
        }
        query
            .execute()
            .await
            .map_err(|error| room_failed(&error))?;
    }

    // Ephemeral rows — a `Revoked` — are retired by acknowledgement. Job
    // rows are not: a job is done when its answer arrives, not when it was
    // handed over.
    let ack_through = batch.ack_through;
    if ack_through > 0 {
        sql!(
            db,
            "DELETE FROM host_commands WHERE seq <= {ack_through} AND machine IS NULL"
        )
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;
    }

    let live_until = now_unix().saturating_add(PRESENCE_TTL_SECONDS);
    sql!(
        db,
        "UPDATE host_presence SET live_until = {live_until} \
         WHERE id = 0 AND epoch = {epoch}"
    )
    .execute()
    .await
    .map_err(|error| room_failed(&error))?;
    Ok(NoContent)
}

// ── The mailbox ──

/// Records a job before anybody tries to deliver it.
///
/// Written first and deleted on the answer, so a host that took a job and
/// died is asked again rather than losing it — and so a job planned while
/// the machine is away is waiting when it comes back. A command that is
/// not container work takes the same table with no machine: it is
/// delivered by the same cursor and retired by acknowledgement.
async fn hold(
    db: &DurableDb,
    container: Option<&str>,
    command: &ControlToHost,
    cache: &Cache,
    commands: &Signal,
) -> Result<(), DurableObjectError> {
    let json = serde_json::to_string(command)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    let now = now_unix();
    ensure_schema(db, cache).await?;
    match container {
        Some(container) => sql!(
            db,
            "INSERT INTO host_commands (machine, json, at_unix) \
             VALUES ({container}, {json}, {now})"
        )
        .execute()
        .await
        .map_err(|error| stored(&error)),
        None => sql!(
            db,
            "INSERT INTO host_commands (machine, json, at_unix) \
             VALUES (NULL, {json}, {now})"
        )
        .execute()
        .await
        .map_err(|error| stored(&error)),
    }?;
    commands.notify();
    Ok(())
}

/// Drops outstanding container jobs when a host is revoked.
///
/// When `keep_stops` is true (a live host is currently streaming commands),
/// `ContainerJob::Stop` rows are preserved so the host drains running
/// containers before acting on the `Revoked` command queued right after them,
/// while pending `Create`/`Start`/`Remove` rows are cancelled. When the host
/// is offline (`keep_stops == false`), its token is already revoked and it
/// can never attach again, so every pending job row is cleared.
async fn forget_jobs(
    db: &DurableDb,
    cache: &Cache,
    keep_stops: bool,
) -> Result<(), DurableObjectError> {
    ensure_schema(db, cache).await?;
    if !keep_stops {
        sql!(db, "DELETE FROM host_commands WHERE machine IS NOT NULL")
            .execute()
            .await
            .map_err(|error| stored(&error))?;
        return Ok(());
    }

    let rows: Vec<CommandRow> = sql!(
        db,
        "SELECT seq, json FROM host_commands WHERE machine IS NOT NULL"
    )
    .fetch_all()
    .await
    .map_err(|error| stored(&error))?;
    for row in rows {
        let is_stop = matches!(
            serde_json::from_value::<ControlToHost>(row.json),
            Ok(ControlToHost::Run {
                job: ContainerJob::Stop { .. }
            })
        );
        if !is_stop {
            sql!(db, "DELETE FROM host_commands WHERE seq = {row.seq}")
                .execute()
                .await
                .map_err(|error| stored(&error))?;
        }
    }
    Ok(())
}

async fn pending_jobs(db: &DurableDb, cache: &Cache) -> Result<u32, DurableObjectError> {
    ensure_schema(db, cache).await?;
    let count: u64 = sql!(
        db,
        "SELECT COUNT(*) AS pending FROM host_commands WHERE machine IS NOT NULL"
    )
    .fetch_scalar()
    .await
    .map_err(|error| stored(&error))?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// The schema this build expects.
///
/// The durable answer to "is the schema already there" lives in the
/// `schema_meta` table [`crate::schema_version`] keeps, and the object
/// itself remembers having read it: checking an answer the object already
/// holds costs nothing at all. Bump it when the DDL below changes so a
/// room built by an older build upgrades once, on its next call.
const SCHEMA_VERSION: i64 = 3;

/// Creates the mailbox if this is the room's first write.
///
/// `AUTOINCREMENT` rather than a counter in the object: the order jobs
/// were planned in has to survive the object being rebuilt around every
/// event, and the database is the only thing here that guarantees it.
async fn ensure_schema(db: &DurableDb, cache: &Cache) -> Result<(), DurableObjectError> {
    crate::schema_version::ensure(
        db,
        cache,
        SCHEMA_VERSION,
        &[
            // Every command owed to the host. `machine` names the container a
            // job acts on — the answer deletes by `seq` (or oldest for that
            // container when a legacy daemon omits `command_seq`) — and is
            // NULL for non-job commands, which acknowledgement deletes instead.
            "CREATE TABLE IF NOT EXISTS host_commands (\
             seq     INTEGER PRIMARY KEY AUTOINCREMENT, \
             machine TEXT, \
             json    TEXT    NOT NULL, \
             at_unix INTEGER NOT NULL)",
            // One row, because a room has exactly one host. The epoch names
            // the current attach; `live_until` is the deadline its contact
            // renews.
            "CREATE TABLE IF NOT EXISTS host_presence (\
             id         INTEGER PRIMARY KEY CHECK (id = 0), \
             epoch      INTEGER NOT NULL, \
             live_until INTEGER NOT NULL)",
            // One row per attach, recording how far into that epoch's frame
            // numbering the room has stored.
            "CREATE TABLE IF NOT EXISTS host_frames (\
             epoch   INTEGER PRIMARY KEY, \
             through INTEGER NOT NULL)",
            // Per-frame receipt log for atomic, idempotent batch application.
            // Inserting rows here fires `host_frame_log_apply` in the same
            // implicit SQLite transaction, retiring each answered job and
            // advancing `host_frames.through` atomically without BEGIN/COMMIT.
            "CREATE TABLE IF NOT EXISTS host_frame_log (\
             epoch       INTEGER NOT NULL, \
             frame_seq   INTEGER NOT NULL, \
             machine     TEXT    NOT NULL, \
             command_seq INTEGER, \
             PRIMARY KEY (epoch, frame_seq))",
            "CREATE TRIGGER IF NOT EXISTS host_frame_log_apply \
             AFTER INSERT ON host_frame_log \
             BEGIN \
                 DELETE FROM host_commands \
                 WHERE (NEW.command_seq IS NOT NULL AND seq = NEW.command_seq AND machine = NEW.machine) \
                    OR (NEW.command_seq IS NULL AND seq = (\
                        SELECT MIN(seq) FROM host_commands WHERE machine = NEW.machine)); \
                 INSERT INTO host_frames (epoch, through) VALUES (NEW.epoch, NEW.frame_seq) \
                 ON CONFLICT (epoch) DO UPDATE SET \
                     through = max(excluded.through, host_frames.through); \
             END",
            // The room's row-read ledger — one row per UTC day, debited by
            // `row_budget::charge_reads`, and the circuit breaker that keeps a
            // runaway reader here from spending the account's quota.
            crate::row_budget::SCHEMA,
        ],
    )
    .await
}

/// The room's own database failed, which is a runtime fault rather than
/// anything a caller did.
fn stored(error: &skyzen_services::DurableDbError) -> DurableObjectError {
    DurableObjectError::Runtime(error.to_string())
}

/// Reads the presence row, if an attach has ever written one.
async fn read_presence(db: &DurableDb) -> Result<Option<PresenceRow>, DurableObjectError> {
    sql!(
        db,
        "SELECT epoch, live_until FROM host_presence WHERE id = 0"
    )
    .fetch_optional()
    .await
    .map_err(|error| stored(&error))
}

/// Whether the host's presence marker is still inside its deadline.
async fn host_live(db: &DurableDb) -> Result<bool, DurableObjectError> {
    let Some(presence) = read_presence(db).await? else {
        return Ok(false);
    };
    Ok(presence.live_until >= now_unix())
}

async fn put_facts(kv: &DurableKv, facts: &HostFacts) -> Result<(), DurableObjectError> {
    kv.put_json(KEY_FACTS, facts)
        .await
        .map_err(|error| DurableObjectError::Runtime(error.to_string()))
}

async fn read_facts(kv: &DurableKv) -> Result<Option<HostFacts>, ApiError> {
    kv.get_json::<HostFacts>(KEY_FACTS)
        .await
        .map_err(|error| ApiError::Room(error.to_string()))
}

// ── The room's own HTTP surface ──

/// Reads the internal header a Worker→room call must carry.
fn internal(headers: &Headers) -> Result<(), ApiError> {
    if headers.get(HEADER_INTERNAL) == Some(INTERNAL) {
        Ok(())
    } else {
        Err(ApiError::Room(
            "a host room route was reached without the internal marker".to_owned(),
        ))
    }
}

/// Sends one command to the host, holding the container work it cannot
/// take right now.
async fn run_command(
    headers: Headers,
    body: Bytes,
    State(cache): State<Cache>,
    State(commands): State<Signal>,
    db: DurableDb,
) -> Outcome<NoContent> {
    dispatch(&headers, &body, &db, &cache, &commands)
        .await
        .into()
}

/// Reads the command out of the body itself.
///
/// A [`ControlToHost`] carries a whole
/// [`ContainerJob`](flyco_provider::host::ContainerJob), which carries a
/// daemon bootstrap — three credentials, a repository and a commit
/// identity — and none of that is an API schema anybody publishes. The
/// room's internal routes are spoken only by this Worker, so the body is
/// decoded here rather than dragged through `OpenAPI`.
async fn dispatch(
    headers: &Headers,
    body: &[u8],
    db: &DurableDb,
    cache: &Cache,
    commands: &Signal,
) -> Result<NoContent, ApiError> {
    let command: ControlToHost = serde_json::from_slice(body)
        .map_err(|error| ApiError::Room(format!("a host command did not decode: {error}")))?;
    dispatch_command(headers, &command, db, cache, commands).await
}

async fn dispatch_command(
    headers: &Headers,
    command: &ControlToHost,
    db: &DurableDb,
    cache: &Cache,
    commands: &Signal,
) -> Result<NoContent, ApiError> {
    internal(headers)?;
    ensure_schema(db, cache)
        .await
        .map_err(|error| room_failed(&error))?;

    // Written before it can be delivered, so a host that takes a job and
    // dies is asked again rather than losing it — and so a job planned
    // while the machine is away is waiting when it comes back.
    let live = host_live(db).await.map_err(|error| room_failed(&error))?;
    match command {
        ControlToHost::Run { job } => {
            hold(db, Some(job.container()), command, cache, commands)
                .await
                .map_err(|error| room_failed(&error))?;
        }
        // Composed by the room itself as it ends a superseded stream —
        // the one command nobody may send it.
        ControlToHost::Superseded => {
            return Err(ApiError::RoomRefused(Box::new(Problem::of_type(
                "superseded-is-room-composed",
                400,
                "Bad Request",
                "superseded is the room's own last word on a superseded stream, not a command it accepts",
            ))));
        }
        ControlToHost::Revoked if live => {
            // Cancel non-Stop jobs first, preserving any `ContainerJob::Stop`
            // drain rows, then enqueue `Revoked` strictly after them in `seq`
            // order so the live host drains stops before exiting.
            forget_jobs(db, cache, true)
                .await
                .map_err(|error| room_failed(&error))?;
            hold(db, None, command, cache, commands)
                .await
                .map_err(|error| room_failed(&error))?;
        }
        ControlToHost::Revoked => {
            forget_jobs(db, cache, false)
                .await
                .map_err(|error| room_failed(&error))?;
            tracing::warn!(
                ?command,
                "dropped a command: this host has no live attachment"
            );
        }
    }
    Ok(NoContent)
}

/// Serves what the room knows about its machine.
async fn read_status(
    headers: Headers,
    kv: DurableKv,
    State(cache): State<Cache>,
    db: DurableDb,
) -> Outcome<Json<HostStatus>> {
    status(&headers, &kv, &db, &cache).await.into()
}

async fn status(
    headers: &Headers,
    kv: &DurableKv,
    db: &DurableDb,
    cache: &Cache,
) -> Result<Json<HostStatus>, ApiError> {
    internal(headers)?;
    ensure_schema(db, cache)
        .await
        .map_err(|error| room_failed(&error))?;
    Ok(Json(HostStatus {
        connected: host_live(db).await.map_err(|error| room_failed(&error))?,
        facts: read_facts(kv).await?,
        pending_jobs: pending_jobs(db, cache)
            .await
            .map_err(|error| room_failed(&error))?,
    }))
}

fn room_failed(error: &impl core::fmt::Display) -> ApiError {
    ApiError::Room(error.to_string())
}
