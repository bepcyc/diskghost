//! The ZFS rescan delta against a fake `zfs` (see `testkit::FakeZfs`):
//! a delta rescan must leave exactly the rows a full walk would, and fall
//! back to the walk — saying why — whenever the delta cannot be proven
//! complete. Every file lives in the test's own temp directory.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::index::{scan_with, Index, ScanSummary};
use crate::testkit::*;
use crate::{Options, Progress};

const DS: &str = "tank/A";

fn now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Estate whose location "A" is the dataset tank/A, with one snapshot
/// taken 100 s ago.
fn zfs_estate(label: &str) -> (Estate, i64) {
    let e = Estate::new(label);
    let creation = now_s() - 100;
    {
        let mut z = e.zfs.lock().unwrap();
        z.datasets.insert("A".into(), DS.into());
        z.state.insert(
            DS.into(),
            ("111".into(), format!("{DS}@s1\t5001\t{creation}\n")),
        );
    }
    (e, creation)
}

fn opts(zfs_diff: bool) -> Options {
    Options {
        zfs_diff,
        ..Options::default()
    }
}

fn scan_into(e: &Estate, idx: &Index, o: &Options) -> ScanSummary {
    let root = e.loc("A");
    e.reads.lock().unwrap().clear();
    e.with_probe(|p| scan_with(idx, &root, o, &Progress::default(), p))
        .unwrap()
}

/// `stream_bytes` of libzfs_diff.c: printable ASCII except space and `\`
/// as-is, everything else `\ooo` (four octal digits).
fn esc(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str()
        .as_bytes()
        .iter()
        .map(|&c| {
            if c > b' ' && c != b'\\' && c < 0o177 {
                (c as char).to_string()
            } else {
                format!("\\{c:04o}")
            }
        })
        .collect()
}

fn set_diff(e: &Estate, lines: &[String]) {
    e.zfs.lock().unwrap().diff = Some(Ok(lines.iter().map(|l| format!("{l}\n")).collect()));
}

/// Pretend every row was written long before the snapshot: its inode
/// change time far in the past, not racy.
fn age_rows(e: &Estate, creation: i64) {
    e.idx
        .conn
        .execute(
            "UPDATE files SET ctime_ns = ?1, racy = 0",
            [(creation - 1000) * 1_000_000_000],
        )
        .unwrap();
}

type Row = (String, i64, i64, i64);

