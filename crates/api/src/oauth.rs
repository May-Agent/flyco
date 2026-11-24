//! GitHub OAuth: the three endpoints that turn a browser into a flyco session.
//!
//! Flyco has no password of its own and sets no cookies. Before starting, the
//! SPA generates a per-tab PKCE verifier in `sessionStorage` and sends its
//! S256 `code_challenge` to [`start`], which verifies Turnstile, mints a
//! single-use `state`, and returns GitHub's authorize URL. GitHub returns the
//! browser to [`callback`], which performs no exchange and mints no session —
//! it forwards `code` and `state` in the URL fragment to `/auth/complete`,
//! keeping them out of server logs and `Referer` headers. The SPA then checks
//! `state` against `sessionStorage` and calls [`exchange`] with the stored
//! `code_verifier`, proving the browser that started sign-in is the one
//! redeeming it.
//!
//! [`callback`] is also where a Codespaces link comes back: the OAuth app
//! registers one URI per hostname, so the two GitHub flows share this path and
//! disjoint `state` prefixes decide whether the browser is forwarded to
//! `/connect/return` or `/auth/complete`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use flyco_core::AuthorizeUrl;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use skyzen::extract::Query;
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv};
use url::Url;

use crate::config::ApiConfig;
use crate::crypto::prefixed_token;
use crate::error::ApiError;
use crate::extract::Headers;
use crate::github::{GithubClient, GithubOauth as _, SCOPE};
use crate::problem::Outcome;
use crate::provider_oauth::{self, ProviderCallback};
use crate::respond::SeeOther;
use crate::turnstile::{SiteVerify as _, TurnstileClient};
use crate::{expiring, session, users};

/// Where the browser is sent to approve the OAuth app.
const AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";

/// How long a browser has to complete the round trip.
const STATE_TTL_SECONDS: u64 = 10 * 60;

/// Prefix that distinguishes sign-in states from Codespaces link states on the
/// shared GitHub callback without reading KV.
const STATE_PREFIX: &str = "gh_";

/// Exact length of an unpadded base64url-encoded 32-byte SHA-256 digest.
const S256_CHALLENGE_LEN: usize = 43;

/// Minimum length of an RFC 7636 PKCE verifier.
const PKCE_VERIFIER_MIN_LEN: usize = 43;

/// Maximum length of an RFC 7636 PKCE verifier.
const PKCE_VERIFIER_MAX_LEN: usize = 128;

/// Where the browser lands to complete sign-in. Resolved against the
/// configured redirect URI, so it always shares the callback's origin.
const POST_LOGIN_PATH: &str = "/auth/complete";

/// Constant-time equality for opaque secrets and digests.
pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (&a, &b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Validates that `challenge` is a 43-character unpadded base64url S256
/// digest (32 decoded bytes).
fn valid_code_challenge(challenge: &str) -> bool {
    challenge.len() == S256_CHALLENGE_LEN
        && challenge
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        && BASE64URL
            .decode(challenge)
            .is_ok_and(|decoded| decoded.len() == 32)
}

/// Verifies that `verifier` hashes under S256 to `expected_challenge`.
fn verify_pkce_s256(verifier: &str, expected_challenge: &str) -> bool {
    if !(PKCE_VERIFIER_MIN_LEN..=PKCE_VERIFIER_MAX_LEN).contains(&verifier.len()) {
        return false;
    }
    let computed = BASE64URL.encode(Sha256::digest(verifier.as_bytes()));
    constant_time_eq(computed.as_bytes(), expected_challenge.as_bytes())
}

/// GitHub's authorize endpoint as a parsed URL.
fn authorize_endpoint() -> Url {
    Url::parse(AUTHORIZE_URL).expect("the GitHub authorize URL is a valid absolute URL")
}

/// GitHub's authorize URL for one redirect URI and scope set.
///
/// The sign-in and the Codespaces link both begin at the same endpoint and
/// return to the same callback; what differs between them is the prefixed
/// `state` minted for each and what the token is granted.
pub(crate) fn authorize_url(client_id: &str, redirect_uri: &Url, scope: &str, state: &str) -> Url {
    let mut authorize_url = authorize_endpoint();
    authorize_url
        .query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri.as_str())
        .append_pair("scope", scope)
        .append_pair("state", state);
    authorize_url
}

