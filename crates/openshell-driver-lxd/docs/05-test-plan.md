# Test Plan: Running the LXD Driver Phase 1 PoC Against a Real Daemon

> **Status note.** This plan was originally written before real LXD access
> was available — at the time, nothing in it had actually been run.
> Stages 0, 1, and, eventually, the Stage 2
> end-to-end smoke test this plan calls a stretch goal "out of reach for a
> first pass" have all since been executed for real, on the exact VM
> (`brawny-roadrunner`) this plan names, and all passed. See the crate
> README's "Step 0 result" and "What's actually implemented" sections, and
> `04-implementation-plan.md`'s dated status updates, for the actual
> results. What follows remains accurate as the reasoning and procedure
> that got there (the VM disk-budget analysis, the cross-compilation
> approach, the pass/fail decision tree) — read it as the plan that
> worked, not as a still-open gap.

Local equivalent of the empirical-validation step `04-implementation-plan.md`
calls for before any of Phase 1's Definition of Done items can be checked off.
`crates/openshell-driver-lxd/` compiles and passes its unit tests
(`04-implementation-plan.md`'s Deliverables, first item), but every one of
those tests runs against `src/test_utils.rs`'s in-process stub, never a real
LXD daemon. This document is the concrete, staged procedure for closing that
gap on a real Ubuntu host, plus an honest accounting of what does and does not
fit in a first pass.

Companion script: `crates/openshell-driver-lxd/hack/run-vm-tests.sh` — a
single, defensive, ready-to-run implementation of Stages 0 and 1 below,
written to be executed by a human from a terminal that actually has network
access to the target host (see "Getting the repository onto the VM").

## Target host used to write this plan

A Multipass VM: name `brawny-roadrunner`, IP `192.168.252.3`, Ubuntu 26.04
LTS, arm64, 4 CPUs, ~3.8 GB RAM, **7.7 GB disk**. The disk figure matters —
see "Resource constraints" below before starting Stage 1.

## Resource constraints specific to this VM

- **Disk (7.7 GB total) is the binding constraint, not CPU or RAM.** A rough
  budget: Ubuntu base OS (~2 GB already used before anything below runs) +
  LXD snap and its core snaps (~300-500 MB) + a Rust toolchain via `rustup`
  (~1.2-1.5 GB) + a C/C++ toolchain for `protobuf-src`'s from-source protobuf
  build (see "Stage 1 prerequisites") + `target/` build artifacts for two
  crates (debug profile, realistically 1-3 GB given the `tokio`/`tonic`/
  `hyper`/`rustls` dependency tree) + one pulled `ubuntu:22.04` LXD container
  image (a few hundred MB to ~1 GB depending on compression). These numbers
  can plausibly exceed 7.7 GB before Stage 1 even starts its real-daemon
  test. **Resize the VM's disk before Stage 1** (Stage 0 alone — one
  container, no Rust toolchain needed if using the cross-compiled binary
  option below — has a much smaller footprint and may fit as-is):

  ```shell
  # run from an interactive terminal with full network access
  multipass stop brawny-roadrunner
  multipass set local.brawny-roadrunner.disk=20G
  multipass start brawny-roadrunner
  # if the disk was already near-full before resizing, the partition won't
  # auto-expand -- check, then resize manually if needed:
  multipass exec brawny-roadrunner -- df -h /
  multipass exec brawny-roadrunner -- sudo parted /dev/sda resizepart 1 100%
  multipass exec brawny-roadrunner -- sudo resize2fs /dev/sda1
  ```

