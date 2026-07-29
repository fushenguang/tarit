## Context

See `proposal.md` - Why/What Changes for motivation. Facts that shape the approach here:

- `stop_vm()` (`orch/crates/taritd/src/supervisor.rs:3183`) is the only entry point that reaches `teardown_vm()` (`:3739`), which unconditionally deletes the VM's overlay file (`{socket_dir}/overlays/{uuid}.cow`) unless it's a registered golden artifact. Six call sites reach it today: `DELETE` API, `shutdown_sweep`, three branches inside `readopt_one` (net/scheduler/lock-poison recovery failure), and `quarantine_readopted_runtime`.
- There is no dedicated "stop" HTTP verb today - only `pause`/`suspend`/`resume`/`snapshot` exist alongside `create`/`get`/`list`/`delete`.
- `VmStatus::Stopped` (`orch/crates/tarit-types/src/lib.rs:13-20`) is already a durable, retained store row (not deleted) - it just currently always coincides with "overlay already deleted."
- `store.delete_vm` (`orch/crates/tarit-store/src/lib.rs:633`) already does a real `DELETE FROM vms`; today it's only reachable from two narrow create-rollback paths, never from the normal stop/delete flow.
- Schema changes use an existing additive-only helper, `ensure_column(conn, table, column, definition)` (`tarit-store/src/lib.rs:1032-1051`), which checks `PRAGMA table_info` and runs `ALTER TABLE ... ADD COLUMN` if missing. There is no separate migrations directory/tool in this codebase.
- `vmm-core`'s `restore()` path already has the low-level primitive for adopting an existing overlay file: `prepare_restore_overlay` (`vmm/crates/vmm-core/src/controller.rs:2049`) calls `OwnedScratchFile::adopt_private(target)` when the target path already exists, and rejects the adoption if the target is a golden artifact via `reject_golden_overlay_target`. This is only reachable through `restore()`, which mandatorily requires a RAM `snapshot_path`.
- `CreateVmRequest` (`orch/crates/tarit-types/src/lib.rs:241`) and `VmSpawnConfig` (`orch/crates/taritd/src/supervisor.rs:113`) are the two structs that would carry a new `restart_policy` field end to end.

## Goals / Non-Goals

**Goals:**
- Make the disk-deleting path unreachable except through one explicit, force-flagged call.
- Give VMs a real "stopped but resumable via a cold boot" state, backed by a new vmm-core primitive.
- Let a VM opt into automatic recovery after a host reboot, bounded by existing scheduler capacity checks.

**Non-Goals** (beyond what `proposal.md` already excludes):
- No change to golden-registry/warm-pool clone mechanics beyond making the force-delete path respect the existing golden-artifact check (unchanged behavior, just re-verified in the new code path).
- No change to how network allocations are represented; `start`/auto-restart request a fresh allocation through the existing scheduler path rather than trying to preserve the VM's previous IP (see Open Questions).
- No feature flag / gradual rollout mechanism - this ships as one atomic change (see Migration Plan for why that's judged safe here).

## Decisions

### 1. Split `teardown_vm` into `stop_vm` (non-destructive) and `purge_vm` (destructive)
`stop_vm` keeps today's process-kill/network-release/cgroup-cleanup logic but **drops** the overlay-unlink and never calls `store.delete_vm`. `purge_vm` calls `stop_vm` first (idempotent if already stopped), then does what `teardown_vm` does today: unlink the overlay (unless golden-owned) and remove the store row.

All six existing `teardown_vm` call sites are repointed at `stop_vm`; only the new `DELETE ...?force=true` handler calls `purge_vm`.

*Alternative considered*: keep one `teardown_vm` and thread a `delete_disk: bool` through all six call sites. Rejected - a future call site could default that bool to `true` (or a merge conflict could flip it) and silently reopen the data-loss bug. Two distinctly-named functions make the destructive path something you have to deliberately reach for.

### 2. Redefine `VmStatus::Stopped` in place; no new enum variant
`Stopped` keeps its name but its meaning becomes "process not running, disk/record retained" (Docker `exited`-state parity). A force-deleted VM's row is removed entirely via `store.delete_vm`, not left in some new terminal status.

*Alternative considered*: add `VmStatus::Halted` as a new variant, leave `Stopped` meaning "gone." Rejected - every current internal reader of `Stopped` already treats it as "row retained," so no real compatibility is preserved; this would just double the number of "not running" states API clients need to branch on for no behavioral gain.

### 3. New vmm-core primitive: cold boot from an existing overlay
Extend the existing `create()` RPC's per-volume config with an `overlay_mode: CreateNew | Adopt` choice (mirrors the naming of `restore()`'s existing `RestoreOverlay` enum), rather than adding a whole new `ApiRequest` variant. When `Adopt` is selected, the volume-open code reuses `OwnedScratchFile::adopt_private` (the same primitive `prepare_restore_overlay` already uses) instead of `create_new`'s `O_CREAT|O_EXCL`, and reuses `reject_golden_overlay_target`'s check so a golden image can't be started-over directly.

The key difference from `restore()` + `Seeded` overlay: this path pairs an adopted overlay with an **ordinary cold boot** (fresh kernel boot, no RAM/register state replay), where `restore()` pairs an adopted overlay with replayed RAM/device state from a snapshot. No snapshot file is involved anywhere in this path.

