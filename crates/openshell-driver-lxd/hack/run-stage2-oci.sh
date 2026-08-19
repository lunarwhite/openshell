#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Phase 2, Step 2 of crates/openshell-driver-lxd/docs/04-implementation-plan.md: validate
# the OCI-to-LXD image conversion pipeline (crates/openshell-driver-lxd/
# src/image.rs) against a real registry and a real LXD daemon.
#
# This is deliberately NOT run-stage2.sh's "oci" mode again. That mode
# shells out to skopeo+umoci by hand, outside the driver, as a one-time
# manual conversion done *before* the driver ever starts — it was a
# de-risking shortcut for validating driver/gateway/lifecycle plumbing
# without also betting on an unproven image pipeline in the same step
# (see run-stage2.sh's own header). That pipeline now exists for real,
# inside the driver, wired into CreateSandbox itself (Phase 2, Step 1).
# This script's whole point is to exercise *that* code path, live,
# through the actual CLI's `--from` (BYOC) flag — the same flag Docker
# and Podman sandboxes already use to accept arbitrary OCI images. Only
# `--from` and this script's own diagnostics touch anything; the driver
# binary itself does every pull/merge/package/upload/cache step, exactly
# as a real deployment would.
#
# WHO RUNS THIS AND WHERE: same constraints as run-stage2.sh -- run this
# yourself, from your own terminal, against a real Ubuntu/Linux environment
# with real network access to it (an agent's sandboxed shell tool typically
# cannot reach a Multipass VM's or WSL2 distro's private network) AND real
# network access to the container registries below (ghcr.io, docker.io).
#
#   WSL2:      wsl -d Ubuntu -- bash crates/openshell-driver-lxd/hack/run-stage2-oci.sh
#   Multipass: multipass exec <vm> -- bash /mnt/openshell/crates/openshell-driver-lxd/hack/run-stage2-oci.sh
#
# Prerequisites this script assumes already done (Stage 0/1, run-vm-tests.sh):
#   - LXD installed, initialized (storage pool + a network).
#   - Rust toolchain present.
#   - Real network access from this VM to ghcr.io and docker.io.
#
# WHAT THIS DOES, IN ORDER:
#   1. Builds openshell-sandbox, openshell-driver-lxd, openshell-gateway,
#      and the openshell CLI natively -- no image prep at all this time.
#   2. Starts openshell-driver-lxd WITHOUT --lxd-image, proving Phase 2's
#      relaxed startup validation (a driver can now run entirely off
#      sandbox-supplied images; see config.rs::validate's doc comment).
#   3. Deletes any `openshell-oci-*` LXD image aliases left over from a
#      previous run of *this* script, so a re-run's "cache miss" test
#      actually measures a cache miss, not a stale hit from last time.
#   4. Test A -- conversion-mechanism check, using a plain, reliable,
#      no-auth-needed image (default: ubuntu:26.04). Proves the pull /
#      whiteout-aware merge / metadata.yaml+tar packaging / LXD upload /
#      digest-cache-alias steps all work end to end, independent of
#      whether that specific image is lifecycle-compatible (it isn't --
#      no `sandbox` user, same gap `run-stage2.sh` hit before its "bake a
#      user" fix -- so this test only asserts the *image* got converted
#      and cached, not that the sandbox reaches Ready).
#   5. Test B -- first real lifecycle run using the actual OpenShell
#      sandbox image (default: ghcr.io/nvidia/openshell-community/
#      sandboxes/base:latest), a genuine cache MISS. Full
#      create -> exec -> delete, timed. **Budget 20 minutes for this
#      specific step** -- a real, 13-layer, ~2.7GB image's first
#      conversion took over 4 minutes just for the whiteout-aware merge
#      (per-file `fs::copy`+`chmod`, not hardlinks/reflinks) before this
#      script's timeouts were widened to match, plus however long the
#      LXD upload of a multi-GB tarball itself takes -- confirmed
#      genuinely slow, not hung, only after widening this past an
#      earlier, too-short value that made every run look like a failure
#      without ever finding out which.
#   6. Test C -- second real lifecycle run, same image reference, a
#      genuine cache HIT (the alias Test B just created). Full
#      create -> exec -> delete, timed, and compared against Test B's
#      timing as evidence the cache-by-digest design actually skips the
#      expensive layer-download/merge/upload path on a repeat.
#   7. Writes one consolidated results file, mirroring run-stage2.sh's
#      convention, under crates/openshell-driver-lxd/hack/results/.
#
# WHAT THIS DELIBERATELY DOES NOT DO / KNOWN SIMPLIFICATIONS:
#   - No TLS/mTLS, no _gateway.lxd -- same simplifications run-stage2.sh
#     documents; this script inherits its driver/gateway startup shape
#     wholesale rather than re-deriving it.
#   - Single-daemon, single-run, debug build. No concurrent-conversion,
#     multi-arch, or restart-mid-conversion coverage -- see
#     04-implementation-plan.md's Phase 2 Step 9 for where that
#     belongs once this first real run proves the happy path.
#   - Does not attempt registry auth (OPENSHELL_REGISTRY_USERNAME/
#     OPENSHELL_REGISTRY_TOKEN) -- both default images are public.
#
# ENVIRONMENT VARIABLES (all optional):
#   OPENSHELL_LXD_OCI_CONVERSION_IMAGE  Plain OCI image for Test A
#                                       (default: ubuntu:26.04)
#   OPENSHELL_LXD_OCI_LIFECYCLE_IMAGE   Real sandbox image for Tests B/C
#                                       (default: ghcr.io/nvidia/
#                                       openshell-community/sandboxes/base:latest)
#   OPENSHELL_LXD_OCI_SKIP_BUILD        Set to 1 to reuse already-built
#                                       binaries instead of rebuilding

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

