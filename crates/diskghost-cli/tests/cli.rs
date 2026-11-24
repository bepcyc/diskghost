//! Integration tests: drive the actual compiled `diskghost` binary.
//! Cargo exposes its path via CARGO_BIN_EXE_<name>.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_diskghost")
}

/// A directory that always exists (this crate's own source tree).
fn sample_dir() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

#[test]
fn scan_human_output() {
    let out = Command::new(bin())
        .args(["scan", sample_dir(), "--top", "3"])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("total:"), "missing total line:\n{s}");
    assert!(s.contains("Biggest sub-folders:"));
}

#[test]
fn scan_json_is_valid() {
    let out = Command::new(bin())
        .args(["scan", sample_dir(), "--json"])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
    assert!(v.get("total_size").is_some());
    assert!(v.get("root_files_count").is_some());
    assert!(v.get("skipped").is_some());
}

#[test]
fn dupes_json_has_groups() {
    let out = Command::new(bin())
        .args(["dupes", sample_dir(), "--min-mb", "999999", "--json"])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
    assert!(v["groups"].as_array().unwrap().is_empty());
    assert!(v.get("reclaim").is_none() && v.get("warnings").is_none());
}

#[test]
fn exclude_flag_is_accepted() {
    let out = Command::new(bin())
        .args(["scan", sample_dir(), "--exclude", "target", "--json"])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
}

#[test]
fn bad_path_fails() {
    let out = Command::new(bin())
        .args(["scan", "definitely-not-a-real-path-xyz-123"])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
}

