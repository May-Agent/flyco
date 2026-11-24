//! Waiting for a provider to finish something it accepted.
//!
//! Every driver has the same problem in a different dialect — Azure hands
//! back an `Azure-AsyncOperation` URL, GCP a zonal operation resource, EC2
//! nothing at all but an instance state that has to be read until it settles
//! — and every one of them needs the same two decisions: how long to wait
//! before the next poll, and when to give up. Those decisions are here so
//! there is one of each rather than three that drift.
//!
//! The provider's own `Retry-After` always wins. The backoff below is what a
//! driver falls back to when the service states nothing, and it is short at
//! first because most operations finish in single-digit seconds and capped
//! low because a machine creation is minutes and polling it every ten
//! seconds costs nothing.

use crate::ProviderError;

/// Backoff between polls when the service states no `Retry-After`.
pub const DEFAULT_BACKOFF_SECONDS: [u32; 4] = [1, 2, 5, 10];

/// Shortest `Retry-After` a driver honours.
///
/// A service answering `Retry-After: 0` would otherwise spin in a tight loop
/// spending the Worker's subrequest budget without pausing or advancing a
/// throttle deadline.
pub const MIN_RETRY_AFTER_SECONDS: u32 = 1;

/// Longest `Retry-After` a driver waits in one sleep.
pub const MAX_RETRY_AFTER_SECONDS: u32 = 60;

/// Maximum pages any single provider pagination walk will follow.
///
/// Guards against a service that issues continuation tokens indefinitely and
/// keeps every walk bounded inside a Worker invocation's subrequest ceiling.
pub const MAX_PAGINATION_PAGES: usize = 40;

/// Longest a single asynchronous operation is followed for.
///
/// At the ten-second polling cap this is forty minutes, which is an order of
/// magnitude beyond the minute or two a machine takes — and a measured Azure
/// spot machine sat in `Creating` for over five, so the ceiling is not
/// theoretical. Past it a driver gives up rather than polling for the life
/// of the process.
pub const MAX_POLL_ATTEMPTS: usize = 240;

/// How many polls one control-plane invocation spends on a build before
/// handing back a [`Continuation`](crate::Continuation).
///
/// A Cloudflare Worker on the free plan may make fifty subrequests in one
/// invocation, and a provision spends some of them before it starts
/// polling: the token, the region policy, the environment, the job. Twelve
/// polls is about a hundred seconds of the default backoff — the minute or
/// two most builds take, so most never yield — and leaves the invocation
/// well inside the ceiling that ended a six-minute cold-image build
/// (issue #257). The continuation that carries on gets twelve more under
/// its own budget, up to [`MAX_POLL_ATTEMPTS`] across all of them.
pub const POLLS_PER_INVOCATION: usize = 12;

/// The delay before the `attempt`-th poll, honouring `Retry-After` first.
///
/// `Retry-After` is clamped to
/// `[MIN_RETRY_AFTER_SECONDS, MAX_RETRY_AFTER_SECONDS]` so a zero value never
/// spins without waiting and an extreme value never stalls a Worker invocation.
#[must_use]
pub const fn poll_delay(retry_after: Option<u32>, attempt: usize) -> u32 {
    if let Some(seconds) = retry_after {
        if seconds < MIN_RETRY_AFTER_SECONDS {
            return MIN_RETRY_AFTER_SECONDS;
        }
        if seconds > MAX_RETRY_AFTER_SECONDS {
            return MAX_RETRY_AFTER_SECONDS;
        }
        return seconds;
    }
    let last = DEFAULT_BACKOFF_SECONDS.len() - 1;
    DEFAULT_BACKOFF_SECONDS[if attempt < last { attempt } else { last }]
}

/// Bounds a provider pagination walk by page count and cursor progress.
///
/// Every paginated read across AWS, Azure, GCP, and Codespaces advances
/// through this helper so a server returning `nextToken` / `nextLink` forever
/// or repeating a previous cursor fails fast instead of exhausting the
/// Worker's subrequest budget.
#[derive(Debug, Clone)]
pub struct PageCursor {
    max_pages: usize,
    pages: usize,
    seen: Vec<String>,
}

