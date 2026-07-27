//! Guest memory backend built on `vm-memory::GuestMemoryMmap`.

use crate::dirty::{DirtyBitmap, SoftwareDirtyBitmap};
use std::sync::Arc;
use thiserror::Error;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend as _, GuestMemoryMmap};

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory region creation failed: {0}")]
    Region(String),
    #[error("guest memory assembly failed: {0}")]
    Assembly(String),
    #[error("out of bounds: addr=0x{0:x} size={1}")]
    OutOfBounds(u64, u64),
}

/// A guest physical address space backed by one or two mmap'd regions.
///
/// Below `MMIO_GAP_START` (see `vmm_loader::memmap`, not imported here to
/// keep this crate dependency-free — callers pass the gap explicitly), this
/// is a single contiguous region starting at GPA 0, same as always. For a
/// `size_bytes` that would otherwise reach into the fixed low device MMIO
/// window (`vmm-core::controller::build_devices` places devices there,
/// always below 4 GiB — ACPI's `Memory32Fixed` descriptor is 32-bit only, so
/// that placement can never move up to make room), the constructor instead
/// builds two regions: `[0, gap_start)` unchanged, and the remainder
/// relocated to start at `gap_end` (conventionally 4 GiB) — the same
/// "memory hole" relocation real x86 firmware performs so no requested RAM
/// is ever lost to the hole. `size_bytes` still reports the *total* guest
/// RAM (sum of both regions), matching what was requested.
///
/// A split `GuestMemory` has two entries in `inner` — callers that assume a
/// single flat region (e.g. anything using [`Self::as_ptr`] with the full
/// `size_bytes` as a length) must check [`Self::is_split`] first; see its
/// doc comment.
#[derive(Clone)]
pub struct GuestMemory {
    pub inner: Arc<GuestMemoryMmap>,
    pub size_bytes: u64,
    host_dirty: SoftwareDirtyBitmap,
}

impl GuestMemory {
    /// Build a single-region guest memory of `size_bytes` starting at GPA 0.
    /// Callers that must stay below any device MMIO window (i.e. anything
    /// booting a real x86_64 guest) should use [`Self::new_with_mmio_hole`]
    /// instead so large requests don't silently collide with device
    /// addresses.
    pub fn new(size_bytes: u64) -> Result<Self, MemoryError> {
        Self::new_with_flags(size_bytes, false, None)
    }

    /// Build guest memory with huge pages (2 MiB). Reduces TLB misses during
    /// the page-fault storm of UFFD lazy restore (E2B reports 5x faster
    /// first read). Requires `vm.nr_hugepages > 0` on the host.
    pub fn new_hugepages(size_bytes: u64) -> Result<Self, MemoryError> {
        Self::new_with_flags(size_bytes, true, None)
    }

    /// Build guest memory of `size_bytes`, relocating any portion that would
    /// land at or above `gap_start` to instead start at `gap_end` — so the
    /// caller's fixed device MMIO window `[gap_start, gap_end)` is always
    /// free of guest RAM regardless of `size_bytes`. For `size_bytes <=
    /// gap_start` this is identical to [`Self::new`] (single region, GPA 0).
    pub fn new_with_mmio_hole(
        size_bytes: u64,
        gap_start: u64,
        gap_end: u64,
    ) -> Result<Self, MemoryError> {
        Self::new_with_flags(size_bytes, false, Some((gap_start, gap_end)))
    }

    fn new_with_flags(
        size_bytes: u64,
        huge_pages: bool,
        mmio_hole: Option<(u64, u64)>,
    ) -> Result<Self, MemoryError> {
        if size_bytes == 0 || !size_bytes.is_multiple_of(4096) {
            return Err(MemoryError::Region(format!(
                "size must be a non-zero multiple of 4096, got {size_bytes}"
            )));
        }
        // For huge pages, round up to 2 MiB boundary.
        let actual_size = if huge_pages {
            let hp_size = 2 * 1024 * 1024u64;
            if !size_bytes.is_multiple_of(hp_size) {
                ((size_bytes / hp_size) + 1) * hp_size
            } else {
                size_bytes
            }
        } else {
            size_bytes
        };

        let ranges = match mmio_hole {
            Some((gap_start, gap_end)) if actual_size > gap_start => {
                debug_assert!(gap_end > gap_start);
                vec![
                    (GuestAddress(0), gap_start as usize),
                    (GuestAddress(gap_end), (actual_size - gap_start) as usize),
                ]
            }
            _ => vec![(GuestAddress(0), actual_size as usize)],
        };
        let inner = GuestMemoryMmap::from_ranges(&ranges)
            .map_err(|e| MemoryError::Assembly(format!("guest memory: {e}")))?;

        Ok(Self {
            inner: Arc::new(inner),
            size_bytes: actual_size,
            host_dirty: SoftwareDirtyBitmap::new(),
        })
    }

