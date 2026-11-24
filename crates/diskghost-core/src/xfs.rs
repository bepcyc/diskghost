//! XFS rescan delta: every inode's size, times and mode from one sequential
//! `XFS_IOC_BULKSTAT` pass (inode-number order, no path lookups) instead of
//! a `stat` per file — so a rescan lists only folders whose inode changed.
//!
//! Layouts and rules, from the kernel sources (fetched 2026-11-24):
//! - `fs/xfs/libxfs/xfs_fs.h`: `struct xfs_bulk_ireq` (64 bytes) and
//!   `struct xfs_bulkstat` (192 bytes, `bs_version == 5`), `XFS_IOC_BULKSTAT
//!   = _IOR('X', 127, struct xfs_bulkstat_req)`.
//! - `fs/xfs/xfs_ioctl.c` `xfs_ioc_bulkstat`: `if (!capable(CAP_SYS_ADMIN))
//!   return -EPERM;` — root only. `xfs_bulk_ireq_teardown` writes the next
//!   start inode back to `hdr.ino`; a pass past the last inode returns
//!   `ocount == 0`.
//!
//! The owner's decision (2026-11-24): no privilege is ever granted to
//! diskghost. This path runs only when the process already has
//! CAP_SYS_ADMIN (for example `index add` run as root); otherwise the scan
//! walks and says why.

use std::collections::HashMap;
use std::io;
use std::path::Path;

/// What the delta needs of one inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InodeInfo {
    pub(crate) ino: u64,
    pub(crate) size: u64,
    /// As [`crate::walk::modified_ns`]: 0 before the epoch.
    pub(crate) mtime_ns: u64,
    /// As `stat_of`: `None` before the epoch.
    pub(crate) ctime_ns: Option<u64>,
    /// `S_IFMT` bits and permissions.
    pub(crate) mode: u32,
    pub(crate) nlink: u32,
}

impl InodeInfo {
    pub(crate) fn is_dir(&self) -> bool {
        self.mode & 0o170000 == 0o040000
    }
    pub(crate) fn is_file(&self) -> bool {
        self.mode & 0o170000 == 0o100000
    }
}

/// Seconds + nanoseconds as `modified_ns` makes them: 0 before the epoch.
pub(crate) fn time_ns(sec: i64, nsec: u32) -> u64 {
    if sec < 0 {
        return 0;
    }
    (sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(nsec))
}

/// `_IOR(ty, nr, size)` on Linux (asm-generic/ioctl.h): dir 2 (read) in
/// bits 30-31, size in 16-29, type in 8-15, number in 0-7.
pub(crate) const fn ior(ty: u8, nr: u8, size: usize) -> u64 {
    (2u64 << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | nr as u64
}

/// `sizeof(struct xfs_bulk_ireq)`, `sizeof(struct xfs_bulkstat)`.
pub(crate) const IREQ_SIZE: usize = 64;
pub(crate) const BULKSTAT_SIZE: usize = 192;
/// `XFS_IOC_BULKSTAT`: the request struct is the 64-byte header followed by
/// a flexible array, so its size is the header's.
pub(crate) const XFS_IOC_BULKSTAT: u64 = ior(b'X', 127, IREQ_SIZE);
const _: () = assert!(XFS_IOC_BULKSTAT == 0x8040_587f);

/// Records asked per call.
const BATCH: usize = 1024;

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_ne_bytes([b[off], b[off + 1]])
}
fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(b[off..off + 4].try_into().unwrap_or([0; 4]))
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(b[off..off + 8].try_into().unwrap_or([0; 8]))
}

/// Decode one `struct xfs_bulkstat` (offsets from `xfs_fs.h`).
pub(crate) fn decode(rec: &[u8]) -> Result<InodeInfo, String> {
    if rec.len() < BULKSTAT_SIZE {
        return Err(format!("bulkstat record of {} bytes", rec.len()));
    }
    let version = u16_at(rec, 124);
    if version != 5 {
        return Err(format!("bulkstat record version {version}, not 5"));
    }
    let ctime_s = u64_at(rec, 48) as i64;
    Ok(InodeInfo {
        ino: u64_at(rec, 0),
        size: u64_at(rec, 8),
        mtime_ns: time_ns(u64_at(rec, 40) as i64, u32_at(rec, 84)),
        ctime_ns: (ctime_s >= 0).then(|| time_ns(ctime_s, u32_at(rec, 88))),
        mode: u32::from(u16_at(rec, 132)),
        nlink: u32_at(rec, 112),
    })
}

/// Whether CAP_SYS_ADMIN (bit 21) is in this process's effective set, per
/// `/proc/self/status`. Only a pre-check: in a user namespace it can be
/// set while the kernel's `capable()` still says no — the ioctl's EPERM is
/// the answer that counts.
pub(crate) fn has_cap_sys_admin() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("CapEff:"))
                .and_then(|h| u64::from_str_radix(h.trim(), 16).ok())
        })
        .is_some_and(|caps| caps & (1 << 21) != 0)
}

