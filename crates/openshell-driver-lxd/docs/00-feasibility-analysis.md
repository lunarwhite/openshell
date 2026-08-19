# LXD Driver Feasibility Analysis

> **Status note.** This is the original technical feasibility analysis for
> LXD support, retained as a historical decision record — not the current
> plan of record. Its "defer" conclusion reflected a narrower framing (LXD
> as an interchangeable alternative to Docker/Podman/Kubernetes) that a
> later decision has since moved past: OpenShell should support LXD
> (LXD/LXC on Ubuntu, container instances) natively. Current documents,
> following the project's own triage → spike → design → implementation
> chain locally:
>
> - `01-triage.md` — local triage record.
> - `02-spike.md` — technical investigation and proposed approach.
> - `03-design-rfc.md` — local RFC-shaped design draft.
> - `04-implementation-plan.md` — phased implementation plan with user
>   stories, deliverables, and test plans.
>
> Note also that this document's Incus discussion (§2.1's "LXD vs Incus"
> framing throughout) is no longer in scope: current direction targets
> LXD/LXC on Ubuntu only.
>
> Also note: several passages below cited "the current issue draft" for
> user-facing framing (real-world scenarios, a cluster-mobility scenario,
> an "Alternatives Considered" section covering cloud-native drivers and a
> Firecracker-backed VM driver). No such issue was ever filed —
> `01-triage.md` documents the decision to keep this work local-only
> instead — so those were dangling pointers. Fixed inline to point at the
> local document that now covers the same ground, or to note plainly where
> nothing does.

## Executive Summary

Adding LXD as a compute driver for OpenShell is **architecturally feasible** but brings **moderate complexity** with **limited value-add** over existing runtimes, when evaluated purely as an alternative to Docker/Podman/Kubernetes. LXD's system-container model aligns with OpenShell's isolation requirements, but its heavyweight operational footprint and overlap with existing drivers made it a lower-priority target under that framing.

---

## 1. LXD Overview

### What is LXD?

LXD is a next-generation system container and virtual machine manager built on top of LXC (Linux Containers). Key characteristics:

- **System containers**: Full OS environments with init systems (systemd), not single-application containers
- **REST API**: All operations exposed via HTTP API over Unix socket or TLS/TCP
- **Image-based**: Uses pre-built images for various Linux distributions
- **Advanced features**: Live migration, snapshots, storage management, clustering
- **Dual mode**: Supports both system containers (LXC-based) and virtual machines (QEMU-based)

### LXD vs Current OpenShell Runtimes

| Aspect | Docker/Podman | LXD System Containers | VM (libkrun) | LXD VMs |
|--------|---------------|----------------------|--------------|----------|
| **Isolation boundary** | Container + nested netns | System container + nested netns | Hardware-level VM | Hardware-level VM |
| **Init system** | Supervisor as PID 1 | Full systemd possible | Supervisor as PID 1 | Full systemd possible |
| **Startup time** | <1s | 2-5s | 10-30s | 5-15s |
| **Resource overhead** | Minimal | Low | Moderate | Moderate |
| **Live migration** | No | Yes | No | Yes |
| **Operational complexity** | Low | Moderate | Moderate | High |
| **Primary use case** | App containers | System containers | Micro-VMs | Full VMs |

---

## 2. Architectural Fit Assessment

### 2.1 Driver Interface Compatibility

**Status:** ✅ **COMPATIBLE**

LXD's REST API cleanly covers every `compute_driver.proto` RPC — `POST /1.0/instances`, `GET/PUT/DELETE /1.0/instances[/<name>]`, and the `/1.0/events` websocket. See the implementation plan's Phase 1 "Implementation Steps" for the exact per-RPC mapping; it isn't repeated here.

### 2.2 Supervisor Delivery Model

**Status:** ⚠️ **REQUIRES ADAPTATION**

Current OpenShell supervisor delivery mechanisms, for context:

| Driver | Mechanism |
|--------|-----------|
| Docker | Bind-mount from host |
| Podman | OCI image volume mount |
| Kubernetes | Init container or sidecar image |
| VM | Embedded in rootfs |

