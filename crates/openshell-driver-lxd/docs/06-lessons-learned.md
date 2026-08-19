# Debugging an LXD Compute Driver: What I'd Tell Canonical's Tooling Team

*Notes from building and hardening `openshell-driver-lxd` — a real, unmocked LXD
integration for a container-sandboxing platform. Written as interview-prep
material, framed the way I'd discuss it with an LXD/developer-tooling team.*

## TL;DR

Getting a new compute backend from "compiles" to "a real sandbox lifecycle
passes against a real daemon" took ~25 iterations against a live LXD instance
across two milestones: Phase 1 (a hand-prepared image) and Phase 2 (this
crate's own OCI-to-LXD conversion pipeline, tested against a real, unmodified
sandbox image with no manual prep at all). Roughly fifteen distinct,
previously-unknown bugs surfaced across both — split roughly evenly between
"LXD works differently than the container runtime I already knew," "our own
diagnostics weren't good enough to see the real error," and, in Phase 2
specifically, "a pipeline that processes untrusted third-party archives has
to think about privilege the way a security-sensitive tool does, not the way
a build script does." The single most expensive Phase 1 bug was a **one-line
capability-list omission** that took four rounds of instrumentation to even
*see*. The single most expensive Phase 2 bug was **a tar-extraction pipeline
silently discarding every file's real ownership**, which stayed invisible for
several fixes because the *symptom* it eventually caused looked identical to
an unrelated missing-capability error. None of these were exotic once
understood — all were "the abstraction leaks in a way the happy path never
shows you."

*Addendum, Phase 2 Steps 5-8 (mTLS, resource limits, driver-config
mounts, rollback hardening): a different kind of round from the two
above. Nothing here has been run against a real daemon yet, so there's
no live-failure narrative to tell — the one real finding (below) came
from reading LXD's own docs carefully before writing any code, not from
a container that wouldn't boot. That's itself worth noting: the
cap.keep and network-bring-up lessons above cost real debugging time
precisely because the mismatch with the Docker/Podman mental model
wasn't visible until a live run exposed it. This one didn't have to
cost anything, because the docs were checked first.*

## The headline lesson: `lxc.cap.keep` is not what Docker/Podman engineers expect

Coming from Docker/Podman, "capabilities" means an *additive* mental model:
the runtime ships a sane default set, you add what you need, you drop what
you don't. Podman's own default set already includes `CAP_SETUID`/
`CAP_SETGID`/`CAP_CHOWN`/`CAP_FOWNER` — a driver author never has to think
about them; they're just there.

LXD's `raw.lxc: lxc.cap.keep = X Y Z` has **no such baseline**. It's an
exhaustive allowlist. Everything not named is dropped — including the
capabilities a container would otherwise carry by default. There's no "keep
the defaults, plus these." I ported a capability list from a Podman driver's
README table, it compiled clean, passed a unit test that asserted the
generated config *contains every entry in the same list it was built from*
(a tautology that can never catch a *missing* entry), and quietly dropped
`setuid`/`setgid`/`chown`/`fowner` on the floor. The supervisor's own
privilege-drop call — `setuid()`/`setgid()`, executed while still notionally
running as UID 0 — failed with a bare `EPERM`, because modern Linux enforces
capability checks for these syscalls regardless of the "root can do anything"
folklore.

**The transferable lesson**: when porting a security/isolation config between
runtimes, translate the *effective* permission set, not the *literal* list of
named flags in the other runtime's docs. "Additive allowlist" and "exhaustive
allowlist" look identical in a code review diff and are operationally
opposite.

## Second lesson: an error can be real, correctly constructed, and still never reach you

This was the more painful one to track down, because it looked like a moving
target. I added descriptive `.map_err(...)` context to a candidate failing
syscall inside a `pre_exec` closure — a `setns(CLONE_NEWNET)` call — reran,
and got back the *exact same bare `Invalid argument (os error 22)`*, byte for
byte, as before the fix. Wrapped a second candidate (seccomp filter
installation). Same result. No change, at all, twice.

The actual mechanism: `std::process::Command::pre_exec()`'s closure runs in
the forked child, before `exec()`. If it fails, the *only* channel back to
the parent process is a raw OS `errno`, sent over a pipe across the fork
boundary — that's the entire wire protocol. An error built without a real
`raw_os_error()` (which is what *every* `io::Error::other(...)`-wrapped or
`miette`-formatted error is) has nothing transmissible, so libstd substitutes
a generic sentinel. No amount of enriching the *returned* error was ever
going to work; the parent process literally cannot receive it. Two
consecutive "here's more context" fixes produced zero observable change —
which, on reflection, was itself the diagnostic. When a fix that *should*
change an error message doesn't, that's evidence about the channel, not
about your fix.

