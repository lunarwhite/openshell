// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Thin async HTTP client for the LXD REST API over a Unix socket.
//!
//! No mature async Rust LXD client crate exists (the one community `lxd`
//! crate on crates.io is a stale, unmaintained pre-2018 synchronous
//! library) — this hand-rolls a client the same way the Podman driver does,
//! against `hyper` + `UnixStream`, rather than depending on it.
//!
//! LXD wraps every response in a standard envelope (`type`, `status_code`,
//! `metadata`, and — for asynchronous operations like create/delete/state
//! changes — an `operation` path to poll). This module owns that envelope
//! handling so callers in [`crate::driver`] only ever see the resolved
//! `metadata` payload or a resolved error.

use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tracing::debug;

/// Timeout for individual LXD API calls (excluding operation waits).
const API_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for polling an async operation to completion via
/// `/1.0/operations/<uuid>/wait`.
const OPERATION_WAIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Timeout for the image-upload HTTP call itself (sending the tarball body
/// and receiving the initial envelope) — much longer than [`API_TIMEOUT`]
/// because a real sandbox image is hundreds of MB, not a small JSON body.
const IMAGE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Timeout for polling an image-import operation to completion. LXD does
/// real work here (writing the image to the storage pool) that can
/// meaningfully exceed the fast create/delete/state-change operations
/// [`OPERATION_WAIT_TIMEOUT`] was sized for.
const IMAGE_IMPORT_WAIT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum LxdApiError {
    #[error("LXD instance not found: {0}")]
    NotFound(String),
    #[error("LXD API conflict: {0}")]
    Conflict(String),
    #[error("LXD API error ({status_code}): {message}")]
    Api { status_code: i64, message: String },
    #[error("connection error: {0}")]
    Connection(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("JSON error: {0}")]
    Json(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("LXD operation failed: {0}")]
    OperationFailed(String),
}

/// Maximum instance/resource name length, matching LXD's own limit.
const MAX_NAME_LEN: usize = 63;

/// Validate that a resource name is safe for URL path interpolation and
/// satisfies LXD's own naming rules (RFC 1123 label: alphanumeric and `-`,
/// must not start or end with `-`).
pub fn validate_name(name: &str) -> Result<(), LxdApiError> {
    if name.is_empty() {
        return Err(LxdApiError::InvalidInput(
            "name must not be empty".to_string(),
        ));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(LxdApiError::InvalidInput(format!(
            "name exceeds LXD's maximum length of {MAX_NAME_LEN} characters (got {})",
            name.len()
        )));
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return Err(LxdApiError::InvalidInput(format!(
            "name must start and end with an alphanumeric character: {name:?}"
        )));
    }
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(LxdApiError::InvalidInput(format!(
            "name contains invalid characters (LXD allows only alphanumerics and '-'): {name:?}"
        )));
    }
    Ok(())
}

/// The standard LXD response envelope.
#[derive(Debug, Clone, Deserialize)]
struct LxdEnvelope {
    #[serde(rename = "type")]
    response_type: String,
    #[serde(default)]
    status_code: i64,
    #[serde(default)]
    metadata: Value,
    #[serde(default)]
    operation: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_code: i64,
}

/// A resolved LXD background operation, after polling `/wait` to completion.
#[derive(Debug, Clone, Deserialize)]
pub struct OperationResult {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub status_code: i64,
    #[serde(default)]
    pub err: String,
    #[serde(default)]
    pub metadata: Value,
}

/// Instance status codes LXD reports (subset relevant to sandbox lifecycle).
pub mod status_code {
    pub const STARTING: i64 = 100;
    pub const RUNNING: i64 = 103;
    pub const STOPPED: i64 = 102;
    pub const ERROR: i64 = 400;
}

/// An LXD instance summary/detail, as returned by create/get/list.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Instance {
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub status_code: i64,
    #[serde(default)]
    pub config: HashMap<String, String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
}

/// Managed-network state, used to read the bridge gateway address back for
/// the driver's callback-endpoint construction.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NetworkState {
    #[serde(default)]
    pub addresses: Vec<NetworkAddress>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NetworkAddress {
    pub address: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub scope: String,
}

/// Response shape for `GET /1.0/images/aliases/<name>` — only the
/// `target` fingerprint is needed by callers, but the full shape is kept
/// so a `Debug`/log dump isn't lossy.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageAliasInfo {
    pub name: String,
    pub target: String,
}

