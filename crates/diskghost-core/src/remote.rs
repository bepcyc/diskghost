//! Remote locations over SSH (`ssh://host/path`), rclone-style: nothing is
//! installed on the remote host. Its own shell tools enumerate and hash the
//! files *there*; only names, sizes, times, inode numbers and digests cross
//! the network — never file content.
//!
//! # Read-only
//!
//! Every command this module sends is a reader: `uname`, `command -v`,
//! `find` (`-printf` / `-print0`, never `-delete` or `-exec`), `stat`, `cd` +
//! `pwd`, `test -d`, and a hash tool fed from `<`. No command writes,
//! renames or removes anything on the remote host; a test records every
//! argv a whole remote scan sends and checks it.
//!
//! # Steps of `index add ssh://host/path`
//!
//! 1. [`probe`] (one ssh call): `uname -s`; which of `b3sum`, `md5sum`,
//!    `sha1sum`, `find`, `stat` exist (`command -v`); whether `find` takes
//!    `-printf` (a tiny test run — BSD and BusyBox `find` lack it); which
//!    `stat` dialect answers (GNU `-c` or BSD `-f`). A host with none of the
//!    three hash tools is unusable, and the error names what was probed.
//! 2. [`resolve_root`] (one call): `cd` + `pwd -P` canonicalises the root;
//!    GNU `stat -f -c '%i %T'` gives the filesystem id and type (BSD: the
//!    device number of the root, type unknown); GNU `stat -c %m` gives the
//!    filesystem's mountpoint (BSD, or when that fails: the last field of
//!    `df -P .`).
//!
//! # The location
//!
//! Like a local volume, a remote location is a *filesystem*, not a folder:
//! uuid `ssh://host/<fs id>`, `root` = the remote mountpoint, file paths
//! relative to it, and the folder given recorded as a scan root relative to
//! the mountpoint (`''` = the mountpoint itself). Two folders of one remote
//! filesystem fill one location, and a rescan of one folder only drops
//! vanished rows below that folder. When the host tells no fs id, or no
//! mountpoint that is a path prefix of the folder, the fs id is not used:
//! the uuid is `ssh://host:<folder>`, `root` = the folder, scan root `''`.
//! 3. [`enum_remote`]: GNU `find ROOT -xdev -type f -printf
//!    '%s\t%T@\t%i\t%p\0'` — one call for the whole tree. Without `-printf`:
//!    `find ROOT -xdev -type f -print0`, then `stat` in batches of
//!    [`BATCH`] paths. Every file is a size-collision candidate until its
//!    size is known, so the fallback stats *all* files: ⌈files / 200⌉ extra
//!    ssh calls, and mtimes in whole seconds. `-xdev`: a location is one
//!    filesystem.
//! 4. [`hash_remote`]: the files whose size collides with another indexed
//!    file are hashed whole on the remote host, in batches of at most
//!    [`BATCH`] paths and [`BATCH_BYTES`] of argument text, with the first
//!    available tool of `b3sum` → `md5sum` → `sha1sum`. One output record per
//!    input path, empty when the file could not be read: results align by
//!    position, never by parsing names back.
//!
//! # Digests of different algorithms
//!
//! Local files are hashed with BLAKE3; a remote host without `b3sum` gives
//! MD5 or SHA-1 digests. Digests of different algorithms cannot be
//! compared: a BLAKE3 row and an MD5 row of the same size are **never**
//! reported as duplicates — the report lists them as `unverifiable`. Groups
//! key on (size, algorithm, digest). Install `b3sum` on the remote host to
//! compare its files with local ones.

use std::io;
use std::process::{Command, Stdio};

/// What one remote command printed and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOut {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Exit code; `-1` when killed by a signal. ssh itself exits 255 when
    /// it cannot reach the host.
    pub code: i32,
}

/// Runs one command on a remote host — the seam tests replace.
pub trait RemoteExec: Sync {
    /// Run `argv` (program and arguments, no shell syntax: each element is
    /// one word) on the host.
    fn exec(&self, argv: &[&str]) -> io::Result<ExecOut>;
}

