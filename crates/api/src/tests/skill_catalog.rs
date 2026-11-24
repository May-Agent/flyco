//! Coverage of marketplaces and the skill catalog.
//!
//! The catalog's contract is that a marketplace is *read* somewhere other
//! than the request that wants it: a first read answers `pending` and
//! enqueues the job, and the job's document is what the next request
//! serves. What these tests pin is that pair, that the built-in
//! marketplace is there for everyone and cannot be removed, that one user's
//! marketplaces are unreachable from another's, and that an install turns a
//! directory in a repository into the same zipped bundle an upload would
//! have produced.

use flyco_core::{
    AddMarketplace, BUILT_IN_MARKETPLACE, CurrentUser, InstallCatalogSkill, MarketplaceView,
    Problem, SkillCatalog, SkillView,
};
use skyzen_services::queue::ReceiveOptions;
use skyzen_services::{Db, Kv, Queue, Storage};
use skyzen_test::mock::InMemoryQueue;
use skyzen_test::{TestClient, TestContext};

use crate::provisioning_queue::ProvisioningJob;
use crate::session;
use crate::testing::{
    FakeRepoFiles, TestGithub, migrate, migrated_router_on, seed_other_user, seed_user,
    test_config, test_router_with_github,
};

/// A marketplace with two plugins: one naming its skills, one relying on
/// the default layout, and one pointing at another repository.
const MANIFEST: &[u8] = br#"{
  "name": "example-marketplace",
  "owner": { "name": "Example" },
  "plugins": [
    { "name": "document-skills", "source": "./", "skills": ["./skills/xlsx"] },
    { "name": "tooling", "source": "./plugins/tooling" },
    { "name": "elsewhere", "source": { "source": "github", "repo": "other/repo" } }
  ]
}"#;

const XLSX_SKILL: &[u8] =
    b"---\nname: xlsx\ndescription: Read and write spreadsheets.\n---\n\n# Spreadsheets\n";
const XLSX_SCRIPT: &[u8] = b"print('hello')\n";
const LINT_SKILL: &[u8] = b"---\nname: lint\ndescription: Lint a checkout.\n---\n";
const REVIEW_SKILL: &[u8] = b"---\nname: review\ndescription: Review a diff.\n---\n";

const MARKET_FILES: FakeRepoFiles = &[
    (".claude-plugin/marketplace.json", MANIFEST),
    ("skills/xlsx/SKILL.md", XLSX_SKILL),
    ("skills/xlsx/scripts/convert.py", XLSX_SCRIPT),
    ("plugins/tooling/skills/lint/SKILL.md", LINT_SKILL),
    // The `elsewhere` plugin's files are in ELSEWHERE_FILES, below.
];

/// The repository the `elsewhere` plugin points at: its skills are the
/// marketplace's too, read one repository further out.
const ELSEWHERE_FILES: FakeRepoFiles = &[("skills/review/SKILL.md", REVIEW_SKILL)];

const MARKET: &str = "example/marketplace";
const ELSEWHERE: &str = "other/repo";

const REPOS: &[(&str, FakeRepoFiles)] = &[(MARKET, MARKET_FILES), (ELSEWHERE, ELSEWHERE_FILES)];

fn github() -> TestGithub {
    TestGithub::default().with_files(REPOS)
}

async fn sign_in(kv: &Kv, user: CurrentUser) -> String {
    session::issue(kv, user.id).await.expect("issue a session")
}

/// Runs the refresh the queue would have run, for the job that was queued.
async fn run_queued_refresh(db: &Db, kv: &Kv, queue: &Queue, jobs: usize) {
    let batch = queue
        .receive_json::<ProvisioningJob>(ReceiveOptions::new().with_max_messages(16))
        .await
        .expect("read the queue");
    assert_eq!(
        batch.len(),
        jobs,
        "one refresh per marketplace that has not been read"
    );
    for message in batch {
        let ProvisioningJob::RefreshMarketplace {
            user,
            repo,
            git_ref,
        } = message.body
        else {
            panic!("the catalog enqueues marketplace refreshes and nothing else");
        };
        let marketplace = crate::marketplaces::Marketplace {
            repo: repo.parse().expect("a slug"),
            git_ref,
        };
        crate::skill_catalog::refresh(db, &test_config(), kv, &github(), user, &marketplace)
            .await
            .expect("read the marketplace");
    }
}