CONVERSION_IMAGE="${OPENSHELL_LXD_OCI_CONVERSION_IMAGE:-ubuntu:26.04}"
LIFECYCLE_IMAGE="${OPENSHELL_LXD_OCI_LIFECYCLE_IMAGE:-ghcr.io/nvidia/openshell-community/sandboxes/base:latest}"
SKIP_BUILD="${OPENSHELL_LXD_OCI_SKIP_BUILD:-0}"
# Deliberately the *same* network name/subnet run-stage2.sh uses
# (openshell / 10.88.77.1/24), not a freshly invented one. An earlier
# version of this script used a distinct "openshell-oci" name to avoid
# any theoretical overlap with a concurrent run-stage2.sh driver -- but
# that name had never actually been proven to work, and running against
# it failed instance creation outright: `POST /1.0/instances` errored
# with `Failed starting device "eth0": Parent device "openshell-oci"
# does not exist`, even though `ensure_network` reported the network as
# ready at driver startup (`ensure_network`'s own GET-then-maybe-POST
# check succeeding doesn't guarantee the underlying kernel bridge device
# actually came up -- unresolved LXD-level mystery, not something this
# script can diagnose further without different tooling). Reusing the
# exact name+subnet the passing Stage 2 run already validated end to
# end sidesteps the question entirely, at the acceptable cost of not
# running concurrently with a live run-stage2.sh driver (this repo's
# scripts are run sequentially by a single user, never concurrently).
BRIDGE_SUBNET="10.88.77.1/24"
BRIDGE_GATEWAY_IP="${BRIDGE_SUBNET%/*}"

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS_DIR="$REPO_ROOT/crates/openshell-driver-lxd/hack/results"
RUN_DIR="$RESULTS_DIR/stage2-oci-$TIMESTAMP"
mkdir -p "$RUN_DIR"
RESULTS_MD="$RESULTS_DIR/stage2-oci-$TIMESTAMP.md"

STATE_DIR="$HOME/.cache/openshell-lxd-stage2-oci/$TIMESTAMP"
mkdir -p "$STATE_DIR"
JWT_DIR="$STATE_DIR/jwt"
mkdir -p "$JWT_DIR"

GATEWAY_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("",0)); print(s.getsockname()[1]); s.close()')"
DRIVER_UDS="$STATE_DIR/lxd-driver.sock"
GATEWAY_CONFIG="$STATE_DIR/gateway.toml"
GATEWAY_DB="$STATE_DIR/gateway.db"
GATEWAY_LOG="$RUN_DIR/gateway.log"
DRIVER_LOG="$RUN_DIR/driver.log"

export XDG_CONFIG_HOME="$STATE_DIR/config"
export XDG_DATA_HOME="$STATE_DIR/data"
GATEWAY_NAME="openshell-lxd-stage2-oci"

