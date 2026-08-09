// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! LXD compute driver plumbing.
//!
//! Mirrors `compute::vm`'s shape (see that module's own doc comment) for
//! the same reason: `openshell-driver-lxd` runs as a gateway-managed
//! subprocess speaking the `openshell.compute.v1.ComputeDriver` RPC
//! surface over a Unix domain socket, isolating the newest, least-proven
//! code (the LXD REST client, the OCI-to-LXD image conversion pipeline)
//! from the gateway process itself — not full in-process integration
//! like Docker/Podman. See `.claude/plans/lxd-04-implementation-plan.md`'s
//! Phase 2, Steps 3-4.
//!
//! - [`LxdComputeConfig`]: gateway-local configuration (state dir, driver
//!   binary, supervisor binary, managed network/storage-pool settings).
//! - [`spawn`]: spawn the driver subprocess, wait for its UDS to be
//!   ready, and return a live gRPC channel plus a [`ManagedDriverProcess`]
//!   handle that reaps the subprocess and cleans up the socket on drop.
//!
//! This module deliberately does *not* depend on the `openshell-driver-lxd`
//! crate — the gateway only ever talks to it as a subprocess over gRPC,
//! the same decoupling `compute::vm` maintains from `openshell-driver-vm`.
//! Default values below are independently chosen to match that crate's
//! own defaults (`crates/openshell-driver-lxd/src/config.rs`), not
//! imported from it.
//!
//! **Multi-tenancy model (Phase 2 Step 4, decided and documented here):**
//! a single, dedicated LXD project (currently LXD's own `default`
//! project) holds every OpenShell-managed instance, filtered by the
//! `user.openshell.sandbox_id`/`user.openshell.sandbox_name` labels the
//! driver already stamps on every instance it creates
//! (`crates/openshell-driver-lxd/src/instance.rs`) — the same
//! shared-namespace-plus-managed-label shape the Podman driver already
//! uses, not LXD's own per-tenant project feature. Multiple gateways
//! sharing one LXD daemon is out of scope for Phase 2 (see
//! `crates/openshell-driver-lxd/docs/03-design-rfc.md`'s "Non-goals");
//! there is exactly one tenant — this gateway — so a dedicated LXD
//! *project* (which exists specifically to isolate multiple independent
//! tenants sharing one daemon) would add
//! real complexity (every `lxc`/REST call needing an explicit
//! `?project=` query parameter, a one-time project-creation step, and a
//! second place resource limits/quotas can silently diverge from what
//! the driver's own config says) for a property — tenant isolation —
//! nothing in this phase actually needs yet. If a future phase needs
//! genuine multi-gateway isolation on one LXD host, revisit this
//! decision then rather than pre-building it speculatively now.

use super::AcquiredRemoteDriverEndpoint;
#[cfg(unix)]
use super::ManagedDriverProcess;
#[cfg(unix)]
use super::managed_driver_hardening::{prepare_managed_driver_socket_path, resolve_binary_path};
use crate::config_file::OtlpConfig;
#[cfg(unix)]
use crate::otel_tracing::TraceContextInterceptor;
#[cfg(unix)]
use hyper_util::rt::TokioIo;
#[cfg(unix)]
use openshell_core::proto::compute::v1::{
    GetCapabilitiesRequest, compute_driver_client::ComputeDriverClient,
};
use openshell_core::{ComputeDriverKind, Config, Error, Result};
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::{process::Stdio, sync::Arc, time::Duration};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
use tokio::process::Command;
use tonic::transport::Channel;
#[cfg(unix)]
use tonic::transport::Endpoint;
#[cfg(unix)]
use tower::service_fn;

const DRIVER_BIN_NAME: &str = "openshell-driver-lxd";
const SUPERVISOR_BIN_NAME: &str = "openshell-sandbox";
const COMPUTE_DRIVER_SOCKET_RUN_DIR: &str = "run";
const COMPUTE_DRIVER_SOCKET_NAME: &str = "compute-driver.sock";

/// Matches `openshell-driver-lxd`'s own `DEFAULT_LXD_SOCKET_PATH`
/// (`crates/openshell-driver-lxd/src/config.rs`) — duplicated, not
/// imported; see this module's own doc comment on why.
const DEFAULT_LXD_SOCKET_PATH: &str = "/var/snap/lxd/common/lxd/unix.socket";
const DEFAULT_NETWORK_NAME: &str = "openshell";
const DEFAULT_NETWORK_IPV4_SUBNET: &str = "10.77.99.1/24";
const DEFAULT_STORAGE_POOL: &str = "default";

