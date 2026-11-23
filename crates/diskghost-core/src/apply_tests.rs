//! Tests of `apply` / `undo` (kept out of `apply.rs`, so that file holds
//! only the code that acts on user files — `grep` it for unlinks and see
//! none). Every file here lives in the test's own temp directory.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::apply::*;
use crate::index::Role;
use crate::plan::{plan_with, Plan};
use crate::policy::{Distinct, Policy};
use crate::testkit::*;
use crate::Progress;

fn policy(min: u64) -> Policy {
    Policy {
        min_copies: min,
        distinct: Distinct::PhysicalDevice,
        locations: Vec::new(),
    }
}

fn set_role(e: &Estate, name: &str, role: Role) {
    e.idx
        .configure(&Estate::uuid(name), Some(role), "local")
        .unwrap();
}

/// `content(1)` as `f.bin` on A (normal) and on each scratch volume of
/// `scratch`; a plan with min_copies 1, written to `<dir>/plan.json`.
fn setup(label: &str, scratch: &[&str]) -> (Estate, PathBuf, Plan) {
    let e = Estate::new(label);
    write(&e.loc("A").join("f.bin"), &content(1));
    for s in scratch {
        write(&e.loc(s).join("f.bin"), &content(1));
    }
    e.add("A");
    for s in scratch {
        e.add(s);
        set_role(&e, s, Role::Scratch);
    }
    let (path, plan) = make(&e, &policy(1));
    (e, path, plan)
}

fn make(e: &Estate, p: &Policy) -> (PathBuf, Plan) {
    let plan = e
        .with_probe(|pr| plan_with(&e.idx, p, &Progress::default(), pr))
        .unwrap();
    let path = e.dir.join("plan.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    (path, plan)
}

fn run(e: &Estate, plan: &Path, dry_run: bool, paranoid: bool) -> RunReport {
    let opts = ApplyOptions { dry_run, paranoid };
    e.with_probe(|pr| apply_with(plan, &e.idx, &opts, pr))
        .unwrap()
}

fn status_of(r: &RunReport, from: &Path) -> Status {
    r.outcomes
        .iter()
        .find(|o| o.from == from)
        .unwrap_or_else(|| panic!("{from:?} not in {r:#?}"))
        .status
}

#[cfg(unix)]
fn ino(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).unwrap().ino()
}

#[cfg(not(unix))]
fn ino(_p: &Path) -> u64 {
    // Mirrors `stat_of`'s non-unix fallback in index.rs: no inode key on
    // Windows, so the ino-shaped quarantine name degenerates to `.0` there
    // exactly the way production apply computes it.
    0
}

fn qdir(e: &Estate, name: &str) -> PathBuf {
    e.loc(name).join(QUARANTINE_DIR)
}

/// Overwrite a file's first byte in place, keeping its size, inode and
/// modification time — a change only a re-read can see.
fn tamper(p: &Path) {
    let mtime = std::fs::metadata(p).unwrap().modified().unwrap();
    let mut f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    let mut c = std::fs::read(p).unwrap();
    c[0] ^= 0xff;
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&c[..1]).unwrap();
    f.set_modified(mtime).unwrap();
}

#[test]
fn journal_lives_beside_the_plan() {
    assert_eq!(
        journal_path(Path::new("/x/plan.json")),
        PathBuf::from("/x/plan.json.journal.jsonl")
    );
}

