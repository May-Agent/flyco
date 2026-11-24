//! The daemon and host relay: REST in, SSE out.
//!
//! There is no socket to join any more. A daemon or an enrolled machine
//! holds three routes against its room — attach, a command stream, and a
//! frame POST — and this module is the Worker half of each: forward an
//! already middleware-authenticated caller into the Durable Object that owns
//! the session or the machine.
//!
//! # Authentication
//!
//! Both peers are native HTTP clients, so both present an ordinary bearer
//! credential: a session daemon its `fd_` token checked by
//! [`crate::middleware::RequireDaemon`], an enrolled machine its `fh_` token
//! checked by [`crate::middleware::RequireHost`]. Each is bound to the id in
//! the path before the handler runs, so these functions take the verified id
//! directly without repeating the D1 credential lookup.
//!
//! # What lives on the durable side
//!
//! A session goes live when its daemon attaches, not when a provider's
//! API returned a machine — and a Durable Object cannot reach D1, so
//! the durable half of that happens here, on the authenticated hop that
//! carries the daemon into the room. The same goes for a host: it is
//! online when its room can see the attachment, and [`hosts::arrived`]
//! is what writes that down.

use flyco_core::wire::{DaemonAttach, DaemonAttached, DaemonFrames};
use flyco_core::{HostId, SessionId};
use flyco_provider::host::{HostAttach, HostFrames};
use skyzen::Response;
use skyzen_services::Db;

use crate::error::ApiError;
use crate::host_room::HostAttachResponse;
use crate::respond::NoContent;
use crate::rooms::{HostRooms, Rooms};
use crate::{hosts, sessions};

/// Attaches a session's daemon to its room.
///
/// # Errors
///
/// Returns [`ApiError::ProtocolMismatch`] if the daemon speaks another wire
/// protocol, or [`ApiError::Room`] if the room could not be reached.
pub async fn attach_daemon(
    rooms: &Rooms,
    db: &Db,
    session: SessionId,
    attach: DaemonAttach,
) -> Result<DaemonAttached, ApiError> {
    if attach.protocol_version != flyco_core::WIRE_PROTOCOL_VERSION {
        return Err(ApiError::ProtocolMismatch {
            daemon: attach.protocol_version,
            control: flyco_core::WIRE_PROTOCOL_VERSION,
        });
    }

    // A session goes live when its daemon greets the control plane, not
    // when a provider's API returned a machine: a machine that exists is
    // not an agent that is ready.
    sessions::daemon_arrived(db, rooms, session).await?;

    let attached = rooms.daemon_attach(db, session, &attach).await?;
    tracing::info!(%session, epoch = attached.epoch, "a daemon attached to its room");
    Ok(attached)
}

/// Opens a session daemon's command stream.
///
/// The answer is the room's SSE stream, handed through still running.
///
/// # Errors
///
/// Returns [`ApiError::RelayEpochStale`] if the epoch names a superseded
/// attach, or [`ApiError::Room`] if the room could not be reached.
pub async fn daemon_commands(
    rooms: &Rooms,
    session: SessionId,
    epoch: u64,
) -> Result<Response, ApiError> {
    rooms.daemon_commands(session, epoch).await
}

/// Accepts one batch of a session daemon's outbound frames.
///
/// # Errors
///
/// Returns [`ApiError::RelayEpochStale`] if the epoch is stale,
/// [`ApiError::RelayFramesGap`] if the batch skips a sequence number, or
/// [`ApiError::Room`] if the room could not be reached.
pub async fn daemon_frames(
    rooms: &Rooms,
    db: &Db,
    session: SessionId,
    batch: DaemonFrames,
) -> Result<NoContent, ApiError> {
    rooms.daemon_frames(db, session, &batch).await?;
    Ok(NoContent)
}

/// Attaches an enrolled machine to its host room.
///
/// # Errors
///
/// Returns [`ApiError::Room`] if the room could not be reached.
pub async fn attach_host(
    rooms: &HostRooms,
    db: &Db,
    host: HostId,
    attach: HostAttach,
) -> Result<HostAttachResponse, ApiError> {
    // A machine that attaches is online, and this is the only hop where
    // the control plane can write that down: the attach itself reaches a
    // Durable Object, which cannot touch D1. The reverse — an attachment
    // that went quiet — is learned from the room on the next read.
    hosts::arrived(db, host).await?;

    let attached = rooms.attach(host, &attach).await?;
    tracing::info!(%host, epoch = attached.epoch, "a machine attached to its host room");
    Ok(attached)
}

/// Opens an enrolled machine's command stream.
///
/// # Errors
///
/// Returns [`ApiError::RelayEpochStale`] if the epoch names a superseded
/// attach, or [`ApiError::Room`] if the room could not be reached.
pub async fn host_commands(
    rooms: &HostRooms,
    host: HostId,
    epoch: u64,
) -> Result<Response, ApiError> {
    rooms.commands(host, epoch).await
}

/// Accepts one batch of an enrolled machine's outbound frames.
///
/// # Errors
///
/// Returns [`ApiError::RelayEpochStale`] if the epoch is stale,
/// [`ApiError::RelayFramesGap`] if the batch skips a sequence number, or
/// [`ApiError::Room`] if the room could not be reached.
pub async fn host_frames(
    rooms: &HostRooms,
    host: HostId,
    batch: HostFrames,
) -> Result<NoContent, ApiError> {
    rooms.frames(host, &batch).await?;
    Ok(NoContent)
}