/// Every inode of the XFS filesystem mounted at `mount` that `keep`
/// accepts, from BULKSTAT: streamed, so only the kept ones use memory.
#[cfg(target_os = "linux")]
pub(crate) fn bulkstat(
    mount: &Path,
    keep: &dyn Fn(&InodeInfo) -> bool,
) -> io::Result<HashMap<u64, InodeInfo>> {
    use std::os::fd::AsRawFd;
    let dir = std::fs::File::open(mount)?;
    let mut buf = vec![0u8; IREQ_SIZE + BATCH * BULKSTAT_SIZE];
    let mut next = 0u64;
    let mut out = HashMap::new();
    loop {
        buf[..IREQ_SIZE].fill(0);
        buf[0..8].copy_from_slice(&next.to_ne_bytes());
        buf[12..16].copy_from_slice(&(BATCH as u32).to_ne_bytes());
        // SAFETY: `buf` is a live, writable buffer of IREQ_SIZE + BATCH *
        // BULKSTAT_SIZE bytes laid out as struct xfs_bulkstat_req with
        // icount = BATCH; the kernel writes at most that much.
        let rc = unsafe {
            libc::ioctl(
                dir.as_raw_fd(),
                XFS_IOC_BULKSTAT as libc::Ioctl,
                buf.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let ocount = u32_at(&buf, 16) as usize;
        if ocount == 0 {
            return Ok(out);
        }
        if ocount > BATCH {
            return Err(io::Error::other(
                "bulkstat returned more records than asked",
            ));
        }
        for i in 0..ocount {
            let off = IREQ_SIZE + i * BULKSTAT_SIZE;
            let info = decode(&buf[off..off + BULKSTAT_SIZE]).map_err(io::Error::other)?;
            if keep(&info) {
                out.insert(info.ino, info);
            }
        }
        next = u64_at(&buf, 0);
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn bulkstat(
    _mount: &Path,
    _keep: &dyn Fn(&InodeInfo) -> bool,
) -> io::Result<HashMap<u64, InodeInfo>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "XFS bulkstat is Linux only",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A record laid out as `struct xfs_bulkstat` in `xfs_fs.h`.
    fn record(ino: u64, size: u64, mtime: (i64, u32), ctime: (i64, u32), mode: u16) -> Vec<u8> {
        let mut r = vec![0u8; BULKSTAT_SIZE];
        let put = |r: &mut Vec<u8>, off: usize, b: &[u8]| r[off..off + b.len()].copy_from_slice(b);
        put(&mut r, 0, &ino.to_ne_bytes());
        put(&mut r, 8, &size.to_ne_bytes());
        put(&mut r, 40, &mtime.0.to_ne_bytes());
        put(&mut r, 48, &ctime.0.to_ne_bytes());
        put(&mut r, 84, &mtime.1.to_ne_bytes());
        put(&mut r, 88, &ctime.1.to_ne_bytes());
        put(&mut r, 112, &2u32.to_ne_bytes());
        put(&mut r, 124, &5u16.to_ne_bytes());
        put(&mut r, 132, &mode.to_ne_bytes());
        r
    }

    #[test]
    fn the_ioctl_number_is_ior_x_127_of_the_header() {
        assert_eq!(XFS_IOC_BULKSTAT, 0x8040_587f);
        assert_eq!(ior(b'X', 127, 64), 0x8040_587f);
    }

    #[test]
    fn a_record_decodes_per_the_header_layout() {
        let r = record(4242, 77, (1_700_000_000, 5), (1_700_000_100, 9), 0o100644);
        let i = decode(&r).unwrap();
        assert_eq!(
            i,
            InodeInfo {
                ino: 4242,
                size: 77,
                mtime_ns: 1_700_000_000_000_000_005,
                ctime_ns: Some(1_700_000_100_000_000_009),
                mode: 0o100644,
                nlink: 2,
            }
        );
        assert!(i.is_file() && !i.is_dir());
        let d = decode(&record(1, 0, (0, 0), (0, 0), 0o040755)).unwrap();
        assert!(d.is_dir() && !d.is_file());
    }

    #[test]
    fn times_match_the_walk_including_before_the_epoch() {
        // modified_ns gives 0 for any time before the epoch.
        let i = decode(&record(1, 0, (-5, 500), (-1, 0), 0o100644)).unwrap();
        assert_eq!((i.mtime_ns, i.ctime_ns), (0, None));
        // And the same nanoseconds as a real stat of a real file.
        let d = std::env::temp_dir().join(format!("dg-xfs-time-{}", std::process::id()));
        std::fs::write(&d, b"x").unwrap();
        let m = std::fs::metadata(&d).unwrap();
        use std::os::unix::fs::MetadataExt;
        let r = record(
            m.ino(),
            m.len(),
            (m.mtime(), m.mtime_nsec() as u32),
            (m.ctime(), m.ctime_nsec() as u32),
            m.mode() as u16,
        );
        let i = decode(&r).unwrap();
        let st = crate::index::stat_of(&m);
        assert_eq!((i.mtime_ns, i.ctime_ns), (st.mtime_ns, st.ctime_ns));
        let _ = std::fs::remove_file(&d);
    }

    #[test]
    fn wrong_versions_and_short_records_are_errors() {
        let mut r = record(1, 0, (0, 0), (0, 0), 0o100644);
        r[124] = 1;
        r[125] = 0;
        assert!(decode(&r).is_err(), "version 1 is not the v5 layout");
        assert!(decode(&r[..100]).is_err());
    }

    #[test]
    fn the_capability_pre_check_reads_cap_eff() {
        // Whatever this process has, the answer is the bit in CapEff.
        let s = std::fs::read_to_string("/proc/self/status").unwrap();
        let hex = s
            .lines()
            .find_map(|l| l.strip_prefix("CapEff:"))
            .unwrap()
            .trim()
            .to_string();
        let caps = u64::from_str_radix(&hex, 16).unwrap();
        assert_eq!(has_cap_sys_admin(), caps & (1 << 21) != 0);
    }

    #[test]
    fn without_the_capability_the_ioctl_is_refused_or_not_xfs() {
        // On this (non-XFS or unprivileged) test box the real call fails;
        // the scan turns that error into a walk.
        if has_cap_sys_admin() {
            return;
        }
        let e = bulkstat(&std::env::temp_dir(), &|_| true).unwrap_err();
        assert!(
            matches!(
                e.raw_os_error(),
                Some(libc::EPERM | libc::ENOTTY | libc::EINVAL)
            ),
            "{e}"
        );
    }
}
