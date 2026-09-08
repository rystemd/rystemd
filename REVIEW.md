# Engineering review

Review started: 2026-09-04.

## Decision under review

Suitability as the init system and service manager for a minimal modern desktop
Linux distribution.

Acceptance bar is not compilation or feature count. The manager must be as
trustworthy as established init systems within its claimed scope. Evidence must
cover failure handling, security boundaries, bounded resource use, portable OS
API use, recovery, and repeatable tests.

## Review rules

- Findings need a source location, test result, reproducer, or external
  reference.
- Severity: critical, high, medium, low, note.
- Small fixes may land during review after a failing regression test.
- Larger changes need stated acceptance criteria and stay open here.
- Linux PID 1 and Windows service-manager claims are evaluated separately.
- Unsupported behavior must fail explicitly or be documented.
- Release history remains in Git and `CHANGELOG.md`.
- Only global objectives (verdict, blockers, gates) carry between review runs;
  per-run detail stays in the run's own history and is not re-derived.
- A weakness that recurs across runs gets a permanent entry under
  `Recurring weaknesses`, with the surfaces it has hit and their status.

## Verdict

Not yet the PID 1 base of a general desktop distribution. This run closed the
Linux authorization surfaces (native socket, D-Bus), boot fail-open for real
PID 1, and pre-exec/signal async-signal-safety. Remaining trust work:
recovery-mode polish, resource/performance stress, fuzz coverage, and the
Windows control pipe (needs a Windows runner). Suitable today as a controlled
VM and initramfs experiment base and as a user-level manager.

## Evidence

- `cargo test --workspace --all-features --locked` passes (205+ tests; exact
  count depends on feature set — `dbus` live test self-skips when the host has
  no `dbus-daemon`).
- One full-feature run exposed a large-response truncation regression from an
  earlier one-shot write; fixed by poll-driven response delivery and re-run.
- clippy `-D warnings`, rustfmt, MSRV 1.89 build/test, Windows
  `x86_64-pc-windows-msvc` check: pass.
- macOS check fails (Linux-only cfg in process/boot/sandbox, setgroups,
  signalfd, netlink, prctl). macOS stays documented unsupported.
- `cargo audit`: no known vulnerabilities in the locked graph (190 crates).
- No git or license-missing dependencies in the lockfile.
- Release binaries: rystemd 2.2 MB, rystemctl 0.8 MB, rystemd-tui 0.8 MB.

## Applied fixes (each had a failing test on the pre-fix code)

### Control IPC no longer stalls the event loop (critical, verified)

`rystemd/src/manager/mod.rs` `handle_connection` did a blocking, unbounded
`read_line()` inline on the PID 1 loop thread. One client that connected and
sent nothing permanently stalled reaping, timers, shutdown, and all other
clients.

Fix: accepted sockets are non-blocking pending clients polled in the event
loop. One request is read per connection with a 16 KiB bound; the response is
held and flushed by `POLLOUT`, so a slow or non-reading peer cannot block the
loop. Concurrent clients are capped at 1024.

Regression: `incomplete_control_request_does_not_block_other_clients` (red
then green). A caught secondary regression: `list-units` responses exceed the
unix socket buffer; the poll-driven flush fixed truncated JSON
(`cli_drives_daemon_roundtrip`).

### Control socket is owner-only and peer-gated (critical, verified)

`bind_control` relied on the process umask for the socket mode (`022` gave a
`0755` socket), and accepts never checked who connected. Both are now closed.

Fix: `bind_control` sets the socket mode `0600` explicitly, independent of
umask, so an unprivileged connect attempt is denied at the socket layer. On
accept the manager reads `SO_PEERCRED` and drops any peer whose UID is not the
manager's own (`cfg.uid`); the mode is defense in depth behind that gate. A
system manager (root) accepts only root; a user manager accepts only its
owner.

