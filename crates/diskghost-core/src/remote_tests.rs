//! Tests of remote (ssh) locations. No real ssh anywhere: [`FakeHost`]
//! answers the commands in memory, and [`LocalExec`] runs the very same
//! read-only commands on this machine, inside the test's own temp directory
//! only — never anywhere else (owner's hard safety rule).

use std::io;
use std::path::PathBuf;

use crate::apply::{apply_with, read_journal, ApplyOptions, Status};
use crate::index::Role;
use crate::plan::plan_with;
use crate::policy::{Distinct, Policy};
use crate::remote::*;
use crate::testkit::*;
use crate::Progress;

const URL: &str = "ssh://adroid//storage";
const UUID: &str = "ssh://adroid/a1b2c3d4:/storage";
const T0: u64 = 1_700_000_000_123_456_789;

fn caps(x: &FakeHost) -> Capabilities {
    probe(x).unwrap()
}

// ---- ssh command line, URLs, hosts -----------------------------------

#[test]
fn argv_is_shell_quoted_into_one_remote_command() {
    assert_eq!(shell_quote("plain"), "'plain'");
    assert_eq!(shell_quote("it's"), r"'it'\''s'");
    assert_eq!(shell_quote("$(rm -rf /) `x` *"), "'$(rm -rf /) `x` *'");
    assert_eq!(shell_quote(""), "''");
    let x = SshExec {
        host: "adroid".into(),
        ssh: "ssh".into(),
        extra: SshExec::new("adroid").extra,
    };
    assert_eq!(
        x.extra,
        ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"].map(String::from)
    );
    assert_eq!(
        x.args(&["find", "/a b", "-printf", "%p\\0"]),
        [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            "adroid",
            r"'find' '/a b' '-printf' '%p\0'",
        ]
        .map(String::from)
    );
}

#[test]
fn ssh_urls_split_into_host_and_path() {
    let ok = |u: &str| parse_ssh_url(u).unwrap();
    assert_eq!(
        ok("ssh://adroid//storage"),
        ("adroid".into(), "/storage".into())
    );
    assert_eq!(
        ok("ssh://u@nas.lan/data/x"),
        ("u@nas.lan".into(), "data/x".into())
    );
    assert_eq!(ok("ssh://adroid"), ("adroid".into(), ".".into()));
    assert_eq!(ok("ssh://adroid/"), ("adroid".into(), ".".into()));
    for bad in [
        "adroid:/x",
        "ssh:///x",
        "ssh://-oProxyCommand=evil/x",
        "ssh://a b/x",
        "ssh://host:22/x",
    ] {
        assert!(parse_ssh_url(bad).is_err(), "{bad}");
    }
}

#[test]
fn remote_hosts_are_validated_and_plain_names_still_refused() {
    assert!(validate_remote_host("adroid").is_ok());
    assert!(validate_remote_host("u@nas.lan").is_ok());
    assert!(validate_remote_host("-oProxyCommand=x").is_err());
    assert!(validate_remote_host("").is_err());
    assert!(crate::index::validate_host("ssh://adroid").is_ok());
    assert!(crate::index::validate_host("ssh://-x").is_err());
    assert!(crate::index::validate_host("nas.lan").is_err());
}

// ---- probe -----------------------------------------------------------

#[test]
fn probe_runs_one_call_in_a_fixed_order() {
    let h = FakeHost::linux();
    let c = caps(&h);
    assert_eq!(h.calls().len(), 1, "one ssh session");
    let call = &h.calls()[0];
    assert_eq!(&call[..2], ["sh", "-c"]);
    let s = &call[2];
    let at = |needle: &str| s.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(at("uname -s") < at("command -v"));
    assert!(at("b3sum") < at("md5sum") && at("md5sum") < at("sha1sum"));
    assert!(at("sha1sum") < at("find stat") && at("-printf") > at("command -v"));
    assert_eq!(c.os, "Linux");
    assert_eq!(c.tools, ["b3sum", "md5sum", "sha1sum", "find", "stat"]);
    assert!(c.find_printf);
    assert_eq!(c.stat, StatFlavor::Gnu);
    assert_eq!(c.hash, HashAlgo::Blake3);
}

