//! Linking a cloud account by signing in with it.
//!
//! Five routes per vendor, and between them the user does one thing: approve
//! a consent screen. [`azure_start`] mints a user-bound attempt (and a PKCE
//! verifier where the vendor supports PKCE) and hands back an authorize URL;
//! the vendor returns the browser to [`azure_callback`], which performs no
//! exchange and sets no cookies — it forwards `provider`, `code`, and `state`
//! in the URL fragment to `/connect/return`; the authenticated SPA then posts
//! `{attempt_id, code, state}` to [`azure_exchange`] carrying its Bearer
//! credential, proving the signed-in user owns the attempt before any code is
//! redeemed; the flow tab [`azure_poll`]s until the attempt is authorized; and
//! [`azure_finish`] creates the credential inside the account the user chose
//! and links it. Google and Codespaces follow the same contract.
//!
//! **Why callbacks never exchange codes.** A browser navigation arriving from
//! `login.microsoftonline.com`, `accounts.google.com`, or `github.com` carries
//! no flyco credential (flyco sets no cookies). Redeeming a code on that `GET`
//! would let an attacker feed their own `authorize_url` to a victim and store
//! the victim's grant on the attacker's attempt. Forwarding `code` and `state`
//! in the fragment to `/connect/return` requires the signed-in SPA to present
//! `Authorization: Bearer` to [`azure_exchange`]/[`gcp_exchange`]/[`codespaces_exchange`],
//! which verifies `attempt.user == current_user`, `attempt.provider == provider`,
//! and `attempt.state == state` before redeeming the code (with PKCE for Azure
//! and GCP).
//!
//! The vendor tokens an authorized attempt obtained live in the attempt until
//! the finish spends them. They are secrets: nothing here returns them, and
//! the attempt's `Debug` shows neither.
#![expect(
    clippy::too_many_arguments,
    reason = "a finish handler takes four injected services, the path, the body and two \
              stores; bundling them would be a struct that exists only to shorten one \
              signature"
)]

use flyco_core::{
    CurrentUser, FinishAzureOauth, FinishCodespacesOauth, FinishGcpOauth, LinkProvider,
    ProviderAccountView, ProviderCredentials, ProviderOauthAttemptId, ProviderOauthChoice,
    ProviderOauthProgress, ProviderOauthStart, UserId,
};
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv, Queue};

use crate::clouds::Clouds;
use crate::codespaces::{Codespaces, CodespacesLink as _};
use crate::config::ApiConfig;
use crate::crypto::{pkce, prefixed_token, random_token};
use crate::error::ApiError;
use crate::expiring;
use crate::extract::path_id;
use crate::github::{
    CODESPACE_SCOPE, GithubClient, GithubGrant, GithubOauth as _, GithubToken, REPO_SCOPE, SCOPE,
};
use crate::google::{self, GoogleClient, GoogleOauth as _};
use crate::microsoft::{self, AzureTokens, MicrosoftClient, MicrosoftOauth as _};
use crate::oauth::constant_time_eq;
use crate::problem::Outcome;
use crate::provider_accounts;
use crate::respond::{Created, SeeOther};
use crate::users;

/// Where Microsoft returns the browser.
///
/// One constant for the route and for the redirect URI the authorize URL
/// carries, so the two cannot drift into a sign-in that comes back to a
/// path nothing serves.
pub const AZURE_CALLBACK_PATH: &str = "/v1/providers/azure/oauth/callback";

/// Where Google returns the browser.
pub const GCP_CALLBACK_PATH: &str = "/v1/providers/gcp/oauth/callback";

/// Where the SPA takes over once the browser is back.
const RETURN_PATH: &str = "/connect/return";

/// Names the vendor on the return page.
const PROVIDER_PARAM: &str = "provider";

/// Prefix that distinguishes Codespaces link states from GitHub sign-in states
/// on the shared `/v1/auth/github/callback` route without a KV read.
const CODESPACES_STATE_PREFIX: &str = "cs_";

/// How long a browser has to complete the round trip.
///
/// Longer than the GitHub sign-in's ten minutes: a first sign-in at a cloud
/// vendor routinely runs through a password, a second factor, an account
/// picker and a consent screen, and the first live run expired at exactly
/// ten minutes with the user still on the vendor's pages. Before the
/// exchange the attempt holds nothing but a state and optional PKCE verifier;
/// after it, the vendor tokens sit in the store only until the finish spends
/// them.
const ATTEMPT_TTL_SECONDS: u64 = 30 * 60;

/// Prefix every attempt is stored under.
const ATTEMPT_KEY_PREFIX: &str = "auth:provider-oauth:";

/// Which vendor one sign-in belongs to.
///
/// Carried in the attempt as well as in the path so a Google exchange cannot
/// complete an Azure sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Provider {
    /// Microsoft, for an Azure subscription.
    Azure,
    /// Google, for a GCP project.
    Gcp,
    /// GitHub, for a Codespaces account.
    Codespaces,
}

impl Provider {
    /// How the return page and the route paths name it.
    const fn slug(self) -> &'static str {
        match self {
            Self::Azure => "azure",
            Self::Gcp => "gcp",
            Self::Codespaces => "codespaces",
        }
    }

    /// This vendor's refusal, as the problem a caller sees.
    const fn rejected(self, reason: String) -> ApiError {
        match self {
            Self::Azure => ApiError::MicrosoftRejected { reason },
            Self::Gcp => ApiError::GoogleRejected { reason },
            Self::Codespaces => ApiError::GithubRejected { reason },
        }
    }
}

