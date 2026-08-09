// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! LXD compute driver.
//!
//! LXD container instances on Ubuntu; resource limits, guest mTLS, and
//! driver-config bind mounts are all built (Phase 2, Steps 5-7). See the
//! crate-level doc comment in `lib.rs` and `README.md` for status.

use crate::client::{LxdApiError, LxdClient};
use crate::config::LxdComputeConfig;
use crate::instance::{self, driver_sandbox_status_from_instance, sandbox_name_from_instance};
use crate::watcher::{self, WatchStream};
use openshell_core::ComputeDriverError;
#[cfg(target_os = "linux")]
use openshell_core::proto::compute::v1::gateway_listener_requirement::Selector;
use openshell_core::proto::compute::v1::{
    DriverSandbox, GatewayListenerRequirement, GetCapabilitiesResponse,
};
#[cfg(target_os = "linux")]
use std::net::SocketAddr;
use url::Url;

impl From<LxdApiError> for ComputeDriverError {
    fn from(value: LxdApiError) -> Self {
        match value {
            LxdApiError::Conflict(_) => Self::AlreadyExists,
            LxdApiError::NotFound(msg) => Self::Message(format!("not found: {msg}")),
            LxdApiError::InvalidInput(msg) => Self::InvalidArgument(msg),
            other => Self::Message(other.to_string()),
        }
    }
}

/// Config key this driver sets on every instance it creates, used to find
/// an instance back given only a sandbox ID (the gRPC surface for
/// get/stop/delete only carries the ID, not the full `DriverSandbox` needed
/// to reconstruct an instance name) — mirrors the Podman driver's
/// label-based lookup, not a name-reconstruction approach.
const SANDBOX_ID_CONFIG_KEY: &str = "user.openshell.sandbox_id";

/// The OCI image reference a sandbox requests for itself, if any.
/// Mirrors `openshell-driver-vm/src/driver.rs`'s `requested_sandbox_image`
/// helper exactly (same proto field, same empty-string-means-absent
/// convention) so a sandbox spec means the same thing across drivers.
fn requested_sandbox_image(sandbox: &DriverSandbox) -> Option<&str> {
    sandbox
        .spec
        .as_ref()
        .and_then(|spec| spec.template.as_ref())
        .map(|template| template.image.trim())
        .filter(|image| !image.is_empty())
}

/// Best-effort removal of the entrypoint script and JWT token this driver
/// writes to the host filesystem before `create_instance`, used on any
/// `create_sandbox` failure from that point onward.
///
/// Deliberately leaves `instance::image_staging_dir`'s directory alone --
/// see `create_sandbox`'s own doc comment for why. Both paths share a
/// parent directory (`entrypoint_script_host_path`/
/// `sandbox_token_host_path`'s doc comments), so this removes the two
/// specific files rather than the whole per-sandbox directory, which
/// would otherwise also delete that separately-preserved staging content.
async fn cleanup_sandbox_delivery_files(sandbox_id: &str) {
    if let Ok(path) = instance::entrypoint_script_host_path(sandbox_id)
        && let Err(err) = tokio::fs::remove_file(&path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            sandbox_id,
            path = %path.display(),
            error = %err,
            "Failed to clean up entrypoint script after a failed sandbox create"
        );
    }
    if let Ok(path) = instance::sandbox_token_host_path(sandbox_id)
        && let Err(err) = tokio::fs::remove_file(&path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            sandbox_id,
            path = %path.display(),
            error = %err,
            "Failed to clean up JWT token file after a failed sandbox create"
        );
    }
}

/// LXD compute driver managing sandbox instances via the LXD REST API.
#[derive(Clone)]
pub struct LxdComputeDriver {
    client: LxdClient,
    config: LxdComputeConfig,
}

impl std::fmt::Debug for LxdComputeDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LxdComputeDriver")
            .field("socket_path", &self.config.socket_path)
            .field("default_image", &self.config.default_image)
            .field("network_name", &self.config.network_name)
            .finish()
    }
}