#[test]
fn hash_tool_chain_degrades_b3sum_md5sum_sha1sum() {
    let c = caps(&FakeHost::linux().with_tools(&["md5sum", "sha1sum", "find", "stat"]));
    assert_eq!(c.hash, HashAlgo::Md5);
    let c = caps(&FakeHost::linux().with_tools(&["sha1sum", "find"]));
    assert_eq!(c.hash, HashAlgo::Sha1);
    assert_eq!(c.tools, ["sha1sum", "find"]);
}

#[test]
fn a_host_without_any_hash_tool_is_unusable_and_says_what_was_probed() {
    let e = probe(&FakeHost::linux().with_tools(&["find", "stat"])).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::Unsupported);
    let m = e.to_string();
    for t in PROBED_TOOLS {
        assert!(m.contains(t), "{m}");
    }
    assert!(m.contains("unusable"), "{m}");
}

#[test]
fn an_unreachable_host_is_an_error_not_a_panic() {
    let h = FakeHost::linux();
    h.down.store(true, std::sync::atomic::Ordering::SeqCst);
    let e = probe(&h).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotConnected);
    assert!(!remote_online(&h, "/storage"));
}

// ---- enumeration -----------------------------------------------------

#[test]
fn gnu_find_printf_output_is_parsed_exactly() {
    let mut out = Vec::new();
    for rec in [
        "12\t1700000000.1234567890\t77\t/storage/a.bin",
        "0\t1700000001\t78\t/storage/sub/tab\there",
        "5\t1700000002.5\t79\t/storage/new\nline",
        "garbage",
        "3\t1700000003.0\t80\t/elsewhere/x",
    ] {
        out.extend(rec.as_bytes());
        out.push(0);
    }
    out.extend(b"4\t1.0\t81\t/storage/bad-\xff-utf8\0");
    let (files, skipped) = parse_printf(&out, "/storage");
    assert_eq!(
        files,
        vec![
            RemoteFile {
                path: "a.bin".into(),
                size: 12,
                mtime_ns: 1_700_000_000_123_456_789,
                ino: 77
            },
            RemoteFile {
                path: "sub/tab\there".into(),
                size: 0,
                mtime_ns: 1_700_000_001_000_000_000,
                ino: 78
            },
            RemoteFile {
                path: "new\nline".into(),
                size: 5,
                mtime_ns: 1_700_000_002_500_000_000,
                ino: 79
            },
        ]
    );
    assert_eq!(skipped, 3, "garbage, outside the root, not UTF-8");
    assert_eq!(parse_mtime("12.000000001"), Some(12_000_000_001));
    assert_eq!(parse_mtime("x"), None);
    // Root "/" itself.
    let (f, _) = parse_printf(b"1\t1\t2\t/etc/x\0", "/");
    assert_eq!(f[0].path, "etc/x");
}

#[test]
fn gnu_enumeration_is_one_find_call_on_one_filesystem() {
    let h = FakeHost::linux();
    h.put("/storage/a", b"aaa", T0);
    h.put("/storage/d/b", b"bb", T0);
    h.put("/other/c", b"c", T0);
    let c = caps(&h);
    let l = enum_remote(&h, &c, "/storage").unwrap();
    assert!(l.complete);
    let names: Vec<&str> = l.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["a", "d/b"]);
    assert_eq!(l.files[0].mtime_ns, T0);
    let finds: Vec<_> = h.calls().into_iter().filter(|c| c[0] == "find").collect();
    assert_eq!(finds.len(), 1);
    assert!(finds[0].contains(&"-xdev".to_string()));
    assert!(finds[0].contains(&"-printf".to_string()));
    assert_eq!(
        h.count(|c| c.get(2).map(String::as_str) == Some(STAT_SCRIPT)),
        0
    );
}

