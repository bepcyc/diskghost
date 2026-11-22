//! Is a volume on a spinning disk? Whole-file hashing reads every byte of
//! every candidate: on an SSD many readers at once keep the device busy, on
//! a hard disk they make the head seek between files and the run crawls. So
//! the duplicate search asks this per location and reads rotational volumes
//! with only [`ROTATIONAL_HASH_THREADS`] readers, in path order.
//!
//! Resolution: the mount holding the path (`/proc/self/mountinfo`) → its
//! source device (`/dev/sda2`, `/dev/mapper/root` → `dm-0`) or, for a source
//! that is no device node, its `major:minor` via `/sys/dev/block` → the
//! kernel's `/sys/class/block/<name>` entry. A partition answers for its
//! parent disk (`sda2` → `sda`, `nvme0n1p2` → `nvme0n1`, `mmcblk0p1` →
//! `mmcblk0`); a device-mapper or md device for the disks below it
//! (`slaves/`): rotational when any of them is. Unknown (tmpfs, network,
//! anything not a block device) counts as not rotational — today's full
//! parallelism.

use std::path::Path;

/// Readers at once on one rotational volume: two keeps the queue fed while
/// one request is in flight, without turning the read into a seek storm.
pub(crate) const ROTATIONAL_HASH_THREADS: usize = 2;

/// Directory readers at once on one rotational volume: the same reasoning
/// as [`ROTATIONAL_HASH_THREADS`] — many parallel `readdir`/`stat` streams
/// on one head are a seek storm.
pub(crate) const ROTATIONAL_SCAN_THREADS: usize = 2;

/// Whether block device `name` (a `/sys/class/block` entry) spins, read from
/// the sysfs tree rooted at `sys`. `None` when unknown.
pub(crate) fn device_rotational(sys: &Path, name: &str) -> Option<bool> {
    rotational_at(sys, name, 0)
}

/// Stacks deeper than this (LVM on LUKS on md on …) are not followed.
const MAX_STACK: u32 = 8;

