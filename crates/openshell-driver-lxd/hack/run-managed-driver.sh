#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Phase 2, Steps 3-4 of crates/openshell-driver-lxd/docs/04-implementation-plan.md: validate
# that the gateway itself can spawn, connect to, and cleanly tear down the
# LXD compute driver as a *managed* subprocess -- the `compute::lxd` module
# added to `openshell-server` (mirroring `compute::vm`'s shape).
#
# This is deliberately NOT run-stage2.sh/run-stage2-oci.sh again. Both of
# those scripts start `openshell-driver-lxd` themselves, by hand, as an
# *unmanaged extension driver* the gateway only ever dials via
# `[openshell.drivers.lxd].socket_path` -- correct for proving the driver's
# own lifecycle/image-conversion logic, but it never once exercises
# `compute::lxd::spawn()`, the gateway's own binary-resolution
# (`resolve_compute_driver_bin`/`resolve_supervisor_bin`), its private
# state/socket-directory hardening
# (`compute::managed_driver_hardening`), or `ManagedDriverProcess`'s
# graceful-shutdown/cleanup path. This script's whole point is that code,
# specifically: the gateway process is started with only
# `compute_drivers = ["lxd"]` configured (the same config shape a real
# `compute_drivers = ["lxd"]` deployment would use) and must launch the
# driver *itself*.
#
# WHO RUNS THIS AND WHERE: same constraints as run-stage2.sh/
# run-stage2-oci.sh -- run this yourself, from your own terminal, against a
# real Ubuntu/Linux environment (an agent's sandboxed shell tool typically
# cannot reach a Multipass VM's or WSL2 distro's private network).
#
#   WSL2:      wsl -d Ubuntu -- bash crates/openshell-driver-lxd/hack/run-managed-driver.sh
#   Multipass: multipass exec <vm> -- bash /mnt/openshell/crates/openshell-driver-lxd/hack/run-managed-driver.sh
#
# Prerequisites this script assumes already done:
#   - Stage 0/1 (run-vm-tests.sh): LXD installed, initialized (storage pool
#     + a network), Rust toolchain present.
#   - Stage 2 (run-stage2-oci.sh), at least once: not strictly required,
#     but if $LIFECYCLE_IMAGE's digest is already cached under an
#     `openshell-oci-*` LXD image alias from an earlier run, Step 6 below
#     completes in seconds instead of several minutes. Either way this
#     script still passes; it's a speed difference, not a correctness one.
#
# WHAT THIS DOES, IN ORDER:
#   1. Builds openshell-sandbox, openshell-driver-lxd, openshell-gateway,
#      and the openshell CLI natively.
#   2. Writes a gateway.toml with `compute_drivers = ["lxd"]` and a full
#      `[openshell.drivers.lxd]` table (state_dir, driver_dir,
#      supervisor_bin, lxd_socket_path, network settings) -- no
#      `socket_path` key at all, since `lxd` is now a reserved built-in
#      driver name and `resolve_configured_compute_driver` rejects a
#      socket-endpoint override for one of those outright.
#   3. Starts *only* openshell-gateway. Does not touch
#      openshell-driver-lxd directly at any point.
#   4. Confirms the gateway actually spawned a driver child process (via
#      `pgrep -P <gateway pid>`) and that its own compute-driver socket
#      came up at the path `compute::lxd::compute_driver_socket_path`
#      would compute from this run's `state_dir`.
#   5. Runs a full `sandbox create -> exec -> delete` lifecycle through
#      the real CLI, using the same real sandbox image
#      run-stage2-oci.sh's Test B/C use -- proving the driver behaves
#      identically once gateway-managed, not just that it starts.
#   6. Sends the gateway a graceful SIGTERM (not SIGKILL) and confirms:
#      the gateway itself exits within a bounded window, the driver child
#      process it spawned is *also* gone afterward (not orphaned), and its
#      socket file was removed -- directly exercising
#      `ManagedDriverProcess::shutdown()`'s SIGTERM/wait/SIGKILL-escalation
#      path and its `Drop` backstop.
#   7. Writes one consolidated results file, mirroring run-stage2-oci.sh's
#      convention, under crates/openshell-driver-lxd/hack/results/.
#
# WHAT THIS DELIBERATELY DOES NOT DO / KNOWN SIMPLIFICATIONS:
#   - No TLS/mTLS -- same simplification run-stage2.sh documents. The LXD
#     driver has no guest-mTLS support yet either way (Phase 2 Step 5).
#   - Does not test driver-*crash* recovery (kill -9 the driver child
#     mid-lifecycle and confirm the gateway notices) -- that's closer to
#     Phase 2 Step 8 (rollback/reconciliation hardening) than Steps 3-4's
#     own scope of "does the spawn/wire-up/shutdown path work at all."
#   - Single-daemon, single-run, debug build, sequential (not concurrent
#     with run-stage2*.sh) -- same posture those scripts document.
#   - Unlike run-stage2-oci.sh, there is no separate driver.log this time:
#     `compute::lxd::spawn()` sets the driver child's stdout/stderr to
#     Stdio::inherit(), so the driver's own tracing output lands mixed
#     into $GATEWAY_LOG (inherited from whatever the gateway process's
#     own stdout/stderr already is) -- this is inherent to how a real
#     managed subprocess actually behaves, not a diagnostics gap to fix.
#
# ENVIRONMENT VARIABLES (all optional):
#   OPENSHELL_LXD_MANAGED_LIFECYCLE_IMAGE  Real sandbox image for the
#                                          lifecycle test (default:
#                                          ghcr.io/nvidia/openshell-
#                                          community/sandboxes/base:latest)
#   OPENSHELL_LXD_MANAGED_SKIP_BUILD       Set to 1 to reuse already-built
#                                          binaries instead of rebuilding

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

