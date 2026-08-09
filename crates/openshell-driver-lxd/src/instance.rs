// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `DriverSandbox` ↔ LXD instance-spec translation.
//!
//! **The security-posture constants in this module ([`security_config`])
//! have been validated against a real LXD daemon — with caveats that keep
//! this from being a closed question.**
//! The Step 0 confinement spike (`hack/confinement-spike.sh`, run via
//! `hack/run-vm-tests.sh`) ran twice against a real daemon (Ubuntu
//! 26.04 LTS, LXD 6.9 snap, kernel `7.0.0-28` then `7.0.0-29`) and both
//! times confirmed `security.nesting=true` plus the Podman-equivalent
//! capability set below lets the supervisor's `ip netns add` and
//! `unshare --net` primitives succeed unprivileged. Full run artifacts are
//! not retained past this point in the branch's history -- see
//! `hack/README.md`.
//!
//! Two caveats, both load-bearing, neither a reason to change this
//! function's output:
//!
//! 1. **Both runs' "Step A" (no `security.nesting` requested at all) also
//!    passed the same two probes** — `confinement-spike.sh` flags this as
//!    an anomalous, not a clean, pass. `security.nesting=true` may not be
//!    load-bearing at all on this specific LXD/kernel combination. Keeping
//!    it here regardless is still safe (strictly more permissive than what
//!    was proven sufficient, never less), but do not assume either result
//!    transfers to a different LXD version, Ubuntu release, or kernel
//!    without re-running the spike there — both runs so far share the same
//!    base VM image family.
//! 2. **Landlock was not verified by either of the two runs above.** At the
//!    time of both runs, `openshell-sandbox` had no `--landlock-probe`
//!    flag, so the spike's probe honestly reported this as unverified
//!    rather than silently claiming a pass (an earlier version did exactly
//!    that, via an unconditional fallback command that could not fail).
//!    **That specific gap is now closed in code** — `--landlock-probe`
//!    exists (`crates/openshell-sandbox/src/main.rs`, backed by
//!    `openshell_supervisor_process::sandbox::probe_landlock`) and
//!    `confinement-spike.sh`'s probe now calls it for real. Neither of the
//!    two runs cited above exercised it, though — a fresh run is needed
//!    before this caveat can be marked resolved rather than just
//!    "no longer structurally impossible to check."

use crate::client::LxdApiError;
use openshell_core::ComputeDriverError;
use openshell_core::proto::compute::v1::{DriverCondition, DriverSandbox, DriverSandboxStatus};
use serde_json::{Value, json};

/// Linux capabilities granted to the supervisor, mirroring the Podman
/// driver's *effective* set (`crates/openshell-driver-podman/README.md`,
/// "Capability Breakdown" *and* its "Notable Implementation Decisions"
/// item 5) — the closest analogue for an unprivileged, user-namespaced
/// local container runtime.
///
/// **This list must include more than the Podman README's "Capability
/// Breakdown" table alone** — that table is only the capabilities Podman's
/// driver *adds on top of* Podman's own container runtime defaults.
/// Podman (like Docker) ships `SETUID`/`SETGID`/`CHOWN`/`FOWNER` in its
/// default capability set already, so that driver only needs to avoid
/// *dropping* them; it never has to list them explicitly.
///
/// LXD's `raw.lxc: lxc.cap.keep` has no equivalent "default set plus
/// additions" concept: it is an **exhaustive** allowlist — every
/// capability not named here is dropped, including ones a container
/// would otherwise carry by default. A first version of this list
/// mirrored only Podman's "additions" table and omitted `setuid`/`setgid`/
/// `chown`/`fowner` as a result — compiling and passing every unit test,
/// including a self-referential one asserting this same (incomplete)
/// list's members are present. It looked correct right up until a real
/// Stage 2 run actually exercised `drop_privileges()` for the first time:
/// `setuid()`/`setgid()` failed with a bare `EPERM`, and libstd's own
/// `pre_exec` fork/exec error-propagation channel silently discarded that
/// distinguishing detail on the way back to the parent process (see
/// `process::write_pre_exec_diagnostic`'s doc comment in
/// `openshell-supervisor-process` for why, and why a raw `libc::write` to
/// fd 2 was the only way to actually see it). `chown`/`fowner` haven't
/// independently failed yet, but are included now for the same reason
/// `setuid`/`setgid` were missing: nothing here has ever been an
/// intentional, considered "no" for LXD specifically, only an
/// accidental omission from copying the wrong table.
const SUPERVISOR_CAPABILITIES: &[&str] = &[
    "sys_admin",
    "net_admin",
    "sys_ptrace",
    "syslog",
    "dac_read_search",
    "setpcap",
    "setuid",
    "setgid",
    "chown",
    "fowner",
];

/// Security-posture config keys, validated against a real LXD daemon —
/// see the module doc comment for the exact runs, and its two caveats
/// (nesting's necessity unconfirmed by an anomalous Step A pass; Landlock
/// unverified by any automated probe) before treating this as beyond
/// question. `security.nesting=true` is kept regardless of the first
/// caveat: it is strictly more permissive than what was proven sufficient,
/// never less, so keeping it costs nothing even if it turns out redundant
/// on this specific LXD/kernel combination.
fn security_config() -> Vec<(&'static str, String)> {
    vec![
        ("security.privileged", "false".to_string()),
        ("security.nesting", "true".to_string()),
        (
            "raw.lxc",
            format!("lxc.cap.keep = {}\n", SUPERVISOR_CAPABILITIES.join(" ")),
        ),
    ]
}

/// Fixed device names used on every sandbox instance.
const DEVICE_SUPERVISOR_BIN: &str = "openshell-supervisor";
const DEVICE_SANDBOX_JWT: &str = "openshell-jwt";
const DEVICE_ENTRYPOINT: &str = "openshell-entrypoint";
const DEVICE_ETH0: &str = "eth0";
const DEVICE_TLS_CA: &str = "openshell-tls-ca";
const DEVICE_TLS_CERT: &str = "openshell-tls-cert";
const DEVICE_TLS_KEY: &str = "openshell-tls-key";

/// In-instance paths the disk devices above are mounted at.
pub const SUPERVISOR_BIN_GUEST_PATH: &str = "/opt/openshell/bin/openshell-sandbox";
/// Aliases the shared constant Docker/Podman/VM already deliver the JWT
/// to (`openshell_core::driver_utils::SANDBOX_TOKEN_MOUNT_PATH`), rather
/// than duplicating the literal path — see the `OPENSHELL_SANDBOX_TOKEN_FILE`
/// comment in `build_instance_spec` for why *both* the file and the env
/// var pointing at it are required.
pub const SANDBOX_JWT_GUEST_PATH: &str = openshell_core::driver_utils::SANDBOX_TOKEN_MOUNT_PATH;
pub const ENTRYPOINT_GUEST_PATH: &str = "/opt/openshell/bin/openshell-entrypoint.sh";

/// Driver-owned environment variables. Always overrides sandbox
/// image/template values, per the architecture-wide rule
/// (`architecture/compute-runtimes.md`, "Supervisor Delivery").
const ENV_SANDBOX: &str = "OPENSHELL_SANDBOX";
const ENV_SANDBOX_ID: &str = "OPENSHELL_SANDBOX_ID";
const ENV_ENDPOINT: &str = "OPENSHELL_ENDPOINT";
const ENV_SSH_SOCKET_PATH: &str = "OPENSHELL_SSH_SOCKET_PATH";

/// Construct and validate an LXD instance name from a sandbox.
///
/// Mirrors the Podman driver's `validated_container_name` shape, but LXD's
/// naming rules are stricter (RFC 1123 label, hyphens only, no dots or
/// underscores — see [`crate::client::validate_name`]), so the workspace
/// and name components are sanitized before joining.
pub fn instance_name(sandbox: &DriverSandbox) -> Result<String, ComputeDriverError> {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    };
    let mut parts = vec!["openshell".to_string()];
    if !sandbox.workspace.is_empty() {
        parts.push(sanitize(&sandbox.workspace));
    }
    if sandbox.id.is_empty() {
        parts.push(sanitize(&sandbox.name));
    } else {
        parts.push(sanitize(&sandbox.id));
    }
    let name = parts.join("-");
    crate::client::validate_name(&name)
        .map_err(|e| ComputeDriverError::Precondition(e.to_string()))?;
    Ok(name)
}

/// Parsed, LXD-ready resource limits for one sandbox instance.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct LxdResourceLimits {
    /// `limits.cpu.allowance` value, e.g. `"50ms/100ms"`.
    ///
    /// Deliberately *not* `limits.cpu` (a whole-core count/CPU-set
    /// pinning primitive — see its own doc comment in the LXD instance
    /// options reference) despite the implementation plan's shorthand
    /// wording ("map onto `limits.cpu`"): `limits.cpu` bare would only
    /// let this driver express whole-core-or-more, throwing away any
    /// request finer than 1 full core and changing what `nproc` reports
    /// inside the sandbox — a materially different guest-visible effect
    /// from Docker's `--cpus`/Podman's CFS quota, both of which throttle
    /// via cgroup bandwidth control without restricting visible CPU
    /// count. `limits.cpu.allowance`'s "chunk of time" form
    /// (`"<quota>ms/<period>ms"`) is LXD's own cgroup-CFS-bandwidth
    /// equivalent of exactly that mechanism (a *hard* cap, not the
    /// percentage form's soft/burstable one) — the actually-equivalent
    /// mapping for a Kubernetes-style `cpu_limit`, which is itself a hard
    /// CFS quota under Kubernetes' own hood.
    cpu_allowance: Option<String>,
    /// `limits.memory` value in plain bytes with LXD's `B` suffix, e.g.
    /// `"536870912B"` — avoids picking a "nice" MiB/GiB display unit and
    /// any associated rounding; LXD parses `B` as an exact
    /// multiply-by-one suffix (see the "Units for storage and network
    /// limits" reference).
    memory_bytes: Option<String>,
}

