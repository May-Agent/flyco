-- Flyco control plane: per-account monthly spend quotas.
--
-- `provider_accounts.monthly_limit_micros` stores an optional per-account
-- monthly USD cap (in microdollars, matching `budgets.limit_micros`).
-- Indexing `spend_events(at_unix, budget_id)` keeps monthly spend rollups
-- per `provider_account_id` bounded to the current UTC calendar month.

ALTER TABLE provider_accounts
    ADD COLUMN monthly_limit_micros INTEGER
    CHECK (monthly_limit_micros IS NULL OR monthly_limit_micros > 0);

CREATE INDEX spend_events_by_at_unix
    ON spend_events (at_unix, budget_id);