/// Whether `state` was minted for a Codespaces provider link rather than a
/// GitHub sign-in.
pub(crate) fn is_codespaces_state(state: &str) -> bool {
    state.starts_with(CODESPACES_STATE_PREFIX)
}

/// The vendor tokens one authorized attempt holds.
///
/// Secrets by construction: they act as the user inside their own cloud
/// account until the finish spends them, so this value is never returned to
/// anybody and never rendered.
#[derive(Serialize, Deserialize)]
enum Secrets {
    /// Microsoft's pair, plus the directory they act in.
    Azure(AzureTokens),
    /// Google's single short-lived token.
    Gcp(String),
    /// GitHub's OAuth grant, plus what `GET /user` answered beside it.
    Codespaces {
        /// The granted token — the account credential in full.
        token: String,
        /// What the grant is renewed with, when the OAuth app expires user
        /// tokens — `None` for an attempt recorded before flyco kept it.
        #[serde(default)]
        refresh_token: Option<String>,
        /// When `token` stops working, seconds since the Unix epoch.
        #[serde(default)]
        token_expires_at_unix: Option<u64>,
        /// The account's immutable id, which `verify` re-checks on every
        /// link so a renamed login cannot slip a credential onto the wrong
        /// account.
        owner_id: i64,
        /// The plan's included core-hours, read at the exchange rather
        /// than at the finish so the stored grant is the one the user was
        /// told they had.
        included_core_hours: u32,
    },
}

impl core::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Azure(_) => f.write_str("Secrets::Azure(..)"),
            Self::Gcp(_) => f.write_str("Secrets::Gcp(..)"),
            Self::Codespaces { .. } => f.write_str("Secrets::Codespaces(..)"),
        }
    }
}

/// How far one sign-in has got.
#[derive(Debug, Serialize, Deserialize)]
enum Stage {
    /// The browser has not exchanged the authorization code yet.
    Started,
    /// The vendor said who signed in and what they may link.
    Authorized {
        /// The account that signed in, as the vendor names it.
        account: String,
        /// What that account may link.
        choices: Vec<ProviderOauthChoice>,
        /// What the finish will act with.
        secrets: Secrets,
    },
    /// The sign-in ended without a grant. Kept so the page polling this
    /// attempt learns it is over, with the same problem the return page saw.
    Failed {
        /// The problem slug.
        problem: String,
        /// The vendor's own words.
        reason: String,
    },
}

/// One cloud sign-in in flight, as the key-value store holds it.
#[derive(Serialize, Deserialize)]
struct Attempt {
    /// Who started it. An attempt exchanged, polled or finished by anybody
    /// else is not this attempt.
    user: UserId,
    /// Which vendor it is with.
    provider: Provider,
    /// The `state` published in the authorize URL.
    state: String,
    /// Server-held PKCE verifier for vendors that support PKCE (Azure, GCP).
    #[serde(default)]
    pkce_verifier: Option<String>,
    /// How far it has got.
    stage: Stage,
}

impl core::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Attempt")
            .field("user", &self.user)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

/// Where one attempt lives in the key-value store.
fn attempt_key(id: ProviderOauthAttemptId) -> String {
    let rendered = id.to_string();
    let mut key = String::with_capacity(ATTEMPT_KEY_PREFIX.len() + rendered.len());
    key.push_str(ATTEMPT_KEY_PREFIX);
    key.push_str(&rendered);
    key
}

/// The query string a vendor appends when it returns the browser.
///
/// Every field but `state` is optional because a refusal is a redirect too:
/// a user who closes the consent screen comes back with `error` and no
/// `code`, and that has to reach the return page cleanly rather than as an
/// extractor failure.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct ProviderCallback {
    /// The single-use authorization code.
    #[serde(default)]
    code: Option<String>,
    /// The `state` this control plane minted in the matching `start`.
    pub(crate) state: String,
    /// The vendor's machine-readable refusal, when it refused.
    #[serde(default)]
    error: Option<String>,
    /// Its explanation.
    #[serde(default)]
    error_description: Option<String>,
}

impl ProviderCallback {
    /// Raw authorization code, if present.
    pub(crate) fn code_raw(&self) -> Option<&str> {
        self.code.as_deref().filter(|code| !code.is_empty())
    }

    /// Raw vendor refusal slug, if present.
    pub(crate) fn error_raw(&self) -> Option<&str> {
        self.error.as_deref().filter(|error| !error.is_empty())
    }

    /// Raw vendor refusal description, if present.
    pub(crate) fn error_description_raw(&self) -> Option<&str> {
        self.error_description
            .as_deref()
            .filter(|description| !description.is_empty())
    }
}

/// Request body of `POST /v1/providers/{azure,gcp,codespaces}/oauth/exchange`.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct ExchangeProviderOauth {
    /// The attempt id returned by the matching `POST .../oauth/start`.
    pub attempt_id: ProviderOauthAttemptId,
    /// The `state` echoed back by the vendor to `/connect/return`.
    pub state: String,
    /// The single-use authorization code, when consent was granted.
    #[serde(default)]
    pub code: Option<String>,
    /// The vendor's machine-readable refusal, when consent was declined.
    #[serde(default)]
    pub error: Option<String>,
    /// The vendor's explanation of the refusal, when stated.
    #[serde(default)]
    pub error_description: Option<String>,
}

