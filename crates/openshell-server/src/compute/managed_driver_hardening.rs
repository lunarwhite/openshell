// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared hardening for a gateway-managed driver subprocess's private
//! state/socket directories.
//!
//! Every managed driver (currently `vm`, and `lxd` as of Phase 2) needs the
//! same defensive checks before binding a Unix domain socket the gateway
//! will dial: the state directory and the socket's parent directory must
//! be real, owner-only directories (not symlinks, not world/group
//! readable, not owned by a different uid), and a stale socket left behind
//! by a crashed previous run must be a genuine socket owned by the current
//! process before it's safe to unlink and rebind. Extracted here (originally
//! `compute::vm`'s own, driver-name-hardcoded private functions) so a
//! second managed driver doesn't duplicate ~150 lines of the same security
//! logic verbatim — see
//! `crates/openshell-driver-lxd/docs/04-implementation-plan.md`'s Phase 2
//! Step 3.
//!
//! Every function takes a `driver_label` purely for error-message text
//! (e.g. `"vm"` or `"lxd"`) — the actual logic is identical regardless of
//! which managed driver is calling it.
//!
//! Also home to [`resolve_binary_path`], the "look in a configured
//! directory, then conventional install locations, then next to the
//! gateway's own executable" search shared by every managed driver's own
//! `resolve_compute_driver_bin`-shaped function — plus, for `lxd`
//! specifically, resolving the *supervisor* binary path the same way (a
//! second binary that driver needs, distinct from its own driver binary).

