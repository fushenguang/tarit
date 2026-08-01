//! Clone fan-out — restore N instances from one base snapshot.
//!
//! PaaS use case: snapshot a VM with Node.js installed, then stamp out
//! 100 clones for burst traffic. Each clone gets:
//! - Independent memory (UFFD lazy fault from the shared snapshot mmap)
//! - Independent disk (sparse CoW overlay)
//! - Independent network (unique MAC + netns + tap)
//! - Independent PRNG (virtio-rng re-seeds from /dev/urandom)
//! - Independent clock (fresh kvmclock base)
//!
//! Target: <10ms per clone for the hand-off (UFFD returns immediately;
//! pages fault in on demand during guest execution).

use crate::config::NetConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
use std::time::Instant;

/// A single clone's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneSpec {
    pub id: String,
    pub snapshot_path: String,
    /// CoW overlay path for this clone's disk.
    pub overlay_path: Option<String>,
    /// Network config (unique MAC + tap per clone).
    pub net: Option<NetConfig>,
}

/// Result of cloning N instances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneResult {
    pub cloned: Vec<CloneSpec>,
    pub total_ms: u64,
    pub per_clone_ms: f64,
}

/// Build clone specs for N instances from a base snapshot.
///
/// Each clone gets:
/// - A unique ID: `{base_id}-{i}`
/// - A CoW overlay path over the base volume
/// - A unique MAC: `02:00:00:00:HI:LO`
/// - A unique tap name: `{base_id}{i}tap0`
pub fn build_clone_specs(
    base_id: &str,
    snapshot_path: &str,
    base_volume: Option<&str>,
    n: u32,
    overlay_dir: &str,
) -> Vec<CloneSpec> {
    let mut specs = Vec::with_capacity(n as usize);
    for i in 0..n {
        let mac = format!("02:00:00:00:{:02x}:{:02x}", (i >> 8) & 0xff, i & 0xff);
        let overlay = base_volume.map(|_| {
            crate::gc::owned_overlay_path(Path::new(overlay_dir), i as usize)
                .to_string_lossy()
                .into_owned()
        });

        let net = NetConfig {
            tap: format!("{base_id}{i}tap0"),
            guest_mac: Some(mac),
            guest_ip: Some(format!("172.16.{}.{}", i / 256, i % 256)),
            port_forwards: vec![],
        };

        specs.push(CloneSpec {
            id: format!("{base_id}-{i}"),
            snapshot_path: snapshot_path.to_string(),
            overlay_path: overlay,
            net: Some(net),
        });
    }
    specs
}

/// Clone fan-out: restore N instances from a base snapshot.
///
/// This is the PaaS "burst of 100" path. Each clone:
/// 1. Creates a fresh KvmVm (new kvmclock → clock reset → CRNG re-seed)
/// 2. UFFD-registers the memory with the snapshot file (lazy fault-in)
/// 3. Creates a sparse CoW overlay for the disk
/// 4. Sets up a unique tap + netns
///
/// The actual VM boot happens lazily — pages fault in on demand during
/// guest execution. The hand-off (creating the VM + registering UFFD)
/// is <10ms per clone.
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
pub fn clone_fanout(
    controller: &crate::controller::VmmController,
    specs: &[CloneSpec],
    _base_volume: Option<&str>,
) -> CloneResult {
    let start = Instant::now();
    let mut cloned = Vec::new();

    for spec in specs {
        // Restore from the base snapshot. Each restore creates a fresh
        // sparse CoW overlay when `overlay_path` is set, so the clone never
        // reuses the golden snapshot's saved upper layer.
        //
        // Each restore also creates a fresh
        // KvmVm with its own kvmclock → the guest detects the clock jump
        // and re-seeds its CRNG from virtio-rng.
        match controller.restore(&spec.snapshot_path, spec.overlay_path.clone()) {
            Ok(()) => {
                log::info!(
                    "clone {}: restored (overlay={})",
                    spec.id,
                    spec.overlay_path.as_deref().unwrap_or("none")
                );
                cloned.push(spec.clone());
            }
            Err(e) => {
                log::warn!("clone {}: failed: {e}", spec.id);
            }
        }
    }

    let total_ms = start.elapsed().as_millis() as u64;
    let per_clone_ms = if cloned.is_empty() {
        0.0
    } else {
        total_ms as f64 / cloned.len() as f64
    };

    CloneResult {
        cloned,
        total_ms,
        per_clone_ms,
    }
}

