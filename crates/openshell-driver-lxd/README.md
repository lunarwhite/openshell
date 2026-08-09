# openshell-driver-lxd

> **Status: Phase 1 proof of concept complete; Phase 2's OCI-to-LXD
> conversion pipeline now also passes its own end-to-end lifecycle
> against a real LXD daemon.** The confinement spike (Step 0 below)
> passed twice reproducibly; `cargo test -p openshell-driver-lxd --
> --ignored` (the real-daemon `LxdClient` integration test) passes;
> `crates/openshell-driver-lxd/spike/run-stage2.sh` (Phase 1, a
> hand-prepared sandbox image) passes; and, as of 2026-08-09,
> `crates/openshell-driver-lxd/spike/run-stage2-oci.sh` (Phase 2, the
> real, unmodified `ghcr.io/nvidia/openshell-community/sandboxes/
> base:latest` image, pulled and converted by this crate's own
> `src/image.rs` pipeline — no manual prep at all) **also passes**: a
> real `sandbox create -> exec -> delete` lifecycle on a cache-miss
> conversion (224s) and again on a cache-hit resolution of the same
> digest (9s, ~25x faster, direct evidence the digest-cache design
> works). See "Step 0 result" and "What's actually implemented" below
> for the exact path there, including several real, driver-,
> supervisor-, and image-pipeline-level bugs found and fixed only by
> this real-daemon testing (none of which any unit test caught) — and
> "What the Stage 2 pass does NOT prove" for the caveats before treating
> this as production-ready. **Also implemented since (Phase 2, Steps
> 5-8): guest mTLS, resource limits, driver-config bind mounts, and
> rollback/reconciliation hardening — unit-test-verified only, not yet
> exercised against a real daemon.** See "What's actually implemented"
> below for both.

LXD compute driver for OpenShell — Phase 1 scope: LXD/LXC on Ubuntu,
container-type instances only, run as an unmanaged extension driver (zero
changes to gateway core). See:

- `.claude/plans/lxd-01-triage.md` — is this a real problem?
- `.claude/plans/lxd-02-spike.md` — technical investigation
- `.claude/plans/lxd-03-design-rfc.md` — design decisions and their rationale
- `.claude/plans/lxd-04-implementation-plan.md` — the full phased plan this
  crate implements Phase 1 of

## Step 0 result: **PASS, anomalously — read the caveat before trusting this broadly**

Run twice against a real daemon (`brawny-roadrunner`, a Multipass VM), on
two slightly different kernel point releases (`7.0.0-28` then `7.0.0-29`),
with the same result both times:

```
Nesting alone (security.nesting=true, unprivileged): PASS
Nesting + narrow raw.apparmor (if nesting alone failed): not needed
security.privileged=true required: no
LXD version tested: Client version: 6.9 Server version: 6.9 
Ubuntu version tested: Ubuntu 26.04 LTS
Storage backend tested: dir
Date: 2026-08-09
```

**Two things this fixed-format block doesn't capture, both load-bearing:**

1. **`security.nesting=true` may not even be necessary here.** Both runs'
   "Step A" (default unprivileged container, *no* nesting requested at all)
   *also* passed the same two probes Step B passed. `confinement-spike.sh`
   itself flags this as the "surprising, double check" outcome, not a
   plain clean pass — see its Step A block. Shipping with
   `security.nesting=true` anyway is still safe (it's strictly more
   permissive, not less, than what these two runs prove sufficient), but
   don't assume *either* Step's result transfers to a different LXD/kernel
   combination without re-running there. Both runs so far are the same
   base VM image family (Ubuntu 26.04 LTS, LXD 6.9 snap) — this has not
   been tried on Ubuntu 22.04/24.04 or an older LXD version.
2. **Landlock was not verified by either run above.** At the time of both
   runs, the real `openshell-sandbox` binary had no `--landlock-probe`
   flag, so the spike's probe honestly reported this as unverified rather
   than silently claiming a pass (an earlier version did exactly that, via
   an unconditional fallback that could not fail). **This gap is now closed
   in code**: `--landlock-probe` exists
   (`crates/openshell-sandbox/src/main.rs`, backed by
   `openshell_supervisor_process::sandbox::probe_landlock` — a real
   `landlock_create_ruleset` syscall check, not a heuristic), and
   `confinement-spike.sh`'s probe now calls it and gates pass/fail on the
   result, same as the other two primitives. Neither run above exercised
   it, though — re-run the spike to get an actual Landlock result rather
   than treating "the flag exists now" as equivalent to "it was checked."

Full run artifacts: `.claude/plans/lxd-test-results/20260808T101714Z.md`
and `.../20260809T004546Z.md`.

