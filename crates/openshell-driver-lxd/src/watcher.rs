// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Maps LXD instance lifecycle events to the compute-driver watch protocol.

use crate::client::{LxdApiError, LxdClient};
use crate::instance::{driver_sandbox_status_from_instance, sandbox_name_from_instance};
use futures::Stream;
use openshell_core::ComputeDriverError;
use openshell_core::proto::compute::v1::{
    DriverSandbox, WatchSandboxesDeletedEvent, WatchSandboxesEvent, WatchSandboxesSandboxEvent,
    watch_sandboxes_event,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::net::UnixStream;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, warn};

const SANDBOX_ID_CONFIG_KEY: &str = "user.openshell.sandbox_id";

pub type WatchStream =
    Pin<Box<dyn Stream<Item = Result<WatchSandboxesEvent, ComputeDriverError>> + Send>>;

#[derive(Debug, Deserialize)]
struct LxdEventEnvelope {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    metadata: LxdLifecycleMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct LxdLifecycleMetadata {
    #[serde(default)]
    action: String,
    #[serde(default)]
    source: String,
}

fn sandbox_event(sandbox: DriverSandbox) -> WatchSandboxesEvent {
    WatchSandboxesEvent {
        payload: Some(watch_sandboxes_event::Payload::Sandbox(
            WatchSandboxesSandboxEvent {
                sandbox: Some(sandbox),
            },
        )),
    }
}

fn deleted_event(sandbox_id: String) -> WatchSandboxesEvent {
    WatchSandboxesEvent {
        payload: Some(watch_sandboxes_event::Payload::Deleted(
            WatchSandboxesDeletedEvent { sandbox_id },
        )),
    }
}

/// Extract the instance name from an LXD lifecycle event's `source` path
/// (`/1.0/instances/<name>`).
///
/// LXD also emits lifecycle events for an instance's *sub-resources* --
/// e.g. `/1.0/instances/<name>/backups/<backup>` -- whose `source` carries
/// that suffix too. Taking the whole remainder as the instance name (an
/// earlier version of this function did) misparses those as a single,
/// much longer, invalid "instance name" -- found running a real Stage 2
/// test: `get_instance()` correctly rejected a 70-character name derived
/// from a `.../backups/lxc.log` source, logging a harmless but noisy
/// warning for an event that was never about the instance's own lifecycle
/// at all. Only the first path segment is ever a real instance name.
fn instance_name_from_source(source: &str) -> Option<&str> {
    let rest = source.strip_prefix("/1.0/instances/")?;
    Some(rest.split('/').next().unwrap_or(rest))
}

/// Start a watch stream that emits current state and live events.
///
/// Subscribes to LXD's `/1.0/events` websocket *before* listing existing
/// instances, to avoid a race that would drop events created between the
/// list and the subscription — mirroring the Podman driver's exact
/// ordering (`crates/openshell-driver-podman/src/watcher.rs`).
///
/// # Reconnection contract
///
/// **This stream is single-use and must not reconnect internally.** When
/// the LXD events websocket drops (daemon restart, socket error, or clean
/// shutdown), the stream terminates with a final error item and stops
/// producing events. The gateway's `ComputeRuntime::watch_loop` in
/// `openshell-server` already owns reconnection generically, with backoff,
/// for every driver — calling `watch_sandboxes()` again after a delay,
/// which calls [`watch`] again and re-syncs state from a fresh `list`. A
/// driver-local reconnect here would race with that retry and produce
/// duplicate initial-sync events that corrupt the gateway's sandbox index.
/// (This is the same contract the Podman driver documents; verified against
/// its actual implementation rather than assumed — see
/// `lxd-independent-review`'s Stage 4 comparison for where an earlier draft
/// of this plan got this backwards.)
pub async fn watch(client: LxdClient, socket_path: PathBuf) -> Result<WatchStream, LxdApiError> {
    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<WatchSandboxesEvent, ComputeDriverError>>(256);

    // 1. Subscribe first.
    let stream = UnixStream::connect(&socket_path)
        .await
        .map_err(|e| LxdApiError::Connection(format!("{}: {e}", socket_path.display())))?;
    let request = "ws://localhost/1.0/events?type=lifecycle"
        .into_client_request()
        .map_err(|e| LxdApiError::Connection(e.to_string()))?;
    let (ws_stream, _response) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|e| LxdApiError::Connection(format!("events websocket handshake: {e}")))?;

