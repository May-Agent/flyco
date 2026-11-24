//! `flyco login`, `flyco logout`, `flyco auth status`.
//!
//! Login is a browser-approval handshake: `POST /v1/cli-sessions` opens an
//! attempt (unauthenticated — the whole point is that the CLI holds no
//! credential yet), the user approves it on the PWA's `/cli/authorize`
//! page under their existing session, and the poll that follows collects
//! the minted `fk_` key exactly once.

use flyco_core::{CliSessionKey, CreateCliSession, CurrentUser};
use zenwave::ResponseExt as _;

use crate::client::Api;
use crate::{Exit, Failure, Outcome, creds, out};

/// The poll cadence while the user is deciding.
const POLL_EVERY: core::time::Duration = core::time::Duration::from_secs(2);

/// Most consecutive transient `5xx`/transport failures tolerated while polling.
const MAX_POLL_TRANSIENT: u32 = 3;

/// `flyco login`.
///
/// With `--token`, stores the given key directly — the headless path for
/// agents and SSH'd boxes. Without one, runs the browser handshake.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the control plane refuses or
/// the credentials cannot be stored.
pub async fn login(api: &Api, token: Option<String>, mode: out::Mode) -> Outcome<()> {
    match token {
        Some(key) => login_with_token(api, key, mode).await,
        None => login_browser(api, mode).await,
    }
}

/// `flyco login --token …`: check the key names someone before saving it.
async fn login_with_token(api: &Api, key: String, mode: out::Mode) -> Outcome<()> {
    // The only way to know a key is good is to use it: authenticate as it
    // once, then save it if `me` answered.
    let probe = Api::new(api_base(api), Some(key.clone()));
    let me: CurrentUser = probe.get("/v1/me").await?;
    let credentials = creds::Credentials { key_id: None, key };
    let path = creds::store(&credentials)?;
    match mode {
        out::Mode::Json => out::emit(&serde_json::json!({
            "login": me.login,
            "credentials": path.display().to_string(),
        })),
        out::Mode::Human => out::print(&format!(
            "Signed in as {} — key stored at {}",
            me.login,
            path.display()
        )),
    }
}

/// The browser handshake.
async fn login_browser(api: &Api, mode: out::Mode) -> Outcome<()> {
    let hostname = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let session: flyco_core::CliSession = api
        .post(
            "/v1/cli-sessions",
            &CreateCliSession {
                hostname: Some(hostname),
            },
        )
        .await?;

    match mode {
        // The attempt itself is the answer an agent needs: the URL to open
        // and the poll token are the whole document.
        out::Mode::Json => out::emit(&session)?,
        out::Mode::Human => {
            out::print("Open this page to approve the sign-in:")?;
            out::print(&format!("  {}", session.authorize_url))?;
            if open::that(&session.authorize_url).is_ok() {
                out::print("(opened in your browser — waiting for approval)")?;
            }
        }
    }

    let key = poll_for_key(api, &session).await?;

    let credentials = creds::Credentials {
        key_id: Some(key.key_id),
        key: key.key,
    };
    let path = creds::store(&credentials)?;
    match mode {
        out::Mode::Json => out::emit(&serde_json::json!({
            "key_id": credentials.key_id,
            "credentials": path.display().to_string(),
        })),
        out::Mode::Human => out::print(&format!("Signed in — key stored at {}", path.display())),
    }
}

/// Polls `GET /v1/cli-sessions/{id}?s=<poll_token>` until the attempt is
/// approved, denied, expired, budget-exhausted, or times out.
async fn poll_for_key(api: &Api, session: &flyco_core::CliSession) -> Outcome<CliSessionKey> {
    let deadline = tokio::time::Instant::now()
        + core::time::Duration::from_secs(session.expires_at_unix.saturating_sub(now_unix()) + 30);
    let poll = format!("/v1/cli-sessions/{}?s={}", session.id, session.poll_token);
    let mut transient_failures = 0_u32;

    loop {
        let sleep_for = match api.poll(&poll).await {
            Ok(response) => match response.status().as_u16() {
                200 => {
                    return response
                        .into_json::<CliSessionKey>()
                        .await
                        .map_err(|error| {
                            Failure::transport(format!("the approval answered badly: {error}"))
                        });
                }
                202 => {
                    transient_failures = 0;
                    POLL_EVERY
                }
                403 => {
                    return Err(Failure::problem(
                        Exit::Auth,
                        "the sign-in was denied".to_owned(),
                    ));
                }
                404 | 410 => {
                    return Err(Failure::problem(
                        Exit::NotFoundOrConflict,
                        "the sign-in attempt expired or was already collected".to_owned(),
                    ));
                }
                429 => {
                    let wait = crate::client::named_wait(&response);
                    let body = response.into_string().await.unwrap_or_default();
                    let failure = crate::client::problem("GET", &poll, 429, &body, wait);
                    if failure.is_budget_exhausted()
                        || wait.is_some_and(|w| w > crate::client::MAX_RETRY_AFTER)
                    {
                        return Err(failure);
                    }
                    let delay = wait
                        .unwrap_or_else(|| {
                            crate::client::backoff(
                                POLL_EVERY,
                                crate::client::MAX_RETRY_AFTER,
                                transient_failures,
                            )
                        })
                        .max(POLL_EVERY);
                    transient_failures = transient_failures.saturating_add(1);
                    delay
                }
                status => {
                    let body = response.into_string().await.unwrap_or_default();
                    let failure = crate::client::problem("GET", &poll, status, &body, None);
                    if !failure.is_transient() || transient_failures >= MAX_POLL_TRANSIENT {
                        return Err(failure);
                    }
                    let delay = crate::client::backoff(
                        POLL_EVERY,
                        crate::client::MAX_RETRY_AFTER,
                        transient_failures,
                    );
                    transient_failures = transient_failures.saturating_add(1);
                    delay
                }
            },
            Err(failure) => {
                if !failure.is_transient()
                    || failure.is_budget_exhausted()
                    || transient_failures >= MAX_POLL_TRANSIENT
                {
                    return Err(failure);
                }
                let delay = crate::client::backoff(
                    POLL_EVERY,
                    crate::client::MAX_RETRY_AFTER,
                    transient_failures,
                );
                transient_failures = transient_failures.saturating_add(1);
                delay
            }
        };

        let now = tokio::time::Instant::now();
        if now >= deadline || now + sleep_for > deadline {
            return Err(Failure::problem(
                Exit::Timeout,
                "the sign-in was not approved in time".to_owned(),
            ));
        }
        tokio::time::sleep(sleep_for).await;
    }
}