LXD has three candidate mechanisms: pre-building images with the supervisor baked in (native to LXD's model, works for both container and VM types, but requires a custom image build/publish pipeline); file-push via the LXD file API (works with any base image, no build pipeline, but is a post-creation injection step); and mounting a host path as a disk device (closest to Docker's bind-mount model). This original analysis assumed a host-path disk device requires privileged access and breaks LXD clustering (this option is not viable if clustering ever matters, per §4.3) — **the privileged-access half of that assumption was never actually checked here and was later confirmed false**: Phase 1's confinement spike validated a read-only, `shift=true` disk device on a fully unprivileged (`security.privileged=false`) instance (see `02-spike.md`'s "Risks & Open Questions" for where this was flagged as unverified, and the crate README's "Step 0 result" for the empirical confirmation). The implementation plan selects exactly this disk-device mechanism for Phase 1, not file-push — the create-then-inject race window file-push would add was disqualifying; that decision and its rationale live there, not here.

### 2.3 Network Model

**Status:** ⚠️ **HYBRID COMPATIBILITY**

OpenShell requires an outbound supervisor callback to the gateway, a nested network namespace for agent process isolation, and a policy-enforcing CONNECT proxy. Of LXD's three networking modes:

- **Bridged (default):** the instance gets an IP on the LXD bridge and can reach the host via `_gateway.lxd` (`_gateway.incus` on Incus) on a managed bridge, LXD/Incus ≥ 4.16 — or, per the implementation plan's actual choice, by having the driver read the bridge's own gateway IP back from LXD's network config and inject it directly, keeping `_gateway.lxd` a documented fallback rather than the verified path (that fallback remains unexercised even after real-daemon testing; see the crate README's "What the Stage 2 pass does NOT prove"). The supervisor can create its own nested netns exactly as it does under Docker/Podman.
- **Host mode:** excluded — it exposes the host network namespace directly, breaking the isolation boundary every existing driver maintains.
- **Routed mode:** gives the instance a routable IP directly but requires more complex host-side routing setup; not evaluated further here.

### 2.4 Security and Isolation

**Status:** ✅ **COMPATIBLE WITH ENHANCEMENTS**

OpenShell's layered isolation model works in LXD:

| Layer | LXD System Container | LXD VM |
|-------|---------------------|---------|
| **Supervisor root** | Container root (user-namespaced) | VM root |
| **Nested netns** | Same mechanism as Docker/Podman | Works in VM |
| **Landlock** | Kernel feature available | Available in guest |
| **Seccomp** | Applied by supervisor | Applied in guest |
| **Proxy enforcement** | Same supervisor code | Same supervisor code |
| **User namespaces** | LXD uses by default (`security.idmap.isolated`) | N/A (VM isolation) |

Additional LXD-native security features layer on top of, not instead of, OpenShell's own controls: AppArmor/SELinux profiles, resource limits (CPU, memory, processes), and device access control. LXD containers can be granted the same Linux capability set the Podman driver already requests for its own supervisor — see `crates/openshell-driver-lxd/src/instance.rs`'s `SUPERVISOR_CAPABILITIES` for the exact list and `raw.lxc: lxc.cap.keep` for the grant mechanism (an *exhaustive* allowlist, not Docker/Podman's additive one — see `06-lessons-learned.md`'s headline lesson for why that distinction mattered); not repeated here.

### 2.5 Credential Injection

**Status:** ✅ **MULTIPLE OPTIONS**

OpenShell needs to inject the gateway callback endpoint, sandbox identity, the supervisor relay socket path, TLS material (when HTTPS is enabled), and the sandbox JWT. LXD mechanisms:

1. **Environment variables** — simple, works like Docker/Podman, but visible in `lxc config show` and `/proc/<pid>/environ`:

   ```json
   {
     "environment": {
       "OPENSHELL_ENDPOINT": "https://_gateway.lxd:17670",
       "OPENSHELL_SANDBOX_ID": "sandbox-123"
     }
   }
   ```

2. **File push for secrets** — more secure for the JWT, matches the Docker/Podman bind-mount model:

   ```bash
   lxc file push token.jwt sandbox-123/etc/openshell/token.jwt --mode=0400
   ```

3. **LXD profiles** — a template profile carrying common OpenShell config, with per-sandbox overrides layered on top. Not evaluated elsewhere in this document set; worth considering during implementation for reducing per-instance config duplication.

### 2.6 Resource Management

**Status:** ✅ **WELL SUPPORTED**

LXD provides fine-grained resource controls that map directly onto `DriverResourceRequirements`:

