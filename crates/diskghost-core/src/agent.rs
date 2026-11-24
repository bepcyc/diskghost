//! `diskghost agent`: the remote side of an `ssh://` location's top tier.
//!
//! `index add ssh://host/path` uploads this very program to the host
//! (see [`crate::remote::ensure_agent`]) and runs `<agent> agent` there over
//! ssh. The agent reads newline-delimited JSON requests on stdin and answers
//! each with one or more JSON frames on stdout, one per line, until EOF.
//! Requests are independent (no state between them), so one ssh session can
//! carry many. stdout carries frames only; diagnostics go to stderr.
//!
//! # Read-only by construction
//!
//! There is no request that writes, renames or removes anything: the four
//! operations below only list, stat and read files. This module names no
//! call that changes a file (a test checks its source), and the program
//! opens no index, cache or config before serving.
//!
//! # Protocol (`"v": 1`)
//!
//! | request | frames |
//! |---|---|
//! | `{"v":1,"op":"hello"}` | `{"v":1,"agent":"diskghost","ver":..,"os":..,"arch":..,"blake3":true}` |
//! | `{"op":"enumerate","root":"/abs","xdev":true}` | `{"f":{"p":<rel>,"s":<size>,"m":<mtime_ns>,"d":<dev>,"i":<ino>}}`… `{"skip":{"p":<lossy>,"why":..}}`… then `{"end":{"n":<files>,"complete":<bool>,"skipped":<n>}}` |
//! | `{"op":"stat_fs","root":<path>}` | `{"fs":{"root":<canonical>,"id":<hex or null>,"mount":<path or null>,"cow":<bool>,"rotational":<bool>}}` |
//! | `{"op":"hash","paths":[..],"sample":<bool>}` | `{"h":{"p":..,"full":<blake3 hex or null>,"sample":<blake3 hex or null>,"err":<null or msg>}}` per path, then `{"end":{"n":..}}` |
//!
//! Any failure of a request is one `{"error":{"code":..,"msg":..}}` frame
//! (`version`, `bad_request`, `unknown_op`, `not_found`); the agent keeps
//! serving. A file that cannot be hashed is an `err` in its own `h` frame —
//! it never aborts the batch.
//!
//! `enumerate` lists regular files like `find ROOT -xdev -type f`: every
//! hard-link name, no symlinks, sizes are logical lengths, and `xdev`
//! (default `true`, as `find -xdev`) keeps it on the root's filesystem; it
//! descends like the local walk. `complete` is `false` when a folder or file
//! could not be read — rows below it must not count as vanished. A name that
//! is not UTF-8 is a `skip` frame (lossy path) instead of an `f` frame.
//!
//! `stat_fs` answers what GNU `stat -f -c %i` (filesystem id, hex) and
//! `stat -c %m` (mountpoint) print, so a remote filesystem is one location
//! whichever tier lists it; `cow` and `rotational` use the same detection
//! as a local volume. `hash` reads spinning disks with few readers in path
//! order, like the local duplicate search.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde_json::{json, Value};

use crate::index_report::{hash_plan, in_order, HashItem};
use crate::{Options, Progress};

/// Protocol version (`"v"`).
pub const PROTOCOL: u64 = 1;

/// Answer every request line of `input` on `out` until EOF. Only an I/O
/// error on `out` ends it early.
pub fn serve<R: BufRead, W: Write>(input: R, mut out: W) -> io::Result<()> {
    for line in input.split(b'\n') {
        let line = line?;
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        handle(text, &mut out)?;
        out.flush()?;
    }
    Ok(())
}

fn emit(out: &mut dyn Write, v: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, v)?;
    out.write_all(b"\n")
}

fn error(code: &str, msg: impl Into<String>) -> Value {
    json!({"error": {"code": code, "msg": msg.into()}})
}

fn handle(line: &str, out: &mut dyn Write) -> io::Result<()> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => return emit(out, &error("bad_request", "a request is a JSON object")),
        Err(e) => return emit(out, &error("bad_request", e.to_string())),
    };
    if let Some(v) = req.get("v") {
        if v.as_u64() != Some(PROTOCOL) {
            return emit(
                out,
                &error(
                    "version",
                    format!("this agent speaks protocol v{PROTOCOL}, not v{v}"),
                ),
            );
        }
    }
    match req.get("op").and_then(Value::as_str) {
        Some("hello") => emit(out, &hello()),
        Some("enumerate") => enumerate(&req, out),
        Some("stat_fs") => stat_fs(&req, out),
        Some("hash") => hash(&req, out),
        Some(op) => emit(out, &error("unknown_op", format!("unknown op {op:?}"))),
        None => emit(out, &error("bad_request", "no \"op\"")),
    }
}