fn state_key(state: &str) -> String {
    let mut key = String::with_capacity(17 + state.len());
    key.push_str("auth:oauth-state:");
    key.push_str(state);
    key
}

/// What KV holds between [`start`] and [`exchange`].
#[derive(Debug, Serialize, Deserialize)]
struct PendingSignIn {
    /// The browser's S256 PKCE challenge; [`exchange`] insists on its matching
    /// verifier so a callback cannot be replayed into another browser.
    code_challenge: String,
}

/// What `POST /v1/auth/github/start` must carry.
///
/// This is the one public route whose whole job is minting account state,
/// which makes it the one a registration bot wants — so the request carries
/// both human proof and the initiating browser's PKCE challenge.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct StartGithubLogin {
    /// The `cf-turnstile-response` token the login page's widget produced.
    turnstile_token: String,
    /// Base64url-encoded SHA-256 (`S256`) challenge of the verifier the
    /// initiating browser holds in `sessionStorage`.
    code_challenge: String,
}

/// What `POST /v1/auth/github/exchange` must carry.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct ExchangeGithubLogin {
    /// The single-use authorization code GitHub returned to the callback.
    pub code: String,
    /// The `state` minted by [`start`] and echoed back by GitHub.
    pub state: String,
    /// The `sessionStorage`-held PKCE verifier matching the `code_challenge`
    /// sent to [`start`].
    pub code_verifier: String,
}

/// What `POST /v1/auth/github/exchange` answers with on success.
///
/// The plaintext session token is returned once over JSON to the browser tab
/// that initiated sign-in; flyco sets no cookies.
#[derive(Serialize, Deserialize, skyzen::ToSchema)]
pub struct GithubSession {
    /// The freshly minted `fs_` browser session token.
    pub token: String,
}

impl core::fmt::Debug for GithubSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GithubSession").finish_non_exhaustive()
    }
}

/// Begins a GitHub sign-in, returning the URL to send the browser to.
///
/// The `code_challenge` shape and the Turnstile token are verified *before*
/// any state is minted: a bot or malformed caller gets no OAuth `state`, no KV
/// write, and no path to the exchange.
///
/// Per-call cost: 0 D1 statements, 1 KV write (`auth:oauth-state:{state}`),
/// 0 Durable Object calls, 0 R2 operations, 1 external subrequest (Turnstile
/// siteverify).
#[skyzen::openapi]
pub async fn start(
    State(config): State<ApiConfig>,
    State(verifier): State<TurnstileClient>,
    headers: Headers,
    kv: Kv,
    Json(request): Json<StartGithubLogin>,
) -> Outcome<Json<AuthorizeUrl>> {
    begin(&config, &verifier, &headers, &kv, &request)
        .await
        .into()
}

async fn begin(
    config: &ApiConfig,
    verifier: &TurnstileClient,
    headers: &Headers,
    kv: &Kv,
    request: &StartGithubLogin,
) -> Result<Json<AuthorizeUrl>, ApiError> {
    if !valid_code_challenge(&request.code_challenge) {
        return Err(ApiError::UnknownOauthState);
    }

    let verification = verifier
        .verify(
            config.turnstile_secret(),
            &request.turnstile_token,
            crate::turnstile::remote_ip(headers),
        )
        .await?;
    verification.evaluate(config.turnstile_hostnames())?;

    let state = prefixed_token(STATE_PREFIX)?;
    expiring::put(
        kv,
        &state_key(&state),
        &PendingSignIn {
            code_challenge: request.code_challenge.clone(),
        },
        STATE_TTL_SECONDS,
    )
    .await?;

    let authorize_url = authorize_url(
        config.github_client_id(),
        config.redirect_uri(),
        SCOPE,
        &state,
    );

    tracing::debug!("issued a GitHub authorize URL");
    Ok(Json(AuthorizeUrl {
        authorize_url: authorize_url.into(),
    }))
}