echo "==> Testing conversion mechanism with: $CONVERSION_IMAGE"
echo "==> Testing full lifecycle with:       $LIFECYCLE_IMAGE"
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

DRIVER_PID=""
GATEWAY_PID=""
cleanup() {
    local exit_code=$?
    echo ""
    echo "--- cleanup ---"
    if [ -n "$GATEWAY_PID" ] && kill -0 "$GATEWAY_PID" 2>/dev/null; then
        kill "$GATEWAY_PID" 2>/dev/null || true
        wait "$GATEWAY_PID" 2>/dev/null || true
    fi
    if [ -n "$DRIVER_PID" ] && kill -0 "$DRIVER_PID" 2>/dev/null; then
        kill "$DRIVER_PID" 2>/dev/null || true
        wait "$DRIVER_PID" 2>/dev/null || true
    fi
    if [ "$exit_code" -ne 0 ]; then
        echo "NOTE: exiting non-zero ($exit_code); preserving $STATE_DIR and $RUN_DIR for debugging."
        if [ -f "$GATEWAY_LOG" ]; then
            echo "=== gateway log (tail) ==="
            tail -n 60 "$GATEWAY_LOG" 2>/dev/null || true
        fi
        if [ -f "$DRIVER_LOG" ]; then
            echo "=== driver log (tail) ==="
            tail -n 80 "$DRIVER_LOG" 2>/dev/null || true
        fi
    fi
}
trap cleanup EXIT

# ── Step 0: clean up leftover instances AND cached openshell-oci-* images ──
#
# Leftover instances: same rationale as run-stage2.sh (a create-failed run
# doesn't run `sandbox delete`, since the CLI/gateway path is exactly
# what's under test).
#
# Leftover cached images: unlike run-stage2.sh, THIS script's entire
# purpose includes proving a cache *miss* path works (Test B) -- if a
# previous run of this same script already converted and cached
# $LIFECYCLE_IMAGE under its digest alias, Test B would silently become
# a second cache-hit test instead of the cache-miss test it's supposed to
# be, and nothing would say so.
log_section "Step 0: clean up leftover instances and cached openshell-oci-* images"
for candidate in $(lxc list --format csv -c n 2>/dev/null); do
    if [ -n "$(lxc config get "$candidate" user.openshell.sandbox_id 2>/dev/null)" ]; then
        echo "==> Deleting leftover instance from a previous run: $candidate"
        lxc delete --force "$candidate" 2>/dev/null || true
    fi
done
for alias in $(lxc image alias list --format csv 2>/dev/null | cut -d, -f1); do
    case "$alias" in
        openshell-oci-*)
            echo "==> Deleting cached image alias from a previous run: $alias"
            lxc image delete "$alias" 2>/dev/null || true
            ;;
    esac
done

if [ "$(sudo snap get lxd daemon.debug 2>/dev/null)" != "true" ]; then
    echo "==> Enabling LXD daemon debug logging (one-time; restarts the daemon)"
    sudo snap set lxd daemon.debug=true
    sudo systemctl restart snap.lxd.daemon
    sudo lxd waitready --timeout=60
fi

# ── Step 1: build binaries ───────────────────────────────────────────────────

log_section "Step 1: build binaries"
if [ "$SKIP_BUILD" = "1" ]; then
    echo "==> OPENSHELL_LXD_OCI_SKIP_BUILD=1, reusing existing binaries"
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

# ── Step 3: start the LXD driver -- deliberately WITHOUT --lxd-image ───────
#
# Proves Phase 2's relaxed startup validation (config.rs::validate no
# longer requires a pinned default_image): every sandbox this script
# creates supplies its own image via --from, so a fallback pinned image
# is never needed. Running this way is itself part of what's under test,
# not just a convenience.

log_section "Step 3: start openshell-driver-lxd (no --lxd-image)"
"$DRIVER_BIN" \
    --bind-uds "$DRIVER_UDS" \
    --supervisor-bin "$SUPERVISOR_BIN" \
    --network-name openshell \
    --network-ipv4-subnet "$BRIDGE_SUBNET" \
    --grpc-endpoint "http://${BRIDGE_GATEWAY_IP}:${GATEWAY_PORT}" \
    --gateway-port "$GATEWAY_PORT" \
    >"$DRIVER_LOG" 2>&1 &
