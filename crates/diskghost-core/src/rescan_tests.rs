//! Rescans of filesystems whose inode numbers do not survive a remount
//! (exFAT, FAT) and of files whose time is too close to the scan to trust
//! ("racy"). Every file lives in the test's own temp directory.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::apply::*;
use crate::index::{is_racy, RACY_WINDOW_NS};
use crate::plan::plan_with;
use crate::policy::{Distinct, Policy};
use crate::testkit::*;
use crate::Progress;

fn ino(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).unwrap().ino()
}

fn set_mtime(p: &Path, t: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn ago(secs: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(secs)
}

/// What a remount does to an exFAT file: same name, bytes and time, a new
/// inode number. Done here by copying and renaming over the file.
fn churn(p: &Path) {
    let before = ino(p);
    let mtime = std::fs::metadata(p).unwrap().modified().unwrap();
    let tmp = p.with_extension("churn");
    std::fs::write(&tmp, std::fs::read(p).unwrap()).unwrap();
    set_mtime(&tmp, mtime);
    std::fs::rename(&tmp, p).unwrap();
    assert_ne!(ino(p), before, "a new inode");
    assert_eq!(std::fs::metadata(p).unwrap().modified().unwrap(), mtime);
}

/// Overwrite in place (same inode, same size), then put the time back:
/// a write inside the timestamp tick the scan saw.
fn rewrite_keeping_time(p: &Path, tag: u8) {
    let mtime = std::fs::metadata(p).unwrap().modified().unwrap();
    let n = std::fs::metadata(p).unwrap().len() as usize;
    std::fs::write(p, vec![tag; n]).unwrap();
    set_mtime(p, mtime);
}

fn stored(e: &Estate, name: &str, rel: &str) -> (i64, bool) {
    e.idx
        .conn
        .query_row(
            "SELECT f.ino, f.racy FROM files f JOIN locations l ON l.id = f.location_id \
             WHERE l.uuid = ?1 AND f.path = ?2",
            rusqlite::params![Estate::uuid(name), rel],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn location_ino_stable(e: &Estate, name: &str) -> bool {
    e.idx
        .conn
        .query_row(
            "SELECT ino_stable FROM locations WHERE uuid = ?1",
            [Estate::uuid(name)],
            |r| r.get(0),
        )
        .unwrap()
}

fn unstable(e: &Estate, name: &str) {
    e.ino_unstable.lock().unwrap().insert(name.to_string());
}

#[test]
fn racy_window_bounds() {
    let start = 10 * RACY_WINDOW_NS;
    assert!(is_racy(start, start));
    assert!(is_racy(start + 1, start), "a time in the future");
    assert!(is_racy(start - RACY_WINDOW_NS, start));
    assert!(!is_racy(start - RACY_WINDOW_NS - 1, start));
    assert!(is_racy(0, 1), "no underflow near the epoch");
}

#[test]
fn remount_churn_keeps_hashes_on_a_fat_like_volume_and_resets_them_elsewhere() {
    let e = Estate::new("rescan-churn");
    // S keeps inode numbers (ext4-like), U does not (exFAT-like).
    unstable(&e, "U");
    for v in ["S", "U"] {
        let f = e.loc(v).join("f.bin");
        write(&f, &content(1));
        set_mtime(&f, ago(600));
    }
    assert_eq!(e.scan("S").new, 1);
    assert_eq!(e.scan("U").new, 1);
    assert!(location_ino_stable(&e, "S"));
    assert!(!location_ino_stable(&e, "U"));
    assert_eq!(stored(&e, "U", "f.bin"), (0, false), "no inode stored");
    assert_ne!(stored(&e, "S", "f.bin").0, 0);
    let (_, _, sample_u, _) = e.row("U", "f.bin");
    assert!(sample_u.is_some(), "the size collides: sampled");

    churn(&e.loc("S").join("f.bin"));
    churn(&e.loc("U").join("f.bin"));

    let s = e.scan("U");
    assert_eq!(
        (s.unchanged, s.changed, s.racy_rechecked),
        (1, 0, 0),
        "{s:?}"
    );
    assert!(e.reads().is_empty(), "nothing re-read: {:?}", e.reads());
    assert_eq!(e.row("U", "f.bin").2, sample_u, "sample kept");

    let s = e.scan("S");
    assert_eq!((s.unchanged, s.changed), (0, 1), "a new inode is a change");
}

#[test]
fn a_real_change_is_still_seen_without_inode_numbers() {
    let e = Estate::new("rescan-churn-change");
    unstable(&e, "U");
    let f = e.loc("U").join("f.bin");
    write(&f, &content(1));
    write(&e.loc("U").join("twin.bin"), &content(1));
    set_mtime(&f, ago(600));
    set_mtime(&e.loc("U").join("twin.bin"), ago(600));
    e.scan("U");
    let before = e.row("U", "f.bin").2;

    // Same size, another time: the (size, mtime) predicate sees it.
    std::fs::write(&f, content(2)).unwrap();
    set_mtime(&f, ago(300));
    let s = e.scan("U");
    assert_eq!((s.changed, s.unchanged), (1, 1), "{s:?}");
    assert!(e.reads().contains(&f), "re-read");
    assert_ne!(e.row("U", "f.bin").2, before);
}

#[test]
fn documented_limit_same_size_and_time_new_inode_new_bytes_is_not_seen_without_inode_numbers() {
    // The price of (size, mtime) on exFAT/FAT: a different file renamed
    // over this one with the same size and time (`cp -p` + rename) looks
    // unchanged. docs/rescan-delta.md states it; this pins it.
    let e = Estate::new("rescan-churn-limit");
    unstable(&e, "U");
    let f = e.loc("U").join("f.bin");
    write(&f, &content(1));
    set_mtime(&f, ago(600));
    e.scan("U");
    let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
    let tmp = e.loc("U").join("other");
    std::fs::write(&tmp, content(2)).unwrap();
    set_mtime(&tmp, mtime);
    std::fs::rename(&tmp, &f).unwrap();
    let s = e.scan("U");
    assert_eq!((s.unchanged, s.changed), (1, 0), "{s:?}");
}

#[test]
fn rows_indexed_with_inode_numbers_switch_without_rehashing() {
    // An index from before this change (or a volume first seen as
    // stable) holds real inode numbers; the first scan that knows the
    // filesystem drops them once instead of resetting every hash.
    let e = Estate::new("rescan-switch");
    for v in ["A", "U"] {
        let f = e.loc(v).join("f.bin");
        write(&f, &content(1));
        set_mtime(&f, ago(600));
    }
    e.scan("A");
    e.scan("U");
    assert_ne!(stored(&e, "U", "f.bin").0, 0);
    let sample = e.row("U", "f.bin").2;
    assert!(sample.is_some());

    unstable(&e, "U");
    churn(&e.loc("U").join("f.bin"));
    let s = e.scan("U");
    assert_eq!((s.unchanged, s.changed), (1, 0), "{s:?}");
    assert_eq!(stored(&e, "U", "f.bin").0, 0);
    assert_eq!(e.row("U", "f.bin").2, sample);
    assert!(e.reads().is_empty());
}

#[test]
fn a_write_in_the_scan_tick_is_caught_by_the_next_scan() {
    let e = Estate::new("rescan-racy");
    let fresh = e.loc("A").join("fresh.bin");
    let old = e.loc("A").join("old.bin");
    write(&fresh, &content(1));
    write(&old, &content(1));
    set_mtime(&old, ago(600));
    let s = e.scan("A");
    assert_eq!(s.new, 2);
    assert!(
        stored(&e, "A", "fresh.bin").1,
        "time within the window: racy"
    );
    assert!(!stored(&e, "A", "old.bin").1);
    let fresh_sample = e.row("A", "fresh.bin").2;
    let old_sample = e.row("A", "old.bin").2;
    assert!(fresh_sample.is_some() && old_sample.is_some());

    // Both rewritten in place with their time put back: the stat of each
    // is what the scan recorded.
    rewrite_keeping_time(&fresh, 7);
    rewrite_keeping_time(&old, 7);

    let s = e.scan("A");
    // `stat` saw no change (changed 0); the re-read did, and says so.
    assert_eq!(
        (s.racy_rechecked, s.racy_changed, s.changed, s.unchanged),
        (1, 1, 0, 1),
        "{s:?}"
    );
    assert!(e.reads().contains(&fresh), "the racy file is re-read");
    assert_ne!(e.row("A", "fresh.bin").2, fresh_sample, "new sample");
    // Not racy and time put back by hand: the known blind spot of any
    // stat predicate (`touch -r`), unchanged by this guard.
    assert_eq!(e.row("A", "old.bin").2, old_sample);
}

#[test]
fn the_report_rereads_a_racy_row() {
    let e = Estate::new("rescan-racy-report");
    let a = e.loc("A").join("f.bin");
    let b = e.loc("B").join("f.bin");
    write(&a, &content(1));
    write(&b, &content(1));
    set_mtime(&b, ago(600));
    e.add("A");
    e.add("B");
    assert!(stored(&e, "A", "f.bin").1);
    rewrite_keeping_time(&a, 9);
    let r = e.report(&Estate::opts());
    assert!(e.reads().contains(&a), "racy row re-read: {:?}", e.reads());
    assert!(
        r.groups.is_empty(),
        "the changed bytes are no copy: {:#?}",
        r.groups
    );
}

#[test]
fn apply_and_undo_work_without_inode_numbers() {
    let e = Estate::new("rescan-apply");
    unstable(&e, "B");
    for v in ["A", "B"] {
        let f = e.loc(v).join("f.bin");
        write(&f, &content(1));
        set_mtime(&f, ago(600));
    }
    e.add("A");
    e.add("B");
    e.idx
        .configure(
            &Estate::uuid("B"),
            Some(crate::index::Role::Scratch),
            "local",
        )
        .unwrap();
    let policy = Policy {
        min_copies: 1,
        distinct: Distinct::PhysicalDevice,
        locations: Vec::new(),
    };
    let plan = e
        .with_probe(|pr| plan_with(&e.idx, &policy, &Progress::default(), pr))
        .unwrap();
    let path = e.dir.join("plan.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let a = &plan.groups[0].actions[0];
    assert_eq!(a.path, e.loc("B").join("f.bin"));
    assert_eq!(a.expect.ino, 0, "no inode number in the plan");

    // A remount between plan and apply: new inode, same file.
    churn(&a.path);
    let opts = ApplyOptions {
        dry_run: false,
        paranoid: true,
    };
    let r = e
        .with_probe(|pr| apply_with(&path, &e.idx, &opts, pr))
        .unwrap();
    assert_eq!(r.count(Status::Done), 1, "{r:#?}");
    assert!(!a.path.exists());

    let u = undo(&journal_path(&path), false).unwrap();
    assert_eq!(u.count(Status::Undone), 1, "{u:#?}");
    assert_eq!(std::fs::read(&a.path).unwrap(), content(1));
}

#[test]
fn apply_without_inode_numbers_still_refuses_a_changed_file() {
    let e = Estate::new("rescan-apply-stale");
    unstable(&e, "B");
    for v in ["A", "B"] {
        let f = e.loc(v).join("f.bin");
        write(&f, &content(1));
        set_mtime(&f, ago(600));
    }
    e.add("A");
    e.add("B");
    e.idx
        .configure(
            &Estate::uuid("B"),
            Some(crate::index::Role::Scratch),
            "local",
        )
        .unwrap();
    let policy = Policy {
        min_copies: 1,
        distinct: Distinct::PhysicalDevice,
        locations: Vec::new(),
    };
    let plan = e
        .with_probe(|pr| plan_with(&e.idx, &policy, &Progress::default(), pr))
        .unwrap();
    let path = e.dir.join("plan.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let target = plan.groups[0].actions[0].path.clone();
    set_mtime(&target, ago(60));
    let opts = ApplyOptions {
        dry_run: false,
        paranoid: false,
    };
    let r = e
        .with_probe(|pr| apply_with(&path, &e.idx, &opts, pr))
        .unwrap();
    assert_eq!(r.count(Status::Stale), 1, "{r:#?}");
    assert!(target.exists());
}

#[test]
fn a_racy_row_with_the_same_bytes_is_rechecked_not_reported_changed() {
    let e = Estate::new("rescan-racy-same");
    write(&e.loc("A").join("a.bin"), &content(1));
    write(&e.loc("A").join("b.bin"), &content(1));
    e.scan("A");
    assert!(stored(&e, "A", "a.bin").1 && stored(&e, "A", "b.bin").1);
    let s = e.scan("A");
    assert_eq!(
        (s.racy_rechecked, s.racy_changed, s.changed),
        (2, 0, 0),
        "{s:?}"
    );
}
