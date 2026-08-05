//! VMM / VM lifecycle state machine.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    /// Created but no vCPUs running yet.
    Created,
    /// vCPUs are running.
    Running,
    /// vCPUs paused (KVM_RUN interrupted); memory + devices still in place.
    Paused,
    /// vCPUs paused and resident guest RAM dropped; userfaultfd rehydrates on resume.
    Suspended,
    /// Shut down.
    Stopped,
}

/// A point-in-time health/info snapshot of the single VM, returned by the
/// `Status` API op. Cheap to build (no guest interaction) so an orchestrator
/// can poll it on a health-check interval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmStatus {
    pub state: VmState,
    /// Milliseconds since the VM was (re)created in this process.
    pub uptime_ms: u64,
    pub vcpus: u8,
    pub mem_mib: u64,
    pub volumes: usize,
    pub nets: usize,
    pub kernel: String,
    /// True while a vCPU thread exists and has not exited (running or paused).
    /// False before boot, after stop, or after an abnormal guest death.
    pub vcpu_alive: bool,
    /// Backing-store I/O failures summed across this VM's block devices.
    ///
    /// `vcpu_alive` answers "is the guest executing"; this answers "is its disk
    /// still there". They are independent: on 2026-08-04 a VM ran with healthy
    /// vCPUs for twelve minutes after its storage had gone, its filesystem
    /// read-only and its workload dead, while the orchestrator saw nothing
    /// wrong (fushenguang/tarit#28).
    ///
    /// Non-zero is sticky — it does not clear if later requests succeed. Once a
    /// guest filesystem has reacted to an I/O error by remounting itself
    /// read-only, the damage is done; treat any non-zero value as "this VM
    /// needs attention", not as a live gauge of current disk health.
    ///
    /// `#[serde(default)]` so a newer orchestrator can still read a status
    /// response from an older `vmm` that predates this field — the two are
    /// separate binaries and are not necessarily replaced in the same instant.
    #[serde(default)]
    pub blk_io_errors: u64,
}

impl VmState {
    /// Return true if this state can transition into running.
    #[must_use]
    pub fn can_run(self) -> bool {
        matches!(
            self,
            VmState::Created | VmState::Paused | VmState::Suspended
        )
    }

    /// Return true if this state can be paused.
    #[must_use]
    pub fn can_pause(self) -> bool {
        matches!(self, VmState::Running)
    }

    /// Return true if this state can be snapshotted.
    #[must_use]
    pub fn can_snapshot(self) -> bool {
        matches!(self, VmState::Paused | VmState::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_transitions() {
        assert!(VmState::Created.can_run());
        assert!(VmState::Running.can_pause());
        assert!(VmState::Paused.can_run());
        assert!(VmState::Suspended.can_run());
        assert!(VmState::Running.can_snapshot());
        assert!(!VmState::Stopped.can_run());
    }
}
