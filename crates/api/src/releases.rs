//! Public execution-plane release artifacts.
//!
//! Large binaries cannot ride inside the Worker: the free Worker bundle limit
//! is smaller than one `flycod` build. They live in the deployment's R2 bucket
//! so every channel serves the exact artifacts its wire protocol expects, and
//! this module exposes only the objects [`flyco_core::release`] names — the
//! same table `cargo xtask publish-flycod` writes, so the allowlist and the
//! publisher cannot drift apart.

use flyco_core::release;
use skyzen_services::{Storage, StorageError};

/// Bucket prefix this channel's objects live under.
pub(crate) const ROOT: &str = "releases/dev";

/// One allowlisted release artifact read from object storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseArtifact {
    /// Bytes returned to the machine installer.
    pub body: Vec<u8>,
    /// Fixed media type for this artifact kind.
    pub content_type: &'static str,
}

/// Reads an allowlisted development-channel artifact.
///
/// Unknown names never reach object storage. A known name whose object has
/// not been published is also absent, which makes a half-published release
/// fail as a 404 rather than serving unrelated bucket content.
///
/// The daemon objects — the binaries and the checksums attesting them — are
/// read from the prefix of the wire protocol version this control plane
/// speaks ([`PublishedObject::storage_key`](release::PublishedObject::storage_key)),
/// so a machine is handed exactly the daemon this control plane accepts at
/// attach: the install path cannot pull a daemon an older publish left
/// behind, and a protocol nobody ever published for answers 404 rather than
/// a daemon that would be refused at attach (issue #382).
///
/// # Errors
///
/// Returns [`StorageError`] if R2 cannot be read.
pub async fn get(storage: &Storage, name: &str) -> Result<Option<ReleaseArtifact>, StorageError> {
    get_with_digest(storage, name, None).await
}

/// Reads an allowlisted development-channel artifact.
///
/// When `sha256` is supplied for a versioned binary, looks up the
/// content-addressed key (`wire-<N>/<sha256>/<name>`) so a re-publish never
/// races the edge cache between a binary and its `.sha256`. Costs one R2 `get`
/// when the name (and optional digest) are valid.
///
/// # Errors
///
/// Returns [`StorageError`] if R2 cannot be read.
pub async fn get_with_digest(
    storage: &Storage,
    name: &str,
    sha256: Option<&str>,
) -> Result<Option<ReleaseArtifact>, StorageError> {
    let Some(object) = release::object(name) else {
        return Ok(None);
    };
    let key = match sha256 {
        Some(digest) => {
            if !object.versioned || !is_valid_sha256(digest) {
                return Ok(None);
            }
            format!("{ROOT}/{}", object.versioned_digest_key(digest))
        }
        None => format!("{ROOT}/{}", object.storage_key()),
    };
    Ok(storage.get(&key).await?.map(|stored| ReleaseArtifact {
        body: stored.body,
        content_type: object.content_type,
    }))
}

