#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Stage 2 of crates/openshell-driver-lxd/docs/05-test-plan.md: a full sandbox lifecycle
# (create -> exec -> delete) against a real LXD daemon, using the actual
# openshell-driver-lxd binary, a hand-converted sandbox image, a real
# openshell-gateway, and the openshell CLI.
#
# WHO RUNS THIS AND WHERE: same constraints as run-vm-tests.sh -- run this
# yourself, from your own terminal, against a real Ubuntu/Linux environment
# with real network access to it (an agent's sandboxed shell tool typically
# cannot reach a Multipass VM's or WSL2 distro's private network; see
# run-vm-tests.sh's own header and 05-test-plan.md).
#
#   WSL2:      wsl -d Ubuntu -- bash crates/openshell-driver-lxd/hack/run-stage2.sh
#   Multipass: multipass exec <vm> -- bash /mnt/openshell/crates/openshell-driver-lxd/hack/run-stage2.sh
#
# Prerequisites this script assumes already done (Stage 0/1, run-vm-tests.sh):
#   - LXD installed, initialized (storage pool + a network -- this script
#     creates its own separate "openshell" network via the driver itself,
#     independent of whatever run-vm-tests.sh's confinement spike used).
#   - Rust toolchain present.
#   - Real network access from this VM to ghcr.io (to pull the sandbox image).
#
# WHAT THIS DOES, IN ORDER:
#   1. Prepares one base image under the local alias `openshell-sandbox-base`.
#      Two modes (OPENSHELL_LXD_STAGE2_BASE_MODE):
#        - "ubuntu" (default): `lxc image copy ubuntu:<release> local:`. No
#          OCI conversion at all -- see the CORRECTION note below for why
#          this is the default now.
#        - "oci": converts an OCI image
#          (ghcr.io/nvidia/openshell-community/sandboxes/base:latest by
#          default) into LXD's unified-tarball-as-directory image format
#          via skopeo + umoci, then `lxc image import`.
#      Either way, idempotent: skipped if the target alias already exists.
#
#      CORRECTION #1 (found running this against a real daemon): the
#      original default image, ghcr.io/nvidia/openshell/sandbox, returns
#      403 DENIED on an anonymous token request -- confirmed not
#      environment-specific by checking sibling images from an unrelated
#      network: gateway, supervisor, and helm-chart all return 200 for the
#      same anonymous token request; sandbox alone was denied,
#      tag-independent.
#      CORRECTION #2 (root cause of #1, surfaced by the user, not
#      independently found here): that path isn't merely private, it's
#      stale. NVIDIA/OpenShell#267 moved sandbox images out of this repo
#      entirely, into a separate community registry namespace -- the
#      current path is ghcr.io/nvidia/openshell-community/sandboxes/base,
#      now this script's "oci" default above. Neither correction changes
#      "ubuntu" mode's status as this script's default: Stage 2's actually
#      novel, actually-at-risk surface is the driver/gateway/lifecycle
#      wiring, not which base rootfs holds the supervisor, and the
#      supervisor is delivered identically via a disk device either way.
#   2. Builds openshell-sandbox, openshell-driver-lxd, openshell-gateway, and
#      the openshell CLI natively.
#   3. Generates a sandbox-JWT signing key (openssl) -- no TLS/mTLS material
#      at all. This intentionally runs the gateway with --disable-tls: the
#      LXD driver has no mechanism yet to deliver client mTLS material into
#      a sandbox (explicit Phase 2 item -- see the crate README's "What's
#      explicitly NOT implemented"), so attempting mTLS here would fail for
#      a reason unrelated to whatever Stage 2 is actually trying to prove.
#   4. Starts the openshell-driver-lxd binary on a Unix socket (--bind-uds --
#      see main.rs's doc comment on why this, not --bind-address, is what a
#      real gateway can actually dial).
#   5. Starts openshell-gateway pointed at that socket, bound to 0.0.0.0 (not
#      127.0.0.1) so the sandbox supervisor -- reachable only via the LXD
#      bridge's host-side IP, not loopback -- can dial back to it.
#   6. Registers the gateway with the CLI (plaintext, matching --disable-tls)
#      and runs sandbox create -> exec -> delete.
#   7. Writes one consolidated results file, mirroring run-vm-tests.sh's
#      convention, under crates/openshell-driver-lxd/hack/results/.
#
# WHAT THIS DELIBERATELY DOES NOT DO / KNOWN SIMPLIFICATIONS (read before
# trusting a "PASS" from this script as more than "the happy path works"):
#   - Bypasses the _gateway.lxd / GetGatewayListenerRequirements code path
#     entirely by passing the driver's own bridge gateway IP directly as
#     grpc_endpoint. That path (LxdComputeDriver::gateway_listener_requirements
#     in driver.rs) remains UNEXERCISED by this script. This was a
#     deliberate simplification to de-risk the first Stage 2 attempt, not a
#     claim that path works.
#   - No TLS/mTLS at all (see step 3 above) -- this is not the production
#     posture Phase 2 needs to reach; it's the simplest configuration that
#     could plausibly prove the lifecycle works at all.
#   - Uses a plain stock OCI image conversion recipe (metadata.yaml with
#     only the mandatory architecture/creation_date fields) that has never
#     been run before this script was written. The exact metadata.yaml
#     shape and unified-tarball-as-directory approach are based on LXD's
#     published image-format documentation, not on prior verified use in
#     this codebase -- treat a failure at the "lxc image import" or
#     "lxc launch" step as genuinely new information, not a regression.
#   - Does not attempt resource limits, driver-config mounts, or any other
#     Phase 2 feature-parity item -- lifecycle correctness only.
#
# ENVIRONMENT VARIABLES (all optional):
#   OPENSHELL_LXD_STAGE2_BASE_MODE     "ubuntu" (default) or "oci" -- see
#                                      Step 1's CORRECTION note above.
#   OPENSHELL_LXD_STAGE2_UBUNTU_IMAGE  LXD `ubuntu:` remote alias to copy
#                                      when BASE_MODE=ubuntu (default:
#                                      ubuntu:26.04, matching this VM's own
#                                      release -- deliberately not an older
#                                      release, to avoid linking the
#                                      supervisor against glibc symbols
#                                      newer than what an older container
#                                      userland provides; LXD containers use
#                                      the host kernel but their own libc)
#   OPENSHELL_LXD_STAGE2_IMAGE_REF     OCI image to convert when
#                                      BASE_MODE=oci
#                                      (default: ghcr.io/nvidia/openshell-community/sandboxes/base:latest)
#   OPENSHELL_LXD_STAGE2_IMAGE_ALIAS   Local LXD image alias to prepare
#                                      (default: openshell-sandbox-base)
#   OPENSHELL_LXD_STAGE2_BRIDGE_SUBNET Explicit CIDR for the driver's managed
#                                      bridge network (default: 10.88.77.1/24
#                                      -- deliberately different from
#                                      run-vm-tests.sh's confinement-spike
#                                      subnet and the driver's own compiled
#                                      default, so this script's network
#                                      never collides with either)
#   OPENSHELL_LXD_STAGE2_SKIP_BUILD    Set to 1 to reuse already-built
#                                      binaries instead of rebuilding
#   OPENSHELL_LXD_STAGE2_SKIP_IMAGE    Set to 1 to skip image conversion
#                                      (assumes the alias already exists)

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