impl ExchangeProviderOauth {
    /// The code, or the vendor's own reason there is none.
    fn code(&self, provider: Provider) -> Result<&str, ApiError> {
        if let Some(refusal) = self.error.as_deref().filter(|value| !value.is_empty()) {
            let reason = self
                .error_description
                .as_deref()
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || refusal.to_owned(),
                    |description| {
                        let mut reason =
                            String::with_capacity(refusal.len() + description.len() + 2);
                        reason.push_str(refusal);
                        reason.push_str(": ");
                        reason.push_str(description);
                        reason
                    },
                );
            return Err(provider.rejected(reason));
        }
        self.code
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                provider
                    .rejected("the sign-in came back with neither a code nor a reason".to_owned())
            })
    }
}

/// Forwards a public vendor callback to the SPA's `/connect/return` page with
/// the callback parameters in the URL fragment.
fn forward_callback(
    config: &ApiConfig,
    provider: Provider,
    callback: &ProviderCallback,
) -> SeeOther {
    let mut url = config
        .redirect_uri()
        .join(RETURN_PATH)
        .expect("a rooted path always resolves against an absolute redirect URI");
    let mut fragment = url::form_urlencoded::Serializer::new(String::new());
    fragment.append_pair(PROVIDER_PARAM, provider.slug());
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
    SeeOther(url)
}

/// Forwards a Codespaces callback arriving on `/v1/auth/github/callback` to
/// `/connect/return#provider=codespaces&...`.
pub(crate) fn forward_codespaces_callback(
    config: &ApiConfig,
    callback: &ProviderCallback,
) -> SeeOther {
    forward_callback(config, Provider::Codespaces, callback)
}

/// Mints an attempt and the authorize URL that belongs to it.
async fn begin(
    config: &ApiConfig,
    kv: &Kv,
    user: UserId,
    provider: Provider,
) -> Result<ProviderOauthStart, ApiError> {
    let attempt_id = ProviderOauthAttemptId::generate();
    let (state, pkce_verifier, authorize_url) = match provider {
        Provider::Azure => {
            let state = random_token()?;
            let pair = pkce()?;
            let url = microsoft::authorize_url(
                config.azure_oauth_client_id(),
                config.azure_oauth_redirect_uri().as_str(),
                &state,
                &pair.challenge,
            );
            (state, Some(pair.verifier), url)
        }
        Provider::Gcp => {
            let state = random_token()?;
            let pair = pkce()?;
            let url = google::authorize_url(
                config.google_oauth_client_id(),
                config.gcp_oauth_redirect_uri().as_str(),
                &state,
                &pair.challenge,
            );
            (state, Some(pair.verifier), url)
        }
        Provider::Codespaces => {
            let state = prefixed_token(CODESPACES_STATE_PREFIX)?;
            let url = crate::oauth::authorize_url(
                config.github_client_id(),
                // The sign-in's own callback: the OAuth app registers one URI
                // per hostname, and the `cs_` state prefix routes Codespaces
                // returns to `/connect/return` without touching KV.
                config.redirect_uri(),
                SCOPE,
                &state,
            );
            (state, None, url)
        }
    };

    expiring::put(
        kv,
        &attempt_key(attempt_id),
        &Attempt {
            user,
            provider,
            state,
            pkce_verifier,
            stage: Stage::Started,
        },
        ATTEMPT_TTL_SECONDS,
    )
    .await?;

    tracing::debug!(provider = provider.slug(), "issued a cloud authorize URL");
    Ok(ProviderOauthStart {
        attempt_id,
        authorize_url: authorize_url.into(),
    })
}

/// Reads the caller's own attempt, whatever stage it is at.
///
/// An attempt that is unknown, expired, another vendor's or another user's
/// is one answer, deliberately: the attempt id is the only thing that names
/// an attempt, and telling a caller which of the four theirs is would be a
/// free oracle over somebody else's sign-in.
async fn mine(
    kv: &Kv,
    user: UserId,
    provider: Provider,
    attempt_id: ProviderOauthAttemptId,
) -> Result<(String, Attempt), ApiError> {
    let key = attempt_key(attempt_id);
    let attempt = expiring::get::<Attempt>(kv, &key)
        .await?
        .filter(|attempt| attempt.user == user && attempt.provider == provider)
        .ok_or(ApiError::ProviderOauthAttemptExpired)?;
    Ok((key, attempt))
}

/// Reads the caller's attempt and insists it is still in `Stage::Started` and
/// carries `state`.
async fn claim_started(
    kv: &Kv,
    user: UserId,
    provider: Provider,
    attempt_id: ProviderOauthAttemptId,
    state: &str,
) -> Result<(String, Attempt), ApiError> {
    let (key, attempt) = mine(kv, user, provider, attempt_id).await?;
    if !matches!(attempt.stage, Stage::Started)
        || !constant_time_eq(attempt.state.as_bytes(), state.as_bytes())
    {
        return Err(ApiError::UnknownOauthState);
    }
    Ok((key, attempt))
}

