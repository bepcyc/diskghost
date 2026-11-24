//! The btrfs inode delta against a fake subvolume info / tree search
//! (`testkit::FakeXfs`: the generation is the clock, "changed since G" is
//! every inode whose change time is after G — what item transids give on
//! a real filesystem). A delta rescan must leave exactly the rows a full
//! walk would, list only folders whose inode changed, and walk — saying
//! why — without CAP_SYS_ADMIN, when the search fails, or for another
//! subvolume. Every file lives in the test's own temp directory.

use crate::index::{scan_with, Index, ScanSummary};
use crate::testkit::*;
use crate::{Options, Progress};

fn btrfs_estate(label: &str, cap: bool) -> Estate {
    let e = Estate::new(label);
    {
        let mut x = e.xfs.lock().unwrap();
        x.btrfs.insert("A".into());
        x.btrfs_uuid = "uuid-1".into();
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

/// What a walk (no capability) does to a copy of this same index.
fn walk_of_copy(e: &Estate, n: u32) -> Vec<Row> {
    let p = e.dir.join(format!("copy-{n}.db"));
    e.idx
        .conn
        .execute("VACUUM INTO ?1", [p.to_str().unwrap()])
        .unwrap();
    let idx = Index::open(&p).unwrap();
    let cap = std::mem::replace(&mut e.xfs.lock().unwrap().cap, false);
    let root = e.loc("A");
    let s = e
        .with_probe(|pr| scan_with(&idx, &root, &Options::default(), &Progress::default(), pr))
        .unwrap();
    e.xfs.lock().unwrap().cap = cap;
    assert!(s.delta.is_none(), "{s:?}");
    rows(&idx)
}

fn has_table(e: &Estate) -> bool {
    e.idx
        .conn
        .query_row(
            "SELECT inode_ref_root IS NOT NULL AND inode_ref_kind = 'btrfs' FROM locations",
            [],
            |r| r.get::<_, Option<bool>>(0),
        )
        .unwrap()
        .unwrap_or(false)
}

/// Delta vs walk: take the walk of a copy of the index *before* the delta
/// scan, then the delta scan on the index itself.
fn delta_equals_walk(e: &Estate, n: u32) -> ScanSummary {
    let want = walk_of_copy(e, n);
    let s = scan(e, &Options::default());
    assert!(s.delta.is_some() && s.complete, "{s:?}");
    assert_eq!(rows(&e.idx), want);
    s
}

fn ready(label: &str) -> Estate {
    let e = btrfs_estate(label, true);
    let a = e.loc("A");
    write(&a.join("top.bin"), &content(1));
    write(&a.join("x/y/z/deep.bin"), &content(2));
    write(&a.join("x/keep.bin"), &content(3));
    write(&a.join("m/one.bin"), &content(4));
    write(&a.join("m/two.bin"), &content(5));
    write(&a.join("sub/inner/s.bin"), &content(6));
    // Written long ago (outside the racy window), as files usually are.
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for f in [
        "top.bin",
        "x/y/z/deep.bin",
        "x/keep.bin",
        "m/one.bin",
        "m/two.bin",
        "sub/inner/s.bin",
    ] {
        std::fs::File::options()
            .write(true)
            .open(a.join(f))
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
    }
    let s = scan(&e, &Options::default());
    assert!(s.complete && s.delta.is_none(), "{s:?}");
    assert!(
        s.delta_note.as_deref().unwrap().contains("recorded one"),
        "{s:?}"
    );
    assert!(has_table(&e));
    e
}

#[test]
fn a_deep_in_place_write_is_found_without_listing_a_folder() {
    let e = ready("btrfs-deep-write");
    let deep = e.loc("A").join("x/y/z/deep.bin");
    std::fs::write(&deep, vec![9u8; BIG]).unwrap();
    let s = delta_equals_walk(&e, 1);
    assert_eq!((s.changed, s.seen), (1, 1), "{s:?}");
}

#[test]
fn every_kind_of_change_leaves_the_rows_a_walk_would() {
    let e = ready("btrfs-all-changes");
    let a = e.loc("A");
    write(&a.join("x/y/new.bin"), &content(8));
    std::fs::remove_file(a.join("top.bin")).unwrap();
    std::fs::rename(a.join("x/keep.bin"), a.join("x/kept.bin")).unwrap();
    std::fs::rename(a.join("m/one.bin"), a.join("x/y/z/one.bin")).unwrap();
    write(&a.join("m/tmp"), &content(10));
    std::fs::rename(a.join("m/tmp"), a.join("m/two.bin")).unwrap();
    std::fs::rename(a.join("sub"), a.join("sub2")).unwrap();
    std::fs::remove_dir_all(a.join("x/y/z")).unwrap();
    write(&a.join("x/y/z/again.bin"), &content(11));
    std::fs::hard_link(a.join("m/two.bin"), a.join("x/link.bin")).unwrap();
    write(&a.join("swap"), b"file");
    delta_equals_walk(&e, 1);
    std::fs::remove_file(a.join("swap")).unwrap();
    write(&a.join("swap/inside.bin"), &content(12));
    std::fs::remove_file(a.join("x/link.bin")).unwrap();
    delta_equals_walk(&e, 2);
}

#[test]
fn nothing_changed_lists_nothing_and_reads_nothing() {
    let e = ready("btrfs-quiet");
    let s = delta_equals_walk(&e, 1);
    assert_eq!(
        (s.seen, s.changed, s.new, s.vanished),
        (0, 0, 0, 0),
        "{s:?}"
    );
    assert!(e.reads().is_empty());
}

#[test]
fn without_cap_sys_admin_it_walks_and_says_why() {
    let e = btrfs_estate("btrfs-no-cap", false);
    write(&e.loc("A").join("f.bin"), &content(1));
    for _ in 0..2 {
        let s = scan(&e, &Options::default());
        assert!(s.delta.is_none() && s.complete, "{s:?}");
        assert!(
            s.delta_note
                .as_deref()
                .unwrap()
                .contains("tree search, which needs CAP_SYS_ADMIN"),
            "{s:?}"
        );
    }
    assert_eq!(e.xfs.lock().unwrap().btrfs_calls, 0);
    assert!(!has_table(&e));
}

#[test]
fn a_failed_tree_search_means_a_walk_and_the_table_is_forgotten() {
    for errno in [libc::EPERM, libc::ENOTTY] {
        let e = ready(&format!("btrfs-errno-{errno}"));
        write(&e.loc("A").join("m/new.bin"), &content(14));
        e.xfs.lock().unwrap().btrfs_errno = Some(errno);
        let s = scan(&e, &Options::default());
        assert!(s.delta.is_none() && s.complete, "{s:?}");
        assert!(
            s.delta_note
                .as_deref()
                .unwrap()
                .contains("tree search failed"),
            "{s:?}"
        );
        assert!(!has_table(&e));
    }
}

#[test]
fn another_subvolume_under_the_same_path_means_a_walk() {
    // Deleted and made again (or a snapshot rolled in): another uuid, so
    // the stored generation means nothing for it.
    let e = ready("btrfs-new-subvol");
    e.xfs.lock().unwrap().btrfs_uuid = "uuid-2".into();
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_none() && s.complete, "{s:?}");
    assert!(
        s.delta_note.as_deref().unwrap().contains("subvolume yet"),
        "{s:?}"
    );
    assert!(has_table(&e), "a new table for the new subvolume");
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_some(), "{s:?}");
}

