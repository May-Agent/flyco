-- Flyco control plane: per-user R2 object storage quota and runtime operator settings.
--
-- Operational quotas live in D1 (`operator_settings` for deployment defaults,
-- `users.storage_quota_bytes` for per-user overrides) so operators can adjust
-- limits at runtime without code changes or redeploys.
--
-- `user_storage_objects` mirrors every user-owned R2 object (transcripts,
-- handoffs, workdir patches, and skill bundles) by its object key so quota
-- enforcement and `GET /v1/usage/storage` cost indexed D1 reads instead of
-- scanning R2, overwrites track exact byte deltas, and deletes reclaim space
-- immediately.

CREATE TABLE operator_settings (
    key             TEXT    PRIMARY KEY,
    value           TEXT    NOT NULL,
    updated_at_unix INTEGER NOT NULL
);

-- Initial per-user R2 storage quota: 10 GB (10,000,000,000 bytes), matching
-- Cloudflare R2's 10 GB-month free tier.
INSERT OR IGNORE INTO operator_settings (key, value, updated_at_unix)
VALUES ('default_storage_quota_bytes', '10000000000', 0);

ALTER TABLE users ADD COLUMN storage_quota_bytes INTEGER;

CREATE TABLE user_storage_objects (
    object_key      TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    session_id      TEXT,
    category        TEXT    NOT NULL CHECK (category IN ('transcript', 'handoff', 'workdir_patch', 'skill')),
    size_bytes      INTEGER NOT NULL CHECK (size_bytes >= 0),
    updated_at_unix INTEGER NOT NULL
);

CREATE INDEX user_storage_objects_by_user ON user_storage_objects (user_id, category);
CREATE INDEX user_storage_objects_by_session ON user_storage_objects (session_id);

-- Backfill existing skill bundles and handoff transcripts already indexed in D1.
INSERT OR IGNORE INTO user_storage_objects (object_key, user_id, session_id, category, size_bytes, updated_at_unix)
SELECT 'skills/' || id || '.zip', user_id, NULL, 'skill', size_bytes, uploaded_at_unix
FROM skills;

INSERT OR IGNORE INTO user_storage_objects (object_key, user_id, session_id, category, size_bytes, updated_at_unix)
SELECT 'handoffs/' || h.session_id || '/transcript', s.user_id, h.session_id, 'handoff', h.transcript_bytes, COALESCE(h.completed_at_unix, h.created_at_unix)
FROM handoffs h
JOIN sessions s ON s.id = h.session_id
WHERE h.transcript_bytes IS NOT NULL;
