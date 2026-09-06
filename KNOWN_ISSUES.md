# Known issues

This file records current compatibility boundaries: which systemd features are
not implemented, which platform or distribution lanes are out of scope, and
which surface areas are intentionally limited. Release history belongs in
git and changelog files. Open engineering issues — bugs, trust gaps,
required fixes — are tracked as GitHub issues labelled `v0.3.0`:
<https://github.com/rystemd/rystemd/issues?q=is%3Aissue+is%3Aopen+label%3Av0.3.0>.
This file and the issue list cover different surfaces and are intentionally
not merged.

## Distribution and boot

- Alpine APKs are unsigned. Installation requires
  `--allow-untrusted --force-non-repository` until a signing key and repository
  trust path exist.
- Alpine APKs target musl systems. They are not packages for glibc-based
  distributions.
- PID 1 boot remains experimental. The live VM uses a reduced target graph and
  does not claim compatibility with a native Fedora `default.target` graph.
- Boot and live VM scripts are not part of the tag-triggered release CI gate.
- The Alpine scenario runner embeds static musl binaries in its initramfs. It
  separately validates APK installation, but does not yet boot an APK-installed
  root filesystem as an acceptance test.

## Job and transaction compatibility

- Native start and stop operations expose job IDs and completion status. Normal
  replacement and `replace-irreversibly` are implemented for the tested
  service-conflict path. Direct start with `ignore-dependencies` is supported.
- Other job modes remain incomplete. Missing areas include full transaction
  merge rules, cycle diagnostics, complete restart propagation, and a D-Bus job
  object model with `JobNew` and `JobRemoved` signals.
- `--wait` uses client polling. PID 1 remains event-driven, but the job result
  retention table has no bounded eviction policy.
- `daemon-reexec` is an explicit no-op. Runtime state transfer across a real
  process re-exec is not implemented.
- `--show-transaction` and `-T` are accepted and ignored. No transaction graph
  renderer exists.

## Control and journal surfaces

- The CLI covers common lifecycle, status, dependency, enablement, masking,
  journal, and observability operations. `edit`, `preset`, `link`, `revert`, and
  `cancel` remain unsupported.
- Several accepted compatibility flags have no effect, including selected
  verbosity, plain-output, and list-socket modifiers.
- The journal is a rystemd-specific plain on-disk store. It is not the journald
  binary format, does not implement the journald forwarding protocol, and is
  queried through `rystemctl journal` rather than native `journalctl`.
- D-Bus support is partial. Manager reads, unit objects, and selected activation
  paths exist. Full systemd1 control methods, job objects, job signals, and
  complete `PropertiesChanged` behavior remain absent.
- `systemd-run`, `varlinkctl`, and native `journalctl` protocol behavior are not
  provided by the Alpine compatibility image. Related upstream assertions are
  harness boundaries, not parity claims.

## Unit and service coverage

- `.slice`, `.scope`, and `.automount` units are not implemented.
- Runtime, state, cache, log, and configuration directory directives are not
  implemented.
- Watchdog enforcement, `NotifyAccess=`, PAM integration, supplementary groups,
  dynamic users, and several resource and identity directives are incomplete.
- Type `notify` supports `READY=1`; watchdog behavior is absent.
- Cgroup resource control covers a subset of cgroup v2 settings. Device policy
  and eBPF access control are absent.
- Service sandboxing covers the implemented mount, namespace, capability,
  seccomp, device, and address-family directives only. Dynamic users, kernel
  protection, IP address policy, and device allow lists remain incomplete.

## Platform coverage

- macOS is not a supported target and has no release artifact.
- Windows supports foreground and oneshot service subsets. POSIX account
  switching, graceful signal phases, D-Bus service activation, and inherited
  socket handles are incomplete.
- Linux identity integration through external glibc NSS modules is not a musl
  static-binary guarantee. LDAP, SSSD, NIS, locale, and related behavior can
  differ between release lanes.

## Test coverage

- No push or pull-request CI workflow exists. Release checks run on tag pushes.
- Live PID 1 runtime behavior, root-only sandbox paths, cgroup enforcement, and
  several Windows paths require dedicated environments.
- Parser fuzzing and calendar or timespan property testing are absent.
- The upstream `TEST-03-JOBS` script is a compatibility probe. Its Alpine
  adaptation includes explicit harness substitutions and does not establish
  full systemd integration-test compatibility.
- The current Alpine run passes the job-transaction and client-wait sections,
  then exits with `RYNTEST_DONE rc=1` at the unsupported `systemd-run` scope
  test. This is a harness boundary, not a remaining `unstoppable.service`
  transaction failure.