fn hello() -> Value {
    json!({
        "v": PROTOCOL,
        "agent": "diskghost",
        "ver": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "blake3": true,
    })
}

fn root_of(req: &Value) -> Result<&str, Value> {
    req.get("root")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| error("bad_request", "no \"root\""))
}

fn enumerate(req: &Value, out: &mut dyn Write) -> io::Result<()> {
    let root = match root_of(req) {
        Ok(r) => Path::new(r),
        Err(e) => return emit(out, &e),
    };
    if !root.is_absolute() {
        return emit(out, &error("bad_request", "\"root\" must be absolute"));
    }
    match std::fs::metadata(root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return emit(out, &error("not_found", "root is not a folder")),
        Err(e) => return emit(out, &error("not_found", e.to_string())),
    }
    let xdev = req.get("xdev").and_then(Value::as_bool).unwrap_or(true);
    let opts = Options {
        same_file_system: xdev,
        skip_remote: true,
        ..Options::default()
    };
    let progress = Progress::default();
    let (mut n, mut skipped, mut complete) = (0u64, 0u64, true);
    let mut failed: Option<io::Error> = None;
    let _ = crate::walk::walk(root, &opts, &progress, |b| {
        if b.skipped > 0 {
            complete = false;
        }
        if failed.is_some() {
            return;
        }
        for (p, why) in &b.skips {
            skipped += 1;
            if let Err(e) = emit(
                out,
                &json!({"skip": {"p": p.to_string_lossy(), "why": why}}),
            ) {
                failed = Some(e);
                progress.cancel();
                return;
            }
        }
        for (p, listed) in b.names_with_stats() {
            // The walk's own `stat` when it took one (links are not
            // followed here, as `symlink_metadata`): one `stat` per file.
            let st = match listed {
                Some(st) => Ok(st.clone()),
                None => std::fs::symlink_metadata(p).map(|m| crate::index::stat_of(&m)),
            };
            let frame = match (st, p.strip_prefix(root)) {
                (Ok(st), Ok(rel)) => match rel.to_str() {
                    Some(rel) if !rel.is_empty() => {
                        n += 1;
                        json!({"f": {"p": rel, "s": st.size, "m": st.mtime_ns,
                                     "d": st.dev, "i": st.ino}})
                    }
                    _ => {
                        skipped += 1;
                        json!({"skip": {"p": p.to_string_lossy(), "why": "name is not UTF-8"}})
                    }
                },
                (Err(e), _) => {
                    // Gone or unreadable between listing and stat: not
                    // listed, so nothing below may count as vanished.
                    skipped += 1;
                    complete = false;
                    json!({"skip": {"p": p.to_string_lossy(), "why": e.to_string()}})
                }
                (_, Err(_)) => continue,
            };
            if let Err(e) = emit(out, &frame) {
                failed = Some(e);
                progress.cancel();
                return;
            }
        }
    });
    if let Some(e) = failed {
        return Err(e);
    }
    emit(
        out,
        &json!({"end": {"n": n, "complete": complete, "skipped": skipped}}),
    )
}

fn stat_fs(req: &Value, out: &mut dyn Write) -> io::Result<()> {
    let root = match root_of(req) {
        Ok(r) => r,
        Err(e) => return emit(out, &e),
    };
    let canon = match std::fs::canonicalize(root) {
        Ok(c) => c,
        Err(e) => return emit(out, &error("not_found", format!("{root}: {e}"))),
    };
    let Some(canon_s) = canon.to_str() else {
        return emit(out, &error("bad_request", "root is not UTF-8"));
    };
    let mount = mount_point(&canon);
    emit(
        out,
        &json!({"fs": {
            "root": canon_s,
            "id": fs_id(&canon),
            "mount": mount.as_deref().and_then(Path::to_str),
            "cow": crate::index::is_cow_fs(&canon),
            "rotational": crate::rotational::is_rotational(&canon),
        }}),
    )
}

