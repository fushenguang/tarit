#!/usr/bin/env bash
# ci/autostart-gate.sh — validate that vmm-agent auto-launches a workload on
# every guest boot with NO host-side trigger, so a workload survives a VM
# stop/start cycle (task #6 part 1: guest-side auto-recovery primitive).
#
# Boots a real guest, installs a tiny script at /etc/vmm-agent/autostart via
# exec (this is what a bootstrapper like Huntaway would do once, right after
# first boot), releases the overlay from vmm-core's owned-scratch tracking
# (required for the overlay to survive `stop` at all — see
# overlay-reuse-gate.sh), stops the VM (leaving the overlay, and therefore the
# autostart script, on disk), then boots an entirely fresh `vmm serve` process
# pointed at the SAME overlay — an exact analogue of taritd's `start_local` on
# a Stopped VM. Round 2 does NOT exec anything before checking the result: if
# the autostart marker exists, vmm-agent ran the script on its own.
#
# Run on the KVM host:
#   sudo bash ci/autostart-gate.sh
set -uo pipefail

VMM="${VMM:-$HOME/tarit/vmm/target/release/vmm}"
KERNEL="${KERNEL:-/tmp/vmlinux.microvm}"
ROOTFS="${ROOTFS:-/tmp/agent-rootfs.ext4}"
OVERLAY="${OVERLAY:-/tmp/autostart-gate.cow}"
SOCK=/tmp/vmm-autostart-gate.sock
LOG1=/tmp/vmm-autostart-gate-round1.log
LOG2=/tmp/vmm-autostart-gate-round2.log
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
# birth time). See overlay-reuse-gate.sh for why this shells out to `stat`.
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

echo "=== round 1: fresh boot, install the autostart script (no autostart yet) ==="
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

echo "=== install /etc/vmm-agent/autostart (round 1 boots BEFORE this exists, so it must not have run yet) ==="
api '{"op":"exec","command":"mkdir -p /etc/vmm-agent && printf \"#!/bin/sh\\necho autostart-ran >> /root/autostart-ran.log\\n\" > /etc/vmm-agent/autostart && chmod 755 /etc/vmm-agent/autostart && sync","timeout_ms":20000}'
echo ""

echo "=== confirm nothing ran it yet (round 1's agent already started before the file existed) ==="
api '{"op":"exec","command":"cat /root/autostart-ran.log 2>&1 || echo NOFILE","timeout_ms":20000}'
echo ""

echo "=== stop round 1 (leave overlay + autostart script on disk) ==="
api '{"op":"stop"}'
sleep 1
kill "$SERVE_PID" 2>/dev/null || true
wait "$SERVE_PID" 2>/dev/null
sleep 1

echo "=== round 2: entirely fresh vmm serve process, SAME overlay — an exact analogue of start_local on a Stopped VM ==="
# No exec call before the check below: the guest reboots fully from scratch,
# vmm-agent starts fresh, and (if run_autostart_if_present exists and works)
# finds /etc/vmm-agent/autostart already on the retained overlay and runs it
# with no host-side trigger at all.
RUST_LOG=info "$VMM" serve --socket "$SOCK" >"$LOG2" 2>&1 &
SERVE_PID2=$!
sleep 1
api "$CREATE_CFG"
echo ""
echo "  (waiting 25s for systemd + vmm-agent to start and, hopefully, self-trigger autostart)"
sleep 25

echo "=== read back the marker (this is the actual assertion) ==="
RESULT=$(api '{"op":"exec","command":"cat /root/autostart-ran.log 2>&1 || echo NOFILE","timeout_ms":20000}')
echo "$RESULT"
echo ""
if echo "$RESULT" | grep -q "autostart-ran"; then
  echo "PASS: autostart script ran on its own during round 2's boot, with no host-side exec trigger"
else
  echo "FAIL: autostart marker not found — guest boot did not self-trigger /etc/vmm-agent/autostart"
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
