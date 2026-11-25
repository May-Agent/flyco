//! The GitHub grant after sign-in: its renewal halves are stored sealed, a
//! grant near its end is renewed before it is used, and a grant GitHub
//! will not renew is reported as revoked rather than handed out to fail.
//!
//! The sign-in round trip itself — the single-use `state`, the
//! fragment-carried session token — is covered in [`crate::oauth`]'s own
//! tests. What is asserted here is everything about the grant it leaves
//! behind, in `users` and in a linked Codespaces account.

use flyco_core::ProviderCredentials;
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::TestContext;
use skyzen_test::mock::InMemoryQueue;
use url::Url;

use crate::clock::now_unix;
use crate::github::{GithubGrant, GithubToken};
use crate::harness_accounts::REFRESH_WINDOW_SECONDS;
use crate::testing::{
    GITHUB_ACCESS_TOKEN, GITHUB_REFRESH_TOKEN, GITHUB_RENEWED_ACCESS_TOKEN,
    GITHUB_RENEWED_REFRESH_TOKEN, TestGithub, migrate, seed_expiring_codespaces_account, seed_user,
    seed_user_with_grant, test_config, test_router_with_github,
};
use crate::{provisioning, users};

/// The grant columns a test reads back, aliased so the struct's fields do
/// not share the schema's `github_` prefix.
#[derive(skyzen::FromRow)]
struct GrantRow {
    token_enc: String,
    refresh_token_enc: Option<String>,
    token_expires_at_unix: Option<u64>,
}

const GRANT_COLUMNS: &str = "github_token_enc AS token_enc, \
     github_refresh_token_enc AS refresh_token_enc, \
     github_token_expires_at_unix AS token_expires_at_unix";

/// A grant that is about to stop working, with the credential that renews
/// it — what GitHub hands back once the OAuth app expires user tokens.
fn expiring_grant() -> GithubGrant {
    GithubGrant {
        token: GithubToken {
            access_token: GITHUB_ACCESS_TOKEN.to_owned(),
        },
        refresh_token: Some(GITHUB_REFRESH_TOKEN.to_owned()),
        expires_at_unix: Some(now_unix() + 60),
    }
}

#[skyzen::test]
async fn a_sign_in_stores_the_renewal_halves_sealed(ctx: TestContext, _kv: Kv, db: Db) {
    migrate(&db).await;
    let client = ctx.client(test_router_with_github(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        TestGithub::expiring(),
    ));

    let pair = crate::crypto::pkce().expect("mint PKCE");
    let response = client
        .post("/v1/auth/github/start")
        .json(&serde_json::json!({
            "turnstile_token": "test-token",
            "code_challenge": pair.challenge,
        }))
        .send()
        .await;
    response.assert_status(200);
    let body: flyco_core::AuthorizeUrl = response.json();
    let state = Url::parse(&body.authorize_url)
        .expect("the authorize URL is absolute")
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("the authorize URL carries a state");

    client
        .get(&format!("/v1/auth/github/callback?code=abc&state={state}"))
        .send()
        .await
        .assert_status(303);

    client
        .post("/v1/auth/github/exchange")
        .json(&serde_json::json!({
            "code": "abc",
            "state": state,
            "code_verifier": pair.verifier,
        }))
        .send()
        .await
        .assert_status(200);

    let row: GrantRow = db
        .query(&format!("SELECT {GRANT_COLUMNS} FROM users"))
        .fetch_one()
        .await
        .expect("read the stored grant");

    // Neither half is ever at rest as plaintext.
    assert!(!row.token_enc.contains(GITHUB_ACCESS_TOKEN));
    let sealed_refresh = row
        .refresh_token_enc
        .expect("an expiring grant stores the credential that renews it");
    assert!(!sealed_refresh.contains(GITHUB_REFRESH_TOKEN));

    let cipher = test_config().token_cipher();
    assert_eq!(
        cipher
            .open(&sealed_refresh)
            .expect("unseal the refresh half"),
        GITHUB_REFRESH_TOKEN
    );
    assert!(
        row.token_expires_at_unix
            .expect("an expiring grant is dated")
            > now_unix() + REFRESH_WINDOW_SECONDS
    );
}

