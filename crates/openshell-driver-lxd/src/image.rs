// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OCI-to-LXD image conversion pipeline.
//!
//! Phase 2, Step 1 of `docs/04-implementation-plan.md` — see
//! that document's "LXD system-container constraints" subsection for the
//! four requirements this module exists to satisfy (init/PID1
//! compatibility is the *caller's* responsibility via
//! [`crate::instance::build_entrypoint_script`], not this module's; the
//! other three — signal contract documentation, OCI config translation,
//! and digest-level caching — are addressed here).
//!
//! LXD has no native OCI image support (unlike Docker/Podman/Kubernetes,
//! which each understand OCI images natively) — every `OpenShell` sandbox
//! image is an OCI image, so this pipeline is what makes "any sandbox
//! image the other drivers accept" true for LXD too. It:
//!
//! 1. Pulls the requested image's manifest and layers directly from the
//!    registry using `oci-client` (already a workspace dependency, used
//!    the same way by `openshell-driver-vm`) — no `skopeo`/`umoci`
//!    subprocess dependency, unlike the manual conversion
//!    `hack/run-stage2.sh`'s "oci" mode originally used to prove this was
//!    possible at all.
//! 2. Merges layers into a single rootfs directory, honoring OCI
//!    whiteouts (`.wh.*` per-file deletions, `.wh..wh..opq` opaque
//!    directory resets) — the same algorithm
//!    `openshell-driver-vm/src/driver.rs`'s `merge_layer_directory`
//!    uses, reimplemented here rather than shared as a cross-crate
//!    dependency (the two driver crates are independent by design; see
//!    `docs/03-design-rfc.md`'s "Non-goals").
//! 3. Parses the image's config blob (`Env`/`WorkingDir`/`User`/
//!    `Entrypoint`/`Cmd`) — lost entirely by a raw unpack-and-flatten,
//!    since that metadata lives in the config JSON, not the layers.
//!    Returned to the caller as [`OciImageConfig`] for translation into
//!    LXD instance config; this module only extracts it.
//! 4. Packages the merged rootfs plus a generated `metadata.yaml` into a
//!    single "unified" tarball and uploads it via
//!    [`crate::client::LxdClient::create_image_from_unified_tarball`].
//! 5. Caches the result by image digest: a
//!    `openshell-oci-<digest-prefix>` alias, checked *before* any layer
//!    download. A repeat conversion of the same digest costs one small
//!    manifest+config fetch, not a full re-flatten — but this caches
//!    whole converted images, not individual layers, so a large base
//!    layer shared across different tags of the *same* digest is the
//!    only case that hits the cache; different digests that happen to
//!    share a base layer each still pay the full flatten once.

use crate::client::{LxdApiError, LxdClient};
use oci_client::client::{Client as OciClient, ClientConfig};
use oci_client::manifest::{ImageIndexEntry, OciDescriptor};
use oci_client::secrets::RegistryAuth;
use oci_client::{Reference, RegistryOperation};
use serde::Deserialize;
use sha2::{Digest as Sha2Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::sync::Mutex as AsyncMutex;

/// Errors from the OCI-to-LXD image conversion pipeline.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("invalid OCI image reference '{0}': {1}")]
    InvalidReference(String, String),
    #[error("registry authentication failed for '{0}': {1}")]
    Auth(String, String),
    #[error("failed to pull manifest for '{0}': {1}")]
    Manifest(String, String),
    #[error("failed to pull blob '{0}' for '{1}': {2}")]
    Blob(String, String, String),
    #[error("digest mismatch for {0}: expected {1}, got {2}")]
    DigestMismatch(String, String, String),
    #[error("failed to parse OCI image config: {0}")]
    Config(String),
    #[error("failed to extract layer '{0}': {1}")]
    LayerExtract(String, String),
    #[error("failed to merge layer into rootfs: {0}")]
    LayerMerge(String),
    #[error("failed to package converted image: {0}")]
    Package(String),
    #[error("LXD API error while resolving cache/uploading image: {0}")]
    Lxd(#[from] LxdApiError),
    #[error("I/O error: {0}")]
    Io(String),
}

// ── Registry client setup ────────────────────────────────────────────────

/// Build an OCI registry client that resolves multi-arch image indexes to
/// the host's own Linux architecture. Mirrors
/// `openshell-driver-vm/src/driver.rs`'s `registry_client`/
/// `linux_platform_resolver` exactly — LXD containers run natively on the
/// host architecture (no cross-arch emulation), same constraint the VM
/// driver has.
#[must_use]
pub fn registry_client() -> OciClient {
    OciClient::new(ClientConfig {
        platform_resolver: Some(Box::new(linux_platform_resolver)),
        ..Default::default()
    })
}

fn linux_platform_resolver(manifests: &[ImageIndexEntry]) -> Option<String> {
    let expected_arch = linux_oci_arch();
    manifests
        .iter()
        .find_map(|entry| {
            let platform = entry.platform.as_ref()?;
            (platform.os.to_string() == "linux"
                && platform.architecture.to_string() == expected_arch)
                .then(|| entry.digest.clone())
        })
        .or_else(|| {
            manifests.iter().find_map(|entry| {
                let platform = entry.platform.as_ref()?;
                (platform.os.to_string() == "linux").then(|| entry.digest.clone())
            })
        })
}

/// The host architecture in OCI's naming convention (`amd64`/`arm64`/...),
/// used to pick the right entry out of a multi-arch image index.
fn linux_oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    }
}

/// The host architecture in LXD's own naming convention, for
/// `metadata.yaml`'s mandatory `architecture` field. LXD uses GNU/Linux
/// triplet-style names (`x86_64`, `aarch64`), not OCI's (`amd64`,
/// `arm64`) — these are genuinely different vocabularies for the same
/// concept, not a typo.
fn lxd_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => other,
    }
}