If a future run's result is "needs `security.privileged=true`," **stop** —
per the design doc, that's a stop-and-reconsider-the-whole-design outcome,
not a fallback to ship. A sandbox driver that needs root-equivalent host
privilege to unblock its own isolation setup has defeated its own purpose.

[`src/instance.rs`](src/instance.rs)'s `security_config()` (renamed from
`security_config_pending_spike()`) has been updated to reflect this
validated-with-caveats result — it no longer encodes only the starting
hypothesis. See that function's doc comment for the same caveats stated
above, kept next to the code they justify.

## What's actually implemented (Phase 1)

Built in parallel with the spike, per the implementation plan's explicit
sequencing ("scaffolding is orthogonal to the spike's outcome"):

- All seven `ComputeDriver` RPCs (`GetCapabilities`, `ValidateSandboxCreate`,
  `CreateSandbox`, `GetSandbox`/`ListSandboxes`, `StopSandbox`,
  `DeleteSandbox`, `WatchSandboxes`).
- A hand-rolled async LXD REST client over the Unix socket (`src/client.rs`)
  — no mature async Rust LXD client crate exists, matching the Podman
  driver's own precedent of hand-rolling rather than depending on one.
  Handles LXD's `sync`/`async`/`error` response envelope, including polling
  `/1.0/operations/<uuid>/wait` to completion for async calls — **both
  create and delete are async operations in LXD**, not just create.
- Instance-spec building (`src/instance.rs`): sandbox naming, the
  now-validated-with-caveats security posture (see "Step 0 result" above),
  and supervisor/JWT delivery via a read-only, `shift=true` disk device —
  deliberately not LXD's file-push API, which is a separate post-create RPC
  into the instance's own mutable storage layer.
- Sandbox lookup by ID via a `user.openshell.sandbox_id` config key
  (label-based, matching the Podman driver's pattern), since the gRPC
  surface for get/stop/delete only carries a sandbox ID, not enough context
  to reconstruct an instance name.
- The sandbox's *original* name (as the gateway/CLI know it) is stamped
  into a second label, `user.openshell.sandbox_name`, distinct from the
  LXD instance's own sanitized, prefixed name. Found necessary running a
  real Stage 2 lifecycle test: reporting the LXD instance name back as the
  sandbox name in `get_sandbox`/`list_sandboxes`/`watcher.rs` made the
  gateway's reconciliation store reject every single watch event as an
  attempted rename ("sandbox name cannot be changed after creation").
- `lxc.init.cmd` (via `raw.lxc`) overrides the container's PID 1 to a
  generated entrypoint script (a third disk device,
  `openshell-entrypoint`), not the supervisor binary directly. LXD
  containers have no Docker-`ENTRYPOINT` equivalent — without this
  override, the container just boots its rootfs's own default init
  (systemd) and never runs the supervisor at all. The entrypoint script
  itself statically assigns `eth0` an address in the driver's managed
  bridge subnet before exec'ing the supervisor — replacing PID 1 skips
  the container's *entire* normal boot sequence (systemd/cloud-init/
  netplan), which is what would otherwise run DHCP; LXD's "bridged" NIC
  model, unlike Docker/Podman/Kubernetes' externally-injected IPs,
  depends entirely on the guest doing that itself. **The static-IP
  derivation (`instance::static_host_octet`) is a deterministic-but-
  uncoordinated Phase 1 stopgap with no collision detection against other
  sandboxes on the same bridge** — found and fixed running a real Stage 2
  test (the supervisor started and retried its gateway connection
  repeatedly, failing every time, because `eth0` had no address at all).
  A real fix needs either an in-guest DHCP client invocation or a proper
  collision-checked IPAM; this is neither.