/// Parse `template.resources` into LXD-ready limit strings.
///
/// Fails loudly on a malformed or non-positive quantity (mirrors the
/// Docker driver's `docker_resource_limits`/`parse_cpu_limit`/
/// `parse_memory_limit`, not the Podman driver's fall-back-to-a-default
/// behavior on parse failure — a malformed operator- or sandbox-supplied
/// quantity silently becoming "whatever the default happens to be" is a
/// worse failure mode for a new driver to inherit than a clear rejection
/// at create time). `cpu_request`/`memory_request` are explicitly
/// rejected, not silently ignored: LXD, like Docker, has no
/// minimum-reservation primitive distinct from its limit (its own
/// `limits.memory.enforce=soft` means "may exceed the *limit* under
/// pressure", not "guaranteed floor" — not a valid mapping target for a
/// *request*).
fn lxd_resource_limits(
    template: Option<&openshell_core::proto::compute::v1::DriverSandboxTemplate>,
) -> Result<LxdResourceLimits, ComputeDriverError> {
    let Some(resources) = template.and_then(|t| t.resources.as_ref()) else {
        return Ok(LxdResourceLimits::default());
    };

    if !resources.cpu_request.trim().is_empty() {
        return Err(ComputeDriverError::Precondition(
            "lxd compute driver does not support resources.requests.cpu".to_string(),
        ));
    }
    if !resources.memory_request.trim().is_empty() {
        return Err(ComputeDriverError::Precondition(
            "lxd compute driver does not support resources.requests.memory".to_string(),
        ));
    }

    Ok(LxdResourceLimits {
        cpu_allowance: parse_cpu_allowance(&resources.cpu_limit)?,
        memory_bytes: parse_memory_bytes(&resources.memory_limit)?,
    })
}

/// Parse a Kubernetes-style CPU quantity (`"500m"`, `"2"`, `"1.5"`) into
/// an LXD `limits.cpu.allowance` "chunk of time" string against a fixed
/// 100ms period -- see [`LxdResourceLimits::cpu_allowance`]'s doc comment
/// for why this key, not bare `limits.cpu`.
const CPU_ALLOWANCE_PERIOD_MS: u64 = 100;

fn parse_cpu_allowance(value: &str) -> Result<Option<String>, ComputeDriverError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let cores = if let Some(millicores) = value.strip_suffix('m') {
        let millicores = millicores.parse::<f64>().map_err(|_| {
            ComputeDriverError::Precondition(format!(
                "invalid lxd cpu_limit '{value}'; expected an integer or millicore quantity",
            ))
        })?;
        millicores / 1000.0
    } else {
        value.parse::<f64>().map_err(|_| {
            ComputeDriverError::Precondition(format!(
                "invalid lxd cpu_limit '{value}'; expected an integer or millicore quantity",
            ))
        })?
    };
    if !cores.is_finite() || cores <= 0.0 {
        return Err(ComputeDriverError::Precondition(
            "lxd cpu_limit must be greater than zero".to_string(),
        ));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let quota_ms = (cores * CPU_ALLOWANCE_PERIOD_MS as f64).round().max(1.0) as u64;
    Ok(Some(format!("{quota_ms}ms/{CPU_ALLOWANCE_PERIOD_MS}ms")))
}

/// Parse a Kubernetes-style memory quantity (`"512Mi"`, `"2Gi"`, `"1G"`)
/// into an exact byte count formatted with LXD's plain-bytes `B` suffix.
/// Supports both Kubernetes' binary (`Ki`/`Mi`/`Gi`/...) and decimal
/// (`K`/`M`/`G`/...) suffixes, and a bare byte count with no suffix.
fn parse_memory_bytes(value: &str) -> Result<Option<String>, ComputeDriverError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    let number_end = value
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(value.len());
    let (number, suffix) = value.split_at(number_end);
    let amount = number.parse::<f64>().map_err(|_| {
        ComputeDriverError::Precondition(format!(
            "invalid lxd memory_limit '{value}'; expected a Kubernetes-style quantity",
        ))
    })?;
    if !amount.is_finite() || amount <= 0.0 {
        return Err(ComputeDriverError::Precondition(
            "lxd memory_limit must be greater than zero".to_string(),
        ));
    }

    let multiplier = match suffix {
        "" => 1_f64,
        "Ki" => 1024_f64,
        "Mi" => 1024_f64.powi(2),
        "Gi" => 1024_f64.powi(3),
        "Ti" => 1024_f64.powi(4),
        "Pi" => 1024_f64.powi(5),
        "Ei" => 1024_f64.powi(6),
        "K" => 1000_f64,
        "M" => 1000_f64.powi(2),
        "G" => 1000_f64.powi(3),
        "T" => 1000_f64.powi(4),
        "P" => 1000_f64.powi(5),
        "E" => 1000_f64.powi(6),
        _ => {
            return Err(ComputeDriverError::Precondition(format!(
                "invalid lxd memory_limit suffix '{suffix}'",
            )));
        }
    };

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = (amount * multiplier).round() as u64;
    Ok(Some(format!("{bytes}B")))
}

/// Validate and translate a driver-config-level PIDs limit
/// (`config.sandbox_pids_limit`) into LXD's `limits.processes` value.
/// Mirrors `openshell-driver-docker`'s `docker_pids_limit` exactly: `< 0`
/// is a config error, `0` inherits LXD's own default (unlimited, so no
/// `limits.processes` key at all rather than an explicit `"0"`, which
/// LXD would otherwise interpret as *zero processes allowed*), `> 0` is
/// passed through as-is.
fn lxd_pids_limit(value: i64) -> Result<Option<String>, ComputeDriverError> {
    if value < 0 {
        return Err(ComputeDriverError::Precondition(
            "lxd sandbox_pids_limit must be zero or greater".to_string(),
        ));
    }
    if value == 0 {
        Ok(None)
    } else {
        Ok(Some(value.to_string()))
    }
}

/// A single validated driver-config mount, ready to become a `disk`
/// device.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedLxdMount {
    source: String,
    target: String,
    read_only: bool,
}