#[skyzen::test]
async fn a_grant_near_its_end_is_renewed_before_it_is_used(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user_with_grant(&db, &expiring_grant()).await;

    let token = users::github_token(&db, &test_config(), &TestGithub::expiring(), user.id)
        .await
        .expect("unseal the grant");
    assert_eq!(
        token.access_token, GITHUB_RENEWED_ACCESS_TOKEN,
        "the caller is handed the token the renewal produced"
    );

    // The rotation is persisted: GitHub rotates both halves on every
    // redemption, so what the row holds now is what it honors next.
    let user_id = user.id;
    let row: GrantRow = skyzen::sql!(
        db,
        "SELECT github_token_enc AS token_enc, \
         github_refresh_token_enc AS refresh_token_enc, \
         github_token_expires_at_unix AS token_expires_at_unix \
         FROM users WHERE id = {user_id}"
    )
    .fetch_one()
    .await
    .expect("read the rotated grant");

    let cipher = test_config().token_cipher();
    assert_eq!(
        cipher.open(&row.token_enc).expect("unseal the token"),
        GITHUB_RENEWED_ACCESS_TOKEN
    );
    assert_eq!(
        cipher
            .open(&row.refresh_token_enc.expect("the rotated half is stored"))
            .expect("unseal the refresh half"),
        GITHUB_RENEWED_REFRESH_TOKEN
    );
    assert!(
        row.token_expires_at_unix
            .is_some_and(|at| at > now_unix() + REFRESH_WINDOW_SECONDS),
        "the stored expiry follows the rotation"
    );
}

#[skyzen::test]
async fn a_grant_with_time_left_is_handed_over_untouched(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let grant = GithubGrant {
        expires_at_unix: Some(now_unix() + REFRESH_WINDOW_SECONDS + 3_600),
        ..expiring_grant()
    };
    let user = seed_user_with_grant(&db, &grant).await;

    // The fake would refuse the refresh: reaching the token anyway is the
    // proof none was attempted.
    let token = users::github_token(&db, &test_config(), &TestGithub::refused_renewal(), user.id)
        .await
        .expect("a grant with time left needs no renewal");
    assert_eq!(token.access_token, GITHUB_ACCESS_TOKEN);
}

#[skyzen::test]
async fn a_grant_github_never_expires_is_used_as_stored(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user(&db).await;

    let token = users::github_token(&db, &test_config(), &TestGithub::refused_renewal(), user.id)
        .await
        .expect("an undated grant needs no renewal");
    assert_eq!(token.access_token, GITHUB_ACCESS_TOKEN);
}

#[skyzen::test]
async fn a_refresh_github_refuses_is_revocation(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user_with_grant(&db, &expiring_grant()).await;

    let error = users::github_token(&db, &test_config(), &TestGithub::refused_renewal(), user.id)
        .await
        .expect_err("a grant that cannot be renewed is dead");

    assert_eq!(
        error.problem().kind,
        "https://flyco.dev/problems/github-token-revoked"
    );
}

#[skyzen::test]
async fn an_expired_grant_with_no_renewal_half_is_revoked(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let grant = GithubGrant {
        refresh_token: None,
        expires_at_unix: Some(now_unix() - 1),
        ..expiring_grant()
    };
    let user = seed_user_with_grant(&db, &grant).await;

    let error = users::github_token(&db, &test_config(), &TestGithub::expiring(), user.id)
        .await
        .expect_err("an expired grant with nothing to renew it is dead");
    assert_eq!(
        error.problem().kind,
        "https://flyco.dev/problems/github-token-revoked"
    );
}

