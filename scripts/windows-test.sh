#!/usr/bin/env bash
# Windows build/test via a smolvm Windows sandbox (see GH issue #6).
#
# On a Linux dev host we cannot link or run the native x86_64-pc-windows-msvc
# target (needs Windows' link.exe / a running Windows). Instead:
#
#   - `windows-test.sh check-msvc` runs `cargo check --target
#     x86_64-pc-windows-msvc` on the Linux host (no linker — compiles and
#     type-checks the cfg(windows) code paths). This needs no VM and is fast.
#
#   - `windows-test.sh test` boots a real Windows 11 sandbox via SmolVM and
#     runs `cargo test --target x86_64-pc-windows-msvc` *inside* it. This is
#     the path that exercises the actual Windows runtime (the control pipe, the
#     SCM dispatcher, unit tests in rystemd/tests/windows.rs). It requires a
#     Windows 11 qcow2 with OpenSSH + rustup + the MSVC toolchain preinstalled.
#
# The built Windows image is a large, transient artifact. It is cached OUTSIDE
# the repo under $SMOLVM_IMAGE_DIR (default ~/.smolvm/images/win11.qcow2) and
# is deletable; it is never committed. SmolVM boots it read-only and stacks a
# per-VM overlay, so the golden image is never modified.
#
# smolvm is NOT assumed present: this script installs it (into a dedicated
# venv under $HOME/.smolvm) and its system prerequisites on first use. It
# needs sudo for the dnf/apt prereqs only; smolvm itself installs unprivileged.
set -uo pipefail

# ---- defaults -------------------------------------------------------------
SMOLVM_DIR="${SMOLVM_DIR:-$HOME/.smolvm}"
SMOLVM_VENV="$SMOLVM_DIR/venv"
SMOLVM_IMAGE_DIR="${SMOLVM_IMAGE_DIR:-$SMOLVM_DIR/images}"
DEFAULT_IMAGE="$SMOLVM_IMAGE_DIR/win11.qcow2"
SSH_USER="${SMOLVM_SSH_USER:-Administrator}"
SSH_PASSWORD="${SMOLVM_SSH_PASSWORD:-}"
MEM_MB="${SMOLVM_MEM_MB:-4096}"

usage() {
  cat <<'EOF'
usage: windows-test.sh COMMAND [options]

Commands
  check-msvc       cargo check --target x86_64-pc-windows-msvc (Linux host, no VM)
  test             boot a smolvm Windows sandbox and run the native Windows tests
  build-image      build a Windows 11 qcow2 from ISOs (smolvm windows build-image)
  doctor           verify smolvm + prerequisites are usable (no VM)

Options
  --image PATH        Windows qcow2 (default ~/.smolvm/images/win11.qcow2)
  --user NAME         Windows account (default Administrator)
  --password PASS     Windows account password
  --iso PATH          Windows 11 ISO (build-image only)
  --virtio-win PATH   virtio-win driver ISO (build-image only)
  --mem MB            VM memory in MiB (default 4096)

check-msvc runs entirely on the Linux host and needs no VM.
test / build-image need a Windows image; build-image needs --iso and --virtio-win.

The Windows 11 ISO is a legal download from Microsoft and must be obtained by
the user (this script does not fetch it): https://www.microsoft.com/en-
ca/software-download/windows11 — on this host an image is cached OUTSIDE the
repo under ~/.smolvm/images/ (transient, deletable). The virtio-win driver
ISO (~877 MB) is fetched by this script from fedorapeople.org if not present.
EOF
}

# ---- self-install smolvm + prerequisites ----------------------------------
ensure_smolvm() {
  if ! "$SMOLVM_VENV/bin/smolvm" doctor >/dev/null 2>&1; then
    echo "==> installing smolvm into $SMOLVM_VENV (not assumed present)" >&2
    python3 -m venv "$SMOLVM_VENV"
    "$SMOLVM_VENV/bin/pip" install --upgrade --quiet smolvm
  fi
  # System prerequisites (qemu-img for overlays, swtpm for the Win11 TPM,
  # xorriso only for build-image). Non-fatal if unavailable: doctor reports it.
  if ! command -v qemu-img >/dev/null 2>&1 || ! command -v swtpm >/dev/null 2>&1; then
    echo "==> installing system prerequisites (qemu-img swtpm xorriso)" >&2
    if command -v dnf >/dev/null 2>&1; then sudo dnf install -y qemu-img swtpm xorriso >&2
    elif command -v apt-get >/dev/null 2>&1; then sudo apt-get install -y qemu-system-common swtpm xorriso >&2
    else echo "warning: install qemu-img + swtpm manually (see smolvm doctor)" >&2; fi
  fi
}

