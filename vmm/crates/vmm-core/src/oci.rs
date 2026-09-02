//! OCI image fetching — pull from registries (Docker Hub, GHCR) and
//! convert to a bootable ext4 disk image.
//!
//! Pipeline:
//! 1. Pull the OCI image using `skopeo` (handles auth, manifest, layers)
//! 2. Unpack the layers using `umoci` (flattens overlay layers to a dir)
//! 3. Create an ext4 disk image from the directory using `mke2fs -d`
//! 4. Boot the VM with the ext4 image as a virtio-blk volume + root=/dev/vda
//!
//! This runs as an external pipeline (skopeo + umoci + mke2fs) rather than
//! a pure-Rust implementation because:
//! - skopeo handles registry auth, manifest parsing, layer pulling, caching
//! - umoci handles layer unpacking (tar.gz + overlay whiteout)
//! - mke2fs -d creates a proper ext4 filesystem from a directory
//!
//! The VMM binary itself stays lean — the OCI pipeline is an orchestrator
//! concern. The VMM just boots the resulting ext4 image.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OciError {
    #[error("skopeo not found — install with: apt install skopeo")]
    SkopeoNotFound,
    #[error("umoci not found — install with: apt install umoci")]
    UmociNotFound,
    #[error("mke2fs not found — install with: apt install e2fsprogs")]
    Mke2fsNotFound,
    #[error("command failed: {cmd}: {stderr}")]
    CommandFailed { cmd: String, stderr: String },
    #[error("invalid output path: {0}")]
    InvalidOutput(String),
    #[error("unsafe OCI rootfs path: {0}")]
    UnsafePath(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// An OCI image reference (e.g., "docker://ubuntu:22.04", "ghcr.io/owner/repo:tag").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciImageRef {
    /// Full image reference (e.g., "docker://ubuntu:22.04").
    pub reference: String,
    /// Optional auth file path (for private registries).
    pub auth_file: Option<String>,
}

/// Result of pulling + converting an OCI image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciPullResult {
    /// Path to the resulting ext4 disk image.
    pub disk_image_path: String,
    /// Size of the disk image in bytes.
    pub size_bytes: u64,
    /// Time taken in milliseconds.
    pub elapsed_ms: u64,
    /// How the guest exec agent was wired into the image, if at all.
    /// Initless app images (`node:20`) boot straight to the agent as PID 1;
    /// systemd-bearing images get a `vmm-agent.service` unit instead.
    pub agent_wiring: Option<AgentWiring>,
}

/// How the guest exec agent is wired into a converted image (tarit#41).
///
/// `Init` replaces `/sbin/init` with a symlink to the agent — the only
/// option for initless OCI images. `SystemdUnit` installs and enables
/// `vmm-agent.service` instead, for images that ship a real init: an
/// in-guest `apt install/upgrade systemd-sysv` can replace the `/sbin/init`
/// symlink, and the next reboot then execs systemd as PID 1 with nothing
/// starting the agent — the exec channel dies while the guest looks
/// perfectly healthy. The unit also buys in-guest crash recovery
/// (`Restart=always`) for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentWiring {
    /// `/sbin/init` → symlink to `/usr/sbin/vmm-agent` (agent runs as PID 1).
    Init,
    /// `/etc/systemd/system/vmm-agent.service` enabled for
    /// `multi-user.target`, with `serial-getty@ttyS0` masked.
    SystemdUnit,
}

/// Pull an OCI image and convert it to a bootable ext4 disk image.
///
/// Steps:
/// 1. `skopeo copy docker://image:tag oci:image-oci:default`
/// 2. `umoci unpack --image image-oci:default rootfs-dir`
/// 3. `mke2fs -t ext4 -d rootfs-dir -L rootfs disk.ext4 1G`
pub fn pull_and_convert(
    image: &OciImageRef,
    output_path: &Path,
    size_mb: u64,
) -> Result<OciPullResult, OciError> {
    pull_and_convert_with_agent(image, output_path, size_mb, None)
}