/// Resolve registry credentials the same way
/// `openshell-driver-vm/src/driver.rs`'s `registry_auth` does — shared
/// environment-variable convention (`OPENSHELL_REGISTRY_USERNAME`/
/// `OPENSHELL_REGISTRY_TOKEN`), including the GHCR anonymous-username
/// special case, so an operator configuring registry access for one
/// driver doesn't have to learn a second, LXD-specific convention.
pub fn registry_auth(image_ref: &str) -> Result<RegistryAuth, ImageError> {
    let username = env_non_empty("OPENSHELL_REGISTRY_USERNAME");
    let token = env_non_empty("OPENSHELL_REGISTRY_TOKEN");

    match token {
        Some(token) => {
            let username = match username {
                Some(username) => username,
                None if image_reference_registry_host(image_ref)
                    .eq_ignore_ascii_case("ghcr.io") =>
                {
                    "__token__".to_string()
                }
                None => {
                    return Err(ImageError::Auth(
                        image_ref.to_string(),
                        "OPENSHELL_REGISTRY_USERNAME is required when OPENSHELL_REGISTRY_TOKEN is set for non-GHCR registries".to_string(),
                    ));
                }
            };
            Ok(RegistryAuth::Basic(username, token))
        }
        None => Ok(RegistryAuth::Anonymous),
    }
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn image_reference_registry_host(image_ref: &str) -> &str {
    let mut parts = image_ref.splitn(2, '/');
    let first = parts.next().unwrap_or(image_ref);
    let has_path = parts.next().is_some();
    if has_path
        && (first.contains('.') || first.contains(':') || first.eq_ignore_ascii_case("localhost"))
    {
        first
    } else {
        "docker.io"
    }
}

/// Parse a `--lxd-image`/sandbox-template image string into an OCI
/// [`Reference`].
pub fn parse_registry_reference(image_ref: &str) -> Result<Reference, ImageError> {
    Reference::try_from(image_ref)
        .map_err(|err| ImageError::InvalidReference(image_ref.to_string(), err.to_string()))
}

// ── OCI image config (Env/WorkingDir/User/Entrypoint/Cmd) ───────────────

/// The subset of an OCI image's config JSON this driver translates into
/// LXD instance config. Field names match the OCI Image Spec's `config`
/// object exactly (`Env`, `WorkingDir`, `User`, `Entrypoint`, `Cmd`) —
/// see <https://github.com/opencontainers/image-spec/blob/main/config.md>.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OciImageConfig {
    /// `KEY=value` pairs, in the image's own declared order (later
    /// entries in the image config JSON should win on collision, matching
    /// how Docker/Podman apply `ENV` directives — callers merging this
    /// with driver-controlled environment variables must still apply the
    /// architecture-wide rule that driver-controlled values win, per
    /// `docs/03-design-rfc.md`'s "Credential injection" row).
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OciConfigBlob {
    #[serde(default)]
    config: OciConfigBlobInner,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct OciConfigBlobInner {
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    working_dir: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    entrypoint: Vec<String>,
    #[serde(default)]
    cmd: Vec<String>,
}

/// Parse an OCI image config blob's raw JSON bytes into [`OciImageConfig`].
///
/// Deliberately tolerant: every field is optional in the OCI spec (a
/// `scratch`-based image may have none of them), so a missing or
/// empty-object `config` key parses to
/// [`OciImageConfig::default`], not an error.
pub fn parse_oci_image_config(bytes: &[u8]) -> Result<OciImageConfig, ImageError> {
    let blob: OciConfigBlob =
        serde_json::from_slice(bytes).map_err(|err| ImageError::Config(err.to_string()))?;
    Ok(OciImageConfig {
        env: blob.config.env,
        working_dir: non_empty(blob.config.working_dir),
        user: non_empty(blob.config.user),
        entrypoint: blob.config.entrypoint,
        cmd: blob.config.cmd,
    })
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

// ── Layer extraction and whiteout-aware merge ────────────────────────────

/// A path's `uid`/`gid` as declared by the *original* OCI layer's tar
/// header, tracked independently of the driver's own staging filesystem.
///
/// Extraction runs as whatever host user the driver process itself runs
/// as — commonly non-root (this pipeline has no requirement that it be
/// root). `chown` to a UID other than the caller's own always fails with
/// `EPERM` for a non-root process, as a basic Unix permission rule, not a
/// `tar`-crate limitation to work around: a directory declared
/// `uid=0`/`gid=0` in a layer (e.g. `/run`, root-owned in virtually every
/// base image) silently ends up owned by the *extracting* process's own
/// UID on disk instead — confirmed directly against the real `tar` crate
/// version this crate depends on. That silent substitution was
/// undetectable for a long time because most paths either don't care
/// (world-writable ones like `/tmp`) or aren't exercised by anything this
/// pipeline's own tests touch — until a real sandbox image's supervisor
/// tried `mkdir /run/netns` (root-owned, mode 0755, so ownership *does*
/// matter there) and got `EACCES`, indistinguishable from a genuine
/// missing-capability error at the call site.
///
/// The fix is not to make extraction preserve ownership on disk (that
/// would require running as root, which this pipeline should not
/// require) but to never rely on staging-disk ownership being correct at
/// all: this struct is threaded through extraction and merge so the
/// *final* packaged tarball's headers can be set explicitly from it,
/// independent of whatever the non-root staging process was forced to
/// leave on disk. LXD's own image-unpack step (which does run with real
/// idmap/root privileges) is what actually applies this once and for all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntryOwnership {
    uid: u64,
    gid: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayerCompression {
    None,
    Gzip,
    Zstd,
}

// `media_type` is an OCI media type string (e.g.
// `application/vnd.oci.image.layer.v1.tar+gzip`), not a filesystem path,
// so case-sensitive comparison is correct.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn layer_compression_from_media_type(media_type: &str) -> Result<LayerCompression, ImageError> {
    if media_type.is_empty() {
        return Err(ImageError::LayerExtract(
            String::new(),
            "layer media type is missing".to_string(),
        ));
    }
    if media_type.ends_with("+zstd") {
        return Ok(LayerCompression::Zstd);
    }
    if media_type.ends_with("+gzip") || media_type.ends_with(".gzip") {
        return Ok(LayerCompression::Gzip);
    }
    if media_type.ends_with(".tar")
        || media_type.ends_with("tar")
        || media_type == "application/vnd.oci.image.layer.v1.tar"
        || media_type == "application/vnd.oci.image.layer.nondistributable.v1.tar"
    {
        return Ok(LayerCompression::None);
    }
    Err(ImageError::LayerExtract(
        String::new(),
        format!("unsupported layer media type '{media_type}'"),
    ))
}

/// Extract one downloaded layer blob into its own directory, decompressing
/// according to its OCI media type, and return each extracted path's
/// original declared ownership (see [`EntryOwnership`]'s doc comment for
/// why this can't just be left on disk). Mirrors
/// `openshell-driver-vm/src/driver.rs`'s `extract_layer_blob_to_dir` for
/// the extraction shape; the per-entry iteration (rather than a single
/// `Archive::unpack(dest)` call) is this module's own addition, needed to
/// read each entry's header before/while unpacking it.
fn extract_layer_blob_to_dir(
    blob_path: &Path,
    media_type: &str,
    dest: &Path,
) -> Result<HashMap<PathBuf, EntryOwnership>, ImageError> {
    if dest.exists() {
        fs::remove_dir_all(dest)
            .map_err(|e| ImageError::Io(format!("remove {}: {e}", dest.display())))?;
    }
    fs::create_dir_all(dest)
        .map_err(|e| ImageError::Io(format!("create {}: {e}", dest.display())))?;

    let file = File::open(blob_path)
        .map_err(|e| ImageError::Io(format!("open {}: {e}", blob_path.display())))?;
    let extract = |reader: &mut dyn Read| -> Result<HashMap<PathBuf, EntryOwnership>, ImageError> {
        let mut archive = tar::Archive::new(reader);
        let mut ownership = HashMap::new();
        let entries = archive.entries().map_err(|e| {
            ImageError::LayerExtract(blob_path.display().to_string(), e.to_string())
        })?;
        for entry in entries {
            let mut entry = entry.map_err(|e| {
                ImageError::LayerExtract(blob_path.display().to_string(), e.to_string())
            })?;
            let header = entry.header();
            let uid = header.uid().unwrap_or(0);
            let gid = header.gid().unwrap_or(0);
            let relative_path = header
                .path()
                .map_err(|e| {
                    ImageError::LayerExtract(blob_path.display().to_string(), e.to_string())
                })?
                .into_owned();
            let unpacked = entry.unpack_in(dest).map_err(|e| {
                ImageError::LayerExtract(blob_path.display().to_string(), e.to_string())
            })?;
            // `unpack_in` returns `false` (without erroring) for entries it
            // skips as unsafe (e.g. a path that would escape `dest` via
            // `..`) -- no point tracking ownership for a path that was
            // never actually written.
            if unpacked {
                ownership.insert(dest.join(&relative_path), EntryOwnership { uid, gid });
            }
        }
        Ok(ownership)
    };
    match layer_compression_from_media_type(media_type)? {
        LayerCompression::None => extract(&mut { file }),
        LayerCompression::Gzip => extract(&mut flate2::read::GzDecoder::new(file)),
        LayerCompression::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::new(file).map_err(|e| {
                ImageError::LayerExtract(blob_path.display().to_string(), e.to_string())
            })?;
            extract(&mut decoder)
        }
    }
}

/// Merge one extracted layer directory into the accumulating rootfs,
/// honoring OCI whiteouts. Faithfully reimplements
/// `openshell-driver-vm/src/driver.rs`'s `merge_layer_directory` (same
/// algorithm: an opaque whiteout marker clears the target directory's
/// existing contents first; a per-file whiteout removes the named target
/// path and is not itself copied; everything else overwrites by copy).
///
/// `layer_ownership` is this layer's own extraction-time map (see
/// [`extract_layer_blob_to_dir`]), keyed by absolute paths under
/// `source_dir`. `rootfs_ownership` is the running, whole-rootfs map
/// (keyed by absolute paths under `target_dir`, i.e. the same key space
/// [`append_tree_to_archive`] will look paths up in later) — updated here
/// with the exact same override/removal semantics already applied to
/// file *content* above: a later layer's entry for the same path replaces
/// an earlier layer's, and a whiteout removal also drops any tracked
/// ownership for the path it removes.
fn merge_layer_directory(
    source_dir: &Path,
    target_dir: &Path,
    layer_ownership: &HashMap<PathBuf, EntryOwnership>,
    rootfs_ownership: &mut HashMap<PathBuf, EntryOwnership>,
) -> Result<(), ImageError> {
    fs::create_dir_all(target_dir)
        .map_err(|e| ImageError::LayerMerge(format!("create {}: {e}", target_dir.display())))?;

    let mut entries = fs::read_dir(source_dir)
        .map_err(|e| ImageError::LayerMerge(format!("read {}: {e}", source_dir.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ImageError::LayerMerge(format!("read {}: {e}", source_dir.display())))?;
    entries.sort_by_key(fs::DirEntry::file_name);

    if entries
        .iter()
        .any(|entry| entry.file_name().to_string_lossy() == ".wh..wh..opq")
    {
        clear_directory_contents(target_dir)?;
        rootfs_ownership.retain(|path, _| !path.starts_with(target_dir));
    }

    for entry in entries {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name == ".wh..wh..opq" {
            continue;
        }
        if let Some(hidden_name) = name.strip_prefix(".wh.") {
            let removed_path = target_dir.join(hidden_name);
            remove_path_if_exists(&removed_path)?;
            rootfs_ownership.remove(&removed_path);
            continue;
        }

        let source_path = entry.path();
        let dest_path = target_dir.join(&file_name);
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|e| ImageError::LayerMerge(format!("stat {}: {e}", source_path.display())))?;
        let file_type = metadata.file_type();

        if let Some(owner) = layer_ownership.get(&source_path) {
            rootfs_ownership.insert(dest_path.clone(), *owner);
        }

        if file_type.is_dir() {
            if let Ok(dest_metadata) = fs::symlink_metadata(&dest_path)
                && !dest_metadata.file_type().is_dir()
                && !path_is_dir_or_symlink_to_dir(&dest_path)?
            {
                remove_path_if_exists(&dest_path)?;
            }
            fs::create_dir_all(&dest_path).map_err(|e| {
                ImageError::LayerMerge(format!("create {}: {e}", dest_path.display()))
            })?;
            merge_layer_directory(&source_path, &dest_path, layer_ownership, rootfs_ownership)?;
            if fs::symlink_metadata(&dest_path)
                .map_err(|e| ImageError::LayerMerge(format!("stat {}: {e}", dest_path.display())))?
                .file_type()
                .is_dir()
            {
                fs::set_permissions(&dest_path, metadata.permissions()).map_err(|e| {
                    ImageError::LayerMerge(format!("chmod {}: {e}", dest_path.display()))
                })?;
            }
        } else if file_type.is_file() {
            remove_path_if_exists(&dest_path)?;
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    ImageError::LayerMerge(format!("create {}: {e}", parent.display()))
                })?;
            }
            fs::copy(&source_path, &dest_path).map_err(|e| {
                ImageError::LayerMerge(format!(
                    "copy {} to {}: {e}",
                    source_path.display(),
                    dest_path.display()
                ))
            })?;
            fs::set_permissions(&dest_path, metadata.permissions()).map_err(|e| {
                ImageError::LayerMerge(format!("chmod {}: {e}", dest_path.display()))
            })?;
        } else if file_type.is_symlink() {
            copy_symlink(&source_path, &dest_path)?;
        } else {
            return Err(ImageError::LayerMerge(format!(
                "unsupported layer entry type at {}",
                source_path.display()
            )));
        }
    }

    Ok(())
}