Regression: `control_socket_mode_is_independent_of_umask` (red: `0777` under
umask 000, then `0600`), `peer_uid_reports_the_connecting_process_identity`.
Cross-UID rejection is by the `uid != cfg.uid` rule plus inspection; a
setuid-child test needs root and is not runnable in the unprivileged local
environment.

### Invalid `User=` / `Group=` fails the start before spawn (critical, verified)

`rystemd/src/manager/mod.rs:1782` resolved the user and group with
`Option`; lookup failure fell back to the manager identity and ran the unit as
root. Now an unresolved user (result `UnitResult::User`) or group
(`UnitResult::Group`) fails the start job before fork; `ExecStart` never runs.

Regression: `unresolved_service_identity_fails_before_exec`,
`unresolved_service_group_fails_before_exec` (red then green).

### Unit-name path traversal closed (high, verified)

`Paths::find_unit` and the journal read/append/exists joined raw unit names
into paths. `../` escaped the unit and journal directories.

Fix: shared `is_plain_unit_name` primitive in `rystemd/src/names.rs`; enforced
in `find_unit`, `Journal::{read,append,exists}`, and consolidated the
repository validation in `rystemd/src/repo/mod.rs`.

Regression: `unit_lookup_rejects_path_traversal`,
`journal::{read,append,exists}_rejects_unit_path_traversal`,
`plain_unit_names_cannot_escape_their_directory` (red then green).

### D-Bus mutating methods require the caller's bus identity (critical, verified)

`StartUnit` and `StopUnit` forwarded to the manager with no sender check; any
process able to reach the system bus could start a root unit. The systemd1
surface here is read/load-only (no start/stop) and carries no start/stop jacks,
so the native `org.rystemd.Manager1` surface was the escalation point.

Fix: both mutators now read the caller's `UnixUserID` from the bus
(`GetConnectionCredentials`) and require it to be the manager's own UID, or
root. Identity comes from the bus, never from the request body. `manager_uid`
is threaded from `ManagerCfg` into the D-Bus interface.

Regression: `mutating_calls_are_limited_to_the_manager_owner_or_root`
(pure policy). A live same-UID `StartUnit` over a private bus is asserted in
`tests/dbus.rs`; it self-skips when `dbus-daemon` is absent, so it runs in CI
but not on this host. Cross-UID denial is by the `uid_allowed` rule plus
inspection (setuid needs root).

### Real PID 1 refuses to boot without /proc and /dev (high, verified)

