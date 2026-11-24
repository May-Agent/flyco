//! `GET /v1/sessions/{id}/turns` backed by the `SessionRoom` turn projection.
//!
//! Every test drives a real [`SessionRoom`] over in-memory SQLite so turn
//! projection, cursor advancement across >500-event turns, legacy schema v3
//! backfill, and `row_budget` debits are all verified against actual storage.

use flyco_core::{
    ClientEvent, ContextWindow, HarnessEvent, MessageOrigin, PROMPT_EXCERPT_CHARS, SessionId,
    TurnPage, UsageReport, Usd,
};
use skyzen::durable::DurableObject as _;
use skyzen::{Method, Request};
use skyzen_services::durable::DurableDb;
use skyzen_test::mock::InMemoryDurableDb;

use crate::room::{EVENT_PAGE_LIMIT, SessionRoom};
use crate::testing::{room_request, rows_billed};

/// A session room on storage the test can inspect directly.
struct Room {
    session: SessionId,
    object: SessionRoom,
    db: InMemoryDurableDb,
}

impl Room {
    async fn open() -> Self {
        Self {
            session: SessionId::generate(),
            object: SessionRoom::default(),
            db: InMemoryDurableDb::in_memory()
                .await
                .expect("an in-memory database"),
        }
    }

    fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Request {
        let mut request = room_request(self.session, method, path, body);
        request
            .extensions_mut()
            .insert(DurableDb::new(self.db.clone()));
        request
    }

    async fn record(&self, event: &ClientEvent) {
        let request = self.request(
            Method::POST,
            "/internal/broadcast",
            Some(serde_json::to_vec(event).expect("serialize")),
        );
        let response = self
            .object
            .fetch()
            .go(request)
            .await
            .expect("the room answered");
        assert_eq!(response.status().as_u16(), 200, "a broadcast is stored");
    }

    async fn turns(&self, after: u64, limit: u32) -> TurnPage {
        let request = self.request(
            Method::GET,
            &format!("/internal/turns?after={after}&limit={limit}"),
            None,
        );
        let response = self
            .object
            .fetch()
            .go(request)
            .await
            .expect("the room answered");
        assert_eq!(
            response.status().as_u16(),
            200,
            "the room should serve a turn page"
        );
        let body = response
            .into_body()
            .into_bytes()
            .await
            .expect("a readable body");
        serde_json::from_slice(&body).expect("a turn page")
    }

    async fn spent(&self) -> u64 {
        rows_billed(&DurableDb::new(self.db.clone())).await
    }
}

fn usage() -> UsageReport {
    UsageReport {
        input_tokens: 12,
        output_tokens: 34,
        context: Some(ContextWindow {
            used_tokens: 1_000,
            size_tokens: 200_000,
        }),
        estimated_cost: Some(Usd::from_cents(7)),
    }
}

fn said(text: &str) -> ClientEvent {
    ClientEvent::UserMessage {
        text: text.to_owned(),
        origin: MessageOrigin::User,
    }
}

fn harness(event: HarnessEvent) -> ClientEvent {
    ClientEvent::Harness { event }
}

fn started(turn: &str) -> ClientEvent {
    harness(HarnessEvent::TurnStarted {
        turn_id: turn.to_owned(),
    })
}

fn completed(turn: &str) -> ClientEvent {
    harness(HarnessEvent::TurnCompleted {
        turn_id: turn.to_owned(),
        usage: usage(),
    })
}

#[skyzen::test]
async fn a_turn_is_named_by_the_message_that_began_it_and_bills_only_turn_rows() {
    let room = Room::open().await;
    for event in [
        said("audit the relay"),
        started("t-1"),
        harness(HarnessEvent::AssistantDelta {
            turn_id: "t-1".to_owned(),
            text: "working...".to_owned(),
        }),
        completed("t-1"),
    ] {
        room.record(&event).await;
    }

    let page = room.turns(0, 10).await;
    assert_eq!(page.turns.len(), 1);
    let turn = &page.turns[0];
    assert_eq!(turn.turn_id, "t-1");
    assert_eq!(turn.prompt_excerpt, "audit the relay");
    assert!(turn.completed_at_unix.is_some());
    assert_eq!(turn.usage, Some(usage()));
    assert!(page.next_cursor.is_none());
    assert_eq!(
        room.spent().await,
        1,
        "reading the turn projection bills only the turn rows read, not stream deltas"
    );
}

