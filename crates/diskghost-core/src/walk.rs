//! The parallel directory walk behind every scan, duplicate search and delete.
//!
//! Each directory is listed by one task on a thread pool owned by this walk
//! (so concurrent walks never starve each other), and every subdirectory found
//! becomes a new task. A file's size comes from the directory listing itself:
//! on Windows the `FindNextFile` data already carries size and attributes, so
//! no file is ever opened — that per-file open used to be most of a scan's
//! cost. The listing of one directory is handed to the caller as a [`Batch`]
//! while the walk goes on, so building a tree overlaps with the I/O.

#[cfg(any(unix, test))]
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Mutex};

use crate::{
    build_globset, is_excluded, is_mount_point, on_disk_size, FileEntry, Options, Progress,
};

/// What one directory contains.
pub(crate) struct Batch {
    pub(crate) dir: PathBuf,
    pub(crate) files: Vec<FileEntry>,
    /// Last-modified time of each of `files` (nanoseconds since the Unix
    /// epoch, 0 if unknown) — from the same listing, so it costs nothing.
    pub(crate) modified: Vec<u64>,
    /// Subdirectories (also those not descended into because of `max_depth`).
    pub(crate) subdirs: Vec<PathBuf>,
    /// The `subdirs` never descended into because they are out of bounds —
    /// on another filesystem than the root, on a remote one, or a remote
    /// cycle. Depth-limited folders are *not* here: theirs is a depth cut.
    pub(crate) bound: Vec<PathBuf>,
    /// Further names of a file already counted (a hard link, or a followed
    /// link to it): not charged again, but a consumer that needs every name
    /// — the persistent index — still sees them.
    pub(crate) aliases: Vec<PathBuf>,
    /// The `stat` the listing already took of each of `files`, then of each
    /// of `aliases`, in order (`files.len() + aliases.len()` entries) — so a
    /// consumer that needs size/mtime/dev/inode (the persistent index) does
    /// not `stat` every file a second time. On a network or FUSE volume
    /// each `stat` is a round trip. Empty when the walk had no per-file
    /// `stat` (the MFT engine): consumers then `stat` themselves.
    pub(crate) stats: Vec<crate::index::Stat>,
    /// Entries neither counted nor followed: symlinks and junctions when links
    /// are not followed, sockets, devices. A delete still has to remove them.
    pub(crate) others: Vec<PathBuf>,
    /// Entries (or this directory itself) that could not be read.
    pub(crate) skipped: u64,
    /// Bounded (path, reason) pairs behind `skipped`: enough to point a
    /// report at what was missed without copying a whole tree of names.
    pub(crate) skips: Vec<(PathBuf, &'static str)>,
}

impl Batch {
    /// Every name the batch holds — `files`, then `aliases` — with the
    /// `stat` the listing took of it, or `None` when the walk took none
    /// (the caller then has to `stat` the path itself).
    pub(crate) fn names_with_stats(
        &self,
    ) -> impl Iterator<Item = (&PathBuf, Option<&crate::index::Stat>)> {
        let have = self.stats.len() == self.files.len() + self.aliases.len();
        self.files
            .iter()
            .map(|f| &f.path)
            .chain(&self.aliases)
            .enumerate()
            .map(move |(i, p)| (p, if have { self.stats.get(i) } else { None }))
    }
}

/// Threads for a walk with `Options::threads == None`: directory listing is
/// latency-bound, so twice the cores keeps fast SSDs busy.
pub(crate) fn default_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cores * 2).clamp(4, 64)
}

/// Reader threads for a walk: the caller's explicit choice always wins;
/// otherwise a spinning disk gets
/// [`crate::rotational::ROTATIONAL_SCAN_THREADS`] — many parallel
/// directory reads on one head are a seek storm — and anything else the
/// machine default.
fn pick_threads(requested: Option<usize>, rotational: bool) -> usize {
    match requested {
        Some(t) => t.max(1),
        None if rotational => crate::rotational::ROTATIONAL_SCAN_THREADS,
        None => default_threads(),
    }
}

struct Ctx<'a> {
    root: &'a Path,
    glob: globset::GlobSet,
    max_depth: Option<usize>,
    follow: bool,
    progress: &'a Progress,
    tx: mpsc::Sender<Batch>,
    /// Canonical directories already entered — only kept when following
    /// links, where a link back up the tree would otherwise loop forever.
    seen: Option<Mutex<HashSet<PathBuf>>>,
    /// Inodes already counted — hard links and followed symlinks reach the
    /// same file through several names; it is charged only once.
    files_seen: Mutex<HashSet<(u64, u64)>>,
    /// Stay on the filesystem of `root`: mounted children are listed but
    /// never descended into (`du -x` semantics).
    same_file_system: bool,
    /// Device ids of remote filesystems ([`Options::skip_remote`]) other than
    /// the root's: listed when met, never descended into.
    remote: HashSet<u64>,
    /// Device of the root itself, when the root sits on a remote filesystem
    /// (the user's explicit choice — it is walked). Its directories get the
    /// lexical cycle guard: remote symlink loops answer with fresh inodes per
    /// path, so identity dedup can never fire there.
    remote_root: Option<u64>,
}

impl Ctx<'_> {
    fn first_visit(&self, dir: &Path) -> bool {
        let Some(seen) = &self.seen else { return true };
        let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        seen.lock().map(|mut s| s.insert(key)).unwrap_or(true)
    }
}

/// Walk `root`, calling `sink` on the caller's thread with each directory's
/// [`Batch`] as soon as it is listed (in no particular order). Honours
/// exclusions, max depth, link following and cancellation.
pub(crate) fn walk(
    root: &Path,
    opts: &Options,
    progress: &Progress,
    mut sink: impl FnMut(Batch),
) -> (Engine, Option<String>) {
    walk_dyn(root, opts, progress, &mut sink)
}