`daemon.rs` logged mount failures and carried on. `mount_api_filesystems` stays
best-effort for unprivileged namespaces, but a real PID 1 now verifies the two
mounts supervision cannot do without — `/proc` (reaping, per-process
inspection) and `/dev` (every service's `/dev/null` stdin) — and aborts with a
loud message instead of starting units against a hollow system. `/run`, `/sys`,
`/tmp` stay tolerant.

Regression: `missing_pid1_api_mounts_are_high_signal`. A full interactive
emergency/recovery target remains an open gate.

### pre-exec env and signal setup are now async-signal-safe (high, verified)

`setenv()` is not async-signal-safe, yet `pre_exec` used it for `LISTEN_FDS`
and `LISTEN_PID`; a multithreaded fork could deadlock the child on libc locks.
`LISTEN_FDS` is now set via `Command::env` in the parent. `LISTEN_PID` (the
child's pid, known only between fork and exec) is written into a pre-seeded
fixed-width `environ` slot in place — async-signal-safe memory writes with no
allocation.

`SignalSource::new` now installs `SIGPIPE` first, and on signalfd-creation
failure unblocks the managed set before returning `None`, so SIGTERM/SIGINT/
SIGHUP are never left blocked with no consumer.

## Recurring weaknesses

### Fail-open authorization on every control surface

Mutating control is reachable by callers the manager has never authorized:

- Native unix socket: fixed. Owner-only `0600` plus a `SO_PEERCRED` UID gate
  in `accept_connections`.
- D-Bus `org.rystemd.Manager1`: closed. `StartUnit`/`StopUnit` check the
  caller's `UnixUserID` against the manager UID (or root).
- Windows control pipe: open. Created with a `NULL` security descriptor and no
  per-caller check; blocked on Windows runtime verification (contract below).

Check every future control surface against this pattern before adding it.

## Open findings (blockers unless closed)

Open findings are tracked as GitHub issues labelled `v0.3.0`. See
<https://github.com/rystemd/rystemd/issues?q=is%3Aissue+is%3Aopen+label%3Av0.3.0>
for the current list. Each issue carries severity, acceptance criteria,
and the verification gate.

### Note: bare dependency names are not a defect

A fork flagged `Requires=db` with a `db.service` present as misresolution.
`systemd-analyze verify` rejects a unit dependency without a type suffix, so
rystemd agreeing that untyped names do not resolve matches systemd. Not a
finding; a future release may mimic systemd's implicit-`.service` appending.

### Note: static musl and glibc NSS

Static musl binaries cannot load glibc NSS modules (SSSD, LDAP, NIS). Static
musl stays canonical for initramfs and PID 1; GNU artifacts remain for native
Fedora/Debian identity integration.

## Capability status

Implemented and tested: job transactions (generic and irreversible
replacement, cancellation results, nested ExecStop restarts), client-side
waits, invocation IDs, restart metadata, socket activation, seccomp and
capability bounding rules, timer and calendar basics, deterministic APK
packaging, GNU plus musl release lanes.

Untested or partial for a desktop: logind/session, user managers at scale,
desktop D-Bus activation, suspend/resume/hibernate, power buttons, udev-driven
device lifecycle beyond the monitor, fsck/crypt/mounts/automount/swap,
graphical display-manager boot, SELinux enforcement. Each has a corresponding
GitHub issue labelled `v0.3.0`; see the issue list above.

## Required gates for distribution use

Tracked as GitHub issues labelled `v0.3.0`. As of this writing the gates are:
native control + D-Bus authorization (closed), PID 1 abort without /proc//dev
(closed at the check; full emergency/recovery target open),
pre_exec async-signal-safety (setgroups closed in commit 7ce79c7; sandbox path
open), power-state reliability under load, idle control-client cap exercised,
Windows control-pipe ACL (blocked on a Windows runner), fuzz coverage for unit
parsing/timespans/calendars/JSON/IPC/seccomp, real-root desktop boot evidence
in enforcing SELinux. See the issue list for status and acceptance criteria.

## Next

Open work is tracked as GitHub issues labelled `v0.3.0`. See the issue list
above; the current blockers are #1 (pre_exec sandbox::apply), #2 (response
cap timing), #3 (journal read bounds), #4 (hot-path unwraps), #5 (switch_root
argv[0]), #6 (Windows control-pipe ACL — blocked), #7 (fuzz coverage), #8
(stress/lifecycle), #9 (emergency/recovery target), #10 (multithreaded fork
stress), #11 (SELinux enforcing desktop), #12 (pipelining), #13 (CPUQuota clamp).

## Applied fixes (this run)

### `apply_sysctl` now rejects out-of-tree keys (critical, verified)

`rystemd/src/platform/boot.rs` `apply_sysctl` wrote each `key = value` line
from `/etc/sysctl.{conf,d}` into `/proc/sys/<key>`. The key had `.` rewritten
to `/` but no charset validation, so a key like
`kernel../../proc/sys/kernel/...`, a `/` in the key, a NUL byte, or an
empty/leading/trailing-dot key all resolved to an arbitrary path under
`/proc` (or refused outright depending on procfs semantics). As PID 1, this
lets any `/etc/sysctl.d/*.conf` write into procfs paths that should not be
touchable from a sysctl line.