- Delivering the sandbox JWT as a file (above) is necessary but not
  sufficient: the supervisor's token resolution
  (`openshell_core::grpc_client::acquire_sandbox_token`) only ever checks
  three *environment variables* (`SANDBOX_TOKEN`, `SANDBOX_TOKEN_FILE`,
  `K8S_SA_TOKEN_FILE`), never a fixed path. Docker, Podman, and the VM
  driver all set `OPENSHELL_SANDBOX_TOKEN_FILE` pointing at the same mount
  path; this driver was the only one that never did. Found the hard way:
  a real Stage 2 run failed identically to a network problem ("Policy
  fetch failed, retrying" x4, then exit) even *after* a genuine network
  fix, until a raw TCP probe from inside the entrypoint script (see
  above) succeeded while the supervisor's own gRPC call kept failing
  regardless — proving the network was never the (remaining) problem.
- The entrypoint script's own diagnostic-output redirect (`build_entrypoint_script`) now uses a standalone `exec >/var/log/openshell-entrypoint.log 2>&1` rather than wrapping the network-setup commands in a `{ ...; } >file 2>&1` compound-command redirect. POSIX shell restores the original fds once a `{ ...; }` block's own redirect ends — so the previous version silently stopped redirecting output right before the script's final line, `exec {supervisor}`, which is exactly the one line whose output mattered most. Found the hard way running a real Stage 2 test: the supervisor successfully authenticated, fetched its policy, and stood up its proxy (`NET:LISTEN`) — then exited(1) for a still-unknown reason, and *every* captured log (the entrypoint log, `lxc info --show-log`, the gateway's own log) came up empty, because the exit happened after the point where output silently stopped being captured. `lxc info --show-log` itself was also misidentified as "the console log" in this driver's own diagnostics tooling — it's liblxc's internal trace log; the actual PID 1 console/tty ring buffer is `lxc console --show-log`, now captured separately by `run-stage2.sh` as defense in depth for failures that happen before the entrypoint script's redirect takes effect at all.
- Once that redirect was actually fixed, the real failure surfaced:
  `explicit process user 'sandbox' was not found in the image`. The
  supervisor's default policy (`process.run_as_user` unset,
  `openshell-supervisor-process/src/process.rs::validate_sandbox_user`)
  requires a real `sandbox` entry in `/etc/passwd`/`/etc/group` before it
  drops privileges to launch the sandboxed process — every *real* sandbox
  image bakes this in at build time (see `e2e/mcp-conformance/Dockerfile.
  client`, `scripts/agents/gator/Dockerfile`), but `run-stage2.sh`'s
  default "ubuntu" mode was pulling a stock `ubuntu:26.04` cloud image
  with neither. This is a test-image gap, not a driver bug — the fix is
  entirely in `run-stage2.sh`'s "ubuntu" mode image prep, which now
  launches a throwaway container from the copied image, bakes in a
  `sandbox` user/group, and republishes it under the same alias before
  any sandbox instance ever uses it.
