use flyco_core::{
    BudgetId, BudgetSignal, BudgetStage, CloudProviderKind, MachineSpec, MachineState, Runtime,
    SessionState, SpendKind, Usd,
};
use skyzen::sql;
use skyzen_services::Db;
use skyzen_test::TestContext;

use crate::budgets;
use crate::metering;
use crate::rooms::{NativeRooms, Rooms};
use crate::testing::{migrate, seed_provider_account, seed_session, seed_user};
use crate::{machines, sessions};

#[derive(Debug, skyzen::FromRow)]
struct LedgerRow {
    kind: SpendKind,
    amount_micros: Usd,
}

#[skyzen::test]
async fn scheduled_metering_records_compute_and_storage_once_and_pauses(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user(&db).await;
    let session = seed_session(&db, &user).await;
    let account = seed_provider_account(&db, user.id).await;
    let machine = machines::reserve(
        &db,
        session,
        account,
        &MachineSpec {
            provider: CloudProviderKind::Host,
            machine_type: "build.lexo.cool".to_owned(),
            runtime: Runtime::Container,
            region: "build.lexo.cool".to_owned(),
            spot: false,
            disk_gib: 64,
        },
    )
    .await
    .expect("reserve machine");
    let active = SessionState::Active;
    let running = MachineState::Running;
    let origin = 100_u64;
    sql!(
        db,
        "UPDATE sessions SET state = {active} WHERE id = {session}"
    )
    .execute()
    .await
    .expect("activate session");
    sql!(
        db,
        "UPDATE machines SET state = {running}, hourly_micros = 9500000, \
         storage_hourly_micros = 500000, compute_meter_started_at_unix = {origin}, \
         compute_metered_at_unix = {origin}, storage_meter_started_at_unix = {origin}, \
         storage_metered_at_unix = {origin} WHERE id = {machine}"
    )
    .execute()
    .await
    .expect("price machine");

    metering::accrue(&db, 3_700).await.expect("accrue one hour");
    metering::accrue(&db, 3_700)
        .await
        .expect("repeat the same cron safely");

    let budget: BudgetId = sql!(db, "SELECT budget_id FROM sessions WHERE id = {session}")
        .fetch_scalar()
        .await
        .expect("budget id");
    let view = budgets::view(&db, budget).await.expect("budget view");
    assert_eq!(view.spent, Usd::from_dollars(10));
    assert_eq!(view.stage, BudgetStage::Exhausted);
    let ledger: Vec<LedgerRow> = sql!(
        db,
        "SELECT kind, amount_micros FROM spend_events WHERE budget_id = {budget} \
         ORDER BY kind"
    )
    .fetch_all()
    .await
    .expect("ledger");
    assert_eq!(ledger.len(), 120, "two one-minute meters for one hour");
    assert_eq!(
        ledger.iter().map(|row| row.amount_micros).sum::<Usd>(),
        Usd::from_dollars(10)
    );
    assert!(ledger.iter().any(|row| row.kind == SpendKind::Compute));
    assert!(ledger.iter().any(|row| row.kind == SpendKind::Storage));
    assert!(
        budgets::pending_signals(&db)
            .await
            .expect("outbox")
            .iter()
            .any(|row| row.signal == BudgetSignal::Pause)
    );

    let rooms = Rooms::from_native(NativeRooms::new(), crate::rooms::NativeUserStreams::new());
    metering::deliver(&db, &rooms).await.expect("deliver pause");
    assert_eq!(
        sessions::state_of(&db, user.id, session)
            .await
            .expect("state"),
        SessionState::Paused
    );
    assert!(
        budgets::pending_signals(&db)
            .await
            .expect("empty outbox")
            .is_empty()
    );
}

#[skyzen::test]
async fn deliver_drops_terminal_session_signals_and_isolates_per_session_errors(
    _ctx: TestContext,
    db: Db,
) {
    migrate(&db).await;
    let user = seed_user(&db).await;

    // 1. Already-archived session with a queued Pause signal: must be dropped.
    let archived = seed_session(&db, &user).await;
    let archived_state = SessionState::Archived;
    sql!(
        db,
        "UPDATE sessions SET state = {archived_state} WHERE id = {archived}"
    )
    .execute()
    .await
    .expect("archive");

    // 2. Already-failed session with a queued Warning signal: must be dropped.
    let failed = seed_session(&db, &user).await;
    let failed_state = SessionState::Failed;
    sql!(
        db,
        "UPDATE sessions SET state = {failed_state} WHERE id = {failed}"
    )
    .execute()
    .await
    .expect("fail");

    // 3. Provisioning session with a Pause signal: InvalidTransition must be
    // isolated so the active session behind it is still delivered.
    let provisioning = seed_session(&db, &user).await;

    // 4. Active session with a Pause signal: must still be paused and delivered.
    let active = seed_session(&db, &user).await;
    let active_state = SessionState::Active;
    sql!(
        db,
        "UPDATE sessions SET state = {active_state} WHERE id = {active}"
    )
    .execute()
    .await
    .expect("activate");

    let pause = BudgetSignal::Pause;
    let warning = BudgetSignal::Warn80;
    for (ordinal, (session_id, signal)) in [
        (1_u32, (archived, pause)),
        (2_u32, (failed, warning)),
        (3_u32, (provisioning, pause)),
        (4_u32, (active, pause)),
    ] {
        let budget_id: BudgetId =
            sql!(db, "SELECT budget_id FROM sessions WHERE id = {session_id}")
                .fetch_scalar()
                .await
                .expect("budget id");
        let id = flyco_core::BudgetSignalId::generate();
        sql!(
            db,
            "INSERT INTO budget_signals \
             (id, budget_id, session_id, signal, ordinal, created_at_unix, delivered) \
             VALUES ({id}, {budget_id}, {session_id}, {signal}, {ordinal}, 1000, 0)"
        )
        .execute()
        .await
        .expect("insert signal");
    }

    let rooms = Rooms::from_native(NativeRooms::new(), crate::rooms::NativeUserStreams::new());
    metering::deliver(&db, &rooms)
        .await
        .expect("deliver must isolate per-session errors");

    assert_eq!(
        sessions::state_of(&db, user.id, active)
            .await
            .expect("active state"),
        SessionState::Paused,
        "an earlier session's InvalidTransition must not block later sessions"
    );

    let remaining = budgets::pending_signals(&db).await.expect("outbox");
    assert_eq!(
        remaining.len(),
        1,
        "archived and failed signals were dropped and active delivered; only provisioning remains"
    );
    assert_eq!(remaining[0].session_id, provisioning);
}

#[skyzen::test]
async fn run_leg_isolates_failures_so_later_legs_still_run() {
    let mut ran = Vec::new();
    let mut failures = Vec::new();

    metering::run_leg(
        "first",
        async {
            ran.push("first");
            Err::<(), &str>("boom-1")
        },
        &mut failures,
    )
    .await;
    metering::run_leg(
        "second",
        async {
            ran.push("second");
            Ok::<(), &str>(())
        },
        &mut failures,
    )
    .await;
    metering::run_leg(
        "third",
        async {
            ran.push("third");
            Err::<(), &str>("boom-3")
        },
        &mut failures,
    )
    .await;

    assert_eq!(ran, vec!["first", "second", "third"]);
    let summary = metering::summarize_failures(&failures).expect("two failures recorded");
    assert!(summary.contains("first: boom-1"), "{summary}");
    assert!(summary.contains("third: boom-3"), "{summary}");
    assert!(!summary.contains("second"), "{summary}");
}
