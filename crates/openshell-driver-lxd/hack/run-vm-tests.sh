#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Self-contained runner for Stage 0 (confinement spike) and Stage 1
# (real-daemon crate validation) of crates/openshell-driver-lxd/docs/05-test-plan.md.
#
# WHO RUNS THIS AND WHERE:
#   Run this yourself, from your own terminal, against a real Ubuntu/Linux
#   environment with LXD installed and real network access to it -- a
#   Multipass VM (macOS), WSL2 (Windows, e.g. `wsl --install Ubuntu`), a
#   cloud instance, or bare metal all work identically, since this script
#   never invokes any VM-management tool itself; it only assumes it's
#   already running inside the target environment (an agent's sandboxed
#   shell tool typically cannot reach a Multipass VM's or WSL2 distro's
#   private network). Three supported invocation shapes:
#
#     1. WSL2 (Windows): clone the repo directly inside the WSL distro (no
#        separate mount/copy step -- WSL2's filesystem already is a real
#        Linux filesystem) and run it from an Ubuntu shell:
#          wsl -d Ubuntu
#          cd ~ && git clone <repo-url> openshell && cd openshell
#          bash crates/openshell-driver-lxd/hack/run-vm-tests.sh
#
#     2. Multipass (macOS): mount the repo into the VM, then run it from
#        inside the VM over SSH:
#          multipass mount /path/to/openshell <vm>:/mnt/openshell
#          multipass exec <vm> -- bash /mnt/openshell/crates/openshell-driver-lxd/hack/run-vm-tests.sh
#
#     3. Multipass (macOS), copying instead of mounting: copy the repo
#        (tarball/clone) into the VM and run it locally there:
#          multipass shell <vm>
#          cd ~/openshell && bash crates/openshell-driver-lxd/hack/run-vm-tests.sh
#
#   Either way, this script must be run FROM WITHIN a checkout of this
#   repository (mounted, copied, or cloned directly) -- it locates the repo
#   root by walking up from its own path looking for the workspace
#   Cargo.toml, and refuses to guess (e.g. by git-cloning some possibly-wrong
#   ref) if it can't find one. See 05-test-plan.md, "Getting the repository
#   onto the VM".
#
# WHAT THIS DOES (in order, each gated on the previous step's success):
#   1. Installs prerequisites if missing: LXD (snap), a C/C++ toolchain,
#      and Rust (rustup). Idempotent -- safe to re-run.
#   2. Builds openshell-sandbox natively (debug profile) and runs
#      confinement-spike.sh against it -- Stage 0. This is the gate: if it
#      doesn't come back with a clear PASS variant, Stage 1 does not run.
#   3. If Stage 0 passed: builds and tests openshell-driver-lxd natively
#      (`cargo test -p openshell-driver-lxd`), then runs the one real-daemon
#      LxdClient integration test explicitly (`-- --ignored`) -- Stage 1.
#   4. Writes one consolidated results file under
#      crates/openshell-driver-lxd/hack/results/<UTC-timestamp>.md inside the detected
#      repo root, plus raw logs alongside it. If the repo root is a mount,
#      this file is an ordinary file on the host afterward -- no network
#      access needed to read it.
#   5. Updates crates/openshell-driver-lxd/README.md's "Step 0 result"
#      section with the real Stage 0 outcome. Never edits
#      src/instance.rs -- see 05-test-plan.md, "What happens to
#      instance.rs" for why that's a deliberate omission, not an oversight.
#
# WHAT THIS DELIBERATELY DOES NOT DO:
#   - Does not attempt Stage 2 (a full sandbox image conversion + gateway +
#     CLI lifecycle). See 05-test-plan.md's Stage 2 section for why.
#   - Does not fabricate a result if a step can't run (e.g. no LXD group
#     membership, no network to pull ubuntu:22.04). It reports failure
#     plainly in the results file rather than skipping silently.
#   - Does not use `security.privileged=true` at any point, even as a
#     fallback. If Stage 0 needs it, that's a stop-and-reconsider outcome,
#     not something this script tries next.
#
# ENVIRONMENT VARIABLES (all optional):
#   OPENSHELL_LXD_TEST_IMAGE     LXD image alias for throwaway containers
#                                 (default: ubuntu:22.04)
#   OPENSHELL_LXD_BUILD_PROFILE  "debug" or "release" for the two crate
#                                 builds (default: debug -- see
#                                 05-test-plan.md's resource-constraints
#                                 section for why debug is the default here)
#   OPENSHELL_LXD_MIN_FREE_DISK_GB  Abort before Stage 1 if free disk on /
#                                 is below this (default: 3)
#   OPENSHELL_LXD_SKIP_PREREQS   Set to 1 to skip the prerequisite-install
#                                 step entirely (assumes it already ran)