/// Like [`pull_and_convert`], but also injects the guest exec agent as the
/// image's init when `agent_path` is given. OCI/Docker images ship only a root
/// filesystem — no kernel, no init system (the container runtime runs the
/// entrypoint as PID 1). To boot one as a microVM we supply the kernel and drop
/// in the agent as `/usr/sbin/vmm-agent`, pointing `/sbin/init` at it (unless
/// the image already has an init). Booted with the agent as PID 1 it mounts the
/// pseudo-filesystems and serves the exec channel, so `node:20`, `python:3` and
/// friends become directly runnable sandboxes.
pub fn pull_and_convert_with_agent(
    image: &OciImageRef,
    output_path: &Path,
    size_mb: u64,
    agent_path: Option<&Path>,
) -> Result<OciPullResult, OciError> {
    let start = std::time::Instant::now();

    // Check for required tools.
    check_tool("skopeo")?;
    check_tool("umoci")?;
    check_tool("mke2fs")?;

    let final_output = resolved_output_path(output_path)?;
    let output_dir = final_output
        .parent()
        .expect("resolved output always has a parent");
    // Every build receives an atomic, mode-0700 workspace. Fixed sibling
    // directories allowed concurrent builds to delete or corrupt each other.
    let workspace = create_oci_workspace(output_dir)?;
    let oci_dir = workspace.path().join("oci-image");
    let rootfs_dir = workspace.path().join("bundle");
    let staged_output = workspace.path().join("disk.ext4");

    // Step 1: Pull the image via skopeo.
    log::info!("oci: pulling {} via skopeo", image.reference);
    let mut skopeo_cmd = std::process::Command::new("skopeo");
    skopeo_cmd
        .arg("copy")
        .arg(&image.reference)
        .arg(format!("oci:{}:default", oci_dir.display()));
    if let Some(ref auth) = image.auth_file {
        skopeo_cmd.arg("--authfile").arg(auth);
    }
    run_command(skopeo_cmd, "skopeo copy")?;

    // Step 2: Unpack layers via umoci.
    log::info!("oci: unpacking layers via umoci");
    let mut umoci = Command::new("umoci");
    umoci
        .arg("unpack")
        .arg("--image")
        .arg(format!("{}:default", oci_dir.display()))
        .arg(&rootfs_dir);
    run_command(umoci, "umoci unpack")?;

    // Step 2b: Inject the exec agent. Initless images boot straight to the
    // agent as init; systemd-bearing images get a vmm-agent.service unit so
    // an in-guest systemd-sysv upgrade cannot brick the exec channel on the
    // next reboot (tarit#41).
    let agent_wiring = match agent_path {
        Some(agent) => Some(inject_agent_init(&rootfs_dir.join("rootfs"), agent)?),
        None => None,
    };

    // Step 3: Create ext4 disk image via mke2fs. umoci unpacks the root fs into
    // a `rootfs` subdirectory of the bundle.
    let src_dir = rootfs_dir.join("rootfs");
    log::info!("oci: creating ext4 disk image ({}MB)", size_mb);
    let mut mke2fs = Command::new("mke2fs");
    mke2fs
        .arg("-t")
        .arg("ext4")
        .arg("-d")
        .arg(&src_dir)
        .arg("-L")
        .arg("rootfs")
        .arg("-F")
        .arg(&staged_output)
        .arg(format!("{}M", size_mb));
    run_command(mke2fs, "mke2fs")?;
    publish_disk_image(&staged_output, &final_output)?;

    let size_bytes = std::fs::metadata(&final_output)?.len();
    let elapsed_ms = start.elapsed().as_millis() as u64;

    log::info!(
        "oci: conversion complete — {} bytes in {}ms (agent_wiring={agent_wiring:?})",
        size_bytes,
        elapsed_ms
    );

    Ok(OciPullResult {
        disk_image_path: output_path.to_string_lossy().to_string(),
        size_bytes,
        elapsed_ms,
        agent_wiring,
    })
}

