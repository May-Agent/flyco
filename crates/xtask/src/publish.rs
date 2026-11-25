//! `cargo xtask publish-flycod`: the one path that puts `flycod` in R2.
//!
//! Cross-compiles the daemon for both Linux architectures against a pinned
//! glibc, writes the checksum files `crates/api/src/releases.rs` serves,
//! stages the installer and both systemd units out of `crates/xtask/install/`,
//! uploads every object to the channel's prefix, and reads back what the
//! control plane actually serves. It refuses to publish a daemon speaking a
//! wire protocol the deployed Worker does not, because that combination
//! refuses every machine provisioned from it at attach.
//!
//! In CI, `.github/workflows/publish-flycod.yml` splits the procedure across
//! native per-architecture runners (`--arch x86_64` on `ubuntu-latest` and
//! `--arch aarch64` on `ubuntu-24.04-arm`, zero QEMU) and a `--finalize` job
//! that assembles the multi-architecture manifest list and uploads to R2.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail, ensure};
use askama::Template as _;
use cargo_metadata::MetadataCommand;
use clap::Parser;
use flyco_core::release::{ASSETS, BINARY_COUNT, INSTALLER};
use serde::Deserialize;
use tokio::fs;
use tracing::info;
use zenwave::{ResponseExt as _, header::CONTENT_TYPE};

use crate::release::{
    ARCHITECTURES, Architecture, ArchitectureArtifacts, CONTAINERFILE, Channel, ENTRYPOINT,
    IMAGE_DIR, Invocation, Release, ReleaseObject, WireCheck, asset_source,
};

/// An external program the publish runs, and how to install it.
#[derive(Debug, Clone, Copy)]
struct RequiredTool {
    /// Executable looked up on `PATH`.
    program: &'static str,
    /// The command that installs it, named in the failure.
    install: &'static str,
}

/// What cross-compiling needs. `cargo-zigbuild` drives the build and `zig`
/// is the C toolchain it links with; either one missing is a hard stop
/// rather than a fallback to the host toolchain, which cannot produce a
/// glibc-pinned Linux binary at all.
const BUILD_TOOLS: [RequiredTool; 2] = [
    RequiredTool {
        program: "cargo-zigbuild",
        install: "cargo install cargo-zigbuild",
    },
    RequiredTool {
        program: "zig",
        install: "brew install zig (or https://ziglang.org/download/)",
    },
];

/// What building the session image needs: a Docker CLI with buildx, which
/// every current Docker Desktop, `OrbStack` and the Ubuntu runner carry.
const IMAGE_TOOLS: [RequiredTool; 1] = [RequiredTool {
    program: "docker",
    install: "https://docs.docker.com/get-docker/ (OrbStack on a Mac)",
}];

/// What uploading needs.
const UPLOAD_TOOLS: [RequiredTool; 1] = [RequiredTool {
    program: "wrangler",
    install: "npm install --global wrangler",
}];

/// How the registry is logged into when a push is refused: any GitHub account
/// with package write access to the `May-Agent` organization, with the one
/// scope a package push needs.
const REGISTRY_LOGIN: &str = "gh auth refresh --hostname github.com --scopes write:packages && gh auth token | docker login ghcr.io --username \"$(gh api user --jq .login)\" --password-stdin";

/// Builds and publishes the `flycod` release artifacts.
#[derive(Debug, Parser)]
pub struct PublishFlycod {
    /// Release channel to publish to: the `releases/<channel>/` prefix in the
    /// bucket, and the deployment whose wire protocol version must match.
    #[arg(long, default_value = "dev")]
    channel: Channel,
    /// Build, stage, and push-by-digest a single Linux architecture (`x86_64`
    /// or `aarch64`) for a split native CI workflow.
    #[arg(long, conflicts_with = "finalize")]
    arch: Option<Architecture>,
    /// Finalize a split CI publish from previously staged per-architecture
    /// binaries and image digests: assemble the multi-architecture manifest
    /// list, upload all objects to R2, and verify what the control plane serves.
    #[arg(long, conflicts_with = "arch")]
    finalize: bool,
    /// Build and checksum the artifacts without uploading anything.
    #[arg(long)]
    dry_run: bool,
    /// Publish even though the channel's control plane speaks a different
    /// wire protocol version.
    #[arg(long)]
    allow_wire_mismatch: bool,
}

