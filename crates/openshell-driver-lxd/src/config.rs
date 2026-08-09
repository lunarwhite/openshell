// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

/// Default LXD Unix socket path on Ubuntu when LXD is installed as a snap
/// (the packaging path this driver targets; see the implementation plan's
/// non-goal on the legacy Debian/Ubuntu-archive package).
pub const DEFAULT_LXD_SOCKET_PATH: &str = "/var/snap/lxd/common/lxd/unix.socket";

/// Default managed LXD bridge network name.
pub const DEFAULT_NETWORK_NAME: &str = "openshell";

/// Default explicit IPv4 subnet for the managed bridge network, used only
/// when the network doesn't already exist and needs to be created.
///
/// LXD's own auto-picker for this (used when no `ipv4.address` is given)
/// is empirically unreliable in nested/VM environments — see
/// [`crate::client::LxdClient::ensure_network`]'s doc comment for the
/// confirmed failure mode this default avoids.
pub const DEFAULT_NETWORK_IPV4_SUBNET: &str = "10.77.99.1/24";

/// Default LXD storage pool name.
///
/// Phase 1 pins to one backend rather than claiming backend-agnostic
/// behavior (see `lxd-04-implementation-plan.md`, "Step 0"): `shift=true`
/// idmap-shifted disk devices — needed for supervisor/JWT delivery without
/// `security.privileged` — do not behave uniformly across LXD's storage
/// drivers. This default assumes a `dir`-backed storage pool named
/// `default`, the common case on a fresh LXD install.
pub const DEFAULT_STORAGE_POOL: &str = "default";

/// Default instance stop timeout in seconds before LXD escalates.
pub const DEFAULT_STOP_TIMEOUT_SECS: u32 = 45;

/// Configuration for the Phase 1 LXD compute driver.
///
/// Deliberately thin relative to the Docker/Podman configs: no resource
/// limits, driver-config mounts, or mTLS fields yet — those are explicit
/// Phase 2 feature-parity items (`lxd-03-design-rfc.md`, "Phase 2 shape").
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LxdComputeConfig {
    /// LXD API Unix socket path.
    pub socket_path: PathBuf,
    /// Pinned LXD image alias or fingerprint used for every sandbox in this
    /// phase. General OCI-image resolution is a Phase 2 item (the
    /// OCI-to-LXD image conversion pipeline); Phase 1 targets exactly one
    /// image, converted once by hand (`umoci unpack` + `lxc image import`)
    /// outside the driver.
    pub default_image: String,
    /// Name of the managed LXD bridge network. Created if it does not
    /// already exist.
    pub network_name: String,
    /// Explicit IPv4 subnet (CIDR, e.g. `"10.77.99.1/24"`) applied when
    /// `network_name` needs to be created. Ignored if the network already
    /// exists. See [`crate::client::LxdClient::ensure_network`]'s doc
    /// comment for why this isn't left to LXD's own auto-picker.
    pub network_ipv4_subnet: String,
    /// LXD storage pool used for the instance's root disk.
    pub storage_pool: String,
    /// Host path to the `openshell-sandbox` supervisor binary.
    ///
    /// Phase 1 scope: a bare config path only, delivered via a read-only
    /// `disk` device. No image-based extraction/caching (Docker's
    /// `resolve_supervisor_bin` pattern) — that's a Phase 2 item.
    pub supervisor_bin: PathBuf,
    /// Gateway gRPC endpoint the sandbox supervisor dials back to.
    ///
    /// When empty, the driver reads the managed bridge's gateway IP back
    /// from LXD's network state and constructs a direct address, rather
    /// than relying solely on the `_gateway.lxd` DNS alias (documented
    /// fallback only — see the design doc's networking section).
    pub grpc_endpoint: String,
    /// Port the gateway server is actually listening on.
    pub gateway_port: u16,
    /// Unix socket path inside the sandbox the supervisor's SSH relay uses.
    pub sandbox_ssh_socket_path: String,
    /// Instance stop timeout in seconds (LXD escalates after this elapses).
    pub stop_timeout_secs: u32,
}

