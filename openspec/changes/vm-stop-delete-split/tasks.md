## 1. Baseline: pin the current bug with failing tests

- [ ] 1.1 Write a failing integration test (`orch`) that boots a VM, calls the current `DELETE` endpoint, and asserts the overlay file is deleted today - this becomes the permanent regression test for "DELETE without force must NOT delete the disk" once its assertion is flipped in section 3.
- [ ] 1.2 Write a failing test (`orch`) for `readopt_one`'s net-recovery / scheduler-recovery / lock-poison failure branches asserting the overlay currently gets deleted in each of those paths too.

## 2. Store & types foundation (`tarit-store`, `tarit-types`)

- [ ] 2.1 Add `restart_policy` column to the `vms` table via `ensure_column` (`TEXT NOT NULL DEFAULT 'no'`).
- [ ] 2.2 Add `RestartPolicy` enum (`No` / `Always`) to `tarit-types`; add `VmRecord.restart_policy` and `CreateVmRequest.restart_policy` (defaulted to `No`, same `#[serde(default = ...)]` pattern as `memory_mib`/`vcpus`).
- [ ] 2.3 Wire `restart_policy` through store row (de)serialization (`row_to_vm_record` and insert/upsert paths).
- [ ] 2.4 Update `VmStatus::Stopped`'s doc comment to the new "disk retained" meaning; grep for and fix any existing comments/tests that assume the old "Stopped == disk already deleted" meaning.

## 3. `supervisor.rs`: split `stop_vm` / `purge_vm`

- [ ] 3.1 Rename current `teardown_vm` body into `purge_vm` (behavior unchanged); introduce a new `stop_vm` that does everything `purge_vm` does except the overlay unlink and `store.delete_vm` call.
- [ ] 3.2 Repoint all six existing `teardown_vm` call sites - the HTTP-delete path, `shutdown_sweep`/`stop_all`, the three `readopt_one` failure branches, and `quarantine_readopted_runtime` - at the new non-destructive `stop_vm`.
- [ ] 3.3 Re-run tasks 1.1/1.2's tests; confirm they now fail in the opposite direction (overlay unexpectedly retained under the old assertion); flip their assertions to the new expected behavior and confirm green.

## 4. `vmm-core`: cold boot from an existing overlay

- [ ] 4.1 Write a failing test in `vmm-core` asserting `create()` today rejects an existing overlay path (`O_CREAT|O_EXCL` failure) - pins the current gap before adding the new mode.
- [ ] 4.2 Add `overlay_mode: CreateNew | Adopt` to the per-volume create config; wire `Adopt` to reuse `OwnedScratchFile::adopt_private` (the same primitive `prepare_restore_overlay` already uses for `restore()`).
- [ ] 4.3 Reuse `reject_golden_overlay_target`'s check so `Adopt` mode refuses a path currently registered as a golden artifact.
- [ ] 4.4 Add tests: adopting a valid existing overlay succeeds and boots correctly; adopting a missing path fails clearly (no silent fallback to `CreateNew`); adopting a golden-registered path is rejected.

## 5. `taritd` API: stop / start / delete?force

- [ ] 5.1 Add `POST /v1/vms/{id}/stop` route + handler calling the new `stop_vm`.
- [ ] 5.2 Add `POST /v1/vms/{id}/start` route + handler calling the new cold-boot-from-overlay primitive (task 4.2); fail with a clear not-found/conflict error if the overlay file is missing.
- [ ] 5.3 Add a `force` query param to `DELETE /v1/vms/{id}`; absent/`false` routes to `stop_vm`, `force=true` routes to `purge_vm`.
- [ ] 5.4 `ops.rs`: add `purge_local`/`start_local`; adjust `stop_local`/`stop_all_local` to call the non-destructive path.
- [ ] 5.5 Integration tests covering all three endpoints end to end: stop retains disk, start reboots from the retained disk, delete without force retains, delete with force purges and subsequently 404s.

## 6. Startup auto-restart sweep

- [ ] 6.1 Write a failing test: seed the store with a `Stopped` + `restart_policy=always` VM, simulate a taritd startup pass, and assert it is currently NOT auto-started - pins the gap.
- [ ] 6.2 Implement `restart_policy_sweep`, invoked from `main.rs` right after `readopt_running_vms` completes and post-readopt status corrections are applied.
- [ ] 6.3 Route the sweep's boot attempts through the existing scheduler reservation path so it's capacity-bounded like any other boot; on capacity shortfall leave the VM `Stopped` (not `Error`), on genuine boot failure mark `Error`.
- [ ] 6.4 Tests: successful auto-restart; one VM's failure doesn't block others or crash-loop taritd startup; capacity-insufficient case leaves the VM `Stopped` rather than `Error`.

## 7. Docs & wrap-up

- [ ] 7.1 Update `orch/docs/API.md` (new `stop`/`start` endpoints, `DELETE` semantics change, `restart_policy` field).
- [ ] 7.2 Update `orch/docs/CONFIGURATION.md` if any new env/config knobs are introduced along the way.
- [ ] 7.3 Add a `CHANGELOG.md` entry explicitly calling out the **BREAKING** `DELETE` semantics change.
- [ ] 7.4 Run the full `cargo test` suite for both the `vmm/` and `orch/` workspaces; run the relevant `vmm/ci/*.sh` gate scripts under real KVM on the server per this repo's verify guidance.
- [ ] 7.5 Live-verify on `dev.fujia.site` (`192.168.31.50`): create -> stop -> start (disk retained, VM comes back) -> delete without force (still retained) -> delete with `force=true` (actually gone) -> simulate a taritd restart with `restart_policy=always` to confirm auto-restart. Snapshot via `tarit-snapshot.sh` before the taritd restart step, per this project's standing safety discipline.