DRIVER_PID=$!
echo "==> Driver started (pid $DRIVER_PID), waiting for its socket at $DRIVER_UDS"
for _ in $(seq 1 30); do
    if [ -S "$DRIVER_UDS" ]; then
        break
    fi
    if ! kill -0 "$DRIVER_PID" 2>/dev/null; then
        echo "ERROR: driver exited before creating its socket; see $DRIVER_LOG" >&2
        exit 1
    fi
    sleep 1
done
if [ ! -S "$DRIVER_UDS" ]; then
    echo "ERROR: driver socket never appeared at $DRIVER_UDS; see $DRIVER_LOG" >&2
    exit 1
fi
echo "==> Driver socket ready (no pinned default image configured)."

# ── Step 4: start the gateway ────────────────────────────────────────────────

log_section "Step 4: start openshell-gateway"
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
socket_path = "${DRIVER_UDS}"
EOF
echo "==> Wrote $GATEWAY_CONFIG"

"$GATEWAY_BIN" --config "$GATEWAY_CONFIG" --disable-tls \
    --db-url "sqlite:${GATEWAY_DB}?mode=rwc" \
    >"$GATEWAY_LOG" 2>&1 &
GATEWAY_PID=$!
echo "==> Gateway started (pid $GATEWAY_PID)"

# ── Step 5: register the gateway with the CLI ───────────────────────────────

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
    echo "ERROR: gateway did not become ready after ${elapsed}s. Last status output:" >&2
    echo "$last_status" >&2
    exit 1
fi
echo "==> Gateway ready after ${elapsed}s."

# ── Diagnostics helpers (adapted from run-stage2.sh) ────────────────────────