LIFECYCLE_IMAGE="${OPENSHELL_LXD_MANAGED_LIFECYCLE_IMAGE:-ghcr.io/nvidia/openshell-community/sandboxes/base:latest}"
SKIP_BUILD="${OPENSHELL_LXD_MANAGED_SKIP_BUILD:-0}"
# Same network name/subnet run-stage2.sh/run-stage2-oci.sh already
# validated end to end -- see run-stage2-oci.sh's own comment on why a
# freshly invented name is a real, previously-hit failure mode, not a
# theoretical one.
BRIDGE_SUBNET="10.88.77.1/24"

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS_DIR="$REPO_ROOT/crates/openshell-driver-lxd/hack/results"
RUN_DIR="$RESULTS_DIR/managed-driver-$TIMESTAMP"
mkdir -p "$RUN_DIR"
RESULTS_MD="$RESULTS_DIR/managed-driver-$TIMESTAMP.md"

STATE_DIR="$HOME/.cache/openshell-lxd-managed-driver/$TIMESTAMP"
mkdir -p "$STATE_DIR"
JWT_DIR="$STATE_DIR/jwt"
mkdir -p "$JWT_DIR"
# The *driver's own* gateway-side state dir -- distinct from $STATE_DIR
# (this script's own scratch space) and from the LXD daemon's own state.
# Passed explicitly as [openshell.drivers.lxd].state_dir so this script
# knows exactly where to look for the driver's compute-driver socket
# without needing to parse gateway.toml back out.
#
# Deliberately under /tmp with a short, PID-based name -- not nested
# under $STATE_DIR ($HOME/.cache/openshell-lxd-managed-driver/<UTC
# timestamp>/...) the way an earlier version of this script did. Unix
# domain socket paths are capped at sizeof(sockaddr_un.sun_path) - 1 (107
# bytes on Linux), and compute::lxd::compute_driver_socket_path() always
# appends a fixed "/run/compute-driver.sock" suffix -- that nested,
# $HOME-rooted path plus that suffix came out to exactly 108 characters,
# one over the limit, and failed with "path must be shorter than
# SUN_LEN" the first time this script ran for real. /tmp plus a PID
# (the same uniqueness convention run-stage2-oci.sh's own sandbox names
# already use, e.g. "lxd-oci-a-$$") leaves a wide margin instead.
LXD_DRIVER_STATE_DIR="/tmp/openshell-lxd-managed-$$"
DRIVER_SOCKET_PATH="$LXD_DRIVER_STATE_DIR/run/compute-driver.sock"

GATEWAY_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("",0)); print(s.getsockname()[1]); s.close()')"
GATEWAY_CONFIG="$STATE_DIR/gateway.toml"
GATEWAY_DB="$STATE_DIR/gateway.db"
GATEWAY_LOG="$RUN_DIR/gateway.log"

export XDG_CONFIG_HOME="$STATE_DIR/config"
export XDG_DATA_HOME="$STATE_DIR/data"
GATEWAY_NAME="openshell-lxd-managed-driver"

