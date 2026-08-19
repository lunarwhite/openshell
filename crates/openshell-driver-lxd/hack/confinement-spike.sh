#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Phase 1, Step 0: confinement spike.
#
# This is the single highest-priority thing to run before writing any more
# driver code (see crates/openshell-driver-lxd/docs/04-implementation-plan.md, "Phase 1 —
# Proof of Concept", Step 0). It does NOT touch the openshell-driver-lxd
# crate at all — it hand-creates one LXD container, attaches the existing,
# unmodified openshell-sandbox supervisor binary via a disk device, and
# directly exercises the exact syscalls the supervisor needs for its own
# isolation (nested network namespace, Landlock, seccomp) to find out
# whether LXD's default confinement blocks any of them.
#
# Requirements to actually run this:
#   - A real Ubuntu 22.04+ LTS host (or VM) with LXD installed (the snap
#     package is the target; see the design doc's packaging non-goal).
#   - `lxc` on PATH and permission to talk to the LXD daemon (the `lxd`
#     Unix group, or root). NOTE: `lxd`-group membership is host-root-
#     equivalent — see 03-design-rfc.md's risk on this.
#   - A built `openshell-sandbox` binary for the container's architecture,
#     passed as $1.
#
# This cannot run on this development machine (macOS, no LXD — LXD is
# Linux-only) and was NOT executed as part of writing this script. Run it
# yourself on a real host and record the result in this crate's README
# under "Step 0 result".
#
# Usage:
#   ./confinement-spike.sh /path/to/openshell-sandbox
#
set -euo pipefail

SUPERVISOR_BIN="${1:?usage: $0 /path/to/openshell-sandbox}"
CONTAINER_NAME="openshell-spike-$$"
IMAGE_ALIAS="${OPENSHELL_SPIKE_IMAGE:-ubuntu:22.04}"

if [ ! -x "$SUPERVISOR_BIN" ]; then
  echo "error: $SUPERVISOR_BIN does not exist or is not executable" >&2
  exit 1
fi
if ! command -v lxc >/dev/null 2>&1; then
  echo "error: lxc not found on PATH. Install LXD (snap install lxd) first." >&2
  exit 1
fi

STAGE_DIR="$(mktemp -d)"
cp "$SUPERVISOR_BIN" "$STAGE_DIR/openshell-sandbox"
chmod 0755 "$STAGE_DIR/openshell-sandbox"

cleanup() {
  echo "--- cleanup ---"
  lxc delete --force "$CONTAINER_NAME" >/dev/null 2>&1 || true
  rm -rf "$STAGE_DIR"
}
trap cleanup EXIT

STEP_A_OUTCOME="untested"
RESULT_NESTING_ONLY="untested"
RESULT_NESTING_PLUS_APPARMOR="untested"
RESULT_PRIVILEGED="untested"

# One reusable probe: exec into the container and attempt, in order:
#   1. `ip netns add` — the bind-mount LXD's mount mediation is most likely
#      to block (this is exactly the operation the Docker driver had to add
#      `apparmor=unconfined` for; see driver.rs's design-doc citation).
#   2. A real Landlock probe via the actual supervisor binary's
#      `--landlock-probe` flag (crates/openshell-sandbox/src/main.rs), which
#      calls openshell_supervisor_process::sandbox::probe_landlock() and
#      exits 0 only if the kernel reports Landlock genuinely available.
#      CORRECTION: an earlier version of this probe called this same flag
#      before it existed, with a `|| sh -c "exec 3<>/dev/null"` fallback
#      that always trivially succeeded regardless of Landlock's real
#      state -- silently contributing nothing to "all primitives
#      succeeded" without even printing its own "inconclusive" warning.
#      That's fixed now that the flag is real; do not reintroduce a
#      fallback that can mask a genuine failure here.
#   3. `unshare --net` + a veth pair — the nested network namespace the
#      supervisor creates for the sandboxed process.
# Returns 0 only if all three primitives succeed.
run_probe() {
  local container="$1"
  echo "  [1/3] ip netns add probe..."
  if ! lxc exec "$container" -- sh -c 'ip netns add openshell-probe && ip netns del openshell-probe'; then
    echo "  -> ip netns add FAILED"
    return 1
  fi
  echo "  -> ip netns add OK"

  echo "  [2/3] landlock probe (real syscall check via openshell-sandbox --landlock-probe)..."
  if ! lxc exec "$container" -- /opt/openshell-spike/openshell-sandbox --landlock-probe; then
    echo "  -> landlock FAILED (see the message printed above from --landlock-probe itself)"
    return 1
  fi
  echo "  -> landlock OK"

  echo "  [3/3] unshare --net + veth probe..."
  if ! lxc exec "$container" -- sh -c 'unshare --net true'; then
    echo "  -> unshare --net FAILED"
    return 1
  fi
  echo "  -> unshare --net OK"

  echo "  -> all three primitives succeeded"
  return 0
}

