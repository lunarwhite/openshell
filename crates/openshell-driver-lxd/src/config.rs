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
/// behavior (see `docs/04-implementation-plan.md`, "Step 0"): `shift=true`
/// idmap-shifted disk devices — needed for supervisor/JWT delivery without
/// `security.privileged` — do not behave uniformly across LXD's storage
/// drivers. This default assumes a `dir`-backed storage pool named
/// `default`, the common case on a fresh LXD install.
pub const DEFAULT_STORAGE_POOL: &str = "default";

/// Default instance stop timeout in seconds before LXD escalates.
pub const DEFAULT_STOP_TIMEOUT_SECS: u32 = 45;

/// Default max concurrent processes/threads per sandbox instance. Reuses
/// Docker/Podman's own shared default rather than picking an independent
/// number — see [`openshell_core::config::DEFAULT_SANDBOX_PIDS_LIMIT`]'s
/// doc comment for why that specific value was chosen.
pub use openshell_core::config::DEFAULT_SANDBOX_PIDS_LIMIT;

/// Configuration for the LXD compute driver.
///
/// mTLS (Phase 2 Step 5), resource limits (Step 6), and driver-config
/// mounts (Step 7) are all built — see `guest_tls_ca`/`guest_tls_cert`/
/// `guest_tls_key`, `sandbox_pids_limit`, and `enable_bind_mounts` below.
/// CPU/memory limits themselves aren't driver-config fields at all: they
/// come from the sandbox's own `spec.template.resources` (a per-sandbox
/// request, not an operator setting) — see
/// [`crate::instance::lxd_resource_limits`].
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
    /// Host path to the CA certificate for sandbox guest mTLS.
    ///
    /// When all three TLS paths (`guest_tls_ca`, `guest_tls_cert`,
    /// `guest_tls_key`) are set, the driver delivers them into every
    /// sandbox instance via the same read-only `shift=true` disk-device
    /// mechanism already used for the supervisor binary and JWT (see
    /// `instance::build_instance_spec`) — not a bind-mount string, and
    /// not the OCI image volume mechanism Docker's mTLS delivery uses
    /// (LXD has neither of those; disk devices are this driver's one
    /// delivery primitive for everything).
    pub guest_tls_ca: Option<PathBuf>,
    /// Host path to the client certificate for sandbox guest mTLS.
    pub guest_tls_cert: Option<PathBuf>,
    /// Host path to the client private key for sandbox guest mTLS.
    pub guest_tls_key: Option<PathBuf>,
    /// Max concurrent processes/threads allowed inside a sandbox instance,
    /// mapped onto LXD's `limits.processes` config key. `0` means
    /// "inherit LXD's own default" (unlimited) rather than "zero
    /// processes allowed" — mirrors Docker/Podman's `docker_pids_limit`/
    /// `podman_pids_limit` exactly, so operators moving a deployment
    /// between drivers keep the same "0 = inherit" convention.
    pub sandbox_pids_limit: i64,
    /// Whether a sandbox's `driver_config.mounts` may request a host-path
    /// bind mount. Mirrors Docker/Podman's own `enable_bind_mounts` gate
    /// exactly: `false` by default, since an arbitrary host path chosen
    /// by whoever can create a sandbox is an operator-trust decision, not
    /// a per-sandbox one.
    pub enable_bind_mounts: bool,
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
            guest_tls_ca: None,
            guest_tls_cert: None,
            guest_tls_key: None,
            sandbox_pids_limit: DEFAULT_SANDBOX_PIDS_LIMIT,
            enable_bind_mounts: false,
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
            .field("guest_tls_ca", &self.guest_tls_ca)
            .field("guest_tls_cert", &self.guest_tls_cert)
            .field("guest_tls_key", &self.guest_tls_key)
            .field("sandbox_pids_limit", &self.sandbox_pids_limit)
            .field("enable_bind_mounts", &self.enable_bind_mounts)
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
        self.validate_tls_config()?;
        if self.sandbox_pids_limit < 0 {
            return Err(crate::client::LxdApiError::InvalidInput(
                "sandbox_pids_limit must be zero or greater".to_string(),
            ));
        }
        Ok(())
    }

    /// Returns `true` when all three guest mTLS paths are configured.
    #[must_use]
    pub fn tls_enabled(&self) -> bool {
        self.guest_tls_ca.is_some() && self.guest_tls_cert.is_some() && self.guest_tls_key.is_some()
    }

    /// Validate guest mTLS configuration consistency.
    ///
    /// Returns `Ok(())` when either all three TLS paths are set (full
    /// mTLS) or none are set (plaintext). Returns an error naming the
    /// missing fields when only a subset is provided — mirrors the
    /// Podman driver's `validate_tls_config` (`crates/
    /// openshell-driver-podman/src/config.rs`) exactly: this prevents
    /// silently falling back to plaintext when an operator partially
    /// configures mTLS, which would be a confusing, security-relevant
    /// surprise to discover only once a sandbox's supervisor fails (or
    /// worse, silently succeeds unauthenticated) at its first callback.
    pub fn validate_tls_config(&self) -> Result<(), crate::client::LxdApiError> {
        let has_ca = self.guest_tls_ca.is_some();
        let has_cert = self.guest_tls_cert.is_some();
        let has_key = self.guest_tls_key.is_some();

        if (has_ca && has_cert && has_key) || (!has_ca && !has_cert && !has_key) {
            return Ok(());
        }

        let mut missing = Vec::new();
        if !has_ca {
            missing.push("--lxd-tls-ca / OPENSHELL_LXD_TLS_CA");
        }
        if !has_cert {
            missing.push("--lxd-tls-cert / OPENSHELL_LXD_TLS_CERT");
        }
        if !has_key {
            missing.push("--lxd-tls-key / OPENSHELL_LXD_TLS_KEY");
        }

        Err(crate::client::LxdApiError::InvalidInput(format!(
            "Partial TLS configuration: all three TLS paths must be provided together. \
             Missing: {}",
            missing.join(", ")
        )))
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

    fn tls_paths() -> (PathBuf, PathBuf, PathBuf) {
        (
            PathBuf::from("/etc/openshell/ca.pem"),
            PathBuf::from("/etc/openshell/cert.pem"),
            PathBuf::from("/etc/openshell/key.pem"),
        )
    }

    #[test]
    fn tls_enabled_is_false_by_default() {
        assert!(!LxdComputeConfig::default().tls_enabled());
    }

    #[test]
    fn tls_enabled_requires_all_three_paths() {
        let (ca, cert, key) = tls_paths();
        let cfg = LxdComputeConfig {
            guest_tls_ca: Some(ca),
            guest_tls_cert: Some(cert),
            guest_tls_key: Some(key),
            ..LxdComputeConfig::default()
        };
        assert!(cfg.tls_enabled());
    }

    #[test]
    fn validate_tls_config_accepts_none_configured() {
        assert!(LxdComputeConfig::default().validate_tls_config().is_ok());
    }

    #[test]
    fn validate_tls_config_accepts_all_three_configured() {
        let (ca, cert, key) = tls_paths();
        let cfg = LxdComputeConfig {
            guest_tls_ca: Some(ca),
            guest_tls_cert: Some(cert),
            guest_tls_key: Some(key),
            ..LxdComputeConfig::default()
        };
        assert!(cfg.validate_tls_config().is_ok());
    }

    #[test]
    fn validate_tls_config_rejects_ca_only() {
        let (ca, _, _) = tls_paths();
        let cfg = LxdComputeConfig {
            guest_tls_ca: Some(ca),
            ..LxdComputeConfig::default()
        };
        let err = cfg.validate_tls_config().unwrap_err().to_string();
        assert!(err.contains("--lxd-tls-cert"));
        assert!(err.contains("--lxd-tls-key"));
        assert!(!err.contains("--lxd-tls-ca"));
    }

    #[test]
    fn validate_tls_config_rejects_cert_and_key_without_ca() {
        let (_, cert, key) = tls_paths();
        let cfg = LxdComputeConfig {
            guest_tls_cert: Some(cert),
            guest_tls_key: Some(key),
            ..LxdComputeConfig::default()
        };
        let err = cfg.validate_tls_config().unwrap_err().to_string();
        assert!(err.contains("--lxd-tls-ca"));
        assert!(!err.contains("--lxd-tls-cert"));
        assert!(!err.contains("--lxd-tls-key"));
    }

    #[test]
    fn validate_tls_config_rejects_key_only() {
        let (_, _, key) = tls_paths();
        let cfg = LxdComputeConfig {
            guest_tls_key: Some(key),
            ..LxdComputeConfig::default()
        };
        assert!(cfg.validate_tls_config().is_err());
    }

    #[test]
    fn default_config_uses_the_shared_pids_limit_default() {
        assert_eq!(
            LxdComputeConfig::default().sandbox_pids_limit,
            DEFAULT_SANDBOX_PIDS_LIMIT
        );
    }

    #[test]
    fn validate_rejects_negative_pids_limit() {
        let cfg = LxdComputeConfig {
            supervisor_bin: PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            sandbox_pids_limit: -1,
            ..LxdComputeConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("sandbox_pids_limit"));
    }

    #[test]
    fn validate_accepts_zero_pids_limit_as_inherit() {
        let cfg = LxdComputeConfig {
            supervisor_bin: PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            sandbox_pids_limit: 0,
            ..LxdComputeConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_partial_tls_even_with_supervisor_bin_set() {
        let (ca, _, _) = tls_paths();
        let cfg = LxdComputeConfig {
            supervisor_bin: PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            guest_tls_ca: Some(ca),
            ..LxdComputeConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("Partial TLS configuration"));
    }
}
