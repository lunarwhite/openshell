// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration construction for built-in compute drivers.

use super::{
    DriverStartupContext, GuestTlsPaths, driver_config_from_context, driver_config_from_file,
};
use crate::compute::LxdComputeConfig;
use crate::compute::VmComputeConfig;
use crate::config_file;
use openshell_core::{ComputeDriverKind, Error, Result};
use openshell_driver_docker::DockerComputeConfig;
use openshell_driver_kubernetes::KubernetesComputeConfig;
use openshell_driver_podman::PodmanComputeConfig;
use std::path::PathBuf;

/// Build the selected Kubernetes config from TOML plus runtime defaults.
pub fn kubernetes_config_from_context(
    context: DriverStartupContext<'_>,
) -> Result<KubernetesComputeConfig> {
    let mut cfg = driver_config_from_context(context, ComputeDriverKind::Kubernetes.as_str())?;
    apply_kubernetes_runtime_defaults(&mut cfg);
    Ok(cfg)
}

pub fn kubernetes_config_for_k8s_sa_bootstrap(
    file: Option<&config_file::ConfigFile>,
) -> Result<KubernetesComputeConfig> {
    let Some(file) = file else {
        return Err(Error::config(
            "K8s ServiceAccount bootstrap requires [openshell.drivers.kubernetes] when sandbox JWT issuing is enabled in-cluster",
        ));
    };
    if !file.openshell.drivers.contains_key("kubernetes") {
        return Err(Error::config(
            "K8s ServiceAccount bootstrap requires [openshell.drivers.kubernetes] when sandbox JWT issuing is enabled in-cluster",
        ));
    }
    driver_config_from_file(Some(file), ComputeDriverKind::Kubernetes.as_str())
}

/// Build the selected Podman config from TOML plus runtime defaults.
pub fn podman_config_from_context(
    context: DriverStartupContext<'_>,
) -> Result<PodmanComputeConfig> {
    let mut podman = driver_config_from_context(context, ComputeDriverKind::Podman.as_str())?;
    apply_podman_runtime_defaults(&mut podman, context);
    Ok(podman)
}

/// Build the selected Docker config from TOML plus runtime defaults.
pub fn docker_config_from_context(
    context: DriverStartupContext<'_>,
) -> Result<DockerComputeConfig> {
    let mut cfg = driver_config_from_context(context, ComputeDriverKind::Docker.as_str())?;
    apply_docker_runtime_defaults(&mut cfg, context);
    Ok(cfg)
}

/// Build the selected VM config from TOML plus runtime defaults.
pub fn vm_config_from_context(context: DriverStartupContext<'_>) -> Result<VmComputeConfig> {
    let mut cfg = driver_config_from_context(context, ComputeDriverKind::Vm.as_str())?;
    apply_vm_runtime_defaults(&mut cfg, context);
    Ok(cfg)
}

/// Build the selected LXD config from TOML plus runtime defaults.
pub fn lxd_config_from_context(context: DriverStartupContext<'_>) -> Result<LxdComputeConfig> {
    let mut cfg = driver_config_from_context(context, ComputeDriverKind::Lxd.as_str())?;
    apply_lxd_runtime_defaults(&mut cfg, context);
    Ok(cfg)
}

fn apply_kubernetes_runtime_defaults(k8s: &mut KubernetesComputeConfig) {
    if let Ok(size) = std::env::var("OPENSHELL_K8S_WORKSPACE_DEFAULT_STORAGE_SIZE") {
        k8s.workspace_default_storage_size = size;
    }
    if let Ok(storage_class) = std::env::var("OPENSHELL_K8S_WORKSPACE_STORAGE_CLASS") {
        k8s.workspace_storage_class = storage_class;
    }
}