/// One entry of `template.driver_config.mounts` for the LXD driver.
///
/// **Scope decision: `bind` only** — no `volume`/`tmpfs`/`image`
/// variants, unlike Docker/Podman's own mount-config enums. Those three
/// each need machinery this driver doesn't have any other reason to
/// build: `volume` would mean creating and garbage-collecting an
/// LXD-managed custom storage volume (a real resource with its own
/// lifecycle, not a bare config translation); `tmpfs` has no native LXD
/// `disk`-device equivalent at all (would itself need a volume, backed
/// by a tmpfs-capable storage driver, created first); `image` (OCI image
/// content mounted read-only) has no LXD equivalent whatsoever. `bind`
/// alone maps onto a plain `disk` device with a host-path `source` — the
/// same primitive this driver already uses for the supervisor binary,
/// JWT, and TLS material (`build_instance_spec`) — with no new resource
/// type to create or clean up. Revisit if a real use case needs one of
/// the other three.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum LxdDriverMountConfig {
    Bind {
        source: String,
        target: String,
        #[serde(default = "default_true")]
        read_only: bool,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LxdSandboxDriverConfig {
    mounts: Vec<LxdDriverMountConfig>,
}

impl LxdSandboxDriverConfig {
    /// Parse `template.driver_config` (a `google.protobuf.Struct`) into
    /// this driver's own typed mount config. Mirrors Docker's
    /// `DockerSandboxDriverConfig::from_template` exactly, reusing the
    /// same shared JSON bridge (`openshell_core::proto_struct`) rather
    /// than hand-walking the `Struct`.
    fn from_template(
        template: &openshell_core::proto::compute::v1::DriverSandboxTemplate,
    ) -> Result<Self, String> {
        let Some(config) = template.driver_config.as_ref() else {
            return Ok(Self::default());
        };
        serde_json::from_value(openshell_core::proto_struct::struct_to_json_value(config))
            .map_err(|err| format!("invalid lxd driver_config: {err}"))
    }
}

/// Validate every driver-config mount and translate it into a validated,
/// device-ready form. Reuses `openshell_core::driver_mounts` wholesale
/// for source/target validation — the exact same rules Docker/Podman
/// enforce (absolute host paths, no reserved-path collisions, no
/// duplicate targets), not a reimplementation.
fn validated_lxd_mounts(
    mounts: &[LxdDriverMountConfig],
    enable_bind_mounts: bool,
) -> Result<Vec<ValidatedLxdMount>, ComputeDriverError> {
    let mut targets = std::collections::HashSet::new();
    let mut validated = Vec::with_capacity(mounts.len());
    for mount in mounts {
        let LxdDriverMountConfig::Bind {
            source,
            target,
            read_only,
        } = mount;
        if !enable_bind_mounts {
            return Err(ComputeDriverError::Precondition(
                "lxd bind mounts require enable_bind_mounts = true in [openshell.drivers.lxd]"
                    .to_string(),
            ));
        }
        openshell_core::driver_mounts::validate_absolute_mount_source(source, "bind source")
            .map_err(ComputeDriverError::Precondition)?;
        openshell_core::driver_mounts::validate_container_mount_target(target)
            .map_err(ComputeDriverError::Precondition)?;
        let normalized_target = openshell_core::driver_mounts::normalize_mount_target(target);
        if !targets.insert(normalized_target.clone()) {
            return Err(ComputeDriverError::Precondition(format!(
                "duplicate lxd driver_config mount target '{normalized_target}'"
            )));
        }
        validated.push(ValidatedLxdMount {
            source: source.clone(),
            target: normalized_target,
            read_only: *read_only,
        });
    }
    Ok(validated)
}

/// Build the full `POST /1.0/instances` request body for a sandbox.
///
/// Phase 1 scope: container type only, one pinned image (`config.default_image`
/// — see [`crate::config::LxdComputeConfig`]), supervisor binary and JWT
/// delivered via read-only `shift=true` disk devices (not LXD's file-push
/// API — see the design doc's rationale), no resource limits or
/// driver-config mounts (Phase 2 items).
pub fn build_instance_spec(
    sandbox: &DriverSandbox,
    config: &crate::config::LxdComputeConfig,
    grpc_endpoint: &str,
    image_alias: &str,
    image_env: &[String],
) -> Result<Value, ComputeDriverError> {
    let name = instance_name(sandbox)?;

    let mut instance_config = serde_json::Map::new();
    for (key, value) in security_config() {
        instance_config.insert(key.to_string(), Value::String(value));
    }

    // Append `lxc.init.cmd` to `raw.lxc` (LXD only allows one `raw.lxc`
    // key; multiple `lxc.*` lines must be concatenated into that single
    // multi-line value, not set via a second `raw.lxc` insertion, which
    // would silently overwrite `security_config()`'s capability line
    // instead of adding to it).
    //
    // Without this, the container boots its rootfs's own default init
    // (`/sbin/init` -> systemd) and never executes the supervisor at all
    // -- LXD containers have no Docker-`ENTRYPOINT`-equivalent concept;
    // `lxc.init.cmd` is LXC's own mechanism for replacing PID 1. Found
    // running a real Stage 2 lifecycle test: `CreateSandbox` succeeded and
    // the LXD instance started, but the sandbox never left "Requesting
    // compute" because nothing ever ran the supervisor to make the
    // callback connection that flips it to Ready. This mirrors what
    // Docker/Podman/Kubernetes already do implicitly (the supervisor *is*
    // the container's entrypoint there) -- the supervisor is already
    // designed to run as PID 1 (it does exactly that on every other
    // driver), so no supervisor-side change is implied by this.
    //
    // Points at the entrypoint *script* (below), not the supervisor binary
    // directly -- see that script's own doc comment for why: replacing
    // PID 1 skips the container's entire normal boot sequence (systemd,
    // cloud-init, netplan), which is what would otherwise run DHCP and
    // bring up eth0. `lxc.init.cmd`'s value is a bare path with no
    // argument parsing guarantees, so a shell one-liner is delivered as a
    // real file rather than risked as an inline `/bin/sh -c '...'` string
    // that depends on unverified LXC tokenization behavior.
    let existing_raw_lxc = instance_config
        .get("raw.lxc")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    instance_config.insert(
        "raw.lxc".to_string(),
        Value::String(format!(
            "{existing_raw_lxc}lxc.init.cmd = {ENTRYPOINT_GUEST_PATH}\n"
        )),
    );

    let sandbox_id = sandbox.id.clone();
    instance_config.insert(
        "user.openshell.sandbox_id".to_string(),
        Value::String(sandbox_id.clone()),
    );
    // The LXD instance name (`name` above) is a sanitized, prefixed,
    // LXD-naming-rules-compliant string derived from the sandbox's
    // workspace+id (see `instance_name()`) -- it is NOT the sandbox's own
    // `name` field the gateway/CLI use. Every place that reconstructs a
    // `DriverSandbox` from an LXD instance later (get/list/watch) needs
    // the *original* name back, not this driver's internal LXD-facing
    // one -- reporting the instance name instead is exactly what made a
    // real Stage 2 run fail with the gateway's own reconciliation store
    // rejecting every watch event as "sandbox name cannot be changed
    // after creation" (the driver was, from the gateway's point of view,
    // trying to rename the sandbox on every single event). Stamp the
    // original name into its own label, mirroring sandbox_id above, so
    // `driver.rs`/`watcher.rs` can read it back instead of falling back
    // to the instance name.
    instance_config.insert(
        "user.openshell.sandbox_name".to_string(),
        Value::String(sandbox.name.clone()),
    );

    // Image-provided environment variables (Phase 2: OCI config
    // translation, see `crate::image`'s module doc comment, point 3).
    // Applied *before* the driver-controlled block below so driver values
    // always win on key collision — `serde_json::Map::insert` overwrites,
    // and insertion order here is what makes the architecture-wide
    // override rule ("driver-controlled values always override
    // template/image values") actually hold. `image_env` is empty (and
    // this loop a no-op) for a sandbox using the driver's pinned
    // `default_image` rather than its own `spec.template.image` — that
    // path never resolved an OCI config to begin with.
    for entry in image_env {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        instance_config.insert(
            format!("environment.{key}"),
            Value::String(value.to_string()),
        );
    }

    // Driver-controlled environment variables. Applied last (as their own
    // top-level config entries) so they always win over any
    // template/image-provided `environment.*` entries merged in above —
    // see the architecture-wide override rule cited above.
    instance_config.insert(
        format!("environment.{ENV_SANDBOX}"),
        Value::String(sandbox.name.clone()),
    );
    instance_config.insert(
        format!("environment.{ENV_SANDBOX_ID}"),
        Value::String(sandbox_id),
    );
    instance_config.insert(
        format!("environment.{ENV_ENDPOINT}"),
        Value::String(grpc_endpoint.to_string()),
    );
    let ssh_socket_path = sandbox.spec.as_ref().map_or_else(
        || config.sandbox_ssh_socket_path.clone(),
        |_| config.sandbox_ssh_socket_path.clone(),
    );
    instance_config.insert(
        format!("environment.{ENV_SSH_SOCKET_PATH}"),
        Value::String(ssh_socket_path),
    );

    // Resource limits (Phase 2, Step 6). `template.resources` is
    // sandbox-supplied (a request, not a driver setting), so it's read
    // straight from `sandbox`; `sandbox_pids_limit` is a driver-config
    // knob, so it comes from `config` instead -- see each function's own
    // doc comment for why they're validated/parsed the way they are.
    let resource_limits = lxd_resource_limits(
        sandbox
            .spec
            .as_ref()
            .and_then(|spec| spec.template.as_ref()),
    )?;
    if let Some(allowance) = resource_limits.cpu_allowance {
        instance_config.insert("limits.cpu.allowance".to_string(), Value::String(allowance));
    }
    if let Some(memory) = resource_limits.memory_bytes {
        instance_config.insert("limits.memory".to_string(), Value::String(memory));
    }
    if let Some(processes) = lxd_pids_limit(config.sandbox_pids_limit)? {
        instance_config.insert("limits.processes".to_string(), Value::String(processes));
    }

    // Driver-config mounts (Phase 2, Step 7). Parsed here (validation
    // doesn't need `devices` yet), translated into devices below once
    // `devices` exists.
    let template = sandbox
        .spec
        .as_ref()
        .and_then(|spec| spec.template.as_ref());
    let driver_mounts = match template {
        Some(template) => LxdSandboxDriverConfig::from_template(template)
            .map_err(ComputeDriverError::Precondition)?,
        None => LxdSandboxDriverConfig::default(),
    };
    let validated_mounts = validated_lxd_mounts(&driver_mounts.mounts, config.enable_bind_mounts)?;

    let mut devices = serde_json::Map::new();
    devices.insert(
        DEVICE_ETH0.to_string(),
        json!({
            "type": "nic",
            "nictype": "bridged",
            "parent": config.network_name,
        }),
    );
    devices.insert(
        DEVICE_SUPERVISOR_BIN.to_string(),
        json!({
            "type": "disk",
            "source": config.supervisor_bin.display().to_string(),
            "path": SUPERVISOR_BIN_GUEST_PATH,
            "readonly": "true",
            "shift": "true",
        }),
    );
    devices.insert(
        DEVICE_ENTRYPOINT.to_string(),
        json!({
            "type": "disk",
            "source": entrypoint_script_host_path(&sandbox.id)
                .map_err(ComputeDriverError::from)?
                .display()
                .to_string(),
            "path": ENTRYPOINT_GUEST_PATH,
            "readonly": "true",
            "shift": "true",
        }),
    );

    for (index, mount) in validated_mounts.iter().enumerate() {
        devices.insert(
            format!("openshell-mount-{index}"),
            json!({
                "type": "disk",
                "source": mount.source,
                "path": mount.target,
                "readonly": mount.read_only.to_string(),
                "shift": "true",
            }),
        );
    }

    if let Some(token_path) = sandbox
        .spec
        .as_ref()
        .filter(|spec| !spec.sandbox_token.trim().is_empty())
        .and_then(|_| sandbox_token_host_path(&sandbox.id).ok())
    {
        devices.insert(
            DEVICE_SANDBOX_JWT.to_string(),
            json!({
                "type": "disk",
                "source": token_path.display().to_string(),
                "path": SANDBOX_JWT_GUEST_PATH,
                "readonly": "true",
                "shift": "true",
            }),
        );
        // Delivering the JWT file alone is not enough -- the supervisor's
        // token resolution (`openshell_core::grpc_client::acquire_sandbox_token`)
        // only ever checks three *environment variables* in order
        // (`SANDBOX_TOKEN`, `SANDBOX_TOKEN_FILE`, `K8S_SA_TOKEN_FILE`); it
        // never assumes a fixed path. Without this, every one of this
        // sandbox's outbound RPCs (starting with its very first policy
        // fetch) fails with "no sandbox token source available", wrapped
        // by the supervisor's own retry-with-backoff loop into the same
        // observable shape as a network failure ("Policy fetch failed,
        // retrying" x4, then exit) -- found running a real Stage 2 test
        // *after* fixing an actual, separate network bring-up gap, only to
        // hit this still failing identically because a raw TCP probe to
        // the gateway succeeded while the supervisor's own gRPC call kept
        // failing. Docker, Podman, and the VM driver already set exactly
        // this env var pointing at the same mount path constant
        // (`openshell_core::container_paths::SANDBOX_TOKEN_MOUNT_PATH`,
        // which is `SANDBOX_JWT_GUEST_PATH` here) -- this driver was the
        // one that never did.
        instance_config.insert(
            format!(
                "environment.{}",
                openshell_core::sandbox_env::SANDBOX_TOKEN_FILE
            ),
            Value::String(SANDBOX_JWT_GUEST_PATH.to_string()),
        );
    }

    // Guest mTLS material (Phase 2, Step 5). `config.validate()` already
    // enforces "all three or none" at driver startup
    // (`LxdComputeConfig::validate_tls_config`), so by the time this runs,
    // either all three are `Some` or all three are `None` — this `if let`
    // is a straightforward gate on the already-validated invariant, not a
    // second place that invariant needs re-checking. Delivered via the
    // same read-only `shift=true` disk-device mechanism as the supervisor
    // binary and JWT above, to the same fixed guest paths Docker/Podman/VM
    // already use (`openshell_core::container_paths::TLS_*_MOUNT_PATH`),
    // so the supervisor's own TLS-loading code
    // (`openshell_core::grpc_client`) works identically regardless of
    // which driver delivered the certificates.
    if let (Some(ca), Some(cert), Some(key)) = (
        &config.guest_tls_ca,
        &config.guest_tls_cert,
        &config.guest_tls_key,
    ) {
        devices.insert(
            DEVICE_TLS_CA.to_string(),
            json!({
                "type": "disk",
                "source": ca.display().to_string(),
                "path": openshell_core::container_paths::TLS_CA_MOUNT_PATH,
                "readonly": "true",
                "shift": "true",
            }),
        );
        devices.insert(
            DEVICE_TLS_CERT.to_string(),
            json!({
                "type": "disk",
                "source": cert.display().to_string(),
                "path": openshell_core::container_paths::TLS_CERT_MOUNT_PATH,
                "readonly": "true",
                "shift": "true",
            }),
        );
        devices.insert(
            DEVICE_TLS_KEY.to_string(),
            json!({
                "type": "disk",
                "source": key.display().to_string(),
                "path": openshell_core::container_paths::TLS_KEY_MOUNT_PATH,
                "readonly": "true",
                "shift": "true",
            }),
        );
        instance_config.insert(
            format!("environment.{}", openshell_core::sandbox_env::TLS_CA),
            Value::String(openshell_core::container_paths::TLS_CA_MOUNT_PATH.to_string()),
        );
        instance_config.insert(
            format!("environment.{}", openshell_core::sandbox_env::TLS_CERT),
            Value::String(openshell_core::container_paths::TLS_CERT_MOUNT_PATH.to_string()),
        );
        instance_config.insert(
            format!("environment.{}", openshell_core::sandbox_env::TLS_KEY),
            Value::String(openshell_core::container_paths::TLS_KEY_MOUNT_PATH.to_string()),
        );
    }

    Ok(json!({
        "name": name,
        "type": "container",
        "source": {
            "type": "image",
            "alias": image_alias,
        },
        "config": Value::Object(instance_config),
        "devices": Value::Object(devices),
    }))
}

/// Resolve the host-side path the driver writes a sandbox's JWT to, before
/// attaching it as a disk device.
///
/// Reuses the existing driver-agnostic helper Docker already uses for the
/// same purpose (`openshell_core::driver_utils::sandbox_token_path`),
/// rather than inventing a second convention.
pub fn sandbox_token_host_path(sandbox_id: &str) -> Result<std::path::PathBuf, LxdApiError> {
    openshell_core::driver_utils::sandbox_token_path("lxd", None, sandbox_id)
        .map_err(|e| LxdApiError::InvalidInput(e.to_string()))
}

/// Resolve the host-side path the driver writes a sandbox's entrypoint
/// script to (see [`build_entrypoint_script`]), before attaching it as a
/// disk device. Reuses the JWT's own per-sandbox directory rather than
/// inventing a second one.
pub fn entrypoint_script_host_path(sandbox_id: &str) -> Result<std::path::PathBuf, LxdApiError> {
    let jwt_path = sandbox_token_host_path(sandbox_id)?;
    let parent = jwt_path.parent().ok_or_else(|| {
        LxdApiError::InvalidInput("sandbox token path has no parent directory".to_string())
    })?;
    Ok(parent.join("entrypoint.sh"))
}

/// Resolve the host-side scratch directory the OCI-to-LXD image
/// conversion pipeline (`crate::image::ensure_lxd_image`) uses for
/// pull/merge/package staging, when a sandbox requests its own image
/// (`spec.template.image`) rather than relying on the driver's pinned
/// `default_image`. Reuses the JWT's own per-sandbox directory, same as
/// [`entrypoint_script_host_path`], rather than inventing a third
/// per-sandbox location.
pub fn image_staging_dir(sandbox_id: &str) -> Result<std::path::PathBuf, LxdApiError> {
    let jwt_path = sandbox_token_host_path(sandbox_id)?;
    let parent = jwt_path.parent().ok_or_else(|| {
        LxdApiError::InvalidInput("sandbox token path has no parent directory".to_string())
    })?;
    Ok(parent.join("image-staging"))
}

/// Derive a deterministic static IPv4 host octet (2-254, avoiding the
/// bridge's own `.1` gateway and the `.255` broadcast address) for a
/// sandbox within the driver's managed `/24` bridge subnet.
///
/// **Phase 1 stopgap, not a real IPAM.** Overriding `lxc.init.cmd` (see
/// [`build_instance_spec`]) means the container never runs its normal
/// boot sequence (systemd/cloud-init/netplan), so nothing ever performs
/// the DHCP negotiation LXD's "bridged" NIC model otherwise relies on
/// entirely to give the guest an IP at all — unlike Docker/Podman/
/// Kubernetes, which inject the container's IP externally, LXD expects
/// the *guest* to configure its own network, exactly like a real VM.
/// Found running a real Stage 2 test: the supervisor started, tried to
/// reach the gateway, and failed repeatedly ("Policy fetch failed,
/// retrying") because `eth0` never had an IP address at all. This
/// hash-based derivation is a deterministic-but-uncoordinated pick with
/// no collision detection against other sandboxes on the same bridge —
/// acceptable for validating the lifecycle one sandbox at a time, not for
/// concurrent or production use. A real fix needs either an in-guest DHCP
/// client invocation or a proper collision-checked IPAM.
fn static_host_octet(sandbox_id: &str) -> u8 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sandbox_id.hash(&mut hasher);
    2 + u8::try_from(hasher.finish() % 253).unwrap_or(0)
}

