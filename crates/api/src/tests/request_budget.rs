//! The per-principal request budgets, driven through the router (issue
//! #342).
//!
//! [`crate::request_budget`]'s own tests pin the ledger arithmetic; these
//! pin what a caller sees: the per-minute refusal before any credential
//! is resolved, the daily refusal once a principal's ceiling is reached,
//! the `Retry-After` both carry, and that one principal's day is nobody
//! else's.

use flyco_core::{CurrentUser, Problem};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::TestContext;
use skyzen_test::mock::InMemoryQueue;

use crate::request_budget::{Limit, Limits, Principal, WINDOW_SECONDS, recorded};
use crate::testing::{migrate, seed_user, test_router_budgeted};
use crate::{clock, session};

/// Ceilings a test reaches in a handful of requests.
///
/// The per-minute limits stay far above the per-day ones except where a
/// test lowers one on purpose, so each test exercises one bound.
fn limits() -> Limits {
    let wide = Limit {
        per_minute: 1_000,
        per_day: 1_000,
    };
    Limits {
        user: Limit {
            per_minute: 1_000,
            per_day: 3,
        },
        daemon: wide,
        host: wide,
        public: Limit {
            per_minute: 1_000,
            per_day: 2,
        },
    }
}

async fn router_with(db: &Db, limits: Limits) -> Router {
    migrate(db).await;
    test_router_budgeted(db.clone(), Queue::new(InMemoryQueue::new()), limits)
}

async fn signed_in(kv: &Kv, db: &Db) -> (CurrentUser, String) {
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    (user, token)
}

#[skyzen::test]
async fn a_user_at_its_daily_ceiling_is_refused_until_midnight(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, limits()).await);
    let (user, token) = signed_in(&kv, &db).await;

    for _ in 0..3 {
        client
            .get("/v1/me")
            .bearer(&token)
            .send()
            .await
            .assert_status(200);
    }

    let refused = client.get("/v1/me").bearer(&token).send().await;
    refused.assert_status(429);
    refused.assert_header("content-type", "application/problem+json");
    let problem: Problem = refused.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/request-budget-exhausted"
    );
    // The wait is the time to UTC midnight, which only the clock knows;
    // that it is named at all is the contract.
    refused.assert_header_exists("retry-after");

    // The day's total reached D1: the first request and the one that hit
    // the ceiling both flushed.
    let now = clock::now_unix();
    assert_eq!(
        recorded(&db, &Principal::User(user.id), now)
            .await
            .expect("read the ledger"),
        3
    );
}

#[skyzen::test]
async fn one_principals_day_is_not_anothers(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, limits()).await);
    let (_, first) = signed_in(&kv, &db).await;
    let second_user = crate::testing::seed_other_user(&db).await;
    let second = session::issue(&kv, second_user.id)
        .await
        .expect("issue a session");

    for _ in 0..3 {
        client
            .get("/v1/me")
            .bearer(&first)
            .send()
            .await
            .assert_status(200);
    }
    client
        .get("/v1/me")
        .bearer(&first)
        .send()
        .await
        .assert_status(429);

    client
        .get("/v1/me")
        .bearer(&second)
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn a_spent_user_is_refused_on_every_credential_it_holds(ctx: TestContext, kv: Kv, db: Db) {
    // The budget is the account's: a second browser session of the same
    // user runs into the same ceiling. It costs one credential lookup —
    // the pre-auth block list is keyed by credential — and is then blocked
    // like the first.
    let client = ctx.client(router_with(&db, limits()).await);
    let (user, browser) = signed_in(&kv, &db).await;
    let other_tab = session::issue(&kv, user.id).await.expect("issue a session");

    for _ in 0..3 {
        client
            .get("/v1/me")
            .bearer(&browser)
            .send()
            .await
            .assert_status(200);
    }
    // The ceiling was reached on the third request; the ledger knows.
    let refused = client.get("/v1/me").bearer(&other_tab).send().await;
    refused.assert_status(200);
    client
        .get("/v1/me")
        .bearer(&other_tab)
        .send()
        .await
        .assert_status(429);
}

