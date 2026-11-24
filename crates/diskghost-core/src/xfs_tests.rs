//! The XFS inode delta against a fake BULKSTAT (`testkit::FakeXfs`, which
//! reports every inode under the fake volume as `lstat` sees it — real
//! inode change times): a delta rescan must leave exactly the rows a full
//! walk would, list only folders whose inode changed, and walk — saying
//! why — without CAP_SYS_ADMIN or when BULKSTAT fails. Every file lives in
//! the test's own temp directory.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::index::{scan_with, Index, ScanSummary};
use crate::testkit::*;
use crate::{Options, Progress};

/// Estate whose location "A" is XFS; `cap`: whether the process has
/// CAP_SYS_ADMIN.
fn xfs_estate(label: &str, cap: bool) -> Estate {
    let e = Estate::new(label);
    {
        let mut x = e.xfs.lock().unwrap();
        x.volumes.insert("A".into());
        x.cap = cap;
    }
    e
}

fn scan(e: &Estate, o: &Options) -> ScanSummary {
    let root = e.loc("A");
    e.reads.lock().unwrap().clear();
    e.with_probe(|p| scan_with(&e.idx, &root, o, &Progress::default(), p))
        .unwrap()
}

/// (path, size, mtime, inode, inode change time, racy): everything a
/// scan records about a file except its hashes and generation.
type Row = (String, i64, i64, i64, Option<i64>, bool);

fn rows(idx: &Index) -> Vec<Row> {
    let mut st = idx
        .conn
        .prepare("SELECT path, size, mtime_ns, ino, ctime_ns, racy FROM files ORDER BY path")
        .unwrap();
    st.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// What a full walk (no capability, so no delta) records, in its own DB.
fn walked_rows(e: &Estate) -> Vec<Row> {
    let idx = Index::open(&e.dir.join(format!("walk-{}/index.db", rows(&e.idx).len()))).unwrap();
    let cap = std::mem::replace(&mut e.xfs.lock().unwrap().cap, false);
    let root = e.loc("A");
    let s = e
        .with_probe(|p| scan_with(&idx, &root, &Options::default(), &Progress::default(), p))
        .unwrap();
    e.xfs.lock().unwrap().cap = cap;
    assert!(s.complete && s.delta.is_none(), "{s:?}");
    rows(&idx)
}

fn dir_rows(e: &Estate) -> usize {
    e.idx
        .conn
        .query_row("SELECT COUNT(*) FROM dirs", [], |r| r.get::<_, i64>(0))
        .unwrap() as usize
}

fn has_table(e: &Estate) -> bool {
    e.idx
        .conn
        .query_row(
            "SELECT inode_ref_root IS NOT NULL FROM locations",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// Rows written long ago: not racy (racy rows are always looked at).
fn settle(e: &Estate) {
    e.idx.conn.execute("UPDATE files SET racy = 0", []).unwrap();
}

fn set_mtime(p: &Path, t: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// A tree, scanned once with the capability: the folder table is made.
fn ready(label: &str) -> Estate {
    let e = xfs_estate(label, true);
    let a = e.loc("A");
    write(&a.join("top.bin"), &content(1));
    write(&a.join("x/y/z/deep.bin"), &content(2));
    write(&a.join("x/y/z/twin.bin"), &content(2));
    write(&a.join("x/keep.bin"), &content(3));
    write(&a.join("m/one.bin"), &content(4));
    write(&a.join("m/two.bin"), &content(5));
    write(&a.join("sub/s1.bin"), &content(6));
    write(&a.join("sub/inner/s2.bin"), &content(7));
    // Folder change times within 2 s of a scan are not trusted (racy):
    // let these be older, as folders usually are.
    std::thread::sleep(Duration::from_millis(2100));
    let s = scan(&e, &Options::default());
    assert!(s.complete && s.delta.is_none(), "{s:?}");
    assert!(
        s.delta_note.as_deref().unwrap().contains("recorded one"),
        "{s:?}"
    );
    assert!(has_table(&e));
    assert_eq!(dir_rows(&e), 7, "A, x, x/y, x/y/z, m, sub, sub/inner");
    settle(&e);
    e
}

#[test]
fn a_deep_in_place_write_in_a_clean_folder_is_found_without_listing() {
    let e = ready("xfs-deep-write");
    // An in-place write changes only the file's inode: no folder's
    // change time moves, so no folder is listed. (Restoring folder mtimes
    // would itself change their ctime — and be seen.)
    let deep = e.loc("A").join("x/y/z/deep.bin");
    let mtime = std::fs::metadata(&deep).unwrap().modified().unwrap();
    std::fs::write(&deep, vec![9u8; BIG]).unwrap(); // same size, in place
    set_mtime(&deep, mtime - Duration::from_secs(1)); // and the time set back
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some(), "{s:?}");
    assert_eq!((s.changed, s.seen), (1, 1), "{s:?}");
    assert_eq!(rows(&e.idx), walked_rows(&e));
}

#[test]
fn every_kind_of_change_leaves_the_rows_a_walk_would() {
    let e = ready("xfs-all-changes");
    let a = e.loc("A");
    // create, delete, rename within and across folders, rename-over
    write(&a.join("x/y/new.bin"), &content(8));
    std::fs::remove_file(a.join("top.bin")).unwrap();
    std::fs::rename(a.join("x/keep.bin"), a.join("x/kept.bin")).unwrap();
    std::fs::rename(a.join("m/one.bin"), a.join("x/y/z/one.bin")).unwrap();
    write(&a.join("m/tmp"), &content(10));
    std::fs::rename(a.join("m/tmp"), a.join("m/two.bin")).unwrap();
    // mv of a subtree; a folder removed and made again under its name
    std::fs::rename(a.join("sub"), a.join("sub2")).unwrap();
    std::fs::remove_dir_all(a.join("x/y/z")).unwrap();
    write(&a.join("x/y/z/again.bin"), &content(11));
    // hard link added in another folder; a file replaced by a folder
    std::fs::hard_link(a.join("m/two.bin"), a.join("x/link.bin")).unwrap();
    write(&a.join("swap"), b"file");
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some(), "{s:?}");
    assert_eq!(rows(&e.idx), walked_rows(&e));
    std::fs::remove_file(a.join("swap")).unwrap();
    write(&a.join("swap/inside.bin"), &content(12));
    std::fs::remove_file(a.join("x/link.bin")).unwrap();
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some() && s.complete, "{s:?}");
    assert_eq!(rows(&e.idx), walked_rows(&e));
}

#[test]
fn nothing_changed_lists_nothing_and_reads_nothing() {
    let e = ready("xfs-quiet");
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some(), "{s:?}");
    assert_eq!(
        (s.seen, s.changed, s.new, s.vanished),
        (0, 0, 0, 0),
        "{s:?}"
    );
    assert!(e.reads().is_empty());
    assert_eq!(rows(&e.idx), walked_rows(&e));
}

