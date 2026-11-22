//! Persistent duplicate-search index (SQLite) — RED stage: API stubs only.

use std::io;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::walk::MountEntry;
use crate::{Options, Progress};

/// Role of a location in later copy policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Normal,
    Master,
    Scratch,
}

/// The persistent index.
pub struct Index {
    #[allow(dead_code)]
    conn: Connection,
}

/// What one [`index_scan`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScanSummary {
    pub uuid: String,
    pub root: PathBuf,
    pub seen: u64,
    pub new: u64,
    pub changed: u64,
    pub unchanged: u64,
    pub vanished: u64,
    pub sampled: u64,
    pub skipped: u64,
    pub complete: bool,
}

/// One location as `diskghost index status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocationStatus {
    pub uuid: String,
    pub host: String,
    pub role: Role,
    pub root: PathBuf,
    pub online: bool,
    pub files: u64,
    pub bytes: u64,
    pub last_seen: i64,
}

impl Index {
    pub fn open(_path: &Path) -> io::Result<Index> {
        unimplemented!("M1")
    }
    pub fn status(&self) -> io::Result<Vec<LocationStatus>> {
        unimplemented!("M1")
    }
    pub fn forget(&self, _uuid: &str) -> io::Result<bool> {
        unimplemented!("M1")
    }
    pub fn configure(&self, _uuid: &str, _role: Option<Role>, _host: &str) -> io::Result<()> {
        unimplemented!("M1")
    }
}

pub fn validate_host(_host: &str) -> Result<(), String> {
    unimplemented!("M1")
}

pub fn volume_uuid(_path: &Path) -> io::Result<String> {
    unimplemented!("M1")
}

pub fn is_cow_fs(_path: &Path) -> bool {
    unimplemented!("M1")
}

fn is_cow_magic(_f_type: u64) -> bool {
    unimplemented!("M1")
}

/// A volume as named by the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Volume {
    pub(crate) key: String,
    pub(crate) mount_point: PathBuf,
}

fn resolve_volume(
    _mounts: &[MountEntry],
    _dev: u64,
    _path: &Path,
    _uuid_of: &dyn Fn(&str) -> Option<String>,
) -> Option<Volume> {
    unimplemented!("M1")
}

pub fn index_scan(
    idx: &Index,
    root: &Path,
    opts: &Options,
    progress: &Progress,
) -> io::Result<ScanSummary> {
    scan_observed(idx, root, opts, progress, &|_| {})
}