- CPU (4) and RAM (~3.8 GB) are adequate for building these two crates
  specifically (they pull in far less than the full workspace — no
  `openshell-prover`/Z3, no TUI/ratatui, no Kubernetes client) but a
  release-profile build of the full workspace would be a poor fit for this
  VM's specs. `run-vm-tests.sh` builds in debug profile for exactly this
  reason (also matching the crate README's own `cargo build -p
  openshell-driver-lxd` example, which is debug by default).

## Getting the repository onto the VM

Try these in order:

**(a) `multipass mount` — try this first, from the user's own terminal:**

```shell
multipass mount /Users/yuewu/Desktop/workspace/k8s/openshell brawny-roadrunner:/mnt/openshell
```

This is the preferred option because it makes the results file mechanism
below actually work end-to-end: `run-vm-tests.sh` writes its output under
`crates/openshell-driver-lxd/hack/results/` *inside the repo it
finds itself in* (this section originally said `architecture/plans/
lxd-test-results/`, which never matched what the script actually did —
see `hack/run-vm-tests.sh`'s own `RESULTS_DIR` for the real
path). If that repo is a mount, the result lands back on the host's real
filesystem as an ordinary file — no network operation needed to read it
afterward.

**(b) `git clone` fresh inside the VM — fallback, with a caveat.** This only
produces a *working* checkout of this branch if `lxd-driver-support` has
already been pushed somewhere the VM can reach — confirm push status
first. If the branch is still local-only on the Mac, cloning `main` inside
the VM gets a checkout **without** the LXD
driver crate at all, which would silently test nothing relevant. Only use
this path after either pushing the branch to a remote the VM can reach, or
manually confirming what ref you're actually cloning:

```shell
multipass exec brawny-roadrunner -- git clone --branch lxd-driver-support <your-remote-url> ~/openshell
```

**(c) `multipass transfer` a tarball — fallback, works regardless of git
remote state** (captures uncommitted changes too, which matters if the
branch's work hasn't been pushed or fully committed at the time this
procedure runs):

```shell
cd /Users/yuewu/Desktop/workspace/k8s/openshell
tar --exclude=target --exclude=.git -czf /tmp/openshell.tar.gz .
multipass transfer /tmp/openshell.tar.gz brawny-roadrunner:/home/ubuntu/openshell.tar.gz
multipass exec brawny-roadrunner -- bash -c 'mkdir -p ~/openshell && tar -xzf openshell.tar.gz -C ~/openshell'
```

**Recommendation: (a) mount, fallback to (c) tarball transfer if mount is
also blocked from the user's own terminal.** Don't reach for (b) unless the
branch is confirmed pushed somewhere reachable.

`run-vm-tests.sh` detects its own repository root by walking up from its own
file location looking for the workspace `Cargo.toml` (`[workspace]` with
`members = ["crates/*"]`, matching the root `Cargo.toml`). It does **not**
attempt a `git clone` fallback itself if that fails — given the caveat above,
guessing at a ref to clone risks silently testing the wrong code. If it can't
find a workspace root, it prints instructions and exits rather than guessing.

## Stage 0 — Confinement spike

**This is the gating step.** Nothing else in this plan, or in
`04-implementation-plan.md`'s Phase 1, is meaningful until this produces
a real result. See `crates/openshell-driver-lxd/hack/confinement-spike.sh`
and the implementation plan's Phase 1 "Implementation Steps", Step 0
("Confinement spike") for the full rationale; this section is the
executable procedure.

### Step 0.1 — Build a Linux/arm64 `openshell-sandbox` binary

Two ways to get this binary; the tradeoff is where the compute happens, not
correctness — either produces the same binary shape.

**Option A (recommended): cross-compile from the Mac host.** This repository
already has a working, documented cross-compilation path for exactly this
binary, built for exactly this purpose (the VM driver embeds a
cross-compiled `openshell-sandbox` the same way):
`tasks/scripts/vm/build-supervisor-bundle.sh`'s `run_supervisor_build()`
runs (with a plain `cargo build` fallback when `cargo-zigbuild` isn't on
`PATH`):

```shell
cargo zigbuild --release -p openshell-sandbox --target aarch64-unknown-linux-gnu --manifest-path Cargo.toml
```

using `cargo-zigbuild` + `zig` (both already present in this environment via
`mise` — confirmed present at `~/.local/share/mise/installs/github-rust-cross-cargo-zigbuild/0.22.3/`
and `~/.local/share/mise/installs/zig/0.14.1/`, both listed as workspace
tools in `mise.toml`). The target `aarch64-unknown-linux-gnu` matches
`brawny-roadrunner`'s arm64 CPU running Linux instead of macOS —
`cargo-zigbuild` handles the glibc sysroot so this doesn't need a full
cross-toolchain install. The existing `mise run vm:supervisor` task (in
`tasks/vm.toml`) already wraps this build with the right target
auto-selected from `uname -m`, so the simplest invocation from the repo root
is:

```shell
mise run vm:supervisor
# binary lands at target/aarch64-unknown-linux-gnu/release/openshell-sandbox
```

This keeps the VM's scarce disk and RAM entirely out of the picture for this
step — the build runs on the Mac, and only the resulting single binary
(copied via whichever mechanism from "Getting the repository onto the VM" is
in use) needs to reach the VM. See below for why `run-vm-tests.sh` itself
uses Option B instead.

**Option B: build natively on the VM.** Simpler mental model (no
cross-compilation), but spends the VM's disk on a full Rust toolchain +
target directory for a binary that Option A produces without touching the VM
at all. Only worth it if Option A's tools aren't available in whatever
environment actually runs this step:

```shell
# on the VM
curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
cd ~/openshell   # or wherever the repo landed
cargo build --release -p openshell-sandbox
# binary lands at target/release/openshell-sandbox
```

`run-vm-tests.sh` implements Option B (it runs entirely on the VM, by
design — see the script's own header), but this plan recommends Option A
when a human is driving interactively, specifically to protect the 7.7 GB
disk budget.

### Step 0.2 — Run the confinement spike

```shell
# on the VM, as a user in the lxd group (or root)
sudo snap install lxd
sudo lxd init --minimal   # if not already initialized
chmod +x hack/confinement-spike.sh
./hack/confinement-spike.sh /path/to/openshell-sandbox
```

Exact behavior: launches one throwaway container with LXD defaults (Step
A, expected to fail — nesting not yet requested), then one with
`security.nesting=true` (Step B), and — only if Step B fails — one more
with `security.nesting=true` plus a narrow `raw.apparmor` line for `nsfs`
mounts (Step C). Each launch attaches the supervisor binary via a
read-only `shift=true` disk device and runs three probes inside the
container (`ip netns add`, a Landlock probe attempt, `unshare --net`).
Containers are deleted after each step; a `trap cleanup EXIT` also removes
any container matching `openshell-spike-$$` if the script is interrupted.

### What "pass" looks like

The script prints a `RESULT SUMMARY` block with two lines:

```
Nesting alone (security.nesting=true, unprivileged):        PASS | FAIL | pass (nesting wasn't even needed...)
Nesting + narrow raw.apparmor (only run if nesting failed):  PASS | FAIL | untested
```

| Outcome | Meaning | Next step |
|---|---|---|
| `PASS` (nesting alone, exact match) | Confirms the hypothesis already encoded in `instance.rs`'s `security_config()` (renamed from `security_config_pending_spike()` once this hypothesis was validated) | Proceed to Stage 1. The hypothesis needs no code change — see "What happens to `instance.rs`" below. |
| `pass (nesting wasn't even needed...)` (Step A alone passed) | Genuinely surprising — the implementation plan explicitly calls this out as needing a double-check (`confinement-spike.sh`'s "Step A" block), since it would mean the driver doesn't need `security.nesting=true` at all | Proceed to Stage 1, but do **not** treat this as confirming the hypothesis as-is. Investigate why before trusting it (possible causes: a already-privileged shell, a misconfigured probe, or a genuinely more permissive LXD default than expected). |
| `FAIL` then apparmor `PASS` | Nesting alone insufficient; the narrow `raw.apparmor` addition closed the gap | Proceed to Stage 1, but `instance.rs` needs a real edit (add the `raw.apparmor` line) before it's correct — flag for review, don't self-edit from a VM script (see below). |
| `FAIL` then apparmor `FAIL` | **Stop-and-reconsider outcome** (the implementation plan's Phase 1 Step 0, and the design doc's "Phase 1 shape: unmanaged extension driver" section, both call this a stop-and-reconsider-scope outcome, not a shippable fallback) | **Do not proceed to Stage 1.** Do not try `security.privileged=true` "just to see." This is a design-level problem, not a config tweak — escalate per the design doc rather than patching around it. |

Also check for AppArmor denials the three probes don't directly surface —
the design doc's "messy middle" risk (the spike's own "(Highest risk,
resolve first...)" bullet under "Risks & Open Questions" cites the same
concern the confinement script prints inline in its "Step B" block):

