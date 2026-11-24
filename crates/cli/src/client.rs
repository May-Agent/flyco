//! The control-plane HTTP client.
//!
//! One small type over zenwave rather than a request builder per call site:
//! every request carries the API key, every failure becomes a [`Failure`]
//! whose text is the server's `application/problem+json` verbatim, and only
//! idempotent `GET`s retry — a mutation that might have happened is never
//! sent twice, because a second `POST /v1/sessions` is a second billed
//! machine.

use core::time::Duration;

use rand::RngExt as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;
use zenwave::{Client as _, Method, ResponseExt as _};

use crate::{Exit, Failure};

/// How many times a retryable `GET` is sent after the first.
const GET_ATTEMPTS: u32 = 3;

/// Floor of the `GET` retry backoff ladder.
const GET_BACKOFF_MIN: Duration = Duration::from_millis(500);

/// Ceiling of the `GET` retry backoff ladder.
const GET_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// The longest a single request may take before the transport gives up.
///
/// Generous because several routes relay to a session's daemon and wait on
/// its answer — the control plane's own deadline sits under this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest `Retry-After` a request will sleep out.
///
/// The per-minute window never names more than a minute or two; a `429`
/// asking for longer is the daily budget spent, and the answer to that is
/// the problem document in the failure — not a process asleep for an hour
/// inside a `GET` retry.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Capped exponential backoff with full jitter (`crates/daemon/src/control/wire.rs`).
#[must_use]
pub(crate) fn backoff(min: Duration, max: Duration, attempt: u32) -> Duration {
    let ceiling = min.saturating_mul(1_u32 << attempt.min(6)).min(max);
    let max_ms = u64::try_from(ceiling.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    let jittered = rand::rng().random_range(1..=max_ms);
    Duration::from_millis(jittered)
}

/// Whether an RFC 9457 problem body names a daily/row budget exhaustion that
/// must stop immediately without retrying.
#[must_use]
pub(crate) fn is_budget_exhausted_body(body: &str) -> bool {
    serde_json::from_str::<flyco_core::Problem>(body).is_ok_and(|problem| {
        problem.kind.ends_with("/request-budget-exhausted")
            || problem.kind.ends_with("/row-budget-exhausted")
    })
}

/// A bearer-token-authenticated handle on the control plane.
#[derive(Debug, Clone)]
pub struct Api {
    base: Url,
    token: Option<String>,
}

/// The verbs the client speaks, so `send` can tell a `GET` worth retrying
/// from everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    const fn method(self) -> Method {
        match self {
            Self::Get => Method::GET,
            Self::Post => Method::POST,
            Self::Put => Method::PUT,
            Self::Patch => Method::PATCH,
            Self::Delete => Method::DELETE,
        }
    }

    /// `GET` alone is retried: it is the only verb here that cannot create
    /// anything, so a retry is a re-read, never a second charge.
    const fn safe(self) -> bool {
        matches!(self, Self::Get)
    }
}

impl Api {
    /// A client rooted at `base` (`https://flyco.dev` unless `FLYCO_API_URL`
    /// said otherwise), presenting `token` as `Authorization: Bearer`.
    #[must_use]
    pub const fn new(base: Url, token: Option<String>) -> Self {
        Self { base, token }
    }

    /// The control-plane root this client talks to.
    #[must_use]
    pub fn base(&self) -> Url {
        self.base.clone()
    }

    /// The absolute URL of `path`, which always begins `/v1/…`.
    fn url(&self, path: &str) -> Result<Url, Failure> {
        self.base
            .join(path)
            .map_err(|error| Failure::usage(format!("bad API path {path}: {error}")))
    }