    /// True if this `GuestMemory` was split across the MMIO hole (i.e. it
    /// has more than one mmap'd region). Callers that need a single
    /// contiguous view of all guest RAM (raw snapshot dump/restore) must
    /// check this first and reject or handle split memory explicitly —
    /// [`Self::as_ptr`] only ever points at the first region.
    pub fn is_split(&self) -> bool {
        self.inner.iter().count() > 1
    }

    /// Raw pointer to the start of the first mmap'd region.
    ///
    /// SAFETY contract for callers: the returned pointer is valid for reads
    /// and writes of `size_bytes` bytes **only when [`Self::is_split`] is
    /// false** — a split `GuestMemory`'s first region is smaller than
    /// `size_bytes` (the remainder lives in a second, non-adjacent mmap), so
    /// treating this pointer as the base of a `size_bytes`-long buffer would
    /// read/write past the first mapping. Used by the snapshot dumper, which
    /// must reject split memory rather than call this.
    pub fn as_ptr(&self) -> *const u8 {
        self.inner
            .iter()
            .next()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut())
    }

    /// Read `buf.len()` bytes from guest physical address `gpa`.
    /// Returns Err if the read is out of bounds.
    pub fn read_phys(&self, gpa: u64, buf: &mut [u8]) -> Result<(), MemoryError> {
        self.inner
            .read_slice(buf, GuestAddress(gpa))
            .map_err(|_| MemoryError::OutOfBounds(gpa, buf.len() as u64))
    }

    /// Write `buf` to guest physical address `gpa`.
    pub fn write_phys(&self, gpa: u64, buf: &[u8]) -> Result<(), MemoryError> {
        self.inner
            .write_slice(buf, GuestAddress(gpa))
            .map_err(|_| MemoryError::OutOfBounds(gpa, buf.len() as u64))?;
        self.mark_host_dirty(gpa, buf.len() as u64);
        Ok(())
    }

    pub fn host_dirty_tracker(&self) -> SoftwareDirtyBitmap {
        self.host_dirty.clone()
    }

    pub fn mark_host_dirty(&self, gpa: u64, len: u64) {
        self.host_dirty.mark_range(gpa, len);
    }

    pub fn drain_host_dirty(&self) -> DirtyBitmap {
        self.host_dirty.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_small_guest_memory() {
        let m = GuestMemory::new(4096).expect("4K");
        assert_eq!(m.size_bytes, 4096);
    }

    #[test]
    fn rejects_unaligned_size() {
        assert!(GuestMemory::new(100).is_err());
        assert!(GuestMemory::new(0).is_err());
    }

    #[test]
    fn builds_typical_256mib() {
        let m = GuestMemory::new(256 * 1024 * 1024).expect("256MiB");
        assert_eq!(m.size_bytes, 256 * 1024 * 1024);
    }

    #[test]
    fn write_phys_marks_host_dirty_pages() {
        let m = GuestMemory::new(3 * 4096).expect("12K");
        m.write_phys(0x0fff, &[1, 2]).unwrap();

        let dirty = m.drain_host_dirty();
        assert!(dirty.contains(0));
        assert!(dirty.contains(0x1000));
        assert_eq!(dirty.len(), 2);
        assert!(m.drain_host_dirty().is_empty());
    }

    const GAP_START: u64 = 0x_D000_0000; // 3.25 GiB
    const GAP_END: u64 = 0x1_0000_0000; // 4 GiB

    #[test]
    fn mmio_hole_below_gap_stays_single_region() {
        let m = GuestMemory::new_with_mmio_hole(256 * 1024 * 1024, GAP_START, GAP_END)
            .expect("256MiB below gap");
        assert!(!m.is_split());
        assert_eq!(m.size_bytes, 256 * 1024 * 1024);
    }

    #[test]
    fn mmio_hole_above_gap_splits_and_relocates() {
        let requested = GAP_START + 512 * 1024 * 1024; // 3.25 GiB + 512 MiB
        let m = GuestMemory::new_with_mmio_hole(requested, GAP_START, GAP_END).expect("above gap");
        assert!(m.is_split());
        // Total reported RAM must equal exactly what was requested — no
        // capacity lost to the hole, it's relocated, not discarded.
        assert_eq!(m.size_bytes, requested);
        // Both halves must be reachable at their expected guest addresses.
        m.write_phys(GAP_START - 4096, &[0xAA]).unwrap(); // last byte below the gap
        m.write_phys(GAP_END, &[0xBB]).unwrap(); // first byte above the gap
        let mut buf = [0u8; 1];
        m.read_phys(GAP_START - 4096, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAA);
        m.read_phys(GAP_END, &mut buf).unwrap();
        assert_eq!(buf[0], 0xBB);
        // The gap itself must not be backed by any region.
        assert!(m.read_phys(GAP_START, &mut buf).is_err());
    }

    #[test]
    fn mmio_hole_exactly_at_gap_start_stays_single_region() {
        // size_bytes == gap_start exactly: RAM ends right where the device
        // window begins, no overlap and no need to split.
        let m = GuestMemory::new_with_mmio_hole(GAP_START, GAP_START, GAP_END).expect("at gap");
        assert!(!m.is_split());
        assert_eq!(m.size_bytes, GAP_START);
    }
}