/// [`walk`] with a type-erased sink, so the MFT engine can hand folders it
/// cannot read itself back to the directory walk.
pub(crate) fn walk_dyn(
    root: &Path,
    opts: &Options,
    progress: &Progress,
    sink: &mut dyn FnMut(Batch),
) -> (Engine, Option<String>) {
    let remote = if opts.skip_remote {
        remote_devices()
    } else {
        HashSet::new()
    };
    walk_with_remote(root, opts, progress, sink, remote)
}

/// [`walk_dyn`] with the set of remote filesystems (device ids) given — the
/// seam that lets tests mark a local mount as remote.
fn walk_with_remote(
    root: &Path,
    opts: &Options,
    progress: &Progress,
    sink: &mut dyn FnMut(Batch),
    mut remote: HashSet<u64>,
) -> (Engine, Option<String>) {
    // Why the MFT was asked for but not used.
    let mut fallback = None;
    if opts.mft {
        #[cfg(windows)]
        if opts.follow_symlinks {
            fallback = Some("links are followed only by the directory walk".to_string());
        } else {
            match crate::mft::walk(root, opts, progress, sink) {
                Ok(()) => return (Engine::Mft, None),
                Err(e) => fallback = Some(e.to_string()),
            }
        }
        #[cfg(not(windows))]
        {
            fallback = Some("the MFT engine exists on Windows only".to_string());
        }
    }
    // Device of the root: the reference for both bounds. A root on a remote
    // filesystem is the user's explicit choice, so that one is walked.
    let root_dev = if opts.same_file_system || !remote.is_empty() {
        dev_of(None, root)
    } else {
        None
    };
    let remote_root = root_dev.filter(|d| remote.contains(d));
    if let Some(dev) = root_dev {
        remote.remove(&dev);
    }
    let threads = pick_threads(opts.threads, crate::rotational::is_rotational(root));
    let (tx, rx) = mpsc::channel();
    let ctx = Ctx {
        root,
        glob: build_globset(&opts.exclude),
        max_depth: opts.max_depth,
        follow: opts.follow_symlinks,
        progress,
        tx,
        seen: opts.follow_symlinks.then(|| Mutex::new(HashSet::new())),
        files_seen: Mutex::new(HashSet::new()),
        same_file_system: opts.same_file_system,
        remote,
        remote_root,
    };
    if ctx.follow {
        ctx.first_visit(root);
    }
    std::thread::scope(|ts| {
        ts.spawn(move || {
            let start = root.to_path_buf();
            match rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|i| format!("diskghost-walk-{i}"))
                .build()
            {
                Ok(pool) => pool.scope(|s| visit(s, &ctx, start, 0, root_dev)),
                Err(_) => rayon::scope(|s| visit(s, &ctx, start, 0, root_dev)),
            }
            // `ctx` (and with it the sender) drops here, ending the receiver loop.
        });
        for batch in rx {
            sink(batch);
        }
    });
    (Engine::Walk, fallback)
}

