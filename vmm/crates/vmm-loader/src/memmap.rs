//! Guest physical memory map — the E820 layout for x86_64 microVMs.
//!
//! This module is pure arithmetic with no x86-only types, so it (and its
//! tests) compile and run on any host. The actual `boot_params` struct
//! wiring lives in [`crate::x86_64`] and is `target_arch = "x86_64"`-gated.
//!
//! Layout:
//!
//! | GPA range | Type | Purpose |
//! |---|---|---|
//! | `0x0`..`0xA_0000` | RAM | low memory (640 KiB) |
//! | `0xA_0000`..`0x10_0000` | reserved | legacy VGA + BIOS data (384 KiB) |
//! | `0x10_0000`..`MMIO_GAP_START` | RAM | high memory before the device window |
//! | `MMIO_GAP_START`..`MMIO_GAP_END` | reserved | device MMIO window (virtio-mmio) |
//! | `MMIO_GAP_END`..`mem_end` | RAM | any RAM beyond the window, relocated here |
//!
//! `vmm-core::controller::build_devices` places every virtio-mmio device
//! inside `[MMIO_GAP_START, MMIO_GAP_END)` and nowhere else — that address
//! is fixed, not derived from `mem_size_bytes`, because ACPI's
//! `Memory32Fixed` resource descriptor (used to tell the guest where each
//! device lives) is a 32-bit field and can't address anything at or past
//! 4 GiB (`MMIO_GAP_END`). So instead of moving the device window for large
//! VMs, guest RAM that would otherwise reach into the window is relocated
//! to resume at `MMIO_GAP_END` — the same "memory hole" trick real x86
//! firmware performs above the low 4 GiB. No requested RAM is lost, it just
//! isn't contiguous once `mem_size_bytes > MMIO_GAP_START`.
//!
//! `vmm-memory-backend::GuestMemory::new_with_mmio_hole` (called with these
//! same two constants from `vmm-core::controller`) does the matching split
//! of the actual `KVM_SET_USER_MEMORY_REGION` backing — the E820 map here
//! and the real memory layout there must always agree, which is why both
//! read from this one module.

// --- x86_64 boot / E820 constants (from kernel Documentation/x86/boot.txt) --

/// `boot_flag` magic — 0xaa55.
pub const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
/// `header` magic — `HdrS` (0x5372_6448).
pub const KERNEL_HDR_MAGIC: u32 = 0x5372_6448;
/// `type_of_loader` for an unregistered bootloader.
pub const KERNEL_LOADER_OTHER: u8 = 0xff;
/// `kernel_alignment` for a relocatable kernel.
pub const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;
/// Start of the EBDA (Extended Bios Data Area).
pub const EBDA_START: u64 = 0x0009_fc00;
/// E820 memory type: usable RAM.
pub const E820_RAM: u32 = 1;
/// E820 memory type: reserved.
pub const E820_RESERVED: u32 = 2;

// --- Guest physical address layout (matches rust-vmm's vmm-reference) ---

/// The zero page (boot_params) lives at this GPA. For bzImage, the zero page
/// IS the setup code's header at 0x10000 — the VMM patches the header fields
/// directly in the loaded setup code rather than writing a separate
/// `boot_params` struct (which would clobber the setup code's data).
pub const ZERO_PAGE_ADDR: u64 = 0x0001_0000;
/// High-memory start — the kernel is loaded just above this.
pub const HIMEM_START: u64 = 0x0010_0000; // 1 MiB
/// Start of the device MMIO window — fixed, never moves. See the module docs
/// for why (ACPI `Memory32Fixed` is 32-bit only).
pub const MMIO_GAP_START: u64 = 0x_D000_0000; // 3.25 GiB
/// End of the device MMIO window / where relocated high memory resumes.
/// Must stay at or below `u32::MAX + 1` — anything the device window itself
/// needs must fit in `[MMIO_GAP_START, MMIO_GAP_END)`.
pub const MMIO_GAP_END: u64 = 0x1_0000_0000; // 4 GiB