fn scan_observed(
    _idx: &Index,
    _root: &Path,
    _opts: &Options,
    _progress: &Progress,
    _on_read: &(dyn Fn(&Path) + Sync),
) -> io::Result<ScanSummary> {
    unimplemented!("M1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::parse_mountinfo_entry;
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime};

    // ---- fixtures -------------------------------------------------------

    /// A fresh directory owned by this test alone. Tests create and delete
    /// files only inside it — never anywhere else (owner's hard safety rule).
    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-index-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Move a file's mtime to a time it cannot already have.
    fn touch(path: &Path, secs_ago: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
            .unwrap();
    }

    fn open_db(dir: &Path) -> Index {
        Index::open(&dir.join("db").join("index.db")).unwrap()
    }

    /// Scan `root`, recording every file whose content was read.
    fn scan(idx: &Index, root: &Path) -> (ScanSummary, BTreeSet<PathBuf>) {
        let reads = Mutex::new(BTreeSet::new());
        let s = scan_observed(
            idx,
            root,
            &Options::default(),
            &Progress::default(),
            &|p: &Path| {
                reads.lock().unwrap().insert(p.to_path_buf());
            },
        )
        .unwrap();
        (s, reads.into_inner().unwrap())
    }

    /// (sample, full) of one indexed file, by absolute path.
    fn hashes(idx: &Index, abs: &Path) -> Option<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        let mut st = idx
            .conn
            .prepare(
                "SELECT f.sample, f.full, l.root, f.path FROM files f \
                 JOIN locations l ON l.id = f.location_id",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, Option<Vec<u8>>>(0)?,
                    r.get::<_, Option<Vec<u8>>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (s, f, root, path) = row.unwrap();
            if Path::new(&root).join(path) == abs {
                return Some((s, f));
            }
        }
        None
    }

    fn row_count(idx: &Index) -> i64 {
        idx.conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap()
    }

    // ---- volume identity (pure, fixture mountinfo) ----------------------

    const MOUNTINFO: &str = "\
35 2 0:30 / / rw,relatime shared:1 - btrfs /dev/nvme1n1p2 rw,ssd,subvolid=5,subvol=/
36 35 0:44 /@home /home rw,relatime shared:2 - btrfs /dev/nvme1n1p2 rw,subvolid=257,subvol=/@home
31 35 0:27 / /run rw,nosuid,nodev - tmpfs tmpfs rw,size=13179404k
2309 35 8:2 / /media/u/X9\\040Pro1 rw,nosuid,nodev,relatime shared:1706 - exfat /dev/sda2 rw
2702 35 8:3 / /media/u/dev_portable rw,relatime shared:1858 - ext4 /dev/sda3 rw
2703 35 8:3 /data /srv/data rw,relatime - ext4 /dev/sda3 rw
2840 35 0:85 / /var/lib/docker/merged rw,relatime - overlay overlay rw,lowerdir=/x
";

    fn mounts() -> Vec<MountEntry> {
        MOUNTINFO
            .lines()
            .filter_map(parse_mountinfo_entry)
            .collect()
    }

    fn by_uuid(source: &str) -> Option<String> {
        match source {
            "/dev/nvme1n1p2" => Some("1111-root".into()),
            "/dev/sda2" => Some("2222-X9".into()),
            "/dev/sda3" => Some("3333-portable".into()),
            _ => None,
        }
    }

    #[test]
    fn mountinfo_entry_decodes_octal_escapes() {
        let m = mounts();
        let x9 = m.iter().find(|e| e.source == "/dev/sda2").unwrap();
        assert_eq!(x9.mount_point, PathBuf::from("/media/u/X9 Pro1"));
        assert_eq!((x9.major, x9.minor), (8, 2));
        assert_eq!(x9.fstype, "exfat");
        assert_eq!(x9.root, "/");
    }

    #[test]
    fn plain_mount_resolves_to_filesystem_uuid() {
        let dev = crate::walk::makedev(8, 2);
        let v = resolve_volume(
            &mounts(),
            dev,
            Path::new("/media/u/X9 Pro1/photos/a.jpg"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(v.key, "2222-X9");
        assert_eq!(v.mount_point, PathBuf::from("/media/u/X9 Pro1"));
    }

    #[test]
    fn bind_mount_or_subvolume_keeps_its_filesystem_root_in_the_key() {
        // Same filesystem, different visible subtree: the files under /srv/data
        // are not the files under /media/u/dev_portable, so the key differs.
        let dev = crate::walk::makedev(8, 3);
        let v = resolve_volume(&mounts(), dev, Path::new("/srv/data/x"), &by_uuid).unwrap();
        assert_eq!(v.key, "3333-portable:/data");
        assert_eq!(v.mount_point, PathBuf::from("/srv/data"));
        let home = resolve_volume(
            &mounts(),
            crate::walk::makedev(0, 44),
            Path::new("/home/u/f"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(home.key, "1111-root:/@home");
    }

    #[test]
    fn pseudo_filesystems_fall_back_to_the_device_number() {
        let v = resolve_volume(
            &mounts(),
            crate::walk::makedev(0, 27),
            Path::new("/run/x"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(v.key, "dev:0:27");
        assert_eq!(v.mount_point, PathBuf::from("/run"));
        let o = resolve_volume(
            &mounts(),
            crate::walk::makedev(0, 85),
            Path::new("/var/lib/docker/merged/etc"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(o.key, "dev:0:85");
    }

    #[test]
    fn btrfs_nested_subvolume_belongs_to_the_enclosing_mount() {
        // `stat` on a nested btrfs subvolume reports an anonymous device that
        // mountinfo never lists; the volume is still the enclosing filesystem.
        let v = resolve_volume(
            &mounts(),
            crate::walk::makedev(0, 31),
            Path::new("/tmp/x"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(v.key, "1111-root");
        assert_eq!(v.mount_point, PathBuf::from("/"));
    }

    #[test]
    fn unknown_device_on_a_non_btrfs_mount_is_not_guessed() {
        assert_eq!(
            resolve_volume(
                &mounts(),
                crate::walk::makedev(9, 9),
                Path::new("/media/u/dev_portable/x"),
                &by_uuid
            ),
            None
        );
    }

    #[test]
    fn cow_filesystem_magics() {
        for m in [
            0x9123_683e,
            0x2fc1_2fc1,
            0x5846_5342,
            0xca45_1a4e,
            0x794c_7630,
        ] {
            assert!(is_cow_magic(m), "{m:#x}");
        }
        for m in [0xef53, 0x0102_1994, 0x6969, 0x5346_544e] {
            assert!(!is_cow_magic(m), "{m:#x}");
        }
    }

    #[test]
    fn live_volume_uuid_and_cow_probe_do_not_fail() {
        let d = tmpdir("live");
        assert!(!volume_uuid(&d).unwrap().is_empty());
        let _ = is_cow_fs(&d);
    }

    // ---- schema ---------------------------------------------------------

    #[test]
    fn schema_is_created_versioned_and_reopened() {
        let d = tmpdir("schema");
        let idx = open_db(&d);
        let v: i64 = idx
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        let mode: String = idx
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = idx
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        write(&d.join("t/a"), b"x");
        scan(&idx, &d.join("t"));
        drop(idx);
        let idx = open_db(&d);
        assert_eq!(row_count(&idx), 1);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let d = tmpdir("newer");
        drop(open_db(&d));
        let c = Connection::open(d.join("db/index.db")).unwrap();
        c.pragma_update(None, "user_version", 99).unwrap();
        drop(c);
        assert!(Index::open(&d.join("db/index.db")).is_err());
    }

    // ---- scanning -------------------------------------------------------

    #[test]
    fn first_scan_of_unique_sizes_reads_nothing() {
        let d = tmpdir("unique");
        let t = d.join("t");
        write(&t.join("a"), b"1");
        write(&t.join("sub/b"), b"22");
        write(&t.join("c"), b"333");
        let idx = open_db(&d);
        let (s, reads) = scan(&idx, &t);
        assert!(
            reads.is_empty(),
            "nothing collides, nothing is read: {reads:?}"
        );
        assert_eq!((s.seen, s.new, s.sampled), (3, 3, 0));
        assert!(s.complete);
        for f in ["a", "sub/b", "c"] {
            assert_eq!(hashes(&idx, &t.join(f)), Some((None, None)), "{f}");
        }
    }

    #[test]
    fn an_unchanged_file_is_never_read_again() {
        let d = tmpdir("unchanged");
        let t = d.join("t");
        write(&t.join("a"), &[7u8; 40_000]);
        write(&t.join("b"), &[7u8; 40_000]);
        write(&t.join("c"), b"unique");
        let idx = open_db(&d);
        let (s1, r1) = scan(&idx, &t);
        assert_eq!(r1, BTreeSet::from([t.join("a"), t.join("b")]));
        assert_eq!(s1.sampled, 2);
        let (s2, r2) = scan(&idx, &t);
        assert!(r2.is_empty(), "nothing changed, nothing re-read: {r2:?}");
        assert_eq!((s2.unchanged, s2.new, s2.changed), (3, 0, 0));
    }

    #[test]
    fn touching_one_file_rereads_only_that_file() {
        let d = tmpdir("touch");
        let t = d.join("t");
        write(&t.join("a"), &[7u8; 40_000]);
        write(&t.join("b"), &[7u8; 40_000]);
        write(&t.join("c"), b"unique");
        let idx = open_db(&d);
        scan(&idx, &t);
        let b_before = hashes(&idx, &t.join("b")).unwrap();
        assert!(b_before.0.is_some());
        touch(&t.join("a"), 3600);
        let (s, reads) = scan(&idx, &t);
        assert_eq!(reads, BTreeSet::from([t.join("a")]));
        assert_eq!((s.changed, s.unchanged, s.sampled), (1, 2, 1));
        assert_eq!(hashes(&idx, &t.join("b")).unwrap(), b_before);
        let a = hashes(&idx, &t.join("a")).unwrap();
        assert_eq!(a.0, b_before.0, "same content, same sample");
    }

    #[test]
    fn a_copy_found_later_samples_both_sides() {
        let d = tmpdir("twotrees");
        let (ta, tb) = (d.join("a"), d.join("b"));
        write(&ta.join("movie"), &[9u8; 50_000]);
        write(&tb.join("movie copy"), &[9u8; 50_000]);
        write(&tb.join("unique"), &[1u8; 123]);
        let idx = open_db(&d);
        // Alone, A's file collides with nothing: not read.
        let (_, r1) = scan(&idx, &ta);
        assert!(r1.is_empty());
        assert_eq!(hashes(&idx, &ta.join("movie")), Some((None, None)));
        // B brings a same-size file: both sides get their sample.
        let (s, r2) = scan(&idx, &tb);
        assert_eq!(
            r2,
            BTreeSet::from([ta.join("movie"), tb.join("movie copy")])
        );
        assert_eq!(s.sampled, 2);
        let a = hashes(&idx, &ta.join("movie")).unwrap();
        let b = hashes(&idx, &tb.join("movie copy")).unwrap();
        assert!(a.0.is_some());
        assert_eq!(a.0, b.0);
        assert_eq!(hashes(&idx, &tb.join("unique")), Some((None, None)));
        // A's rows survive B's scan: vanished is scoped to the walked subtree.
        assert_eq!(row_count(&idx), 3);
    }

    #[test]
    fn small_samples_are_also_full_hashes() {
        let d = tmpdir("smallfull");
        let t = d.join("t");
        write(&t.join("a"), b"same bytes");
        write(&t.join("b"), b"same bytes");
        let idx = open_db(&d);
        scan(&idx, &t);
        let (s, f) = hashes(&idx, &t.join("a")).unwrap();
        assert_eq!(s, f);
        assert_eq!(f.unwrap(), blake3::hash(b"same bytes").as_bytes().to_vec());
    }

    #[test]
    fn rescan_marks_seen_and_drops_vanished_rows() {
        let d = tmpdir("vanish");
        let t = d.join("t");
        write(&t.join("keep"), b"k");
        write(&t.join("gone"), b"gg");
        let idx = open_db(&d);
        scan(&idx, &t);
        std::fs::remove_file(t.join("gone")).unwrap(); // inside own tempdir
        let (s, _) = scan(&idx, &t);
        assert_eq!((s.vanished, s.unchanged), (1, 1));
        assert_eq!(hashes(&idx, &t.join("gone")), None);
        let (gen, seen): (i64, i64) = idx
            .conn
            .query_row(
                "SELECT (SELECT value FROM meta WHERE key = 'scan_generation'), \
                 (SELECT MAX(seen) FROM files)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(gen, seen, "the surviving row is marked seen by this scan");
    }

    #[test]
    fn an_incomplete_scan_deletes_nothing() {
        let d = tmpdir("cancelled");
        let t = d.join("t");
        write(&t.join("a"), b"a");
        write(&t.join("b"), b"bb");
        let idx = open_db(&d);
        scan(&idx, &t);
        let p = Progress::default();
        p.cancel();
        let s = scan_observed(&idx, &t, &Options::default(), &p, &|_| {}).unwrap();
        assert!(!s.complete);
        assert_eq!(s.vanished, 0);
        assert_eq!(row_count(&idx), 2);
    }

    // ---- locations ------------------------------------------------------

    fn insert_location(idx: &Index, uuid: &str, root: &str) {
        idx.conn
            .execute(
                "INSERT INTO locations (uuid, root, online, last_seen) VALUES (?1, ?2, 1, 1)",
                rusqlite::params![uuid, root],
            )
            .unwrap();
        let id = idx.conn.last_insert_rowid();
        for (i, size) in [10i64, 20, 30].into_iter().enumerate() {
            idx.conn
                .execute(
                    "INSERT INTO files (location_id, path, size, mtime_ns, dev, ino, seen) \
                     VALUES (?1, ?2, ?3, 0, 0, ?4, 1)",
                    rusqlite::params![id, format!("f{i}"), size, i as i64],
                )
                .unwrap();
        }
    }

    #[test]
    fn unplugged_volume_is_offline_and_keeps_its_rows() {
        let d = tmpdir("offline");
        let idx = open_db(&d);
        insert_location(&idx, "dead-beef", &d.join("not-mounted").to_string_lossy());
        let st = idx.status().unwrap();
        let l = st.iter().find(|l| l.uuid == "dead-beef").unwrap();
        assert!(!l.online);
        assert_eq!((l.files, l.bytes), (3, 60));
        assert_eq!(row_count(&idx), 3, "status never deletes");
    }

    #[test]
    fn an_existing_mountpoint_of_another_volume_is_offline() {
        // An unplugged disk leaves its (empty) mountpoint directory behind.
        let d = tmpdir("emptymp");
        let idx = open_db(&d);
        insert_location(&idx, "not-this-volume", &d.to_string_lossy());
        let st = idx.status().unwrap();
        assert!(!st[0].online);
    }

    #[test]
    fn a_scanned_volume_is_online() {
        let d = tmpdir("online");
        write(&d.join("t/a"), b"a");
        let idx = open_db(&d);
        let s = scan(&idx, &d.join("t")).0;
        let st = idx.status().unwrap();
        let l = st.iter().find(|l| l.uuid == s.uuid).unwrap();
        assert!(l.online);
        assert_eq!((l.files, l.bytes, l.role), (1, 1, Role::Normal));
        assert_eq!(l.host, "local");
    }

    #[test]
    fn forget_drops_the_location_and_cascades_to_its_files() {
        let d = tmpdir("forget");
        let idx = open_db(&d);
        insert_location(&idx, "a-uuid", "/nowhere-a");
        insert_location(&idx, "b-uuid", "/nowhere-b");
        assert!(idx.forget("a-uuid").unwrap());
        assert!(!idx.forget("a-uuid").unwrap());
        assert_eq!(row_count(&idx), 3);
        assert_eq!(idx.status().unwrap().len(), 1);
    }

    #[test]
    fn configure_sets_role_and_host() {
        let d = tmpdir("configure");
        let idx = open_db(&d);
        insert_location(&idx, "c-uuid", "/nowhere-c");
        idx.configure("c-uuid", Some(Role::Master), "localhost")
            .unwrap();
        let l = &idx.status().unwrap()[0];
        assert_eq!((l.role, l.host.as_str()), (Role::Master, "localhost"));
        idx.configure("c-uuid", None, "local").unwrap();
        assert_eq!(idx.status().unwrap()[0].role, Role::Master);
        assert!(idx.configure("missing", None, "local").is_err());
    }

    #[test]
    fn only_local_hosts_are_accepted() {
        assert!(validate_host("local").is_ok());
        assert!(validate_host("localhost").is_ok());
        let e = validate_host("nas.lan").unwrap_err();
        assert!(e.contains("remote locations not yet supported"), "{e}");
    }
}