#[test]
fn a_bulkstat_taken_before_a_change_never_hides_it_for_good() {
    // Folder times come from the BULKSTAT pass taken before listing. A
    // file made after that pass may be missed by this scan, but the folder
    // keeps its old time in the table, so the next scan lists it.
    let e = ready("xfs-capture-order");
    let a = e.loc("A");
    let before = inodes_under(&a);
    write(&a.join("m/late.bin"), &content(13));
    e.xfs.lock().unwrap().stale = Some(before);
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some(), "{s:?}");
    assert!(
        rows(&e.idx).iter().all(|r| r.0 != "m/late.bin"),
        "missed by this scan"
    );
    e.xfs.lock().unwrap().stale = None;
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some(), "{s:?}");
    assert!(rows(&e.idx).iter().any(|r| r.0 == "m/late.bin"), "{s:?}");
    assert_eq!(rows(&e.idx), walked_rows(&e));
}

#[test]
fn without_cap_sys_admin_it_walks_and_says_why() {
    let e = xfs_estate("xfs-no-cap", false);
    write(&e.loc("A").join("f.bin"), &content(1));
    for _ in 0..2 {
        let s = scan(&e, &Options::default());
        assert!(s.delta.is_none() && s.complete, "{s:?}");
        assert!(
            s.delta_note.as_deref().unwrap().contains("CAP_SYS_ADMIN"),
            "{s:?}"
        );
    }
    assert_eq!(e.xfs.lock().unwrap().calls, 0, "BULKSTAT never asked");
    assert!(!has_table(&e) && dir_rows(&e) == 0);
}

#[test]
fn a_bulkstat_error_means_a_walk_and_the_table_is_forgotten() {
    // EPERM although CapEff says yes (a user namespace), ENOTTY (old
    // kernel): either way, walk.
    for errno in [libc::EPERM, libc::ENOTTY] {
        let e = ready(&format!("xfs-errno-{errno}"));
        write(&e.loc("A").join("m/new.bin"), &content(14));
        e.xfs.lock().unwrap().errno = Some(errno);
        let s = scan(&e, &Options::default());
        assert!(s.delta.is_none() && s.complete, "{s:?}");
        assert!(
            s.delta_note.as_deref().unwrap().contains("BULKSTAT failed"),
            "{s:?}"
        );
        assert!(!has_table(&e) && dir_rows(&e) == 0);
        assert_eq!(rows(&e.idx), walked_rows(&e));
    }
}

