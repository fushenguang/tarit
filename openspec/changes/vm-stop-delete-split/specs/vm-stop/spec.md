## Purpose

Lets operators halt a running VM's guest process without destroying its private overlay disk or store record, so that stopping a VM is always a recoverable, non-destructive operation by default.

## ADDED Requirements

### Requirement: Stop endpoint retains disk and record
The system SHALL provide `POST /v1/vms/{id}/stop` that halts the VM's guest process, releases its network allocation and cgroup, and leaves the VM's private overlay disk file and store record intact.

#### Scenario: Stopping a running VM
- **WHEN** a client calls `POST /v1/vms/{id}/stop` on a Running VM
- **THEN** the VM process is terminated, the VM's status becomes Stopped, and its overlay disk file still exists on disk afterward

### Requirement: Stopped status implies recoverable state
`VmStatus::Stopped` SHALL mean the VM's guest process is not running but its private overlay disk and store record are retained and available for a future start.

#### Scenario: Listing a stopped VM
- **WHEN** a client calls `GET /v1/vms/{id}` for a VM that was stopped (not force-deleted)
- **THEN** the response shows status Stopped and the VM's overlay disk file is confirmed present on the host filesystem

### Requirement: Host shutdown does not destroy VM disks
The `shutdown_sweep` triggered before host/process shutdown SHALL stop all locally running VMs without deleting their overlay disks or store records.

#### Scenario: Host shutdown sweep
- **WHEN** taritd receives SIGTERM/SIGINT and `reap_on_shutdown` is enabled
- **THEN** every locally running VM transitions to Stopped with its overlay disk retained, not deleted

### Requirement: Readopt failure paths do not destroy disks
When taritd fails to fully re-adopt a previously running VM after its own restart (network allocation recovery failure, scheduler reservation failure, running-map lock poisoning, or post-adoption quarantine), the system SHALL retain that VM's overlay disk and store record rather than deleting them.

#### Scenario: Readopt fails to recover network allocation
- **WHEN** taritd restarts and `readopt_one` cannot recover a previously running VM's network allocation
- **THEN** the VM's process is stopped/cleaned up but its overlay disk file is not deleted, and its store record is retained (not removed)

#### Scenario: Post-adoption quarantine
- **WHEN** a re-adopted VM's observed runtime state does not match Running/Paused/Suspended and is quarantined
- **THEN** the VM's overlay disk file is not deleted as part of quarantine handling