# Same rationale as run-vm-tests.sh: keep Cargo's build output off the
# mount and on this VM's own native disk.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/openshell-lxd-target}"
echo "==> CARGO_TARGET_DIR: $CARGO_TARGET_DIR"

# `multipass exec` runs a non-login, non-interactive shell, which does not
# source ~/.bashrc/~/.profile -- rustup's own installer only wires PATH
# through those, via ~/.cargo/env. run-vm-tests.sh sources this after
# installing Rust for exactly this reason; this script needs the same
# handling even though it assumes Stage 0/1 already installed Rust, or
# every `cargo` invocation below fails with "command not found" despite
# the toolchain genuinely being present.
if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo not found even after sourcing ~/.cargo/env. Run Stage 0/1" >&2
    echo "       (run-vm-tests.sh) first, or install Rust manually." >&2
    exit 1
fi
echo "==> cargo: $(cargo --version)"

# ── Config ───────────────────────────────────────────────────────────────────

BASE_MODE="${OPENSHELL_LXD_STAGE2_BASE_MODE:-ubuntu}"
UBUNTU_IMAGE="${OPENSHELL_LXD_STAGE2_UBUNTU_IMAGE:-ubuntu:26.04}"
# ghcr.io/nvidia/openshell-community/sandboxes/base:latest is the current
# path -- ghcr.io/nvidia/openshell/sandbox (this crate's docs' original
# default, and this script's own original default) was deprecated by
# NVIDIA/OpenShell#267, which moved sandbox images to a separate community
# registry entirely. That PR is *why* the old path came back DENIED rather
# than just being a visibility bug -- it doesn't meaningfully exist at that
# path anymore, not "exists but private."
IMAGE_REF="${OPENSHELL_LXD_STAGE2_IMAGE_REF:-ghcr.io/nvidia/openshell-community/sandboxes/base:latest}"
IMAGE_ALIAS="${OPENSHELL_LXD_STAGE2_IMAGE_ALIAS:-openshell-sandbox-base}"
BRIDGE_SUBNET="${OPENSHELL_LXD_STAGE2_BRIDGE_SUBNET:-10.88.77.1/24}"
BRIDGE_GATEWAY_IP="${BRIDGE_SUBNET%/*}"
SKIP_BUILD="${OPENSHELL_LXD_STAGE2_SKIP_BUILD:-0}"
SKIP_IMAGE="${OPENSHELL_LXD_STAGE2_SKIP_IMAGE:-0}"

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS_DIR="$REPO_ROOT/crates/openshell-driver-lxd/hack/results"
RUN_DIR="$RESULTS_DIR/stage2-$TIMESTAMP"
mkdir -p "$RUN_DIR"
RESULTS_MD="$RESULTS_DIR/stage2-$TIMESTAMP.md"

