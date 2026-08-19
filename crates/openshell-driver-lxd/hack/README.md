# Local Testing Scripts

Manual, real-daemon test scripts for `openshell-driver-lxd`. These are not
part of `mise run test`/`mise run ci` and never run in CI — every one of
them needs a real LXD daemon on a real Ubuntu/Linux host, which no CI
runner or sandboxed agent shell has. Run them yourself, from your own
terminal, against one of:

- **WSL2** (Windows) — `wsl --install Ubuntu`, then clone the repo directly
  inside the distro (WSL2's filesystem is already a real Linux filesystem;
  no separate mount/copy step needed).
- **Multipass** (macOS) — a launched Ubuntu VM, with the repo mounted or
  copied in.
- A cloud instance or bare-metal Ubuntu/Linux host.

Each script's own header documents its exact invocation for whichever
environment you're using — see `run-vm-tests.sh`'s header for the fullest
version of that explanation; every other script's header just points back
to it rather than repeating it.

## What each script does, and the order to run them in

1. **`run-vm-tests.sh`** — Stage 0 + Stage 1. Installs prerequisites (LXD,
   a C/C++ toolchain, Rust) if missing, then runs `confinement-spike.sh`
   (below) to confirm LXD can host `openshell-sandbox`'s isolation
   primitives (namespaces, Landlock, seccomp) without privileged mode, then
   builds and tests `openshell-driver-lxd` natively, including the one
   real-daemon integration test. Run this first — everything else assumes
   its prerequisites are already satisfied.
2. **`confinement-spike.sh`** — the actual Stage 0 probe `run-vm-tests.sh`
   calls internally. Only run it directly if you want to re-check
   confinement in isolation (e.g. after an LXD/kernel upgrade) without a
   full `run-vm-tests.sh` pass.
3. **`run-stage2.sh`** — Stage 2: a full `sandbox create → exec → delete`
   lifecycle against a real gateway/CLI, using one hand-prepared plain
   Ubuntu image (bypassing the OCI conversion pipeline, to isolate
   driver/gateway/lifecycle correctness from image-conversion
   correctness).
4. **`run-stage2-oci.sh`** — the same lifecycle, but through this crate's
   own OCI-to-LXD image conversion pipeline (`src/image.rs`) against a
   real, unmodified sandbox image — no manual image prep at all. Needs
   real network access to the container registries it pulls from
   (`ghcr.io`, `docker.io`), in addition to the daemon itself.
5. **`run-managed-driver.sh`** — validates the gateway-managed spawn path
   (`compute_drivers = ["lxd"]` with no manual `--compute-driver-socket`):
   the gateway itself resolves and spawns `openshell-driver-lxd`, runs a
   full lifecycle through it, and on graceful shutdown reaps the driver
   child and removes its socket.
6. **`run-feature-parity.sh`** — Phase 2 Steps 5-8: guest mTLS, resource
   limits, driver-config mounts, and rollback-on-failure, each as its own
   test case against a real daemon.

## Output

Each script (except `confinement-spike.sh`, which only prints to stdout)
writes one consolidated Markdown results file plus a same-named directory
of raw logs under `results/` (gitignored — see that directory's own
`.gitignore` for why). These are ephemeral run artifacts for your own
debugging, not a permanent record: the durable, curated account of what
real runs have found — bugs, fixes, and current verification status —
lives in `../docs/04-implementation-plan.md` and `../docs/06-lessons-learned.md`.

## If a script fails

Read the results file and raw logs it wrote under `results/` first. If
you find and fix a real bug, update `../docs/04-implementation-plan.md`
with what you found (matching that document's own established style: what
broke, why, and how it was fixed) rather than just committing the code
fix silently — that document's whole value is the trail of real,
only-found-by-a-real-daemon issues it accumulates.