#[skyzen::test]
async fn a_codespaces_grant_is_renewed_as_it_loads(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user(&db).await;
    let id = seed_expiring_codespaces_account(&db, user.id, now_unix() + 60).await;

    let linked = provisioning::account(&db, &test_config(), &TestGithub::expiring(), user.id, id)
        .await
        .expect("unseal the account");
    assert!(matches!(
        linked.credentials(),
        ProviderCredentials::Codespaces {
            token,
            refresh_token: Some(refresh_token),
            token_expires_at_unix: Some(expires_at),
            ..
        } if token == GITHUB_RENEWED_ACCESS_TOKEN
            && refresh_token == GITHUB_RENEWED_REFRESH_TOKEN
            && *expires_at > now_unix() + REFRESH_WINDOW_SECONDS
    ));

    // The rotation is persisted: loading through the other path with a
    // GitHub that would refuse a refresh still returns the renewed token —
    // the stored expiry is beyond the window, so none is attempted.
    let reloaded = provisioning::accounts_for(
        &db,
        &test_config(),
        &TestGithub::refused_renewal(),
        user.id,
        None,
    )
    .await
    .expect("reload the account");
    let reloaded = reloaded
        .into_iter()
        .find(|account| account.id == id)
        .expect("the codespaces account is listed");
    assert!(matches!(
        reloaded.credentials(),
        ProviderCredentials::Codespaces { token, .. } if token == GITHUB_RENEWED_ACCESS_TOKEN
    ));
}

#[skyzen::test]
async fn a_codespaces_grant_with_time_left_is_used_as_stored(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user(&db).await;
    let id =
        seed_expiring_codespaces_account(&db, user.id, now_unix() + REFRESH_WINDOW_SECONDS + 3_600)
            .await;

    let linked = provisioning::account(
        &db,
        &test_config(),
        &TestGithub::refused_renewal(),
        user.id,
        id,
    )
    .await
    .expect("a grant with time left needs no renewal");
    assert!(matches!(
        linked.credentials(),
        ProviderCredentials::Codespaces { token, .. } if token == GITHUB_ACCESS_TOKEN
    ));
}

#[skyzen::test]
async fn a_codespaces_grant_github_will_not_renew_is_revoked(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user(&db).await;
    let id = seed_expiring_codespaces_account(&db, user.id, now_unix() + 60).await;

    let error = provisioning::account(
        &db,
        &test_config(),
        &TestGithub::refused_renewal(),
        user.id,
        id,
    )
    .await
    .expect_err("a grant that cannot be renewed is dead");

    assert_eq!(
        error.problem().kind,
        "https://flyco.dev/problems/github-token-revoked"
    );
}