impl LxdComputeDriver {
    /// Connect to the LXD daemon and ensure the managed bridge network
    /// exists.
    pub async fn new(config: LxdComputeConfig) -> Result<Self, LxdApiError> {
        config.validate()?;
        let client = LxdClient::new(config.socket_path.clone());
        client
            .ensure_network(&config.network_name, &config.network_ipv4_subnet)
            .await?;
        Ok(Self { client, config })
    }

    /// Construct a driver instance without contacting a real daemon, for
    /// unit tests that exercise request-building/error-mapping logic
    /// against the in-crate stub server (see `test_utils.rs`).
    #[cfg(test)]
    pub fn for_tests(config: LxdComputeConfig) -> Self {
        let client = LxdClient::new(config.socket_path.clone());
        Self { client, config }
    }

    pub fn capabilities(&self) -> Result<GetCapabilitiesResponse, ComputeDriverError> {
        Ok(openshell_core::driver_utils::build_capabilities_response(
            "lxd",
            openshell_core::VERSION,
            &self.config.default_image,
        ))
    }

    /// Report the gateway exposure needed for the sandbox supervisor's
    /// callback, when the configured endpoint uses a local alias.
    ///
    /// LXD's daemon runs as root with a real host-side bridge — no
    /// pasta/rootless indirection to reason about, so this driver's default
    /// posture is closer to Docker's rootful model than to Podman's
    /// rootless branching (`docs/03-design-rfc.md`, "Proposal" → "Networking").
    /// The driver reads the bridge gateway IP back from LXD's own network
    /// state rather than assuming a fixed DNS alias resolves.
    // Kept `async` to match the gRPC handler signature in `grpc.rs`, which
    // awaits this method. It genuinely awaits `get_network_state` on Linux
    // (LXD's actual target platform); the non-Linux `cfg` arm below is
    // dead weight that only exists so this crate type-checks on a
    // development machine.
    #[cfg_attr(not(target_os = "linux"), allow(clippy::unused_async))]
    pub async fn gateway_listener_requirements(
        &self,
    ) -> Result<Vec<GatewayListenerRequirement>, ComputeDriverError> {
        let endpoint = Url::parse(&self.config.grpc_endpoint).map_err(|err| {
            ComputeDriverError::Precondition(format!(
                "invalid LXD gateway callback endpoint '{}': {err}",
                self.config.grpc_endpoint
            ))
        })?;
        let uses_local_alias = endpoint
            .host_str()
            .is_some_and(|host| matches!(host, "host.openshell.internal" | "_gateway.lxd"));
        if !uses_local_alias {
            return Ok(Vec::new());
        }
        let callback_port = endpoint.port_or_known_default().ok_or_else(|| {
            ComputeDriverError::Precondition(format!(
                "LXD gateway callback endpoint '{}' has no port",
                self.config.grpc_endpoint
            ))
        })?;
        if callback_port != self.config.gateway_port {
            return Err(ComputeDriverError::Precondition(format!(
                "LXD local callback endpoint '{}' uses port {callback_port}, but the gateway \
                 primary listener uses port {}; configure grpc_endpoint with the gateway \
                 primary listener port",
                self.config.grpc_endpoint, self.config.gateway_port
            )));
        }

        #[cfg(target_os = "linux")]
        {
            let network_state = self
                .client
                .get_network_state(&self.config.network_name)
                .await
                .map_err(ComputeDriverError::from)?;
            let gateway_ip = network_state
                .addresses
                .iter()
                .find(|addr| addr.scope == "global" && addr.family == "inet")
                .map(|addr| addr.address.clone())
                .ok_or_else(|| {
                    ComputeDriverError::Precondition(format!(
                        "LXD network '{}' did not report a host bridge gateway address",
                        self.config.network_name
                    ))
                })?;
            let gateway_ip = gateway_ip.split('/').next().unwrap_or(&gateway_ip);
            let gateway_ip = gateway_ip.parse::<std::net::IpAddr>().map_err(|err| {
                ComputeDriverError::Precondition(format!(
                    "LXD bridge gateway address '{gateway_ip}' is invalid: {err}"
                ))
            })?;
            Ok(vec![GatewayListenerRequirement {
                reason: format!(
                    "LXD network '{}' host bridge gateway",
                    self.config.network_name
                ),
                selector: Some(Selector::ExactBindAddress(
                    SocketAddr::new(gateway_ip, callback_port).to_string(),
                )),
            }])
        }
        #[cfg(not(target_os = "linux"))]
        {
            // LXD is Linux-only; this arm only exists so `cargo check`/unit
            // tests run on a non-Linux development machine (this crate is
            // never actually run outside Linux).
            Ok(Vec::new())
        }
    }

