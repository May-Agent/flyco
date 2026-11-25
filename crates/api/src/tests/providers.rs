//! Linking compute, and the two documents the wizards read before they do.
//!
//! What is worth pinning here is that both documents are *derived*. The AWS
//! policy is rendered from the driver's own call sites, so the wizard cannot
//! show a permission set the driver has outgrown; the machine catalog is
//! curated, so the slider's detents are the same short list the agent's
//! resize sees. Both would be easy to hardcode and quietly wrong afterwards.

use flyco_core::{
    AwsIamPolicy, CloudProviderKind, HostId, LinkProvider, MachineCatalog, MachinePricing,
    MachineSpec, Problem, ProviderAccountView, ProviderCredentials, Runtime,
};
use skyzen::sql;
use skyzen_services::sql::Row;
use skyzen_services::{Db, Kv};
use skyzen_test::TestContext;

use crate::error::ApiError;
use crate::testing::{
    SSH_HOST, TestGithub, host_facts, migrated_router, seed_provider_account, seed_session,
    seed_user, test_config,
};
use crate::{machines, provisioning, session};

#[skyzen::test]
async fn the_iam_policy_is_not_public(ctx: TestContext, db: Db) {
    ctx.client(migrated_router(&db).await)
        .get("/v1/providers/aws/iam-policy")
        .send()
        .await
        .assert_status(401);
}

#[skyzen::test]
async fn the_iam_policy_grants_exactly_what_the_driver_calls(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    let response = client
        .get("/v1/providers/aws/iam-policy")
        .bearer(&token)
        .send()
        .await;
    response.assert_status(200);
    let policy: AwsIamPolicy = response.json();

    // The actions are the driver's, not a list maintained beside it.
    assert_eq!(policy.actions, flyco_provider::aws::iam::actions());

    // And the document is real IAM JSON carrying exactly those actions,
    // rather than a string that merely looks like one.
    let document: serde_json::Value =
        serde_json::from_str(&policy.document).expect("the policy is valid JSON");
    assert_eq!(document["Version"], "2012-10-17");
    let granted = document["Statement"][0]["Action"]
        .as_array()
        .expect("a statement lists its actions")
        .iter()
        .map(|action| action.as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(granted, policy.actions);
    assert!(granted.contains(&"ec2:RunInstances".to_owned()));
    assert!(granted.contains(&"sts:GetCallerIdentity".to_owned()));
}

#[skyzen::test]
async fn curation_never_drops_the_machine_the_user_already_owns(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    seed_provider_account(&db, user.id).await;

    let response = client
        .get("/v1/machines/catalog")
        .bearer(&token)
        .send()
        .await;
    response.assert_status(200);
    let catalog = response.json::<MachineCatalog>().entries;

    // No price to rank it by, and it survives anyway: curation removes
    // machines that are known to be worse, and a machine flyco does not
    // meter is not a known-worse. Its size and its architecture are the
    // machine's own report rather than anything flyco invented.
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].machine_type, SSH_HOST);
    assert_eq!(catalog[0].pricing, MachinePricing::UserOwned);
    assert_eq!(catalog[0].capacity, Some(host_facts().capacity()));
    assert_eq!(catalog[0].lineage, Some(host_facts().lineage()));
}

#[skyzen::test]
async fn a_machine_you_own_cannot_be_linked_as_a_credential(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    // Naming a host id here would create an account pointing at a machine
    // nobody enrolled, which could never answer. A host is linked by
    // enrolling it, and by nothing else.
    let response = client
        .post("/v1/providers")
        .bearer(&token)
        .json(&LinkProvider {
            label: "the build host".to_owned(),
            credentials: ProviderCredentials::Host {
                host: HostId::generate(),
            },
            monthly_limit: None,
        })
        .send()
        .await;

    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/host-not-linkable"
    );
}

