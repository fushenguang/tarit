#!/usr/bin/env bash
# ci/overlay-reuse-gate.sh — validate that a cold boot can reuse an existing
# private overlay disk (the vm-stop-delete-split "start a stopped VM" and
# "restart_policy=always" primitive, task group 4).
#
# Boots a real guest, writes a marker file to its disk via exec, releases the
# overlay from vmm-core's owned-scratch auto-cleanup tracking (exactly what
# taritd's supervisor.rs boot_vm now does right after create() succeeds),
# stops the VM (leaving its CoW overlay file on disk), then boots an entirely
# fresh `vmm serve` process pointed at the SAME overlay path, and verifies the
# marker survived.
#
# IMPORTANT finding this gate exists to pin down: `create()`'s real RPC
# dispatch target is `Controller::create_live`, which tracks every volume's
# freshly created overlay as an "owned scratch file"
# (VmTransientFiles::owned_overlays) and DELETES it when the VM instance is
# later dropped (on `stop`) - completely independent of taritd's own
# teardown_vm. Without the release_scratch call below, this gate FAILS (the
# marker never survives) even after the vm-stop-delete-split orchestration
# fix, because vmm-core deletes the overlay itself.
#
# Run on the KVM host:
#   sudo bash ci/overlay-reuse-gate.sh
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/agent-rootfs.ext4}"
OVERLAY="${OVERLAY:-/tmp/overlay-reuse-gate.cow}"
SOCK=/tmp/vmm-overlay-reuse.sock
LOG1=/tmp/vmm-overlay-reuse-round1.log
LOG2=/tmp/vmm-overlay-reuse-round2.log
rm -f "$SOCK" "$LOG1" "$LOG2" "$OVERLAY"

api() {
  python3 - "$SOCK" "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(40)
try:
    s.connect(sys.argv[1]); b = sys.argv[2].encode()
    s.sendall(struct.pack('>I', len(b)) + b)
    rl = struct.unpack('>I', s.recv(4))[0]; d = b''
    while len(d) < rl:
        c = s.recv(rl - len(d))
        if not c: break
        d += c
    sys.stdout.write(d.decode())
except Exception as e:
    print('{"error":"%s"}' % e)
finally:
    s.close()
PY
}

# Build a `release_scratch` request for $1 using its real (device, inode,
# birth time), mirroring OwnedArtifact::capture + client.release_scratch in
# supervisor.rs. ScratchIdentity's equality includes birth time, and Python's
# os.stat() does not expose it on Linux at all - GNU `stat --format='%.9W'`
# does (nanosecond-precision birth time), so shell out to it instead.
release_req_for() {
  read -r dev ino birth < <(stat --format='%d %i %.9W' "$1")
  birth_secs="${birth%.*}"
  birth_frac="${birth#*.}"
  birth_nanos=$((10#$birth_frac))
  python3 - "$1" "$dev" "$ino" "$birth_secs" "$birth_nanos" <<'PY'
import sys, json
path, dev, ino, secs, nanos = sys.argv[1:6]
print(json.dumps({
    "op": "release_scratch",
    "path": path,
    "identity": {
        "device": int(dev),
        "inode": int(ino),
        "created_secs": int(secs),
        "created_nanos": int(nanos),
    },
}))
PY
}

CMDLINE="console=ttyS0 reboot=k panic=-1 pci=off i8042.noaux random.trust_cpu=on nowatchdog nokaslr root=/dev/vda rw"
CREATE_CFG='{"op":"create","config":{"kernel":{"path":"'"$KERNEL"'","cmdline":"'"$CMDLINE"'","initramfs":null},"memory":{"size_mib":512},"vcpus":{"count":1},"volumes":[{"path":"'"$ROOTFS"'","read_only":true,"overlay":"'"$OVERLAY"'"}],"net":[]}}'

echo "=== round 1: fresh boot, overlay does not exist yet ==="
RUST_LOG=info "$VMM" serve --socket "$SOCK" >"$LOG1" 2>&1 &
SERVE_PID=$!
sleep 1
api "$CREATE_CFG"
echo ""

echo "=== release the overlay from vmm-core's owned-scratch tracking ==="
api "$(release_req_for "$OVERLAY")"
echo ""
echo "  (waiting 25s for systemd + vmm-agent to start)"
sleep 25

echo "=== write a persistence marker ==="
api '{"op":"exec","command":"sh -c \"echo overlay-reuse-marker > /root/persisted.txt && sync\"","timeout_ms":20000}'
echo ""

echo "=== stop round 1 (leave overlay on disk, per vm-stop-delete-split semantics) ==="
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
wait "$SERVE_PID" 2>/dev/null
sleep 1

echo "overlay after round 1: $(ls -la "$OVERLAY" 2>&1)"
echo ""

echo "=== round 2: entirely fresh vmm serve process, SAME overlay path ==="
# This alone proves the reuse capability (task group 4): round 2's create()
# must ADOPT the existing overlay (OwnedOverlayGuard::create's adopt-before-
# create-new fallback) rather than fail with EEXIST. Round 2 does not release
# the overlay again here - reconstructing its ScratchIdentity (which includes
# nanosecond birth time) from a shell script is fragile; the authoritative,
# release-included round-trip is `cargo test
# released_overlay_survives_stop_and_is_reusable_by_a_fresh_vmm_process` in
# orch/crates/taritd/src/supervisor.rs, which uses the real
# OwnedArtifact::capture code taritd itself calls.
RUST_LOG=info "$VMM" serve --socket "$SOCK" >"$LOG2" 2>&1 &
SERVE_PID2=$!
sleep 1
api "$CREATE_CFG"
echo ""
echo "  (waiting 25s for systemd + vmm-agent to start)"
sleep 25

echo "=== read back the marker (this is the actual assertion) ==="
RESULT=$(api '{"op":"exec","command":"cat /root/persisted.txt","timeout_ms":20000}')
echo "$RESULT"
echo ""
if echo "$RESULT" | grep -q "overlay-reuse-marker"; then
  echo "PASS: marker written in round 1 survived a fresh vmm process reusing the same overlay in round 2"
else
  echo "FAIL: marker not found — overlay reuse across a fresh vmm process did not preserve prior writes"
fi

echo ""
echo "=== stop round 2 ==="
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID2" 2>/dev/null || true
wait "$SERVE_PID2" 2>/dev/null

echo ""
echo "=== round 2 server log (errors/panics) ==="
grep -niE "panic|error|could not|fault|BUG:" "$LOG2" | tail -30