- With the `sandbox` user/group fixed, a real Stage 2 run got dramatically
  further: policy fetch, OPA engine, proxy bind, Landlock ruleset build,
  `--landlock-probe`-equivalent validation, and even a successful
  `ConnectSupervisor` handshake with the gateway (`supervisor session:
  accepted`) and a `Ready` phase transition all succeeded. It then failed
  spawning the sandbox's entrypoint child with a bare `Invalid argument
  (os error 22)` and no other context anywhere in any captured log — and
  stayed exactly that bare after two rounds of adding descriptive
  `.map_err(...)` wrapping to every candidate fallible step in
  `ProcessHandle::spawn_impl`'s `pre_exec` closure (`setns(CLONE_NEWNET)`,
  then the seccomp filter installation), with the message text never
  once changing. The actual explanation: `std::process::Command::
  pre_exec`'s error-return channel can only carry a raw OS errno back to
  the *parent* process across the fork boundary (it's a single integer
  sent over a pipe) — an error constructed without one, which is what
  every `io::Error::other(...)`/wrapped-miette-message in that whole
  closure produces, has nothing transmissible, so libstd substitutes a
  generic sentinel instead of the real message. No amount of adding
  context to the *returned* error was ever going to surface anything
  different; the message was being discarded by libstd itself, not by
  our own code. The actual fix: write the diagnostic directly to fd 2
  (`process::write_pre_exec_diagnostic`, a raw, async-signal-safe
  `libc::write` — safe inside `pre_exec`) *before* returning, sidestepping
  that channel entirely. This process's stderr is already redirected to
  a real file by the sandbox's entrypoint script's own `exec >file 2>&1`
  by the time any child is spawned, so the line lands somewhere readable
  instead of vanishing into the fork/exec pipe protocol. Applied at every
  fallible step in both `process.rs`'s entrypoint-spawn closure and
  `ssh.rs`'s equivalent SSH-exec closure. `run-stage2.sh` also now
  captures the host VM's `dmesg` tail on any create/exec failure, as a
  second, independent source (kernel-level, not process-level) in case
  the failure is a BPF/seccomp verifier rejection.
  **Resolved.** The very next real Stage 2 run's entrypoint log finally
  showed the real message: `pre_exec: drop_privileges_with_identity
  failed: EPERM: Operation not permitted`. Root cause: this crate's
  `SUPERVISOR_CAPABILITIES` list (`instance.rs`) was missing `setuid`,
  `setgid`, `chown`, and `fowner`. Its doc comment claimed to mirror the
  Podman driver's capability set "exactly," but only copied the Podman
  README's "Capability Breakdown" table — the capabilities Podman's
  driver *adds on top of* Podman's own container-runtime defaults. Podman
  (like Docker) ships `SETUID`/`SETGID`/`CHOWN`/`FOWNER` in its default
  capability set already; that driver only has to avoid *dropping* them,
  never has to list them. LXD's `raw.lxc: lxc.cap.keep` has no "defaults
  plus additions" concept at all — it's an **exhaustive** allowlist, and
  everything not named is dropped, including capabilities a container
  would otherwise carry by default. The omission compiled cleanly and
  passed every unit test (including one asserting this same, incomplete
  list's own members are present in the generated config — a
  self-referential check that could never catch a *missing* entry) right
  up until a real Stage 2 run finally exercised `drop_privileges()` for
  the first time. Fixed by adding all four capabilities to the list.
  **With this fix, the very next Stage 2 run passed the full lifecycle
  end to end**: `sandbox create` (entrypoint spawned, privileges
  dropped, Landlock `restrict_self()` + seccomp enforced, workload ran),
  `sandbox exec` (a second, independent SSH-relayed command), and
  `sandbox delete`, all through the real driver/gateway/CLI stack
  against a real LXD daemon.
- **Phase 2, Step 1: the OCI-to-LXD image conversion pipeline**
  (`src/image.rs`) — real registry pull via `oci-client` (pure Rust, the
  same crate/version `openshell-driver-vm` already uses; no `skopeo`/
  `umoci` subprocess dependency), whiteout-aware layer merge, OCI image
  config (`Env`/`WorkingDir`/`User`/entrypoint) translation into LXD
  instance config, and digest-based caching via an
  `openshell-oci-<digest>` LXD image alias — checked *before* any layer
  download. Wired into `create_sandbox`: a sandbox with its own
  `spec.template.image` (the CLI's `--from`/BYOC flag) resolves through
  this pipeline; one without falls back to the driver's pinned
  `default_image`, which Phase 2 also made optional at driver-startup
  time (a driver can now run entirely off sandbox-supplied images).
  Added `client.rs` image-management methods (`create_image_from_
  unified_tarball`, `create_image_alias`, `get_image_by_alias`) since
  none existed before — Phase 1 only ever consumed a manually
  pre-converted image, never uploaded one itself.
  **First real run against ghcr.io and a real LXD daemon failed with
  `No space left on device` while packaging the converted image.** Root
  cause was a genuine design flaw, not the VM's disk alone: the pipeline
  downloaded and extracted *every* layer to disk before merging *any*
  of them (bounded 4-way concurrency, but `try_collect` still waited for
  all of them), then built a *separate*, fully-merged rootfs on top of
  that — for a real, 13-layer sandbox image, peak staging usage during
  conversion was 3-4x the final image size, before LXD's own storage of
  the uploaded result even enters into it. A 2-layer `ubuntu:26.04`
  conversion (this module's first, smaller test) never showed the
  problem; the real sandbox image did immediately. Fixed by processing
  layers strictly sequentially — download, extract, merge, delete its
  extracted copy, *then* move to the next layer, preserving the same
  manifest-order semantics (later layers can whiteout/override earlier
  ones) without ever holding more than one layer's extracted contents on
  disk at once — and by freeing the merged rootfs directory immediately
  after packaging it into the upload tarball, rather than holding both
  through a potentially slow upload.
- **No coordination between concurrent conversions of the same image
  digest.** tonic gives every `CreateSandbox` call its own task with no
  serialization between them. Running the real sandbox image test
  (13 layers, ~2.7GB) exposed this directly: a second sandbox request
  for the *same* image landed while the first's conversion was still in
  flight, logged its own "cache miss," and started a second, fully
  redundant pull/merge/package/upload of the identical image — actively
  making an already-slow, already-disk-constrained operation worse by
  doubling resource pressure at the exact moment it could least afford
  to. Fixed with a process-wide, per-digest `tokio::sync::Mutex` registry
  (`conversion_lock_for_digest`): the actual conversion work is
  serialized per digest, with a double-checked cache read (once before
  acquiring the lock, once after) so a caller that loses the race to
  *start* converting still gets a cache hit once the winner finishes,
  rather than redoing the work anyway.
- **The real sandbox image's conversion is genuinely slow, not hung —
  confirmed by widening `run-stage2-oci.sh`'s CLI-level timeouts**, which
  had been sized for an early, small (`ubuntu:26.04`, 2 layers) test and
  were far shorter than this driver's own internal upload timeout
  budget (`IMAGE_UPLOAD_TIMEOUT` + `IMAGE_IMPORT_WAIT_TIMEOUT` ≈ 900s).
  The mismatch meant the test script always gave up on the CLI long
  before the driver itself would have, so every run looked identically
  like a failure whether the driver was slowly succeeding or genuinely
  stuck — indistinguishable without widening the outer bound first.
  With the widened timeouts (and the per-digest lock above removing
  contention), a genuine cache-miss conversion of the real 13-layer,
  ~2.7GB sandbox image completed end to end (pull → merge → package →
  upload → cache) in **~6m9s**; a genuine cache-hit resolution of the
  same digest afterward took **~2.8s** — solid, measured evidence the
  digest-cache design's whole point (skip the expensive path on repeat)
  actually holds. The whiteout-aware merge step alone (per-file
  `fs::copy`+`chmod`, not hardlinks/reflinks) took over four minutes for this
  image's ~13 layers. Whether that specific cost is worth optimizing
  (hardlinks for unmodified files, parallelizing the merge) is an open
  question a real, successfully-completed run needs to answer first —
  not something to guess at or pre-optimize from first principles before
  that evidence exists.
- **`run-stage2-oci.sh` invented its own, never-validated LXD network
  name/subnet (`openshell-oci` / `10.89.77.1/24`) instead of reusing the
  exact configuration `run-stage2.sh` already proved end to end
  (`openshell` / `10.88.77.1/24`)** — a test-script bug, not a driver
  bug. `ensure_network`'s startup check (GET, create on 404) reported
  the new network as ready, but every subsequent `sandbox create`
  failed with `LXD API error (500): Failed starting device "eth0":
  Parent device "openshell-oci" does not exist` — the LXD network
  object existed in the daemon's database while the underlying kernel
  bridge device apparently did not, an LXD-level inconsistency this
  driver has no way to detect from the client side (a successful
  `ensure_network` call is not proof the bridge is actually usable).
  `instance.rs`'s device config itself
  (`"nictype": "bridged", "parent": config.network_name`) was correct
  throughout — it just pointed at a network nothing had actually proven
  worked. Fixed by having `run-stage2-oci.sh` reuse `run-stage2.sh`'s
  proven name/subnet instead of inventing a new one, at the cost of not
  supporting a concurrent run of both scripts (an accepted tradeoff —
  this repo's spike scripts are run sequentially by one person, never
  concurrently).
- **The entrypoint script's own diagnostic-log redirect
  (`build_entrypoint_script`) could kill the container outright on a real
  image.** It unconditionally ran `exec >/var/log/openshell-entrypoint.log
  2>&1` before doing anything else. The real sandbox image's `/var/log`
  is not writable by container-root without `CAP_DAC_OVERRIDE` — which
  this driver's `SUPERVISOR_CAPABILITIES` deliberately omits, mirroring
  the Podman driver's own intentional default-capability reduction (see
  that constant's doc comment), not an oversight to fix by re-adding the
  capability. A failed `exec > file` redirect is fatal to a POSIX shell,
  so PID 1 died immediately on container start, well before the network
  bring-up or the supervisor itself ever ran — confirmed directly in
  `lxc console --show-log`'s captured output (`cannot create
  /var/log/openshell-entrypoint.log: Permission denied`), which is also
  why the gateway's `CreateSandbox` RPC itself still reported success
  (the LXD instance really was created) while the CLI's own
  wait-for-ready step timed out afterward — two independent, correctly
  distinct failure points, not a contradiction. Fixed by probing
  writability first inside an `if` (falling back to `/tmp`, whose
  sticky-bit world-writable default every mainstream Linux base image
  already guarantees) instead of letting the real `exec` redirect be the
  first thing to fail.
- **That fix's first version was itself broken by a subtle POSIX shell
  rule** — it probed writability with `: >"$ENTRYPOINT_LOG" 2>/dev/null`.
  `:` is a POSIX *special* builtin, and POSIX mandates that a
  redirection error on a special builtin exits the shell immediately,
  `if` guard or not — a rule that does not apply to ordinary commands.
  Confirmed directly against `dash` (Ubuntu's real `/bin/sh`, and thus
  what actually runs this script as PID 1): the fallback branch was
  never reached, dash exited right there with the identical
  externally-visible symptom (PID 1 dying before the supervisor ever
  started) the fallback was written to fix in the first place — same
  crash, one line later, same real Stage 2 VM run needed to surface it.
  `sh -n` (syntax-only, never executes anything) can't catch a runtime
  rule like this, and the fallback's own *string-content* unit test
  couldn't either (it checks what the generated script says, not how a
  real shell executes it). Fixed by using `true` (an ordinary builtin)
  instead of `:`, and by reordering the redirects (`2>/dev/null` before
  the write attempt, not after) so dash's own diagnostic for the failing
  redirect — which it emits independently of the command's own stderr —
  is actually suppressed.   Added a genuine runtime regression test that
  runs the generated script under `dash` specifically (not whatever `sh`
  resolves to on the test host — confirmed macOS's default `/bin/sh`
  does *not* reproduce this special-builtin behavior, so a test using
  generic `sh` would have passed either way) against a real unwritable
  `/var/log` (true for an unprivileged test-runner user on both Linux
  and macOS, no mocking needed) and asserts the fallback log actually
  gets written.
- **The image conversion pipeline silently drops every layer's declared
  file ownership when the driver process itself runs as a non-root
  user** (the common case — nothing about this pipeline requires root).
  Confirmed directly against the real `tar` crate version this crate
  depends on: a directory declared `uid=0`/`gid=0` in a tar header
  extracts, as a non-root process, owned by the *extracting* process's
  own UID instead — not a `tar`-crate bug to work around, just the basic
  Unix rule that `chown` to a UID other than the caller's own requires
  root or `CAP_CHOWN`. This had already caused the `/var/log` failure
  above, just invisibly — the actual fix there (fall back to `/tmp`)
  sidestepped needing to know the *why*. It stopped being invisible when
  a real sandbox image's supervisor called `mkdir /run/netns` (root-owned
  `/run`, mode 0755, so ownership actually matters — unlike `/tmp`'s
  world-writable bits) and got `EACCES`, indistinguishable at the call
  site from a genuine missing-capability error. Fixed by never relying
  on the driver's own staging-disk ownership being correct at all:
  extraction now reads each tar entry's declared `uid`/`gid` directly
  from its header (not just unpacking and trusting the result) and
  threads that through merge (same later-layer-wins override semantics
  already applied to file *content*) to packaging, where the final
  tarball's headers are set explicitly from the tracked values —
  independent of whatever the non-root staging process actually left on
  disk. LXD's own image-unpack step, which does run with real
  idmap/root privileges, is what makes the ownership real once and for
  all. Added an end-to-end regression test that runs the real
  extract → merge → package pipeline against a synthetic layer
  declaring `uid=0`, asserts the *on-disk staging* ownership is wrong
  (confirming the test isn't passing for the wrong reason), and then
  asserts the *final packaged tarball's header* is still correct.
  **Not yet checked: whether `openshell-driver-vm`'s own OCI conversion
  pipeline (`crates/openshell-driver-vm/src/driver.rs`, which this
  module's own doc comment already notes uses the "same algorithm" for
  layer merging) shares this exact gap** — worth a maintainer's look
  independently of this crate, since VM driver sandboxes built from an
  image with root-owned infrastructure paths could be affected the same
  way if that driver process also commonly runs non-root.
  **With the ownership fix in place, `run-stage2-oci.sh` passed in
  full** (2026-08-09): the real, unmodified `ghcr.io/nvidia/openshell-
  community/sandboxes/base:latest` image — no manual prep, no baked-in
  users, nothing this crate's own test script hand-adjusted — converts,
  boots, runs a real `sandbox exec`, and deletes cleanly, both on a
  cache-miss conversion (224s) and a cache-hit resolution of the
  identical digest right after (9s — roughly 25x faster, direct,
  measured evidence the whole point of the digest-cache design holds).
- **Phase 2, Steps 5-8: mTLS, resource limits, driver-config mounts,
  rollback/reconciliation hardening.** Unit-test-verified only — unlike
  every item above, none of this has been exercised against a real LXD
  daemon yet (no `run-stage2-oci.sh`/`run-managed-driver.sh` re-run with
  these features enabled). Treat this bullet's claims as "the code path
  exists and its unit tests pass," not "proven against a real daemon,"
  until a real run says otherwise — every *other* claim in this file
  earned that distinction from a real Stage 2 failure it caused and then
  fixed; these four have not yet had that chance to be wrong.
  - **mTLS** (`config.rs`'s `guest_tls_ca`/`guest_tls_cert`/
    `guest_tls_key`, validated "all three or none" by `validate_tls_config`):
    delivered via the same read-only `shift=true` disk-device mechanism
    as the supervisor binary and JWT (three more devices in
    `build_instance_spec`), to the same fixed guest paths and env vars
    (`OPENSHELL_TLS_CA`/`_CERT`/`_KEY`) Docker/Podman/VM already use, so
    the supervisor's own TLS-loading code needs no driver-specific
    branch. The gateway side (`compute::lxd::compute_driver_guest_tls_paths`)
    mirrors the VM driver's own gate exactly: only required, and only
    validated for completeness, when `grpc_endpoint` is `https://`.
  - **Resource limits** (`instance::lxd_resource_limits`): `template.
    resources.cpu_limit`/`memory_limit` map onto `limits.cpu.allowance`
    (a `"<quota>ms/100ms"` cgroup-CFS-bandwidth string, *not* bare
    `limits.cpu` — see `LxdResourceLimits::cpu_allowance`'s doc comment
    for why a whole-core-count/CPU-set pinning primitive would have been
    the wrong mapping) and `limits.memory` (an exact byte count with
    LXD's `B` suffix). `cpu_request`/`memory_request` are rejected, not
    silently ignored — LXD, like Docker, has no reservation primitive
    distinct from its limit. `sandbox_pids_limit` (driver config, not a
    sandbox request) maps onto `limits.processes`, `0` inheriting LXD's
    own unlimited default rather than meaning "zero processes allowed" —
    the same convention Docker/Podman already use.
  - **Driver-config mounts** (`instance::LxdDriverMountConfig`): `bind`
    only, deliberately — see that enum's own doc comment for why
    `volume`/`tmpfs`/`image` (which Docker/Podman's own mount-config
    enums support) were scoped out rather than half-built. Reuses
    `openshell_core::driver_mounts` wholesale for source/target
    validation (absolute host paths, no reserved-path collisions, no
    duplicate targets) rather than reimplementing any of it, gated
    behind the same `enable_bind_mounts` operator opt-in Docker/Podman
    already require.
  - **Rollback/reconciliation**: every async LXD call this driver makes
    already polled its own operation to completion before Steps 5-8
    (`LxdClient::send_and_resolve`) — nothing to add there. The actual
    gap: a failed `create_instance` (or a `build_instance_spec` failure,
    e.g. an invalid mount) left the entrypoint script and JWT files
    already written to the host filesystem orphaned, since nothing had
    a reason to clean them up before this driver ever created an
    instance to roll back. Fixed by `cleanup_sandbox_delivery_files`,
    called on every `create_sandbox` failure path from that point
    onward — deliberately *not* touching `image::ensure_lxd_image`'s own
    "leave the staging directory for diagnosis on failure" directory,
    which shares the same per-sandbox parent. Restart-time reconciliation
    needed no new code at all: `get_sandbox`/`list_sandboxes` already
    always re-derive a sandbox's identity and status from LXD's *current*
    instance state (filtered by `user.openshell.sandbox_id`), never from
    any in-memory operation state this process could lose on restart —
    there was never any such state to begin with.
- **Phase 2, Step 9: expanded unit test coverage for lifecycle, network
  isolation, and interrupted delete.** `get_sandbox`/`list_sandboxes`/
  `stop_sandbox`/`delete_sandbox` had no unit tests at all before this —
  every one of them now has both a "no matching instance" and a
  "matching instance" case against the stub server, including
  `list_sandboxes` correctly excluding an unmanaged (unlabeled) LXD
  instance from the same daemon. `delete_sandbox_propagates_a_genuine_
  delete_failure_rather_than_swallowing_it` is the "interrupted delete"
  case the implementation plan calls for: a delete that fails for a real
  reason (not "already gone") must surface as an error, not be
  misreported as success or as nothing-to-delete. Network isolation gets
  one direct assertion (`build_instance_spec_confines_the_nic_to_the_
  configured_managed_bridge`): the `eth0` NIC device always tracks
  `config.network_name`, built with two different network names to
  prove it isn't a hard-coded literal that happens to match every other
  test's default. Resource-limit and mount-translation coverage were
  already built out in Steps 5-8's own tests (above) and are not
  duplicated here. No new coverage was added at the `e2e/rust/` level
  (the full gateway+CLI-driven suite Docker/Podman/VM use) — no other
  driver crate has a `tests/e2e.rs` either (confirmed by inspection, not
  assumed), so "matching the Podman suite" the implementation plan asks
  for means matching Podman's own inline unit-test breadth, which this
  now does. Building a real `e2e-lxd` Cargo feature and CI-runnable
  harness would need a Linux host with a real LXD daemon to ever
  actually exercise it — infeasible from this development machine, and
  a bigger, separate undertaking from a documentation-language
  correction.
- An `/1.0/events` websocket watcher (`src/watcher.rs`) that subscribes
  before listing (to avoid a race that drops events) and — **important
  correction from an earlier draft of the implementation plan** — does
  *not* reconnect internally on disconnect. It terminates the stream with a
  final error item instead. Reconnection is the gateway's
  `ComputeRuntime::watch_loop`'s job, with backoff, for every driver; a
  driver-local reconnect would race with that retry and produce duplicate
  initial-sync events. This is the same contract the Podman driver
  documents (`crates/openshell-driver-podman/src/watcher.rs`) — verified by
  reading its actual implementation while building this, not assumed.
- Unit tests (45, all passing) including stub-server integration tests
  (`src/test_utils.rs`, mirroring Podman's pattern) that exercise the real
  HTTP/envelope-resolution code path without a real LXD daemon.
- One additional real-daemon integration test,
  `client::tests::real_daemon_create_get_list_delete_lifecycle`
  (`src/client.rs`), `#[ignore]`d by default since it requires a real local
  LXD daemon (Linux only) and creates/deletes a real container. Exercises
  `LxdClient::create_instance`/`get_instance`/`list_instances`/
  `delete_instance` directly against a real daemon using a plain stock
  image — the first test in this crate to do that, and (as of 2026-08-09)
  **passing** against a real daemon. Resolves its `OPENSHELL_LXD_TEST_IMAGE`
  default (`ubuntu:22.04`, a `lxc`-CLI `remote:alias` shorthand with no
  meaning to the raw REST API) to a local alias via `lxc image copy` first
  — the REST API has no concept of that shorthand, unlike `lxc launch`. Run
  it explicitly with `cargo test -p openshell-driver-lxd -- --ignored`. See
  `.claude/plans/lxd-05-test-plan.md` (Stage 1).

## What the Stage 2 pass does NOT prove

Read this before treating the `pass` outcome above as more than "the
happy path works end to end" — see `run-stage2.sh`'s own header comment
for the authoritative list, summarized here:

- **No TLS/mTLS.** The run uses `--disable-tls` throughout. A delivery
  mechanism now exists (Phase 2, Step 5 — see "What's actually
  implemented" below), but `run-stage2.sh`/`run-stage2-oci.sh` have not
  been re-run with it enabled, so it remains unverified against a real
  daemon.
- **Bypasses `_gateway.lxd`/`GetGatewayListenerRequirements` entirely**
  by passing the driver's own bridge gateway IP directly as
  `grpc_endpoint`. That code path in `driver.rs` remains unexercised.
- **The static-IP network bring-up is a Phase 1 stopgap** —
  deterministic but uncoordinated, with no collision detection against
  other sandboxes on the same bridge (see `instance::static_host_octet`'s
  doc comment).
- **The "ubuntu" test image required manual prep** (baking in a
  `sandbox` user/group) that a real production sandbox image already
  provides; this doesn't validate OCI image conversion end to end.
- **Single sandbox, single run, `debug` build.** No concurrency,
  restart/reconnect, resource-limit, or long-running-workload coverage.

## What's explicitly NOT implemented (by design, Phase 2 or later)

- General OCI image resolution. Set `default_image` to one LXD image alias
  or fingerprint you convert by hand once
  (`umoci unpack` + `lxc image import`); every sandbox uses that same
  image. The OCI-to-LXD conversion pipeline is Phase 2's largest single
  workstream — see the design doc.
- Auto-detection. This driver is never added to the gateway's
  `detect_driver()` — it's opt-in only, like the VM driver, because LXD has
  no rootless mode and `lxd`-group membership is host-root-equivalent. See
  the design doc's risk section.
- Multi-tenancy via LXD projects, GPU passthrough, clustering, Incus support
  — all explicit non-goals for now.

## Running it

```shell
# 1. Convert one sandbox image by hand (outside this driver):
#    umoci unpack --rootless <oci-image> <bundle-dir>
#    <package bundle-dir into LXD's metadata.yaml + squashfs shape>
#    lxc image import <path> --alias openshell-sandbox-base

# 2. Build and run the driver, bound to a Unix socket -- NOT --bind-address.
#    connect_remote_compute_driver (crates/openshell-server/src/compute/mod.rs)
#    only ever dials a UDS via UnixStream::connect; a bare TCP
#    --bind-address is real but unreachable by any gateway. (An earlier
#    version of this example used --bind-address here, which cannot
#    actually work with a real gateway -- see --bind-uds's doc comment on
#    Args in src/main.rs.)
cargo build -p openshell-driver-lxd
./target/debug/openshell-driver-lxd \
  --lxd-image openshell-sandbox-base \
  --supervisor-bin /path/to/openshell-sandbox \
  --bind-uds /run/openshell/lxd-driver.sock

# 3. Point a gateway at it (no gateway core changes required):
openshell-gateway --drivers lxd --compute-driver-socket /run/openshell/lxd-driver.sock
# or, in gateway.toml:
#   [openshell.gateway]
#   compute_drivers = ["lxd"]
#   [openshell.drivers.lxd]
#   socket_path = "..."
```

## Development notes

- Built and unit-tested on macOS (no LXD available) — the
  `#[cfg(target_os = "linux")]` branch in `driver.rs`'s
  `gateway_listener_requirements` (which reads the LXD bridge's gateway IP
  back from network state) is dead code on this platform by construction;
  `cargo clippy` reports a handful of matching dead-code warnings on
  non-Linux targets. LXD itself only runs on Linux, so this is expected and
  harmless — those warnings disappear on a real Linux build.
- `cargo test -p openshell-driver-lxd` requires binding Unix sockets under
  `/tmp` for the stub-server tests; some sandboxed shells (including the
  one used to build this crate) restrict that and need elevated
  permissions to run those specific tests. `cargo check`/`clippy`/the other
  29 tests are unaffected.