fn is_valid_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether the bucket holds a daemon speaking this control plane's wire
/// protocol: one a machine's installer can actually fetch.
///
/// The probe is the checksum objects rather than the binaries themselves:
/// each pair is published binary-then-checksum, so a checksum's presence
/// means its architecture's pair is complete, and `head` answers it from
/// metadata alone — no daemon is downloaded to ask the question. Both
/// architectures are required, the same completeness a release itself
/// promises.
///
/// # Errors
///
/// Returns [`StorageError`] if R2 cannot be read.
pub async fn daemon_is_published(storage: &Storage) -> Result<bool, StorageError> {
    for architecture in release::BINARIES {
        let key = format!("{ROOT}/{}", architecture.checksum.storage_key());
        if storage.head(&key).await?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use flyco_core::release::{self, OBJECT_COUNT};
    use skyzen_services::Storage;

    use super::{ROOT, daemon_is_published, get};

    fn store() -> Storage {
        Storage::new(skyzen_test::mock::InMemoryStorage::new())
    }

    /// The bucket key a publish writes `object` under.
    fn key(object: release::PublishedObject) -> String {
        format!("{ROOT}/{}", object.storage_key())
    }

    #[skyzen::test]
    async fn an_unknown_name_cannot_read_an_arbitrary_bucket_object() {
        let storage = store();
        storage
            .put("transcripts/private", b"secret".to_vec())
            .await
            .expect("seed");

        assert_eq!(
            get(&storage, "../transcripts/private").await.expect("read"),
            None
        );
    }

    #[skyzen::test]
    async fn a_published_checksum_is_served_with_its_fixed_media_type() {
        let storage = store();
        storage
            .put(
                &key(release::X86_64.checksum),
                b"digest  flycod-linux-x86_64\n".to_vec(),
            )
            .await
            .expect("seed");

        let artifact = get(&storage, "flycod-linux-x86_64.sha256")
            .await
            .expect("read")
            .expect("published");
        assert_eq!(artifact.content_type, "text/plain; charset=utf-8");
        assert_eq!(artifact.body, b"digest  flycod-linux-x86_64\n");
    }

    /// The checksum lives under the wire protocol version its daemon speaks:
    /// the same name on the old unversioned layout is not an answer for this
    /// control plane, because serving it would hand a machine a daemon every
    /// attach refuses (issue #382).
    #[skyzen::test]
    async fn a_daemon_left_under_the_unversioned_layout_is_not_served() {
        let storage = store();
        storage
            .put(
                &format!("{ROOT}/flycod-linux-x86_64.sha256"),
                b"digest  flycod-linux-x86_64\n".to_vec(),
            )
            .await
            .expect("seed");

        assert_eq!(
            get(&storage, "flycod-linux-x86_64.sha256")
                .await
                .expect("read"),
            None
        );
    }

    #[skyzen::test]
    async fn the_probe_is_true_only_when_every_architecture_is_there() {
        let storage = store();
        assert!(!daemon_is_published(&storage).await.expect("read"));

        storage
            .put(
                &key(release::X86_64.checksum),
                b"digest  flycod-linux-x86_64\n".to_vec(),
            )
            .await
            .expect("seed");
        // One architecture is a half-published release: a machine on the
        // other would still boot into an install that cannot finish.
        assert!(!daemon_is_published(&storage).await.expect("read"));

        storage
            .put(
                &key(release::AARCH64.checksum),
                b"digest  flycod-linux-aarch64\n".to_vec(),
            )
            .await
            .expect("seed");
        assert!(daemon_is_published(&storage).await.expect("read"));
    }

    #[skyzen::test]
    async fn the_installer_is_served_as_a_shell_script_and_its_unit_as_text() {
        let storage = store();
        storage
            .put(&format!("{ROOT}/flycod.sh"), b"#!/bin/sh\n".to_vec())
            .await
            .expect("seed");
        storage
            .put(&format!("{ROOT}/flycod.service"), b"[Unit]\n".to_vec())
            .await
            .expect("seed");

        let installer = get(&storage, "flycod.sh")
            .await
            .expect("read")
            .expect("published");
        let unit = get(&storage, "flycod.service")
            .await
            .expect("read")
            .expect("published");

        assert_eq!(installer.content_type, "text/x-shellscript; charset=utf-8");
        assert_eq!(installer.body, b"#!/bin/sh\n");
        assert_eq!(unit.content_type, "text/plain; charset=utf-8");
        assert_eq!(unit.body, b"[Unit]\n");
    }

    /// Every name the publisher writes is a name this module serves, with the
    /// media type the shared table declares.
    #[skyzen::test]
    async fn the_allowlist_is_exactly_the_published_object_table() {
        let storage = store();
        for object in release::OBJECTS {
            storage
                .put(&key(object), object.name.into())
                .await
                .expect("seed");
        }

        for object in release::OBJECTS {
            let artifact = get(&storage, object.name)
                .await
                .expect("read")
                .expect("published");
            assert_eq!(artifact.content_type, object.content_type);
            assert_eq!(artifact.body, object.name.as_bytes());
        }
        assert_eq!(release::OBJECTS.len(), OBJECT_COUNT);
    }

    #[skyzen::test]
    async fn an_allowlisted_object_nobody_published_is_absent() {
        assert_eq!(get(&store(), "flycod.sh").await.expect("read"), None);
    }

    #[skyzen::test]
    async fn digest_lookup_reads_content_addressed_binary_and_rejects_invalid_digests() {
        let storage = store();
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        storage
            .put(
                &format!(
                    "{ROOT}/{}",
                    release::X86_64.binary.versioned_digest_key(digest)
                ),
                b"pinned-bytes".to_vec(),
            )
            .await
            .expect("seed");

        let hit = super::get_with_digest(&storage, "flycod-linux-x86_64", Some(digest))
            .await
            .expect("read")
            .expect("present");
        assert_eq!(hit.body, b"pinned-bytes");
        assert_eq!(hit.content_type, "application/octet-stream");

        assert_eq!(
            super::get_with_digest(&storage, "flycod-linux-x86_64", Some("../bad"))
                .await
                .expect("read"),
            None
        );
        assert_eq!(
            super::get_with_digest(&storage, "flycod.sh", Some(digest))
                .await
                .expect("read"),
            None
        );
    }
}
