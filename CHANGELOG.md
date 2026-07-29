# Changelog

All notable changes to Tarit are documented in this file. The `proto/`,
`vmm/`, and `orch/` workspaces are versioned together.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, minor
versions may contain breaking changes.

## [Unreleased]

### Added

- `POST /v1/vms/{id}/stop`: stop a VM's guest process while retaining its
  private overlay disk and record, so it can be started again later.
- `POST /v1/vms/{id}/start`: cold-start a `stopped` VM, reusing its retained
  overlay disk as its writable layer (no RAM/register state replay).
- `restart_policy` (`no` | `always`) on `POST /v1/vms` and `VmRecord`: an
  `always`-policy VM still `stopped` when taritd itself restarts is
  automatically cold-started, Docker-`--restart=always`-style.

### Changed

- **BREAKING**: `DELETE /v1/vms/{id}` no longer deletes the VM's overlay
  disk by default. Without `?force=true` it now behaves exactly like the new
  `POST .../stop` (disk and record retained); only `?force=true` actually
  purges the disk and removes the record. Deleting data now always requires
  this explicit, deliberate intent - previously it happened on every
  `DELETE` unconditionally.
- SSH gateway client authentication no longer accepts RSA public keys.
- Guest setup now downloads a reproducibly built Linux 6.12 LTS `vmlinux`,
  verifies its pinned SHA-256, and falls back to a checksum-pinned source build.
  Kernel releases are attested and gated by the full c8i promotion suite and a
  minimum three-hour lifecycle soak.
- `vmm kernel install` downloads the pinned kernel with HTTPS and SHA-256
  verification. Interactive `run` and `create` commands can install it when no
  kernel path is supplied.

## [0.1.0] - 2026-07-03

Initial public release of Tarit, a microVM platform for secure, fast,
ephemeral sandboxes, licensed under AGPL-3.0-or-later.

### Added

- `vmm/` 0.1.0: the Tarit VMM, a minimal rust-vmm-based microVM monitor for
  x86_64 Linux with KVM. One process per microVM, MMIO virtio device model
  (block, net, vsock, serial), snapshot/restore with diff snapshots, live
  snapshots, suspend/resume, seccomp and jailer sandboxing, nftables-based
  egress filtering, and vsock exec/PTY into the guest.
- `orch/` 0.1.0: `taritd`, a multi-node orchestrator and control plane with
  an HTTP API, placement, warm pools, networking, snapshots, an SSH/PTY
  gateway, per-key usage stats, and an audit trail backed by PostgreSQL.
- `proto/` 0.1.0: `tarit-proto`, the shared dependency-light crate holding
  the Unix-domain-socket wire protocol between the VMM and any orchestrator.
- Guest tooling: `make guest` builds a guest kernel and pulls an Ubuntu
  rootfs; a guest agent handles exec and PTY inside the VM.
- Project docs (README, per-workspace docs, benchmarks), CI covering fmt,
  clippy, check, tests, and KVM type-checks across all three workspaces, and
  security policy files.