fn path_is_dir_or_symlink_to_dir(path: &Path) -> Result<bool, ImageError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(ImageError::LayerMerge(format!(
            "stat {}: {err}",
            path.display()
        ))),
    }
}

fn clear_directory_contents(dir: &Path) -> Result<(), ImageError> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)
        .map_err(|e| ImageError::LayerMerge(format!("read {}: {e}", dir.display())))?
    {
        let entry =
            entry.map_err(|e| ImageError::LayerMerge(format!("read {}: {e}", dir.display())))?;
        remove_path_if_exists(&entry.path())?;
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<(), ImageError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
            .map_err(|e| ImageError::LayerMerge(format!("remove {}: {e}", path.display())))
    } else {
        fs::remove_file(path)
            .map_err(|e| ImageError::LayerMerge(format!("remove {}: {e}", path.display())))
    }
}

#[cfg(unix)]
fn copy_symlink(source_path: &Path, dest_path: &Path) -> Result<(), ImageError> {
    let target = fs::read_link(source_path)
        .map_err(|e| ImageError::LayerMerge(format!("readlink {}: {e}", source_path.display())))?;
    remove_path_if_exists(dest_path)?;
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ImageError::LayerMerge(format!("create {}: {e}", parent.display())))?;
    }
    std::os::unix::fs::symlink(&target, dest_path).map_err(|e| {
        ImageError::LayerMerge(format!(
            "symlink {} to {}: {e}",
            target.display(),
            dest_path.display()
        ))
    })
}