    // 2. List existing instances for initial state sync, building the
    // name→sandbox_id cache used to resolve later delete events (LXD's
    // lifecycle event for a deleted instance carries only the instance
    // name in its `source` path, not the sandbox ID we stamped into its
    // config — which is already gone by the time the event arrives).
    let mut known_sandbox_ids: HashMap<String, String> = HashMap::new();
    let existing = client.list_instances().await?;
    for instance in &existing {
        let Some(sandbox_id) = instance.config.get(SANDBOX_ID_CONFIG_KEY) else {
            continue;
        };
        known_sandbox_ids.insert(instance.name.clone(), sandbox_id.clone());
        let sandbox = DriverSandbox {
            id: sandbox_id.clone(),
            name: sandbox_name_from_instance(instance),
            namespace: String::new(),
            spec: None,
            status: Some(driver_sandbox_status_from_instance(instance, false)),
            workspace: String::new(),
        };
        if tx.send(Ok(sandbox_event(sandbox))).await.is_err() {
            return Err(LxdApiError::Connection(
                "watch receiver dropped during initial sync".to_string(),
            ));
        }
    }

    tokio::spawn(async move {
        run_event_loop(ws_stream, client, tx, known_sandbox_ids).await;
    });

    Ok(Box::pin(ReceiverStream::new(rx)))
}

async fn run_event_loop(
    mut ws_stream: tokio_tungstenite::WebSocketStream<UnixStream>,
    client: LxdClient,
    tx: tokio::sync::mpsc::Sender<Result<WatchSandboxesEvent, ComputeDriverError>>,
    mut known_sandbox_ids: HashMap<String, String>,
) {
    use futures::StreamExt;

    loop {
        let text = match ws_stream.next().await {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Close(_))) | None => {
                debug!("LXD events websocket closed");
                let _ = tx
                    .send(Err(ComputeDriverError::Message(
                        "LXD events websocket closed".to_string(),
                    )))
                    .await;
                return;
            }
            Some(Ok(_)) => continue, // ignore ping/pong/binary/frame messages
            Some(Err(err)) => {
                warn!(error = %err, "LXD events websocket error");
                let _ = tx
                    .send(Err(ComputeDriverError::Message(format!(
                        "LXD events websocket error: {err}"
                    ))))
                    .await;
                return;
            }
        };

        let Ok(envelope) = serde_json::from_str::<LxdEventEnvelope>(&text) else {
            continue;
        };
        if envelope.event_type != "lifecycle" {
            continue;
        }
        let Some(instance_name) = instance_name_from_source(&envelope.metadata.source) else {
            continue;
        };

        if envelope.metadata.action == "instance-deleted" {
            if let Some(sandbox_id) = known_sandbox_ids.remove(instance_name)
                && tx.send(Ok(deleted_event(sandbox_id))).await.is_err()
            {
                return;
            }
            continue;
        }

        match client.get_instance(instance_name).await {
            Ok(Some(instance)) => {
                let Some(sandbox_id) = instance
                    .config
                    .get(SANDBOX_ID_CONFIG_KEY)
                    .cloned()
                    .or_else(|| known_sandbox_ids.get(instance_name).cloned())
                else {
                    continue;
                };
                known_sandbox_ids.insert(instance_name.to_string(), sandbox_id.clone());
                let sandbox = DriverSandbox {
                    id: sandbox_id,
                    name: sandbox_name_from_instance(&instance),
                    namespace: String::new(),
                    spec: None,
                    status: Some(driver_sandbox_status_from_instance(&instance, false)),
                    workspace: String::new(),
                };
                if tx.send(Ok(sandbox_event(sandbox))).await.is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(err) => {
                warn!(error = %err, instance = %instance_name, "failed to refresh instance after lifecycle event");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_name_from_source_extracts_name() {
        assert_eq!(
            instance_name_from_source("/1.0/instances/openshell-default-abc123"),
            Some("openshell-default-abc123")
        );
    }

    #[test]
    fn instance_name_from_source_strips_sub_resource_suffixes() {
        assert_eq!(
            instance_name_from_source("/1.0/instances/openshell-default-abc123/backups/lxc.log"),
            Some("openshell-default-abc123")
        );
        assert_eq!(
            instance_name_from_source("/1.0/instances/openshell-default-abc123/snapshots/s0"),
            Some("openshell-default-abc123")
        );
    }

    #[test]
    fn instance_name_from_source_rejects_unrelated_paths() {
        assert_eq!(instance_name_from_source("/1.0/operations/xyz"), None);
    }

    #[test]
    fn lifecycle_event_parses_expected_shape() {
        let raw = r#"{
            "type": "lifecycle",
            "timestamp": "2026-07-14T10:00:00Z",
            "metadata": {
                "action": "instance-started",
                "source": "/1.0/instances/openshell-default-abc123"
            }
        }"#;
        let envelope: LxdEventEnvelope = serde_json::from_str(raw).expect("parses");
        assert_eq!(envelope.event_type, "lifecycle");
        assert_eq!(envelope.metadata.action, "instance-started");
        assert_eq!(
            instance_name_from_source(&envelope.metadata.source),
            Some("openshell-default-abc123")
        );
    }
}