/// One poll of a sign-in. Reads, never consumes.
async fn progress(
    kv: &Kv,
    user: UserId,
    provider: Provider,
    attempt_id: ProviderOauthAttemptId,
) -> Result<ProviderOauthProgress, ApiError> {
    let (_, attempt) = mine(kv, user, provider, attempt_id).await?;
    Ok(match attempt.stage {
        Stage::Started => ProviderOauthProgress::Pending,
        Stage::Authorized {
            account, choices, ..
        } => ProviderOauthProgress::Authorized { account, choices },
        Stage::Failed { problem, reason } => ProviderOauthProgress::Failed { problem, reason },
    })
}

/// Marks an attempt as over, so the poll stops waiting.
///
/// The failure is recorded beside the attempt rather than by deleting it:
/// a deleted attempt polls as expired, which tells the user to start again
/// without telling them why the last one ended.
async fn record_failure(
    kv: &Kv,
    key: &str,
    mut attempt: Attempt,
    error: &ApiError,
) -> Result<(), ApiError> {
    attempt.stage = Stage::Failed {
        problem: error.slug().to_owned(),
        reason: error.to_string(),
    };
    expiring::put(kv, key, &attempt, ATTEMPT_TTL_SECONDS).await?;
    Ok(())
}

/// What an authorized attempt holds, for the finish that spends it.
struct Authorized {
    /// Where the attempt lives, so the finish can delete it.
    key: String,
    /// The account that signed in.
    account: String,
    /// What it may link.
    choices: Vec<ProviderOauthChoice>,
    /// What to act with.
    secrets: Secrets,
}

/// Reads the caller's attempt, insisting the exchange has already succeeded.
async fn authorized(
    kv: &Kv,
    user: UserId,
    provider: Provider,
    attempt_id: ProviderOauthAttemptId,
) -> Result<Authorized, ApiError> {
    let (key, attempt) = mine(kv, user, provider, attempt_id).await?;
    let Stage::Authorized {
        account,
        choices,
        secrets,
    } = attempt.stage
    else {
        return Err(ApiError::ProviderOauthNotAuthorized);
    };
    Ok(Authorized {
        key,
        account,
        choices,
        secrets,
    })
}

/// What to call the account, given what the user chose.
///
/// The vendor's own name for the subscription or project, which is what the
/// user recognises in a list. An id the vendor did not list is still a name:
/// whether it can be acted on is the vendor's answer, not this function's.
fn chosen_label(choices: &[ProviderOauthChoice], id: &str) -> String {
    choices
        .iter()
        .find(|choice| choice.id == id)
        .map_or_else(|| id.to_owned(), |choice| choice.name.clone())
}

// ── Azure ──

/// `POST /v1/providers/azure/oauth/start` — begins a Microsoft sign-in.
///
/// Per-call cost: 0 D1 statements, 1 KV write (`auth:provider-oauth:{attempt_id}`),
/// 0 Durable Object calls, 0 R2 operations, 0 subrequests.
#[skyzen::openapi]
pub async fn azure_start(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    kv: Kv,
) -> Outcome<Json<ProviderOauthStart>> {
    begin(&config, &kv, user.id, Provider::Azure)
        .await
        .map(Json)
        .into()
}

/// `GET /v1/providers/azure/oauth/callback` — forwards Microsoft's redirect to
/// `/connect/return#provider=azure&...` without exchanging the code or setting
/// cookies.
///
/// Per-call cost: 0 D1 statements, 0 KV operations, 0 Durable Object calls,
/// 0 R2 operations, 0 subrequests.
#[skyzen::openapi]
pub async fn azure_callback(
    Query(callback): Query<ProviderCallback>,
    State(config): State<ApiConfig>,
) -> Outcome<SeeOther> {
    Ok(forward_callback(&config, Provider::Azure, &callback)).into()
}

/// `POST /v1/providers/azure/oauth/exchange` — verifies the caller owns the
/// attempt and `state`, then redeems Microsoft's code using the attempt's PKCE
/// verifier.
///
/// Per-call cost: 0 D1 statements, 1 KV read + 1 KV write, 0 Durable Object
/// calls, 0 R2 operations, 3 external subrequests to Microsoft (code exchange,
/// Graph token refresh, list subscriptions) on success.
#[skyzen::openapi]
pub async fn azure_exchange(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(microsoft): State<MicrosoftClient>,
    kv: Kv,
    Json(request): Json<ExchangeProviderOauth>,
) -> Outcome<Json<ProviderOauthProgress>> {
    redeem_azure(&config, &microsoft, &kv, user.id, &request)
        .await
        .map(Json)
        .into()
}

/// Redeems what Microsoft sent back, or says why it cannot.
async fn sign_in_azure(
    config: &ApiConfig,
    microsoft: &MicrosoftClient,
    request: &ExchangeProviderOauth,
    code_verifier: &str,
) -> Result<microsoft::SignIn, ApiError> {
    Ok(microsoft
        .sign_in(
            microsoft::OauthClient {
                id: config.azure_oauth_client_id(),
                secret: config.azure_oauth_client_secret(),
            },
            request.code(Provider::Azure)?,
            config.azure_oauth_redirect_uri().as_str(),
            code_verifier,
        )
        .await?)
}