/// Async LXD REST API client communicating over a Unix socket.
#[derive(Debug, Clone)]
pub struct LxdClient {
    socket_path: PathBuf,
}

impl LxdClient {
    #[must_use]
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    async fn connect(
        &self,
    ) -> Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>, LxdApiError> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| LxdApiError::Connection(format!("{}: {e}", self.socket_path.display())))?;

        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| LxdApiError::Connection(e.to_string()))?;

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!(error = %e, "LXD API connection closed");
            }
        });

        Ok(sender)
    }

    fn build_request(method: hyper::Method, path: &str, body: Full<Bytes>) -> Request<Full<Bytes>> {
        Request::builder()
            .method(method)
            .uri(format!("http://localhost{path}"))
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .body(body)
            .expect("valid request")
    }

    async fn send_request(
        &self,
        req: Request<Full<Bytes>>,
        timeout: Duration,
    ) -> Result<Bytes, LxdApiError> {
        let mut sender = self.connect().await?;
        let response = tokio::time::timeout(timeout, sender.send_request(req))
            .await
            .map_err(|_| LxdApiError::Timeout(timeout))?
            .map_err(|e| LxdApiError::Connection(e.to_string()))?;
        tokio::time::timeout(timeout, response.into_body().collect())
            .await
            .map_err(|_| LxdApiError::Timeout(timeout))?
            .map_err(|e| LxdApiError::Connection(e.to_string()))
            .map(http_body_util::Collected::to_bytes)
    }

    /// Perform a request and resolve the LXD envelope, waiting on the
    /// resulting operation if the response is asynchronous.
    async fn request_resolved(
        &self,
        method: hyper::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, LxdApiError> {
        let full_body = match body {
            Some(json) => Full::new(Bytes::from(
                serde_json::to_vec(json).map_err(|e| LxdApiError::Json(e.to_string()))?,
            )),
            None => Full::new(Bytes::new()),
        };
        let req = Self::build_request(method, path, full_body);
        self.send_and_resolve(req, API_TIMEOUT, OPERATION_WAIT_TIMEOUT)
            .await
    }

    /// Send an already-built request and resolve the LXD envelope, waiting
    /// on the resulting operation if the response is asynchronous.
    ///
    /// Split out from [`Self::request_resolved`] so callers that need a
    /// non-JSON body (image upload's raw tarball, sent with
    /// `Content-Type: application/octet-stream`, not
    /// `Self::build_request`'s hardcoded JSON content type) can still reuse
    /// the same envelope/operation-resolution logic instead of duplicating
    /// it. Takes explicit timeouts because image import legitimately needs
    /// much longer than the fast create/delete/state-change calls
    /// [`API_TIMEOUT`]/[`OPERATION_WAIT_TIMEOUT`] are sized for.
    async fn send_and_resolve(
        &self,
        req: Request<Full<Bytes>>,
        send_timeout: Duration,
        operation_wait_timeout: Duration,
    ) -> Result<Value, LxdApiError> {
        let bytes = self.send_request(req, send_timeout).await?;
        let envelope: LxdEnvelope = serde_json::from_slice(&bytes)
            .map_err(|e| LxdApiError::Json(format!("{e}: {}", String::from_utf8_lossy(&bytes))))?;

        if envelope.response_type == "error" {
            return Err(error_from_envelope(&envelope));
        }

        if envelope.response_type == "async" {
            if envelope.operation.is_empty() {
                return Err(LxdApiError::Api {
                    status_code: envelope.status_code,
                    message: "async response missing operation path".to_string(),
                });
            }
            let result = self
                .wait_for_operation(&envelope.operation, operation_wait_timeout)
                .await?;
            if !result.err.is_empty() {
                return Err(LxdApiError::OperationFailed(result.err));
            }
            return Ok(result.metadata);
        }

        Ok(envelope.metadata)
    }

    /// Poll `/1.0/operations/<uuid>/wait` until the operation resolves.
    ///
    /// LXD's own timeout query param bounds a single call; loop in case the
    /// daemon returns before the operation actually finishes (defensive —
    /// observed behavior varies across LXD versions).
    async fn wait_for_operation(
        &self,
        operation_path: &str,
        timeout: Duration,
    ) -> Result<OperationResult, LxdApiError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let wait_path = format!("{operation_path}/wait?timeout=30");
            let req = Self::build_request(hyper::Method::GET, &wait_path, Full::new(Bytes::new()));
            let bytes = self.send_request(req, Duration::from_secs(35)).await?;
            let envelope: LxdEnvelope = serde_json::from_slice(&bytes).map_err(|e| {
                LxdApiError::Json(format!("{e}: {}", String::from_utf8_lossy(&bytes)))
            })?;
            if envelope.response_type == "error" {
                return Err(error_from_envelope(&envelope));
            }
            let result: OperationResult = serde_json::from_value(envelope.metadata.clone())
                .map_err(|e| LxdApiError::Json(e.to_string()))?;
            if matches!(result.status.as_str(), "Success" | "Failure" | "Cancelled") {
                return Ok(result);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(LxdApiError::Timeout(timeout));
            }
        }
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: hyper::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, LxdApiError> {
        let metadata = self.request_resolved(method, path, body).await?;
        serde_json::from_value(metadata).map_err(|e| LxdApiError::Json(e.to_string()))
    }

    // ── Instance operations ─────────────────────────────────────────────

    /// Create an instance from a fully-built LXD instance spec (see
    /// [`crate::instance::build_instance_spec`]). Waits for the create
    /// operation to complete before returning.
    pub async fn create_instance(&self, spec: &Value) -> Result<(), LxdApiError> {
        self.request_resolved(hyper::Method::POST, "/1.0/instances", Some(spec))
            .await?;
        Ok(())
    }

    /// Fetch one instance by name. Returns `Ok(None)` on a 404.
    pub async fn get_instance(&self, name: &str) -> Result<Option<Instance>, LxdApiError> {
        validate_name(name)?;
        match self
            .request_json::<Instance>(hyper::Method::GET, &format!("/1.0/instances/{name}"), None)
            .await
        {
            Ok(instance) => Ok(Some(instance)),
            Err(LxdApiError::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// List all instances with full recursion (equivalent state detail to
    /// [`Self::get_instance`] per-entry).
    pub async fn list_instances(&self) -> Result<Vec<Instance>, LxdApiError> {
        self.request_json(hyper::Method::GET, "/1.0/instances?recursion=2", None)
            .await
    }

    /// Start or stop an instance and wait for the state-change operation.
    pub async fn set_instance_state(
        &self,
        name: &str,
        action: &str,
        timeout_secs: u32,
        force: bool,
    ) -> Result<(), LxdApiError> {
        validate_name(name)?;
        let body = serde_json::json!({
            "action": action,
            "timeout": timeout_secs,
            "force": force,
        });
        self.request_resolved(
            hyper::Method::PUT,
            &format!("/1.0/instances/{name}/state"),
            Some(&body),
        )
        .await?;
        Ok(())
    }

    /// Delete an instance. Returns `Ok(false)` (not an error) when the
    /// instance is already gone, matching the Podman driver's idempotent
    /// delete discipline.
    pub async fn delete_instance(&self, name: &str) -> Result<bool, LxdApiError> {
        validate_name(name)?;
        match self
            .request_resolved(
                hyper::Method::DELETE,
                &format!("/1.0/instances/{name}"),
                None,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(LxdApiError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Read a managed network's runtime state (used to resolve the bridge
    /// gateway IP for the driver's callback-endpoint construction, per the
    /// design doc's networking section).
    pub async fn get_network_state(&self, network_name: &str) -> Result<NetworkState, LxdApiError> {
        validate_name(network_name)?;
        self.request_json(
            hyper::Method::GET,
            &format!("/1.0/networks/{network_name}/state"),
            None,
        )
        .await
    }

    /// Ensure the managed bridge network exists, creating it if not.
    ///
    /// `ipv4_subnet` is an explicit CIDR (e.g. `"10.77.99.1/24"`) applied as
    /// the new network's `ipv4.address` when creation is needed. Omitting
    /// `config` entirely (as an earlier version of this function did) lets
    /// LXD's own auto-picker choose a subnet — the same auto-picker that
    /// `lxd init --minimal` uses, empirically confirmed (see
    /// `hack/run-vm-tests.sh`'s `ensure_lxd_initialized`, and
    /// `docs/05-test-plan.md`) to fail with "Failed
    /// automatically finding an unused IPv4 subnet, manual configuration
    /// required" inside nested/VM environments. This path had never been
    /// exercised against a real daemon before Stage 2 — don't reintroduce
    /// the auto-picker here for the same reason it was replaced there.
    pub async fn ensure_network(
        &self,
        network_name: &str,
        ipv4_subnet: &str,
    ) -> Result<(), LxdApiError> {
        validate_name(network_name)?;
        match self
            .request_json::<Value>(
                hyper::Method::GET,
                &format!("/1.0/networks/{network_name}"),
                None,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(LxdApiError::NotFound(_)) => {
                let body = serde_json::json!({
                    "name": network_name,
                    "type": "bridge",
                    "config": {
                        "ipv4.address": ipv4_subnet,
                        "ipv4.nat": "true",
                        "ipv6.address": "none",
                    },
                });
                self.request_resolved(hyper::Method::POST, "/1.0/networks", Some(&body))
                    .await?;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    // ── Image operations (Phase 2: OCI-to-LXD conversion pipeline) ──────

    /// Look up an image by alias. Returns `Ok(None)` on a 404 — an absent
    /// alias is the expected, common case (a cache miss), not an error.
    pub async fn get_image_by_alias(
        &self,
        alias: &str,
    ) -> Result<Option<ImageAliasInfo>, LxdApiError> {
        validate_name(alias)?;
        match self
            .request_json::<ImageAliasInfo>(
                hyper::Method::GET,
                &format!("/1.0/images/aliases/{alias}"),
                None,
            )
            .await
        {
            Ok(info) => Ok(Some(info)),
            Err(LxdApiError::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Upload a single "unified" image tarball (`metadata.yaml` plus a
    /// `rootfs/` directory in one archive — see
    /// [`crate::image::package_unified_image_tar`]) via `POST /1.0/images`.
    /// Returns the resulting image fingerprint. Waits for the image-import
    /// operation to complete before returning, using
    /// [`IMAGE_IMPORT_WAIT_TIMEOUT`] rather than the default
    /// [`OPERATION_WAIT_TIMEOUT`] (real work — decompressing and writing a
    /// potentially-large image into the storage pool — not a fast
    /// create/delete/state-change call).
    ///
    /// Deliberately raw-body, not multipart: LXD's `POST /1.0/images`
    /// accepts a single unified tarball as a plain
    /// `application/octet-stream` body with no additional form parts,
    /// which is exactly the shape [`crate::image`] produces. Multipart
    /// (separate `metadata`/`rootfs` parts) is a different, split-upload
    /// shape this driver doesn't use.
    pub async fn create_image_from_unified_tarball(
        &self,
        tarball: Vec<u8>,
    ) -> Result<String, LxdApiError> {
        let req = Request::builder()
            .method(hyper::Method::POST)
            .uri("http://localhost/1.0/images")
            .header("Host", "localhost")
            .header("Content-Type", "application/octet-stream")
            .body(Full::new(Bytes::from(tarball)))
            .map_err(|e| LxdApiError::InvalidInput(e.to_string()))?;
        let metadata = self
            .send_and_resolve(req, IMAGE_UPLOAD_TIMEOUT, IMAGE_IMPORT_WAIT_TIMEOUT)
            .await?;
        metadata
            .get("fingerprint")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                LxdApiError::Json(format!(
                    "image import response is missing a fingerprint: {metadata}"
                ))
            })
    }

    /// Point an alias at an existing image fingerprint. Used to cache a
    /// converted OCI image by digest (see [`crate::image`]) so a later
    /// `CreateSandbox` for the same image digest can skip straight to
    /// [`Self::get_image_by_alias`] instead of re-converting.
    pub async fn create_image_alias(
        &self,
        alias: &str,
        target_fingerprint: &str,
    ) -> Result<(), LxdApiError> {
        validate_name(alias)?;
        let body = serde_json::json!({
            "name": alias,
            "target": target_fingerprint,
        });
        self.request_resolved(hyper::Method::POST, "/1.0/images/aliases", Some(&body))
            .await?;
        Ok(())
    }
}

fn error_from_envelope(envelope: &LxdEnvelope) -> LxdApiError {
    let message = if envelope.error.is_empty() {
        "unknown LXD API error".to_string()
    } else {
        envelope.error.clone()
    };
    match envelope.error_code {
        404 => LxdApiError::NotFound(message),
        409 => LxdApiError::Conflict(message),
        code => LxdApiError::Api {
            status_code: code,
            message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::StatusCode;

    #[test]
    fn validate_name_accepts_lxd_style_names() {
        assert!(validate_name("openshell-sandbox-abc123").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_leading_hyphen() {
        let err = validate_name("-sandbox").unwrap_err();
        assert!(err.to_string().contains("alphanumeric"));
    }

    #[test]
    fn validate_name_rejects_trailing_hyphen() {
        let err = validate_name("sandbox-").unwrap_err();
        assert!(err.to_string().contains("alphanumeric"));
    }

    #[test]
    fn validate_name_rejects_invalid_characters() {
        let err = validate_name("sandbox_abc").unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn validate_name_rejects_overlong_names() {
        let long_name = "a".repeat(MAX_NAME_LEN + 1);
        let err = validate_name(&long_name).unwrap_err();
        assert!(err.to_string().contains("maximum length"));
    }

    #[test]
    fn error_from_envelope_maps_404_to_not_found() {
        let envelope = LxdEnvelope {
            response_type: "error".to_string(),
            status_code: 404,
            metadata: Value::Null,
            operation: String::new(),
            error: "not found".to_string(),
            error_code: 404,
        };
        assert!(matches!(
            error_from_envelope(&envelope),
            LxdApiError::NotFound(_)
        ));
    }

    #[test]
    fn error_from_envelope_maps_409_to_conflict() {
        let envelope = LxdEnvelope {
            response_type: "error".to_string(),
            status_code: 409,
            metadata: Value::Null,
            operation: String::new(),
            error: "already exists".to_string(),
            error_code: 409,
        };
        assert!(matches!(
            error_from_envelope(&envelope),
            LxdApiError::Conflict(_)
        ));
    }

    // ── Stub-server integration tests ───────────────────────────────────
    //
    // These exercise the real request-sending/envelope-resolution code path
    // against `crate::test_utils`'s in-process Unix-socket stub, not a real
    // LXD daemon. They validate the client's HTTP/envelope handling, not
    // LXD's actual runtime behavior — see `hack/confinement-spike.sh` for
    // what still requires a real daemon.

    #[tokio::test]
    async fn get_instance_returns_none_on_404() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, request_log, handle) = spawn_lxd_stub(
            "get-instance-404",
            vec![StubResponse::error(404, "not found")],
        );
        let client = LxdClient::new(socket_path.clone());

        let result = client
            .get_instance("openshell-default-abc123")
            .await
            .expect("404 should resolve to Ok(None), not an error");
        assert!(result.is_none());

        handle.await.expect("stub task should finish");
        let requests = request_log
            .lock()
            .expect("request log lock should not be poisoned")
            .clone();
        assert_eq!(requests[0], "GET /1.0/instances/openshell-default-abc123");
        let _ = std::fs::remove_file(socket_path);
    }

    #[tokio::test]
    async fn delete_instance_returns_false_on_already_gone() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, _log, handle) = spawn_lxd_stub(
            "delete-instance-404",
            vec![StubResponse::error(404, "not found")],
        );
        let client = LxdClient::new(socket_path.clone());

        let deleted = client
            .delete_instance("openshell-default-abc123")
            .await
            .expect("delete of an already-gone instance should not error");
        assert!(
            !deleted,
            "already-removed instances should report deleted=false"
        );

        handle.await.expect("stub task should finish");
        let _ = std::fs::remove_file(socket_path);
    }

    #[tokio::test]
    async fn list_instances_parses_sync_envelope() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, _log, handle) = spawn_lxd_stub(
            "list-instances-sync",
            vec![StubResponse::sync_success(serde_json::json!([
                {
                    "name": "openshell-default-abc123",
                    "status": "Running",
                    "status_code": 103,
                    "config": {"user.openshell.sandbox_id": "abc123"}
                }
            ]))],
        );
        let client = LxdClient::new(socket_path.clone());

        let instances = client.list_instances().await.expect("list should succeed");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].name, "openshell-default-abc123");
        assert_eq!(instances[0].status_code, status_code::RUNNING);

        handle.await.expect("stub task should finish");
        let _ = std::fs::remove_file(socket_path);
    }

    #[tokio::test]
    async fn create_instance_waits_for_async_operation_success() {
        use crate::test_utils::{StubResponse, spawn_lxd_stub};

        let (socket_path, request_log, handle) = spawn_lxd_stub(
            "create-instance-async",
            vec![
                // POST /1.0/instances -> async, points at an operation
                StubResponse::new(
                    StatusCode::OK,
                    serde_json::json!({
                        "type": "async",
                        "status_code": 100,
                        "operation": "/1.0/operations/test-op-1",
                        "metadata": {},
                    })
                    .to_string(),
                ),
                // GET /1.0/operations/test-op-1/wait -> resolved success
                StubResponse::sync_success(serde_json::json!({
                    "status": "Success",
                    "status_code": 200,
                    "err": "",
                    "metadata": {"name": "openshell-default-abc123"},
                })),
            ],
        );
        let client = LxdClient::new(socket_path.clone());

        client
            .create_instance(&serde_json::json!({"name": "openshell-default-abc123"}))
            .await
            .expect("create should wait for the async operation and succeed");

        handle.await.expect("stub task should finish");
        let requests = request_log
            .lock()
            .expect("request log lock should not be poisoned")
            .clone();
        assert_eq!(requests[0], "POST /1.0/instances");
        assert!(requests[1].starts_with("GET /1.0/operations/test-op-1/wait"));
        let _ = std::fs::remove_file(socket_path);
    }

    // ── Real-daemon integration check ───────────────────────────────────
    //
    // Everything above exercises `LxdClient` against the in-process stub —
    // useful for the HTTP/envelope code path, but it has never proven this
    // client speaks to an actual LXD daemon correctly (see
    // `docs/05-test-plan.md`, Stage 1). This test is the first
    // one to do that. It is `#[ignore]`d because it requires a real local
    // LXD daemon (Linux only) and creates/deletes a real container — never
    // run automatically by a plain `cargo test`.
    //
    // Run explicitly on a Linux host with LXD installed and reachable:
    //   cargo test -p openshell-driver-lxd -- --ignored real_daemon
    //
    // Deliberately bypasses `crate::instance::build_instance_spec` and uses
    // a plain stock image instead of a converted OpenShell sandbox image —
    // this test validates `LxdClient`'s request/envelope handling against
    // real LXD, not the full sandbox lifecycle (which additionally needs
    // the manually pre-converted image the OCI-image gap requires; see
    // `docs/03-design-rfc.md`'s "Sandbox image" row). It never touches
    // `security.nesting`/capabilities either, since a plain, unstarted
    // container needs neither.
    /// Resolve a `lxc`-CLI-style `remote:alias` reference (e.g.
    /// `"ubuntu:22.04"`) to a local image alias, caching it under a fixed
    /// name so repeated test runs don't re-fetch it.
    ///
    /// Shells out to `lxc image copy` rather than hand-rolling the
    /// `ubuntu:` remote's server/protocol as a REST `source` spec — that
    /// would mean hardcoding a simplestreams URL in this crate that could
    /// drift independently of whatever the local `lxc` install already has
    /// configured for that remote. This is test setup only; the real
    /// driver (`instance::build_instance_spec`) always uses a pinned local
    /// alias/fingerprint and never resolves a remote reference itself (see
    /// `LxdComputeConfig::default_image`'s doc comment) — Phase 1 has no
    /// generic OCI/remote-image resolution.
    async fn ensure_local_test_image(remote_image: &str) -> Result<String, String> {
        const LOCAL_ALIAS: &str = "openshell-lxd-client-test-image";

        let alias_list = tokio::process::Command::new("lxc")
            .args(["image", "alias", "list", "--format", "csv"])
            .output()
            .await
            .map_err(|e| format!("failed to run `lxc image alias list`: {e}"))?;
        let already_present = String::from_utf8_lossy(&alias_list.stdout)
            .lines()
            .any(|line| line.split(',').next() == Some(LOCAL_ALIAS));
        if already_present {
            return Ok(LOCAL_ALIAS.to_string());
        }

        let status = tokio::process::Command::new("lxc")
            .args([
                "image",
                "copy",
                remote_image,
                "local:",
                "--alias",
                LOCAL_ALIAS,
            ])
            .status()
            .await
            .map_err(|e| format!("failed to run `lxc image copy {remote_image} local:`: {e}"))?;
        if !status.success() {
            return Err(format!(
                "`lxc image copy {remote_image} local: --alias {LOCAL_ALIAS}` exited with {status}"
            ));
        }
        Ok(LOCAL_ALIAS.to_string())
    }

    #[tokio::test]
    #[ignore = "requires a real local LXD daemon; run with `cargo test -p openshell-driver-lxd -- --ignored`"]
    async fn real_daemon_create_get_list_delete_lifecycle() {
        let socket_path = std::env::var("OPENSHELL_LXD_TEST_SOCKET").map_or_else(
            |_| PathBuf::from(crate::config::DEFAULT_LXD_SOCKET_PATH),
            PathBuf::from,
        );
        assert!(
            socket_path.exists(),
            "LXD socket not found at {}. This test requires a real local LXD daemon \
             (install: `sudo snap install lxd && sudo lxd init --minimal`), or set \
             OPENSHELL_LXD_TEST_SOCKET to an alternate socket path.",
            socket_path.display()
        );

        // A remote-image alias LXD's default `ubuntu:` remote resolves without
        // any manual conversion step — deliberately not the pinned OpenShell
        // sandbox image, which Phase 1 doesn't have a generic way to fetch.
        //
        // `LxdClient::create_instance` talks straight to `POST
        // /1.0/instances`, which has no concept of the `remote:alias`
        // shorthand `lxc launch`/`lxc image copy` resolve client-side —
        // passing e.g. "ubuntu:22.04" straight through as `source.alias`
        // makes LXD look for a *local* image literally named that (colon
        // included), find nothing, and fail with "Image not provided for
        // instance creation". Resolve it to a local alias first, the same
        // way `lxc launch` does under the hood, via `ensure_local_test_image`.
        let remote_image = std::env::var("OPENSHELL_LXD_TEST_IMAGE")
            .unwrap_or_else(|_| "ubuntu:22.04".to_string());
        let image = ensure_local_test_image(&remote_image)
            .await
            .expect("failed to prepare a local test image (see ensure_local_test_image)");
        let name = format!("openshell-lxd-client-test-{}", std::process::id());
        let client = LxdClient::new(socket_path);

        // Best-effort cleanup of a stale instance from a prior aborted run,
        // before asserting anything so a leftover container can't mask a
        // real failure below.
        let _ = client.delete_instance(&name).await;

        let outcome = run_lifecycle(&client, &name, &image).await;

        // Always attempt cleanup, even if an assertion above already
        // failed/panicked via `expect` — `Drop`-based guards would need
        // `catch_unwind` gymnastics for an async client, so this simple
        // "run then always clean up" shape is deliberate; see the module
        // doc comment for why this is a plain function, not a fixture.
        let cleanup_result = client.delete_instance(&name).await;

        outcome.expect("real-daemon create/get/list/delete lifecycle should succeed");
        assert!(
            cleanup_result.is_ok(),
            "final cleanup delete should not itself error: {cleanup_result:?}"
        );
    }

    /// The actual create → get → list → delete sequence, factored out so
    /// [`real_daemon_create_get_list_delete_lifecycle`] can always run
    /// cleanup afterward regardless of where this returns early.
    async fn run_lifecycle(client: &LxdClient, name: &str, image: &str) -> Result<(), String> {
        client
            .create_instance(&serde_json::json!({
                "name": name,
                "type": "container",
                "source": {"type": "image", "alias": image},
            }))
            .await
            .map_err(|e| format!("create_instance failed: {e}"))?;

        let fetched = client
            .get_instance(name)
            .await
            .map_err(|e| format!("get_instance failed: {e}"))?
            .ok_or_else(|| "instance not found immediately after create".to_string())?;
        if fetched.name != name {
            return Err(format!(
                "get_instance returned name {:?}, expected {name:?}",
                fetched.name
            ));
        }

        let listed = client
            .list_instances()
            .await
            .map_err(|e| format!("list_instances failed: {e}"))?;
        if !listed.iter().any(|i| i.name == name) {
            return Err(format!(
                "created instance {name:?} did not appear in list_instances (saw {:?})",
                listed.iter().map(|i| &i.name).collect::<Vec<_>>()
            ));
        }

        let deleted = client
            .delete_instance(name)
            .await
            .map_err(|e| format!("delete_instance failed: {e}"))?;
        if !deleted {
            return Err("delete_instance reported the instance was already gone".to_string());
        }

        let after_delete = client
            .get_instance(name)
            .await
            .map_err(|e| format!("get_instance after delete failed: {e}"))?;
        if after_delete.is_some() {
            return Err(
                "instance still present after delete_instance returned success".to_string(),
            );
        }

        Ok(())
    }
}