echo "==> Testing gateway-managed driver spawn (compute::lxd::spawn)"
echo "==> Lifecycle test image: $LIFECYCLE_IMAGE"
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
    # Belt-and-suspenders: if the gateway died without cleanly reaping its
    # own managed child (exactly the failure mode Step 6 below is
    # checking for), don't leave an orphaned driver process running past
    # this script's own lifetime.
    local orphan
    for orphan in $(pgrep -f "openshell-driver-lxd.*$LXD_DRIVER_STATE_DIR" 2>/dev/null || true); do
        echo "WARNING: killing orphaned driver process (pid $orphan) left behind" >&2
        kill "$orphan" 2>/dev/null || true
    done
    if [ "$exit_code" -ne 0 ]; then
        echo "NOTE: exiting non-zero ($exit_code); preserving $STATE_DIR, $RUN_DIR, and $LXD_DRIVER_STATE_DIR for debugging."
        if [ -f "$GATEWAY_LOG" ]; then
            echo "=== gateway+driver log (tail; driver output is inherited into this same file) ==="
            tail -n 120 "$GATEWAY_LOG" 2>/dev/null || true
        fi
    fi
}
trap cleanup EXIT

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
    echo "==> OPENSHELL_LXD_MANAGED_SKIP_BUILD=1, reusing existing binaries"
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

# ── Step 2: sandbox-JWT signing key ──────────────────────────────────────────

log_section "Step 2: gateway sandbox-JWT signing key"
(
    umask 077
    openssl genpkey -algorithm Ed25519 -out "$JWT_DIR/signing.pem" >/dev/null 2>&1
)
openssl pkey -in "$JWT_DIR/signing.pem" -pubout -out "$JWT_DIR/public.pem" >/dev/null 2>&1
openssl rand -hex 16 >"$JWT_DIR/kid"
echo "==> JWT signing key generated at $JWT_DIR"

# ── Step 3: write gateway.toml with compute_drivers = ["lxd"] ──────────────
#
# This is the load-bearing difference from run-stage2.sh/run-stage2-oci.sh:
# no [openshell.drivers.lxd].socket_path (that's the *unmanaged extension
# driver* shape, and would in fact be rejected outright now -- "lxd" is a
# reserved built-in ComputeDriverKind, and resolve_configured_compute_driver
# refuses a socket-endpoint override for any reserved name). Instead, the
# full driver config a real `compute_drivers = ["lxd"]` deployment would
# write, and the gateway resolves+spawns+connects to the driver entirely on
# its own via compute::lxd::spawn().
#
# driver_dir/supervisor_bin are set explicitly here (the production-
# realistic shape) rather than relying on the sibling-of-gateway-executable
# fallback -- both binaries happen to already live in the same
# $CARGO_TARGET_DIR/debug directory as the gateway binary in this dev
# setup, which would make the fallback path succeed too, but a real
# deployment's driver_dir is exactly this kind of explicit libexec-style
# path, so exercising that config surface directly is more representative
# than accidentally only ever testing the fallback.

log_section "Step 3: write gateway.toml (compute_drivers = [\"lxd\"], managed)"
cat >"$GATEWAY_CONFIG" <<EOF
[openshell]
version = 1

[openshell.gateway]
bind_address = "0.0.0.0:${GATEWAY_PORT}"
compute_drivers = ["lxd"]

[openshell.gateway.auth]
allow_unauthenticated_users = true

[openshell.gateway.gateway_jwt]
signing_key_path = "${JWT_DIR}/signing.pem"
public_key_path = "${JWT_DIR}/public.pem"
kid_path = "${JWT_DIR}/kid"
gateway_id = "${GATEWAY_NAME}"
ttl_secs = 0

[openshell.drivers.lxd]
state_dir           = "${LXD_DRIVER_STATE_DIR}"
driver_dir          = "$(dirname "$DRIVER_BIN")"
supervisor_bin       = "${SUPERVISOR_BIN}"
lxd_socket_path      = "/var/snap/lxd/common/lxd/unix.socket"
network_name         = "openshell"
network_ipv4_subnet  = "${BRIDGE_SUBNET}"
storage_pool         = "default"
EOF
echo "==> Wrote $GATEWAY_CONFIG"
echo "--- [openshell.drivers.lxd] table ---"
sed -n '/\[openshell.drivers.lxd\]/,$p' "$GATEWAY_CONFIG"

# ── Step 4: start *only* the gateway ─────────────────────────────────────────
#
# No manual openshell-driver-lxd invocation anywhere in this script --
# that's the entire point. If this section needs to change to make the
# driver come up, that's a real Steps 3-4 bug, not a script workaround to
# paper over.