/// The real thing: `ssh <extra> -- <host> <argv, shell-quoted>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshExec {
    /// `host` or `user@host` (anything `ssh` resolves, e.g. a `Host` alias).
    pub host: String,
    /// The ssh program: `ssh`, or `$DISKGHOST_SSH` when set.
    pub ssh: String,
    pub extra: Vec<String>,
}

impl SshExec {
    pub fn new(host: &str) -> SshExec {
        SshExec {
            host: host.to_string(),
            ssh: std::env::var("DISKGHOST_SSH").unwrap_or_else(|_| "ssh".to_string()),
            extra: ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    /// The arguments given to the ssh program for `argv`.
    pub fn args(&self, argv: &[&str]) -> Vec<String> {
        let command: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
        let mut out = self.extra.clone();
        out.push("--".into());
        out.push(self.host.clone());
        out.push(command.join(" "));
        out
    }
}

impl RemoteExec for SshExec {
    fn exec(&self, argv: &[&str]) -> io::Result<ExecOut> {
        let out = Command::new(&self.ssh)
            .args(self.args(argv))
            .stdin(Stdio::null())
            .output()?;
        Ok(ExecOut {
            stdout: out.stdout,
            stderr: out.stderr,
            code: out.status.code().unwrap_or(-1),
        })
    }
}

/// POSIX shell single-quoting: `'…'`, with each `'` as `'\''`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Split `ssh://host/path` into (host, path). The path is everything after
/// the host's `/`: `ssh://h//storage` is `/storage`, `ssh://h/data` is
/// `data` relative to the remote home, `ssh://h` and `ssh://h/` are the
/// home itself (`.`).
pub fn parse_ssh_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("ssh://")
        .ok_or_else(|| format!("not an ssh:// URL: {url:?}"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    validate_remote_host(host).map_err(|e| format!("{url:?}: {e}"))?;
    let path = if path.is_empty() { "." } else { path };
    Ok((host.to_string(), path.to_string()))
}

/// A host name ssh can only read as a host: letters, digits, `.`, `-`, `_`
/// and one `user@`; never starting with `-` (it would be an ssh option).
pub fn validate_remote_host(host: &str) -> Result<(), String> {
    let word = |w: &str| {
        !w.is_empty()
            && !w.starts_with('-')
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    let ok = match host.split_once('@') {
        Some((user, name)) => word(user) && word(name),
        None => word(host),
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "bad remote host {host:?}: use host or user@host (letters, digits, \
             '.', '-', '_'; no port — set one in ~/.ssh/config)"
        ))
    }
}

/// Whether a location's `host` column names a remote host (`ssh://…`).
pub fn is_remote_host(host: &str) -> bool {
    host.starts_with("ssh://")
}

/// A digest algorithm a row's `full` hash was made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HashAlgo {
    Blake3,
    Md5,
    Sha1,
}

impl HashAlgo {
    /// The chain tried on a remote host, best first.
    pub const CHAIN: [HashAlgo; 3] = [HashAlgo::Blake3, HashAlgo::Md5, HashAlgo::Sha1];

    pub fn as_str(self) -> &'static str {
        match self {
            HashAlgo::Blake3 => "blake3",
            HashAlgo::Md5 => "md5",
            HashAlgo::Sha1 => "sha1",
        }
    }

    pub fn parse(s: &str) -> Option<HashAlgo> {
        match s {
            "blake3" => Some(HashAlgo::Blake3),
            "md5" => Some(HashAlgo::Md5),
            "sha1" => Some(HashAlgo::Sha1),
            _ => None,
        }
    }

    /// The remote tool computing it.
    pub fn tool(self) -> &'static str {
        match self {
            HashAlgo::Blake3 => "b3sum",
            HashAlgo::Md5 => "md5sum",
            HashAlgo::Sha1 => "sha1sum",
        }
    }

    /// Digest length in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            HashAlgo::Blake3 => 32,
            HashAlgo::Md5 => 16,
            HashAlgo::Sha1 => 20,
        }
    }
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `stat` dialect a host answers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatFlavor {
    /// GNU coreutils / BusyBox: `stat -c FORMAT`, `stat -f` = filesystem.
    Gnu,
    /// BSD / macOS: `stat -f FORMAT`.
    Bsd,
    None,
}