```yaml
config:
  limits.cpu: "2"              # cpu_request / cpu_limit
  limits.memory: "2GiB"        # memory_request / memory_limit
  limits.processes: "2048"     # PID limit (matches OpenShell default)

devices:
  gpu:
    type: gpu                  # GpuResourceRequirements
    gputype: physical|mig|sriov
```

LXD's finer-grained controls beyond this basic mapping (storage quotas, bandwidth limits, MIG/SR-IOV GPU device types, hot-pluggable resource changes) were meant to be covered as user-facing capabilities in a GitHub issue's "real-world scenarios" section — never filed, per the status note above — so they aren't reproduced anywhere in the current local document set.

---

## 3. Implementation Complexity Analysis

The implementation plan's crate layout and phased breakdown supersede the original component-by-component effort estimate here; see that document for the current, corrected scope. Two things from the original estimate remain useful and aren't covered elsewhere:

**Rust ecosystem:** a `lxd` crate exists on crates.io but may need updates; building directly on `reqwest` plus hand-written API types is the more likely path.

**Testing infrastructure (unique, not covered elsewhere):**

- LXD daemon setup in CI (GitHub Actions) requires a snap or PPA installation step, unlike Docker/Podman which are typically preinstalled on hosted runners.
- VM-mode testing needs nested virtualization support from the CI runner, which is not guaranteed on all hosted runner tiers.
- Expect longer test wall-clock time than Docker/Podman given LXD's heavier daemon and image-fetch model.
- Beyond environment setup, the actual test surface should mirror the existing Docker/Podman e2e paths: lifecycle correctness, network isolation verification, and resource-limit validation.

---

## 4. Operational Considerations

### 4.1 Deployment Prerequisites

| Driver | Host requirement |
|---|---|
| Docker | Just the Docker daemon |
| Podman | Just the Podman socket |
| VM | Embedded libkrun (no external deps) |
| LXD | LXD daemon installed and running; user in the `lxd` group (or TLS certs for remote access); a configured storage pool (dir, zfs, btrfs, lvm, ceph); a configured network (bridge or routed) |

LXD requires more one-time host setup than any current driver. This is a real operational cost, but it's a cost LXD-native target users have typically already paid for other reasons (it's their existing container/VM platform), which is different from asking a Docker/Podman user to newly adopt LXD.

### 4.2 Platform Support

LXD is Linux-only — it does not run on macOS or Windows, unlike Docker (macOS/Windows via Docker Desktop), Podman (macOS via machine, Windows via WSL2), or the VM driver (macOS via libkrun). This is a real limitation for cross-platform developer workflows, though irrelevant to the Linux-only, LXD-native deployment targets this feature is aimed at.

### 4.3 Clustering and HA

LXD's native multi-node clustering (shared distributed storage via Ceph, live migration between nodes, automatic failover) is a real differentiator, though the user-facing "cluster-mobility" framing planned for it never made it past a GitHub issue draft that was never filed (see the status note above); `03-design-rfc.md` treats clustering only as an explicit Non-goal for now, not a scenario worth expanding on here. One caution worth preserving from the original analysis: pursuing this adds real operational complexity and overlaps with what the Kubernetes driver already provides for multi-node orchestration — it's additive for LXD-native operators who don't want Kubernetes, not a general replacement pitch.

### 4.4 Observability

LXD's `/1.0/events` websocket stream maps directly onto `WatchSandboxes`, and resource usage metrics are available via the API — both straightforward integration work. Two costs not discussed elsewhere: log collection needs custom implementation (LXD's own `lxc console`/log API isn't a drop-in replacement for the gateway's existing log plumbing), and metrics need their own integration path into gateway observability rather than falling out of the RPC mapping for free.

---

## 5. Value Proposition Analysis

### 5.1 What LXD Adds

Beyond live migration, clustering, MIG/SR-IOV GPU, and resource quotas — real capabilities that would have anchored a GitHub issue's user-facing scenarios had one been filed (see the status note above) — two capabilities are worth preserving here as they aren't stated elsewhere:

- **Advanced storage**: ZFS/Btrfs snapshots and copy-on-write clones, useful for fast sandbox provisioning from a common base state.
- **Full system-container semantics**: real systemd and multiple concurrent services inside one sandbox, for agent workloads that genuinely need an init system rather than a single supervised process tree.

