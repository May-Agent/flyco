//! Linked cloud-provider accounts, the bonus questionnaire, and the cloud
//! usage panel.
//!
//! Flyco provisions into the *user's own* cloud accounts, so linking one is
//! the step between signing in and running anything. Credentials are sealed
//! by [`crate::crypto::TokenCipher`] on the way in and never come back out:
//! no route here returns a credential, and
//! [`ProviderAccountView`](flyco_core::ProviderAccountView) has no field
//! that could hold one.

use std::collections::{HashMap, HashSet};

use askama::Template;
use flyco_core::{
    AwsIamPolicy, CloudProviderKind, CloudUsageView, CurrentUser, HostId, LinkProvider,
    ProviderAccountId, ProviderAccountView, ProviderBonusHint, ProviderCredentials,
    QuickstartAnswers, UpdateProviderAccount, Usd, UserId,
};
use flyco_provider::LoginKey;
use flyco_provider::aws::iam;
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv, Queue};

use crate::bonuses;
use crate::catalog;
use crate::clock::now_unix;
use crate::clouds::{CloudLink as _, Clouds};
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::github::{GithubClient, GithubOauth};
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};

/// What `credentials_enc` holds once an account has been unlinked.
///
/// The column is `NOT NULL` and cannot be relaxed without rebuilding a
/// table every machine row references, so the scrub writes the one string
/// that is not a sealed anything. Nothing ever unseals it — every read that
/// resolves an account to provision through skips unlinked rows — and if
/// one ever did, [`crate::crypto::TokenCipher::open`] rejects it as
/// truncated. A missed filter is therefore a loud failure rather than a
/// silent provision against a credential the user withdrew.
const SCRUBBED_CREDENTIAL: &str = "";

/// The columns every read on this path projects.
///
/// `credentials_enc` is deliberately absent: a credential that is never
/// selected cannot be leaked by a later edit to a response type.
#[derive(Debug, skyzen::FromRow)]
struct AccountRow {
    id: ProviderAccountId,
    kind: CloudProviderKind,
    label: String,
    linked_at_unix: u64,
    host_id: Option<HostId>,
    monthly_limit_micros: Option<Usd>,
}

#[derive(Debug, skyzen::FromRow)]
struct AccountSpendRow {
    provider_account_id: ProviderAccountId,
    spent_micros: Usd,
}

impl AccountRow {
    fn into_view(self, monthly_spent: Usd) -> ProviderAccountView {
        ProviderAccountView {
            id: self.id,
            kind: self.kind,
            label: self.label,
            linked_at_unix: self.linked_at_unix,
            host_id: self.host_id,
            monthly_limit: self.monthly_limit_micros,
            monthly_spent,
        }
    }
}

/// First second (`00:00:00`) of the UTC calendar month containing `now_unix`.
///
/// Uses Howard Hinnant's civil-from-days / days-from-civil bijection so the
/// Worker needs no calendar crate.
#[must_use]
pub(crate) const fn utc_month_start_unix(now_unix: u64) -> u64 {
    const SECONDS_PER_DAY: u64 = 86_400;
    let days = (now_unix / SECONDS_PER_DAY).cast_signed();
    // Shift epoch from 1970-01-01 to 0000-03-01 (719_468 days earlier).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097).cast_unsigned();
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe.cast_signed() + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    // Convert (year, month, 1) back to days since 1970-01-01.
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let target_era = if shifted_year >= 0 {
        shifted_year
    } else {
        shifted_year - 399
    } / 400;
    let year_of_era = (shifted_year - target_era * 400).cast_unsigned();
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let first_day = target_era * 146_097 + day_of_era.cast_signed() - 719_468;
    if first_day <= 0 {
        0
    } else {
        first_day.cast_unsigned() * SECONDS_PER_DAY
    }
}

/// Formats a `Usd` microdollar amount as `$X.XX` for error messages.
fn format_usd(amount: Usd) -> String {
    let micros = amount.micros();
    let cents = micros / 10_000;
    let dollars = cents / 100;
    let rem_cents = cents % 100;
    format!("${dollars}.{rem_cents:02}")
}

