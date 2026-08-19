---
authors:
  - "(local draft — author TBD when/if filed upstream)"
state: implemented
links:
  - (no originating GitHub issue — local-only workflow; see 01-triage.md and 02-spike.md)
---

# RFC (local draft, unnumbered) — Native LXD Compute Driver

Local equivalent of the `create-rfc` skill's output. Real OpenShell RFCs
require a maintainer-assigned number from an originating GitHub issue
(`rfc/README.md`); since no issue will be filed at this stage, this document
follows the RFC template structure without a number or a submission,
strictly to drive the local design decision. Renumber and file for real
only if/when upstream engagement (deferred, out of scope for now) happens.
**State reflects current reality, not the state this was drafted in:**
both phases below have since substantially shipped — see
`04-implementation-plan.md`'s dated status updates and the crate README for
current, ground-truth status; this document is left describing the
decisions and their rationale as reasoned at design time, not updated
blow-by-blow as implementation diverged from the plan in the specific
places called out inline below.

## Summary

Add a native LXD compute driver so OpenShell runs on Ubuntu hosts that have
LXD and no other container/orchestration runtime. Delivered in two scoped
phases: a proof of concept as an out-of-tree-pattern extension driver
(zero gateway core changes), then promotion to a first-class
`ComputeDriverKind`, kept **opt-in** (like the VM driver) rather than
auto-detected, with feature parity against Docker and Podman. Scope is
LXD/LXC on Ubuntu, container-type instances only — Incus and LXD VM-type
instances are explicit non-goals for now. LXD has no native OCI image
support, unlike every other driver's backend — Phase 2 must include an
image-conversion pipeline before "parity" is meaningful.

## Motivation

Every current compute driver requires something beyond what a stock LXD
host already provides. Docker, Podman, and Kubernetes each need a separate
runtime installed. The VM driver doesn't need any of those, but it's
deliberately excluded from auto-detection and isn't shipped in the snap
package at all — so in practice it's undiscoverable, and a machine with only
LXD gets a hard, unguided startup failure from the standard install flow.
Ubuntu users and operators who have LXD and have deliberately not added
Docker, Podman, or Kubernetes have no path to a working OpenShell install
today. Left unchanged, this population simply can't use OpenShell without
first adopting a runtime they didn't want.

## Non-goals

- **Incus support.** Out of scope. The target user runs LXD, not Incus —
  even though Incus's `instance_oci` type would sidestep the OCI-image gap
  below. Structure LXD-facing code as its own module, not a new trait
  hierarchy, so a future Incus target stays plausible without building for
  it speculatively now.
- **LXD VM-type instances.** Container-type only for both phases covered by
  this RFC. VM mode is a separate, larger effort (different boot path,
  storage, and migration model) and is not scoped here.
- **Upstream contribution process.** Filing a GitHub issue, requesting a
  real RFC number, and opening a PR against `NVIDIA/OpenShell` are
  deliberately deferred and not planned as part of this document.