#[skyzen::test]
async fn the_per_minute_bound_refuses_before_any_credential_is_resolved(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let mut limits = limits();
    limits.user.per_minute = 2;
    limits.user.per_day = 1_000;
    // The refused sign-ins are charged to the address; keep that day open
    // so the per-minute bound is the one that answers.
    limits.public.per_day = 1_000;
    let client = ctx.client(router_with(&db, limits).await);

    // A credential nobody minted: resolving it would be a KV read and a
    // 401. The third request is refused before either.
    for _ in 0..2 {
        client
            .get("/v1/me")
            .bearer("fs_never-minted")
            .send()
            .await
            .assert_status(401);
    }
    let refused = client.get("/v1/me").bearer("fs_never-minted").send().await;
    refused.assert_status(429);
    let problem: Problem = refused.json();
    assert_eq!(problem.kind, "https://flyco.dev/problems/rate-limited");
    refused.assert_header("retry-after", &WINDOW_SECONDS.to_string());
}

#[skyzen::test]
async fn anonymous_traffic_is_charged_to_its_address(ctx: TestContext, _kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, limits()).await);

    for _ in 0..2 {
        client.get("/v1/healthz").send().await.assert_status(200);
    }
    let refused = client.get("/v1/healthz").send().await;
    refused.assert_status(429);
    let problem: Problem = refused.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/request-budget-exhausted"
    );

    // Two different addresses are two principals.
    client
        .get("/v1/healthz")
        .header("cf-connecting-ip", "203.0.113.7")
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn a_credential_that_does_not_resolve_spends_the_address_not_a_user(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let client = ctx.client(router_with(&db, limits()).await);

    // Two refused sign-ins spend the address's day (public ceiling: 2);
    // the third is refused before the credential is even looked up.
    for _ in 0..2 {
        client
            .get("/v1/me")
            .bearer("fk_never-minted")
            .send()
            .await
            .assert_status(401);
    }
    client
        .get("/v1/me")
        .bearer("fk_never-minted")
        .send()
        .await
        .assert_status(429);
    let now = clock::now_unix();
    assert_eq!(
        recorded(&db, &Principal::Address("local".to_owned()), now)
            .await
            .expect("read the ledger"),
        2
    );
}

#[skyzen::test]
async fn rotating_fake_bearer_prefixes_cannot_bypass_ip_rate_limit_or_daily_budget(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let mut limits = limits();
    limits.public.per_minute = 2;
    limits.public.per_day = 100;
    let client = ctx.client(router_with(&db, limits).await);

    // Three requests from one IP, each presenting a brand-new fake `fs_` token:
    // the third hits the per-minute IP bound even though every token hash is new.
    for nonce in 0..2 {
        client
            .get("/v1/me")
            .header("cf-connecting-ip", "198.51.100.10")
            .bearer(&format!("fs_forged-{nonce}"))
            .send()
            .await
            .assert_status(401);
    }
    let throttled = client
        .get("/v1/me")
        .header("cf-connecting-ip", "198.51.100.10")
        .bearer("fs_forged-2")
        .send()
        .await;
    throttled.assert_status(429);
    assert_eq!(
        throttled.json::<Problem>().kind,
        "https://flyco.dev/problems/rate-limited"
    );
    // Subsequent requests from the same IP with yet another fake token are
    // refused pre-auth for the rest of the minute window.
    client
        .get("/v1/me")
        .header("cf-connecting-ip", "198.51.100.10")
        .bearer("fs_forged-3")
        .send()
        .await
        .assert_status(429);

    // Daily exhaustion blocks `ip:<address>` so rotating tokens cannot bypass it.
    let mut daily_limits = self::limits();
    daily_limits.public.per_minute = 1_000;
    daily_limits.public.per_day = 2;
    let daily_client = ctx.client(test_router_budgeted(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        daily_limits,
    ));
    for nonce in 0..2 {
        daily_client
            .get("/v1/me")
            .header("cf-connecting-ip", "198.51.100.11")
            .bearer(&format!("fk_forged-{nonce}"))
            .send()
            .await
            .assert_status(401);
    }
    let exhausted = daily_client
        .get("/v1/me")
        .header("cf-connecting-ip", "198.51.100.11")
        .bearer("fk_forged-brand-new")
        .send()
        .await;
    exhausted.assert_status(429);
    assert_eq!(
        exhausted.json::<Problem>().kind,
        "https://flyco.dev/problems/request-budget-exhausted"
    );
}

