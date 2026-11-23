//! Tests of the agent protocol, served in-process (no ssh, no subprocess):
//! frames in, frames out, on a fixture folder inside the test's own temp
//! directory only.

use serde_json::{json, Value};

use crate::agent::{serve, PROTOCOL};

/// Feed `lines` (raw request lines) to the agent; every stdout line parsed.
fn talk_raw(lines: &[&str]) -> Vec<Value> {
    let mut input = lines.join("\n");
    input.push('\n');
    let mut out = Vec::new();
    serve(input.as_bytes(), &mut out).unwrap();
    let text = String::from_utf8(out).expect("stdout is UTF-8");
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l:?}: {e}")))
        .collect()
}

fn talk(reqs: &[Value]) -> Vec<Value> {
    let lines: Vec<String> = reqs.iter().map(Value::to_string).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    talk_raw(&refs)
}

fn code(frame: &Value) -> &str {
    frame["error"]["code"].as_str().unwrap_or("")
}

#[test]
fn hello_answers_the_handshake() {
    let out = talk(&[json!({"v": 1, "op": "hello"})]);
    assert_eq!(out.len(), 1);
    let h = &out[0];
    assert_eq!(h["v"], PROTOCOL);
    assert_eq!(h["agent"], "diskghost");
    assert_eq!(h["ver"], env!("CARGO_PKG_VERSION"));
    assert_eq!(h["os"], std::env::consts::OS);
    assert_eq!(h["arch"], std::env::consts::ARCH);
    assert_eq!(h["blake3"], true);
}

#[test]
fn a_version_mismatch_unknown_op_and_garbage_are_error_frames_not_aborts() {
    let out = talk_raw(&[
        r#"{"v":2,"op":"hello"}"#,
        r#"{"op":"delete","paths":["/x"]}"#,
        r#"{"op":"unlink"}"#,
        r#"{"op":"write"}"#,
        "this is not json",
        r#"{"v":1,"op":"hello"}"#,
    ]);
    assert_eq!(out.len(), 6, "{out:#?}");
    assert_eq!(code(&out[0]), "version");
    for f in &out[1..4] {
        assert_eq!(code(f), "unknown_op", "{f}");
    }
    assert_eq!(code(&out[4]), "bad_request");
    assert!(out[4]["error"]["msg"].is_string());
    assert_eq!(out[5]["agent"], "diskghost", "still serving after errors");
}

#[test]
fn requests_are_independent_and_answered_in_order() {
    let out = talk(&[
        json!({"v": 1, "op": "hello"}),
        json!({"op": "nope"}),
        json!({"v": 1, "op": "hello"}),
    ]);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0]["agent"], "diskghost");
    assert_eq!(code(&out[1]), "unknown_op");
    assert_eq!(out[2]["agent"], "diskghost");
}

#[test]
fn enumerate_of_a_relative_or_missing_root_is_an_error_frame() {
    let out = talk(&[
        json!({"op": "enumerate", "root": "relative/path"}),
        json!({"op": "enumerate", "root": "/nonexistent-diskghost-agent-test"}),
        json!({"op": "enumerate"}),
    ]);
    assert_eq!(out.len(), 3, "{out:#?}");
    for f in &out {
        assert!(f.get("error").is_some(), "{f}");
    }
}

#[test]
fn the_agent_source_has_no_write_or_delete_call() {
    // Read-only by construction: the module that serves remote requests
    // must not even name a call that writes, renames or removes a file.
    let src = include_str!("agent.rs");
    for bad in [
        "remove_file",
        "remove_dir",
        "rename(",
        "File::create",
        "OpenOptions",
        "set_permissions",
        "hard_link",
        "symlink(",
        "fs::write",
        "fs::copy",
        "create_dir",
        "trash::",
    ] {
        assert!(!src.contains(bad), "agent.rs names {bad}");
    }
}