The only way out was to stop trying to make the return value carry
information it structurally cannot carry, and write the diagnostic straight
to fd 2 (`libc::write`, async-signal-safe, legal inside `pre_exec`) *before*
returning. That worked instantly, because the child had already inherited a
stderr redirected to a real log file three layers up the process tree.

**The transferable lesson**: any error path that crosses a `fork`/IPC/RPC
boundary has a *maximum information capacity*, independent of how much
context you put into the error type on the sending side. If two rounds of
"add more context" produce byte-identical output, stop enriching the return
value and go look at the channel itself.

## Third lesson: LXD's "console log" isn't the console log

`lxc info --show-log` reads as "show me PID 1's console output." It doesn't —
it's liblxc's own internal C-level trace/debug log (namespace setup, mount
sequencing, `child ended on error N`). The actual PID 1 tty/console ring
buffer is a *different* command, `lxc console --show-log`, backed by a
separate file (`.../logs/<instance>/console.log`) that most tooling built
around `lxc info` never touches.

I built a diagnostics harness that dumped `lxc info --show-log` on every
failure and treated an empty result as "the supervisor never printed
anything." It was empty because I was reading the wrong log, not because
nothing was printed. If I were feeding this back to the LXD CLI/docs team:
the naming collision between "the log LXD shows you by default" and "the
log the container's own process actually wrote to" is a real trap for anyone
building automation on top of `lxc`, and a one-line disambiguation in
`lxc info --help`'s output (or just renaming the flag) would save real
debugging time for exactly the kind of person writing a `run-*.sh` harness.

## Fourth lesson: shell redirect scoping is a real, current-day footgun

A generated entrypoint script did:

```sh
{
  echo "..."
  # network setup commands
} >/var/log/openshell-entrypoint.log 2>&1
exec /opt/openshell/bin/openshell-sandbox "$@"
```

POSIX shell redirects on a `{ ...; }` compound command apply **only for the
duration of that block**. The moment the block ends, the original file
descriptors are restored — so the final `exec`, the one line whose output
mattered most, was never captured at all. It went to whatever fd 2 was
*before* the block, in this case LXD's raw pty console. The fix is a
standalone `exec >file 2>&1` (no braces) as the first statement, which
persistently redirects the *current shell's* fds for everything after it,
including a subsequent `exec` into another program.

This is the kind of bug generated shell scripts are especially prone to,
because `{ ... } >file` "looks" like it should behave like a shell function
with baked-in output redirection — it doesn't, and the difference is
invisible until the one line after the block is the one you needed.

## Fifth lesson: LXD's network model has no equivalent of "the orchestrator hands you an IP"

Docker, Podman, and Kubernetes all inject an IP address into a container from
the outside — the runtime does it, the guest never has to ask. LXD's
"bridged NIC" device model doesn't: it plugs a veth into a bridge and expects
the *guest's own boot sequence* (systemd-networkd, cloud-init, DHCP client)
to configure the interface, same as it would on bare metal. Overriding
`lxc.init.cmd` to boot straight into a supervisor binary — which is exactly
what you want for a lightweight sandbox, skipping seconds of systemd/DHCP
negotiation — means you've also skipped the *only* mechanism that would have
configured the network. Nothing tells you this until the container comes up
with an unconfigured `eth0` and every subsequent gRPC call from inside times
out looking like a firewall problem.

**The transferable lesson**: "override PID 1" is a much bigger decision on
LXD than the equivalent `ENTRYPOINT` override on Docker, because Docker's
network setup happens in namespace/plumbing the runtime owns, while LXD's
happens in guest userspace that PID 1 was supposed to run.

## Sixth lesson: a tar pipeline running as non-root silently rewrites history

The OCI-to-LXD conversion pipeline pulls real container-image layers,
extracts them, merges them into one rootfs, and repackages the result as a
new tarball for LXD to import. Every step of that ran fine, against two
different images, through weeks of iteration — until a real sandbox image's
own supervisor called `mkdir /run/netns` and got `EACCES`, indistinguishable
at the call site from yet another missing-capability problem (which is
exactly what the last several bugs had actually been).

It wasn't a capability problem. `/run` is root-owned, mode `0755`, in
essentially every base image — completely ordinary. The pipeline's own
staging process, though, runs as an unprivileged host user, and `chown` to a
UID other than your own always fails with `EPERM` if you're not root —
a basic, decades-old Unix rule, not a bug in the tar library being used. I
proved this with a five-line reproduction: build an in-memory tar archive
with one directory entry whose header declares `uid=0`, extract it as the
current (non-root) user, and stat the result. It comes back owned by the
extracting process's own UID — the declared `uid=0` is silently discarded,
not rejected, not logged, not surfaced anywhere. Every file that happened to
land on a permissive path (`/tmp`, world-writable) kept working by accident.
The first file that needed its *real* declared ownership to function
(`/run`, owner-only-writable) broke, and the error it produced looked
nothing like "ownership got silently rewritten three pipeline stages ago."