#[skyzen::test]
async fn the_built_in_marketplace_is_listed_for_everybody_and_cannot_be_added(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router_on(&db, Queue::new(InMemoryQueue::new())).await;
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.clone()).await;
    let client = ctx.client(router);

    let listed = client
        .get("/v1/marketplaces")
        .bearer(&token)
        .send()
        .await
        .json::<Vec<MarketplaceView>>();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].repo, BUILT_IN_MARKETPLACE);
    assert!(listed[0].built_in);
    assert_eq!(listed[0].id, None);

    let again = client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: BUILT_IN_MARKETPLACE.to_owned(),
            git_ref: None,
        })
        .send()
        .await;
    again.assert_status(422);
    assert!(
        again
            .json::<Problem>()
            .kind
            .ends_with("invalid-marketplace")
    );
}

#[skyzen::test]
async fn a_marketplace_is_added_once_and_is_the_adder_s_alone(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router_on(&db, Queue::new(InMemoryQueue::new())).await;
    let user = seed_user(&db).await;
    let other = seed_other_user(&db).await;
    let token = sign_in(&kv, user).await;
    let other_token = sign_in(&kv, other).await;
    let client = ctx.client(router);

    let added = client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: Some("main".to_owned()),
        })
        .send()
        .await;
    added.assert_status(201);
    let added = added.json::<MarketplaceView>();
    assert_eq!(added.repo, MARKET);
    assert_eq!(added.git_ref.as_deref(), Some("main"));
    assert!(!added.built_in);

    let clash = client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: None,
        })
        .send()
        .await;
    clash.assert_status(409);
    assert!(
        clash
            .json::<Problem>()
            .kind
            .ends_with("marketplace-already-added")
    );

    // Somebody else's list has only the built-in one, and their delete
    // cannot reach this row.
    let theirs = client
        .get("/v1/marketplaces")
        .bearer(&other_token)
        .send()
        .await
        .json::<Vec<MarketplaceView>>();
    assert_eq!(theirs.len(), 1);
    assert!(theirs[0].built_in);

    let id = added.id.expect("an added marketplace has an id");
    client
        .delete(&format!("/v1/marketplaces/{id}"))
        .bearer(&other_token)
        .send()
        .await
        .assert_status(404);

    client
        .delete(&format!("/v1/marketplaces/{id}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);
    assert_eq!(
        client
            .get("/v1/marketplaces")
            .bearer(&token)
            .send()
            .await
            .json::<Vec<MarketplaceView>>()
            .len(),
        1
    );
}

#[skyzen::test]
async fn a_catalog_reports_pending_until_the_queue_has_read_the_marketplace(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    let _ = storage;
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.clone()).await;
    let client = ctx.client(router);

    client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: Some("main".to_owned()),
        })
        .send()
        .await
        .assert_status(201);

    // Nothing has been read yet: both marketplaces are pending and neither
    // reads as "offers nothing".
    let pending = client
        .get("/v1/catalog/skills")
        .bearer(&token)
        .send()
        .await
        .json::<SkillCatalog>();
    assert_eq!(pending.skills, [] as [flyco_core::CatalogSkill; 0]);
    assert_eq!(pending.pending.len(), 2);
    assert!(pending.pending.contains(&MARKET.to_owned()));
    assert!(pending.pending.contains(&BUILT_IN_MARKETPLACE.to_owned()));

    // Adding asked for one read and the first catalog request asked for the
    // other; a second request within the claim asks for neither.
    run_queued_refresh(&db, &kv, &queue, 2).await;

    let read = client
        .get("/v1/catalog/skills")
        .bearer(&token)
        .send()
        .await
        .json::<SkillCatalog>();
    assert_eq!(read.pending, [] as [std::string::String; 0]);
    // The built-in marketplace is not in the fake GitHub, so it is a
    // failure rather than an empty success.
    assert_eq!(read.failed.len(), 1);
    assert_eq!(read.failed[0].marketplace, BUILT_IN_MARKETPLACE);

    let names: Vec<&str> = read
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    // `review` comes from the repository the `elsewhere` plugin points at,
    // and is listed as one of the marketplace's own.
    assert_eq!(names, vec!["lint", "review", "xlsx"], "{names:?}");
    let xlsx = read
        .skills
        .iter()
        .find(|skill| skill.name == "xlsx")
        .expect("listed");
    assert_eq!(xlsx.description, "Read and write spreadsheets.");
    assert_eq!(xlsx.plugin, "document-skills");
}