STATE_DIR="$HOME/.cache/openshell-lxd-stage2/$TIMESTAMP"
mkdir -p "$STATE_DIR"
WORK_DIR="$STATE_DIR/work"
JWT_DIR="$STATE_DIR/jwt"
mkdir -p "$WORK_DIR" "$JWT_DIR"

GATEWAY_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("",0)); print(s.getsockname()[1]); s.close()')"
DRIVER_UDS="$STATE_DIR/lxd-driver.sock"
GATEWAY_CONFIG="$STATE_DIR/gateway.toml"
GATEWAY_DB="$STATE_DIR/gateway.db"
GATEWAY_LOG="$RUN_DIR/gateway.log"
DRIVER_LOG="$RUN_DIR/driver.log"

export XDG_CONFIG_HOME="$STATE_DIR/config"
export XDG_DATA_HOME="$STATE_DIR/data"
GATEWAY_NAME="openshell-lxd-stage2"

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

have_cmd() { command -v "$1" >/dev/null 2>&1; }

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
            tail -n 60 "$DRIVER_LOG" 2>/dev/null || true
        fi
    fi
}
trap cleanup EXIT

# ── Step 0: clean up leftover instances from previous failed runs ──────────
#
# `create-failed` deliberately doesn't run `sandbox delete` (the CLI/gateway
# path is exactly what's under test), and earlier versions of this script
# didn't clean up the LXD instance directly either -- every prior failed
# run on this VM has left one real, running LXD container behind. Sweep
# for any instance this driver ever created (identified by having a
# user.openshell.sandbox_id label at all, regardless of value) and remove
# it before starting a fresh run, rather than letting them accumulate on a
# 30G disk across a long debugging session.
log_section "Step 0: clean up leftover instances from previous runs"
for candidate in $(lxc list --format csv -c n 2>/dev/null); do
    if [ -n "$(lxc config get "$candidate" user.openshell.sandbox_id 2>/dev/null)" ]; then
        echo "==> Deleting leftover instance from a previous run: $candidate"
        lxc delete --force "$candidate" 2>/dev/null || true
    fi
done

# Enable verbose LXD daemon logging once, idempotently. Without this, the
# per-instance log at /var/snap/lxd/common/lxd/logs/<instance>/lxc.log
# exists but stays completely empty even for a crashed instance -- found
# this the hard way, capturing a genuinely empty file from a real failed
# run rather than assuming it would explain anything by default. This is
# a one-time VM-level operational setting, not something this driver's
# own instance spec should ever set (trace-level logging has no place in
# a real per-sandbox config).
if [ "$(sudo snap get lxd daemon.debug 2>/dev/null)" != "true" ]; then
    echo "==> Enabling LXD daemon debug logging (one-time; restarts the daemon)"
    sudo snap set lxd daemon.debug=true
    sudo systemctl restart snap.lxd.daemon
    sudo lxd waitready --timeout=60
fi

# ── Step 1 & 2: prepare a base image under $IMAGE_ALIAS ─────────────────────

log_section "Step 1/2: prepare base image (mode: $BASE_MODE)"

if [ "$SKIP_IMAGE" = "1" ] || lxc image alias list --format csv 2>/dev/null | grep -q "^${IMAGE_ALIAS},"; then
    echo "==> Alias '$IMAGE_ALIAS' already present (or SKIP_IMAGE=1); not (re)preparing."
