//! btrfs rescan delta: the inodes of a subvolume changed since a
//! generation, from one `BTRFS_IOC_TREE_SEARCH_V2` over its INODE_ITEMs,
//! instead of a `stat` per file — so a rescan lists only folders whose inode
//! changed.
//!
//! From the kernel sources (fetched 2026-11-24):
//! - `fs/btrfs/ioctl.c` `btrfs_ioctl_tree_search_v2`: `if
//!   (!capable(CAP_SYS_ADMIN)) return -EPERM;` — root only.
//!   `BTRFS_IOC_GET_SUBVOL_INFO` has no such check (subvolume id, uuid and
//!   generation for anyone).
//! - `fs/btrfs/inode.c` `fill_inode_item`: every write of an inode item sets
//!   `transid = trans->transid` — data writes, chmod, rename, link and
//!   unlink all rewrite the item (its times or link count change), so an
//!   inode whose item `transid` is not above G did not change since G.
//! - `include/uapi/linux/btrfs.h`: the search key's transid range filters
//!   by the *metadata block's* generation, not the item's ("The number does
//!   not have any meaning related to the transaction in which an
//!   individual item … was created or changed"), and the range is a slice
//!   of the linear key space, so items of other types come back too: both
//!   are filtered here.
//! - `include/uapi/linux/btrfs_tree.h`: `struct btrfs_inode_item` (packed,
//!   160 bytes).
//!
//! Owner's decision (2026-11-24): diskghost is never given a privilege; this
//! runs only when the process already has CAP_SYS_ADMIN.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::xfs::{ior, time_ns, InodeInfo};

/// `_IOWR(ty, nr, size)` (asm-generic/ioctl.h): dir 3.
pub(crate) const fn iowr(ty: u8, nr: u8, size: usize) -> u64 {
    (3u64 << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | nr as u64
}

const MAGIC: u8 = 0x94;
/// `sizeof(struct btrfs_ioctl_search_key)`, of the v2 args (key + u64).
pub(crate) const SEARCH_KEY_SIZE: usize = 104;
pub(crate) const SEARCH_ARGS_V2_SIZE: usize = SEARCH_KEY_SIZE + 8;
/// `sizeof(struct btrfs_ioctl_search_header)`.
pub(crate) const SEARCH_HEADER_SIZE: usize = 32;
/// `sizeof(struct btrfs_ioctl_get_subvol_info_args)`.
pub(crate) const SUBVOL_INFO_SIZE: usize = 504;
/// `sizeof(struct btrfs_inode_item)`.
pub(crate) const INODE_ITEM_SIZE: usize = 160;
pub(crate) const BTRFS_IOC_TREE_SEARCH_V2: u64 = iowr(MAGIC, 17, SEARCH_ARGS_V2_SIZE);
pub(crate) const BTRFS_IOC_GET_SUBVOL_INFO: u64 = ior(MAGIC, 60, SUBVOL_INFO_SIZE);
const _: () = assert!(BTRFS_IOC_TREE_SEARCH_V2 == 0xC070_9411);
const _: () = assert!(BTRFS_IOC_GET_SUBVOL_INFO == 0x81F8_943C);
const INODE_ITEM_KEY: u32 = 1;
const FIRST_FREE_OBJECTID: u64 = 256;

/// Result buffer per call.
const BUF: usize = 256 * 1024;

fn u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap_or([0; 4]))
}
fn u64_le(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap_or([0; 8]))
}
fn u32_ne(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(b[off..off + 4].try_into().unwrap_or([0; 4]))
}
fn u64_ne(b: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(b[off..off + 8].try_into().unwrap_or([0; 8]))
}

/// A subvolume: who it is and the last transaction committed in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subvol {
    pub(crate) treeid: u64,
    pub(crate) uuid: String,
    pub(crate) generation: u64,
}

/// Decode `struct btrfs_ioctl_get_subvol_info_args` (host-endian ioctl
/// struct: treeid 0, name 8..264, parent_id 264, dirid 272, generation
/// 280, flags 288, uuid 296..312).
pub(crate) fn decode_subvol(b: &[u8]) -> Result<Subvol, String> {
    if b.len() < SUBVOL_INFO_SIZE {
        return Err(format!("subvol info of {} bytes", b.len()));
    }
    Ok(Subvol {
        treeid: u64_ne(b, 0),
        generation: u64_ne(b, 280),
        uuid: b[296..312].iter().map(|x| format!("{x:02x}")).collect(),
    })
}