/// The health document `/v1/healthz` answers with.
#[derive(Debug, Deserialize)]
struct Health {
    /// Wire protocol version that deployment speaks to daemons.
    wire_protocol_version: u32,
}

/// The JSON document `docker buildx build --metadata-file` writes.
#[derive(Debug, Deserialize)]
struct ImageBuildMetadata {
    /// Digest of the pushed single-architecture image (`sha256:...`).
    #[serde(rename = "containerimage.digest")]
    digest: String,
}

/// The workspace paths the publish works in.
#[derive(Debug, Clone)]
struct Workspace {
    /// Directory the build is run from, and the one the installer sources are
    /// read out of.
    root: PathBuf,
    /// Directory cargo writes build output to.
    target: PathBuf,
}

impl PublishFlycod {
    /// Runs the publish.
    ///
    /// # Errors
    ///
    /// Returns an error when a required tool is missing, the wire protocol
    /// versions disagree, the cross-compile fails, an installer source is
    /// missing, an upload fails, or what the control plane serves afterwards
    /// is not what was staged.
    pub async fn run(self) -> Result<()> {
        if let Some(arch) = self.arch {
            return self.run_arch(arch).await;
        }
        if self.finalize {
            return self.run_finalize().await;
        }
        self.run_all().await
    }

    /// Checks that the channel's deployed control plane agrees on the wire
    /// protocol version unless `--allow-wire-mismatch` was passed, returning
    /// `(publishing, wire_mismatch)`.
    async fn check_wire(&self) -> Result<(u32, bool)> {
        let publishing = flyco_core::WIRE_PROTOCOL_VERSION;
        let deployed = deployed_wire_version(self.channel).await?;
        info!(
            origin = self.channel.origin,
            wire_protocol_version = deployed,
            "control plane"
        );
        WireCheck {
            channel: self.channel,
            publishing,
            deployed,
            allowed_mismatch: self.allow_wire_mismatch,
        }
        .approve()?;
        Ok((publishing, publishing != deployed))
    }

    /// Single-architecture native build step of the split CI workflow.
    async fn run_arch(self, architecture: Architecture) -> Result<()> {
        let publishing = flyco_core::WIRE_PROTOCOL_VERSION;
        info!(
            channel = %self.channel,
            arch = %architecture,
            wire_protocol_version = publishing,
            dry_run = self.dry_run,
            "building flycod architecture"
        );

        require(&BUILD_TOOLS)?;
        require(&IMAGE_TOOLS)?;
        let (publishing, wire_mismatch) = self.check_wire().await?;

        let workspace = workspace().await?;
        run_to_completion(&Invocation::zigbuild_for(&[architecture]), &workspace.root).await?;

        let artifact = stage_arch(self.channel, &workspace, architecture).await?;
        info!(
            object = artifact.binary.name(),
            digest = artifact.line.digest,
            file = %artifact.binary.file.display(),
            "built"
        );

        let stage_dir = self.channel.stage_dir(&workspace.target);
        let tags = self.channel.image_tags(publishing, wire_mismatch);
        let dry_run_tag = self.dry_run.then_some(tags[0].as_str());
        let build = Invocation::image_build_arch(architecture, &stage_dir, dry_run_tag);
        run_to_completion(&build, &workspace.root)
            .await
            .with_context(|| {
                if self.dry_run {
                    format!("building the {architecture} session image")
                } else {
                    format!(
                        "building and pushing the {architecture} session image; if the registry refused the push, log in with: {REGISTRY_LOGIN}"
                    )
                }
            })?;

        if self.dry_run {
            run_to_completion(&Invocation::image_smoke(&tags[0]), &workspace.root).await?;
            info!(
                arch = %architecture,
                image = %tags[0],
                directory = %stage_dir.display(),
                "dry run: staged and built native image, nothing uploaded"
            );
            return Ok(());
        }

        let metadata_path = architecture.image_metadata_file(&stage_dir);
        let metadata_bytes = fs::read(&metadata_path)
            .await
            .with_context(|| format!("reading {}", metadata_path.display()))?;
        let metadata: ImageBuildMetadata = serde_json::from_slice(&metadata_bytes)
            .with_context(|| format!("parsing {}", metadata_path.display()))?;
        let digest = metadata.digest.trim();
        ensure!(
            is_sha256_digest(digest),
            "unexpected containerimage.digest in {}: {digest:?}",
            metadata_path.display()
        );
        let digest_path = architecture.image_digest_file(&stage_dir);
        fs::write(&digest_path, format!("{digest}\n"))
            .await
            .with_context(|| format!("writing {}", digest_path.display()))?;
        info!(
            arch = %architecture,
            digest,
            file = %digest_path.display(),
            "pushed single-arch image by digest"
        );
        Ok(())
    }