fn apply_podman_runtime_defaults(
    podman: &mut PodmanComputeConfig,
    context: DriverStartupContext<'_>,
) {
    podman.gateway_port = context.gateway_port;
    apply_podman_env_overrides(podman);
    apply_guest_tls_defaults_to_split_fields(
        &mut podman.guest_tls_ca,
        &mut podman.guest_tls_cert,
        &mut podman.guest_tls_key,
        context.guest_tls,
    );
}

fn apply_docker_runtime_defaults(cfg: &mut DockerComputeConfig, context: DriverStartupContext<'_>) {
    apply_guest_tls_defaults_to_split_fields(
        &mut cfg.guest_tls_ca,
        &mut cfg.guest_tls_cert,
        &mut cfg.guest_tls_key,
        context.guest_tls,
    );
}

fn apply_vm_runtime_defaults(cfg: &mut VmComputeConfig, context: DriverStartupContext<'_>) {
    if cfg.state_dir.as_os_str().is_empty() {
        cfg.state_dir = VmComputeConfig::default_state_dir();
    }
    if cfg.grpc_endpoint.trim().is_empty()
        && (!context.gateway_tls_enabled || context.guest_tls.is_some())
    {
        let scheme = if context.gateway_tls_enabled {
            "https"
        } else {
            "http"
        };
        cfg.grpc_endpoint = format!("{scheme}://127.0.0.1:{}", context.gateway_port);
    }

    apply_guest_tls_defaults_to_split_fields(
        &mut cfg.guest_tls_ca,
        &mut cfg.guest_tls_cert,
        &mut cfg.guest_tls_key,
        context.guest_tls,
    );
}

/// LXD has no guest-mTLS callback support yet (Phase 2 Step 5, not yet
/// built), so this doesn't call `apply_guest_tls_defaults_to_split_fields`
/// the way `apply_vm_runtime_defaults`/`apply_podman_runtime_defaults` do
/// — add that once `LxdComputeConfig` actually has `guest_tls_*` fields
/// to fill in.
fn apply_lxd_runtime_defaults(cfg: &mut LxdComputeConfig, context: DriverStartupContext<'_>) {
    if cfg.state_dir.as_os_str().is_empty() {
        cfg.state_dir = LxdComputeConfig::default_state_dir();
    }
    if cfg.grpc_endpoint.trim().is_empty() && !context.gateway_tls_enabled {
        // Unlike VM (which also serves guest-mTLS-capable https://
        // endpoints), the LXD driver has no guest TLS material to deliver
        // yet, so an https:// gateway can't be defaulted into here safely
        // -- leave grpc_endpoint empty (spawn() itself rejects that with a
        // clear error) rather than construct a URL the driver has no
        // client certificate to actually dial.
        //
        // Critically, this is *not* "http://127.0.0.1:<port>" the way VM's
        // own default is: an LXD sandbox is a real bridged network
        // namespace, not a shared-loopback VM guest -- 127.0.0.1 from
        // inside the sandbox is the sandbox's *own* loopback, entirely
        // unrelated to the gateway's. This exact bug shipped once already
        // and was only caught by a real run (run-managed-driver.sh):
        // CreateSandbox itself succeeded, but the supervisor inside the
        // container could never reach a gateway address that didn't
        // actually route to it, so it never fetched its policy or
        // reported Ready -- indistinguishable, from the CLI's own
        // wait-for-ready timeout, from a dozen other "never becomes
        // Ready" causes already debugged this same week. Derive the
        // bridge's own gateway-facing address from network_ipv4_subnet
        // instead -- the exact address ensure_network() (client.rs)
        // configures as the bridge's own ipv4.address, and the same value
        // run-stage2.sh/run-stage2-oci.sh's own BRIDGE_GATEWAY_IP
        // ("${BRIDGE_SUBNET%/*}") already computes by hand and passes
        // explicitly, now proven working across many real runs.
        let bridge_gateway_ip = cfg
            .network_ipv4_subnet
            .split('/')
            .next()
            .filter(|addr| !addr.trim().is_empty())
            .unwrap_or("127.0.0.1");
        cfg.grpc_endpoint = format!("http://{bridge_gateway_ip}:{}", context.gateway_port);
    }
}