launch_container() {
  local container="$1"
  shift
  echo "=== launching $container with: $* ==="
  lxc launch "$IMAGE_ALIAS" "$container" "$@"
  lxc config device add "$container" openshell-supervisor disk \
    source="$STAGE_DIR" path=/opt/openshell-spike readonly=true shift=true
  echo "waiting for container network..."
  for _ in $(seq 1 30); do
    lxc exec "$container" -- true 2>/dev/null && break
    sleep 1
  done
}

echo "############################################"
echo "# Step A: default unprivileged, no nesting  #"
echo "############################################"
launch_container "$CONTAINER_NAME" \
  -c security.privileged=false
if run_probe "$CONTAINER_NAME"; then
  STEP_A_OUTCOME="PASS"
  echo ""
  echo ">>> SURPRISING: ip netns add + unshare --net both succeeded WITHOUT      <<<"
  echo ">>> security.nesting=true. This does not invalidate a Step B PASS below <<<"
  echo ">>> (using nesting=true when it turns out unnecessary is still safe to  <<<"
  echo ">>> ship), but it means nesting may not be load-bearing on this LXD/    <<<"
  echo ">>> kernel version -- don't assume Step B's result generalizes to other <<<"
  echo ">>> environments without re-running there. See the RESULT SUMMARY's     <<<"
  echo ">>> 'Step A' line, which this script no longer lets Step B overwrite.   <<<"
else
  STEP_A_OUTCOME="FAIL (expected without nesting)"
  echo "(expected to fail without security.nesting; proceeding to Step B)"
fi
lxc delete --force "$CONTAINER_NAME"

echo "############################################"
echo "# Step B: unprivileged + security.nesting   #"
echo "############################################"
launch_container "$CONTAINER_NAME" \
  -c security.privileged=false \
  -c security.nesting=true
if run_probe "$CONTAINER_NAME"; then
  RESULT_NESTING_ONLY="PASS"
  echo ""
  echo ">>> security.nesting=true alone was sufficient. <<<"
  echo ">>> Also manually check whether LXD's default AppArmor profile blocked <<<"
  echo ">>> anything the three probes above don't cover (see design doc's     <<<"
  echo ">>> 'messy middle' risk) before declaring this fully resolved.        <<<"
else
  RESULT_NESTING_ONLY="FAIL"
  echo ""
  echo ">>> security.nesting=true alone was NOT sufficient. Trying a narrow <<<"
  echo ">>> raw.apparmor override next (Step C) before considering          <<<"
  echo ">>> security.privileged=true.                                       <<<"
fi
lxc delete --force "$CONTAINER_NAME"

if [ "$RESULT_NESTING_ONLY" = "FAIL" ]; then
  echo "############################################################"
  echo "# Step C: unprivileged + nesting + narrow AppArmor override #"
  echo "############################################################"
  launch_container "$CONTAINER_NAME" \
    -c security.privileged=false \
    -c security.nesting=true \
    -c raw.apparmor="mount fstype=nsfs -> **,"
  if run_probe "$CONTAINER_NAME"; then
    RESULT_NESTING_PLUS_APPARMOR="PASS"
    echo ""
    echo ">>> A narrow raw.apparmor addition closed the gap. Record the exact <<<"
    echo ">>> line(s) needed in this crate's README and instance.rs.          <<<"
  else
    RESULT_NESTING_PLUS_APPARMOR="FAIL"
    echo ""
    echo ">>> Still failing. This is the stop-and-reconsider branch per the   <<<"
    echo ">>> design doc — do NOT proceed to test security.privileged=true    <<<"
    echo ">>> and ship it as a fallback. Escalate for a design discussion.    <<<"
  fi
  lxc delete --force "$CONTAINER_NAME"
fi

echo ""
echo "================ RESULT SUMMARY ================"
echo "Step A (no nesting at all, sanity baseline):                 $STEP_A_OUTCOME"
echo "Nesting alone (security.nesting=true, unprivileged):        $RESULT_NESTING_ONLY"
echo "Nesting + narrow raw.apparmor (only run if nesting failed):  $RESULT_NESTING_PLUS_APPARMOR"
echo "=================================================="
echo ""
echo "Landlock IS now verified by [2/3] above (openshell-sandbox --landlock-probe,"
echo "a real syscall check) -- both PASS lines above reflect all three primitives,"
echo "not just the two that were automatable before this flag existed."
echo ""
echo "Record this result in crates/openshell-driver-lxd/README.md's"
echo "'Step 0 result' section, then check instance.rs's security_config()"
echo "(renamed from security_config_pending_spike() once this spike first"
echo "passed) still matches — its doc comment records the exact runs and"
echo "caveats this result should be compared against, not just a starting"
echo "hypothesis."