/// Split a `/24` CIDR (e.g. `"10.88.77.1/24"`) into its base network
/// (`"10.88.77"`) and gateway address (`"10.88.77.1"`).
///
/// Pinned to `/24` -- every subnet this crate documents or defaults to
/// ([`crate::config::DEFAULT_NETWORK_IPV4_SUBNET`], and every value used
/// in this driver's own test scripts) is a `/24`; a different prefix
/// length would need real CIDR arithmetic, not this string-splitting
/// shortcut.
fn subnet_base_and_gateway(cidr: &str) -> Option<(String, String)> {
    let gateway = cidr.split('/').next()?;
    let octets: Vec<&str> = gateway.split('.').collect();
    if octets.len() != 4 {
        return None;
    }
    let base = format!("{}.{}.{}", octets[0], octets[1], octets[2]);
    Some((base, gateway.to_string()))
}

/// Build the entrypoint script content delivered via the
/// `openshell-entrypoint` disk device and run as `lxc.init.cmd`.
///
/// Statically assigns `eth0` an address in the driver's managed bridge
/// subnet and a default route, then execs the real supervisor — see
/// [`static_host_octet`]'s doc comment for why this exists and its real
/// limitations. `2>/dev/null` on the address-assignment step tolerates
/// re-runs (e.g. a container restart) where the address may already be
/// assigned.
pub fn build_entrypoint_script(
    config: &crate::config::LxdComputeConfig,
    sandbox_id: &str,
) -> String {
    let (subnet_base, gateway_ip) = subnet_base_and_gateway(&config.network_ipv4_subnet)
        .unwrap_or_else(|| ("10.88.77".to_string(), "10.88.77.1".to_string()));
    let host_octet = static_host_octet(sandbox_id);
    let static_ip = format!("{subnet_base}.{host_octet}");

    // Best-effort target host:port for the TCP connectivity probe below,
    // parsed from the same endpoint the supervisor itself will dial.
    let parsed = url::Url::parse(&config.grpc_endpoint).ok();
    let target_host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .unwrap_or(&gateway_ip)
        .to_string();
    let target_port = parsed
        .as_ref()
        .and_then(url::Url::port)
        .unwrap_or(config.gateway_port);

    format!(
        "#!/bin/sh\n\
         # Phase 1 stopgap network bring-up -- see static_host_octet()'s doc\n\
         # comment in instance.rs for why this exists and its real limitations\n\
         # (no collision checking, not real DHCP).\n\
         #\n\
         # As a raw `lxc.init.cmd` process, this never goes through a login\n\
         # shell/profile, so PATH is not guaranteed to include where `ip`\n\
         # actually lives (or may be unset entirely) -- set an explicit,\n\
         # comprehensive one rather than assume. Without `set -e`, a failing\n\
         # `ip` command here would otherwise silently do nothing and fall\n\
         # through to `exec` anyway, which is exactly the failure mode an\n\
         # earlier version of this script had no visibility into at all: no\n\
         # console capture exists for a custom lxc.init.cmd process, so its\n\
         # own stdout/stderr is redirected to a real file under /var/log\n\
         # instead -- the one channel already proven reachable (it's where\n\
         # the supervisor's own rolling log lands) and already captured by\n\
         # run-stage2.sh's diagnostics (which globs `openshell*.log`).\n\
         #\n\
         # This redirect is a standalone `exec > file 2>&1` (not a `{{ ...\n\
         # }} > file` compound-command redirect) *specifically* so it stays\n\
         # in effect for the rest of the script, including the final `exec\n\
         # {SUPERVISOR_BIN_GUEST_PATH}` below: POSIX shell restores the\n\
         # original fds once a `{{ ...; }}` block's own redirect ends, so a\n\
         # scoped redirect would silently stop applying right before the\n\
         # supervisor starts -- which is exactly the failure mode found\n\
         # running a real Stage 2 test (the supervisor panicked/exited(1)\n\
         # immediately after binding its proxy listener, and neither this\n\
         # log file nor `lxc info --show-log` captured why, because that\n\
         # output went to the LXD pty console log instead, which\n\
         # run-stage2.sh wasn't capturing either).\n\
         export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n\
         # /var/log's ownership/permissions are whatever the source OCI\n\
         # image's layers declared -- not every image leaves it writable by\n\
         # container-root without CAP_DAC_OVERRIDE, which this driver's\n\
         # supervisor capability set deliberately omits (see\n\
         # SUPERVISOR_CAPABILITIES's doc comment: LXD's `lxc.cap.keep` is an\n\
         # exhaustive allowlist, and DAC_OVERRIDE is one of the capabilities\n\
         # intentionally dropped from Podman's own default set, not an\n\
         # oversight to fix by re-adding it here). A real sandbox image hit\n\
         # exactly this: `exec >/var/log/...` failed with EACCES, and since\n\
         # a failed `exec` redirect kills a POSIX shell outright, PID 1 died\n\
         # before ever reaching the network bring-up below, well before the\n\
         # supervisor itself even started. Probe writability first with a\n\
         # harmless write attempt, inside an `if`, which POSIX shells do NOT\n\
         # treat as fatal the way a bare failed `exec` redirect is --\n\
         # **except** when the probe command is a POSIX *special* builtin\n\
         # like `:`: POSIX mandates that a redirection error on a special\n\
         # builtin exits the shell immediately, `if` guard or not (confirmed\n\
         # directly against dash, Ubuntu's real `/bin/sh` and thus what\n\
         # actually runs this script as PID 1 -- an initial version of this\n\
         # fallback used `:` and dash exited before ever reaching the\n\
         # `ENTRYPOINT_LOG=/tmp/...` line below, an entirely different\n\
         # failure mode than the one this fallback exists to fix, but with\n\
         # the identical externally-visible symptom of PID 1 dying on\n\
         # startup). `true` is a *regular* builtin -- same redirection\n\
         # syntax, but a failure there behaves like any ordinary command's\n\
         # nonzero exit status, which `if` *does* correctly catch. Redirect\n\
         # stderr to `/dev/null` *before* the write attempt, not after: dash\n\
         # emits its own diagnostic for the failing redirect itself (not\n\
         # through the command's own stderr), so only a redirect already in\n\
         # effect at that point suppresses it. Unlike /var/log, /tmp's\n\
         # sticky-bit world-writable default is something every Linux\n\
         # distro's base filesystem layout already guarantees.\n\
         ENTRYPOINT_LOG=/var/log/openshell-entrypoint.log\n\
         if ! true 2>/dev/null >\"$ENTRYPOINT_LOG\"; then\n\
         ENTRYPOINT_LOG=/tmp/openshell-entrypoint.log\n\
         fi\n\
         exec >\"$ENTRYPOINT_LOG\" 2>&1\n\
         echo \"=== openshell entrypoint: $(date -u 2>&1) ===\"\n\
         echo \"entrypoint log: $ENTRYPOINT_LOG\"\n\
         echo \"PATH=$PATH\"\n\
         command -v ip || echo 'ip not found on PATH'\n\
         ip addr add {static_ip}/24 dev eth0; echo \"ip addr add exit: $?\"\n\
         ip link set eth0 up; echo \"ip link set up exit: $?\"\n\
         ip route add default via {gateway_ip} dev eth0; echo \"ip route add exit: $?\"\n\
         ip addr show eth0\n\
         ip route show\n\
         echo \"--- TCP probe to {target_host}:{target_port} (the supervisor's own gateway target) ---\"\n\
         # `/bin/sh` (dash on Ubuntu) has no /dev/tcp; the supervisor's own\n\
         # gRPC error text is unavailable here too (see instance.rs's own\n\
         # comment on build_entrypoint_script for why -- the shared\n\
         # OcsfShorthandLayer formatter drops every field except \"message\"\n\
         # for plain tracing events, so this is the most direct way left to\n\
         # tell a network/firewall block apart from an application-layer\n\
         # failure without changing shared logging code). Only run this if\n\
         # bash happens to be present; skip cleanly otherwise.\n\
         if command -v bash >/dev/null 2>&1; then\n\
         bash -c 'exec 3<>/dev/tcp/{target_host}/{target_port}' \\\n\
         && echo 'TCP connect: OK' || echo 'TCP connect: FAILED'\n\
         else\n\
         echo 'bash not present; skipping TCP probe'\n\
         fi\n\
         echo \"=== exec'ing supervisor: $(date -u 2>&1) ===\"\n\
         exec {SUPERVISOR_BIN_GUEST_PATH} \"$@\"\n"
    )
}