/// Configuration for launching and talking to the LXD compute driver.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LxdComputeConfig {
    /// Working directory for this driver's gateway-side state (its own
    /// compute-driver socket) — distinct from the LXD daemon's own state,
    /// which the driver subprocess talks to via `lxd_socket_path`.
    pub state_dir: PathBuf,

    /// Directory to search for the `openshell-driver-lxd` binary before
    /// the gateway falls back to its conventional install paths and
    /// sibling binary.
    pub driver_dir: Option<PathBuf>,

    /// Host path to the `openshell-sandbox` supervisor binary, which the
    /// driver delivers into each LXD instance via a read-only disk
    /// device. When unset, resolved the same way as `driver_dir` (search
    /// dirs, then sibling of the gateway's own executable) — but unlike
    /// the driver binary itself, a wrong guess here fails every sandbox
    /// `create`, not just driver startup, so prefer setting this
    /// explicitly in production.
    pub supervisor_bin: Option<PathBuf>,

    /// Path to the LXD API Unix socket (the LXD daemon's own socket).
    pub lxd_socket_path: PathBuf,

    /// Pinned LXD image alias or fingerprint used as a fallback for any
    /// sandbox that doesn't specify its own image. Optional as of Phase
    /// 2 — a sandbox with its own `spec.template.image` resolves through
    /// the driver's OCI-to-LXD conversion pipeline instead and never
    /// touches this value.
    pub default_image: String,

    /// Gateway gRPC endpoint the sandbox supervisor dials back to.
    pub grpc_endpoint: String,

    /// Managed LXD bridge network name. Created if it does not already
    /// exist.
    pub network_name: String,

    /// Explicit IPv4 subnet (CIDR, e.g. `"10.77.99.1/24"`) applied when
    /// `network_name` needs to be created. Ignored if it already exists.
    pub network_ipv4_subnet: String,

    /// LXD storage pool used for sandbox instances' root disks.
    pub storage_pool: String,

    /// Unix socket path inside the sandbox the supervisor's SSH relay
    /// uses.
    pub sandbox_ssh_socket_path: String,

    /// Host-side CA certificate for the guest's mTLS client bundle.
    pub guest_tls_ca: Option<PathBuf>,
    /// Host-side client certificate for the guest's mTLS client bundle.
    pub guest_tls_cert: Option<PathBuf>,
    /// Host-side private key for the guest's mTLS client bundle.
    pub guest_tls_key: Option<PathBuf>,
    /// Max concurrent processes/threads allowed inside a sandbox
    /// instance. `0` inherits the LXD driver's own default.
    pub sandbox_pids_limit: i64,
    /// Whether a sandbox's `driver_config.mounts` may request a
    /// host-path bind mount. Off by default.
    pub enable_bind_mounts: bool,
}

impl LxdComputeConfig {
    /// Default working directory for this driver's gateway-side state.
    #[must_use]
    pub fn default_state_dir() -> PathBuf {
        openshell_core::paths::openshell_state_dir().map_or_else(
            |_| PathBuf::from("target/openshell-lxd-driver"),
            |dir| dir.join("lxd-driver"),
        )
    }
}