/// `GET /v1/auth/github/callback` — forwards the returning browser to the SPA
/// with the OAuth parameters in the URL fragment.
///
/// Never exchanges the code, never consumes state, never mints a session
/// token, and sets no cookies: the SPA validates `state` against the
/// initiating browser's storage and redeems the code via an explicit `POST`.
///
/// Per-call cost: 0 D1 statements, 0 KV operations, 0 Durable Object calls,
/// 0 R2 operations, 0 subrequests.
#[skyzen::openapi]
pub async fn callback(
    Query(callback): Query<ProviderCallback>,
    State(config): State<ApiConfig>,
) -> Outcome<SeeOther> {
    if provider_oauth::is_codespaces_state(&callback.state) {
        return Ok(provider_oauth::forward_codespaces_callback(
            &config, &callback,
        ))
        .into();
    }
    Ok(SeeOther(completion_url(&config, &callback))).into()
}

/// `POST /v1/auth/github/exchange` — validates the browser's `state` and PKCE
/// verifier, exchanges the GitHub code, upserts the account, and mints an
/// `fs_` session token.
///
/// Per-call cost: 1 KV read + 2 KV writes (`take` state + `session::issue`),
/// 1 D1 upsert (`users::upsert_from_github`), 0 Durable Object calls, 0 R2
/// operations, 2 external subrequests to GitHub (`access_token` + `GET /user`).
#[skyzen::openapi]
pub async fn exchange(
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    kv: Kv,
    db: Db,
    Json(request): Json<ExchangeGithubLogin>,
) -> Outcome<Json<GithubSession>> {
    redeem(&config, &github, &kv, &db, &request)
        .await
        .map(Json)
        .into()
}

async fn redeem(
    config: &ApiConfig,
    github: &GithubClient,
    kv: &Kv,
    db: &Db,
    request: &ExchangeGithubLogin,
) -> Result<GithubSession, ApiError> {
    if request.code.is_empty() || request.state.is_empty() || request.code_verifier.is_empty() {
        return Err(ApiError::UnknownOauthState);
    }

    let Some(pending) = expiring::take::<PendingSignIn>(kv, &state_key(&request.state)).await?
    else {
        return Err(ApiError::UnknownOauthState);
    };

    if !verify_pkce_s256(&request.code_verifier, &pending.code_challenge) {
        return Err(ApiError::UnknownOauthState);
    }

    let grant = github
        .exchange_code(
            config.github_client_id(),
            config.github_client_secret(),
            &request.code,
            config.redirect_uri().as_str(),
        )
        .await?;
    // What the token may *do* is read fresh wherever it matters — a scope
    // can be narrowed after the fact, and a copy in D1 would say it still
    // holds.
    let account = github.current_user(&grant.token).await?;

    let user = users::upsert_from_github(db, config, &account.user, &grant).await?;
    let session_token = session::issue(kv, user.id).await?;

    tracing::info!(login = %user.login, "completed a GitHub sign-in");
    Ok(GithubSession {
        token: session_token,
    })
}

/// Where the browser is sent once GitHub returns: the SPA's completion route
/// on the callback's own origin, with the callback parameters in the fragment.
fn completion_url(config: &ApiConfig, callback: &ProviderCallback) -> Url {
    let mut url = config
        .redirect_uri()
        .join(POST_LOGIN_PATH)
        .expect("a rooted path always resolves against an absolute redirect URI");
    let mut fragment = url::form_urlencoded::Serializer::new(String::new());
    if let Some(error) = callback.error_raw() {
        fragment.append_pair("error", error);
        if let Some(description) = callback.error_description_raw() {
            fragment.append_pair("error_description", description);
        }
    } else if let Some(code) = callback.code_raw() {
        fragment.append_pair("code", code);
    } else {
        fragment.append_pair("error", "missing_code");
    }
    fragment.append_pair("state", &callback.state);
    url.set_fragment(Some(&fragment.finish()));
    url
}

#[cfg(test)]
mod tests {
    use flyco_core::AuthorizeUrl;
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;
    use url::Url;

    use super::{GithubSession, PendingSignIn, state_key};
    use crate::crypto::pkce;
    use crate::github::SCOPE;
    use crate::testing::{
        CLIENT_ID, GITHUB_ACCESS_TOKEN, GITHUB_ID, GITHUB_LOGIN, REDIRECT_URI, migrated_router,
        test_config,
    };
    use crate::{expiring, session, users};