```shell
sudo journalctl -k --since "-10 min" | grep -i apparmor
```

`run-vm-tests.sh` runs this automatically and includes any hits in the
results file, but a hit there doesn't automatically mean failure (an AppArmor
denial on an operation the three probes don't exercise might be irrelevant to
the supervisor) — read the denied operation and judge relevance by hand.

### What happens to `instance.rs`

**`run-vm-tests.sh` does not edit `crates/openshell-driver-lxd/src/instance.rs`
under any outcome, including a clean `PASS`.** It updates
`README.md`'s "Step 0 result" block (a data section that exists specifically
to be filled in) and, when the result is an exact `PASS` with no anomaly, it
writes a note in the results file recommending that `security_config_pending_spike()`
be treated as confirmed and its pending-spike doc comment removed. Applying
that specific source edit is left to a human or a reviewing agent, not the
script itself: an empirically-validated security posture is exactly the
kind of change that deserves a second pair of eyes before it lands. A
script running unattended on a VM shouldn't be the one making that
judgment call silently.

**Resolved (2026-08-09):** the spike did pass, and this recommendation was
applied — the function is now named `security_config()`, and its doc
comment reflects the validated-with-caveats result instead of a pending
hypothesis. This section is left describing the *process* that produced
that outcome, not the current function name, since that's the part future
readers actually need if they re-run the spike on a new environment.