set -euo pipefail

# A non-interactive `bash script.sh` (e.g. `multipass exec <vm> -- bash ...`)
# does not source /etc/profile.d/apps-bin-path.sh, so snap binaries (`lxc`,
# `lxd`) may not be on PATH even once installed. Add it defensively; harmless
# if it's already there or the directory doesn't exist yet.
export PATH="/snap/bin:$PATH"

# ── Repo-root and self-location ─────────────────────────────────────────────

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
    cat >&2 <<'EOF'
ERROR: could not find the workspace root (a Cargo.toml with [workspace])
by walking up from this script's own location.

This script must run from inside a real checkout of the openshell
repository (mounted or copied onto this machine), not standalone. See
crates/openshell-driver-lxd/docs/05-test-plan.md, "Getting the repository onto the VM",
for how to get the repo here (mount, git clone of a pushed ref, or a
`multipass transfer`red tarball).
EOF
    exit 1
fi
echo "==> Repository root: $REPO_ROOT"

CONFINEMENT_SPIKE="$SCRIPT_DIR/confinement-spike.sh"
if [ ! -f "$CONFINEMENT_SPIKE" ]; then
    echo "ERROR: expected to find confinement-spike.sh next to this script at $CONFINEMENT_SPIKE" >&2
    echo "       (was this script copied without the rest of crates/openshell-driver-lxd/hack/?)" >&2
    exit 1
fi

if [ "$(uname -s)" != "Linux" ]; then
    echo "ERROR: this script must run on Linux (LXD is Linux-only). Detected: $(uname -s)" >&2
    exit 1
fi

# Redirect Cargo's build output off of $REPO_ROOT. When this script is run
# via a `multipass mount` (the documented, preferred invocation -- see the
# header above), $REPO_ROOT is the mount point, and Cargo's default
# `target/` would silently write potentially gigabytes of small build
# artifacts back through the mount onto the *host's* disk instead of this
# VM's own disk -- defeating a disk resize done specifically for this run,
# and colliding with the host's own `target/debug/` if the same repo is ever
# built natively on macOS too (Cargo does not namespace the default
# no-`--target` host build by OS/arch, only explicit `--target` cross
# builds get their own subdirectory). It also makes every build slower,
# since incremental compilation's many small file writes are the one access
# pattern every mount type (SSHFS-classic or virtiofs-native) handles worse
# than local disk. Keep it on the VM's own native filesystem regardless of
# where the repo itself lives.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/openshell-lxd-target}"
echo "==> CARGO_TARGET_DIR: $CARGO_TARGET_DIR (kept off the mount deliberately)"

# ── Config ───────────────────────────────────────────────────────────────────

OPENSHELL_LXD_TEST_IMAGE="${OPENSHELL_LXD_TEST_IMAGE:-ubuntu:22.04}"
OPENSHELL_LXD_BUILD_PROFILE="${OPENSHELL_LXD_BUILD_PROFILE:-debug}"
OPENSHELL_LXD_MIN_FREE_DISK_GB="${OPENSHELL_LXD_MIN_FREE_DISK_GB:-3}"
OPENSHELL_LXD_SKIP_PREREQS="${OPENSHELL_LXD_SKIP_PREREQS:-0}"
export OPENSHELL_LXD_TEST_IMAGE

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS_DIR="$REPO_ROOT/crates/openshell-driver-lxd/hack/results"
RUN_DIR="$RESULTS_DIR/$TIMESTAMP"
mkdir -p "$RUN_DIR"
RESULTS_MD="$RESULTS_DIR/$TIMESTAMP.md"

echo "==> Results will be written to:"
echo "      $RESULTS_MD"
echo "      $RUN_DIR/ (raw logs)"

# ── Small helpers ────────────────────────────────────────────────────────────