### 5.2 What LXD Doesn't Add

Worth stating plainly as a balanced record, even though the decision has since moved past pure feature-overlap reasoning: LXD does not, by itself, improve on Docker/Podman for basic isolation or local-development ergonomics, does not out-orchestrate a mature Kubernetes cluster, and does not add isolation strength beyond what the existing VM (libkrun) driver already provides. Its ecosystem is smaller than Docker's or Kubernetes', and its operational footprint is heavier than Docker/Podman's. None of this is a reason not to support it — the point of the current proposal is serving users who already have LXD and specifically don't want to add Docker/Podman/Kubernetes — but it's why LXD was never going to win a "which single driver is objectively best" comparison, and that was never the right question.

---

## 6. Technical Risks

### 6.1 High-Risk Areas

Three risks from the original analysis remain open and aren't tracked elsewhere:

1. **API stability** — LXD API versions require careful handling across the range of versions a driver needs to support.
2. **State synchronization** — mapping LXD's instance states (starting, stopping, error, etc.) onto OpenShell's `DriverCondition` model needs care around transient/ambiguous states.
3. **Network configuration conflicts** — a driver-managed bridge network could collide with an operator's existing LXD network setup on the same host.

(A fourth original risk here, "file push timing" — a race window between instance start and supervisor file injection — is moot: §2.2 above already reflects the implementation plan's decision to use a read-only disk device instead of file-push specifically to avoid that race window.)

(The original fifth risk here — untested nested-namespace/Landlock/seccomp support inside an LXD container — is now tracked in the implementation plan's Phase 1 Step 0 confinement spike; not repeated here.)

### 6.2 Maintenance Burden

Ongoing costs beyond initial implementation: tracking LXD API changes across versions, maintaining the OCI-to-LXD image conversion pipeline (built in Phase 2 — see `04-implementation-plan.md`'s "LXD system-container constraints on the OCI pipeline"), platform-specific bug handling, additional e2e test infrastructure, and documentation/support load. The original analysis estimated this at roughly 15-20% additional driver-maintenance overhead relative to an existing driver — a rough, unverified planning estimate, not a measured figure.

---

## 7. Alternative Approaches

Superseded by `02-spike.md`'s "Alternative Approaches Considered" and `03-design-rfc.md`'s "Alternatives" sections, which cover the same ground (pointing users at the existing, libkrun-backed VM driver instead; the unmanaged-extension-driver path; Incus) with updated reasoning reflecting the decision to pursue native support. Not repeated here.

---

## 8. Recommendations

### 8.1 Original Recommendation: Defer

Rationale at the time: limited value-add over existing drivers evaluated as interchangeable alternatives, Linux-only reach, operational complexity without a named use case, and maintenance burden without a proportional identified benefit.

### 8.2 Conditions That Were Identified for Reversal

The original analysis named exactly the conditions that would justify proceeding: an explicit customer/partner request with a concrete deployment, an existing LXD infrastructure integration requirement, a Canonical partnership, or a community contributor stepping forward to maintain it. A Canonical-affiliated contributor requesting exactly this is why this document set now exists.

### 8.3 Phased Approach

Superseded by the implementation plan's own phased breakdown (Phase 1 proof of concept → Phase 2 native driver and feature parity; upstream contribution deferred out of both phases). Not repeated here.

---

## 9. Implementation Sketch

Superseded in full by the spike and implementation plan's crate structure, phase-by-phase design, and API reference — all corrected for the `_gateway.lxd` networking fact and rescoped to LXD only. See `02-spike.md` and `04-implementation-plan.md`.

---

## 10. Conclusion

The original technical verdict stands on its own narrow terms: LXD is feasible with moderate implementation complexity, and if evaluated purely as "which single driver is best," it occupies an awkward middle ground — not as lightweight as Docker/Podman, not as orchestrated as Kubernetes, not as portable as either. What this analysis under-weighted is that LXD-native users aren't choosing between LXD and those other drivers at all — they already have LXD and nothing else, which reframes "middle ground" as "the only option that fits." See `03-design-rfc.md`'s Summary and Motivation for that framing and `04-implementation-plan.md` for how to build it.

---

**Document version:** 1.2 (further corrected; see status note)
**Original author:** Analysis based on OpenShell architecture review
**Original date:** 2026-07-14
**Status:** Historical decision record — superseded in part, see status note at top