## Stage 1 — Real-daemon crate validation

Only runs (in `run-vm-tests.sh`; only makes sense at all) after Stage 0
reports a `PASS` variant. Two things happen here, both native builds on the
VM (deliberately, unlike Stage 0's binary — see "Why native here" below):

### Stage 1 prerequisites (Ubuntu, one-time)

```shell
sudo apt-get update
sudo apt-get install -y build-essential cmake pkg-config libssl-dev \
  clang libclang-dev git curl ca-certificates
curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
```

This list mirrors the CI image's baseline (`deploy/docker/Dockerfile.ci`'s
system-dependencies layer) minus the pieces this crate doesn't need
(`libz3-dev` — that's `openshell-prover`, not in this dependency graph;
Docker/`gh`/Node/etc. — CI infrastructure, not a build requirement).
`build-essential`/`cmake`/`clang`/`libclang-dev` matter because `aws-lc-sys`
(pulled in transitively via `rustls`'s AWS-LC crypto backend, confirmed in
this crate's own dependency tree) needs `cmake` and a C compiler to build
the AWS-LC library from source on first build. **Not** — as an earlier
version of this paragraph claimed — because `openshell-core`'s build
script compiles protobuf from source: it now vendors a prebuilt `protoc`
binary via `protoc-bin-vendored` instead of the old `protobuf-src`
dependency, specifically because `protobuf-src` needs autotools/`sh` and
breaks MSVC builds (see `crates/openshell-core/build.rs`'s own comment).
Either way, this apt/cmake cost is normal for any fresh Linux box building
this workspace, not something the LXD driver introduces, but it does mean
the *first* build of either crate below is slower than subsequent ones.
A `rust-toolchain.toml` at the repo root pins Rust 1.95.0; `rustup` (even
the `minimal` profile) auto-installs that exact version the first time
`cargo` runs inside the repo, so no manual `rustup toolchain install` step
is needed.

### Step 1.1 — Build and run the full unit test suite natively

```shell
cd <repo root on the VM>
cargo test -p openshell-driver-lxd
```

**What this proves that hasn't been proven before:** the crate's own
README (its "Development notes" section) documents that the stub-server
tests in `client.rs`, which bind a Unix socket under `/tmp`, were blocked
in the sandboxed shell this crate was originally built in, needing
elevated permissions to run at all. On a real, unsandboxed Linux VM,
there's no reason for that restriction to exist — this is the first
opportunity to confirm every one of them (including the real-daemon test
below, which is `#[ignore]`d — see Step 1.2) actually passes as a normal,
unprivileged native process, not just under manually-granted permissions
in a dev sandbox.

**Pass:** `cargo test`'s summary line reports zero failures — e.g.
`test result: ok. 115 passed; 0 failed; 1 ignored` as of this writing
(115 passed, 1 ignored; see `04-implementation-plan.md`'s most recent
test-count update for today's actual total, since this grows as the crate
gains coverage — don't treat a higher passed-count than what's written
here as a failure). **Fail:** any run reporting a nonzero failed count —
investigate the specific failing test's output before assuming it's
environmental; these tests don't touch a real LXD daemon at all (they're
stub-server tests), so a failure here points at something Linux-specific
in the client/envelope code, not at LXD itself.

### Step 1.2 — Run the new real-daemon `LxdClient` integration check

This plan's authoring pass added one new test,
`client::tests::real_daemon_create_get_list_delete_lifecycle` (in
`crates/openshell-driver-lxd/src/client.rs`), specifically because
**no test anywhere in this crate, before this plan, exercised the real
`LxdClient` type against a real LXD daemon** — every existing test either
hits the in-process stub or checks pure translation logic
(`instance.rs`/`config.rs`). It's `#[ignore]`d by default (never runs under a
plain `cargo test`) since it requires a real daemon and creates/deletes a
real container:

