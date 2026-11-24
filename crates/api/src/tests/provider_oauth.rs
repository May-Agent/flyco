//! Linking a cloud account by signing in with it, end to end through the
//! router.
//!
//! The wire format of the vendor calls is pinned separately, against
//! recorded exchanges, in [`crate::microsoft`] and [`crate::google`]. What is
//! asserted here is everything around them: that the authorize URL carries
//! this deployment's own client, callback and PKCE challenge, that the public
//! callback forwards parameters in the fragment without exchanging or setting
//! cookies, that the exchange requires the initiating user's Bearer session
//! and matching `state`, that a state redeems once, that a poll is not a
//! consumption, and that the finish ends in the same store path
//! `POST /v1/providers` does.

use flyco_core::{
    CloudProviderKind, CurrentUser, FinishAzureOauth, FinishGcpOauth, Problem, ProviderAccountView,
    ProviderOauthProgress, ProviderOauthStart,
};
use serde_json::json;
use skyzen::routing::Router;
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::mock::InMemoryQueue;
use skyzen_test::{TestClient, TestContext, TestResponse};
use url::Url;

use crate::anthropic::ClaudeClient;
use crate::devin::DevinClient;
use crate::google::GoogleClient;
use crate::microsoft::MicrosoftClient;
use crate::openai::CodexClient;
use crate::session;
use crate::testing::{
    AZURE_APP_CLIENT_SECRET, AZURE_CLIENT_ID, AZURE_SUBSCRIPTION_ID, AZURE_SUBSCRIPTION_NAME,
    CLOUD_CODE, GCP_ACCOUNT, GCP_PROJECT_ID, GCP_PROJECT_NAME, GCP_SERVICE_ACCOUNT_JSON,
    GOOGLE_CLIENT_ID, REDIRECT_URI, TestClaude, TestCodex, TestDevin, TestGithub, TestGoogle,
    TestMicrosoft, migrate, seed_other_user, seed_user, test_router_with,
};
use crate::vendors::Vendors;

const AZURE_START: &str = "/v1/providers/azure/oauth/start";
const GCP_START: &str = "/v1/providers/gcp/oauth/start";
const PROVIDERS: &str = "/v1/providers";

fn exchange_path(provider: &str) -> String {
    format!("/v1/providers/{provider}/oauth/exchange")
}

fn poll_path(provider: &str, started: &ProviderOauthStart) -> String {
    format!("/v1/providers/{provider}/oauth/{}", started.attempt_id)
}

fn finish_path(provider: &str, started: &ProviderOauthStart) -> String {
    format!(
        "/v1/providers/{provider}/oauth/{}/finish",
        started.attempt_id
    )
}

fn callback_path(provider: &str, code: &str, state: &str) -> String {
    format!("/v1/providers/{provider}/oauth/callback?code={code}&state={state}")
}

/// A signed-in caller and the router they call, with both cloud vendors
/// standing in for whatever this test needs them to be.
async fn signed_in_with(
    ctx: &TestContext,
    kv: &Kv,
    db: &Db,
    microsoft: TestMicrosoft,
    google: TestGoogle,
) -> (TestClient<Router>, CurrentUser, String) {
    migrate(db).await;
    let router = test_router_with(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        TestGithub::default(),
        Vendors::new(
            ClaudeClient::Fake(TestClaude),
            CodexClient::Fake(TestCodex::approved()),
            MicrosoftClient::Fake(microsoft),
            GoogleClient::Fake(google),
            DevinClient::Fake(TestDevin),
        ),
    );
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    (ctx.client(router), user, token)
}

/// The same, with both vendors signing the user in.
async fn signed_in(
    ctx: &TestContext,
    kv: &Kv,
    db: &Db,
) -> (TestClient<Router>, CurrentUser, String) {
    signed_in_with(
        ctx,
        kv,
        db,
        TestMicrosoft::succeeding(),
        TestGoogle::succeeding(),
    )
    .await
}

/// Begins a sign-in and hands back what the page was given.
async fn begin(client: &TestClient<Router>, token: &str, path: &str) -> ProviderOauthStart {
    let response = client.post(path).bearer(token).send().await;
    response.assert_status(200);
    response.json()
}