#[test]
fn apply_moves_into_quarantine_at_the_volume_root_and_journals_it() {
    let (e, plan_path, plan) = setup("apply-basic", &["B"]);
    let g = &plan.groups[0];
    assert_eq!(g.actions.len(), 1);
    let from = e.loc("B").join("f.bin");
    let n = ino(&from);
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &from), Status::Done, "{r:#?}");
    let to = qdir(&e, "B").join(&g.hash[..16]).join(format!("f.bin.{n}"));
    assert_eq!(r.outcomes[0].to.as_deref(), Some(to.as_path()));
    assert!(!from.exists(), "moved away");
    assert_eq!(std::fs::read(&to).unwrap(), content(1), "content intact");
    assert_eq!(ino(&to), n, "a rename, not a copy");
    // The kept copy is untouched.
    assert_eq!(std::fs::read(e.loc("A").join("f.bin")).unwrap(), content(1));
    assert!(!qdir(&e, "A").exists());

    let j = read_journal(&journal_path(&plan_path)).unwrap();
    let st: Vec<Status> = j.iter().map(|l| l.status).collect();
    assert_eq!(st, vec![Status::Pending, Status::Done], "{j:#?}");
    let done = &j[1];
    assert_eq!(done.action_id, g.actions[0].id);
    assert_eq!(
        (done.from.as_path(), done.to.as_deref()),
        (from.as_path(), Some(to.as_path()))
    );
    assert_eq!(done.group, g.hash);
    assert_eq!(done.ino, Some(n));
    assert!(done.ts > 0);
    // Raw lines: one JSON object each.
    let raw = std::fs::read_to_string(journal_path(&plan_path)).unwrap();
    for line in raw.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        for k in ["action_id", "from", "to", "group", "status", "ts"] {
            assert!(v.get(k).is_some(), "{k} missing in {line}");
        }
    }
}

#[test]
fn a_second_run_skips_what_is_done() {
    let (e, plan_path, _) = setup("apply-twice", &["B"]);
    run(&e, &plan_path, false, false);
    let lines = read_journal(&journal_path(&plan_path)).unwrap().len();
    let r = run(&e, &plan_path, false, false);
    assert_eq!(r.count(Status::Skipped), 1, "{r:#?}");
    assert_eq!(
        read_journal(&journal_path(&plan_path)).unwrap().len(),
        lines
    );
}

#[test]
fn a_killed_run_resumes_with_only_the_rest() {
    let (e, plan_path, plan) = setup("apply-resume", &["B", "C"]);
    let acts = &plan.groups[0].actions;
    assert_eq!(acts.len(), 2, "{plan:#?}");
    // The journal of a run killed after its first action.
    let first = &acts[0];
    let line = JournalEntry {
        action_id: first.id.clone(),
        from: first.path.clone(),
        to: Some(e.dir.join("elsewhere")),
        group: plan.groups[0].hash.clone(),
        status: Status::Done,
        ts: 1,
        ino: Some(first.expect.ino),
        detail: None,
    };
    std::fs::write(
        journal_path(&plan_path),
        format!("{}\n", serde_json::to_string(&line).unwrap()),
    )
    .unwrap();
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &first.path), Status::Skipped);
    assert!(first.path.exists(), "a journaled action is never redone");
    assert_eq!(status_of(&r, &acts[1].path), Status::Done);
    assert!(!acts[1].path.exists());
}

#[test]
fn a_run_killed_mid_rename_is_reconciled() {
    let (e, plan_path, plan) = setup("apply-pending", &["B"]);
    let g = &plan.groups[0];
    let a = &g.actions[0];
    // Killed after the rename, before the `done` line.
    let to = e
        .dir
        .join("B")
        .join(QUARANTINE_DIR)
        .join("x")
        .join("f.bin.1");
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::rename(&a.path, &to).unwrap();
    let line = JournalEntry {
        action_id: a.id.clone(),
        from: a.path.clone(),
        to: Some(to.clone()),
        group: g.hash.clone(),
        status: Status::Pending,
        ts: 1,
        ino: Some(a.expect.ino),
        detail: None,
    };
    std::fs::write(
        journal_path(&plan_path),
        format!("{}\n", serde_json::to_string(&line).unwrap()),
    )
    .unwrap();
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &a.path), Status::Done, "{r:#?}");
    let j = read_journal(&journal_path(&plan_path)).unwrap();
    assert_eq!(j.last().unwrap().status, Status::Done);
    assert_eq!(j.last().unwrap().to.as_deref(), Some(to.as_path()));
    // And undo brings it back from where it really went.
    let u = undo(&journal_path(&plan_path), false).unwrap();
    assert_eq!(u.count(Status::Undone), 1, "{u:#?}");
    assert_eq!(std::fs::read(&a.path).unwrap(), content(1));
}