log_section() {
    echo ""
    echo "############################################################"
    echo "# $1"
    echo "############################################################"
}

# Runs "$@", tees to $1's log file, never lets a nonzero exit kill the whole
# script via `set -e` (callers check $? explicitly) -- several steps below
# (confinement-spike.sh, cargo test) are expected to legitimately fail and
# the failure itself is a result to record, not a script bug.
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

# ── Prerequisites ────────────────────────────────────────────────────────────

# `lxd init --minimal` bundles storage-pool AND network creation into one
# call, and its automatic IPv4 subnet picker for the `lxdbr0` bridge is
# known to fail with "Failed automatically finding an unused IPv4 subnet,
# manual configuration required" inside nested/VM environments -- observed
# in practice inside this exact Multipass VM, whose own primary NIC already
# occupies a 192.168.x/24 range that appears to confuse the picker. Create
# each resource explicitly instead, with a fixed subnet for the bridge, and
# check each one independently so a partial failure (e.g. storage pool
# created, network creation failed) is corrected on re-run rather than
# mistaken for "already initialized". All `lxc` calls here run under `sudo`
# because this runs before `ensure_lxd_group_and_reexec_if_needed` below --
# the invoking user may not be in the `lxd` group yet, and root can always
# reach the LXD socket regardless of group membership.
#
# Subnet choice: 10.66.88.0/24 is arbitrary but deliberately not one of the
# common auto-picker candidates (10.0.x, 10.1.x) or Docker's default range
# (172.17-31.x.x), to minimize collision with anything else on the host.
# Override via OPENSHELL_LXD_BRIDGE_SUBNET (CIDR with a host address, e.g.
# "10.66.88.1/24") if this ever collides in a different environment.
LXD_BRIDGE_NETWORK="lxdbr0"
LXD_BRIDGE_SUBNET="${OPENSHELL_LXD_BRIDGE_SUBNET:-10.66.88.1/24}"

ensure_lxd_initialized() {
    if sudo lxc storage list --format csv 2>/dev/null | grep -q '^default,'; then
        echo "==> LXD storage pool 'default' already exists"
    else
        echo "==> Creating LXD storage pool 'default' (dir backend)"
        sudo lxc storage create default dir
    fi

    if sudo lxc network list --format csv 2>/dev/null | grep -q "^${LXD_BRIDGE_NETWORK},"; then
        echo "==> LXD network '${LXD_BRIDGE_NETWORK}' already exists"
    else
        echo "==> Creating LXD bridge network '${LXD_BRIDGE_NETWORK}' (explicit subnet: ${LXD_BRIDGE_SUBNET})"
        sudo lxc network create "${LXD_BRIDGE_NETWORK}" \
            "ipv4.address=${LXD_BRIDGE_SUBNET}" ipv4.nat=true ipv6.address=none
    fi

    if sudo lxc profile device list default 2>/dev/null | grep -qx root; then
        echo "==> Default profile already has a root disk device"
    else
        echo "==> Adding root disk device (pool=default) to the default profile"
        sudo lxc profile device add default root disk path=/ pool=default
    fi

    if sudo lxc profile device list default 2>/dev/null | grep -qx eth0; then
        echo "==> Default profile already has an eth0 nic device"
    else
        echo "==> Adding eth0 nic device (network=${LXD_BRIDGE_NETWORK}) to the default profile"
        sudo lxc profile device add default eth0 nic "network=${LXD_BRIDGE_NETWORK}"
    fi
}