log_section "Step 4: start openshell-gateway (it must spawn the LXD driver itself)"
"$GATEWAY_BIN" --config "$GATEWAY_CONFIG" --disable-tls \
    --db-url "sqlite:${GATEWAY_DB}?mode=rwc" \
    >"$GATEWAY_LOG" 2>&1 &
GATEWAY_PID=$!
echo "==> Gateway started (pid $GATEWAY_PID)"

# ── Step 5: register the gateway with the CLI, wait for readiness ──────────

log_section "Step 5: register gateway with CLI, wait for readiness"
GATEWAY_ENDPOINT="http://127.0.0.1:${GATEWAY_PORT}"
GATEWAY_CONFIG_DIR="$XDG_CONFIG_HOME/openshell/gateways/$GATEWAY_NAME"
mkdir -p "$GATEWAY_CONFIG_DIR"
cat >"$GATEWAY_CONFIG_DIR/metadata.json" <<EOF
{
  "name": "${GATEWAY_NAME}",
  "gateway_endpoint": "${GATEWAY_ENDPOINT}",
  "is_remote": false,
  "gateway_port": ${GATEWAY_PORT},
  "auth_mode": "plaintext"
}
EOF
printf '%s' "$GATEWAY_NAME" >"$XDG_CONFIG_HOME/openshell/active_gateway"
export OPENSHELL_GATEWAY_ENDPOINT="$GATEWAY_ENDPOINT"

elapsed=0
ready=0
last_status=""
while [ "$elapsed" -lt 60 ]; do
    if ! kill -0 "$GATEWAY_PID" 2>/dev/null; then
        echo "ERROR: gateway exited before becoming ready -- likely a driver spawn failure; see $GATEWAY_LOG" >&2
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
    echo "ERROR: gateway did not become ready after ${elapsed}s. Last status output:" >&2
    echo "$last_status" >&2
    exit 1
fi
echo "==> Gateway ready after ${elapsed}s (its own driver.initialize span in the log confirms the spawned driver answered GetCapabilities)."

# ── Step 6: confirm the gateway actually spawned a driver child process ────
#
# "The gateway became ready" already implies this indirectly (readiness
# requires build_compute_runtime to have succeeded, which requires
# compute::lxd::spawn() to have returned an established gRPC channel) --
# but confirm it *directly and observably* too: a real child process,
# parented by the gateway, and a real socket at the exact path
# compute::lxd::compute_driver_socket_path() computes.

log_section "Step 6: confirm the driver is a real child process of the gateway"
DRIVER_PID="$(pgrep -P "$GATEWAY_PID" -f "openshell-driver-lxd" 2>/dev/null | head -1 || true)"
if [ -z "$DRIVER_PID" ]; then
    echo "ERROR: no openshell-driver-lxd process found parented by gateway pid $GATEWAY_PID" >&2
    echo "--- all children of $GATEWAY_PID ---" >&2
    pgrep -P "$GATEWAY_PID" -l 2>/dev/null >&2 || true
    exit 1
fi
echo "==> Driver child process confirmed: pid $DRIVER_PID (parent: $GATEWAY_PID)"
if [ ! -S "$DRIVER_SOCKET_PATH" ]; then
    echo "ERROR: expected driver socket not found at $DRIVER_SOCKET_PATH" >&2
    exit 1
fi
echo "==> Driver socket confirmed at $DRIVER_SOCKET_PATH"
SOCKET_MODE_DIR="$(stat -c '%a' "$(dirname "$DRIVER_SOCKET_PATH")" 2>/dev/null || echo unknown)"
echo "==> Socket parent dir mode: $SOCKET_MODE_DIR (managed_driver_hardening should have restricted this to 700)"

# ── Step 7: full lifecycle through the gateway-managed driver ──────────────
#
# Same real sandbox image run-stage2-oci.sh's Test B/C use. If its digest
# is already cached from an earlier run-stage2-oci.sh run, this is a fast
# cache hit; if not, a slower cache-miss conversion -- either way, this
# proves the driver behaves identically whether launched by hand or by the
# gateway itself, which is the actual thing under test here (not image
# conversion timing again, already covered by run-stage2-oci.sh).

