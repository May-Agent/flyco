//! The `users` table.
//!
//! One row per GitHub account, keyed by GitHub's immutable numeric id
//! because logins can be renamed and reused. The account's GitHub grant is
//! sealed before it gets here and is the single source of truth for every
//! GitHub call made as that account — repository pickers, checkouts, and
//! same-account GitHub Codespaces provisioning alike.

use core::future::Future;

use flyco_core::{
    CloudProviderKind, CurrentUser, ProviderAccountId, ProviderCredentials, SESSION_CAP_MAX,
    SESSION_CAP_MIN, UserId,
};
use skyzen::sql;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::github::{GithubError, GithubGrant, GithubOauth, GithubToken, GithubUser};
use crate::harness_accounts::REFRESH_WINDOW_SECONDS;
use crate::provider_accounts::StoredSecrets;

/// The columns every read on this path projects.
#[derive(Debug, skyzen::FromRow)]
struct IdentityRow {
    id: UserId,
    login: String,
    session_cap: u32,
}

impl From<IdentityRow> for CurrentUser {
    fn from(row: IdentityRow) -> Self {
        Self {
            id: row.id,
            login: row.login,
            session_cap: row.session_cap,
        }
    }
}

/// The stored halves of a user's GitHub grant, alongside the immutable
/// `github_id` that pairs it with a same-owner Codespaces account.
#[derive(Debug, Clone, skyzen::FromRow)]
struct GrantRow {
    github_id: i64,
    token_enc: String,
    refresh_token_enc: Option<String>,
    token_expires_at_unix: Option<u64>,
}

/// Minimal projection of a linked Codespaces row inspected during one-time
/// split-brain grant absorption.
#[derive(Debug, skyzen::FromRow)]
struct CodespacesAccountRow {
    id: ProviderAccountId,
    credentials_enc: String,
}

/// Records a completed GitHub sign-in, creating the account on first sight.
///
/// A returning user keeps their flyco [`UserId`] — everything else in the
/// product hangs off it — while their login and sealed grant are refreshed
/// to what GitHub just told us. When the OAuth app expires user tokens the
/// grant carries the refresh credential it is renewed with; when it does
/// not, those columns stay `NULL` and the grant is used until revoked.
///
/// # Errors
///
/// Returns [`ApiError`] if the database rejects the write or the row it
/// returns is not the one the schema promises.
pub async fn upsert_from_github(
    db: &Db,
    config: &ApiConfig,
    account: &GithubUser,
    grant: &GithubGrant,
) -> Result<CurrentUser, ApiError> {
    let cipher = config.token_cipher();
    let sealed_token = cipher.seal(&grant.token.access_token)?;
    let sealed_refresh = grant
        .refresh_token
        .as_deref()
        .map(|refresh| cipher.seal(refresh))
        .transpose()?;
    let expires_at = grant.expires_at_unix;
    let row: IdentityRow = sql!(
        db,
        "INSERT INTO users (id, github_id, login, github_token_enc, \
             github_refresh_token_enc, github_token_expires_at_unix, created_at_unix) \
         VALUES ({UserId::generate()}, {account.id}, {account.login.clone()}, \
                 {sealed_token}, {sealed_refresh}, {expires_at}, {now_unix()}) \
         ON CONFLICT (github_id) DO UPDATE SET \
         login = excluded.login, \
         github_token_enc = excluded.github_token_enc, \
         github_refresh_token_enc = excluded.github_refresh_token_enc, \
         github_token_expires_at_unix = excluded.github_token_expires_at_unix \
         RETURNING id, login, session_cap"
    )
    .fetch_one()
    .await?;

    Ok(row.into())
}

/// Loads the identity behind a [`UserId`].
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or the stored row is not a
/// valid identity.
pub async fn find(db: &Db, id: UserId) -> Result<Option<CurrentUser>, ApiError> {
    let row: Option<IdentityRow> = sql!(
        db,
        "SELECT id, login, session_cap FROM users WHERE id = {id}"
    )
    .fetch_optional()
    .await?;

    Ok(row.map(Into::into))
}

/// Reads the immutable GitHub user id recorded for `id`.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub(crate) async fn github_id(db: &Db, id: UserId) -> Result<Option<i64>, ApiError> {
    Ok(sql!(db, "SELECT github_id FROM users WHERE id = {id}")
        .fetch_scalar_optional()
        .await?)
}