    /// Finalize step of the split CI workflow: merge per-arch image digests
    /// into the multi-architecture manifest list, upload all release objects
    /// to R2, and verify what the control plane serves.
    async fn run_finalize(self) -> Result<()> {
        let publishing = flyco_core::WIRE_PROTOCOL_VERSION;
        info!(
            channel = %self.channel,
            wire_protocol_version = publishing,
            dry_run = self.dry_run,
            "finalizing flycod publish"
        );

        if !self.dry_run {
            require(&IMAGE_TOOLS)?;
            require(&UPLOAD_TOOLS)?;
        }
        let (publishing, wire_mismatch) = self.check_wire().await?;

        let workspace = workspace().await?;
        let release = stage_finalize(self.channel, &workspace).await?;
        for artifact in &release.artifacts {
            info!(
                object = artifact.binary.name(),
                digest = artifact.line.digest,
                file = %artifact.binary.file.display(),
                "verified staged binary"
            );
        }
        for asset in &release.assets {
            info!(
                object = asset.name(),
                file = %asset.file.display(),
                "copied from the repository"
            );
        }

        let stage_dir = self.channel.stage_dir(&workspace.target);
        if self.dry_run {
            info!(
                objects = release.objects().count(),
                directory = %stage_dir.display(),
                "dry run: verified staged binaries and assets, nothing uploaded"
            );
            return Ok(());
        }

        let mut digests = Vec::with_capacity(BINARY_COUNT);
        for architecture in ARCHITECTURES {
            let digest_path = architecture.image_digest_file(&stage_dir);
            let raw = fs::read_to_string(&digest_path)
                .await
                .with_context(|| format!("reading {}", digest_path.display()))?;
            let digest = raw.trim();
            ensure!(
                is_sha256_digest(digest),
                "invalid image digest in {}: {digest:?}",
                digest_path.display()
            );
            digests.push(digest.to_owned());
        }

        // Assemble and publish the multi-arch manifest list before touching R2
        // so a refused registry push leaves live installer objects untouched.
        let tags = self.channel.image_tags(publishing, wire_mismatch);
        run_to_completion(
            &Invocation::image_merge_manifests(&tags, &digests),
            &workspace.root,
        )
        .await
        .with_context(|| {
            format!("merging session image manifests; if the registry refused the push, log in with: {REGISTRY_LOGIN}")
        })?;
        for tag in &tags {
            run_to_completion(&Invocation::image_inspect(tag), &workspace.root).await?;
            info!(image = %tag, "pushed and read back");
        }

        for (key, upload) in release.upload_invocations() {
            run_to_completion(&upload, &workspace.root).await?;
            info!(key = %key, "uploaded");
        }

        verify(&release, &workspace.root, wire_mismatch).await
    }