#[test]
fn bsd_find_falls_back_to_print0_and_batched_stat() {
    let h = FakeHost::bsd();
    for i in 0..450 {
        h.put(&format!("/storage/f{i:04}"), &[1; 7], T0);
    }
    let c = caps(&h);
    assert!(!c.find_printf);
    assert_eq!(c.stat, StatFlavor::Bsd);
    let l = enum_remote(&h, &c, "/storage").unwrap();
    assert!(l.complete);
    assert_eq!(l.files.len(), 450);
    assert!(l.files.iter().all(|f| f.size == 7));
    // Whole seconds only on this path.
    assert_eq!(l.files[0].mtime_ns, T0 / 1_000_000_000 * 1_000_000_000);
    assert_eq!(
        h.count(|c| c[0] == "find" && c.contains(&"-printf".to_string())),
        0,
        "the probe already said -printf is missing"
    );
    assert_eq!(
        h.count(|c| c[0] == "find" && c.contains(&"-print0".to_string())),
        1
    );
    let stats = h.count(|c| c.get(2).map(String::as_str) == Some(STAT_SCRIPT));
    assert_eq!(stats, 3, "450 files in batches of 200");
}

#[test]
fn a_find_error_makes_the_listing_incomplete() {
    let h = FakeHost::linux();
    h.put("/storage/a", b"a", T0);
    h.find_code.store(1, std::sync::atomic::Ordering::SeqCst);
    let l = enum_remote(&h, &caps(&h), "/storage").unwrap();
    assert!(!l.complete);
    assert_eq!(l.files.len(), 1);
}

#[test]
fn the_root_is_resolved_with_its_filesystem_id() {
    let h = FakeHost::linux();
    h.put("/storage/a", b"a", T0);
    h.put("/home/u/data/x", b"x", T0);
    let c = caps(&h);
    let r = resolve_root(&h, &c, "/storage").unwrap();
    assert_eq!(r.root, "/storage");
    assert_eq!(r.fsid.as_deref(), Some("a1b2c3d4"));
    assert_eq!(r.fstype.as_deref(), Some("ext2/ext3"));
    assert_eq!(resolve_root(&h, &c, "data").unwrap().root, "/home/u/data");
    assert!(resolve_root(&h, &c, "/nope").is_err());
    let b = FakeHost::bsd();
    b.put("/storage/a", b"a", T0);
    let r = resolve_root(&b, &caps(&b), "/storage").unwrap();
    assert_eq!(r.fsid.as_deref(), Some("dev66"));
    assert_eq!(r.fstype, None);
}

// ---- hashing ---------------------------------------------------------

#[test]
fn batches_respect_the_path_count_and_the_byte_cap() {
    let p = |n: usize, len: usize| -> Vec<String> {
        (0..n).map(|i| format!("/{i:0>w$}", w = len - 1)).collect()
    };
    assert!(batches::<String>(&[]).is_empty());
    assert_eq!(batches(&p(200, 10)), vec![0..200]);
    assert_eq!(batches(&p(201, 10)), vec![0..200, 200..201]);
    assert_eq!(batches(&p(400, 10)), vec![0..200, 200..400]);
    // 1000-byte paths: the byte cap bites before the count does.
    let long = p(200, 1000);
    let b = batches(&long);
    assert!(b.len() > 1, "{b:?}");
    for r in &b {
        let bytes: usize = long[r.clone()]
            .iter()
            .map(|s| shell_quote(s).len() + 1)
            .sum();
        assert!(bytes <= BATCH_BYTES && r.len() <= BATCH);
    }
    assert_eq!(b.last().unwrap().end, 200);
    // One path longer than the cap is a batch of its own.
    let huge = vec!["/".to_string() + &"x".repeat(BATCH_BYTES), "/y".into()];
    assert_eq!(batches(&huge), vec![0..1, 1..2]);
}

#[test]
fn hashing_is_batched_by_200_and_aligned_by_position() {
    let h = FakeHost::linux();
    let mut paths = Vec::new();
    for i in 0..201u32 {
        let p = format!("/storage/f{i}");
        h.put(&p, &i.to_le_bytes(), T0);
        paths.push(p);
    }
    h.set_unreadable("/storage/f7");
    let c = caps(&h);
    let got = hash_remote(&h, &c, &paths).unwrap();
    assert_eq!(got.len(), 201);
    assert_eq!(got[7], None, "unreadable: no digest, the rest stay aligned");
    for i in [0usize, 8, 199, 200] {
        assert_eq!(
            got[i].as_deref(),
            Some(&blake3::hash(&(i as u32).to_le_bytes()).as_bytes()[..])
        );
    }
    let calls: Vec<_> = h
        .calls()
        .into_iter()
        .filter(|c| c.get(2).map(String::as_str) == Some(HASH_SCRIPT))
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].len() - 5, 200);
    assert_eq!(calls[1].len() - 5, 1);
    assert_eq!(calls[0][4], "b3sum");
}