    async fn begin(client: &skyzen_test::TestClient<skyzen::routing::Router>) -> (String, String) {
        let pair = pkce().expect("mint PKCE");
        let response = client
            .post("/v1/auth/github/start")
            .json(&serde_json::json!({
                "turnstile_token": "test-token",
                "code_challenge": pair.challenge,
            }))
            .send()
            .await;
        response.assert_status(200);
        let body: AuthorizeUrl = response.json();

        let url = Url::parse(&body.authorize_url).expect("the authorize URL is absolute");
        assert_eq!(url.host_str(), Some("github.com"));

        let query: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert!(query.contains(&("client_id".to_owned(), CLIENT_ID.to_owned())));
        assert!(query.contains(&("scope".to_owned(), SCOPE.to_owned())));
        assert!(query.contains(&("redirect_uri".to_owned(), REDIRECT_URI.to_owned())));

        let state = query
            .into_iter()
            .find_map(|(key, value)| (key == "state").then_some(value))
            .expect("the authorize URL carries a state");
        (state, pair.verifier)
    }

    #[skyzen::test]
    async fn start_mints_a_state_and_records_the_challenge(ctx: TestContext, kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let (state, _verifier) = begin(&client).await;

        assert!(
            expiring::get::<PendingSignIn>(&kv, &state_key(&state))
                .await
                .expect("read the state")
                .is_some()
        );
    }

    #[skyzen::test]
    async fn start_rejects_a_malformed_code_challenge(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let response = client
            .post("/v1/auth/github/start")
            .json(&serde_json::json!({
                "turnstile_token": "test-token",
                "code_challenge": "too-short",
            }))
            .send()
            .await;

        response.assert_status(400);
        assert_eq!(
            response.json::<flyco_core::Problem>().kind,
            "https://flyco.dev/problems/unknown-oauth-state"
        );
    }

