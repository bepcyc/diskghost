//! Tests of remote (ssh) locations. No real ssh anywhere: [`FakeHost`]
//! answers the commands in memory, and [`LocalExec`] runs the very same
//! read-only commands on this machine, inside the test's own temp directory
//! only — never anywhere else (owner's hard safety rule).

use std::io;
use std::path::PathBuf;

#[cfg(unix)]
use crate::apply::read_journal;
use crate::apply::{apply_with, ApplyOptions, Status};
use crate::index::Role;
use crate::plan::plan_with;
use crate::policy::{Distinct, Policy};
use crate::remote::*;
use crate::testkit::*;
use crate::Progress;

const URL: &str = "ssh://dg-test//storage";
const UUID: &str = "ssh://dg-test/a1b2c3d4";
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
        host: "dg-test".into(),
        ssh: "ssh".into(),
        extra: SshExec::new("dg-test").extra,
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
            "dg-test",
            r"'find' '/a b' '-printf' '%p\0'",
        ]
        .map(String::from)
    );
}

#[test]
fn ssh_urls_split_into_host_and_path() {
    let ok = |u: &str| parse_ssh_url(u).unwrap();
    assert_eq!(
        ok("ssh://dg-test//storage"),
        ("dg-test".into(), "/storage".into())
    );
    assert_eq!(
        ok("ssh://u@nas.lan/data/x"),
        ("u@nas.lan".into(), "data/x".into())
    );
    assert_eq!(ok("ssh://dg-test"), ("dg-test".into(), ".".into()));
    assert_eq!(ok("ssh://dg-test/"), ("dg-test".into(), ".".into()));
    for bad in [
        "dg-test:/x",
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
    assert!(validate_remote_host("dg-test").is_ok());
    assert!(validate_remote_host("u@nas.lan").is_ok());
    assert!(validate_remote_host("-oProxyCommand=x").is_err());
    assert!(validate_remote_host("").is_err());
    assert!(crate::index::validate_host("ssh://dg-test").is_ok());
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

#[test]
fn the_root_is_resolved_with_its_mountpoint_in_the_same_call() {
    let h = FakeHost::linux().with_mount(Some("/storage"));
    h.put("/storage/a/x", b"x", T0);
    let c = caps(&h);
    let r = resolve_root(&h, &c, "/storage/a").unwrap();
    assert_eq!(r.mount.as_deref(), Some("/storage"), "GNU stat -c %m");
    assert_eq!(h.calls().len(), 2, "probe + one resolve call");
    let b = FakeHost::bsd().with_mount(Some("/storage"));
    b.put("/storage/a/x", b"x", T0);
    let r = resolve_root(&b, &caps(&b), "/storage/a").unwrap();
    assert_eq!(r.mount.as_deref(), Some("/storage"), "last field of df -P");
    // Not obtainable, or not a path prefix of the root: unknown.
    let n = FakeHost::linux().with_mount(None);
    n.put("/storage/a/x", b"x", T0);
    assert_eq!(resolve_root(&n, &caps(&n), "/storage").unwrap().mount, None);
    let o = FakeHost::linux().with_mount(Some("/stor"));
    o.put("/storage/a/x", b"x", T0);
    assert_eq!(resolve_root(&o, &caps(&o), "/storage").unwrap().mount, None);
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
#[cfg(unix)]
struct LocalExec;

#[cfg(unix)]
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
#[cfg(target_os = "linux")] // real find/stat output is GNU-flavoured; BSD differs
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
    if c.stat != StatFlavor::None {
        // The real `stat -c %m` / `df -P` names the mountpoint above it.
        let m = r.mount.clone().expect("mountpoint of the temp dir");
        assert!(
            m == "/" || r.root == m || r.root.starts_with(&format!("{m}/")),
            "{m} is not a prefix of {}",
            r.root
        );
    }
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
    assert_eq!(v, 5);
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
    // The fake filesystem is mounted at `/`: root = the mountpoint, paths
    // relative to it, the folder a scan root.
    assert_eq!(s.root, PathBuf::from("/"));
    assert_eq!((s.seen, s.new, s.hashed), (3, 3, Some(2)));
    assert_eq!(s.algo.as_deref(), Some("blake3"));
    assert!(s.complete);
    let (host, root, cow) = loc_row(&e, UUID);
    assert_eq!(
        (host.as_str(), root.as_str(), cow),
        ("ssh://dg-test", "/", Some(0))
    );
    assert_eq!(scan_roots_of(&e, UUID), ["storage"]);
    let rows = remote_rows(&e, UUID);
    let b3 = blake3::hash(&content(1)).as_bytes().to_vec();
    assert_eq!(
        rows[0],
        (
            "storage/a.bin".into(),
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
    assert_eq!((st[0].host.as_str(), st[0].online), ("ssh://dg-test", true));
    assert_eq!(st[0].files, 1);
    e.hosts_down.lock().unwrap().insert("ssh://dg-test".into());
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

#[cfg(unix)]
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
    let url = format!("ssh://dg-test/{}", shadow.display());
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
    use crate::index_report::filesystem_of;
    assert_eq!(filesystem_of("ssh://dg-test/a1b2"), "ssh://dg-test/a1b2");
    assert_eq!(filesystem_of("ssh://dg-test/dev66"), "ssh://dg-test/dev66");
    assert_eq!(
        filesystem_of("ssh://dg-test:/storage"),
        "ssh://dg-test:/storage"
    );
    assert_eq!(filesystem_of("ssh://dg-test:/a:b"), "ssh://dg-test:/a:b");
    assert_ne!(filesystem_of("ssh://a/1"), filesystem_of("ssh://b/1"));
}

fn scan_roots_of(e: &Estate, uuid: &str) -> Vec<String> {
    let mut st = e
        .idx
        .conn
        .prepare(
            "SELECT s.path FROM scan_roots s JOIN locations l ON l.id = s.location_id \
             WHERE l.uuid = ?1 ORDER BY s.path",
        )
        .unwrap();
    st.query_map([uuid], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn location_count(e: &Estate) -> i64 {
    e.idx
        .conn
        .query_row("SELECT COUNT(*) FROM locations", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn two_folders_of_one_remote_filesystem_share_one_location_like_a_volume() {
    let e = Estate::new("remote-two-folders");
    let h = FakeHost::linux().with_mount(Some("/storage"));
    h.put("/storage/a/x", &content(1), T0);
    h.put("/storage/b/y", &content(1), T0);
    let sa = e.add_remote("ssh://dg-test//storage/a", &h).unwrap();
    let sb = e.add_remote("ssh://dg-test//storage/b", &h).unwrap();
    let uuid = "ssh://dg-test/a1b2c3d4";
    assert_eq!((sa.uuid.as_str(), sb.uuid.as_str()), (uuid, uuid));
    assert_eq!(sb.root, PathBuf::from("/storage"));
    assert_eq!(location_count(&e), 1);
    let (host, root, _) = loc_row(&e, uuid);
    assert_eq!(
        (host.as_str(), root.as_str()),
        ("ssh://dg-test", "/storage")
    );
    assert_eq!(scan_roots_of(&e, uuid), ["a", "b"]);
    let rows = remote_rows(&e, uuid);
    let paths: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(paths, ["a/x", "b/y"]);
    // Hashed on the host at mount + '/' + path.
    let hashed: Vec<String> = h
        .calls()
        .into_iter()
        .filter(|c| c.get(2).map(String::as_str) == Some(HASH_SCRIPT))
        .flat_map(|c| c[5..].to_vec())
        .collect();
    assert_eq!(hashed, ["/storage/a/x", "/storage/b/y"]);
    assert!(rows.iter().all(|r| r.2.is_some()));
    // A rescan of folder a after its file was removed drops only that row.
    h.remove("/storage/a/x");
    let s = e.add_remote("ssh://dg-test//storage/a", &h).unwrap();
    assert!(s.complete);
    assert_eq!((s.seen, s.vanished), (0, 1));
    let rows = remote_rows(&e, uuid);
    let paths: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(paths, ["b/y"], "b's rows stay");
    assert_eq!(scan_roots_of(&e, uuid), ["a", "b"]);
}

#[test]
fn a_gnu_host_uuid_is_exactly_host_and_filesystem_id() {
    let e = Estate::new("remote-uuid-gnu");
    let h = FakeHost::linux().with_mount(Some("/storage"));
    h.put("/storage/f.bin", b"f", T0);
    let s = e.add_remote(URL, &h).unwrap();
    assert_eq!(s.uuid, "ssh://dg-test/a1b2c3d4");
    assert_eq!(scan_roots_of(&e, "ssh://dg-test/a1b2c3d4"), [""]);
    assert_eq!(remote_rows(&e, "ssh://dg-test/a1b2c3d4")[0].0, "f.bin");
}

#[test]
fn without_a_usable_filesystem_id_the_uuid_is_host_and_folder() {
    let hosts = [
        // stat answers no dialect: no fs id at all.
        FakeHost::linux().with_stat("none"),
        // fs id known, mountpoint not obtainable.
        FakeHost::linux().with_mount(None),
        // fs id known, "mountpoint" not a prefix of the folder.
        FakeHost::linux().with_mount(Some("/elsewhere")),
    ];
    for (i, h) in hosts.iter().enumerate() {
        let e = Estate::new(&format!("remote-uuid-root-{i}"));
        h.put("/storage/sub/f.bin", b"f", T0);
        let s = e.add_remote(URL, h).unwrap();
        assert_eq!(s.uuid, "ssh://dg-test:/storage", "host {i}");
        let (_, root, _) = loc_row(&e, "ssh://dg-test:/storage");
        assert_eq!(root, "/storage", "host {i}");
        assert_eq!(
            scan_roots_of(&e, "ssh://dg-test:/storage"),
            [""],
            "host {i}"
        );
        assert_eq!(remote_rows(&e, "ssh://dg-test:/storage")[0].0, "sub/f.bin");
    }
}

// ---- plan and apply --------------------------------------------------

fn policy(min: u64) -> Policy {
    Policy {
        min_copies: min,
        distinct: Distinct::None,
        locations: Vec::new(),
    }
}

#[cfg(unix)]
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
        .configure(UUID, Some(Role::Normal), "ssh://dg-test")
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
                // Only output to /dev/null or another descriptor — never a file.
                let stripped = w
                    .replace("2>/dev/null", "")
                    .replace(">/dev/null", "")
                    .replace("2>&1", "");
                assert!(!stripped.contains('>'), "redirect to a file: {argv:?}");
                for bad in ["rm ", "mv ", "chmod", "truncate", "touch "] {
                    assert!(!w.contains(bad), "{argv:?}");
                }
            }
        }
    }
}

#[test]
#[cfg(unix)]
fn the_real_fallback_commands_list_a_local_folder_without_printf() {
    use std::os::unix::fs::MetadataExt;
    let e = Estate::new("remote-local-fallback");
    let root = e.loc("r");
    write(&root.join("a.bin"), &content(1));
    write(&root.join("sub dir/it's\nb.bin"), &content(2));
    let mut c = probe(&LocalExec).unwrap();
    if c.stat != StatFlavor::Gnu {
        return; // this machine's stat is not GNU-style: nothing to run
    }
    c.find_printf = false; // force `-print0` + the real STAT_SCRIPT
    let mut l = enum_remote(&LocalExec, &c, root.to_str().unwrap()).unwrap();
    assert!(l.complete);
    assert_eq!(l.skipped, 0);
    l.files.sort_by(|a, b| a.path.cmp(&b.path));
    let names: Vec<&str> = l.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["a.bin", "sub dir/it's\nb.bin"]);
    for f in &l.files {
        let m = std::fs::metadata(root.join(&f.path)).unwrap();
        assert_eq!((f.size, f.ino), (m.len(), m.ino()));
        assert_eq!(
            f.mtime_ns,
            m.mtime() as u64 * 1_000_000_000,
            "whole seconds"
        );
    }
}

// ---- the agent tier (M5) ---------------------------------------------

use crate::remote::{AgentBinary, AgentMode};
use std::sync::atomic::Ordering as AtomicOrdering;

fn test_bin() -> AgentBinary {
    AgentBinary {
        bytes: b"\x7fELF not really an agent, only bytes to upload".to_vec(),
        ver: env!("CARGO_PKG_VERSION").into(),
        os: "Linux".into(),
        arch: "x86_64".into(),
    }
}

fn x86() -> FakeHost {
    FakeHost::linux().with_arch("x86_64")
}

/// The kind of call: `probe`, `locate`, `upload`, `agent`, or the program.
fn kind(argv: &[String]) -> String {
    match argv {
        [sh, c, s, ..] if sh == "sh" && c == "-c" => match s.as_str() {
            s if s == PROBE_SCRIPT => "probe".into(),
            s if s == LOCATE_SCRIPT => "locate".into(),
            s if s == UPLOAD_SCRIPT => "upload".into(),
            s if s == RESOLVE_SCRIPT => "resolve".into(),
            s if s == HASH_SCRIPT => "hash".into(),
            s if s == STAT_SCRIPT => "stat".into(),
            _ => "sh?".into(),
        },
        [_, a] if a == "agent" => "agent".into(),
        [p, ..] => p.clone(),
        [] => String::new(),
    }
}

fn kinds(h: &FakeHost) -> Vec<String> {
    h.calls().iter().map(|c| kind(c)).collect()
}

#[test]
fn the_probe_tells_os_and_arch_even_without_hash_tools() {
    let h = x86().with_tools(&["find", "stat"]);
    let (info, caps) = probe_host(&h).unwrap();
    assert_eq!((info.os.as_str(), info.arch.as_str()), ("Linux", "x86_64"));
    // The shell tier is unusable, but that is not the probe's verdict on
    // the host: the agent tier may still serve it.
    assert_eq!(caps.unwrap_err().kind(), io::ErrorKind::Unsupported);
    assert!(PROBE_SCRIPT.contains("uname -m"));
    assert_eq!(h.calls().len(), 1, "still one call");
}

#[test]
fn the_agent_file_name_is_content_addressed() {
    let b = test_bin();
    let hex8 = &blake3::hash(&b.bytes).to_hex()[..8];
    assert_eq!(
        b.name(),
        format!("diskghost-agent-{}-{hex8}", env!("CARGO_PKG_VERSION"))
    );
    let mut other = test_bin();
    other.bytes.push(0);
    assert_ne!(other.name(), b.name());
    assert!(!b.name().contains(' ') && !b.name().contains('/'));
}

#[test]
fn ensure_agent_locates_uploads_then_says_hello_in_that_order() {
    let h = x86();
    let b = test_bin();
    let (info, _) = probe_host(&h).unwrap();
    let p = ensure_agent(&h, &info, &b).unwrap().expect("agent");
    let want = format!("/home/u/.cache/diskghost/{}", b.name());
    assert_eq!(p, PathBuf::from(&want));
    assert_eq!(kinds(&h), ["probe", "locate", "upload", "agent"]);
    let calls = h.calls();
    // The upload: one script, streamed via stdin, into the cache dir only.
    let up = &calls[2];
    assert_eq!(&up[4..], [b.name(), b.bytes.len().to_string()]);
    let s = &up[2];
    let at = |n: &str| s.find(n).unwrap_or_else(|| panic!("{n} in {s}"));
    assert!(s.contains(r#""$HOME/.cache/diskghost""#), "{s}");
    assert!(at("mkdir -p") < at("cat >") && at("cat >") < at("chmod 755"));
    assert!(at("chmod 755") < at("mv -n"));
    assert_eq!(h.inputs.lock().unwrap()[0], b.bytes, "binary sent on stdin");
    assert_eq!(h.cache.lock().unwrap().get(&want), Some(&b.bytes));
    // The hello: run the uploaded file itself.
    assert_eq!(calls[3], [want.clone(), "agent".into()]);
    let hello = String::from_utf8(h.inputs.lock().unwrap()[1].clone()).unwrap();
    assert!(hello.contains(r#""op":"hello""#), "{hello}");
}

#[test]
fn a_second_run_reuses_the_uploaded_agent() {
    let h = x86();
    let b = test_bin();
    let (info, _) = probe_host(&h).unwrap();
    let first = ensure_agent(&h, &info, &b).unwrap();
    h.calls.lock().unwrap().clear();
    let again = ensure_agent(&h, &info, &b).unwrap();
    assert_eq!(first, again);
    assert_eq!(kinds(&h), ["locate", "agent"], "no second upload");
}

#[test]
fn unsupported_os_or_arch_never_uploads() {
    let b = test_bin();
    for h in [
        FakeHost::linux().with_arch("aarch64"),
        FakeHost::linux(), // no arch told
        FakeHost::bsd().with_arch("x86_64"),
    ] {
        let (info, _) = probe_host(&h).unwrap();
        assert_eq!(ensure_agent(&h, &info, &b).unwrap(), None);
        assert_eq!(kinds(&h), ["probe"], "{info:?}");
    }
    // A local binary for another target is not offered either.
    let h = x86();
    let (info, _) = probe_host(&h).unwrap();
    let mut arm = test_bin();
    arm.arch = "aarch64".into();
    assert_eq!(ensure_agent(&h, &info, &arm).unwrap(), None);
    assert_eq!(kinds(&h), ["probe"]);
}

#[test]
fn a_hello_mismatch_falls_back_to_the_shell_and_records_it() {
    let e = Estate::new("agent-mismatch");
    let h = x86();
    h.put("/storage/a.bin", &content(1), T0);
    h.put("/storage/b.bin", &content(1), T0);
    *h.hello.lock().unwrap() = Some(
        r#"{"v":1,"agent":"diskghost","ver":"0.0.1","os":"linux","arch":"x86_64","blake3":true}"#
            .into(),
    );
    let b = test_bin();
    let (info, _) = probe_host(&h).unwrap();
    assert_eq!(ensure_agent(&h, &info, &b).unwrap(), None);
    let s = e
        .add_remote_via(URL, &h, AgentMode::Auto, Some(&b))
        .unwrap();
    assert_eq!(s.transport.as_deref(), Some("shell"));
    let note = s.agent_note.clone().expect("the fallback is explained");
    assert!(note.contains("0.0.1") && note.contains("hello"), "{note}");
    assert_eq!((s.seen, s.hashed), (2, Some(2)));
    assert_eq!(transport_of(&e, UUID), "shell");
    // The broken agent is not uploaded again on the next run.
    let uploads = h.count(|c| kind(c) == "upload");
    e.add_remote_via(URL, &h, AgentMode::Auto, Some(&b))
        .unwrap();
    assert_eq!(h.count(|c| kind(c) == "upload"), uploads);
    assert_eq!(uploads, 1);
}

#[test]
fn a_failed_upload_falls_back_to_the_shell() {
    let e = Estate::new("agent-upload-fail");
    let h = x86();
    h.upload_fail.store(true, AtomicOrdering::SeqCst);
    h.put("/storage/a.bin", &content(1), T0);
    let s = e
        .add_remote_via(URL, &h, AgentMode::Auto, Some(&test_bin()))
        .unwrap();
    assert_eq!(s.transport.as_deref(), Some("shell"));
    let note = s.agent_note.clone().expect("the fallback is explained");
    assert!(
        note.contains("upload") && note.contains("No space left"),
        "{note}"
    );
    assert!(
        !kinds(&h).contains(&"agent".to_string()),
        "no hello of a failed upload"
    );
}

#[test]
fn agent_never_forces_the_shell_tier_without_a_single_agent_call() {
    let e = Estate::new("agent-never");
    let h = x86();
    h.put("/storage/a.bin", &content(1), T0);
    let s = e
        .add_remote_via(URL, &h, AgentMode::Never, Some(&test_bin()))
        .unwrap();
    assert_eq!(s.transport.as_deref(), Some("shell"));
    assert_eq!(s.agent_note, None, "never asked for: nothing to explain");
    for k in kinds(&h) {
        assert!(!["locate", "upload", "agent"].contains(&k.as_str()), "{k}");
    }
}

#[test]
fn an_sftp_only_host_says_files_would_be_read_over_the_network() {
    let e = Estate::new("agent-sftp-only");
    let h = x86();
    h.no_shell.store(true, AtomicOrdering::SeqCst);
    let err = e
        .add_remote_via(URL, &h, AgentMode::Auto, Some(&test_bin()))
        .unwrap_err();
    let m = err.to_string();
    assert!(m.contains("remote shell unavailable on dg-test"), "{m}");
    assert!(m.contains("read over the network"), "{m}");
    assert!(m.contains("upload an agent or enable shell"), "{m}");
    assert_eq!(location_count(&e), 0);
    // A host that cannot be reached at all is still "unreachable".
    let h = x86();
    h.down.store(true, AtomicOrdering::SeqCst);
    let err = e
        .add_remote_via(URL, &h, AgentMode::Auto, Some(&test_bin()))
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    assert!(err.to_string().contains("unreachable"), "{err}");
    assert!(!err.to_string().contains("read over the network"), "{err}");
}

#[test]
fn a_host_with_neither_tools_nor_an_agent_is_unusable() {
    let e = Estate::new("agent-unusable");
    let h = FakeHost::linux().with_arch("aarch64").with_tools(&["find"]);
    let err = e
        .add_remote_via(URL, &h, AgentMode::Auto, Some(&test_bin()))
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    assert!(err.to_string().contains("unusable"), "{err}");
}

fn transport_of(e: &Estate, uuid: &str) -> String {
    e.idx
        .conn
        .query_row(
            "SELECT transport FROM locations WHERE uuid = ?1",
            [uuid],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn a_v4_index_migrates_to_v5_keeping_its_rows_as_shell_locations() {
    let e = Estate::new("agent-migrate-v5");
    let path = e.dir.join("v4.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        for m in &crate::index::MIGRATIONS[..4] {
            c.execute_batch(m).unwrap();
        }
        c.pragma_update(None, "user_version", 4).unwrap();
        c.execute(
            "INSERT INTO locations (uuid, host, root, online, last_seen) \
             VALUES ('ssh://h/1', 'ssh://h', '/', 1, 1)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO files (location_id, path, size, mtime_ns, dev, ino, algo, full, seen) \
             VALUES (1, 'a', 5, 1, 0, 1, 'md5', ?1, 1)",
            [vec![7u8; 16]],
        )
        .unwrap();
        c.execute(
            "INSERT INTO scan_roots (location_id, path) VALUES (1, 'data')",
            [],
        )
        .unwrap();
    }
    let idx = crate::index::Index::open(&path).unwrap();
    let v: i64 = idx
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 5);
    let row: (Vec<u8>, String, String, String) = idx
        .conn
        .query_row(
            "SELECT f.full, f.algo, l.transport, (SELECT path FROM scan_roots) \
             FROM files f JOIN locations l ON l.id = f.location_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (vec![7u8; 16], "md5".into(), "shell".into(), "data".into())
    );
    assert!(idx
        .conn
        .execute("UPDATE locations SET transport = 'ftp'", [])
        .is_err());
    assert!(idx
        .conn
        .execute("UPDATE locations SET transport = 'agent'", [])
        .is_ok());
}

/// The real commands on this machine (like [`LocalExec`]), with the agent
/// "already uploaded": `locate` answers `have`, and running it serves the
/// request frames in-process with [`crate::agent::serve`]. Records every
/// argv and every frame; refuses any upload.
#[cfg(unix)]
struct LocalAgent {
    calls: std::sync::Mutex<Vec<Vec<String>>>,
    frames: std::sync::Mutex<Vec<serde_json::Value>>,
    /// Cut an `enumerate` answer after its first file frame (a dropped
    /// connection, a killed agent).
    cut: std::sync::atomic::AtomicBool,
}

/// Where the "uploaded" agent sits (never created: `LocalAgent` serves it).
#[cfg(unix)]
const LOCAL_AGENT_DIR: &str = "/nonexistent/.cache/diskghost";

#[cfg(unix)]
impl LocalAgent {
    fn new() -> LocalAgent {
        LocalAgent {
            calls: std::sync::Mutex::new(Vec::new()),
            frames: std::sync::Mutex::new(Vec::new()),
            cut: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[cfg(unix)]
impl RemoteExec for LocalAgent {
    fn exec(&self, argv: &[&str]) -> io::Result<ExecOut> {
        self.calls
            .lock()
            .unwrap()
            .push(argv.iter().map(|s| s.to_string()).collect());
        match argv {
            ["sh", "-c", s, "sh", name] if *s == LOCATE_SCRIPT => Ok(ExecOut {
                stdout: format!("have {LOCAL_AGENT_DIR}/{name}\n").into_bytes(),
                ..ExecOut::default()
            }),
            _ => LocalExec.exec(argv),
        }
    }

    fn exec_stdin(&self, argv: &[&str], input: &[u8]) -> io::Result<ExecOut> {
        self.calls
            .lock()
            .unwrap()
            .push(argv.iter().map(|s| s.to_string()).collect());
        match argv {
            [p, "agent"] if p.starts_with(&format!("{LOCAL_AGENT_DIR}/")) => {
                for l in String::from_utf8_lossy(input).lines() {
                    self.frames
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(l).unwrap());
                }
                let mut out = Vec::new();
                crate::agent::serve(input, &mut out)?;
                let enumerate = String::from_utf8_lossy(input).contains(r#""op":"enumerate""#);
                if enumerate && self.cut.load(AtomicOrdering::SeqCst) {
                    let first = out.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
                    out.truncate(first);
                    return Ok(ExecOut {
                        stdout: out,
                        stderr: b"Connection reset".to_vec(),
                        code: 1,
                    });
                }
                Ok(ExecOut {
                    stdout: out,
                    ..ExecOut::default()
                })
            }
            _ => Err(io::Error::other(format!("LocalAgent refuses {argv:?}"))),
        }
    }
}

#[cfg(unix)]
fn local_target_ok() -> bool {
    let (info, _) = probe_host(&LocalExec).unwrap();
    (info.os.as_str(), info.arch.as_str()) == crate::remote::AGENT_TARGET
}

#[cfg(unix)]
type Row = (String, i64, i64, i64, Option<Vec<u8>>, String);

#[cfg(unix)]
fn all_rows(idx: &crate::index::Index, uuid: &str) -> Vec<Row> {
    let mut st = idx
        .conn
        .prepare(
            "SELECT f.path, f.size, f.mtime_ns, f.ino, f.full, f.algo FROM files f \
             JOIN locations l ON l.id = f.location_id WHERE l.uuid = ?1 ORDER BY f.path",
        )
        .unwrap();
    let rows = st
        .query_map([uuid], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
#[cfg(unix)]
fn the_agent_tier_resolves_the_same_location_as_the_shell_tier() {
    if !local_target_ok() {
        return;
    }
    let e = Estate::new("agent-statfs-parity");
    let root = e.loc("r");
    write(&root.join("a.bin"), b"a");
    let c = probe(&LocalExec).unwrap();
    let shell = resolve_root(&LocalExec, &c, root.to_str().unwrap()).unwrap();
    let a = crate::remote::AgentExec {
        exec: &LocalAgent::new(),
        agent: format!("{LOCAL_AGENT_DIR}/{}", test_bin().name()),
    };
    let fs = a.stat_fs(root.to_str().unwrap()).unwrap();
    assert_eq!(fs.root.root, shell.root);
    if c.stat == StatFlavor::Gnu {
        // Same fs id and mountpoint as GNU `stat -f -c %i` / `stat -c %m`:
        // one remote filesystem is one location whichever tier lists it.
        assert_eq!(fs.root.fsid, shell.fsid);
        assert_eq!(fs.root.mount, shell.mount);
    }
}

#[test]
#[cfg(unix)]
fn the_agent_tier_and_the_shell_tier_index_the_same_rows() {
    if !local_target_ok() {
        return;
    }
    let e = Estate::new("agent-parity");
    let root = e.loc("r");
    write(&root.join("a.bin"), &content(1));
    write(&root.join("sub dir/it's b.bin"), &content(1));
    write(&root.join("c.bin"), &content(2));
    write(&root.join("u.txt"), b"unique");
    std::fs::hard_link(root.join("a.bin"), root.join("a-link.bin")).unwrap();
    let url = format!("ssh://local/{}", root.display());
    let shell = e
        .add_remote_via(&url, &LocalExec, AgentMode::Never, None)
        .unwrap();
    assert_eq!(shell.transport.as_deref(), Some("shell"));

    let idx2 = crate::index::Index::open(&e.dir.join("db2/index.db")).unwrap();
    let la = LocalAgent::new();
    let agent = crate::index::remote_scan_with(
        &idx2,
        &url,
        &la,
        AgentMode::Auto,
        Some(&test_bin()),
        &crate::Progress::default(),
    )
    .unwrap();
    assert_eq!(agent.transport.as_deref(), Some("agent"));
    assert_eq!(agent.algo.as_deref(), Some("blake3"));
    assert_eq!(agent.uuid, shell.uuid, "one filesystem, one location");
    assert_eq!(agent.root, shell.root);
    assert_eq!((agent.seen, agent.new), (shell.seen, shell.new));
    assert_eq!(agent.hashed, Some(4), "a, its link, b and c collide");
    assert!(agent.complete);
    assert_eq!(transport_of_idx(&idx2, &agent.uuid), "agent");
    let (s_rows, a_rows) = (all_rows(&e.idx, &shell.uuid), all_rows(&idx2, &agent.uuid));
    assert_eq!(s_rows.len(), 5);
    let strip = |r: &[Row]| -> Vec<(String, i64, i64, i64)> {
        r.iter()
            .map(|(p, s, m, i, ..)| (p.clone(), *s, *m, *i))
            .collect()
    };
    assert_eq!(strip(&a_rows), strip(&s_rows));
    for r in &a_rows {
        // Rows are relative to the location root (the mountpoint).
        let bytes = std::fs::read(agent.root.join(&r.0)).unwrap();
        let want = (r.1 == BIG as i64).then(|| blake3::hash(&bytes).as_bytes().to_vec());
        assert_eq!(r.4, want, "{}", r.0);
    }
    if shell.algo.as_deref() == Some("blake3") {
        assert_eq!(a_rows, s_rows);
    }
    // Rescan through the agent: nothing changed, nothing re-hashed.
    let again = crate::index::remote_scan_with(
        &idx2,
        &url,
        &la,
        AgentMode::Auto,
        Some(&test_bin()),
        &crate::Progress::default(),
    )
    .unwrap();
    assert_eq!((again.unchanged, again.hashed), (5, Some(0)));
}

#[cfg(unix)]
fn transport_of_idx(idx: &crate::index::Index, uuid: &str) -> String {
    idx.conn
        .query_row(
            "SELECT transport FROM locations WHERE uuid = ?1",
            [uuid],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
#[cfg(unix)]
fn a_whole_agent_run_only_ever_sends_read_requests() {
    if !local_target_ok() {
        return;
    }
    let e = Estate::new("agent-readonly");
    let root = e.loc("r");
    write(&root.join("a.bin"), &content(1));
    write(&root.join("b.bin"), &content(1));
    let url = format!("ssh://local/{}", root.display());
    let la = LocalAgent::new();
    let s = e
        .add_remote_via(&url, &la, AgentMode::Auto, Some(&test_bin()))
        .unwrap();
    assert_eq!(s.transport.as_deref(), Some("agent"));
    assert!(std::fs::read(root.join("a.bin")).unwrap() == content(1));
    let frames = la.frames.lock().unwrap().clone();
    assert!(!frames.is_empty());
    for f in &frames {
        let op = f["op"].as_str().unwrap_or("");
        assert!(
            ["hello", "enumerate", "stat_fs", "hash"].contains(&op),
            "{f}"
        );
    }
    for argv in la.calls.lock().unwrap().iter() {
        let k = kind(argv);
        assert_ne!(k, "upload", "agent already there: no upload");
        for w in argv {
            for bad in ["rm ", "mv ", "chmod", "truncate", "touch ", "cat >"] {
                assert!(!w.contains(bad), "{argv:?}");
            }
        }
    }
}

#[test]
fn the_upload_is_the_only_writing_command_and_stays_in_the_cache_dir() {
    let e = Estate::new("agent-upload-scope");
    let h = x86();
    h.put("/storage/a.bin", &content(1), T0);
    // The fake agent answers hello only, so the scan itself errors out
    // after the handshake; what matters here is every argv sent.
    let _ = e.add_remote_via(URL, &h, AgentMode::Auto, Some(&test_bin()));
    let writing: Vec<Vec<String>> = h
        .calls()
        .into_iter()
        .filter(|argv| {
            argv.iter().any(|w| {
                ["mkdir", "cat >", "chmod", "mv "]
                    .iter()
                    .any(|bad| w.contains(bad))
            })
        })
        .collect();
    assert_eq!(writing.len(), 1, "{writing:#?}");
    assert_eq!(kind(&writing[0]), "upload");
    let script = &writing[0][2];
    for w in ["rm ", "rm -", "unlink", "-delete", "truncate"] {
        assert!(!script.contains(w), "{script}");
    }
    // Every path the script writes is under the cache dir.
    for line in script.lines() {
        if line.contains("cat >") || line.contains("chmod") || line.contains("mv -n") {
            assert!(line.contains("\"$t\"") || line.contains("\"$d/"), "{line}");
        }
    }
    assert!(script.contains(r#"d="$HOME/.cache/diskghost""#), "{script}");
    let cached: Vec<String> = h.cache.lock().unwrap().keys().cloned().collect();
    for p in cached {
        assert!(p.starts_with(&h.agent_dir()), "{p}");
    }
}

#[test]
fn an_unsupported_host_says_why_the_agent_was_not_used() {
    let e = Estate::new("agent-note-arch");
    let h = FakeHost::linux().with_arch("aarch64");
    h.put("/storage/a.bin", &content(1), T0);
    let s = e
        .add_remote_via(URL, &h, AgentMode::Auto, Some(&test_bin()))
        .unwrap();
    assert_eq!(s.transport.as_deref(), Some("shell"));
    let note = s.agent_note.expect("explained");
    assert!(
        note.contains("aarch64") && note.contains("x86_64"),
        "{note}"
    );
    // No agent binary for this build at all: said too.
    let h = x86();
    h.put("/storage/a.bin", &content(1), T0);
    let s = e.add_remote_via(URL, &h, AgentMode::Auto, None).unwrap();
    assert!(s.agent_note.is_some_and(|n| n.contains("this build")));
    // The agent ran: nothing to explain.
    let e2 = Estate::new("agent-note-none");
    let h = x86();
    let s = e2.add_remote_via(URL, &h, AgentMode::Auto, Some(&test_bin()));
    // (the fake agent answers hello only, so the scan fails after it)
    assert!(s.is_err());
}

// ---- AgentExec frame parsing and batching (characterization) ---------

/// Answers every `exec_stdin` with `answer(stdin)`; records each stdin.
struct Canned<F: Fn(&[u8]) -> ExecOut + Sync> {
    answer: F,
    inputs: std::sync::Mutex<Vec<Vec<u8>>>,
}

impl<F: Fn(&[u8]) -> ExecOut + Sync> RemoteExec for Canned<F> {
    fn exec(&self, _argv: &[&str]) -> io::Result<ExecOut> {
        Err(io::Error::other("Canned: exec_stdin only"))
    }
    fn exec_stdin(&self, _argv: &[&str], input: &[u8]) -> io::Result<ExecOut> {
        self.inputs.lock().unwrap().push(input.to_vec());
        Ok((self.answer)(input))
    }
}

fn canned<F: Fn(&[u8]) -> ExecOut + Sync>(answer: F) -> Canned<F> {
    Canned {
        answer,
        inputs: std::sync::Mutex::new(Vec::new()),
    }
}

fn answer(stdout: &str, code: i32) -> ExecOut {
    ExecOut {
        stdout: stdout.as_bytes().to_vec(),
        stderr: b"agent: killed".to_vec(),
        code,
    }
}

#[test]
fn an_enumerate_stream_without_its_end_frame_is_incomplete() {
    let f = r#"{"f":{"p":"a.bin","s":5,"m":7,"d":1,"i":9}}"#;
    for (out, code) in [
        (format!("{f}\n"), 0),
        (format!("{f}\n"), 1),
        (
            format!("{f}\n{{\"end\":{{\"n\":1,\"complete\":true}}}}\n"),
            0,
        ),
    ] {
        let c = canned(|_| answer(&out, code));
        let a = crate::remote::AgentExec {
            exec: &c,
            agent: "/a".into(),
        };
        let l = a.enumerate("/r").unwrap();
        assert_eq!(l.files.len(), 1);
        assert_eq!(
            (
                l.files[0].path.as_str(),
                l.files[0].size,
                l.files[0].mtime_ns,
                l.files[0].ino
            ),
            ("a.bin", 5, 7, 9)
        );
        assert_eq!(l.complete, out.contains("end") && code == 0, "{out} {code}");
    }
    // Bad frames: an absolute or empty path is skipped, never stored.
    let c = canned(|_| {
        answer(
            "{\"f\":{\"p\":\"/etc/x\",\"s\":1,\"m\":1,\"i\":1}}\n\
             {\"f\":{\"p\":\"\",\"s\":1,\"m\":1,\"i\":1}}\n\
             {\"skip\":{\"p\":\"x\",\"why\":\"y\"}}\n\
             {\"end\":{\"n\":0,\"complete\":true}}\n",
            0,
        )
    });
    let a = crate::remote::AgentExec {
        exec: &c,
        agent: "/a".into(),
    };
    let l = a.enumerate("/r").unwrap();
    assert!(l.files.is_empty());
    assert_eq!((l.skipped, l.complete), (3, true));
}

#[test]
fn a_line_that_is_not_json_is_an_error_not_a_guess() {
    let c = canned(|_| answer("{\"f\":{}}\nSegmentation fault\n", 0));
    let a = crate::remote::AgentExec {
        exec: &c,
        agent: "/a".into(),
    };
    assert_eq!(
        a.enumerate("/r").unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    // An error frame is an error too.
    let c = canned(|_| answer(r#"{"error":{"code":"not_found","msg":"gone"}}"#, 0));
    let a = crate::remote::AgentExec {
        exec: &c,
        agent: "/a".into(),
    };
    assert_eq!(a.stat_fs("x").unwrap_err().kind(), io::ErrorKind::NotFound);
}

#[test]
fn hash_requests_are_batched_in_one_session_and_aligned_by_path() {
    let n = crate::remote::AGENT_HASH_BATCH * 2 + 5;
    let paths: Vec<String> = (0..n).map(|i| format!("/r/f{i}")).collect();
    // The agent answers every path but the 7th, in reverse order.
    let c = canned(|input| {
        let mut out = String::new();
        for line in String::from_utf8_lossy(input).lines() {
            let req: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(req["op"], "hash");
            let ps: Vec<&str> = req["paths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p.as_str().unwrap())
                .collect();
            for p in ps.iter().rev().filter(|p| **p != "/r/f7") {
                let full = blake3::hash(p.as_bytes()).to_hex();
                out += &format!(
                    "{}\n",
                    serde_json::json!({"h": {"p": p, "full": full.as_str(), "sample": null, "err": null}})
                );
            }
            out += &format!("{}\n", serde_json::json!({"end": {"n": ps.len()}}));
        }
        answer(&out, 0)
    });
    let a = crate::remote::AgentExec {
        exec: &c,
        agent: "/a".into(),
    };
    let d = a.hash(&paths).unwrap();
    let inputs = c.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1, "one ssh session");
    let lines = String::from_utf8(inputs[0].clone()).unwrap();
    assert_eq!(
        lines.lines().count(),
        3,
        "batches of {}",
        crate::remote::AGENT_HASH_BATCH
    );
    assert_eq!(d.len(), n);
    for (i, (p, got)) in paths.iter().zip(&d).enumerate() {
        if i == 7 {
            assert_eq!(*got, None, "no h frame: unknown, not a guess");
        } else {
            assert_eq!(
                got.as_deref(),
                Some(&blake3::hash(p.as_bytes()).as_bytes()[..])
            );
        }
    }
    assert!(a.hash(&[]).unwrap().is_empty());
    assert_eq!(
        c.inputs.lock().unwrap().len(),
        1,
        "nothing sent for nothing"
    );
}

#[test]
#[cfg(unix)]
fn an_agent_scan_cut_short_drops_no_rows() {
    if !local_target_ok() {
        return;
    }
    let e = Estate::new("agent-cut");
    let root = e.loc("r");
    for n in ["a", "b", "c"] {
        write(&root.join(format!("{n}.bin")), &content(1));
    }
    let url = format!("ssh://local/{}", root.display());
    let la = LocalAgent::new();
    let s = e
        .add_remote_via(&url, &la, AgentMode::Auto, Some(&test_bin()))
        .unwrap();
    assert_eq!((s.new, s.transport.as_deref()), (3, Some("agent")));
    // Now the agent's stream dies after its first file frame.
    la.cut.store(true, AtomicOrdering::SeqCst);
    let s = e
        .add_remote_via(&url, &la, AgentMode::Auto, Some(&test_bin()))
        .unwrap();
    assert!(!s.complete);
    assert_eq!((s.seen, s.vanished), (1, 0));
    assert_eq!(all_rows(&e.idx, &s.uuid).len(), 3, "nothing dropped");
}

// ---- ssh:// scans (GUI / `diskghost scan`): a tree, no index ----------

fn tree_listing(url: &str, h: &dyn RemoteExec) -> io::Result<RemoteListing> {
    remote_listing_with(url, h, AgentMode::Never, None, &Progress::default())
}

#[test]
fn a_remote_folder_becomes_a_tree_rooted_at_its_ssh_url() {
    let h = FakeHost::linux();
    h.put("/storage/a.bin", &[0; 10], T0);
    h.put("/storage/sub/b.bin", &[0; 100], T0);
    h.put("/storage/sub/deep/c.bin", &[0; 1000], T0);
    // Sorts between `sub` and `sub/…` byte-wise: folders must still come
    // out whole.
    h.put("/storage/sub-x.txt", &[0; 7], T0);
    h.put("/storage/sub.d/e.bin", &[0; 3], T0);
    h.put("/storage/z.bin", &[0; 5], T0);
    let l = tree_listing(URL, &h).unwrap();
    assert_eq!(l.root, "/storage");
    assert_eq!(l.host, "dg-test");
    assert_eq!(l.url(), "ssh://dg-test//storage");
    assert!(l.listing.complete && !l.cancelled);
    let t = l.tree();
    let root = PathBuf::from("ssh://dg-test//storage");
    assert_eq!(t.root(), root.as_path());
    assert!(t.complete());
    let r = t.root_report(10);
    assert_eq!(r.total_size, 10 + 100 + 1000 + 7 + 3 + 5);
    assert_eq!(r.total_files, 6);
    assert_eq!(r.total_dirs, 3, "sub, sub/deep, sub.d");
    let sub = t.report(&root.join("sub"), 10).unwrap();
    assert_eq!((sub.total_size, sub.total_files), (1100, 2));
    let deep = t.report(&root.join("sub").join("deep"), 10).unwrap();
    assert_eq!(deep.total_size, 1000);
    let kids: Vec<PathBuf> = t
        .children(&root, 10)
        .unwrap()
        .into_iter()
        .map(|c| c.path)
        .collect();
    assert!(kids.contains(&root.join("sub")) && kids.contains(&root.join("sub.d")));
    // The URL root round-trips: a rescan of it lists the same folder.
    assert_eq!(parse_ssh_url(&l.url()).unwrap().1, "/storage");
}

#[test]
fn hard_links_on_the_remote_count_once_in_the_tree() {
    let h = FakeHost::linux();
    h.put_ino("/storage/a.bin", &[0; 50], T0, 77);
    h.put_ino("/storage/backup/a.bin", &[0; 50], T0, 77);
    h.put_ino("/storage/b.bin", &[0; 9], T0, 78);
    let t = tree_listing(URL, &h).unwrap().tree();
    let r = t.root_report(10);
    assert_eq!((r.total_size, r.total_files), (59, 2));
}

#[test]
fn an_incomplete_remote_listing_gives_a_partial_tree_never_a_complete_one() {
    let h = FakeHost::linux();
    h.put("/storage/a.bin", &[0; 10], T0);
    h.find_code.store(1, AtomicOrdering::SeqCst);
    let l = tree_listing(URL, &h).unwrap();
    assert!(!l.listing.complete);
    let t = l.tree();
    assert!(!t.complete(), "a partial listing must never be snapshotted");
    assert!(!t.cancelled(), "partial, not cancelled");
}

#[test]
fn a_cancelled_remote_scan_lists_nothing_and_is_marked_cancelled() {
    let h = FakeHost::linux();
    h.put("/storage/a.bin", &[0; 10], T0);
    let p = Progress::default();
    p.cancel();
    let l = remote_listing_with(URL, &h, AgentMode::Never, None, &p).unwrap();
    assert!(l.cancelled);
    assert_eq!(h.count(|c| kind(c) == "find"), 0, "{:?}", kinds(&h));
    let t = l.tree();
    assert!(t.cancelled() && !t.complete());
}

#[test]
fn a_remote_scan_only_probes_resolves_and_lists() {
    let h = FakeHost::linux();
    h.put("/storage/a.bin", &[0; 10], T0);
    h.put("/storage/b.bin", &[0; 10], T0);
    let l = tree_listing(URL, &h).unwrap();
    assert_eq!(kinds(&h), ["probe", "resolve", "find"]);
    assert_eq!(l.transport, "shell");
    assert_eq!(l.agent_note, None);
    // Neither tier reports free space: unknown, never made up.
    assert_eq!(l.disk, None);
}

#[test]
fn a_remote_scan_says_why_the_agent_was_not_used() {
    let h = FakeHost::linux().with_arch("aarch64");
    h.put("/storage/a.bin", &[0; 10], T0);
    let l = remote_listing_with(
        URL,
        &h,
        AgentMode::Auto,
        Some(&test_bin()),
        &Progress::default(),
    )
    .unwrap();
    assert_eq!(l.transport, "shell");
    let note = l.agent_note.unwrap();
    assert!(note.contains("aarch64"), "{note}");
    assert_eq!(h.count(|c| kind(c) == "upload"), 0);
}

#[test]
fn remote_scan_errors_carry_the_hosts_own_message() {
    // Not there.
    let h = FakeHost::linux();
    let err = tree_listing("ssh://dg-test//nope", &h).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(err.to_string().contains("not found"), "{err}");
    // Unreachable.
    let h = FakeHost::linux();
    h.down.store(true, AtomicOrdering::SeqCst);
    let err = tree_listing(URL, &h).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    assert!(err.to_string().contains("unreachable"), "{err}");
    // SFTP-only.
    let h = x86();
    h.no_shell.store(true, AtomicOrdering::SeqCst);
    let err = remote_listing_with(
        URL,
        &h,
        AgentMode::Auto,
        Some(&test_bin()),
        &Progress::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("read over the network"), "{err}");
    // Not an ssh URL / a bad host.
    let h = FakeHost::linux();
    assert_eq!(
        tree_listing("ssh://-oProxyCommand=x/", &h)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(h.calls().is_empty());
}

#[test]
#[cfg(unix)]
fn the_agent_tier_and_the_shell_tier_build_the_same_tree() {
    if !local_target_ok() {
        return;
    }
    let e = Estate::new("tree-agent-parity");
    let root = e.loc("r");
    write(&root.join("a.bin"), &content(1));
    write(&root.join("sub dir/it's b.bin"), &content(2));
    write(&root.join("sub dir/deeper/c.bin"), b"c");
    let url = format!("ssh://localhost/{}", root.to_str().unwrap());
    let shell = tree_listing(&url, &LocalExec).unwrap();
    let a = LocalAgent::new();
    let agent = remote_listing_with(
        &url,
        &a,
        AgentMode::Auto,
        Some(&test_bin()),
        &Progress::default(),
    )
    .unwrap();
    assert_eq!((shell.transport, agent.transport), ("shell", "agent"));
    let (ts, ta) = (shell.tree(), agent.tree());
    let (rs, ra) = (ts.root_report(10), ta.root_report(10));
    assert_eq!(
        (rs.total_size, rs.total_files, rs.total_dirs),
        (ra.total_size, ra.total_files, ra.total_dirs)
    );
    assert_eq!(rs.total_files, 3);
    assert_eq!(rs.total_dirs, 2);
    assert!(ts.complete() && ta.complete());
}

// ---- live progress while a remote listing streams --------------------

/// Delivers its stdout in `chunks` through `exec_live` and records the
/// progress counter after every chunk: a streamed listing must count what
/// arrived so far, not only store totals at the end.
struct Chunked {
    chunks: Vec<Vec<u8>>,
    progress: Progress,
    seen: std::sync::Mutex<Vec<u64>>,
}

impl RemoteExec for Chunked {
    fn exec(&self, _argv: &[&str]) -> io::Result<ExecOut> {
        Err(io::Error::other("Chunked answers exec_live only"))
    }
    fn exec_live(
        &self,
        _argv: &[&str],
        _stdin: Option<&[u8]>,
        on_chunk: &mut dyn FnMut(&[u8]),
    ) -> io::Result<ExecOut> {
        let mut all = Vec::new();
        for ch in &self.chunks {
            all.extend_from_slice(ch);
            on_chunk(ch);
            self.seen.lock().unwrap().push(self.progress.files());
        }
        Ok(ExecOut {
            stdout: all,
            stderr: Vec::new(),
            code: 0,
        })
    }
}

#[test]
fn a_streamed_find_listing_ticks_progress_as_records_arrive() {
    let rec = |p: &str, s: u64| format!("{s}\t1700000000\t9\t/r/{p}\0").into_bytes();
    let progress = Progress::default();
    let x = Chunked {
        chunks: vec![rec("a.bin", 5), rec("b.bin", 7)],
        progress: progress.clone(),
        seen: std::sync::Mutex::new(Vec::new()),
    };
    let caps = Capabilities {
        os: "Linux".into(),
        tools: vec![],
        find_printf: true,
        stat: StatFlavor::Gnu,
        hash: HashAlgo::Md5,
    };
    let l = enum_remote_live(&x, &caps, "/r", &progress).unwrap();
    assert_eq!(l.files.len(), 2);
    // The progress counter already held the first chunk's record.
    assert_eq!(*x.seen.lock().unwrap(), vec![1, 2]);
    assert_eq!((progress.files(), progress.bytes()), (2, 12));
}

#[test]
fn an_agent_enumerate_ticks_progress_per_file_frame() {
    let progress = Progress::default();
    let f1 = r#"{"f":{"p":"a.bin","s":5,"m":7,"i":9}}"#;
    let f2 = r#"{"f":{"p":"b.bin","s":7,"m":7,"i":10}}"#;
    let x = Chunked {
        chunks: vec![
            format!("{f1}\n").into_bytes(),
            format!("{f2}\n{{\"end\":{{\"n\":2,\"complete\":true}}}}\n").into_bytes(),
        ],
        progress: progress.clone(),
        seen: std::sync::Mutex::new(Vec::new()),
    };
    let a = AgentExec {
        exec: &x,
        agent: "agent".into(),
    };
    let l = a.enumerate_live("/r", &progress).unwrap();
    assert_eq!(l.files.len(), 2);
    assert_eq!(*x.seen.lock().unwrap(), vec![1, 2]);
    assert_eq!((progress.files(), progress.bytes()), (2, 12));
}

#[test]
#[cfg(unix)]
fn a_live_listing_on_real_tools_ends_with_exact_counts() {
    let e = Estate::new("remote-live-exec");
    let root = e.loc("r");
    write(&root.join("a.bin"), &content(1));
    write(&root.join("b.bin"), &content(2));
    let c = probe(&LocalExec).unwrap();
    let p = Progress::default();
    let l = enum_remote_live(&LocalExec, &c, root.to_str().unwrap(), &p).unwrap();
    assert_eq!(l.files.len(), 2);
    assert_eq!(p.files(), 2);
    assert_eq!(p.bytes(), l.files.iter().map(|f| f.size).sum::<u64>());
}