/// Adds the fixture marketplace, which every install test starts from.
async fn add_market<E: skyzen::Endpoint + Clone>(client: &TestClient<E>, token: &str) {
    client
        .post("/v1/marketplaces")
        .bearer(token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: Some("main".to_owned()),
        })
        .send()
        .await
        .assert_status(201);
}

/// Asks for one skill, whatever the request is.
fn install(plugin: &str, name: &str) -> InstallCatalogSkill {
    InstallCatalogSkill {
        marketplace: MARKET.to_owned(),
        plugin: plugin.to_owned(),
        name: name.to_owned(),
    }
}

#[skyzen::test]
async fn installing_a_skill_stores_its_directory_as_one_row(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.clone()).await;
    let client = ctx.client(router);

    add_market(&client, &token).await;
    run_queued_refresh(&db, &kv, &queue, 1).await;

    let installed = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("document-skills", "xlsx"))
        .send()
        .await;
    installed.assert_status(201);
    let installed = installed.json::<SkillView>();
    assert_eq!(installed.name, "xlsx");

    // It is now an ordinary skill — one row, because the daemon mounts it
    // into every harness's directory rather than one per harness.
    let listed = client
        .get("/v1/skills")
        .bearer(&token)
        .send()
        .await
        .json::<Vec<SkillView>>();
    assert_eq!(listed.len(), 1);

    // And installing the same skill again replaces that row rather than
    // adding to it: the name keeps the id it already had.
    let again = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("document-skills", "xlsx"))
        .send()
        .await
        .json::<SkillView>();
    assert_eq!(again.id, installed.id);
    let listed = client
        .get("/v1/skills")
        .bearer(&token)
        .send()
        .await
        .json::<Vec<SkillView>>();
    assert_eq!(listed.len(), 1);

    let bundle = storage
        .get(&format!("skills/{}.zip", installed.id))
        .await
        .expect("read the object")
        .expect("the bundle is stored");
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bundle.body)).expect("a readable archive");
    let mut names: Vec<String> = archive
        .file_names()
        .map(|name| name.expect("valid archive entry name").into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["SKILL.md", "scripts/convert.py"]);
    let mut entry = archive.by_name("SKILL.md").expect("the skill file");
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut body).expect("read it back");
    assert_eq!(body, XLSX_SKILL);
}

#[skyzen::test]
async fn an_install_that_cannot_be_served_says_which_of_the_three_it_is(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.clone()).await;
    let client = ctx.client(router);

    add_market(&client, &token).await;

    // Before the read, an install has nothing to install from and says so
    // rather than inventing an answer.
    let early = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("document-skills", "xlsx"))
        .send()
        .await;
    early.assert_status(409);
    assert!(
        early
            .json::<Problem>()
            .kind
            .ends_with("skill-catalog-not-ready")
    );

    run_queued_refresh(&db, &kv, &queue, 1).await;

    // A skill the marketplace does not offer is a 404.
    let unknown = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("document-skills", "nope"))
        .send()
        .await;
    unknown.assert_status(404);
    assert!(
        unknown
            .json::<Problem>()
            .kind
            .ends_with("catalog-skill-not-found")
    );
}