The fix wasn't to make extraction preserve ownership on disk — that would
require running the whole pipeline as root, trading one security concern
(processing untrusted third-party archives) for a bigger one. It was to
stop trusting the filesystem to remember something it structurally cannot
hold under these privileges: track each entry's *declared* ownership
out-of-band, thread it through every merge/override step alongside (not
instead of) the file content, and apply it explicitly when writing the
*final* tar headers — never reading ownership back off the non-root staging
disk at all. The consuming system (LXD's own image importer, which does run
with real root/idmap privileges) is the only place ownership needs to become
real again.

**The transferable lesson**: any pipeline that extracts a third-party
archive as a non-root process and later re-emits it is implicitly making a
promise — "this output faithfully represents the input" — that the
underlying primitives cannot keep for you by default. `tar` extracting as
non-root doesn't error when it can't honor a declared UID; it just does
something else and stays quiet about it. If a pipeline's own correctness
depends on a property (ownership, in this case; it could just as easily be
extended attributes, ACLs, or hardlink structure) that non-root extraction
can't actually preserve, that property needs to be tracked explicitly, not
assumed from "the extraction call didn't return an error."

## Seventh lesson: not every POSIX builtin fails the same way on a failed redirect

A generated entrypoint script's own defensive fallback — "try writing to
`/var/log`, fall back to `/tmp` if that fails" — used the most minimal
command imaginable to probe writability: `:`, the shell's no-op builtin,
inside an `if`. It looked unimpeachable: `if` catches nonzero exit codes,
and a no-op with a failed redirect should just... fail, right, and get
caught by the `if`?

Not for `:` specifically. POSIX designates a specific list of builtins —
`:`, `break`, `continue`, `eval`, `exec`, `exit`, `export`, `readonly`,
`return`, `set`, `shift`, `times`, `trap`, `unset` — as *special*, and
mandates that a redirection error on a special builtin exits the shell
immediately, `if` guard or not. `true` is not on that list; `:` is. Under
`dash` (Ubuntu's real `/bin/sh`, and so the actual shell running this
script as PID 1), the fallback using `:` died at the exact line meant to
detect and recover from a failure, reproducing the identical symptom
(PID 1 dying on startup) the fallback existed to fix — one line later,
same crash, new cause. Under the Mac's own default `/bin/sh` used to
smoke-test the script locally, the same code ran fine: that shell doesn't
enforce this rule the same way, so local testing gave a false pass that
only a real `dash` run (or a test that explicitly invokes `dash`, not
whatever `sh` happens to resolve to) could have caught.

**The transferable lesson**: "any command inside an `if` is safe from
killing the script" is true for ordinary commands and false for a specific,
enumerable list of shell builtins whose failure semantics POSIX defines
differently on purpose. When writing defensive shell code meant to run
under a *specific* target shell, test against that shell by name, not
whatever generic `sh` happens to be on the development machine.

## Eighth lesson: `limits.cpu` looks like Kubernetes' `cpu_limit`, but it isn't

The implementation plan's own shorthand for this step said "map the
sandbox's CPU limit onto LXD's `limits.cpu`." Taken at face value,
that's a one-line change: parse the Kubernetes-style quantity string
(`"500m"`, `"2"`), write it into `limits.cpu`, done — and it would have
compiled, passed a unit test asserting the string round-trips, and
looked completely reasonable in review.

It would also have been the wrong mapping. LXD's `limits.cpu` — read
the actual instance-options reference, not just the key name — is
"[a] number or a specific range of CPUs to expose to the instance."
That's Docker's `--cpuset-cpus`/`--cpus-shares`-adjacent *pinning and
visibility* model: how many (or which) host cores the guest can see and
use, e.g. via `nproc`. It is not a throttle. A Kubernetes/Docker-style
`cpu_limit` of `"500m"` (half a core) has no meaningful whole-number-of-
CPUs translation — rounding up to `"1"` would double the actual
allowance while still reporting a different `nproc` count than the
guest would see under Docker or Podman for the identical request. The
key that's actually the CFS-bandwidth throttle — same cgroup mechanism
Docker's `--cpus` and Podman's own `CpuLimits{quota,period}` already
use under the hood — is a *different* key, `limits.cpu.allowance`,
taking a `"<quota>ms/<period>ms"` chunk-of-time string. `limits.cpu`
and `limits.cpu.allowance` sit right next to each other in the same
docs table, answer what sound like the same question ("how much CPU"),
and do genuinely different things.

Two things made this findable without a live failure: **the plan's own
wording used the word "limit," which only means one thing in every
other driver in this codebase** (a hard ceiling, not a pinning
assignment) — the docs table's own wording ("expose to the instance")
was the tell that this specific key answers a different question than
the one being asked. And the same table is where `limits.memory`
actually lives too, so checking one key's exact semantics before
writing code was already the path of least resistance, not extra work
bolted on.

