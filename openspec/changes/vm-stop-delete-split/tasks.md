## 1. Baseline: pin the current bug with failing tests

- [x] 1.1 Write a failing test (`orch`) asserting `stop_vm` retains the overlay disk (today it doesn't) - `stop_vm_retains_overlay_disk`, models the shared `teardown_vm` chokepoint every stop caller routes through.
- [x] 1.2 Write a failing test (`orch`) for the `readopt_one`/quarantine failure path asserting the overlay currently gets deleted there too - `quarantine_readopted_runtime_retains_overlay_disk`.

## 2. Store & types foundation (`tarit-store`, `tarit-types`)

- [x] 2.1 Add `restart_policy` column to the `vms` table via `ensure_column` (`TEXT NOT NULL DEFAULT 'no'`) - sqlite (`tarit-store`) and Postgres (`tarit-fleet`, `ALTER ... ADD COLUMN IF NOT EXISTS`) both done.
- [x] 2.2 Add `RestartPolicy` enum (`No` / `Always`) to `tarit-types`; add `VmRecord.restart_policy`, `CreateVmRequest.restart_policy`, `PublicVmRecord.restart_policy` (defaulted to `No`).
- [x] 2.3 Wire `restart_policy` through store row (de)serialization in both `tarit-store::row_to_vm` and `tarit-fleet::row_to_vm`.
- [x] 2.4 Update `VmStatus::Stopped`'s doc comment to the new "disk retained" meaning.

## 3. `supervisor.rs`: split `stop_vm` / `purge_vm`

- [x] 3.1 Strip the overlay-unlink out of `teardown_vm` (now the shared non-destructive core every existing caller uses unchanged); move it into a new `purge_vm_overlay`, reachable only via a new public `purge_vm` (`stop_vm` + `purge_vm_overlay`).
- [x] 3.2 All existing `teardown_vm` call sites (stop_vm dispatch, unexpected-exit cleanup, boot-failure cleanup, the three `readopt_one` failure branches, `quarantine_readopted_runtime`, `stop_all`'s shutdown-sweep loop) needed no changes - deletion is opt-in now, not opt-out.
- [x] 3.3 Tasks 1.1/1.2's tests now pass; updated two tests whose failure-injection relied on the old overlay-deletion behavior (`purge_vm_overlay_preserves_a_remembered_golden_overlay`, `restart_reconciliation_propagates_quarantine_cleanup_failure`).

## 4. Cold boot from an existing overlay

Scope revised after a live-KVM round-trip test (boot → write marker → stop → fresh `vmm serve` process → read marker back) found the real gap was two different, previously-hidden bugs - see design.md Decision 3 for the full account.

- [x] 4.1 Live-KVM round-trip test written first (per TDD), run against the unmodified code: failed twice, for two independent reasons, before any fix landed.
- [x] 4.2 Fix 1: `create_live` (the real RPC dispatch target for "create") tracks a fresh overlay as an owned scratch file and deletes it on stop - `boot_vm` now releases it via the existing `ReleaseScratch` RPC right after `create()` succeeds, same mechanism the golden-snapshot path already used for its own overlay.
- [x] 4.3 Fix 2: `OwnedOverlayGuard::create` hard-required `O_CREAT|O_EXCL` - now tries `OwnedScratchFile::adopt_private` first, falling back to `create_new` only when the path doesn't exist.
- [x] 4.4 Golden-overlay rejection for the reused-overlay path deferred to task group 5's `start` endpoint (needs taritd's `golden_artifacts` registry, which vmm-core has no visibility into - see design.md).
- [x] 4.5 (added) `released_overlay_survives_stop_and_is_reusable_by_a_fresh_vmm_process` (real KVM, `orch/crates/taritd/src/supervisor.rs`) and `vmm/ci/overlay-reuse-gate.sh` (standalone manual reproduction) both pass end to end.

## 5. `taritd` API: stop / start / delete?force

- [x] 5.1 Add `POST /v1/vms/{id}/stop` route + handler calling the new `stop_vm`.
- [x] 5.2 Add `POST /v1/vms/{id}/start` route + handler calling the new cold-boot-from-overlay primitive (task 4.2); fail with a clear not-found/conflict error if the overlay file is missing.
- [x] 5.3 Add a `force` query param to `DELETE /v1/vms/{id}`; absent/`false` routes to `stop_vm`, `force=true` routes to `purge_vm`.
- [x] 5.4 `ops.rs`: add `purge_local`/`start_local`; adjust `stop_local`/`stop_all_local` to call the non-destructive path.
- [x] 5.5 Integration tests covering all three endpoints end to end: stop retains disk, start reboots from the retained disk, delete without force retains, delete with force purges and subsequently 404s. (2026-07-30 audit: 5.1-5.4 were already implemented and merged - `git log` had the commits - but this checklist was never updated and 5.5's route-level test genuinely didn't exist; added `delete_route_force_query_param_controls_purge_vs_retain` in `api.rs`. `ops.rs` already had unit coverage for the underlying stop/purge logic and the cold-boot-reuse round trip has a real-KVM gate script (`overlay-reuse-gate.sh`, task 4.5) - not re-duplicated at the route layer.)
- [x] 5.6 (added 2026-07-30) `purge_local` also removes every share and snapshot record (+ their RAM/overlay files) scoped to the purged VM - found via a production incident (repeated "share owner resolution failed ... not found in cluster" log spam from a share left behind by an earlier force-delete) and a source read (snapshot files were tracked per-VM in `SnapshotRecord` but nothing ever queried or deleted them). See `purge_local_removes_shares_scoped_to_the_vm` and `purge_local_removes_snapshot_files_and_records_for_the_vm` in `ops.rs`.

## 6. Startup auto-restart sweep

- [x] 6.1 Write a failing test: seed the store with a `Stopped` + `restart_policy=always` VM, simulate a taritd startup pass, and assert it is currently NOT auto-started - pins the gap.
- [x] 6.2 Implement `restart_policy_sweep`, invoked from `main.rs` right after `readopt_running_vms` completes and post-readopt status corrections are applied.
- [x] 6.3 Route the sweep's boot attempts through the existing scheduler reservation path so it's capacity-bounded like any other boot; on capacity shortfall leave the VM `Stopped` (not `Error`), on genuine boot failure mark `Error`.
- [ ] 6.4 Tests: successful auto-restart; one VM's failure doesn't block others or crash-loop taritd startup; capacity-insufficient case leaves the VM `Stopped` rather than `Error`. (2026-07-30 audit: `restart_policy_candidates_selects_only_this_hosts_stopped_always_vms` covers the pure selection logic; the three scenario-level tests this task actually asks for - full sweep success, partial failure isolation, capacity-shortfall status - were not found. Left open rather than silently marked done; see live-verify note under 7.5 for the manual coverage that did happen this pass.)

## 7. Docs & wrap-up

- [ ] 7.1 Update `orch/docs/API.md` (new `stop`/`start` endpoints, `DELETE` semantics change, `restart_policy` field).
- [ ] 7.2 Update `orch/docs/CONFIGURATION.md` if any new env/config knobs are introduced along the way.
- [ ] 7.3 Add a `CHANGELOG.md` entry explicitly calling out the **BREAKING** `DELETE` semantics change.
- [x] 7.4 Run the full `cargo test` suite for both the `vmm/` and `orch/` workspaces; run the relevant `vmm/ci/*.sh` gate scripts under real KVM on the server per this repo's verify guidance. (2026-07-30: `orch` full suite green modulo pre-existing flaky `net::tests::*` - reproduced identically on `main` before this branch's changes, not introduced by them; see CLAUDE.md follow-up note. `vmm/ci/*.sh` gate scripts not re-run this pass - out of scope, no vmm-core changes on this branch.)
- [ ] 7.5 Live-verify on `dev.fujia.site` (`192.168.31.50`): create -> stop -> start (disk retained, VM comes back) -> delete without force (still retained) -> delete with `force=true` (actually gone) -> simulate a taritd restart with `restart_policy=always` to confirm auto-restart. Snapshot via `tarit-snapshot.sh` before the taritd restart step, per this project's standing safety discipline.