#[cfg(unix)]
use openshell_core::{Error, Result};
#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
pub(super) fn current_euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Resolve a named binary's path.
///
/// Resolution order:
/// 1. `{driver_dir}/{bin_name}`, if `driver_dir` is `Some` — no fallback
///    chain when this is set explicitly; an operator who names a
///    directory means exactly that directory.
/// 2. Otherwise: a `libexec`/`libexec/openshell` dir relative to the
///    gateway's own executable, then conventional install locations
///    (`~/.local/libexec/openshell`, `/usr/libexec/openshell`,
///    `/usr/local/libexec/openshell`, `/usr/local/libexec`).
/// 3. Sibling of the gateway's own executable (last-resort fallback so
///    local development builds still work out of the box).
///
/// `hint` is appended to the not-found error, naming the config key a
/// caller should set (e.g. `"[openshell.drivers.vm].driver_dir"`).
#[cfg(unix)]
pub(super) fn resolve_binary_path(
    bin_name: &str,
    driver_dir: Option<&Path>,
    hint: &str,
) -> Result<std::path::PathBuf> {
    use std::path::PathBuf;

    let mut searched: Vec<PathBuf> = Vec::new();
    for dir in binary_search_dirs(driver_dir) {
        let candidate = dir.join(bin_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
        push_unique_path(&mut searched, candidate);
    }

    let current_exe = std::env::current_exe()
        .map_err(|e| Error::config(format!("failed to resolve current executable: {e}")))?;
    let Some(parent) = current_exe.parent() else {
        return Err(Error::config(format!(
            "current executable '{}' has no parent directory",
            current_exe.display()
        )));
    };
    let sibling = parent.join(bin_name);
    if sibling.is_file() {
        return Ok(sibling);
    }
    push_unique_path(&mut searched, sibling);

    let searched_display = searched
        .iter()
        .map(|p| format!("'{}'", p.display()))
        .collect::<Vec<_>>()
        .join(", ");
    Err(Error::config(format!(
        "{bin_name} binary not found (searched {searched_display}); {hint}"
    )))
}

#[cfg(unix)]
fn binary_search_dirs(driver_dir: Option<&Path>) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;

    if let Some(dir) = driver_dir {
        return vec![dir.to_path_buf()];
    }
    let mut dirs = Vec::new();
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(prefix) = current_exe.parent().and_then(Path::parent)
    {
        push_unique_path(&mut dirs, prefix.join("libexec"));
        push_unique_path(&mut dirs, prefix.join("libexec").join("openshell"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        push_unique_path(
            &mut dirs,
            home.join(".local").join("libexec").join("openshell"),
        );
    }
    push_unique_path(&mut dirs, PathBuf::from("/usr/libexec/openshell"));
    push_unique_path(&mut dirs, PathBuf::from("/usr/local/libexec/openshell"));
    push_unique_path(&mut dirs, PathBuf::from("/usr/local/libexec"));
    dirs
}

#[cfg(unix)]
fn push_unique_path(paths: &mut Vec<std::path::PathBuf>, path: std::path::PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

/// Create (if needed) and restrict a managed driver's private state
/// directory to `0700`, owned by the current process.
#[cfg(unix)]
pub(super) fn prepare_state_dir(
    state_dir: &Path,
    expected_uid: u32,
    driver_label: &str,
) -> Result<()> {
    std::fs::create_dir_all(state_dir).map_err(|err| {
        Error::execution(format!(
            "failed to create {driver_label} driver state dir '{}': {err}",
            state_dir.display()
        ))
    })?;
    let metadata = checked_directory_metadata(
        state_dir,
        expected_uid,
        &format!("{driver_label} driver state dir"),
    )?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).map_err(
            |err| {
                Error::execution(format!(
                    "failed to restrict {driver_label} driver state dir '{}': {err}",
                    state_dir.display()
                ))
            },
        )?;
    }
    Ok(())
}

/// Create (if needed) and restrict a managed driver's socket parent
/// directory to `0700`, owned by the current process.
#[cfg(unix)]
pub(super) fn prepare_private_socket_dir(
    socket_dir: &Path,
    expected_uid: u32,
    driver_label: &str,
) -> Result<()> {
    std::fs::create_dir_all(socket_dir).map_err(|err| {
        Error::execution(format!(
            "failed to create {driver_label} compute driver socket dir '{}': {err}",
            socket_dir.display()
        ))
    })?;
    let _ = checked_directory_metadata(
        socket_dir,
        expected_uid,
        &format!("{driver_label} compute driver socket dir"),
    )?;
    std::fs::set_permissions(socket_dir, std::fs::Permissions::from_mode(0o700)).map_err(|err| {
        Error::execution(format!(
            "failed to restrict {driver_label} compute driver socket dir '{}': {err}",
            socket_dir.display()
        ))
    })
}

#[cfg(unix)]
pub(super) fn checked_directory_metadata(
    path: &Path,
    expected_uid: u32,
    label: &str,
) -> Result<std::fs::Metadata> {
    let metadata = std::fs::symlink_metadata(path).map_err(|err| {
        Error::execution(format!(
            "failed to stat {label} '{}': {err}",
            path.display()
        ))
    })?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(Error::execution(format!(
            "{label} '{}' is a symlink; refusing to use it",
            path.display()
        )));
    }
    if !file_type.is_dir() {
        return Err(Error::execution(format!(
            "{label} '{}' is not a directory",
            path.display()
        )));
    }
    if metadata.uid() != expected_uid {
        return Err(Error::execution(format!(
            "{label} '{}' is owned by uid {} but current euid is {}",
            path.display(),
            metadata.uid(),
            expected_uid
        )));
    }
    Ok(metadata)
}

/// Remove a stale socket left behind by a previous crashed run of a managed
/// driver, refusing to touch anything that isn't verifiably a socket this
/// process owns.
#[cfg(unix)]
pub(super) fn remove_stale_socket(
    socket_path: &Path,
    expected_uid: u32,
    driver_label: &str,
) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(Error::execution(format!(
                "failed to stat {driver_label} compute driver socket '{}': {err}",
                socket_path.display()
            )));
        }
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(Error::execution(format!(
            "{driver_label} compute driver socket '{}' is a symlink; refusing to remove it",
            socket_path.display()
        )));
    }
    if metadata.uid() != expected_uid {
        return Err(Error::execution(format!(
            "{driver_label} compute driver socket '{}' is owned by uid {} but current euid is {}",
            socket_path.display(),
            metadata.uid(),
            expected_uid
        )));
    }
    if !file_type.is_socket() {
        return Err(Error::execution(format!(
            "{driver_label} compute driver socket path '{}' exists but is not a Unix socket",
            socket_path.display()
        )));
    }
    std::fs::remove_file(socket_path).map_err(|err| {
        Error::execution(format!(
            "failed to remove stale {driver_label} compute driver socket '{}': {err}",
            socket_path.display()
        ))
    })
}