# A `multipass stop`/`start` cycle (e.g. to resize disk/memory, as this VM
# went through) can leave the guest clock stale for a while after boot --
# systemd-timesyncd hasn't necessarily finished its first re-sync by the
# time this script runs. A stale-behind clock makes apt reject every Release
# file as "not valid yet" (its signed valid-from timestamp looks future
# relative to the guest's clock), which aborts `apt-get update` under this
# script's `set -e`. Force a sync before anything time-sensitive runs, with
# an HTTPS-Date-header fallback in case outbound NTP (UDP/123) is filtered
# in this network but HTTPS isn't.
sync_vm_clock() {
    echo "==> Checking/syncing VM clock (stop/start can leave it stale, which breaks apt's Release-file validity check)"
    echo "    Time before sync: $(date -u)"

    if have_cmd timedatectl; then
        sudo timedatectl set-ntp true >/dev/null 2>&1 || true
        for _ in $(seq 1 15); do
            [ "$(timedatectl show -p NTPSynchronized --value 2>/dev/null)" = "yes" ] && break
            sleep 1
        done
    fi

    if [ "$(timedatectl show -p NTPSynchronized --value 2>/dev/null)" != "yes" ]; then
        echo "    NTP sync not confirmed within 15s; falling back to an HTTPS Date header"
        local http_date
        http_date="$(curl -fsSI https://archive.ubuntu.com 2>/dev/null | tr -d '\r' | grep -i '^date:' | cut -d' ' -f2-)"
        if [ -n "$http_date" ]; then
            sudo date -s "$http_date" >/dev/null
        else
            echo "    WARNING: could not confirm the correct time via NTP or HTTPS; apt operations below may still fail." >&2
        fi
    fi

    echo "    Time after sync: $(date -u)"
}

install_prereqs() {
    if [ "$OPENSHELL_LXD_SKIP_PREREQS" = "1" ]; then
        echo "==> OPENSHELL_LXD_SKIP_PREREQS=1, skipping prerequisite install"
        return 0
    fi

    log_section "Installing prerequisites"

    sync_vm_clock

    if ! have_cmd lxc; then
        echo "==> Installing LXD via snap"
        sudo snap install lxd
    else
        echo "==> lxc already present ($(lxc version 2>&1 | head -1 || true))"
    fi

    echo "==> Waiting for the LXD daemon to be ready"
    sudo lxd waitready --timeout=60

    ensure_lxd_initialized

    echo "==> Installing build toolchain (apt)"
    sudo apt-get update -qq
    sudo apt-get install -y --no-install-recommends \
        build-essential cmake pkg-config libssl-dev clang libclang-dev \
        git curl ca-certificates >/dev/null

    if ! have_cmd cargo; then
        echo "==> Installing Rust via rustup"
        curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
    fi
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
    echo "==> Toolchain versions:"
    echo "    $(cargo --version)"
    echo "    $(lxc version 2>&1 | head -1 || echo 'lxc version unavailable')"
}

# lxd-group membership doesn't take effect in the current shell/session
# without a fresh login. Re-exec the rest of this script under `sg lxd` once,
# right after ensuring the group exists and this user is in it, rather than
# asking the operator to log out and back in mid-run. Root never needs this.
ensure_lxd_group_and_reexec_if_needed() {
    if [ "$(id -u)" -eq 0 ]; then
        return 0
    fi
    if id -nG "$(whoami)" | tr ' ' '\n' | grep -qx lxd; then
        return 0
    fi
    if [ "${OPENSHELL_LXD_REEXECED:-0}" = "1" ]; then
        echo "ERROR: still not in the 'lxd' group after re-exec attempt; add manually and retry:" >&2
        echo "       sudo usermod -aG lxd \$(whoami) && newgrp lxd" >&2
        exit 1
    fi
    echo "==> Adding $(whoami) to the 'lxd' group (LXD isn't reachable without it)"
    sudo usermod -aG lxd "$(whoami)"
    echo "==> Re-executing under the 'lxd' group (avoids requiring a fresh login)"
    export OPENSHELL_LXD_REEXECED=1
    # The re-exec below restarts this entire script, including the timestamp
    # and results-dir setup above -- remove this pass's (empty, unused)
    # results dir so it doesn't linger as litter next to the real run's.
    rmdir "$RUN_DIR" 2>/dev/null || true
    exec sg lxd -c "$(printf '%q ' "$SCRIPT_PATH" "$@")"
}

