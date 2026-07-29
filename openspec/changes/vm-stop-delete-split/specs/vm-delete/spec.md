## Purpose

Makes destroying a VM's private overlay disk require an explicit, unambiguous force intent, so that a plain delete call — like a plain stop — can never destroy user data by accident.

## ADDED Requirements

### Requirement: Delete without force is non-destructive
`DELETE /v1/vms/{id}` without a `force` parameter SHALL stop the VM (if running) and SHALL retain its overlay disk file and store record — equivalent to the vm-stop capability's stop behavior.

#### Scenario: Deleting without force
- **WHEN** a client calls `DELETE /v1/vms/{id}` without `force=true`
- **THEN** the VM's process (if any) is stopped, its overlay disk file still exists afterward, and its store record is still retrievable via `GET /v1/vms/{id}`

### Requirement: Force delete purges disk and record
`DELETE /v1/vms/{id}?force=true` SHALL stop the VM (if running), delete its overlay disk file, and remove its store record.

#### Scenario: Force deleting a stopped VM
- **WHEN** a client calls `DELETE /v1/vms/{id}?force=true` on a Stopped VM
- **THEN** the VM's overlay disk file is deleted, its store record is removed, and a subsequent `GET /v1/vms/{id}` returns not-found

### Requirement: Force delete never removes golden artifacts
Force delete SHALL NOT remove an overlay disk file that is registered as a golden artifact owned by the warm-pool registry, even when `force=true` is passed.

#### Scenario: Force-deleting a golden source VM
- **WHEN** a client calls `DELETE /v1/vms/{id}?force=true` on a VM whose overlay is registered as a golden artifact
- **THEN** the golden overlay file is not deleted, though the VM's own process and record are handled per existing golden-registry rules
