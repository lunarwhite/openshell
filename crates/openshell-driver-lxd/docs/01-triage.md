# Triage: Native LXD Compute Driver Support

Local equivalent of the `triage-issue` skill's output. No GitHub issue exists
and none will be filed — this establishes the same facts a human would need
for disposition, recorded locally instead of as an issue comment. Scope:
**LXD/LXC on Ubuntu only.** Incus is explicitly out of scope — the target
user runs LXD, not Incus, and there is no need to design for both.

## Triage Assessment

**Classification:** `validated-feature`

### Summary

OpenShell has no compute driver that works on a host with only LXD
installed. The three container/orchestration drivers (Docker, Podman,
Kubernetes) each require a separate runtime; the one driver that doesn't
(VM, via libkrun/KVM) is deliberately opt-in-only and excluded from
auto-detection, and isn't shipped at all in the snap package. The proposal —
a native, auto-detected LXD compute driver — is technically coherent and
feasible. This finding is based on direct investigation of the driver
selection code, the packaging scripts, and the `compute_driver.proto`
contract, not just the feature description.

### Investigation

- `openshell_core::config::detect_driver()` checks, in order: a Kubernetes
  in-cluster environment, a reachable Podman socket, a reachable Docker
  socket. It returns `None` otherwise and never considers the VM driver,
  which the gateway startup path explicitly rejects from auto-detection
  (`"vm compute driver is opt-in only"`).
- The Debian package bundles the VM driver binary with no Docker/Podman
  dependency, but nothing sets `OPENSHELL_DRIVERS`, so a machine with only
  LXD installed gets a hard startup failure from a default `install.sh` run.
- The snap package doesn't bundle the VM driver at all and hard-wires the
  `docker` plug as its only compute-runtime interface.
- The gateway already exposes a stable extension point for exactly this
  kind of new driver (`compute_driver.proto` over a Unix socket,
  `RemoteDriverConfig { socket_path }`), and the built-in VM driver already
  proves the pattern works end-to-end (gateway-spawned, same wire contract).
  This is what makes the proposal a "yes, and here's how," not just "maybe."
- A prior internal feasibility pass (`00-feasibility-analysis.md`) reached a
  "defer" conclusion, but that pass evaluated LXD narrowly as an
  interchangeable alternative to Docker/Podman/Kubernetes. It did not weigh
  the population of users who have LXD and specifically do not want to add
  a third-party runtime, which is the actual problem this proposal targets.

### Affected Components

| Component | Role |
|---|---|
| `crates/openshell-server` | Compute-driver selection (`ComputeDriverKind`, `detect_driver`, `configured_compute_driver`) |
| `crates/openshell-core` | Shared `ComputeDriverKind` enum and driver-name parsing |
| `proto/compute_driver.proto` | The gRPC contract any new driver implements |
| `install.sh`, `deploy/deb/` | Ubuntu packaging and install-time driver selection |
| `docs/about/installation.mdx`, `docs/reference/support-matrix.mdx`, `docs/reference/sandbox-compute-drivers.mdx` | User-facing driver documentation |

### Impact Signals

- **Affected users/scope:** Ubuntu users and operators who have LXD
  installed and have deliberately not installed Docker, Podman, or
  Kubernetes.
- **Regression:** No — this is new capability, not a fix.
- **Workaround:** Running Docker or Podman nested inside an LXD container
  works today with zero code changes, but doesn't solve the actual problem —
  it relocates the dependency rather than removing it.
- **Evidence quality:** High. Based on direct reading of the driver
  selection code, packaging scripts, and protocol definitions across
  multiple investigation sessions, not just the feature description.

### Disposition

This work was directly requested by the repository owner, who intends to
implement and eventually contribute it. Per project convention, a direct
user request authorizes the next phase without requiring `state:accepted`
or any `agent:*` label — those exist to gate *unattended* queue processing
on a real GitHub repository, and neither applies to this local, no-issue
workflow. Proceeding to spike.