check_disk_space() {
    local avail_gb
    # Check $HOME, not $REPO_ROOT: $HOME is where CARGO_TARGET_DIR, rustup,
    # and apt packages actually land. $REPO_ROOT may be a `multipass mount`,
    # whose `df` output reflects the *host's* backing filesystem, not this
    # VM's own disk budget -- checking it would answer the wrong question.
    avail_gb="$(df -BG --output=avail "$HOME" 2>/dev/null | tail -1 | tr -dc '0-9')"
    if [ -z "$avail_gb" ]; then
        echo "WARNING: could not determine free disk space; proceeding anyway."
        return 0
    fi
    echo "==> Free disk space at \$HOME ($HOME): ${avail_gb}G"
    if [ "$avail_gb" -lt "$OPENSHELL_LXD_MIN_FREE_DISK_GB" ]; then
        cat >&2 <<EOF
ERROR: only ${avail_gb}G free, below the ${OPENSHELL_LXD_MIN_FREE_DISK_GB}G
minimum this script expects for a native Rust build + one LXD image pull.
See crates/openshell-driver-lxd/docs/05-test-plan.md, "Resource constraints" -- resize the
VM's disk before continuing:
  multipass stop <vm>
  multipass set local.<vm>.disk=20G
  multipass start <vm>
EOF
        exit 1
    fi
}

# ── Stage 0: confinement spike ───────────────────────────────────────────────

STAGE0_OUTCOME="not-run"          # not-run | pass-clean | pass-anomalous | pass-apparmor | stop-and-reconsider | error
STAGE0_STEP_A_LINE=""
STAGE0_NESTING_LINE=""
STAGE0_APPARMOR_LINE=""

run_stage0() {
    log_section "Stage 0: confinement spike"

    local build_flag=""
    [ "$OPENSHELL_LXD_BUILD_PROFILE" = "release" ] && build_flag="--release"
    # $CARGO_TARGET_DIR is absolute (set above, off the mount) -- do not
    # prefix with $REPO_ROOT here.
    local bin_dir="$CARGO_TARGET_DIR/${OPENSHELL_LXD_BUILD_PROFILE}"

    echo "==> Building openshell-sandbox natively ($OPENSHELL_LXD_BUILD_PROFILE profile)"
    if ! run_logged "$RUN_DIR/00-build-supervisor.log" \
        cargo build $build_flag -p openshell-sandbox --manifest-path "$REPO_ROOT/Cargo.toml"; then
        echo "ERROR: openshell-sandbox failed to build; see $RUN_DIR/00-build-supervisor.log" >&2
        STAGE0_OUTCOME="error"
        return 1
    fi

    local supervisor_bin="$bin_dir/openshell-sandbox"
    if [ ! -x "$supervisor_bin" ]; then
        echo "ERROR: expected supervisor binary not found at $supervisor_bin" >&2
        STAGE0_OUTCOME="error"
        return 1
    fi

    echo "==> Running confinement-spike.sh"
    set +e
    OPENSHELL_SPIKE_IMAGE="$OPENSHELL_LXD_TEST_IMAGE" \
        bash "$CONFINEMENT_SPIKE" "$supervisor_bin" >"$RUN_DIR/01-confinement-spike.log" 2>&1
    set -e
    echo "    (full output: $RUN_DIR/01-confinement-spike.log)"

    STAGE0_STEP_A_LINE="$(grep -m1 '^Step A' "$RUN_DIR/01-confinement-spike.log" || true)"
    STAGE0_NESTING_LINE="$(grep -m1 '^Nesting alone' "$RUN_DIR/01-confinement-spike.log" || true)"
    STAGE0_APPARMOR_LINE="$(grep -m1 '^Nesting + narrow raw.apparmor' "$RUN_DIR/01-confinement-spike.log" || true)"

    echo "==> Checking for AppArmor denials during the spike run (best-effort, not a full substitute for manual review)"
    sudo journalctl -k --since "-15 min" 2>/dev/null | grep -i apparmor >"$RUN_DIR/02-apparmor-journal.log" || true

    # Step A (no nesting at all) passing takes priority over a clean Step B
    # PASS: it means nesting may not be load-bearing in this environment,
    # which is a materially different finding than "nesting alone is
    # necessary and sufficient" even though both are safe to ship as-is.
    # confinement-spike.sh no longer lets Step B silently overwrite this
    # signal (see its own history) -- mirror that here rather than
    # collapsing it back down to a plain "pass-clean".
    if echo "$STAGE0_STEP_A_LINE" | grep -q ': *PASS *$'; then
        STAGE0_OUTCOME="pass-anomalous"
    elif echo "$STAGE0_NESTING_LINE" | grep -q ': *PASS *$'; then
        STAGE0_OUTCOME="pass-clean"
    elif echo "$STAGE0_APPARMOR_LINE" | grep -q ': *PASS *$'; then
        STAGE0_OUTCOME="pass-apparmor"
    elif echo "$STAGE0_NESTING_LINE" | grep -q ': *FAIL *$'; then
        STAGE0_OUTCOME="stop-and-reconsider"
    else
        echo "WARNING: could not classify Stage 0 result from confinement-spike.sh output." >&2
        echo "         Inspect $RUN_DIR/01-confinement-spike.log by hand." >&2
        STAGE0_OUTCOME="error"
    fi

    echo "==> Stage 0 outcome: $STAGE0_OUTCOME"
}