/// Map an LXD instance's status code onto `DriverCondition`s.
///
/// LXD status codes: 100 Starting, 101 Started, 102 Stopped, 103 Running,
/// 104 Cancelling, 105 Pending, 106 Starting, 107 Stopping, 108 Aborting,
/// 109 Freezing, 110 Frozen, 111 Thawed, 200 Success, 400 Failure,
/// 401 Cancelled.
pub fn driver_condition_from_status_code(status_code: i64, status_text: &str) -> DriverCondition {
    use crate::client::status_code;
    let (condition_type, ready) = match status_code {
        code if code == status_code::RUNNING => ("Ready", true),
        code if code == status_code::ERROR => ("Ready", false),
        _ => ("Provisioning", false),
    };
    DriverCondition {
        r#type: condition_type.to_string(),
        status: if ready { "True" } else { "False" }.to_string(),
        reason: status_text.to_string(),
        message: format!("LXD instance status: {status_text} ({status_code})"),
        last_transition_time: String::new(),
    }
}

/// Config key holding the sandbox's *original* name (as the gateway/CLI
/// know it) — distinct from the LXD instance's own sanitized, prefixed
/// name (see [`instance_name`]). Stamped at creation time by
/// [`build_instance_spec`], mirroring the sandbox-ID label above.
pub const SANDBOX_NAME_CONFIG_KEY: &str = "user.openshell.sandbox_name";

/// Resolve an LXD instance's original sandbox name from its
/// [`SANDBOX_NAME_CONFIG_KEY`] label, falling back to the LXD instance
/// name only for instances that predate this label (there should be none
/// in practice, since this driver stamps it on every create — but a
/// silent panic/`None` here would be a worse failure mode than a
/// slightly-wrong display name).
///
/// Every place that reconstructs a `DriverSandbox`/`DriverSandboxStatus`
/// from an LXD instance (`get_sandbox`, `list_sandboxes`, both watcher.rs
/// sync paths) must go through this, not `instance.name` directly — using
/// the raw instance name here is exactly what made a real Stage 2 run fail
/// with the gateway's reconciliation store rejecting every single watch
/// event as "sandbox name cannot be changed after creation" (the driver
/// was, from the gateway's point of view, trying to rename the sandbox on
/// every event).
pub fn sandbox_name_from_instance(instance: &crate::client::Instance) -> String {
    instance
        .config
        .get(SANDBOX_NAME_CONFIG_KEY)
        .cloned()
        .unwrap_or_else(|| instance.name.clone())
}