#[test]
fn a_pending_action_whose_file_never_moved_is_simply_redone() {
    let (e, plan_path, plan) = setup("apply-pending-unmoved", &["B"]);
    let g = &plan.groups[0];
    let a = &g.actions[0];
    let line = JournalEntry {
        action_id: a.id.clone(),
        from: a.path.clone(),
        to: Some(e.dir.join("B").join(QUARANTINE_DIR).join("never")),
        group: g.hash.clone(),
        status: Status::Pending,
        ts: 1,
        ino: Some(a.expect.ino),
        detail: None,
    };
    std::fs::write(
        journal_path(&plan_path),
        format!("{}\n", serde_json::to_string(&line).unwrap()),
    )
    .unwrap();
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &a.path), Status::Done, "{r:#?}");
    assert!(!a.path.exists());
}

#[test]
fn a_file_changed_since_the_plan_is_stale_and_untouched() {
    let (e, plan_path, _) = setup("apply-stale", &["B"]);
    let from = e.loc("B").join("f.bin");
    let mut c = content(1);
    c.push(0);
    std::fs::write(&from, &c).unwrap();
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &from), Status::Stale, "{r:#?}");
    assert_eq!(std::fs::read(&from).unwrap(), c, "untouched");
    assert!(!qdir(&e, "B").exists());
    let j = read_journal(&journal_path(&plan_path)).unwrap();
    assert_eq!(j.last().unwrap().status, Status::Stale);

    // Gone entirely: stale as well.
    let (e, plan_path, _) = setup("apply-stale-gone", &["B"]);
    let from = e.loc("B").join("f.bin");
    std::fs::rename(&from, e.loc("B").join("moved.bin")).unwrap();
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &from), Status::Stale);
}

#[test]
fn same_size_and_mtime_but_other_content_is_a_hash_mismatch() {
    let (e, plan_path, _) = setup("apply-hash", &["B"]);
    let from = e.loc("B").join("f.bin");
    let before = std::fs::metadata(&from).unwrap().modified().unwrap();
    tamper(&from);
    assert_eq!(
        std::fs::metadata(&from).unwrap().modified().unwrap(),
        before
    );
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &from), Status::HashMismatch, "{r:#?}");
    assert!(from.exists());
    assert!(!qdir(&e, "B").exists());
}

#[test]
fn paranoid_mode_compares_bytes_with_the_kept_copy() {
    // The kept copy changed (same size and mtime): the target still hashes
    // right, but it is no longer the same as what stays.
    let (e, plan_path, _) = setup("apply-paranoid", &["B"]);
    let from = e.loc("B").join("f.bin");
    tamper(&e.loc("A").join("f.bin"));
    let r = run(&e, &plan_path, false, true);
    assert_eq!(status_of(&r, &from), Status::ParanoidMismatch, "{r:#?}");
    assert!(from.exists());
    let j = read_journal(&journal_path(&plan_path)).unwrap();
    assert_eq!(j.last().unwrap().status, Status::ParanoidMismatch);

    // Equal bytes: moved.
    let (e, plan_path, _) = setup("apply-paranoid-ok", &["B"]);
    let from = e.loc("B").join("f.bin");
    let r = run(&e, &plan_path, false, true);
    assert_eq!(status_of(&r, &from), Status::Done, "{r:#?}");

    // No kept copy left to compare with: untouched.
    let (e, plan_path, _) = setup("apply-paranoid-none", &["B"]);
    let from = e.loc("B").join("f.bin");
    std::fs::rename(e.loc("A").join("f.bin"), e.loc("A").join("away.bin")).unwrap();
    let r = run(&e, &plan_path, false, true);
    assert_eq!(status_of(&r, &from), Status::ParanoidNoReference, "{r:#?}");
    assert!(from.exists());
}