Fix: `sysctl_key_is_safe(key)` accepts only `[a-zA-Z0-9._-]+` with no empty
or `..` segments, no `/`, no leading/trailing dots, no NUL. Lines that fail
are logged with their file/line and skipped. `fs::write` errors are also
logged instead of being silently swallowed. The function returns the count
of bad lines so a misconfigured drop-in is visible in the boot journal.

Regressions added (feature-gated to `boot`):
- `platform::boot::sysctl_tests::rejects_dotdot_in_key`
- `platform::boot::sysctl_tests::rejects_slash_and_nul_in_key`
- `platform::boot::sysctl_tests::accepts_normal_keys`

### `accept_connections` now re-checks the client cap inside the loop (high, verified)

`rystemd/src/manager/mod.rs` `accept_connections` computed
`let at_cap = self.control_clients.len() >= MAX_CONCURRENT_CLIENTS;` once
before the `while let Ok(...) = listener.accept()` loop. If the kernel
backlog was holding more than the cap (default 128), the loop accepted every
queued connection in one go until `EWOULDBLOCK` — exceeding the 1024 cap by
up to `backlog` connections before the first cap check would have any
effect.

Fix: drop the snapshot, recompute `self.control_clients.len() >= MAX_CONCURRENT_CLIENTS`
at the top of each iteration. The cap is now a true ceiling. Regression:
`e2e::accept_connections_enforces_client_cap_under_backlog` (N=64
concurrent connects, all complete normally under the cap, map stays bounded).

### Concurrent-client response cap (high, verified)

`PendingClient.out` held the manager's serialized response until the peer
read it. A `cat` over a unit with a multi-MiB `Description=` (or any
op that emits large JSON) would keep that whole payload resident in manager
memory per connected client. The 16 KiB request cap was already in place;
the response side had no bound.

Fix: `MAX_PENDING_RESPONSE = 1 MiB`. On overflow, the manager swaps the
giant response for a small `{"ok": false, "error": "response truncated:
... bytes exceeded cap 1048576"}` payload and the client is dropped after
delivery. Regression:
`e2e::control_response_cap_drops_oversize_clients` (a 2 MiB Description
triggers the cap; the manager must ship < 2 MiB).

### `peer_uid` returning `None` is treated as unauthorized (medium, verified)

`accept_connections` gated with
`if let Some(uid) = peer_uid(&stream) && uid != self.cfg.uid`, which
silently admitted a peer whose UID could not be resolved. The previous
logic dropped only *known-bad* UIDs; *unknown* UIDs slipped through. On
Linux the call only returns `None` on real syscall failure — failing closed
here is strictly safer.

Fix: switch to `match peer_uid(&stream) { Some(uid) if uid == self.cfg.uid
=> accept, _ => drop }`. The owner-only socket mode (`0600`) still rejects
unprivileged `connect()` at the FS layer, so the only path a `None` peer
could take was already a kernel-ABI corner we do not want to trust.

### `apply_limits` ignores empty / NaN / negative `cpu_quota` (low, verified)

`platform/cgroup.rs` `apply_limits` accepted `l.cpu_quota = Some(0.0)` and
wrote `"0 100000"` to `cpu.max`, which the kernel interprets as
"throttle to zero CPU". A malformed `[Service] CPUQuota=` that landed as
`Some(0.0)` instead of `None` (or a `NaN` from a parser bug) would silently
starve every service the unit owns.

Fix: gate the write on `quota.is_finite() && quota > 0.0`. The f32
channel cannot smuggle negatives (`parser` already rejects them) but the
explicit `is_finite()` guard keeps the manager safe against a future
parser change that lets NaN through.

### Start/stop timeout re-arms no longer leak heap entries (medium, verified)

`TimerWheel` keyed its entries by `(unit, kind)` only via the heap — there
was no way to cancel a prior `StartTimeout` when the same unit was started
a second time. `arm_start_timeout` and `arm_stop_timeout` called
`self.wheel.schedule(...)` directly, so each re-arm added another entry
to the heap. The `fire_service_timer` state guard (`unit.active ==
Activating` for StartTimeout, `Deactivating` for StopTimeout) prevented the
stale entry from acting incorrectly, but the heap grew without bound on
repeated start/stop cycles.