impl Default for LxdComputeConfig {
    fn default() -> Self {
        Self {
            state_dir: Self::default_state_dir(),
            driver_dir: None,
            supervisor_bin: None,
            lxd_socket_path: PathBuf::from(DEFAULT_LXD_SOCKET_PATH),
            default_image: openshell_core::image::default_sandbox_image(),
            grpc_endpoint: String::new(),
            network_name: DEFAULT_NETWORK_NAME.to_string(),
            network_ipv4_subnet: DEFAULT_NETWORK_IPV4_SUBNET.to_string(),
            storage_pool: DEFAULT_STORAGE_POOL.to_string(),
            sandbox_ssh_socket_path: openshell_core::container_paths::SSH_SOCKET_PATH.to_string(),
            guest_tls_ca: None,
            guest_tls_cert: None,
            guest_tls_key: None,
            sandbox_pids_limit: openshell_core::config::DEFAULT_SANDBOX_PIDS_LIMIT,
            enable_bind_mounts: false,
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LxdGuestTlsPaths {
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Resolve and validate the guest mTLS material for the LXD driver,
/// mirroring [`super::vm::compute_driver_guest_tls_paths`] exactly: only
/// required (and only checked for "all three or none") when
/// `grpc_endpoint` uses `https://`. A plaintext `grpc_endpoint` makes any
/// configured TLS paths moot, so this returns `Ok(None)` without even
/// looking at them.
#[cfg(unix)]
pub fn compute_driver_guest_tls_paths(
    lxd_config: &LxdComputeConfig,
) -> Result<Option<LxdGuestTlsPaths>> {
    if !lxd_config.grpc_endpoint.starts_with("https://") {
        return Ok(None);
    }

    let provided = [
        lxd_config.guest_tls_ca.as_ref(),
        lxd_config.guest_tls_cert.as_ref(),
        lxd_config.guest_tls_key.as_ref(),
    ];
    if provided.iter().all(Option::is_none) {
        return Err(Error::config(
            "lxd compute driver requires guest_tls_ca, guest_tls_cert, and guest_tls_key when grpc_endpoint uses https://",
        ));
    }

    let Some(ca) = lxd_config.guest_tls_ca.clone() else {
        return Err(Error::config(
            "guest_tls_ca is required when LXD guest TLS materials are configured",
        ));
    };
    let Some(cert) = lxd_config.guest_tls_cert.clone() else {
        return Err(Error::config(
            "guest_tls_cert is required when LXD guest TLS materials are configured",
        ));
    };
    let Some(key) = lxd_config.guest_tls_key.clone() else {
        return Err(Error::config(
            "guest_tls_key is required when LXD guest TLS materials are configured",
        ));
    };

    for path in [&ca, &cert, &key] {
        if !path.is_file() {
            return Err(Error::config(format!(
                "lxd guest TLS material '{}' does not exist or is not a file",
                path.display()
            )));
        }
    }

    Ok(Some(LxdGuestTlsPaths { ca, cert, key }))
}

/// Resolve the `openshell-driver-lxd` binary path. See
/// [`super::managed_driver_hardening::resolve_binary_path`]'s doc comment
/// for the exact search order.
#[cfg(unix)]
pub fn resolve_compute_driver_bin(lxd_config: &LxdComputeConfig) -> Result<PathBuf> {
    resolve_binary_path(
        DRIVER_BIN_NAME,
        lxd_config.driver_dir.as_deref(),
        "install it under [openshell.drivers.lxd].driver_dir, a conventional libexec path such as ~/.local/libexec/openshell, /usr/libexec/openshell, or /usr/local/libexec{,/openshell}, or place it next to the gateway binary",
    )
}

/// Resolve the `openshell-sandbox` supervisor binary path: the explicit
/// `supervisor_bin` override if set, otherwise the same search-dir
/// resolution as [`resolve_compute_driver_bin`].
#[cfg(unix)]
pub fn resolve_supervisor_bin(lxd_config: &LxdComputeConfig) -> Result<PathBuf> {
    if let Some(explicit) = &lxd_config.supervisor_bin {
        if explicit.is_file() {
            return Ok(explicit.clone());
        }
        return Err(Error::config(format!(
            "[openshell.drivers.lxd].supervisor_bin '{}' does not exist or is not a file",
            explicit.display()
        )));
    }
    resolve_binary_path(
        SUPERVISOR_BIN_NAME,
        lxd_config.driver_dir.as_deref(),
        "set [openshell.drivers.lxd].supervisor_bin explicitly, install it under driver_dir, a conventional libexec path such as ~/.local/libexec/openshell, /usr/libexec/openshell, or /usr/local/libexec{,/openshell}, or place it next to the gateway binary",
    )
}

/// Path of the Unix domain socket the driver will listen on.
pub fn compute_driver_socket_path(lxd_config: &LxdComputeConfig) -> PathBuf {
    lxd_config
        .state_dir
        .join(COMPUTE_DRIVER_SOCKET_RUN_DIR)
        .join(COMPUTE_DRIVER_SOCKET_NAME)
}

/// Launch the LXD compute-driver subprocess, wait for its UDS to come up,
/// and return a gRPC `Channel` connected to it plus a process handle that
/// kills the subprocess and removes the socket on drop.
#[cfg(unix)]
pub async fn spawn(
    config: &Config,
    lxd_config: &LxdComputeConfig,
    otlp_config: Option<&OtlpConfig>,
) -> Result<AcquiredRemoteDriverEndpoint> {
    if lxd_config.grpc_endpoint.trim().is_empty() {
        return Err(Error::config(
            "grpc_endpoint is required when using the lxd compute driver",
        ));
    }

    let driver_bin = resolve_compute_driver_bin(lxd_config)?;
    let supervisor_bin = resolve_supervisor_bin(lxd_config)?;
    let socket_path = compute_driver_socket_path(lxd_config);
    let guest_tls_paths = compute_driver_guest_tls_paths(lxd_config)?;
    prepare_managed_driver_socket_path(&lxd_config.state_dir, &socket_path, "lxd")?;

    let mut command = Command::new(&driver_bin);
    command.kill_on_drop(true);
    command.stdin(Stdio::null());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    command.arg("--bind-uds").arg(&socket_path);
    command.arg("--log-level").arg(&config.log_level);
    append_otlp_args(&mut command, otlp_config);
    command
        .arg("--grpc-endpoint")
        .arg(&lxd_config.grpc_endpoint);
    command
        .arg("--gateway-port")
        .arg(config.bind_address.port().to_string());
    command.arg("--lxd-socket").arg(&lxd_config.lxd_socket_path);
    if !lxd_config.default_image.trim().is_empty() {
        command.arg("--lxd-image").arg(&lxd_config.default_image);
    }
    command.arg("--supervisor-bin").arg(&supervisor_bin);
    command.arg("--network-name").arg(&lxd_config.network_name);
    command
        .arg("--network-ipv4-subnet")
        .arg(&lxd_config.network_ipv4_subnet);
    command.arg("--storage-pool").arg(&lxd_config.storage_pool);
    if !lxd_config.sandbox_ssh_socket_path.trim().is_empty() {
        command
            .arg("--sandbox-ssh-socket-path")
            .arg(&lxd_config.sandbox_ssh_socket_path);
    }
    if let Some(tls) = guest_tls_paths {
        command.arg("--lxd-tls-ca").arg(tls.ca);
        command.arg("--lxd-tls-cert").arg(tls.cert);
        command.arg("--lxd-tls-key").arg(tls.key);
    }
    command
        .arg("--lxd-pids-limit")
        .arg(lxd_config.sandbox_pids_limit.to_string());
    if lxd_config.enable_bind_mounts {
        command.arg("--lxd-enable-bind-mounts");
    }

    let mut child = command.spawn().map_err(|e| {
        Error::execution(format!(
            "failed to launch lxd compute driver '{}': {e}",
            driver_bin.display()
        ))
    })?;
    let channel = wait_for_compute_driver(&socket_path, &mut child).await?;
    let process = Arc::new(ManagedDriverProcess::new(child, socket_path));
    Ok(AcquiredRemoteDriverEndpoint::managed_builtin(
        ComputeDriverKind::Lxd,
        channel,
        process,
    ))
}

#[cfg(not(unix))]
pub async fn spawn(
    _config: &Config,
    _lxd_config: &LxdComputeConfig,
    _otlp_config: Option<&OtlpConfig>,
) -> Result<AcquiredRemoteDriverEndpoint> {
    Err(Error::config(
        "the lxd compute driver requires unix domain socket support",
    ))
}

#[cfg(unix)]
fn append_otlp_args(command: &mut Command, otlp_config: Option<&OtlpConfig>) {
    if let Some(config) = otlp_config {
        command.arg("--otlp-endpoint").arg(&config.endpoint);
    }
}

#[cfg(unix)]
#[tracing::instrument(
    name = "driver.wait_for_ready",
    skip_all,
    fields(
        otel.name = "driver.wait_for_ready",
        otel.status_code = tracing::field::Empty,
        driver.name = "lxd",
    )
)]
async fn wait_for_compute_driver(
    socket_path: &Path,
    child: &mut tokio::process::Child,
) -> Result<Channel> {
    let mut last_error: Option<String> = None;
    for _ in 0..100 {
        let try_wait_result = child.try_wait().map_err(|e| {
            Error::execution(format!("failed to poll lxd compute driver process: {e}"))
        })?;
        if let Some(status) = try_wait_result {
            return Err(Error::execution(format!(
                "lxd compute driver exited before becoming ready with status {status}"
            )));
        }

        match connect_compute_driver(socket_path).await {
            Ok(channel) => {
                let mut client =
                    ComputeDriverClient::with_interceptor(channel.clone(), TraceContextInterceptor);
                match client
                    .get_capabilities(tonic::Request::new(GetCapabilitiesRequest {}))
                    .await
                {
                    Ok(_) => return Ok(channel),
                    Err(status) => last_error = Some(status.to_string()),
                }
            }
            Err(err) => last_error = Some(err.to_string()),
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err(Error::execution(format!(
        "timed out waiting for lxd compute driver socket '{}': {}",
        socket_path.display(),
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )))
}

#[cfg(unix)]
async fn connect_compute_driver(socket_path: &Path) -> Result<Channel> {
    let socket_path = socket_path.to_path_buf();
    let display_path = socket_path.clone();
    Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(service_fn(move |_: tonic::transport::Uri| {
            let socket_path = socket_path.clone();
            async move { UnixStream::connect(socket_path).await.map(TokioIo::new) }
        }))
        .await
        .map_err(|e| {
            Error::execution(format!(
                "failed to connect to lxd compute driver socket '{}': {e}",
                display_path.display()
            ))
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        LxdComputeConfig, append_otlp_args, compute_driver_socket_path, resolve_compute_driver_bin,
        resolve_supervisor_bin, wait_for_compute_driver,
    };
    use crate::config_file::OtlpConfig;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn lxd_driver_command_includes_gateway_otlp_endpoint() {
        let mut command = tokio::process::Command::new("openshell-driver-lxd");
        append_otlp_args(
            &mut command,
            Some(&OtlpConfig {
                endpoint: "http://collector.internal:4317".to_string(),
                service_name: Some("custom-gateway".to_string()),
            }),
        );

        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args, ["--otlp-endpoint", "http://collector.internal:4317"]);
    }

    #[tokio::test]
    async fn readiness_probe_propagates_the_active_trace() {
        use crate::otel_tracing::test_exporter;
        use crate::test_support::FakeComputeDriver;

        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("compute-driver.sock");
        let driver = FakeComputeDriver::new();
        let _server = driver.serve_uds(&socket_path).unwrap();
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("read _")
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let traced = test_exporter::install_traced();
        wait_for_compute_driver(&socket_path, &mut child)
            .await
            .unwrap();

        let readiness = traced.spans_named("driver.wait_for_ready");
        assert_eq!(readiness.len(), 1, "one readiness operation should finish");
        test_exporter::assert_is_root(&readiness[0]);
        let trace_id = readiness[0].span_context.trace_id().to_string();
        assert_eq!(
            driver.traceparents().len(),
            1,
            "the readiness capability probe should carry trace context"
        );
        assert!(
            driver.traceparents()[0].contains(&trace_id),
            "the readiness probe should be part of the active trace"
        );
    }

    #[test]
    fn resolve_driver_bin_uses_driver_dir_when_binary_present() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("openshell-driver-lxd");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let lxd_config = LxdComputeConfig {
            driver_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        assert_eq!(resolve_compute_driver_bin(&lxd_config).unwrap(), bin);
    }

    #[test]
    fn resolve_driver_bin_error_mentions_driver_dir_hint() {
        let dir = tempdir().unwrap(); // empty — no driver binary present

        let lxd_config = LxdComputeConfig {
            driver_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let err = resolve_compute_driver_bin(&lxd_config)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[openshell.drivers.lxd].driver_dir"));
        assert!(err.contains("openshell-driver-lxd"));
    }

    #[test]
    fn resolve_supervisor_bin_prefers_explicit_override() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("my-supervisor");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let lxd_config = LxdComputeConfig {
            supervisor_bin: Some(bin.clone()),
            ..Default::default()
        };
        assert_eq!(resolve_supervisor_bin(&lxd_config).unwrap(), bin);
    }

    #[test]
    fn resolve_supervisor_bin_rejects_a_missing_explicit_override() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");

        let lxd_config = LxdComputeConfig {
            supervisor_bin: Some(missing),
            ..Default::default()
        };
        let err = resolve_supervisor_bin(&lxd_config).unwrap_err().to_string();
        assert!(err.contains("supervisor_bin"));
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn resolve_supervisor_bin_falls_back_to_driver_dir_search() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("openshell-sandbox");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let lxd_config = LxdComputeConfig {
            driver_dir: Some(dir.path().to_path_buf()),
            supervisor_bin: None,
            ..Default::default()
        };
        assert_eq!(resolve_supervisor_bin(&lxd_config).unwrap(), bin);
    }

    #[test]
    fn compute_driver_socket_path_uses_private_run_dir() {
        let state_dir = PathBuf::from("/tmp/openshell-lxd-state");
        let lxd_config = LxdComputeConfig {
            state_dir: state_dir.clone(),
            ..Default::default()
        };

        assert_eq!(
            compute_driver_socket_path(&lxd_config),
            state_dir.join("run").join("compute-driver.sock")
        );
    }

    #[test]
    fn default_config_targets_default_network_and_storage_pool() {
        let cfg = LxdComputeConfig::default();
        assert_eq!(cfg.network_name, "openshell");
        assert_eq!(cfg.network_ipv4_subnet, "10.77.99.1/24");
        assert_eq!(cfg.storage_pool, "default");
        assert_eq!(
            cfg.lxd_socket_path,
            PathBuf::from("/var/snap/lxd/common/lxd/unix.socket")
        );
    }
}
