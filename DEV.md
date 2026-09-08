# Development

## Workspace

Cargo workspace with three crates:

| Crate | Role |
| --- | --- |
| `rystemd` | Library, manager, daemon, PID 1 entry point |
| `rystemctl` | `systemctl`-compatible CLI and native client |
| `rystemd-tui` | Terminal client using the control API |

Core modules:

| Module | Responsibility |
| --- | --- |
| `unit` | Unit-file parser and typed configuration |
| `manager` | Unit state, jobs, supervision, timers, activation |
| `manager::deps` | Dependency expansion and ordering |
| `manager::ops` | Shared typed operations and status records |
| `platform` | Unix and Windows process, IPC, signal, mount, and service seams |
| `ipc` / `client` | JSON-line control protocol |
| `control` | In-process and remote control APIs |
| `daemon` | Manager process entry point |
| `enable` / `paths` | Install links and filesystem layout |

## Local build

```sh
cargo build --workspace
cargo build -p rystemd --features boot
cargo build -p rystemctl
cargo build -p rystemd-tui
```

The default manager build is suitable for a normal user or service-manager
process. The `boot` feature adds PID 1 filesystem setup, early boot handling,
getty setup, and power control.

## Test organization

| Location | Coverage |
| --- | --- |
| `rystemd/src/**` unit tests | Parser, calendar, manager, sandbox, and protocol logic |
| `rystemd/tests/e2e.rs` | Lifecycle and control API tests against scratch paths |
| `rystemd/tests/seccomp.rs` | Linux seccomp behavior |
| `rystemd/tests/privileged.rs` | Root-only namespace and device cases |
| `rystemd/tests/dbus.rs` | Linux D-Bus surface when enabled |
| `rystemctl/tests/cli.rs` | Real CLI and daemon subprocess round trips |
| `scripts/ns-boot-test.sh` | Rootless namespace boot smoke test |
| `scripts/vm-test.sh` | Automated QEMU boot test |
| `scripts/live-vm.sh` | Interactive PID 1 VM |
| `scripts/run-systemd-test.sh` | Alpine compatibility scenario runner |
| `.github/workflows/ci.yml` | Push/PR CI: test, privileged, Windows lint+test+build (no publish) |

Standard gates:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace
cargo test --workspace --all-features

git diff --check
```

Shell and workflow checks:

```sh
bash -n scripts/*.sh release.sh
python3 -c 'import yaml; yaml.safe_load(open(".github/workflows/release.yml"))'
```

The privileged tests self-skip without the required privileges. A skip is not
runtime coverage. Run the relevant test under a real root environment when
validating namespace or device isolation.

## Live environments

Interactive live VM:

```sh
scripts/live-vm.sh
```

The script builds a reduced initramfs, fetches the pinned kernel when no kernel
path is supplied, and boots rystemd as PID 1. It does not modify the host boot
image. `DEMO.md` documents the guest commands.

Alpine compatibility scenario:

```sh
scripts/run-systemd-test.sh TEST-03-JOBS
```

The runner builds pinned Alpine and musl inputs, boots a static manager and
client, runs the adapted upstream scenario, and requires this guest marker:

```text
RYNTEST_DONE rc=0
```

A non-zero marker is a compatibility failure. Alpine userland and harness
adapters do not establish Fedora systemd, D-Bus, journald, or `systemd-run`
parity.

## Cross-target checks

Installed targets can be type-checked without a linker:

```sh
cargo check --workspace --locked --target x86_64-unknown-linux-musl
cargo check --workspace --locked --target aarch64-unknown-linux-musl
cargo check --workspace --locked --target x86_64-pc-windows-msvc
```

Musl release builds use the pinned Alpine sysroot scripts. GNU Linux builds
serve Fedora and Debian package lanes. Windows runtime tests require Windows.

### Writing and testing Windows code (CI, not local emulation)

A Linux dev host cannot link or run the native `x86_64-pc-windows-msvc`
target (it needs Windows' `link.exe` / a running Windows). Windows
verification is done in the **real-Windows CI pipeline** (`.github/workflows/ci.yml`),
which runs on every untagged branch push and PR: it lints
(`cargo clippy --workspace --all-targets -D warnings`), runs
`cargo test --workspace --locked`, and builds the MSVC target — without
packaging or publishing (that is reserved for the tag-gated `release.yml`).

So the workflow for platform/windows changes (e.g. issue #6, the control-pipe
security descriptor): push an untagged commit, then read the `windows` job's
test results to confirm whether the change works or how it fails. Local QEMU
emulation for Windows was tried and abandoned — the unattended install could
not complete reliably — so CI is the sole source of Windows verification.

## Release procedure

Release automation is tag-driven. The tag is the version source for build
metadata and release assets.

Pre-release gates:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features
scripts/run-systemd-test.sh TEST-03-JOBS
```

Create a local release tag:

```sh
./release.sh patch
```

The script runs `cargo check`, updates member crate versions, commits pending
changes when approved by the release operator, and creates an annotated tag.
Without `--push`, no remote branch or tag is pushed.

Trigger the release workflow explicitly:

```sh
./release.sh patch --push
```

The workflow builds and packages:

| Target | Release output |
| --- | --- |
| `x86_64-unknown-linux-gnu` | tar.gz, DEB, RPM |
| `aarch64-unknown-linux-gnu` | tar.gz, DEB, RPM |
| `x86_64-unknown-linux-musl` | Alpine APK |
| `aarch64-unknown-linux-musl` | Alpine APK |
| `x86_64-pc-windows-msvc` | ZIP, MSI, tests |

Alpine APK generation is handled by `scripts/package-alpine.sh`. GNU package
generation is handled by `scripts/package-linux.sh` and `packaging/`.