    /// Thin well-formedness check, mirroring Podman's `validated_sandbox_create`
    /// pre-check. All four existing drivers implement this RPC; it is not
    /// optional in the generated service trait.
    // Kept `async` to match the gRPC handler signature in `grpc.rs`, which
    // awaits this method, and because Phase 2 will need it to actually
    // check image existence via the client.
    #[allow(clippy::unused_async)]
    pub async fn validate_sandbox_create(
        &self,
        sandbox: &DriverSandbox,
    ) -> Result<(), ComputeDriverError> {
        instance::instance_name(sandbox)?;
        // A sandbox with its own `spec.template.image` resolves that image
        // via the OCI-to-LXD conversion pipeline (`crate::image`,
        // Phase 2) and never touches `default_image` at all -- only
        // reject here when *neither* source of an image is present.
        if requested_sandbox_image(sandbox).is_none() && self.config.default_image.trim().is_empty()
        {
            return Err(ComputeDriverError::Precondition(
                "no pinned LXD image configured and the sandbox specified no image \
                 of its own (set `default_image` or the sandbox's `template.image`)"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Create a sandbox instance and start it, waiting for LXD's async
    /// operations to complete (create, then start) before returning.
    ///
    /// Rollback on failure mirrors the Podman driver's discipline: any
    /// partially-created instance is removed, and any host-side delivery
    /// files already written (entrypoint script, JWT) are cleaned up,
    /// before returning the error. Every LXD API call this function makes
    /// (`create_instance`, `set_instance_state`, `delete_instance`) already
    /// polls its own async operation to completion inside
    /// [`crate::client::LxdClient::send_and_resolve`] before returning --
    /// there is no separate "poll to completion" step to add here.
    ///
    /// Deliberately does *not* remove `instance::image_staging_dir`'s
    /// directory on failure -- that's `crate::image::ensure_lxd_image`'s
    /// own, separately documented "leave in place for diagnosis on
    /// failure" decision, which this function's own rollback must not
    /// silently override.
    pub async fn create_sandbox(&self, sandbox: &DriverSandbox) -> Result<(), ComputeDriverError> {
        let name = instance::instance_name(sandbox)?;

        // Resolve the image to boot: a sandbox-provided OCI reference
        // (`spec.template.image`) goes through the conversion pipeline
        // (Phase 2); otherwise fall back to the driver's pinned
        // `default_image` (Phase 1 behavior, preserved). `image_env` is
        // only non-empty on the OCI path -- see `build_instance_spec`'s
        // doc comment on why insertion order there, not this function,
        // is what makes driver-controlled env vars still win.
        let (image_alias, image_env) = match requested_sandbox_image(sandbox) {
            Some(image_ref) => {
                let staging_dir =
                    instance::image_staging_dir(&sandbox.id).map_err(ComputeDriverError::from)?;
                let converted =
                    crate::image::ensure_lxd_image(&self.client, image_ref, &staging_dir)
                        .await
                        .map_err(|err| {
                            ComputeDriverError::Message(format!(
                                "failed to resolve sandbox image '{image_ref}': {err}"
                            ))
                        })?;
                let _ = tokio::fs::remove_dir_all(&staging_dir).await;
                (converted.alias, converted.config.env)
            }
            None => (self.config.default_image.clone(), Vec::new()),
        };

        // Unconditional, unlike the JWT below -- every sandbox needs its
        // network brought up before the supervisor can do anything, not
        // just the ones with a minted token. See
        // `instance::build_entrypoint_script`'s doc comment for why this
        // exists at all (overriding `lxc.init.cmd` skips the container's
        // normal boot sequence, which is what would otherwise run DHCP).
        let entrypoint_path =
            instance::entrypoint_script_host_path(&sandbox.id).map_err(ComputeDriverError::from)?;
        if let Some(parent) = entrypoint_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ComputeDriverError::Message(format!("failed to create entrypoint directory: {e}"))
            })?;
        }
        let entrypoint_script = instance::build_entrypoint_script(&self.config, &sandbox.id);
        tokio::fs::write(&entrypoint_path, entrypoint_script)
            .await
            .map_err(|e| {
                ComputeDriverError::Message(format!("failed to write entrypoint script: {e}"))
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&entrypoint_path, std::fs::Permissions::from_mode(0o755))
                .await
                .map_err(|e| {
                    ComputeDriverError::Message(format!(
                        "failed to make entrypoint script executable: {e}"
                    ))
                })?;
        }

        if let Some(token) = sandbox
            .spec
            .as_ref()
            .map(|spec| spec.sandbox_token.trim())
            .filter(|token| !token.is_empty())
        {
            let token_path =
                instance::sandbox_token_host_path(&sandbox.id).map_err(ComputeDriverError::from)?;
            if let Some(parent) = token_path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ComputeDriverError::Message(format!("failed to create JWT directory: {e}"))
                })?;
            }
            tokio::fs::write(&token_path, format!("{token}\n"))
                .await
                .map_err(|e| ComputeDriverError::Message(format!("failed to write JWT: {e}")))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    tokio::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o400))
                        .await;
            }
        }

        let spec = match instance::build_instance_spec(
            sandbox,
            &self.config,
            &self.config.grpc_endpoint,
            &image_alias,
            &image_env,
        ) {
            Ok(spec) => spec,
            Err(err) => {
                cleanup_sandbox_delivery_files(&sandbox.id).await;
                return Err(err);
            }
        };

        if let Err(err) = self.client.create_instance(&spec).await {
            // The instance either never got created or LXD itself rolled
            // back an internally-failed create -- either way, nothing
            // this driver created (on LXD's side) survives a failed
            // create_instance to delete. The host-side delivery files
            // are a different story: they were already written before
            // this call and nothing else will ever clean them up.
            cleanup_sandbox_delivery_files(&sandbox.id).await;
            return Err(err.into());
        }

        if let Err(err) = self
            .client
            .set_instance_state(&name, "start", 30, false)
            .await
        {
            // Roll back the just-created instance so a failed start doesn't
            // leave an orphaned, never-started sandbox behind.
            let _ = self.client.delete_instance(&name).await;
            cleanup_sandbox_delivery_files(&sandbox.id).await;
            return Err(err.into());
        }

        Ok(())
    }

    pub async fn stop_sandbox(&self, sandbox_id: &str) -> Result<(), ComputeDriverError> {
        let Some(instance) = self.find_instance_by_sandbox_id(sandbox_id).await? else {
            return Err(ComputeDriverError::Message(format!(
                "sandbox '{sandbox_id}' not found"
            )));
        };
        self.client
            .set_instance_state(&instance.name, "stop", self.config.stop_timeout_secs, false)
            .await
            .map_err(ComputeDriverError::from)
    }

    /// Delete a sandbox instance, idempotent by sandbox ID.
    ///
    /// `DELETE /1.0/instances/<name>` is just as asynchronous as create —
    /// [`LxdClient::delete_instance`] already waits for that operation.
    /// Returns `Ok(false)` (not an error) when nothing was found to delete.
    pub async fn delete_sandbox(&self, sandbox_id: &str) -> Result<bool, ComputeDriverError> {
        let Some(instance) = self.find_instance_by_sandbox_id(sandbox_id).await? else {
            return Ok(false);
        };
        // Best-effort stop before delete; LXD refuses to delete a running
        // instance. Ignore the stop result — delete's own error covers it.
        let _ = self
            .client
            .set_instance_state(&instance.name, "stop", self.config.stop_timeout_secs, true)
            .await;
        let deleted = self
            .client
            .delete_instance(&instance.name)
            .await
            .map_err(ComputeDriverError::from)?;
        Ok(deleted)
    }

    /// Restart-time reconciliation (Phase 2, Step 8) falls out of this
    /// function's — and [`Self::list_sandboxes`]'s — own shape rather
    /// than needing a dedicated reconcile routine: both always re-derive
    /// a sandbox's identity and status from LXD's *current* instance
    /// state, filtered by the `user.openshell.sandbox_id` config key this
    /// driver stamps at create time
    /// ([`SANDBOX_ID_CONFIG_KEY`]/[`instance::build_instance_spec`]),
    /// never from any in-memory operation/pending state this process
    /// might have held. There is no such state to lose on a driver
    /// restart in the first place — every async LXD call this driver
    /// makes already blocks until its own operation resolves (see
    /// [`crate::client::LxdClient::create_instance`]'s doc comment)
    /// before returning, so nothing is ever left "in flight" from this
    /// process's point of view across a restart. The gateway's own
    /// generic reconciler (`openshell_server::compute::ComputeRuntime::
    /// reconcile_store_with_backend`) polls [`Self::list_sandboxes`]
    /// exactly the same way after *its* restart, so a gateway restart is
    /// likewise transparent without this driver doing anything special.
    pub async fn get_sandbox(
        &self,
        sandbox_id: &str,
    ) -> Result<Option<DriverSandbox>, ComputeDriverError> {
        let Some(instance) = self.find_instance_by_sandbox_id(sandbox_id).await? else {
            return Ok(None);
        };
        Ok(Some(DriverSandbox {
            id: sandbox_id.to_string(),
            name: sandbox_name_from_instance(&instance),
            namespace: String::new(),
            spec: None,
            status: Some(driver_sandbox_status_from_instance(&instance, false)),
            workspace: String::new(),
        }))
    }

    pub async fn list_sandboxes(&self) -> Result<Vec<DriverSandbox>, ComputeDriverError> {
        let instances = self
            .client
            .list_instances()
            .await
            .map_err(ComputeDriverError::from)?;
        Ok(instances
            .iter()
            .filter_map(|instance| {
                let sandbox_id = instance.config.get(SANDBOX_ID_CONFIG_KEY)?.clone();
                Some(DriverSandbox {
                    id: sandbox_id,
                    name: sandbox_name_from_instance(instance),
                    namespace: String::new(),
                    spec: None,
                    status: Some(driver_sandbox_status_from_instance(instance, false)),
                    workspace: String::new(),
                })
            })
            .collect())
    }

    pub async fn watch_sandboxes(&self) -> Result<WatchStream, ComputeDriverError> {
        watcher::watch(self.client.clone(), self.config.socket_path.clone())
            .await
            .map_err(ComputeDriverError::from)
    }

    /// Find an instance by the sandbox ID this driver stamped into its
    /// `user.openshell.sandbox_id` config key at create time.
    ///
    /// A label/config-based lookup, not name reconstruction, because the
    /// gRPC surface for get/stop/delete only carries the sandbox ID.
    async fn find_instance_by_sandbox_id(
        &self,
        sandbox_id: &str,
    ) -> Result<Option<crate::client::Instance>, ComputeDriverError> {
        let instances = self
            .client
            .list_instances()
            .await
            .map_err(ComputeDriverError::from)?;
        Ok(instances.into_iter().find(|instance| {
            instance
                .config
                .get(SANDBOX_ID_CONFIG_KEY)
                .map(String::as_str)
                == Some(sandbox_id)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lxd_api_error_conflict_maps_to_already_exists() {
        let err: ComputeDriverError = LxdApiError::Conflict("exists".to_string()).into();
        assert!(matches!(err, ComputeDriverError::AlreadyExists));
    }

    #[test]
    fn lxd_api_error_not_found_maps_to_message() {
        let err: ComputeDriverError = LxdApiError::NotFound("gone".to_string()).into();
        assert!(matches!(err, ComputeDriverError::Message(_)));
    }

    #[tokio::test]
    async fn validate_sandbox_create_rejects_missing_pinned_image() {
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..LxdComputeConfig::default()
        });
        let sandbox = DriverSandbox {
            id: "abc".to_string(),
            name: "demo".to_string(),
            namespace: String::new(),
            spec: None,
            status: None,
            workspace: "default".to_string(),
        };
        let err = driver
            .validate_sandbox_create(&sandbox)
            .await
            .expect_err("missing image should fail validation");
        assert!(matches!(err, ComputeDriverError::Precondition(_)));
    }

    #[tokio::test]
    async fn validate_sandbox_create_accepts_a_sandbox_supplied_image_without_a_pinned_default() {
        use openshell_core::proto::compute::v1::{DriverSandboxSpec, DriverSandboxTemplate};

        // No `default_image` configured at all -- Phase 1's only path.
        // This should still validate because the sandbox brings its own
        // OCI reference, which the conversion pipeline resolves instead
        // (Phase 2).
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..LxdComputeConfig::default()
        });
        let sandbox = DriverSandbox {
            id: "abc".to_string(),
            name: "demo".to_string(),
            namespace: String::new(),
            spec: Some(DriverSandboxSpec {
                template: Some(DriverSandboxTemplate {
                    image: "ghcr.io/example/sandbox:latest".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            status: None,
            workspace: "default".to_string(),
        };
        driver
            .validate_sandbox_create(&sandbox)
            .await
            .expect("sandbox-supplied image should satisfy validation on its own");
    }

    #[test]
    fn requested_sandbox_image_reads_the_template_field() {
        use openshell_core::proto::compute::v1::{DriverSandboxSpec, DriverSandboxTemplate};

        let sandbox = DriverSandbox {
            id: "abc".to_string(),
            name: "demo".to_string(),
            namespace: String::new(),
            spec: Some(DriverSandboxSpec {
                template: Some(DriverSandboxTemplate {
                    image: "  ghcr.io/example/sandbox:latest  ".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            status: None,
            workspace: "default".to_string(),
        };
        assert_eq!(
            requested_sandbox_image(&sandbox),
            Some("ghcr.io/example/sandbox:latest")
        );
    }

    #[tokio::test]
    async fn create_sandbox_cleans_up_delivery_files_when_create_instance_fails() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};
        use openshell_core::proto::compute::v1::DriverSandboxSpec;

        let (socket_path, _request_log, handle) = spawn_lxd_stub(
            "create-sandbox-rollback",
            vec![
                // POST /1.0/instances -> error, nothing ever gets created.
                StubResponse::error(400, "boom"),
            ],
        );
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            socket_path: socket_path.clone(),
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..LxdComputeConfig::default()
        });
        let sandbox_id = format!(
            "test-rollback-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after unix epoch")
                .as_nanos()
        );
        let sandbox = DriverSandbox {
            id: sandbox_id.clone(),
            name: "demo".to_string(),
            namespace: String::new(),
            spec: Some(DriverSandboxSpec {
                sandbox_token: "test-jwt".to_string(),
                ..Default::default()
            }),
            status: None,
            workspace: "default".to_string(),
        };

        driver
            .create_sandbox(&sandbox)
            .await
            .expect_err("create_instance failure should propagate");

        handle.await.expect("stub task should finish");

        let entrypoint_path = instance::entrypoint_script_host_path(&sandbox_id)
            .expect("entrypoint path should resolve");
        let token_path =
            instance::sandbox_token_host_path(&sandbox_id).expect("token path should resolve");
        assert!(
            !entrypoint_path.exists(),
            "entrypoint script should be cleaned up after a failed create_instance: {}",
            entrypoint_path.display()
        );
        assert!(
            !token_path.exists(),
            "JWT token file should be cleaned up after a failed create_instance: {}",
            token_path.display()
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn create_sandbox_cleans_up_delivery_files_when_start_fails() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};
        use openshell_core::proto::compute::v1::DriverSandboxSpec;

        let (socket_path, request_log, handle) = spawn_lxd_stub(
            "create-sandbox-start-rollback",
            vec![
                // POST /1.0/instances -> sync success (instance created).
                StubResponse::sync_success(serde_json::json!({})),
                // PUT /1.0/instances/<name>/state (start) -> error.
                StubResponse::error(400, "start boom"),
                // DELETE /1.0/instances/<name> (rollback) -> sync success.
                StubResponse::sync_success(serde_json::json!({})),
            ],
        );
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            socket_path: socket_path.clone(),
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..LxdComputeConfig::default()
        });
        let sandbox_id = format!(
            "test-start-rollback-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after unix epoch")
                .as_nanos()
        );
        let sandbox = DriverSandbox {
            id: sandbox_id.clone(),
            name: "demo".to_string(),
            namespace: String::new(),
            spec: Some(DriverSandboxSpec {
                sandbox_token: "test-jwt".to_string(),
                ..Default::default()
            }),
            status: None,
            workspace: "default".to_string(),
        };

        driver
            .create_sandbox(&sandbox)
            .await
            .expect_err("start failure should propagate");

        handle.await.expect("stub task should finish");
        let requests = request_log
            .lock()
            .expect("request log lock should not be poisoned")
            .clone();
        assert!(
            requests.iter().any(|r| r.starts_with("DELETE ")),
            "a failed start should roll back the created instance: {requests:?}"
        );

        let entrypoint_path = instance::entrypoint_script_host_path(&sandbox_id)
            .expect("entrypoint path should resolve");
        let token_path =
            instance::sandbox_token_host_path(&sandbox_id).expect("token path should resolve");
        assert!(
            !entrypoint_path.exists(),
            "entrypoint script should be cleaned up after a failed start: {}",
            entrypoint_path.display()
        );
        assert!(
            !token_path.exists(),
            "JWT token file should be cleaned up after a failed start: {}",
            token_path.display()
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    fn labeled_instance_json(sandbox_id: &str, sandbox_name: &str) -> serde_json::Value {
        serde_json::json!({
            "name": format!("openshell-default-{sandbox_id}"),
            "status": "Running",
            "status_code": 103,
            "config": {
                "user.openshell.sandbox_id": sandbox_id,
                "user.openshell.sandbox_name": sandbox_name,
            }
        })
    }

    #[tokio::test]
    async fn get_sandbox_returns_none_when_no_matching_instance_exists() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, _log, handle) = spawn_lxd_stub(
            "get-sandbox-no-match",
            vec![StubResponse::sync_success(serde_json::json!([]))],
        );
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            socket_path: socket_path.clone(),
            ..LxdComputeConfig::default()
        });

        let result = driver
            .get_sandbox("no-such-sandbox")
            .await
            .expect("lookup should not error");
        assert!(result.is_none());

        handle.await.expect("stub task should finish");
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn get_sandbox_returns_the_matching_sandbox_by_label() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, _log, handle) = spawn_lxd_stub(
            "get-sandbox-match",
            vec![StubResponse::sync_success(serde_json::json!([
                labeled_instance_json("abc123", "demo")
            ]))],
        );
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            socket_path: socket_path.clone(),
            ..LxdComputeConfig::default()
        });

        let sandbox = driver
            .get_sandbox("abc123")
            .await
            .expect("lookup should not error")
            .expect("matching sandbox should be found");
        assert_eq!(sandbox.id, "abc123");
        assert_eq!(sandbox.name, "demo");

        handle.await.expect("stub task should finish");
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn list_sandboxes_excludes_instances_without_the_managed_label() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let unrelated_instance = serde_json::json!({
            "name": "some-other-container",
            "status": "Running",
            "status_code": 103,
            "config": {}
        });
        let (socket_path, _log, handle) = spawn_lxd_stub(
            "list-sandboxes-mixed",
            vec![StubResponse::sync_success(serde_json::json!([
                labeled_instance_json("abc123", "demo"),
                unrelated_instance,
            ]))],
        );
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            socket_path: socket_path.clone(),
            ..LxdComputeConfig::default()
        });

        let sandboxes = driver
            .list_sandboxes()
            .await
            .expect("list should not error");
        assert_eq!(
            sandboxes.len(),
            1,
            "an LXD instance with no user.openshell.sandbox_id label must not be reported as a managed sandbox: {sandboxes:?}"
        );
        assert_eq!(sandboxes[0].id, "abc123");

        handle.await.expect("stub task should finish");
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn stop_sandbox_returns_an_error_when_no_matching_instance_exists() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, _log, handle) = spawn_lxd_stub(
            "stop-sandbox-no-match",
            vec![StubResponse::sync_success(serde_json::json!([]))],
        );
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            socket_path: socket_path.clone(),
            ..LxdComputeConfig::default()
        });

        let err = driver
            .stop_sandbox("no-such-sandbox")
            .await
            .expect_err("stopping an unknown sandbox should fail, not silently succeed");
        assert!(err.to_string().contains("not found"));

        handle.await.expect("stub task should finish");
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn stop_sandbox_issues_a_stop_state_change_for_the_matching_instance() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, request_log, handle) = spawn_lxd_stub(
            "stop-sandbox-match",
            vec![
                // GET /1.0/instances?recursion=2 -- find the instance.
                StubResponse::sync_success(serde_json::json!([labeled_instance_json(
                    "abc123", "demo"
                )])),
                // PUT /1.0/instances/<name>/state -- stop.
                StubResponse::sync_success(serde_json::json!({})),
            ],
        );
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            socket_path: socket_path.clone(),
            ..LxdComputeConfig::default()
        });

        driver
            .stop_sandbox("abc123")
            .await
            .expect("stop should succeed for a matching instance");

        handle.await.expect("stub task should finish");
        let requests = request_log
            .lock()
            .expect("request log lock should not be poisoned")
            .clone();
        assert!(
            requests
                .iter()
                .any(|r| r == "PUT /1.0/instances/openshell-default-abc123/state"),
            "expected a state-change PUT for the resolved instance name: {requests:?}"
        );
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn delete_sandbox_returns_false_when_no_matching_instance_exists() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, _log, handle) = spawn_lxd_stub(
            "delete-sandbox-no-match",
            vec![StubResponse::sync_success(serde_json::json!([]))],
        );
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            socket_path: socket_path.clone(),
            ..LxdComputeConfig::default()
        });

        let deleted = driver
            .delete_sandbox("no-such-sandbox")
            .await
            .expect("deleting an already-gone sandbox should not error");
        assert!(!deleted);

        handle.await.expect("stub task should finish");
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn delete_sandbox_propagates_a_genuine_delete_failure_rather_than_swallowing_it() {
        // The "interrupted delete" case the implementation plan calls for:
        // a delete that fails for a real reason (not "already gone") must
        // surface as an error so the gateway's own reconciliation retries
        // it, rather than this driver silently reporting success (or
        // silently reporting "nothing to delete") for a sandbox that is
        // still very much there.
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, _log, handle) = spawn_lxd_stub(
            "delete-sandbox-genuine-failure",
            vec![
                // GET /1.0/instances?recursion=2 -- find the instance.
                StubResponse::sync_success(serde_json::json!([labeled_instance_json(
                    "abc123", "demo"
                )])),
                // PUT .../state (best-effort stop) -- result is intentionally
                // ignored by delete_sandbox, but the request still happens.
                StubResponse::sync_success(serde_json::json!({})),
                // DELETE /1.0/instances/<name> -- a real, non-404 failure.
                StubResponse::error(500, "boom"),
            ],
        );
        let driver = LxdComputeDriver::for_tests(LxdComputeConfig {
            socket_path: socket_path.clone(),
            ..LxdComputeConfig::default()
        });

        driver.delete_sandbox("abc123").await.expect_err(
            "a genuine delete failure must not be reported as success or as 'already gone'",
        );

        handle.await.expect("stub task should finish");
        let _ = std::fs::remove_file(&socket_path);
    }

    #[test]
    fn requested_sandbox_image_is_none_without_a_spec() {
        let sandbox = DriverSandbox {
            id: "abc".to_string(),
            name: "demo".to_string(),
            namespace: String::new(),
            spec: None,
            status: None,
            workspace: "default".to_string(),
        };
        assert_eq!(requested_sandbox_image(&sandbox), None);
    }
}