async fn redeem_azure(
    config: &ApiConfig,
    microsoft: &MicrosoftClient,
    kv: &Kv,
    user: UserId,
    request: &ExchangeProviderOauth,
) -> Result<ProviderOauthProgress, ApiError> {
    let (key, mut attempt) = claim_started(
        kv,
        user,
        Provider::Azure,
        request.attempt_id,
        &request.state,
    )
    .await?;
    let Some(code_verifier) = attempt.pkce_verifier.as_deref() else {
        return Err(ApiError::CorruptRecord(
            "an Azure attempt is missing its PKCE verifier",
        ));
    };
    let signed_in = match sign_in_azure(config, microsoft, request, code_verifier).await {
        Ok(signed_in) => signed_in,
        Err(error) => {
            record_failure(kv, &key, attempt, &error).await?;
            return Err(error);
        }
    };

    let progress = ProviderOauthProgress::Authorized {
        account: signed_in.account.clone(),
        choices: signed_in.choices.clone(),
    };
    attempt.stage = Stage::Authorized {
        account: signed_in.account,
        choices: signed_in.choices,
        secrets: Secrets::Azure(signed_in.tokens),
    };
    expiring::put(kv, &key, &attempt, ATTEMPT_TTL_SECONDS).await?;
    tracing::info!(provider = "azure", "a cloud sign-in was authorized");
    Ok(progress)
}

/// `GET /v1/providers/azure/oauth/{attempt_id}` — polls it once.
#[skyzen::openapi]
pub async fn azure_poll(
    State(user): State<CurrentUser>,
    params: Params,
    kv: Kv,
) -> Outcome<Json<ProviderOauthProgress>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    progress(&kv, user.id, Provider::Azure, attempt_id)
        .await
        .map(Json)
        .into()
}

/// `POST /v1/providers/azure/oauth/{attempt_id}/finish` — links the chosen
/// subscription.
#[skyzen::openapi]
pub async fn azure_finish(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(microsoft): State<MicrosoftClient>,
    State(clouds): State<Clouds>,
    params: Params,
    Json(request): Json<FinishAzureOauth>,
    kv: Kv,
    db: Db,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    finish_azure(
        &config, &microsoft, &clouds, &kv, &db, &queue, user.id, attempt_id, request,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

#[expect(
    clippy::too_many_arguments,
    reason = "finishing a sign-in names both vendors, both stores, and the \
              attempt it is redeeming"
)]
async fn finish_azure(
    config: &ApiConfig,
    microsoft: &MicrosoftClient,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
    attempt_id: ProviderOauthAttemptId,
    request: FinishAzureOauth,
) -> Result<ProviderAccountView, ApiError> {
    let attempt = authorized(kv, user, Provider::Azure, attempt_id).await?;
    let Secrets::Azure(tokens) = attempt.secrets else {
        return Err(ApiError::CorruptRecord(
            "an Azure attempt is holding Google's tokens",
        ));
    };

    let identity = microsoft
        .create_identity(&tokens, &request.subscription_id)
        .await?;
    tracing::info!(
        account = %attempt.account,
        "created an Azure service principal for a linked subscription"
    );

    let view = provider_accounts::link(
        db,
        config,
        clouds,
        kv,
        queue,
        user,
        LinkProvider {
            label: chosen_label(&attempt.choices, &request.subscription_id),
            credentials: ProviderCredentials::Azure {
                tenant_id: identity.tenant_id,
                client_id: identity.client_id,
                client_secret: identity.client_secret,
                subscription_id: request.subscription_id,
            },
        },
    )
    .await?;

    // Spent: the account exists, and the vendor tokens in the attempt have
    // done the one thing they were kept for.
    kv.delete(&attempt.key).await?;
    Ok(view)
}

// ── GCP ──

/// `POST /v1/providers/gcp/oauth/start` — begins a Google sign-in.
///
/// Per-call cost: 0 D1 statements, 1 KV write (`auth:provider-oauth:{attempt_id}`),
/// 0 Durable Object calls, 0 R2 operations, 0 subrequests.
#[skyzen::openapi]
pub async fn gcp_start(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    kv: Kv,
) -> Outcome<Json<ProviderOauthStart>> {
    begin(&config, &kv, user.id, Provider::Gcp)
        .await
        .map(Json)
        .into()
}

/// `GET /v1/providers/gcp/oauth/callback` — forwards Google's redirect to
/// `/connect/return#provider=gcp&...` without exchanging the code or setting
/// cookies.
///
/// Per-call cost: 0 D1 statements, 0 KV operations, 0 Durable Object calls,
/// 0 R2 operations, 0 subrequests.
#[skyzen::openapi]
pub async fn gcp_callback(
    Query(callback): Query<ProviderCallback>,
    State(config): State<ApiConfig>,
) -> Outcome<SeeOther> {
    Ok(forward_callback(&config, Provider::Gcp, &callback)).into()
}