doctor() {
  ensure_smolvm
  "$SMOLVM_VENV/bin/smolvm" doctor
}

# ---- check-msvc (Linux host, no VM) ---------------------------------------
cmd_check_msvc() {
  ensure_smolvm
  echo "==> cargo check --workspace --target x86_64-pc-windows-msvc (Linux host)" >&2
  exec cargo check --workspace --locked --target x86_64-pc-windows-msvc
}

# ---- image handling ---------------------------------------------------------
resolve_image() {
  local img="${1:-$DEFAULT_IMAGE}"
  if [ -f "$img" ]; then
    echo "$img"
    return 0
  fi
  echo "error: Windows image not found at $img" >&2
  echo "  build one first: windows-test.sh build-image --iso <Win11.iso> --virtio-win <virtio-win.iso>" >&2
  echo "  (cached outside the repo as a transient, deletable artifact)" >&2
  return 1
}

cmd_build_image() {
  ensure_smolvm
  : "${ISO:?build-image requires --iso PATH}"
  : "${VIRTIO_WIN:?build-image requires --virtio-win PATH}"
  local out="${IMAGE:-$DEFAULT_IMAGE}"
  mkdir -p "$(dirname "$out")"
  if [ -f "$out" ]; then
    echo "==> image already exists: $out (refusing to overwrite; --output is guarded by smolvm)" >&2
    exit 1
  fi
  echo "==> building Windows image (15-30 min unattended) -> $out" >&2
  exec "$SMOLVM_VENV/bin/smolvm" windows build-image \
    --iso "$ISO" --virtio-win-iso "$VIRTIO_WIN" \
    --username "$SSH_USER" --password "$SSH_PASSWORD" \
    --output "$out"
}

# ---- boot a Windows sandbox and run native tests ----------------------------
cmd_test() {
  ensure_smolvm
  local img
  img=$(resolve_image "${IMAGE:-$DEFAULT_IMAGE}") || exit 1
  [ -n "$SSH_PASSWORD" ] || {
    echo "error: test needs --password (Windows account for OpenSSH)" >&2
    exit 1
  }
  echo "==> booting Windows sandbox on $img (memory ${MEM_MB} MiB)" >&2
  # Upload the working tree, then run the native cargo test inside Windows.
  # The repo is uploaded to C:\Users\<user>\rystemd; the image must already
  # have rustup + the MSVC toolchain installed (bake it into the qcow2).
  exec "$SMOLVM_VENV/bin/python" - "$img" "$SSH_USER" "$SSH_PASSWORD" "$MEM_MB" <<'PYEOF'
import os, sys
from smolvm import SmolVM

img, user, password, mem = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
repo = os.path.abspath(os.getcwd())

with SmolVM(os="windows", image=img, ssh_user=user, ssh_password=password, memory=mem) as vm:
    vm.wait_for_ssh()
    # Upload the source tree to the Windows guest.
    dest = f"C:\\Users\\{user}\\rystemd"
    for root, _dirs, files in os.walk(repo):
        # Skip build artifacts and VCS metadata — huge and not needed.
        skip = ("/target/", "/.git/", "/.idea/", "/book/")
        if any(s in root.replace(repo, "") + "/" for s in skip):
            continue
        for f in files:
            local = os.path.join(root, f)
            rel = os.path.relpath(local, repo)
            remote = dest + "\\" + rel.replace("/", "\\")
            vm.upload_file(local, remote)
    print(f"==> source uploaded to {dest}")
    r = vm.run(f"cd {dest} && cargo test --workspace --target x86_64-pc-windows-msvc")
    print(r.stdout)
    print(r.stderr, file=sys.stderr)
    sys.exit(r.exit_code)
PYEOF
}

# ---- arg parse --------------------------------------------------------------
CMD=""
IMAGE=""
ISO=""
VIRTIO_WIN=""
while [ $# -gt 0 ]; do
  case "$1" in
    check-msvc|test|build-image|doctor) CMD="$1"; shift ;;
    --image) IMAGE="$2"; shift 2 ;;
    --user) SSH_USER="$2"; shift 2 ;;
    --password) SSH_PASSWORD="$2"; shift 2 ;;
    --iso) ISO="$2"; shift 2 ;;
    --virtio-win) VIRTIO_WIN="$2"; shift 2 ;;
    --mem) MEM_MB="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage; exit 2 ;;
  esac
done

case "$CMD" in
  check-msvc) cmd_check_msvc ;;
  test) cmd_test ;;
  build-image) cmd_build_image ;;
  doctor) doctor ;;
  *) usage; exit 2 ;;
esac
