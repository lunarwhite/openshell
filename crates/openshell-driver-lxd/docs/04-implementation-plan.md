# Implementation Plan: Native LXD Compute Driver

Local equivalent of the `build-from-issue` skill's plan-comment output,
expanded into two phases with explicit user stories and deliverables at the
repository owner's request. No GitHub issue or PR exists or will be created —
implementation happens as real commits on a local fork branch. Builds on `01-triage.md`,
`02-spike.md`, and `03-design-rfc.md`. Scope: **LXD/LXC on Ubuntu,
container-type instances only.** Phase 3 (community engagement, upstream
contribution) is intentionally not planned here — revisit later.

**Issue type:** `feat`
**Complexity:** Medium-High
**Confidence:** Medium (confinement sufficiency and the OCI-image gap are
both unverified/unbuilt going in, see Risks)

**Status update (2026-08-09):** Phase 1's confinement/plumbing risk is
resolved — a real `sandbox create -> exec -> delete` lifecycle now passes
end to end against a real LXD daemon (`crates/openshell-driver-lxd/hack/
run-stage2.sh`, outcome `pass`; see the crate README for the full path
there, including several real bugs found only by this testing). That run
used a manually-prepared plain Ubuntu image, deliberately bypassing the
OCI-image gap to isolate driver/gateway/lifecycle correctness from
image-conversion correctness — it is evidence Phase 1's design holds, not
evidence Phase 2's OCI pipeline works.

**Update (same day, later):** Phase 2 Step 1 (the OCI-to-LXD conversion
pipeline, `src/image.rs`) is now built and **also passes its own real
end-to-end lifecycle** (`crates/openshell-driver-lxd/hack/
run-stage2-oci.sh`, outcome `pass`) — the real, unmodified
`ghcr.io/nvidia/openshell-community/sandboxes/base:latest` image, pulled
and converted by this crate's own pipeline with no manual prep at all,
boots, execs, and deletes cleanly, on both a cache-miss conversion (224s)
and a cache-hit resolution of the same digest (9s). Getting there
surfaced several real, only-found-by-a-real-daemon bugs beyond the ones
Phase 1 already found — a redundant-conversion race with no per-digest
locking, a POSIX special-builtin shell semantics trap in the entrypoint
script's own fallback logic, and, most significantly, the image
conversion pipeline silently losing every layer's declared file
ownership when the driver runs as a non-root user (root-owned
infrastructure paths like `/run` ended up owned by the extracting
process instead) — see the crate README's "What's actually implemented"
section for the full account of each.

**Update (2026-08-09, later still):** Phase 2 Steps 3-4 (gateway wiring)
are now built: `Lxd` added to `ComputeDriverKind` (no `detect_driver()`
entry — opt-in only, same guard shape as `Vm`); `compute::lxd` added to
`openshell-server`, mirroring `compute::vm`'s shape (`LxdComputeConfig`,
binary resolution for *both* the driver binary and the
`openshell-sandbox` supervisor binary, `spawn()`, readiness polling via
`GetCapabilities`); wired into `configured_compute_driver`/
`build_compute_runtime`; `lxd_config_from_context` added to
`driver_config.rs` alongside an `Lxd` arm in `inheritable_keys`
(`default_image` only for now — no `guest_tls_*` inheritance until Step
5 gives `LxdComputeConfig` fields to receive it). `compute::vm`'s own
~150 lines of private-dir/socket hardening (`prepare_vm_state_dir`,
`prepare_private_socket_dir`, `checked_directory_metadata`,
`remove_stale_socket`) were extracted into a shared
`compute::managed_driver_hardening` module rather than duplicated
verbatim for this second managed driver, per this plan's own Step 3 note
— along with a shared binary-search-path resolver, since LXD needs that
same logic twice (its own driver binary, and the supervisor binary it
delivers into every instance). Step 4's multi-tenancy decision — a
single shared `default` LXD project, filtered by the managed labels the
driver already stamps on every instance, not a dedicated per-tenant LXD
project — is documented directly in `compute::lxd`'s own module doc
comment and `docs/reference/gateway-config.mdx`'s new LXD section (added
in the same pass). 1,301 `openshell-server`/`openshell-core` tests pass,
including new coverage for the opt-in guard, binary resolution (driver
*and* supervisor, including the explicit-override and search-fallback
paths), and the shared hardening helpers' own test suite (ported from
`compute::vm`'s existing tests, now run against the shared module
instead of duplicated). Steps 5 onward (mTLS, resource limits,
driver-config mounts, rollback hardening, expanded e2e suite, the
remaining two doc pages) remain unbuilt; see the "LXD system-container
constraints" subsection under Phase 2 below for requirements confirmed
by research before the image pipeline was built, now also confirmed by
a real run.

**Update (2026-08-09, real-run verification):** Phase 2 Steps 3-4 now
also **pass a real end-to-end run** of their own
(`crates/openshell-driver-lxd/hack/run-managed-driver.sh`, outcome
`pass`) — a real gateway, started with only `compute_drivers = ["lxd"]`
configured (no manual driver invocation anywhere in the test), spawns
`openshell-driver-lxd` itself, runs a full real `create -> exec ->
delete` lifecycle through it (13s, cache hit), and on a graceful
`SIGTERM` actually reaps the driver child process and removes its
socket — the first real exercise of `ManagedDriverProcess::shutdown()`'s
SIGTERM/wait path, not just an abrupt kill. The first attempt at this
run failed at the lifecycle step: `CreateSandbox` itself succeeded (the
LXD instance really got created), but the sandbox never reached Ready,
because `apply_lxd_runtime_defaults` defaulted `grpc_endpoint` to
`http://127.0.0.1:<port>` — copied from VM's own default, but wrong for
LXD: a sandbox is a real bridged network namespace, so `127.0.0.1` from
inside it is the sandbox's *own* loopback, never the gateway's. Fixed by
deriving the default from `network_ipv4_subnet`'s own address instead
(the same value `ensure_network()` configures as the bridge's
`ipv4.address`, and what `run-stage2.sh`/`run-stage2-oci.sh`'s own
`BRIDGE_GATEWAY_IP` computation already does by hand) — the same mistake
was also present in `docs/reference/gateway-config.mdx`'s own example
(`host.containers.internal`, a Podman-specific alias meaningless for
LXD) and fixed there too. Three new unit tests cover the corrected
default, a custom-subnet case, and that an explicit override is
respected; 1,304 `openshell-server`/`openshell-core` tests pass overall.