#[test]
fn dry_run_touches_nothing() {
    let (e, plan_path, plan) = setup("apply-dry", &["B"]);
    let from = e.loc("B").join("f.bin");
    let r = run(&e, &plan_path, true, true);
    assert!(r.dry_run);
    let o = &r.outcomes[0];
    assert_eq!(o.status, Status::WouldQuarantine, "{r:#?}");
    let to = o.to.as_ref().unwrap();
    assert!(to.starts_with(qdir(&e, "B").join(&plan.groups[0].hash[..16])));
    assert!(from.exists());
    assert!(!to.exists());
    assert!(!qdir(&e, "B").exists(), "no directory made");
    assert!(!journal_path(&plan_path).exists(), "no journal written");
}

#[test]
fn an_existing_quarantine_file_is_never_replaced() {
    let (e, plan_path, plan) = setup("apply-noreplace", &["B"]);
    let from = e.loc("B").join("f.bin");
    let n = ino(&from);
    let dir = qdir(&e, "B").join(&plan.groups[0].hash[..16]);
    let taken = dir.join(format!("f.bin.{n}"));
    write(&taken, b"someone else's file");
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &from), Status::Done, "{r:#?}");
    assert_eq!(std::fs::read(&taken).unwrap(), b"someone else's file");
    let to = r.outcomes[0].to.clone().unwrap();
    assert_ne!(to, taken);
    assert_eq!(to.parent(), Some(dir.as_path()));
    assert_eq!(std::fs::read(&to).unwrap(), content(1));
}

#[test]
#[cfg(unix)]
fn a_permission_failure_is_an_error_and_the_run_goes_on() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let (e, plan_path, plan) = setup("apply-perm", &["B", "C"]);
    let b = e.loc("B");
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o555)).unwrap();
    let r = run(&e, &plan_path, false, false);
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(plan.groups[0].actions.len(), 2);
    assert_eq!(status_of(&r, &b.join("f.bin")), Status::Error, "{r:#?}");
    assert!(b.join("f.bin").exists());
    assert_eq!(status_of(&r, &e.loc("C").join("f.bin")), Status::Done);
    assert!(r.has_errors());
}

#[test]
fn hand_edits_cannot_point_apply_at_a_master_or_outside_the_location() {
    let (e, plan_path, mut plan) = setup("apply-guard", &["B"]);
    // B becomes master after the plan was made.
    set_role(&e, "B", Role::Master);
    let r = run(&e, &plan_path, false, false);
    assert_eq!(r.outcomes[0].status, Status::Error, "{r:#?}");
    assert!(e.loc("B").join("f.bin").exists());
    set_role(&e, "B", Role::Scratch);

    // A path that climbs out of its location.
    let a = &mut plan.groups[0].actions[0];
    a.path = e.loc("B").join("..").join("A").join("f.bin");
    std::fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
    let r = run(&e, &plan_path, false, false);
    assert_eq!(r.outcomes[0].status, Status::Error, "{r:#?}");
    assert!(e.loc("A").join("f.bin").exists());

    // An unknown plan version is refused outright.
    plan.version = 99;
    std::fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
    let opts = ApplyOptions::default();
    let err = e
        .with_probe(|pr| apply_with(&plan_path, &e.idx, &opts, pr))
        .unwrap_err();
    assert!(err.to_string().contains("version"), "{err}");
}

#[test]
fn a_location_unreachable_at_apply_time_is_untouched() {
    let (e, plan_path, _) = setup("apply-offline", &["B"]);
    e.unplug("B");
    let r = run(&e, &plan_path, false, false);
    assert_eq!(r.outcomes[0].status, Status::Offline, "{r:#?}");
    assert!(e.loc("B").join("f.bin").exists());
}

