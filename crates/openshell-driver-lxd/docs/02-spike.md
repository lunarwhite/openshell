# Spike: Native LXD Compute Driver

Local equivalent of the `create-spike` skill's output (normally a
structured GitHub issue). No issue will be filed — this is the investigation
record a human would use to decide whether to proceed to design. Follows
from `01-triage.md`. Scope: **LXD/LXC on Ubuntu, container instances
only.** Incus and VM-mode instances are explicitly not evaluated here.

## Problem Statement

An Ubuntu machine with only LXD installed cannot run OpenShell today. Every
built-in compute driver either requires a third-party runtime (Docker,
Podman, Kubernetes) or is excluded from auto-detection and undiscoverable
(VM). There is no path from "I have LXD" to "I have a working sandbox" that
doesn't involve installing something else first.

## Technical Context

The gateway selects a compute driver in one of two ways: an explicit
`compute_drivers = [...]` config entry, or auto-detection when that's unset.
Auto-detection (`openshell_core::config::detect_driver()`) checks for a
Kubernetes in-cluster environment, then a reachable Podman socket, then a
reachable Docker socket — LXD isn't in that list, and the VM driver is
excluded from auto-detection by explicit design (`configured_compute_driver`
in `crates/openshell-server/src/lib.rs` rejects an auto-detected VM
selection with `"vm compute driver is opt-in only"`).

Every compute driver, built-in or not, implements the same gRPC contract:
`openshell.compute.v1.ComputeDriver` in `proto/compute_driver.proto`
(`GetCapabilities`, `CreateSandbox`, `GetSandbox`/`ListSandboxes`,
`StopSandbox`, `DeleteSandbox`, `WatchSandboxes`,
`GetGatewayListenerRequirements`). The gateway itself never touches
container/VM/pod primitives directly — it delegates entirely to whichever
driver is selected, and the driver is responsible for standing up the
workload and running the existing, unmodified `openshell-sandbox`
supervisor inside it. This is the same contract the built-in VM driver
speaks today, gateway-spawned over a Unix socket rather than in-process.

## Affected Components