#[test]
fn any_other_scan_forgets_the_table() {
    let e = ready("btrfs-forget");
    let o = Options {
        exclude: vec!["*.tmp".into()],
        ..Options::default()
    };
    write(&e.loc("A").join("m/x.tmp"), &content(15));
    let s = scan(&e, &o);
    assert!(s.delta.is_none(), "{s:?}");
    assert!(!has_table(&e));
    let s = scan(&e, &Options::default());
    assert!(s.delta.is_none(), "{s:?}");
    assert!(rows(&e.idx).iter().any(|r| r.0 == "m/x.tmp"));
    assert!(has_table(&e));
}

#[test]
fn the_generation_advances_after_each_complete_delta() {
    let e = ready("btrfs-advance");
    let g1: i64 = e
        .idx
        .conn
        .query_row("SELECT inode_ref_gen FROM locations", [], |r| r.get(0))
        .unwrap();
    write(&e.loc("A").join("m/later.bin"), &content(16));
    delta_equals_walk(&e, 1);
    let g2: i64 = e
        .idx
        .conn
        .query_row("SELECT inode_ref_gen FROM locations", [], |r| r.get(0))
        .unwrap();
    assert!(g2 > g1, "{g1} -> {g2}");
    // And what changed before g2 is not asked again.
    let s = delta_equals_walk(&e, 2);
    assert_eq!(s.seen, 0, "{s:?}");
}