fn apply_guest_tls_defaults_to_split_fields(
    ca: &mut Option<PathBuf>,
    cert: &mut Option<PathBuf>,
    key: &mut Option<PathBuf>,
    defaults: Option<&GuestTlsPaths>,
) {
    if ca.is_none()
        && cert.is_none()
        && key.is_none()
        && let Some(paths) = defaults
    {
        *ca = Some(paths.ca.clone());
        *cert = Some(paths.cert.clone());
        *key = Some(paths.key.clone());
    }
}

fn apply_podman_env_overrides(podman: &mut PodmanComputeConfig) {
    if let Ok(p) = std::env::var("OPENSHELL_PODMAN_SOCKET") {
        podman.socket_path = Some(PathBuf::from(p));
    }
    if let Ok(ip) = std::env::var("OPENSHELL_PODMAN_HOST_GATEWAY_IP") {
        podman.host_gateway_ip = ip;
    }
    if let Ok(mode) = std::env::var("OPENSHELL_PODMAN_USERNS") {
        podman.userns = Some(mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn test_context(file: Option<&config_file::ConfigFile>) -> DriverStartupContext<'_> {
        static EMPTY_ENDPOINT_OVERRIDES: std::sync::LazyLock<BTreeMap<String, PathBuf>> =
            std::sync::LazyLock::new(BTreeMap::new);
        DriverStartupContext {
            file,
            guest_tls: None,
            gateway_port: openshell_core::config::DEFAULT_SERVER_PORT,
            gateway_tls_enabled: false,
            endpoint_overrides: &EMPTY_ENDPOINT_OVERRIDES,
        }
    }

    #[test]
    fn k8s_sa_bootstrap_rejects_missing_kubernetes_driver_config() {
        let err = kubernetes_config_for_k8s_sa_bootstrap(None).unwrap_err();
        assert!(err.to_string().contains("[openshell.drivers.kubernetes]"));

        let file: config_file::ConfigFile =
            toml::from_str("[openshell.gateway]\n").expect("valid config");
        let err = kubernetes_config_for_k8s_sa_bootstrap(Some(&file)).unwrap_err();
        assert!(err.to_string().contains("[openshell.drivers.kubernetes]"));
    }

    #[test]
    fn k8s_sa_bootstrap_uses_configured_namespace_and_service_account() {
        let file: config_file::ConfigFile = toml::from_str(
            r#"
[openshell.gateway]

[openshell.drivers.kubernetes]
namespace = "sandboxes"
service_account_name = "sandbox-sa"
"#,
        )
        .expect("valid config");

        let cfg = kubernetes_config_for_k8s_sa_bootstrap(Some(&file)).unwrap();
        assert_eq!(cfg.namespace, "sandboxes");
        assert_eq!(cfg.service_account_name, "sandbox-sa");
    }

    #[test]
    fn podman_config_reads_bind_mount_opt_in_from_driver_table() {
        let file: config_file::ConfigFile = toml::from_str(
            r"
[openshell.drivers.podman]
enable_bind_mounts = true
",
        )
        .expect("valid config");

        let cfg = podman_config_from_context(test_context(Some(&file))).expect("podman config");

        assert!(cfg.enable_bind_mounts);
    }

    #[test]
    fn docker_config_reads_bind_mount_opt_in_from_driver_table() {
        let file: config_file::ConfigFile = toml::from_str(
            r"
[openshell.drivers.docker]
enable_bind_mounts = true
",
        )
        .expect("valid config");

        let cfg = docker_config_from_context(test_context(Some(&file))).expect("docker config");

        assert!(cfg.enable_bind_mounts);
    }

    #[test]
    fn docker_config_reads_socket_path_from_driver_table() {
        let file: config_file::ConfigFile = toml::from_str(
            r#"
[openshell.drivers.docker]
socket_path = "/tmp/docker.sock"
"#,
        )
        .expect("valid config");

        let cfg = docker_config_from_context(test_context(Some(&file))).expect("docker config");

        assert_eq!(cfg.socket_path, Some(PathBuf::from("/tmp/docker.sock")));
    }

    #[test]
    fn docker_config_reports_selected_invalid_driver_table() {
        let file: config_file::ConfigFile = toml::from_str(
            r"
[openshell.drivers.docker]
unknown_docker_key = true
",
        )
        .expect("valid config");

        let err = docker_config_from_context(test_context(Some(&file))).unwrap_err();

        assert!(
            err.to_string()
                .contains("invalid [openshell.drivers.docker] table")
        );
    }

    #[test]
    fn vm_config_reports_selected_invalid_driver_table() {
        let file: config_file::ConfigFile = toml::from_str(
            r#"
[openshell.drivers.vm]
mem_mib = "not-a-number"
"#,
        )
        .expect("valid config");

        let err = vm_config_from_context(test_context(Some(&file))).unwrap_err();

        assert!(
            err.to_string()
                .contains("invalid [openshell.drivers.vm] table")
        );
    }

    #[test]
    fn lxd_config_defaults_grpc_endpoint_to_the_bridge_gateway_ip_not_loopback() {
        // Regression test for a real bug: an earlier version of this
        // function defaulted grpc_endpoint to "http://127.0.0.1:<port>",
        // copying VM's own default verbatim. That's correct for VM (a
        // guest sharing loopback via libkrun's own port-forwarding) but
        // wrong for LXD: a sandbox is a real bridged network namespace,
        // and 127.0.0.1 from inside it is the sandbox's *own* loopback,
        // not the gateway's -- CreateSandbox itself would succeed (the
        // LXD instance really gets created) while the supervisor inside
        // it could never actually reach the gateway to fetch its policy,
        // so the sandbox would silently never become Ready. Only a real
        // gateway-managed run (run-managed-driver.sh) caught this --
        // nothing about the RPC succeeding or failing surfaces it.
        let file: config_file::ConfigFile = toml::from_str(
            r#"
[openshell.drivers.lxd]
supervisor_bin = "/usr/local/libexec/openshell/openshell-sandbox"
network_ipv4_subnet = "10.88.77.1/24"
"#,
        )
        .expect("valid config");

        let cfg = lxd_config_from_context(test_context(Some(&file))).expect("lxd config");

        assert_eq!(
            cfg.grpc_endpoint,
            format!(
                "http://10.88.77.1:{}",
                openshell_core::config::DEFAULT_SERVER_PORT
            ),
            "grpc_endpoint should default to the bridge's own gateway IP \
             (the address portion of network_ipv4_subnet), not 127.0.0.1"
        );
    }

    #[test]
    fn lxd_config_derives_grpc_endpoint_from_a_custom_subnet() {
        let file: config_file::ConfigFile = toml::from_str(
            r#"
[openshell.drivers.lxd]
network_ipv4_subnet = "192.168.99.1/24"
"#,
        )
        .expect("valid config");

        let cfg = lxd_config_from_context(test_context(Some(&file))).expect("lxd config");

        assert!(
            cfg.grpc_endpoint.starts_with("http://192.168.99.1:"),
            "expected the custom subnet's own address, got: {}",
            cfg.grpc_endpoint
        );
    }

    #[test]
    fn lxd_config_does_not_override_an_explicit_grpc_endpoint() {
        let file: config_file::ConfigFile = toml::from_str(
            r#"
[openshell.drivers.lxd]
network_ipv4_subnet = "10.88.77.1/24"
grpc_endpoint = "http://host.openshell.internal:9999"
"#,
        )
        .expect("valid config");

        let cfg = lxd_config_from_context(test_context(Some(&file))).expect("lxd config");

        assert_eq!(cfg.grpc_endpoint, "http://host.openshell.internal:9999");
    }
}