#[skyzen::test]
async fn a_skill_of_a_plugin_in_another_repository_installs_from_that_repository(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.clone()).await;
    let client = ctx.client(router);

    add_market(&client, &token).await;
    run_queued_refresh(&db, &kv, &queue, 1).await;

    let installed = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("elsewhere", "review"))
        .send()
        .await;
    installed.assert_status(201);
    let installed = installed.json::<SkillView>();
    assert_eq!(installed.name, "review");

    let bundle = storage
        .get(&format!("skills/{}.zip", installed.id))
        .await
        .expect("read the object")
        .expect("the bundle is stored");
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bundle.body)).expect("a readable archive");
    let names: Vec<String> = archive
        .file_names()
        .map(|name| name.expect("valid archive entry name").into_owned())
        .collect();
    assert_eq!(names, vec!["SKILL.md"]);
    let mut entry = archive.by_name("SKILL.md").expect("the skill file");
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut body).expect("read it back");
    assert_eq!(body, REVIEW_SKILL);
}

#[skyzen::test]
async fn a_marketplace_somebody_else_added_cannot_be_installed_from(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let other = seed_other_user(&db).await;
    let token = sign_in(&kv, user).await;
    let other_token = sign_in(&kv, other).await;
    let client = ctx.client(router);

    client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: Some("main".to_owned()),
        })
        .send()
        .await
        .assert_status(201);
    run_queued_refresh(&db, &kv, &queue, 1).await;

    let refused = client
        .post("/v1/catalog/skills")
        .bearer(&other_token)
        .json(&install("document-skills", "xlsx"))
        .send()
        .await;
    refused.assert_status(404);
    assert!(
        refused
            .json::<Problem>()
            .kind
            .ends_with("marketplace-not-found")
    );
}

#[skyzen::test]
async fn the_catalog_needs_a_signed_in_user(ctx: TestContext, db: Db) {
    let router = migrated_router_on(&db, Queue::new(InMemoryQueue::new())).await;
    let client = ctx.client(router);
    client
        .get("/v1/catalog/skills")
        .send()
        .await
        .assert_status(401);
    client
        .get("/v1/marketplaces")
        .send()
        .await
        .assert_status(401);
}

#[skyzen::test]
async fn user_added_marketplaces_are_kv_isolated_and_check_github_access(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let owner = seed_user(&db).await;
    let other = seed_other_user(&db).await;
    let owner_token = sign_in(&kv, owner).await;
    let other_token = sign_in(&kv, other).await;
    let client = ctx.client(router);

    // User A adds `MARKET` and refreshes it into KV.
    add_market(&client, &owner_token).await;
    run_queued_refresh(&db, &kv, &queue, 1).await;

    // User B adds the same repository slug: User B must NOT read User A's
    // cached skills from KV — User B's own user-scoped key is still pending.
    add_market(&client, &other_token).await;
    let other_catalog = client
        .get("/v1/catalog/skills")
        .bearer(&other_token)
        .send()
        .await
        .json::<SkillCatalog>();
    assert_eq!(
        other_catalog.skills,
        [] as [flyco_core::CatalogSkill; 0],
        "user B must never see user A's cached marketplace skills"
    );
    assert!(other_catalog.pending.contains(&MARKET.to_owned()));

    // A repository the caller's GitHub token cannot read is refused on add.
    let unreadable_router = test_router_with_github(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        TestGithub {
            unreadable: Some("secret/private-skills"),
            ..github()
        },
    );
    let refused = ctx
        .client(unreadable_router)
        .post("/v1/marketplaces")
        .bearer(&other_token)
        .json(&AddMarketplace {
            repo: "secret/private-skills".to_owned(),
            git_ref: None,
        })
        .send()
        .await;
    refused.assert_status(422);
    assert!(
        refused
            .json::<Problem>()
            .kind
            .ends_with("invalid-marketplace")
    );
}

#[derive(Clone)]
struct ScriptedGithub {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    file_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    fail_auth: bool,
    skill_count: usize,
    bundle_file_count: usize,
}

impl crate::github::GithubOauth for ScriptedGithub {
    fn exchange_code(
        &self,
        _client_id: &str,
        _client_secret: &str,
        _code: &str,
        _redirect_uri: &str,
    ) -> impl Future<Output = Result<crate::github::GithubGrant, crate::github::GithubError>> + Send
    {
        std::future::ready(Err(crate::github::GithubError::Transport {
            call: crate::github::GithubCall::TokenExchange,
            message: "unused in scripted github".to_owned(),
        }))
    }