| Component | Key Files | Role |
|---|---|---|
| Driver selection | `crates/openshell-core/src/config.rs` (`detect_driver`, `ComputeDriverKind`), `crates/openshell-server/src/lib.rs` (`configured_compute_driver`, `build_compute_runtime`) | Decides which driver runs; currently has no LXD awareness |
| Driver protocol | `proto/compute_driver.proto` | The contract any new driver — built-in or extension — implements |
| Existing driver reference | `crates/openshell-driver-podman/` | Closest structural analogue: capability grants, network model, mount schema, rollback-on-create-failure |
| Existing managed-driver reference | `crates/openshell-driver-vm/`, `crates/openshell-server/src/compute/vm.rs` | Shows the gateway-spawned, socket-connected driver pattern this proposal would eventually follow |
| OCI-image conversion reference | `crates/openshell-driver-vm/src/rootfs.rs` | LXD has no native OCI image support (see design doc) — this is the direct template for the conversion pipeline Phase 2 needs (shipped as a pure-Rust pipeline, not the `umoci`-based one originally sketched here; see `04-implementation-plan.md`'s Phase 2 Scope section) |
| Packaging | `install.sh`, `deploy/deb/control.in`, `snapcraft.yaml` | Where Ubuntu users actually hit the gap today |
| Docs | `docs/about/installation.mdx`, `docs/reference/support-matrix.mdx`, `docs/reference/sandbox-compute-drivers.mdx` | Where the driver would need to become discoverable |

## Technical Investigation

### Architecture Overview

Compute drivers are peers, not a hierarchy: the gateway holds no
driver-specific logic beyond translating between its own `DriverSandbox`
model and whatever the driver reports back. Isolation (Landlock, seccomp,
the nested network namespace, the CONNECT proxy) is entirely owned by the
`openshell-sandbox` supervisor running *inside* the workload — the driver's
only isolation-relevant job is granting the supervisor enough Linux
capabilities to do that job itself. The Podman driver is the cleanest
existing reference for this: it runs the supervisor as root inside the
container with a specific capability set
(`SYS_ADMIN`/`NET_ADMIN`/`SYS_PTRACE`/`SYSLOG`/`DAC_READ_SEARCH`/`SETPCAP`)
and otherwise gets out of the way.

### Code References

| Location | Description |
|---|---|
| `crates/openshell-core/src/config.rs::detect_driver` | Auto-detection order; would need an LXD socket probe added |
| `crates/openshell-server/src/lib.rs::configured_compute_driver` | Where VM's opt-in-only exclusion is hardcoded; the eventual insertion point for LXD if it should be excluded or included |
| `crates/openshell-server/src/compute/mod.rs::connect_remote_compute_driver` | The entire gateway-side mechanism for talking to a driver over a Unix socket — this is what makes a PoC possible with zero core changes |
| `crates/openshell-server/src/compute/driver_config.rs::RemoteDriverConfig` | The minimal config surface (`socket_path`) for an unmanaged extension driver |
| `crates/openshell-driver-podman/README.md` (Creation Flow, Capability Breakdown sections) | Direct pattern to follow for capability grants and rollback-on-failure |
| `crates/openshell-driver-vm/README.md`, `crates/openshell-server/src/compute/vm.rs` | Direct pattern for a gateway-managed (spawned, socket-connected) driver |

### Current Behavior

A fresh Ubuntu install with only LXD: `install.sh` installs the `.deb`
package (which bundles the VM driver binary and has no Docker/Podman
dependency), enables the systemd user service, and waits for the gateway to
report a working listener. The gateway calls `detect_driver()`, which
checks Podman then Docker, finds neither, returns `None`, and
`configured_compute_driver` returns a hard config error. The gateway process
never comes up. `install.sh` times out waiting for it and dumps generic
diagnostics with no LXD-specific guidance, because none exists.

### What Would Need to Change

For a working PoC (Phase 1 — see the implementation plan for the full
breakdown):

- A new driver implementing **seven** `ComputeDriver` RPCs against the
  local LXD Unix socket — the original six lifecycle RPCs plus
  `ValidateSandboxCreate`, which every existing driver implements and which
  is not optional in the generated service trait — using
  `POST/GET/PUT/DELETE /1.0/instances[/<name>]` and the `/1.0/events`
  websocket.
- Container-type LXD instances, `security.nesting=true`, and the same
  capability set the Podman driver already grants. **This is the single
  largest unverified assumption in the whole investigation, and it is
  riskier than "just test it": unlike Docker's `apparmor=unconfined` or
  Podman's `seccomp_profile_path: unconfined`, LXD has no equally narrow,
  single-purpose toggle for the specific mount/syscall operations the
  supervisor needs. The two available levers, `security.nesting` and
  `security.privileged`, both trade away isolation beyond just "unblock
  netns setup," and the fallback if neither works cleanly could be
  `security.privileged=true` — root mapped to host root — which would
  undercut the point of adding an LXD driver to a sandboxing product at
  all.** Resolve this empirically, before writing any driver code: inside a
  default (unprivileged) LXD container, attempt `ip netns add`, a
  `landlock_create_ruleset` probe call, and `unshare --net` + veth
  creation — first with LXD defaults, then again with
  `security.nesting=true` — to see exactly which primitive fails under
  which setting and whether `security.privileged` is actually required.
- Supervisor and JWT delivery mechanism is **not yet settled** — two
  candidates, unresolved: (a) file-push (`POST /1.0/instances/<name>/files`),
  no image-build pipeline needed, but a post-creation injection step with a
  create-then-inject race window; (b) an LXD `disk` device pointing at a
  host-side file, read-only — the direct LXD analogue of Docker's bind-mount
  delivery (Docker is the better reference driver for this specific
  mechanism, not Podman, since Podman's OCI image-volume approach has no
  LXD equivalent). Option (b) removes the race window entirely, but an
  earlier, unverified feasibility pass claimed disk-device host-path mounts
  "require privileged access" — that claim has never actually been checked
  against real LXD behavior by any investigation so far. Resolve which
  claim is true as part of the same Phase 1 validation pass as the
  capability/nesting question, and prefer (b) if it doesn't actually need
  `security.privileged`. Either way, reuse the existing, driver-agnostic
  `openshell_core::driver_utils::sandbox_token_path` /
  `openshell_core::paths::set_file_owner_only` helpers Docker already uses
  for the JWT file — no new helper needed.
- A managed-LXD-bridge network with the supervisor callback reaching the
  gateway via `_gateway.lxd` (dnsmasq-resolved on any managed bridge, LXD
  ≥ 4.16) — verified by direct research, not assumed. Whether the driver
  needs to declare a `GetGatewayListenerRequirements` entry for this is
  itself open: LXD's default bridge is host-routable on Linux (unlike
  rootless Podman's pasta), which may mean no special listener requirement
  is needed at all — but that depends on the gateway's own default bind
  address, not just the bridge's routability, and hasn't been checked
  either way yet.

For native/parity (Phase 2): adding `Lxd` to `ComputeDriverKind`, wiring it
into `configured_compute_driver`/`build_compute_runtime`, adding an LXD
socket probe to `detect_driver()`, and matching the Podman driver's
existing feature surface (mTLS, resource limits, driver-config mounts,
rollback-on-failure).

### Alternative Approaches Considered

**Separate repository vs. a new crate in this fork's `crates/`.** A
standalone `openshell-driver-lxd` repository was the original framing (it
avoids needing any change merged upstream), but it has real friction for
development speed: it would need to either vendor `proto/compute_driver.proto`
and duplicate the `tonic`/`prost` codegen setup, or take a fragile
path-dependency across two repositories, and it gets none of the existing
e2e harness (`mise run e2e:docker`/`e2e:podman`/`e2e:vm`, the underlying
pytest suite) or `mise run pre-commit`/`test` tooling for free. A crate
inside this fork's existing Cargo workspace (`crates/openshell-driver-lxd/`)
gets all of that for free by construction, and directly matches "implement
on a local fork branch." **Decision: crate in this fork's `crates/`, not a
separate repository.**

**Unmanaged extension driver vs. wiring in as a built-in from day one.** The
gateway already supports operator-run drivers connected via
`--compute-driver-socket` with zero core changes. Starting there for Phase 1
is faster to a working demo and defers the (larger, riskier) core-wiring
change until the design is proven. Phase 2 is exactly that graduation step.
**Decision: extension-driver pattern for Phase 1, native `ComputeDriverKind`
wiring for Phase 2.**

**LXD only vs. also supporting Incus.** Not evaluated in this spike by
explicit scope decision — the target user runs LXD, not Incus, and
designing for both would add surface area with no immediate need.

### Patterns to Follow

- Capability grants, rollback-on-create-failure, and the network/trust
  model (nested netns via `nsenter --net=`, not `ip netns exec`, to avoid
  needing real `CAP_SYS_ADMIN` in the host user namespace): `crates/openshell-driver-podman/README.md`.
  Podman is the closest analogue here specifically because it's the only
  other driver dealing with a local, Unix-socket daemon API and
  unprivileged/user-namespaced containers — the same situation LXD is in.
- Supervisor and JWT file delivery (bind-mount-style host path injection):
  `crates/openshell-driver-docker/README.md` and
  `openshell_core::driver_utils::sandbox_token_path`/
  `openshell_core::paths::set_file_owner_only` — Docker is the closer
  analogue here, not Podman, since Podman's OCI image-volume mechanism has
  no LXD equivalent.
- Gateway-managed, socket-connected driver lifecycle (for the eventual
  Phase 2 native shape): `crates/openshell-driver-vm/README.md` and
  `crates/openshell-server/src/compute/vm.rs`.
- Driver-config mount schema (`bind`/`volume`/`tmpfs` equivalents, read-only
  by default, protected-path rejection): both the Docker and Podman driver
  READMEs.
- No maintained async Rust LXD client exists (unlike Docker's `bollard`).
  Hand-roll a thin client over the LXD Unix socket, following Podman's own
  precedent of hand-rolling against `hyper` + `UnixStream` rather than
  depending on a third-party crate.

## Proposed Approach

Build a new `crates/openshell-driver-lxd` crate in this fork, targeting LXD
container instances on Ubuntu only. Phase 1 proves the design as an
unmanaged extension driver with no gateway core changes, validating the
capability/nesting assumption empirically against a real local LXD daemon.
Phase 2 promotes it to a native, auto-detected `ComputeDriverKind` with
feature parity against Docker/Podman and a matching e2e test suite. See
`04-implementation-plan.md` for the full breakdown.

## Scope Assessment

- **Complexity:** Medium-High — well-scoped and follows existing driver
  patterns closely (Podman for capabilities/rollback/network, Docker for
  file delivery), but the capability/nesting question isn't just "one
  unknown to check": LXD has no narrow, single-purpose confinement toggle
  the way Docker/Podman do, so an unfavorable result could force a real
  design detour (a maintained custom `raw.lxc`/`raw.seccomp` override, or
  falling back to `security.privileged=true`), not just a config tweak.
- **Confidence:** Medium — high confidence in the protocol/config mechanics
  and the RPC-to-implementation mapping (verified directly against the
  codebase across multiple reference drivers), medium confidence in the
  supervisor-inside-LXD-container assumption specifically, because LXD's
  confinement model outside this repository hasn't been exercised yet.
- **Estimated files to change (Phase 1):** ~8-10 new files in a new crate;
  zero changes to existing files.
- **Issue type:** `feat`

## Risks & Open Questions

- **(Highest risk — resolved.)** Does `security.nesting=true` plus the
  Podman-equivalent capability set let the supervisor's
  nested-netns/Landlock/seccomp setup run unmodified inside an LXD
  container, without needing `security.privileged=true`? See "What Would
  Need to Change" above for why this needed a direct empirical test rather
  than an assumption based on Docker/Podman's experience. **Resolved**: the
  crate README's "Step 0 result" section records the confinement spike's
  actual outcome — nesting alone passed twice, with the caveat that a
  no-nesting run also passed both times, so nesting's strict necessity is
  unconfirmed rather than cleanly proven.
- Does an LXD `disk` device with a host-path `source` actually require
  `security.privileged=true`, as an earlier, unverified feasibility pass
  claimed? **Resolved**: no — the crate README's "Step 0 result" confirms a
  read-only, `shift=true` disk device works on a fully unprivileged
  instance, and Phase 1 used it over file-push for exactly the
  create-then-inject race-window reason this risk called out.
- Which LXD capability-grant mechanism (`raw.lxc` vs. newer native
  `security.syscalls`-style config) is stable across the LXD versions
  targeted? To be confirmed during Phase 1.
- Does `_gateway.lxd` resolve reliably across the LXD version range on
  target Ubuntu releases? Documented as requiring LXD ≥ 4.16 and a managed
  bridge network; needs a direct check against whatever LXD version ships
  on the target Ubuntu release(s).
- Does the driver actually need to declare a `GetGatewayListenerRequirements`
  entry for the `_gateway.lxd` callback, or is LXD's default bridge being
  host-routable on Linux enough on its own? Depends on the gateway's default
  bind address, not just the bridge — unconfirmed either way.

## Disposition Readiness

- **State:** ready for design (equivalent of `state:validated`)
- **Assessment:** The investigation supports proceeding directly to a
  design/implementation plan. The one real open risk (capability/nesting
  sufficiency) is a Phase 1 validation task, not a blocker to starting.
- **Missing evidence:** None blocking. Empirical confirmation of the
  capability/nesting assumption is Phase 1's first task, not a prerequisite
  to writing the design.

## Test Considerations

- **Unit tests:** LXD instance-spec translation (`DriverSandbox` ↔ LXD
  config/devices JSON), LXD status-to-`DriverCondition` mapping — follow the
  existing Podman driver's unit test structure.
- **Integration/e2e tests:** full lifecycle (create → connect → exec →
  delete) against a real local LXD daemon, network isolation verification,
  resource-limit enforcement — mirror the existing `mise run e2e:podman`
  pattern once a comparable `e2e:lxd` lane exists.
- **Test infrastructure gap:** LXD isn't preinstalled the way Docker/Podman
  often are on CI runners; a CI lane would need an LXD install step (snap or
  PPA) before any e2e test can run. Not a Phase 1 blocker since Phase 1
  validates locally, but a real Phase 2 task.