log_section "Step 7: full lifecycle (create -> exec -> delete) via the managed driver"
LIFECYCLE_NAME="lxd-managed-$$"
LIFECYCLE_OUTCOME="not-run"
LIFECYCLE_START=$(date +%s)
LIFECYCLE_CREATE_OUTPUT=""
if LIFECYCLE_CREATE_OUTPUT="$(timeout 1200 "$CLI_BIN" sandbox create --name "$LIFECYCLE_NAME" --from "$LIFECYCLE_IMAGE" -- echo managed-driver-ok 2>&1)"; then
    echo "$LIFECYCLE_CREATE_OUTPUT" >"$RUN_DIR/07a-lifecycle-create.log"
    LIFECYCLE_EXEC_OUTPUT=""
    if LIFECYCLE_EXEC_OUTPUT="$(timeout 60 "$CLI_BIN" sandbox exec --name "$LIFECYCLE_NAME" -- echo managed-driver-exec-ok 2>&1)"; then
        echo "$LIFECYCLE_EXEC_OUTPUT" >"$RUN_DIR/07b-lifecycle-exec.log"
        if echo "$LIFECYCLE_EXEC_OUTPUT" | grep -q "managed-driver-exec-ok"; then
            LIFECYCLE_OUTCOME="pass"
        else
            LIFECYCLE_OUTCOME="exec-ran-but-output-unexpected"
        fi
    else
        echo "$LIFECYCLE_EXEC_OUTPUT" >"$RUN_DIR/07b-lifecycle-exec.log"
        LIFECYCLE_OUTCOME="exec-failed"
    fi
    timeout 60 "$CLI_BIN" sandbox delete "$LIFECYCLE_NAME" >"$RUN_DIR/07c-lifecycle-delete.log" 2>&1 \
        || echo "WARNING: lifecycle sandbox delete failed; see $RUN_DIR/07c-lifecycle-delete.log" >&2
else
    echo "$LIFECYCLE_CREATE_OUTPUT" >"$RUN_DIR/07a-lifecycle-create.log"
    LIFECYCLE_OUTCOME="create-failed"
    lxc delete --force "$LIFECYCLE_NAME" >/dev/null 2>&1 || true
fi
LIFECYCLE_END=$(date +%s)
LIFECYCLE_DURATION=$((LIFECYCLE_END - LIFECYCLE_START))
echo "==> Lifecycle outcome: $LIFECYCLE_OUTCOME (${LIFECYCLE_DURATION}s)"

# ── Step 8: verify graceful shutdown reaps the managed driver ──────────────
#
# The load-bearing assertion for Steps 3-4's "managed" half: a SIGTERM to
# the gateway (not SIGKILL -- this must go through ManagedDriverProcess::
# shutdown()'s graceful path, not just whatever Drop does on an abrupt
# kill) must result in the driver child process actually exiting too, not
# becoming an orphan, and the driver's own socket file being removed.

log_section "Step 8: verify gateway shutdown reaps the managed driver subprocess"
DRIVER_ALIVE_BEFORE=0
if kill -0 "$DRIVER_PID" 2>/dev/null; then
    DRIVER_ALIVE_BEFORE=1
fi
echo "==> Driver pid $DRIVER_PID alive before shutdown: $([ "$DRIVER_ALIVE_BEFORE" = "1" ] && echo yes || echo no)"

kill -TERM "$GATEWAY_PID" 2>/dev/null || true
SHUTDOWN_START=$(date +%s)
GATEWAY_EXITED=0
for _ in $(seq 1 30); do
    if ! kill -0 "$GATEWAY_PID" 2>/dev/null; then
        GATEWAY_EXITED=1
        break
    fi
    sleep 1
done
SHUTDOWN_DURATION=$(($(date +%s) - SHUTDOWN_START))
if [ "$GATEWAY_EXITED" -eq 1 ]; then
    echo "==> Gateway exited ${SHUTDOWN_DURATION}s after SIGTERM"
else
    echo "ERROR: gateway did not exit within 30s of SIGTERM (pid $GATEWAY_PID); see $GATEWAY_LOG" >&2
fi
# Tell the exit trap the gateway is already handled either way, so it
# doesn't try to signal an already-reaped (or hung) process again.
GATEWAY_PID=""

DRIVER_STILL_RUNNING=0
if kill -0 "$DRIVER_PID" 2>/dev/null; then
    DRIVER_STILL_RUNNING=1
    echo "ERROR: driver pid $DRIVER_PID is STILL RUNNING after gateway shutdown -- ManagedDriverProcess cleanup did not reap it" >&2
    kill "$DRIVER_PID" 2>/dev/null || true
else
    echo "==> Driver pid $DRIVER_PID is no longer running after gateway shutdown -- managed cleanup worked"