elif [ "$BASE_MODE" = "ubuntu" ]; then
    echo "==> Copying $UBUNTU_IMAGE to local alias '$IMAGE_ALIAS' (no OCI conversion --"
    echo "    see this script's header 'BASE_MODE' note for why this is the default)"
    run_logged "$RUN_DIR/02-lxc-image-copy.log" \
        lxc image copy "$UBUNTU_IMAGE" local: --alias "$IMAGE_ALIAS" \
        || { echo "ERROR: lxc image copy failed; see $RUN_DIR/02-lxc-image-copy.log" >&2; exit 1; }
    echo "==> Copied. $(lxc image list "$IMAGE_ALIAS" --format csv)"

    # The supervisor's default policy (process.run_as_user unset -> "sandbox",
    # see openshell-supervisor-process/src/process.rs::validate_sandbox_user)
    # requires a real 'sandbox' entry in the image's /etc/passwd and
    # /etc/group before it will drop privileges and start the sandboxed
    # process -- a stock cloud image has neither. Real sandbox images
    # (deploy/docker's supervisor image is a different artifact; see
    # e2e/mcp-conformance/Dockerfile.client and
    # scripts/agents/gator/Dockerfile for the actual precedent) always bake
    # this user in at build time. Found the hard way running a real Stage 2
    # test: the supervisor authenticated, fetched its policy, and started
    # its proxy successfully, then failed with "explicit process user
    # 'sandbox' was not found in the image" right as it tried to drop
    # privileges to launch the sandboxed command -- and every log this
    # script captures came up empty until the entrypoint script's own
    # stdout/stderr redirect was fixed to actually cover this exact failure
    # (see instance.rs::build_entrypoint_script's doc comment).
    echo "==> Baking a 'sandbox' user/group into '$IMAGE_ALIAS' via a throwaway container"
    PREP_CONTAINER="openshell-stage2-image-prep-$$"
    run_logged "$RUN_DIR/02d-image-prep-launch.log" \
        lxc launch "$IMAGE_ALIAS" "$PREP_CONTAINER" \
        || { echo "ERROR: failed to launch image-prep container; see $RUN_DIR/02d-image-prep-launch.log" >&2; exit 1; }
    prep_ready=0
    for _ in $(seq 1 30); do
        if lxc exec "$PREP_CONTAINER" -- true >/dev/null 2>&1; then
            prep_ready=1
            break
        fi
        sleep 1
    done
    if [ "$prep_ready" -ne 1 ]; then
        echo "ERROR: image-prep container never became execute-ready" >&2
        lxc delete --force "$PREP_CONTAINER" 2>/dev/null || true
        exit 1
    fi
    if ! run_logged "$RUN_DIR/02e-image-prep-useradd.log" \
        lxc exec "$PREP_CONTAINER" -- sh -c 'groupadd -r sandbox && useradd -r -g sandbox -s /usr/sbin/nologin sandbox'; then
        echo "ERROR: failed to create 'sandbox' user/group in image-prep container; see $RUN_DIR/02e-image-prep-useradd.log" >&2
        lxc delete --force "$PREP_CONTAINER" 2>/dev/null || true
        exit 1
    fi
    lxc stop "$PREP_CONTAINER" --timeout 10 >/dev/null 2>&1 || lxc stop "$PREP_CONTAINER" --force >/dev/null 2>&1
    # Publish unaliased first and swap the alias pointer only after success,
    # so a publish failure never leaves $IMAGE_ALIAS pointing at nothing.
    PUBLISH_OUTPUT="$(lxc publish "$PREP_CONTAINER" 2>&1)" || {
        echo "$PUBLISH_OUTPUT" >"$RUN_DIR/02f-image-prep-publish.log"
        echo "ERROR: failed to publish image-prep container; see $RUN_DIR/02f-image-prep-publish.log" >&2
        lxc delete --force "$PREP_CONTAINER" 2>/dev/null || true
        exit 1
    }
    echo "$PUBLISH_OUTPUT" >"$RUN_DIR/02f-image-prep-publish.log"
    PREPPED_FINGERPRINT="$(echo "$PUBLISH_OUTPUT" | grep -oE '[0-9a-f]{64}' | head -1)"
    lxc delete --force "$PREP_CONTAINER" >/dev/null 2>&1 || true
    if [ -z "$PREPPED_FINGERPRINT" ]; then
        echo "ERROR: could not parse a fingerprint out of lxc publish's output; see $RUN_DIR/02f-image-prep-publish.log" >&2
        exit 1
    fi
    lxc image alias delete "$IMAGE_ALIAS" >/dev/null 2>&1 || true
    run_logged "$RUN_DIR/02g-image-prep-realias.log" \
        lxc image alias create "$IMAGE_ALIAS" "$PREPPED_FINGERPRINT" \
        || { echo "ERROR: failed to re-point alias '$IMAGE_ALIAS' at the prepped image; see $RUN_DIR/02g-image-prep-realias.log" >&2; exit 1; }
    echo "==> Re-aliased '$IMAGE_ALIAS' to the prepped image (fingerprint: ${PREPPED_FINGERPRINT:0:12}...) with a 'sandbox' user baked in"
elif [ "$BASE_MODE" = "oci" ]; then
    echo "==> Installing prerequisites (skopeo, umoci)"
    if ! have_cmd skopeo || ! have_cmd umoci; then
        sudo apt-get update -qq
        sudo apt-get install -y --no-install-recommends skopeo umoci >/dev/null
    fi
    echo "==> skopeo: $(skopeo --version 2>&1 | head -1)"
    echo "==> umoci: $(umoci --version 2>&1 | head -1)"

    OCI_LAYOUT="$WORK_DIR/sandbox-oci"
    BUNDLE_DIR="$WORK_DIR/sandbox-bundle"
    echo "==> Pulling $IMAGE_REF into an OCI layout (skopeo)"
    run_logged "$RUN_DIR/02a-skopeo-copy.log" \
        sudo skopeo copy "docker://${IMAGE_REF}" "oci:${OCI_LAYOUT}:latest" \
        || { echo "ERROR: skopeo copy failed; see $RUN_DIR/02a-skopeo-copy.log (if this is the same 403/DENIED error this script's header documents, that's a GHCR package-visibility fact, not fixable here -- rerun with OPENSHELL_LXD_STAGE2_BASE_MODE=ubuntu, the default, instead)" >&2; exit 1; }

    echo "==> Unpacking with umoci (as root -- NOT --rootless, so file ownership"
    echo "    from the image's layers transfers literally; LXD's own idmap"
    echo "    shifting for unprivileged containers operates at the container"
    echo "    runtime level and does not need the source image pre-shifted)"
    run_logged "$RUN_DIR/02b-umoci-unpack.log" \
        sudo umoci unpack --image "${OCI_LAYOUT}:latest" "$BUNDLE_DIR" \
        || { echo "ERROR: umoci unpack failed; see $RUN_DIR/02b-umoci-unpack.log" >&2; exit 1; }

    echo "==> Writing metadata.yaml (mandatory fields only: architecture, creation_date)"
    ARCH="$(uname -m)"
    sudo tee "$BUNDLE_DIR/metadata.yaml" >/dev/null <<EOF