#[skyzen::test]
async fn a_turn_spanning_more_than_500_events_advances_cursor_cleanly() {
    let room = Room::open().await;
    let delta_count = usize::try_from(EVENT_PAGE_LIMIT).expect("usize") + 150;

    room.record(&said("large turn")).await;
    room.record(&started("t-big")).await;
    for _ in 0..delta_count {
        room.record(&harness(HarnessEvent::AssistantDelta {
            turn_id: "t-big".to_owned(),
            text: ".".to_owned(),
        }))
        .await;
    }
    room.record(&completed("t-big")).await;
    room.record(&said("follow-up")).await;
    room.record(&started("t-next")).await;
    room.record(&completed("t-next")).await;

    let first = room.turns(0, 1).await;
    assert_eq!(first.turns.len(), 1);
    assert_eq!(first.turns[0].turn_id, "t-big");
    assert!(first.turns[0].completed_at_unix.is_some());
    let cursor = first
        .next_cursor
        .as_deref()
        .expect("second turn exists")
        .parse::<u64>()
        .expect("numeric cursor");
    assert!(cursor > 0, "cursor must strictly advance past 0");

    let second = room.turns(cursor, 1).await;
    assert_eq!(second.turns.len(), 1);
    assert_eq!(second.turns[0].turn_id, "t-next");
    assert_eq!(second.turns[0].prompt_excerpt, "follow-up");
    assert!(second.next_cursor.is_none());

    assert_eq!(
        room.spent().await,
        3,
        "two pages across a >500-event turn billed only 3 turn rows, not >650 event rows"
    );
}

#[skyzen::test]
async fn a_failed_turn_completes_without_usage_and_an_open_turn_has_no_completion() {
    let room = Room::open().await;
    for event in [
        said("break something"),
        started("t-1"),
        harness(HarnessEvent::TurnFailed {
            turn_id: "t-1".to_owned(),
            error: "the model refused".to_owned(),
        }),
        said("still running"),
        started("t-2"),
    ] {
        room.record(&event).await;
    }

    let page = room.turns(0, 10).await;
    assert_eq!(page.turns.len(), 2);
    assert_eq!(page.turns[0].turn_id, "t-1");
    assert!(page.turns[0].completed_at_unix.is_some());
    assert!(page.turns[0].usage.is_none());
    assert_eq!(page.turns[1].turn_id, "t-2");
    assert!(page.turns[1].completed_at_unix.is_none());
    assert!(page.next_cursor.is_none());
}

#[skyzen::test]
async fn unprompted_and_long_prompts_and_orphan_completions_match_contract() {
    let room = Room::open().await;
    let long = "x".repeat(PROMPT_EXCERPT_CHARS * 2);
    for event in [
        completed("t-orphan"),
        started("t-unprompted"),
        completed("t-unprompted"),
        said(&long),
        started("t-long"),
        completed("t-long"),
    ] {
        room.record(&event).await;
    }

    let page = room.turns(0, 10).await;
    assert_eq!(
        page.turns
            .iter()
            .map(|turn| turn.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec!["t-unprompted", "t-long"]
    );
    assert_eq!(page.turns[0].prompt_excerpt, "");
    assert_eq!(
        page.turns[1].prompt_excerpt.chars().count(),
        PROMPT_EXCERPT_CHARS
    );
    assert!(page.turns[1].prompt_excerpt.ends_with('…'));
}

#[skyzen::test]
async fn a_legacy_v3_room_backfills_preexisting_events_on_upgrade() {
    let db = InMemoryDurableDb::in_memory()
        .await
        .expect("an in-memory database");
    let durable = DurableDb::new(db.clone());
    // Seed a schema-v3 room with pre-existing `events` rows (no `turns` table yet).
    for ddl in [
        "CREATE TABLE schema_meta (id INTEGER PRIMARY KEY CHECK (id = 0), version INTEGER NOT NULL)",
        "INSERT INTO schema_meta (id, version) VALUES (0, 3)",
        "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, json TEXT NOT NULL, at_unix INTEGER NOT NULL)",
        crate::row_budget::SCHEMA,
    ] {
        durable.query(ddl).execute().await.expect("seed v3 ddl");
    }
    for (index, event) in [
        said("before upgrade"),
        started("t-legacy"),
        completed("t-legacy"),
    ]
    .into_iter()
    .enumerate()
    {
        let json = serde_json::to_string(&event).expect("serialize");
        let at = 1_800_000_000_u64 + index as u64;
        skyzen::sql!(
            durable,
            "INSERT INTO events (json, at_unix) VALUES ({json}, {at})"
        )
        .execute()
        .await
        .expect("seed legacy event");
    }

    let room = Room {
        session: SessionId::generate(),
        object: SessionRoom::default(),
        db,
    };

    let page = room.turns(0, 10).await;
    assert_eq!(page.turns.len(), 1);
    assert_eq!(page.turns[0].turn_id, "t-legacy");
    assert_eq!(page.turns[0].prompt_excerpt, "before upgrade");
    assert_eq!(page.turns[0].usage, Some(usage()));
    let after_first = room.spent().await;
    assert_eq!(after_first, 4, "3 backfilled events + 1 turn row");

    // Subsequent reads touch only `turns`, never rescanning `events`.
    let again = room.turns(0, 10).await;
    assert_eq!(again.turns.len(), 1);
    assert_eq!(room.spent().await - after_first, 1);
}