/// Run the full sequence a managed driver needs before binding its socket:
/// harden the state dir, harden the socket's parent dir, and clear any
/// stale socket left behind by a crashed previous run.
#[cfg(unix)]
pub(super) fn prepare_managed_driver_socket_path(
    state_dir: &Path,
    socket_path: &Path,
    driver_label: &str,
) -> Result<()> {
    let expected_uid = current_euid();
    prepare_state_dir(state_dir, expected_uid, driver_label)?;
    let parent = socket_path.parent().ok_or_else(|| {
        Error::execution(format!(
            "{driver_label} compute driver socket path '{}' has no parent directory",
            socket_path.display()
        ))
    })?;
    prepare_private_socket_dir(parent, expected_uid, driver_label)?;
    remove_stale_socket(socket_path, expected_uid, driver_label)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener as StdUnixListener;
    use tempfile::tempdir;

    #[test]
    fn prepare_state_dir_restricts_mode_to_0700() {
        let base = tempdir().unwrap();
        let state_dir = base.path().join("state");
        prepare_state_dir(&state_dir, current_euid(), "test").unwrap();
        let mode = std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn checked_directory_metadata_rejects_symlink() {
        let base = tempdir().unwrap();
        let real_dir = base.path().join("real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let link = base.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        let err = checked_directory_metadata(&link, current_euid(), "test dir").unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[test]
    fn checked_directory_metadata_rejects_wrong_owner() {
        let base = tempdir().unwrap();
        let dir = base.path().join("owned");
        std::fs::create_dir_all(&dir).unwrap();
        let err = checked_directory_metadata(&dir, current_euid() + 1, "test dir").unwrap_err();
        assert!(err.to_string().contains("owned by uid"));
    }

    #[test]
    fn remove_stale_socket_is_a_noop_when_absent() {
        let base = tempdir().unwrap();
        let socket_path = base.path().join("nonexistent.sock");
        remove_stale_socket(&socket_path, current_euid(), "test").unwrap();
    }

    #[test]
    fn remove_stale_socket_removes_a_real_owned_socket() {
        let base = tempdir().unwrap();
        let socket_path = base.path().join("stale.sock");
        let listener = StdUnixListener::bind(&socket_path).unwrap();
        drop(listener);
        assert!(socket_path.exists());
        remove_stale_socket(&socket_path, current_euid(), "test").unwrap();
        assert!(!socket_path.exists());
    }

    #[test]
    fn remove_stale_socket_refuses_a_non_socket_path() {
        let base = tempdir().unwrap();
        let path = base.path().join("not-a-socket");
        std::fs::write(&path, b"hello").unwrap();
        let err = remove_stale_socket(&path, current_euid(), "test").unwrap_err();
        assert!(err.to_string().contains("not a Unix socket"));
    }

    #[test]
    fn prepare_managed_driver_socket_path_hardens_state_and_socket_dirs() {
        let base = tempdir().unwrap();
        let state_dir = base.path().join("state");
        let socket_path = state_dir.join("run").join("compute-driver.sock");
        prepare_managed_driver_socket_path(&state_dir, &socket_path, "test").unwrap();
        let state_mode = std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(state_mode, 0o700);
        let socket_dir_mode = std::fs::metadata(socket_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(socket_dir_mode, 0o700);
    }
}