#[skyzen::test]
async fn unlinking_an_account_with_a_live_machine_counts_what_it_would_strand(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let account = seed_provider_account(&db, user.id).await;
    let session = seed_session(&db, &user).await;
    let machine = machines::reserve(
        &db,
        session,
        account,
        &MachineSpec {
            provider: CloudProviderKind::Host,
            machine_type: SSH_HOST.to_owned(),
            runtime: Runtime::Container,
            region: SSH_HOST.to_owned(),
            spot: false,
            disk_gib: 64,
        },
    )
    .await
    .expect("reserve a machine on the account");

    let refused = client
        .delete(&format!("/v1/providers/{account}"))
        .bearer(&token)
        .send()
        .await;
    refused.assert_status(409);
    let problem = refused.json::<Problem>();
    assert_eq!(problem.kind, "https://flyco.dev/problems/provider-in-use");
    // Counted as a member of the document as well as in the sentence: the
    // dialog that explains the refusal states the number, and a sentence
    // written for a person is free to be reworded (RFC 9457 §3.2).
    assert_eq!(problem.extensions.active_sessions, Some(1));
    assert!(
        problem.detail.contains('1'),
        "and in the sentence, for a reader: {}",
        problem.detail
    );

    // What the refusal counts is live machines and nothing else: an account
    // whose machines are all destroyed is one nothing is running on.
    let destroyed = flyco_core::MachineState::Destroyed;
    sql!(
        db,
        "UPDATE machines SET state = {destroyed} WHERE provider_account_id = {account}"
    )
    .execute()
    .await
    .expect("destroy the machine");

    client
        .delete(&format!("/v1/providers/{account}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);

    // Gone from everywhere an account is offered…
    let listed = client.get("/v1/providers").bearer(&token).send().await;
    listed.assert_status(200);
    assert_eq!(
        listed.json::<Vec<ProviderAccountView>>(),
        [] as [flyco_core::ProviderAccountView; 0]
    );

    // …and gone as something to provision through, which is what the
    // scrubbed credential means. Naming it by id is a 404 like any account
    // the caller does not have.
    let github = TestGithub::default();
    assert!(matches!(
        provisioning::account(&db, &test_config(), &github, user.id, account).await,
        Err(ApiError::ProviderAccountNotFound)
    ));

    // But the machine row still exists and still names it. That row is the
    // spend history the budget ledger explains, and it would have nothing
    // to point at if unlinking had deleted the account (issue #153).
    let row: Row = sql!(
        db,
        "SELECT provider_account_id, state FROM machines WHERE id = {machine}"
    )
    .fetch_one()
    .await
    .expect("the machine row outlives the account it ran on");
    assert_eq!(
        row.get::<String>("provider_account_id")
            .expect("provider_account_id"),
        account.to_string()
    );

    // What is left of the account is a name and a date, with no credential
    // in it.
    let kept: Row = sql!(
        db,
        "SELECT label, credentials_enc, unlinked_at_unix FROM provider_accounts \
         WHERE id = {account}"
    )
    .fetch_one()
    .await
    .expect("the account row is kept as history");
    assert_eq!(
        kept.get::<String>("credentials_enc")
            .expect("credentials_enc"),
        "",
        "the sealed credential is scrubbed, not kept beside a flag"
    );
    assert!(
        kept.get::<i64>("unlinked_at_unix")
            .expect("unlinked_at_unix")
            > 0
    );

    // Unlinking it again is a 404: there is no credential left to withdraw,
    // and reporting success would say one had just been.
    client
        .delete(&format!("/v1/providers/{account}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(404);
}

#[skyzen::test]
async fn an_unlinked_account_is_offered_by_nothing_that_provisions(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let account = seed_provider_account(&db, user.id).await;

    // Everything the account feeds while it is linked: the catalog the
    // machine slider is built from, and the machine flyco would pick.
    let catalog = client
        .get("/v1/machines/catalog")
        .bearer(&token)
        .send()
        .await;
    catalog.assert_status(200);
    assert_ne!(
        catalog.json::<MachineCatalog>().entries,
        [] as [flyco_core::MachineCatalogEntry; 0]
    );
    client
        .get("/v1/machines/default")
        .bearer(&token)
        .send()
        .await
        .assert_status(200);

    client
        .delete(&format!("/v1/providers/{account}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);

    // And afterwards the user reads exactly as one who linked nothing:
    // there is no catalog to offer and no machine to pick, because the
    // credential that would have answered for both is gone.
    let catalog = client
        .get("/v1/machines/catalog")
        .bearer(&token)
        .send()
        .await;
    catalog.assert_status(200);
    assert_eq!(
        catalog.json::<MachineCatalog>().entries,
        [] as [flyco_core::MachineCatalogEntry; 0]
    );

    let default = client
        .get("/v1/machines/default")
        .bearer(&token)
        .send()
        .await;
    default.assert_status(422);
    assert_eq!(
        default.json::<Problem>().kind,
        "https://flyco.dev/problems/no-deployable-linux-machine"
    );

    // Usage reads the same accounts, so an unlinked one meters nothing.
    let usage = client.get("/v1/usage/cloud").bearer(&token).send().await;
    usage.assert_status(200);
    assert_eq!(
        usage.json::<Vec<flyco_core::CloudUsageView>>(),
        [] as [flyco_core::CloudUsageView; 0]
    );
}

#[skyzen::test]
async fn linking_the_same_account_again_is_a_new_row_beside_the_old_one(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let first = seed_provider_account(&db, user.id).await;

    client
        .delete(&format!("/v1/providers/{first}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);

    let second = seed_provider_account(&db, user.id).await;
    assert_ne!(
        second, first,
        "relinking mints an account, it does not revive one"
    );

    let listed = client.get("/v1/providers").bearer(&token).send().await;
    let listed: Vec<ProviderAccountView> = listed.json();
    assert_eq!(
        listed.iter().map(|account| account.id).collect::<Vec<_>>(),
        vec![second],
        "the history stays history: only the live account is offered"
    );
}

#[skyzen::test]
async fn patch_provider_updates_and_clears_monthly_quota(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let account = seed_provider_account(&db, user.id).await;

    // Set monthly limit to $40.00 and rename.
    let updated = client
        .patch(&format!("/v1/providers/{account}"))
        .bearer(&token)
        .json(&flyco_core::UpdateProviderAccount {
            label: Some("gcp-account-1".to_owned()),
            monthly_limit: Some(flyco_core::Usd::from_micros(40_000_000)),
            clear_monthly_limit: false,
        })
        .send()
        .await;
    updated.assert_status(200);
    let view: ProviderAccountView = updated.json();
    assert_eq!(view.label, "gcp-account-1");
    assert_eq!(
        view.monthly_limit,
        Some(flyco_core::Usd::from_micros(40_000_000))
    );
    assert_eq!(view.monthly_spent, flyco_core::Usd::ZERO);

    // Zero limit is rejected.
    let zero = client
        .patch(&format!("/v1/providers/{account}"))
        .bearer(&token)
        .json(&flyco_core::UpdateProviderAccount {
            label: None,
            monthly_limit: Some(flyco_core::Usd::ZERO),
            clear_monthly_limit: false,
        })
        .send()
        .await;
    zero.assert_status(422);
    assert_eq!(
        zero.json::<Problem>().kind,
        "https://flyco.dev/problems/invalid-provider-account-update"
    );

    // Clear monthly limit.
    let cleared = client
        .patch(&format!("/v1/providers/{account}"))
        .bearer(&token)
        .json(&flyco_core::UpdateProviderAccount {
            label: None,
            monthly_limit: None,
            clear_monthly_limit: true,
        })
        .send()
        .await;
    cleared.assert_status(200);
    assert_eq!(cleared.json::<ProviderAccountView>().monthly_limit, None);
}

#[skyzen::test]
async fn per_account_monthly_quota_rolls_up_current_month_spend_and_enforces_cap(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    let first = seed_provider_account(&db, user.id).await;
    let second = seed_provider_account(&db, user.id).await;

    // Cap account 1 at $40 and account 2 at $100.
    client
        .patch(&format!("/v1/providers/{first}"))
        .bearer(&token)
        .json(&flyco_core::UpdateProviderAccount {
            label: Some("gcp-40".to_owned()),
            monthly_limit: Some(flyco_core::Usd::from_micros(40_000_000)),
            clear_monthly_limit: false,
        })
        .send()
        .await
        .assert_status(200);
    client
        .patch(&format!("/v1/providers/{second}"))
        .bearer(&token)
        .json(&flyco_core::UpdateProviderAccount {
            label: Some("gcp-100".to_owned()),
            monthly_limit: Some(flyco_core::Usd::from_micros(100_000_000)),
            clear_monthly_limit: false,
        })
        .send()
        .await
        .assert_status(200);

    // Record $40 spend in the current month on account 1 and $100 in a prior
    // month on account 2 (prior-month spend must not count toward this month).
    let s1 = seed_session(&db, &user).await;
    let s2 = seed_session(&db, &user).await;
    let spec = MachineSpec {
        provider: CloudProviderKind::Gcp,
        machine_type: "c3d-standard-32".to_owned(),
        runtime: Runtime::Vm,
        region: "us-central1".to_owned(),
        spot: true,
        disk_gib: 50,
    };
    machines::reserve(&db, s1, first, &spec)
        .await
        .expect("reserve m1");
    machines::reserve(&db, s2, second, &spec)
        .await
        .expect("reserve m2");

    let now = crate::clock::now_unix();
    let prior_month = crate::provider_accounts::utc_month_start_unix(now).saturating_sub(3_600);
    let b1: String = sql!(db, "SELECT id FROM budgets WHERE session_id = {s1}")
        .fetch_scalar()
        .await
        .expect("b1");
    let b2: String = sql!(db, "SELECT id FROM budgets WHERE session_id = {s2}")
        .fetch_scalar()
        .await
        .expect("b2");
    sql!(
        db,
        "INSERT INTO spend_events (id, budget_id, kind, amount_micros, at_unix, detail) \
         VALUES ('se_cur_1', {b1}, 'compute', 40000000, {now}, 'current month')"
    )
    .execute()
    .await
    .expect("insert current month spend");
    sql!(
        db,
        "INSERT INTO spend_events (id, budget_id, kind, amount_micros, at_unix, detail) \
         VALUES ('se_old_2', {b2}, 'compute', 100000000, {prior_month}, 'previous month')"
    )
    .execute()
    .await
    .expect("insert prior month spend");

    // GET /v1/providers reports current-month spend per account.
    let listed: Vec<ProviderAccountView> = client
        .get("/v1/providers")
        .bearer(&token)
        .send()
        .await
        .json();
    let v1 = listed.iter().find(|a| a.id == first).expect("first listed");
    let v2 = listed
        .iter()
        .find(|a| a.id == second)
        .expect("second listed");
    assert_eq!(v1.monthly_spent, flyco_core::Usd::from_micros(40_000_000));
    assert_eq!(v2.monthly_spent, flyco_core::Usd::ZERO);

    // Account 1 is exhausted and rejected; account 2 remains eligible.
    let exhausted = crate::provider_accounts::exhausted_accounts(&db, user.id, now)
        .await
        .expect("exhausted_accounts");
    assert!(exhausted.contains(&first));
    assert!(!exhausted.contains(&second));

    let err = crate::provider_accounts::check_monthly_quota(&db, user.id, first, now)
        .await
        .expect_err("capped account must be rejected");
    assert!(matches!(err, ApiError::ProviderMonthlyQuotaExceeded { .. }));
    crate::provider_accounts::check_monthly_quota(&db, user.id, second, now)
        .await
        .expect("uncapped/under-limit account must pass");
}

#[test]
fn utc_month_start_handles_leap_years_and_boundaries() {
    // 2024-02-29T23:59:59Z -> 2024-02-01T00:00:00Z
    assert_eq!(
        crate::provider_accounts::utc_month_start_unix(1_709_251_199),
        1_706_745_600
    );
    // 2026-11-25T01:00:00Z -> 2026-11-01T00:00:00Z
    assert_eq!(
        crate::provider_accounts::utc_month_start_unix(1_795_482_000),
        1_793_491_200
    );
}
