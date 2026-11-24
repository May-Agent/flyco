-- Flyco control plane: bind `Idempotency-Key` rows to a canonical SHA-256
-- digest of the create payload so reusing a key with different parameters
-- is refused, and index `created_at_unix` for expired-row pruning.
ALTER TABLE idempotency_keys ADD COLUMN request_hash TEXT;
CREATE INDEX idempotency_keys_created_at ON idempotency_keys (created_at_unix);