- **GPU passthrough (MIG/SR-IOV or LXD's `gputype=mig`).** Deferred, not
  permanently excluded — LXD's `gpu` device already accepts the same
  `nvidia.com/gpu=<n>` CDI notation Docker/Podman use, so this is less work
  than it looks, but it adds a second untested variable (GPU device-node
  visibility under an unprivileged container) on top of the confinement
  question below and isn't needed for core lifecycle parity.
- **Multi-host clustering and live migration.** LXD's native clustering
  would duplicate the Kubernetes driver's scheduling role with a weaker
  ecosystem. The driver talks to exactly one local LXD daemon over its
  local Unix socket — the same single-machine posture Docker and Podman
  already have.
- **Remote (TLS) LXD servers.** The driver only ever dials the local Unix
  socket, never a remote HTTPS LXD endpoint. Accepting one reopens a second
  TLS-trust/cert-rotation problem next to the gateway's own mTLS story for
  a use case nobody has asked for.
- **LXD projects as a tenancy/workspace boundary.** LXD projects map
  naturally onto `DriverSandbox.workspace`, but adopting that mapping now
  adds a second per-sandbox namespacing concept before the basic lifecycle
  is proven. Use the default project in Phase 1; revisit as a Phase 2+
  parity candidate.
- **Supporting both the LXD snap and the legacy Debian/Ubuntu-archive
  package.** They have different socket paths, different default
  AppArmor/seccomp profile versions, and the legacy package is deprecated
  upstream. Target the snap (Canonical's stated default on current Ubuntu
  LTS); fail loudly rather than guess if the legacy package is found
  instead.

## Proposal

### Component boundary

A new crate, `crates/openshell-driver-lxd/`, in this fork's existing Cargo
workspace. It implements `openshell.compute.v1.ComputeDriver` from
`proto/compute_driver.proto` against the local LXD REST API over its Unix
socket. The gateway's public interface doesn't change: sandbox creation,
connection, exec, and deletion behave identically to any other driver from
the CLI's perspective, because the supervisor inside the workload — not the
driver — owns policy enforcement.

### Phase 1 shape: unmanaged extension driver

The gateway already supports an operator-run driver process connected over
a Unix socket with no core code changes
(`RemoteDriverConfig { socket_path }` in
`crates/openshell-server/src/compute/driver_config.rs`). Phase 1 uses this
mechanism directly:

```toml
[openshell.gateway]
compute_drivers = ["lxd"]

[openshell.drivers.lxd]
socket_path = "/run/openshell/lxd.sock"
```

Design choices for this phase, all container-type LXD instances, LXD
default **unprivileged** posture (never `security.privileged=true` as a
starting point):

| Aspect | Choice |
|---|---|
| Supervisor + JWT delivery | Read-only `disk` device with `shift=true` (idmap-aware), staged once from the driver's own state directory — not file-push. File-push is a separate post-create RPC into the container's own mutable writable layer, which turns a supposed-to-be-immutable supervisor binary into container-owned mutable state and adds a create-then-inject race window; a disk device composes into the single atomic create call, matching Docker's bind-mount pattern. |
| Networking | Managed LXD bridge; read the bridge's gateway IP back from LXD's network config and inject it explicitly (mirrors Podman's existing `host_gateway_ip` override code path) as the primary mechanism. `_gateway.lxd` (dnsmasq-resolved, LXD ≥ 4.16, managed bridge) is a documented fallback, not the sole mechanism, since it depends on a specific LXD version and dnsmasq actually running. |
| Credential injection | Environment variables for identity/endpoint (driver-controlled values always override template/image values, matching the architecture-wide rule); the sandbox JWT travels via the same disk-device mechanism as the supervisor binary, never a plain env var. |
| Capabilities | `security.nesting=true` on an unprivileged container, plus the Podman driver's existing capability set (`SYS_ADMIN`, `NET_ADMIN`, `SYS_PTRACE`, `SYSLOG`, `DAC_READ_SEARCH`, `SETPCAP`). Current LXD guidance treats `security.nesting=true` alone on an unprivileged container as having no material security impact — a materially better starting point than treating it symmetrically with `security.privileged`. |
| Sandbox image | **Not general OCI resolution.** LXD has no native OCI image support (see below). Phase 1 pins to one image, manually pre-converted once via `umoci unpack` + `lxc image import`. General image handling is explicitly a Phase 2 problem. |

**Sequencing the confinement risk.** Before writing any gRPC service code,
run a throwaway, driver-free spike: hand-create one unprivileged LXD
container with `security.nesting=true`, attach the existing
`openshell-sandbox` binary via a disk device, and manually exercise its
real startup sequence (netns creation, Landlock ruleset install, seccomp-BPF
install) inside it. Only once that returns a concrete answer — works with
nesting alone, works with nesting plus specific narrow `raw.apparmor`
additions, or doesn't work without `security.privileged` — does the full
`CreateSandbox`/rollback lifecycle get built. Mechanical work that doesn't
depend on the answer (RPC stubs, sandbox-ID validation, wire plumbing
proven against a throwaway privileged container) can proceed in parallel,
clearly labeled as throwaway if the spike's answer forces a design change.
**If the answer is "needs `security.privileged=true`," that's a
stop-and-reconsider-scope outcome, not a shippable fallback** — a sandbox
driver that needs root-equivalent host privilege to unblock its own
isolation setup has defeated its own purpose.

### Phase 2 shape: native driver, kept opt-in

Once Phase 1 validates the design against a real local LXD daemon, promote
it to a built-in `ComputeDriverKind` (`crates/openshell-core/src/config.rs`),
wired into `configured_compute_driver`/`build_compute_runtime`
(`crates/openshell-server/src/lib.rs`) the same way VM is today — as a
**managed subprocess** communicating over the same `compute_driver.proto`
socket contract, not full in-process integration like Docker/Podman. This
isolates the newest, least-proven code (a from-scratch LXD REST client plus
the OCI-conversion pipeline below) in its own process boundary, so a panic
or hang there can't take the gateway down.

**Kept explicitly opt-in, not added to `detect_driver()`'s auto-detection
list.** LXD has no rootless daemon mode: `lxd`-group membership is
long-documented, well-known LXD/Ubuntu security guidance as host-root-
equivalent, because any group member can trivially create a privileged
container. Docker/Podman auto-detection just means "a local daemon socket
answered a ping," and for both of those, reachability and "safe to use this
way" are close to the same fact because genuinely unprivileged/rootless
operation exists. That's not true for LXD. Auto-selecting a driver whose
usability precondition is an unacknowledged host-privilege statement is a
worse failure mode than today's explicit `detect_driver()` config error.
Revisit auto-detection only after Phase 2 has real operational track record
— not as part of the initial native launch.

**The OCI-image gap (the largest Phase 2 workstream).** LXD is purely
image-based with its own format (rootfs + `metadata.yaml`, built by
`distrobuilder`) and has no native OCI image support — that capability
exists only in the out-of-scope Incus fork (`instance_oci`). Every
OpenShell sandbox image is an OCI image, and every other driver gets OCI
handling for free (Docker/Podman pull directly; Kubernetes runs OCI pods
natively). The VM driver is the only other driver that solved this same
problem, and its `crates/openshell-driver-vm/src/rootfs.rs` is the direct
template: unpack the requested OCI image with `umoci`, repackage into the
target runtime's native format, cache by image digest. Phase 2 needs the
LXD-flavored version: `umoci unpack` → package into LXD's expected image
shape (`metadata.yaml` + squashfs/tarball) → `POST /1.0/images` → cache by
digest, mirroring how Docker/Podman pin to an inspected immutable image ID
rather than a mutable tag. (As built, the `umoci` step of this sketch was
replaced by a pure-Rust registry pull plus a hand-rolled Rust layer-merge —
no `umoci`/`skopeo` subprocess at all; see `04-implementation-plan.md`'s
Phase 2 Scope section for the corrected account. The rest of this
paragraph's shape — pull, merge/translate, package, cache by digest —
held.)

**Feature-parity bar for Phase 2**, beyond the gateway-wiring changes above:

- Process identity resolution equivalent to Docker/Podman's
  image-inspection-driven `run_as_user`/`run_as_group` (harder here since it
  means inspecting the unpacked OCI rootfs before conversion, not an
  LXD-native image).
- Rollback-on-create-failure with Podman's idempotent-by-sandbox-ID
  discipline, adapted for LXD's **async operation model** (instance create
  returns a background operation UUID to poll, not a synchronous result) —
  a restart mid-create needs the same "reconcile against sandbox ID, don't
  assume in-flight state" handling the VM driver already has.
- Driver-config mounts (bind/volume/tmpfs equivalents) matching the
  Docker/Podman `enable_bind_mounts`-gated pattern.
- Resource limits (`DriverResourceRequirements` CPU/memory) actually
  enforced, not silently ignored the way VM currently ignores them.
- Basic OCSF/tracing parity with the other drivers' lifecycle logging.

GPU and LXD-projects-as-workspace-scoping remain stretch items past this
bar, per Non-goals.

### User experience impact

Before this RFC: an Ubuntu user with only LXD cannot get a working gateway
from the standard install flow. After Phase 2: `compute_drivers = ["lxd"]`
or plain auto-detection gives that same user the same zero-configuration
experience Docker and Podman users already have.

## Implementation plan

See `04-implementation-plan.md` for the full phase-by-phase breakdown
with user stories, deliverables, and test plans. Summary:

1. **Phase 1 (PoC):** a throwaway confinement spike first, then new crate,
   extension-driver pattern, container mode, disk-device delivery, pinned
   to one manually pre-converted image, validated against a real local LXD
   daemon on a fork branch. No gateway core changes.
2. **Phase 2 (native/parity):** promote to a built-in, **opt-in**
   `ComputeDriverKind` run as a managed subprocess (VM's shape, not
   Docker/Podman's), build the OCI-image conversion pipeline, reach feature
   parity with Docker/Podman, add a comparable e2e test suite, update
   user-facing docs.

Both phases are implemented incrementally on local fork branches, gated by
`mise run pre-commit` and the relevant test suites before each is
considered done.

## Risks

- **Capability/nesting sufficiency (highest risk).** LXD has no toggle as
  narrow as Docker's `apparmor=unconfined` or Podman's
  `seccomp_profile_path: unconfined` — the two available levers
  (`security.nesting`, `security.privileged`) either widen exposure beyond
  "unblock namespace setup" or require an unproven custom
  `raw.lxc`/`raw.apparmor` override. If the throwaway spike shows the
  supervisor needs `security.privileged=true`, that's a stop-and-reconsider
  outcome for the whole design, not a shippable fallback. Mitigation: the
  spike runs before any driver code is written, sequenced exactly as
  described in Proposal.
- **LXD has no native OCI image support (load-bearing, not a detail).** Every
  sandbox image OpenShell uses is an OCI image; LXD's own image format is
  incompatible. Mitigation: Phase 1 sidesteps this with one manually
  pre-converted pinned image; Phase 2 builds a conversion pipeline modeled
  on the VM driver's `rootfs.rs` (pull, merge/translate, package, cache by
  digest), scoped as the largest single Phase 2 workstream, not a
  footnote — built as a pure-Rust pipeline rather than the `umoci`-based
  one originally sketched here; see `04-implementation-plan.md`'s Phase 2
  Scope section for the corrected account.
- **The `lxd` group is host-root-equivalent, with no rootless escape hatch.**
  Unlike Podman, LXD has no rootless daemon mode. Whichever account runs
  the driver needs `lxd`-group membership — a materially different
  host-trust statement than reaching a rootless Podman socket. This is a
  primary reason the driver stays opt-in rather than auto-detected.
- **Confinement-profile version drift under unattended upgrades.** A
  `security.nesting=true` configuration validated once against today's
  Ubuntu LTS + LXD version isn't guaranteed to keep working, or keep being
  safe, after an unattended snap/apt upgrade — unlike a pinned container
  image, which protects Docker/Podman/K8s from this exact drift. Mitigation:
  document a tested LXD+kernel version range; add a confinement regression
  test to whatever CI lane exercises this driver.
- **Storage-backend-dependent disk-device behavior.** The `shift=true`
  idmap-aware disk-device option (needed for supervisor/JWT delivery) does
  not behave uniformly across LXD's storage drivers (dir/btrfs/zfs/lvm/ceph).
  Mitigation: pin to and document one tested backend for Phase 1; expand
  only as verified.
- **LXD's async operation model complicates rollback.** Instance creation
  returns a background operation UUID to poll, a different failure-mode
  shape than Docker/Podman's driver code currently handles. Mitigation:
  reuse the VM driver's restart-time reconciliation discipline rather than
  assuming Podman's synchronous-feeling rollback pattern transfers directly.
- **LXD version drift for the networking fallback.** `_gateway.lxd` requires
  LXD ≥ 4.16 and a managed bridge; older LXD or unmanaged-network setups
  have no equivalent, which is exactly why it's a documented fallback and
  not the primary mechanism (see Proposal).
- **No mature async Rust LXD client exists.** The one community `lxd` crate
  on crates.io is a stale, unmaintained 2017-era synchronous library.
  Mitigation: hand-roll a thin REST client following the Podman driver's
  own precedent (`hyper` + `UnixStream`), an absorbable, bounded cost.
- **Narrow demand relative to above-average ongoing maintenance cost.**
  "Ubuntu host with LXD but no Docker/Podman/Kubernetes" is a real but
  narrower audience than the other three drivers', and the risks above
  (version drift, storage-backend variance, async rollback) mean
  above-average ongoing maintenance. Not a reason to reject the design, but
  a factor for human disposition alongside the technical proposal.
- **Two-repository friction was avoided, not eliminated.** Building in this
  fork's `crates/` accelerates development now, but if this never merges
  upstream, that same crate has to either stay a permanent fork or move to
  a standalone repository later. Mitigation: not a Phase 1/2 concern — this
  RFC treats upstream contribution as future, out-of-scope work.

## Alternatives

**Nested Docker/Podman inside an LXD container.** Works today with zero
code changes, since the existing Docker/Podman drivers don't care what
hosts the daemon they talk to. Rejected because it doesn't solve the actual
problem — it relocates the dependency instead of removing it.

**Point users at the existing VM driver instead.** Already in-tree, already
opt-in, already solves "no Docker/Podman/Kubernetes." Rejected as a
substitute because it doesn't serve "I already have LXD provisioned and
want to reuse it" at all — it's a fully separate stack (libkrun, its own
rootfs/overlay handling) that happens to not need a third-party runtime
either, not an LXD integration.