#[test]
fn hashing_uses_the_first_tool_of_the_chain() {
    let h = FakeHost::linux().with_tools(&["md5sum", "sha1sum", "find", "stat"]);
    h.put("/storage/a", b"hello", T0);
    let c = caps(&h);
    let got = hash_remote(&h, &c, &["/storage/a".to_string()]).unwrap();
    assert_eq!(got[0].as_ref().unwrap().len(), 16);
    assert_eq!(
        got[0].as_deref(),
        Some(&FakeHost::digest("md5sum", b"hello")[..])
    );
    let calls = h.calls();
    assert_eq!(calls.last().unwrap()[4], "md5sum");
}

// ---- real commands, run locally (no ssh) -----------------------------

/// Runs the remote commands on this machine — the real `find`, `stat`,
/// `sh` and hash tools — so their syntax and output are tested too.
struct LocalExec;

impl RemoteExec for LocalExec {
    fn exec(&self, argv: &[&str]) -> io::Result<ExecOut> {
        let out = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(ExecOut {
            stdout: out.stdout,
            stderr: out.stderr,
            code: out.status.code().unwrap_or(-1),
        })
    }
}

#[test]
fn the_real_commands_list_and_hash_a_local_folder_identically() {
    use std::os::unix::fs::MetadataExt;
    let e = Estate::new("remote-local-exec");
    let root = e.loc("r");
    write(&root.join("a.bin"), &content(1));
    write(&root.join("sub dir/it's b.bin"), &content(1));
    write(&root.join("c.bin"), &content(2));
    let c = probe(&LocalExec).unwrap();
    let r = resolve_root(&LocalExec, &c, root.to_str().unwrap()).unwrap();
    assert_eq!(PathBuf::from(&r.root), root);
    let mut l = enum_remote(&LocalExec, &c, &r.root).unwrap();
    assert!(l.complete);
    l.files.sort_by(|a, b| a.path.cmp(&b.path));
    let names: Vec<&str> = l.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["a.bin", "c.bin", "sub dir/it's b.bin"]);
    for f in &l.files {
        let m = std::fs::metadata(root.join(&f.path)).unwrap();
        assert_eq!((f.size, f.ino), (m.len(), m.ino()));
        if c.find_printf {
            assert_eq!(f.mtime_ns, crate::walk::modified_ns(&m), "{}", f.path);
        }
    }
    let paths: Vec<String> = l
        .files
        .iter()
        .map(|f| format!("{}/{}", r.root, f.path))
        .collect();
    let mut all = paths.clone();
    all.push(format!("{}/missing", r.root));
    let d = hash_remote(&LocalExec, &c, &all).unwrap();
    assert_eq!(d.len(), 4);
    assert_eq!(d[3], None);
    let (a, cc, b) = (
        d[0].clone().unwrap(),
        d[1].clone().unwrap(),
        d[2].clone().unwrap(),
    );
    assert_eq!(a.len(), c.hash.digest_len());
    assert_eq!(a, b, "same content, same digest");
    assert_ne!(a, cc);
    if c.hash == HashAlgo::Blake3 {
        assert_eq!(a, blake3::hash(&content(1)).as_bytes().to_vec());
    }
    assert!(remote_online(&LocalExec, &r.root));
    assert!(!remote_online(&LocalExec, &format!("{}/nope", r.root)));
}

// ---- the index -------------------------------------------------------