fn resolved_output_path(output_path: &Path) -> Result<PathBuf, OciError> {
    let file_name = output_path.file_name().ok_or_else(|| {
        OciError::InvalidOutput(format!("{} has no file name", output_path.display()))
    })?;
    if file_name.as_bytes().contains(&0) {
        return Err(OciError::InvalidOutput("file name contains NUL".into()));
    }
    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent).map_err(|error| {
        OciError::InvalidOutput(format!(
            "resolve output directory {}: {error}",
            parent.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(&parent)?;
    if !metadata.is_dir() {
        return Err(OciError::InvalidOutput(format!(
            "{} is not a directory",
            parent.display()
        )));
    }
    Ok(parent.join(file_name))
}

fn create_oci_workspace(parent: &Path) -> Result<tempfile::TempDir, OciError> {
    let workspace = tempfile::Builder::new()
        .prefix(".tarit-oci-")
        .tempdir_in(parent)
        .map_err(OciError::Io)?;
    #[cfg(unix)]
    std::fs::set_permissions(workspace.path(), std::fs::Permissions::from_mode(0o700))?;
    Ok(workspace)
}

fn publish_disk_image(staged: &Path, destination: &Path) -> Result<(), OciError> {
    let metadata = std::fs::symlink_metadata(staged)?;
    if !metadata.file_type().is_file() {
        return Err(OciError::UnsafePath(format!(
            "staged disk is not a regular file: {}",
            staged.display()
        )));
    }
    #[cfg(unix)]
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o600))?;

    // Open without following links and flush the completed filesystem before
    // atomically publishing it. The private workspace makes this setup-only;
    // it does not add anything to VM boot or restore latency.
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let staged_file = options.open(staged)?;
    if !staged_file.metadata()?.file_type().is_file() {
        return Err(OciError::UnsafePath(format!(
            "staged disk changed type: {}",
            staged.display()
        )));
    }
    staged_file.sync_all()?;

    if let Ok(existing) = std::fs::symlink_metadata(destination) {
        if existing.file_type().is_dir() {
            return Err(OciError::InvalidOutput(format!(
                "refusing to replace output directory {}",
                destination.display()
            )));
        }
    }
    std::fs::rename(staged, destination)?;
    #[cfg(target_os = "linux")]
    File::open(
        destination
            .parent()
            .expect("resolved destination has a parent"),
    )?
    .sync_all()?;
    Ok(())
}

/// Copy the exec agent into an unpacked rootfs and wire it up, returning how.
/// Every path component is traversed relative to an already-open directory fd
/// with `O_NOFOLLOW`; image-controlled symlinks cannot redirect writes onto
/// the host. The final executable is written under a unique name and renamed
/// over any untrusted leaf atomically.
fn inject_agent_init(rootfs: &Path, agent: &Path) -> Result<AgentWiring, OciError> {
    #[cfg(unix)]
    {
        inject_agent_init_unix(rootfs, agent)
    }
    #[cfg(not(unix))]
    {
        let _ = (rootfs, agent);
        Err(OciError::UnsafePath(
            "agent injection requires Unix openat confinement".into(),
        ))
    }
}

#[cfg(unix)]
fn inject_agent_init_unix(rootfs: &Path, agent: &Path) -> Result<AgentWiring, OciError> {
    let mut root_options = OpenOptions::new();
    root_options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let root = root_options.open(rootfs).map_err(|error| {
        OciError::UnsafePath(format!("open rootfs {}: {error}", rootfs.display()))
    })?;
    if !root.metadata()?.is_dir() {
        return Err(OciError::UnsafePath(format!(
            "rootfs is not a directory: {}",
            rootfs.display()
        )));
    }

    let usr = open_or_create_directory_at(&root, "usr")?;
    let usr_sbin = open_or_create_directory_at(&usr, "sbin")?;
    install_agent_at(&usr_sbin, agent)?;

    let init_dir = match entry_kind_at(&root, "sbin")? {
        None => open_or_create_directory_at(&root, "sbin")?,
        Some(EntryKind::Directory) => open_directory_at(&root, "sbin")?,
        Some(EntryKind::Symlink) => {
            let target = read_link_at(&root, "sbin")?;
            if target.as_bytes() != b"usr/sbin" && target.as_bytes() != b"/usr/sbin" {
                return Err(OciError::UnsafePath(format!(
                    "unsupported /sbin symlink target {:?}; expected usr/sbin",
                    target
                )));
            }
            usr_sbin.try_clone()?
        }
        Some(EntryKind::Other) => {
            return Err(OciError::UnsafePath(
                "/sbin exists but is neither a directory nor a safe symlink".into(),
            ))
        }
    };
    if systemd_binary_at(&root)? {
        mask_serial_getty_at(&root)?;
        enable_agent_unit_at(&root)?;
        log::info!("oci: injected exec agent as vmm-agent.service (systemd image)");
        Ok(AgentWiring::SystemdUnit)
    } else {
        ensure_init_link_at(&init_dir)?;
        log::info!("oci: injected exec agent as init (/usr/sbin/vmm-agent)");
        Ok(AgentWiring::Init)
    }
}

/// The systemd unit installed into systemd-bearing images, kept in lockstep
/// with `guest/agent/bake-agent.sh` (which wires baked-from-scratch images):
/// `Restart=always` survives agent crashes, and the conflict + mask keeps
/// `serial-getty@ttyS0` from stealing console bytes the agent reads.
#[cfg(unix)]
const AGENT_UNIT_CONTENT: &str = r#"[Unit]
Description=VMM serial exec guest agent
Documentation=file:/usr/sbin/vmm-agent
Conflicts=serial-getty@ttyS0.service
After=dev-ttyS0.device

[Service]
Type=simple
ExecStart=/usr/sbin/vmm-agent
Restart=always
RestartSec=1

[Install]
WantedBy=multi-user.target
"#;

#[cfg(unix)]
const SYSTEMD_BINARY_PATHS: [&[&str]; 2] = [
    &["usr", "lib", "systemd", "systemd"],
    &["lib", "systemd", "systemd"],
];

#[cfg(unix)]
fn is_regular_file_at(parent: &File, name: &str) -> Result<bool, OciError> {
    let name = c_name(name)?;
    // SAFETY: stat is immediately initialized by fstatat before being read.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: parent fd and name are valid for the duration of fstatat.
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(false);
        }
        return Err(error.into());
    }
    Ok(stat.st_mode & libc::S_IFMT == libc::S_IFREG)
}