#[cfg(not(unix))]
fn copy_symlink(_source_path: &Path, _dest_path: &Path) -> Result<(), ImageError> {
    Err(ImageError::LayerMerge(
        "symlink layers are only supported on Unix hosts".to_string(),
    ))
}

// ── Digest verification ──────────────────────────────────────────────────

fn verify_descriptor_digest(path: &Path, expected_digest: &str) -> Result<(), ImageError> {
    let expected = expected_digest.strip_prefix("sha256:").ok_or_else(|| {
        ImageError::DigestMismatch(
            path.display().to_string(),
            expected_digest.to_string(),
            "unsupported digest algorithm".to_string(),
        )
    })?;
    let actual = compute_file_sha256_hex(path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ImageError::DigestMismatch(
            path.display().to_string(),
            format!("sha256:{expected}"),
            format!("sha256:{actual}"),
        ))
    }
}

fn compute_file_sha256_hex(path: &Path) -> Result<String, ImageError> {
    let mut file =
        File::open(path).map_err(|e| ImageError::Io(format!("open {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| ImageError::Io(format!("read {}: {e}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Derive a stable, LXD-alias-safe cache key from an image manifest
/// digest (`sha256:<hex>`). LXD alias names only allow alphanumerics and
/// `-` (see `client::validate_name`); a raw digest contains a `:`.
#[must_use]
pub fn cache_alias_for_digest(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    // 20 hex chars is enough to make accidental collisions practically
    // impossible while keeping the alias comfortably under LXD's 63-char
    // name limit alongside the fixed prefix.
    let short = &hex[..hex.len().min(20)];
    format!("openshell-oci-{short}")
}

// ── LXD image packaging ──────────────────────────────────────────────────

/// Write LXD's `metadata.yaml` with the mandatory `architecture`/
/// `creation_date` fields (per LXD's documented image format) plus a
/// description noting the source OCI reference, for operator-facing
/// `lxc image list` output.
fn write_lxd_metadata_yaml(dir: &Path, source_image_ref: &str) -> Result<(), ImageError> {
    let creation_date = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| ImageError::Package(format!("system clock before epoch: {e}")))?
        .as_secs();
    let escaped_ref = source_image_ref.replace('"', "\\\"");
    let contents = format!(
        "architecture: {arch}\n\
         creation_date: {creation_date}\n\
         properties:\n\
         \x20\x20os: openshell-sandbox\n\
         \x20\x20description: \"OpenShell sandbox (converted from {escaped_ref})\"\n",
        arch = lxd_architecture(),
    );
    fs::write(dir.join("metadata.yaml"), contents)
        .map_err(|e| ImageError::Package(format!("write metadata.yaml: {e}")))
}

/// Package a directory containing `metadata.yaml` (top level) and a
/// `rootfs/` subdirectory into a single "unified" tar archive — the
/// format `POST /1.0/images` accepts as a plain
/// `application/octet-stream` body (see
/// [`crate::client::LxdClient::create_image_from_unified_tarball`]).
fn package_unified_image_tar(
    image_dir: &Path,
    output_path: &Path,
    ownership: &HashMap<PathBuf, EntryOwnership>,
) -> Result<(), ImageError> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ImageError::Package(format!("create {}: {e}", parent.display())))?;
    }
    let file = File::create(output_path)
        .map_err(|e| ImageError::Package(format!("create {}: {e}", output_path.display())))?;
    let mut builder = tar::Builder::new(file);
    append_tree_to_archive(&mut builder, image_dir, Path::new(""), ownership)
        .map_err(|e| ImageError::Package(format!("archive {}: {e}", image_dir.display())))?;
    builder
        .finish()
        .map_err(|e| ImageError::Package(format!("finalize {}: {e}", output_path.display())))
}

/// Apply this path's originally-declared ownership (see
/// [`EntryOwnership`]) to a tar header about to be written, if tracked —
/// overriding whatever `set_metadata`/on-disk ownership it was just built
/// with. Every path merged from a real OCI layer is tracked; only
/// driver-generated paths (currently just `metadata.yaml` itself, whose
/// ownership LXD never inspects) are not, so a miss here is expected and
/// not an error.
fn apply_tracked_ownership(
    header: &mut tar::Header,
    source_path: &Path,
    ownership: &HashMap<PathBuf, EntryOwnership>,
) {
    if let Some(owner) = ownership.get(source_path) {
        header.set_uid(owner.uid);
        header.set_gid(owner.gid);
    }
}

fn append_tree_to_archive(
    builder: &mut tar::Builder<File>,
    source: &Path,
    archive_prefix: &Path,
    ownership: &HashMap<PathBuf, EntryOwnership>,
) -> std::io::Result<()> {
    let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let entry_name = entry.file_name();
        let source_path = entry.path();
        let archive_path = if archive_prefix.as_os_str().is_empty() {
            PathBuf::from(&entry_name)
        } else {
            archive_prefix.join(&entry_name)
        };
        let metadata = fs::symlink_metadata(&source_path)?;
        let file_type = metadata.file_type();

        if file_type.is_dir() {
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            apply_tracked_ownership(&mut header, &source_path, ownership);
            header.set_cksum();
            builder.append_data(&mut header, &archive_path, std::io::empty())?;
            append_tree_to_archive(builder, &source_path, &archive_path, ownership)?;
        } else if file_type.is_file() {
            let mut file = File::open(&source_path)?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            apply_tracked_ownership(&mut header, &source_path, ownership);
            header.set_cksum();
            builder.append_data(&mut header, &archive_path, &mut file)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&source_path)?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            header.set_size(0);
            apply_tracked_ownership(&mut header, &source_path, ownership);
            header.set_cksum();
            builder.append_link(&mut header, &archive_path, target)?;
        } else {
            return Err(std::io::Error::other(format!(
                "unsupported rootfs entry type at {}",
                source_path.display()
            )));
        }
    }
    Ok(())
}