/// Validates an optional monthly spend cap: `Some(0)` is rejected because a
/// zero cap cannot fund anything; callers who want no cap omit or clear it.
const fn validate_monthly_limit(limit: Option<Usd>) -> Result<Option<Usd>, ApiError> {
    if let Some(amount) = limit
        && amount.micros() == 0
    {
        return Err(ApiError::InvalidProviderAccountUpdate(
            "monthly_limit must be greater than zero",
        ));
    }
    Ok(limit)
}

/// Metered spend accrued per linked provider account in the current UTC month.
///
/// Cost: 1 indexed D1 read (`spend_events_by_at_unix`).
pub(crate) async fn monthly_spend_by_account(
    db: &Db,
    user: UserId,
    now: u64,
) -> Result<HashMap<ProviderAccountId, Usd>, ApiError> {
    let month_start = utc_month_start_unix(now);
    let rows: Vec<AccountSpendRow> = sql!(
        db,
        "SELECT machines.provider_account_id, COALESCE(SUM(spend_events.amount_micros), 0) AS spent_micros \
         FROM spend_events \
         JOIN budgets ON budgets.id = spend_events.budget_id \
         JOIN machines ON machines.session_id = budgets.session_id \
         JOIN provider_accounts ON provider_accounts.id = machines.provider_account_id \
         WHERE provider_accounts.user_id = {user} \
         AND spend_events.at_unix >= {month_start} \
         GROUP BY machines.provider_account_id"
    )
    .fetch_all()
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.provider_account_id, row.spent_micros))
        .collect())
}

/// Metered spend accrued on one provider account in the current UTC month.
///
/// Cost: 1 indexed D1 read (`spend_events_by_at_unix`).
pub(crate) async fn monthly_spend_for_account(
    db: &Db,
    account: ProviderAccountId,
    now: u64,
) -> Result<Usd, ApiError> {
    let month_start = utc_month_start_unix(now);
    let spent: Usd = sql!(
        db,
        "SELECT COALESCE(SUM(spend_events.amount_micros), 0) \
         FROM spend_events \
         JOIN budgets ON budgets.id = spend_events.budget_id \
         JOIN machines ON machines.session_id = budgets.session_id \
         WHERE machines.provider_account_id = {account} \
         AND spend_events.at_unix >= {month_start}"
    )
    .fetch_scalar()
    .await?;
    Ok(spent)
}

/// Linked accounts of `user` that have a configured monthly cap and have
/// reached or exceeded it in the current UTC month.
///
/// Cost: 1 D1 read for capped accounts + 1 indexed D1 read for monthly spend
/// (skipped when the user has no capped accounts).
pub(crate) async fn exhausted_accounts(
    db: &Db,
    user: UserId,
    now: u64,
) -> Result<HashSet<ProviderAccountId>, ApiError> {
    let capped: Vec<AccountRow> = sql!(
        db,
        "SELECT id, kind, label, linked_at_unix, host_id, monthly_limit_micros \
         FROM provider_accounts \
         WHERE user_id = {user} \
         AND unlinked_at_unix IS NULL \
         AND monthly_limit_micros IS NOT NULL"
    )
    .fetch_all()
    .await?;
    if capped.is_empty() {
        return Ok(HashSet::new());
    }
    let spent = monthly_spend_by_account(db, user, now).await?;
    let mut exhausted = HashSet::new();
    for row in capped {
        if let Some(limit) = row.monthly_limit_micros {
            let current = spent.get(&row.id).copied().unwrap_or(Usd::ZERO);
            if current.micros() >= limit.micros() {
                exhausted.insert(row.id);
            }
        }
    }
    Ok(exhausted)
}