**Leave this deferred, per the earlier internal feasibility pass.** That
pass evaluated LXD as an interchangeable alternative to Docker/Podman/Kubernetes
and reasonably concluded low value-add under that framing. It didn't weigh
the population of users who have LXD and specifically don't want a
third-party runtime, which is the actual audience this RFC targets.

**Target Incus instead of (or in addition to) LXD.** The alternative worth
taking most seriously: Incus already solves the OCI-image gap via
`instance_oci` and shares most of LXD's REST API shape. Still rejected for
now — the stated target audience has LXD (Canonical's larger-install-base
default on Ubuntu), not Incus; the two APIs have publicly diverged since
the 2023 fork and will keep drifting; and building speculative dual-target
abstraction before a single LXD driver has proven itself is premature
generalization. LXD-facing code stays in its own module so an Incus target
remains a plausible follow-on without being built for speculatively now.

**Separate repository instead of a crate in this fork.** Avoids needing any
upstream-adjacent change, but adds real friction today: duplicated proto
codegen, no shared e2e harness, no shared workspace tooling. Rejected for
Phases 1–2 on pure development-velocity grounds; revisit only if/when this
work needs to exist independently of this fork.

**In-process driver (Docker/Podman shape) vs. managed-subprocess driver
(VM shape) for the Phase 2 native form.** Both are legitimate; this RFC
picks the VM shape (see Proposal) because it requires the least new
gateway-side plumbing (`compute/vm.rs`'s spawn/socket-wait/rollback code is
already generic enough to reuse nearly as-is) and isolates the newest,
least-proven code — a from-scratch LXD REST client plus the OCI-conversion
pipeline — in its own process boundary.

