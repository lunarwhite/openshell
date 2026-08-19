#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Phase 2, Steps 5-8 of crates/openshell-driver-lxd/docs/04-implementation-plan.md: validate
# guest mTLS, resource limits, driver-config bind mounts, and rollback
# hardening against a *real* LXD daemon. As of this script's first version,
# none of Steps 5-8 has been exercised against a real daemon -- unit tests
# against the in-crate stub server proved the logic, but not that a real LXD
# instance actually applies `limits.cpu.allowance`/`limits.memory`, actually
# delivers a bind-mounted disk device, or that mTLS material actually lets
# the supervisor complete a real TLS handshake with the gateway. This script
# is the first thing that does.
#
# WHO RUNS THIS AND WHERE: same constraints as every other script in this
# directory -- run this yourself, from your own terminal, against a real
# Ubuntu/Linux environment (an agent's sandboxed shell tool typically cannot
# reach a Multipass VM's or WSL2 distro's private network).
#
#   WSL2:      wsl -d Ubuntu -- bash crates/openshell-driver-lxd/hack/run-feature-parity.sh
#   Multipass: multipass exec <vm> -- bash /mnt/openshell/crates/openshell-driver-lxd/hack/run-feature-parity.sh
#
# Prerequisites this script assumes already done:
#   - Stage 0/1 (run-vm-tests.sh): LXD installed, initialized, Rust present.
#   - run-managed-driver.sh, at least once: not strictly required, but if
#     $LIFECYCLE_IMAGE's digest is already cached under an `openshell-oci-*`
#     LXD image alias from an earlier run, every create below completes in
#     seconds instead of minutes. Either way this script still passes.
#
# WHAT THIS DOES, IN ORDER:
#   1. Builds openshell-sandbox, openshell-driver-lxd, openshell-gateway, and
#      the CLI natively.
#   2. Generates a real mTLS PKI bundle via `openshell-gateway generate-certs`
#      (the gateway's own built-in bundle generator -- no manual openssl CA
#      setup) with the LXD bridge's own gateway IP added as an extra SAN,
#      since it's not in the tool's own default SAN list.
#   3. Writes a gateway.toml with `compute_drivers = ["lxd"]`, gateway-side
#      mTLS ([openshell.gateway.tls] + [openshell.gateway.mtls_auth]), and a
#      [openshell.drivers.lxd] table with guest_tls_ca/cert/key pointing at
#      the same bundle's client cert, and enable_bind_mounts = true.
#      Mirrors e2e/rust/e2e-vm.sh's own proven mTLS recipe, substituting the
#      LXD bridge IP for VM's `host.openshell.internal`.
#   4. Starts *only* openshell-gateway (it spawns the driver itself, same as
#      run-managed-driver.sh) and registers the CLI against it in mTLS mode.
#      Gateway readiness already proves the CLI's own mTLS handshake works;
#      it does not yet prove the *sandbox's* supervisor can do the same.
#   5. Test A (mTLS end-to-end): a full create -> exec -> delete lifecycle
#      with no extra flags. This is the one that actually proves guest mTLS
#      works -- the supervisor inside the sandbox must complete its own TLS
#      handshake against the gateway's client-CA-validated listener using
#      the certs this driver delivered via disk device, or the sandbox never
#      reaches Ready and the CLI's wait-for-ready step times out.
#   6. Test B (resource limits): create with --cpu/--memory, then read
#      /sys/fs/cgroup/cpu.max and /sys/fs/cgroup/memory.max and assert the
#      *exact* expected cgroup-v2 values -- proving the full pipeline (CLI
#      parse -> DriverResourceRequirements -> limits.cpu.allowance/
#      limits.memory -> LXD's own cgroup application) end to end, not just
#      that the LXD config key got set. Reads via `lxc exec` directly
#      against the resolved LXD instance, not `sandbox exec` -- the
#      sandboxed workload's own Landlock policy denies reads under
#      /sys/fs/cgroup by design (this project's whole point is restricting
#      what a sandboxed process can touch), which a real run caught: the
#      first version of this test read "Permission denied" from inside the
#      sandbox and had no way to tell a real cgroup-application bug apart
#      from its own confinement policy correctly doing its job.
#   7. Test C (driver-config bind mount): prepare a host directory with a
#      seed file, create with --driver-config-json requesting a bind mount,
#      and `sandbox exec` both a read of the seeded file and a write back
#      out, then verify the write actually landed on the host filesystem.
#   8. Test D (rollback on create failure): request a sandbox with an
#      invalid (relative-path) bind mount under the same enable_bind_mounts
#      = true config Test C proved works, so this is a real driver-side
#      validation rejection, not a config gate. Confirms both that the CLI
#      reports a clean failure (not a hang) and that no host-side delivery
#      files (entrypoint script, JWT) leak from the failed attempt -- see
#      cleanup_sandbox_delivery_files in driver.rs. Deliberately does not
#      attempt an "interrupted delete" scenario: unlike interrupted create,
#      there is no way to safely inject a genuine mid-operation delete
#      failure against a real daemon without degrading the daemon itself in
#      a way that would affect every other test and script in this
#      directory. That gap is called out explicitly in the results file
#      rather than faked.
#   9. Sends the gateway a graceful SIGTERM and confirms it reaps the driver
#      child and removes its socket, the same check run-managed-driver.sh
#      already does -- included here too since a Steps 5-8 regression could
#      plausibly break shutdown as a side effect (e.g. a new field added to
#      a struct passed across the shutdown path).
#  10. Writes one consolidated results file under crates/openshell-driver-lxd/hack/results/.
#
# WHAT THIS DELIBERATELY DOES NOT DO / KNOWN SIMPLIFICATIONS:
#   - Interrupted delete (see Test D above) -- a real gap, not a decision to
#     skip lightly.
#   - Resource-limit *enforcement* under contention (e.g. a CPU-bound
#     workload actually getting throttled, a memory-bound one actually
#     getting OOM-killed) -- this script proves the cgroup files reflect the
#     configured limit correctly, which proves the whole translation
#     pipeline is correct, but does not run a workload that would trigger
#     the kernel's own enforcement. That's real cgroup/kernel behavior this
#     driver has no control over and no reason to re-prove independently.
#   - Single-daemon, single-run, debug build, sequential -- same posture
#     every other script in this directory documents.
#
# ENVIRONMENT VARIABLES (all optional):
#   OPENSHELL_LXD_FP_LIFECYCLE_IMAGE  Real sandbox image for every lifecycle
#                                     test (default: ghcr.io/nvidia/
#                                     openshell-community/sandboxes/base:latest)
#   OPENSHELL_LXD_FP_SKIP_BUILD       Set to 1 to reuse already-built binaries