    /// `GET path` and decode the JSON body. Retried per the contract.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> crate::Outcome<T> {
        self.request::<(), T>(Verb::Get, path, None, &[]).await
    }

    /// `POST path` with a JSON body, decoding the JSON answer.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> crate::Outcome<T> {
        self.request(Verb::Post, path, Some(body), &[]).await
    }

    /// `POST path` with a JSON body and extra headers (`Idempotency-Key`).
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn post_with_headers<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        headers: &[(&str, &str)],
    ) -> crate::Outcome<T> {
        self.request(Verb::Post, path, Some(body), headers).await
    }

    /// `POST path` with no body, accepting an empty `202`/`204` or any `2xx`
    /// status without decoding JSON.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent or
    /// the answer is a problem document.
    pub async fn post_empty(&self, path: &str) -> crate::Outcome<()> {
        self.request_empty::<()>(Verb::Post, path, None, &[]).await
    }

    /// `POST path` with a JSON body, accepting an empty `202`/`204` or any
    /// `2xx` status without decoding JSON.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent or
    /// the answer is a problem document.
    pub async fn post_json_empty<B: Serialize>(&self, path: &str, body: &B) -> crate::Outcome<()> {
        self.request_empty(Verb::Post, path, Some(body), &[]).await
    }

    /// `PATCH path` with a JSON body, decoding the JSON answer.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn patch<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> crate::Outcome<T> {
        self.request(Verb::Patch, path, Some(body), &[]).await
    }

    /// `PUT path` with a JSON body, decoding the JSON answer.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn put<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> crate::Outcome<T> {
        self.request(Verb::Put, path, Some(body), &[]).await
    }

    /// `PUT path` with a JSON body, accepting any `2xx` status without
    /// decoding JSON.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent or
    /// the answer is a problem document.
    pub async fn put_json_empty<B: Serialize>(&self, path: &str, body: &B) -> crate::Outcome<()> {
        self.request_empty(Verb::Put, path, Some(body), &[]).await
    }

    /// `PUT path` with an opaque byte body, answering no document.
    ///
    /// The handoff uploads are the callers: a patch is not JSON and the
    /// route answers `204`, so the JSON path does not fit.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent
    /// or the answer is a problem document.
    pub async fn put_bytes(&self, path: &str, body: Vec<u8>) -> crate::Outcome<()> {
        let url = self.url(path)?;
        let mut client = zenwave::client();
        let mut builder = client
            .put(url.as_str())
            .map_err(|error| transport("PUT", path, error))?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        let response = with_timeout(async move { builder.bytes_body(body).await })
            .await
            .map_err(|error| refused_or_transport("PUT", path, error))?;
        refused("PUT", path, response.status().as_u16(), response).await
    }

    /// `PUT path` streaming a file as the body — the transcript upload's
    /// shape, so a large one is not read into memory twice.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the file cannot be opened,
    /// the request cannot be sent, or the answer is a problem document.
    pub async fn put_file(&self, path: &str, file: &std::path::Path) -> crate::Outcome<()> {
        let url = self.url(path)?;
        let mut client = zenwave::client();
        let mut builder = client
            .put(url.as_str())
            .map_err(|error| transport("PUT", path, error))?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        let builder = builder
            .file_body(file)
            .await
            .map_err(|error| transport("PUT", path, error))?;
        let response = with_timeout(async move { builder.await })
            .await
            .map_err(|error| refused_or_transport("PUT", path, error))?;
        refused("PUT", path, response.status().as_u16(), response).await
    }

    /// `DELETE path`, accepting an empty `204` or any `2xx` status.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent or
    /// the answer is a problem document.
    pub async fn delete(&self, path: &str) -> crate::Outcome<()> {
        self.request_empty::<()>(Verb::Delete, path, None, &[])
            .await
    }

    /// `GET path` and answer with the status code alone.
    ///
    /// The cli-session poll is the caller: `202` pending, `200` carrying the
    /// key, `410` gone — statuses rather than documents, so it needs the
    /// code and the body both.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent.
    pub async fn poll(&self, path: &str) -> crate::Outcome<zenwave::Response> {
        let url = self.url(path)?;
        let mut client = zenwave::client();
        let mut builder = client
            .get(url.as_str())
            .map_err(|error| transport("GET", path, error))?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        with_timeout(async move { builder.await })
            .await
            .map_err(|error| refused_or_transport("GET", path, error))
    }

    /// `GET path` as a raw SSE response body.
    ///
    /// The response's status is checked before the body is handed over —
    /// an SSE parser would otherwise parse a problem document as event data.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent or
    /// the answer is a problem document.
    pub async fn sse(&self, path: &str) -> crate::Outcome<zenwave::Body> {
        let url = self.url(path)?;
        let mut client = zenwave::client();
        let mut builder = client
            .get(url.as_str())
            .map_err(|error| transport("GET", path, error))?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        let response = with_timeout(async move { builder.await })
            .await
            .map_err(|error| refused_or_transport("GET", path, error))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let wait = refused_wait(&response, status);
            let body = response.into_string().await.unwrap_or_default();
            return Err(problem("GET", path, status, body.as_str(), wait));
        }
        Ok(response.into_body())
    }

    /// Sends a request without reading or decoding a response body.
    async fn request_empty<B: Serialize>(
        &self,
        verb: Verb,
        path: &str,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> crate::Outcome<()> {
        self.send_retried(verb, path, body, headers).await?;
        Ok(())
    }

    /// One request, decoding a JSON body on success.
    async fn request<B: Serialize, T: DeserializeOwned>(
        &self,
        verb: Verb,
        path: &str,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> crate::Outcome<T> {
        let response = self.send_retried(verb, path, body, headers).await?;
        response
            .into_json::<T>()
            .await
            .map_err(|error| transport(verb_name(verb), path, error).with_transient(false))
    }

    /// One request, retried when the verb allows it and the failure says to.
    async fn send_retried<B: Serialize>(
        &self,
        verb: Verb,
        path: &str,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> crate::Outcome<zenwave::Response> {
        let url = self.url(path)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send(verb, &url, body, headers).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    let retryable = verb.safe() && attempt <= GET_ATTEMPTS;
                    let Some(wait) = retry_wait(&error, retryable, attempt) else {
                        return Err(refused_or_transport(verb_name(verb), path, error));
                    };
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    async fn send<B: Serialize>(
        &self,
        verb: Verb,
        url: &Url,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> Result<zenwave::Response, zenwave::Error> {
        let mut client = zenwave::client();
        let mut builder = client.method(verb.method(), url.as_str())?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value)?;
        }
        let builder = match body {
            Some(body) => builder.json_body(body)?,
            None => builder,
        };
        with_timeout(async move { builder.await?.error_for_status().await }).await
    }
}

/// `REQUEST_TIMEOUT` around one request's send-and-headers await.
async fn with_timeout<F, T>(future: F) -> Result<T, zenwave::Error>
where
    F: core::future::Future<Output = Result<T, zenwave::Error>>,
{
    match tokio::time::timeout(REQUEST_TIMEOUT, future).await {
        Ok(done) => done,
        Err(_elapsed) => Err(zenwave::Error::Timeout),
    }
}

const fn verb_name(verb: Verb) -> &'static str {
    match verb {
        Verb::Get => "GET",
        Verb::Post => "POST",
        Verb::Put => "PUT",
        Verb::Patch => "PATCH",
        Verb::Delete => "DELETE",
    }
}

/// What a failed request's answer says about trying again.
#[derive(Debug, Clone, Copy)]
enum Retry {
    /// The failure is final — a 4xx, budget exhaustion, or a body that did not decode.
    Never,
    /// Retryable, and the server named no wait: climb the backoff ladder.
    Default,
    /// Retryable after exactly this long — `Retry-After` honored.
    After(Duration),
}

/// The wait before `attempt`'s retry, or `None` when the failure is
/// final. A named `Retry-After` is honored up to [`MAX_RETRY_AFTER`] —
/// a `429` asking for more is the daily budget spent, and the answer to
/// that is the problem document, not a sleeping process.
fn retry_wait(error: &zenwave::Error, retryable: bool, attempt: u32) -> Option<Duration> {
    if !retryable {
        return None;
    }
    match retry_after(error) {
        Retry::After(named) if named <= MAX_RETRY_AFTER => Some(named),
        Retry::Default => Some(backoff(GET_BACKOFF_MIN, GET_BACKOFF_MAX, attempt - 1)),
        _ => None,
    }
}

/// The wait a failed request asked for, if the failure is one worth
/// waiting out: `429` (except budget exhaustion) and `5xx` honor `Retry-After`,
/// and a transport, I/O, or timeout failure retries on the default ladder.
fn retry_after(error: &zenwave::Error) -> Retry {
    match error {
        zenwave::Error::Http {
            status, response, ..
        } => {
            if response
                .body_text
                .as_deref()
                .is_some_and(is_budget_exhausted_body)
            {
                return Retry::Never;
            }
            let retryable = status.as_u16() == 429 || status.is_server_error();
            if !retryable {
                return Retry::Never;
            }
            named_wait(&response.response).map_or(Retry::Default, Retry::After)
        }
        zenwave::Error::Transport(_)
        | zenwave::Error::Tls(_)
        | zenwave::Error::Timeout
        | zenwave::Error::Io(_)
        | zenwave::Error::Proxy(_) => Retry::Default,
        _ => Retry::Never,
    }
}

/// The `Retry-After` seconds a response named, if it named a number —
/// the header is also allowed to be an HTTP date, which this control
/// plane never sends.
pub(crate) fn named_wait(response: &zenwave::Response) -> Option<Duration> {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// The wait a refused request reports: `Retry-After` on a `429`, where it
/// names the budget window's wait. Other statuses either honor the header
/// silently on the retry ladder (`5xx`) or have no wait to report.
fn refused_wait(response: &zenwave::Response, status: u16) -> Option<Duration> {
    if status == 429 {
        named_wait(response)
    } else {
        None
    }
}

/// The status check the raw-body paths share: a success is `()`, anything
/// else is a problem document to repeat.
async fn refused(
    verb: &'static str,
    path: &str,
    status: u16,
    response: zenwave::Response,
) -> crate::Outcome<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    let wait = refused_wait(&response, status);
    let body = response.into_string().await.unwrap_or_default();
    Err(problem(verb, path, status, &body, wait))
}

/// A transport-level failure: nothing was answered, so there is no problem
/// document to repeat. Takes anything displayable — a body that failed to
/// decode is the same kind of "no usable answer" as a dropped connection.
fn transport(verb: &'static str, path: &str, error: impl core::fmt::Display) -> Failure {
    Failure::transport(format!("{verb} {path}: {error}"))
}

/// A refused request: repeat the problem document verbatim when there is
/// one, name the status when there is not, and let the status pick the
/// exit code — auth refusals are 3, not-found and conflict are 4, every
/// other problem is 5.
fn refused_or_transport(verb: &'static str, path: &str, error: zenwave::Error) -> Failure {
    if let zenwave::Error::Http {
        status, response, ..
    } = &error
    {
        let body = response.body_text.as_deref().unwrap_or("");
        let wait = refused_wait(&response.response, status.as_u16());
        return problem(verb, path, status.as_u16(), body, wait);
    }
    let is_transient = matches!(
        &error,
        zenwave::Error::Transport(_)
            | zenwave::Error::Tls(_)
            | zenwave::Error::Timeout
            | zenwave::Error::Io(_)
            | zenwave::Error::Proxy(_)
    );
    transport(verb, path, error).with_transient(is_transient)
}

pub(crate) fn problem(
    verb: &str,
    path: &str,
    status: u16,
    body: &str,
    retry_after: Option<Duration>,
) -> Failure {
    let code = match status {
        401 | 403 => Exit::Auth,
        404 | 409 | 410 => Exit::NotFoundOrConflict,
        _ => Exit::Problem,
    };
    let budget_exhausted = is_budget_exhausted_body(body);
    let transient = !budget_exhausted && (500..600).contains(&status);
    let text = if body.trim().is_empty() {
        format!("{verb} {path}: HTTP {status}")
    } else {
        body.trim().to_owned()
    };
    Failure::refused(code, text, retry_after)
        .with_transient(transient)
        .with_budget_exhausted(budget_exhausted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    async fn serve_responses(responses: Vec<String>) -> (Url, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
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
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => request.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let _ = socket.write_all(answer.as_bytes()).await;
                });
            }
        });
        (
            Url::parse(&format!("http://127.0.0.1:{port}")).expect("url"),
            count,
        )
    }

    fn empty_status(status: u16, reason: &str) -> String {
        format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
    }

    /// A `429` failure carrying `Retry-After: <secs>`.
    fn rate_limited(secs: u64, kind: &str) -> zenwave::Error {
        let status = zenwave::StatusCode::TOO_MANY_REQUESTS;
        let body = serde_json::json!({
            "type": format!("https://flyco.dev/problems/{kind}"),
            "title": kind,
            "status": 429,
            "detail": kind,
        })
        .to_string();
        let mut response = zenwave::Response::new(zenwave::Body::from_bytes(body.clone()));
        *response.status_mut() = status;
        response.headers_mut().insert(
            "retry-after",
            zenwave::header::HeaderValue::from_str(&secs.to_string())
                .expect("a number is a header value"),
        );
        zenwave::Error::Http {
            status,
            message: "too many requests".to_owned(),
            response: Box::new(zenwave::error::HttpErrorResponse {
                response,
                body_text: Some(body),
            }),
        }
    }

    /// A `Retry-After` past [`MAX_RETRY_AFTER`] is the daily budget spent:
    /// `retry_wait` treats it like a final failure — `None`, never a
    /// sleep — while a wait inside the cap for `rate-limited` is honored verbatim.
    /// `request-budget-exhausted` and `row-budget-exhausted` stop immediately even
    /// when `Retry-After` is small (e.g. near UTC midnight).
    #[test]
    fn retry_after_over_the_cap_or_budget_exhausted_is_never_retried() {
        let over = rate_limited(MAX_RETRY_AFTER.as_secs() + 1, "rate-limited");
        assert!(retry_wait(&over, true, 1).is_none());
        let under = rate_limited(30, "rate-limited");
        assert_eq!(retry_wait(&under, true, 1), Some(Duration::from_secs(30)));
        let exhausted = rate_limited(5, "request-budget-exhausted");
        assert!(retry_wait(&exhausted, true, 1).is_none());
        let row_exhausted = rate_limited(5, "row-budget-exhausted");
        assert!(retry_wait(&row_exhausted, true, 1).is_none());
    }

    #[test]
    fn io_errors_are_retryable_on_get() {
        let err = zenwave::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        let wait = retry_wait(&err, true, 1).expect("Io errors retry");
        assert!(wait >= Duration::from_millis(1) && wait <= GET_BACKOFF_MIN);
    }

    #[test]
    fn backoff_is_capped_and_jittered() {
        let samples: Vec<Duration> = (0..64)
            .map(|_| backoff(Duration::from_millis(500), Duration::from_secs(2), 5))
            .collect();
        for sample in &samples {
            assert!(*sample >= Duration::from_millis(1));
            assert!(*sample <= Duration::from_secs(2));
        }
        let first = samples[0];
        assert!(samples.iter().any(|s| *s != first), "jitter should vary");
    }

    #[tokio::test]
    async fn empty_202_and_204_responses_succeed_without_json_decoding() {
        let (base, count) = serve_responses(vec![
            empty_status(202, "Accepted"),
            empty_status(204, "No Content"),
            empty_status(202, "Accepted"),
            empty_status(204, "No Content"),
            empty_status(204, "No Content"),
        ])
        .await;
        let api = Api::new(base, None);
        api.post_empty("/v1/sessions/s/machine/stop")
            .await
            .expect("post_empty 202");
        api.post_empty("/v1/sessions/s/interrupt")
            .await
            .expect("post_empty 204");
        api.post_json_empty(
            "/v1/sessions/s/messages",
            &serde_json::json!({"text": "hi"}),
        )
        .await
        .expect("post_json_empty 202");
        api.post_json_empty("/v1/sessions/s/handoff/complete", &serde_json::json!({}))
            .await
            .expect("post_json_empty 204");
        api.delete("/v1/api-keys/k").await.expect("delete 204");
        assert_eq!(count.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn empty_helpers_still_surface_problem_responses() {
        let body = serde_json::json!({
            "type": "https://flyco.dev/problems/request-budget-exhausted",
            "title": "request budget exhausted",
            "status": 429,
            "detail": "daily limit reached",
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 429 Too Many Requests\r\n\
             content-type: application/problem+json\r\n\
             retry-after: 15\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let (base, _) = serve_responses(vec![resp]).await;
        let api = Api::new(base, None);
        let failure = api
            .post_empty("/v1/sessions/s/machine/stop")
            .await
            .expect_err("429 should fail");
        assert_eq!(failure.code, Exit::Problem);
        assert!(failure.is_budget_exhausted());
        assert!(!failure.is_transient());
        assert_eq!(failure.retry_after, Some(Duration::from_secs(15)));
    }

    #[tokio::test(start_paused = true)]
    async fn get_retries_503() {
        let problem_503 =
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                .to_owned();
        let ok_200 = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 8\r\nconnection: close\r\n\r\n{\"ok\":1}".to_owned();
        let (base, count) = serve_responses(vec![problem_503, ok_200]).await;
        let task = tokio::spawn(async move {
            let api = Api::new(base, None);
            api.get::<serde_json::Value>("/v1/me").await
        });
        for _ in 0..200 {
            if task.is_finished() {
                break;
            }
            tokio::time::advance(Duration::from_millis(50)).await;
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        }
        let value = task.await.expect("join").expect("retries 503");
        assert_eq!(value["ok"], 1);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn get_stops_immediately_on_budget_exhausted() {
        let body = serde_json::json!({
            "type": "https://flyco.dev/problems/request-budget-exhausted",
            "title": "request budget exhausted",
            "status": 429,
            "detail": "wait",
        })
        .to_string();
        let exhausted = format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/problem+json\r\nretry-after: 5\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let (base, count) = serve_responses(vec![exhausted]).await;
        let api = Api::new(base, None);
        let err = api
            .get::<serde_json::Value>("/v1/me")
            .await
            .expect_err("budget exhausted stops immediately");
        assert!(err.is_budget_exhausted(), "{}", err.text);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