/// `flyco logout`: revoke the stored key, then drop the file.
///
/// Revocation comes first so a failed `DELETE` keeps the file — the
/// credential still works and a retry can still revoke it.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the control plane refuses or
/// the credentials cannot be stored.
pub async fn logout(api: &Api, mode: out::Mode) -> Outcome<()> {
    let Some(credentials) = creds::resolve() else {
        return match mode {
            out::Mode::Json => out::emit(&serde_json::json!({ "logged_in": false })),
            out::Mode::Human => out::print("Not signed in."),
        };
    };
    if let Some(key_id) = credentials.key_id {
        api.delete(&format!("/v1/api-keys/{key_id}")).await?;
    }
    creds::remove();
    match mode {
        out::Mode::Json => out::emit(&serde_json::json!({ "logged_in": false })),
        out::Mode::Human => out::print("Signed out."),
    }
}

/// `flyco auth status`: who the credential belongs to.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the control plane refuses or
/// the credentials cannot be stored.
pub async fn status(api: &Api, mode: out::Mode) -> Outcome<()> {
    let me: CurrentUser = api.get("/v1/me").await?;
    match mode {
        out::Mode::Json => out::emit(&me),
        out::Mode::Human => out::print(&format!(
            "{} ({} sessions at once)",
            me.login, me.session_cap
        )),
    }
}

/// The base URL the configured client points at, for the `--token` probe.
fn api_base(api: &Api) -> url::Url {
    api.base()
}

/// Seconds since the epoch, for the poll's deadline.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    async fn serve_poll(responses: Vec<String>) -> (url::Url, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("port").port();
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        let responses = Arc::new(responses);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let idx = counter.fetch_add(1, Ordering::SeqCst);
                let answer = responses
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| responses.last().cloned().expect("non-empty"));
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0_u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let _ = socket.write_all(answer.as_bytes()).await;
                });
            }
        });
        (
            url::Url::parse(&format!("http://127.0.0.1:{port}")).expect("url"),
            count,
        )
    }

    fn session_stub() -> flyco_core::CliSession {
        flyco_core::CliSession {
            id: flyco_core::CliSessionId::generate(),
            poll_token: "fc_poll".to_owned(),
            authorize_url: "https://flyco.dev/cli/authorize".to_owned(),
            expires_at_unix: now_unix() + 600,
        }
    }

    fn ok_key() -> String {
        let body = serde_json::json!({
            "key_id": flyco_core::ApiKeyId::generate(),
            "key": "fk_secret",
        })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test(start_paused = true)]
    async fn poll_honors_429_retry_after_and_retries_5xx() {
        let accepted =
            "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned();
        let rate_limited_body = serde_json::json!({
            "type": "https://flyco.dev/problems/rate-limited",
            "title": "rate limited",
            "status": 429,
            "detail": "slow down",
        })
        .to_string();
        let rate_limited = format!(
            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 5\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{rate_limited_body}",
            rate_limited_body.len()
        );
        let unavailable =
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                .to_owned();
        let (base, count) = serve_poll(vec![accepted, rate_limited, unavailable, ok_key()]).await;
        let task = tokio::spawn(async move {
            let api = Api::new(base, None);
            poll_for_key(&api, &session_stub()).await
        });
        for _ in 0..400 {
            if task.is_finished() {
                break;
            }
            tokio::time::advance(core::time::Duration::from_millis(50)).await;
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        }
        let key = task
            .await
            .expect("join")
            .expect("poll succeeds after 202, 429, 503");
        assert_eq!(key.key, "fk_secret");
        assert_eq!(count.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn poll_stops_immediately_on_budget_exhausted_and_handles_403_410() {
        let exhausted_body = serde_json::json!({
            "type": "https://flyco.dev/problems/request-budget-exhausted",
            "title": "request budget exhausted",
            "status": 429,
            "detail": "daily limit reached",
        })
        .to_string();
        let exhausted = format!(
            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 10\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{exhausted_body}",
            exhausted_body.len()
        );
        let (base, count) = serve_poll(vec![exhausted]).await;
        let err = poll_for_key(&Api::new(base, None), &session_stub())
            .await
            .expect_err("budget exhausted stops");
        assert_eq!(err.code, Exit::Problem);
        assert!(err.is_budget_exhausted());
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let denied =
            "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned();
        let (base, _) = serve_poll(vec![denied]).await;
        let err = poll_for_key(&Api::new(base, None), &session_stub())
            .await
            .expect_err("403 fails with auth");
        assert_eq!(err.code, Exit::Auth);

        let gone = "HTTP/1.1 410 Gone\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned();
        let (base, _) = serve_poll(vec![gone]).await;
        let err = poll_for_key(&Api::new(base, None), &session_stub())
            .await
            .expect_err("410 fails with not found");
        assert_eq!(err.code, Exit::NotFoundOrConflict);
    }
}