// --- E820 map construction -------------------------------------------------

/// An E820 map entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub mem_type: u32,
}

/// Build the E820 map for a guest with `mem_size_bytes` of RAM.
///
/// See the module docs for the layout. For `mem_size_bytes <= MMIO_GAP_START`
/// (the common case — every VM this project ran before this fix), this is
/// unchanged from before: low, reserved, then one high-RAM entry running to
/// `mem_size_bytes`. Only requests that would reach into the device window
/// get a reserved gap entry plus a second high-RAM entry picking back up at
/// `MMIO_GAP_END`.
pub fn build_e820_map(mem_size_bytes: u64) -> Vec<E820Entry> {
    let mut entries = Vec::with_capacity(4);

    // 1. Low memory 0..0xA_0000 (640 KiB).
    entries.push(E820Entry {
        addr: 0,
        size: 0xA_0000,
        mem_type: E820_RAM,
    });

    // 2. Reserved 0xA_0000..0x10_0000 (VGA + BIOS area, 384 KiB).
    entries.push(E820Entry {
        addr: 0xA_0000,
        size: 0x10_0000 - 0xA_0000,
        mem_type: E820_RESERVED,
    });

    // 3. High memory from HIMEM_START up to the device window (or to the
    // end of RAM, if RAM doesn't reach that far).
    if mem_size_bytes > HIMEM_START {
        let high_end = mem_size_bytes.min(MMIO_GAP_START);
        entries.push(E820Entry {
            addr: HIMEM_START,
            size: high_end - HIMEM_START,
            mem_type: E820_RAM,
        });
    }

    // 4. The device window itself (reserved) + 5. any RAM relocated above
    // it. Both are only emitted when RAM actually reaches the window —
    // otherwise the guest never sees these addresses at all, which is fine:
    // nothing needs to claim address space nothing will ever touch.
    if mem_size_bytes > MMIO_GAP_START {
        entries.push(E820Entry {
            addr: MMIO_GAP_START,
            size: MMIO_GAP_END - MMIO_GAP_START,
            mem_type: E820_RESERVED,
        });
        entries.push(E820Entry {
            addr: MMIO_GAP_END,
            // Relocated size is the full remainder past MMIO_GAP_START, not
            // past MMIO_GAP_END — nothing requested is dropped, it's all
            // still here, just moved: total RAM == mem_size_bytes exactly.
            size: mem_size_bytes - MMIO_GAP_START,
            mem_type: E820_RAM,
        });
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e820_map_layout_small_16mib() {
        // 16 MiB RAM: unchanged from the pre-fix shape — low, reserved,
        // high — the device window is nowhere near this size.
        let m = build_e820_map(16 * 1024 * 1024);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].addr, 0);
        assert_eq!(m[0].size, 0xA_0000);
        assert_eq!(m[0].mem_type, E820_RAM);
        assert_eq!(m[1].addr, 0xA_0000);
        assert_eq!(m[1].mem_type, E820_RESERVED);
        assert_eq!(m[2].addr, HIMEM_START);
        assert_eq!(m[2].mem_type, E820_RAM);
        assert_eq!(m[2].size, 16 * 1024 * 1024 - HIMEM_START);
    }

    #[test]
    fn e820_map_layout_exactly_at_himem_start() {
        // RAM = HIMEM_START exactly: only low + reserved (no high RAM).
        let m = build_e820_map(HIMEM_START);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn e820_map_layout_exactly_at_gap_start_has_no_relocation() {
        // RAM ends exactly where the device window begins: no overlap, so
        // no gap/relocation entries are needed at all.
        let m = build_e820_map(MMIO_GAP_START);
        assert_eq!(m.len(), 3);
        assert_eq!(m[2].addr, HIMEM_START);
        assert_eq!(m[2].size, MMIO_GAP_START - HIMEM_START);
    }

    #[test]
    fn e820_map_layout_past_gap_relocates_the_remainder() {
        // 512 MiB of RAM past the device window: low, reserved, high (up to
        // the window), the window itself (reserved), then the relocated
        // remainder starting at MMIO_GAP_END.
        let requested = MMIO_GAP_START + 512 * 1024 * 1024;
        let m = build_e820_map(requested);
        assert_eq!(m.len(), 5);
        assert_eq!(m[2].addr, HIMEM_START);
        assert_eq!(m[2].size, MMIO_GAP_START - HIMEM_START);
        assert_eq!(m[3].addr, MMIO_GAP_START);
        assert_eq!(m[3].size, MMIO_GAP_END - MMIO_GAP_START);
        assert_eq!(m[3].mem_type, E820_RESERVED);
        assert_eq!(m[4].addr, MMIO_GAP_END);
        assert_eq!(m[4].size, 512 * 1024 * 1024);
        assert_eq!(m[4].mem_type, E820_RAM);
    }

    #[test]
    fn e820_map_layout_large_8gib() {
        // 8 GiB RAM: same five-entry shape, just a bigger relocated tail.
        let requested = 8 * 1024 * 1024 * 1024u64;
        let m = build_e820_map(requested);
        assert_eq!(m.len(), 5);
        let last = m[4];
        assert_eq!(last.addr, MMIO_GAP_END);
        assert_eq!(last.size, requested - MMIO_GAP_START);
        assert_eq!(last.mem_type, E820_RAM);
    }

    #[test]
    fn e820_ram_total_equals_mem_size_even_when_split() {
        // Only the RAM entries (not the reserved device window) must sum to
        // mem_size_bytes, minus the fixed 384 KiB VGA/BIOS carve-out that
        // every request pays regardless of size (0xA_0000..0x10_0000, always
        // reserved — a pre-existing, unrelated quirk of the low 1 MiB, same
        // before and after this fix). That leftover constant offset is the
        // whole point being tested here: past that, no *additional* capacity
        // is lost to the (much bigger) device window, however large the
        // request — it's all relocated, not discarded.
        const VGA_RESERVED: u64 = 0x10_0000 - 0xA_0000;
        for &sz in &[
            16 * 1024 * 1024,
            256 * 1024 * 1024,
            1024 * 1024 * 1024,
            MMIO_GAP_START,
            MMIO_GAP_START + 1, // just past the window's start
            4 * 1024 * 1024 * 1024,
            8 * 1024 * 1024 * 1024,
        ] {
            let m = build_e820_map(sz);
            let ram_total: u64 = m
                .iter()
                .filter(|e| e.mem_type == E820_RAM)
                .map(|e| e.size)
                .sum();
            assert_eq!(ram_total, sz - VGA_RESERVED, "size 0x{sz:x}");
        }
    }

    #[test]
    fn e820_entries_are_contiguous_and_non_overlapping() {
        // Walk the entries; each must start where the previous ended — this
        // still holds with the gap, since the gap is itself an entry.
        for &sz in &[
            16 * 1024 * 1024,
            256 * 1024 * 1024,
            1024 * 1024 * 1024,
            MMIO_GAP_START + 512 * 1024 * 1024,
            8 * 1024 * 1024 * 1024,
        ] {
            let m = build_e820_map(sz);
            for w in m.windows(2) {
                assert_eq!(w[0].addr + w[0].size, w[1].addr, "size 0x{sz:x}");
            }
            assert_eq!(m[0].addr, 0);
        }
    }

    #[test]
    fn gap_entry_never_marked_ram() {
        // However large the request, the device window itself must never
        // be claimed as RAM — that's the exact bug this module used to have
        // (with the wrong window address), and the one this fix must never
        // reintroduce.
        for &sz in &[
            MMIO_GAP_START + 1,
            4 * 1024 * 1024 * 1024,
            8 * 1024 * 1024 * 1024,
        ] {
            let m = build_e820_map(sz);
            for e in &m {
                let overlaps_window = e.addr < MMIO_GAP_END && e.addr + e.size > MMIO_GAP_START;
                if overlaps_window {
                    assert_eq!(e.mem_type, E820_RESERVED, "size 0x{sz:x}");
                }
            }
        }
    }
}