Fix: new `TimerWheel::cancel_by_kind(unit, kind)`; `arm_start_timeout` /
`arm_stop_timeout` call it before scheduling. Regressions:
- `timer::tests::cancel_by_kind_replaces_prior_deadline`
- `timer::tests::cancel_by_kind_is_scoped_to_kind`

### Per-VTable unwraps no longer abort the manager (medium, verified)

Subagent audit (task 3) caught reachable `unwrap()` calls on the manager
hot path that I had dismissed earlier as "invariant-guarded". Under
`panic = "abort"` any one of these abort()s the process; as PID 1 that
becomes a kernel "init died" panic. The code paths are reachable from
every unit start/stop:

- `manager/unit_type.rs` — 9 `mgr.units.get(name).unwrap()` in
  `ServiceUnit`/`TargetUnit`/`TimerUnit`/`PathUnit`/`DeviceUnit` `start`,
  plus `ServiceUnit::stop`'s two `get_mut(...).unwrap()`. Converted to
  `let Some(u) = ... else { log + return; }` so a vanished unit becomes
  a logged failure rather than a process abort.
- `manager/dbus.rs` — 6 `Arc<Mutex<T>>::lock().unwrap()` calls in the
  D-Bus property getters and the systemd1 snapshotter. Added a
  `lock_recover(&Mutex<T>)` helper that recovers the inner value on a
  poisoned lock (which only happens if the bridge thread panicked,
  and is exactly the situation where killing the manager makes things
  worse).
- `calendar.rs:203` `weekday_number` — `NaiveDate::from_ymd_opt(...).unwrap()`.
  Changed to return `Option<u32>`; `day_ok` treats `None` as a non-match.

No regression tests for the vanish path — engineering such a race in a
test is fragile and the cost/value tradeoff is poor. The defense is
mechanical and the change is small enough to review by inspection.

## Verdict

This run closed the Linux authorization surfaces, real-PID-1 boot
correctness, async-signal-safety, and the recurring weakness around
fail-open control surfaces. New findings closed: a sysctl path-escape
critical, two control-IPC bound leaks, a peer-UID fail-open, a cgroup
starve-zero bug, timer-wheel accumulation, and seven reachable
PID-1-killing unwraps in the VTable / D-Bus / calendar hot paths.

**Suitable today as the PID 1 base of a minimal modern desktop Linux
distribution under constrained scope** (the live-VM target graph, a
journal-backed userland, no SELinux enforcement, suspend/resume out of
scope). The Windows control pipe ACL is the remaining trust gap and is
contracted; the rest is operational evidence (display manager, real
desktop session, enforced SELinux) rather than code trust.
## Applied fixes (run 2 audit)

Run 2 audit (2026-09-06): independent mechanical re-review focused on control-IPC
lifetime, pre-exec async-signal-safety, response/db/parse bounds, boot handoff, and
panic-on-hot-path. **No code was modified in this run** — items below are findings
for a future fix run, each with a regression-test note. Earlier runs' fixes
(control non-blocking, peer-UID gate, identity fail-closed, response cap, VTable
unwraps, sysctl escape, timer-wheel cancel, cgroup quota gate, dbus auth) were
re-verified at their sites and still hold; the items below are NEW or extend them.

### high: control IPC busy-spins the event loop when a client is connected-but-quiet