/// Detect a systemd-bearing image: `usr/lib/systemd/systemd` (usr-merged
/// layout) or `lib/systemd/systemd` (classic layout) present as a regular
/// file. Components are walked with O_NOFOLLOW probes so an image-controlled
/// symlink cannot redirect the detection.
#[cfg(unix)]
fn systemd_binary_at(root: &File) -> Result<bool, OciError> {
    'probe: for components in SYSTEMD_BINARY_PATHS {
        let mut dir = root.try_clone()?;
        for (index, component) in components.iter().enumerate() {
            if index + 1 == components.len() {
                if is_regular_file_at(&dir, component)? {
                    return Ok(true);
                }
                continue 'probe;
            }
            match entry_kind_at(&dir, component)? {
                Some(EntryKind::Directory) => {
                    dir = open_directory_at(&dir, component)?;
                }
                _ => continue 'probe,
            }
        }
    }
    Ok(false)
}

/// Write `bytes` as a leaf `name` under the held directory fd, atomically
/// replacing any existing leaf: staged under a unique temporary name with
/// O_EXCL|O_NOFOLLOW, then renamed into place (renameat replaces a leaf
/// symlink without following it).
#[cfg(unix)]
fn write_leaf_at(
    destination_dir: &File,
    name: &str,
    bytes: &[u8],
    mode: libc::mode_t,
) -> Result<(), OciError> {
    static LEAF_SEQ: AtomicU64 = AtomicU64::new(0);
    let sequence = LEAF_SEQ.fetch_add(1, Ordering::Relaxed);
    let temporary_name = format!(".{name}-{}-{sequence}.tmp", std::process::id());
    let temporary = c_name(&temporary_name)?;
    let destination = c_name(name)?;
    // SAFETY: directory fd and names are valid; mode is supplied with O_CREAT.
    let fd = unsafe {
        libc::openat(
            destination_dir.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fd is a fresh successful openat result and is uniquely owned.
    let mut staged = unsafe { File::from_raw_fd(fd) };
    let result = (|| -> Result<(), OciError> {
        staged.write_all(bytes)?;
        staged.flush()?;
        staged.sync_all()?;
        // SAFETY: staged is a valid fd and mode is a valid mode.
        if unsafe { libc::fchmod(staged.as_raw_fd(), mode) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: both names are valid components relative to the same held
        // directory descriptor.
        if unsafe {
            libc::renameat(
                destination_dir.as_raw_fd(),
                temporary.as_ptr(),
                destination_dir.as_raw_fd(),
                destination.as_ptr(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        #[cfg(target_os = "linux")]
        destination_dir.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: temporary is a leaf we created under the held directory.
        unsafe { libc::unlinkat(destination_dir.as_raw_fd(), temporary.as_ptr(), 0) };
    }
    result
}

/// Remove a leaf under the held directory fd; a missing leaf is fine.
#[cfg(unix)]
fn unlink_leaf_at(dir: &File, name: &str) -> Result<(), OciError> {
    let name = c_name(name)?;
    // SAFETY: name is a valid component relative to the held directory fd.
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if rc < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    Ok(())
}

/// Point leaf `name` at `target`, replacing whatever leaf is there (symlinkat
/// alone fails with EEXIST on a pre-existing name).
#[cfg(unix)]
fn replace_symlink_at(dir: &File, name: &str, target: &[u8]) -> Result<(), OciError> {
    unlink_leaf_at(dir, name)?;
    let name = c_name(name)?;
    let target =
        std::ffi::CString::new(target).map_err(|_| OciError::UnsafePath("NUL in target".into()))?;
    // SAFETY: target and leaf name are valid for symlinkat; the held fd
    // confines creation to the image's directory.
    if unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    #[cfg(target_os = "linux")]
    dir.sync_all()?;
    Ok(())
}

/// Install and enable `vmm-agent.service` for `multi-user.target`.
#[cfg(unix)]
fn enable_agent_unit_at(root: &File) -> Result<(), OciError> {
    let etc = open_or_create_directory_at(root, "etc")?;
    let systemd = open_or_create_directory_at(&etc, "systemd")?;
    let system = open_or_create_directory_at(&systemd, "system")?;
    write_leaf_at(
        &system,
        "vmm-agent.service",
        AGENT_UNIT_CONTENT.as_bytes(),
        0o644,
    )?;
    let wants = open_or_create_directory_at(&system, "multi-user.target.wants")?;
    replace_symlink_at(&wants, "vmm-agent.service", b"../vmm-agent.service")?;
    Ok(())
}

/// Mask `serial-getty@ttyS0.service` (it would fight the agent over the
/// serial console) and drop any already-enabled instance of it.
#[cfg(unix)]
fn mask_serial_getty_at(root: &File) -> Result<(), OciError> {
    let etc = open_or_create_directory_at(root, "etc")?;
    let systemd = open_or_create_directory_at(&etc, "systemd")?;
    let system = open_or_create_directory_at(&systemd, "system")?;
    replace_symlink_at(&system, "serial-getty@ttyS0.service", b"/dev/null")?;
    for wants_dir in ["multi-user.target.wants", "getty.target.wants"] {
        if let Ok(wants) = open_directory_at(&system, wants_dir) {
            unlink_leaf_at(&wants, "serial-getty@ttyS0.service")?;
        }
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    Symlink,
    Other,
}

#[cfg(unix)]
fn c_name(name: &str) -> Result<std::ffi::CString, OciError> {
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| OciError::UnsafePath(format!("path component contains NUL: {name:?}")))
}

#[cfg(unix)]
fn entry_kind_at(parent: &File, name: &str) -> Result<Option<EntryKind>, OciError> {
    let name = c_name(name)?;
    // SAFETY: stat is immediately initialized by fstatat before being read.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: parent and name are valid for the duration of fstatat, and stat
    // is valid writable storage.
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    let file_type = stat.st_mode & libc::S_IFMT;
    Ok(Some(if file_type == libc::S_IFDIR {
        EntryKind::Directory
    } else if file_type == libc::S_IFLNK {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    }))
}

#[cfg(unix)]
fn open_or_create_directory_at(parent: &File, name: &str) -> Result<File, OciError> {
    match entry_kind_at(parent, name)? {
        None => {
            let c_name = c_name(name)?;
            // SAFETY: parent fd and component name are valid; mkdirat retains
            // no pointer after returning.
            let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o755) };
            if rc < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
        }
        Some(EntryKind::Directory) => {}
        Some(kind) => {
            return Err(OciError::UnsafePath(format!(
                "component {name:?} is {kind:?}, not a directory"
            )))
        }
    }
    open_directory_at(parent, name)
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &str) -> Result<File, OciError> {
    let name = c_name(name)?;
    // SAFETY: parent fd and name are valid for openat; the returned fd is
    // checked and uniquely transferred to File.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(OciError::UnsafePath(format!(
            "open directory component {:?}: {}",
            name,
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: fd is a fresh successful openat result and is uniquely owned.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn read_link_at(parent: &File, name: &str) -> Result<std::ffi::OsString, OciError> {
    let name = c_name(name)?;
    let mut buffer = vec![0u8; 4096];
    // SAFETY: parent/name are valid and buffer is writable for its full length.
    let len = unsafe {
        libc::readlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if len < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let len =
        usize::try_from(len).map_err(|_| OciError::UnsafePath("negative symlink length".into()))?;
    if len == buffer.len() {
        return Err(OciError::UnsafePath("/sbin symlink target too long".into()));
    }
    buffer.truncate(len);
    Ok(std::ffi::OsString::from_vec(buffer))
}

#[cfg(unix)]
fn install_agent_at(destination_dir: &File, agent: &Path) -> Result<(), OciError> {
    const MAX_AGENT_BYTES: u64 = 64 * 1024 * 1024;
    static AGENT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let mut source_options = OpenOptions::new();
    source_options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut source = source_options.open(agent).map_err(|error| {
        OciError::UnsafePath(format!("open agent {}: {error}", agent.display()))
    })?;
    let source_metadata = source.metadata()?;
    if !source_metadata.file_type().is_file()
        || source_metadata.len() == 0
        || source_metadata.len() > MAX_AGENT_BYTES
    {
        return Err(OciError::UnsafePath(format!(
            "agent must be a non-empty regular file no larger than {MAX_AGENT_BYTES} bytes"
        )));
    }

    let sequence = AGENT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temporary_name = format!(".vmm-agent-{}-{sequence}.tmp", std::process::id());
    let temporary = c_name(&temporary_name)?;
    let destination = c_name("vmm-agent")?;
    // SAFETY: directory fd and name are valid, mode is supplied with O_CREAT.
    let fd = unsafe {
        libc::openat(
            destination_dir.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o700,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fd is a fresh successful openat result and is uniquely owned.
    let mut staged = unsafe { File::from_raw_fd(fd) };
    let result = (|| -> Result<(), OciError> {
        let copied = std::io::copy(&mut source, &mut staged)?;
        if copied != source_metadata.len() {
            return Err(OciError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "agent changed size while being copied",
            )));
        }
        staged.flush()?;
        staged.sync_all()?;
        // SAFETY: staged is a valid file descriptor and 0755 is a valid mode.
        if unsafe { libc::fchmod(staged.as_raw_fd(), 0o755) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: both names are valid components relative to the same held
        // directory descriptor. renameat replaces a leaf symlink, never follows it.
        if unsafe {
            libc::renameat(
                destination_dir.as_raw_fd(),
                temporary.as_ptr(),
                destination_dir.as_raw_fd(),
                destination.as_ptr(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        #[cfg(target_os = "linux")]
        destination_dir.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: temporary is a valid leaf name relative to destination_dir;
        // unlinkat cannot traverse outside that held directory.
        unsafe {
            libc::unlinkat(destination_dir.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}

#[cfg(unix)]
fn ensure_init_link_at(sbin_dir: &File) -> Result<(), OciError> {
    if entry_kind_at(sbin_dir, "init")?.is_some() {
        return Ok(());
    }
    let target = c_name("/usr/sbin/vmm-agent")?;
    let init = c_name("init")?;
    // SAFETY: target and leaf name are valid for symlinkat and the held fd
    // confines creation to the image's /sbin directory.
    if unsafe { libc::symlinkat(target.as_ptr(), sbin_dir.as_raw_fd(), init.as_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    #[cfg(target_os = "linux")]
    sbin_dir.sync_all()?;
    Ok(())
}

/// Check if a tool is available on PATH.
fn check_tool(name: &str) -> Result<(), OciError> {
    match name {
        "skopeo" if which("skopeo").is_none() => Err(OciError::SkopeoNotFound),
        "umoci" if which("umoci").is_none() => Err(OciError::UmociNotFound),
        "mke2fs" if which("mke2fs").is_none() => Err(OciError::Mke2fsNotFound),
        _ => Ok(()),
    }
}

fn which(cmd: &str) -> Option<PathBuf> {
    let output = Command::new("which").arg(cmd).output().ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn run_command(mut cmd: Command, name: &str) -> Result<(), OciError> {
    let output = cmd.output().map_err(|e| OciError::CommandFailed {
        cmd: name.to_string(),
        stderr: e.to_string(),
    })?;
    if !output.status.success() {
        return Err(OciError::CommandFailed {
            cmd: name.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn test_agent(dir: &Path) -> PathBuf {
        let path = dir.join("vmm-agent");
        std::fs::write(&path, b"test-agent").unwrap();
        path
    }

    #[test]
    fn oci_image_ref_serializes() {
        let r = OciImageRef {
            reference: "docker://ubuntu:22.04".into(),
            auth_file: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("docker://ubuntu:22.04"));
    }

    #[test]
    fn which_finds_known_commands() {
        // `ls` should always be available.
        assert!(which("ls").is_some());
        assert!(which("nonexistent_tool_12345").is_none());
    }

    #[test]
    fn oci_workspaces_are_private_and_unique() {
        let parent = tempfile::tempdir().unwrap();
        let first = create_oci_workspace(parent.path()).unwrap();
        let second = create_oci_workspace(parent.path()).unwrap();
        assert_ne!(first.path(), second.path());
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(first.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_uses_fd_relative_paths_and_supports_usrmerge() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::os::unix::fs::symlink("usr/sbin", rootfs.join("sbin")).unwrap();
        let agent = test_agent(temp.path());

        let wiring = inject_agent_init(&rootfs, &agent).unwrap();
        assert_eq!(wiring, AgentWiring::Init);

        assert_eq!(
            std::fs::read(rootfs.join("usr/sbin/vmm-agent")).unwrap(),
            b"test-agent"
        );
        let mode = std::fs::metadata(rootfs.join("usr/sbin/vmm-agent"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(
            std::fs::read_link(rootfs.join("usr/sbin/init")).unwrap(),
            Path::new("/usr/sbin/vmm-agent")
        );
    }

    #[cfg(unix)]
    #[test]
    fn systemd_image_gets_unit_wiring_not_init_symlink() {
        // tarit#41: an image that ships systemd must get vmm-agent.service,
        // never the /sbin/init symlink — an in-guest systemd-sysv upgrade
        // replaces that symlink and the next reboot silently kills the exec
        // channel while the guest looks healthy.
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("usr/lib/systemd")).unwrap();
        std::fs::write(rootfs.join("usr/lib/systemd/systemd"), b"#!/bin/sh\n").unwrap();
        std::fs::create_dir(rootfs.join("sbin")).unwrap();
        let agent = test_agent(temp.path());

        let wiring = inject_agent_init(&rootfs, &agent).unwrap();

        assert_eq!(wiring, AgentWiring::SystemdUnit);
        let system = rootfs.join("etc/systemd/system");
        let unit = std::fs::read_to_string(system.join("vmm-agent.service")).unwrap();
        assert!(unit.contains("ExecStart=/usr/sbin/vmm-agent"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("Conflicts=serial-getty@ttyS0.service"));
        assert_eq!(
            std::fs::read_link(system.join("multi-user.target.wants/vmm-agent.service")).unwrap(),
            Path::new("../vmm-agent.service")
        );
        assert_eq!(
            std::fs::read_link(system.join("serial-getty@ttyS0.service")).unwrap(),
            Path::new("/dev/null")
        );
        // The agent binary itself is still installed.
        assert_eq!(
            std::fs::read(rootfs.join("usr/sbin/vmm-agent")).unwrap(),
            b"test-agent"
        );
        // And the boot-critical init symlink must NOT exist on this image.
        assert!(std::fs::symlink_metadata(rootfs.join("sbin/init")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_rejects_image_controlled_parent_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, rootfs.join("usr")).unwrap();
        let agent = test_agent(temp.path());

        let error = inject_agent_init(&rootfs, &agent).unwrap_err();
        assert!(matches!(error, OciError::UnsafePath(_)));
        assert!(!outside.join("sbin/vmm-agent").exists());
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_replaces_leaf_symlink_without_following_it() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let destination = rootfs.join("usr/sbin");
        std::fs::create_dir_all(&destination).unwrap();
        let victim = temp.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        std::os::unix::fs::symlink(&victim, destination.join("vmm-agent")).unwrap();
        let agent = test_agent(temp.path());

        inject_agent_init(&rootfs, &agent).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert!(std::fs::symlink_metadata(destination.join("vmm-agent"))
            .unwrap()
            .file_type()
            .is_file());
        assert_eq!(
            std::fs::read(destination.join("vmm-agent")).unwrap(),
            b"test-agent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_injection_rejects_unsafe_sbin_symlink_target() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::os::unix::fs::symlink("../../outside", rootfs.join("sbin")).unwrap();
        let agent = test_agent(temp.path());

        let error = inject_agent_init(&rootfs, &agent).unwrap_err();
        assert!(matches!(error, OciError::UnsafePath(_)));
    }

    #[cfg(unix)]
    #[test]
    fn publish_replaces_output_symlink_without_touching_target() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("staged.ext4");
        let output = temp.path().join("output.ext4");
        let victim = temp.path().join("victim");
        std::fs::write(&staged, b"disk").unwrap();
        std::fs::write(&victim, b"untouched").unwrap();
        std::os::unix::fs::symlink(&victim, &output).unwrap();

        publish_disk_image(&staged, &output).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert_eq!(std::fs::read(&output).unwrap(), b"disk");
        assert!(std::fs::symlink_metadata(output)
            .unwrap()
            .file_type()
            .is_file());
    }
}