    fn refresh(
        &self,
        _client_id: &str,
        _client_secret: &str,
        _refresh_token: &str,
    ) -> impl Future<Output = Result<crate::github::GithubGrant, crate::github::GithubError>> + Send
    {
        std::future::ready(Err(crate::github::GithubError::Transport {
            call: crate::github::GithubCall::TokenRefresh,
            message: "unused in scripted github".to_owned(),
        }))
    }

    fn current_user(
        &self,
        _token: &crate::github::GithubToken,
    ) -> impl Future<Output = Result<crate::github::GithubIdentity, crate::github::GithubError>> + Send
    {
        std::future::ready(Err(crate::github::GithubError::Transport {
            call: crate::github::GithubCall::UserProfile,
            message: "unused in scripted github".to_owned(),
        }))
    }

    fn list_repos(
        &self,
        _token: &crate::github::GithubToken,
    ) -> impl Future<Output = Result<Vec<flyco_core::RepoSummary>, crate::github::GithubError>> + Send
    {
        std::future::ready(Ok(Vec::new()))
    }

    fn get_repo(
        &self,
        _token: &crate::github::GithubToken,
        slug: &flyco_core::RepoSlug,
    ) -> impl Future<Output = Result<flyco_core::RepoSummary, crate::github::GithubError>> + Send
    {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_auth {
            return std::future::ready(Err(crate::github::GithubError::Status {
                call: crate::github::GithubCall::Repository,
                status: 401,
                reason: "Bad credentials".to_owned(),
            }));
        }
        std::future::ready(Ok(flyco_core::RepoSummary {
            slug: slug.clone(),
            private: false,
            default_branch: "main".parse().expect("branch"),
            description: None,
            pushed_at_unix: None,
        }))
    }

    fn list_branches(
        &self,
        _token: &crate::github::GithubToken,
        _slug: &flyco_core::RepoSlug,
        _page: u32,
    ) -> impl Future<Output = Result<crate::github::BranchListing, crate::github::GithubError>> + Send
    {
        std::future::ready(Ok(crate::github::BranchListing {
            names: Vec::new(),
            has_more: false,
        }))
    }

    fn read_tree(
        &self,
        _token: &crate::github::GithubToken,
        _slug: &flyco_core::RepoSlug,
        _git_ref: &str,
    ) -> impl Future<Output = Result<crate::github::TreeListing, crate::github::GithubError>> + Send
    {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut entries = vec![crate::github::TreeEntry {
            path: ".claude-plugin/marketplace.json".to_owned(),
            is_file: true,
            size: 128,
        }];
        for idx in 0..self.skill_count {
            entries.push(crate::github::TreeEntry {
                path: format!("plugins/bulk/skills/skill-{idx:03}/SKILL.md"),
                is_file: true,
                size: 40,
            });
        }
        for idx in 0..self.bundle_file_count {
            entries.push(crate::github::TreeEntry {
                path: format!("skills/oversized/file-{idx:03}.txt"),
                is_file: true,
                size: 10,
            });
        }
        std::future::ready(Ok(crate::github::TreeListing {
            entries,
            truncated: false,
        }))
    }

    fn read_file(
        &self,
        _token: &crate::github::GithubToken,
        _slug: &flyco_core::RepoSlug,
        _git_ref: &str,
        path: &str,
    ) -> impl Future<Output = Result<Vec<u8>, crate::github::GithubError>> + Send {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.file_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if path == ".claude-plugin/marketplace.json" {
            return std::future::ready(Ok(
                br#"{"name":"bulk","plugins":[{"name":"bulk","source":"./plugins/bulk"}]}"#
                    .to_vec(),
            ));
        }
        std::future::ready(Ok(b"---\ndescription: Bulk skill\n---\n".to_vec()))
    }
}

