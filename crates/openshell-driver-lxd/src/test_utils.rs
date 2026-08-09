// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared test helpers for openshell-driver-lxd unit tests.
//!
//! Mirrors `crates/openshell-driver-podman/src/test_utils.rs`'s stub-server
//! pattern so `client.rs`/`driver.rs` unit tests never need a real LXD
//! daemon — this is exactly the "orthogonal to the confinement spike"
//! scaffolding the implementation plan describes.

use http_body_util::Full;
use hyper::StatusCode;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::UnixListener;

/// A canned HTTP response for the LXD stub server.
#[derive(Clone)]
pub struct StubResponse {
    pub status: StatusCode,
    pub body: String,
}

impl StubResponse {
    pub fn new(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// A synchronous-success LXD envelope with the given metadata.
    pub fn sync_success(metadata: serde_json::Value) -> Self {
        Self::new(
            StatusCode::OK,
            serde_json::json!({
                "type": "sync",
                "status": "Success",
                "status_code": 200,
                "metadata": metadata,
            })
            .to_string(),
        )
    }

    /// An error envelope, matching LXD's error response shape.
    pub fn error(error_code: i64, message: &str) -> Self {
        Self::new(
            StatusCode::OK,
            serde_json::json!({
                "type": "error",
                "error": message,
                "error_code": error_code,
            })
            .to_string(),
        )
    }
}

pub fn unique_socket_path(test_name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    PathBuf::from(format!(
        "/tmp/openshell-lxd-{test_name}-{}-{nanos}.sock",
        std::process::id()
    ))
}

/// Spawn a Unix-socket HTTP stub that serves the given `responses` in order.
pub fn spawn_lxd_stub(
    test_name: &str,
    responses: Vec<StubResponse>,
) -> (
    PathBuf,
    Arc<Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let socket_path = unique_socket_path(test_name);
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("test socket should bind");
    let request_log = Arc::new(Mutex::new(Vec::new()));
    let response_queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    let expected = response_queue
        .lock()
        .expect("response queue lock should not be poisoned")
        .len();
    let socket_path_for_task = socket_path.clone();
    let log_for_task = request_log.clone();
    let queue_for_task = response_queue;
    let handle = tokio::spawn(async move {
        for _ in 0..expected {
            let (stream, _) = listener.accept().await.expect("test stub should accept");
            let log = log_for_task.clone();
            let queue = queue_for_task.clone();
            let result = http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |req| {
                        let log = log.clone();
                        let queue = queue.clone();
                        async move {
                            let path = req.uri().path_and_query().map_or_else(
                                || req.uri().path().to_string(),
                                |pq| pq.as_str().to_string(),
                            );
                            log.lock()
                                .expect("request log lock should not be poisoned")
                                .push(format!("{} {}", req.method(), path));
                            let response = queue
                                .lock()
                                .expect("response queue lock should not be poisoned")
                                .pop_front()
                                .expect("stub response should exist");
                            Ok::<_, Infallible>(
                                hyper::Response::builder()
                                    .status(response.status)
                                    .body(Full::new(Bytes::from(response.body)))
                                    .expect("stub response should build"),
                            )
                        }
                    }),
                )
                .await;
            let _ = result;
        }
        let _ = std::fs::remove_file(&socket_path_for_task);
    });
    (socket_path, request_log, handle)
}