#[skyzen::test]
async fn daemon_relay_and_host_routes_bill_verified_principals_and_reject_forged_host_attribution(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let mut limits = limits();
    limits.public.per_day = 1;
    limits.daemon.per_day = 10;
    limits.host.per_day = 10;
    let client = ctx.client(router_with(&db, limits).await);

    let user = seed_user(&db).await;
    let session = crate::testing::seed_session(&db, &user).await;
    let daemon_token = crate::daemon_tokens::issue(&db, user.id, session)
        .await
        .expect("mint daemon token")
        .token;
    let host = crate::testing::seed_host(&db, user.id).await;

    // Multiple daemon relay requests succeed past `public.per_day = 1` because
    // `RequireDaemon` attributes them to `Principal::Session(session)`.
    for _ in 0..3 {
        client
            .post(&format!("/v1/sessions/{session}/relay/attach"))
            .header("cf-connecting-ip", "198.51.100.20")
            .bearer(&daemon_token)
            .json(&flyco_core::wire::DaemonAttach {
                protocol_version: flyco_core::WIRE_PROTOCOL_VERSION,
            })
            .send()
            .await
            .assert_status(200);
    }
    let now = clock::now_unix();
    assert_eq!(
        recorded(&db, &Principal::Session(session), now)
            .await
            .expect("read daemon ledger"),
        1, // First request flushed immediately; requests 2..3 are buffered locally.
    );
    assert_eq!(
        recorded(&db, &Principal::Address("198.51.100.20".to_owned()), now)
            .await
            .expect("read ip ledger"),
        0,
        "authenticated daemon relay traffic must not spend the public IP budget"
    );

    // Multiple host relay requests succeed past `public.per_day = 1` because
    // `RequireHost` attributes them to `Principal::Host(host)`.
    for _ in 0..3 {
        client
            .post(&format!("/v1/hosts/{host}/relay/attach"))
            .header("cf-connecting-ip", "198.51.100.21")
            .bearer(crate::testing::HOST_TOKEN)
            .json(&flyco_provider::host::HostAttach {
                facts: Box::new(crate::testing::host_facts()),
            })
            .send()
            .await
            .assert_status(200);
    }
    assert_eq!(
        recorded(&db, &Principal::Host(host), now)
            .await
            .expect("read host ledger"),
        1
    );

    // A forged `fh_` token on a public route carrying a victim host UUID in its
    // path must never charge the victim host's budget.
    let victim_host = flyco_core::HostId::generate();
    client
        .get(&format!("/v1/cli-sessions/{victim_host}?s=bogus"))
        .header("cf-connecting-ip", "198.51.100.22")
        .bearer("fh_forged-token")
        .send()
        .await
        .assert_status(410);
    assert_eq!(
        recorded(&db, &Principal::Host(victim_host), now)
            .await
            .expect("read victim host ledger"),
        0,
        "unverified fh_ tokens must never bill a host principal"
    );
    assert_eq!(
        recorded(&db, &Principal::Address("198.51.100.22".to_owned()), now)
            .await
            .expect("read attacker ip ledger"),
        1
    );
}