/// Replaces the stored GitHub grant on `users` when a same-owner Codespaces
/// OAuth flow mints a fresher grant for the signed-in GitHub account.
///
/// # Errors
///
/// Returns [`ApiError`] if sealing or updating the grant fails.
pub(crate) async fn store_github_grant(
    db: &Db,
    config: &ApiConfig,
    id: UserId,
    grant: &GithubGrant,
) -> Result<(), ApiError> {
    let cipher = config.token_cipher();
    let sealed_token = cipher.seal(&grant.token.access_token)?;
    let sealed_refresh = grant
        .refresh_token
        .as_deref()
        .map(|refresh| cipher.seal(refresh))
        .transpose()?;
    let expires_at = grant.expires_at_unix;
    sql!(
        db,
        "UPDATE users SET \
         github_token_enc = {sealed_token}, \
         github_refresh_token_enc = {sealed_refresh}, \
         github_token_expires_at_unix = {expires_at} \
         WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Sets how many sessions a user may hold at once.
///
/// # Errors
///
/// Returns [`ApiError::InvalidSessionCap`] if `cap` is outside
/// [`SESSION_CAP_MIN`]..=[`SESSION_CAP_MAX`], or a database error otherwise.
pub async fn set_session_cap(db: &Db, id: UserId, cap: u32) -> Result<(), ApiError> {
    if !(SESSION_CAP_MIN..=SESSION_CAP_MAX).contains(&cap) {
        return Err(ApiError::InvalidSessionCap {
            min: SESSION_CAP_MIN,
            max: SESSION_CAP_MAX,
        });
    }

    sql!(db, "UPDATE users SET session_cap = {cap} WHERE id = {id}")
        .execute()
        .await?;

    Ok(())
}

/// Reads back the sealed GitHub token stored for a user.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn sealed_github_token(db: &Db, id: UserId) -> Result<Option<String>, ApiError> {
    Ok(
        sql!(db, "SELECT github_token_enc FROM users WHERE id = {id}")
            .fetch_scalar_optional()
            .await?,
    )
}

/// Reads the stored grant row for a user.
///
/// A row is only ever written by a completed sign-in, which always stores
/// the token half — `None` here means the user id named no row, which is
/// corrupt rather than absent.
async fn grant_row(db: &Db, id: UserId) -> Result<Option<GrantRow>, ApiError> {
    Ok(sql!(
        db,
        "SELECT github_id, \
         github_token_enc AS token_enc, \
         github_refresh_token_enc AS refresh_token_enc, \
         github_token_expires_at_unix AS token_expires_at_unix \
         FROM users WHERE id = {id}"
    )
    .fetch_optional()
    .await?)
}

/// Opens a stored grant's access token.
fn access_token(config: &ApiConfig, row: &GrantRow) -> Result<GithubToken, ApiError> {
    Ok(GithubToken {
        access_token: config.token_cipher().open(&row.token_enc)?,
    })
}

/// Opens a stored grant row into all three halves.
fn grant(config: &ApiConfig, row: &GrantRow) -> Result<GithubGrant, ApiError> {
    Ok(GithubGrant {
        token: access_token(config, row)?,
        refresh_token: row
            .refresh_token_enc
            .as_deref()
            .map(|sealed| config.token_cipher().open(sealed))
            .transpose()?,
        expires_at_unix: row.token_expires_at_unix,
    })
}

/// Whether the stored grant is close enough to its end to renew first.
///
/// A grant GitHub never expires — and every row written before flyco kept
/// refresh credentials — carries no `expires_at`, so it answers false and
/// is used until GitHub revokes it.
fn due_for_renewal(row: &GrantRow) -> bool {
    row.token_expires_at_unix
        .is_some_and(|at| at <= now_unix().saturating_add(REFRESH_WINDOW_SECONDS))
}

/// Absorbs any unmigrated same-owner Codespaces `refresh_token` into `users`
/// and clears the duplicate renewal fields from `provider_accounts`.
///
/// Before `users` became the single owner of a GitHub account's OAuth grant,
/// `POST /v1/providers/codespaces/link` copied the single-use rotating
/// `refresh_token` into `provider_accounts.credentials_enc`. Background
/// catalog or reconcile sweeps could then rotate `provider_accounts` ahead of
/// `users`, leaving `users` with a spent refresh token. Reconciling those
/// rows once promotes whichever grant rotated more recently into `users` and
/// strips the duplicate renewal half from `provider_accounts` so they can
/// never race again.
pub(crate) async fn absorb_split_codespaces_grants(
    db: &Db,
    config: &ApiConfig,
    id: UserId,
) -> Result<(), ApiError> {
    let Some(mut user_row) = grant_row(db, id).await? else {
        return Ok(());
    };
    let codespaces = CloudProviderKind::Codespaces;
    let accounts: Vec<CodespacesAccountRow> = sql!(
        db,
        "SELECT id, credentials_enc FROM provider_accounts \
         WHERE user_id = {id} AND kind = {codespaces} AND unlinked_at_unix IS NULL"
    )
    .fetch_all()
    .await?;
    if accounts.is_empty() {
        return Ok(());
    }

    let cipher = config.token_cipher();
    for account in accounts {
        let Ok(plain) = cipher.open(&account.credentials_enc) else {
            continue;
        };
        let Ok(mut secrets) = serde_json::from_str::<StoredSecrets>(&plain) else {
            continue;
        };
        let ProviderCredentials::Codespaces {
            token,
            refresh_token,
            token_expires_at_unix,
            owner_id,
            ..
        } = &mut secrets.credentials
        else {
            continue;
        };
        if *owner_id != user_row.github_id
            || (refresh_token.is_none() && token_expires_at_unix.is_none())
        {
            continue;
        }

        let provider_is_newer = match (*token_expires_at_unix, user_row.token_expires_at_unix) {
            (Some(_), None) => user_row.refresh_token_enc.is_none(),
            (Some(provider_exp), Some(user_exp)) => provider_exp > user_exp,
            (None, _) => false,
        };
        if provider_is_newer {
            let sealed_token = cipher.seal(token)?;
            let sealed_refresh = refresh_token
                .as_deref()
                .map(|refresh| cipher.seal(refresh))
                .transpose()?;
            let next_exp = *token_expires_at_unix;
            let expected_user_token = user_row.token_enc.clone();
            let written = sql!(
                db,
                "UPDATE users SET \
                 github_token_enc = {sealed_token.clone()}, \
                 github_refresh_token_enc = {sealed_refresh.clone()}, \
                 github_token_expires_at_unix = {next_exp} \
                 WHERE id = {id} AND github_token_enc = {expected_user_token}"
            )
            .execute()
            .await?
            .rows_written;
            if written > 0 {
                user_row.token_enc = sealed_token;
                user_row.refresh_token_enc = sealed_refresh;
                user_row.token_expires_at_unix = next_exp;
                tracing::info!(
                    user = %id,
                    account = %account.id,
                    "promoted a newer same-owner Codespaces GitHub grant into users"
                );
            } else if let Some(reloaded) = grant_row(db, id).await? {
                user_row = reloaded;
            }
        }

        *refresh_token = None;
        *token_expires_at_unix = None;
        let resealed = secrets.seal(config)?;
        let account_id = account.id;
        sql!(
            db,
            "UPDATE provider_accounts SET credentials_enc = {resealed} \
             WHERE id = {account_id} AND credentials_enc = {account.credentials_enc}"
        )
        .execute()
        .await?;
    }
    Ok(())
}

/// Redeems `row`'s `refresh_token` and CAS-writes the rotated grant back to
/// `users`, re-reading the winner if another request rotated it concurrently.
async fn renew_grant_row(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    id: UserId,
    row: &GrantRow,
    sealed_refresh: &str,
) -> Result<GithubGrant, ApiError> {
    let refresh_token = config.token_cipher().open(sealed_refresh)?;
    let renewed = match github
        .refresh(
            config.github_client_id(),
            config.github_client_secret(),
            &refresh_token,
        )
        .await
    {
        Ok(grant) => grant,
        Err(GithubError::Rejected { .. }) => {
            // A refresh token is good exactly once: a refusal on a row that
            // has since changed means a racing renewal already redeemed it
            // and wrote its pair — that pair is the answer.
            let current = grant_row(db, id).await?.ok_or(ApiError::CorruptRecord(
                "the user has no stored GitHub token",
            ))?;
            return if current.token_enc == row.token_enc {
                Err(ApiError::GithubTokenRevoked)
            } else {
                grant(config, &current)
            };
        }
        Err(error) => return Err(error.into()),
    };

    let cipher = config.token_cipher();
    let sealed_token = cipher.seal(&renewed.token.access_token)?;
    let sealed_refresh = renewed
        .refresh_token
        .as_deref()
        .map(|refresh| cipher.seal(refresh))
        .transpose()?;
    let expires_at = renewed.expires_at_unix;
    let expected_token = row.token_enc.clone();
    let written = sql!(
        db,
        "UPDATE users SET \
         github_token_enc = {sealed_token}, \
         github_refresh_token_enc = {sealed_refresh}, \
         github_token_expires_at_unix = {expires_at} \
         WHERE id = {id} AND github_token_enc = {expected_token}"
    )
    .execute()
    .await?
    .rows_written;
    if written == 0 {
        // The row moved under the renewal: another request refreshed the
        // same grant first, and what it stored is the pair GitHub honors
        // next.
        let current = grant_row(db, id).await?.ok_or(ApiError::CorruptRecord(
            "the user has no stored GitHub token",
        ))?;
        return grant(config, &current);
    }

    tracing::info!(user = %id, "renewed a GitHub grant before handing it out");
    Ok(renewed)
}

/// Opens a user's stored GitHub grant whole for a call flyco makes on
/// their behalf.
///
/// The one place the seal is broken: [`github_token`] takes just the
/// bearer half for a single call, while same-account Codespaces provisioning
/// reads the same single grant here so both surfaces share one renewal stream.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the row holds no token,
/// [`ApiError::GithubTokenRevoked`] if the grant can no longer be renewed,
/// or a database or cryptography error otherwise.
pub async fn github_grant(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    id: UserId,
) -> Result<GithubGrant, ApiError> {
    let mut row = grant_row(db, id).await?.ok_or(ApiError::CorruptRecord(
        "the user has no stored GitHub token",
    ))?;
    if !due_for_renewal(&row) {
        return grant(config, &row);
    }

    absorb_split_codespaces_grants(db, config, id).await?;
    row = grant_row(db, id).await?.ok_or(ApiError::CorruptRecord(
        "the user has no stored GitHub token",
    ))?;
    if !due_for_renewal(&row) {
        return grant(config, &row);
    }

    let Some(sealed_refresh) = row.refresh_token_enc.clone() else {
        if row.token_expires_at_unix.is_some_and(|at| at <= now_unix()) {
            return Err(ApiError::GithubTokenRevoked);
        }
        return grant(config, &row);
    };

    renew_grant_row(db, config, github, id, &row, &sealed_refresh).await
}

/// Opens the access token half of a user's stored GitHub grant.
///
/// # Errors
///
/// Returns [`ApiError`] exactly as [`github_grant`] does.
pub async fn github_token(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    id: UserId,
) -> Result<GithubToken, ApiError> {
    github_grant(db, config, github, id)
        .await
        .map(|grant| grant.token)
}

/// Forces one renewal (or picks up a concurrent rotation) after GitHub
/// answered `401` to `rejected`, even when `token_expires_at_unix` had not
/// entered [`REFRESH_WINDOW_SECONDS`] yet.
///
/// Returns `Ok(None)` when the stored credential is a non-expiring token
/// carrying no `refresh_token`, leaving the caller's original `401` handling
/// intact.
pub(crate) async fn force_renew_github_token(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    id: UserId,
    rejected: &GithubToken,
) -> Result<Option<GithubToken>, ApiError> {
    absorb_split_codespaces_grants(db, config, id).await?;
    let row = grant_row(db, id).await?.ok_or(ApiError::CorruptRecord(
        "the user has no stored GitHub token",
    ))?;
    let current = access_token(config, &row)?;
    if current.access_token != rejected.access_token {
        return Ok(Some(current));
    }
    let Some(sealed_refresh) = row.refresh_token_enc.clone() else {
        return Ok(None);
    };
    renew_grant_row(db, config, github, id, &row, &sealed_refresh)
        .await
        .map(|renewed| Some(renewed.token))
}

/// Runs a GitHub API operation with the user's stored token, retrying once
/// with a freshly renewed (or concurrently rotated) token if GitHub answers
/// `HTTP 401`.
///
/// # Errors
///
/// Returns [`ApiError`] if the grant cannot be loaded or renewed, or if the
/// GitHub operation fails.
pub async fn with_github_token<G, T, F, Fut>(
    db: &Db,
    config: &ApiConfig,
    github: &G,
    id: UserId,
    mut op: F,
) -> Result<T, ApiError>
where
    G: GithubOauth,
    F: FnMut(GithubToken) -> Fut,
    Fut: Future<Output = Result<T, GithubError>>,
{
    let token = github_token(db, config, github, id).await?;
    match op(token.clone()).await {
        Ok(value) => Ok(value),
        Err(first @ GithubError::Status { status: 401, .. }) => {
            let Some(fresh) = force_renew_github_token(db, config, github, id, &token).await?
            else {
                return Err(first.into());
            };
            Ok(op(fresh).await?)
        }
        Err(other) => Err(other.into()),
    }
}