/// Decode one `struct btrfs_inode_item` (on-disk, little-endian, packed:
/// generation 0, transid 8, size 16, nlink 40, mode 52, ctime 124 (sec,
/// nsec 132), mtime 136 (sec, nsec 144)) of inode `ino`; with its transid.
pub(crate) fn decode_inode_item(ino: u64, b: &[u8]) -> Result<(u64, InodeInfo), String> {
    if b.len() < INODE_ITEM_SIZE {
        return Err(format!("inode item of {} bytes", b.len()));
    }
    let ctime_s = u64_le(b, 124) as i64;
    Ok((
        u64_le(b, 8),
        InodeInfo {
            ino,
            size: u64_le(b, 16),
            mtime_ns: time_ns(u64_le(b, 136) as i64, u32_le(b, 144)),
            ctime_ns: (ctime_s >= 0).then(|| time_ns(ctime_s, u32_le(b, 132))),
            mode: u32_le(b, 52),
            nlink: u32_le(b, 40),
        },
    ))
}

/// Walk one search result buffer: `(header, item)` pairs; keep INODE_ITEMs
/// whose own transid is above `since`. Returns the last key seen.
pub(crate) fn parse_results(
    buf: &[u8],
    count: usize,
    since: u64,
    out: &mut HashMap<u64, InodeInfo>,
) -> Result<Option<(u64, u32, u64)>, String> {
    let mut off = 0;
    let mut last = None;
    for _ in 0..count {
        if off + SEARCH_HEADER_SIZE > buf.len() {
            return Err("search header past the buffer".into());
        }
        let objectid = u64_ne(buf, off + 8);
        let offset = u64_ne(buf, off + 16);
        let ty = u32_ne(buf, off + 24);
        let len = u32_ne(buf, off + 28) as usize;
        let item = off + SEARCH_HEADER_SIZE;
        if item + len > buf.len() {
            return Err("search item past the buffer".into());
        }
        if ty == INODE_ITEM_KEY {
            let (transid, info) = decode_inode_item(objectid, &buf[item..item + len])?;
            if transid > since {
                out.insert(objectid, info);
            }
        }
        last = Some((objectid, ty, offset));
        off = item + len;
    }
    Ok(last)
}

/// The next search start after key `(objectid, type, offset)`.
pub(crate) fn next_key(k: (u64, u32, u64)) -> Option<(u64, u32, u64)> {
    let (o, t, f) = k;
    if f < u64::MAX {
        Some((o, t, f + 1))
    } else if t < u32::from(u8::MAX) {
        Some((o, t + 1, 0))
    } else {
        o.checked_add(1).map(|o| (o, 0, 0))
    }
}

/// The subvolume holding `path` (no privilege needed).
#[cfg(target_os = "linux")]
pub(crate) fn subvol(path: &Path) -> io::Result<Subvol> {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(path)?;
    let mut b = vec![0u8; SUBVOL_INFO_SIZE];
    // SAFETY: `b` is SUBVOL_INFO_SIZE writable bytes, the struct's size.
    let rc = unsafe {
        libc::ioctl(
            f.as_raw_fd(),
            BTRFS_IOC_GET_SUBVOL_INFO as libc::Ioctl,
            b.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    decode_subvol(&b).map_err(io::Error::other)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn subvol(_path: &Path) -> io::Result<Subvol> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "btrfs is Linux only",
    ))
}