/// Refuses provisioning onto `account` when it has reached its monthly cap.
///
/// Cost: 1 D1 read for the account row + 1 indexed D1 read for monthly spend
/// when `monthly_limit_micros` is set.
pub(crate) async fn check_monthly_quota(
    db: &Db,
    user: UserId,
    account: ProviderAccountId,
    now: u64,
) -> Result<(), ApiError> {
    let row: Option<AccountRow> = sql!(
        db,
        "SELECT id, kind, label, linked_at_unix, host_id, monthly_limit_micros \
         FROM provider_accounts \
         WHERE id = {account} AND user_id = {user} AND unlinked_at_unix IS NULL"
    )
    .fetch_optional()
    .await?;
    let Some(row) = row else {
        return Err(ApiError::ProviderAccountNotFound);
    };
    let Some(limit) = row.monthly_limit_micros else {
        return Ok(());
    };
    let spent = monthly_spend_for_account(db, account, now).await?;
    if spent.micros() >= limit.micros() {
        return Err(ApiError::ProviderMonthlyQuotaExceeded {
            label: row.label,
            spent: format_usd(spent),
            limit: format_usd(limit),
        });
    }
    Ok(())
}

/// Narrows the cloud usage panel to one provider.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct CloudUsageFilter {
    /// Only accounts with this provider.
    pub provider: Option<CloudProviderKind>,
}

/// Lists the caller's linked cloud-provider accounts.
#[skyzen::openapi]
async fn list_providers(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<ProviderAccountView>>> {
    list(&db, user.id).await.map(Json).into()
}

async fn list(db: &Db, user: UserId) -> Result<Vec<ProviderAccountView>, ApiError> {
    // Two ways an account row outlives the thing it provisions through, and
    // neither is one to list. A drained host keeps its account because the
    // machines that ran there still point at it; an unlinked account keeps
    // its row for exactly the same reason, and is told apart by the stamp
    // rather than by the credential it no longer holds.
    let removed = flyco_core::HostState::Removed;
    let rows: Vec<AccountRow> = sql!(
        db,
        "SELECT provider_accounts.id, kind, provider_accounts.label, linked_at_unix, host_id, \
                provider_accounts.monthly_limit_micros \
         FROM provider_accounts \
         LEFT JOIN hosts ON hosts.id = provider_accounts.host_id \
         WHERE provider_accounts.user_id = {user} \
         AND provider_accounts.unlinked_at_unix IS NULL \
         AND (host_id IS NULL OR hosts.state != {removed}) \
         ORDER BY linked_at_unix DESC, provider_accounts.id"
    )
    .fetch_all()
    .await?;

    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let spent = monthly_spend_by_account(db, user, now_unix()).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let current = spent.get(&row.id).copied().unwrap_or(Usd::ZERO);
            row.into_view(current)
        })
        .collect())
}