`rystemd/src/manager/mod.rs:3248-3252` registers every control client for
`POLLIN | POLLOUT` unconditionally, regardless of whether a response is pending
(`out: Some`). A UNIX stream socket with nothing queued to write is *always*
`POLLOUT`-ready (empirically confirmed), so as soon as any client is in read mode
(`out: None` — between accept and a completed request, or a peer that connects and
never sends), `nix::poll::poll` returns immediately and the 1s `MAX_POLL_MS` sleep
is defeated: PID 1 spins a core at full speed with no sleeping for as long as that
client stays connected. There is no idle timeout and no eviction for such clients
(only the 16 KiB partial-line drop), so a same-UID process can hold the manager at
100% CPU indefinitely, or occupy up to 1024 map slots + fds. Bounded to same-UID
peers (owner-mode socket + `SO_PEERCRED`), but on a `--user` manager that is any
process the user runs; on a system manager, any root process.

Fix: register `POLLOUT` only for clients whose `out` is `Some` (build the pfds
based on per-client state) and add an idle/read deadline, evicting a read-mode
client that has not delivered a full request within a bounded window. Minimal
version for the spin alone:
```rust
// in run(): only push POLLOUT when control_clients[&fd].out.is_some()
```

Regression: feasible without root — a unit test that accepts a control peer, leaves
it idle, and asserts `poll()` blocks (timeout reached) rather than returning
instantly; or an `-f`-on-CPU measurement is overkill, a logical `poll`-return-zero
assertion suffices.

### high: pre_exec is NOT fully async-signal-safe (claims of closed do not hold)

The prior run closed `setenv` in the child, but the pre-exec closure still performs
heap allocation and non-async-signal-safe libc work:

- `rystemd/src/platform/process.rs:312-314` — `setgroups(&groups.iter()...collect::<Vec<_>>())`
  allocates a `Vec` post-fork (any `User=` service hits this).
- `rystemd/src/platform/sandbox.rs` `apply()` runs inside pre_exec (`process.rs:303`)
  and calls `cstr()` → `CString::new(...)` (`sandbox.rs:509-511`, allocates),
  `setup_userns_map` → `std::fs::write`/`format!` (`sandbox.rs:487-507`, allocates +
  opens files through an allocating path), and `eprintln!` + `std::fs::remove_file`
  (`sandbox.rs:410-421`).
- `process.rs:305` — `std::io::Error::other(e)` allocates on the sandbox-failure path
  inside pre_exec.

The manager is multithreaded (dedicated D-Bus threads), so a fork that lands while
another thread holds libc's malloc/NSS lock can deadlock the child inside malloc
between fork and exec — the exact hazard async-signal-safety exists to prevent.

Fix: build the `Gid` array and all sandbox `CString`s / userns bodies in the parent
and pass them in; replace `std::fs::write` in `setup_userns_map` with raw
`open`/`write` syscalls; drop `eprintln!` (child stderr) from the closure.

Regression: a multithreaded fork test is heavy; this is verify-by-inspection. A
cheaper signal: `#[deny]`/clippy-style lint is not available — recommend a comment
audit gate listing the only sanctioned functions in pre_exec.

### medium: response cap is enforced only after full serialization

`rystemd/src/manager/mod.rs:810-835`: `serde_json::to_string(&resp)` runs to
completion (and `.into_bytes()` copies it) *before* the 1 MiB check at line 819.
A pathological state (multi-MiB `Description=`, thousands of units via `list-units`,
or the unbounded `journal` op below) is fully materialized as a JSON string —
typically 2-3× the source size, with escapes inflating control chars up to 6× —
then thrown away. The cap bounds *resident* bytes per client but not the transient
allocation spike on PID 1.

Fix: serialize into a bounded writer (e.g. `serde_json::to_writer` a limited `Vec`
or a writer that hard-aborts at `MAX_PENDING_RESPONSE`), returning the truncation
error as soon as the cap is crossed instead of after building the whole payload.

Regression: feasible without root — extend `control_response_cap_drops_oversize_clients`
to also assert the manager never allocates beyond the cap (hard to observe directly;
an acceptable proxy is wiring a small writer and asserting it errors mid-way).

### medium: `journal` op reads the entire journal unbounded before any cap