find_lxd_instance_for_sandbox() {
    local sandbox_name="$1"
    local candidate
    for candidate in $(lxc list --format csv -c n 2>/dev/null); do
        if [ "$(lxc config get "$candidate" user.openshell.sandbox_name 2>/dev/null)" = "$sandbox_name" ]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

# Dump whatever's useful when a test fails: if no LXD instance exists at
# all yet, the failure happened *during image conversion itself* (pull,
# merge, package, or upload) -- the driver log (now carrying image.rs's
# own tracing::info!/debug! calls, added specifically because this
# session's whole prior debugging arc kept re-learning "diagnostics are
# load-bearing infrastructure, not an afterthought") is the only useful
# signal in that case. If an instance *does* exist, fall back to the same
# per-instance diagnostics run-stage2.sh already established.
dump_diagnostics() {
    local sandbox_name="$1"
    local label="$2"
    echo "==> Diagnosing '$label' (sandbox '$sandbox_name')" >&2

    echo "=== driver log (image.rs tracing + full tail) ===" >"$RUN_DIR/${label}-driver-log.log"
    grep -E "resolving sandbox image|pulled image manifest|cache (hit|miss)|downloading layer|all layers downloaded|packaged unified|converted and cached|ImageError|failed to resolve sandbox image" \
        "$DRIVER_LOG" >>"$RUN_DIR/${label}-driver-log.log" 2>/dev/null || true
    echo "--- full driver log tail ---" >>"$RUN_DIR/${label}-driver-log.log"
    tail -n 200 "$DRIVER_LOG" >>"$RUN_DIR/${label}-driver-log.log" 2>/dev/null || true

    lxc image list --format csv >"$RUN_DIR/${label}-lxc-image-list.log" 2>&1 || true

    local instance_name
    if ! instance_name="$(find_lxd_instance_for_sandbox "$sandbox_name")"; then
        echo "    (no LXD instance exists for this sandbox -- failure was in image conversion itself, not the lifecycle; see ${label}-driver-log.log)" >&2
        return 1
    fi
    echo "    LXD instance: $instance_name" >&2
    {
        echo "=== lxc config show --expanded ==="
        lxc config show "$instance_name" --expanded
    } >"$RUN_DIR/${label}-instance-config.log" 2>&1 || true
    lxc exec "$instance_name" -- ps aux >"$RUN_DIR/${label}-ps-aux.log" 2>&1 || true
    lxc console "$instance_name" --show-log >"$RUN_DIR/${label}-console-log.log" 2>&1 || true

    # Once the container has stopped, `lxc exec`/`lxc file pull` can't reach
    # its filesystem at all -- but the storage pool's own on-disk rootfs is
    # still right there on the host (mirrors run-stage2.sh's identical
    # `08k-var-log` pull). Pulls *both* `/var/log` and `/tmp` copies of the
    # entrypoint log: this driver's entrypoint script falls back to `/tmp`
    # when `/var/log` isn't writable (a real sandbox image hit exactly
    # that), and since `exec >"$ENTRYPOINT_LOG" 2>&1` stays in effect
    # through the final `exec {supervisor}`, whichever path won also
    # contains the supervisor's own stdout/stderr -- the only way to see
    # *why* it stopped once the container itself is no longer running.
    local storage_rootfs="/var/snap/lxd/common/lxd/storage-pools/default/containers/${instance_name}/rootfs"
    mkdir -p "$RUN_DIR/${label}-entrypoint-log"
    local pulled_any=0
    for dir in var/log tmp; do
        if sudo test -d "${storage_rootfs}/${dir}" 2>/dev/null; then
            while IFS= read -r -d '' f; do
                sudo cp "$f" "$RUN_DIR/${label}-entrypoint-log/" 2>/dev/null && pulled_any=1
            done < <(sudo find "${storage_rootfs}/${dir}" -maxdepth 1 -name 'openshell*.log' -print0 2>/dev/null)
        fi
    done
    if [ "$pulled_any" = "1" ]; then
        sudo chown "$(id -u):$(id -g)" "$RUN_DIR/${label}-entrypoint-log"/*.log 2>/dev/null || true
    else
        echo "(no openshell*.log found under ${storage_rootfs}/{var/log,tmp})" \
            >"$RUN_DIR/${label}-entrypoint-log/NOT_FOUND.log"
    fi

    echo "$instance_name"
}

# ── Step 6: Test A -- conversion-mechanism check ────────────────────────────
#
# Deliberately does not assert the full lifecycle reaches Ready: a plain
# distro image has no `sandbox` user (the exact gap run-stage2.sh's
# "ubuntu" mode had to work around by baking one in by hand). This test
# only asserts the *conversion* completed -- a new openshell-oci-* image
# alias exists in `lxc image list` -- independent of that gap.

log_section "Step 6: Test A -- conversion mechanism ($CONVERSION_IMAGE)"
TEST_A_NAME="lxd-oci-a-$$"
TEST_A_START=$(date +%s)
TEST_A_OUTPUT=""
TEST_A_CREATE_OK=0
if TEST_A_OUTPUT="$(timeout 300 "$CLI_BIN" sandbox create --name "$TEST_A_NAME" --from "$CONVERSION_IMAGE" -- true 2>&1)"; then
    TEST_A_CREATE_OK=1
fi
TEST_A_END=$(date +%s)
TEST_A_DURATION=$((TEST_A_END - TEST_A_START))
echo "$TEST_A_OUTPUT" >"$RUN_DIR/06a-test-a-create.log"
echo "==> Test A create finished in ${TEST_A_DURATION}s (create succeeded: $TEST_A_CREATE_OK)"

TEST_A_IMAGE_CACHED=0
if lxc image alias list --format csv 2>/dev/null | grep -q "^openshell-oci-"; then
    TEST_A_IMAGE_CACHED=1
    echo "==> Test A: a new openshell-oci-* image alias exists -- conversion mechanism validated"
else
    echo "==> Test A: WARNING -- no openshell-oci-* image alias found after create attempt" >&2
fi
dump_diagnostics "$TEST_A_NAME" "06-test-a" >/dev/null || true
lxc delete --force "$TEST_A_NAME" >/dev/null 2>&1 || true

# ── Step 7: Test B -- real lifecycle, cache MISS ────────────────────────────

log_section "Step 7: Test B -- full lifecycle, cache MISS ($LIFECYCLE_IMAGE)"
TEST_B_NAME="lxd-oci-b-$$"
TEST_B_OUTCOME="not-run"
TEST_B_START=$(date +%s)
TEST_B_CREATE_OUTPUT=""
if TEST_B_CREATE_OUTPUT="$(timeout 1200 "$CLI_BIN" sandbox create --name "$TEST_B_NAME" --from "$LIFECYCLE_IMAGE" -- echo stage2-oci-ok 2>&1)"; then
    echo "$TEST_B_CREATE_OUTPUT" >"$RUN_DIR/07a-test-b-create.log"
    TEST_B_EXEC_OUTPUT=""
    if TEST_B_EXEC_OUTPUT="$(timeout 60 "$CLI_BIN" sandbox exec --name "$TEST_B_NAME" -- echo stage2-oci-exec-ok 2>&1)"; then
        echo "$TEST_B_EXEC_OUTPUT" >"$RUN_DIR/07b-test-b-exec.log"
        if echo "$TEST_B_EXEC_OUTPUT" | grep -q "stage2-oci-exec-ok"; then
            TEST_B_OUTCOME="pass"
        else
            TEST_B_OUTCOME="exec-ran-but-output-unexpected"
        fi
    else
        echo "$TEST_B_EXEC_OUTPUT" >"$RUN_DIR/07b-test-b-exec.log"
        TEST_B_OUTCOME="exec-failed"
    fi
    timeout 60 "$CLI_BIN" sandbox delete "$TEST_B_NAME" >"$RUN_DIR/07c-test-b-delete.log" 2>&1 \
        || echo "WARNING: Test B sandbox delete failed; see $RUN_DIR/07c-test-b-delete.log" >&2
else
    echo "$TEST_B_CREATE_OUTPUT" >"$RUN_DIR/07a-test-b-create.log"
    TEST_B_OUTCOME="create-failed"
fi
TEST_B_END=$(date +%s)
TEST_B_DURATION=$((TEST_B_END - TEST_B_START))
echo "==> Test B outcome: $TEST_B_OUTCOME (${TEST_B_DURATION}s)"
if [ "$TEST_B_OUTCOME" != "pass" ]; then
    STUCK_B="$(dump_diagnostics "$TEST_B_NAME" "07-test-b" || true)"
    if [ -n "${STUCK_B:-}" ]; then
        lxc delete --force "$STUCK_B" >"$RUN_DIR/07d-test-b-cleanup.log" 2>&1 || true
    fi
fi

# ── Step 8: Test C -- real lifecycle, cache HIT ─────────────────────────────
#
# Same image reference as Test B, different sandbox name. If the
# digest-cache design (image.rs::ensure_lxd_image, checked before any
# layer download) works, this should complete in a small fraction of
# Test B's time -- one manifest+config fetch instead of a full
# pull/merge/package/upload.

log_section "Step 8: Test C -- full lifecycle, cache HIT ($LIFECYCLE_IMAGE)"
TEST_C_NAME="lxd-oci-c-$$"
TEST_C_OUTCOME="not-run"
TEST_C_START=$(date +%s)
TEST_C_CREATE_OUTPUT=""
if TEST_C_CREATE_OUTPUT="$(timeout 300 "$CLI_BIN" sandbox create --name "$TEST_C_NAME" --from "$LIFECYCLE_IMAGE" -- echo stage2-oci-cached-ok 2>&1)"; then
    echo "$TEST_C_CREATE_OUTPUT" >"$RUN_DIR/08a-test-c-create.log"
    TEST_C_EXEC_OUTPUT=""
    if TEST_C_EXEC_OUTPUT="$(timeout 60 "$CLI_BIN" sandbox exec --name "$TEST_C_NAME" -- echo stage2-oci-cached-exec-ok 2>&1)"; then
        echo "$TEST_C_EXEC_OUTPUT" >"$RUN_DIR/08b-test-c-exec.log"
        if echo "$TEST_C_EXEC_OUTPUT" | grep -q "stage2-oci-cached-exec-ok"; then
            TEST_C_OUTCOME="pass"
        else
            TEST_C_OUTCOME="exec-ran-but-output-unexpected"
        fi
    else
        echo "$TEST_C_EXEC_OUTPUT" >"$RUN_DIR/08b-test-c-exec.log"
        TEST_C_OUTCOME="exec-failed"
    fi
    timeout 60 "$CLI_BIN" sandbox delete "$TEST_C_NAME" >"$RUN_DIR/08c-test-c-delete.log" 2>&1 \
        || echo "WARNING: Test C sandbox delete failed; see $RUN_DIR/08c-test-c-delete.log" >&2
else
    echo "$TEST_C_CREATE_OUTPUT" >"$RUN_DIR/08a-test-c-create.log"
    TEST_C_OUTCOME="create-failed"
fi
TEST_C_END=$(date +%s)
TEST_C_DURATION=$((TEST_C_END - TEST_C_START))
echo "==> Test C outcome: $TEST_C_OUTCOME (${TEST_C_DURATION}s)"
if [ "$TEST_C_OUTCOME" != "pass" ]; then
    STUCK_C="$(dump_diagnostics "$TEST_C_NAME" "08-test-c" || true)"
    if [ -n "${STUCK_C:-}" ]; then
        lxc delete --force "$STUCK_C" >"$RUN_DIR/08d-test-c-cleanup.log" 2>&1 || true
    fi
fi

CACHE_EFFECTIVE="unknown"
if [ "$TEST_B_OUTCOME" = "pass" ] && [ "$TEST_C_OUTCOME" = "pass" ]; then
    if [ "$TEST_C_DURATION" -lt "$TEST_B_DURATION" ]; then
        CACHE_EFFECTIVE="yes (${TEST_C_DURATION}s vs ${TEST_B_DURATION}s)"
    else
        CACHE_EFFECTIVE="NO -- cache hit was not faster (${TEST_C_DURATION}s vs ${TEST_B_DURATION}s); investigate get_image_by_alias"
    fi
fi

# ── Results file ─────────────────────────────────────────────────────────────

OVERALL_OUTCOME="fail"
if [ "$TEST_A_IMAGE_CACHED" -eq 1 ] && [ "$TEST_B_OUTCOME" = "pass" ] && [ "$TEST_C_OUTCOME" = "pass" ]; then
    OVERALL_OUTCOME="pass"
fi

{
    echo "# LXD driver OCI conversion pipeline run: $TIMESTAMP"
    echo ""
    echo "Produced by \`crates/openshell-driver-lxd/hack/run-stage2-oci.sh\`."
    echo "Raw logs in \`results/stage2-oci-$TIMESTAMP/\`."
    echo ""
    echo "## Config"
    echo ""
    echo '```'
    echo "Conversion-mechanism test image: $CONVERSION_IMAGE"
    echo "Full-lifecycle test image:       $LIFECYCLE_IMAGE"
    echo "Bridge subnet:                   $BRIDGE_SUBNET"
    echo "Gateway port:                    $GATEWAY_PORT"
    echo "Driver started with --lxd-image: no (Phase 2: not required)"
    echo '```'
    echo ""
    echo "## Outcome: \`$OVERALL_OUTCOME\`"
    echo ""
    echo "| Test | Purpose | Outcome | Duration |"
    echo "|---|---|---|---|"
    echo "| A | conversion mechanism ($CONVERSION_IMAGE) | image cached: $TEST_A_IMAGE_CACHED, create succeeded: $TEST_A_CREATE_OK | ${TEST_A_DURATION}s |"
    echo "| B | full lifecycle, cache MISS ($LIFECYCLE_IMAGE) | $TEST_B_OUTCOME | ${TEST_B_DURATION}s |"
    echo "| C | full lifecycle, cache HIT ($LIFECYCLE_IMAGE) | $TEST_C_OUTCOME | ${TEST_C_DURATION}s |"
    echo ""
    echo "**Cache effectiveness:** $CACHE_EFFECTIVE"
    echo ""
    echo "## Logs"
    echo ""
    echo "- \`01-build.log\`, \`01b-build-gateway.log\` -- cargo build (if not skipped)"
    echo "- \`driver.log\`, \`gateway.log\` -- process logs, captured live"
    echo "- \`06a-test-a-create.log\`, \`06-test-a-*\` -- Test A (conversion mechanism)"
    echo "- \`07a\`-\`07d\`, \`07-test-b-*\` -- Test B (cache miss lifecycle)"
    echo "- \`08a\`-\`08d\`, \`08-test-c-*\` -- Test C (cache hit lifecycle)"
} >"$RESULTS_MD"
echo "==> Results written to $RESULTS_MD"

log_section "Done"
echo "Outcome: $OVERALL_OUTCOME"
echo "Test A (conversion):        image cached=$TEST_A_IMAGE_CACHED create_ok=$TEST_A_CREATE_OK (${TEST_A_DURATION}s)"
echo "Test B (cache miss):        $TEST_B_OUTCOME (${TEST_B_DURATION}s)"
echo "Test C (cache hit):         $TEST_C_OUTCOME (${TEST_C_DURATION}s)"
echo "Cache effectiveness:        $CACHE_EFFECTIVE"
if [ "$OVERALL_OUTCOME" != "pass" ]; then
    exit 1
fi
exit 0
