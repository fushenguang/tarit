# CLAUDE.md

Guidance for AI assistants working in this repository.

## What this repo is

An independent fork of [instavm/tarit](https://github.com/instavm/tarit) — see
README.md's "About this fork" for the full story. Two cargo workspaces
developed together:

- `vmm/` — the VMM (rust-vmm-based microVM monitor). Its own workspace; can be
  built/tested standalone.
- `orch/` — `taritd`, the fleet orchestrator. Its own workspace.
- `proto/` — `tarit-proto`, the shared UDS wire-protocol crate (KVM-free),
  depended on by both.

## Fork policy

This fork does not track upstream mergeability — it moves faster and diverges
further as real production needs drive it. It does **not** ignore upstream
though: scan `instavm/tarit`'s commit log periodically (monthly is a reasonable
cadence) for security-relevant fixes in the rust-vmm/KVM/virtio layer, and port
those manually. Full upstream sync is not the goal; security parity is.

## Process: when to use OpenSpec

This repo has OpenSpec set up (`openspec/`, `.claude/commands/opsx/*`), but it
is **not mandatory for every change**. Use the full
explore → change → tasks → verify → archive flow only when a change touches:

- **VM lifecycle** (create / stop / suspend / restore / delete semantics)
- **Data integrity** (snapshot/restore correctness, guest memory handling)
- **A cross-crate contract** (`proto/`, or the `vmm` ↔ `orch` wire protocol)

Everything else — a bug fix contained to one function, a CI script tweak, a
doc update — is engineer judgment. Don't force process onto small, contained
changes; the `openspec/changes/archive/` directory is itself the change log,
so there's no separate changelog file to keep in sync.

## Testing philosophy

**`vmm/` and `orch/` (Rust, the security-critical system layer) require
test-driven work**: when fixing a bug, write a test that reproduces the real
failure first, then fix it. This is not a coverage-chasing exercise — a test
should target an actual failure mode (e.g., a memory round-trip that corrupts
above a specific VM size), not padding for a percentage. See
`vmm/crates/vmm-core/src/controller.rs`'s `split_guest_memory_round_trips_*`
test for the pattern: it exists because that exact bug shipped to production
once.

This TDD requirement is scoped to `vmm/`/`orch/` only — it does not extend to
any application-layer consumer of this project.

## Building and testing

```sh
# vmm/ workspace
cd vmm
cargo check --features boot -p vmm-core     # boot feature gates most of the VM lifecycle code
cargo test --features boot -p vmm-core      # unit tests
sudo bash ci/livesnap-gate.sh               # full KVM gate: live-snapshot, full/diff restore, suspend/resume
                                             # needs KERNEL/BASE_ROOTFS built via `make guest` first

# orch/ workspace
cd orch
cargo test
```

Most correctness-critical code lives behind `#[cfg(all(target_arch = "x86_64",
target_os = "linux", feature = "boot"))]` — always check with `--features
boot` (or `--features kvm` where that's the relevant flag), not the bare
default feature set, or you'll silently skip the real code paths.

## Where to look

- README.md — architecture, quickstart, full doc index.
- `vmm/docs/`, `orch/docs/` — the detailed reference docs README.md links to.
- `SECURITY.md` — the security model and host-side containment this fork
  still relies on.