fi
SOCKET_CLEANED_UP=0
if [ ! -e "$DRIVER_SOCKET_PATH" ]; then
    SOCKET_CLEANED_UP=1
    echo "==> Driver socket removed after shutdown"
else
    echo "WARNING: driver socket still exists after shutdown: $DRIVER_SOCKET_PATH" >&2
fi

# ── Results file ─────────────────────────────────────────────────────────────

SHUTDOWN_CLEAN=0
if [ "$GATEWAY_EXITED" -eq 1 ] && [ "$DRIVER_ALIVE_BEFORE" -eq 1 ] && [ "$DRIVER_STILL_RUNNING" -eq 0 ] && [ "$SOCKET_CLEANED_UP" -eq 1 ]; then
    SHUTDOWN_CLEAN=1
fi

OVERALL_OUTCOME="fail"
if [ -n "$DRIVER_PID" ] && [ "$LIFECYCLE_OUTCOME" = "pass" ] && [ "$SHUTDOWN_CLEAN" -eq 1 ]; then
    OVERALL_OUTCOME="pass"
fi

{
    echo "# LXD gateway-managed driver spawn run: $TIMESTAMP"
    echo ""
    echo "Produced by \`crates/openshell-driver-lxd/hack/run-managed-driver.sh\`."
    echo "Raw logs in \`results/managed-driver-$TIMESTAMP/\`."
    echo ""
    echo "## Config"
    echo ""
    echo '```'
    echo "Lifecycle test image: $LIFECYCLE_IMAGE"
    echo "Bridge subnet:        $BRIDGE_SUBNET"
    echo "Gateway port:         $GATEWAY_PORT"
    echo "Driver spawned by:    the gateway itself (compute::lxd::spawn), not this script"
    echo '```'
    echo ""
    echo "## Outcome: \`$OVERALL_OUTCOME\`"
    echo ""
    echo "| Check | Result |"
    echo "|---|---|"
    echo "| Driver spawned as a real child process of the gateway | $([ -n "$DRIVER_PID" ] && echo "yes (pid $DRIVER_PID)" || echo "no") |"
    if [ -S "$DRIVER_SOCKET_PATH" ]; then
        SOCKET_PRESENT_DESC="yes"
    elif [ "$SOCKET_CLEANED_UP" -eq 1 ]; then
        SOCKET_PRESENT_DESC="yes (removed after shutdown, as expected)"
    else
        SOCKET_PRESENT_DESC="no"
    fi
    echo "| Driver socket present at the expected path | $SOCKET_PRESENT_DESC |"
    echo "| Full lifecycle (create -> exec -> delete) | $LIFECYCLE_OUTCOME (${LIFECYCLE_DURATION}s) |"
    echo "| Graceful SIGTERM reaps the driver child | $([ "$DRIVER_STILL_RUNNING" -eq 0 ] && echo yes || echo NO) |"
    echo "| Driver socket removed after shutdown | $([ "$SOCKET_CLEANED_UP" -eq 1 ] && echo yes || echo NO) |"
    echo "| Gateway itself exited within 30s of SIGTERM | $([ "$GATEWAY_EXITED" -eq 1 ] && echo "yes (${SHUTDOWN_DURATION}s)" || echo NO) |"
    echo ""
    echo "## Logs"
    echo ""
    echo "- \`01-build.log\`, \`01b-build-gateway.log\` -- cargo build (if not skipped)"
    echo "- \`gateway.log\` -- gateway *and* driver output (the driver's stdout/stderr"
    echo "  is inherited from the gateway process that spawned it, so there is no"
    echo "  separate driver.log this time -- see this script's own header comment)"
    echo "- \`07a\`-\`07c\` -- the full create/exec/delete lifecycle"
} >"$RESULTS_MD"
echo "==> Results written to $RESULTS_MD"

log_section "Done"
echo "Outcome: $OVERALL_OUTCOME"
echo "Driver spawned as gateway child:  $([ -n "$DRIVER_PID" ] && echo "yes (pid $DRIVER_PID)" || echo no)"
echo "Full lifecycle:                   $LIFECYCLE_OUTCOME (${LIFECYCLE_DURATION}s)"
echo "Graceful shutdown reaped driver:  $([ "$DRIVER_STILL_RUNNING" -eq 0 ] && echo yes || echo NO)"
echo "Driver socket cleaned up:         $([ "$SOCKET_CLEANED_UP" -eq 1 ] && echo yes || echo NO)"
if [ "$OVERALL_OUTCOME" != "pass" ]; then
    exit 1
fi
exit 0
