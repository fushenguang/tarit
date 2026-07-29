## Purpose

Lets operators, or taritd itself after a host reboot, bring a stopped VM's guest process back up using its existing overlay disk without a full re-create, giving VMs Docker-`--restart=always`-like recovery.

## ADDED Requirements

### Requirement: Cold-start from an existing overlay
The system SHALL support booting a VM's guest process directly from its existing, previously-used private overlay disk file, without requiring a RAM/state snapshot.

#### Scenario: Start a stopped VM
- **WHEN** a client calls `POST /v1/vms/{id}/start` on a Stopped VM whose overlay disk file is still present
- **THEN** a new guest process boots using that exact overlay disk file as its writable layer, and the VM's status becomes Running

### Requirement: Start requires a retained disk
`POST /v1/vms/{id}/start` SHALL fail with a clear not-found/conflict error if the VM's overlay disk file is missing (for example, because it was previously force-deleted).

#### Scenario: Start after force-delete
- **WHEN** a client calls `POST /v1/vms/{id}/start` on a VM whose overlay disk was previously removed via force delete
- **THEN** the request fails with a not-found/conflict error and no new guest process is started

### Requirement: restart_policy field
`VmRecord` SHALL support a `restart_policy` of `no` (default) or `always`, settable at VM creation time and visible in subsequent reads.

#### Scenario: Creating a VM with restart_policy=always
- **WHEN** a client calls `POST /v1/vms` with `restart_policy=always`
- **THEN** the created VM's record persists `restart_policy=always` and it is returned in subsequent `GET /v1/vms/{id}` responses

### Requirement: Automatic restart after host reboot
After taritd starts up and completes `readopt_running_vms`, the system SHALL automatically cold-start every locally-owned VM whose status is Stopped, whose `restart_policy` is `always`, and whose overlay disk file is still present.

#### Scenario: taritd restarts after a host reboot
- **WHEN** taritd starts, finishes `readopt_running_vms`, and finds a Stopped VM with `restart_policy=always` and an intact overlay disk
- **THEN** that VM is automatically cold-started using its existing overlay, without any client request

### Requirement: Automatic restart failure is bounded and observable
If an automatic restart attempt fails during taritd startup (for example, a corrupt overlay or resource exhaustion), the system SHALL record the failure on that VM's record and SHALL NOT let the failure block startup of other VMs or crash-loop the taritd process.

#### Scenario: Automatic restart fails for one VM among many
- **WHEN** a `restart_policy=always` VM's automatic cold-start fails during taritd startup while other `restart_policy=always` VMs succeed
- **THEN** the failed VM's status reflects the failure, taritd startup completes, and the other VMs are unaffected