/// `/proc/self/mountinfo` as text. Read as bytes and decoded lossily: the
/// kernel escapes only space, tab, newline and backslash, so one mountpoint
/// with a non-UTF-8 name would make `read_to_string` fail for the whole
/// table — and every network mount would silently stop being recognised.
#[cfg(unix)]
fn read_mountinfo() -> std::io::Result<String> {
    std::fs::read("/proc/self/mountinfo").map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Device ids of the mounted network and FUSE filesystems, read from the
/// kernel's mount table — no call ever reaches the (possibly hung) share.
#[cfg(target_os = "linux")]
fn remote_devices() -> HashSet<u64> {
    read_mountinfo()
        .map(|info| {
            info.lines()
                .filter_map(parse_mountinfo_line)
                .filter(|(_, t)| is_remote_fs(t))
                .map(|(dev, _)| dev)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn remote_devices() -> HashSet<u64> {
    HashSet::new()
}

/// FUSE filesystems that only re-present local storage: walked like any
/// local disk. Every other FUSE filesystem (`gvfsd-fuse`, `sshfs`, `rclone`,
/// `s3fs`, the document portal, …) counts as remote — its daemon may answer
/// from the network, block, or loop.
#[cfg(any(unix, test))]
const LOCAL_FUSE: &[&str] = &[
    "mergerfs",
    "bindfs",
    "gocryptfs",
    "encfs",
    "cryfs",
    "securefs",
    "unionfs",
    "unionfs-fuse",
    "fuse-overlayfs",
    "ntfs-3g",
    "exfat",
    "exfat-fuse",
    "squashfuse",
];

/// Whether a filesystem type (as in `/proc/self/mountinfo`) is reached over
/// the network or through a non-local FUSE daemon. `fuseblk` (ntfs-3g and
/// exFAT on a local block device) is local. Compiled everywhere tests run:
/// the fixture mountinfo tables below are platform-independent.
#[cfg(any(unix, test))]
pub(crate) fn is_remote_fs(fstype: &str) -> bool {
    match fstype {
        "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "ncpfs" | "afs" | "ceph" | "glusterfs"
        | "lustre" | "davfs" | "fuse" => true,
        t => t
            .strip_prefix("fuse.")
            .is_some_and(|sub| !LOCAL_FUSE.contains(&sub)),
    }
}

/// Device id and filesystem type of one `/proc/self/mountinfo` line
/// (`id parent major:minor root mountpoint options [tags] - fstype ...`).
#[cfg(target_os = "linux")]
fn parse_mountinfo_line(line: &str) -> Option<(u64, &str)> {
    let (head, tail) = line.split_once(" - ")?;
    let (major, minor) = head.split_whitespace().nth(2)?.split_once(':')?;
    let fstype = tail.split_whitespace().next()?;
    Some((makedev(major.parse().ok()?, minor.parse().ok()?), fstype))
}

/// One line of `/proc/self/mountinfo` with every field the persistent index
/// needs to name a volume: `id parent major:minor root mountpoint options
/// [optional tags] - fstype source super-options`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// The device as `stat` reports it ([`makedev`] of `major:minor`).
    pub dev: u64,
    pub major: u64,
    pub minor: u64,
    /// The directory of the filesystem shown at the mountpoint: `/` for a
    /// plain mount, the subdirectory of a bind mount or btrfs subvolume.
    pub root: String,
    /// Where it is mounted (octal escapes such as `\040` decoded).
    pub mount_point: PathBuf,
    pub fstype: String,
    /// What was mounted: a device path such as `/dev/sda1`, or a pseudo
    /// source (`tmpfs`, `overlay`, a ZFS dataset name, …).
    pub source: String,
}

/// Parse one `/proc/self/mountinfo` line in full. The kernel escapes space,
/// tab, newline and backslash in paths as `\ooo` octal; those are decoded.
/// Compiled everywhere tests run: it is a pure parse, and the fixture-table
/// tests exercise it on every platform.
#[cfg(any(unix, test))]
pub(crate) fn parse_mountinfo_entry(line: &str) -> Option<MountEntry> {
    let (head, tail) = line.split_once(" - ")?;
    let mut head = head.split_whitespace();
    let (major, minor) = head.nth(2)?.split_once(':')?;
    let (major, minor) = (major.parse().ok()?, minor.parse().ok()?);
    let root = unescape_mountinfo(head.next()?);
    let mount_point = PathBuf::from(unescape_mountinfo(head.next()?));
    let mut tail = tail.split_whitespace();
    let fstype = unescape_mountinfo(tail.next()?);
    let source = unescape_mountinfo(tail.next()?);
    Some(MountEntry {
        dev: makedev(major, minor),
        major,
        minor,
        root,
        mount_point,
        fstype,
        source,
    })
}

/// One real local filesystem, as the drive list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMount {
    /// The device as `stat` reports it.
    pub dev: u64,
    /// Where the filesystem's own root is mounted (the first of `mounts`).
    pub mount_point: PathBuf,
    /// Filesystem type as the kernel names it (`ext4`, `xfs`, `fuseblk`, …).
    pub fstype: String,
    /// What was mounted (`/dev/sda1`, a ZFS dataset, …).
    pub source: String,
    /// Every place the filesystem's own root is mounted, in mount-table
    /// order — a filesystem mounted more than once keeps every mountpoint
    /// here (the drive tree lists them all under the one volume), while
    /// `mount_point` stays the first.
    pub mounts: Vec<PathBuf>,
}

/// Filesystem types that hold data on local storage. An explicit allowlist:
/// anything else (`tmpfs`, `proc`, `overlay`, `squashfs`, `autofs`, …) is
/// never a drive.
#[cfg(any(unix, test))]
const REAL_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "reiserfs", "jfs", "nilfs2", "bcachefs", "zfs",
    "ufs", "ffs", "hfs", "hfsplus", "apfs", "ntfs", "ntfs3", "fuseblk", "exfat", "vfat", "msdos",
    "udf", "iso9660", "ocfs2", "gfs2",
];

/// The real local filesystems of a `/proc/self/mountinfo` text, each once
/// (first mountpoint of a device wins), in mount-table order. Every further
/// whole-filesystem mount of an already seen device is remembered on its
/// `mounts` — a filesystem mounted twice is still one volume, with both
/// mountpoints.
#[cfg(any(unix, test))]
pub(crate) fn local_mounts_from(info: &str) -> Vec<LocalMount> {
    let mut slots: HashMap<u64, usize> = HashMap::new();
    let mut out: Vec<LocalMount> = Vec::new();
    for m in info
        .lines()
        .filter_map(parse_mountinfo_entry)
        // A bind mount or a btrfs subvolume shows part of a filesystem
        // that is (or could be) listed whole: counting it would count the
        // same data twice.
        .filter(|m| m.root == "/")
        .filter(|m| !is_remote_fs(&m.fstype) && is_storage_fs(&m.fstype))
    {
        match slots.get(&m.dev) {
            Some(&i) => out[i].mounts.push(m.mount_point),
            None => {
                slots.insert(m.dev, out.len());
                out.push(LocalMount {
                    dev: m.dev,
                    fstype: m.fstype,
                    source: m.source,
                    mounts: vec![m.mount_point.clone()],
                    mount_point: m.mount_point,
                });
            }
        }
    }
    out
}

/// Whether `fstype` holds data on local storage: [`REAL_FS`], or a FUSE
/// filesystem that only re-presents local storage ([`LOCAL_FUSE`]).
#[cfg(any(unix, test))]
fn is_storage_fs(fstype: &str) -> bool {
    REAL_FS.contains(&fstype)
        || fstype
            .strip_prefix("fuse.")
            .is_some_and(|sub| LOCAL_FUSE.contains(&sub))
}

/// Every real local filesystem mounted on this machine (Linux: from
/// `/proc/self/mountinfo`; empty where that table cannot be read).
#[cfg(unix)]
pub fn local_mounts() -> Vec<LocalMount> {
    read_mountinfo()
        .map(|info| local_mounts_from(&info))
        .unwrap_or_default()
}

/// Every mount in `/proc/self/mountinfo`, unfiltered — pseudo filesystems,
/// bind mounts and subvolumes included (the drive tree resolves the device
/// behind a mount the storage allowlist drops, such as a btrfs `/` on a
/// subvolume). Empty where the table cannot be read.
#[cfg(unix)]
pub fn mount_table() -> Vec<MountEntry> {
    read_mountinfo()
        .map(|info| info.lines().filter_map(parse_mountinfo_entry).collect())
        .unwrap_or_default()
}

/// Decode the kernel's `\ooo` octal escapes in a mountinfo field.
#[cfg(any(unix, test))]
fn unescape_mountinfo(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..=i + 3].iter().all(|c| (b'0'..=b'7').contains(c));
        if octal {
            let v = b[i + 1..=i + 3]
                .iter()
                .fold(0u32, |acc, c| acc * 8 + u32::from(c - b'0'));
            out.push(u8::try_from(v).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// glibc's `makedev`: the `st_dev` value of device `major:minor`.
#[cfg(any(unix, test))]
pub(crate) fn makedev(major: u64, minor: u64) -> u64 {
    ((major & 0xffff_f000) << 32)
        | ((major & 0x0000_0fff) << 8)
        | ((minor & 0xffff_ff00) << 12)
        | (minor & 0x0000_00ff)
}

/// Device id of a directory (following a link), if the platform has one.
#[cfg(unix)]
fn dev_of(meta: Option<&std::fs::Metadata>, path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    match meta {
        Some(m) => Some(m.dev()),
        None => std::fs::metadata(path).ok().map(|m| m.dev()),
    }
}

#[cfg(not(unix))]
fn dev_of(_meta: Option<&std::fs::Metadata>, _path: &Path) -> Option<u64> {
    None
}

/// A remote filesystem answers a symlink loop with a fresh inode per path —
/// no identity check (canonical path, device+inode) can ever fire. The only
/// signal left is the path itself: a tail of ≥3 consecutive copies of the
/// same period of ≥2 segments is a filesystem cycle (a remote `/sys` repeats
/// `subsystem/<name>`), not a real tree. Kept to period ≥2 so a legitimate
/// `a/a/a/…` style name stays walkable even on a share.
fn looks_like_cycle(path: &Path) -> bool {
    let segs: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    let n = segs.len();
    (2..=16).any(|p| {
        n >= 3 * p
            && segs[n - p..] == segs[n - 2 * p..n - p]
            && segs[n - p..] == segs[n - 3 * p..n - 2 * p]
    })
}

/// How a tree was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// Directory by directory.
    Walk,
    /// From the NTFS Master File Table (Windows only).
    #[cfg_attr(not(windows), allow(dead_code))]
    Mft,
}

/// List `dir` (device `dev`, when known) and spawn a task per subdirectory.
fn visit<'s, 'c: 's>(
    s: &rayon::Scope<'s>,
    ctx: &'c Ctx<'c>,
    dir: PathBuf,
    depth: usize,
    dev: Option<u64>,
) {
    if ctx.progress.cancelled() {
        return;
    }
    let mut b = Batch {
        dir,
        files: Vec::new(),
        modified: Vec::new(),
        subdirs: Vec::new(),
        bound: Vec::new(),
        aliases: Vec::new(),
        stats: Vec::new(),
        others: Vec::new(),
        skipped: 0,
        skips: Vec::new(),
    };
    // Stats of aliases, appended after the files' when the batch is sent.
    let mut alias_stats = Vec::new();
    let entries = match std::fs::read_dir(&b.dir) {
        Ok(rd) => rd,
        Err(_) => {
            b.skipped = 1;
            b.skips.push((b.dir.clone(), "unreadable directory"));
            let _ = ctx.tx.send(b);
            return;
        }
    };
    // Entries of this directory sit at `depth + 1`; descend only while the
    // next level is still within `max_depth` (the root is depth 0).
    let child_depth = depth + 1;
    let descend = ctx.max_depth.is_none_or(|m| child_depth < m);
    let mut bytes = 0u64;
    for entry in entries {
        if ctx.progress.cancelled() {
            break;
        }
        let Ok(entry) = entry else {
            b.skipped += 1;
            if b.skips.len() < 64 {
                b.skips.push((b.dir.clone(), "an entry could not be read"));
            }
            continue;
        };
        let path = entry.path();
        if !ctx.glob.is_empty() && is_excluded(&ctx.glob, ctx.root, &path) {
            continue;
        }
        let Ok(mut ft) = entry.file_type() else {
            b.skipped += 1;
            if b.skips.len() < 64 {
                b.skips.push((path, "unreadable file type"));
            }
            continue;
        };
        // Metadata straight from the listing (free on Windows), or of the
        // link target when following a link.
        let mut meta = None;
        if ft.is_symlink() {
            if !ctx.follow {
                b.others.push(path);
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(m) => {
                    ft = m.file_type();
                    meta = Some(m);
                }
                Err(_) => {
                    b.skipped += 1;
                    if b.skips.len() < 64 {
                        b.skips.push((path, "unreadable metadata"));
                    }
                    continue;
                }
            }
        }
        if ft.is_dir() {
            if ctx.follow && !ctx.first_visit(&path) {
                continue;
            }
            // Out of bounds — listed like a depth-limited folder (shown as
            // not measured) but never opened: another filesystem when the
            // walk stays on the root's (`same_file_system`), and a remote one
            // (`skip_remote`) however it is reached — mounted below, or via a
            // followed link. Remote and FUSE trees otherwise stall the scan
            // on every read, and gvfs loops through a remote `/sys` forever.
            let sub_dev =
                if ctx.same_file_system || !ctx.remote.is_empty() || ctx.remote_root.is_some() {
                    dev_of(meta.as_ref(), &path)
                } else {
                    None
                };
            let foreign = match (sub_dev, dev) {
                (Some(sub), Some(here)) => sub != here,
                // No device ids (not unix, or a failed stat): ask the path.
                _ => ctx.same_file_system && is_mount_point(&path),
            };
            // On a remote-rooted walk, identity dedup can never fire (fresh
            // inode per path), so the lexical cycle guard is the last bound.
            let on_remote_root = matches!((ctx.remote_root, sub_dev), (Some(r), Some(d)) if d == r);
            let in_bounds = !(ctx.same_file_system && foreign)
                && !sub_dev.is_some_and(|d| ctx.remote.contains(&d))
                && !(on_remote_root && looks_like_cycle(&path));
            if descend && !in_bounds {
                b.bound.push(path.clone());
            }
            if descend && in_bounds {
                let sub = path.clone();
                s.spawn(move |s| visit(s, ctx, sub, child_depth, sub_dev));
            }
            b.subdirs.push(path);
        } else if ft.is_file() {
            let meta = match meta.map_or_else(|| entry.metadata(), Ok) {
                Ok(m) => m,
                Err(_) => {
                    b.skipped += 1;
                    if b.skips.len() < 64 {
                        b.skips.push((path, "unreadable metadata"));
                    }
                    continue;
                }
            };
            if let Some(id) = file_key(&meta, ctx.follow) {
                let first = ctx
                    .files_seen
                    .lock()
                    .map(|mut s| s.insert(id))
                    .unwrap_or(true);
                if !first {
                    alias_stats.push(crate::index::stat_of(&meta));
                    b.aliases.push(path);
                    continue;
                }
            }
            let size = on_disk_size(&meta, &path);
            bytes += size;
            b.modified.push(modified_ns(&meta));
            b.stats.push(crate::index::stat_of(&meta));
            b.files.push(FileEntry {
                path,
                size,
                logical_size: meta.len(),
            });
        } else {
            b.others.push(path);
        }
    }
    ctx.progress
        .files
        .fetch_add(b.files.len() as u64, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    b.stats.append(&mut alias_stats);
    let _ = ctx.tx.send(b);
}

/// Device+inode identity of a file that may be reached again — through a
/// second name (hard link, `nlink > 1`) or, when links are followed, a second
/// path (symlink) — so it is charged once. `None` for files that can only be
/// met once: keeps the shared set (and its lock) off the common path.
#[cfg(unix)]
fn file_key(meta: &std::fs::Metadata, follow: bool) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (follow || meta.nlink() > 1).then(|| (meta.dev(), meta.ino()))
}

/// `file_key` elsewhere: no stable file identity in `std` (Windows'
/// `file_index` is unstable, `windows_by_handle`) — no dedup.
#[cfg(not(unix))]
fn file_key(_meta: &std::fs::Metadata, _follow: bool) -> Option<(u64, u64)> {
    None
}

/// Modification time in nanoseconds since the Unix epoch (0 if unknown).
pub(crate) fn modified_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

#[cfg(test)]
// Most cases need unix links/inodes; elsewhere the shared helpers sit unused.
#[cfg_attr(not(unix), allow(unused_imports, dead_code))]
mod tests {
    use super::*;
    use crate::{scan_with_progress, Options, Progress};
    use std::io::Write;

    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-walk-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content).unwrap();
    }

    fn allocated(path: &Path) -> u64 {
        let m = std::fs::metadata(path).unwrap();
        on_disk_size(&m, path)
    }

    /// Two names for one inode must add their bytes once, not twice — a
    /// backup tree made of `cp -al` / `--link-dest` hard links otherwise
    /// reports a multiple of the space it actually occupies.
    #[cfg(unix)]
    #[test]
    fn hardlinked_entries_share_one_charge() {
        let d = tmpdir("hardlink");
        let a = d.join("a.bin");
        write(&a, &[7u8; 100_000]);
        std::fs::hard_link(&a, d.join("b.bin")).unwrap();
        write(&d.join("c.bin"), &[3u8; 10_000]);

        let r = scan_with_progress(&d, 10, &Options::default(), &Progress::default());
        assert_eq!(r.total_files, 2, "a hard link is not a second file");
        assert_eq!(r.total_size, allocated(&a) + allocated(&d.join("c.bin")));
    }

    /// With links followed, a symlink to a file inside the scan reaches the
    /// same inode as the real path — it must not be charged again.
    #[cfg(unix)]
    #[test]
    fn followed_symlink_to_file_is_charged_once() {
        let d = tmpdir("symlink-file");
        let a = d.join("sub").join("a.bin");
        write(&a, &[9u8; 100_000]);
        std::os::unix::fs::symlink(&a, d.join("link")).unwrap();

        let opts = Options {
            follow_symlinks: true,
            ..Options::default()
        };
        let r = scan_with_progress(&d, 10, &opts, &Progress::default());
        assert_eq!(r.total_files, 1, "link and target are one file");
        assert_eq!(r.total_size, allocated(&a));
    }

    /// A symlink to an already-walked directory is not descended into twice
    /// (guards the existing `seen` set — regression coverage).
    #[cfg(unix)]
    #[test]
    fn followed_symlink_to_dir_is_not_rewalked() {
        let d = tmpdir("symlink-dir");
        write(&d.join("real").join("x.bin"), &[5u8; 100_000]);
        std::os::unix::fs::symlink(d.join("real"), d.join("alias")).unwrap();

        let opts = Options {
            follow_symlinks: true,
            ..Options::default()
        };
        let r = scan_with_progress(&d, 10, &opts, &Progress::default());
        assert_eq!(r.total_files, 1);
        assert_eq!(r.total_size, allocated(&d.join("real/x.bin")));
    }

    /// A plain directory on the parent filesystem is not a "drive"; scanning
    /// it separately from `/` would recount the same content.
    #[test]
    fn plain_directory_is_not_a_mount_point() {
        let d = tmpdir("not-a-mount");
        assert!(!crate::is_mount_point(&d));
        assert!(crate::is_mount_point(Path::new("/")));
    }

    /// `same_file_system` keeps a walk on the filesystem of its root: a
    /// mounted child is listed but never opened. A `/` scan must not walk a
    /// FUSE share or a second drive mounted below — live bug: a root scan
    /// descended into `gvfsd` sftp/smb mounts, counted remote filesystems,
    /// and could not be cancelled out of the blocked `read_dir`s.
    #[cfg(target_os = "linux")]
    #[test]
    fn same_file_system_does_not_descend_into_other_filesystems() {
        // Prefer a mount whose parent is an ordinary directory (a small
        // walk); otherwise any direct child of `/` will do.
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
        let mut mounted: Vec<PathBuf> = mounts
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1))
            .map(PathBuf::from)
            .filter(|p| p.is_dir() && crate::is_mount_point(p))
            .collect();
        mounted.sort();
        let (root, target) = mounted
            .iter()
            .filter_map(|m| m.parent().map(|p| (p.to_path_buf(), m.clone())))
            .find(|(p, _)| p != Path::new("/") && !crate::is_mount_point(p))
            .or_else(|| {
                mounted
                    .iter()
                    .find(|m| m.parent() == Some(Path::new("/")))
                    .map(|m| (PathBuf::from("/"), m.clone()))
            })
            .expect("no mounted directory in /proc/self/mounts");

        // Depth: the target would be descended into without the bound.
        let depth = target.strip_prefix(&root).unwrap().components().count();
        let collect = |opts: &Options| {
            let visited = Mutex::new(Vec::new());
            walk(&root, opts, &Progress::default(), |b| {
                visited.lock().unwrap().push(b.dir);
            });
            visited.into_inner().unwrap()
        };

        // Unbounded: the mounted child is walked (documents that the flag is
        // opt-in, not a change of the default traversal).
        let opts = Options {
            max_depth: Some(depth + 1),
            ..Options::default()
        };
        assert!(
            collect(&opts).contains(&target),
            "unbounded walk should descend into {target:?}"
        );

        let opts = Options {
            same_file_system: true,
            ..opts
        };
        assert!(
            !collect(&opts).contains(&target),
            "bounded walk descended into foreign filesystem {target:?}"
        );
    }

    /// Real mount points do compare different from their parent — take the
    /// first non-`/`, non-bind mount from the kernel table.
    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_mounts_are_mount_points() {
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
        let target = mounts
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1))
            .map(PathBuf::from)
            .find(|p| p != Path::new("/") && p.is_dir())
            .expect("no mounted directory in /proc/self/mounts");
        let child = target.join(format!("diskghost-not-mounted-{}", std::process::id()));
        if std::fs::create_dir(&child).is_ok() {
            assert!(!crate::is_mount_point(&child));
            let _ = std::fs::remove_dir(&child);
        }
        assert!(crate::is_mount_point(&target));
    }

    /// A readable mounted directory on another filesystem than `near`, and
    /// its device id — stands in for a remote share (no real network needed).
    /// Only local non-FUSE mounts are candidates, chosen from the mount table
    /// before any `stat`: the tests never touch a real share (gvfs, NFS, …).
    #[cfg(target_os = "linux")]
    fn foreign_mount(near: &Path) -> (PathBuf, u64) {
        use std::os::unix::fs::MetadataExt;
        let here = std::fs::metadata(near).unwrap().dev();
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
        mounts
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace().skip(1);
                let (path, fstype) = (f.next()?, f.next()?);
                (!fstype.starts_with("fuse") && !is_remote_fs(fstype)).then_some(path)
            })
            .map(PathBuf::from)
            .filter(|p| p != Path::new("/") && crate::is_mount_point(p))
            .filter(|p| std::fs::read_dir(p).is_ok())
            .find_map(|p| {
                let dev = std::fs::metadata(&p).ok()?.dev();
                (dev != here).then_some((p, dev))
            })
            .expect("no readable mount on another filesystem")
    }

    /// Directories `walk` listed under `root`, with `remote` marked remote.
    #[cfg(target_os = "linux")]
    fn listed(root: &Path, opts: &Options, remote: &[u64]) -> Vec<PathBuf> {
        let visited = Mutex::new(Vec::new());
        walk_with_remote(
            root,
            opts,
            &Progress::default(),
            &mut |b: Batch| visited.lock().unwrap().push(b.dir),
            remote.iter().copied().collect(),
        );
        visited.into_inner().unwrap()
    }

    /// Live incident (0.12.2, ~600 min, never finished): a GUI scan reached
    /// `/run/user/1000/gvfs/sftp:host=…` through a followed link and walked
    /// the remote host's `/sys` forever — gvfs serves remote symlinks as
    /// plain directories with a new inode per path, so no loop check can
    /// stop it. A followed link into a remote filesystem must not be opened.
    #[cfg(target_os = "linux")]
    #[test]
    fn followed_link_into_remote_fs_is_not_descended() {
        let d = tmpdir("link-remote");
        let (mount, dev) = foreign_mount(&d);
        let link = d.join("share");
        std::os::unix::fs::symlink(&mount, &link).unwrap();
        let opts = Options {
            follow_symlinks: true,
            skip_remote: true,
            max_depth: Some(2),
            ..Options::default()
        };

        // Not remote: the link is followed and its target listed (follow
        // keeps working for local filesystems).
        assert!(listed(&d, &opts, &[]).contains(&link));
        // Remote: listed as a folder of the root, never opened.
        let dirs = listed(&d, &opts, &[dev]);
        assert!(
            !dirs.iter().any(|p| p.starts_with(&link)),
            "walk descended into remote filesystem through {link:?}: {dirs:?}"
        );
        assert!(dirs.contains(&d), "the root itself is still listed");
    }

    /// A remote filesystem mounted below the root is not descended into even
    /// when `same_file_system` is off — the GUI scans ordinary folders
    /// without the bound, and that is how gvfs mounts were reached.
    #[cfg(target_os = "linux")]
    #[test]
    fn mounted_remote_fs_is_not_descended_without_same_file_system() {
        let (mount, dev) = foreign_mount(&std::env::temp_dir());
        let root = mount.parent().unwrap().to_path_buf();
        let opts = Options {
            skip_remote: true,
            max_depth: Some(mount.strip_prefix(&root).unwrap().components().count() + 1),
            ..Options::default()
        };
        // Local: walked (a second local disk inside the tree stays counted).
        assert!(listed(&root, &opts, &[]).contains(&mount));
        // Remote: not walked.
        assert!(
            !listed(&root, &opts, &[dev]).contains(&mount),
            "walk descended into remote mount {mount:?}"
        );
    }

    /// Scanning the remote share itself is the user's explicit choice: a root
    /// on a remote filesystem is walked.
    #[cfg(target_os = "linux")]
    #[test]
    fn remote_root_is_still_walked() {
        let (mount, dev) = foreign_mount(&std::env::temp_dir());
        let opts = Options {
            skip_remote: true,
            max_depth: Some(2),
            ..Options::default()
        };
        let dirs = listed(&mount, &opts, &[dev]);
        let has_subdir = std::fs::read_dir(&mount)
            .unwrap()
            .flatten()
            .any(|e| e.file_type().is_ok_and(|t| t.is_dir()));
        if has_subdir {
            assert!(dirs.len() > 1, "remote root was not walked: {dirs:?}");
        }
    }

    /// The live gvfs mount is classified remote from `/proc/self/mountinfo`
    /// (read-only table, no access to the share): `fuse.gvfsd-fuse`, NFS,
    /// SMB are remote; local and local-FUSE filesystems are not.
    #[test]
    fn remote_fs_types() {
        for t in [
            "fuse.gvfsd-fuse",
            "fuse.sshfs",
            "fuse.rclone",
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
        ] {
            assert!(is_remote_fs(t), "{t} should be remote");
        }
        for t in [
            "ext4",
            "btrfs",
            "xfs",
            "tmpfs",
            "fuseblk",
            "fuse.mergerfs",
            "fuse.gocryptfs",
            "ntfs3",
            "squashfs",
            "overlay",
        ] {
            assert!(!is_remote_fs(t), "{t} should be local");
        }
    }

    /// The remaining 0.12.3 risk, closed here: a scan rooted directly on a
    /// remote filesystem (the user's explicit choice) must still not run
    /// forever — gvfs serves remote symlink loops as plain directories with a
    /// fresh inode per path, so the only signal is the path itself. A tail of
    /// ≥3 consecutive copies of the same ≥2-segment period is a filesystem
    /// loop (remote `/sys` repeats `subsystem/<name>`), not a real tree, and
    /// is listed but never opened. A local root walks the identical shape in
    /// full — remote symlink loops do not exist there.
    #[cfg(target_os = "linux")]
    #[test]
    fn remote_root_stops_at_repeating_path_cycles() {
        use std::os::unix::fs::MetadataExt;
        let d = tmpdir("remote-cycle");
        // The remote `/sys` shape: one repeated <subsystem>/<name> pair.
        let mut deep = d.join("loop");
        for _ in 0..6 {
            deep = deep.join("sub").join("x");
        }
        std::fs::create_dir_all(&deep).unwrap();
        let plain = d.join("plain/a/b/c/d/e");
        std::fs::create_dir_all(&plain).unwrap();
        let dev = std::fs::metadata(&d).unwrap().dev();
        let opts = Options {
            skip_remote: true,
            ..Options::default()
        };

        // Same root marked remote: the cycle is bounded, the plain branch is not.
        let dirs = listed(&d, &opts, &[dev]);
        assert!(
            dirs.iter()
                .all(|p| p.components().filter(|c| c.as_os_str() == "x").count() <= 2),
            "walk descended past a repeating remote cycle: {dirs:?}"
        );
        assert!(dirs.contains(&plain), "plain deep tree must be walked");

        // Same root, local: everything is walked — the guard only fires on a
        // filesystem the walk itself classified as remote.
        let dirs = listed(&d, &opts, &[]);
        assert!(
            dirs.contains(&deep),
            "a local tree with repeated names must be walked in full: {dirs:?}"
        );
    }

    /// `major:minor` from mountinfo maps to the same id `stat` reports.
    #[cfg(target_os = "linux")]
    #[test]
    fn mountinfo_dev_matches_stat() {
        use std::os::unix::fs::MetadataExt;
        let line = "2188 190 0:78 / /run/user/1000/gvfs rw,nosuid,nodev,relatime \
                    shared:1525 - fuse.gvfsd-fuse gvfsd-fuse rw,user_id=1000";
        assert_eq!(
            parse_mountinfo_line(line),
            Some((makedev(0, 78), "fuse.gvfsd-fuse"))
        );
        // Against the kernel: `/proc` (btrfs is the known exception — its
        // mountinfo shows the superblock, `stat` the subvolume; never remote).
        let proc_dev = std::fs::metadata("/proc").unwrap().dev();
        let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        let line = info
            .lines()
            .find(|l| l.split_whitespace().nth(4) == Some("/proc"))
            .expect("/proc not in mountinfo");
        assert_eq!(parse_mountinfo_line(line), Some((proc_dev, "proc")));
    }

    /// A mount table with every kind of entry the drive list meets: only
    /// real local filesystem roots survive, each device once.
    const MOUNTINFO: &str = "\
22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw
23 1 0:22 / /sys rw,nosuid shared:6 - sysfs sysfs rw
29 1 0:26 / / rw,relatime shared:1 - btrfs /dev/nvme1n1p2 rw,ssd,subvol=/
30 29 0:26 /@snapshots /.snapshots rw,relatime shared:2 - btrfs /dev/nvme1n1p2 rw,subvol=/@snapshots
31 29 0:27 / /run rw,nosuid shared:7 - tmpfs tmpfs rw,mode=755
100 29 259:10 / /d/store2 rw,relatime shared:40 - ext4 /dev/nvme0n1p10 rw
101 29 259:4 / /mnt/fast rw,relatime shared:41 - xfs /dev/nvme1n1p4 rw
102 29 259:7 / /home rw,relatime shared:42 - xfs /dev/nvme0n1p9 rw
103 29 259:10 /nix-build /nix/var/nix/big-builds rw,relatime shared:43 - ext4 /dev/nvme0n1p10 rw
104 29 259:10 / /srv/models rw,relatime shared:44 - ext4 /dev/nvme0n1p10 rw
105 31 0:60 / /run/user/1000/gvfs rw,nosuid shared:50 - fuse.gvfsd-fuse gvfsd-fuse rw,user_id=1000
106 31 0:61 / /run/user/1000/doc rw,nosuid shared:51 - fuse.portal portal rw,user_id=1000
107 100 0:70 / /d/store2/docker/overlay2/abc/merged rw shared:60 - overlay overlay rw,lowerdir=/x
108 29 7:3 / /snap/core22/1380 ro,nodev shared:61 - squashfs /dev/loop3 ro
109 29 0:80 / /net/nas rw shared:62 - nfs4 nas:/export rw,vers=4.2
110 29 8:1 / /media/u/win rw shared:63 - fuseblk /dev/sda1 rw,user_id=0
111 29 0:81 / /srv/pool rw shared:64 - fuse.mergerfs pool rw
112 29 8:2 / /media/u/X9\\040Pro rw shared:65 - exfat /dev/sda2 rw
113 29 259:6 / /boot/efi rw shared:66 - vfat /dev/nvme1n1p6 rw
114 29 0:82 / /home/u/remote rw shared:67 - fuse.sshfs u@host:/ rw
115 29 0:83 / /mnt/share rw shared:68 - cifs //nas/share rw
116 29 0:84 / /proc/sys/fs/binfmt_misc rw shared:69 - autofs systemd-1 rw
";

    #[test]
    fn local_mounts_keep_real_local_filesystem_roots_once_each() {
        let m = local_mounts_from(MOUNTINFO);
        let got: Vec<(&str, &str)> = m
            .iter()
            .map(|m| (m.mount_point.to_str().unwrap(), m.fstype.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("/", "btrfs"),
                ("/d/store2", "ext4"),
                ("/mnt/fast", "xfs"),
                ("/home", "xfs"),
                ("/media/u/win", "fuseblk"),
                ("/srv/pool", "fuse.mergerfs"),
                ("/media/u/X9 Pro", "exfat"),
                ("/boot/efi", "vfat"),
            ]
        );
        let models = &m[1];
        assert_eq!(models.dev, makedev(259, 10));
        assert_eq!(models.source, "/dev/nvme0n1p10");
        // The same filesystem mounted at two places stays one volume — but
        // keeps every mountpoint, in mount-table order (`/srv/models` is a
        // second whole mount of 259:10; `/nix/var/nix/big-builds` is a bind
        // of a subtree and still does not count).
        assert_eq!(
            models.mounts,
            [PathBuf::from("/d/store2"), PathBuf::from("/srv/models")]
        );
        assert_eq!(models.mount_point, PathBuf::from("/d/store2"));
        for other in &m {
            if other.dev == models.dev {
                continue;
            }
            assert_eq!(other.mounts, [other.mount_point.as_path()]);
        }
    }

    #[test]
    fn local_mounts_never_list_pseudo_remote_or_sliced_filesystems() {
        let m = local_mounts_from(MOUNTINFO);
        let points: Vec<&Path> = m.iter().map(|m| m.mount_point.as_path()).collect();
        for gone in [
            "/proc",
            "/sys",
            "/run",
            "/.snapshots",
            "/nix/var/nix/big-builds",
            "/srv/models",
            "/run/user/1000/gvfs",
            "/run/user/1000/doc",
            "/d/store2/docker/overlay2/abc/merged",
            "/snap/core22/1380",
            "/net/nas",
            "/home/u/remote",
            "/mnt/share",
            "/proc/sys/fs/binfmt_misc",
        ] {
            assert!(
                !points.contains(&Path::new(gone)),
                "{gone} listed as a drive"
            );
        }
        // Garbage lines are ignored, not a panic.
        assert!(local_mounts_from("garbage\n\n1 2 x\n").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_mounts_of_this_machine_hold_no_pseudo_filesystem() {
        let m = local_mounts();
        let mut devs = HashSet::new();
        for e in &m {
            assert!(
                REAL_FS.contains(&e.fstype.as_str()) || e.fstype.starts_with("fuse."),
                "{e:?}"
            );
            assert!(!is_remote_fs(&e.fstype), "{e:?}");
            assert!(devs.insert(e.dev), "device listed twice: {e:?}");
        }
    }

    /// Threads follow the disk, not a constant: the caller's `--threads`
    /// always wins; otherwise a spinning drive gets a small reader pool
    /// and anything else the usual default.
    #[test]
    fn walk_threads_adapt_to_the_disk() {
        assert_eq!(pick_threads(Some(7), true), 7);
        assert_eq!(pick_threads(Some(7), false), 7);
        assert_eq!(pick_threads(Some(0), true), 1);
        assert_eq!(
            pick_threads(None, true),
            crate::rotational::ROTATIONAL_SCAN_THREADS
        );
        assert_eq!(pick_threads(None, false), default_threads());
    }
}