/// Unix-only cases: the fixture needs unix links, inode metadata and
/// permission modes, so the whole group sits behind one `cfg` instead of a
/// gate per test#[cfg(target_os = "linux")] // fixture writes a non-UTF-8 name; APFS refuses it
    mod unix {
    use super::*;
    use std::path::Path;

    use crate::testkit::*;

    /// A fixture: two copies, a hard link, a non-UTF-8 name, a symlink.
    fn fixture(label: &str) -> (Estate, std::path::PathBuf) {
        use std::os::unix::ffi::OsStrExt;
        let e = Estate::new(label);
        let root = e.loc("r");
        write(&root.join("a.bin"), &content(1));
        write(&root.join("sub/b.bin"), &content(1));
        std::fs::hard_link(root.join("sub/b.bin"), root.join("sub/link.bin")).unwrap();
        write(&root.join("small.txt"), b"hi");
        let bad = root.join(std::ffi::OsStr::from_bytes(b"bad\xff.bin"));
        std::fs::write(bad, b"x").unwrap();
        std::os::unix::fs::symlink(root.join("a.bin"), root.join("sym.bin")).unwrap();
        (e, root)
    }

    #[test]
    fn enumerate_streams_files_skips_non_utf8_and_ends_with_a_count() {
        use std::os::unix::fs::MetadataExt;
        let (_e, root) = fixture("agent-enum");
        let out = talk(&[json!({"op": "enumerate", "root": root.to_str().unwrap()})]);
        let files: Vec<&Value> = out.iter().filter_map(|f| f.get("f")).collect();
        let skips: Vec<&Value> = out.iter().filter_map(|f| f.get("skip")).collect();
        let end = out.last().unwrap();
        assert!(end.get("end").is_some(), "last frame is end: {out:#?}");
        let mut names: Vec<&str> = files.iter().map(|f| f["p"].as_str().unwrap()).collect();
        names.sort_unstable();
        // Every hard-link name, as `find -type f` lists; never the symlink.
        assert_eq!(names, ["a.bin", "small.txt", "sub/b.bin", "sub/link.bin"]);
        for f in &files {
            let m = std::fs::symlink_metadata(root.join(f["p"].as_str().unwrap())).unwrap();
            assert_eq!(f["s"], m.len(), "logical size");
            assert_eq!(f["m"], crate::walk::modified_ns(&m));
            assert_eq!(f["i"], m.ino());
            assert_eq!(f["d"], m.dev());
        }
        assert_eq!(skips.len(), 1, "{out:#?}");
        assert!(skips[0]["p"].as_str().unwrap().contains("bad"));
        assert!(skips[0]["why"].is_string());
        assert_eq!(end["end"]["n"], 4);
        assert_eq!(end["end"]["complete"], true);
    }

    #[test]
    fn an_unreadable_folder_makes_enumerate_incomplete() {
        use std::os::unix::fs::PermissionsExt;
        let (_e, root) = fixture("agent-enum-locked");
        let locked = root.join("locked");
        write(&locked.join("x.bin"), b"x");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&locked).is_ok(); // running as root
        let out = talk(&[json!({"op": "enumerate", "root": root.to_str().unwrap()})]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            return;
        }
        assert_eq!(out.last().unwrap()["end"]["complete"], false, "{out:#?}");
    }

    #[test]
    fn hash_batch_reports_a_bad_file_per_item_and_never_aborts() {
        use std::os::unix::fs::PermissionsExt;
        let (_e, root) = fixture("agent-hash");
        let a = root.join("a.bin");
        let missing = root.join("missing.bin");
        let locked = root.join("locked.bin");
        write(&locked, &content(3));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let locked_readable = std::fs::File::open(&locked).is_ok(); // root
        let small = root.join("small.txt");
        let s = |p: &Path| p.to_str().unwrap().to_string();
        let out = talk(&[json!({
            "op": "hash",
            "paths": [s(&a), s(&missing), s(&locked), s(&small), s(&root)],
            "sample": true
        })]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        let hs: Vec<&Value> = out.iter().filter_map(|f| f.get("h")).collect();
        assert_eq!(hs.len(), 5, "one h frame per path: {out:#?}");
        let by = |p: &Path| {
            *hs.iter()
                .find(|h| h["p"] == s(p))
                .unwrap_or_else(|| panic!("{p:?}"))
        };
        let ha = by(&a);
        assert_eq!(ha["full"], blake3::hash(&content(1)).to_hex().as_str());
        let want_sample = crate::dupes::sample_file(&a, BIG as u64).unwrap().sample;
        assert_eq!(ha["sample"], crate::remote::hex(&want_sample));
        assert!(ha["err"].is_null());
        assert!(by(&missing)["err"].is_string());
        assert!(by(&missing)["full"].is_null());
        if !locked_readable {
            assert!(by(&locked)["err"].is_string());
            assert!(by(&locked)["full"].is_null());
        }
        assert_eq!(by(&small)["full"], blake3::hash(b"hi").to_hex().as_str());
        assert!(by(&root)["err"].is_string(), "a folder is not a file");
        // Without `sample`, none is computed.
        let out = talk(&[json!({"op": "hash", "paths": [s(&a)], "sample": false})]);
        assert!(out[0]["h"]["sample"].is_null(), "{out:#?}");
        assert_eq!(out.last().unwrap()["end"]["n"], 1);
    }

    #[test]
    fn stat_fs_names_the_canonical_root_and_its_filesystem() {
        let (_e, root) = fixture("agent-statfs");
        let out = talk(&[json!({"op": "stat_fs", "root": root.join("sub/..").to_str().unwrap()})]);
        let fs = &out[0]["fs"];
        assert_eq!(fs["root"], root.to_str().unwrap(), "{out:#?}");
        assert!(fs["cow"].is_boolean());
        assert!(fs["rotational"].is_boolean());
        let mount = fs["mount"].as_str().unwrap();
        assert!(root.starts_with(mount), "{mount}");
        assert_eq!(fs["cow"], crate::index::is_cow_fs(&root));
    }
}