architecture: ${ARCH}
creation_date: $(date -u +%s)
properties:
  os: openshell-sandbox
  description: "OpenShell sandbox (converted from ${IMAGE_REF})"
EOF

    echo "==> Importing as a unified image directory (lxc image import supports"
    echo "    a directory directly for this format -- no tarball/compression"
    echo "    step needed; see LXD's image-format docs)"
    run_logged "$RUN_DIR/02c-lxc-image-import.log" \
        sudo lxc image import "$BUNDLE_DIR" --alias "$IMAGE_ALIAS" \
        || { echo "ERROR: lxc image import failed; see $RUN_DIR/02c-lxc-image-import.log" >&2; exit 1; }
    echo "==> Imported. $(lxc image list "$IMAGE_ALIAS" --format csv)"
else
    echo "ERROR: unknown OPENSHELL_LXD_STAGE2_BASE_MODE='$BASE_MODE' (expected 'ubuntu' or 'oci')" >&2
    exit 1
fi

# ── Step 3: build binaries ───────────────────────────────────────────────────

log_section "Step 3: build binaries"
if [ "$SKIP_BUILD" = "1" ]; then
    echo "==> OPENSHELL_LXD_STAGE2_SKIP_BUILD=1, reusing existing binaries"
else
    # openshell-server (the gateway) depends on openshell-prover, which
    # links against Z3 -- a real, additional prerequisite Stage 0/1 never
    # needed (run-vm-tests.sh's own install_prereqs comment notes
    # openshell-driver-lxd's dependency graph specifically doesn't pull in
    # Z3; the full gateway does). Install it here rather than assuming
    # Stage 0/1 already did.
    if ! dpkg -s libz3-dev >/dev/null 2>&1; then
        echo "==> Installing libz3-dev (needed to link openshell-gateway via openshell-prover)"
        sudo apt-get update -qq
        sudo apt-get install -y --no-install-recommends libz3-dev >/dev/null
    fi

    # Two separate invocations, not one combined `-p A -p B --bin
    # openshell-gateway` command: cargo applies a `--bin` target filter
    # across the *entire* package selection, not just the package(s) that
    # actually have a bin by that name -- combined, it silently built only
    # openshell-gateway and produced no binaries at all for the other three
    # packages (caught by actually running this build once before handing
    # this script off, not assumed).
    run_logged "$RUN_DIR/03-build.log" \
        cargo build --manifest-path "$REPO_ROOT/Cargo.toml" \
            -p openshell-sandbox \
            -p openshell-driver-lxd \
            -p openshell-cli \
        || { echo "ERROR: build failed; see $RUN_DIR/03-build.log" >&2; exit 1; }
    run_logged "$RUN_DIR/03b-build-gateway.log" \
        cargo build --manifest-path "$REPO_ROOT/Cargo.toml" \
            -p openshell-server --bin openshell-gateway \
        || { echo "ERROR: gateway build failed; see $RUN_DIR/03b-build-gateway.log" >&2; exit 1; }
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

# ── Step 4: sandbox-JWT signing key (no TLS/mTLS -- see script header) ──────

log_section "Step 4: gateway sandbox-JWT signing key"
(
    umask 077
    openssl genpkey -algorithm Ed25519 -out "$JWT_DIR/signing.pem" >/dev/null 2>&1
)
openssl pkey -in "$JWT_DIR/signing.pem" -pubout -out "$JWT_DIR/public.pem" >/dev/null 2>&1
openssl rand -hex 16 >"$JWT_DIR/kid"
echo "==> JWT signing key generated at $JWT_DIR"

# ── Step 5: start the LXD driver on a Unix socket ───────────────────────────

log_section "Step 5: start openshell-driver-lxd"
"$DRIVER_BIN" \
    --bind-uds "$DRIVER_UDS" \
    --lxd-image "$IMAGE_ALIAS" \
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
echo "==> Driver socket ready."

# ── Step 6: start the gateway (plaintext, bound to 0.0.0.0) ─────────────────

log_section "Step 6: start openshell-gateway"
cat >"$GATEWAY_CONFIG" <<EOF
[openshell]
version = 1

