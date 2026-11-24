//! The row-read budget, unit and end to end.
//!
//! `check_budget`/`debit_reads`/`charge_reads` are exercised directly for
//! their ledger arithmetic, and the room's `GET /internal/events` for the
//! contract issue #351 pinned: an empty page leaves the ledger unwritten,
//! a page of N rows debits N, and a spent room is refused for the ledger's
//! one row rather than the page it asked for.

use flyco_core::{ClientEvent, MessageOrigin, SessionId};
use skyzen::durable::DurableObject as _;
use skyzen::{Method, Request};
use skyzen_services::durable::DurableDb;
use skyzen_test::mock::InMemoryDurableDb;

use crate::error::ApiError;
use crate::room::SessionRoom;
use crate::row_budget::{self, ROWS_PER_DAY, charge_reads};
use crate::testing::{room_request, rows_billed};

/// One object with its ledger schema applied.
async fn object() -> DurableDb {
    let db = DurableDb::new(
        InMemoryDurableDb::in_memory()
            .await
            .expect("an in-memory database"),
    );
    db.query(row_budget::SCHEMA)
        .execute()
        .await
        .expect("the ledger schema applies");
    db
}

/// One Worker→room call against `backend`, injected where the simulator
/// would put the object's storage.
fn call(
    backend: &InMemoryDurableDb,
    session: SessionId,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> Request {
    let mut request = room_request(session, method, path, body);
    request
        .extensions_mut()
        .insert(DurableDb::new(backend.clone()));
    request
}

#[skyzen::test]
async fn charges_accumulate_against_the_day() {
    let db = object().await;
    charge_reads(&db, 100).await.expect("under the cap");
    charge_reads(&db, 50).await.expect("still under");
    assert_eq!(rows_billed(&db).await, 150, "the ledger summed the charges");
}

#[skyzen::test]
async fn a_spent_day_refuses() {
    let db = object().await;
    charge_reads(&db, ROWS_PER_DAY)
        .await
        .expect("the charge that reaches the cap is allowed");
    let error = charge_reads(&db, 1)
        .await
        .expect_err("the next charge is refused");
    assert!(matches!(error, ApiError::RowBudgetExceeded));
}

#[skyzen::test]
async fn the_charge_that_crosses_the_cap_is_billed_once() {
    let db = object().await;
    charge_reads(&db, ROWS_PER_DAY - 1)
        .await
        .expect("under the cap");
    charge_reads(&db, 10)
        .await
        .expect("the crossing charge lands; the refusal comes next");
    assert_eq!(rows_billed(&db).await, ROWS_PER_DAY + 9);
    assert!(matches!(
        charge_reads(&db, 1).await.expect_err("over the cap"),
        ApiError::RowBudgetExceeded
    ));
}

#[skyzen::test]
async fn one_objects_budget_leaves_anothers_alone() {
    let broke = object().await;
    let fine = object().await;
    charge_reads(&broke, ROWS_PER_DAY)
        .await
        .expect("the charge that reaches the cap is allowed");
    assert!(charge_reads(&broke, 1).await.is_err());
    charge_reads(&fine, 1)
        .await
        .expect("a neighbour object's ledger is untouched");
}

#[skyzen::test]
async fn an_empty_events_page_leaves_the_ledger_unwritten() {
    let session = SessionId::generate();
    let object = SessionRoom::default();
    let backend = InMemoryDurableDb::in_memory()
        .await
        .expect("an in-memory database");

    let response = object
        .fetch()
        .go(call(
            &backend,
            session,
            Method::GET,
            "/internal/events?after=0",
            None,
        ))
        .await
        .expect("the room answered");
    assert_eq!(response.status().as_u16(), 200);

    // The refusal check costs one read of the ledger; with nothing in the
    // tail there is nothing to debit, so the ledger holds no row at all.
    let db = DurableDb::new(backend);
    let rows: u64 = db
        .query("SELECT COUNT(*) FROM row_budget")
        .fetch_scalar()
        .await
        .expect("the ledger reads");
    assert_eq!(rows, 0, "an empty page wrote nothing to the ledger");
}

#[skyzen::test]
async fn an_events_page_is_billed_the_rows_it_returned() {
    let session = SessionId::generate();
    let object = SessionRoom::default();
    let backend = InMemoryDurableDb::in_memory()
        .await
        .expect("an in-memory database");

    // Three events in the tail — a page whose bound is 501 rows but whose
    // read is three.
    for index in 0..3_u32 {
        let event = ClientEvent::UserMessage {
            text: format!("message {index}"),
            origin: MessageOrigin::User,
        };
        let response = object
            .fetch()
            .go(call(
                &backend,
                session,
                Method::POST,
                "/internal/broadcast",
                Some(serde_json::to_vec(&event).expect("serialize")),
            ))
            .await
            .expect("the room answered");
        assert_eq!(response.status().as_u16(), 200, "a broadcast is stored");
    }

    let response = object
        .fetch()
        .go(call(
            &backend,
            session,
            Method::GET,
            "/internal/events?after=0",
            None,
        ))
        .await
        .expect("the room answered");
    assert_eq!(response.status().as_u16(), 200);

    let db = DurableDb::new(backend);
    assert_eq!(
        rows_billed(&db).await,
        3,
        "the debit is the rows the page read, not its 501-row bound"
    );
}

#[skyzen::test]
async fn a_room_past_its_budget_answers_429() {
    let session = SessionId::generate();
    let object = SessionRoom::default();
    let backend = InMemoryDurableDb::in_memory()
        .await
        .expect("an in-memory database");
    let db = DurableDb::new(backend.clone());
    db.query(row_budget::SCHEMA)
        .execute()
        .await
        .expect("the ledger schema applies");
    charge_reads(&db, ROWS_PER_DAY)
        .await
        .expect("the charge that reaches the cap is allowed");

    // The refusal lands on the ledger's one row: the day is spent, so the
    // page's select never runs and nothing is debited past the cap.
    let response = object
        .fetch()
        .go(call(
            &backend,
            session,
            Method::GET,
            "/internal/events?after=0",
            None,
        ))
        .await
        .expect("the room answered");
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        rows_billed(&db).await,
        ROWS_PER_DAY,
        "a refused page debits nothing"
    );
}