/// `POST /v1/providers/gcp/oauth/exchange` — verifies the caller owns the
/// attempt and `state`, then redeems Google's code using the attempt's PKCE
/// verifier.
///
/// Per-call cost: 0 D1 statements, 1 KV read + 1 KV write, 0 Durable Object
/// calls, 0 R2 operations, 2 external subrequests to Google (code exchange,
/// list projects) on success.
#[skyzen::openapi]
pub async fn gcp_exchange(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(google): State<GoogleClient>,
    kv: Kv,
    Json(request): Json<ExchangeProviderOauth>,
) -> Outcome<Json<ProviderOauthProgress>> {
    redeem_gcp(&config, &google, &kv, user.id, &request)
        .await
        .map(Json)
        .into()
}

/// Redeems what Google sent back, or says why it cannot.
async fn sign_in_gcp(
    config: &ApiConfig,
    google: &GoogleClient,
    request: &ExchangeProviderOauth,
    code_verifier: &str,
) -> Result<google::SignIn, ApiError> {
    Ok(google
        .sign_in(
            google::OauthClient {
                id: config.google_oauth_client_id(),
                secret: config.google_oauth_client_secret(),
            },
            request.code(Provider::Gcp)?,
            config.gcp_oauth_redirect_uri().as_str(),
            code_verifier,
        )
        .await?)
}

async fn redeem_gcp(
    config: &ApiConfig,
    google: &GoogleClient,
    kv: &Kv,
    user: UserId,
    request: &ExchangeProviderOauth,
) -> Result<ProviderOauthProgress, ApiError> {
    let (key, mut attempt) =
        claim_started(kv, user, Provider::Gcp, request.attempt_id, &request.state).await?;
    let Some(code_verifier) = attempt.pkce_verifier.as_deref() else {
        return Err(ApiError::CorruptRecord(
            "a GCP attempt is missing its PKCE verifier",
        ));
    };
    let signed_in = match sign_in_gcp(config, google, request, code_verifier).await {
        Ok(signed_in) => signed_in,
        Err(error) => {
            record_failure(kv, &key, attempt, &error).await?;
            return Err(error);
        }
    };

    let progress = ProviderOauthProgress::Authorized {
        account: signed_in.account.clone(),
        choices: signed_in.choices.clone(),
    };
    attempt.stage = Stage::Authorized {
        account: signed_in.account,
        choices: signed_in.choices,
        secrets: Secrets::Gcp(signed_in.access_token),
    };
    expiring::put(kv, &key, &attempt, ATTEMPT_TTL_SECONDS).await?;
    tracing::info!(provider = "gcp", "a cloud sign-in was authorized");
    Ok(progress)
}

/// `GET /v1/providers/gcp/oauth/{attempt_id}` — polls it once.
#[skyzen::openapi]
pub async fn gcp_poll(
    State(user): State<CurrentUser>,
    params: Params,
    kv: Kv,
) -> Outcome<Json<ProviderOauthProgress>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    progress(&kv, user.id, Provider::Gcp, attempt_id)
        .await
        .map(Json)
        .into()
}

/// `POST /v1/providers/gcp/oauth/{attempt_id}/finish` — links the chosen
/// project.
#[skyzen::openapi]
pub async fn gcp_finish(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(google): State<GoogleClient>,
    State(clouds): State<Clouds>,
    params: Params,
    Json(request): Json<FinishGcpOauth>,
    kv: Kv,
    db: Db,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    finish_gcp(
        &config, &google, &clouds, &kv, &db, &queue, user.id, attempt_id, request,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

#[expect(
    clippy::too_many_arguments,
    reason = "finishing a sign-in names both vendors, both stores, and the \
              attempt it is redeeming"
)]
async fn finish_gcp(
    config: &ApiConfig,
    google: &GoogleClient,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
    attempt_id: ProviderOauthAttemptId,
    request: FinishGcpOauth,
) -> Result<ProviderAccountView, ApiError> {
    let attempt = authorized(kv, user, Provider::Gcp, attempt_id).await?;
    let Secrets::Gcp(token) = attempt.secrets else {
        return Err(ApiError::CorruptRecord(
            "a Google attempt is holding Microsoft's tokens",
        ));
    };

    let service_account_json = google.create_identity(&token, &request.project_id).await?;
    tracing::info!(
        account = %attempt.account,
        "created a Google service account for a linked project"
    );

    let view = provider_accounts::link(
        db,
        config,
        clouds,
        kv,
        queue,
        user,
        LinkProvider {
            label: chosen_label(&attempt.choices, &request.project_id),
            credentials: ProviderCredentials::Gcp {
                service_account_json,
            },
        },
    )
    .await?;

    kv.delete(&attempt.key).await?;
    Ok(view)
}

// ── Codespaces ──

/// `POST /v1/providers/codespaces/oauth/start` — begins a GitHub sign-in
/// for the `codespace` scope.
///
/// Per-call cost: 0 D1 statements, 1 KV write (`auth:provider-oauth:{attempt_id}`),
/// 0 Durable Object calls, 0 R2 operations, 0 subrequests.
#[skyzen::openapi]
pub async fn codespaces_start(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    kv: Kv,
) -> Outcome<Json<ProviderOauthStart>> {
    begin(&config, &kv, user.id, Provider::Codespaces)
        .await
        .map(Json)
        .into()
}