/// What [`probe`] found on a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// `uname -s`.
    pub os: String,
    /// The probed tools that exist, in probe order.
    pub tools: Vec<String>,
    /// `find` takes `-printf` (GNU find).
    pub find_printf: bool,
    pub stat: StatFlavor,
    /// The first of b3sum → md5sum → sha1sum present.
    pub hash: HashAlgo,
}

/// Tools [`probe`] looks for, in order.
pub const PROBED_TOOLS: [&str; 5] = ["b3sum", "md5sum", "sha1sum", "find", "stat"];

/// The probe, as one shell script (one ssh call).
pub const PROBE_SCRIPT: &str = r#"echo "os $(uname -s 2>/dev/null)"
for t in b3sum md5sum sha1sum find stat; do
  if command -v "$t" >/dev/null 2>&1; then echo "tool $t yes"; else echo "tool $t no"; fi
done
if find / -maxdepth 0 -printf '' >/dev/null 2>&1; then echo "printf yes"; else echo "printf no"; fi
if stat -c %s / >/dev/null 2>&1; then echo "stat gnu"; elif stat -f %z / >/dev/null 2>&1; then echo "stat bsd"; else echo "stat none"; fi
"#;

/// `$1` = root as given, `$2` = stat flavor. Prints the canonical root,
/// then `<fs id> <fs type>` (GNU), `<device>` (BSD) or an empty line, then
/// `mount <mountpoint>` (GNU `stat -c %m`) or `df <last line of df -P .>`.
pub const RESOLVE_SCRIPT: &str = r#"cd -- "$1" || exit 3
pwd -P
case "$2" in
  gnu) stat -f -c '%i %T' . 2>/dev/null || echo ;;
  bsd) stat -f '%d' . 2>/dev/null || echo ;;
  *) echo ;;