*Alternative considered*: a new `ApiRequest::CreateFromOverlay` variant. Rejected - keeps a single `create` request type and reuses the same `CreateNew | Adopt` vocabulary `restore()` already established for the same underlying choice, instead of growing the RPC surface with a near-duplicate request shape.

### 4. `restart_policy` storage
`vms.restart_policy TEXT NOT NULL DEFAULT 'no'`, added via `ensure_column` (same pattern as every other additive column in this table - no new migration mechanism needed). `VmRecord.restart_policy: RestartPolicy` (`enum RestartPolicy { No, Always }`, serde snake_case). `CreateVmRequest.restart_policy: Option<RestartPolicy>`, defaulted the same way `memory_mib`/`vcpus` already default via `#[serde(default = ...)]`.

### 5. Startup auto-restart sweep
New `restart_policy_sweep`, named to mirror the existing `shutdown_sweep`, invoked from `main.rs` right after `readopt_running_vms` completes and post-readopt status corrections are applied - so it only ever sees VMs genuinely left `Stopped`, not ones readopt already reclaimed as `Running`. It queries `store` for this host's `Stopped` + `restart_policy = Always` rows, checks the overlay file still exists per VM, and calls the new cold-boot-from-overlay primitive through the **same scheduler reservation path** normal `create` uses (`scheduler.reserve_existing`/equivalent) - so a host that rebooted with many `always` VMs is capacity-bounded exactly like any other boot storm, not a special-cased bypass.

A VM whose auto-restart attempt fails (missing/corrupt overlay, resource exhaustion) has its failure recorded and is **not** retried again within that same taritd startup; if capacity is the blocker it's simply left `Stopped` (not `Error`) so it can be started manually or picked up if capacity frees up before the next reboot. Only genuine boot failures (bad overlay, etc.) move it to `Error`.

### 6. DELETE API force parameter
`DELETE /v1/vms/{id}` gains `force: Option<bool>` (query param). Absent/`false` → routes to `ops::stop_local` (kept name, now backed by `stop_vm`). `force=true` → routes to a new `ops::purge_local`, backed by `purge_vm`.

## Risks / Trade-offs

- **[Risk]** BREAKING change to DELETE's default behavior could surprise any future caller assuming today's semantics. → **Mitigation**: no known external consumer today (Huntaway hasn't integrated this API yet, per proposal Impact); called out prominently in CHANGELOG; every DELETE call is already audit-logged (`audit::record`), giving a rollout observability trail.
- **[Risk]** Stopped-but-retained overlays accumulate disk usage indefinitely (auto-GC is an explicit Non-goal). → **Mitigation**: flagged as a known gap in `proposal.md`; the existing LVM snapshot safety net is a different concern (catastrophic loss, not steady-state disk growth) and doesn't cover this. Operator must monitor manually until a future GC change.
- **[Risk]** The new cold-start-from-overlay path is new, security-relevant surface (adopting an arbitrary existing file as a block device backing store). → **Mitigation**: TDD rule applies per repo policy; first implementation task must be a failing test modeled on the existing `adopt_private`/`reject_golden_overlay_target` coverage in the restore path.
- **[Risk]** `restore()`'s `Adopt` path and this new `create()`'s `Adopt` path could drift apart over time since they're two call sites. → **Mitigation**: both are designed to share the same `OwnedScratchFile::adopt_private` call and the same golden-artifact rejection check; only RAM/state handling differs between them.
- **[Risk]** Auto-restart sweep running at every taritd startup (not just after a real host reboot) means a plain `systemctl restart taritd` could also trigger unexpected cold-boots of `always`-policy VMs that were deliberately left `Stopped`. → **Mitigation**: this is arguably correct Docker-parity behavior (`--restart=always` containers do come back after `docker restart <daemon>` too), but worth confirming against actual usage expectations during implementation - flagged for the tasks/verify phase rather than blocking design here.

## Migration Plan

- Ships as one atomic release, no feature flag - given no known external API consumer yet, a flag would add complexity without a real staged-rollout audience.
- `ensure_column` migration is additive and runs automatically on next `taritd` start against the existing sqlite file; no manual DB step.
- Existing `Stopped` rows created under the old code (disk already deleted) are unaffected by the migration - they simply aren't startable under the new "start requires a retained disk" rule (§ vm-restart spec), which is the correct, expected outcome, not a regression.
- Rollback: revert to the previous release/tag. No down-migration needed - the added `restart_policy` column is inert if the reverted code doesn't read it.
- Server deployment on `dev.fujia.site` (`192.168.31.50`, `/opt/tarit`): sync to the merged commit, restart `taritd` under the existing `sudo /usr/local/sbin/tarit-snapshot.sh <reason>`-first discipline established for this fork.

## Open Questions

- Should `start`/auto-restart try to preserve the VM's previous network allocation (same tap/IP), or always request a fresh one through the normal scheduler path? This design defaults to "always fresh" (simplest, consistent with a real cold boot) since `TARIT_ENABLE_NET` itself is still shelved for production use on this host per current deployment status - worth revisiting once network-enabled VMs are actually running in production and it's clear whether anything (e.g. a pre-configured egress allowlist) is keyed by IP rather than VM id.