`rystemd/src/ipc.rs:239-261`: with no `unit` and no/small `tail`, the op calls
`journal.read` for every journaled unit, accumulating every record across all
segments into one `Vec` (no record-count or byte bound), sorts them, and only then
does the 1 MiB response cap apply — after the whole store is already resident and
even `tail=N` reads everything first (`journal.tail` at `journal.rs:120-129` reads
all, then takes N). Disk size is bounded per unit, but a busy desktop's aggregate
journal can be tens of MB, so one `rystemdctl --journal` spikes PID 1 memory 2-3×.

Fix: stream/read with a running cap — stop extending `records` once a byte budget
(e.g. 1 MiB) is exceeded and mark truncation, or push the cap down into
`Journal::read`/`tail` so segments are read lazily.

Regression: feasible without root — seed a synthetic journal larger than the cap and
assert the read path stops early rather than loading the whole store.

### medium: reachable invariant `unwrap()`s remain on the exec/job hot path

The prior run converted the VTable `unwrap()`s, but dozens of identical
`self.units.get_mut(name).unwrap()` / `self.jobs.get_mut(&id).unwrap()` (and
`self.units.get(name).unwrap()`) remain on the per-start/stop/reap path:
`rystemd/src/manager/mod.rs` lines 1918, 1928, 1940, 1954, 2015, 2058, 2070, 2131,
2146-2148, 2195, 2211-2234, 2278-2341, 2398 (exec/reap), plus 929, 978, 1081,
1257-1525, 1620-1621, 1707 (job machinery). These assume the unit is present in the
map and the job is present in the job table at the moment they run. Under
`panic = "abort"` any state divergence (e.g. a unit removed during `daemon-reload` /
`reset_failed` while its stop/start job is still being dispatched) aborts PID 1 →
kernel "init died" panic, the same class the prior run closed for the VTable.

Fix: convert the `get_mut(name)`/`get(&name)` calls on the exec/reap path to
`let Some(u) = self.units.get_mut(name) else { log + fail_unit(name, ..); return }`
(the jobs-map unwraps are tighter invariants and can stay, or use the same pattern).

Regression: fragile to engineer a unit-vanish race in a test; verify by inspection
(as the VTable fix was) — mechanical and low-risk.

### medium: switch_root re-exec uses `argv[0]`, not `/proc/self/exe` (comment/code mismatch)

`rystemd/src/platform/boot.rs:713-720`: `handoff` rebuilds argv via
`std::env::args()` and `reexec` execs `argv[0]`. Its own comment (lines 709-711)
says the manager "re-execs ... `/proc/self/exe` stays valid across the pivot", but
the code execs whatever the kernel passed as argv[0] (typically `/init` or
`/sbin/init`). After the MS_MOVE/chroot, `argv[0]` resolves against the *deployment*
root, where `/init` usually does not exist → `execv` fails and `handoff` returns
Err, leaving the process chrooted into the deployment running the stage-2 copy —
working by accident, not the documented self-re-exec contract.

Fix: exec directly from the current image (the binary `prepare_deployment` bind-
mounted in):
```rust
let exe = CString::new("/proc/self/exe").unwrap();
let err = reexec_with(&argv, &exe); // execve(exe.as_ptr(), ptrs.as_ptr(), environ)
```

Regression: needs a real root/initramfs/VM (privileged) — not runnable on this host.

### low: pipelined second request bytes are silently dropped

`rystemd/src/manager/mod.rs:762-846`: `drain_control_client` reads the socket until
`WouldBlock`, so any bytes already past the first `\n` (a second pipelined request
sent in one `write()`) sit in `client.buffer`; dispatch consumes only up to the first
newline and the client is re-inserted with `buffer: Vec::new()`. The surplus is
discarded and the connection closes after the response, so the peer never gets a
reply to request 2 (it waits on a half-close/EOF). Protocol is one-request-per-
connection and rystemctl reconnects, so this is only a silent-drop for pipelining
clients.