esac
mp=
if [ "$2" = gnu ]; then mp=$(stat -c %m . 2>/dev/null) || mp=; fi
case "$mp" in
  /*) echo "mount $mp" ;;
  *) echo "df $(df -P . 2>/dev/null | tail -n 1)" ;;
esac
"#;

/// `$1` = stat flavor, then paths: one NUL-terminated `size mtime_s ino`
/// record per path, empty when it cannot be stat'ed.
pub const STAT_SCRIPT: &str = r#"fl=$1; shift
for f do
  if [ "$fl" = gnu ]; then s=$(stat -c '%s %Y %i' -- "$f" 2>/dev/null) || s=
  else s=$(stat -f '%z %m %i' -- "$f" 2>/dev/null) || s=; fi
  printf '%s\0' "$s"
done
"#;

/// `$1` = hash tool, then paths: one NUL-terminated `<hex>  -` record per
/// path (the tool reads the file from stdin), empty when it cannot be read.
pub const HASH_SCRIPT: &str = r#"t=$1; shift
for f do
  h=$("$t" < "$f" 2>/dev/null) || h=
  printf '%s\0' "$h"
done
"#;

/// The `find -printf` format: size, mtime (seconds.fraction), inode, path.
pub const PRINTF_FORMAT: &str = "%s\\t%T@\\t%i\\t%p\\0";

/// Paths per stat or hash call at most.
pub const BATCH: usize = 200;

/// Bytes of (quoted) path text per call at most: sshd hands the command to
/// the remote shell as one string, and Linux caps one argument at 128 KiB.
pub const BATCH_BYTES: usize = 96 * 1024;

fn unreachable_err(x: &ExecOut) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        format!(
            "host unreachable: {}",
            String::from_utf8_lossy(&x.stderr).trim()
        ),
    )
}

/// Run `argv`; ssh's own failure (exit 255) is an error, a command's
/// non-zero exit is not.
pub(crate) fn run(x: &dyn RemoteExec, argv: &[&str]) -> io::Result<ExecOut> {
    let out = x.exec(argv)?;
    if out.code == 255 {
        return Err(unreachable_err(&out));
    }
    Ok(out)
}

/// Probe what a host offers, in one call. `Err(NotConnected)` when it
/// cannot be reached; `Err(Unsupported)` when it has no hash tool.
pub fn probe(x: &dyn RemoteExec) -> io::Result<Capabilities> {
    let out = run(x, &["sh", "-c", PROBE_SCRIPT])?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut os = String::new();
    let mut tools = Vec::new();
    let mut find_printf = false;
    let mut stat = StatFlavor::None;
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w.as_slice() {
            ["os", rest @ ..] => os = rest.join(" "),
            ["tool", t, "yes"] if PROBED_TOOLS.contains(t) => tools.push(t.to_string()),
            ["printf", v] => find_printf = *v == "yes",
            ["stat", "gnu"] => stat = StatFlavor::Gnu,
            ["stat", "bsd"] => stat = StatFlavor::Bsd,
            _ => {}
        }
    }
    let hash = HashAlgo::CHAIN
        .into_iter()
        .find(|a| tools.iter().any(|t| t == a.tool()));
    let Some(hash) = hash else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "remote host unusable: none of b3sum, md5sum, sha1sum found \
                 (probed: uname -s, command -v {}; found: [{}], os {os:?}{})",
                PROBED_TOOLS.join(" "),
                tools.join(", "),
                if out.code == 0 {
                    String::new()
                } else {
                    format!(
                        "; probe exited {}: {}",
                        out.code,
                        String::from_utf8_lossy(&out.stderr).trim()
                    )
                }
            ),
        ));
    };
    if !tools.iter().any(|t| t == "find") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "remote host unusable: no `find` (probed: {})",
                PROBED_TOOLS.join(" ")
            ),
        ));
    }
    Ok(Capabilities {
        os,
        tools,
        find_printf,
        stat,
        hash,
    })
}

/// A remote root resolved: canonical path, filesystem id and type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRoot {
    pub root: String,
    /// GNU `stat -f %i` (hex), or `dev<N>` from BSD `stat -f %d`; `None`
    /// when unknown (or 0).
    pub fsid: Option<String>,
    /// GNU `stat -f %T` (`ext2/ext3`, `btrfs`, …).
    pub fstype: Option<String>,
    /// The mountpoint of the root's filesystem; `None` when the host told
    /// none, or one that is not a path prefix of `root`.
    pub mount: Option<String>,
}

pub fn resolve_root(x: &dyn RemoteExec, caps: &Capabilities, path: &str) -> io::Result<RemoteRoot> {
    let flavor = match caps.stat {
        StatFlavor::Gnu => "gnu",
        StatFlavor::Bsd => "bsd",
        StatFlavor::None => "none",
    };
    let out = run(x, &["sh", "-c", RESOLVE_SCRIPT, "sh", path, flavor])?;
    let text = String::from_utf8(out.stdout)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "remote path is not UTF-8"))?;
    let mut lines = text.lines();
    let root = lines.next().unwrap_or("").to_string();
    if out.code != 0 || !root.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "remote folder {path:?} not found: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    let fs: Vec<&str> = lines.next().unwrap_or("").splitn(2, ' ').collect();
    let id = fs.first().copied().filter(|id| {
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()) && id.bytes().any(|b| b != b'0')
    });
    let (fsid, fstype) = match caps.stat {
        StatFlavor::Gnu => (
            id.map(str::to_string),
            fs.get(1)
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
        ),
        StatFlavor::Bsd => (id.map(|d| format!("dev{d}")), None),
        StatFlavor::None => (None, None),
    };
    let mount = lines
        .next()
        .and_then(|l| match l.split_once(' ') {
            Some(("mount", m)) => Some(m.to_string()),
            // `df -P`: the mountpoint is the last field (a mountpoint with
            // a space fails the prefix check below).
            Some(("df", d)) => d.split_whitespace().last().map(str::to_string),
            _ => None,
        })
        .filter(|m| is_under(&root, m));
    Ok(RemoteRoot {
        root,
        fsid,
        fstype,
        mount,
    })
}

/// Whether `path` is `mount` or below it (whole path components).
fn is_under(path: &str, mount: &str) -> bool {
    mount.starts_with('/')
        && (mount == "/" || path == mount || path.starts_with(&format!("{mount}/")))
}

/// `rel` (relative, `''` = the base itself) joined to the absolute `base`.
pub fn join_remote(base: &str, rel: &str) -> String {
    match (base, rel) {
        (b, "") => b.to_string(),
        ("/", r) => format!("/{r}"),
        (b, r) => format!("{b}/{r}"),
    }
}

/// One remote file, path relative to the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub path: String,
    pub size: u64,
    pub mtime_ns: u64,
    pub ino: u64,
}

/// Every regular file under a root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub files: Vec<RemoteFile>,
    /// Entries that could not be listed or stat'ed, or with a name that is
    /// not UTF-8.
    pub skipped: u64,
    /// `find` finished without error: rows not listed are really gone.
    pub complete: bool,
}

/// Parse `find -printf '%s\t%T@\t%i\t%p\0'` output of a scan of `root`.
/// Returns the files and how many records were unusable.
pub fn parse_printf(out: &[u8], root: &str) -> (Vec<RemoteFile>, u64) {
    let mut files = Vec::new();
    let mut skipped = 0;
    for rec in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let mut f = rec.splitn(4, |&b| b == b'\t');
        let (Some(size), Some(mtime), Some(ino), Some(path)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            skipped += 1;
            continue;
        };
        let num = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u64>().ok();
        let parsed = (
            num(size),
            std::str::from_utf8(mtime).ok().and_then(parse_mtime),
            num(ino),
            std::str::from_utf8(path)
                .ok()
                .and_then(|p| relative(p, root)),
        );
        match parsed {
            (Some(size), Some(mtime_ns), Some(ino), Some(path)) => files.push(RemoteFile {
                path,
                size,
                mtime_ns,
                ino,
            }),
            _ => skipped += 1,
        }
    }
    (files, skipped)
}

/// `abs` relative to `root`, when strictly below it.
fn relative(abs: &str, root: &str) -> Option<String> {
    let rest = if root == "/" {
        abs.strip_prefix('/')?
    } else {
        abs.strip_prefix(root)?.strip_prefix('/')?
    };
    (!rest.is_empty()).then(|| rest.to_string())
}

/// `%T@` (`seconds[.fraction]`) as nanoseconds, exactly (no float).
pub fn parse_mtime(s: &str) -> Option<u64> {
    let (secs, frac) = s.split_once('.').unwrap_or((s, ""));
    if secs.is_empty() || !secs.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut ns = String::from(&frac[..frac.len().min(9)]);
    while ns.len() < 9 {
        ns.push('0');
    }
    secs.parse::<u64>()
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(ns.parse::<u64>().ok()?)
}

/// List every regular file under `root` (canonical), staying on its
/// filesystem.
pub fn enum_remote(x: &dyn RemoteExec, caps: &Capabilities, root: &str) -> io::Result<Listing> {
    if caps.find_printf {
        let out = run(
            x,
            &[
                "find",
                root,
                "-xdev",
                "-type",
                "f",
                "-printf",
                PRINTF_FORMAT,
            ],
        )?;
        let (files, skipped) = parse_printf(&out.stdout, root);
        return Ok(Listing {
            files,
            skipped,
            complete: out.code == 0,
        });
    }
    // No -printf: names first, then sizes, times and inodes in batches.
    let out = run(x, &["find", root, "-xdev", "-type", "f", "-print0"])?;
    let mut complete = out.code == 0;
    let mut skipped = 0;
    let mut names = Vec::new();
    for rec in out.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        match std::str::from_utf8(rec)
            .ok()
            .and_then(|p| relative(p, root).map(|r| (p.to_string(), r)))
        {
            Some(n) => names.push(n),
            None => skipped += 1,
        }
    }
    let flavor = match caps.stat {
        StatFlavor::Gnu => "gnu",
        StatFlavor::Bsd => "bsd",
        StatFlavor::None => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote host has neither `find -printf` nor a usable `stat`",
            ))
        }
    };
    let abs: Vec<&str> = names.iter().map(|(a, _)| a.as_str()).collect();
    let mut files = Vec::new();
    for range in batches(&abs) {
        let mut argv = vec!["sh", "-c", STAT_SCRIPT, "sh", flavor];
        argv.extend(&abs[range.clone()]);
        let out = run(x, &argv)?;
        let recs: Vec<&[u8]> = records(&out.stdout);
        if recs.len() != range.len() {
            skipped += range.len() as u64;
            complete = false;
            continue;
        }
        for (rec, (_, rel)) in recs.iter().zip(&names[range]) {
            let f: Vec<u64> = std::str::from_utf8(rec)
                .unwrap_or("")
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect();
            match f.as_slice() {
                [size, secs, ino] => files.push(RemoteFile {
                    path: rel.clone(),
                    size: *size,
                    mtime_ns: secs.saturating_mul(1_000_000_000),
                    ino: *ino,
                }),
                _ => {
                    // Gone between find and stat, or unreadable: not
                    // listed, so nothing may count as vanished.
                    skipped += 1;
                    complete = false;
                }
            }
        }
    }
    Ok(Listing {
        files,
        skipped,
        complete,
    })
}

/// NUL-terminated records, one per input of a batch script.
fn records(out: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    // The text after the last NUL (empty when well-formed).
    if v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

/// Split `paths` into batches of at most [`BATCH`] paths and
/// [`BATCH_BYTES`] of quoted text (a longer single path is a batch alone).
pub fn batches<S: AsRef<str>>(paths: &[S]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut bytes) = (0, 0);
    for (i, p) in paths.iter().enumerate() {
        let n = shell_quote(p.as_ref()).len() + 1;
        if i > start && (i - start == BATCH || bytes + n > BATCH_BYTES) {
            out.push(start..i);
            start = i;
            bytes = 0;
        }
        bytes += n;
    }
    if start < paths.len() {
        out.push(start..paths.len());
    }
    out
}

/// Hash the files `paths` (absolute) on the remote host with `caps.hash`:
/// one digest per path, `None` when it could not be read.
pub fn hash_remote(
    x: &dyn RemoteExec,
    caps: &Capabilities,
    paths: &[String],
) -> io::Result<Vec<Option<Vec<u8>>>> {
    let tool = caps.hash.tool();
    let mut out = Vec::with_capacity(paths.len());
    for range in batches(paths) {
        let mut argv = vec!["sh", "-c", HASH_SCRIPT, "sh", tool];
        argv.extend(paths[range.clone()].iter().map(String::as_str));
        let res = run(x, &argv)?;
        let recs = records(&res.stdout);
        if recs.len() != range.len() {
            // Misaligned output: trust none of it.
            out.extend(range.map(|_| None));
            continue;
        }
        out.extend(recs.iter().map(|r| parse_digest(r, caps.hash)));
    }
    Ok(out)
}

/// The first field of a `<hex>  -` record, as bytes of the right length.
fn parse_digest(rec: &[u8], algo: HashAlgo) -> Option<Vec<u8>> {
    let hexs = std::str::from_utf8(rec).ok()?.split_whitespace().next()?;
    let hexs = hexs.strip_prefix('\\').unwrap_or(hexs);
    if hexs.len() != algo.digest_len() * 2 {
        return None;
    }
    (0..hexs.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hexs[i..i + 2], 16).ok())
        .collect()
}

/// Whether `root` is reachable now: the host answers and the folder exists.
/// Never panics; any failure is "offline".
pub fn remote_online(x: &dyn RemoteExec, root: &str) -> bool {
    run(x, &["test", "-d", root]).is_ok_and(|o| o.code == 0)
}

/// [`remote_online`] over real ssh, for a `host` column (`ssh://h`).
pub fn ssh_online(host: &str, root: &str) -> bool {
    match host.strip_prefix("ssh://") {
        Some(h) if validate_remote_host(h).is_ok() => remote_online(&SshExec::new(h), root),
        _ => false,
    }
}