[openshell.gateway]
bind_address = "0.0.0.0:${GATEWAY_PORT}"
compute_drivers = ["lxd"]

# Without this, every authenticated RPC (anything past the health-check
# allowlist -- see multiplex.rs's AuthGrpcRouter) rejects with "missing
# authorization header": no mTLS, no OIDC, and [gateway_jwt] below signs
# *sandbox* JWTs, not CLI/user credentials, so none of those three satisfy
# user auth on their own. "Only use this for trusted local development" is
# exactly this script's situation -- a single, disposable, local test VM.
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
echo "==> Wrote $GATEWAY_CONFIG:"
cat "$GATEWAY_CONFIG"

"$GATEWAY_BIN" --config "$GATEWAY_CONFIG" --disable-tls \
    --db-url "sqlite:${GATEWAY_DB}?mode=rwc" \
    >"$GATEWAY_LOG" 2>&1 &
GATEWAY_PID=$!
echo "==> Gateway started (pid $GATEWAY_PID)"

# ── Step 7: register the gateway with the CLI (plaintext) ───────────────────

log_section "Step 7: register gateway with CLI, wait for readiness"
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

# ── Step 8: sandbox lifecycle ────────────────────────────────────────────────

# Find the LXD instance backing a given sandbox name, via the
# user.openshell.sandbox_name label this driver now stamps (see instance.rs)
# -- not by reconstructing the LXD-internal name ourselves, which would
# just duplicate (and risk drifting from) instance_name()'s own logic.
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