/// A [`GithubOauth`](crate::github::GithubOauth) fake where `GITHUB_REFRESH_TOKEN`
/// is strictly single-use: the first redemption rotates to
/// `GITHUB_RENEWED_REFRESH_TOKEN`, and any second attempt to redeem
/// `GITHUB_REFRESH_TOKEN` fails with `bad_refresh_token` just like GitHub's
/// live token endpoint.
#[derive(Clone, Default)]
struct SingleUseRefreshGithub {
    inner: TestGithub,
    spent_initial_refresh: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SingleUseRefreshGithub {
    fn new() -> Self {
        Self {
            inner: TestGithub::expiring_codespaces(),
            spent_initial_refresh: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl crate::github::GithubOauth for SingleUseRefreshGithub {
    fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> impl core::future::Future<Output = Result<GithubGrant, crate::github::GithubError>> + Send
    {
        self.inner
            .exchange_code(client_id, client_secret, code, redirect_uri)
    }

    fn refresh(
        &self,
        _client_id: &str,
        _client_secret: &str,
        refresh_token: &str,
    ) -> impl core::future::Future<Output = Result<GithubGrant, crate::github::GithubError>> + Send
    {
        let outcome = if refresh_token == GITHUB_REFRESH_TOKEN {
            if self
                .spent_initial_refresh
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                Err(crate::github::GithubError::Rejected {
                    code: "bad_refresh_token".to_owned(),
                    description: "The refresh token was already redeemed.".to_owned(),
                })
            } else {
                Ok(GithubGrant {
                    token: GithubToken {
                        access_token: GITHUB_RENEWED_ACCESS_TOKEN.to_owned(),
                    },
                    refresh_token: Some(GITHUB_RENEWED_REFRESH_TOKEN.to_owned()),
                    expires_at_unix: Some(now_unix() + 28_800),
                })
            }
        } else {
            Err(crate::github::GithubError::Rejected {
                code: "bad_refresh_token".to_owned(),
                description: "Unknown refresh token.".to_owned(),
            })
        };
        core::future::ready(outcome)
    }

    fn current_user(
        &self,
        token: &GithubToken,
    ) -> impl core::future::Future<
        Output = Result<crate::github::GithubIdentity, crate::github::GithubError>,
    > + Send {
        self.inner.current_user(token)
    }

    fn list_repos(
        &self,
        token: &GithubToken,
    ) -> impl core::future::Future<
        Output = Result<Vec<flyco_core::RepoSummary>, crate::github::GithubError>,
    > + Send {
        self.inner.list_repos(token)
    }

    fn get_repo(
        &self,
        token: &GithubToken,
        slug: &flyco_core::RepoSlug,
    ) -> impl core::future::Future<
        Output = Result<flyco_core::RepoSummary, crate::github::GithubError>,
    > + Send {
        self.inner.get_repo(token, slug)
    }

    fn list_branches(
        &self,
        token: &GithubToken,
        slug: &flyco_core::RepoSlug,
        page: u32,
    ) -> impl core::future::Future<
        Output = Result<crate::github::BranchListing, crate::github::GithubError>,
    > + Send {
        self.inner.list_branches(token, slug, page)
    }

    fn read_tree(
        &self,
        token: &GithubToken,
        slug: &flyco_core::RepoSlug,
        git_ref: &str,
    ) -> impl core::future::Future<
        Output = Result<crate::github::TreeListing, crate::github::GithubError>,
    > + Send {
        self.inner.read_tree(token, slug, git_ref)
    }

    fn read_file(
        &self,
        token: &GithubToken,
        slug: &flyco_core::RepoSlug,
        git_ref: &str,
        path: &str,
    ) -> impl core::future::Future<Output = Result<Vec<u8>, crate::github::GithubError>> + Send
    {
        self.inner.read_file(token, slug, git_ref, path)
    }
}

#[skyzen::test]
async fn codespaces_and_users_share_one_single_use_refresh_rotation(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    let user = seed_user_with_grant(
        &db,
        &GithubGrant {
            expires_at_unix: Some(now_unix() + 28_800),
            ..expiring_grant()
        },
    )
    .await;
    let session_token = crate::session::issue(&kv, user.id)
        .await
        .expect("issue session");
    let client = ctx.client(crate::testing::test_router_full(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        TestGithub::expiring_codespaces(),
        crate::testing::TestTurnstile::passing(),
        crate::testing::test_vendors(),
        crate::testing::TestCodespaces::succeeding(),
    ));

    // Link Codespaces directly from the sign-in grant (#294).
    let linked_resp = client
        .post("/v1/providers/codespaces/link")
        .bearer(&session_token)
        .send()
        .await;
    linked_resp.assert_status(201);
    let view: flyco_core::ProviderAccountView = linked_resp.json();

    // Advance the user's grant into the renewal window (e.g. 8 hours later).
    let due = now_unix() + 60;
    let user_id = user.id;
    skyzen::sql!(
        db,
        "UPDATE users SET github_token_expires_at_unix = {due} WHERE id = {user_id}"
    )
    .execute()
    .await
    .expect("age user grant");

    // 1. Background/compute path loads the Codespaces account first: redeems
    //    `GITHUB_REFRESH_TOKEN` once.
    let single_use = SingleUseRefreshGithub::new();
    let linked = provisioning::account(&db, &test_config(), &single_use, user.id, view.id)
        .await
        .expect("codespaces load renews shared grant");
    assert!(matches!(
        linked.credentials(),
        ProviderCredentials::Codespaces { token, .. } if token == GITHUB_RENEWED_ACCESS_TOKEN
    ));

    // 2. Repository picker (`users::github_token`) runs next against the same
    //    single-use fake: must read the already-rotated grant from `users`
    //    instead of redeeming a spent `GITHUB_REFRESH_TOKEN` and failing with 424.
    let repo_token = users::github_token(&db, &test_config(), &single_use, user.id)
        .await
        .expect("users::github_token sees the rotated grant without re-redeeming");
    assert_eq!(repo_token.access_token, GITHUB_RENEWED_ACCESS_TOKEN);
}

#[skyzen::test]
async fn legacy_split_brain_codespaces_grant_self_heals_users_without_reconnecting(
    _ctx: TestContext,
    db: Db,
) {
    migrate(&db).await;
    // Simulate a split-brain D1 state where `users` holds an expired, spent
    // grant from yesterday while `provider_accounts` already rotated to a
    // fresh 8h grant this morning.
    let user = seed_user_with_grant(
        &db,
        &GithubGrant {
            token: GithubToken {
                access_token: "ghu_stale_user_token".to_owned(),
            },
            refresh_token: Some("ghr_already_spent".to_owned()),
            expires_at_unix: Some(now_unix() - 3_600),
        },
    )
    .await;
    let fresh_expiry = now_unix() + 28_800;
    let account_id = crate::provider_accounts::create(
        &db,
        &test_config(),
        user.id,
        crate::testing::GITHUB_LOGIN.to_owned(),
        &ProviderCredentials::Codespaces {
            token: GITHUB_RENEWED_ACCESS_TOKEN.to_owned(),
            refresh_token: Some(GITHUB_RENEWED_REFRESH_TOKEN.to_owned()),
            token_expires_at_unix: Some(fresh_expiry),
            env_repo: crate::testing::CODESPACES_ENV_REPO.to_owned(),
            env_repo_id: crate::testing::CODESPACES_ENV_REPO_ID,
            owner_id: crate::testing::GITHUB_ID,
            included_core_hours: 180,
        },
        None,
    )
    .await
    .expect("seed rotated codespaces row")
    .id;

    // Even with `refused_renewal`, `users::github_token` promotes the fresh
    // grant from `provider_accounts` into `users`, clears the duplicate from
    // `provider_accounts`, and succeeds without asking GitHub to refresh.
    let healed = users::github_token(&db, &test_config(), &TestGithub::refused_renewal(), user.id)
        .await
        .expect("self-heal split-brain grant");
    assert_eq!(healed.access_token, GITHUB_RENEWED_ACCESS_TOKEN);

    let sealed: String = skyzen::sql!(
        db,
        "SELECT credentials_enc FROM provider_accounts WHERE id = {account_id}"
    )
    .fetch_scalar()
    .await
    .expect("read scrubbed provider_accounts row");
    let stored: crate::provider_accounts::StoredSecrets = serde_json::from_str(
        &test_config()
            .token_cipher()
            .open(&sealed)
            .expect("unseal provider row"),
    )
    .expect("decode stored secrets");
    assert!(matches!(
        stored.credentials,
        ProviderCredentials::Codespaces {
            refresh_token: None,
            token_expires_at_unix: None,
            ..
        }
    ));
}

#[skyzen::test]
async fn a_401_outside_the_refresh_window_renews_once_and_serves_repositories(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    // Two hours left on the clock (well outside the 30-minute refresh window),
    // but GitHub already rejects `GITHUB_ACCESS_TOKEN` with `401`.
    let user = seed_user_with_grant(
        &db,
        &GithubGrant {
            expires_at_unix: Some(now_unix() + 7_200),
            ..expiring_grant()
        },
    )
    .await;
    let session_token = crate::session::issue(&kv, user.id)
        .await
        .expect("issue session");
    let client = ctx.client(test_router_with_github(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        TestGithub::stale_access_token(),
    ));

    let response = client
        .get("/v1/github/repos")
        .bearer(&session_token)
        .send()
        .await;
    response.assert_status(200);
    let repos: Vec<flyco_core::RepoSummary> = response.json();
    assert_eq!(repos.len(), 2);
}