// ── Per-digest conversion locking ────────────────────────────────────────
//
// Without this, two `CreateSandbox` calls that both request the same
// image digest around the same time each independently run the full
// pull/merge/package/upload pipeline — not just wasted duplicate work,
// but actively *worse* under load, since it doubles memory/disk/CPU
// pressure at exactly the moment a large, slow conversion is already
// straining those resources (found running a real 13-layer, ~2.7GB
// image: a second sandbox request landed while the first's conversion
// was still in flight, logged its own "cache miss," and started a
// second full conversion of the identical image, instead of either
// waiting for the first to finish or observing a cache hit once it
// did). tonic gives each RPC its own task with no serialization between
// them, so this driver has to provide it itself.
//
// Process-wide, not per-`LxdComputeDriver`-instance: `ensure_lxd_image`
// is a free function (not a method), and a single driver process only
// ever runs one LXD-facing binary anyway, so a process-wide registry is
// equivalent to a driver-instance-scoped one here without threading an
// extra field through every caller.
static IN_FLIGHT_CONVERSIONS: OnceLock<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    OnceLock::new();

/// Get (creating if absent) the async lock guarding conversion of a
/// specific image digest. Entries are never evicted — acceptable here
/// because the number of *distinct* digests a long-running driver
/// process ever converts is bounded by the number of distinct sandbox
/// images actually used, not unbounded; a full eviction scheme (dropping
/// entries once no one holds or awaits them) would need
/// `Arc::strong_count`-based bookkeeping for a real-world-negligible
/// amount of memory saved.
fn conversion_lock_for_digest(digest: &str) -> Arc<AsyncMutex<()>> {
    let registry = IN_FLIGHT_CONVERSIONS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut map = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(digest.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

// ── Orchestration ─────────────────────────────────────────────────────────

/// The result of resolving (or converting and caching) an OCI image as an
/// LXD image.
#[derive(Debug, Clone)]
pub struct ConvertedImage {
    /// The LXD image fingerprint to use as `source.fingerprint` when
    /// creating an instance.
    pub fingerprint: String,
    /// The cache alias this fingerprint is registered under (see
    /// [`cache_alias_for_digest`]).
    pub alias: String,
    /// The source image's declared `Env`/`WorkingDir`/`User`/entrypoint —
    /// always resolved, cache hit or miss (see this module's doc comment,
    /// point 5), so callers get consistent config translation regardless
    /// of whether the expensive layer-flatten path ran this time.
    pub config: OciImageConfig,
}

/// Resolve `image_ref` to a ready-to-use LXD image, converting and
/// caching it if this exact manifest digest hasn't been converted before.
///
/// `staging_dir` is scratch space for the pull/merge/package steps —
/// cleaned up on success, left in place on failure for diagnosis (mirrors
/// this driver's existing diagnostic-preservation convention in
/// `hack/run-stage2.sh`).
pub async fn ensure_lxd_image(
    lxd: &LxdClient,
    image_ref: &str,
    staging_dir: &Path,
) -> Result<ConvertedImage, ImageError> {
    tracing::info!(
        image_ref,
        "resolving sandbox image via OCI-to-LXD conversion pipeline"
    );
    let reference = parse_registry_reference(image_ref)?;
    let auth = registry_auth(image_ref)?;
    let client = registry_client();

    client
        .auth(&reference, &auth, RegistryOperation::Pull)
        .await
        .map_err(|e| ImageError::Auth(image_ref.to_string(), e.to_string()))?;
    let (manifest, digest) = client
        .pull_image_manifest(&reference, &auth)
        .await
        .map_err(|e| ImageError::Manifest(image_ref.to_string(), e.to_string()))?;
    tracing::info!(
        image_ref,
        digest,
        layer_count = manifest.layers.len(),
        "pulled image manifest"
    );

    fs::create_dir_all(staging_dir)
        .map_err(|e| ImageError::Io(format!("create {}: {e}", staging_dir.display())))?;

    // Always resolve the (small) config blob, cache hit or miss — see the
    // module doc comment's point 5 for why.
    let config_path = staging_dir.join("config.json");
    download_blob_to_file(
        &client,
        &reference,
        image_ref,
        &manifest.config,
        &config_path,
    )
    .await?;
    let image_config = parse_oci_image_config(
        &fs::read(&config_path)
            .map_err(|e| ImageError::Io(format!("read {}: {e}", config_path.display())))?,
    )?;
    let _ = fs::remove_file(&config_path);

    let alias = cache_alias_for_digest(&digest);
    if let Some(existing) = lxd.get_image_by_alias(&alias).await? {
        tracing::info!(
            image_ref,
            digest,
            alias,
            fingerprint = existing.target,
            "cache hit: reusing previously converted LXD image, skipping layer download"
        );
        return Ok(ConvertedImage {
            fingerprint: existing.target,
            alias,
            config: image_config,
        });
    }

    // Serialize actual conversion per digest (see this section's own doc
    // comment above `IN_FLIGHT_CONVERSIONS`). Re-check the cache once the
    // lock is held ("double-checked locking"): if a concurrent caller for
    // this same digest was already mid-conversion when we did the first
    // check above, it may have finished and published the alias by the
    // time we actually acquire the lock — in which case this call should
    // observe a cache hit too, not redo the work just because it lost a
    // race to *start*.
    let conversion_lock = conversion_lock_for_digest(&digest);
    let _conversion_guard = conversion_lock.lock().await;
    if let Some(existing) = lxd.get_image_by_alias(&alias).await? {
        tracing::info!(
            image_ref,
            digest,
            alias,
            fingerprint = existing.target,
            "cache hit after waiting for a concurrent conversion of the same digest to finish"
        );
        return Ok(ConvertedImage {
            fingerprint: existing.target,
            alias,
            config: image_config,
        });
    }
    tracing::info!(
        image_ref,
        digest,
        alias,
        "cache miss: converting image from scratch"
    );

    let image_dir = staging_dir.join("image");
    let rootfs_dir = image_dir.join("rootfs");
    fs::create_dir_all(&rootfs_dir)
        .map_err(|e| ImageError::Io(format!("create {}: {e}", rootfs_dir.display())))?;

    let layers_dir = staging_dir.join("layers");
    fs::create_dir_all(&layers_dir)
        .map_err(|e| ImageError::Io(format!("create {}: {e}", layers_dir.display())))?;

    // Deliberately sequential, one layer fully downloaded, extracted,
    // merged, and deleted before the next starts — not the download-all
    // (even with bounded concurrency), *then* merge-all shape this had
    // before. That shape kept every layer's full extracted contents *and*
    // the accumulating merged rootfs on disk simultaneously — for a
    // real, multi-layer sandbox image (13 layers, not this module's own
    // tiny test fixtures), that's 3-4x the final image size in peak
    // staging usage, and is exactly what produced a real "No space left
    // on device" failure running this against a real registry for the
    // first time. Layers must be applied in manifest order regardless
    // (later layers can whiteout/override earlier ones) — processing
    // them one at a time is the same ordering, just without ever holding
    // more than one layer's extracted contents on disk at once.
    // Accumulates every merged path's originally-declared ownership across
    // *all* layers (see `EntryOwnership`'s doc comment) — passed to
    // `package_unified_image_tar` once the whole rootfs is assembled, so
    // the final tarball's headers reflect the source image's real
    // ownership regardless of what this non-root staging process was
    // actually able to leave on disk.
    let mut rootfs_ownership: HashMap<PathBuf, EntryOwnership> = HashMap::new();

    let total_layers = manifest.layers.len();
    for (index, layer) in manifest.layers.iter().cloned().enumerate() {
        let (_, layer_root, layer_ownership) =
            download_and_extract_layer(&client, &reference, image_ref, &layers_dir, layer, index)
                .await?;
        merge_layer_directory(
            &layer_root,
            &rootfs_dir,
            &layer_ownership,
            &mut rootfs_ownership,
        )?;
        let _ = fs::remove_dir_all(&layer_root);
        tracing::debug!(
            image_ref,
            layer_index = index,
            total_layers,
            "merged layer into rootfs and freed its staging copy"
        );
    }
    tracing::info!(image_ref, total_layers, "all layers merged into rootfs");

    write_lxd_metadata_yaml(&image_dir, image_ref)?;

    let tar_path = staging_dir.join("image.tar");
    package_unified_image_tar(&image_dir, &tar_path, &rootfs_ownership)?;
    // Free the merged rootfs now, before reading the tarball into memory
    // and uploading it — its entire content is already duplicated inside
    // `tar_path`, so holding both through a potentially slow upload is
    // pure waste on exactly the resource ("disk space during
    // conversion") a real image just exhausted. `layers_dir` is already
    // empty by this point (each layer's extracted copy was freed
    // immediately after merging, above); `remove_dir_all` here is for
    // the now-empty directory itself, not leftover content.
    let _ = fs::remove_dir_all(&layers_dir);
    let _ = fs::remove_dir_all(&image_dir);
    let tarball = fs::read(&tar_path)
        .map_err(|e| ImageError::Io(format!("read {}: {e}", tar_path.display())))?;
    tracing::info!(
        image_ref,
        tarball_bytes = tarball.len(),
        "packaged unified image tarball; uploading to LXD"
    );

    let fingerprint = lxd.create_image_from_unified_tarball(tarball).await?;
    lxd.create_image_alias(&alias, &fingerprint).await?;
    tracing::info!(
        image_ref,
        digest,
        alias,
        fingerprint,
        "converted and cached new LXD image"
    );

    let _ = fs::remove_file(&tar_path);

    Ok(ConvertedImage {
        fingerprint,
        alias,
        config: image_config,
    })
}

async fn download_and_extract_layer(
    client: &OciClient,
    reference: &Reference,
    image_ref: &str,
    layers_dir: &Path,
    layer: OciDescriptor,
    index: usize,
) -> Result<(usize, PathBuf, HashMap<PathBuf, EntryOwnership>), ImageError> {
    let digest_component = sanitize_digest(&layer.digest);
    let blob_path = layers_dir.join(format!("{index:02}-{digest_component}.blob"));
    let layer_root = layers_dir.join(format!("{index:02}-{digest_component}.root"));

    tracing::debug!(image_ref, layer_index = index, digest = %layer.digest, size = layer.size, "downloading layer");
    download_blob_to_file(client, reference, image_ref, &layer, &blob_path).await?;

    let media_type = layer.media_type.clone();
    let blob_path_for_extract = blob_path.clone();
    let layer_root_for_extract = layer_root.clone();
    let ownership = tokio::task::spawn_blocking(move || {
        extract_layer_blob_to_dir(&blob_path_for_extract, &media_type, &layer_root_for_extract)
    })
    .await
    .map_err(|e| {
        ImageError::LayerExtract(
            layer.digest.clone(),
            format!("extraction task panicked: {e}"),
        )
    })??;

    let _ = fs::remove_file(&blob_path);
    Ok((index, layer_root, ownership))
}

async fn download_blob_to_file(
    client: &OciClient,
    reference: &Reference,
    image_ref: &str,
    descriptor: &OciDescriptor,
    dest: &Path,
) -> Result<(), ImageError> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ImageError::Io(format!("create {}: {e}", parent.display())))?;
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| ImageError::Io(format!("create {}: {e}", dest.display())))?;
    client
        .pull_blob(reference, descriptor, &mut file)
        .await
        .map_err(|e| {
            ImageError::Blob(
                descriptor.digest.clone(),
                image_ref.to_string(),
                e.to_string(),
            )
        })?;
    {
        use tokio::io::AsyncWriteExt as _;
        file.flush()
            .await
            .map_err(|e| ImageError::Io(format!("flush {}: {e}", dest.display())))?;
    }

    let dest_for_digest = dest.to_path_buf();
    let expected_digest = descriptor.digest.clone();
    tokio::task::spawn_blocking(move || {
        verify_descriptor_digest(&dest_for_digest, &expected_digest)
    })
    .await
    .map_err(|e| {
        ImageError::Blob(
            descriptor.digest.clone(),
            image_ref.to_string(),
            format!("verification task panicked: {e}"),
        )
    })?
}