    /// Single-machine full publish (developer laptop path).
    async fn run_all(self) -> Result<()> {
        let publishing = flyco_core::WIRE_PROTOCOL_VERSION;
        info!(
            channel = %self.channel,
            wire_protocol_version = publishing,
            dry_run = self.dry_run,
            "publishing flycod"
        );

        require(&BUILD_TOOLS)?;
        require(&IMAGE_TOOLS)?;
        if !self.dry_run {
            require(&UPLOAD_TOOLS)?;
        }

        let (publishing, wire_mismatch) = self.check_wire().await?;

        let workspace = workspace().await?;
        run_to_completion(&Invocation::zigbuild(), &workspace.root).await?;

        let release = stage(self.channel, &workspace).await?;
        for artifact in &release.artifacts {
            info!(
                object = artifact.binary.name(),
                digest = artifact.line.digest,
                file = %artifact.binary.file.display(),
                "built"
            );
        }
        for asset in &release.assets {
            info!(
                object = asset.name(),
                file = %asset.file.display(),
                "copied from the repository"
            );
        }

        // The image before the bucket: a push the registry refuses (no
        // login, no `write:packages`) stops the publish before a single
        // object has changed under the machines already installing from it.
        let stage_dir = self.channel.stage_dir(&workspace.target);
        let tags = self.channel.image_tags(publishing, wire_mismatch);
        let build = Invocation::image_build(
            self.channel,
            &stage_dir,
            publishing,
            wire_mismatch,
            !self.dry_run,
        );
        run_to_completion(&build, &workspace.root)
            .await
            .with_context(|| {
                if self.dry_run {
                    "building the session image".to_owned()
                } else {
                    format!("building and pushing the session image; if the registry refused the push, log in with: {REGISTRY_LOGIN}")
                }
            })?;
        if self.dry_run {
            run_to_completion(&Invocation::image_smoke(&tags[0]), &workspace.root).await?;
            info!(
                objects = release.objects().count(),
                image = %tags[0],
                directory = %stage_dir.display(),
                "dry run: staged and the image built, nothing uploaded"
            );
            return Ok(());
        }
        for tag in &tags {
            run_to_completion(&Invocation::image_inspect(tag), &workspace.root).await?;
            info!(image = %tag, "pushed and read back");
        }

        for (key, upload) in release.upload_invocations() {
            run_to_completion(&upload, &workspace.root).await?;
            info!(key = %key, "uploaded");
        }

        verify(&release, &workspace.root, wire_mismatch).await
    }
}

/// Checks that `value` is a canonical lowercase `sha256:<64 hex chars>` digest.
fn is_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// Fails unless every tool is on `PATH`, naming what installs the missing one.
fn require(tools: &[RequiredTool]) -> Result<()> {
    for tool in tools {
        if which::which(tool.program).is_err() {
            bail!(
                "{} is not on PATH; install it with: {}",
                tool.program,
                tool.install
            );
        }
    }
    Ok(())
}

/// Reads the wire protocol version the channel's control plane speaks.
async fn deployed_wire_version(channel: Channel) -> Result<u32> {
    let url = channel.health_url();
    let response = zenwave::get(url.as_str())
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .await
        .with_context(|| format!("GET {url}"))?;
    let health: Health = response
        .into_json()
        .await
        .with_context(|| format!("{url} did not answer a health document"))?;
    Ok(health.wire_protocol_version)
}

/// Locates the workspace, as cargo itself reports it.
async fn workspace() -> Result<Workspace> {
    let metadata = tokio::task::spawn_blocking(|| MetadataCommand::new().no_deps().exec())
        .await
        .context("reading cargo metadata")?
        .context("reading cargo metadata")?;
    Ok(Workspace {
        root: metadata.workspace_root.into_std_path_buf(),
        target: metadata.target_directory.into_std_path_buf(),
    })
}

/// Runs one invocation in `directory`, inheriting its output.
async fn run_to_completion(invocation: &Invocation, directory: &Path) -> Result<()> {
    info!(command = %invocation, "running");
    let mut command = invocation.command();
    command.current_dir(directory);
    let status = tokio::process::Command::from(command)
        .status()
        .await
        .with_context(|| format!("spawning {}", invocation.program))?;
    ensure!(status.success(), "`{invocation}` failed: {status}");
    Ok(())
}

/// Writes `bytes` to `architecture.image_bin_path(stage_dir)` so the
/// Containerfile's `COPY image-bin/${TARGETARCH}/flycod` reads it cleanly.
async fn stage_image_bin(architecture: Architecture, stage_dir: &Path, bytes: &[u8]) -> Result<()> {
    let image_bin = architecture.image_bin_path(stage_dir);
    if let Some(parent) = image_bin.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&image_bin, bytes)
        .await
        .with_context(|| format!("writing {}", image_bin.display()))?;
    Ok(())
}

/// Copies `Containerfile` and `flyco-session` into `stage_dir`.
async fn stage_image_sources(workspace_root: &Path, stage_dir: &Path) -> Result<()> {
    let image_dir = IMAGE_DIR
        .iter()
        .fold(workspace_root.to_path_buf(), |path, segment| {
            path.join(segment)
        });
    for source in [CONTAINERFILE, ENTRYPOINT] {
        let from = image_dir.join(source);
        let bytes = fs::read(&from)
            .await
            .with_context(|| format!("reading {}", from.display()))?;
        fs::write(stage_dir.join(source), &bytes)
            .await
            .with_context(|| format!("writing {}", stage_dir.join(source).display()))?;
    }
    Ok(())
}