/// Links a cloud-provider account, sealing its credentials at rest.
#[skyzen::openapi]
async fn link_provider(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(clouds): State<Clouds>,
    Json(request): Json<LinkProvider>,
    db: Db,
    kv: Kv,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    link(&db, &config, &clouds, &kv, &queue, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

/// Links one account: prove the credentials, prepare what flyco owns inside
/// the account, seal, store.
///
/// The one store path, shared by `POST /v1/providers` and by both
/// "Sign in with…" flows, so validation and storage stay one rule rather
/// than three that drift.
///
/// # Errors
///
/// Returns [`ApiError`] if the provider refuses the credentials, the
/// credentials cannot be sealed, or the write fails.
pub(crate) async fn link(
    db: &Db,
    config: &ApiConfig,
    clouds: &Clouds,
    kv: &Kv,
    queue: &Queue,
    user: UserId,
    request: LinkProvider,
) -> Result<ProviderAccountView, ApiError> {
    let monthly_limit = validate_monthly_limit(request.monthly_limit)?;
    // Flyco's, minted here: the browser never sees a key, and the user has
    // nowhere to keep one.
    let login_key = LoginKey::generate();
    let resource_group = clouds.prepare(&request.credentials, &login_key).await?;
    let view = create_with(
        db,
        config,
        user,
        request.label,
        SealedSecrets {
            credentials: &request.credentials,
            machine_login_key: &login_key,
        },
        resource_group,
        None,
        monthly_limit,
    )
    .await?;

    // The account is linked and its catalog has never been read, so the
    // read starts now rather than when somebody first looks: the next screen
    // the user sees is the compute card, and it is asking what this account
    // can deploy.
    catalog::ask_for_refresh(kv, queue, user, view.id).await?;
    Ok(view)
}

/// What `credentials_enc` seals: the credentials the user presented, and
/// the machine login key flyco minted for the account.
///
/// Borrowed for sealing, so a link does not clone secrets it is about to
/// encrypt; [`StoredSecrets`] is the same document read back.
#[derive(Serialize)]
struct SealedSecrets<'a> {
    credentials: &'a ProviderCredentials,
    machine_login_key: &'a LoginKey,
}

/// [`SealedSecrets`], unsealed.
#[derive(Deserialize)]
pub(crate) struct StoredSecrets {
    pub(crate) credentials: ProviderCredentials,
    pub(crate) machine_login_key: LoginKey,
}

impl StoredSecrets {
    /// The sealed document this value writes back as.
    ///
    /// The renewal path's half of `create`: a grant refreshed at load time
    /// is resealed whole, so the row keeps carrying every field it was
    /// written with rather than only the one that changed.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] if the credentials cannot be encoded or sealed.
    pub(crate) fn seal(&self, config: &ApiConfig) -> Result<String, ApiError> {
        Ok(config.token_cipher().seal(
            &serde_json::to_string(&SealedSecrets {
                credentials: &self.credentials,
                machine_login_key: &self.machine_login_key,
            })
            .map_err(|_| ApiError::CorruptRecord("credentials could not be encoded"))?,
        )?)
    }
}

/// Writes the account row a set of credentials opens.
///
/// Shared with host enrollment, which links an account of its own without
/// going anywhere near `POST /v1/providers`: a machine the user owns has to
/// be a provider account, or the compute chip, the curated catalog, session
/// creation and the usage panel would each need a special case for it.
///
/// # Errors
///
/// Returns [`ApiError`] if the credentials cannot be sealed or the write
/// fails.
pub(crate) async fn create(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    label: String,
    credentials: &ProviderCredentials,
    host: Option<HostId>,
) -> Result<ProviderAccountView, ApiError> {
    let login_key = LoginKey::generate();
    create_with(
        db,
        config,
        user,
        label,
        SealedSecrets {
            credentials,
            machine_login_key: &login_key,
        },
        None,
        host,
        None,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal insert helper carrying every column written at link time"
)]
async fn create_with(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    label: String,
    secrets: SealedSecrets<'_>,
    resource_group: Option<String>,
    host: Option<HostId>,
    monthly_limit: Option<Usd>,
) -> Result<ProviderAccountView, ApiError> {
    let kind = secrets.credentials.kind();
    let sealed = config.token_cipher().seal(
        &serde_json::to_string(&secrets)
            .map_err(|_| ApiError::CorruptRecord("credentials could not be encoded"))?,
    )?;

    let id = ProviderAccountId::generate();
    let linked_at = now_unix();
    sql!(
        db,
        "INSERT INTO provider_accounts \
         (id, user_id, kind, label, credentials_enc, resource_group, host_id, \
          linked_at_unix, monthly_limit_micros) \
         VALUES ({id}, {user}, {kind}, {label.clone()}, {sealed}, {resource_group}, {host}, \
                 {linked_at}, {monthly_limit})"
    )
    .execute()
    .await?;

    tracing::info!(?kind, "linked a provider account");

    Ok(ProviderAccountView {
        id,
        kind,
        label,
        linked_at_unix: linked_at,
        host_id: host,
        monthly_limit,
        monthly_spent: Usd::ZERO,
    })
}

/// Updates a linked cloud-provider account's label or monthly spend quota.
///
/// Cost per call: 2 D1 reads (account lookup + current-month spend rollup),
/// 1 D1 write (account update), 0 KV / DO / R2 / subrequests.
#[skyzen::openapi]
async fn update_provider(
    State(user): State<CurrentUser>,
    params: Params,
    Json(request): Json<UpdateProviderAccount>,
    db: Db,
) -> Outcome<Json<ProviderAccountView>> {
    update(&db, user.id, &params, request)
        .await
        .map(Json)
        .into()
}

async fn update(
    db: &Db,
    user: UserId,
    params: &Params,
    request: UpdateProviderAccount,
) -> Result<ProviderAccountView, ApiError> {
    let id: ProviderAccountId = path_id(params, "id")?;
    if request.clear_monthly_limit && request.monthly_limit.is_some() {
        return Err(ApiError::InvalidProviderAccountUpdate(
            "cannot set monthly_limit and clear_monthly_limit together",
        ));
    }
    let next_label = match request.label {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(ApiError::InvalidProviderAccountUpdate(
                    "label cannot be empty",
                ));
            }
            Some(trimmed.to_owned())
        }
        None => None,
    };
    let next_limit = validate_monthly_limit(request.monthly_limit)?;
    if next_label.is_none() && next_limit.is_none() && !request.clear_monthly_limit {
        return Err(ApiError::EmptyUpdate);
    }

    let existing: Option<AccountRow> = sql!(
        db,
        "SELECT id, kind, label, linked_at_unix, host_id, monthly_limit_micros \
         FROM provider_accounts \
         WHERE id = {id} AND user_id = {user} AND unlinked_at_unix IS NULL"
    )
    .fetch_optional()
    .await?;
    let Some(mut row) = existing else {
        return Err(ApiError::ProviderAccountNotFound);
    };

    if let Some(label) = next_label {
        row.label = label;
    }
    if request.clear_monthly_limit {
        row.monthly_limit_micros = None;
    } else if let Some(limit) = next_limit {
        row.monthly_limit_micros = Some(limit);
    }

    sql!(
        db,
        "UPDATE provider_accounts \
         SET label = {row.label.clone()}, monthly_limit_micros = {row.monthly_limit_micros} \
         WHERE id = {id} AND user_id = {user} AND unlinked_at_unix IS NULL"
    )
    .execute()
    .await?;

    let spent = monthly_spend_for_account(db, id, now_unix()).await?;
    Ok(row.into_view(spent))
}