**The transferable lesson**: when a design doc says "map X onto
`runtime.key_name`," treat the key name as a hypothesis, not a given —
especially when the *source* concept (a Kubernetes-style resource
limit) is a well-established convention with one specific meaning
(a throttle) and the *target* runtime has multiple keys that could
plausibly be "the CPU one." A key name that sounds right and a key that
does the same *thing* the source concept means are not guaranteed to be
the same key, and the gap between them is invisible in review, in a
unit test that only checks string formatting, and in `cargo build` —
it's only visible in the runtime's own semantics documentation, or in
whatever a real guest's `nproc` reports once it's too late to be cheap
to fix.

## Process lessons (not LXD-specific, but reinforced hard by this session)

- **Diagnostics are load-bearing infrastructure, not an afterthought.**
  Roughly half of this session's iterations were spent building better
  visibility (fixing the redirect scoping bug, capturing the *right* log,
  adding a host `dmesg` tail) rather than fixing the actual bug — because you
  cannot fix what you cannot see, and guessing against a 60-90 second
  round-trip (rebuild, redeploy, re-run) is far more expensive than a few
  extra minutes spent making the next failure legible.
- **A negative result is still a result.** Two rounds of "wrap the error with
  more context, see zero change" felt like failure in the moment. It was
  actually the exact signal needed to realize the *channel* was the problem,
  not the *content*.
- **Self-consistency is not correctness.** A test that checks generated
  config against the same list used to generate it will pass forever,
  including the day someone forgets to update the list. It caught a syntax
  error class of bug perfectly and a completeness class of bug never.
- **Write the "why" down while it's fresh.** Every fix in this session got a
  doc comment or README paragraph explaining the failure mode *and* why the
  fix works, immediately — not "TODO: explain later." Six weeks from now,
  "the capability list needs setuid/setgid/chown/fowner" is a mystery without
  "because LXD's cap.keep has no baseline, unlike the Podman driver next to
  it that this was copied from."
- **A local lint failure isn't yours until you've checked the baseline.**
  Running the project's real `mise run pre-commit` (not a bare `cargo
  clippy`, which uses a laxer default lint level than the project's own
  `-D warnings` invocation) surfaced a page of failures after Steps 5-8's
  changes — most of them in files this round never touched
  (`ssh.rs`, `image.rs`, `client.rs`). Fixing all of it under time
  pressure would have silently expanded the change's scope into
  unrelated, already-committed code. Checking out the *parent* commit in
  a scratch worktree and re-running the identical lint command first
  showed every one of those failures already present there — confirming
  they predated this round entirely (some are `#[cfg(target_os =
  "linux")]` code that's only "dead" when linting on macOS, a
  platform/target mismatch, not a real defect) and weren't something
  this change was responsible for cleaning up. The two failures that
  *didn't* reproduce on the parent commit (a `collapsible_if`, a
  `match_same_arms`) were the actual, in-scope regressions, and got
  fixed. A five-minute worktree diff turned "is this my problem?" from a
  judgment call into a fact.

## If I were on this team

- The `raw.lxc: lxc.cap.keep` semantics vs. every OCI runtime's additive model
  is exactly the kind of "obvious once you know it, invisible until you're
  bitten" gap that belongs in a prominent callout box in the capabilities
  docs, not just in the config key's reference page.
- `lxc info --show-log` vs `lxc console --show-log` naming is a real support
  burden — I'd bet a nontrivial fraction of "my container exited and I can't
  tell why" issues are people reading the wrong one.
- A `lxc launch --dry-run`-style "what would this raw.lxc/init.cmd/network
  config actually do at boot" preflight would have caught at least two of
  these bugs (dropped capability, no network bring-up) before ever starting
  a real container.
- `POST /1.0/images`'s own docs are silent on ownership expectations for a
  unified-tarball upload. A one-line callout — "if you're building this
  tarball with anything other than a real root/idmap-aware tool, verify
  your tar headers actually carry the ownership you intend, because
  non-root extraction tools commonly discard it silently" — would have
  turned a multi-hour debugging arc into a five-minute read.
- `limits.cpu` and `limits.cpu.allowance` being adjacent rows in the same
  "Resource limits" table, both plausibly answering "how do I limit this
  instance's CPU," is the same category of gap as `lxc.cap.keep`'s
  additive-vs-exhaustive surprise above — obvious once you've read both
  rows closely, easy to get wrong on a skim. A cross-reference on
  `limits.cpu`'s own row ("looking for a Docker `--cpus`-style throttle
  instead of a core-count/pinning assignment? See `limits.cpu.allowance`
  below") would resolve the ambiguity at the exact point someone's about
  to make this mistake, rather than relying on them to read the whole
  table before writing any code.