impl PageCursor {
    /// A cursor bounded to [`MAX_PAGINATION_PAGES`].
    #[must_use]
    pub const fn new() -> Self {
        Self::with_max_pages(MAX_PAGINATION_PAGES)
    }

    /// A cursor bounded to an explicit page cap.
    #[must_use]
    pub const fn with_max_pages(max_pages: usize) -> Self {
        Self {
            max_pages,
            pages: 0,
            seen: Vec::new(),
        }
    }

    /// A URL-based cursor seeded with the first page URL so a `nextLink`
    /// pointing back to page 1 is caught immediately.
    #[must_use]
    pub fn starting_at(initial_url: &str) -> Self {
        Self {
            max_pages: MAX_PAGINATION_PAGES,
            pages: 0,
            seen: vec![initial_url.to_owned()],
        }
    }

    /// Validates and records the next cursor after reading one page.
    ///
    /// Returns `Ok(None)` when the walk has reached the end, `Ok(Some(cursor))`
    /// when another page should be read, or `Err` when the cursor did not
    /// advance or the page cap was reached.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] if the next cursor repeats one
    /// already visited, or [`ProviderError::Rejected`] if `what` still has
    /// another page after the configured maximum.
    pub fn advance(
        &mut self,
        next: Option<String>,
        what: &'static str,
    ) -> Result<Option<String>, ProviderError> {
        self.pages = self.pages.saturating_add(1);
        let Some(token) = next.filter(|token| !token.is_empty()) else {
            return Ok(None);
        };
        if self.seen.iter().any(|previous| previous == &token) {
            return Err(ProviderError::Malformed(
                "provider pagination cursor did not advance",
            ));
        }
        if self.pages >= self.max_pages {
            return Err(ProviderError::Rejected(format!(
                "{what} exceeded the maximum of {} pages",
                self.max_pages
            )));
        }
        self.seen.push(token.clone());
        Ok(Some(token))
    }
}

impl Default for PageCursor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BACKOFF_SECONDS, MAX_RETRY_AFTER_SECONDS, MIN_RETRY_AFTER_SECONDS, PageCursor,
        poll_delay,
    };
    use crate::ProviderError;

    #[test]
    fn a_retry_after_beats_the_default_backoff() {
        assert_eq!(poll_delay(Some(30), 0), 30);
        assert_eq!(poll_delay(None, 0), DEFAULT_BACKOFF_SECONDS[0]);
        assert_eq!(poll_delay(None, 2), DEFAULT_BACKOFF_SECONDS[2]);
        // The backoff is capped rather than unbounded.
        assert_eq!(poll_delay(None, 99), 10);
    }

    #[test]
    fn retry_after_is_clamped_to_a_safe_range() {
        assert_eq!(poll_delay(Some(0), 0), MIN_RETRY_AFTER_SECONDS);
        assert_eq!(poll_delay(Some(1), 0), 1);
        assert_eq!(poll_delay(Some(999), 0), MAX_RETRY_AFTER_SECONDS);
    }

    #[test]
    fn pagination_stops_on_repeated_cursor_or_page_cap() {
        let mut cursor = PageCursor::with_max_pages(2);
        assert_eq!(
            cursor
                .advance(Some("page-2".to_owned()), "test walk")
                .expect("first hop"),
            Some("page-2".to_owned())
        );
        let repeated = cursor
            .advance(Some("page-2".to_owned()), "test walk")
            .expect_err("stalled cursor must fail");
        assert!(matches!(repeated, ProviderError::Malformed(_)));

        let mut capped = PageCursor::with_max_pages(2);
        assert!(
            capped
                .advance(Some("page-2".to_owned()), "test walk")
                .expect("page 1")
                .is_some()
        );
        let over = capped
            .advance(Some("page-3".to_owned()), "test walk")
            .expect_err("page cap must fail");
        assert!(matches!(over, ProviderError::Rejected(_)));

        let mut url_cursor = PageCursor::starting_at("https://example.test/page=1");
        let loop_back = url_cursor
            .advance(Some("https://example.test/page=1".to_owned()), "url walk")
            .expect_err("repeating initial URL must fail");
        assert!(matches!(loop_back, ProviderError::Malformed(_)));
    }
}