```shell
cargo test -p openshell-driver-lxd -- --ignored real_daemon
```

What it does (see the test and its `run_lifecycle` helper for the exact
sequence): connects to the real LXD socket
(`/var/snap/lxd/common/lxd/unix.socket` by default, overridable via
`OPENSHELL_LXD_TEST_SOCKET`), then runs `create_instance` → `get_instance` →
`list_instances` → `delete_instance` → `get_instance` (confirming the
instance is actually gone) against a plain stock `ubuntu:22.04` image
(overridable via `OPENSHELL_LXD_TEST_IMAGE`) — **deliberately not** the
pinned OpenShell sandbox image or `instance::build_instance_spec`, since this
test validates the REST client's request/envelope handling against real LXD,
not the full sandbox lifecycle (which additionally needs the manually
pre-converted image Stage 2 discusses). It never touches
`security.nesting`/capabilities either — a plain, unstarted container needs
neither.

**Pass:** `test result: ok. 1 passed`. **Fail:** read the specific error —
likely candidates given what this test exercises: the `ubuntu:` remote not
reachable (network/DNS issue inside the VM, unrelated to this crate), the
invoking user not in the `lxd` group (permission denied connecting to the
socket), or a real bug in `LxdClient`'s envelope resolution that the stub
tests' hand-written responses didn't happen to exercise.

### Why native here (unlike Stage 0)

Stage 0 recommends cross-compiling *away* from the VM specifically to avoid
spending its disk/RAM on a Rust toolchain for one binary. Stage 1 does the
opposite deliberately: it's building and testing `openshell-driver-lxd`
*itself*, which only matters if it runs as a genuine, unsandboxed Linux
process — cross-compiling it and copying the test binary over would still
leave the "does this crate's socket/permission code actually behave
correctly on real Linux" question unanswered, since binding sockets and
talking to `/var/snap/lxd/...` are exactly the things a cross-compiled binary
run via some indirect mechanism could get subtly wrong in ways a native
build+run doesn't risk.

## Stage 2 (stretch) — End-to-end sandbox smoke test

**Resolved (2026-08-09):** what this section originally scoped as a
separate follow-up effort was completed directly. `hack/run-stage2.sh` (a
hand-prepared image) and `hack/run-stage2-oci.sh` (this crate's own OCI
conversion pipeline) both automate the five steps below and have each
passed against a real daemon — see the crate README's "Step 0 result" and
"What's actually implemented" sections for the outcomes, including the real
bugs found along the way. What follows is left as-written for the reasoning
that originally scoped this as a separate effort, not as a still-open task.

**Scoped honestly at the time of writing: out of reach for a first pass, and
plausibly out of reach for this specific 7.7 GB VM even after a disk
resize, without more dedicated effort than this plan's original scope.**
What follows records what that would have taken:

1. **Convert one real sandbox image** (the implementation plan's Phase 2
   "Implementation Steps" item 1, and the design doc's "The OCI-image gap"
   discussion under "Phase 2 shape: native driver, kept opt-in"):
   `umoci unpack` an existing OpenShell OCI
   sandbox image, then hand-build LXD's expected image shape
   (`metadata.yaml` + squashfs/tarball) and `lxc image import` it. This is
   explicitly **not built yet anywhere in this codebase** — the design doc
   calls the general version of this pipeline "the largest single Phase 2
   workstream," and Phase 1 has no automation for even the one-off manual
   version. Doing this by hand for the first time, correctly, is realistically
   its own multi-hour investigation (image format details, `umoci`
   installation, verifying the resulting LXD image actually boots), not a
   checklist item.
2. **Set up the managed bridge network** `LxdComputeConfig::network_name`
   expects (default `DEFAULT_NETWORK_NAME`, `"openshell"`) —
   `LxdClient::ensure_network` creates it if missing, but Stage 2 would be
   the first time that path runs against a real daemon at all.