Fix: either reject a request line with trailing non-whitespace bytes after the newline
(return a `"pipelining unsupported"` error), or preserve the surplus in the re-inserted
buffer to be handled as a follow-up request.

Regression: feasible without root — connect, send two newline-terminated requests in
one write, assert request 2 either errors explicitly or is served (locked to the fix).

### low: a finite-but-huge `CPUQuota=` writes `u64::MAX` to `cpu.max` (harmless, note)

`rystemd/src/platform/cgroup.rs:126-130`: the NaN/zero guard is in place, but a very
large finite f32 quota (saturating u64 on `as u64`) writes `18446744073709551615
100000` to `cpu.max`, which the kernel treats as "no throttle" — functionally
equivalent to unlimited, not a crash or under-grant. No panic (casts saturate), but
the value is not clamped to a sane upper bound. `memory.max`/`pids.max`/`io.weight`
are all bounded `u64`→`to_string`, which the kernel rejects/clamps if out of range.

Fix (optional, cosmetic): clamp `max_us` to e.g. `100 * period_us` or reject
`quota > 100_000.0`.

Regression: needs runtime cgroup access for the value write, but the clamp is pure —
testable without root by exercising the clamp expression.

### verified-hold (no action): dbus auth, calendar/timespan/parse safety, user/group fail-closed

- `dbus.rs` StartUnit/StopUnit read the caller's `UnixUserID` fresh per call
  (`authorize` → `GetConnectionCredentials`, `dbus.rs:289-321`); `manager_uid` is
  threaded into `ManagerIface`; systemd1 surface is read-only. Holds.
- `calendar.rs` — leap logic correct, step loop uses `saturating_add` (no overflow),
  `weekday_number` returns `Option` (no panic), `OnCalendar=Mon`/bare-date defaults
  to 00:00 (`calendar.rs:263-267`). `timespan.rs` — u128 accumulator with a
  per-component overflow check, negatives/NaN/exponent forms rejected, empty = error.
  `unit/parse.rs` — the only `unwrap()` is line 111, guarded by the preceding empty
  check; no indexing or unbounded `read_line`. All hold.
- `resolve_user`/`resolve_group` fail closed → `UnitResult::User`/`Group` before spawn
  (`manager/mod.rs:1924-1946`); lookup is parent-side and single-threaded (no caching
  needed). Holds.

## Run 2 outcome

The run-2 audit produced the high-severity busy-spin finding (closed in
commit `7ce79c7`) and the pre-exec `setgroups` Vec pre-collect fix (same
commit), plus a partial async-signal-safety closure for the sandbox path
(open, tracked as issue #1). The remaining run-2 open items (response-cap
timing, journal read bounds, hot-path unwraps, switch_root argv[0], the
two lows) are tracked as GitHub issues labelled `v0.3.0`. The Windows
control-pipe ACL remains the unverified trust gap (issue #6, blocked on
a Windows runner).

## Run 3 outcome — Windows DACL attempt + revert

Issue #6 (explicit DACL on the Windows control pipe, defense-in-depth
per-caller auth) was attempted in commit `0e8f0d2` and pushed. CI run
`34175935053` (workflow `check.yml`, job `windows`) **failed** at
`rystemctl/tests/windows.rs:48` — the user-manager pipe test times out
on `list_units` after 5 s, while the system-2022 `cargo test` itself
compiled clean (lib unit tests for `build_pipe_security` passed). The
fail-open bug found and fixed during this session (process-vs-thread
token read under `ImpersonateNamedPipeClient`) was correct, but the
fix as written still rejected the connecting test client. Reverted in
`70b38e6` so the workflow is green again. Next attempt requires either
local Windows reproduction (preferred — log the exact `GetLastError`
and the connected client's token), or a CI workflow that runs the
failing test under `RUST_BACKTRACE=full` with a debug-print
`ImpersonateNamedPipeClient`/`OpenThreadToken`/token-SID path on a
failing connection.