/// Copies all repository installer assets (`ASSETS`) into `stage_dir`.
async fn stage_assets(
    workspace_root: &Path,
    stage_dir: &Path,
) -> Result<[ReleaseObject; flyco_core::release::ASSET_COUNT]> {
    let mut assets = Vec::with_capacity(ASSETS.len());
    for asset in ASSETS {
        let staged = ReleaseObject::staged(asset, stage_dir);
        let source = asset_source(workspace_root, asset);
        let bytes = fs::read(&source)
            .await
            .with_context(|| format!("reading {}", source.display()))?;
        fs::write(&staged.file, &bytes)
            .await
            .with_context(|| format!("writing {}", staged.file.display()))?;
        assets.push(staged);
    }
    Ok(assets
        .try_into()
        .expect("one staged object per installer asset"))
}

/// Stages one architecture's binary, checksum, image binary path, `flycod.sh`,
/// and Containerfile sources for a per-architecture native build.
async fn stage_arch(
    channel: Channel,
    workspace: &Workspace,
    architecture: Architecture,
) -> Result<ArchitectureArtifacts> {
    let stage_dir = channel.stage_dir(&workspace.target);
    fs::create_dir_all(&stage_dir)
        .await
        .with_context(|| format!("creating {}", stage_dir.display()))?;

    let built = architecture.built_binary(&workspace.target);
    let bytes = fs::read(&built)
        .await
        .with_context(|| format!("reading {}", built.display()))?;

    let artifact = ArchitectureArtifacts::stage(architecture, &stage_dir, &bytes);
    fs::write(&artifact.binary.file, &bytes)
        .await
        .with_context(|| format!("writing {}", artifact.binary.file.display()))?;
    fs::write(&artifact.checksum.file, artifact.line.render()?)
        .await
        .with_context(|| format!("writing {}", artifact.checksum.file.display()))?;
    stage_image_bin(architecture, &stage_dir, &bytes).await?;

    // The Containerfile bind-mounts `flycod.sh` out of the staging directory.
    let installer_staged = ReleaseObject::staged(INSTALLER, &stage_dir);
    let installer_source = asset_source(&workspace.root, INSTALLER);
    let installer_bytes = fs::read(&installer_source)
        .await
        .with_context(|| format!("reading {}", installer_source.display()))?;
    fs::write(&installer_staged.file, &installer_bytes)
        .await
        .with_context(|| format!("writing {}", installer_staged.file.display()))?;

    stage_image_sources(&workspace.root, &stage_dir).await?;
    Ok(artifact)
}

/// Validates previously staged per-architecture binaries and checksums in
/// `stage_dir`, then copies the repository installer assets alongside them so
/// `finalize` can upload and attest the full release set.
async fn stage_finalize(channel: Channel, workspace: &Workspace) -> Result<Release> {
    let stage_dir = channel.stage_dir(&workspace.target);
    let mut staged_artifacts = Vec::with_capacity(BINARY_COUNT);

    for architecture in ARCHITECTURES {
        let binary_path = stage_dir.join(architecture.published.binary.name);
        let bytes = fs::read(&binary_path)
            .await
            .with_context(|| format!("reading {}", binary_path.display()))?;
        let artifact = ArchitectureArtifacts::stage(architecture, &stage_dir, &bytes);
        let expected_checksum = artifact.line.render()?;
        let actual_checksum = fs::read_to_string(&artifact.checksum.file)
            .await
            .with_context(|| format!("reading {}", artifact.checksum.file.display()))?;
        ensure!(
            actual_checksum == expected_checksum,
            "{} does not match {}: expected {expected_checksum:?}, found {actual_checksum:?}",
            artifact.checksum.file.display(),
            binary_path.display(),
        );
        staged_artifacts.push(artifact);
    }

    let assets = stage_assets(&workspace.root, &stage_dir).await?;
    Ok(Release {
        channel,
        artifacts: staged_artifacts
            .try_into()
            .expect("one staged artifact per published architecture"),
        assets,
    })
}