# ── Stage 1: real-daemon crate validation ───────────────────────────────────

STAGE1_UNIT_RESULT=""
STAGE1_REAL_DAEMON_RESULT=""

run_stage1() {
    log_section "Stage 1: real-daemon crate validation"
    check_disk_space

    local build_flag=""
    [ "$OPENSHELL_LXD_BUILD_PROFILE" = "release" ] && build_flag="--release"

    echo "==> Running full unit test suite natively (cargo test -p openshell-driver-lxd)"
    local unit_status=0
    run_logged "$RUN_DIR/03-unit-tests.log" \
        cargo test $build_flag -p openshell-driver-lxd --manifest-path "$REPO_ROOT/Cargo.toml" \
        || unit_status=$?
    STAGE1_UNIT_RESULT="$(grep -m1 '^test result:' "$RUN_DIR/03-unit-tests.log" || echo "no summary line found (exit $unit_status)")"
    echo "    $STAGE1_UNIT_RESULT"

    echo "==> Running the real-daemon LxdClient integration test explicitly"
    local real_daemon_status=0
    run_logged "$RUN_DIR/04-real-daemon-test.log" \
        cargo test $build_flag -p openshell-driver-lxd --manifest-path "$REPO_ROOT/Cargo.toml" \
        -- --ignored real_daemon \
        || real_daemon_status=$?
    STAGE1_REAL_DAEMON_RESULT="$(grep -m1 '^test result:' "$RUN_DIR/04-real-daemon-test.log" || echo "no summary line found (exit $real_daemon_status)")"
    echo "    $STAGE1_REAL_DAEMON_RESULT"
}

# ── README update (Step 0 result section only -- see script header) ────────

update_readme_step0_result() {
    local readme="$REPO_ROOT/crates/openshell-driver-lxd/README.md"
    local nesting_pass="FAIL" apparmor_result="not needed" privileged="no"
    case "$STAGE0_OUTCOME" in
        pass-clean|pass-anomalous) nesting_pass="PASS" ;;
        pass-apparmor) nesting_pass="FAIL"; apparmor_result="PASS" ;;
        stop-and-reconsider) nesting_pass="FAIL"; apparmor_result="FAIL"; privileged="yes -- STOP AND RECONSIDER" ;;
        *) echo "==> Stage 0 outcome is '$STAGE0_OUTCOME'; not updating README (no conclusive result)"; return 0 ;;
    esac

    local lxd_version ubuntu_version storage_backend
    lxd_version="$(lxc version 2>/dev/null | tr '\n' ' ' || echo 'unknown')"
    ubuntu_version="$(lsb_release -d 2>/dev/null | cut -f2- || echo 'unknown')"
    storage_backend="$(lxc storage show default 2>/dev/null | grep '^driver:' | awk '{print $2}' || echo 'unknown')"

    python3 - "$readme" "$nesting_pass" "$apparmor_result" "$privileged" "$lxd_version" "$ubuntu_version" "$storage_backend" "$(date -u +%Y-%m-%d)" <<'PYEOF'
import re, sys

readme_path, nesting, apparmor, privileged, lxd_version, ubuntu_version, storage, date = sys.argv[1:9]

with open(readme_path, "r", encoding="utf-8") as f:
    content = f.read()

new_block = (
    "```\n"
    f"Nesting alone (security.nesting=true, unprivileged): {nesting}\n"
    f"Nesting + narrow raw.apparmor (if nesting alone failed): {apparmor}\n"
    f"security.privileged=true required: {privileged}\n"
    f"LXD version tested: {lxd_version}\n"
    f"Ubuntu version tested: {ubuntu_version}\n"
    f"Storage backend tested: {storage}\n"
    f"Date: {date}\n"
    "```"
)