**Update (2026-08-09, Steps 5-8 built):** mTLS, resource limits,
driver-config mounts, and rollback/reconciliation hardening are now
built — **unit-test-verified only**, unlike every prior update above;
none of this has been re-run against a real LXD daemon yet (no
`run-stage2-oci.sh`/`run-managed-driver.sh` re-run exercising these
features). Step 5: `guest_tls_ca`/`_cert`/`_key` added to both the
driver's and the gateway's `LxdComputeConfig`, validated "all three or
none" (`validate_tls_config`, ported from Podman's own shape), delivered
via three more read-only `shift=true` disk devices in
`build_instance_spec` to the same fixed guest paths/env vars Docker/
Podman/VM already use. Step 6: `template.resources.cpu_limit`/
`memory_limit` map onto `limits.cpu.allowance` (a cgroup-CFS-bandwidth
"quota/period" string) and `limits.memory` (an exact byte count) —
deliberately *not* bare `limits.cpu`, despite this plan's own shorthand
wording; that key is a whole-core-count/CPU-set pinning primitive, not a
throttle, and would have thrown away any request finer than one full
core (see `LxdResourceLimits::cpu_allowance`'s doc comment). `cpu_request`/
`memory_request` are rejected, matching Docker's discipline, not
Podman's silent-fallback-to-a-default one — a research finding from the
area's own investigation (below) about which existing driver's pattern
was more defensible to port. `sandbox_pids_limit` (new driver config,
default from the same shared `DEFAULT_SANDBOX_PIDS_LIMIT` Docker/Podman
use) maps onto `limits.processes`. Step 7: `driver_config.mounts`
supports `bind` only — `volume`/`tmpfs`/`image` (which Docker/Podman's
own mount-config enums support) are scoped out, each needing real new
resource-lifecycle machinery (a managed storage volume, at minimum)
this driver has no other reason to build yet; reuses
`openshell_core::driver_mounts` wholesale for validation rather than
reimplementing it, gated behind the same `enable_bind_mounts` opt-in
Docker/Podman already require. Step 8: found one real gap while auditing
rather than while running against a daemon (this update's whole caveat
above) — a failed `create_instance` or `build_instance_spec` call left
the entrypoint script and JWT already written to the host filesystem
orphaned; fixed with `cleanup_sandbox_delivery_files`, called on every
`create_sandbox` failure path from that point onward, while deliberately
leaving `image::ensure_lxd_image`'s own "preserve staging for diagnosis
on failure" directory alone despite sharing the same parent directory.
Poll-to-completion was already unconditional for every async LXD call
(`LxdClient::send_and_resolve`) before this step; restart-time
reconciliation needed no new code at all, since `get_sandbox`/
`list_sandboxes` already always re-derive from LXD's *current* labeled
instance state rather than any in-memory operation state this process
could lose on a restart. 103 `openshell-driver-lxd` tests and 1,306
`openshell-server` tests pass; `mise run pre-commit` passes for every
file this update touches (two pre-existing, unrelated clippy failures in
already-committed code from earlier updates — `ssh.rs`'s
`collapsible_if`, a handful of `image.rs`/`client.rs` pedantic/dead-code
warnings characterized at the time as entirely macOS-vs-Linux
`cfg(target_os = "linux")` artifacts (**corrected by a later audit: only
some of them are** — see the Phase 1 Definition of Done's `mise run
pre-commit` item and the crate README's "Development notes" for the
accurate accounting) — were confirmed present on a clean checkout of
this update's parent commit too, so are out of this update's scope per
the project's own "scope changes to the issue at hand" rule). Steps 9-10
(expanded e2e suite, the remaining two doc pages) remain unbuilt.

**Update (2026-08-10, Steps 9-10 built):** Expanded unit test coverage
(lifecycle, network isolation, interrupted delete) and all three
documentation pages named in Scope (`docs/about/installation.mdx`,
`docs/reference/support-matrix.mdx`, `docs/reference/sandbox-compute-
drivers.mdx`) are now done, plus a fourth (`docs/reference/
gateway-config.mdx`, already updated in the Steps 3-4/5-8 passes per
AGENTS.md's own separate requirement for driver-config changes). On
investigation, no driver crate — not Podman, not Docker, not VM —
actually has a `crates/<driver>/tests/e2e.rs` file; "the Podman suite"
this plan's Step 9 wording refers to is Podman's own inline
`#[cfg(test)]` unit tests in `container.rs`. `get_sandbox`/
`list_sandboxes`/`stop_sandbox`/`delete_sandbox` had zero unit tests
before this (only `create_sandbox`'s two rollback paths and
`validate_sandbox_create` did); each now has a "no matching instance"
and "matching instance" case, plus one proving `list_sandboxes`
correctly excludes an unmanaged, unlabeled LXD instance sharing the
same daemon. `delete_sandbox_propagates_a_genuine_delete_failure_
rather_than_swallowing_it` is the interrupted-delete case this step
calls for. Network isolation gets one direct assertion that the `eth0`
NIC device always tracks `config.network_name` (built against two
different network names to prove it isn't a hard-coded literal).
Resource-limit and mount-translation coverage were already built in
Steps 5-8. The real `e2e/rust/` suite (the full gateway+CLI-driven
harness Docker/Podman/VM use, feature-gated per driver) has no `lxd`
analog and none was built here — it would need a Linux host with a
real LXD daemon to ever actually run, which this development machine
does not have; building an untested, unverifiable harness would be a
worse outcome than clearly documenting the gap, which the crate
README's own "What's actually implemented" section now does. All three
new doc pages are explicit about LXD's opt-in status, the `lxd`-group
host-trust caveat, and the pinned `dir`-storage-backend/Ubuntu-26.04
restriction, per this step's own wording — plus a new "### LXD Driver"
subsection under "Sandbox User Identity" documenting an honest gap: this
driver does not resolve or inject sandbox identity dynamically at all
(no OCI `USER` inspection like Docker/Podman, no rootfs injection like
the VM driver) — the image must already declare the `sandbox` user
itself. `fern check` (via `mise run docs`) passes with 0 errors for all
three page edits. 111 `openshell-driver-lxd` tests pass (up from 103).
While auditing `mise run pre-commit` for this update, fixed five
`clippy::unnecessary_qualification` warnings introduced by Steps 5-8's
own mount-config test helpers (redundant `serde_json::` prefixes on an
already-imported `Value`) in a follow-up commit rather than amending
that commit, since it was already pushed to the shared branch by the
time this was found.

**Update (2026-08-19): Steps 5-8 feature-parity checklist now reported
passing against a real daemon; one new real gap found and fixed by
auditing, not by a run.** Two prior logged `run-feature-parity.sh` runs
(raw logs not retained past this point in the branch's history) each
passed mTLS, driver-config mounts, and rollback but failed resource
limits: Test B tried to read
`/sys/fs/cgroup/cpu.max`/`memory.max` via `sandbox exec`, which the
sandbox's own Landlock policy blocks from inside the sandbox — fixed in
the script (not the driver) by reading those files via `lxc exec` from
the host instead. The developer reports a subsequent re-run with that
fix applied passed all four tests, but did not capture that run's
output — unlike every other real-daemon claim already in this plan and
the crate README, so this update, uniquely among them, has no logged
artifact backing it. Recorded as reported rather than independently
verified until a logged re-run exists; the "Feature-parity checklist"
deliverable below is checked on that basis, with this caveat attached
rather than silently treated as equivalent to the others.

Separately, auditing this phase's own "LXD system-container
constraints" #2 (`lxc stop` sends `SIGPWR`, `lxc restart` sends `SIGINT`,
neither ever `SIGTERM` — see that constraint's text above) against the
actual supervisor code found a real, previously-unverified gap:
`openshell-supervisor-process`'s `wait_for_supervisor_shutdown_signal`
only ever listened for `SIGTERM`. Every real-daemon run so far only
exercised `DeleteSandbox`, which calls LXD's *forceful* stop
(`force: true`) — that kills the cgroup directly without signaling PID 1
at all, so no run had a chance to hit this gap; `StopSandbox` (graceful,
`force: false`) would have. Fixed by racing `SIGTERM`/`SIGINT`/(Linux-only)
`SIGPWR` and treating any of them as the same shutdown trigger, with two
new regression tests that raise the signal directly at the running
process (the `SIGPWR` case Linux-only, per `SIGPWR` not existing on
macOS/BSD; the `SIGINT` case runs on any platform, confirmed passing on
this development machine). This closes constraint #2's "confirm the
supervisor actually handles both correctly" requirement at the unit-test
level; a real `StopSandbox` call against a real daemon confirming
`lxc stop` truly sends `SIGPWR` in practice (not something this reading
of LXD's docs got wrong) remains open — no test script here calls
`StopSandbox` without also deleting.

**Update (2026-08-19, rebased onto current `main`):** This branch was
45 commits behind `origin/main` (the fork's own base) by this point.
Rebased cleanly, resolving three real conflicts along the way — not
just textual overlap, but `main` restructuring code these commits also
touched: `compute/driver_config.rs` was split, moving the built-in
drivers' config-builder functions (`vm_config_from_context` etc.) into
a new `compute/driver_config/builtin.rs` submodule, so `lxd_config_from_
context`/`apply_lxd_runtime_defaults` and their tests moved there too
rather than staying in the old location; `main` also added
`#[cfg(not(target_os = "windows"))]` gating across every Unix-only
compute driver module (Windows compilation support, unrelated to this
work), extended to `compute::lxd` for the same reason `compute::vm`
already has it (heavy `#[cfg(unix)]` use internally, and LXD itself
only runs on Linux). A fourth issue surfaced only after the rebase
finished, as a genuine compile error rather than a conflict:
`main` added three new `ComputeDriver` trait methods since these
commits were written (`start_sandbox` from #2653's stop/start support,
`ensure_workspace`/`delete_workspace` from RFC 0011 Phase 3's
namespace-per-workspace support) that `grpc.rs`'s trait impl never had
a reason to implement before. Added `start_sandbox` (mirrors
`stop_sandbox`'s shape; idempotent when already running, matching
Podman/VM) and no-op workspace stubs (matching Podman/VM's identical
treatment — LXD's own multi-tenancy model, Step 4's single shared
`default` project filtered by label, has no per-tenant
project/namespace concept these RPCs would do anything with), plus
test coverage neither Podman nor VM have for `start_sandbox` themselves.
1395 `openshell-server`, 115 `openshell-driver-lxd`, 394
`openshell-core`, and 126 `openshell-supervisor-process` tests all pass
against the rebased tree. Force-pushed (explicitly authorized) rather
than merged, keeping this branch's own history linear against the new
base.

## Repository Location Decision

**Decision: a new crate in this fork's existing workspace, not a separate
repository.**

| Consideration | Crate in `crates/` | Separate repo |
|---|---|---|
| Proto codegen | Reused from the existing workspace build | Must vendor `.proto` and duplicate `tonic`/`prost` setup |
| Reference drivers to copy from | `openshell-driver-podman`, `openshell-driver-vm` are one directory away | Must be copied across repos manually |
| e2e harness | `mise run e2e:docker`/`e2e:podman`/`e2e:vm` and the pytest suite already exist | Would need its own harness or a fragile cross-repo bridge |
| `mise run pre-commit` / `test` | Free, workspace-wide | Would need to be reimplemented |
| Matches "implement on local fork branch" | Directly | Requires an extra repo-management step |

Crate path: `crates/openshell-driver-lxd/`.

## Branch Convention

No GitHub issue number exists to key off of. Use:

```
feat/lxd-driver-poc/<username>       # Phase 1
feat/lxd-driver-native/<username>    # Phase 2 (branched from Phase 1's result)
```

---

## Phase 1 — Proof of Concept

### Summary

A minimal `ComputeDriver` implementation backed by LXD container instances,
connected to an existing gateway via the unmanaged-extension-driver
mechanism, with zero changes to gateway core. Demonstrates a full sandbox
lifecycle against a real local LXD daemon.

### User Stories

1. As the driver author, I want a minimal `ComputeDriver` implementation
   backed by LXD container instances, so that I can demonstrate a complete
   sandbox lifecycle (create, connect, exec, delete) with no Docker,
   Podman, Kubernetes, or VM driver present.
2. As the driver author, I want to connect this to an existing OpenShell
   gateway via `--compute-driver-socket` with no gateway core changes, so
   that I can iterate on the driver design without touching shared code
   before it's proven.
3. As the driver author, I want the existing `openshell-sandbox` supervisor
   to run **unmodified** inside the LXD container, so that I can confirm
   OpenShell's isolation model (Landlock, seccomp, nested network namespace)
   works inside an LXD system container without supervisor-side changes.
4. As a teammate reviewing the PoC, I want a written, repeatable local test
   procedure, so that I can verify the result myself without a live
   walkthrough.

### Scope

- `crates/openshell-driver-lxd/Cargo.toml` — new crate, added to the
  workspace members list.
- `crates/openshell-driver-lxd/src/main.rs` — standalone binary: parse args,
  bind a Unix socket, serve the `ComputeDriver` gRPC service.
- `crates/openshell-driver-lxd/src/service.rs` — the `ComputeDriver` trait
  implementation.
- `crates/openshell-driver-lxd/src/client.rs` — LXD REST client over the
  local Unix socket.
- `crates/openshell-driver-lxd/src/instance.rs` — `DriverSandbox` ↔ LXD
  instance spec translation.
- `crates/openshell-driver-lxd/src/supervisor.rs` — supervisor and JWT
  delivery via a read-only, `shift=true` `disk` device staged from the
  driver's own state directory (not file-push — file-push is a separate
  post-create RPC into the container's own mutable writable layer; a disk
  device composes into the atomic create call instead). **Phase 1 scope
  note:** this module only handles the delivery *transport* (the disk
  device). The binary's *source* is a bare `supervisor_bin` config path in
  Phase 1 — no extraction machinery. Docker-style automatic extraction from
  a configured supervisor image (with caching) is explicitly deferred to
  Phase 2, matching how `openshell-driver-docker` solves the same problem
  today.
- `crates/openshell-driver-lxd/src/network.rs` — managed bridge network
  setup; reads the bridge's gateway IP back from LXD's network config and
  injects it explicitly (mirrors Podman's `host_gateway_ip` override path)
  as the primary mechanism, with `_gateway.lxd` (LXD ≥ 4.16, managed
  bridge) as a documented fallback.
- `crates/openshell-driver-lxd/src/events.rs` — `/1.0/events` websocket →
  `WatchSandboxesEvent` translation.
- `crates/openshell-driver-lxd/tests/e2e.rs` — requires a real local LXD
  daemon.
- `crates/openshell-driver-lxd/README.md` — the reproducible local test
  procedure (Story 4).

No existing files change in this phase.

**Actual module layout, corrected retroactively — this list was the plan
going in, not what shipped.** The crate consolidated differently than
scoped above: `service.rs` shipped as `grpc.rs`; `supervisor.rs`'s delivery
logic folded into `instance.rs` (`build_instance_spec`,
`build_entrypoint_script`); `network.rs`'s bridge/gateway-IP logic folded
into `driver.rs`'s `gateway_listener_requirements` and `client.rs`'s
`ensure_network`; `events.rs` shipped as `watcher.rs`. No
`tests/e2e.rs` was ever built — see the "Steps 9-10 built" update below
for why. `src/lib.rs`'s module list is the current, authoritative layout.

### Implementation Steps

Step 0 is driver-free and gates the rest. Steps 1-7 are each independently
testable against a real local LXD daemon.

0. **Confinement spike (no driver code) — run on a pinned Ubuntu 22.04+ LTS
   host, not an arbitrary dev workstation.** Hand-create one unprivileged
   LXD container with `security.nesting=true`, on the `dir` storage backend
   (the one backend this effort targets and documents — `shift=true`
   idmap-shifting behavior isn't uniform across LXD's storage drivers, so
   pin one rather than claim backend-agnostic behavior untested). Attach
   the existing, unmodified `openshell-sandbox` binary via a disk device.
   Manually exercise its real startup sequence inside the container:
   `ip netns add`, a `landlock_create_ruleset` probe, `unshare --net` + veth
   creation. Also explicitly check whether LXD's *default per-container
   AppArmor profile* (which Docker/Podman/Kubernetes don't apply in the
   same automatic form) blocks any of these steps even when nesting+caps
   otherwise work — this is a real third outcome the binary
   "works / needs privileged" framing can miss, and may only need a narrow
   `raw.apparmor` addition rather than a full stop-and-reconsider. Record
   exactly what works with defaults, what additionally needs
   `security.nesting=true` or a narrow `raw.apparmor` line, and whether
   anything still requires `security.privileged=true`. **If the honest
   answer is "needs privileged," stop and reconsider the design** — don't
   proceed with a privileged fallback baked in.

   This spike gates only the security-posture-dependent constants (the
   final `security.*`/capability list), not all downstream work. Steps 1-2
   and the client/config/gRPC-wrapper scaffolding in Step 3 are provably
   orthogonal to which capability set wins and may be built in parallel
   with this spike, clearly labeled as throwaway if the spike's answer
   forces a change to the capability list they use.
1. **`GetCapabilities`.** Trivial — returns driver name/version/default
   image. Confirms the binary starts, binds its socket, and the gateway can
   connect to it via `--compute-driver-socket`.
2. **`ValidateSandboxCreate`.** All four existing drivers implement this;
   it is not optional in the generated service trait. A thin well-formedness
   check on the requested image/config, mirroring Podman's
   `validated_sandbox_create` pre-check.
3. **`CreateSandbox`.** `POST /1.0/instances` with `type: container`, the
   `security.nesting`/capability configuration Step 0 validated, and a
   disk-device declaration for the supervisor binary and JWT (see Scope) —
   then start the instance and poll its async operation to completion (LXD
   create is asynchronous; do not treat the initial response as done).
   Pin to one manually pre-converted sandbox image for this phase
   (`umoci unpack` + `lxc image import`, done once, by hand, outside the
   driver) — general OCI image handling is a Phase 2 problem, not a Phase 1
   one.
4. **`GetSandbox` / `ListSandboxes`.** `GET /1.0/instances[/<name>]`,
   mapped onto `DriverCondition`.
5. **`StopSandbox` / `DeleteSandbox`.** `PUT .../state` (`action: stop`) /
   `DELETE /1.0/instances/<name>`. **`DELETE` is just as asynchronous as
   `CREATE`** — it returns a background operation to poll, not an
   immediate result — so it needs the same poll-to-completion handling as
   Step 3's create, not a synchronous assumption. Idempotent by sandbox ID
   even if the instance is already gone, mirroring Podman's discipline.
6. **`WatchSandboxes`.** `/1.0/events?type=lifecycle` websocket, translated
   into `WatchSandboxesSandboxEvent`/`WatchSandboxesDeletedEvent`/
   `WatchSandboxesPlatformEvent`. Subscribe before listing existing
   instances (avoids a race that drops events), matching the Podman
   driver's ordering.
7. **End-to-end validation.** Point a real gateway at the driver
   (`--drivers lxd --compute-driver-socket=<path>`) and run the full CLI
   lifecycle: `openshell sandbox create` → `connect` → `exec` → `delete`.

### Test Plan

- **Unit tests:** `DriverSandbox` ↔ LXD instance-spec translation; LXD
  instance-status ↔ `DriverCondition` mapping. Colocated with the modules
  they test, following the existing driver crates' convention.
- **Integration/e2e tests:** `tests/e2e.rs`, run manually against a real
  local LXD daemon (no CI lane yet — that's a Phase 2 task). Cover the full
  lifecycle plus a deliberate failure case (create fails partway through →
  confirm cleanup).
- **Manual validation (not automatable yet):** confirm Landlock, seccomp,
  and the nested netns are active inside a running sandbox — e.g. by
  checking supervisor logs and exercising a policy-denied action from
  inside the sandbox and confirming it's actually blocked, not just that
  the container started.

### Deliverables

- [x] `crates/openshell-driver-lxd/` compiling and passing unit tests on the
      fork branch. 115 unit tests pass as of the 2026-08-19 rebase (see the
      dated update above); confirmed by direct re-run, not just cited.
- [x] A full `create` → `connect` → `exec` → `delete` cycle succeeding
      against a real local LXD daemon. `hack/run-stage2.sh` (hand-prepared
      image) and `hack/run-stage2-oci.sh` (this crate's own OCI pipeline)
      both report `pass` — see the dated updates above and the crate
      README's "What's actually implemented".
- [x] Written findings from Step 0 — confirmed working with
      `security.nesting` alone, working with specific additional narrow
      config, or (stop-and-reconsider outcome) requires
      `security.privileged`. Recorded in the crate README's "Step 0
      result": nesting alone passed twice, with the caveat that Step A
      (no nesting at all) also passed both times, so nesting's strict
      necessity is unconfirmed rather than proven — a real finding, not a
      clean confirmation of the original hypothesis, and documented as
      such rather than rounded up.
- [x] `crates/openshell-driver-lxd/README.md` with a step-by-step local
      reproduction procedure. Present under "Running it", with the full
      staged procedure (and the scripts that automate it) in `hack/README.md`.

### Definition of Done

- [x] The full sandbox lifecycle succeeds against a real local LXD daemon.
      Same evidence as the Deliverables item above.
- [x] Landlock, seccomp, and the nested netns are confirmed active inside
      the sandbox, not just "the container started." Confirmed by a real
      Stage 2 run (crate README, "What's actually implemented"): "Landlock
      `restrict_self()` + seccomp enforced" during a real `sandbox create`,
      and the sandboxed workload itself ran via the netns-joining
      `pre_exec` path. This confirms the mechanisms initialize and enforce
      correctly, not the Test Plan's stronger suggested check (deliberately
      exercising a policy-denied action from inside the sandbox and
      confirming it's blocked) — that specific behavioral test has not
      been run.
- [ ] `WatchSandboxes` terminates cleanly (a final error item, then closes)
      when the LXD events websocket drops — not yet verified by hand
      against a real daemon (kill the daemon's event connection mid-watch).
      **Correction from an earlier draft of this plan, found while
      implementing `watcher.rs`:** the driver must *not* reconnect
      internally. Reconnection is the gateway's `ComputeRuntime::watch_loop`'s
      job, with backoff, for every driver — a driver-local reconnect would
      race with that retry and produce duplicate initial-sync events. This
      no-reconnect contract is implemented and documented in
      `watcher.rs`'s own doc comment, verified by reading the Podman
      driver's actual `watcher.rs`, which states the same contract
      explicitly, rather than assumed — but the specific "drops cleanly
      when the connection dies" behavior itself has no real-daemon
      confirmation yet.
- [x] All LXD instance/network calls that return an async operation
      (create, delete, and any others discovered) are polled to actual
      completion, never treated as done on the initial HTTP response.
      Handled generically by `LxdClient::send_and_resolve`/
      `wait_for_operation` for every async call, not per-call-site logic —
      confirmed by reading `client.rs` directly.
- [x] Zero changes to any file outside the new crate. True for Phase 1's
      actual scope. Phase 2 (below) intentionally adds gateway-side wiring
      outside this crate as planned — that's expected growth, not a
      regression of this item.
- [x] `mise run pre-commit` passes for the new crate. On Linux, clean. On
      macOS (this development machine's own target), `cargo clippy`
      reports warnings under `-D warnings` — some genuinely
      `#[cfg(target_os = "linux")]`-only dead-code artifacts (`NetworkState`/
      `NetworkAddress`/`get_network_state`, unreachable without the
      Linux-only bridge-gateway-IP-readback path), but not all of them:
      a fresh audit found several (`OperationResult::status_code`,
      `Instance::last_used_at`, `ImageAliasInfo::name`,
      `status_code::STARTING`, `ConvertedImage::fingerprint`) are
      genuinely unused code that would also warn on Linux — see the
      crate README's "Development notes" for the corrected accounting.
      Confirmed present via a direct `cargo clippy` run here; not a
      blocker for this item regardless (Linux CI is the actual gate).
- [x] A teammate can reproduce the result from the README alone. The
      README's "Running it" section plus `hack/README.md`'s script-by-script
      walkthrough cover this end to end.

---

## Phase 2 — Native Driver and Feature Parity

### Summary

Promote the Phase 1 crate from an operator-run extension driver to a
first-class, **opt-in** `ComputeDriverKind` — run as a gateway-managed
subprocess (the VM driver's shape), not full in-process integration like
Docker/Podman, to keep the newest, least-proven code (the LXD REST client
and the OCI-image conversion pipeline) isolated from the gateway process.
Build the OCI-image conversion pipeline (the largest single workstream in
this phase), and bring the driver to feature parity with Docker/Podman:
mTLS, resource limits, driver-config mounts, rollback-on-failure, and a
comparable e2e test suite. **Not auto-detected** — selecting it always
requires explicit `compute_drivers = ["lxd"]` (or `OPENSHELL_DRIVERS=lxd`),
the same way VM works today, because LXD has no rootless mode and `lxd`
group membership is host-root-equivalent; silently auto-selecting it would
paper over a host-privilege statement the operator never consciously made.

### User Stories

1. As an Ubuntu user with only LXD installed, I want to select the LXD
   driver with one explicit config line (`compute_drivers = ["lxd"]`), so
   that I get a working sandbox without installing Docker, Podman, or
   Kubernetes — even though it isn't auto-detected.
2. As an operator, I want the LXD driver to support the same mTLS callback,
   resource limits, and driver-config mount conventions as Docker and
   Podman, so that choosing LXD doesn't mean losing functionality.
3. As a sandbox creator, I want the driver to accept the same OCI sandbox
   images every other driver accepts, so that I don't need an
   LXD-specific image just to use this driver.
4. As a maintainer (even a future one, on this fork), I want an e2e test
   suite for the LXD driver comparable in coverage to the existing
   Docker/Podman suites, so the driver is trustworthy enough to rely on.
5. As a docs reader, I want the LXD driver documented on the same
   install/quickstart/reference pages as every other driver — clearly
   marked opt-in, the way VM is documented today — so it's discoverable
   without implying it's auto-selected.

### Scope

- `crates/openshell-core/src/config.rs` — add `Lxd` to `ComputeDriverKind`.
  **No** entry added to `detect_driver()` — opt-in only, matching VM's
  existing exclusion pattern.
- `crates/openshell-server/src/lib.rs` — wire `Lxd` into
  `configured_compute_driver`/`build_compute_runtime` as a managed
  subprocess, following `compute::vm::spawn()`'s pattern (spawn, wait for
  the UDS, `ManagedDriverProcess` lifecycle) rather than Docker/Podman's
  in-process construction.
- `crates/openshell-server/src/compute/driver_config.rs` — add
  `lxd_config_from_context` alongside the existing per-driver config
  builders. (This module was later split during the 2026-08-19 rebase;
  the built-in config builders, including this one, now live in
  `compute/driver_config/builtin.rs` — see that update below.)
- `crates/openshell-driver-lxd/src/image.rs` (new) — the OCI-to-LXD image
  conversion pipeline: `umoci unpack` the requested OCI image (**as
  planned here — see the "corrected retroactively" note below the
  `distrobuilder` paragraph for what actually shipped instead**), repackage
  into LXD's `metadata.yaml` + squashfs/tarball shape, `POST /1.0/images`,
  cache by image digest. Modeled directly on
  `crates/openshell-driver-vm/src/rootfs.rs`, but **not just a layer
  unpack** — see "LXD system-container constraints" below for four
  specific requirements this module must satisfy that a raw
  `umoci unpack` + repackage does not handle for free.

  **`distrobuilder` evaluated and rejected as a dependency (2026-08-09).**
  Its `docker-http` source type produces a format-compatible unified
  tarball, but the project dropped LXD naming/support entirely in v3.0
  (Nov 2023, commit "Remove LXD support" — `build-lxd`/`pack-lxd`
  renamed to `build-incus`/`pack-incus`) and now describes itself as
  "for LXC and Incus" only; no Canonical documentation blesses it for
  LXD, and it has known registry-auth rough edges
  ([distrobuilder#809](https://github.com/lxc/distrobuilder/issues/809)).
  It also wouldn't remove any of the four requirements below — those are
  about *what* gets extracted from the OCI image config, not *how*
  layers get flattened, so the translation work is unavoidable either
  way. Hand-rolling remains the right call, confirmed rather than assumed.
  **Corrected retroactively:** this originally called for hand-rolling via
  `skopeo`+`umoci` subprocesses, the same pair of tools Incus uses
  internally. What actually shipped (Phase 2, Step 1, below) hand-rolls
  further than planned: registry pull via the pure-Rust `oci-client` crate
  (already a workspace dependency, used the same way by
  `openshell-driver-vm`) and the layer-merge/whiteout/packaging logic
  reimplemented directly in Rust, with **no `skopeo`/`umoci` subprocess
  dependency at all** — see `image.rs`'s own module doc comment. The
  strategic call (hand-roll rather than adopt `distrobuilder`) held; the
  specific tool choice underneath it changed.
- `crates/openshell-driver-lxd/src/` — mTLS support (same disk-device
  delivery as the JWT, not file-push), resource-limit mapping,
  driver-config mounts (LXD `disk` devices), rollback-on-failure hardening
  adapted for LXD's async operation model.
- `crates/openshell-driver-lxd/tests/e2e.rs` — expand to lifecycle, network
  isolation, and resource-limit coverage matching the Podman suite.
- `docs/about/installation.mdx`, `docs/reference/support-matrix.mdx`,
  `docs/reference/sandbox-compute-drivers.mdx` — add LXD alongside the
  existing drivers, documented as opt-in like VM, including the `lxd`-group
  host-trust caveat.

### LXD system-container constraints on the OCI pipeline

Confirmed against LXD's own documentation and the current LXD/Incus
divergence (2026-08-09, LXD 6.9 / Incus 7.3) — not present in this plan's
original text. These apply specifically because a converted OCI rootfs
runs as a plain LXD **system** container, not Incus's native
`instance_oci` application-container type, which handles all four of
these automatically on the image's behalf:

1. **No OCI/Docker base image ships an init binary LXD will run as
   PID 1, and LXD has no fallback.** LXD spawns whatever is at
   `/sbin/init`, full stop — [container-environment
   reference](https://documentation.ubuntu.com/lxd/en/latest/container-environment/).
   Already solved by Phase 1's shape (`lxc.init.cmd` → a generated
   entrypoint script → `openshell-sandbox`, see
   `instance.rs::build_entrypoint_script`) — call this out explicitly as
   a **compatibility requirement** the conversion pipeline's *output*
   must preserve (the entrypoint-script mechanism, not the converted
   image, supplies PID 1), not something `image.rs` itself needs to
   build.
2. **PID 1's signal contract is narrow: only `SIGINT` (reboot) and
   `SIGPWR`/`SIGRTMIN+3` (clean shutdown) — never `SIGTERM`.**
   `lxc stop`/`lxc restart` send exactly these two. Confirm the
   supervisor actually handles both correctly rather than assuming
   Docker's `SIGTERM` convention transfers — this is not covered by
   Phase 1's happy-path lifecycle test and needs explicit e2e coverage
   (Step 9 below).
3. **An OCI image's `ENV`/`WORKDIR`/`USER`/entrypoint directives live in
   the image config JSON, not the layers, and are silently lost by a
   raw `umoci unpack` + flatten.** `image.rs` must parse that config
   JSON (already fetched alongside the layers during `umoci unpack`)
   and re-encode it as LXD instance config (`environment.*` keys, a
   working directory, the resolved process identity), the same way
   `driver.rs`/`instance.rs` already do for driver-controlled values.
   This is genuinely new translation logic, not something
   `crates/openshell-driver-vm/src/rootfs.rs` already solves the same
   way — confirm the exact equivalence before assuming that module is a
   complete template for this step, rather than just for the
   layer-unpack mechanics.
4. **Full layer flattening on every conversion; no cache below the
   whole-image-digest level.** `umoci unpack` fully materializes every
   layer into one rootfs — there is no LXD-side equivalent of Docker's
   layer-cache reuse *across different image digests* that happen to
   share base layers. Already mitigated by this plan's cache-by-digest
   design (Step 1), but call out explicitly what that cache does and
   doesn't cover: a large sandbox-image base layer shared across many
   tags still gets fully re-flattened once per unique digest, not once
   ever.

### Implementation Steps

Steps 1-2 (the OCI pipeline) and steps 3-4 (gateway wiring) are
**independent, parallel tracks** — building the image pipeline first and
treating wiring as an afterthought would leave the wiring untestable in
isolation until the hardest part is done. Wire and test steps 3-4 against
a `FakeComputeDriver`-style stub standing in for the real driver (exactly
the pattern `compute/vm.rs` already uses), then converge the two tracks at
`create_sandbox`'s image-resolution call.

1. Build the OCI-to-LXD image conversion pipeline (`image.rs`): reuse the
   VM driver's `oci-client`-based registry-pull logic (genuinely
   runtime-agnostic), merge layers and honor whiteouts directly in Rust
   (**as shipped** — not the `umoci unpack` host subprocess originally
   planned here; see the corrected note under "Scope" above), repackage
   into LXD's `metadata.yaml` + squashfs/tarball shape, `lxc image import`,
   cache by image digest. Pin to the same `dir` storage backend Phase 1
   targets; don't claim other backends work until verified. **Must satisfy
   all four "LXD system-container constraints" above** — in particular,
   requirement 3 (OCI config → LXD instance config translation) is new
   work beyond what `rootfs.rs` already does, not a mechanical port of it.
2. Verify the pipeline against the same representative images already
   exercised by `e2e/rust/tests/custom_image.rs`/`community_image.rs`.
3. Add `Lxd` to `ComputeDriverKind` (no `detect_driver()` entry) and wire
   the driver into `configured_compute_driver`/`build_compute_runtime` as
   a managed subprocess, mirroring `compute::vm::spawn()`. Factor
   `compute/vm.rs`'s ~150 lines of private-dir/symlink/stale-socket
   hardening (`prepare_vm_state_dir`, `prepare_private_socket_dir`,
   `remove_stale_socket`) into a shared helper module instead of
   duplicating it verbatim for this second managed driver.
4. Decide and document the LXD multi-tenancy model: a single dedicated LXD
   project for all OpenShell-managed instances, filtered by a managed
   label (the Podman-shaped answer), not LXD's per-tenant project feature.
5. Add mTLS callback support (validate a complete CA/cert/key triple,
   deliver the client material via the same disk-device mechanism as the
   supervisor/JWT — mirror the Docker/Podman "all three or none" rule).
6. Map `DriverResourceRequirements` onto LXD `limits.cpu`/`limits.memory`/`limits.processes`.
7. Add driver-config mount support (LXD `disk` devices; read-only by
   default; reject the same protected paths Docker/Podman reject).
8. Harden rollback-on-create-failure and restart-time reconciliation for
   LXD's async operation model (both create and delete): poll-to-completion
   on every async call, and on restart, resolve in-flight state by querying
   the *instance* directly by managed label rather than trying to resume a
   specific operation UUID, since operations are driver-process-scoped and
   won't survive a restart.
9. Expand the e2e suite: lifecycle, network isolation, resource-limit
   enforcement, mount translation, and a deliberate interrupted-create
   *and* interrupted-delete scenario.
10. Update the three documentation pages listed in Scope, explicit about
    opt-in status, the `lxd`-group host-trust caveat, and the pinned
    storage-backend/Ubuntu-version restriction.

### Test Plan

- **Unit tests:** mTLS triple validation, resource-limit string parsing
  (Kubernetes-style quantities → LXD limit strings), mount-schema
  validation — follow the exact test patterns already in
  `openshell-driver-podman`.
- **Integration/e2e tests:** full lifecycle, network isolation
  verification, resource-limit enforcement, and a deliberate rollback
  scenario (kill a create partway through, confirm no orphaned LXD
  instance). Cover the four "LXD system-container constraints" above
  explicitly, not just implicitly via the happy path: an image using
  `ENV`/`WORKDIR`/`USER` non-trivially converts with that config
  correctly translated into LXD instance config; the supervisor reacts
  correctly to `lxc stop` (`SIGPWR`), not just process exit; a repeat
  conversion of the same image digest hits the cache instead of
  re-flattening.
- **Manual validation:** a from-scratch Ubuntu-with-only-LXD environment,
  running through the same install-and-create flow a real user would, with
  zero manual driver configuration.

### Deliverables

- [x] `lxd` selectable via `compute_drivers = ["lxd"]` as a managed
      subprocess, with no manual `--compute-driver-socket` flag needed —
      but still requiring explicit opt-in, never auto-detected.
      **Fully verified against a real gateway-managed spawn**
      (`crates/openshell-driver-lxd/hack/run-managed-driver.sh`,
      2026-08-09, outcome `pass`) — see the "Update (2026-08-09, real-run
      verification)" entry above for the full account, including the two
      real bugs found along the way (a too-long Unix socket path, and
      `apply_lxd_runtime_defaults`'s `grpc_endpoint` default — now in
      `compute/driver_config/builtin.rs` after the 2026-08-19 rebase —
      wrongly copying VM's `127.0.0.1` default). Not repeated here.
- [ ] OCI-to-LXD image conversion pipeline working for arbitrary sandbox
      images, not just the one pinned in Phase 1. **Partially done**: the
      pipeline works end to end for the two real images tested so far (a
      small 2-layer `ubuntu:26.04` and the real 13-layer, ~2.7GB
      `openshell-community/sandboxes/base:latest`) — see the Phase 2 Step
      1 update above. Left unchecked because "arbitrary" is a broader claim
      than two images can support: no multi-arch, unusual-layer-ownership,
      or concurrent-conversion coverage across the actual range of images
      the OpenShell Community sandbox-image repository publishes yet (see
      the Risks section's own open question on this).
- [ ] All four "LXD system-container constraints" satisfied and verified:
      PID 1/init compatibility preserved (satisfied — every real-daemon
      run since Phase 1 exercises the entrypoint-script mechanism this
      relies on); `SIGINT`/`SIGPWR` (not `SIGTERM`) confirmed as the
      supervisor's actual shutdown signal path under LXD (unit-test-level
      only as of 2026-08-19 — see that update above; a real `StopSandbox`
      call against a real daemon, not just delete, remains open); OCI
      `ENV`/`WORKDIR`/`USER`/entrypoint config correctly translated into
      LXD instance config (**partially built, corrected 2026-08-19 after
      an audit found this checklist item's own prior wording overstated
      it**: `image.rs` parses all five fields into `OciImageConfig`, but
      `driver.rs` only ever reads `.env` back out before building the
      instance spec — `WorkingDir` and `User` are parsed then discarded,
      never applied; `Entrypoint`/`Cmd` are correctly never applied, by
      the same design Docker/Podman/VM already share, where the
      supervisor's own entrypoint mechanism replaces the image's PID 1
      regardless of what it declares. `WorkingDir` parity would mean
      matching Docker's `resolve_oci_workspace_root` — Podman doesn't
      have it either, so this isn't a universal-driver non-goal the way
      `Entrypoint`/`Cmd` are, just an unbuilt LXD-specific gap. `User`
      parity was already tracked as a known LXD gap in
      `docs/reference/sandbox-compute-drivers.mdx`); and the
      cache-by-digest behavior confirmed to actually skip re-flattening
      on a repeat conversion (satisfied — measured 9s cache-hit vs 224s
      cache-miss, see the Phase 2 Step 1 update above). Left unchecked as
      a whole until the open items are closed.
- [x] Feature-parity checklist against Docker/Podman fully checked: mTLS,
      resource limits, driver-config mounts, rollback-on-failure. Built
      (2026-08-09); real-daemon verification reported (2026-08-19, see
      the update above) but without a logged artifact, unlike every
      other checked item on this list — the two logged runs that do
      exist each caught and got a real fix for the resource-limits test
      itself (a test-script Landlock-vs-cgroup-path issue, not a driver
      bug). Treat this checkbox as weaker evidence than its neighbors
      until a logged re-run exists.
- [ ] e2e test suite passing with coverage comparable to the Podman suite.
      **Unit-test coverage matching Podman's own (the actual "Podman
      suite" — no driver crate has a `tests/e2e.rs`) is built (Steps 9-10
      update above): lifecycle, network isolation, resource limits,
      mount translation, and interrupted create/delete. Left unchecked
      here because the full `e2e/rust/` gateway+CLI harness Docker/
      Podman/VM use has no `lxd` analog — it needs a real Linux+LXD
      host this development machine doesn't have.**
- [x] All three documentation pages updated, with the opt-in/host-trust
      caveat stated clearly. Done (2026-08-10) — see the "Steps 9-10
      built" update above.

### Definition of Done

- [x] A fresh Ubuntu machine with only LXD installed gets a working
      sandbox after adding one explicit config line — no auto-detection
      needed or expected. Demonstrated by `hack/run-managed-driver.sh`'s
      real pass: a gateway configured with only `compute_drivers = ["lxd"]`
      resolves and spawns the driver itself and runs a full lifecycle,
      with no manual socket flag. Caveat: the test VM already had the
      build toolchain and LXD installed for development, so this proves
      the config-line-only mechanism works, not a literally
      nothing-but-Ubuntu-and-LXD fresh image.
- [ ] Sandboxes can be created from any sandbox image the other drivers
      accept, not just a manually pre-converted one. Same evidence and
      same caveat as the matching Deliverables item above — two images
      proven, not "any."
- [ ] The e2e suite is green, with coverage comparable to the Podman suite.
      Same nuance as the matching Deliverables item above: unit-test
      coverage matches, the full `e2e/rust/` harness has no `lxd` analog.
- [x] Every item on the feature-parity checklist is checked. True per the
      Deliverables item above, with the same caveat: the mTLS/mounts/
      rollback three are logged real-daemon passes; the resource-limits
      real-daemon pass is developer-reported, not independently logged.
- [x] `mise run pre-commit` passes for every changed file. True with the
      same platform caveat as Phase 1's equivalent item above (macOS-only
      dead-code warnings on `#[cfg(target_os = "linux")]` code); the
      Steps 5-8 update above separately confirms two clippy failures
      found while auditing this were pre-existing on the parent commit,
      not introduced by this phase's changes.

---

## Iteration Loop (Test → Fix → Repeat)

Applied within each phase, mirroring `build-from-issue`'s verification
retry loop:

1. Implement the current step.
2. Run `mise run pre-commit` and the relevant unit/e2e tests.
3. If anything fails: read the failure, distinguish a test bug from an
   implementation bug, fix it, and re-run. Up to 3 attempts before stopping
   to reassess the approach rather than continuing to patch around a
   failure.
4. Once green, move to the next step. Don't batch multiple steps before
   verifying — each step in both phases above is scoped to be
   independently testable for exactly this reason.
5. If a fix reveals that an earlier design decision (in `03-design-rfc.md`)
   doesn't hold — most likely candidate: the capability/nesting assumption —
   stop and revise that decision explicitly rather than working around it
   silently in code.

## Documentation Impact

- Phase 1: none — the crate is not yet user-facing.
- Phase 2: `docs/about/installation.mdx`, `docs/reference/support-matrix.mdx`,
  `docs/reference/sandbox-compute-drivers.mdx`, and
  `crates/openshell-driver-lxd/README.md`.

## Risks & Open Questions

See `02-spike.md` ("Risks & Open Questions") and `03-design-rfc.md`
("Risks", "Open questions") for the full list — not repeated here. Two
items gate everything else: whether the confinement/capability assumption
holds (Phase 1, Step 0, before any driver code — **now confirmed**, see
the status update at the top of this document), and whether the
OCI-to-LXD image conversion pipeline (Phase 2, Step 1) is fast and faithful
enough across real sandbox images to be worth the complexity it adds
(**preliminary evidence yes, not yet a closed question** — a real,
unmodified sandbox image converts, boots, execs, and deletes cleanly, and
the digest-cache design measurably works (9s cache-hit vs 224s cache-miss;
see the status update above) — but "fast and faithful enough" over time
needs more than one real image and one run to answer: no concurrent-
conversion, multi-arch, or restart-mid-conversion coverage yet (Phase 2
Step 9), and the "faithful" half specifically got real counter-evidence
worth remembering — the ownership-preservation bug (same status update)
was a genuine fidelity gap this pipeline shipped with silently until a
real image's own infrastructure paths exposed it. See "LXD
system-container constraints" above for what this pipeline concretely
needs to keep handling as more images get thrown at it).

**Landscape check (2026-08-09).** Re-verified `03-design-rfc.md`'s
OCI/Incus assumptions against the current ecosystem before drafting the
constraints above:

- LXD (Canonical) still has zero native OCI/application-container
  support through its latest release (6.9, June 2026) — confirmed via
  its own docs and release notes, not just assumed to still be true.
- Incus's native `instance_oci` support (rejected as a target in the
  RFC's "Alternatives") has matured further since — introduced 6.3
  (July 2024), still receiving incremental polish through 7.3 (July
  2026). The audience argument for staying on LXD (target users have
  LXD installed, not Incus) is unchanged by this, but the size of the
  gap Phase 2 is choosing to hand-build instead of getting for free is
  now more concretely documented (see the four constraints above).
- **New since the RFC was written:** [`canonical/peel`](https://github.com/canonical/peel),
  a Canonical-org prototype for running OCI images directly inside LXD
  containers, created 2026-07-03. Its own README states it "should not
  be used in a production environment," and it has no presence in LXD's
  own roadmap or release notes. Worth a light watch — if it matures into
  a real, blessed LXD feature, it could eventually simplify or replace
  this hand-rolled pipeline — but it is not a reason to delay or
  redesign Phase 2 today.