impl Default for LxdComputeConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from(DEFAULT_LXD_SOCKET_PATH),
            default_image: String::new(),
            network_name: DEFAULT_NETWORK_NAME.to_string(),
            network_ipv4_subnet: DEFAULT_NETWORK_IPV4_SUBNET.to_string(),
            storage_pool: DEFAULT_STORAGE_POOL.to_string(),
            supervisor_bin: PathBuf::new(),
            grpc_endpoint: String::new(),
            gateway_port: openshell_core::config::DEFAULT_SERVER_PORT,
            sandbox_ssh_socket_path: openshell_core::container_paths::SSH_SOCKET_PATH.to_string(),
            stop_timeout_secs: DEFAULT_STOP_TIMEOUT_SECS,
        }
    }
}

impl std::fmt::Debug for LxdComputeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LxdComputeConfig")
            .field("socket_path", &self.socket_path)
            .field("default_image", &self.default_image)
            .field("network_name", &self.network_name)
            .field("network_ipv4_subnet", &self.network_ipv4_subnet)
            .field("storage_pool", &self.storage_pool)
            .field("supervisor_bin", &self.supervisor_bin)
            .field("grpc_endpoint", &self.grpc_endpoint)
            .field("gateway_port", &self.gateway_port)
            .field("sandbox_ssh_socket_path", &self.sandbox_ssh_socket_path)
            .field("stop_timeout_secs", &self.stop_timeout_secs)
            .finish()
    }
}

impl LxdComputeConfig {
    /// Validate that the fields required to actually start are present.
    ///
    /// `default_image` is deliberately *not* required here, unlike Phase
    /// 1's original version of this check. It's still a useful pinned
    /// fallback for a sandbox that specifies no image of its own, but
    /// Phase 2's OCI-to-LXD conversion pipeline (`crate::image`) means a
    /// driver can now run entirely off sandbox-supplied images (the CLI's
    /// `--from`/BYOC flag) — refusing to even *start* without a pinned
    /// default would make that configuration impossible. The equivalent,
    /// still-enforced check now lives in
    /// `driver::LxdComputeDriver::validate_sandbox_create`, which *can*
    /// see whether a given sandbox brings its own image, at the point
    /// where that actually matters.
    pub fn validate(&self) -> Result<(), crate::client::LxdApiError> {
        if self.supervisor_bin.as_os_str().is_empty() {
            return Err(crate::client::LxdApiError::InvalidInput(
                "supervisor_bin must be set to the host path of the openshell-sandbox binary"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_targets_snap_socket_and_managed_network() {
        let cfg = LxdComputeConfig::default();
        assert_eq!(cfg.socket_path, PathBuf::from(DEFAULT_LXD_SOCKET_PATH));
        assert_eq!(cfg.network_name, DEFAULT_NETWORK_NAME);
        assert_eq!(cfg.network_ipv4_subnet, DEFAULT_NETWORK_IPV4_SUBNET);
        assert_eq!(cfg.storage_pool, DEFAULT_STORAGE_POOL);
        assert_eq!(cfg.stop_timeout_secs, DEFAULT_STOP_TIMEOUT_SECS);
    }

    #[test]
    fn validate_accepts_a_missing_pinned_image() {
        // Phase 2: a driver can run entirely off sandbox-supplied images
        // (the CLI's `--from`/BYOC flag, resolved via `crate::image`)
        // without ever configuring a pinned `default_image` — see this
        // function's own doc comment for why the equivalent check moved
        // to `validate_sandbox_create` instead.
        let cfg = LxdComputeConfig {
            supervisor_bin: PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..LxdComputeConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_missing_supervisor_bin() {
        let cfg = LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            ..LxdComputeConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("supervisor_bin"));
    }

    #[test]
    fn validate_accepts_complete_config() {
        let cfg = LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..LxdComputeConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }
}