fn rows(idx: &Index) -> Vec<Row> {
    let mut st = idx
        .conn
        .prepare("SELECT path, size, mtime_ns, ino FROM files ORDER BY path")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// What a full walk of the same folder records, in a database of its own.
fn walked_rows(e: &Estate) -> Vec<Row> {
    let idx = Index::open(&e.dir.join("walk/index.db")).unwrap();
    let s = scan_into(e, &idx, &Options::default());
    assert!(s.complete && s.delta.is_none());
    rows(&idx)
}

fn sample(e: &Estate, rel: &str) -> Option<Vec<u8>> {
    e.row("A", rel).2
}

#[test]
fn a_delta_rescan_leaves_exactly_the_rows_a_walk_would() {
    let (e, creation) = zfs_estate("zfs-delta");
    let a = e.loc("A");
    write(&a.join("keep.bin"), &content(1));
    write(&a.join("mod.bin"), &content(2));
    write(&a.join("del.bin"), &content(3));
    write(&a.join("ren.bin"), &content(4));
    write(&a.join("dir/x.bin"), &content(5));
    write(&a.join("dir/y.bin"), &content(5));

    let s = scan_into(&e, &e.idx, &opts(true));
    assert!(s.complete && s.delta.is_none(), "{s:?}");
    assert!(
        s.delta_note.as_deref().unwrap().contains("no snapshot"),
        "{s:?}"
    );
    let r: (String, String) = e
        .idx
        .conn
        .query_row("SELECT zfs_ref, zfs_ref_root FROM locations", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(r, (format!("{DS}@s1"), String::new()));
    age_rows(&e, creation);
    let x_sample = sample(&e, "dir/x.bin");
    assert!(x_sample.is_some(), "x and y collide: sampled");

    std::fs::write(a.join("mod.bin"), b"now shorter").unwrap();
    std::fs::remove_file(a.join("del.bin")).unwrap();
    std::fs::rename(a.join("ren.bin"), a.join("ren 2.bin")).unwrap();
    std::fs::rename(a.join("dir"), a.join("dir2")).unwrap();
    write(&a.join("new.bin"), &content(6));
    set_diff(
        &e,
        &[
            format!("M\t/\t{}/", esc(&a)),
            format!("M\tF\t{}", esc(&a.join("mod.bin"))),
            format!("-\tF\t{}", esc(&a.join("del.bin"))),
            format!(
                "R\tF\t{}\t{}",
                esc(&a.join("ren.bin")),
                esc(&a.join("ren 2.bin"))
            ),
            format!("R\t/\t{}\t{}", esc(&a.join("dir")), esc(&a.join("dir2"))),
            format!("+\tF\t{}", esc(&a.join("new.bin"))),
        ],
    );

    let s = scan_into(&e, &e.idx, &opts(true));
    assert_eq!(
        s.delta.as_deref(),
        Some("zfs diff since tank/A@s1: 6 changes"),
        "{s:?}"
    );
    assert!(s.complete, "{s:?}");
    // Looked at: mod, ren 2, new, dir2/x, dir2/y — not keep.bin.
    assert_eq!(s.seen, 5, "{s:?}");
    // A renamed folder's files kept their hashes (moved, then unchanged).
    assert_eq!(sample(&e, "dir2/x.bin"), x_sample);
    assert!(
        e.reads().iter().all(|p| !p.ends_with("x.bin")),
        "{:?}",
        e.reads()
    );
    assert_eq!(rows(&e.idx), walked_rows(&e));
}

#[test]
fn a_file_born_after_the_snapshot_and_removed_again_is_dropped() {
    // Absent from the snapshot and from the live dataset: no diff line.
    // Its inode change time (newer than the snapshot) is what finds it.
    let (e, creation) = zfs_estate("zfs-born-late");
    let a = e.loc("A");
    write(&a.join("old.bin"), &content(1));
    write(&a.join("late.bin"), &content(2));
    scan_into(&e, &e.idx, &opts(true));
    e.idx
        .conn
        .execute(
            "UPDATE files SET ctime_ns = ?1, racy = 0 WHERE path = 'old.bin'",
            [(creation - 1000) * 1_000_000_000],
        )
        .unwrap();
    std::fs::remove_file(a.join("late.bin")).unwrap();
    set_diff(&e, &[]);
    let s = scan_into(&e, &e.idx, &opts(true));
    assert!(s.delta.is_some(), "{s:?}");
    assert_eq!((s.vanished, s.seen), (1, 0), "{s:?}");
    assert_eq!(rows(&e.idx), walked_rows(&e));
}

#[test]
fn a_row_of_unknown_change_time_is_looked_at() {
    // Rows from before schema v7 have no ctime: never trusted unseen.
    let (e, creation) = zfs_estate("zfs-null-ctime");
    let a = e.loc("A");
    write(&a.join("f.bin"), &content(1));
    scan_into(&e, &e.idx, &opts(true));
    age_rows(&e, creation);
    e.idx
        .conn
        .execute("UPDATE files SET ctime_ns = NULL", [])
        .unwrap();
    std::fs::write(a.join("f.bin"), b"changed, and no diff line says so").unwrap();
    set_diff(&e, &[]);
    let s = scan_into(&e, &e.idx, &opts(true));
    assert_eq!((s.seen, s.changed), (1, 1), "{s:?}");
}

fn fallback(e: &Estate, want: &str) {
    let s = scan_into(e, &e.idx, &opts(true));
    assert!(s.delta.is_none(), "{s:?}");
    let note = s.delta_note.clone().unwrap_or_default();
    assert!(note.contains(want), "{want:?} not in {note:?}");
    assert!(s.complete, "the walk ran: {s:?}");
    assert_eq!(rows(&e.idx), walked_rows(e));
}

/// A dataset scanned once (reference s1 recorded) and a file changed.
fn ready(label: &str) -> Estate {
    let (e, creation) = zfs_estate(label);
    write(&e.loc("A").join("f.bin"), &content(1));
    scan_into(&e, &e.idx, &opts(true));
    age_rows(&e, creation);
    std::fs::write(e.loc("A").join("f.bin"), b"changed").unwrap();
    set_diff(&e, &[]); // would miss the change if trusted
    e
}

#[test]
fn a_pruned_snapshot_means_a_walk() {
    let e = ready("zfs-pruned");
    e.zfs.lock().unwrap().state.insert(
        DS.into(),
        ("111".into(), format!("{DS}@s2\t6001\t{}\n", now_s() - 50)),
    );
    fallback(&e, "no longer exists");
}

#[test]
fn a_snapshot_made_again_under_the_same_name_means_a_walk() {
    let e = ready("zfs-remade");
    e.zfs.lock().unwrap().state.insert(
        DS.into(),
        ("111".into(), format!("{DS}@s1\t9999\t{}\n", now_s() - 50)),
    );
    fallback(&e, "made again");
}

#[test]
fn a_dataset_with_another_guid_means_a_walk() {
    let e = ready("zfs-guid");
    e.zfs.lock().unwrap().state.get_mut(DS).unwrap().0 = "222".into();
    fallback(&e, "another guid");
}

#[test]
fn no_permission_for_zfs_diff_means_a_walk() {
    let e = ready("zfs-perm");
    e.zfs.lock().unwrap().diff = Some(Err(
        "The sys_config privilege or diff delegated permission is needed".into(),
    ));
    fallback(&e, "diff delegated permission");
}

#[test]
fn diff_paths_outside_the_mountpoint_mean_a_walk() {
    let e = ready("zfs-outside");
    set_diff(&e, &["M\tF\t/elsewhere/f.bin".to_string()]);
    fallback(&e, "not under the mountpoint");
}

#[test]
fn an_unparsable_diff_means_a_walk() {
    let e = ready("zfs-garbage");
    set_diff(&e, &["?\tF\t/x".to_string()]);
    fallback(&e, "unknown change type");
}

#[test]
fn no_snapshot_before_the_scan_means_the_next_scan_walks_too() {
    let (e, _) = zfs_estate("zfs-none");
    e.zfs
        .lock()
        .unwrap()
        .state
        .insert(DS.into(), ("111".into(), String::new()));
    write(&e.loc("A").join("f.bin"), &content(1));
    let s = scan_into(&e, &e.idx, &opts(true));
    assert!(
        s.delta_note
            .as_deref()
            .unwrap()
            .contains("diskghost does not create snapshots"),
        "{s:?}"
    );
    fallback(&e, "no snapshot");
}

#[test]
fn without_the_option_zfs_is_never_asked() {
    let (e, _) = zfs_estate("zfs-off");
    write(&e.loc("A").join("f.bin"), &content(1));
    let s = scan_into(&e, &e.idx, &Options::default());
    assert!(s.delta.is_none() && s.delta_note.is_none(), "{s:?}");
    assert!(e.zfs.lock().unwrap().calls.is_empty());
}

#[test]
fn not_a_dataset_or_options_a_diff_cannot_honour_mean_a_walk() {
    let e = Estate::new("zfs-not-zfs");
    write(&e.loc("A").join("f.bin"), &content(1));
    let s = scan_into(&e, &e.idx, &opts(true));
    assert!(
        s.delta_note
            .as_deref()
            .unwrap()
            .contains("not a ZFS dataset"),
        "{s:?}"
    );
    let e = ready("zfs-exclude");
    let o = Options {
        zfs_diff: true,
        exclude: vec!["*.tmp".into()],
        ..Options::default()
    };
    let s = scan_into(&e, &e.idx, &o);
    assert!(s.delta.is_none());
    assert!(
        s.delta_note.as_deref().unwrap().contains("--exclude"),
        "{s:?}"
    );
}

#[test]
fn a_path_that_cannot_be_read_keeps_its_rows_and_the_next_scan_walks() {
    use std::os::unix::fs::PermissionsExt;
    let (e, creation) = zfs_estate("zfs-unreadable");
    let a = e.loc("A");
    write(&a.join("locked/f.bin"), &content(1));
    scan_into(&e, &e.idx, &opts(true));
    age_rows(&e, creation);
    let locked = a.join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::symlink_metadata(locked.join("f.bin")).is_ok();
    set_diff(&e, &[format!("M\tF\t{}", esc(&locked.join("f.bin")))]);
    let s = scan_into(&e, &e.idx, &opts(true));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        return; // running as root: a mode cannot make it unreadable
    }
    assert!(s.delta.is_some(), "{s:?}");
    assert!(!s.complete && s.vanished == 0, "{s:?}");
    assert_eq!(rows(&e.idx).len(), 1, "the row is kept");
    let r: Option<String> = e
        .idx
        .conn
        .query_row("SELECT zfs_ref FROM locations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(r, None, "incomplete: no reference, the next scan walks");
}

/// On a real dataset: set DISKGHOST_ZFS_TEST_DIR to a folder on a ZFS
/// dataset that has a snapshot older than a minute, and run with
/// `--ignored` as a user allowed `zfs diff` (root, or `zfs allow <user>
/// diff,mount <dataset>`). Ignored by default: CI has no pool, and making a
/// file-backed pool needs root.
#[test]
#[ignore = "needs a ZFS dataset with a snapshot and the zfs diff permission"]
fn real_zfs_delta_matches_a_walk() {
    let dir = PathBuf::from(
        std::env::var("DISKGHOST_ZFS_TEST_DIR")
            .expect("DISKGHOST_ZFS_TEST_DIR: a folder on a ZFS dataset"),
    );
    let db = std::env::temp_dir().join(format!("dg-zfs-real-{}", std::process::id()));
    let idx = Index::open(&db.join("a.db")).unwrap();
    let o = opts(true);
    let first = crate::index::index_scan(&idx, &dir, &o, &Progress::default()).unwrap();
    assert!(first.complete, "{first:?}");
    std::thread::sleep(Duration::from_secs(1));
    let second = crate::index::index_scan(&idx, &dir, &o, &Progress::default()).unwrap();
    assert!(second.delta.is_some(), "{second:?}");
    let walk = Index::open(&db.join("b.db")).unwrap();
    crate::index::index_scan(&walk, &dir, &Options::default(), &Progress::default()).unwrap();
    assert_eq!(rows(&idx), rows(&walk));
    let _ = std::fs::remove_dir_all(&db);
}

fn has_ref(e: &Estate) -> bool {
    e.idx
        .conn
        .query_row("SELECT zfs_ref IS NOT NULL FROM locations", [], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn a_scan_with_exclude_records_no_reference() {
    // Its rows lack the excluded files: a delta from it would never add
    // the ones that did not change since the snapshot.
    let (e, creation) = zfs_estate("zfs-excl-ref");
    let a = e.loc("A");
    write(&a.join("f.bin"), &content(1));
    write(&a.join("x.tmp"), &content(2));
    let o = Options {
        zfs_diff: true,
        exclude: vec!["*.tmp".into()],
        ..Options::default()
    };
    let s = scan_into(&e, &e.idx, &o);
    assert!(s.complete, "{s:?}");
    assert!(!has_ref(&e));
    age_rows(&e, creation);
    set_diff(&e, &[]);
    fallback(&e, "no snapshot");
}

#[test]
fn any_other_scan_of_the_location_forgets_the_reference() {
    let (e, creation) = zfs_estate("zfs-forget-ref");
    let a = e.loc("A");
    write(&a.join("f.bin"), &content(1));
    write(&a.join("x.tmp"), &content(2));
    scan_into(&e, &e.idx, &opts(true));
    assert!(has_ref(&e));
    // A plain scan with --exclude drops x.tmp's row.
    let o = Options {
        exclude: vec!["*.tmp".into()],
        ..Options::default()
    };
    scan_into(&e, &e.idx, &o);
    assert!(!has_ref(&e), "forgotten");
    age_rows(&e, creation);
    set_diff(&e, &[]);
    // Trusting the old reference would leave x.tmp unindexed.
    fallback(&e, "no snapshot");
}

#[test]
fn a_file_with_several_names_means_a_walk() {
    // zfs diff names one path per object: the other name's row would
    // stay stale if the delta were trusted.
    let (e, creation) = zfs_estate("zfs-hardlink");
    let a = e.loc("A");
    write(&a.join("one.bin"), &content(1));
    std::fs::hard_link(a.join("one.bin"), a.join("two.bin")).unwrap();
    scan_into(&e, &e.idx, &opts(true));
    age_rows(&e, creation);
    std::fs::write(a.join("one.bin"), b"changed through one name").unwrap();
    set_diff(&e, &[format!("M\tF\t{}", esc(&a.join("one.bin")))]);
    fallback(&e, "several names");
}

#[test]
fn a_link_count_change_means_a_walk() {
    let e = ready("zfs-linkdelta");
    let a = e.loc("A");
    set_diff(&e, &[format!("M\tF\t{}\t(+1)", esc(&a.join("f.bin")))]);
    fallback(&e, "hard link was added or removed");
}

#[test]
fn a_file_that_lost_all_its_names_means_a_walk() {
    // Both names gone: zfs prints one `-` line and no link-count line
    // (no links left to count); the other name is only in the index.
    let (e, creation) = zfs_estate("zfs-all-names-gone");
    let a = e.loc("A");
    write(&a.join("one.bin"), &content(1));
    std::fs::hard_link(a.join("one.bin"), a.join("two.bin")).unwrap();
    write(&a.join("keep.bin"), &content(2));
    scan_into(&e, &e.idx, &opts(true));
    age_rows(&e, creation);
    std::fs::remove_file(a.join("one.bin")).unwrap();
    std::fs::remove_file(a.join("two.bin")).unwrap();
    set_diff(&e, &[format!("-\tF\t{}", esc(&a.join("one.bin")))]);
    fallback(&e, "one of several names");
}
