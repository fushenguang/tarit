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

## Notable past bugs (read before touching VM stop/delete/create)

The `vm-stop-delete-split` change (see `openspec/changes/vm-stop-delete-split/`
and its `design.md` for the full account) found three real bugs while making
"stop" retain a VM's overlay disk by default. Each one independently defeated
the fix, and none were obvious from reading the orchestration code alone —
they only surfaced by writing a real create → stop → fresh-process → reboot
round-trip test on live KVM and a real HTTP client:

- **`Controller::create_live` (`vmm-core/src/controller.rs`), not the unused
  `Controller::create`, is the real dispatch target for the `create` RPC.**
  The two functions coexist and are easy to conflate. `create_live` tracks
  every freshly created overlay as an "owned scratch file" and **deletes it
  when the VM instance later drops** — a second, independent overlay-deletion
  path living entirely inside vmm-core, invisible to and unaffected by
  anything the orchestrator (`taritd`) does. If you change stop/delete
  semantics at the `orch/` layer, check whether the disk is *also* tracked
  for auto-cleanup at this layer — the fix was calling the existing
  `ReleaseScratch` RPC right after `create()` succeeds (the same mechanism
  the golden-snapshot capture path already used for its own overlay).
- **`OwnedOverlayGuard::create` hard-required `O_CREAT|O_EXCL`** for every
  volume overlay, rejecting any path that already existed. This was the
  actual blocker for "cold-boot reusing an existing overlay" — fixed by
  trying `OwnedScratchFile::adopt_private` first (the same primitive
  `restore()`'s `prepare_restore_overlay` already used), falling back to
  `create_new` only when the path is new.
- **`cluster::resolve_owner`'s single-host fallback excluded `VmStatus::Stopped`
  from "does this VM exist locally"** (`orch/crates/taritd/src/cluster.rs`).
  Every handler that reads or acts on a VM (`GET`, `/status`, `/start`, ...)
  calls `resolve_owner` first, so this silently 404'd every one of them
  against a stopped VM — directly contradicting this repo's own `API.md`,
  which documented that a stopped VM's record stays gettable. This predates
  `vm-stop-delete-split`: it was presumably written back when `Stopped`
  implicitly meant "already deleted, a dying husk", which was true before
  this change and is exactly the assumption this change overturns. If a
  future change gives another status new "this is still a real, useful
  record" semantics, grep `resolve_owner` for the same class of stale
  exclusion.

Separately: `orch/crates/taritd/src/ops.rs`'s `test_state_with_durable_writer()`
test helper returns `(AppState, Receiver<StoreWrite>)`. Any test path that
reaches real SQLite persistence (`persist_stopped_record` and friends)
`.await`s an ack on that channel — if the test discards the receiver
(`let (state, _writes) = ...`) instead of spawning a consumer for it, `cargo
test` hangs silently instead of failing. See `warm_publication_failures_
retain_the_live_vm_and_retry_ownership` for the consumer-loop pattern to copy.

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