/// `POST /v1/providers/codespaces/oauth/exchange` — verifies the caller owns
/// the attempt and `state`, then redeems GitHub's code and checks the
/// `codespace` and `repo` scopes.
///
/// Per-call cost: 0 D1 statements, 1 KV read + 1 KV write, 0 Durable Object
/// calls, 0 R2 operations, 2 external subrequests to GitHub (`access_token` +
/// `GET /user`) on success.
#[skyzen::openapi]
pub async fn codespaces_exchange(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    kv: Kv,
    Json(request): Json<ExchangeProviderOauth>,
) -> Outcome<Json<ProviderOauthProgress>> {
    redeem_codespaces(&config, &github, &kv, user.id, &request)
        .await
        .map(Json)
        .into()
}

/// Redeems what GitHub sent back: the token, the account it acts as, and
/// the proof it carries the scopes a codespace needs.
async fn sign_in_codespaces(
    config: &ApiConfig,
    github: &GithubClient,
    request: &ExchangeProviderOauth,
) -> Result<CodespacesSignIn, ApiError> {
    let grant = github
        .exchange_code(
            config.github_client_id(),
            config.github_client_secret(),
            request.code(Provider::Codespaces)?,
            config.redirect_uri().as_str(),
        )
        .await?;
    let identity = github.current_user(&grant.token).await?;
    if !identity.grants_scope(CODESPACE_SCOPE) {
        return Err(Provider::Codespaces.rejected(format!(
            "the sign-in did not grant the `{CODESPACE_SCOPE}` scope — without it flyco \
             cannot create or drive a codespace"
        )));
    }
    if !identity.grants_repo_scope() {
        return Err(Provider::Codespaces.rejected(
            "the sign-in did not grant the `repo` scope — without it flyco cannot create \
             or write the private repository a codespace is built from"
                .to_owned(),
        ));
    }
    Ok(CodespacesSignIn {
        grant,
        login: identity.user.login,
        owner_id: identity.user.id,
        included_core_hours: flyco_provider::codespaces::included_core_hours(
            identity.user.plan.as_ref().map(|plan| plan.name.as_str()),
        ),
    })
}

/// What the exchange keeps of a GitHub sign-in until the finish spends it.
struct CodespacesSignIn {
    /// The grant in full — the token is the credential, and the refresh
    /// half is what renews it once the grant nears its end.
    grant: GithubGrant,
    /// Who it acts as.
    login: String,
    /// The account's immutable id.
    owner_id: i64,
    /// Core-hours the account's plan includes monthly.
    included_core_hours: u32,
}

async fn redeem_codespaces(
    config: &ApiConfig,
    github: &GithubClient,
    kv: &Kv,
    user: UserId,
    request: &ExchangeProviderOauth,
) -> Result<ProviderOauthProgress, ApiError> {
    let (key, mut attempt) = claim_started(
        kv,
        user,
        Provider::Codespaces,
        request.attempt_id,
        &request.state,
    )
    .await?;
    let signed_in = match sign_in_codespaces(config, github, request).await {
        Ok(signed_in) => signed_in,
        Err(error) => {
            record_failure(kv, &key, attempt, &error).await?;
            return Err(error);
        }
    };

    let progress = ProviderOauthProgress::Authorized {
        account: signed_in.login.clone(),
        choices: Vec::new(),
    };
    // No choices — there is nothing a GitHub account has many of that a
    // codespace provisions into; flyco creates the one repository it needs.
    attempt.stage = Stage::Authorized {
        account: signed_in.login,
        choices: Vec::new(),
        secrets: Secrets::Codespaces {
            token: signed_in.grant.token.access_token,
            refresh_token: signed_in.grant.refresh_token,
            token_expires_at_unix: signed_in.grant.expires_at_unix,
            owner_id: signed_in.owner_id,
            included_core_hours: signed_in.included_core_hours,
        },
    };
    expiring::put(kv, &key, &attempt, ATTEMPT_TTL_SECONDS).await?;
    tracing::info!(provider = "codespaces", "a cloud sign-in was authorized");
    Ok(progress)
}

/// `GET /v1/providers/codespaces/oauth/{attempt_id}` — polls it once.
#[skyzen::openapi]
pub async fn codespaces_poll(
    State(user): State<CurrentUser>,
    params: Params,
    kv: Kv,
) -> Outcome<Json<ProviderOauthProgress>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    progress(&kv, user.id, Provider::Codespaces, attempt_id)
        .await
        .map(Json)
        .into()
}