    #[skyzen::test]
    async fn callback_forwards_code_and_state_in_fragment_without_minting_a_session(
        ctx: TestContext,
        kv: Kv,
        db: Db,
    ) {
        let client = ctx.client(migrated_router(&db).await);
        let (state, _verifier) = begin(&client).await;

        let response = client
            .get(&format!("/v1/auth/github/callback?code=abc&state={state}"))
            .send()
            .await;

        response.assert_status(303);
        assert!(
            response.headers().get("set-cookie").is_none(),
            "flyco authenticates with the Authorization header only"
        );

        let location = response
            .headers()
            .get("location")
            .expect("the response redirects")
            .to_str()
            .expect("the location is ASCII");
        let location = Url::parse(location).expect("the location is an absolute URL");

        let callback_origin = Url::parse(REDIRECT_URI)
            .expect("the test redirect URI is absolute")
            .origin();
        assert_eq!(location.origin(), callback_origin);
        assert_eq!(location.path(), "/auth/complete");
        assert!(
            location.query().is_none(),
            "code and state must stay in the fragment"
        );

        let fragment = location
            .fragment()
            .expect("code and state ride the fragment");
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(fragment.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(pairs.contains(&("code".to_owned(), "abc".to_owned())));
        assert!(pairs.contains(&("state".to_owned(), state.clone())));
        assert!(
            !pairs.iter().any(|(k, _)| k == "token"),
            "GET callback must never mint or return an fs_ token"
        );

        // Neither KV state nor D1 users were modified by the unauthenticated GET.
        assert!(
            expiring::get::<PendingSignIn>(&kv, &state_key(&state))
                .await
                .expect("read state")
                .is_some()
        );
        let users_count: i64 = db
            .query("SELECT COUNT(*) FROM users")
            .fetch_scalar()
            .await
            .expect("count users");
        assert_eq!(users_count, 0);
    }

    #[skyzen::test]
    async fn exchange_with_an_unknown_state_is_rejected(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let pair = pkce().expect("mint PKCE");
        let response = client
            .post("/v1/auth/github/exchange")
            .json(&serde_json::json!({
                "code": "abc",
                "state": "gh_never-issued",
                "code_verifier": pair.verifier,
            }))
            .send()
            .await;

        response.assert_status(400);
        response.assert_header("content-type", "application/problem+json");
        assert_eq!(
            response.json::<flyco_core::Problem>().kind,
            "https://flyco.dev/problems/unknown-oauth-state"
        );
    }

    #[skyzen::test]
    async fn exchange_with_a_mismatched_pkce_verifier_is_rejected_and_consumes_state(
        ctx: TestContext,
        _kv: Kv,
        db: Db,
    ) {
        let client = ctx.client(migrated_router(&db).await);
        let (state, verifier) = begin(&client).await;
        let wrong = pkce().expect("mint wrong PKCE");

        let refused = client
            .post("/v1/auth/github/exchange")
            .json(&serde_json::json!({
                "code": "abc",
                "state": state,
                "code_verifier": wrong.verifier,
            }))
            .send()
            .await;
        refused.assert_status(400);
        assert_eq!(
            refused.json::<flyco_core::Problem>().kind,
            "https://flyco.dev/problems/unknown-oauth-state"
        );

        // Even the genuine verifier cannot reuse the burned state.
        let replay = client
            .post("/v1/auth/github/exchange")
            .json(&serde_json::json!({
                "code": "abc",
                "state": state,
                "code_verifier": verifier,
            }))
            .send()
            .await;
        replay.assert_status(400);
    }

    #[skyzen::test]
    async fn a_state_is_only_good_once(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let (state, verifier) = begin(&client).await;
        let body = serde_json::json!({
            "code": "abc",
            "state": state,
            "code_verifier": verifier,
        });

        client
            .post("/v1/auth/github/exchange")
            .json(&body)
            .send()
            .await
            .assert_status(200);
        client
            .post("/v1/auth/github/exchange")
            .json(&body)
            .send()
            .await
            .assert_status(400);
    }

    #[skyzen::test]
    async fn exchange_signs_the_user_in_and_sets_no_cookie(ctx: TestContext, kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let (state, verifier) = begin(&client).await;

        let response = client
            .post("/v1/auth/github/exchange")
            .json(&serde_json::json!({
                "code": "abc",
                "state": state,
                "code_verifier": verifier,
            }))
            .send()
            .await;

        response.assert_status(200);
        assert!(
            response.headers().get("set-cookie").is_none(),
            "flyco authenticates with the Authorization header only"
        );
        let body: GithubSession = response.json();
        assert!(body.token.starts_with(session::TOKEN_PREFIX));

        let user_id = session::resolve(&kv, &body.token)
            .await
            .expect("resolve the session")
            .expect("the issued token is live");

        let user = users::find(&db, user_id)
            .await
            .expect("read the user")
            .expect("the exchange created the user row");
        assert_eq!(user.login, GITHUB_LOGIN);
    }

    #[skyzen::test]
    async fn the_github_token_is_stored_sealed(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let (state, verifier) = begin(&client).await;
        client
            .post("/v1/auth/github/exchange")
            .json(&serde_json::json!({
                "code": "abc",
                "state": state,
                "code_verifier": verifier,
            }))
            .send()
            .await
            .assert_status(200);

        let stored: Vec<String> = db
            .query("SELECT github_token_enc FROM users")
            .fetch_scalars()
            .await
            .expect("read the stored token");

        let sealed = stored.first().expect("one user row exists");
        assert!(!sealed.contains(GITHUB_ACCESS_TOKEN));
        assert_eq!(
            test_config().token_cipher().open(sealed).expect("unseal"),
            GITHUB_ACCESS_TOKEN
        );
    }

    #[skyzen::test]
    async fn a_second_sign_in_reuses_the_same_flyco_user(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);

        let mut ids = Vec::new();
        for _ in 0..2 {
            let (state, verifier) = begin(&client).await;
            client
                .post("/v1/auth/github/exchange")
                .json(&serde_json::json!({
                    "code": "abc",
                    "state": state,
                    "code_verifier": verifier,
                }))
                .send()
                .await
                .assert_status(200);

            let id: String = skyzen::sql!(db, "SELECT id FROM users WHERE github_id = {GITHUB_ID}")
                .fetch_scalar()
                .await
                .expect("exactly one row per GitHub account");
            ids.push(id);
        }

        assert_eq!(ids[0], ids[1]);
    }
}