#[test]
fn undo_restores_every_moved_file_and_apply_can_redo_it() {
    let (e, plan_path, _) = setup("undo", &["B", "C"]);
    let froms = [e.loc("B").join("f.bin"), e.loc("C").join("f.bin")];
    let inos: Vec<u64> = froms.iter().map(|f| ino(f)).collect();
    run(&e, &plan_path, false, false);
    assert!(froms.iter().all(|f| !f.exists()));
    let j = journal_path(&plan_path);

    let dry = undo(&j, true).unwrap();
    assert_eq!(dry.count(Status::WouldRestore), 2, "{dry:#?}");
    assert!(froms.iter().all(|f| !f.exists()), "dry run moves nothing");

    let u = undo(&j, false).unwrap();
    assert_eq!(u.count(Status::Undone), 2, "{u:#?}");
    for (f, n) in froms.iter().zip(&inos) {
        assert_eq!(std::fs::read(f).unwrap(), content(1));
        assert_eq!(ino(f), *n);
    }
    assert_eq!(
        read_journal(&j).unwrap().last().unwrap().status,
        Status::Undone
    );
    // Idempotent.
    let again = undo(&j, false).unwrap();
    assert_eq!(again.count(Status::Undone), 0);
    assert_eq!(again.count(Status::Skipped), 2, "{again:#?}");
    // An undone action is not "done": apply does it again.
    let r = run(&e, &plan_path, false, false);
    assert_eq!(r.count(Status::Done), 2, "{r:#?}");
}

#[test]
fn undo_never_replaces_a_file_that_took_the_old_name() {
    let (e, plan_path, _) = setup("undo-conflict", &["B"]);
    let from = e.loc("B").join("f.bin");
    let r = run(&e, &plan_path, false, false);
    let to = r.outcomes[0].to.clone().unwrap();
    write(&from, b"new file under the old name");
    let u = undo(&journal_path(&plan_path), false).unwrap();
    assert_eq!(u.count(Status::Conflict), 1, "{u:#?}");
    assert_eq!(
        std::fs::read(&from).unwrap(),
        b"new file under the old name"
    );
    assert_eq!(std::fs::read(&to).unwrap(), content(1));
}

#[test]
fn quarantined_files_are_no_copies_in_later_reports_and_plans() {
    let (e, plan_path, _) = setup("apply-rescan", &["B"]);
    run(&e, &plan_path, false, false);
    // Re-index B: the quarantined file is on it now.
    e.add("B");
    let r = e.report(&Estate::opts());
    assert!(
        r.groups.is_empty(),
        "quarantine is no copy: {:#?}",
        r.groups
    );
    let (_, plan) = make(&e, &policy(1));
    assert!(plan.groups.is_empty(), "{plan:#?}");
}

// ---- quarantine under the scan root -----------------------------------

/// The quarantine target `apply` should pick for `from` under `scan_root`.
fn target(scan_root: &Path, hash: &str, from: &Path) -> PathBuf {
    scan_root
        .join(QUARANTINE_DIR)
        .join(&hash[..16])
        .join(format!(
            "{}.{}",
            from.file_name().unwrap().to_str().unwrap(),
            ino(from)
        ))
}

/// `content(1)` on A (kept, normal) and at each `rel` inside volume B
/// (scratch); each folder of `roots` (inside B) added as a scan root.
fn setup_roots(label: &str, files: &[&str], roots: &[&str]) -> (Estate, PathBuf, Plan) {
    let e = Estate::new(label);
    write(&e.loc("A").join("f.bin"), &content(1));
    for f in files {
        write(&e.loc("B").join(f), &content(1));
    }
    e.add("A");
    for r in roots {
        e.add_at(&e.loc("B").join(r));
    }
    set_role(&e, "B", Role::Scratch);
    let (path, plan) = make(&e, &policy(1));
    (e, path, plan)
}

#[test]
fn two_scan_roots_of_one_volume_quarantine_under_their_own_root() {
    let (e, plan_path, plan) = setup_roots("roots-two", &["x/f.bin", "y/f.bin"], &["x", "y"]);
    let g = &plan.groups[0];
    assert_eq!(g.actions.len(), 2, "{plan:#?}");
    let (fx, fy) = (e.loc("B").join("x/f.bin"), e.loc("B").join("y/f.bin"));
    let (tx, ty) = (
        target(&e.loc("B").join("x"), &g.hash, &fx),
        target(&e.loc("B").join("y"), &g.hash, &fy),
    );
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &fx), Status::Done, "{r:#?}");
    assert_eq!(status_of(&r, &fy), Status::Done, "{r:#?}");
    assert_eq!(std::fs::read(&tx).unwrap(), content(1));
    assert_eq!(std::fs::read(&ty).unwrap(), content(1));
    assert!(!qdir(&e, "B").exists(), "never at the mount root");
    // undo brings both back from their scan roots.
    let u = undo(&journal_path(&plan_path), false).unwrap();
    assert_eq!(u.count(Status::Undone), 2, "{u:#?}");
    assert!(fx.exists() && fy.exists());
}