3. **Build and run the actual `openshell-driver-lxd` binary**
   (`cargo build -p openshell-driver-lxd`, then the exact invocation in the
   crate README's "Running it" section), passing the converted image's
   alias and the Stage 0/1-built supervisor binary path.
4. **Build and run a real gateway** pointed at the driver's socket (also in
   "Running it") — this pulls in the full `openshell-server` dependency
   graph and its own runtime requirements (mTLS bundle handling, auth,
   etc.), none of which this plan has touched or verified fits this VM's
   resources.
5. **Run the CLI lifecycle** (`openshell sandbox create` → `connect` → `exec`
   → `delete`) against that gateway.

Why this is out of scope for a first pass: step 1 alone is a real,
undocumented, multi-step manual procedure this plan cannot responsibly
reduce to "and then run this script" without actually doing it once
first (or finding someone who has) — and doing it wrong produces a
confusing failure two layers away from the actual mistake (a broken LXD
image looks like a driver bug from the `CreateSandbox` RPC's perspective).
Combined with the disk-budget pressure Stage 1 already documented (a full
gateway process, plus a converted sandbox image, plus everything Stage 1
needed, is a lot to ask of 7.7 GB even resized to 20 GB), the honest
recommendation is: **treat Stage 2 as a separate follow-up effort once
Stages 0-1 have a real result**, not a same-session stretch goal.

## Definition of done for this test plan

- [x] Stage 0 has run on `brawny-roadrunner` (or another real Ubuntu/LXD
      host) and produced one of the four outcomes in the Stage 0 table
      above — recorded, not assumed. Run twice, both `PASS` (anomalously —
      see the crate README's "Step 0 result").
- [x] If Stage 0 passed: Stage 1's two `cargo test` invocations have both run
      natively on the VM and their pass/fail counts are recorded. Both pass
      per the crate README's status banner (the real-daemon `--ignored`
      test explicitly; the full unit suite as a prerequisite
      `run-vm-tests.sh` runs before Stage 2 ever gets to run, and Stage 2's
      later passes confirm it did).
- [x] `crates/openshell-driver-lxd/README.md`'s "Step 0 result" block
      reflects the real Stage 0 outcome (mechanical update, done by
      `run-vm-tests.sh`).
- [x] Stage 0's actual result was the *anomalous* pass (nesting alone
      wasn't even required), not a clean no-anomaly pass, so the letter of
      this item's original condition wasn't met — but the substance was:
      the recommendation was reviewed and applied anyway (see "What
      happens to `instance.rs`" above): `security_config_pending_spike()`
      was renamed to `security_config()`, with its doc comment updated to
      describe the validated-with-caveats result rather than a clean one.
- [x] N/A — Stage 0 did not hit the stop-and-reconsider outcome (it
      passed), so no escalation was needed.

## References

- `04-implementation-plan.md` — Phase 1 "Implementation Steps" (Step 0),
  "Test Plan", "Deliverables", "Definition of Done".
- `02-spike.md` — "Risks & Open Questions".
- `03-design-rfc.md` — "Non-goals" (packaging/storage pins), "Risks".
- `crates/openshell-driver-lxd/README.md` — "Step 0 result", "Running it",
  "Development notes" (the sandboxed-shell test restriction).
- `crates/openshell-driver-lxd/src/instance.rs` — module doc comment,
  `security_config()` (renamed from `security_config_pending_spike()`).
- `crates/openshell-driver-lxd/src/client.rs` — `LxdClient`, the
  `real_daemon_create_get_list_delete_lifecycle` integration test.
- `crates/openshell-driver-lxd/hack/confinement-spike.sh` — the full spike
  procedure.
- `mise.toml`'s `zig`/`cargo-zigbuild` tool entries, the `vm:supervisor`
  task in `tasks/vm.toml`, `run_supervisor_build()` in
  `tasks/scripts/vm/build-supervisor-bundle.sh`, and
  `ensure_build_nofile_limit()` in `tasks/scripts/build-env.sh` — the
  existing `cargo zigbuild` cross-compilation path Stage 0 reuses.
- `deploy/docker/Dockerfile.ci`'s system-dependencies `apt-get install`
  layer — the apt package baseline Stage 1's prerequisites list is drawn
  from.