#[skyzen::test]
async fn per_user_auth_failure_never_poisons_the_shared_built_in_marketplace_cache(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let revoked_user = seed_user(&db).await;
    let healthy_user = seed_other_user(&db).await;
    let revoked_token = sign_in(&kv, revoked_user.clone()).await;
    let healthy_token = sign_in(&kv, healthy_user.clone()).await;
    let client = ctx.client(router);

    let built_in = crate::marketplaces::Marketplace::built_in();
    let failing_gh = ScriptedGithub {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        file_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fail_auth: true,
        skill_count: 0,
        bundle_file_count: 0,
    };
    crate::skill_catalog::refresh(
        &db,
        &test_config(),
        &kv,
        &failing_gh,
        revoked_user.id,
        &built_in,
    )
    .await
    .expect("refresh records per-user auth failure");

    // The revoked user sees their own failure.
    let revoked_view = client
        .get("/v1/catalog/skills")
        .bearer(&revoked_token)
        .send()
        .await
        .json::<SkillCatalog>();
    assert_eq!(revoked_view.failed.len(), 1);

    // The healthy user is NOT poisoned: built-in is still pending (never failed)
    // and populates the shared cache cleanly when refreshed.
    let healthy_before = client
        .get("/v1/catalog/skills")
        .bearer(&healthy_token)
        .send()
        .await
        .json::<SkillCatalog>();
    assert_eq!(
        healthy_before.failed,
        [] as [flyco_core::MarketplaceProblem; 0]
    );
    assert!(
        healthy_before
            .pending
            .contains(&BUILT_IN_MARKETPLACE.to_owned())
    );

    let healthy_gh = ScriptedGithub {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        file_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fail_auth: false,
        skill_count: 2,
        bundle_file_count: 0,
    };
    crate::skill_catalog::refresh(
        &db,
        &test_config(),
        &kv,
        &healthy_gh,
        healthy_user.id,
        &built_in,
    )
    .await
    .expect("healthy refresh succeeds");

    let healthy_after = client
        .get("/v1/catalog/skills")
        .bearer(&healthy_token)
        .send()
        .await
        .json::<SkillCatalog>();
    assert_eq!(
        healthy_after.failed,
        [] as [flyco_core::MarketplaceProblem; 0]
    );
    assert_eq!(healthy_after.skills.len(), 2);
}

#[skyzen::test]
async fn marketplace_refresh_and_skill_bundle_stay_under_workers_subrequest_ceiling(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.clone()).await;
    let client = ctx.client(router);

    // 60 skills in one marketplace: refresh must strictly cap GitHub subrequests
    // at `MAX_MARKETPLACE_SUBREQUESTS` (45 < 50) while still listing every skill.
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let bulk_gh = ScriptedGithub {
        calls: calls.clone(),
        file_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fail_auth: false,
        skill_count: 60,
        bundle_file_count: 0,
    };
    let built_in = crate::marketplaces::Marketplace::built_in();
    crate::skill_catalog::refresh(&db, &test_config(), &kv, &bulk_gh, user.id, &built_in)
        .await
        .expect("bulk refresh succeeds within budget");
    let total_calls = calls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        total_calls <= crate::skill_catalog::MAX_MARKETPLACE_SUBREQUESTS,
        "refresh issued {total_calls} GitHub calls, exceeding {}",
        crate::skill_catalog::MAX_MARKETPLACE_SUBREQUESTS
    );

    let catalog = client
        .get("/v1/catalog/skills")
        .bearer(&token)
        .send()
        .await
        .json::<SkillCatalog>();
    assert_eq!(catalog.skills.len(), 60);

    // A skill with MAX_SKILL_FILES + 1 files is refused before fetching any file.
    let bundle_file_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let oversized_gh = ScriptedGithub {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        file_calls: bundle_file_calls.clone(),
        fail_auth: false,
        skill_count: 0,
        bundle_file_count: crate::skill_catalog::MAX_SKILL_FILES + 1,
    };
    let slug: flyco_core::RepoSlug = "example/oversized".parse().expect("slug");
    let refusal = crate::skill_catalog::bundle_for_test(
        &oversized_gh,
        &crate::github::GithubToken {
            access_token: "t".to_owned(),
        },
        &slug,
        "main",
        "skills/oversized",
    )
    .await
    .expect_err("oversized skill must be rejected");
    assert_eq!(refusal.slug(), "invalid-skill");
    assert_eq!(
        bundle_file_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "rejected oversized skill must not issue per-file subrequests"
    );
}