fn loc_row(e: &Estate, uuid: &str) -> (String, String, Option<i64>) {
    e.idx
        .conn
        .query_row(
            "SELECT host, root, cow FROM locations WHERE uuid = ?1",
            [uuid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

fn remote_rows(e: &Estate, uuid: &str) -> Vec<(String, i64, Option<Vec<u8>>, String)> {
    let mut st = e
        .idx
        .conn
        .prepare(
            "SELECT f.path, f.size, f.full, f.algo FROM files f \
             JOIN locations l ON l.id = f.location_id WHERE l.uuid = ?1 ORDER BY f.path",
        )
        .unwrap();
    st.query_map([uuid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn schema_4_adds_the_digest_algorithm_and_migrates_v3_rows() {
    let e = Estate::new("remote-schema");
    write(&e.loc("A").join("x.bin"), b"x");
    e.add("A");
    let v: i64 = e
        .idx
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 4);
    let rows = remote_rows(&e, &Estate::uuid("A"));
    assert_eq!(rows[0].3, "blake3", "default for local rows");
    // An md5 digest is 16 bytes; blake3 must stay 32.
    let c = &e.idx.conn;
    let set = |len: usize, algo: &str| {
        c.execute(
            "UPDATE files SET full = ?1, algo = ?2",
            rusqlite::params![vec![1u8; len], algo],
        )
    };
    assert!(set(16, "md5").is_ok());
    assert!(set(20, "sha1").is_ok());
    assert!(set(16, "blake3").is_err());
    assert!(set(32, "crc32").is_err());
}

#[test]
fn a_v3_index_migrates_to_v4_keeping_its_hashes() {
    let e = Estate::new("remote-migrate");
    let path = e.dir.join("v3.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        for m in &crate::index::MIGRATIONS[..3] {
            c.execute_batch(m).unwrap();
        }
        c.pragma_update(None, "user_version", 3).unwrap();
        c.execute(
            "INSERT INTO locations (uuid, root, online, last_seen) VALUES ('u', '/r', 1, 1)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO files (location_id, path, size, mtime_ns, dev, ino, sample, full, seen) \
             VALUES (1, 'a', 5, 1, 1, 1, ?1, ?1, 1)",
            [vec![7u8; 32]],
        )
        .unwrap();
        c.execute(
            "INSERT INTO scan_roots (location_id, path) VALUES (1, '')",
            [],
        )
        .unwrap();
    }
    let idx = crate::index::Index::open(&path).unwrap();
    let (full, algo, n): (Vec<u8>, String, i64) = idx
        .conn
        .query_row(
            "SELECT full, algo, (SELECT COUNT(*) FROM scan_roots) FROM files",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((full, algo.as_str(), n), (vec![7u8; 32], "blake3", 1));
    // The cascade from locations still works on the rebuilt table.
    assert!(idx.forget("u").unwrap());
    let left: i64 = idx
        .conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

#[test]
fn index_add_ssh_records_a_remote_location_and_hashes_collisions_there() {
    let e = Estate::new("remote-add");
    let h = FakeHost::linux();
    h.put("/storage/a.bin", &content(1), T0);
    h.put("/storage/sub/b.bin", &content(1), T0);
    h.put("/storage/u.bin", b"unique", T0);
    let s = e.add_remote(URL, &h).unwrap();
    assert_eq!(s.uuid, UUID);
    assert_eq!(s.root, PathBuf::from("/storage"));
    assert_eq!((s.seen, s.new, s.hashed), (3, 3, Some(2)));
    assert_eq!(s.algo.as_deref(), Some("blake3"));
    assert!(s.complete);
    let (host, root, cow) = loc_row(&e, UUID);
    assert_eq!(
        (host.as_str(), root.as_str(), cow),
        ("ssh://adroid", "/storage", Some(0))
    );
    let rows = remote_rows(&e, UUID);
    let b3 = blake3::hash(&content(1)).as_bytes().to_vec();
    assert_eq!(
        rows[0],
        (
            "a.bin".into(),
            BIG as i64,
            Some(b3.clone()),
            "blake3".into()
        )
    );
    assert_eq!(rows[1].2, Some(b3));
    assert_eq!(rows[2].2, None, "unique size: never hashed");
    // Nothing local was read for it.
    assert!(e.reads().is_empty());
}

#[test]
fn a_rescan_rehashes_only_what_changed_and_drops_what_vanished() {
    let e = Estate::new("remote-rescan");
    let h = FakeHost::linux();
    h.put("/storage/a.bin", &content(1), T0);
    h.put("/storage/b.bin", &content(1), T0);
    h.put("/storage/c.bin", &content(1), T0);
    e.add_remote(URL, &h).unwrap();
    let hashes_before = h.count(|c| c.get(2).map(String::as_str) == Some(HASH_SCRIPT));
    h.put("/storage/b.bin", &content(2), T0 + 5);
    h.remove("/storage/c.bin");
    let s = e.add_remote(URL, &h).unwrap();
    assert_eq!((s.unchanged, s.changed, s.vanished), (1, 1, 1));
    let rows = remote_rows(&e, UUID);
    assert_eq!(rows.len(), 2);
    // b changed: its old digest is gone. a and b no longer share a size
    // with anything but each other → both hashed again only if needed.
    assert_ne!(rows[1].2, rows[0].2);
    let hashed: Vec<_> = h
        .calls()
        .into_iter()
        .filter(|c| c.get(2).map(String::as_str) == Some(HASH_SCRIPT))
        .skip(hashes_before)
        .flat_map(|c| c[5..].to_vec())
        .collect();
    assert_eq!(hashed, ["/storage/b.bin"], "a kept its digest");
    // An incomplete listing drops nothing.
    h.remove("/storage/a.bin");
    h.find_code.store(1, std::sync::atomic::Ordering::SeqCst);
    let s = e.add_remote(URL, &h).unwrap();
    assert!(!s.complete);
    assert_eq!(s.vanished, 0);
    assert_eq!(remote_rows(&e, UUID).len(), 2);
}

#[test]
fn an_unreachable_host_fails_the_add_and_records_nothing() {
    let e = Estate::new("remote-down");
    let h = FakeHost::linux();
    h.down.store(true, std::sync::atomic::Ordering::SeqCst);
    let err = e.add_remote(URL, &h).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    let n: i64 = e
        .idx
        .conn
        .query_row("SELECT COUNT(*) FROM locations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn status_probes_remote_hosts_online_and_offline() {
    let e = Estate::new("remote-status");
    let h = FakeHost::linux();
    h.put("/storage/a.bin", b"a", T0);
    e.add_remote(URL, &h).unwrap();
    let st = e.with_probe(|p| e.idx.status_with(p)).unwrap();
    assert_eq!(st.len(), 1);
    assert_eq!((st[0].host.as_str(), st[0].online), ("ssh://adroid", true));
    assert_eq!(st[0].files, 1);
    e.hosts_down.lock().unwrap().insert("ssh://adroid".into());
    let st = e.with_probe(|p| e.idx.status_with(p)).unwrap();
    assert!(!st[0].online, "unreachable host → offline, no panic");
    assert_eq!(st[0].files, 1, "rows kept");
}

#[test]
fn local_and_remote_blake3_copies_form_one_group() {
    let e = Estate::new("remote-group-b3");
    write(&e.loc("A").join("f.bin"), &content(1));
    e.add("A");
    let h = FakeHost::linux();
    h.put("/storage/f.bin", &content(1), T0);
    e.add_remote(URL, &h).unwrap();
    let r = e.report(&Estate::opts());
    assert_eq!(r.groups.len(), 1, "{:#?}", r.groups);
    let g = &r.groups[0];
    assert_eq!(g.algo, "blake3");
    assert_eq!(g.unique_copies, 2);
    assert_eq!(
        g.hash.as_deref(),
        Some(blake3::hash(&content(1)).to_hex().as_str())
    );
    assert!(r.unverifiable.is_empty());
}

#[test]
fn mixed_algorithms_never_group_and_are_reported_unverifiable() {
    let e = Estate::new("remote-mixed");
    write(&e.loc("A").join("f.bin"), &content(1));
    write(&e.loc("A").join("g.bin"), &content(1));
    e.add("A");
    let h = FakeHost::linux().with_tools(&["md5sum", "find", "stat"]);
    h.put("/storage/r1.bin", &content(1), T0);
    h.put("/storage/r2.bin", &content(1), T0);
    let s = e.add_remote(URL, &h).unwrap();
    assert_eq!(s.algo.as_deref(), Some("md5"));
    assert_eq!(remote_rows(&e, UUID)[0].3, "md5");
    let r = e.report(&Estate::opts());
    // Two groups: local blake3 pair, remote md5 pair — never one of four.
    let mut algos: Vec<(&str, u64)> = r
        .groups
        .iter()
        .map(|g| (g.algo.as_str(), g.unique_copies))
        .collect();
    algos.sort();
    assert_eq!(algos, [("blake3", 2), ("md5", 2)]);
    let md5 = r.groups.iter().find(|g| g.algo == "md5").unwrap();
    assert_eq!(
        md5.hash.as_deref(),
        Some(hex(&FakeHost::digest("md5sum", &content(1))).as_str())
    );
    assert!(md5
        .copies
        .iter()
        .all(|c| c.location_uuid == UUID && c.path.starts_with("/storage")));
    assert_eq!(r.unverifiable.len(), 1, "{:#?}", r.unverifiable);
    let u = &r.unverifiable[0];
    assert_eq!(u.size, BIG as u64);
    assert_eq!(u.algos, ["blake3", "md5"]);
    assert_eq!(u.copies.len(), 4);
}

#[test]
fn a_remote_row_never_takes_a_hash_from_a_local_file_at_the_same_path() {
    // The remote root is a path that also exists here, holding other bytes.
    let e = Estate::new("remote-shadow");
    let shadow = e.loc("shadow");
    write(&shadow.join("f.bin"), &same_ends(1));
    write(&e.loc("A").join("f.bin"), &content(1));
    e.add("A");
    let h = FakeHost::linux().with_tools(&["md5sum", "find", "stat"]);
    h.put(&format!("{}/f.bin", shadow.display()), &content(1), T0);
    let url = format!("ssh://adroid/{}", shadow.display());
    let s = e.add_remote(&url, &h).unwrap();
    let rows = remote_rows(&e, &s.uuid);
    let md5 = FakeHost::digest("md5sum", &content(1));
    assert_eq!(rows[0].2.as_deref(), Some(&md5[..]));
    // A local scan, a report and a plan never read the local look-alike.
    e.add("A");
    let r = e.report(&Estate::opts());
    assert!(
        !e.reads().iter().any(|p| p.starts_with(&shadow)),
        "{:?}",
        e.reads()
    );
    assert_eq!(remote_rows(&e, &s.uuid)[0].2.as_deref(), Some(&md5[..]));
    assert_eq!(remote_rows(&e, &s.uuid)[0].3, "md5");
    assert!(r.groups.is_empty());
    assert_eq!(r.unverifiable.len(), 1);
}

#[test]
fn a_remote_location_is_its_own_device_per_filesystem() {
    assert_eq!(
        crate::index_report::filesystem_of("ssh://adroid/a1b2:/storage"),
        "ssh://adroid/a1b2"
    );
    assert_eq!(
        crate::index_report::filesystem_of("ssh://adroid:/storage"),
        "ssh://adroid:/storage"
    );
    assert_ne!(
        crate::index_report::filesystem_of("ssh://a/1:/x"),
        crate::index_report::filesystem_of("ssh://b/1:/x")
    );
}

// ---- plan and apply --------------------------------------------------

fn policy(min: u64) -> Policy {
    Policy {
        min_copies: min,
        distinct: Distinct::None,
        locations: Vec::new(),
    }
}

#[test]
fn plan_marks_remote_copies_and_apply_skips_them_explicitly() {
    let e = Estate::new("remote-apply");
    write(&e.loc("A").join("f.bin"), &content(1));
    write(&e.loc("A").join("g.bin"), &content(1));
    e.add("A");
    let h = FakeHost::linux();
    h.put("/storage/f.bin", &content(1), T0);
    e.add_remote(URL, &h).unwrap();
    let plan = e
        .with_probe(|p| plan_with(&e.idx, &policy(1), &Progress::default(), p))
        .unwrap();
    assert_eq!(plan.groups.len(), 1);
    let g = &plan.groups[0];
    // The local copy is kept (a copy we can verify and reach), the other
    // local one is quarantined, the remote one is listed but marked.
    assert_eq!(g.kept.len(), 1);
    assert!(!g.kept[0].remote);
    assert_eq!(g.kept[0].path, e.loc("A").join("f.bin"));
    let remote: Vec<_> = g.actions.iter().filter(|a| a.remote).collect();
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].path, PathBuf::from("/storage/f.bin"));
    assert_eq!(remote[0].location_uuid, UUID);
    assert_eq!(plan.summary.actions, 1, "local actions only");
    assert_eq!(plan.summary.remote_actions, 1);
    assert_eq!(plan.summary.quarantine_bytes, BIG as u64);
    let json = serde_json::to_value(&plan).unwrap();
    assert_eq!(
        json["groups"][0]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["remote"] == true)
            .count(),
        1
    );

    let path = e.dir.join("plan.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    // Dry run: reported, nothing written.
    let dry = e
        .with_probe(|p| {
            apply_with(
                &path,
                &e.idx,
                &ApplyOptions {
                    dry_run: true,
                    paranoid: false,
                },
                p,
            )
        })
        .unwrap();
    assert_eq!(dry.count(Status::RemoteSkipped), 1);
    assert!(!crate::apply::journal_path(&path).exists());
    let run = e
        .with_probe(|p| apply_with(&path, &e.idx, &ApplyOptions::default(), p))
        .unwrap();
    assert_eq!(run.count(Status::Done), 1);
    assert_eq!(run.count(Status::RemoteSkipped), 1);
    assert!(!run.has_errors());
    let j = read_journal(&crate::apply::journal_path(&path)).unwrap();
    let skipped: Vec<_> = j
        .iter()
        .filter(|l| l.status == Status::RemoteSkipped)
        .collect();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].from, PathBuf::from("/storage/f.bin"));
    assert_eq!(skipped[0].to, None);
    // The fake host was only read.
    assert!(h.files.lock().unwrap().contains_key("/storage/f.bin"));
    // undo ignores it.
    let u = crate::apply::undo(&crate::apply::journal_path(&path), false).unwrap();
    assert_eq!(u.count(Status::Undone), 1);
}

#[test]
fn a_remote_md5_group_is_planned_but_every_action_is_skipped() {
    let e = Estate::new("remote-md5-plan");
    let h = FakeHost::linux().with_tools(&["md5sum", "find", "stat"]);
    h.put("/storage/a.bin", &content(1), T0);
    h.put("/storage/b.bin", &content(1), T0);
    e.add_remote(URL, &h).unwrap();
    e.idx
        .configure(UUID, Some(Role::Normal), "ssh://adroid")
        .unwrap();
    let plan = e
        .with_probe(|p| plan_with(&e.idx, &policy(1), &Progress::default(), p))
        .unwrap();
    assert_eq!(plan.groups.len(), 1);
    let g = &plan.groups[0];
    assert_eq!(g.algo, "md5");
    assert_eq!(g.hash.len(), 32);
    assert!(g.kept[0].remote);
    assert!(g.actions.iter().all(|a| a.remote));
    assert_eq!(plan.summary.actions, 0);
    let path = e.dir.join("plan.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let run = e
        .with_probe(|p| apply_with(&path, &e.idx, &ApplyOptions::default(), p))
        .unwrap();
    assert_eq!(run.count(Status::RemoteSkipped), 1);
    assert_eq!(run.outcomes.len(), 1);
}

#[test]
fn every_remote_command_of_a_whole_run_is_read_only() {
    let e = Estate::new("remote-readonly");
    write(&e.loc("A").join("f.bin"), &content(1));
    e.add("A");
    for h in [FakeHost::linux(), FakeHost::bsd()] {
        h.put("/storage/f.bin", &content(1), T0);
        h.put("/storage/g.bin", &content(1), T0);
        e.add_remote(URL, &h).unwrap();
        e.add_remote(URL, &h).unwrap();
        let _ = remote_online(&h, "/storage");
        assert!(h.calls().len() >= 4);
        for argv in h.calls() {
            for w in &argv {
                for bad in [
                    "rm", "mv", "-delete", "-exec", "-execdir", "-ok", "tee", "dd",
                ] {
                    assert_ne!(w, bad, "{argv:?}");
                }
                let stripped = w.replace("2>/dev/null", "").replace(">/dev/null", "");
                assert!(!stripped.contains('>'), "redirect to a file: {argv:?}");
                for bad in ["rm ", "mv ", "chmod", "truncate", "touch "] {
                    assert!(!w.contains(bad), "{argv:?}");
                }
            }
        }
    }
}