set -euo pipefail
export PATH="/snap/bin:$PATH"

SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
SCRIPT_DIR="$(dirname "$SCRIPT_PATH")"

find_repo_root() {
    local dir="$SCRIPT_DIR"
    while [ "$dir" != "/" ]; do
        if [ -f "$dir/Cargo.toml" ] && grep -q '^\[workspace\]' "$dir/Cargo.toml" 2>/dev/null; then
            echo "$dir"
            return 0
        fi
        dir="$(dirname "$dir")"
    done
    return 1
}

REPO_ROOT="$(find_repo_root || true)"
if [ -z "${REPO_ROOT:-}" ]; then
    echo "ERROR: could not find the workspace root. Run this from inside a checkout of the openshell repo (mounted or copied)." >&2
    exit 1
fi
echo "==> Repository root: $REPO_ROOT"

if [ "$(uname -s)" != "Linux" ]; then
    echo "ERROR: this script must run on Linux (LXD is Linux-only). Detected: $(uname -s)" >&2
    exit 1
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/openshell-lxd-target}"
echo "==> CARGO_TARGET_DIR: $CARGO_TARGET_DIR"

if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo not found even after sourcing ~/.cargo/env. Run Stage 0/1 (run-vm-tests.sh) first, or install Rust manually." >&2
    exit 1
fi
echo "==> cargo: $(cargo --version)"

# ── Config ───────────────────────────────────────────────────────────────────

LIFECYCLE_IMAGE="${OPENSHELL_LXD_FP_LIFECYCLE_IMAGE:-ghcr.io/nvidia/openshell-community/sandboxes/base:latest}"
SKIP_BUILD="${OPENSHELL_LXD_FP_SKIP_BUILD:-0}"
# Same proven network name/subnet run-stage2.sh/run-stage2-oci.sh/
# run-managed-driver.sh already validated end to end.
BRIDGE_SUBNET="10.88.77.1/24"
BRIDGE_IP="${BRIDGE_SUBNET%/*}"

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS_DIR="$REPO_ROOT/crates/openshell-driver-lxd/hack/results"
RUN_DIR="$RESULTS_DIR/feature-parity-$TIMESTAMP"
mkdir -p "$RUN_DIR"
RESULTS_MD="$RESULTS_DIR/feature-parity-$TIMESTAMP.md"

STATE_DIR="$HOME/.cache/openshell-lxd-feature-parity/$TIMESTAMP"
mkdir -p "$STATE_DIR"
JWT_DIR="$STATE_DIR/jwt"
PKI_DIR="$STATE_DIR/pki"
mkdir -p "$JWT_DIR" "$PKI_DIR"

# Deliberately under /tmp with a short, PID-based name, not nested under
# $STATE_DIR -- see run-managed-driver.sh's own comment on why: Unix domain
# socket paths are capped at SUN_LEN (107 bytes on Linux), and a
# $HOME/.cache-nested path plus compute::lxd::compute_driver_socket_path()'s
# fixed "/run/compute-driver.sock" suffix has already blown that budget once.
LXD_DRIVER_STATE_DIR="/tmp/openshell-lxd-fp-$$"
DRIVER_SOCKET_PATH="$LXD_DRIVER_STATE_DIR/run/compute-driver.sock"

# Isolates this run's per-sandbox delivery files (entrypoint script, JWT --
# see instance::entrypoint_script_host_path/sandbox_token_host_path) under
# $STATE_DIR instead of the real ~/.local/state/openshell/lxd/ on this VM.
# Test D's rollback check depends on being able to look at a location this
# script fully controls and can snapshot before/after.
export XDG_STATE_HOME="$STATE_DIR/state"
mkdir -p "$XDG_STATE_HOME"
LXD_DRIVER_SANDBOX_STATE_DIR="$XDG_STATE_HOME/openshell/lxd"

GATEWAY_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("",0)); print(s.getsockname()[1]); s.close()')"
GATEWAY_CONFIG="$STATE_DIR/gateway.toml"
GATEWAY_DB="$STATE_DIR/gateway.db"
GATEWAY_LOG="$RUN_DIR/gateway.log"

export XDG_CONFIG_HOME="$STATE_DIR/config"
export XDG_DATA_HOME="$STATE_DIR/data"
GATEWAY_NAME="openshell-lxd-feature-parity"