/// `POST /v1/providers/codespaces/oauth/{attempt_id}/finish` — creates the
/// environment repository and links the account.
#[skyzen::openapi]
pub async fn codespaces_finish(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(codespaces): State<Codespaces>,
    State(clouds): State<Clouds>,
    params: Params,
    Json(_request): Json<FinishCodespacesOauth>,
    kv: Kv,
    db: Db,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    finish_codespaces(
        &config,
        &codespaces,
        &clouds,
        &kv,
        &db,
        &queue,
        user.id,
        attempt_id,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

async fn finish_codespaces(
    config: &ApiConfig,
    codespaces: &Codespaces,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
    attempt_id: ProviderOauthAttemptId,
) -> Result<ProviderAccountView, ApiError> {
    let attempt = authorized(kv, user, Provider::Codespaces, attempt_id).await?;
    let Secrets::Codespaces {
        token,
        refresh_token,
        token_expires_at_unix,
        owner_id,
        included_core_hours,
    } = attempt.secrets
    else {
        return Err(ApiError::CorruptRecord(
            "a Codespaces attempt is holding another vendor's tokens",
        ));
    };

    let view = link_codespaces_account(
        config,
        codespaces,
        clouds,
        kv,
        db,
        queue,
        user,
        &attempt.account,
        GithubGrant {
            token: GithubToken {
                access_token: token,
            },
            refresh_token,
            expires_at_unix: token_expires_at_unix,
        },
        owner_id,
        included_core_hours,
    )
    .await?;

    kv.delete(&attempt.key).await?;
    Ok(view)
}

/// `POST /v1/providers/codespaces/link` — links the account straight from
/// the sign-in grant.
///
/// Sign-in has asked for the Codespaces scope set since grants began
/// carrying it, so the grant a user signed in with usually *is* the
/// credential the link stores — this route proves it with the scopes
/// `GET /user` reports, then does the finish's own work without the
/// OAuth attempt. A grant that predates the ask answers
/// [`ApiError::GithubScopeMissing`], which is what sends the page through
/// the OAuth flow instead.
#[skyzen::openapi]
pub async fn codespaces_link(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    State(codespaces): State<Codespaces>,
    State(clouds): State<Clouds>,
    kv: Kv,
    db: Db,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    link_codespaces(
        &config,
        &github,
        &codespaces,
        &clouds,
        &kv,
        &db,
        &queue,
        user.id,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

async fn link_codespaces(
    config: &ApiConfig,
    github: &GithubClient,
    codespaces: &Codespaces,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
) -> Result<ProviderAccountView, ApiError> {
    let grant = users::github_grant(db, config, github, user).await?;
    let identity = github.current_user(&grant.token).await?;
    for scope in [REPO_SCOPE, CODESPACE_SCOPE] {
        if !identity.grants_scope(scope) {
            return Err(ApiError::GithubScopeMissing { scope });
        }
    }
    link_codespaces_account(
        config,
        codespaces,
        clouds,
        kv,
        db,
        queue,
        user,
        &identity.user.login,
        grant,
        identity.user.id,
        flyco_provider::codespaces::included_core_hours(
            identity.user.plan.as_ref().map(|plan| plan.name.as_str()),
        ),
    )
    .await
}

/// The work both doors into a Codespaces link share once each has proven
/// the grant carries what provisioning needs: the environment repository is
/// created first — `link`'s own verify re-reads it, so building it here is
/// what makes the recorded credential one that already provisions.
async fn link_codespaces_account(
    config: &ApiConfig,
    codespaces: &Codespaces,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
    login: &str,
    grant: GithubGrant,
    owner_id: i64,
    included_core_hours: u32,
) -> Result<ProviderAccountView, ApiError> {
    let environment = codespaces
        .ensure_environment(
            &grant.token,
            login,
            &flyco_provider::codespaces::devcontainer_json(&config.control_plane_url()),
        )
        .await
        .map_err(crate::clouds::rejected)?;
    tracing::info!(
        account = %login,
        repository = %environment.full_name,
        "prepared a Codespaces environment repository"
    );

    provider_accounts::link(
        db,
        config,
        clouds,
        kv,
        queue,
        user,
        LinkProvider {
            // The GitHub login is the name the user recognises: it is the
            // one account this credential can ever act as.
            label: login.to_owned(),
            credentials: ProviderCredentials::Codespaces {
                token: grant.token.access_token,
                refresh_token: grant.refresh_token,
                token_expires_at_unix: grant.expires_at_unix,
                env_repo: environment.full_name,
                env_repo_id: environment.id,
                owner_id,
                included_core_hours,
            },
        },
    )
    .await
}

/// The two public callbacks.
///
/// Public because a browser returning from a cloud vendor carries no flyco
/// credential. Neither performs any exchange or storage access — each
/// redirects straight to `/connect/return` with the callback parameters in
/// the fragment so the signed-in SPA performs the authenticated exchange.
pub fn public_routes() -> Vec<RouteNode> {
    Route::new((
        AZURE_CALLBACK_PATH.at(azure_callback),
        GCP_CALLBACK_PATH.at(gcp_callback),
    ))
    .into_route_nodes()
}

/// The thirteen authenticated routes of the three cloud sign-ins.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/providers/azure/oauth/start".post(azure_start),
        "/v1/providers/azure/oauth/exchange".post(azure_exchange),
        "/v1/providers/azure/oauth/{attempt_id}".at(azure_poll),
        "/v1/providers/azure/oauth/{attempt_id}/finish".post(azure_finish),
        "/v1/providers/gcp/oauth/start".post(gcp_start),
        "/v1/providers/gcp/oauth/exchange".post(gcp_exchange),
        "/v1/providers/gcp/oauth/{attempt_id}".at(gcp_poll),
        "/v1/providers/gcp/oauth/{attempt_id}/finish".post(gcp_finish),
        "/v1/providers/codespaces/link".post(codespaces_link),
        "/v1/providers/codespaces/oauth/start".post(codespaces_start),
        "/v1/providers/codespaces/oauth/exchange".post(codespaces_exchange),
        "/v1/providers/codespaces/oauth/{attempt_id}".at(codespaces_poll),
        "/v1/providers/codespaces/oauth/{attempt_id}/finish".post(codespaces_finish),
    ))
    .into_route_nodes()
}
