//! Coverage of the web-push routes.
//!
//! The behaviour worth pinning is the upsert: a browser re-subscribes on
//! every service-worker update, and a second row for the same endpoint means
//! every notification arrives twice.

use flyco_core::{PushKeys, PushSubscription, PushSubscriptionView};
use skyzen_services::{Db, Kv};
use skyzen_test::TestContext;

use crate::session;
use crate::testing::{migrated_router, seed_other_user, seed_user};

const ENDPOINT: &str = "https://web.push.apple.com/QQAA-flyco-test-endpoint";

fn subscription() -> PushSubscription {
    PushSubscription {
        endpoint: ENDPOINT.to_owned(),
        expiration_time: None,
        keys: PushKeys {
            p256dh: "BH1HTeKM7-NwaLGHEqxeu2IamQaVVLkcsFHPIHmsCnqxcBHPQBprF41bEMOr3O1hUQ2jU1opNEm1F_lZV_sxMP8".to_owned(),
            auth: "sBXU5_tIYz-5w7G2B25BEw".to_owned(),
        },
    }
}

#[skyzen::test]
async fn re_subscribing_the_same_browser_keeps_one_row(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let client = ctx.client(router);

    let first = client
        .post("/v1/push/subscriptions")
        .bearer(&token)
        .json(&subscription())
        .send()
        .await;
    first.assert_status(201);
    let first: PushSubscriptionView = first.json();

    // The service worker updated and the browser subscribed again.
    let second = client
        .post("/v1/push/subscriptions")
        .bearer(&token)
        .json(&subscription())
        .send()
        .await;
    second.assert_status(201);
    let second: PushSubscriptionView = second.json();

    assert_eq!(
        first.id, second.id,
        "the same endpoint must land on the row it already has"
    );
}

#[skyzen::test]
async fn a_subscription_belongs_to_the_browser_that_registered_it(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let owner = seed_user(&db).await;
    let stranger = seed_other_user(&db).await;
    let owner_token = session::issue(&kv, owner.id)
        .await
        .expect("issue a session");
    let stranger_token = session::issue(&kv, stranger.id)
        .await
        .expect("issue a session");
    let client = ctx.client(router);

    let registered: PushSubscriptionView = client
        .post("/v1/push/subscriptions")
        .bearer(&owner_token)
        .json(&subscription())
        .send()
        .await
        .json();

    let path = format!("/v1/push/subscriptions/{}", registered.id);
    client
        .delete(&path)
        .bearer(&stranger_token)
        .send()
        .await
        .assert_status(404);

    client
        .delete(&path)
        .bearer(&owner_token)
        .send()
        .await
        .assert_status(204);
}

#[skyzen::test]
async fn push_always_reports_the_public_half_of_its_required_identity(ctx: TestContext, db: Db) {
    let response = ctx
        .client(migrated_router(&db).await)
        .get("/v1/push/vapid-public-key")
        .send()
        .await;

    response.assert_status(200);
}

fn subscription_at(endpoint: &str) -> PushSubscription {
    PushSubscription {
        endpoint: endpoint.to_owned(),
        expiration_time: None,
        keys: PushKeys {
            p256dh: "BH1HTeKM7-NwaLGHEqxeu2IamQaVVLkcsFHPIHmsCnqxcBHPQBprF41bEMOr3O1hUQ2jU1opNEm1F_lZV_sxMP8".to_owned(),
            auth: "sBXU5_tIYz-5w7G2B25BEw".to_owned(),
        },
    }
}

#[skyzen::test]
async fn cross_user_endpoint_takeover_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let owner = seed_user(&db).await;
    let attacker = seed_other_user(&db).await;
    let owner_token = session::issue(&kv, owner.id)
        .await
        .expect("issue owner session");
    let attacker_token = session::issue(&kv, attacker.id)
        .await
        .expect("issue attacker session");
    let client = ctx.client(router);

    let first = client
        .post("/v1/push/subscriptions")
        .bearer(&owner_token)
        .json(&subscription())
        .send()
        .await;
    first.assert_status(201);
    let view: PushSubscriptionView = first.json();

    let hijack = client
        .post("/v1/push/subscriptions")
        .bearer(&attacker_token)
        .json(&subscription())
        .send()
        .await;
    hijack.assert_status(409);
    assert_eq!(
        hijack.json::<flyco_core::Problem>().kind,
        "https://flyco.dev/problems/push-subscription-conflict"
    );

    // Owner still owns the row and can delete it.
    client
        .delete(&format!("/v1/push/subscriptions/{}", view.id))
        .bearer(&owner_token)
        .send()
        .await
        .assert_status(204);
}

#[skyzen::test]
async fn per_user_subscription_cap_blocks_excess_endpoints_while_allowing_resubscribe(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let client = ctx.client(router);

    for index in 0..crate::push::MAX_SUBSCRIPTIONS_PER_USER {
        client
            .post("/v1/push/subscriptions")
            .bearer(&token)
            .json(&subscription_at(&format!(
                "https://fcm.googleapis.com/fcm/send/device-{index}"
            )))
            .send()
            .await
            .assert_status(201);
    }

    // Re-subscribing an existing endpoint still works even at the cap.
    client
        .post("/v1/push/subscriptions")
        .bearer(&token)
        .json(&subscription_at(
            "https://fcm.googleapis.com/fcm/send/device-0",
        ))
        .send()
        .await
        .assert_status(201);

    // An 11th distinct endpoint is rejected.
    let over_cap = client
        .post("/v1/push/subscriptions")
        .bearer(&token)
        .json(&subscription_at(
            "https://fcm.googleapis.com/fcm/send/device-overflow",
        ))
        .send()
        .await;
    over_cap.assert_status(422);
    assert_eq!(
        over_cap.json::<flyco_core::Problem>().kind,
        "https://flyco.dev/problems/invalid-push-subscription"
    );
}

#[skyzen::test]
async fn unvetted_or_private_push_endpoints_are_refused(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let client = ctx.client(router);

    for disallowed in [
        "http://web.push.apple.com/insecure",
        "https://127.0.0.1/push",
        "https://[::1]/push",
        "https://169.254.169.254/latest/meta-data",
        "https://localhost/push",
        "https://flyco.test/v1/healthz",
        "https://attacker.example.com/push",
        "https://web.push.apple.com:8443/custom-port",
        "https://user:pass@fcm.googleapis.com/fcm/send/abc",
        "https://evilpush.apple.com/not-a-subdomain",
    ] {
        let refused = client
            .post("/v1/push/subscriptions")
            .bearer(&token)
            .json(&subscription_at(disallowed))
            .send()
            .await;
        refused.assert_status(422);
        assert_eq!(
            refused.json::<flyco_core::Problem>().kind,
            "https://flyco.dev/problems/invalid-push-subscription",
            "expected {disallowed} to be rejected"
        );
    }

    for allowed in [
        "https://web.push.apple.com/QQAA-test",
        "https://fcm.googleapis.com/fcm/send/test",
        "https://updates.push.services.mozilla.com/wpush/v2/test",
        "https://wns2-bl2p.notify.windows.com/w/?token=abc",
    ] {
        client
            .post("/v1/push/subscriptions")
            .bearer(&token)
            .json(&subscription_at(allowed))
            .send()
            .await
            .assert_status(201);
    }
}
