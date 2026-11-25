//! End-to-end coverage of per-user R2 storage accounting, hard quota
//! enforcement, and runtime operator quota overrides.

use flyco_core::{Problem, SkillView, StorageUsageBreakdown, StorageUsageView};
use skyzen_services::{Db, Kv, Storage};
use skyzen_test::TestContext;

use crate::daemon_tokens;
use crate::session;
use crate::storage_quota;
use crate::testing::{migrated_router, seed_session, seed_user};

const SMALL_SKILL: &[u8] = b"PK\x03\x04small";
const LARGER_SKILL: &[u8] = b"PK\x03\x04much-larger-skill-bundle";

async fn sign_in(kv: &Kv, user: flyco_core::CurrentUser) -> String {
    session::issue(kv, user.id).await.expect("issue a session")
}

#[skyzen::test]
async fn new_user_reports_default_ten_gb_quota_and_zero_usage(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    _storage: Storage,
) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    let res = client.get("/v1/usage/storage").bearer(&token).send().await;
    res.assert_status(200);
    let usage: StorageUsageView = res.json();
    assert_eq!(usage.used_bytes, 0);
    assert_eq!(usage.quota_bytes, 10_000_000_000);
    assert_eq!(usage.breakdown, StorageUsageBreakdown::default());
}

#[skyzen::test]
async fn skill_upload_overwrite_and_delete_update_usage_accurately(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    _storage: Storage,
) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    let created = client
        .post("/v1/skills?name=lint")
        .bearer(&token)
        .body(SMALL_SKILL)
        .send()
        .await;
    created.assert_status(201);
    let skill: SkillView = created.json();

    let usage: StorageUsageView = client
        .get("/v1/usage/storage")
        .bearer(&token)
        .send()
        .await
        .json();
    assert_eq!(usage.used_bytes, SMALL_SKILL.len() as u64);
    assert_eq!(usage.breakdown.skills_bytes, SMALL_SKILL.len() as u64);

    // Overwriting the same skill replaces its byte count rather than summing both.
    let updated = client
        .post("/v1/skills?name=lint")
        .bearer(&token)
        .body(LARGER_SKILL)
        .send()
        .await;
    updated.assert_status(201);

    let usage: StorageUsageView = client
        .get("/v1/usage/storage")
        .bearer(&token)
        .send()
        .await
        .json();
    assert_eq!(usage.used_bytes, LARGER_SKILL.len() as u64);
    assert_eq!(usage.breakdown.skills_bytes, LARGER_SKILL.len() as u64);

    // Deleting the skill immediately reclaims its quota.
    let deleted = client
        .delete(&format!("/v1/skills/{}", skill.id))
        .bearer(&token)
        .send()
        .await;
    deleted.assert_status(204);

    let usage: StorageUsageView = client
        .get("/v1/usage/storage")
        .bearer(&token)
        .send()
        .await
        .json();
    assert_eq!(usage.used_bytes, 0);
    assert_eq!(usage.breakdown.skills_bytes, 0);
}

#[skyzen::test]
async fn daemon_transcript_and_workdir_patch_writes_are_accounted_and_enforced(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.clone()).await;
    let session = seed_session(&db, &user).await;
    let daemon = daemon_tokens::issue(&db, user.id, session)
        .await
        .expect("mint daemon token");
    let client = ctx.client(router);

    // Tighten runtime quota for this user to 50 bytes.
    storage_quota::set_user_quota_bytes(&db, user.id, Some(50))
        .await
        .expect("override quota");

    let batch0 = b"{\"line\":0}\n"; // 11 bytes
    let batch_res = client
        .put(&format!("/v1/sessions/{session}/transcript/main/batches/0"))
        .bearer(&daemon.token)
        .body(batch0.as_slice())
        .send()
        .await;
    batch_res.assert_status(204);

    let patch = b"diff --git a/a b/a\n"; // 19 bytes
    let patch_res = client
        .put(&format!("/v1/sessions/{session}/workdir-patch?repo=flyco"))
        .bearer(&daemon.token)
        .body(patch.as_slice())
        .send()
        .await;
    patch_res.assert_status(204);

    let usage: StorageUsageView = client
        .get("/v1/usage/storage")
        .bearer(&token)
        .send()
        .await
        .json();
    assert_eq!(usage.quota_bytes, 50);
    assert_eq!(usage.breakdown.transcripts_bytes, batch0.len() as u64);
    assert_eq!(usage.breakdown.workdir_patches_bytes, patch.len() as u64);
    assert_eq!(usage.used_bytes, (batch0.len() + patch.len()) as u64);

    // Writing a 30-byte batch pushes projected total to 60 > 50 and must be refused
    // BEFORE touching R2.
    let too_big = vec![b'x'; 30];
    let refused = client
        .put(&format!("/v1/sessions/{session}/transcript/main/batches/1"))
        .bearer(&daemon.token)
        .body(too_big)
        .send()
        .await;
    refused.assert_status(413);
    let problem: Problem = refused.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/storage-quota-exceeded"
    );
    assert!(
        storage
            .head(&format!("transcripts/{session}/main/00000001.jsonl"))
            .await
            .expect("head")
            .is_none(),
        "rejected write must not land in R2"
    );

    // Raising the runtime default after clearing the override immediately unblocks writes.
    storage_quota::set_default_quota_bytes(&db, 200)
        .await
        .expect("raise default");
    storage_quota::set_user_quota_bytes(&db, user.id, None)
        .await
        .expect("clear override");

    let retry = client
        .put(&format!("/v1/sessions/{session}/transcript/main/batches/1"))
        .bearer(&daemon.token)
        .body(vec![b'x'; 30])
        .send()
        .await;
    retry.assert_status(204);

    let usage: StorageUsageView = client
        .get("/v1/usage/storage")
        .bearer(&token)
        .send()
        .await
        .json();
    assert_eq!(usage.quota_bytes, 200);
    assert_eq!(usage.used_bytes, (batch0.len() + patch.len() + 30) as u64);
}