/// Puts every object in the staging directory under its published name: both
/// freshly built binaries, the checksum file attesting each one, and the
/// installer and both units copied verbatim out of the repository.
///
/// Everything is uploaded from here, so a `--dry-run` leaves behind exactly
/// the bytes a real publish would have sent.
async fn stage(channel: Channel, workspace: &Workspace) -> Result<Release> {
    let stage_dir = channel.stage_dir(&workspace.target);
    fs::create_dir_all(&stage_dir)
        .await
        .with_context(|| format!("creating {}", stage_dir.display()))?;

    let mut sources = Vec::with_capacity(BINARY_COUNT);
    for architecture in ARCHITECTURES {
        let built = architecture.built_binary(&workspace.target);
        sources.push(
            fs::read(&built)
                .await
                .with_context(|| format!("reading {}", built.display()))?,
        );
    }

    let mut sources = sources.into_iter();
    let staged = ARCHITECTURES.map(|architecture| {
        let bytes = sources
            .next()
            .expect("one built binary per published architecture");
        (
            ArchitectureArtifacts::stage(architecture, &stage_dir, &bytes),
            bytes,
        )
    });

    for (artifact, bytes) in &staged {
        fs::write(&artifact.binary.file, bytes)
            .await
            .with_context(|| format!("writing {}", artifact.binary.file.display()))?;
        fs::write(&artifact.checksum.file, artifact.line.render()?)
            .await
            .with_context(|| format!("writing {}", artifact.checksum.file.display()))?;
        stage_image_bin(artifact.architecture, &stage_dir, bytes).await?;
    }

    let assets = stage_assets(&workspace.root, &stage_dir).await?;
    stage_image_sources(&workspace.root, &stage_dir).await?;

    Ok(Release {
        channel,
        artifacts: staged.map(|(artifact, _)| artifact),
        assets,
    })
}

/// Reads back what the control plane (or R2 directly under `--allow-wire-mismatch`)
/// serves and fails on any difference.
///
/// This is the only check that covers the whole path — build, checksum,
/// upload, bucket, Worker allowlist, media type — so a publish is not
/// finished until it passes. When `--allow-wire-mismatch` publishes ahead of
/// a Worker deploy, the old Worker still serves the old wire protocol, so
/// objects are read back directly from R2 instead.
async fn verify(release: &Release, directory: &Path, wire_mismatch: bool) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("reading the clock")?
        .as_nanos();

    for object in release.verified_objects() {
        let expected = fs::read_to_string(&object.file)
            .await
            .with_context(|| format!("reading {}", object.file.display()))?;
        if wire_mismatch {
            let get = Invocation::r2_get(release.channel, object);
            let output = tokio::process::Command::from({
                let mut command = get.command();
                command.current_dir(directory);
                command
            })
            .output()
            .await
            .with_context(|| format!("spawning `{get}`"))?;
            ensure!(output.status.success(), "`{get}` failed: {}", output.status);
            let served = String::from_utf8(output.stdout)
                .with_context(|| format!("`{get}` returned non-UTF-8 text"))?;
            ensure!(
                served == expected,
                "R2 {} holds {served:?}, this publish staged {expected:?}",
                release.channel.bucket_key(&object.published)
            );
            info!(
                object = object.name(),
                "verified directly in R2 (wire mismatch)"
            );
            continue;
        }
        let url = release.channel.verification_url(object.name(), nonce);
        let response = zenwave::get(url.as_str())
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .await
            .with_context(|| format!("GET {url}"))?;
        let served_type = response
            .headers()
            .get(CONTENT_TYPE)
            .with_context(|| format!("{url} answered without a content type"))?
            .to_str()
            .with_context(|| format!("{url} answered a content type that is not text"))?
            .to_owned();
        ensure!(
            served_type == object.published.content_type,
            "{url} is served as {served_type:?}, this publish uploaded {:?}",
            object.published.content_type
        );
        let served = response
            .into_string()
            .await
            .with_context(|| format!("{url} did not answer text"))?;
        ensure!(
            served.as_str() == expected,
            "{url} serves {served:?}, this publish staged {expected:?}"
        );
        info!(object = object.name(), "verified as served");
    }
    Ok(())
}