echo "==> Testing LXD driver feature parity: mTLS, resource limits, driver-config mounts, rollback"
echo "==> Lifecycle test image: $LIFECYCLE_IMAGE"
echo "==> Bridge IP (also the mTLS server cert's extra SAN): $BRIDGE_IP"
echo "==> Results will be written to:"
echo "      $RESULTS_MD"
echo "      $RUN_DIR/ (raw logs)"

log_section() {
    echo ""
    echo "############################################################"
    echo "# $1"
    echo "############################################################"
}

run_logged() {
    local log_file="$1"
    shift
    set +e
    "$@" >"$log_file" 2>&1
    local status=$?
    set -e
    return "$status"
}

GATEWAY_PID=""
cleanup() {
    local exit_code=$?
    echo ""
    echo "--- cleanup ---"
    if [ -n "$GATEWAY_PID" ] && kill -0 "$GATEWAY_PID" 2>/dev/null; then
        kill "$GATEWAY_PID" 2>/dev/null || true
        wait "$GATEWAY_PID" 2>/dev/null || true
    fi
    local orphan
    for orphan in $(pgrep -f "openshell-driver-lxd.*$LXD_DRIVER_STATE_DIR" 2>/dev/null || true); do
        echo "WARNING: killing orphaned driver process (pid $orphan) left behind" >&2
        kill "$orphan" 2>/dev/null || true
    done
    if [ "$exit_code" -ne 0 ]; then
        echo "NOTE: exiting non-zero ($exit_code); preserving $STATE_DIR, $RUN_DIR, and $LXD_DRIVER_STATE_DIR for debugging."
        if [ -f "$GATEWAY_LOG" ]; then
            echo "=== gateway+driver log (tail; driver output is inherited into this same file) ==="
            tail -n 150 "$GATEWAY_LOG" 2>/dev/null || true
        fi
    fi
}
trap cleanup EXIT