/// Create a copy-on-write overlay from a base volume using `copy_file_range`
/// (Linux). On filesystems that support reflinks (btrfs, XFS), this creates
/// a true CoW copy. On others, it falls back to a full copy.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub fn create_cow_overlay(base_path: &str, overlay_path: &str) -> Result<(), String> {
    use std::fs;
    use std::os::unix::io::AsRawFd;

    let src = fs::File::open(base_path).map_err(|e| format!("open base: {e}"))?;
    let dst = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(overlay_path)
        .map_err(|e| format!("create overlay: {e}"))?;

    let src_size = src.metadata().map_err(|e| format!("metadata: {e}"))?.len();

    // copy_file_range does NOT guarantee copying the full requested length in
    // one call -- per copy_file_range(2), "a successful call may copy fewer
    // than len bytes", and this is not an error. The caller must loop over
    // the remaining bytes. The previous version of this function checked
    // only `ret < 0` (hard failure) and treated any non-negative return as
    // "fully copied", silently leaving the overlay's tail as whatever
    // truncate(true) left it (typically a sparse hole, i.e. zeros) whenever
    // the kernel returned a short count -- verified live in production: this
    // produced VM overlays with real files (e.g. a 12.8MB pnpm.mjs) corrupted
    // partway through, non-deterministically, under I/O pressure.
    let mut copied: u64 = 0;
    let mut copy_file_range_failed = false;
    while copied < src_size {
        let mut src_off = copied as i64;
        let mut dst_off = copied as i64;
        let remaining = (src_size - copied) as usize;
        // SAFETY: `src` and `dst` are valid file descriptors kept alive for
        // the duration of this call; `src_off`/`dst_off` are valid `i64`
        // lvalues the kernel reads the starting offset from and writes the
        // post-copy offset back into.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_copy_file_range,
                src.as_raw_fd(),
                &mut src_off as *mut i64,
                dst.as_raw_fd(),
                &mut dst_off as *mut i64,
                remaining,
                0u32,
            )
        };
        if ret < 0 {
            copy_file_range_failed = true;
            break;
        }
        if ret == 0 {
            // Zero with bytes still remaining means the kernel hit EOF on
            // the source before `src_size` -- the base file changed size
            // underneath us, or metadata() lied. Either way, silently
            // treating this as success would ship a truncated overlay.
            return Err(format!(
                "copy_file_range returned 0 with {} bytes still remaining (base file may have changed size)",
                src_size - copied
            ));
        }
        copied += ret as u64;
    }

    if copy_file_range_failed {
        // Fallback: regular copy. Re-truncate first -- copy_file_range may
        // have partially written the destination before failing.
        let mut src = fs::File::open(base_path).map_err(|e| format!("reopen: {e}"))?;
        std::io::copy(
            &mut src,
            &mut fs::File::create(overlay_path).map_err(|e| format!("recreate: {e}"))?,
        )
        .map_err(|e| format!("copy: {e}"))?;
    }

    log::info!("CoW overlay: {base_path} → {overlay_path} ({src_size} bytes)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_specs_creates_n_clones() {
        let specs = build_clone_specs("base", "/snap.bin", Some("/rootfs.ext4"), 5, "/tmp");
        assert_eq!(specs.len(), 5);
        assert_eq!(specs[0].id, "base-0");
        assert_eq!(specs[4].id, "base-4");
    }

    #[test]
    fn each_clone_has_unique_mac_and_tap() {
        let specs = build_clone_specs("base", "/snap.bin", None, 3, "/tmp");
        let macs: Vec<_> = specs
            .iter()
            .map(|s| s.net.as_ref().unwrap().guest_mac.clone())
            .collect();
        let taps: Vec<_> = specs
            .iter()
            .map(|s| s.net.as_ref().unwrap().tap.clone())
            .collect();
        assert_eq!(
            macs.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        assert_eq!(
            taps.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
    }

    #[test]
    fn clone_macs_are_local_unicast() {
        let specs = build_clone_specs("base", "/snap.bin", None, 256, "/tmp");
        for spec in &specs {
            let mac = spec.net.as_ref().unwrap().guest_mac.as_ref().unwrap();
            // First octet 0x02 = locally-administered unicast.
            assert!(mac.starts_with("02:00:00:00:"));
        }
    }

    #[test]
    fn clone_ips_are_unique() {
        let specs = build_clone_specs("base", "/snap.bin", None, 300, "/tmp");
        let ips: Vec<_> = specs
            .iter()
            .map(|s| s.net.as_ref().unwrap().guest_ip.clone())
            .collect();
        assert_eq!(
            ips.iter().collect::<std::collections::HashSet<_>>().len(),
            300
        );
    }

    #[test]
    fn overlay_paths_set_when_volume_provided() {
        let specs = build_clone_specs("base", "/snap.bin", Some("/rootfs.ext4"), 2, "/tmp");
        assert!(specs[0].overlay_path.is_some());
        let overlay = specs[0].overlay_path.as_ref().unwrap();
        assert!(overlay.contains("/vmm-ov-"));
        assert!(overlay.ends_with("-0.cow"));
    }

    #[test]
    fn overlay_paths_none_when_no_volume() {
        let specs = build_clone_specs("base", "/snap.bin", None, 2, "/tmp");
        assert!(specs[0].overlay_path.is_none());
    }
}

#[cfg(test)]
mod create_cow_overlay_tests {
    use super::create_cow_overlay;
    use std::fs;
    use std::io::Write;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-work")
            .join(format!("{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Regression test for a real bug: create_cow_overlay used to check only
    // `ret < 0` from copy_file_range and treat any non-negative return as a
    // full copy, even though copy_file_range(2) explicitly does not
    // guarantee copying the full requested length in one call. A large
    // patterned file (multi-MB, larger than a single typical copy_file_range
    // chunk) exercises the loop; byte-for-byte equality is the only thing
    // that matters here.
    #[test]
    fn overlay_matches_base_byte_for_byte_for_a_large_file() {
        let dir = scratch_dir("cow-overlay-large");
        let base = dir.join("base.img");
        let overlay = dir.join("overlay.img");

        // 8 MiB of non-repeating content so a truncated/short copy is
        // detectable anywhere in the file, not just at a fixed offset.
        let mut f = fs::File::create(&base).unwrap();
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8; // 251 is prime, avoids trivial 256-periodicity
        }
        f.write_all(&buf).unwrap();
        drop(f);

        create_cow_overlay(base.to_str().unwrap(), overlay.to_str().unwrap())
            .expect("create_cow_overlay should succeed");

        let overlay_bytes = fs::read(&overlay).unwrap();
        assert_eq!(
            overlay_bytes.len(),
            buf.len(),
            "overlay must be the same length as the base file"
        );
        assert_eq!(
            overlay_bytes, buf,
            "overlay content must match the base file byte-for-byte"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overlay_matches_base_for_a_small_file() {
        let dir = scratch_dir("cow-overlay-small");
        let base = dir.join("base.img");
        let overlay = dir.join("overlay.img");
        fs::write(&base, b"hello cow overlay").unwrap();

        create_cow_overlay(base.to_str().unwrap(), overlay.to_str().unwrap())
            .expect("create_cow_overlay should succeed");

        assert_eq!(fs::read(&overlay).unwrap(), b"hello cow overlay");
        fs::remove_dir_all(dir).unwrap();
    }
}