/// Build a `DriverSandbox` status snapshot from an observed LXD instance.
pub fn driver_sandbox_status_from_instance(
    instance: &crate::client::Instance,
    deleting: bool,
) -> DriverSandboxStatus {
    DriverSandboxStatus {
        sandbox_name: sandbox_name_from_instance(instance),
        instance_id: instance.name.clone(),
        agent_fd: String::new(),
        sandbox_fd: String::new(),
        conditions: vec![driver_condition_from_status_code(
            instance.status_code,
            &instance.status,
        )],
        deleting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openshell_core::proto::compute::v1::DriverSandbox;

    fn test_sandbox() -> DriverSandbox {
        DriverSandbox {
            id: "abc123".to_string(),
            name: "demo".to_string(),
            namespace: String::new(),
            spec: None,
            status: None,
            workspace: "default".to_string(),
        }
    }

    #[test]
    fn build_instance_spec_stamps_original_sandbox_name_label() {
        let sandbox = test_sandbox();
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.0.0.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        // The LXD instance name ("openshell-default-abc123") and the
        // sandbox's own name ("demo") are deliberately different strings
        // -- see instance_name_is_lxd_safe below. Both must be independently
        // recoverable: the instance name from the top-level "name" field
        // (LXD's own requirement), the original sandbox name from this
        // label (this driver's own requirement, or the gateway rejects
        // every later get/list/watch report as an attempted rename).
        assert_eq!(
            spec["config"][SANDBOX_NAME_CONFIG_KEY],
            Value::String("demo".to_string())
        );
    }

    #[test]
    fn sandbox_name_from_instance_prefers_the_label_over_the_instance_name() {
        let mut config = std::collections::HashMap::new();
        config.insert(SANDBOX_NAME_CONFIG_KEY.to_string(), "demo".to_string());
        let instance = crate::client::Instance {
            name: "openshell-default-abc123".to_string(),
            status: String::new(),
            status_code: 0,
            config,
            last_used_at: None,
        };
        assert_eq!(sandbox_name_from_instance(&instance), "demo");
    }

    #[test]
    fn sandbox_name_from_instance_falls_back_to_instance_name_without_the_label() {
        let instance = crate::client::Instance {
            name: "openshell-default-abc123".to_string(),
            status: String::new(),
            status_code: 0,
            config: std::collections::HashMap::new(),
            last_used_at: None,
        };
        assert_eq!(
            sandbox_name_from_instance(&instance),
            "openshell-default-abc123"
        );
    }

    #[test]
    fn subnet_base_and_gateway_splits_a_slash_24_cidr() {
        assert_eq!(
            subnet_base_and_gateway("10.88.77.1/24"),
            Some(("10.88.77".to_string(), "10.88.77.1".to_string()))
        );
    }

    #[test]
    fn subnet_base_and_gateway_rejects_malformed_input() {
        assert_eq!(subnet_base_and_gateway("not-a-cidr"), None);
        assert_eq!(subnet_base_and_gateway("10.88.77/24"), None);
    }

    #[test]
    fn static_host_octet_is_deterministic_and_in_range() {
        let a = static_host_octet("sandbox-1");
        let b = static_host_octet("sandbox-1");
        let c = static_host_octet("sandbox-2");
        assert_eq!(a, b, "same sandbox id must always get the same octet");
        assert!((2..=254).contains(&a));
        assert!((2..=254).contains(&c));
    }

    #[test]
    fn build_entrypoint_script_configures_eth0_before_exec() {
        let config = crate::config::LxdComputeConfig {
            network_ipv4_subnet: "10.88.77.1/24".to_string(),
            ..crate::config::LxdComputeConfig::default()
        };
        let script = build_entrypoint_script(&config, "sandbox-1");
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("ip addr add 10.88.77."));
        assert!(script.contains("/24 dev eth0"));
        assert!(script.contains("ip link set eth0 up"));
        assert!(script.contains("ip route add default via 10.88.77.1 dev eth0"));
        assert!(script.ends_with(&format!("exec {SUPERVISOR_BIN_GUEST_PATH} \"$@\"\n")));
    }

    #[test]
    fn build_entrypoint_script_falls_back_to_tmp_when_var_log_is_not_writable() {
        let config = crate::config::LxdComputeConfig {
            network_ipv4_subnet: "10.88.77.1/24".to_string(),
            ..crate::config::LxdComputeConfig::default()
        };
        let script = build_entrypoint_script(&config, "sandbox-1");
        assert!(
            script.contains("ENTRYPOINT_LOG=/var/log/openshell-entrypoint.log"),
            "should still prefer /var/log when writable: {script}"
        );
        assert!(
            script.contains("ENTRYPOINT_LOG=/tmp/openshell-entrypoint.log"),
            "should fall back to /tmp when /var/log is not writable: {script}"
        );
        // The fallback must be probed via a plain `if`, never a bare `exec`
        // redirect -- a failed `exec > file` redirect kills a POSIX shell
        // outright (this exact failure mode is what broke a real sandbox
        // image's container startup before this fallback existed), so the
        // *actual* long-lived redirect must come after the writability
        // check has already picked a working path, not be the check itself.
        let exec_redirect_pos = script
            .find("exec >\"$ENTRYPOINT_LOG\" 2>&1")
            .expect("script must redirect stdout/stderr to the resolved log path");
        let fallback_pos = script
            .find("ENTRYPOINT_LOG=/tmp/openshell-entrypoint.log")
            .expect("fallback assignment must be present");
        assert!(
            fallback_pos < exec_redirect_pos,
            "the /tmp fallback must be decided before the exec redirect runs"
        );
    }

    #[test]
    fn build_entrypoint_script_survives_an_unwritable_var_log_under_dash() {
        // A *runtime* regression test, deliberately distinct from `sh -n`
        // syntax-checking below: `sh -n` only parses the script, it never
        // executes anything, so it cannot catch a runtime behavior
        // difference like "does a failed redirect on this specific builtin
        // kill the shell". That gap is exactly how an earlier version of
        // this fallback (using `:` instead of `true` as the writability
        // probe) shipped broken -- `sh -n` passed, the *other* dedicated
        // fallback test above passed (it only inspects the generated
        // string, not runtime behavior), and it still took a real Stage 2
        // VM run to discover dash exits immediately on a redirection error
        // for `:` specifically (a POSIX *special* builtin) even inside an
        // `if` guard, a rule that does not apply to ordinary commands.
        //
        // Runs under `dash` specifically, not whatever `sh` resolves to on
        // the test host -- confirmed directly that this matters: macOS's
        // `/bin/sh` does not reproduce dash's special-builtin-exit
        // behavior, so a test using generic `sh` would pass regardless of
        // which builtin the fallback probe used, defeating the point.
        // Ubuntu (the real target) symlinks `/bin/sh` to dash, so this is
        // not testing a hypothetical shell, it is testing THE shell.
        if std::process::Command::new("dash")
            .arg("-c")
            .arg("true")
            .status()
            .is_err()
        {
            eprintln!("dash not available on this host; skipping");
            return;
        }

        let config = crate::config::LxdComputeConfig {
            network_ipv4_subnet: "10.88.77.1/24".to_string(),
            ..crate::config::LxdComputeConfig::default()
        };
        let script = build_entrypoint_script(&config, "sandbox-1");

        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("entrypoint.sh");
        std::fs::write(&script_path, &script).expect("write script");

        // Real `/var/log` is unwritable by an unprivileged test-runner user
        // on both Linux and macOS (root-owned, mode not world-writable) --
        // exercising the actual fallback path needs no mocking, the
        // ambient test environment already provides the exact condition
        // this fallback exists to handle.
        let is_root = std::env::var("USER").as_deref() == Ok("root")
            || std::process::Command::new("id")
                .arg("-u")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
                .unwrap_or(false);
        if is_root {
            eprintln!("running as root; skipping (would defeat the /var/log-unwritable premise)");
            return;
        }

        // The script's own final `exec {SUPERVISOR_BIN_GUEST_PATH}` targets
        // a path that doesn't exist on the test host, so dash will exit
        // nonzero once it gets there -- expected, and irrelevant to this
        // test. What matters is that it gets *past* the entrypoint-log
        // setup first, not that the whole script "succeeds" end to end.
        let _ = std::process::Command::new("dash")
            .arg(&script_path)
            .status();

        let fallback_log = std::path::Path::new("/tmp/openshell-entrypoint.log");
        let contents = std::fs::read_to_string(fallback_log).unwrap_or_else(|e| {
            panic!(
                "expected the /tmp fallback log to exist after running under dash \
                 with an unwritable /var/log, but reading it failed: {e}\n\
                 script:\n{script}"
            )
        });
        assert!(
            contents.contains("openshell entrypoint:"),
            "fallback log exists but is missing the expected banner: {contents:?}"
        );
        assert!(
            contents.contains("entrypoint log: /tmp/openshell-entrypoint.log"),
            "fallback log should record that it fell back to /tmp: {contents:?}"
        );
        let _ = std::fs::remove_file(fallback_log);
    }

    #[test]
    fn build_entrypoint_script_is_syntactically_valid_shell() {
        // The generated script is never syntax-checked by anything before
        // it runs for real as PID 1 inside a container -- catch a broken
        // format!() edit (mismatched braces, a stray quote) here instead
        // of burning another real Stage 2 round-trip on it.
        let config = crate::config::LxdComputeConfig {
            network_ipv4_subnet: "10.88.77.1/24".to_string(),
            ..crate::config::LxdComputeConfig::default()
        };
        let script = build_entrypoint_script(&config, "sandbox-1");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("entrypoint.sh");
        std::fs::write(&path, &script).expect("write script");

        let status = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .status()
            .expect("run `sh -n`");
        assert!(
            status.success(),
            "generated entrypoint script has a shell syntax error:\n{script}"
        );
    }

    #[test]
    fn build_instance_spec_delivers_entrypoint_script_and_points_init_cmd_at_it() {
        let sandbox = test_sandbox();
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        let device = &spec["devices"][DEVICE_ENTRYPOINT];
        assert_eq!(device["type"], "disk");
        assert_eq!(device["path"], ENTRYPOINT_GUEST_PATH);
        assert_eq!(device["readonly"], "true");
        assert_eq!(device["shift"], "true");
        let source = device["source"].as_str().expect("source is a string");
        assert!(
            source.ends_with("entrypoint.sh"),
            "entrypoint device source should be the generated script: {source}"
        );
    }

    #[test]
    fn instance_name_is_lxd_safe() {
        let name = instance_name(&test_sandbox()).expect("valid name");
        assert_eq!(name, "openshell-default-abc123");
        crate::client::validate_name(&name).expect("name passes LXD validation");
    }

    #[test]
    fn instance_name_sanitizes_non_alphanumeric_components() {
        let sandbox = DriverSandbox {
            workspace: "my_workspace.v2".to_string(),
            ..test_sandbox()
        };
        let name = instance_name(&sandbox).expect("valid name");
        crate::client::validate_name(&name).expect("sanitized name passes LXD validation");
    }

    #[test]
    fn build_instance_spec_sets_validated_security_posture() {
        let sandbox = test_sandbox();
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "https://_gateway.lxd:17670",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        assert_eq!(spec["type"], "container");
        assert_eq!(spec["config"]["security.privileged"], "false");
        assert_eq!(spec["config"]["security.nesting"], "true");
        let raw_lxc = spec["config"]["raw.lxc"]
            .as_str()
            .expect("raw.lxc is a string");
        assert!(
            raw_lxc.contains(&format!("lxc.init.cmd = {ENTRYPOINT_GUEST_PATH}")),
            "raw.lxc should override PID 1 to the entrypoint script (which \
             brings up eth0, then execs the supervisor), or the \
             container just boots its rootfs's default init and never \
             runs either: {raw_lxc}"
        );
        for cap in SUPERVISOR_CAPABILITIES {
            assert!(
                raw_lxc.contains(cap),
                "missing capability {cap} in {raw_lxc}"
            );
        }
    }

    #[test]
    fn build_instance_spec_delivers_supervisor_via_disk_device_not_file_push() {
        let sandbox = test_sandbox();
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "https://_gateway.lxd:17670",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        let device = &spec["devices"][DEVICE_SUPERVISOR_BIN];
        assert_eq!(device["type"], "disk");
        assert_eq!(device["path"], SUPERVISOR_BIN_GUEST_PATH);
        assert_eq!(device["readonly"], "true");
        assert_eq!(device["shift"], "true");
    }

    #[test]
    fn build_instance_spec_points_the_supervisor_at_its_own_delivered_jwt() {
        // Delivering the JWT file alone is not enough -- the supervisor
        // only ever looks for it via OPENSHELL_SANDBOX_TOKEN_FILE (or the
        // other two env-var sources), never a fixed path. A real Stage 2
        // run failed identically to a network problem ("Policy fetch
        // failed, retrying" x4, then exit) even *after* a genuine network
        // fix, until this env var was added too -- confirmed via a raw TCP
        // probe succeeding while the supervisor's own gRPC call kept
        // failing regardless.
        let sandbox = DriverSandbox {
            spec: Some(openshell_core::proto::compute::v1::DriverSandboxSpec {
                sandbox_token: "test-jwt".to_string(),
                ..Default::default()
            }),
            ..test_sandbox()
        };
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        assert!(
            spec["devices"][DEVICE_SANDBOX_JWT]["path"] == SANDBOX_JWT_GUEST_PATH,
            "JWT device should still be delivered when a token is present: {spec}"
        );
        assert_eq!(
            spec["config"][format!(
                "environment.{}",
                openshell_core::sandbox_env::SANDBOX_TOKEN_FILE
            )],
            SANDBOX_JWT_GUEST_PATH
        );
    }

    #[test]
    fn build_instance_spec_merges_image_env_but_driver_env_wins_on_collision() {
        let sandbox = test_sandbox();
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let image_env = vec![
            "PYTHONPATH=/opt/venv/lib".to_string(),
            format!("{ENV_ENDPOINT}=attacker-controlled-value"),
            "not-a-key-value-pair".to_string(),
        ];
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &image_env,
        )
        .expect("spec builds");

        assert_eq!(
            spec["config"]["environment.PYTHONPATH"],
            Value::String("/opt/venv/lib".to_string())
        );
        // The image tried to set OPENSHELL_ENDPOINT too -- the
        // driver-controlled value (inserted after image_env, see
        // build_instance_spec's doc comment) must win, not the image's.
        assert_eq!(
            spec["config"][format!("environment.{ENV_ENDPOINT}")],
            Value::String("http://10.88.77.1:8443".to_string())
        );
        // A malformed (no `=`) entry must not panic and must not appear.
        assert!(
            !spec["config"]
                .as_object()
                .unwrap()
                .contains_key("environment.not-a-key-value-pair")
        );
    }

    #[test]
    fn build_instance_spec_omits_jwt_env_var_without_a_token() {
        let sandbox = test_sandbox(); // spec: None, per test_sandbox()
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        assert!(spec["devices"].get(DEVICE_SANDBOX_JWT).is_none());
        assert!(
            spec["config"]
                .get(format!(
                    "environment.{}",
                    openshell_core::sandbox_env::SANDBOX_TOKEN_FILE
                ))
                .is_none()
        );
    }

    #[test]
    fn parse_cpu_allowance_supports_cores_and_millicores() {
        assert_eq!(
            parse_cpu_allowance("250m").unwrap(),
            Some("25ms/100ms".to_string())
        );
        assert_eq!(
            parse_cpu_allowance("2").unwrap(),
            Some("200ms/100ms".to_string())
        );
        assert_eq!(
            parse_cpu_allowance("1.5").unwrap(),
            Some("150ms/100ms".to_string())
        );
        assert_eq!(parse_cpu_allowance("").unwrap(), None);
    }

    #[test]
    fn parse_cpu_allowance_rejects_zero_and_negative_and_malformed() {
        assert!(parse_cpu_allowance("0").is_err());
        assert!(parse_cpu_allowance("-1").is_err());
        assert!(parse_cpu_allowance("not-a-number").is_err());
    }

    #[test]
    fn parse_memory_bytes_supports_binary_and_decimal_quantities() {
        assert_eq!(
            parse_memory_bytes("512Mi").unwrap(),
            Some("536870912B".to_string())
        );
        assert_eq!(
            parse_memory_bytes("1G").unwrap(),
            Some("1000000000B".to_string())
        );
        assert_eq!(
            parse_memory_bytes("2Gi").unwrap(),
            Some("2147483648B".to_string())
        );
        assert_eq!(parse_memory_bytes("").unwrap(), None);
    }

    #[test]
    fn parse_memory_bytes_rejects_zero_negative_and_unknown_suffix() {
        assert!(parse_memory_bytes("0").is_err());
        assert!(parse_memory_bytes("-1Gi").is_err());
        assert!(parse_memory_bytes("12XB").is_err());
    }

    #[test]
    fn lxd_pids_limit_zero_inherits_and_negative_errors() {
        assert_eq!(lxd_pids_limit(0).unwrap(), None);
        assert_eq!(lxd_pids_limit(2048).unwrap(), Some("2048".to_string()));
        assert!(lxd_pids_limit(-1).is_err());
    }

    #[test]
    fn lxd_resource_limits_rejects_cpu_and_memory_requests() {
        use openshell_core::proto::compute::v1::DriverResourceRequirements;

        let with_cpu_request = openshell_core::proto::compute::v1::DriverSandboxTemplate {
            resources: Some(DriverResourceRequirements {
                cpu_request: "500m".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = lxd_resource_limits(Some(&with_cpu_request)).unwrap_err();
        assert!(matches!(err, ComputeDriverError::Precondition(_)));

        let with_memory_request = openshell_core::proto::compute::v1::DriverSandboxTemplate {
            resources: Some(DriverResourceRequirements {
                memory_request: "256Mi".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = lxd_resource_limits(Some(&with_memory_request)).unwrap_err();
        assert!(matches!(err, ComputeDriverError::Precondition(_)));
    }

    #[test]
    fn lxd_resource_limits_applies_cpu_and_memory_limits() {
        use openshell_core::proto::compute::v1::DriverResourceRequirements;

        let template = openshell_core::proto::compute::v1::DriverSandboxTemplate {
            resources: Some(DriverResourceRequirements {
                cpu_limit: "500m".to_string(),
                memory_limit: "2Gi".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let limits = lxd_resource_limits(Some(&template)).expect("limits parse");
        assert_eq!(limits.cpu_allowance, Some("50ms/100ms".to_string()));
        assert_eq!(limits.memory_bytes, Some("2147483648B".to_string()));
    }

    #[test]
    fn lxd_resource_limits_is_empty_without_a_template() {
        assert_eq!(
            lxd_resource_limits(None).unwrap(),
            LxdResourceLimits::default()
        );
    }

    #[test]
    fn build_instance_spec_applies_resource_limits_from_template() {
        use openshell_core::proto::compute::v1::{
            DriverResourceRequirements, DriverSandboxSpec, DriverSandboxTemplate,
        };

        let sandbox = DriverSandbox {
            spec: Some(DriverSandboxSpec {
                template: Some(DriverSandboxTemplate {
                    resources: Some(DriverResourceRequirements {
                        cpu_limit: "2".to_string(),
                        memory_limit: "512Mi".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..test_sandbox()
        };
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            sandbox_pids_limit: 512,
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        assert_eq!(
            spec["config"]["limits.cpu.allowance"],
            Value::String("200ms/100ms".to_string())
        );
        assert_eq!(
            spec["config"]["limits.memory"],
            Value::String("536870912B".to_string())
        );
        assert_eq!(
            spec["config"]["limits.processes"],
            Value::String("512".to_string())
        );
    }

    #[test]
    fn build_instance_spec_omits_resource_limit_keys_by_default() {
        let sandbox = test_sandbox();
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            sandbox_pids_limit: 0,
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        assert!(spec["config"].get("limits.cpu.allowance").is_none());
        assert!(spec["config"].get("limits.memory").is_none());
        assert!(spec["config"].get("limits.processes").is_none());
    }

    #[test]
    fn build_instance_spec_propagates_a_rejected_cpu_request() {
        use openshell_core::proto::compute::v1::{
            DriverResourceRequirements, DriverSandboxSpec, DriverSandboxTemplate,
        };

        let sandbox = DriverSandbox {
            spec: Some(DriverSandboxSpec {
                template: Some(DriverSandboxTemplate {
                    resources: Some(DriverResourceRequirements {
                        cpu_request: "100m".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..test_sandbox()
        };
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let err = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect_err("cpu_request must be rejected");
        assert!(matches!(err, ComputeDriverError::Precondition(_)));
    }

    fn bind_mount_json(source: &str, target: &str, read_only: bool) -> Value {
        json!({
            "type": "bind",
            "source": source,
            "target": target,
            "read_only": read_only,
        })
    }

    fn driver_config_struct(mounts: Vec<Value>) -> prost_types::Struct {
        let value = json!({ "mounts": mounts });
        let Value::Object(object) = value else {
            unreachable!()
        };
        openshell_core::proto_struct::json_object_to_struct(object).expect("valid struct")
    }

    fn sandbox_with_driver_config(driver_config: prost_types::Struct) -> DriverSandbox {
        use openshell_core::proto::compute::v1::{DriverSandboxSpec, DriverSandboxTemplate};
        DriverSandbox {
            spec: Some(DriverSandboxSpec {
                template: Some(DriverSandboxTemplate {
                    driver_config: Some(driver_config),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..test_sandbox()
        }
    }

    #[test]
    fn driver_config_rejects_bind_mounts_unless_enabled() {
        let sandbox = sandbox_with_driver_config(driver_config_struct(vec![bind_mount_json(
            "/host/data",
            "/sandbox/data",
            true,
        )]));
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            enable_bind_mounts: false,
            ..crate::config::LxdComputeConfig::default()
        };
        let err = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect_err("bind mount must be rejected without enable_bind_mounts");
        assert!(err.to_string().contains("enable_bind_mounts"));
    }

    #[test]
    fn driver_config_delivers_an_enabled_bind_mount_as_a_disk_device() {
        let sandbox = sandbox_with_driver_config(driver_config_struct(vec![bind_mount_json(
            "/host/data",
            "/sandbox/data",
            false,
        )]));
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            enable_bind_mounts: true,
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        let device = &spec["devices"]["openshell-mount-0"];
        assert_eq!(device["type"], "disk");
        assert_eq!(device["source"], "/host/data");
        assert_eq!(device["path"], "/sandbox/data");
        assert_eq!(device["readonly"], "false");
        assert_eq!(device["shift"], "true");
    }

    #[test]
    fn driver_config_defaults_bind_mounts_to_read_only() {
        let value = json!({
            "mounts": [{
                "type": "bind",
                "source": "/host/data",
                "target": "/sandbox/data",
            }]
        });
        let Value::Object(object) = value else {
            unreachable!()
        };
        let driver_config =
            openshell_core::proto_struct::json_object_to_struct(object).expect("valid struct");
        let sandbox = sandbox_with_driver_config(driver_config);
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            enable_bind_mounts: true,
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        assert_eq!(spec["devices"]["openshell-mount-0"]["readonly"], "true");
    }

    #[test]
    fn driver_config_rejects_relative_bind_source_when_enabled() {
        let sandbox = sandbox_with_driver_config(driver_config_struct(vec![bind_mount_json(
            "relative/path",
            "/sandbox/data",
            true,
        )]));
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            enable_bind_mounts: true,
            ..crate::config::LxdComputeConfig::default()
        };
        let err = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect_err("relative bind source must be rejected");
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn driver_config_rejects_reserved_mount_targets() {
        let sandbox = sandbox_with_driver_config(driver_config_struct(vec![bind_mount_json(
            "/host/data",
            "/etc/openshell/tls/client",
            true,
        )]));
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            enable_bind_mounts: true,
            ..crate::config::LxdComputeConfig::default()
        };
        let err = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect_err("reserved target must be rejected");
        assert!(err.to_string().contains("/etc/openshell"));
    }

    #[test]
    fn driver_config_rejects_duplicate_mount_targets() {
        let sandbox = sandbox_with_driver_config(driver_config_struct(vec![
            bind_mount_json("/host/a", "/sandbox/data", true),
            bind_mount_json("/host/b", "/sandbox/data", true),
        ]));
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            enable_bind_mounts: true,
            ..crate::config::LxdComputeConfig::default()
        };
        let err = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect_err("duplicate target must be rejected");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn driver_config_rejects_unknown_mount_type() {
        let value = json!({
            "mounts": [{
                "type": "volume",
                "source": "my-volume",
                "target": "/sandbox/data",
            }]
        });
        let Value::Object(object) = value else {
            unreachable!()
        };
        let driver_config =
            openshell_core::proto_struct::json_object_to_struct(object).expect("valid struct");
        let sandbox = sandbox_with_driver_config(driver_config);
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            enable_bind_mounts: true,
            ..crate::config::LxdComputeConfig::default()
        };
        let err = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect_err("unsupported mount type must be rejected");
        assert!(err.to_string().contains("invalid lxd driver_config"));
    }

    #[test]
    fn build_instance_spec_has_no_mount_devices_without_driver_config() {
        let sandbox = test_sandbox();
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        assert!(spec["devices"].get("openshell-mount-0").is_none());
    }

    #[test]
    fn build_instance_spec_omits_tls_devices_when_unconfigured() {
        let sandbox = test_sandbox();
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "http://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        assert!(spec["devices"].get(DEVICE_TLS_CA).is_none());
        assert!(spec["devices"].get(DEVICE_TLS_CERT).is_none());
        assert!(spec["devices"].get(DEVICE_TLS_KEY).is_none());
        assert!(
            spec["config"]
                .get(format!(
                    "environment.{}",
                    openshell_core::sandbox_env::TLS_CA
                ))
                .is_none()
        );
    }

    #[test]
    fn build_instance_spec_delivers_tls_material_via_disk_devices_when_configured() {
        let sandbox = test_sandbox();
        let config = crate::config::LxdComputeConfig {
            default_image: "openshell-sandbox-base".to_string(),
            supervisor_bin: std::path::PathBuf::from("/opt/openshell/bin/openshell-sandbox"),
            guest_tls_ca: Some(std::path::PathBuf::from("/etc/openshell/ca.pem")),
            guest_tls_cert: Some(std::path::PathBuf::from("/etc/openshell/cert.pem")),
            guest_tls_key: Some(std::path::PathBuf::from("/etc/openshell/key.pem")),
            ..crate::config::LxdComputeConfig::default()
        };
        let spec = build_instance_spec(
            &sandbox,
            &config,
            "https://10.88.77.1:8443",
            &config.default_image,
            &[],
        )
        .expect("spec builds");

        assert_eq!(spec["devices"][DEVICE_TLS_CA]["type"], "disk");
        assert_eq!(
            spec["devices"][DEVICE_TLS_CA]["source"],
            "/etc/openshell/ca.pem"
        );
        assert_eq!(
            spec["devices"][DEVICE_TLS_CA]["path"],
            openshell_core::container_paths::TLS_CA_MOUNT_PATH
        );
        assert_eq!(spec["devices"][DEVICE_TLS_CA]["readonly"], "true");
        assert_eq!(spec["devices"][DEVICE_TLS_CA]["shift"], "true");
        assert_eq!(
            spec["devices"][DEVICE_TLS_CERT]["source"],
            "/etc/openshell/cert.pem"
        );
        assert_eq!(
            spec["devices"][DEVICE_TLS_KEY]["source"],
            "/etc/openshell/key.pem"
        );

        assert_eq!(
            spec["config"][format!("environment.{}", openshell_core::sandbox_env::TLS_CA)],
            openshell_core::container_paths::TLS_CA_MOUNT_PATH
        );
        assert_eq!(
            spec["config"][format!("environment.{}", openshell_core::sandbox_env::TLS_CERT)],
            openshell_core::container_paths::TLS_CERT_MOUNT_PATH
        );
        assert_eq!(
            spec["config"][format!("environment.{}", openshell_core::sandbox_env::TLS_KEY)],
            openshell_core::container_paths::TLS_KEY_MOUNT_PATH
        );
    }

    #[test]
    fn driver_condition_maps_running_to_ready_true() {
        let condition = driver_condition_from_status_code(103, "Running");
        assert_eq!(condition.r#type, "Ready");
        assert_eq!(condition.status, "True");
    }

    #[test]
    fn driver_condition_maps_starting_to_provisioning() {
        let condition = driver_condition_from_status_code(100, "Starting");
        assert_eq!(condition.r#type, "Provisioning");
        assert_eq!(condition.status, "False");
    }

    #[test]
    fn driver_condition_maps_error_to_ready_false() {
        let condition = driver_condition_from_status_code(400, "Failure");
        assert_eq!(condition.r#type, "Ready");
        assert_eq!(condition.status, "False");
    }
}