fn sanitize_digest(digest: &str) -> String {
    digest.replace([':', '/', '@'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_oci_image_config_extracts_env_workdir_user_entrypoint() {
        let json = br#"{
            "config": {
                "Env": ["PATH=/usr/local/bin:/usr/bin", "LANG=C.UTF-8"],
                "WorkingDir": "/app",
                "User": "1000:1000",
                "Entrypoint": ["/usr/bin/python3"],
                "Cmd": ["app.py"]
            }
        }"#;
        let config = parse_oci_image_config(json).expect("parse config");
        assert_eq!(
            config.env,
            vec![
                "PATH=/usr/local/bin:/usr/bin".to_string(),
                "LANG=C.UTF-8".to_string()
            ]
        );
        assert_eq!(config.working_dir, Some("/app".to_string()));
        assert_eq!(config.user, Some("1000:1000".to_string()));
        assert_eq!(config.entrypoint, vec!["/usr/bin/python3".to_string()]);
        assert_eq!(config.cmd, vec!["app.py".to_string()]);
    }

    #[test]
    fn parse_oci_image_config_defaults_on_missing_config_key() {
        let config = parse_oci_image_config(b"{}").expect("parse config");
        assert_eq!(config, OciImageConfig::default());
    }

    #[test]
    fn parse_oci_image_config_defaults_on_empty_config_object() {
        let config = parse_oci_image_config(br#"{"config":{}}"#).expect("parse config");
        assert_eq!(config, OciImageConfig::default());
    }

    #[test]
    fn parse_oci_image_config_rejects_invalid_json() {
        assert!(parse_oci_image_config(b"not json").is_err());
    }

    #[test]
    fn cache_alias_for_digest_strips_algorithm_prefix_and_is_lxd_safe() {
        let alias = cache_alias_for_digest("sha256:0123456789abcdef0123456789abcdef01234567");
        assert_eq!(alias, "openshell-oci-0123456789abcdef0123");
        assert!(crate::client::validate_name(&alias).is_ok());
    }

    #[test]
    fn cache_alias_for_digest_handles_short_digests_without_panicking() {
        let alias = cache_alias_for_digest("sha256:ab");
        assert_eq!(alias, "openshell-oci-ab");
    }

    #[test]
    fn layer_compression_from_media_type_recognizes_gzip_zstd_and_plain_tar() {
        assert_eq!(
            layer_compression_from_media_type("application/vnd.oci.image.layer.v1.tar+gzip")
                .unwrap(),
            LayerCompression::Gzip
        );
        assert_eq!(
            layer_compression_from_media_type("application/vnd.oci.image.layer.v1.tar+zstd")
                .unwrap(),
            LayerCompression::Zstd
        );
        assert_eq!(
            layer_compression_from_media_type("application/vnd.oci.image.layer.v1.tar").unwrap(),
            LayerCompression::None
        );
    }

    #[test]
    fn layer_compression_from_media_type_rejects_unknown_and_empty() {
        assert!(layer_compression_from_media_type("").is_err());
        assert!(
            layer_compression_from_media_type("application/vnd.docker.container.image.v1+json")
                .is_err()
        );
    }

    #[test]
    fn merge_layer_directory_honors_per_file_and_opaque_whiteouts() {
        let base = unique_temp_dir();
        let rootfs = base.join("rootfs");
        let layer = base.join("layer");

        fs::create_dir_all(rootfs.join("dir")).unwrap();
        fs::write(rootfs.join("dir/keep.txt"), "keep").unwrap();
        fs::write(rootfs.join("removed.txt"), "old").unwrap();

        fs::create_dir_all(&layer).unwrap();
        fs::create_dir_all(layer.join("dir")).unwrap();
        fs::write(layer.join(".wh.removed.txt"), "").unwrap();
        fs::write(layer.join("dir/.wh..wh..opq"), "").unwrap();
        fs::write(layer.join("dir/new.txt"), "new").unwrap();

        let mut rootfs_ownership = HashMap::new();
        merge_layer_directory(&layer, &rootfs, &HashMap::new(), &mut rootfs_ownership).unwrap();

        assert!(!rootfs.join("removed.txt").exists());
        assert!(
            !rootfs.join("dir/keep.txt").exists(),
            "opaque whiteout should clear prior dir contents"
        );
        assert_eq!(
            fs::read_to_string(rootfs.join("dir/new.txt")).unwrap(),
            "new"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn merge_layer_directory_overwrites_files_across_layers() {
        let base = unique_temp_dir();
        let rootfs = base.join("rootfs");
        let layer = base.join("layer");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&layer).unwrap();
        fs::write(rootfs.join("app.conf"), "v1").unwrap();
        fs::write(layer.join("app.conf"), "v2").unwrap();

        let mut rootfs_ownership = HashMap::new();
        merge_layer_directory(&layer, &rootfs, &HashMap::new(), &mut rootfs_ownership).unwrap();

        assert_eq!(fs::read_to_string(rootfs.join("app.conf")).unwrap(), "v2");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn write_lxd_metadata_yaml_contains_mandatory_fields_and_escapes_the_source_ref() {
        let base = unique_temp_dir();
        fs::create_dir_all(&base).unwrap();

        write_lxd_metadata_yaml(&base, "ghcr.io/example/\"weird\":latest").unwrap();

        let contents = fs::read_to_string(base.join("metadata.yaml")).unwrap();
        assert!(contents.contains("architecture:"));
        assert!(contents.contains("creation_date:"));
        assert!(contents.contains("\\\"weird\\\""));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn package_unified_image_tar_includes_metadata_and_rootfs_tree() {
        let base = unique_temp_dir();
        let image_dir = base.join("image");
        let rootfs = image_dir.join("rootfs");
        fs::create_dir_all(rootfs.join("etc")).unwrap();
        fs::write(rootfs.join("etc/hostname"), "sandbox\n").unwrap();
        write_lxd_metadata_yaml(&image_dir, "example:latest").unwrap();

        let tar_path = base.join("image.tar");
        package_unified_image_tar(&image_dir, &tar_path, &HashMap::new()).unwrap();

        let file = File::open(&tar_path).unwrap();
        let mut archive = tar::Archive::new(file);
        let paths: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert!(paths.contains(&"metadata.yaml".to_string()));
        assert!(paths.iter().any(|p| p == "rootfs/etc/hostname"));

        let _ = fs::remove_dir_all(&base);
    }

    /// Build a minimal, valid tar archive (bytes, not written to disk) with
    /// one directory entry and one file entry, both declaring a specific
    /// `uid`/`gid` in their headers — a stand-in for a real OCI layer
    /// blob's own declared ownership, without needing a real registry.
    fn build_test_layer_tar(dir_uid: u64, dir_gid: u64, file_uid: u64, file_gid: u64) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());

        let mut dir_header = tar::Header::new_gnu();
        dir_header.set_path("run").unwrap();
        dir_header.set_entry_type(tar::EntryType::Directory);
        dir_header.set_size(0);
        dir_header.set_mode(0o755);
        dir_header.set_uid(dir_uid);
        dir_header.set_gid(dir_gid);
        dir_header.set_cksum();
        builder.append(&dir_header, std::io::empty()).unwrap();

        let contents = b"hello";
        let mut file_header = tar::Header::new_gnu();
        file_header.set_path("run/config").unwrap();
        file_header.set_entry_type(tar::EntryType::Regular);
        file_header.set_size(contents.len() as u64);
        file_header.set_mode(0o644);
        file_header.set_uid(file_uid);
        file_header.set_gid(file_gid);
        file_header.set_cksum();
        builder.append(&file_header, &contents[..]).unwrap();

        builder.into_inner().unwrap()
    }

    /// End-to-end regression test for the exact bug that broke a real
    /// sandbox image: a supervisor's `mkdir /run/netns` failing with
    /// `EACCES` inside an LXD container, traced back to this pipeline
    /// silently losing `/run`'s root ownership during conversion (see
    /// `EntryOwnership`'s doc comment for the full mechanism). Runs the
    /// real extract -> merge -> package pipeline (not a unit test of any
    /// one function in isolation) against a synthetic layer declaring
    /// `uid=0` for a directory, and asserts the *final packaged tarball's
    /// header* still says `uid=0` -- proving the fix works end to end,
    /// not just that some intermediate function has the right signature.
    #[test]
    fn image_pipeline_preserves_declared_ownership_despite_non_root_staging() {
        // No `libc` dependency needed to learn our own euid: any path we
        // just created ourselves is necessarily owned by it.
        let base_for_uid_probe = unique_temp_dir();
        fs::create_dir_all(&base_for_uid_probe).unwrap();
        use std::os::unix::fs::MetadataExt;
        let current_uid = u64::from(fs::metadata(&base_for_uid_probe).unwrap().uid());
        let _ = fs::remove_dir_all(&base_for_uid_probe);

        // If this test process happens to run as root, the bug this test
        // guards against can't manifest at all (root *can* chown to
        // arbitrary UIDs, so extraction would preserve uid=0 correctly
        // even without this fix) -- the assertion below would pass for
        // the wrong reason. Use a uid guaranteed to differ from root
        // rather than skip outright, so the test still means something
        // even if some future CI runner happens to run as root.
        let declared_uid = if current_uid == 0 { 4242 } else { 0 };

        let base = unique_temp_dir();
        fs::create_dir_all(&base).unwrap();
        let blob_path = base.join("layer.tar");
        fs::write(
            &blob_path,
            build_test_layer_tar(declared_uid, declared_uid, declared_uid, declared_uid),
        )
        .unwrap();

        let layer_root = base.join("layer-root");
        let layer_ownership = extract_layer_blob_to_dir(
            &blob_path,
            "application/vnd.oci.image.layer.v1.tar",
            &layer_root,
        )
        .unwrap();

        // Confirm the premise: on-disk ownership is NOT preserved (this
        // is what makes the bug real, not hypothetical) -- the extracted
        // directory is actually owned by whoever ran this test, not by
        // `declared_uid`.
        let on_disk_uid = fs::metadata(layer_root.join("run")).unwrap().uid();
        assert_ne!(
            u64::from(on_disk_uid),
            declared_uid,
            "test premise violated: on-disk ownership was preserved, so this test can't \
             distinguish 'fix works' from 'bug never applied here' (current_uid={current_uid})"
        );

        let rootfs_dir = base.join("image").join("rootfs");
        fs::create_dir_all(&rootfs_dir).unwrap();
        let mut rootfs_ownership = HashMap::new();
        merge_layer_directory(
            &layer_root,
            &rootfs_dir,
            &layer_ownership,
            &mut rootfs_ownership,
        )
        .unwrap();

        let image_dir = base.join("image");
        write_lxd_metadata_yaml(&image_dir, "test:latest").unwrap();
        let tar_path = base.join("packaged.tar");
        package_unified_image_tar(&image_dir, &tar_path, &rootfs_ownership).unwrap();

        let file = File::open(&tar_path).unwrap();
        let mut archive = tar::Archive::new(file);
        let mut checked_dir = false;
        let mut checked_file = false;
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.header().path().unwrap().into_owned();
            if path == Path::new("rootfs/run") {
                assert_eq!(
                    entry.header().uid().unwrap(),
                    declared_uid,
                    "packaged tarball must carry the original layer's declared uid for /run, \
                     not whatever the non-root staging process left on disk"
                );
                checked_dir = true;
            } else if path == Path::new("rootfs/run/config") {
                assert_eq!(entry.header().uid().unwrap(), declared_uid);
                checked_file = true;
            }
        }
        assert!(
            checked_dir,
            "expected to find rootfs/run in the packaged tarball"
        );
        assert!(
            checked_file,
            "expected to find rootfs/run/config in the packaged tarball"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn conversion_lock_for_digest_returns_the_same_lock_for_the_same_digest() {
        let a = conversion_lock_for_digest("sha256:same-digest-test");
        let b = conversion_lock_for_digest("sha256:same-digest-test");
        assert!(
            Arc::ptr_eq(&a, &b),
            "two lookups of the same digest should return the same underlying lock"
        );
    }

    #[test]
    fn conversion_lock_for_digest_returns_different_locks_for_different_digests() {
        let a = conversion_lock_for_digest("sha256:digest-one");
        let b = conversion_lock_for_digest("sha256:digest-two");
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different digests must not share a lock"
        );
    }

    #[tokio::test]
    async fn conversion_lock_for_digest_actually_serializes_concurrent_holders() {
        let digest = "sha256:concurrency-test-digest";
        let lock = conversion_lock_for_digest(digest);
        let order = Arc::new(StdMutex::new(Vec::new()));

        let guard = lock.lock().await;
        let lock2 = conversion_lock_for_digest(digest);
        let order2 = order.clone();
        let waiter = tokio::spawn(async move {
            let _guard = lock2.lock().await;
            order2.lock().unwrap().push("waiter");
        });

        // Give the spawned task a chance to actually block on the lock
        // before we record that we're still holding it and release it.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        order.lock().unwrap().push("holder");
        drop(guard);
        waiter.await.expect("waiter task should complete");

        assert_eq!(*order.lock().unwrap(), vec!["holder", "waiter"]);
    }

    #[test]
    fn image_reference_registry_host_recognizes_ghcr_and_defaults_to_docker_io() {
        assert_eq!(
            image_reference_registry_host(
                "ghcr.io/nvidia/openshell-community/sandboxes/base:latest"
            ),
            "ghcr.io"
        );
        assert_eq!(image_reference_registry_host("ubuntu:26.04"), "docker.io");
        assert_eq!(
            image_reference_registry_host("library/ubuntu:26.04"),
            "docker.io"
        );
    }

    #[test]
    fn registry_auth_defaults_to_anonymous_without_env_vars() {
        // SAFETY: test-only env var manipulation, no other test in this
        // module reads these same keys concurrently within one process
        // (cargo test runs each test in its own thread but shares env).
        unsafe {
            std::env::remove_var("OPENSHELL_REGISTRY_TOKEN");
            std::env::remove_var("OPENSHELL_REGISTRY_USERNAME");
        }
        let auth = registry_auth("ubuntu:26.04").expect("resolve auth");
        assert!(matches!(auth, RegistryAuth::Anonymous));
    }

    fn unique_temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "openshell-driver-lxd-image-test-{}-{nanos}-{suffix}",
            std::process::id()
        ))
    }
}
