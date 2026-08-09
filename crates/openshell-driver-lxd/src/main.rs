// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use miette::{IntoDiagnostic, Result};
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[cfg(unix)]
use futures::Stream;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::pin::Pin;
#[cfg(unix)]
use std::task::{Context, Poll};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

use openshell_core::VERSION;
use openshell_core::proto::compute::v1::compute_driver_server::ComputeDriverServer;
use openshell_driver_lxd::config::{
    DEFAULT_LXD_SOCKET_PATH, DEFAULT_NETWORK_IPV4_SUBNET, DEFAULT_NETWORK_NAME,
    DEFAULT_SANDBOX_PIDS_LIMIT, DEFAULT_STORAGE_POOL,
};
use openshell_driver_lxd::{ComputeDriverService, LxdComputeConfig, LxdComputeDriver};

/// Standalone LXD compute driver — Phase 1 proof of concept.
///
/// Run this alongside a gateway and point the gateway at it via
/// `--drivers lxd --compute-driver-socket <path to a UDS you bind here>`,
/// or `compute_drivers = ["lxd"]` + `[openshell.drivers.lxd].socket_path`
/// in the gateway's TOML config. This binary is operator-run — the gateway
/// does not spawn, supervise, or remove it (the "unmanaged extension
/// driver" pattern; see `architecture/compute-runtimes.md`'s "Extension"
/// row and `docs/03-design-rfc.md`).
#[derive(Parser)]
#[command(name = "openshell-driver-lxd")]
#[command(version = VERSION)]
struct Args {
    /// Address this driver's own gRPC service listens on.
    ///
    /// Point the gateway at this address (or a UDS — TCP is for local
    /// development convenience; production use should prefer a UDS the
    /// gateway dials via `--compute-driver-socket`, matching every other
    /// driver's trust model).
    #[arg(
        long,
        env = "OPENSHELL_COMPUTE_DRIVER_BIND",
        default_value = "127.0.0.1:50062"
    )]
    bind_address: SocketAddr,

    /// Unix domain socket path to serve the gRPC service on, instead of the
    /// TCP `--bind-address` above.
    ///
    /// This is what actually matters for wiring up to a real gateway:
    /// `connect_remote_compute_driver`
    /// (`crates/openshell-server/src/compute/mod.rs`) — the code behind
    /// `--compute-driver-socket`/`[openshell.drivers.<name>].socket_path` —
    /// only ever dials a UDS via `UnixStream::connect`, never TCP. Without
    /// this flag, `--bind-address`'s TCP listener is real but unreachable
    /// by any gateway; it only remains useful for ad hoc local debugging
    /// (e.g. `grpcurl`). When this is set, it takes priority and
    /// `--bind-address` is not bound at all.
    #[arg(long, env = "OPENSHELL_COMPUTE_DRIVER_UDS")]
    bind_uds: Option<PathBuf>,

    #[arg(long, env = "OPENSHELL_LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Path to the LXD API Unix socket.
    #[arg(long, env = "OPENSHELL_LXD_SOCKET", default_value = DEFAULT_LXD_SOCKET_PATH)]
    lxd_socket: PathBuf,

    /// Pinned LXD image alias or fingerprint used as a fallback for any
    /// sandbox that doesn't specify its own image.
    ///
    /// Optional as of Phase 2: a sandbox that brings its own OCI
    /// reference (the CLI's `--from`/BYOC flag) resolves it via the
    /// OCI-to-LXD conversion pipeline (`crate::image`) and never touches
    /// this value. Leave unset to run a driver that only ever serves
    /// sandbox-supplied images. See the crate README.
    #[arg(long, env = "OPENSHELL_LXD_IMAGE", default_value = "")]
    lxd_image: String,

    /// Host path to the `openshell-sandbox` supervisor binary.
    #[arg(long, env = "OPENSHELL_LXD_SUPERVISOR_BIN")]
    supervisor_bin: PathBuf,

    /// Managed LXD bridge network name.
    #[arg(long, env = "OPENSHELL_LXD_NETWORK_NAME", default_value = DEFAULT_NETWORK_NAME)]
    network_name: String,

    /// Explicit IPv4 subnet (CIDR) applied when `network_name` needs to be
    /// created. Ignored if it already exists. See
    /// `LxdClient::ensure_network`'s doc comment for why this isn't left
    /// to LXD's own subnet auto-picker.
    #[arg(
        long,
        env = "OPENSHELL_LXD_NETWORK_IPV4_SUBNET",
        default_value = DEFAULT_NETWORK_IPV4_SUBNET
    )]
    network_ipv4_subnet: String,

    /// LXD storage pool for sandbox instances.
    #[arg(long, env = "OPENSHELL_LXD_STORAGE_POOL", default_value = DEFAULT_STORAGE_POOL)]
    storage_pool: String,

    #[arg(long, env = "OPENSHELL_GRPC_ENDPOINT")]
    grpc_endpoint: Option<String>,

    /// Port the gateway server is listening on.
    #[arg(
        long,
        env = "OPENSHELL_GATEWAY_PORT",
        default_value_t = openshell_core::config::DEFAULT_SERVER_PORT
    )]
    gateway_port: u16,

    #[arg(
        long,
        env = "OPENSHELL_SANDBOX_SSH_SOCKET_PATH",
        default_value = openshell_core::container_paths::SSH_SOCKET_PATH
    )]
    sandbox_ssh_socket_path: String,

    /// Host path to the CA certificate for sandbox guest mTLS.
    ///
    /// All three of `--lxd-tls-ca`/`--lxd-tls-cert`/`--lxd-tls-key` must be
    /// set together, or none at all — see `LxdComputeConfig::
    /// validate_tls_config`.
    #[arg(long, env = "OPENSHELL_LXD_TLS_CA")]
    lxd_tls_ca: Option<PathBuf>,

    /// Host path to the client certificate for sandbox guest mTLS.
    #[arg(long, env = "OPENSHELL_LXD_TLS_CERT")]
    lxd_tls_cert: Option<PathBuf>,

    /// Host path to the client private key for sandbox guest mTLS.
    #[arg(long, env = "OPENSHELL_LXD_TLS_KEY")]
    lxd_tls_key: Option<PathBuf>,

    /// Max concurrent processes/threads allowed inside a sandbox instance.
    /// `0` inherits LXD's own default (unlimited).
    #[arg(
        long,
        env = "OPENSHELL_LXD_PIDS_LIMIT",
        default_value_t = DEFAULT_SANDBOX_PIDS_LIMIT
    )]
    lxd_pids_limit: i64,

    /// Allow sandboxes to request host-path bind mounts via
    /// `driver_config.mounts`. An operator-trust decision, off by
    /// default.
    #[arg(long, env = "OPENSHELL_LXD_ENABLE_BIND_MOUNTS")]
    lxd_enable_bind_mounts: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level)),
        )
        .init();

    let driver = LxdComputeDriver::new(LxdComputeConfig {
        socket_path: args.lxd_socket,
        default_image: args.lxd_image,
        network_name: args.network_name,
        network_ipv4_subnet: args.network_ipv4_subnet,
        storage_pool: args.storage_pool,
        supervisor_bin: args.supervisor_bin,
        grpc_endpoint: args.grpc_endpoint.unwrap_or_default(),
        gateway_port: args.gateway_port,
        sandbox_ssh_socket_path: args.sandbox_ssh_socket_path,
        guest_tls_ca: args.lxd_tls_ca,
        guest_tls_cert: args.lxd_tls_cert,
        guest_tls_key: args.lxd_tls_key,
        sandbox_pids_limit: args.lxd_pids_limit,
        enable_bind_mounts: args.lxd_enable_bind_mounts,
        ..LxdComputeConfig::default()
    })
    .await
    .into_diagnostic()?;

    let service = ComputeDriverServer::new(ComputeDriverService::new(driver));

    if let Some(uds_path) = args.bind_uds {
        #[cfg(unix)]
        {
            return serve_uds(service, uds_path).await;
        }
        #[cfg(not(unix))]
        {
            let _ = uds_path;
            return Err(miette::miette!("--bind-uds requires a Unix platform"));
        }
    }

    info!(address = %args.bind_address, "Starting LXD compute driver (Phase 1 PoC) on TCP (unreachable by a real gateway -- see --bind-uds)");
    tonic::transport::Server::builder()
        .add_service(service)
        .serve_with_shutdown(args.bind_address, async {
            tokio::signal::ctrl_c().await.ok();
            info!("Received shutdown signal, draining in-flight requests");
        })
        .await
        .into_diagnostic()
}