#[test]
fn any_other_scan_forgets_the_table() {
    let e = ready("xfs-forget");
    let o = Options {
        exclude: vec!["*.tmp".into()],
        ..Options::default()
    };
    write(&e.loc("A").join("m/x.tmp"), &content(15));
    let s = scan(&e, &o);
    assert!(s.delta.is_none(), "{s:?}");
    assert!(!has_table(&e) && dir_rows(&e) == 0, "forgotten");
    // The next full scan walks (and makes a new table): x.tmp is indexed.
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_none(), "{s:?}");
    assert!(rows(&e.idx).iter().any(|r| r.0 == "m/x.tmp"));
    assert!(has_table(&e));
}

#[test]
fn a_scan_of_another_folder_does_not_use_the_table() {
    let e = ready("xfs-other-folder");
    let sub = e.loc("A").join("x");
    let s = e
        .with_probe(|p| scan_with(&e.idx, &sub, &Options::default(), &Progress::default(), p))
        .unwrap();
    assert!(
        s.delta.is_none(),
        "table belongs to the whole volume: {s:?}"
    );
}

#[test]
fn other_filesystems_are_untouched() {
    let e = Estate::new("xfs-not-xfs");
    e.xfs.lock().unwrap().cap = true;
    write(&e.loc("A").join("f.bin"), &content(1));
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_none() && s.delta_note.is_none(), "{s:?}");
    assert_eq!(e.xfs.lock().unwrap().calls, 0);
}

#[test]
fn a_folder_time_within_the_racy_window_is_not_trusted() {
    // Made just now: a later change in the same timestamp tick could leave
    // the time as is, so the table keeps none and the next scan lists it.
    let e = xfs_estate("xfs-racy-dirs", true);
    write(&e.loc("A").join("fresh/f.bin"), &content(1));
    scan(&e, &Options::default());
    let null: i64 = e
        .idx
        .conn
        .query_row(
            "SELECT COUNT(*) FROM dirs WHERE ctime_ns IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(null as usize, dir_rows(&e), "every fresh folder");
    settle(&e);
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some() && s.seen >= 1, "listed again: {s:?}");
}

#[test]
fn a_subtree_moved_to_another_folder_leaves_the_rows_a_walk_would() {
    let e = ready("xfs-cross-move");
    let a = e.loc("A");
    std::fs::rename(a.join("x/y"), a.join("m/y")).unwrap();
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some(), "{s:?}");
    assert_eq!(rows(&e.idx), walked_rows(&e));
}

/// A copy of the index as it is now (`VACUUM INTO`), to which a walk
/// is applied by [`walk_rescan_rows`]: what a walk would leave of *this*
/// index (a walk keeps rows under a mount it does not enter, so a fresh
/// database is not the baseline there).
fn copy_index(e: &Estate, n: u32) -> std::path::PathBuf {
    let p = e.dir.join(format!("copy-{n}.db"));
    e.idx
        .conn
        .execute("VACUUM INTO ?1", [p.to_str().unwrap()])
        .unwrap();
    p
}

fn walk_rescan_rows(e: &Estate, copy: &Path) -> Vec<Row> {
    let idx = Index::open(copy).unwrap();
    let cap = std::mem::replace(&mut e.xfs.lock().unwrap().cap, false);
    let root = e.loc("A");
    let s = e
        .with_probe(|p| scan_with(&idx, &root, &Options::default(), &Progress::default(), p))
        .unwrap();
    e.xfs.lock().unwrap().cap = cap;
    assert!(s.delta.is_none(), "{s:?}");
    rows(&idx)
}

/// A btrfs subvolume: another `st_dev` below the tree, made without root
/// (the test box's /tmp is btrfs). `None` where that is not possible.
fn other_device(p: &Path) -> Option<()> {
    let ok = std::process::Command::new("btrfs")
        .args(["subvolume", "create"])
        .arg(p)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    use std::os::unix::fs::MetadataExt;
    let parent = std::fs::metadata(p.parent()?).ok()?.dev();
    (ok && std::fs::metadata(p).ok()?.dev() != parent).then_some(())
}