fn rotational_at(sys: &Path, name: &str, depth: u32) -> Option<bool> {
    if depth > MAX_STACK || name.is_empty() || name.contains('/') {
        return None;
    }
    let class = sys.join("class/block").join(name);
    // The class entry links to the device directory; a partition's sits
    // inside its disk's, and only the disk has the queue.
    let dev = std::fs::canonicalize(&class).ok()?;
    let dev = if dev.join("partition").is_file() {
        dev.parent()?.to_path_buf()
    } else {
        dev
    };
    // A stacked device (dm, md) spins when any disk below it does.
    if let Ok(slaves) = std::fs::read_dir(dev.join("slaves")) {
        let below: Vec<Option<bool>> = slaves
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .map(|s| rotational_at(sys, &s, depth + 1))
            .collect();
        if !below.is_empty() {
            return if below.contains(&Some(true)) {
                Some(true)
            } else if below.iter().all(Option::is_some) {
                Some(false)
            } else {
                None
            };
        }
    }
    match std::fs::read_to_string(dev.join("queue/rotational"))
        .ok()?
        .trim()
    {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Kernel name of the block device behind a mount: the device node the
/// `source` resolves to, else `major:minor` looked up in `sys/dev/block`.
pub(crate) fn device_name(sys: &Path, source: &str, major: u64, minor: u64) -> Option<String> {
    let name_of = |p: &Path| {
        std::fs::canonicalize(p)
            .ok()?
            .file_name()?
            .to_str()
            .map(String::from)
    };
    if source.starts_with("/dev/") {
        if let Some(n) = name_of(Path::new(source)) {
            return Some(n);
        }
    }
    // Anonymous devices (major 0: tmpfs, btrfs subvolumes, FUSE…) are no
    // block device at all.
    if major == 0 {
        return None;
    }
    name_of(&sys.join("dev/block").join(format!("{major}:{minor}")))
}

/// Whether `path` lives on a rotational (spinning) disk. `false` when that
/// cannot be told.
pub fn is_rotational(path: &Path) -> bool {
    is_rotational_with(path, &|name| device_rotational(Path::new("/sys"), name))
}

/// [`is_rotational`] with the per-device lookup injected (tests need no
/// spinning disk): the mount of `path` is resolved for real, its device
/// name handed to `lookup`.
pub(crate) fn is_rotational_with(path: &Path, lookup: &dyn Fn(&str) -> Option<bool>) -> bool {
    mount_device(path).is_some_and(|name| lookup(&name) == Some(true))
}

/// Kernel name of the block device holding `path`, when it is one.
pub(crate) fn mount_device(path: &Path) -> Option<String> {
    let m = crate::index::mount_of(path)?;
    device_name(Path::new("/sys"), &m.source, m.major, m.minor)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-rot-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    /// A fake `/sys`: `devices/…/block/<disk>[/<part>]` directories with the
    /// `/sys/class/block/<name>` symlinks the kernel makes.
    struct FakeSys(PathBuf);

    impl FakeSys {
        fn new(label: &str) -> FakeSys {
            let root = tmpdir(label);
            std::fs::create_dir_all(root.join("class/block")).unwrap();
            std::fs::create_dir_all(root.join("dev/block")).unwrap();
            FakeSys(root)
        }

        fn link(&self, name: &str, target: &Path) {
            std::os::unix::fs::symlink(target, self.0.join("class/block").join(name)).unwrap();
        }

        fn disk(&self, name: &str, rotational: &str) -> PathBuf {
            let dir = self.0.join("devices/pci0/block").join(name);
            std::fs::create_dir_all(dir.join("queue")).unwrap();
            std::fs::write(dir.join("queue/rotational"), rotational).unwrap();
            self.link(name, &dir);
            dir
        }

        fn partition(&self, disk: &Path, name: &str) -> PathBuf {
            let dir = disk.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("partition"), "2\n").unwrap();
            self.link(name, &dir);
            dir
        }

        /// A device-mapper / md device stacked on `slaves`.
        fn stacked(&self, name: &str, slaves: &[&str]) {
            let dir = self.0.join("devices/virtual/block").join(name);
            std::fs::create_dir_all(dir.join("queue")).unwrap();
            // dm devices report 0 whatever lies below: the slaves decide.
            std::fs::write(dir.join("queue/rotational"), "0\n").unwrap();
            std::fs::create_dir_all(dir.join("slaves")).unwrap();
            for s in slaves {
                std::os::unix::fs::symlink(
                    self.0.join("class/block").join(s),
                    dir.join("slaves").join(s),
                )
                .unwrap();
            }
            self.link(name, &dir);
        }
    }

    #[test]
    fn partitions_answer_for_their_disk() {
        let sys = FakeSys::new("parts");
        let sda = sys.disk("sda", "1\n");
        sys.partition(&sda, "sda2");
        let nvme = sys.disk("nvme0n1", "0\n");
        sys.partition(&nvme, "nvme0n1p2");
        let mmc = sys.disk("mmcblk0", "0\n");
        sys.partition(&mmc, "mmcblk0p1");

        assert_eq!(device_rotational(&sys.0, "sda"), Some(true));
        assert_eq!(device_rotational(&sys.0, "sda2"), Some(true));
        assert_eq!(device_rotational(&sys.0, "nvme0n1p2"), Some(false));
        assert_eq!(device_rotational(&sys.0, "mmcblk0p1"), Some(false));
        assert_eq!(device_rotational(&sys.0, "sdz9"), None, "unknown device");
        std::fs::remove_dir_all(&sys.0).ok();
    }

    #[test]
    fn stacked_devices_answer_for_the_disks_below() {
        let sys = FakeSys::new("dm");
        let sdb = sys.disk("sdb", "1\n");
        sys.partition(&sdb, "sdb1");
        let nvme = sys.disk("nvme0n1", "0\n");
        sys.partition(&nvme, "nvme0n1p3");
        sys.stacked("dm-0", &["sdb1"]); // LUKS on a hard disk
        sys.stacked("dm-1", &["nvme0n1p3"]); // LUKS on NVMe
        sys.stacked("md0", &["nvme0n1p3", "sdb1"]); // mixed: the slow one wins
        sys.stacked("dm-2", &["dm-0"]); // LVM on LUKS on a hard disk

        assert_eq!(device_rotational(&sys.0, "dm-0"), Some(true));
        assert_eq!(device_rotational(&sys.0, "dm-1"), Some(false));
        assert_eq!(device_rotational(&sys.0, "md0"), Some(true));
        assert_eq!(device_rotational(&sys.0, "dm-2"), Some(true));
        std::fs::remove_dir_all(&sys.0).ok();
    }

    #[test]
    fn device_names_come_from_the_node_or_the_device_number() {
        let sys = FakeSys::new("names");
        let sdc = sys.disk("sdc", "1\n");
        let part = sys.partition(&sdc, "sdc1");
        std::os::unix::fs::symlink(&part, sys.0.join("dev/block/8:33")).unwrap();
        // No device node: the number names it.
        assert_eq!(
            device_name(&sys.0, "nas:/share", 8, 33).as_deref(),
            Some("sdc1")
        );
        // Pseudo filesystems have neither.
        assert_eq!(device_name(&sys.0, "tmpfs", 0, 27), None);
        std::fs::remove_dir_all(&sys.0).ok();
    }

    #[test]
    fn the_lookup_decides_and_unknown_is_not_rotational() {
        let d = tmpdir("inject");
        // Where the temp dir is on no block device (tmpfs), there is
        // nothing to ask and the answer is "not rotational".
        let named = mount_device(&d).is_some();
        assert_eq!(is_rotational_with(&d, &|_| Some(true)), named);
        assert!(!is_rotational_with(&d, &|_| Some(false)));
        assert!(!is_rotational_with(&d, &|_| None));
        // The real lookup answers without panicking wherever this runs.
        let _ = is_rotational(&d);
        std::fs::remove_dir_all(&d).ok();
    }

    /// On a real machine: `DISKGHOST_ROTATIONAL_CHECK="/path=1,/other=0"`
    /// (read-only: one `stat` and sysfs reads per path).
    #[test]
    #[ignore = "needs real disks; run with DISKGHOST_ROTATIONAL_CHECK"]
    fn real_disks_when_asked() {
        let spec = std::env::var("DISKGHOST_ROTATIONAL_CHECK").unwrap_or_default();
        for pair in spec.split(',').filter(|p| !p.is_empty()) {
            let (path, want) = pair.rsplit_once('=').unwrap();
            let got = is_rotational(Path::new(path));
            eprintln!(
                "{path}: device {:?}, rotational {got}",
                mount_device(Path::new(path))
            );
            assert_eq!(got, want == "1", "{path}");
        }
    }
}