/// Serve the `ComputeDriver` service on a Unix domain socket — the only
/// transport `connect_remote_compute_driver` actually dials. See
/// `--bind-uds`'s doc comment on [`Args`] for why this exists.
#[cfg(unix)]
async fn serve_uds(
    service: ComputeDriverServer<ComputeDriverService>,
    socket_path: PathBuf,
) -> Result<()> {
    // Remove a stale socket left behind by a previous crashed run --
    // `UnixListener::bind` fails with `AddrInUse` on an existing path even
    // when nothing is actually listening on it anymore.
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).map_err(|e| {
            miette::miette!(
                "failed to remove stale socket at {}: {e}",
                socket_path.display()
            )
        })?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            miette::miette!(
                "failed to create parent directory for {}: {e}",
                socket_path.display()
            )
        })?;
    }
    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| miette::miette!("failed to bind UDS at {}: {e}", socket_path.display()))?;

    info!(socket = %socket_path.display(), "Starting LXD compute driver (Phase 1 PoC) on a Unix socket");
    tonic::transport::Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(UnixIncoming { listener }, async {
            tokio::signal::ctrl_c().await.ok();
            info!("Received shutdown signal, draining in-flight requests");
        })
        .await
        .into_diagnostic()
}

/// Adapts a [`UnixListener`] into the `Stream` tonic's `serve_with_incoming*`
/// expects. Mirrors `crates/openshell-server/src/test_support.rs`'s
/// identically-named private helper (no shared crate exists for this small
/// a shim; not worth a dependency edge for ~10 lines).
#[cfg(unix)]
struct UnixIncoming {
    listener: UnixListener,
}

#[cfg(unix)]
impl Stream for UnixIncoming {
    type Item = io::Result<UnixStream>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut().listener.poll_accept(cx) {
            Poll::Ready(Ok((stream, _addr))) => Poll::Ready(Some(Ok(stream))),
            Poll::Ready(Err(err)) => Poll::Ready(Some(Err(err))),
            Poll::Pending => Poll::Pending,
        }
    }
}