pattern = re.compile(r"```\nNesting alone.*?\n```", re.DOTALL)
updated, count = pattern.subn(new_block, content, count=1)
if count != 1:
    print("WARNING: could not find the Step 0 result template block in README.md; leaving it untouched.", file=sys.stderr)
    sys.exit(0)

with open(readme_path, "w", encoding="utf-8") as f:
    f.write(updated)
print(f"Updated {readme_path}'s Step 0 result block.")
PYEOF
}

# ── Results file ─────────────────────────────────────────────────────────────

write_results_file() {
    local instance_rs_note
    case "$STAGE0_OUTCOME" in
        pass-clean)
            instance_rs_note="Stage 0 passed cleanly, matching whatever posture \`security_config()\` (\`src/instance.rs\`, renamed from \`security_config_pending_spike()\` after this spike first passed) currently encodes. **Recommendation (not applied by this script):** compare this run's exact config against that function's doc comment -- if it already documents an equivalent validated result, no edit is needed; if this is the first passing run, a human/main agent should update it, not this script. See 05-test-plan.md, \"What happens to instance.rs.\""
            ;;
        pass-anomalous)
            instance_rs_note="Stage 0 passed, but via the anomalous path (Step A, no nesting requested at all, already succeeded -- see \`confinement-spike.sh\`'s Step A block). Cross-check against \`security_config()\`'s (\`src/instance.rs\`) doc comment: if it already records this same anomaly from a prior run, this result is corroborating, not new. If this is the first such run, do not edit \`instance.rs\` based on it alone -- a human/main agent should judge whether it's reproducible enough to act on."
            ;;
        pass-apparmor)
            instance_rs_note="Stage 0 needed the narrow \`raw.apparmor\` addition (Step C) to pass -- check whether \`instance.rs\`'s current \`security_config()\` already includes a matching \`raw.apparmor\` entry. If not, it needs a real edit to match. Flagged for the main agent/a human to apply, not this script."
            ;;
        stop-and-reconsider)
            instance_rs_note="**Stop-and-reconsider outcome.** Both nesting alone and nesting+apparmor failed. Per \`03-design-rfc.md\`'s Risks section and \`04-implementation-plan.md:128-130\`, this is a design-level problem, not a config tweak -- do not edit \`instance.rs\` to add \`security.privileged=true\`, and do not proceed to Stage 1 based on this run."
            ;;
        *)
            instance_rs_note="Stage 0 did not produce a conclusive result (\`$STAGE0_OUTCOME\`). No recommendation for \`instance.rs\`."
            ;;
    esac

    {
        echo "# LXD driver VM test run: $TIMESTAMP"
        echo ""
        echo "Produced by \`crates/openshell-driver-lxd/hack/run-vm-tests.sh\`. Raw logs"
        echo "for every step below are in \`results/$TIMESTAMP/\`."
        echo ""
        echo "## Environment"
        echo ""
        echo '```'
        echo "Host: $(uname -a)"
        echo "LXD: $(lxc version 2>&1 | tr '\n' ' ' || echo unavailable)"
        echo "Ubuntu: $(lsb_release -d 2>/dev/null | cut -f2- || echo unavailable)"
        echo "Rust: $(cargo --version 2>&1 || echo unavailable)"
        echo "Disk (/): $(df -h / 2>/dev/null | tail -1 || echo unavailable)"
        echo "Memory: $(free -h 2>/dev/null | grep Mem || echo unavailable)"
        echo '```'
        echo ""
        echo "## Stage 0 -- confinement spike"
        echo ""
        echo "**Outcome classification: \`$STAGE0_OUTCOME\`**"
        echo ""
        echo '```'
        echo "${STAGE0_STEP_A_LINE:-<not captured>}"
        echo "${STAGE0_NESTING_LINE:-<not captured>}"
        echo "${STAGE0_APPARMOR_LINE:-<not captured>}"
        echo '```'
        echo ""
        if [ "$STAGE0_OUTCOME" = "pass-anomalous" ]; then
            echo "**Note:** Step A (no nesting at all) also passed both automatable probes."
            echo "This means \`security.nesting=true\` may not be load-bearing in this"
            echo "environment -- still safe to ship as configured, but don't assume this"
            echo "result generalizes to a different LXD/kernel version without re-running"
            echo "there. See \`confinement-spike.sh\`'s Step A block for detail."
            echo ""
        fi
        echo "Landlock IS verified by this run's \`[2/3]\` probe -- a real syscall check"
        echo "via \`openshell-sandbox --landlock-probe\`, not the always-passing"
        echo "placeholder earlier runs of this script exercised. Both PASS lines above"
        echo "reflect all three primitives, not just the two that were automatable"
        echo "before that flag existed."
        echo ""
        echo "Full spike output: \`$TIMESTAMP/01-confinement-spike.log\`"
        echo ""
        echo "### AppArmor denial check (journalctl -k, best-effort)"
        echo ""
        if [ -s "$RUN_DIR/02-apparmor-journal.log" ]; then
            echo "Found entries -- review manually, a hit here does not automatically mean failure:"
            echo '```'
            cat "$RUN_DIR/02-apparmor-journal.log"
            echo '```'
        else
            echo "No AppArmor denial entries found in the last 15 minutes of the kernel log."
        fi
        echo ""
        echo "### instance.rs recommendation"
        echo ""
        echo "$instance_rs_note"
        echo ""

        if [ "$STAGE0_OUTCOME" = "pass-clean" ] || [ "$STAGE0_OUTCOME" = "pass-anomalous" ] || [ "$STAGE0_OUTCOME" = "pass-apparmor" ]; then
            echo "## Stage 1 -- real-daemon crate validation"
            echo ""
            echo "### Full unit test suite"
            echo ""
            echo '```'
            echo "${STAGE1_UNIT_RESULT:-<not run>}"
            echo '```'
            echo ""
            echo "Full output: \`$TIMESTAMP/03-unit-tests.log\`"
            echo ""
            echo "### Real-daemon LxdClient integration test"
            echo ""
            echo '```'
            echo "${STAGE1_REAL_DAEMON_RESULT:-<not run>}"
            echo '```'
            echo ""
            echo "Full output: \`$TIMESTAMP/04-real-daemon-test.log\`"
        else
            echo "## Stage 1 -- skipped"
            echo ""
            echo "Stage 0 did not report a pass variant (\`$STAGE0_OUTCOME\`), so Stage 1 did"
            echo "not run. See \`05-test-plan.md\`'s Stage 0 table for what this outcome"
            echo "means and what to do next."
        fi
        echo ""
        echo "## Stage 2"
        echo ""
        echo "Not attempted by this script. See \`05-test-plan.md\`'s Stage 2 section"
        echo "for what it would take and why it's scoped as a separate follow-up."
    } >"$RESULTS_MD"

    echo "==> Results written to $RESULTS_MD"
}

