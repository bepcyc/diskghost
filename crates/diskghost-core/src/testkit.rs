//! Test fixtures shared by the index report, plan and apply tests: a
//! database plus folders that each pretend to be a volume of their own.
//! Tests create and delete files only inside their own temp directory —
//! never anywhere else (owner's hard safety rule).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;

use crate::index::{remote_scan_with, scan_with, Index, Probe, ScanSummary, Volume};
use crate::index_report::{report_with, IndexDupesReport, IndexReportOptions};
use crate::remote::{
    ExecOut, RemoteExec, HASH_SCRIPT, LOCATE_SCRIPT, PRINTF_FORMAT, PROBE_SCRIPT, RESOLVE_SCRIPT,
    STAT_SCRIPT, UPLOAD_SCRIPT,
};
use crate::{Options, Progress};

// ---- fixtures -------------------------------------------------------

/// A fresh directory owned by this test alone. Tests create and delete
/// files only inside it — never anywhere else (owner's hard safety rule).
pub(crate) fn tmpdir(label: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("diskghost-report-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::canonicalize(&d).unwrap()
}

pub(crate) fn write(path: &Path, content: &[u8]) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// Larger than two samples (2 × 16 KiB), so `index add` stores only the
/// sample and the whole-file hash is the report's job.
pub(crate) const BIG: usize = 40 * 1024;

pub(crate) fn content(tag: u8) -> Vec<u8> {
    (0..BIG).map(|i| (i % 251) as u8 ^ tag).collect()
}

/// Same size and same two ends as `content(tag)`, different middle.
pub(crate) fn same_ends(tag: u8) -> Vec<u8> {
    let mut c = content(tag);
    c[BIG / 2] ^= 0xff;
    c
}

/// A test estate: a database plus folders that each pretend to be a
/// volume of their own (`vol-<name>`), online while the folder exists.
pub(crate) struct Estate {
    pub(crate) dir: PathBuf,
    pub(crate) idx: Index,
    pub(crate) cow: bool,
    pub(crate) rotational: bool,
    pub(crate) reads: Mutex<Vec<PathBuf>>,
    /// Remote hosts (`ssh://h`) that do not answer.
    pub(crate) hosts_down: Mutex<HashSet<String>>,
    /// Locations whose filesystem pretends to hand out new inode numbers
    /// per mount, like exFAT/FAT (`Volume::ino_stable == false`).
    pub(crate) ino_unstable: Mutex<HashSet<String>>,
}

impl Estate {
    pub(crate) fn new(label: &str) -> Estate {
        let dir = tmpdir(label);
        let idx = Index::open(&dir.join("db/index.db")).unwrap();
        Estate {
            dir,
            idx,
            cow: false,
            rotational: false,
            reads: Mutex::new(Vec::new()),
            hosts_down: Mutex::new(HashSet::new()),
            ino_unstable: Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn loc(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub(crate) fn uuid(name: &str) -> String {
        format!("vol-{name}")
    }

    pub(crate) fn with_probe<T>(&self, f: impl FnOnce(&Probe) -> T) -> T {
        let base = self.dir.clone();
        let unstable = &self.ino_unstable;
        let volume = move |p: &Path| -> io::Result<Volume> {
            let rel = p.strip_prefix(&base).map_err(io::Error::other)?;
            let top = rel
                .components()
                .next()
                .ok_or_else(|| io::Error::other("not in a location"))?;
            let name = top.as_os_str().to_str().unwrap().to_string();
            Ok(Volume {
                key: Estate::uuid(&name),
                mount_point: base.join(&name),
                ino_stable: !unstable.lock().unwrap().contains(&name),
            })
        };
        // Like the real check: the folder is there *and* holds this
        // very volume (another disk may be mounted at the same place).
        let online = |root: &Path, uuid: &str| {
            root.is_dir()
                && root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| uuid == Estate::uuid(n))
        };
        let cow = self.cow;
        let cow = move |_: &Path| cow;
        let rot = self.rotational;
        let rotational = move |_: &Path| rot;
        let reads = &self.reads;
        let on_read = move |p: &Path| reads.lock().unwrap().push(p.to_path_buf());
        let down = &self.hosts_down;
        let remote_online = move |host: &str, _root: &str| !down.lock().unwrap().contains(host);
        let probe = Probe {
            remote_online: &remote_online,
            volume: &volume,
            online: &online,
            cow: &cow,
            rotational: &rotational,
            on_read: &on_read,
        };
        f(&probe)
    }

    pub(crate) fn add(&self, name: &str) {
        self.add_at(&self.loc(name));
    }

    /// `index add` of any folder inside the estate — a whole fake volume
    /// (`loc(name)`) or a subfolder of one (a scan root below its mount).
    pub(crate) fn add_at(&self, root: &Path) {
        let root = root.to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        let s = self
            .with_probe(|p| {
                scan_with(
                    &self.idx,
                    &root,
                    &Options::default(),
                    &Progress::default(),
                    p,
                )
            })
            .unwrap();
        assert!(s.complete, "{s:?}");
    }

    /// `index add` of a whole fake volume, returning what the scan did.
    pub(crate) fn scan(&self, name: &str) -> ScanSummary {
        let root = self.loc(name);
        std::fs::create_dir_all(&root).unwrap();
        self.reads.lock().unwrap().clear();
        self.with_probe(|p| {
            scan_with(
                &self.idx,
                &root,
                &Options::default(),
                &Progress::default(),
                p,
            )
        })
        .unwrap()
    }

    /// `index add ssh://…` through a fake host.
    pub(crate) fn add_remote(&self, url: &str, host: &FakeHost) -> io::Result<ScanSummary> {
        remote_scan_with(
            &self.idx,
            url,
            host,
            crate::remote::AgentMode::Never,
            None,
            &Progress::default(),
        )
    }

    /// `index add ssh://…` through `x`, with the agent tier offered `bin`.
    pub(crate) fn add_remote_via(
        &self,
        url: &str,
        x: &dyn crate::remote::RemoteExec,
        mode: crate::remote::AgentMode,
        bin: Option<&crate::remote::AgentBinary>,
    ) -> io::Result<ScanSummary> {
        remote_scan_with(&self.idx, url, x, mode, bin, &Progress::default())
    }

    pub(crate) fn report(&self, opts: &IndexReportOptions) -> IndexDupesReport {
        self.reads.lock().unwrap().clear();
        self.with_probe(|p| report_with(&self.idx, opts, &Progress::default(), p))
            .unwrap()
    }

    pub(crate) fn reads(&self) -> BTreeSet<PathBuf> {
        self.reads.lock().unwrap().iter().cloned().collect()
    }

    /// Pretend a location's disk was unplugged: its last known root no
    /// longer exists. Only the index row changes — no file is touched.
    pub(crate) fn unplug(&self, name: &str) {
        let gone = self.dir.join(format!("unplugged-{name}"));
        assert!(!gone.exists());
        let n = self
            .idx
            .conn
            .execute(
                "UPDATE locations SET root = ?2 WHERE uuid = ?1",
                rusqlite::params![Estate::uuid(name), gone.to_str().unwrap()],
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Another disk now sits where `name` was mounted: its last known
    /// root is `other`'s folder (index rows only; no file is touched).
    pub(crate) fn remount_over(&self, name: &str, other: &str) {
        let n = self
            .idx
            .conn
            .execute(
                "UPDATE locations SET root = ?2 WHERE uuid = ?1",
                rusqlite::params![Estate::uuid(name), self.loc(other).to_str().unwrap()],
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// (size, mtime_ns, sample, full) of the row of `name`/`rel`.
    pub(crate) fn row(
        &self,
        name: &str,
        rel: &str,
    ) -> (i64, i64, Option<Vec<u8>>, Option<Vec<u8>>) {
        self.idx
            .conn
            .query_row(
                "SELECT f.size, f.mtime_ns, f.sample, f.full FROM files f \
                 JOIN locations l ON l.id = f.location_id \
                 WHERE l.uuid = ?1 AND f.path = ?2",
                rusqlite::params![Estate::uuid(name), rel],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    pub(crate) fn opts() -> IndexReportOptions {
        IndexReportOptions {
            min_size: 1,
            ..IndexReportOptions::default()
        }
    }
}

impl Drop for Estate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- a fake remote host ---------------------------------------------

/// One file on a [`FakeHost`].
#[derive(Debug, Clone)]
pub(crate) struct FakeFile {
    pub(crate) content: Vec<u8>,
    pub(crate) mtime_ns: u64,
    pub(crate) ino: u64,
    /// The hash tool cannot read it.
    pub(crate) unreadable: bool,
}

/// A remote host in memory: answers exactly the commands `remote` sends
/// (it recognises their scripts) and records every argv. Nothing here
/// touches a real file or network.
pub(crate) struct FakeHost {
    pub(crate) os: String,
    pub(crate) tools: Vec<&'static str>,
    pub(crate) printf: bool,
    /// `gnu`, `bsd` or `none`.
    pub(crate) stat: &'static str,
    pub(crate) home: String,
    pub(crate) fsid: Option<String>,
    pub(crate) fstype: Option<String>,
    /// Mountpoint of the filesystem holding every folder; `None`: neither
    /// `stat -c %m` nor `df -P` tells it.
    pub(crate) mount: Option<String>,
    pub(crate) files: Mutex<BTreeMap<String, FakeFile>>,
    pub(crate) dirs: Mutex<BTreeSet<String>>,
    pub(crate) down: AtomicBool,
    /// Exit code of `find` (non-zero: some folder could not be read).
    pub(crate) find_code: AtomicI32,
    pub(crate) calls: Mutex<Vec<Vec<String>>>,
    next_ino: Mutex<u64>,
    /// `uname -m`; `None`: the probe prints no `arch` line (so the agent
    /// tier is never tried — the pre-M5 behaviour).
    pub(crate) arch: Option<String>,
    /// sshd refuses to run commands (an SFTP-only account): every exec
    /// ends with ssh's exit 255.
    pub(crate) no_shell: AtomicBool,
    /// The upload script fails (disk full, `noexec` home, …).
    pub(crate) upload_fail: AtomicBool,
    /// Files the upload wrote, by absolute path (only ever under
    /// `$HOME/.cache/diskghost/`).
    pub(crate) cache: Mutex<BTreeMap<String, Vec<u8>>>,
    /// What an uploaded agent answers `hello`; `None` = a matching agent
    /// of this build's version.
    pub(crate) hello: Mutex<Option<String>>,
    /// stdin of every `exec_stdin`, in call order.
    pub(crate) inputs: Mutex<Vec<Vec<u8>>>,
}

impl FakeHost {
    /// GNU/Linux with every tool.
    pub(crate) fn linux() -> FakeHost {
        FakeHost {
            os: "Linux".into(),
            tools: vec!["b3sum", "md5sum", "sha1sum", "find", "stat"],
            printf: true,
            stat: "gnu",
            home: "/home/u".into(),
            fsid: Some("a1b2c3d4".into()),
            fstype: Some("ext2/ext3".into()),
            mount: Some("/".into()),
            files: Mutex::new(BTreeMap::new()),
            dirs: Mutex::new(["/".to_string(), "/home".into(), "/home/u".into()].into()),
            down: AtomicBool::new(false),
            find_code: AtomicI32::new(0),
            calls: Mutex::new(Vec::new()),
            next_ino: Mutex::new(1000),
            arch: None,
            no_shell: AtomicBool::new(false),
            upload_fail: AtomicBool::new(false),
            cache: Mutex::new(BTreeMap::new()),
            hello: Mutex::new(None),
            inputs: Mutex::new(Vec::new()),
        }
    }

    /// FreeBSD-like: no `find -printf`, BSD `stat`, no b3sum.
    pub(crate) fn bsd() -> FakeHost {
        FakeHost {
            os: "FreeBSD".into(),
            tools: vec!["md5sum", "sha1sum", "find", "stat"],
            printf: false,
            stat: "bsd",
            fsid: None,
            fstype: None,
            ..FakeHost::linux()
        }
    }

    /// The mountpoint the host reports (`None`: it tells none).
    pub(crate) fn with_mount(mut self, mount: Option<&str>) -> FakeHost {
        self.mount = mount.map(str::to_string);
        self
    }

    /// The `stat` dialect: `gnu`, `bsd` or `none`.
    pub(crate) fn with_stat(mut self, stat: &'static str) -> FakeHost {
        self.stat = stat;
        self
    }

    /// `uname -m` answers `arch`.
    pub(crate) fn with_arch(mut self, arch: &str) -> FakeHost {
        self.arch = Some(arch.to_string());
        self
    }

    /// The agent's directory on this host.
    pub(crate) fn agent_dir(&self) -> String {
        format!("{}/.cache/diskghost", self.home)
    }

    pub(crate) fn with_tools(mut self, tools: &[&'static str]) -> FakeHost {
        self.tools = tools.to_vec();
        self
    }

    /// Put a file at `abs` (its folders appear too); returns its inode.
    pub(crate) fn put(&self, abs: &str, content: &[u8], mtime_ns: u64) -> u64 {
        let ino = {
            let mut n = self.next_ino.lock().unwrap();
            *n += 1;
            *n
        };
        self.put_ino(abs, content, mtime_ns, ino);
        ino
    }

    pub(crate) fn put_ino(&self, abs: &str, content: &[u8], mtime_ns: u64, ino: u64) {
        let mut dirs = self.dirs.lock().unwrap();
        let mut d = Path::new(abs).parent();
        while let Some(p) = d {
            dirs.insert(p.to_str().unwrap().to_string());
            d = p.parent();
        }
        self.files.lock().unwrap().insert(
            abs.to_string(),
            FakeFile {
                content: content.to_vec(),
                mtime_ns,
                ino,
                unreadable: false,
            },
        );
    }

    pub(crate) fn remove(&self, abs: &str) {
        self.files.lock().unwrap().remove(abs);
    }

    pub(crate) fn set_unreadable(&self, abs: &str) {
        self.files.lock().unwrap().get_mut(abs).unwrap().unreadable = true;
    }

    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    /// Calls whose argv starts with `head` (`["sh", "-c", SCRIPT]` …).
    pub(crate) fn count(&self, pred: impl Fn(&[String]) -> bool) -> usize {
        self.calls().iter().filter(|c| pred(c)).count()
    }

    /// The digest `tool` prints for `content` (real BLAKE3 for b3sum; for
    /// md5sum / sha1sum a stand-in of the right length — only equality
    /// matters to the index).
    pub(crate) fn digest(tool: &str, content: &[u8]) -> Vec<u8> {
        match tool {
            "b3sum" => blake3::hash(content).as_bytes().to_vec(),
            "md5sum" => fake_digest(b"md5:", content, 16),
            "sha1sum" => fake_digest(b"sha1:", content, 20),
            _ => panic!("no such tool {tool}"),
        }
    }

    fn resolve(&self, p: &str) -> String {
        let joined = if p == "." {
            self.home.clone()
        } else if p.starts_with('/') {
            p.to_string()
        } else {
            format!("{}/{p}", self.home)
        };
        let mut out = String::new();
        for c in joined.split('/').filter(|c| !c.is_empty() && *c != ".") {
            out.push('/');
            out.push_str(c);
        }
        if out.is_empty() {
            "/".into()
        } else {
            out
        }
    }

    fn under<'f>(
        files: &'f BTreeMap<String, FakeFile>,
        root: &'f str,
    ) -> impl Iterator<Item = (&'f String, &'f FakeFile)> {
        files
            .iter()
            .filter(move |(p, _)| root == "/" || p.starts_with(&format!("{root}/")))
    }
}

fn fake_digest(tag: &[u8], content: &[u8], len: usize) -> Vec<u8> {
    let mut h = blake3::Hasher::new();
    h.update(tag);
    h.update(content);
    h.finalize().as_bytes()[..len].to_vec()
}

fn ok(stdout: Vec<u8>) -> io::Result<ExecOut> {
    Ok(ExecOut {
        stdout,
        stderr: Vec::new(),
        code: 0,
    })
}

fn fail(code: i32, msg: &str) -> io::Result<ExecOut> {
    Ok(ExecOut {
        stdout: Vec::new(),
        stderr: msg.as_bytes().to_vec(),
        code,
    })
}

impl RemoteExec for FakeHost {
    fn exec(&self, argv: &[&str]) -> io::Result<ExecOut> {
        self.calls
            .lock()
            .unwrap()
            .push(argv.iter().map(|s| s.to_string()).collect());
        if self.down.load(Ordering::SeqCst) {
            return fail(255, "ssh: connect to host fake port 22: Connection refused");
        }
        if self.no_shell.load(Ordering::SeqCst) {
            return fail(255, "exec request failed on channel 0");
        }
        let has = |t: &str| self.tools.contains(&t);
        match argv {
            ["sh", "-c", s, "sh", name] if *s == LOCATE_SCRIPT => {
                let path = format!("{}/{name}", self.agent_dir());
                let have = self.cache.lock().unwrap().contains_key(&path);
                ok(format!("{} {path}\n", if have { "have" } else { "need" }).into_bytes())
            }
            ["sh", "-c", s] if *s == PROBE_SCRIPT => {
                let mut o = format!("os {}\n", self.os);
                if let Some(a) = &self.arch {
                    o += &format!("arch {a}\n");
                }
                for t in ["b3sum", "md5sum", "sha1sum", "find", "stat"] {
                    o += &format!("tool {t} {}\n", if has(t) { "yes" } else { "no" });
                }
                o += &format!("printf {}\n", if self.printf { "yes" } else { "no" });
                o += &format!("stat {}\n", self.stat);
                ok(o.into_bytes())
            }
            ["sh", "-c", s, "sh", path, flavor] if *s == RESOLVE_SCRIPT => {
                let root = self.resolve(path);
                if !self.dirs.lock().unwrap().contains(&root) {
                    return fail(3, "sh: cd: can't cd to it");
                }
                let fs = match (*flavor, &self.fsid) {
                    ("gnu", Some(id)) => format!(
                        "{id} {}",
                        self.fstype.clone().unwrap_or_else(|| "ext2/ext3".into())
                    ),
                    ("bsd", _) => "66".to_string(),
                    _ => String::new(),
                };
                // GNU `stat -c %m`; otherwise the last line of `df -P`.
                let mount = match (*flavor, &self.mount) {
                    ("gnu", Some(m)) => format!("mount {m}"),
                    (_, Some(m)) => format!("df /dev/fake0 1000 500 500 50% {m}"),
                    (_, None) => "df ".to_string(),
                };
                ok(format!("{root}\n{fs}\n{mount}\n").into_bytes())
            }
            ["find", root, "-xdev", "-type", "f", "-printf", fmt] => {
                if !self.printf {
                    return fail(1, "find: -printf: unknown primary or operator");
                }
                assert_eq!(*fmt, PRINTF_FORMAT);
                let files = self.files.lock().unwrap();
                let mut o = Vec::new();
                for (p, f) in FakeHost::under(&files, root) {
                    let (s, ns) = (f.mtime_ns / 1_000_000_000, f.mtime_ns % 1_000_000_000);
                    o.extend(
                        format!("{}\t{s}.{ns:09}0\t{}\t{p}", f.content.len(), f.ino).as_bytes(),
                    );
                    o.push(0);
                }
                Ok(ExecOut {
                    stdout: o,
                    stderr: Vec::new(),
                    code: self.find_code.load(Ordering::SeqCst),
                })
            }
            ["find", root, "-xdev", "-type", "f", "-print0"] => {
                let files = self.files.lock().unwrap();
                let mut o = Vec::new();
                for (p, _) in FakeHost::under(&files, root) {
                    o.extend(p.as_bytes());
                    o.push(0);
                }
                Ok(ExecOut {
                    stdout: o,
                    stderr: Vec::new(),
                    code: self.find_code.load(Ordering::SeqCst),
                })
            }
            ["sh", "-c", s, "sh", flavor, paths @ ..] if *s == STAT_SCRIPT => {
                let files = self.files.lock().unwrap();
                let mut o = Vec::new();
                for p in paths {
                    if let (Some(f), true) = (files.get(*p), *flavor == self.stat) {
                        o.extend(
                            format!(
                                "{} {} {}",
                                f.content.len(),
                                f.mtime_ns / 1_000_000_000,
                                f.ino
                            )
                            .as_bytes(),
                        );
                    }
                    o.push(0);
                }
                ok(o)
            }
            ["sh", "-c", s, "sh", tool, paths @ ..] if *s == HASH_SCRIPT => {
                let files = self.files.lock().unwrap();
                let mut o = Vec::new();
                for p in paths {
                    match files.get(*p) {
                        Some(f) if has(tool) && !f.unreadable => o.extend(
                            format!(
                                "{}  -",
                                crate::remote::hex(&FakeHost::digest(tool, &f.content))
                            )
                            .as_bytes(),
                        ),
                        _ => {}
                    }
                    o.push(0);
                }
                ok(o)
            }
            ["test", "-d", root] => {
                if self.dirs.lock().unwrap().contains(*root) {
                    ok(Vec::new())
                } else {
                    fail(1, "")
                }
            }
            _ => fail(127, "fake host: unknown command"),
        }
    }

    fn exec_stdin(&self, argv: &[&str], input: &[u8]) -> io::Result<ExecOut> {
        self.calls
            .lock()
            .unwrap()
            .push(argv.iter().map(|s| s.to_string()).collect());
        self.inputs.lock().unwrap().push(input.to_vec());
        if self.down.load(Ordering::SeqCst) {
            return fail(255, "ssh: connect to host fake port 22: Connection refused");
        }
        if self.no_shell.load(Ordering::SeqCst) {
            return fail(255, "exec request failed on channel 0");
        }
        match argv {
            ["sh", "-c", s, "sh", name, len] if *s == UPLOAD_SCRIPT => {
                if self.upload_fail.load(Ordering::SeqCst) {
                    return fail(1, "cat: write error: No space left on device");
                }
                if len.parse::<usize>().ok() != Some(input.len()) {
                    return fail(4, "short upload");
                }
                let path = format!("{}/{name}", self.agent_dir());
                self.cache
                    .lock()
                    .unwrap()
                    .entry(path.clone())
                    .or_insert_with(|| input.to_vec());
                ok(format!("{path}\n").into_bytes())
            }
            [path, "agent"] if self.cache.lock().unwrap().contains_key(*path) => {
                let hello = self.hello.lock().unwrap().clone().unwrap_or_else(|| {
                    serde_json::json!({
                        "v": 1, "agent": "diskghost", "ver": env!("CARGO_PKG_VERSION"),
                        "os": "linux", "arch": "x86_64", "blake3": true
                    })
                    .to_string()
                });
                let mut o = String::new();
                for line in String::from_utf8_lossy(input).lines() {
                    let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
                    if v["op"] == "hello" {
                        o += &hello;
                    } else {
                        o += r#"{"error":{"code":"unknown_op","msg":"fake agent"}}"#;
                    }
                    o.push('\n');
                }
                ok(o.into_bytes())
            }
            _ => fail(127, "fake host: no such program"),
        }
    }
}