/// Completes the authenticated exchange step for `started`.
async fn exchange_ok(
    client: &TestClient<Router>,
    token: &str,
    provider: &str,
    started: &ProviderOauthStart,
    code: &str,
) -> ProviderOauthProgress {
    let response = client
        .post(&exchange_path(provider))
        .bearer(token)
        .json(&json!({
            "attempt_id": started.attempt_id,
            "state": state_of(started),
            "code": code,
        }))
        .send()
        .await;
    response.assert_status(200);
    assert!(response.headers().get("set-cookie").is_none());
    response.json()
}

/// The `state` an authorize URL published.
fn state_of(started: &ProviderOauthStart) -> String {
    Url::parse(&started.authorize_url)
        .expect("the authorize URL is absolute")
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("the authorize URL carries a state")
}

/// Where a `303` sent the browser.
fn location(response: &TestResponse) -> Url {
    let raw = response
        .headers()
        .get("location")
        .expect("the response redirects")
        .to_str()
        .expect("the location is ASCII");
    Url::parse(raw).expect("the location is an absolute URL")
}

/// One query parameter of a URL.
fn query(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

/// One fragment parameter of a URL.
fn fragment_param(url: &Url, name: &str) -> Option<String> {
    let fragment = url.fragment()?;
    url::form_urlencoded::parse(fragment.as_bytes())
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

// ── Starting ──

#[skyzen::test]
async fn an_azure_sign_in_starts_at_microsoft_with_this_deployments_own_client_and_pkce(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let url = Url::parse(&started.authorize_url).expect("the authorize URL is absolute");
    assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
    assert_eq!(
        query(&url, "client_id").as_deref(),
        Some(AZURE_CLIENT_ID),
        "the URL presents the client this deployment registered"
    );

    let redirect = query(&url, "redirect_uri").expect("the URL names a callback");
    assert_eq!(
        redirect, "https://flyco.test/v1/providers/azure/oauth/callback",
        "the callback shares the configured origin, so the browser comes back to flyco"
    );

    let scope = query(&url, "scope").expect("the URL names its scopes");
    assert!(
        scope.contains("management.azure.com"),
        "listing subscriptions and assigning a role need ARM: {scope}"
    );
    assert!(
        scope.contains("graph.microsoft.com"),
        "creating the service principal needs Graph: {scope}"
    );
    assert_eq!(
        query(&url, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert_eq!(
        query(&url, "code_challenge").map(|c| c.len()),
        Some(43),
        "S256 PKCE challenge is 43 base64url chars"
    );
    assert_ne!(state_of(&started), "");
}

#[skyzen::test]
async fn a_gcp_sign_in_starts_at_google_with_this_deployments_own_client_and_pkce(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;

    let url = Url::parse(&started.authorize_url).expect("the authorize URL is absolute");
    assert_eq!(url.host_str(), Some("accounts.google.com"));
    assert_eq!(query(&url, "client_id").as_deref(), Some(GOOGLE_CLIENT_ID));
    assert_eq!(
        query(&url, "redirect_uri").as_deref(),
        Some("https://flyco.test/v1/providers/gcp/oauth/callback")
    );
    assert!(
        query(&url, "scope")
            .expect("the URL names its scopes")
            .contains("cloud-platform")
    );
    assert_eq!(
        query(&url, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert_eq!(query(&url, "code_challenge").map(|c| c.len()), Some(43));
}

#[skyzen::test]
async fn a_sign_in_cannot_be_started_without_a_flyco_session(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, _token) = signed_in(&ctx, &kv, &db).await;
    client.post(AZURE_START).send().await.assert_status(401);
}

// ── The callback (forwarding only, zero exchange, zero cookies) ──

#[skyzen::test]
async fn callback_forwards_code_and_state_in_fragment_without_exchanging_or_setting_cookies(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;
    let state = state_of(&started);

    let response = client
        .get(&callback_path("azure", CLOUD_CODE, &state))
        .send()
        .await;
    response.assert_status(303);
    assert!(
        response.headers().get("set-cookie").is_none(),
        "flyco never sets cookies on OAuth callbacks"
    );

    let landed = location(&response);
    let origin = Url::parse(REDIRECT_URI)
        .expect("the test redirect URI is absolute")
        .origin();
    assert_eq!(landed.origin(), origin);
    assert_eq!(landed.path(), "/connect/return");
    assert!(
        landed.query().is_none(),
        "code and state must ride the fragment, never the query string"
    );
    assert_eq!(
        fragment_param(&landed, "provider").as_deref(),
        Some("azure")
    );
    assert_eq!(fragment_param(&landed, "code").as_deref(), Some(CLOUD_CODE));
    assert_eq!(
        fragment_param(&landed, "state").as_deref(),
        Some(state.as_str())
    );

    // Crucially, the unauthenticated GET did NOT exchange the code: the
    // attempt is still Pending until the signed-in SPA calls POST .../exchange.
    let polled = client
        .get(&poll_path("azure", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(200);
    assert_eq!(
        polled.json::<ProviderOauthProgress>(),
        ProviderOauthProgress::Pending
    );
}

#[skyzen::test]
async fn callback_forwards_vendor_refusal_in_fragment(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;
    let state = state_of(&started);

    let response = client
        .get(&format!(
            "/v1/providers/gcp/oauth/callback?error=access_denied&error_description=denied&state={state}"
        ))
        .send()
        .await;
    response.assert_status(303);
    let landed = location(&response);
    assert_eq!(landed.path(), "/connect/return");
    assert!(landed.query().is_none());
    assert_eq!(fragment_param(&landed, "provider").as_deref(), Some("gcp"));
    assert_eq!(
        fragment_param(&landed, "error").as_deref(),
        Some("access_denied")
    );
    assert_eq!(
        fragment_param(&landed, "error_description").as_deref(),
        Some("denied")
    );
    assert_eq!(
        fragment_param(&landed, "state").as_deref(),
        Some(state.as_str())
    );
}

// ── Authenticated exchange & CSRF defence ──

#[skyzen::test]
async fn exchange_requires_a_flyco_session(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    client
        .post(&exchange_path("azure"))
        .json(&json!({
            "attempt_id": started.attempt_id,
            "state": state_of(&started),
            "code": CLOUD_CODE,
        }))
        .send()
        .await
        .assert_status(401);
}

#[skyzen::test]
async fn cross_account_callback_replay_is_rejected_and_leaves_the_attempt_untouched(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, attacker_token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &attacker_token, AZURE_START).await;

    let victim = seed_other_user(&db).await;
    let victim_token = session::issue(&kv, victim.id)
        .await
        .expect("issue a session for the other user");

    // Victim's browser attempts to redeem a callback carrying attacker's
    // attempt_id and state: must be refused because attempt.user != victim.
    let replayed = client
        .post(&exchange_path("azure"))
        .bearer(&victim_token)
        .json(&json!({
            "attempt_id": started.attempt_id,
            "state": state_of(&started),
            "code": CLOUD_CODE,
        }))
        .send()
        .await;
    replayed.assert_status(404);
    assert_eq!(
        replayed.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );

    // Attacker's attempt never received victim's grant.
    let polled = client
        .get(&poll_path("azure", &started))
        .bearer(&attacker_token)
        .send()
        .await;
    polled.assert_status(200);
    assert_eq!(
        polled.json::<ProviderOauthProgress>(),
        ProviderOauthProgress::Pending
    );
}

#[skyzen::test]
async fn exchange_with_a_mismatched_state_is_rejected(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let response = client
        .post(&exchange_path("azure"))
        .bearer(&token)
        .json(&json!({
            "attempt_id": started.attempt_id,
            "state": "wrong-state",
            "code": CLOUD_CODE,
        }))
        .send()
        .await;
    response.assert_status(400);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/unknown-oauth-state"
    );
}

#[skyzen::test]
async fn one_vendors_attempt_cannot_be_exchanged_on_the_other_vendors_route(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let response = client
        .post(&exchange_path("gcp"))
        .bearer(&token)
        .json(&json!({
            "attempt_id": started.attempt_id,
            "state": state_of(&started),
            "code": CLOUD_CODE,
        }))
        .send()
        .await;
    response.assert_status(404);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn an_attempt_exchanges_exactly_once(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    exchange_ok(&client, &token, "azure", &started, CLOUD_CODE).await;

    let again = client
        .post(&exchange_path("azure"))
        .bearer(&token)
        .json(&json!({
            "attempt_id": started.attempt_id,
            "state": state_of(&started),
            "code": CLOUD_CODE,
        }))
        .send()
        .await;
    again.assert_status(400);
    assert_eq!(
        again.json::<Problem>().kind,
        "https://flyco.dev/problems/unknown-oauth-state"
    );
}

#[skyzen::test]
async fn a_vendor_refusal_during_exchange_records_failure_for_poll(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in_with(
        &ctx,
        &kv,
        &db,
        TestMicrosoft::refusing(),
        TestGoogle::succeeding(),
    )
    .await;
    let started = begin(&client, &token, AZURE_START).await;

    let response = client
        .post(&exchange_path("azure"))
        .bearer(&token)
        .json(&json!({
            "attempt_id": started.attempt_id,
            "state": state_of(&started),
            "code": CLOUD_CODE,
        }))
        .send()
        .await;
    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/microsoft-rejected"
    );

    let polled = client
        .get(&poll_path("azure", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(200);
    match polled.json::<ProviderOauthProgress>() {
        ProviderOauthProgress::Failed { problem, .. } => {
            assert_eq!(problem, "microsoft-rejected");
        }
        other => panic!("expected Failed on poll, got {other:?}"),
    }
}

#[skyzen::test]
async fn a_declined_consent_posted_to_exchange_records_failure_for_poll(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;

    let response = client
        .post(&exchange_path("gcp"))
        .bearer(&token)
        .json(&json!({
            "attempt_id": started.attempt_id,
            "state": state_of(&started),
            "error": "access_denied",
            "error_description": "denied",
        }))
        .send()
        .await;
    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/google-rejected"
    );

    let polled = client
        .get(&poll_path("gcp", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(200);
    match polled.json::<ProviderOauthProgress>() {
        ProviderOauthProgress::Failed { problem, reason } => {
            assert_eq!(problem, "google-rejected");
            assert!(reason.contains("denied"), "reason: {reason}");
        }
        other => panic!("expected Failed on poll, got {other:?}"),
    }
}

// ── Polling ──

#[skyzen::test]
async fn a_poll_before_the_exchange_is_pending(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    for _ in 0..3 {
        let response = client
            .get(&poll_path("azure", &started))
            .bearer(&token)
            .send()
            .await;
        response.assert_status(200);
        assert_eq!(
            response.json::<ProviderOauthProgress>(),
            ProviderOauthProgress::Pending
        );
    }
}

#[skyzen::test]
async fn one_users_sign_in_is_not_another_users_to_poll(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let other = seed_other_user(&db).await;
    let other_token = session::issue(&kv, other.id)
        .await
        .expect("issue a session for the other user");

    let stolen = client
        .get(&poll_path("azure", &started))
        .bearer(&other_token)
        .send()
        .await;
    stolen.assert_status(404);
    assert_eq!(
        stolen.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );

    // And the owner's own sign-in is untouched by the refusal.
    client
        .get(&poll_path("azure", &started))
        .bearer(&token)
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn an_unknown_attempt_is_indistinguishable_from_an_expired_one(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;

    let response = client
        .get("/v1/providers/azure/oauth/11111111-2222-4333-8444-555555555555")
        .bearer(&token)
        .send()
        .await;
    response.assert_status(404);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );
}

// ── Finishing ──

#[skyzen::test]
async fn finishing_before_the_exchange_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let response = client
        .post(&finish_path("azure", &started))
        .bearer(&token)
        .json(&FinishAzureOauth {
            subscription_id: AZURE_SUBSCRIPTION_ID.to_owned(),
        })
        .send()
        .await;

    response.assert_status(409);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-not-authorized"
    );
}

#[skyzen::test]
async fn finishing_an_azure_sign_in_links_the_subscription_the_user_chose(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;
    let exchanged = exchange_ok(&client, &token, "azure", &started, CLOUD_CODE).await;
    assert_eq!(
        exchanged,
        ProviderOauthProgress::Authorized {
            account: "me@lexo.cool".to_owned(),
            choices: vec![flyco_core::ProviderOauthChoice {
                id: AZURE_SUBSCRIPTION_ID.to_owned(),
                name: AZURE_SUBSCRIPTION_NAME.to_owned(),
            }],
        }
    );

    let polled = client
        .get(&poll_path("azure", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(200);
    assert_eq!(polled.json::<ProviderOauthProgress>(), exchanged);

    let response = client
        .post(&finish_path("azure", &started))
        .bearer(&token)
        .json(&FinishAzureOauth {
            subscription_id: AZURE_SUBSCRIPTION_ID.to_owned(),
        })
        .send()
        .await;

    response.assert_status(201);
    let view: ProviderAccountView = response.json();
    assert_eq!(view.kind, CloudProviderKind::Azure);
    assert_eq!(
        view.label, AZURE_SUBSCRIPTION_NAME,
        "the card is named after the subscription the user recognised"
    );

    // The same list `POST /v1/providers` writes into.
    let listed = client.get(PROVIDERS).bearer(&token).send().await;
    listed.assert_status(200);
    let accounts: Vec<ProviderAccountView> = listed.json();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, view.id);

    // The service principal flyco just created is sealed like any other
    // credential: the client secret never reaches the database in the clear.
    let user_id = user.id;
    let sealed: String = skyzen::sql!(
        db,
        "SELECT credentials_enc FROM provider_accounts WHERE user_id = {user_id}"
    )
    .fetch_scalar()
    .await
    .expect("read the sealed credential");
    assert!(!sealed.contains(AZURE_APP_CLIENT_SECRET));

    // And the sign-in is spent.
    let polled = client
        .get(&poll_path("azure", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(404);
    assert_eq!(
        polled.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn finishing_a_gcp_sign_in_links_the_project_the_user_chose(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;
    exchange_ok(&client, &token, "gcp", &started, CLOUD_CODE).await;

    let polled = client
        .get(&poll_path("gcp", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(200);
    assert_eq!(
        polled.json::<ProviderOauthProgress>(),
        ProviderOauthProgress::Authorized {
            account: GCP_ACCOUNT.to_owned(),
            choices: vec![flyco_core::ProviderOauthChoice {
                id: GCP_PROJECT_ID.to_owned(),
                name: GCP_PROJECT_NAME.to_owned(),
            }],
        }
    );

    let response = client
        .post(&finish_path("gcp", &started))
        .bearer(&token)
        .json(&FinishGcpOauth {
            project_id: GCP_PROJECT_ID.to_owned(),
        })
        .send()
        .await;

    response.assert_status(201);
    let view: ProviderAccountView = response.json();
    assert_eq!(view.kind, CloudProviderKind::Gcp);
    assert_eq!(view.label, GCP_PROJECT_NAME);

    let listed = client.get(PROVIDERS).bearer(&token).send().await;
    listed.assert_status(200);
    assert_eq!(listed.json::<Vec<ProviderAccountView>>().len(), 1);

    // The minted service-account key is a private key: it is sealed before
    // the database sees it, exactly as a pasted one would be.
    let user_id = user.id;
    let sealed: String = skyzen::sql!(
        db,
        "SELECT credentials_enc FROM provider_accounts WHERE user_id = {user_id}"
    )
    .fetch_scalar()
    .await
    .expect("read the sealed credential");
    assert!(!sealed.contains("BEGIN PRIVATE KEY"));
    assert!(!sealed.contains(GCP_SERVICE_ACCOUNT_JSON.trim()));
}

#[skyzen::test]
async fn a_finished_sign_in_cannot_be_finished_again(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;
    exchange_ok(&client, &token, "gcp", &started, CLOUD_CODE).await;

    let finish = finish_path("gcp", &started);
    let body = FinishGcpOauth {
        project_id: GCP_PROJECT_ID.to_owned(),
    };

    client
        .post(&finish)
        .bearer(&token)
        .json(&body)
        .send()
        .await
        .assert_status(201);

    let again = client.post(&finish).bearer(&token).json(&body).send().await;
    again.assert_status(404);
    assert_eq!(
        again.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn one_users_sign_in_is_not_another_users_to_finish(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;
    exchange_ok(&client, &token, "gcp", &started, CLOUD_CODE).await;

    let other = seed_other_user(&db).await;
    let other_token = session::issue(&kv, other.id)
        .await
        .expect("issue a session for the other user");

    let stolen = client
        .post(&finish_path("gcp", &started))
        .bearer(&other_token)
        .json(&FinishGcpOauth {
            project_id: GCP_PROJECT_ID.to_owned(),
        })
        .send()
        .await;
    stolen.assert_status(404);
    assert_eq!(
        stolen.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );

    // The owner can still finish: the refusal spent nothing.
    client
        .post(&finish_path("gcp", &started))
        .bearer(&token)
        .json(&FinishGcpOauth {
            project_id: GCP_PROJECT_ID.to_owned(),
        })
        .send()
        .await
        .assert_status(201);
}