/// `syncfs` the filesystem holding `path`: commits the running
/// transaction, so inode items still held as delayed updates are in the
/// tree a search reads. No privilege needed.
#[cfg(target_os = "linux")]
pub(crate) fn sync_fs(f: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: a valid open fd; syncfs takes nothing else.
    if unsafe { libc::syncfs(f.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Every inode of the subvolume holding `path` whose item changed in a
/// transaction after `since` (CAP_SYS_ADMIN). The filesystem is synced
/// first (after G was read by the caller): every change made before the
/// scan is then committed and in the tree, with a transid above G.
#[cfg(target_os = "linux")]
pub(crate) fn changed_since(path: &Path, since: u64) -> io::Result<HashMap<u64, InodeInfo>> {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(path)?;
    sync_fs(&f)?;
    let mut buf = vec![0u8; SEARCH_ARGS_V2_SIZE + BUF];
    let mut out = HashMap::new();
    let mut start = (FIRST_FREE_OBJECTID, INODE_ITEM_KEY, 0u64);
    loop {
        buf[..SEARCH_ARGS_V2_SIZE].fill(0);
        let put =
            |b: &mut [u8], off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_ne_bytes());
        // tree_id 0 = the subvolume of the fd.
        put(&mut buf, 8, start.0); // min_objectid
        put(&mut buf, 16, u64::MAX); // max_objectid
        put(&mut buf, 24, start.2); // min_offset
        put(&mut buf, 32, u64::MAX); // max_offset
        put(&mut buf, 40, since.saturating_add(1)); // min_transid (block filter)
        put(&mut buf, 48, u64::MAX); // max_transid
        buf[56..60].copy_from_slice(&start.1.to_ne_bytes()); // min_type
        buf[60..64].copy_from_slice(&INODE_ITEM_KEY.to_ne_bytes()); // max_type
        buf[64..68].copy_from_slice(&u32::MAX.to_ne_bytes()); // nr_items
        put(&mut buf, SEARCH_KEY_SIZE, BUF as u64); // buf_size
                                                    // SAFETY: `buf` holds the 112-byte args followed by BUF bytes, and
                                                    // buf_size says BUF: the kernel writes no further.
        let rc = unsafe {
            libc::ioctl(
                f.as_raw_fd(),
                BTRFS_IOC_TREE_SEARCH_V2 as libc::Ioctl,
                buf.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let count = u32_ne(&buf, 64) as usize;
        if count == 0 {
            return Ok(out);
        }
        let last = parse_results(&buf[SEARCH_ARGS_V2_SIZE..], count, since, &mut out)
            .map_err(io::Error::other)?;
        // The key range is a slice of the linear key space: continue right
        // after the last key returned, whatever its type.
        match last.and_then(next_key) {
            Some(k) if k > start => start = k,
            Some(_) => return Err(io::Error::other("tree search made no progress")),
            None => return Ok(out),
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn changed_since(_path: &Path, _since: u64) -> io::Result<HashMap<u64, InodeInfo>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "btrfs is Linux only",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A `struct btrfs_inode_item` (little-endian, packed) per btrfs_tree.h.
    fn inode_item(
        transid: u64,
        size: u64,
        mtime: (i64, u32),
        ctime: (i64, u32),
        mode: u32,
    ) -> Vec<u8> {
        let mut b = vec![0u8; INODE_ITEM_SIZE];
        let put = |b: &mut Vec<u8>, off: usize, v: &[u8]| b[off..off + v.len()].copy_from_slice(v);
        put(&mut b, 0, &7u64.to_le_bytes()); // generation
        put(&mut b, 8, &transid.to_le_bytes());
        put(&mut b, 16, &size.to_le_bytes());
        put(&mut b, 40, &3u32.to_le_bytes()); // nlink
        put(&mut b, 52, &mode.to_le_bytes());
        put(&mut b, 124, &ctime.0.to_le_bytes());
        put(&mut b, 132, &ctime.1.to_le_bytes());
        put(&mut b, 136, &mtime.0.to_le_bytes());
        put(&mut b, 144, &mtime.1.to_le_bytes());
        b
    }

    /// One search result: header (transid, objectid, offset, type, len;
    /// host-endian) then the item.
    fn result(objectid: u64, ty: u32, offset: u64, item: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&99u64.to_ne_bytes());
        b.extend_from_slice(&objectid.to_ne_bytes());
        b.extend_from_slice(&offset.to_ne_bytes());
        b.extend_from_slice(&ty.to_ne_bytes());
        b.extend_from_slice(&(item.len() as u32).to_ne_bytes());
        b.extend_from_slice(item);
        b
    }

    #[test]
    fn ioctl_numbers_and_sizes() {
        assert_eq!(BTRFS_IOC_TREE_SEARCH_V2, 0xC070_9411);
        assert_eq!(BTRFS_IOC_GET_SUBVOL_INFO, 0x81F8_943C);
        assert_eq!(iowr(0x94, 17, 112), 0xC070_9411);
        assert_eq!((SEARCH_HEADER_SIZE, INODE_ITEM_SIZE), (32, 160));
    }

    #[test]
    fn an_inode_item_decodes_per_the_header_layout() {
        let (t, i) = decode_inode_item(
            300,
            &inode_item(42, 77, (1_700_000_000, 5), (1_700_000_100, 9), 0o100644),
        )
        .unwrap();
        assert_eq!(t, 42);
        assert_eq!(
            i,
            InodeInfo {
                ino: 300,
                size: 77,
                mtime_ns: 1_700_000_000_000_000_005,
                ctime_ns: Some(1_700_000_100_000_000_009),
                mode: 0o100644,
                nlink: 3,
            }
        );
        let (_, d) = decode_inode_item(1, &inode_item(1, 0, (-3, 0), (-1, 0), 0o040755)).unwrap();
        assert!(d.is_dir() && d.mtime_ns == 0 && d.ctime_ns.is_none());
        assert!(decode_inode_item(1, &[0u8; 100]).is_err());
    }

    #[test]
    fn only_inode_items_newer_than_the_generation_are_kept() {
        let mut buf = Vec::new();
        buf.extend(result(
            256,
            1,
            0,
            &inode_item(10, 1, (1, 0), (1, 0), 0o040755),
        ));
        buf.extend(result(256, 12, 256, b"an INODE_REF, not ours"));
        buf.extend(result(
            257,
            1,
            0,
            &inode_item(5, 2, (1, 0), (1, 0), 0o100644),
        ));
        buf.extend(result(
            258,
            1,
            0,
            &inode_item(11, 3, (1, 0), (1, 0), 0o100644),
        ));
        let mut out = HashMap::new();
        let last = parse_results(&buf, 4, 9, &mut out).unwrap();
        assert_eq!(last, Some((258, 1, 0)));
        let mut keys: Vec<_> = out.keys().copied().collect();
        keys.sort();
        assert_eq!(keys, vec![256, 258], "257 is older than generation 9");
        assert!(
            parse_results(&buf[..40], 2, 0, &mut HashMap::new()).is_err(),
            "cut off"
        );
    }

    #[test]
    fn the_search_continues_right_after_the_last_key() {
        assert_eq!(next_key((5, 1, 0)), Some((5, 1, 1)));
        assert_eq!(next_key((5, 12, u64::MAX)), Some((5, 13, 0)));
        assert_eq!(next_key((5, 255, u64::MAX)), Some((6, 0, 0)));
        assert_eq!(next_key((u64::MAX, 255, u64::MAX)), None);
    }

    #[test]
    fn subvolume_info_decodes_and_the_real_call_works_without_privilege_here() {
        let mut b = vec![0u8; SUBVOL_INFO_SIZE];
        b[0..8].copy_from_slice(&5u64.to_ne_bytes());
        b[280..288].copy_from_slice(&1234u64.to_ne_bytes());
        b[296..312].copy_from_slice(&[0xab; 16]);
        let s = decode_subvol(&b).unwrap();
        assert_eq!((s.treeid, s.generation), (5, 1234));
        assert_eq!(s.uuid, "ab".repeat(16));
        // On a btrfs /tmp the unprivileged call answers; elsewhere it is
        // refused as not btrfs — never EPERM.
        match subvol(&std::env::temp_dir()) {
            Ok(s) => assert!(s.generation > 0 && s.treeid >= 5, "{s:?}"),
            Err(e) => assert_ne!(e.raw_os_error(), Some(libc::EPERM), "{e}"),
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn syncing_the_filesystem_needs_no_privilege() {
        let f = std::fs::File::open(std::env::temp_dir()).unwrap();
        sync_fs(&f).unwrap();
    }

    #[test]
    fn tree_search_needs_cap_sys_admin() {
        if crate::xfs::has_cap_sys_admin() {
            return;
        }
        let e = changed_since(&std::env::temp_dir(), 0).unwrap_err();
        assert!(
            matches!(
                e.raw_os_error(),
                Some(libc::EPERM | libc::ENOTTY | libc::EINVAL)
            ),
            "{e}"
        );
    }
}