# Resolve an OpenShell sandbox name to the underlying LXD instance name, via
# the same user.openshell.sandbox_name label instance::build_instance_spec
# stamps on every instance it creates (see driver.rs's own label-based
# lookup, mirrored here since this script has no gRPC client of its own).
# Needed for Test B: `sandbox exec` runs *inside* the sandbox's own
# Landlock-confined workload, and reading /sys/fs/cgroup/* from there is
# denied by design (this project's whole point is restricting what a
# sandboxed process can touch) -- `lxc exec` goes directly into the
# container via LXD's own API, bypassing the supervisor and its policy
# entirely, which is the only way to read the *real* cgroup file content.
find_lxd_instance_by_sandbox_name() {
    local target_name="$1"
    local candidate
    for candidate in $(lxc list --format csv -c n 2>/dev/null); do
        if [ "$(lxc config get "$candidate" user.openshell.sandbox_name 2>/dev/null)" = "$target_name" ]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

# ── Step 0: clean up leftover instances from a previous run ────────────────

log_section "Step 0: clean up leftover instances"
for candidate in $(lxc list --format csv -c n 2>/dev/null); do
    if [ -n "$(lxc config get "$candidate" user.openshell.sandbox_id 2>/dev/null)" ]; then
        echo "==> Deleting leftover instance from a previous run: $candidate"
        lxc delete --force "$candidate" 2>/dev/null || true
    fi
done

# ── Step 1: build binaries ───────────────────────────────────────────────────

log_section "Step 1: build binaries"
if [ "$SKIP_BUILD" = "1" ]; then
    echo "==> OPENSHELL_LXD_FP_SKIP_BUILD=1, reusing existing binaries"
else
    if ! dpkg -s libz3-dev >/dev/null 2>&1; then
        echo "==> Installing libz3-dev (needed to link openshell-gateway via openshell-prover)"
        sudo apt-get update -qq
        sudo apt-get install -y --no-install-recommends libz3-dev >/dev/null
    fi

    run_logged "$RUN_DIR/01-build.log" \
        cargo build --manifest-path "$REPO_ROOT/Cargo.toml" \
            -p openshell-sandbox \
            -p openshell-driver-lxd \
            -p openshell-cli \
        || { echo "ERROR: build failed; see $RUN_DIR/01-build.log" >&2; exit 1; }
    run_logged "$RUN_DIR/01b-build-gateway.log" \
        cargo build --manifest-path "$REPO_ROOT/Cargo.toml" \
            -p openshell-server --bin openshell-gateway \
        || { echo "ERROR: gateway build failed; see $RUN_DIR/01b-build-gateway.log" >&2; exit 1; }
fi

SUPERVISOR_BIN="$CARGO_TARGET_DIR/debug/openshell-sandbox"
DRIVER_BIN="$CARGO_TARGET_DIR/debug/openshell-driver-lxd"
GATEWAY_BIN="$CARGO_TARGET_DIR/debug/openshell-gateway"
CLI_BIN="$CARGO_TARGET_DIR/debug/openshell"
for bin in "$SUPERVISOR_BIN" "$DRIVER_BIN" "$GATEWAY_BIN" "$CLI_BIN"; do
    if [ ! -x "$bin" ]; then
        echo "ERROR: expected binary not found at $bin" >&2
        exit 1
    fi
done
echo "==> Binaries ready: $SUPERVISOR_BIN, $DRIVER_BIN, $GATEWAY_BIN, $CLI_BIN"

# ── Step 2: gateway sandbox-JWT signing key ──────────────────────────────────

log_section "Step 2: gateway sandbox-JWT signing key"
(
    umask 077
    openssl genpkey -algorithm Ed25519 -out "$JWT_DIR/signing.pem" >/dev/null 2>&1
)
openssl pkey -in "$JWT_DIR/signing.pem" -pubout -out "$JWT_DIR/public.pem" >/dev/null 2>&1
openssl rand -hex 16 >"$JWT_DIR/kid"
echo "==> JWT signing key generated at $JWT_DIR"

# ── Step 3: generate the mTLS PKI bundle ─────────────────────────────────────
#
# openshell-gateway's own built-in bundle generator (crates/openshell-server/
# src/certgen.rs) -- no manual openssl CA setup. The default SAN list
# (DEFAULT_SERVER_SANS in openshell-bootstrap/src/pki.rs) already covers
# 127.0.0.1, which is all the CLI itself needs; the bridge IP is what the
# *sandbox's* supervisor dials (grpc_endpoint), and it is not in that
# default list, so it must be added explicitly.

log_section "Step 3: generate mTLS PKI bundle (openshell-gateway generate-certs)"
"$GATEWAY_BIN" generate-certs --output-dir "$PKI_DIR" \
    --server-san host.openshell.internal \
    --server-san "$BRIDGE_IP" \
    >"$RUN_DIR/03-generate-certs.log" 2>&1
echo "==> PKI bundle generated at $PKI_DIR"
ls -la "$PKI_DIR" "$PKI_DIR/server" "$PKI_DIR/client" >>"$RUN_DIR/03-generate-certs.log" 2>&1 || true

# ── Step 4: write gateway.toml with mTLS + enable_bind_mounts ───────────────
#
# Mirrors e2e/rust/e2e-vm.sh's proven [openshell.gateway.tls]/
# [openshell.gateway.mtls_auth] recipe exactly, substituting the LXD bridge
# IP for VM's host.openshell.internal alias -- LXD has no gvproxy-style
# host-loopback proxy; the sandbox's supervisor must reach the gateway's
# real bind address directly over the bridge network.

log_section "Step 4: write gateway.toml (mTLS + enable_bind_mounts = true)"
cat >"$GATEWAY_CONFIG" <<EOF
[openshell]
version = 1

[openshell.gateway]
bind_address = "0.0.0.0:${GATEWAY_PORT}"
compute_drivers = ["lxd"]

[openshell.gateway.tls]
cert_path = "${PKI_DIR}/server/tls.crt"
key_path = "${PKI_DIR}/server/tls.key"
client_ca_path = "${PKI_DIR}/ca.crt"

[openshell.gateway.mtls_auth]
enabled = true

[openshell.gateway.gateway_jwt]
signing_key_path = "${JWT_DIR}/signing.pem"
public_key_path = "${JWT_DIR}/public.pem"
kid_path = "${JWT_DIR}/kid"
gateway_id = "${GATEWAY_NAME}"
ttl_secs = 0

[openshell.drivers.lxd]
state_dir            = "${LXD_DRIVER_STATE_DIR}"
driver_dir           = "$(dirname "$DRIVER_BIN")"
supervisor_bin       = "${SUPERVISOR_BIN}"
lxd_socket_path      = "/var/snap/lxd/common/lxd/unix.socket"
network_name         = "openshell"
network_ipv4_subnet  = "${BRIDGE_SUBNET}"
storage_pool         = "default"
grpc_endpoint        = "https://${BRIDGE_IP}:${GATEWAY_PORT}"
guest_tls_ca         = "${PKI_DIR}/ca.crt"
guest_tls_cert       = "${PKI_DIR}/client/tls.crt"
guest_tls_key        = "${PKI_DIR}/client/tls.key"
enable_bind_mounts   = true
EOF
echo "==> Wrote $GATEWAY_CONFIG"
echo "--- [openshell.drivers.lxd] table ---"
sed -n '/\[openshell.drivers.lxd\]/,$p' "$GATEWAY_CONFIG"

# ── Step 5: start the gateway, register the CLI in mTLS mode ───────────────

log_section "Step 5: start openshell-gateway (mTLS) and register the CLI"
"$GATEWAY_BIN" --config "$GATEWAY_CONFIG" \
    --db-url "sqlite:${GATEWAY_DB}?mode=rwc" \
    >"$GATEWAY_LOG" 2>&1 &
GATEWAY_PID=$!
echo "==> Gateway started (pid $GATEWAY_PID)"

CLI_GATEWAY_ENDPOINT="https://127.0.0.1:${GATEWAY_PORT}"
GATEWAY_CONFIG_DIR="$XDG_CONFIG_HOME/openshell/gateways/$GATEWAY_NAME"
mkdir -p "$GATEWAY_CONFIG_DIR/mtls"
cp "$PKI_DIR/ca.crt"         "$GATEWAY_CONFIG_DIR/mtls/ca.crt"
cp "$PKI_DIR/client/tls.crt" "$GATEWAY_CONFIG_DIR/mtls/tls.crt"
cp "$PKI_DIR/client/tls.key" "$GATEWAY_CONFIG_DIR/mtls/tls.key"
cat >"$GATEWAY_CONFIG_DIR/metadata.json" <<EOF
{
  "name": "${GATEWAY_NAME}",
  "gateway_endpoint": "${CLI_GATEWAY_ENDPOINT}",
  "is_remote": false,
  "gateway_port": ${GATEWAY_PORT}
}
EOF
printf '%s' "$GATEWAY_NAME" >"$XDG_CONFIG_HOME/openshell/active_gateway"
export OPENSHELL_GATEWAY_ENDPOINT="$CLI_GATEWAY_ENDPOINT"

elapsed=0
ready=0
last_status=""
while [ "$elapsed" -lt 60 ]; do
    if ! kill -0 "$GATEWAY_PID" 2>/dev/null; then
        echo "ERROR: gateway exited before becoming ready; see $GATEWAY_LOG" >&2
        exit 1
    fi
    if last_status="$("$CLI_BIN" status --output json 2>&1)" \
        && printf '%s\n' "$last_status" | grep -Eq '"status"[[:space:]]*:[[:space:]]*"connected"'; then
        ready=1
        break
    fi
    sleep 2
    elapsed=$((elapsed + 2))
done
if [ "$ready" -ne 1 ]; then
    echo "ERROR: gateway did not become ready after ${elapsed}s (this already implies the CLI's own mTLS handshake is broken). Last status output:" >&2
    echo "$last_status" >&2
    exit 1
fi
echo "==> Gateway ready after ${elapsed}s -- the CLI's own mTLS handshake against [openshell.gateway.tls]/[mtls_auth] succeeded."

# ── Step 6: confirm the driver spawned as a real child process ─────────────

log_section "Step 6: confirm the driver is a real child process of the gateway"
DRIVER_PID="$(pgrep -P "$GATEWAY_PID" -f "openshell-driver-lxd" 2>/dev/null | head -1 || true)"
if [ -z "$DRIVER_PID" ]; then
    echo "ERROR: no openshell-driver-lxd process found parented by gateway pid $GATEWAY_PID" >&2
    exit 1
fi
echo "==> Driver child process confirmed: pid $DRIVER_PID (parent: $GATEWAY_PID)"
if [ ! -S "$DRIVER_SOCKET_PATH" ]; then
    echo "ERROR: expected driver socket not found at $DRIVER_SOCKET_PATH" >&2
    exit 1
fi
echo "==> Driver socket confirmed at $DRIVER_SOCKET_PATH"

# Every sandbox name below is deliberately short -- MAX_ROUTABLE_NAME_LEN
# (crates/openshell-server/src/grpc/mod.rs) caps a sandbox name at 19
# characters (three DNS-routable segments -- workspace, sandbox, service --
# plus two `--` delimiters must fit one 63-char DNS label: 19+2+19+2+19=61).
# This is a real, universal gateway constraint, not LXD-specific -- an
# earlier version of this script used names like "lxd-fp-limits-$$" and
# "lxd-fp-rollback-$$", which came out to 20-22 characters with a 6-digit
# PID and failed CreateSandbox instantly with "name exceeds maximum length"
# before the driver was ever involved. Worse, Test D's own failure-reason
# check below was loose enough that this exact error (which also contains
# the word "invalid") passed as if it were the *intended* mount-validation
# failure -- a real gap in this script's own correctness, not just Test B's
# failure. "lxd-fp-<letter>-$$" leaves headroom even for a 7-digit PID.

# ── Test A: guest mTLS end-to-end ────────────────────────────────────────────
#
# The load-bearing test for Step 5. Gateway readiness above already proved
# the *CLI's* mTLS handshake works; it says nothing about the *sandbox's*
# supervisor, which is a completely separate TLS client using completely
# separate delivered certs (the same client cert/key, but read from a disk
# device inside the container, not from $XDG_CONFIG_HOME). If guest mTLS
# delivery or the supervisor's own TLS client config is broken, this
# sandbox creates its LXD instance successfully (that part doesn't need
# TLS) but never leaves "Requesting compute" -- the CLI's wait-for-ready
# step will time out identically to a dozen other "never becomes Ready"
# causes already debugged in this crate's own history.

log_section "Test A: guest mTLS end-to-end (create -> exec -> delete over https)"
TEST_A_NAME="lxd-fp-a-$$"
TEST_A_OUTCOME="not-run"
TEST_A_START=$(date +%s)
TEST_A_CREATE_OUTPUT=""
if TEST_A_CREATE_OUTPUT="$(timeout 1200 "$CLI_BIN" sandbox create --name "$TEST_A_NAME" --from "$LIFECYCLE_IMAGE" -- echo mtls-create-ok 2>&1)"; then
    echo "$TEST_A_CREATE_OUTPUT" >"$RUN_DIR/testA-create.log"
    TEST_A_EXEC_OUTPUT=""
    if TEST_A_EXEC_OUTPUT="$(timeout 60 "$CLI_BIN" sandbox exec --name "$TEST_A_NAME" -- echo mtls-exec-ok 2>&1)"; then
        echo "$TEST_A_EXEC_OUTPUT" >"$RUN_DIR/testA-exec.log"
        if echo "$TEST_A_EXEC_OUTPUT" | grep -q "mtls-exec-ok"; then
            TEST_A_OUTCOME="pass"
        else
            TEST_A_OUTCOME="exec-ran-but-output-unexpected"
        fi
    else
        echo "$TEST_A_EXEC_OUTPUT" >"$RUN_DIR/testA-exec.log"
        TEST_A_OUTCOME="exec-failed"
    fi
    timeout 60 "$CLI_BIN" sandbox delete "$TEST_A_NAME" >"$RUN_DIR/testA-delete.log" 2>&1 \
        || echo "WARNING: Test A sandbox delete failed; see $RUN_DIR/testA-delete.log" >&2
else
    echo "$TEST_A_CREATE_OUTPUT" >"$RUN_DIR/testA-create.log"
    TEST_A_OUTCOME="create-failed"
    lxc delete --force "$TEST_A_NAME" >/dev/null 2>&1 || true
fi
TEST_A_DURATION=$(( $(date +%s) - TEST_A_START ))
echo "==> Test A outcome: $TEST_A_OUTCOME (${TEST_A_DURATION}s)"

# ── Test B: resource limits end-to-end ──────────────────────────────────────
#
# --cpu 500m -> DriverResourceRequirements.cpu_limit = "500m" ->
# instance::parse_cpu_allowance -> limits.cpu.allowance = "50ms/100ms" ->
# LXD's own cgroup-v2 application -> /sys/fs/cgroup/cpu.max = "50000 100000"
# (LXD's millisecond string, converted to the kernel's microsecond units).
# --memory 256Mi -> memory_limit = "256Mi" -> parse_memory_bytes ->
# limits.memory = "268435456B" -> /sys/fs/cgroup/memory.max = "268435456".
# Checking the *exact* string, not just "some limit is set", is what
# actually proves the whole translation pipeline end to end.

log_section "Test B: resource limits (--cpu 500m --memory 256Mi)"
TEST_B_NAME="lxd-fp-b-$$"
TEST_B_OUTCOME="not-run"
EXPECTED_CPU_MAX="50000 100000"
EXPECTED_MEMORY_MAX="268435456"
TEST_B_CREATE_OUTPUT=""
if TEST_B_CREATE_OUTPUT="$(timeout 120 "$CLI_BIN" sandbox create --name "$TEST_B_NAME" --cpu 500m --memory 256Mi -- echo limits-create-ok 2>&1)"; then
    echo "$TEST_B_CREATE_OUTPUT" >"$RUN_DIR/testB-create.log"
    CPU_MAX_ACTUAL=""
    MEMORY_MAX_ACTUAL=""
    TEST_B_INSTANCE="$(find_lxd_instance_by_sandbox_name "$TEST_B_NAME" || true)"
    if [ -z "$TEST_B_INSTANCE" ]; then
        TEST_B_OUTCOME="lxd-instance-not-found"
        echo "no matching LXD instance found for sandbox name $TEST_B_NAME" >"$RUN_DIR/testB-cgroup-values.log"
    elif CPU_MAX_ACTUAL="$(timeout 30 lxc exec "$TEST_B_INSTANCE" -- cat /sys/fs/cgroup/cpu.max 2>&1)" \
        && MEMORY_MAX_ACTUAL="$(timeout 30 lxc exec "$TEST_B_INSTANCE" -- cat /sys/fs/cgroup/memory.max 2>&1)"; then
        CPU_MAX_ACTUAL="$(echo "$CPU_MAX_ACTUAL" | tr -d '\r' | xargs)"
        MEMORY_MAX_ACTUAL="$(echo "$MEMORY_MAX_ACTUAL" | tr -d '\r' | xargs)"
        {
            echo "LXD instance: $TEST_B_INSTANCE"
            echo "cpu.max: expected='$EXPECTED_CPU_MAX' actual='$CPU_MAX_ACTUAL'"
            echo "memory.max: expected='$EXPECTED_MEMORY_MAX' actual='$MEMORY_MAX_ACTUAL'"
        } >"$RUN_DIR/testB-cgroup-values.log"
        if [ "$CPU_MAX_ACTUAL" = "$EXPECTED_CPU_MAX" ] && [ "$MEMORY_MAX_ACTUAL" = "$EXPECTED_MEMORY_MAX" ]; then
            TEST_B_OUTCOME="pass"
        else
            TEST_B_OUTCOME="cgroup-values-mismatch"
        fi
    else
        TEST_B_OUTCOME="cgroup-read-failed"
        {
            echo "LXD instance: $TEST_B_INSTANCE"
            echo "cpu.max read: $CPU_MAX_ACTUAL"
            echo "memory.max read: $MEMORY_MAX_ACTUAL"
        } >"$RUN_DIR/testB-cgroup-values.log"
    fi
    timeout 60 "$CLI_BIN" sandbox delete "$TEST_B_NAME" >"$RUN_DIR/testB-delete.log" 2>&1 \
        || echo "WARNING: Test B sandbox delete failed; see $RUN_DIR/testB-delete.log" >&2
else
    echo "$TEST_B_CREATE_OUTPUT" >"$RUN_DIR/testB-create.log"
    TEST_B_OUTCOME="create-failed"
    lxc delete --force "$TEST_B_NAME" >/dev/null 2>&1 || true
fi
echo "==> Test B outcome: $TEST_B_OUTCOME"

# ── Test C: driver-config bind mount end-to-end ─────────────────────────────

log_section "Test C: driver-config bind mount"
TEST_C_NAME="lxd-fp-c-$$"
TEST_C_OUTCOME="not-run"
BIND_HOST_DIR="$STATE_DIR/bind-mount-host-dir"
mkdir -p "$BIND_HOST_DIR"
chmod 0777 "$BIND_HOST_DIR"
echo "host-bind-ok" >"$BIND_HOST_DIR/input.txt"
DRIVER_CONFIG_JSON='{"lxd":{"mounts":[{"type":"bind","source":"'"$BIND_HOST_DIR"'","target":"/sandbox/bind-mount","read_only":false}]}}'
TEST_C_CREATE_OUTPUT=""
if TEST_C_CREATE_OUTPUT="$(timeout 120 "$CLI_BIN" sandbox create --name "$TEST_C_NAME" --driver-config-json "$DRIVER_CONFIG_JSON" -- \
        sh -lc 'test "$(cat /sandbox/bind-mount/input.txt)" = host-bind-ok && printf sandbox-bind-ok > /sandbox/bind-mount/output.txt && cat /sandbox/bind-mount/output.txt' 2>&1)"; then
    echo "$TEST_C_CREATE_OUTPUT" >"$RUN_DIR/testC-create.log"
    if echo "$TEST_C_CREATE_OUTPUT" | grep -q "sandbox-bind-ok" \
        && [ -f "$BIND_HOST_DIR/output.txt" ] \
        && [ "$(cat "$BIND_HOST_DIR/output.txt")" = "sandbox-bind-ok" ]; then
        TEST_C_OUTCOME="pass"
    else
        TEST_C_OUTCOME="host-side-write-not-observed"
    fi
    timeout 60 "$CLI_BIN" sandbox delete "$TEST_C_NAME" >"$RUN_DIR/testC-delete.log" 2>&1 \
        || echo "WARNING: Test C sandbox delete failed; see $RUN_DIR/testC-delete.log" >&2
else
    echo "$TEST_C_CREATE_OUTPUT" >"$RUN_DIR/testC-create.log"
    TEST_C_OUTCOME="create-failed"
    lxc delete --force "$TEST_C_NAME" >/dev/null 2>&1 || true
fi
echo "==> Test C outcome: $TEST_C_OUTCOME"

# ── Test D: rollback on a real create failure ───────────────────────────────
#
# A relative bind source fails openshell_core::driver_mounts::
# validate_absolute_mount_source inside build_instance_spec -- *after* the
# entrypoint script and (if a token were present) the JWT are already
# written to $LXD_DRIVER_SANDBOX_STATE_DIR/<sandbox_id>/, but before
# create_instance is ever called. Snapshot the file count in that directory
# before and after: an unchanged count proves cleanup_sandbox_delivery_files
# actually ran, not just that the create failed.

log_section "Test D: rollback on create failure (invalid bind mount source)"
TEST_D_NAME="lxd-fp-d-$$"
FILES_BEFORE="$(find "$LXD_DRIVER_SANDBOX_STATE_DIR" -type f 2>/dev/null | wc -l | tr -d ' ')"
INVALID_DRIVER_CONFIG_JSON='{"lxd":{"mounts":[{"type":"bind","source":"relative/not-absolute","target":"/sandbox/bad-mount","read_only":false}]}}'
TEST_D_CREATE_OUTPUT=""
TEST_D_CREATE_FAILED=0
if TEST_D_CREATE_OUTPUT="$(timeout 60 "$CLI_BIN" sandbox create --name "$TEST_D_NAME" --driver-config-json "$INVALID_DRIVER_CONFIG_JSON" -- echo should-not-run 2>&1)"; then
    echo "$TEST_D_CREATE_OUTPUT" >"$RUN_DIR/testD-create.log"
else
    echo "$TEST_D_CREATE_OUTPUT" >"$RUN_DIR/testD-create.log"
    TEST_D_CREATE_FAILED=1
fi
# Belt-and-suspenders: this create must fail, so no real instance should
# exist -- but if it somehow got created anyway, do not leak it.
lxc delete --force "$TEST_D_NAME" >/dev/null 2>&1 || true

FILES_AFTER="$(find "$LXD_DRIVER_SANDBOX_STATE_DIR" -type f 2>/dev/null | wc -l | tr -d ' ')"
{
    echo "Files under $LXD_DRIVER_SANDBOX_STATE_DIR before: $FILES_BEFORE"
    echo "Files under $LXD_DRIVER_SANDBOX_STATE_DIR after:  $FILES_AFTER"
    find "$LXD_DRIVER_SANDBOX_STATE_DIR" -type f 2>/dev/null || true
} >"$RUN_DIR/testD-state-dir-snapshot.log"

# Deliberately specific, not a loose "invalid" match: this driver's own
# validate_absolute_mount_source (openshell_core::driver_mounts) rejects a
# non-absolute bind source with exactly "bind source must be an absolute
# host path" -- a *generic* "invalid"/"bind" match would also match an
# unrelated failure (e.g. the CreateSandbox-level "name exceeds maximum
# length" error this very script tripped on once already, which also
# contains the word "invalid") and silently report Test D as passing for
# the wrong reason.
if [ "$TEST_D_CREATE_FAILED" -eq 1 ] && echo "$TEST_D_CREATE_OUTPUT" | grep -qi "absolute host path"; then
    if [ "$FILES_AFTER" -eq "$FILES_BEFORE" ]; then
        TEST_D_OUTCOME="pass"
    else
        TEST_D_OUTCOME="create-failed-but-delivery-files-leaked"
    fi
elif [ "$TEST_D_CREATE_FAILED" -eq 1 ]; then
    TEST_D_OUTCOME="create-failed-for-an-unexpected-reason"
else
    TEST_D_OUTCOME="create-unexpectedly-succeeded"
fi
echo "==> Test D outcome: $TEST_D_OUTCOME (files before=$FILES_BEFORE after=$FILES_AFTER)"

# ── Step 7: verify graceful shutdown reaps the managed driver ──────────────

log_section "Step 7: verify gateway shutdown reaps the managed driver subprocess"
DRIVER_ALIVE_BEFORE=0
if kill -0 "$DRIVER_PID" 2>/dev/null; then
    DRIVER_ALIVE_BEFORE=1
fi

kill -TERM "$GATEWAY_PID" 2>/dev/null || true
GATEWAY_EXITED=0
for _ in $(seq 1 30); do
    if ! kill -0 "$GATEWAY_PID" 2>/dev/null; then
        GATEWAY_EXITED=1
        break
    fi
    sleep 1
done
if [ "$GATEWAY_EXITED" -eq 1 ]; then
    echo "==> Gateway exited after SIGTERM"
else
    echo "ERROR: gateway did not exit within 30s of SIGTERM (pid $GATEWAY_PID); see $GATEWAY_LOG" >&2
fi
GATEWAY_PID=""

DRIVER_STILL_RUNNING=0
if kill -0 "$DRIVER_PID" 2>/dev/null; then
    DRIVER_STILL_RUNNING=1
    echo "ERROR: driver pid $DRIVER_PID is STILL RUNNING after gateway shutdown" >&2
    kill "$DRIVER_PID" 2>/dev/null || true
else
    echo "==> Driver pid $DRIVER_PID is no longer running after gateway shutdown"
fi
SOCKET_CLEANED_UP=0
if [ ! -e "$DRIVER_SOCKET_PATH" ]; then
    SOCKET_CLEANED_UP=1
    echo "==> Driver socket removed after shutdown"
else
    echo "WARNING: driver socket still exists after shutdown: $DRIVER_SOCKET_PATH" >&2
fi
SHUTDOWN_CLEAN=0
if [ "$GATEWAY_EXITED" -eq 1 ] && [ "$DRIVER_ALIVE_BEFORE" -eq 1 ] && [ "$DRIVER_STILL_RUNNING" -eq 0 ] && [ "$SOCKET_CLEANED_UP" -eq 1 ]; then
    SHUTDOWN_CLEAN=1
fi

# ── Results file ─────────────────────────────────────────────────────────────

OVERALL_OUTCOME="fail"
if [ "$TEST_A_OUTCOME" = "pass" ] && [ "$TEST_B_OUTCOME" = "pass" ] && [ "$TEST_C_OUTCOME" = "pass" ] \
    && [ "$TEST_D_OUTCOME" = "pass" ] && [ "$SHUTDOWN_CLEAN" -eq 1 ]; then
    OVERALL_OUTCOME="pass"
fi

{
    echo "# LXD feature-parity (Phase 2 Steps 5-8) real-daemon run: $TIMESTAMP"
    echo ""
    echo "Produced by \`crates/openshell-driver-lxd/hack/run-feature-parity.sh\`."
    echo "Raw logs in \`results/feature-parity-$TIMESTAMP/\`."
    echo ""
    echo "## Config"
    echo ""
    echo '```'
    echo "Lifecycle test image: $LIFECYCLE_IMAGE"
    echo "Bridge subnet:        $BRIDGE_SUBNET"
    echo "Gateway port:         $GATEWAY_PORT (mTLS enabled)"
    echo '```'
    echo ""
    echo "## Outcome: \`$OVERALL_OUTCOME\`"
    echo ""
    echo "| Test | Result |"
    echo "|---|---|"
    echo "| A: guest mTLS end-to-end (create -> exec -> delete over https) | $TEST_A_OUTCOME (${TEST_A_DURATION}s) |"
    echo "| B: resource limits (cgroup cpu.max/memory.max exact match) | $TEST_B_OUTCOME |"
    echo "| C: driver-config bind mount (read + write round trip) | $TEST_C_OUTCOME |"
    echo "| D: rollback on create failure (no leaked delivery files) | $TEST_D_OUTCOME |"
    echo "| Graceful shutdown still reaps the managed driver | $([ "$SHUTDOWN_CLEAN" -eq 1 ] && echo yes || echo NO) |"
    echo ""
    echo "## Known gap"
    echo ""
    echo "Interrupted **delete** (as opposed to interrupted create, covered by"
    echo "Test D) is not exercised here -- there is no safe way to inject a"
    echo "genuine mid-operation LXD delete failure against a real daemon without"
    echo "degrading the daemon itself in a way that would affect every other"
    echo "script in this directory. Remains unit-test-only coverage"
    echo "(\`delete_sandbox_propagates_a_genuine_delete_failure_rather_than_"
    echo "swallowing_it\` in \`driver.rs\`)."
    echo ""
    echo "## Logs"
    echo ""
    echo "- \`01-build.log\`, \`01b-build-gateway.log\` -- cargo build (if not skipped)"
    echo "- \`03-generate-certs.log\` -- mTLS PKI bundle generation"
    echo "- \`gateway.log\` -- gateway *and* driver output"
    echo "- \`testA-*\`, \`testB-*\`, \`testC-*\`, \`testD-*\` -- per-test create/exec/delete logs"
} >"$RESULTS_MD"
echo "==> Results written to $RESULTS_MD"

log_section "Done"
echo "Outcome: $OVERALL_OUTCOME"
echo "Test A (mTLS):        $TEST_A_OUTCOME"
echo "Test B (limits):      $TEST_B_OUTCOME"
echo "Test C (mounts):      $TEST_C_OUTCOME"
echo "Test D (rollback):    $TEST_D_OUTCOME"
echo "Graceful shutdown:    $([ "$SHUTDOWN_CLEAN" -eq 1 ] && echo yes || echo NO)"
if [ "$OVERALL_OUTCOME" != "pass" ]; then
    exit 1
fi
exit 0