#[test]
fn nested_scan_roots_the_longest_covering_one_wins() {
    let (e, plan_path, plan) =
        setup_roots("roots-nested", &["x/f.bin", "x/y/f.bin"], &["x", "x/y"]);
    let g = &plan.groups[0];
    assert_eq!(g.actions.len(), 2, "{plan:#?}");
    let (outer, inner) = (e.loc("B").join("x/f.bin"), e.loc("B").join("x/y/f.bin"));
    let (to_outer, to_inner) = (
        target(&e.loc("B").join("x"), &g.hash, &outer),
        target(&e.loc("B").join("x/y"), &g.hash, &inner),
    );
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &outer), Status::Done, "{r:#?}");
    assert_eq!(status_of(&r, &inner), Status::Done, "{r:#?}");
    assert!(to_outer.is_file(), "{r:#?}");
    assert!(to_inner.is_file(), "{r:#?}");
    assert!(!qdir(&e, "B").exists());
}

#[test]
fn a_path_no_scan_root_covers_falls_back_to_the_mount_root() {
    // Scan roots `a` and `ab`; then `ab` is dropped from the index (as an
    // index from before scan roots were recorded would lack it). `ab/f.bin`
    // is covered by no scan root — `a` is a string prefix, not a folder one.
    let (e, plan_path, plan) = setup_roots("roots-fallback", &["ab/f.bin"], &["a", "ab"]);
    let n = e
        .idx
        .conn
        .execute("DELETE FROM scan_roots WHERE path = 'ab'", [])
        .unwrap();
    assert_eq!(n, 1, "the scan root of `ab` was recorded");
    let g = &plan.groups[0];
    let from = e.loc("B").join("ab/f.bin");
    let to = target(&e.loc("B"), &g.hash, &from);
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &from), Status::Done, "{r:#?}");
    assert!(to.is_file(), "{r:#?}");
    assert!(!e.loc("B").join("a").join(QUARANTINE_DIR).exists());
    assert!(!e.loc("B").join("ab").join(QUARANTINE_DIR).exists());
}

#[test]
#[cfg(unix)]
fn an_unwritable_scan_root_is_an_error_never_a_fallback_and_resume_retries() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let (e, plan_path, plan) = setup_roots("roots-perm", &["x/f.bin"], &["x"]);
    let x = e.loc("B").join("x");
    let from = x.join("f.bin");
    let to = target(&x, &plan.groups[0].hash, &from);
    std::fs::set_permissions(&x, std::fs::Permissions::from_mode(0o555)).unwrap();
    let r = run(&e, &plan_path, false, false);
    std::fs::set_permissions(&x, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status_of(&r, &from), Status::Error, "{r:#?}");
    assert!(from.exists());
    assert!(
        !qdir(&e, "B").exists(),
        "no silent fallback to the mount root"
    );
    // The error is retried by the next run.
    let r = run(&e, &plan_path, false, false);
    assert_eq!(status_of(&r, &from), Status::Done, "{r:#?}");
    assert!(to.is_file(), "{r:#?}");
}

#[test]
fn files_quarantined_under_a_scan_root_are_no_copies_after_its_rescan() {
    let (e, plan_path, _) = setup_roots("roots-rescan", &["x/f.bin"], &["x"]);
    let r = run(&e, &plan_path, false, false);
    assert_eq!(r.count(Status::Done), 1, "{r:#?}");
    assert!(e.loc("B").join("x").join(QUARANTINE_DIR).is_dir());
    e.add_at(&e.loc("B").join("x"));
    let rep = e.report(&Estate::opts());
    assert!(
        rep.groups.is_empty(),
        "quarantine is no copy: {:#?}",
        rep.groups
    );
    let (_, plan) = make(&e, &policy(1));
    assert!(plan.groups.is_empty(), "{plan:#?}");
}