## Prior art

- `crates/openshell-driver-podman/` — the direct pattern for capability
  grants, network model, and mount schema.
- `crates/openshell-driver-vm/` and `crates/openshell-server/src/compute/vm.rs`
  — the direct pattern for a gateway-managed, socket-connected driver (the
  shape Phase 2 grows toward).
- `01-triage.md`, `02-spike.md` — the investigation this RFC builds on.
- `00-feasibility-analysis.md` — the earlier feasibility pass; superseded in
  part, kept as a historical decision record.

## Open questions

- **Resolved.** Confirmed empirically in Phase 1, Step 0:
  `security.nesting=true` alone (and, anomalously, even LXD's unprivileged
  defaults with no nesting requested) let the supervisor's netns +
  Landlock + seccomp-BPF setup succeed, with no `security.privileged`
  needed — see the crate README's "Step 0 result" for the full outcome
  and caveats.
- **Resolved.** Phase 1/2 pin to and document only the `dir` storage
  backend (see `04-implementation-plan.md`'s Phase 1 Step 0 and the crate
  README) — other backends remain untested and unsupported.
- What's the real fidelity/performance cost of the conversion pipeline
  across the range of images the OpenShell Community sandbox-image
  repository actually publishes — does anything (multi-arch manifests,
  device nodes, unusual layer ownership) break it? (Partially answered:
  the ownership question already surfaced a real bug on the one real
  sandbox image tested — see `04-implementation-plan.md`'s Phase 2 Step 1
  update and `06-lessons-learned.md`'s sixth lesson. Multi-arch and the
  rest of the Community repository's actual image range remain open.)
- What LXD (and Ubuntu/kernel) version range is this driver validated
  against, and is there a plan to catch confinement-relevant regressions
  (a real historical example exists: an Ubuntu AppArmor/`pivot_root`
  regression inside LXD containers) before they reach users?
- Which LXD versions ship on the Ubuntu release(s) actually being targeted,
  and do they meet the `_gateway.lxd` fallback's (≥ 4.16) requirement, for
  hosts where the bridge-gateway-IP-readback approach isn't preferred?
- Is there enough real operator demand to justify this driver's
  above-average ongoing maintenance cost relative to the other three
  built-ins? A disposition question, not a technical one, but one that
  should be answered explicitly rather than assumed by momentum once a PoC
  exists.