# Dump everything useful about the actual running (or crashed) LXD
# instance when the CLI-level lifecycle doesn't tell us why: is the
# supervisor actually PID 1, did it print anything before dying, does
# LXD's own expanded config actually contain the raw.lxc/lxc.init.cmd this
# driver is supposed to have set. Self-diagnosing rather than requiring
# another round-trip for "what does the container actually look like".
dump_instance_diagnostics() {
    local sandbox_name="$1"
    local instance_name
    if ! instance_name="$(find_lxd_instance_for_sandbox "$sandbox_name")"; then
        # Callers capture this function's stdout as the instance name --
        # every informational line here must go to stderr, or it pollutes
        # that capture instead of just returning the name.
        echo "WARNING: no LXD instance found with user.openshell.sandbox_name=$sandbox_name -- the sandbox_name label itself may not be landing; check 08d-lxc-list.log" >&2
        lxc list --format csv >"$RUN_DIR/08d-lxc-list.log" 2>&1 || true
        return 1
    fi
    echo "==> Diagnosing LXD instance '$instance_name' for sandbox '$sandbox_name'" >&2
    {
        echo "=== lxc config show --expanded ==="
        lxc config show "$instance_name" --expanded
    } >"$RUN_DIR/08d-instance-config.log" 2>&1 || true
    lxc exec "$instance_name" -- ps aux >"$RUN_DIR/08e-ps-aux.log" 2>&1 || true
    lxc exec "$instance_name" -- cat /proc/1/cmdline >"$RUN_DIR/08f-pid1-cmdline.log" 2>&1 || true
    # `lxc info --show-log` is actually liblxc's own C-level trace/debug
    # log (the "Log:" section) -- NOT the container's PID 1 console/tty
    # output, despite this function's original name for it. It explains
    # *lxc-internal* start/stop mechanics (namespace setup, mount, cgroup,
    # "child ended on error N") even when the instance is stopped, no
    # `lxc exec` required.
    lxc info "$instance_name" --show-log >"$RUN_DIR/08g-lxc-trace-log.log" 2>&1 || true
    # `lxc console --show-log` is the *actual* PID 1 console/tty ring
    # buffer (`/var/snap/lxd/common/lxd/logs/<instance>/console.log`).
    # This is the one channel that would have caught the entrypoint
    # script's `exec {supervisor}` stdout/stderr if the `exec >file 2>&1`
    # redirect earlier in the generated script (see
    # instance.rs::build_entrypoint_script) hadn't been applied yet, or if
    # `/bin/sh` itself failed before reaching that line -- found the hard
    # way when a `{ ... } >file` *compound-command* redirect (rather than
    # a standalone `exec >file`) silently stopped applying right before
    # the final `exec` into the supervisor, and neither this log nor
    # 08k-var-log/openshell-entrypoint.log below caught the supervisor's
    # own exit(1) because of it.
    lxc console "$instance_name" --show-log >"$RUN_DIR/08g-console-log.log" 2>&1 || true
    local lxc_daemon_log="/var/snap/lxd/common/lxd/logs/${instance_name}/lxc.log"
    if sudo test -s "$lxc_daemon_log" 2>/dev/null; then
        sudo cat "$lxc_daemon_log" >"$RUN_DIR/08h-lxc-daemon-log.log" 2>&1 || true
    else
        echo "(per-instance daemon log at $lxc_daemon_log is missing or empty)" >"$RUN_DIR/08h-lxc-daemon-log.log"
    fi
    # The main daemon log logs container start/stop/exec activity at
    # normal (non-debug) verbosity too, unlike the per-instance log above
    # -- grep it for anything mentioning this specific instance rather
    # than dumping the whole (multi-container-shared) file.
    sudo grep -F "$instance_name" /var/snap/lxd/common/lxd/logs/lxd.log 2>/dev/null \
        >"$RUN_DIR/08j-lxd-daemon-log-filtered.log" || true
    # The supervisor's own tracing output goes to a rolling file under
    # /var/log inside the *container* (crates/openshell-sandbox/src/main.rs:
    # openshell.<date>.log, plus a separate openshell-ocsf.<date>.log), not
    # to the console -- confirmed empty above. This is the actual
    # application-level log, not just "did LXC exec it".
    #
    # CORRECTION: `lxc file pull -r .../var/log <dest>` (an earlier version
    # of this) aborts entirely the first time it hits a file it can't
    # recreate locally -- stock Ubuntu ships /var/log/README as a symlink
    # to /usr/share/doc/systemd/README.logs, and recreating that symlink on
    # the host side failed with "permission denied", aborting the whole
    # pull before it ever reached our actual log files. Go straight to the
    # storage pool's host-side rootfs path instead (this driver is pinned
    # to the "dir" backend throughout -- see config.rs's own doc comment on
    # DEFAULT_STORAGE_POOL), copying only the specific files this driver
    # cares about rather than a whole directory tree full of unrelated
    # symlinks.
    local storage_rootfs="/var/snap/lxd/common/lxd/storage-pools/default/containers/${instance_name}/rootfs"
    mkdir -p "$RUN_DIR/08k-var-log"
    if sudo test -d "${storage_rootfs}/var/log" 2>/dev/null; then
        sudo find "${storage_rootfs}/var/log" -maxdepth 1 -name 'openshell*.log' \
            -exec sudo cp {} "$RUN_DIR/08k-var-log/" \; \
            >"$RUN_DIR/08k-var-log-pull.log" 2>&1 || true
        # sudo cp leaves these root-owned; fix that so they're readable the
        # same way every other file in $RUN_DIR already is.
        sudo chown "$(id -u):$(id -g)" "$RUN_DIR"/08k-var-log/*.log 2>/dev/null || true
    else
        echo "(no /var/log found at ${storage_rootfs}/var/log)" >"$RUN_DIR/08k-var-log-pull.log"
    fi
    # The kernel logs verbose BPF/seccomp verifier rejection details (well
    # beyond a syscall's bare errno) to the ring buffer, not to any
    # per-process or per-container log -- relevant here because
    # `seccomp(SECCOMP_SET_MODE_FILTER)` failing with a bare EINVAL and no
    # other context is exactly the failure mode under investigation as of
    # this comment (see openshell-driver-lxd's README "What's actually
    # implemented" section). This is the *host* VM's dmesg, not the
    # container's -- LXD containers share the host kernel, so a rejection
    # triggered by a syscall made inside the container still logs here.
    # CAP_SYSLOG is in this driver's raw.lxc capability keep-list, but that
    # only matters for reading dmesg *from inside* a running container,
    # which is moot once PID 1 has already exited and the instance stopped.
    dmesg -T 2>&1 | tail -100 >"$RUN_DIR/08l-host-dmesg-tail.log" || \
        sudo dmesg -T 2>&1 | tail -100 >"$RUN_DIR/08l-host-dmesg-tail.log" || \
        echo "(dmesg unavailable or empty)" >"$RUN_DIR/08l-host-dmesg-tail.log"
    echo "    pid1 cmdline: $(tr '\0' ' ' <"$RUN_DIR/08f-pid1-cmdline.log" 2>/dev/null)" >&2
    echo "$instance_name"
}

log_section "Step 8: sandbox lifecycle (create -> exec -> delete)"
SANDBOX_NAME="lxd-stage2-$$"
LIFECYCLE_OUTCOME="not-run"

CREATE_OUTPUT=""
if CREATE_OUTPUT="$(timeout 120 "$CLI_BIN" sandbox create --name "$SANDBOX_NAME" -- echo stage2-create-ok 2>&1)"; then
    echo "$CREATE_OUTPUT" >"$RUN_DIR/08a-sandbox-create.log"
    echo "==> sandbox create succeeded:"
    echo "$CREATE_OUTPUT"

    EXEC_OUTPUT=""
    if EXEC_OUTPUT="$(timeout 60 "$CLI_BIN" sandbox exec --name "$SANDBOX_NAME" -- echo stage2-exec-ok 2>&1)"; then
        echo "$EXEC_OUTPUT" >"$RUN_DIR/08b-sandbox-exec.log"
        echo "==> sandbox exec succeeded:"
        echo "$EXEC_OUTPUT"
        if echo "$EXEC_OUTPUT" | grep -q "stage2-exec-ok"; then
            LIFECYCLE_OUTCOME="pass"
        else
            LIFECYCLE_OUTCOME="exec-ran-but-output-unexpected"
        fi
    else
        echo "$EXEC_OUTPUT" >"$RUN_DIR/08b-sandbox-exec.log"
        echo "ERROR: sandbox exec failed; see $RUN_DIR/08b-sandbox-exec.log" >&2
        dump_instance_diagnostics "$SANDBOX_NAME" >/dev/null || true
        LIFECYCLE_OUTCOME="exec-failed"
    fi

    timeout 60 "$CLI_BIN" sandbox delete "$SANDBOX_NAME" \
        >"$RUN_DIR/08c-sandbox-delete.log" 2>&1 \
        || echo "WARNING: sandbox delete failed; see $RUN_DIR/08c-sandbox-delete.log (leftover LXD instance may need manual cleanup: lxc delete --force <name>)" >&2
else
    echo "$CREATE_OUTPUT" >"$RUN_DIR/08a-sandbox-create.log"
    echo "ERROR: sandbox create failed; see $RUN_DIR/08a-sandbox-create.log" >&2
    STUCK_INSTANCE="$(dump_instance_diagnostics "$SANDBOX_NAME" || true)"
    LIFECYCLE_OUTCOME="create-failed"
fi

echo "==> Lifecycle outcome: $LIFECYCLE_OUTCOME"

# Best-effort cleanup of whatever LXD instance this run created, even on
# failure -- create-failed above deliberately does NOT run `sandbox
# delete` (the CLI/gateway path is exactly what's broken), so without this
# every failed run leaks one real LXD container. On this VM's 30G disk,
# across a debugging session with several failed runs, that adds up.
if [ "$LIFECYCLE_OUTCOME" = "create-failed" ] && [ -n "${STUCK_INSTANCE:-}" ]; then
    echo "==> Cleaning up stuck instance '$STUCK_INSTANCE' directly via lxc (bypassing the broken CLI path)"
    lxc delete --force "$STUCK_INSTANCE" >"$RUN_DIR/08i-cleanup-stuck-instance.log" 2>&1 || true
fi

# ── Results file ─────────────────────────────────────────────────────────────

{
    echo "# LXD driver Stage 2 run: $TIMESTAMP"
    echo ""
    echo "Produced by \`crates/openshell-driver-lxd/hack/run-stage2.sh\`. Raw logs"
    echo "in \`results/stage2-$TIMESTAMP/\`."
    echo ""
    echo "## Config"
    echo ""
    echo '```'
    echo "Base mode:       $BASE_MODE"
    echo "Ubuntu image:    $UBUNTU_IMAGE"
    echo "OCI image ref:   $IMAGE_REF"
    echo "Image alias:     $IMAGE_ALIAS"
    echo "Bridge subnet:   $BRIDGE_SUBNET (gateway IP: $BRIDGE_GATEWAY_IP)"
    echo "Gateway port:    $GATEWAY_PORT"
    echo "Driver socket:   $DRIVER_UDS"
    echo '```'
    echo ""
    echo "## Outcome: \`$LIFECYCLE_OUTCOME\`"
    echo ""
    echo "Known simplifications this run does NOT prove anything about --"
    echo "see this script's own header comment for the full list:"
    echo "- No TLS/mTLS (the driver has no mTLS delivery mechanism yet)."
    echo "- Bypasses \`_gateway.lxd\`/\`GetGatewayListenerRequirements\` entirely"
    echo "  by using a direct, pre-known bridge IP."
    echo "- The image conversion recipe (metadata.yaml + directory import) is"
    echo "  based on LXD's documented format, not prior verified use here."
    echo ""
    echo "## Logs"
    echo ""
    echo "- \`02-lxc-image-copy.log\` + \`02d\`-\`02g\` (ubuntu mode, incl. baking in a 'sandbox' user) or \`02a-skopeo-copy.log\`/\`02b-umoci-unpack.log\`/\`02c-lxc-image-import.log\` (oci mode) -- base image prep (if not skipped)"
    echo "- \`03-build.log\`, \`03b-build-gateway.log\` -- cargo build (if not skipped)"
    echo "- \`driver.log\`, \`gateway.log\` -- process logs, captured live"
    echo "- \`08a-sandbox-create.log\`, \`08b-sandbox-exec.log\`, \`08c-sandbox-delete.log\` -- CLI lifecycle output"
    echo "- \`08d-instance-config.log\`, \`08e-ps-aux.log\`, \`08f-pid1-cmdline.log\`, \`08g-lxc-trace-log.log\`, \`08g-console-log.log\`, \`08h-lxc-daemon-log.log\`, \`08j-lxd-daemon-log-filtered.log\`, \`08k-var-log/\`, \`08l-host-dmesg-tail.log\` -- LXD instance diagnostics (if create or exec failed)"
} >"$RESULTS_MD"
echo "==> Results written to $RESULTS_MD"

log_section "Done"
echo "Outcome: $LIFECYCLE_OUTCOME"
if [ "$LIFECYCLE_OUTCOME" != "pass" ]; then
    exit 1
fi
exit 0