#[derive(Debug, Clone)]
struct CountingStreamDb {
    inner: skyzen_services::durable::SqliteDurableDb,
    table: &'static str,
    selects: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingStreamDb {
    async fn open(table: &'static str) -> Self {
        Self {
            inner: skyzen_services::durable::SqliteDurableDb::in_memory()
                .await
                .expect("an in-memory database"),
            table,
            selects: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn reset(&self) {
        self.selects.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn count(&self) -> usize {
        self.selects.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl skyzen_services::DurableDbBackend for CountingStreamDb {
    async fn query(
        &self,
        query: &str,
        params: &[skyzen_services::DbValue],
    ) -> Result<skyzen_services::DbExecResult, skyzen_services::DurableDbError> {
        if query.contains(self.table) && query.contains("WHERE seq >") {
            self.selects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.inner.query(query, params).await
    }

    async fn execute(
        &self,
        query: &str,
        params: &[skyzen_services::DbValue],
    ) -> Result<skyzen_services::DbExecResult, skyzen_services::DurableDbError> {
        self.inner.execute(query, params).await
    }

    async fn database_size(&self) -> Result<u64, skyzen_services::DurableDbError> {
        self.inner.database_size().await
    }

    async fn sync(&self) -> Result<(), skyzen_services::DurableDbError> {
        self.inner.sync().await
    }
}

async fn assert_sse_eof(response: skyzen::Response, label: &str) {
    use std::time::Duration;

    use futures_util::StreamExt as _;

    assert_eq!(response.status().as_u16(), 200);
    let mut sse = response.into_body().into_sse();
    let timeout = futures_timer::Delay::new(Duration::from_secs(5));
    let next_item = sse.next();
    futures_util::pin_mut!(next_item, timeout);
    match futures_util::future::select(next_item, timeout).await {
        futures_util::future::Either::Left((None, _)) => {}
        futures_util::future::Either::Left((Some(item), _)) => {
            panic!("expected {label} EOF on exhausted budget, got {item:?}")
        }
        futures_util::future::Either::Right(_) => {
            panic!("{label} timed out instead of closing on exhausted budget")
        }
    }
}

#[skyzen::test]
async fn an_exhausted_row_budget_closes_session_command_and_desktop_streams() {
    use flyco_core::wire::{DaemonAttach, DaemonFrames};
    use flyco_core::{ControlToDaemon, DaemonToControl, WIRE_PROTOCOL_VERSION};
    use skyzen_services::durable::DurableKv;
    use skyzen_test::mock::InMemoryDurableKv;

    let cmd_db = CountingStreamDb::open("FROM daemon_commands").await;
    let kv = InMemoryDurableKv::new();
    let session = SessionId::generate();
    let object = SessionRoom::default();

    let make_req = |db: &CountingStreamDb, method: Method, path: &str, body: Option<Vec<u8>>| {
        let mut request = room_request(session, method, path, body);
        request.extensions_mut().insert(DurableDb::new(db.clone()));
        request.extensions_mut().insert(DurableKv::new(kv.clone()));
        request
    };

    for (path, body) in [
        (
            "/internal/daemon-attach",
            serde_json::to_vec(&DaemonAttach {
                protocol_version: WIRE_PROTOCOL_VERSION,
            })
            .expect("serialize"),
        ),
        (
            "/internal/command",
            serde_json::to_vec(&ControlToDaemon::UserMessage {
                text: "queued".to_owned(),
                origin: MessageOrigin::User,
            })
            .expect("serialize"),
        ),
        (
            "/internal/frames",
            serde_json::to_vec(&DaemonFrames {
                epoch: 1,
                from_seq: 1,
                ack_through: 0,
                frames: vec![DaemonToControl::DesktopChunk {
                    keyframe: true,
                    data: b"key".to_vec(),
                }],
            })
            .expect("serialize"),
        ),
    ] {
        let resp = object
            .fetch()
            .go(make_req(&cmd_db, Method::POST, path, Some(body)))
            .await
            .expect("room answered");
        assert_eq!(resp.status().as_u16(), 200);
    }

    let db = DurableDb::new(cmd_db.clone());
    charge_reads(&db, ROWS_PER_DAY)
        .await
        .expect("spend the day's budget");
    cmd_db.reset();

    let commands_resp = object
        .fetch()
        .go(make_req(
            &cmd_db,
            Method::GET,
            "/internal/commands?epoch=1",
            None,
        ))
        .await
        .expect("command stream answered");
    assert_sse_eof(commands_resp, "command stream").await;
    assert_eq!(
        cmd_db.count(),
        0,
        "command stream must not query daemon_commands once budget is exhausted"
    );

    let chunk_db = CountingStreamDb {
        inner: cmd_db.inner.clone(),
        table: "FROM desktop_chunks",
        selects: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let desktop_resp = object
        .fetch()
        .go(make_req(
            &chunk_db,
            Method::GET,
            "/internal/desktop/watch",
            None,
        ))
        .await
        .expect("desktop stream answered");
    assert_sse_eof(desktop_resp, "desktop stream").await;
    assert_eq!(
        chunk_db.count(),
        0,
        "desktop stream must not query desktop_chunks once budget is exhausted"
    );
    assert_eq!(rows_billed(&db).await, ROWS_PER_DAY);
}

#[skyzen::test]
async fn an_exhausted_row_budget_closes_host_command_stream() {
    use flyco_core::{HostId, MachineId};
    use flyco_provider::host::{ContainerJob, ControlToHost, HostAttach, container_name};
    use skyzen::Body;
    use skyzen_services::durable::DurableKv;
    use skyzen_test::mock::InMemoryDurableKv;

    use crate::host_room::{HEADER_HOST, HostRoom};
    use crate::room::{HEADER_INTERNAL, INTERNAL};

    let backend = CountingStreamDb::open("FROM host_commands").await;
    let kv = InMemoryDurableKv::new();
    let host = HostId::generate();
    let object = HostRoom::default();

    let make_req = |method: Method, path: &str, body: Option<Vec<u8>>| {
        let mut request = Request::new(body.map_or_else(Body::empty, Body::from));
        *request.method_mut() = method;
        *request.uri_mut() = format!("https://host-room.flyco.invalid{path}")
            .parse()
            .expect("a valid host room URL");
        request
            .headers_mut()
            .insert(HEADER_INTERNAL, INTERNAL.parse().expect("valid header"));
        request
            .headers_mut()
            .insert(HEADER_HOST, host.to_string().parse().expect("valid header"));
        request.headers_mut().insert(
            skyzen::header::CONTENT_TYPE,
            skyzen::header::HeaderValue::from_static("application/json"),
        );
        request
            .extensions_mut()
            .insert(DurableDb::new(backend.clone()));
        request.extensions_mut().insert(DurableKv::new(kv.clone()));
        request
    };

    let attach = object
        .fetch()
        .go(make_req(
            Method::POST,
            "/internal/attach",
            Some(
                serde_json::to_vec(&HostAttach {
                    facts: Box::new(crate::testing::host_facts()),
                })
                .expect("serialize"),
            ),
        ))
        .await
        .expect("attach answered");
    assert_eq!(attach.status().as_u16(), 200);

    let job = ControlToHost::Run {
        job: ContainerJob::Stop {
            container: container_name(MachineId::generate()),
        },
    };
    let queued = object
        .fetch()
        .go(make_req(
            Method::POST,
            "/internal/command",
            Some(serde_json::to_vec(&job).expect("serialize")),
        ))
        .await
        .expect("command answered");
    assert_eq!(queued.status().as_u16(), 204);

    let db = DurableDb::new(backend.clone());
    charge_reads(&db, ROWS_PER_DAY)
        .await
        .expect("spend the day's budget");
    backend.reset();

    let stream_resp = object
        .fetch()
        .go(make_req(Method::GET, "/internal/commands?epoch=1", None))
        .await
        .expect("host command stream answered");
    assert_sse_eof(stream_resp, "host command stream").await;
    assert_eq!(
        backend.count(),
        0,
        "host command stream must not query host_commands once budget is exhausted"
    );
    assert_eq!(rows_billed(&db).await, ROWS_PER_DAY);
}