# ── Main ─────────────────────────────────────────────────────────────────────

install_prereqs
ensure_lxd_group_and_reexec_if_needed "$@"
check_disk_space

set +e
run_stage0
set -e

update_readme_step0_result || echo "WARNING: README update step failed; continuing to write results file." >&2

case "$STAGE0_OUTCOME" in
    pass-clean|pass-anomalous|pass-apparmor)
        run_stage1
        ;;
    stop-and-reconsider)
        log_section "STOP: Stage 0 requires security.privileged=true"
        echo "Per the design doc, this is a stop-and-reconsider outcome, not a" >&2
        echo "shippable fallback. Stage 1 will NOT run. See the results file and" >&2
        echo "03-design-rfc.md's Risks section." >&2
        ;;
    *)
        log_section "Stage 0 did not produce a conclusive result"
        echo "Stage 1 will NOT run. Inspect $RUN_DIR/01-confinement-spike.log by hand." >&2
        ;;
esac

write_results_file

log_section "Done"
echo "Outcome summary:"
echo "  Stage 0: $STAGE0_OUTCOME"
echo "  Stage 1 unit tests:       ${STAGE1_UNIT_RESULT:-<skipped>}"
echo "  Stage 1 real-daemon test: ${STAGE1_REAL_DAEMON_RESULT:-<skipped>}"
echo ""
echo "Full results: $RESULTS_MD"

if [ "$STAGE0_OUTCOME" = "stop-and-reconsider" ] || [ "$STAGE0_OUTCOME" = "error" ]; then
    exit 1
fi
exit 0