#[test]
fn reclaim_defaults_to_dry_run() {
    // Without --apply, a reclaim must not change anything and must say DRY-RUN.
    let out = Command::new(bin())
        .args([
            "dupes",
            sample_dir(),
            "--min-mb",
            "0",
            "--reclaim",
            "delete",
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("DRY-RUN"), "expected dry-run notice:\n{s}");
}

/// A throwaway directory unique to this test process.
fn scratch(label: &str) -> std::path::PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("diskghost-cli-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn scan_save_then_since_and_diff() {
    let d = scratch("snap");
    let data = d.join("data");
    std::fs::create_dir_all(data.join("keep")).unwrap();
    std::fs::write(data.join("keep/a.bin"), [0u8; 100]).unwrap();
    let old = d.join("old.json");
    let new = d.join("new.json");

    // First scan saves a snapshot; nothing to compare yet.
    let out = Command::new(bin())
        .args([
            "scan",
            data.to_str().unwrap(),
            "--save",
            old.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    assert!(old.is_file(), "snapshot file should exist");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("snapshot saved"), "stderr:\n{err}");

    // Grow one folder, add another, then compare while saving again.
    std::fs::write(data.join("keep/b.bin"), [0u8; 100_000]).unwrap();
    std::fs::create_dir_all(data.join("fresh")).unwrap();
    std::fs::write(data.join("fresh/c.bin"), [0u8; 100]).unwrap();
    let out = Command::new(bin())
        .args([
            "scan",
            data.to_str().unwrap(),
            "--since",
            old.to_str().unwrap(),
            "--save",
            new.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Changes in"), "missing diff section:\n{s}");
    assert!(s.contains("Grew:"), "keep should have grown:\n{s}");
    assert!(s.contains("New folders:"), "fresh should be new:\n{s}");

    // JSON mode nests the diff next to the report fields.
    let out = Command::new(bin())
        .args([
            "scan",
            data.to_str().unwrap(),
            "--since",
            old.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON");
    assert!(v.get("total_size").is_some());
    let diff = v.get("diff").expect("diff key");
    assert_eq!(diff["grown"].as_array().unwrap().len(), 1);
    assert_eq!(diff["added"].as_array().unwrap().len(), 1);
    assert!(diff["delta"].as_i64().unwrap() > 0);

    // Standalone diff of the two saved snapshots, human and JSON.
    let out = Command::new(bin())
        .args(["diff", old.to_str().unwrap(), new.to_str().unwrap()])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Grew:") && s.contains("New folders:"), "{s}");

    let out = Command::new(bin())
        .args([
            "diff",
            old.to_str().unwrap(),
            new.to_str().unwrap(),
            "--at",
            data.join("keep").to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON");
    assert_eq!(v["files_delta"].as_i64().unwrap(), 1);
    assert!(v["grown"].as_array().unwrap().is_empty()); // keep has no sub-folders

    // Comparing identical snapshots says so.
    let out = Command::new(bin())
        .args(["diff", new.to_str().unwrap(), new.to_str().unwrap()])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("nothing changed"));

    std::fs::remove_dir_all(&d).ok();
}

/// A successful `--json` run writes one JSON document to stdout and nothing to
/// stderr, so the output stays valid even when both streams are merged.
fn json_only(out: &std::process::Output) -> serde_json::Value {
    assert!(out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.is_empty(), "stderr should be empty:\n{err}");
    serde_json::from_slice(&out.stdout).expect("stdout is pure JSON")
}

#[test]
fn scan_json_save_reports_in_json() {
    let d = scratch("jsonsave");
    let data = d.join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("a.bin"), [0u8; 100]).unwrap();
    let snap = d.join("snap.json");

    let out = Command::new(bin())
        .args([
            "scan",
            data.to_str().unwrap(),
            "--json",
            "--save",
            snap.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    let v = json_only(&out);
    assert!(snap.is_file(), "snapshot file should exist");
    assert!(v.get("total_size").is_some());
    assert_eq!(v["snapshot_saved"].as_str(), snap.to_str());

    // Without --save there is no such key.
    let out = Command::new(bin())
        .args(["scan", data.to_str().unwrap(), "--json"])
        .output()
        .expect("run diskghost");
    assert!(json_only(&out).get("snapshot_saved").is_none());

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn scan_json_save_failure_leaves_stdout_empty() {
    let d = scratch("jsonsavefail");
    // A directory where the snapshot file should go cannot be overwritten.
    let taken = d.join("taken");
    std::fs::create_dir_all(&taken).unwrap();

    let out = Command::new(bin())
        .args([
            "scan",
            sample_dir(),
            "--json",
            "--save",
            taken.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot save snapshot"));

    std::fs::remove_dir_all(&d).ok();
}

/// Three copies of one file plus a unique one, under a fresh scratch dir.
fn dupes_fixture(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let d = scratch(label);
    let data = d.join("data");
    std::fs::create_dir_all(&data).unwrap();
    for name in ["a.bin", "b.bin", "c.bin"] {
        std::fs::write(data.join(name), [7u8; 50_000]).unwrap();
    }
    std::fs::write(data.join("unique.bin"), [9u8; 50_000]).unwrap();
    (d, data)
}

#[test]
fn dupes_json_reclaim_reports_in_json() {
    let (d, data) = dupes_fixture("jsonreclaim");

    let out = Command::new(bin())
        .args([
            "dupes",
            data.to_str().unwrap(),
            "--min-mb",
            "0",
            "--reclaim",
            "delete",
            "--json",
        ])
        .output()
        .expect("run diskghost");
    let v = json_only(&out);
    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["files"].as_array().unwrap().len(), 3);
    let r = &v["reclaim"];
    assert_eq!(r["dry_run"], true);
    assert_eq!(r["removed"], 2);
    assert_eq!(r["reclaimed"], 100_000);
    assert!(r["errors"].as_array().unwrap().is_empty());
    // A dry run changes nothing.
    for name in ["a.bin", "b.bin", "c.bin", "unique.bin"] {
        assert!(data.join(name).is_file(), "{name} should still exist");
    }

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn dupes_json_cache_warning_goes_into_json() {
    let (d, data) = dupes_fixture("jsonwarn");
    // A directory where the cache file should go cannot be overwritten.
    let taken = d.join("taken");
    std::fs::create_dir_all(&taken).unwrap();

    let out = Command::new(bin())
        .args([
            "dupes",
            data.to_str().unwrap(),
            "--min-mb",
            "0",
            "--cache",
            taken.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run diskghost");
    let v = json_only(&out);
    assert_eq!(v["groups"].as_array().unwrap().len(), 1);
    let warnings = v["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0]
        .as_str()
        .unwrap()
        .starts_with("cannot save hash cache"));

    // Without --json the warning stays on stderr.
    let out = Command::new(bin())
        .args([
            "dupes",
            data.to_str().unwrap(),
            "--min-mb",
            "0",
            "--cache",
            taken.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning: cannot save hash cache"));

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn invalid_exclude_glob_fails() {
    for cmd in ["scan", "dupes"] {
        let out = Command::new(bin())
            .args([cmd, sample_dir(), "--exclude", "a[", "--json"])
            .output()
            .expect("run diskghost");
        assert!(!out.status.success(), "{cmd} should fail");
        assert!(out.stdout.is_empty(), "{cmd}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("invalid exclude glob(s): a["), "{cmd}: {err}");
    }
}

#[test]
fn snapshot_errors_are_reported() {
    let d = scratch("snaperr");
    let bad = d.join("bad.json");
    std::fs::write(&bad, "not json").unwrap();

    // A broken --since fails before scanning.
    let out = Command::new(bin())
        .args(["scan", sample_dir(), "--since", bad.to_str().unwrap()])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read snapshot"));

    // A missing file for diff fails too.
    let out = Command::new(bin())
        .args([
            "diff",
            bad.to_str().unwrap(),
            d.join("none.json").to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());

    // Snapshots of different roots cannot be compared.
    let a = d.join("a");
    let b = d.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let sa = d.join("a.json");
    let sb = d.join("b.json");
    for (dir, file) in [(&a, &sa), (&b, &sb)] {
        let out = Command::new(bin())
            .args([
                "scan",
                dir.to_str().unwrap(),
                "--save",
                file.to_str().unwrap(),
            ])
            .output()
            .expect("run diskghost");
        assert!(out.status.success());
    }
    let out = Command::new(bin())
        .args(["diff", sa.to_str().unwrap(), sb.to_str().unwrap()])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("different directories"));

    // --at must name a folder inside the newer snapshot.
    let out = Command::new(bin())
        .args([
            "diff",
            sa.to_str().unwrap(),
            sa.to_str().unwrap(),
            "--at",
            a.join("nope").to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a directory in the snapshot"));

    std::fs::remove_dir_all(&d).ok();
}

/// root/top.bin, root/a/1.bin, root/a/x/2.bin and an empty root/c.
fn depth_fixture(label: &str) -> std::path::PathBuf {
    let d = scratch(label);
    std::fs::create_dir_all(d.join("a/x")).unwrap();
    std::fs::create_dir_all(d.join("c")).unwrap();
    std::fs::write(d.join("top.bin"), [0u8; 100]).unwrap();
    std::fs::write(d.join("a/1.bin"), [0u8; 100]).unwrap();
    std::fs::write(d.join("a/x/2.bin"), [0u8; 100]).unwrap();
    d
}

#[test]
fn max_depth_never_shows_unread_folders_as_empty() {
    let d = depth_fixture("depth");
    let dir = d.to_str().unwrap();
    let run = |args: &[&str]| {
        Command::new(bin())
            .args(["scan", dir])
            .args(args)
            .output()
            .expect("run diskghost")
    };
    let child = |v: &serde_json::Value, name: &str| {
        v["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["path"].as_str().unwrap().ends_with(name))
            .cloned()
            .unwrap()
    };

    // A full scan is complete: no flag anywhere.
    let v = json_only(&run(&["--json"]));
    assert!(v.get("complete").is_none(), "{v}");
    assert!(v["children"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c.get("complete").is_none()));
    assert_eq!(v["total_files"], 3);

    // Depth 1: a and c were not read — not measured, never "empty".
    let v = json_only(&run(&["--max-depth", "1", "--json"]));
    assert_eq!(v["complete"], false);
    assert_eq!(v["total_files"], 1);
    for name in ["a", "c"] {
        let c = child(&v, name);
        assert_eq!(c["complete"], false, "{c}");
        assert_eq!(c["size"], 0);
    }
    let out = run(&["--max-depth", "1"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("total: ≥ "), "{s}");
    assert!(s.contains("DEPTH LIMIT (--max-depth 1)"), "{s}");
    for name in ["a", "c"] {
        let line = s
            .lines()
            .find(|l| l.trim_end().ends_with(&format!("{name}  (not measured)")))
            .unwrap_or_else(|| panic!("no unmeasured line for {name}:\n{s}"));
        assert!(line.trim_start().starts_with("? "), "{line}");
        assert!(!line.contains(" B "), "{line}");
    }

    // Depth 2: a's own file is counted, a/x is not — a is a lower bound;
    // c was read and really is empty.
    let v = json_only(&run(&["--max-depth", "2", "--json"]));
    let a = child(&v, "a");
    assert_eq!(a["complete"], false);
    assert_eq!(a["file_count"], 1);
    assert!(a["size"].as_u64().unwrap() > 0);
    assert!(child(&v, "c").get("complete").is_none());
    let s = String::from_utf8_lossy(&run(&["--max-depth", "2"]).stdout).into_owned();
    let line = s
        .lines()
        .find(|l| {
            l.trim_end()
                .ends_with(&format!("{}a", std::path::MAIN_SEPARATOR))
        })
        .unwrap_or_else(|| panic!("no line for a:\n{s}"));
    assert!(line.trim_start().starts_with("≥ "), "{line}");

    // A limit deeper than the tree reads everything.
    let v = json_only(&run(&["--max-depth", "3", "--json"]));
    assert!(v.get("complete").is_none(), "{v}");
    assert_eq!(v["total_files"], 3);

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn max_depth_is_refused_where_it_would_mislead() {
    let d = depth_fixture("depthbad");
    let dir = d.to_str().unwrap();
    // A valid snapshot to compare against, from a full scan.
    let full = d.join("full.dgs");
    let out = Command::new(bin())
        .args(["scan", dir, "--save", full.to_str().unwrap()])
        .output()
        .expect("run diskghost");
    assert!(out.status.success(), "{out:?}");
    let snap = d.join("s.dgs");
    for (args, why) in [
        (
            vec![
                "scan",
                dir,
                "--max-depth",
                "1",
                "--save",
                snap.to_str().unwrap(),
            ],
            "cannot be used with",
        ),
        (
            vec![
                "scan",
                dir,
                "--max-depth",
                "1",
                "--since",
                full.to_str().unwrap(),
            ],
            "cannot be used with",
        ),
        (vec!["scan", dir, "--max-depth", "0"], "invalid value '0'"),
        (vec!["dupes", dir, "--max-depth", "0"], "invalid value '0'"),
    ] {
        let out = Command::new(bin())
            .args(&args)
            .output()
            .expect("run diskghost");
        assert!(!out.status.success(), "{args:?} should fail");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(why), "{args:?}: {err}");
    }
    assert!(!snap.exists(), "no snapshot of a partial scan");
    std::fs::remove_dir_all(&d).ok();
}

// ---- `diskghost index` ---------------------------------------------------
// Every test points --db into its own scratch dir: nothing touches the real
// ~/.local/share, and files are created/deleted only inside that scratch dir.

fn index_cmd(db: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .arg("index")
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .expect("run diskghost")
}

#[test]
#[cfg(unix)]
fn index_add_then_status_json() {
    let d = scratch("index-add");
    let tree = d.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("a"), b"same").unwrap();
    std::fs::write(tree.join("sub/b"), b"same").unwrap();
    std::fs::write(tree.join("c"), b"unique!").unwrap();
    // Written long before the scan: outside the index's 2 s racy window,
    // where every file is sampled whatever its size.
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for f in ["a", "sub/b", "c"] {
        std::fs::File::options()
            .write(true)
            .open(tree.join(f))
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
    }
    let db = d.join("db/index.db");

    let out = index_cmd(
        &db,
        &["--json", "add", "--role", "master", tree.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let added: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let s = &added[0];
    assert_eq!(s["new"], 3);
    assert_eq!(s["sampled"], 2);
    let uuid = s["uuid"].as_str().unwrap().to_string();

    let out = index_cmd(&db, &["status", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let st: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let loc = st
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["uuid"] == uuid.as_str())
        .expect("scanned location listed");
    assert_eq!(loc["online"], true);
    assert_eq!(loc["role"], "master");
    assert_eq!(loc["files"], 3);
    assert_eq!(loc["bytes"], 15);
    let canon = std::fs::canonicalize(&tree).unwrap();
    assert_eq!(
        loc["roots"],
        serde_json::json!([canon.to_str().unwrap()]),
        "{loc:#}"
    );

    // Human output names the location and its scan root too.
    let out = index_cmd(&db, &["status"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&uuid));
    assert!(text.contains(canon.to_str().unwrap()), "{text}");

    // forget removes it from the index (and only from the index).
    let out = index_cmd(&db, &["forget", &uuid]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tree.join("a").exists(), "forget never touches files");
    let out = index_cmd(&db, &["--json", "status"]);
    let st: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(st.as_array().unwrap().is_empty());

    // Forgetting an unknown uuid fails.
    let out = index_cmd(&db, &["forget", &uuid]);
    assert!(!out.status.success());
}

#[test]
#[cfg(unix)]
fn index_add_rejects_remote_hosts() {
    let d = scratch("index-remote");
    let db = d.join("index.db");
    let out = index_cmd(&db, &["add", "--host", "nas.lan", d.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ssh://"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = index_cmd(&db, &["add", "--host", "localhost", d.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn index_add_rejects_a_missing_path() {
    let d = scratch("index-missing");
    let out = index_cmd(
        &d.join("index.db"),
        &["add", d.join("nope").to_str().unwrap()],
    );
    assert!(!out.status.success());
}

// ---- `diskghost dupes --index` -------------------------------------------
// The report is built from the index in the test's own --db; files are only
// created inside the scratch dir, and taking a volume "offline" only edits
// the index row (the folder itself stays where it is).

/// a.bin, sub/b.bin: one content; c.bin: a hard link to a.bin; unique.bin:
/// same size, other content; small.txt: a unique size.
#[cfg(unix)]
fn index_fixture(label: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let d = scratch(label);
    let tree = d.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    // Reported paths are under the volume's real mountpoint.
    let tree = std::fs::canonicalize(&tree).unwrap();
    std::fs::write(tree.join("a.bin"), [7u8; 50_000]).unwrap();
    std::fs::write(tree.join("sub/b.bin"), [7u8; 50_000]).unwrap();
    std::fs::hard_link(tree.join("a.bin"), tree.join("c.bin")).unwrap();
    std::fs::write(tree.join("unique.bin"), [9u8; 50_000]).unwrap();
    std::fs::write(tree.join("small.txt"), b"only one of me").unwrap();
    let db = d.join("db/index.db");
    let out = index_cmd(&db, &["add", tree.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (d, tree, db)
}

fn dupes_index(db: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(["dupes", "--index", "--db"])
        .arg(db)
        .args(["--min-mb", "0"])
        .args(args)
        .output()
        .expect("run diskghost")
}

#[test]
#[cfg(unix)]
fn dupes_index_json_reports_physical_copies_and_singles() {
    let (d, tree, db) = index_fixture("dupes-index");
    let out = dupes_index(&db, &["--json", "--singles"]);
    let v = json_only(&out);
    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{v:#}");
    let g = &groups[0];
    assert_eq!(g["size"], 50_000);
    assert_eq!(g["hash"].as_str().unwrap().len(), 64);
    assert_eq!(g["reclaimable_bytes"], 50_000);
    assert!(g["savings_may_be_shared"].is_boolean());
    let copies = g["copies"].as_array().unwrap();
    assert_eq!(copies.len(), 2, "the hard link is not a copy: {g:#}");
    let mut names = Vec::new();
    for c in copies {
        assert!(c["location_uuid"].is_string());
        assert_eq!(c["available"], true);
        names.push(c["path"].as_str().unwrap().to_string());
        for s in c["shared_inodes"].as_array().unwrap() {
            names.push(s.as_str().unwrap().to_string());
        }
    }
    names.sort();
    let want: Vec<String> = ["a.bin", "c.bin", "sub/b.bin"]
        .iter()
        .map(|n| tree.join(n).to_str().unwrap().to_string())
        .collect();
    assert_eq!(names, want);
    assert!(v["warnings"].is_array());
    let singles = v["singles"].as_array().unwrap();
    let files: u64 = singles.iter().map(|s| s["files"].as_u64().unwrap()).sum();
    let bytes: u64 = singles.iter().map(|s| s["bytes"].as_u64().unwrap()).sum();
    assert_eq!((files, bytes), (2, 50_000 + 14));
    assert!(singles.iter().all(|s| s.get("paths").is_none()));

    // Human output names the group and the singles.
    let out = dupes_index(&db, &["--singles"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("sub/b.bin"), "{text}");
    assert!(text.to_lowercase().contains("single"), "{text}");
    std::fs::remove_dir_all(&d).ok();
}

#[test]
#[cfg(unix)]
fn dupes_index_singles_paths_go_to_a_file_or_into_the_output() {
    let (d, tree, db) = index_fixture("dupes-index-paths");
    let list = d.join("singles.txt");
    let out = dupes_index(&db, &["--json", "--singles-paths", list.to_str().unwrap()]);
    let v = json_only(&out);
    assert!(v["singles"][0].get("paths").is_none(), "{v:#}");
    let mut lines: Vec<String> = std::fs::read_to_string(&list)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    lines.sort();
    assert_eq!(
        lines,
        vec![
            tree.join("small.txt").to_str().unwrap().to_string(),
            tree.join("unique.bin").to_str().unwrap().to_string(),
        ]
    );

    let out = dupes_index(&db, &["--json", "--singles-paths"]);
    let v = json_only(&out);
    let paths = v["singles"][0]["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
#[cfg(unix)]
fn dupes_index_offline_copies_count_and_are_never_suggested() {
    let (d, _tree, db) = index_fixture("dupes-index-offline");
    // Hash everything while online, then "unplug": point the location's
    // root at a folder that does not exist (index row only).
    assert!(dupes_index(&db, &["--json"]).status.success());
    let gone = d.join("unplugged");
    let c = rusqlite::Connection::open(&db).unwrap();
    let n = c
        .execute("UPDATE locations SET root = ?1", [gone.to_str().unwrap()])
        .unwrap();
    assert_eq!(n, 1);
    drop(c);

    let out = dupes_index(&db, &["--json"]);
    let v = json_only(&out);
    let g = &v["groups"][0];
    assert_eq!(g["copies"].as_array().unwrap().len(), 2, "{v:#}");
    assert_eq!(g["reclaimable_bytes"], 50_000);
    assert_eq!(g["actionable_bytes"], 0);
    for c in g["copies"].as_array().unwrap() {
        assert_eq!(c["available"], false);
        assert!(c["suggest"].is_null(), "offline copy suggested: {c:#}");
    }
    let out = dupes_index(&db, &[]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("offline"));
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn dupes_needs_a_path_or_the_index_and_index_mode_does_not_reclaim() {
    let d = scratch("dupes-index-args");
    let db = d.join("index.db");
    let out = Command::new(bin()).arg("dupes").output().unwrap();
    assert!(!out.status.success(), "no path, no --index");
    for extra in [
        vec!["--reclaim", "delete"],
        vec!["--apply"],
        vec!["--cache", "x"],
        vec![d.to_str().unwrap()],
    ] {
        let out = dupes_index(&db, &extra);
        assert!(!out.status.success(), "--index with {extra:?} must fail");
    }
    // An empty index is no error.
    let out = dupes_index(&db, &["--json"]);
    let v = json_only(&out);
    assert!(v["groups"].as_array().unwrap().is_empty());
    std::fs::remove_dir_all(&d).ok();
}

// ---- `diskghost plan` / `apply` / `undo` -----------------------------------
// Plans come from the test's own --db. `apply` runs here only as --dry-run:
// a real apply quarantines at the volume's root, which is outside the
// scratch dir (the real moves are tested in the core crate on fake volumes).
// `undo` runs for real on a journal whose paths are all in the scratch dir.

#[cfg(unix)]
fn plan_cmd(db: &std::path::Path, out: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(["plan", "--index", "--db"])
        .arg(db)
        .arg("-o")
        .arg(out)
        .args(args)
        .output()
        .expect("run diskghost")
}

#[cfg(unix)]
fn read_json(p: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

#[test]
#[cfg(unix)]
fn plan_writes_a_readable_versioned_plan() {
    let (d, tree, db) = index_fixture("plan");
    // Default policy: two copies on two devices — one device here, so the
    // group is under-protected and nothing is to move.
    let out_file = d.join("plan.json");
    let out = plan_cmd(&db, &out_file, &[]);
    assert!(out.status.success(), "{out:?}");
    let p = read_json(&out_file);
    assert_eq!(p["version"], 1);
    assert!(p["created"].is_string());
    assert_eq!(p["policy"]["min_copies"], 2);
    assert_eq!(p["policy"]["distinct"], "physical_device");
    let g = &p["groups"][0];
    assert_eq!(g["under_protected"], true, "{p:#}");
    assert!(g["actions"].as_array().unwrap().is_empty());
    assert!(String::from_utf8_lossy(&out.stdout).contains("under-protected"));

    // One copy anywhere: the second copy goes.
    let pol = d.join("policy.toml");
    std::fs::write(&pol, "[policy]\nmin_copies = 1\ndistinct = \"none\"\n").unwrap();
    let out = plan_cmd(&db, &out_file, &["--policy", pol.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let p = read_json(&out_file);
    let g = &p["groups"][0];
    assert_eq!(g["under_protected"], false);
    let acts = g["actions"].as_array().unwrap();
    assert_eq!(acts.len(), 1, "{p:#}");
    assert_eq!(acts[0]["type"], "quarantine");
    assert_eq!(acts[0]["path"], tree.join("sub/b.bin").to_str().unwrap());
    for k in ["size", "mtime_ns", "dev", "ino", "full"] {
        assert!(acts[0]["expect"].get(k).is_some(), "expect.{k}");
    }
    // The plan never touches a file.
    assert!(tree.join("sub/b.bin").exists());
    std::fs::remove_dir_all(&d).ok();
}

#[test]
#[cfg(unix)]
fn plan_rejects_a_bad_policy_naming_the_field() {
    let (d, _tree, db) = index_fixture("plan-bad-policy");
    let pol = d.join("policy.toml");
    std::fs::write(&pol, "[policy]\ndistinct = \"nope\"\n").unwrap();
    let out_file = d.join("plan.json");
    let out = plan_cmd(&db, &out_file, &["--policy", pol.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("distinct"), "{err}");
    assert!(!out_file.exists(), "no plan from a bad policy");
    // plan needs --index and an output file.
    let out = Command::new(bin())
        .args(["plan", "-o"])
        .arg(&out_file)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let out = Command::new(bin())
        .args(["plan", "--index", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    assert!(!out.status.success());
    std::fs::remove_dir_all(&d).ok();
}

#[test]
#[cfg(unix)]
fn apply_dry_run_reports_and_touches_nothing() {
    let (d, tree, db) = index_fixture("apply-dry");
    let pol = d.join("policy.toml");
    std::fs::write(&pol, "[policy]\nmin_copies = 1\ndistinct = \"none\"\n").unwrap();
    let plan = d.join("plan.json");
    assert!(plan_cmd(&db, &plan, &["--policy", pol.to_str().unwrap()])
        .status
        .success());
    let out = Command::new(bin())
        .args(["apply", "--json", "--dry-run", "--paranoid", "--db"])
        .arg(&db)
        .arg(&plan)
        .output()
        .unwrap();
    let v = json_only(&out);
    let o = &v["outcomes"][0];
    assert_eq!(o["status"], "would_quarantine", "{v:#}");
    // Under the folder `index add` was given, never at the volume root
    // (which may be `/`, unwritable and outside the user's scope).
    let to = std::path::Path::new(o["to"].as_str().unwrap());
    assert!(
        to.starts_with(tree.join(".diskghost-quarantine")),
        "{} not under {}",
        to.display(),
        tree.display()
    );
    assert!(tree.join("sub/b.bin").exists());
    assert!(!d.join("plan.json.journal.jsonl").exists());
    // Human output too.
    let out = Command::new(bin())
        .args(["apply", "--dry-run", "--db"])
        .arg(&db)
        .arg(&plan)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("would"));
    std::fs::remove_dir_all(&d).ok();
}

// ---- unix-only cases -------------------------------------------------
// Everything below needs a unix filesystem or a unix process model — an
// inode number in the undo journal, an executable `#!/bin/sh` stand-in for
// ssh — so the whole group sits behind one `cfg` instead of a gate per
// test or helper.
#[cfg(unix)]
mod unix {
    use super::*;

    #[test]
    fn undo_moves_journaled_files_back() {
        let d = scratch("undo");
        let d = std::fs::canonicalize(&d).unwrap();
        let from = d.join("data/f.bin");
        let to = d.join("q/f.bin.1");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(&to, b"payload").unwrap();
        let ino = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&to).unwrap().ino()
        };
        let journal = d.join("plan.json.journal.jsonl");
        let line = serde_json::json!({
            "action_id": "x-1", "from": from, "to": to, "group": "00",
            "status": "done", "ts": 1, "ino": ino
        });
        std::fs::write(&journal, format!("{line}\n")).unwrap();

        let out = Command::new(bin())
            .args(["undo", "--json", "--dry-run"])
            .arg(&journal)
            .output()
            .unwrap();
        let v = json_only(&out);
        assert_eq!(v["outcomes"][0]["status"], "would_restore", "{v:#}");
        assert!(to.exists() && !from.exists());

        let out = Command::new(bin())
            .arg("undo")
            .arg(&journal)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(std::fs::read(&from).unwrap(), b"payload");
        assert!(!to.exists());
        let last = std::fs::read_to_string(&journal).unwrap();
        let last: serde_json::Value = serde_json::from_str(last.lines().last().unwrap()).unwrap();
        assert_eq!(last["status"], "undone");
        std::fs::remove_dir_all(&d).ok();
    }

    // ---- remote locations (`ssh://`) -------------------------------------------
    // No real ssh: `DISKGHOST_SSH` points at a stand-in script that drops the ssh
    // options and host and runs the (read-only) command locally, inside the
    // test's own scratch dir. A marker file makes the "host" unreachable.

    fn fake_ssh(d: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = d.join("fake-ssh");
        std::fs::write(
        &script,
        "#!/bin/sh\n\
         while [ \"$1\" = \"-o\" ]; do shift 2; done\n\
         [ \"$1\" = \"--\" ] && shift\n\
         host=$1; shift\n\
         if [ -e \"$FAKE_SSH_DOWN\" ]; then echo \"ssh: connect to host $host port 22: Connection refused\" >&2; exit 255; fi\n\
         exec sh -c \"$1\"\n",
    )
    .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn remote_cmd(d: &std::path::Path, args: &[&str]) -> std::process::Output {
        // The "remote" home is the scratch dir: an agent upload lands in
        // <scratch>/home/.cache/diskghost, never in the real home.
        Command::new(bin())
            .env("HOME", d.join("home"))
            .env("DISKGHOST_SSH", d.join("fake-ssh"))
            .env("FAKE_SSH_DOWN", d.join("host-down"))
            .args(args)
            .output()
            .expect("run diskghost")
    }

    #[cfg(target_os = "linux")] // fsid form depends on the host's GNU df/mountinfo; macOS resolves /var→/private/var
    #[test]
    fn index_add_ssh_status_and_dupes_over_a_stand_in_ssh() {
        let d = scratch("index-ssh");
        fake_ssh(&d);
        let remote = std::fs::canonicalize(&d).unwrap().join("remote");
        std::fs::create_dir_all(remote.join("sub")).unwrap();
        let same: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(remote.join("a.bin"), &same).unwrap();
        std::fs::write(remote.join("sub/b.bin"), &same).unwrap();
        std::fs::write(remote.join("c.txt"), b"unique").unwrap();
        let db = d.join("db/index.db");
        let db_s = db.to_str().unwrap();
        let url = format!("ssh://fakehost/{}", remote.display());

        // The shell tier end to end (the agent tier: the test below).
        let out = remote_cmd(
            &d,
            &[
                "index", "--db", db_s, "--json", "add", "--agent", "never", &url,
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let added: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(added[0]["transport"], "shell");
        let s = &added[0];
        let uuid = s["uuid"].as_str().unwrap().to_string();
        // One location per remote filesystem: `ssh://host/<fs id>`, no folder.
        let fsid = uuid.strip_prefix("ssh://fakehost/").unwrap_or("");
        assert!(
            !fsid.is_empty() && fsid.bytes().all(|b| b.is_ascii_hexdigit()),
            "{uuid}"
        );
        assert_eq!(s["new"], 3);
        assert_eq!(s["hashed"], 2);
        assert!(remote.join("a.bin").exists() && remote.join("sub/b.bin").exists());

        let out = remote_cmd(&d, &["index", "--db", db_s, "status", "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let st: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(st[0]["host"], "ssh://fakehost");
        // `root` is the remote mountpoint: a path prefix of the folder given.
        let root = std::path::PathBuf::from(st[0]["root"].as_str().unwrap());
        assert!(remote.starts_with(&root), "{root:?} vs {remote:?}");
        assert_eq!(st[0]["online"], true);
        assert_eq!(st[0]["files"], 3);

        let out = remote_cmd(
            &d,
            &["dupes", "--index", "--db", db_s, "--min-mb", "0", "--json"],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let groups = r["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{r:#}");
        assert_eq!(groups[0]["unique_copies"], 2);
        assert!(r["unverifiable"].as_array().unwrap().is_empty());

        // The host goes away: status says offline, a new add fails cleanly.
        std::fs::write(d.join("host-down"), b"").unwrap();
        let out = remote_cmd(&d, &["index", "--db", db_s, "status", "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let st: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(st[0]["online"], false);
        assert_eq!(st[0]["files"], 3, "rows kept");
        let out = remote_cmd(&d, &["index", "--db", db_s, "add", &url]);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("unreachable"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn index_add_rejects_a_malformed_ssh_url() {
        let d = scratch("index-ssh-bad");
        fake_ssh(&d);
        let db = d.join("index.db");
        let out = remote_cmd(
            &d,
            &[
                "index",
                "--db",
                db.to_str().unwrap(),
                "add",
                "ssh://-oProxyCommand=x/y",
            ],
        );
        assert!(!out.status.success());
        std::fs::remove_dir_all(&d).ok();
    }

    // ---- the remote agent (M5) -------------------------------------------------

    #[test]
    fn agent_subcommand_speaks_json_lines_on_stdout_and_creates_nothing() {
        use std::io::Write;
        let d = scratch("agent-stdio");
        let root = std::fs::canonicalize(&d).unwrap().join("tree");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.bin"), b"aaaa").unwrap();
        std::fs::write(root.join("sub/b.bin"), b"bbbb").unwrap();
        let home = d.join("home");
        let mut child = Command::new(bin())
            .arg("agent")
            .env("HOME", &home)
            .env("XDG_DATA_HOME", d.join("xdg"))
            .env("XDG_CACHE_HOME", d.join("xdg-cache"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run diskghost agent");
        let a = root.join("a.bin");
        let req = format!(
            "{}\n{}\n{}\nnot json\n{}\n",
            r#"{"v":1,"op":"hello"}"#,
            serde_json::json!({"op": "enumerate", "root": root}),
            serde_json::json!({"op": "hash", "paths": [a, root.join("gone")], "sample": false}),
            r#"{"op":"stat_fs","root":"/"}"#,
        );
        child
            .stdin
            .take()
            .unwrap()
            .write_all(req.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let frames: Vec<serde_json::Value> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?}: {e}")))
            .collect();
        assert_eq!(frames[0]["agent"], "diskghost");
        assert_eq!(frames[0]["ver"], env!("CARGO_PKG_VERSION"));
        let files = frames.iter().filter(|f| f.get("f").is_some()).count();
        assert_eq!(files, 2);
        let hs: Vec<&serde_json::Value> = frames.iter().filter_map(|f| f.get("h")).collect();
        assert_eq!(hs.len(), 2);
        assert!(hs.iter().any(|h| h["err"].is_string()), "{hs:?}");
        assert!(frames.iter().any(|f| f["error"]["code"] == "bad_request"));
        assert!(
            frames.last().unwrap().get("fs").is_some(),
            "served after an error"
        );
        for p in [home, d.join("xdg"), d.join("xdg-cache")] {
            assert!(!p.exists(), "agent created {p:?}");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    fn agent_files(d: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(d.join("home/.cache/diskghost"))
            .map(|r| r.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn index_add_ssh_uploads_the_agent_into_the_remote_cache_once_and_uses_it() {
        use std::os::unix::fs::PermissionsExt;
        if !(cfg!(target_os = "linux") && cfg!(target_arch = "x86_64")) {
            return;
        }
        let d = scratch("index-ssh-agent");
        fake_ssh(&d);
        let remote = std::fs::canonicalize(&d).unwrap().join("remote");
        std::fs::create_dir_all(remote.join("sub")).unwrap();
        let same: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(remote.join("a.bin"), &same).unwrap();
        std::fs::write(remote.join("sub/b.bin"), &same).unwrap();
        let db = d.join("db/index.db");
        let db_s = db.to_str().unwrap();
        let url = format!("ssh://fakehost/{}", remote.display());

        let out = remote_cmd(&d, &["index", "--db", db_s, "--json", "add", &url]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let s: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(s[0]["transport"], "agent", "{s:#}");
        assert_eq!(s[0]["algo"], "blake3");
        assert_eq!(
            (s[0]["new"].clone(), s[0]["hashed"].clone()),
            (2.into(), 2.into())
        );
        let files = agent_files(&d);
        assert_eq!(files.len(), 1, "{files:?}");
        let name = files[0].file_name().unwrap().to_str().unwrap().to_string();
        let prefix = format!("diskghost-agent-{}-", env!("CARGO_PKG_VERSION"));
        assert!(name.starts_with(&prefix), "{name}");
        let mode = std::fs::metadata(&files[0]).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read(&files[0]).unwrap(),
            std::fs::read(bin()).unwrap()
        );
        let mtime = std::fs::metadata(&files[0]).unwrap().modified().unwrap();

        // Again: the same file is reused, not uploaded a second time.
        let out = remote_cmd(&d, &["index", "--db", db_s, "--json", "add", &url]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let s: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(s[0]["transport"], "agent");
        assert_eq!(s[0]["unchanged"], 2);
        assert_eq!(agent_files(&d), files);
        assert_eq!(
            std::fs::metadata(&files[0]).unwrap().modified().unwrap(),
            mtime
        );

        // `--agent never`: the shell tier, same location.
        let out = remote_cmd(
            &d,
            &[
                "index", "--db", db_s, "--json", "add", "--agent", "never", &url,
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let n: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(n[0]["transport"], "shell");
        assert_eq!(n[0]["uuid"], s[0]["uuid"], "one location whichever tier");
        assert_eq!(n[0]["unchanged"], 2);
        // The remote files were only read.
        assert_eq!(std::fs::read(remote.join("a.bin")).unwrap(), same);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn scan_of_an_ssh_url_lists_the_remote_folder_over_a_stand_in_ssh() {
        let d = scratch("scan-ssh");
        fake_ssh(&d);
        let remote = std::fs::canonicalize(&d).unwrap().join("remote");
        std::fs::create_dir_all(remote.join("sub")).unwrap();
        std::fs::write(remote.join("a.bin"), vec![1u8; 3000]).unwrap();
        std::fs::write(remote.join("sub/b.bin"), vec![2u8; 500]).unwrap();
        let url = format!("ssh://fakehost/{}", remote.display());

        let out = remote_cmd(&d, &["scan", "--json", "--agent", "never", &url]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["total_size"], 3500);
        assert_eq!(v["total_files"], 2);
        assert_eq!(v["root"], format!("ssh://fakehost/{}", remote.display()));
        assert_eq!(v["transport"], "shell");
        // `complete` is written only when false.
        assert!(v.get("complete").is_none(), "{v:#}");
        assert_eq!(v["cancelled"], false);

        // Human output names the remote root.
        let out = remote_cmd(&d, &["scan", "--agent", "never", &url]);
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("ssh://fakehost/"));

        // Unreachable: the ssh message, not "not a directory".
        std::fs::write(d.join("host-down"), b"").unwrap();
        let out = remote_cmd(&d, &["scan", "--agent", "never", &url]);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("unreachable"), "{err}");
        assert!(!err.contains("not a directory"), "{err}");
    }
}