/// The topmost ancestor of `path` (canonical) on the same device — what
/// GNU `stat -c %m` prints.
#[cfg(unix)]
fn mount_point(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(path).ok()?.dev();
    let mut top = path.to_path_buf();
    for anc in path.ancestors().skip(1) {
        match std::fs::metadata(anc) {
            Ok(m) if m.dev() == dev => top = anc.to_path_buf(),
            _ => break,
        }
    }
    Some(top)
}

#[cfg(not(unix))]
fn mount_point(_path: &Path) -> Option<PathBuf> {
    None
}

/// `statfs.f_fsid` as GNU `stat -f -c %i` prints it: the two 32-bit words,
/// first one high, in lowercase hex. `None` when unknown.
#[cfg(target_os = "linux")]
fn fs_id(path: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `c` is a valid NUL-terminated path; `buf` is written by the
    // kernel on success and only read then.
    let rc = unsafe { libc::statfs(c.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: statfs returned 0, so the struct is initialised.
    let st = unsafe { buf.assume_init() };
    // `fsid_t` is two C ints whose fields libc keeps private: read its
    // bytes as two native-endian 32-bit words.
    const _: () = assert!(std::mem::size_of::<libc::fsid_t>() == 8);
    // SAFETY: fsid_t is a plain 8-byte C struct (checked above).
    let words: [u32; 2] = unsafe { std::mem::transmute(st.f_fsid) };
    let id = (u64::from(words[0]) << 32) | u64::from(words[1]);
    (id != 0).then(|| format!("{id:x}"))
}

#[cfg(not(target_os = "linux"))]
fn fs_id(_path: &Path) -> Option<String> {
    None
}

/// One `hash` result.
struct Hashed {
    full: Option<String>,
    sample: Option<String>,
    err: Option<String>,
}

fn hash(req: &Value, out: &mut dyn Write) -> io::Result<()> {
    let Some(paths) = req.get("paths").and_then(Value::as_array) else {
        return emit(out, &error("bad_request", "no \"paths\" array"));
    };
    let Some(paths) = paths
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<&str>>>()
    else {
        return emit(out, &error("bad_request", "\"paths\" must be strings"));
    };
    let want_sample = req.get("sample").and_then(Value::as_bool).unwrap_or(false);
    let mut results: HashMap<PathBuf, Hashed> = HashMap::new();
    let mut items = Vec::new();
    // Spinning or not, asked once per device.
    let mut spins: HashMap<u64, (bool, usize)> = HashMap::new();
    for p in &paths {
        let path = PathBuf::from(p);
        if results.contains_key(&path) || items.iter().any(|i: &HashItem| i.path == path) {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => {
                let dev = crate::index::stat_of(&m).dev;
                let next = spins.len();
                let (rotational, location) = *spins
                    .entry(dev)
                    .or_insert_with(|| (crate::rotational::is_rotational(&path), next));
                items.push(HashItem {
                    path,
                    size: m.len(),
                    rotational,
                    location,
                });
            }
            Ok(_) => {
                results.insert(path, failure("not a regular file"));
            }
            Err(e) => {
                results.insert(path, failure(&e.to_string()));
            }
        }
    }
    let one = |it: &HashItem| -> (PathBuf, Hashed) {
        let full = crate::dupes::full_hash_with(&it.path, it.size, !it.rotational);
        let sample = if want_sample && full.is_some() {
            crate::dupes::sample_file(&it.path, it.size).map(|s| s.sample)
        } else {
            None
        };
        let h = match full {
            Some(f) => Hashed {
                full: Some(crate::remote::hex(&f)),
                sample: sample.map(|s| crate::remote::hex(&s)),
                err: None,
            },
            None => failure("cannot be read"),
        };
        (it.path.clone(), h)
    };
    for batch in hash_plan(items) {
        let done: Vec<(PathBuf, Hashed)> = match batch.threads {
            None => batch.items.par_iter().map(one).collect(),
            Some(n) => in_order(n, &batch.items, &one),
        };
        results.extend(done);
    }
    for p in &paths {
        let h = results.get(Path::new(p));
        let frame = match h {
            Some(h) => json!({"h": {"p": p, "full": h.full, "sample": h.sample, "err": h.err}}),
            None => json!({"h": {"p": p, "full": null, "sample": null, "err": "not hashed"}}),
        };
        emit(out, &frame)?;
    }
    emit(out, &json!({"end": {"n": paths.len()}}))
}

fn failure(msg: &str) -> Hashed {
    Hashed {
        full: None,
        sample: None,
        err: Some(msg.to_string()),
    }
}