/// Unlinks a cloud-provider account.
///
/// The row survives the unlink and the credential does not. Every machine
/// flyco ever built there still names this account — that is the spend
/// history the budget ledger explains — so deleting the row would either
/// fail the foreign key holding the history together or, if it cascaded,
/// erase it. What is deleted is the only part that matters: the sealed
/// credential is scrubbed, the account is stamped unlinked, and it stops
/// appearing anywhere flyco offers something to provision through.
///
/// Linking the same cloud account again writes a new row. This one is
/// history from here on, and reading it back by id is a 404 like any
/// account the caller does not have.
#[skyzen::openapi]
async fn unlink_provider(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    unlink(&db, user.id, &params).await.into()
}

async fn unlink(db: &Db, user: UserId, params: &Params) -> Result<NoContent, ApiError> {
    let id: ProviderAccountId = path_id(params, "id")?;

    // Scoped by user so somebody else's account is indistinguishable from
    // one that does not exist — and by the stamp, so an account already
    // unlinked is too. Unlinking twice is not idempotent success: the
    // second call names a row nothing can be done with any more, and
    // answering `204` would report that a credential had just been
    // withdrawn when none was there to withdraw.
    let owned: Option<AccountRow> = sql!(
        db,
        "SELECT id, kind, label, linked_at_unix, host_id, monthly_limit_micros \
         FROM provider_accounts \
         WHERE id = {id} AND user_id = {user} AND unlinked_at_unix IS NULL"
    )
    .fetch_optional()
    .await?;
    if owned.is_none() {
        return Err(ApiError::ProviderAccountNotFound);
    }

    // Unlinking discards the only credentials that can destroy what is
    // running there, so a live machine makes this a 409 rather than a leak
    // nobody can clean up afterwards.
    let destroyed = flyco_core::MachineState::Destroyed;
    let live: u32 = sql!(
        db,
        "SELECT COUNT(*) AS live FROM machines \
         WHERE provider_account_id = {id} AND state != {destroyed}"
    )
    .fetch_scalar()
    .await?;
    if live > 0 {
        return Err(ApiError::ProviderInUse { sessions: live });
    }

    // Scrubbed and stamped in one statement, because a row that had lost
    // its credential without being marked unlinked would be an account the
    // API still offers and nothing can provision through.
    sql!(
        db,
        "UPDATE provider_accounts \
         SET credentials_enc = {SCRUBBED_CREDENTIAL}, unlinked_at_unix = {now_unix()} \
         WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    tracing::info!(account = %id, "unlinked a provider account");
    Ok(NoContent)
}

/// The minimal IAM policy, as a document ready to paste into IAM.
///
/// A compiled template over `flyco_provider::aws::iam::actions()`, which is
/// itself derived from the driver's own call sites, so the policy the wizard
/// shows cannot drift from the permissions the driver needs — see that
/// module for the test that holds it to them.
#[derive(Debug, Template)]
#[template(path = "aws/iam_policy.json", escape = "none")]
struct IamPolicyDocument {
    /// Every action to grant, `service:Action`, sorted.
    actions: Vec<String>,
}

/// Shows the least privilege an AWS access key needs to run flyco sessions.
///
/// Served rather than checked into the frontend because it is a fact about
/// this build of the driver: a copy in a template somewhere else would be
/// right on the day it was written and quietly wrong afterwards.
#[skyzen::openapi]
async fn aws_iam_policy(State(_user): State<CurrentUser>) -> Outcome<Json<AwsIamPolicy>> {
    iam_policy().map(Json).into()
}

fn iam_policy() -> Result<AwsIamPolicy, ApiError> {
    let actions = iam::actions();
    let document = IamPolicyDocument {
        actions: actions.clone(),
    }
    .render()
    .map_err(|_| ApiError::CorruptRecord("the AWS IAM policy template did not render"))?;

    Ok(AwsIamPolicy { document, actions })
}

/// Suggests free-credit programmes the caller qualifies for.
///
/// Two questions decide it — new customer, student — because those are the
/// two facts every provider's credit programme keys on.
#[skyzen::openapi]
async fn provider_quickstart(
    State(_user): State<CurrentUser>,
    Json(answers): Json<QuickstartAnswers>,
) -> Outcome<Json<Vec<ProviderBonusHint>>> {
    Ok(Json(bonuses::matching(&answers))).into()
}

/// Reports metered cloud spend per linked account.
///
/// The provider's own meter is the authority — a total flyco assembled from
/// its machine records would miss the storage, egress and support charges
/// on the same invoice — so every row here was read from the account it
/// describes, over the window it names.
///
/// Not every linked account produces a row. A machine the user owns is
/// hardware they already pay for: flyco meters nothing there and says
/// nothing, rather than reporting a `$0.00` that would read as "this costs
/// nothing".
#[skyzen::openapi]
async fn cloud_usage(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Query(filter): Query<CloudUsageFilter>,
    db: Db,
) -> Outcome<Json<Vec<CloudUsageView>>> {
    usage(&db, &config, &github, user.id, filter.provider)
        .await
        .map(Json)
        .into()
}

/// Reads every linked account's meter into one document.
///
/// An account whose usage cannot be read is skipped with a warning rather
/// than failing the request, exactly as the machine catalog does: one
/// expired credential must not hide every other account's spend.
async fn usage(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    user: UserId,
    provider: Option<CloudProviderKind>,
) -> Result<Vec<CloudUsageView>, ApiError> {
    let accounts = crate::provisioning::accounts_for(db, config, github, user, provider).await?;
    let now = now_unix();

    let mut rows = Vec::new();
    for account in accounts {
        match crate::provisioning::cloud_usage(&account, now).await {
            Ok(Some(spend)) => rows.push(CloudUsageView::of(account.id, account.kind(), spend)),
            Ok(None) => {
                tracing::debug!(
                    account = %account.id,
                    kind = ?account.kind(),
                    "this provider meters nothing on flyco's behalf"
                );
            }
            Err(error) => {
                tracing::warn!(
                    account = %account.id,
                    %error,
                    "skipping a provider account whose usage could not be read"
                );
            }
        }
    }
    Ok(rows)
}

/// The user-scoped provider-account routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/providers".at(list_providers).post(link_provider),
        "/v1/providers/aws/iam-policy".at(aws_iam_policy),
        "/v1/providers/quickstart".post(provider_quickstart),
        "/v1/providers/{id}"
            .patch(update_provider)
            .delete(unlink_provider),
        "/v1/usage/cloud".at(cloud_usage),
    ))
    .into_route_nodes()
}