#[test]
fn a_mountpoint_keeps_its_rows_and_an_unmount_is_noticed() {
    let e = xfs_estate("xfs-mounts", true);
    let a = e.loc("A");
    write(&a.join("plain/f.bin"), &content(1));
    write(&a.join("d/old.bin"), &content(2));
    if other_device(&a.join("vol")).is_none() {
        eprintln!("SKIP: no btrfs subvolume possible here (another st_dev without root)");
        return;
    }
    write(&a.join("vol/inside.bin"), &content(3));
    std::thread::sleep(Duration::from_millis(2100));
    scan(&e, &Options::default());
    settle(&e);
    let ino: i64 = e
        .idx
        .conn
        .query_row("SELECT ino FROM dirs WHERE path = 'vol'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ino, -1, "a mountpoint is kept as always-changed");
    // H1: a known folder replaced by another filesystem under its name —
    // a walk keeps rows under a mount it does not enter; so must the delta.
    std::fs::remove_dir_all(a.join("d")).unwrap();
    assert!(other_device(&a.join("d")).is_some());
    let copy = copy_index(&e, 1);
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some(), "{s:?}");
    assert_eq!(rows(&e.idx), walk_rescan_rows(&e, &copy));
    assert!(
        rows(&e.idx).iter().any(|r| r.0 == "d/old.bin"),
        "kept, as the walk keeps it"
    );
    // H2: the other filesystem goes (an unmount, here a subvolume removed)
    // and a plain folder is there instead: it is walked.
    std::fs::remove_dir_all(a.join("vol")).unwrap();
    write(&a.join("vol/now.bin"), &content(4));
    let copy = copy_index(&e, 2);
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some(), "{s:?}");
    assert!(rows(&e.idx).iter().any(|r| r.0 == "vol/now.bin"), "{s:?}");
    assert_eq!(rows(&e.idx), walk_rescan_rows(&e, &copy));
    let _ = std::fs::remove_dir_all(a.join("d"));
}

#[test]
fn a_folder_that_cannot_be_listed_keeps_its_rows_and_the_table_is_forgotten() {
    use std::os::unix::fs::PermissionsExt;
    let e = ready("xfs-unreadable");
    let m = e.loc("A").join("m");
    std::fs::set_permissions(&m, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&m).is_ok();
    let before = rows(&e.idx);
    let s = scan(&e, &Options::default());
    std::fs::set_permissions(&m, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        return; // root: a mode cannot make it unreadable
    }
    assert!(s.delta.is_some() && !s.complete, "{s:?}");
    assert_eq!(rows(&e.idx), before, "nothing dropped");
    assert!(!has_table(&e), "incomplete: the next scan walks");
}

#[test]
fn nested_mounts_under_clean_and_changed_folders_keep_the_walks_rows() {
    let e = xfs_estate("xfs-nested-mounts", true);
    let a = e.loc("A");
    write(&a.join("clean/f.bin"), &content(1));
    write(&a.join("busy/g.bin"), &content(2));
    if other_device(&a.join("clean/vol")).is_none() {
        eprintln!("SKIP: no btrfs subvolume possible here (another st_dev without root)");
        return;
    }
    write(&a.join("clean/vol/in.bin"), &content(3));
    std::thread::sleep(Duration::from_millis(2100));
    scan(&e, &Options::default());
    settle(&e);
    // A mount under a clean folder: looked at, nothing else listed.
    let copy = copy_index(&e, 1);
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some() && s.complete, "{s:?}");
    assert_eq!(rows(&e.idx), walk_rescan_rows(&e, &copy));
    // A new mount under a changed folder: noted, not entered.
    assert!(other_device(&a.join("busy/vol2")).is_some());
    write(&a.join("busy/vol2/x.bin"), &content(4));
    let copy = copy_index(&e, 2);
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some() && s.complete, "{s:?}");
    assert_eq!(rows(&e.idx), walk_rescan_rows(&e, &copy));
    let ino: i64 = e
        .idx
        .conn
        .query_row("SELECT ino FROM dirs WHERE path = 'busy/vol2'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(ino, -1);
}

#[test]
fn a_second_mount_of_the_same_filesystem_below_means_a_walk() {
    // A bind mount on the same device: the walk enters it, the delta
    // would not see it (no folder inode or time changes on mount).
    use std::os::unix::fs::MetadataExt;
    let e = ready("xfs-bind");
    let a = e.loc("A");
    let dev = std::fs::metadata(&a).unwrap().dev();
    e.xfs.lock().unwrap().mounts = vec![crate::walk::MountEntry {
        dev,
        major: 0,
        minor: 0,
        root: "/m".into(),
        mount_point: a.join("x/bound"),
        fstype: "xfs".into(),
        source: "/dev/fake".into(),
    }];
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_none() && s.complete, "{s:?}");
    assert!(
        s.delta_note
            .as_deref()
            .unwrap()
            .contains("second mount of this filesystem"),
        "{s:?}"
    );
    assert!(!has_table(&e), "forgotten");
}
