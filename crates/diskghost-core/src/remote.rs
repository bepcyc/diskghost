//! Remote locations over SSH (`ssh://host/path`). The files are enumerated
//! and hashed *on the host*; only names, sizes, times, inode numbers and
//! digests cross the network — never file content. Two tiers:
//!
//! 1. **Agent** (mutagen / `borg serve` style): on a Linux x86_64 host this
//!    program uploads itself once to `~/.cache/diskghost/` under a
//!    content-addressed name ([`AgentBinary::name`]), checks its `hello`,
//!    and speaks the JSON-lines protocol of [`crate::agent`] with it over
//!    ssh stdio ([`AgentExec`]). BLAKE3 everywhere, like local files.
//! 2. **Shell** (rclone style, nothing installed): the host's own `find`,
//!    `stat` and hash tools, as described below.
//!
//! [`ensure_agent`] picks the tier: any failure of the agent (unsupported
//! os/arch, upload refused, a `hello` that does not match — e.g. a
//! dynamically linked build on a host without its libraries) falls back to
//! the shell tier; `index add --agent never` forces it. The tier used is
//! recorded per location (`locations.transport`). A host that answers ssh
//! but runs no command at all (an SFTP-only account) fails with
//! [`shell_unavailable`]: diskghost will not read every file over the
//! network instead.
//!
//! # Read-only
//!
//! The protocol has no operation that writes or removes anything. The only
//! remote write is the agent upload itself ([`UPLOAD_SCRIPT`]), confined to
//! `$HOME/.cache/diskghost/` and never replacing a file. Every shell-tier
//! command is a reader: `uname`, `command -v`,
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

    /// [`RemoteExec::exec`] with `input` written to the command's stdin
    /// (the agent upload, and the agent's request frames).
    fn exec_stdin(&self, argv: &[&str], input: &[u8]) -> io::Result<ExecOut> {
        let _ = (argv, input);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this transport cannot send input to a remote command",
        ))
    }

    /// [`RemoteExec::exec`]/[`RemoteExec::exec_stdin`] reporting `on_chunk`
    /// with stdout pieces as they arrive — live progress for a listing
    /// that can run for hours. The default reports one chunk once the
    /// command ends (a transport that cannot stream).
    fn exec_live(
        &self,
        argv: &[&str],
        stdin: Option<&[u8]>,
        on_chunk: &mut dyn FnMut(&[u8]),
    ) -> io::Result<ExecOut> {
        let out = match stdin {
            Some(input) => self.exec_stdin(argv, input)?,
            None => self.exec(argv)?,
        };
        on_chunk(&out.stdout);
        Ok(out)
    }
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

    fn exec_stdin(&self, argv: &[&str], input: &[u8]) -> io::Result<ExecOut> {
        use std::io::Write;
        let mut child = Command::new(&self.ssh)
            .args(self.args(argv))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        // Written from another thread while this one drains stdout and
        // stderr: a big input and a big answer cannot deadlock each other.
        let out = std::thread::scope(|s| {
            let writer = s.spawn(move || {
                // A remote command that stops reading early is not an
                // error here: its exit code tells.
                let _ = stdin.write_all(input);
            });
            let out = child.wait_with_output();
            let _ = writer.join();
            out
        })?;
        Ok(ExecOut {
            stdout: out.stdout,
            stderr: out.stderr,
            code: out.status.code().unwrap_or(-1),
        })
    }

    fn exec_live(
        &self,
        argv: &[&str],
        input: Option<&[u8]>,
        on_chunk: &mut dyn FnMut(&[u8]),
    ) -> io::Result<ExecOut> {
        use std::io::{Read, Write};
        let mut child = Command::new(&self.ssh)
            .args(self.args(argv))
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let (res, stdout_bytes, stderr_bytes) = std::thread::scope(|s| {
            let err = s.spawn(move || {
                let mut v = Vec::new();
                let _ = stderr.read_to_end(&mut v);
                v
            });
            let wr = stdin.take().map(|mut w| {
                let input = input.unwrap_or_default();
                s.spawn(move || {
                    // A remote command that stops reading early is not
                    // an error here: its exit code tells.
                    let _ = w.write_all(input);
                })
            });
            let mut out = Vec::new();
            let mut buf = [0u8; 64 * 1024];
            let res = loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        out.extend_from_slice(&buf[..n]);
                        on_chunk(&buf[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => break Err(e),
                }
            };
            if res.is_err() {
                let _ = child.kill();
            }
            if let Some(h) = wr {
                let _ = h.join();
            }
            let err = err.join().unwrap_or_default();
            (res, out, err)
        });
        res?;
        let status = child.wait()?;
        Ok(ExecOut {
            stdout: stdout_bytes,
            stderr: stderr_bytes,
            code: status.code().unwrap_or(-1),
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
echo "arch $(uname -m 2>/dev/null)"
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

/// What sshd says when it reached the host but will not run a command
/// (`ForceCommand internal-sftp`, a restricted shell, `no-pty`+`command=`).
const EXEC_REFUSED: [&str; 6] = [
    "exec request failed",
    "shell request failed",
    "sftp connections only",
    "only sftp",
    "not allowed to execute",
    "command not allowed",
];

/// ssh's exit 255: either the host was not reached (DNS, timeout, refused,
/// no key — `NotConnected`), or it was and refuses to run commands
/// (`PermissionDenied`, the message being ssh's own words; see
/// [`shell_unavailable`]).
fn ssh_failure(x: &ExecOut) -> io::Error {
    let err = String::from_utf8_lossy(&x.stderr);
    let lower = err.to_lowercase();
    if EXEC_REFUSED.iter().any(|p| lower.contains(p)) {
        io::Error::new(io::ErrorKind::PermissionDenied, err.trim().to_string())
    } else {
        unreachable_err(x)
    }
}

/// The error for a host that answers ssh but runs no command: neither tier
/// can work there, and the only way left — reading every file over SFTP —
/// is what diskghost refuses to do silently.
pub fn shell_unavailable(host: &str, detail: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "remote shell unavailable on {host} — files would have to be read over the \
             network; upload an agent or enable shell (ssh: {detail})"
        ),
    )
}

/// Run `argv`; ssh's own failure (exit 255) is an error, a command's
/// non-zero exit is not.
pub(crate) fn run(x: &dyn RemoteExec, argv: &[&str]) -> io::Result<ExecOut> {
    let out = x.exec(argv)?;
    if out.code == 255 {
        return Err(ssh_failure(&out));
    }
    Ok(out)
}

/// [`run`] reporting stdout pieces to `on_chunk` as they arrive.
fn run_live(
    x: &dyn RemoteExec,
    argv: &[&str],
    on_chunk: &mut dyn FnMut(&[u8]),
) -> io::Result<ExecOut> {
    let out = x.exec_live(argv, None, on_chunk)?;
    if out.code == 255 {
        return Err(ssh_failure(&out));
    }
    Ok(out)
}

/// Probe what a host offers, in one call. `Err(NotConnected)` when it
/// cannot be reached; `Err(Unsupported)` when it has no hash tool.
pub fn probe(x: &dyn RemoteExec) -> io::Result<Capabilities> {
    probe_host(x)?.1
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

/// Counts `find -printf` records as they stream in: files and their bytes
/// so far, for live progress. A record split across chunks is counted
/// when its `\0` terminator arrives.
#[derive(Default)]
struct PrintfProgress {
    pending: Vec<u8>,
    files: u64,
    bytes: u64,
}

impl PrintfProgress {
    fn feed(&mut self, chunk: &[u8], progress: &crate::Progress) {
        self.pending.extend_from_slice(chunk);
        let mut done = 0;
        for (i, &b) in self.pending.iter().enumerate() {
            if b == 0 {
                done = i + 1;
            }
        }
        for rec in self.pending[..done]
            .split(|&b| b == 0)
            .filter(|r| !r.is_empty())
        {
            // `%s\t…`: the size is the first field of a well-formed record.
            let size = rec
                .split(|&b| b == b'\t')
                .next()
                .and_then(|w| std::str::from_utf8(w).ok())
                .and_then(|w| w.parse::<u64>().ok());
            self.files += 1;
            self.bytes += size.unwrap_or(0);
        }
        self.pending.drain(..done);
        progress.set_counts(self.files, self.bytes);
    }
}

/// List every regular file under `root` (canonical), staying on its
/// filesystem.
pub fn enum_remote(x: &dyn RemoteExec, caps: &Capabilities, root: &str) -> io::Result<Listing> {
    enum_remote_live(x, caps, root, &crate::Progress::default())
}

/// [`enum_remote`] reporting `progress` as the listing arrives — record
/// by record — instead of storing the totals once at the end.
pub fn enum_remote_live(
    x: &dyn RemoteExec,
    caps: &Capabilities,
    root: &str,
    progress: &crate::Progress,
) -> io::Result<Listing> {
    if caps.find_printf {
        let mut seen = PrintfProgress::default();
        let out = run_live(
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
            &mut |chunk| seen.feed(chunk, progress),
        )?;
        let (files, skipped) = parse_printf(&out.stdout, root);
        progress.set_counts(files.len() as u64, files.iter().map(|f| f.size).sum());
        return Ok(Listing {
            files,
            skipped,
            complete: out.code == 0,
        });
    }
    // No -printf: names first, then sizes, times and inodes in batches.
    // Every `\0` ending a name counts one more file found.
    let mut names_seen = 0u64;
    let out = run_live(
        x,
        &["find", root, "-xdev", "-type", "f", "-print0"],
        &mut |chunk| {
            names_seen += chunk.iter().filter(|&&b| b == 0).count() as u64;
            progress.set_counts(names_seen, 0);
        },
    )?;
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
        progress.set_counts(names.len() as u64, files.iter().map(|f| f.size).sum());
    }
    progress.set_counts(files.len() as u64, files.iter().map(|f| f.size).sum());
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

// ---- the agent tier (M5) ---------------------------------------------

/// What [`probe_host`] tells about a host before any tool check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostInfo {
    /// `uname -s` (`Linux`).
    pub os: String,
    /// `uname -m` (`x86_64`); empty when not told.
    pub arch: String,
}

/// Probe a host: its os/arch, and the shell-tier capabilities (`Err` when
/// it has no usable hash tool or `find` — the agent tier may still work).
/// The outer `Err` is a host that cannot be reached or runs no command.
pub fn probe_host(x: &dyn RemoteExec) -> io::Result<(HostInfo, io::Result<Capabilities>)> {
    let out = run(x, &["sh", "-c", PROBE_SCRIPT])?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut info = HostInfo::default();
    let mut told_os = false;
    let mut tools = Vec::new();
    let mut find_printf = false;
    let mut stat = StatFlavor::None;
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w.as_slice() {
            ["os", rest @ ..] => {
                told_os = true;
                info.os = rest.join(" ");
            }
            ["arch", rest @ ..] => info.arch = rest.join(" "),
            ["tool", t, "yes"] if PROBED_TOOLS.contains(t) => tools.push(t.to_string()),
            ["printf", v] => find_printf = *v == "yes",
            ["stat", "gnu"] => stat = StatFlavor::Gnu,
            ["stat", "bsd"] => stat = StatFlavor::Bsd,
            _ => {}
        }
    }
    if !told_os && out.code != 0 {
        // Connected, but the probe never ran: a host that runs no command.
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the probe did not run (exit {}): {}",
                out.code,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    let os = info.os.clone();
    let hash = HashAlgo::CHAIN
        .into_iter()
        .find(|a| tools.iter().any(|t| t == a.tool()));
    let caps = match hash {
        None => Err(io::Error::new(
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
        )),
        Some(_) if !tools.iter().any(|t| t == "find") => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "remote host unusable: no `find` (probed: {})",
                PROBED_TOOLS.join(" ")
            ),
        )),
        Some(hash) => Ok(Capabilities {
            os,
            tools,
            find_printf,
            stat,
            hash,
        }),
    };
    Ok((info, caps))
}

/// Whether to try the agent tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Upload/reuse the agent where the host can run it, else the shell tier.
    Auto,
    /// Always the shell tier (parity tests, hosts that forbid binaries).
    Never,
}

/// How a remote location's files were listed and hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// `diskghost agent` at `agent_path` on the host.
    Agent { agent_path: std::path::PathBuf },
    /// The host's own `find` / `stat` / hash tools.
    Shell,
}

impl Transport {
    /// The `locations.transport` value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Transport::Agent { .. } => "agent",
            Transport::Shell => "shell",
        }
    }
}

/// The only (os, arch) an agent is built for today.
pub const AGENT_TARGET: (&str, &str) = ("Linux", "x86_64");

/// A binary that can serve as the agent: this program's own bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBinary {
    pub bytes: Vec<u8>,
    /// Version it answers in `hello`.
    pub ver: String,
    /// `uname -s` / `uname -m` it runs on.
    pub os: String,
    pub arch: String,
}

impl AgentBinary {
    /// This very program, when it is built for [`AGENT_TARGET`].
    ///
    /// Read from [`std::env::current_exe`]: the static (musl) release is
    /// the artifact meant for this. A dynamically linked build uploads too,
    /// but a host without its libraries fails the `hello` check, and the
    /// shell tier is used — never a silently broken agent.
    pub fn current() -> Option<AgentBinary> {
        if !(cfg!(target_os = "linux") && cfg!(target_arch = "x86_64")) {
            return None;
        }
        let bytes = std::fs::read(std::env::current_exe().ok()?).ok()?;
        Some(AgentBinary {
            bytes,
            ver: env!("CARGO_PKG_VERSION").to_string(),
            os: AGENT_TARGET.0.to_string(),
            arch: AGENT_TARGET.1.to_string(),
        })
    }

    /// The `diskghost` program at `path` as the agent (a GUI is no agent
    /// itself: it ships the command-line program next to it). Assumed to be
    /// this version; a different one fails the `hello` check and the shell
    /// tier is used.
    pub fn from_file(path: &std::path::Path) -> Option<AgentBinary> {
        if !(cfg!(target_os = "linux") && cfg!(target_arch = "x86_64")) {
            return None;
        }
        Some(AgentBinary {
            bytes: std::fs::read(path).ok()?,
            ver: env!("CARGO_PKG_VERSION").to_string(),
            os: AGENT_TARGET.0.to_string(),
            arch: AGENT_TARGET.1.to_string(),
        })
    }

    /// Content-addressed remote file name:
    /// `diskghost-agent-<ver>-<first 8 hex of BLAKE3(bytes)>`.
    pub fn name(&self) -> String {
        format!(
            "diskghost-agent-{}-{}",
            self.ver,
            &blake3::hash(&self.bytes).to_hex()[..8]
        )
    }
}

/// `$1` = agent file name: `have <abs path>` when it is already there and
/// executable, else `need <abs path>`. Read-only.
pub const LOCATE_SCRIPT: &str = r#"[ -n "$HOME" ] || exit 5
d="$HOME/.cache/diskghost"
if [ -f "$d/$1" ] && [ -x "$d/$1" ]; then echo "have $d/$1"; else echo "need $d/$1"; fi
"#;

/// `$1` = agent file name, `$2` = its byte length; stdin = the binary.
/// The only command that writes on a remote host, and only inside
/// `$HOME/.cache/diskghost/`: a temp file, checked for its full length,
/// made executable and moved — never over an existing file (`mv -n`) —
/// to its content-addressed name. Prints the final path. Nothing is ever
/// removed: an interrupted upload leaves its `.tmp` file behind.
pub const UPLOAD_SCRIPT: &str = r#"set -e
[ -n "$HOME" ] || exit 5
d="$HOME/.cache/diskghost"
mkdir -p "$d"
t="$d/.$1.tmp.$$"
cat > "$t"
[ "$(wc -c < "$t" | tr -d ' ')" = "$2" ] || exit 4
chmod 755 "$t"
mv -n "$t" "$d/$1"
echo "$d/$1"
"#;

/// Make sure the host has a working agent: reuse or upload it, then check
/// its `hello`. `Ok(None)` = use the shell tier (unsupported os/arch, any
/// upload failure, a hello that does not match).
pub fn ensure_agent(
    exec: &dyn RemoteExec,
    probe: &HostInfo,
    bin: &AgentBinary,
) -> io::Result<Option<std::path::PathBuf>> {
    Ok(ensure_agent_why(exec, probe, bin).ok())
}

/// [`ensure_agent`], telling why the agent cannot be used (`Err`): the
/// fallback to the shell tier is reported, never silent.
pub fn ensure_agent_why(
    exec: &dyn RemoteExec,
    probe: &HostInfo,
    bin: &AgentBinary,
) -> Result<std::path::PathBuf, String> {
    let (os, arch) = AGENT_TARGET;
    if (bin.os.as_str(), bin.arch.as_str()) != AGENT_TARGET {
        return Err(format!(
            "this build ({} {}) is not an agent for {os} {arch}",
            bin.os, bin.arch
        ));
    }
    if (probe.os.as_str(), probe.arch.as_str()) != AGENT_TARGET {
        let arch_told = if probe.arch.is_empty() {
            "an unknown machine type"
        } else {
            probe.arch.as_str()
        };
        return Err(format!(
            "the host runs {} on {arch_told}; agents exist for {os} {arch} only",
            if probe.os.is_empty() {
                "an unknown os"
            } else {
                probe.os.as_str()
            }
        ));
    }
    let name = bin.name();
    let stderr = |o: &ExecOut| String::from_utf8_lossy(&o.stderr).trim().to_string();
    let found = match run(exec, &["sh", "-c", LOCATE_SCRIPT, "sh", &name]) {
        Ok(o) if o.code == 0 => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            return Err(format!(
                "could not look for it in ~/.cache/diskghost (exit {}): {}",
                o.code,
                stderr(&o)
            ))
        }
        Err(e) => return Err(format!("could not look for it in ~/.cache/diskghost: {e}")),
    };
    let (have, path) = match found.split_once(' ') {
        Some(("have", p)) => (true, p.to_string()),
        Some(("need", p)) => (false, p.to_string()),
        _ => return Err(format!("unexpected answer looking for it: {found:?}")),
    };
    if !path.starts_with('/') || !path.ends_with(&format!("/.cache/diskghost/{name}")) {
        return Err(format!("unexpected agent path {path:?}"));
    }
    if !have {
        let len = bin.bytes.len().to_string();
        match exec.exec_stdin(&["sh", "-c", UPLOAD_SCRIPT, "sh", &name, &len], &bin.bytes) {
            Ok(o) if o.code == 0 && String::from_utf8_lossy(&o.stdout).trim() == path => {}
            Ok(o) => {
                return Err(format!(
                    "upload to {path} failed (exit {}): {}",
                    o.code,
                    stderr(&o)
                ))
            }
            Err(e) => return Err(format!("upload to {path} failed: {e}")),
        }
    }
    let agent = AgentExec {
        exec,
        agent: path.clone(),
    };
    match agent.hello() {
        Ok(h)
            if h.ver == bin.ver && h.arch == probe.arch && h.os.eq_ignore_ascii_case(&probe.os) =>
        {
            Ok(std::path::PathBuf::from(path))
        }
        Ok(h) => Err(format!(
            "{path} answered hello as {} {} {}, expected {} {} {}",
            h.ver, h.os, h.arch, bin.ver, probe.os, probe.arch
        )),
        Err(e) => Err(format!("{path} did not answer hello: {e}")),
    }
}

/// The agent's answer to `stat_fs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFs {
    pub root: RemoteRoot,
    pub cow: bool,
    pub rotational: bool,
}

/// The agent's answer to `hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub ver: String,
    pub os: String,
    pub arch: String,
}

/// Talks to `agent` (`<agent_path> agent`) on the host through `exec`.
pub struct AgentExec<'a> {
    pub exec: &'a dyn RemoteExec,
    pub agent: String,
}

impl AgentExec<'_> {
    /// Send `reqs` in one session; every frame of the answer.
    fn request(&self, reqs: &[serde_json::Value]) -> io::Result<Vec<serde_json::Value>> {
        self.request_live(reqs, &mut |_| {})
    }

    /// `request` invoking `on_frame` with each frame as it arrives — a
    /// long `enumerate` stream reports progress this way.
    fn request_live(
        &self,
        reqs: &[serde_json::Value],
        on_frame: &mut dyn FnMut(&serde_json::Value),
    ) -> io::Result<Vec<serde_json::Value>> {
        let mut input = Vec::new();
        for r in reqs {
            serde_json::to_writer(&mut input, r)?;
            input.push(b'\n');
        }
        let mut pending: Vec<u8> = Vec::new();
        let mut frames = Vec::new();
        let mut parse_err = None;
        let mut take_lines = |pending: &mut Vec<u8>, all: bool| {
            let upto = if all {
                pending.len()
            } else {
                pending
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map(|i| i + 1)
                    .unwrap_or(0)
            };
            for line in pending[..upto]
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
            {
                match serde_json::from_slice::<serde_json::Value>(line) {
                    Ok(v) => {
                        on_frame(&v);
                        frames.push(v);
                    }
                    Err(e) => {
                        parse_err = Some(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("agent sent a line that is not JSON: {e}"),
                        ))
                    }
                }
            }
            pending.drain(..upto);
        };
        let out = self
            .exec
            .exec_live(&[&self.agent, "agent"], Some(&input), &mut |chunk| {
                pending.extend_from_slice(chunk);
                take_lines(&mut pending, false);
            })?;
        // A last line without its terminator, when one ever arrives.
        take_lines(&mut pending, true);
        if out.code == 255 {
            return Err(ssh_failure(&out));
        }
        if let Some(e) = parse_err {
            return Err(e);
        }
        if out.code != 0 && frames.is_empty() {
            return Err(io::Error::other(format!(
                "agent exited {}: {}",
                out.code,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(frames)
    }

    /// The `hello` handshake: a diskghost agent of this protocol.
    pub fn hello(&self) -> io::Result<Hello> {
        let f = self.request(&[serde_json::json!({"v": crate::agent::PROTOCOL, "op": "hello"})])?;
        let h = f.first().cloned().unwrap_or_default();
        if let Some(e) = agent_error(&h) {
            return Err(e);
        }
        if h["agent"] != "diskghost" || h["v"] != crate::agent::PROTOCOL || h["blake3"] != true {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "not a diskghost agent of protocol v{}: {h}",
                    crate::agent::PROTOCOL
                ),
            ));
        }
        let s = |k: &str| h[k].as_str().unwrap_or("").to_string();
        Ok(Hello {
            ver: s("ver"),
            os: s("os"),
            arch: s("arch"),
        })
    }

    /// `stat_fs` of `path` (relative paths: to the remote home).
    pub fn stat_fs(&self, path: &str) -> io::Result<AgentFs> {
        let f = self.request(&[serde_json::json!({"op": "stat_fs", "root": path})])?;
        let fr = f.first().cloned().unwrap_or_default();
        if let Some(e) = agent_error(&fr) {
            return Err(if e.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("remote folder {path:?} not found: {e}"),
                )
            } else {
                e
            });
        }
        let fs = &fr["fs"];
        let root = fs["root"]
            .as_str()
            .filter(|r| r.starts_with('/'))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad stat_fs frame: {fr}"),
                )
            })?
            .to_string();
        let fsid = fs["id"].as_str().filter(|id| {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_hexdigit())
                && id.bytes().any(|b| b != b'0')
        });
        let mount = fs["mount"].as_str().filter(|m| is_under(&root, m));
        Ok(AgentFs {
            root: RemoteRoot {
                fsid: fsid.map(str::to_string),
                fstype: None,
                mount: mount.map(str::to_string),
                root,
            },
            cow: fs["cow"] == true,
            rotational: fs["rotational"] == true,
        })
    }

    /// Every regular file under `root` (canonical), on its filesystem.
    pub fn enumerate(&self, root: &str) -> io::Result<Listing> {
        self.enumerate_live(root, &crate::Progress::default())
    }

    /// `enumerate` reporting `progress` per file frame as it arrives.
    pub fn enumerate_live(&self, root: &str, progress: &crate::Progress) -> io::Result<Listing> {
        let mut files = 0u64;
        let mut bytes = 0u64;
        let frames = self.request_live(
            &[serde_json::json!({"op": "enumerate", "root": root, "xdev": true})],
            &mut |fr| {
                if let Some(f) = fr.get("f") {
                    let valid = f["p"]
                        .as_str()
                        .is_some_and(|p| !p.is_empty() && !p.starts_with('/'));
                    if valid && f["m"].as_u64().is_some() && f["i"].as_u64().is_some() {
                        if let Some(size) = f["s"].as_u64() {
                            files += 1;
                            bytes += size;
                            progress.set_counts(files, bytes);
                        }
                    }
                }
            },
        )?;
        let mut l = Listing::default();
        let mut ended = false;
        for fr in &frames {
            if let Some(e) = agent_error(fr) {
                return Err(e);
            }
            if let Some(f) = fr.get("f") {
                let (Some(p), Some(size), Some(mtime_ns), Some(ino)) = (
                    f["p"]
                        .as_str()
                        .filter(|p| !p.is_empty() && !p.starts_with('/')),
                    f["s"].as_u64(),
                    f["m"].as_u64(),
                    f["i"].as_u64(),
                ) else {
                    l.skipped += 1;
                    continue;
                };
                l.files.push(RemoteFile {
                    path: p.to_string(),
                    size,
                    mtime_ns,
                    ino,
                });
            } else if fr.get("skip").is_some() {
                l.skipped += 1;
            } else if let Some(end) = fr.get("end") {
                ended = true;
                l.complete = end["complete"] == true;
            }
        }
        // A stream cut before its end frame lists only part of the tree.
        l.complete &= ended;
        Ok(l)
    }

    /// BLAKE3 of each of `paths` (absolute), hashed on the host: one digest
    /// per path, `None` when it could not be read.
    pub fn hash(&self, paths: &[String]) -> io::Result<Vec<Option<Vec<u8>>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let reqs: Vec<serde_json::Value> = paths
            .chunks(AGENT_HASH_BATCH)
            .map(|c| serde_json::json!({"op": "hash", "paths": c, "sample": false}))
            .collect();
        let frames = self.request(&reqs)?;
        let mut by_path: std::collections::HashMap<&str, Vec<u8>> =
            std::collections::HashMap::new();
        for fr in &frames {
            if let Some(e) = agent_error(fr) {
                return Err(e);
            }
            let h = &fr["h"];
            if let (Some(p), Some(full)) = (h["p"].as_str(), h["full"].as_str()) {
                if let Some(d) = parse_digest(full.as_bytes(), HashAlgo::Blake3) {
                    by_path.insert(p, d);
                }
            }
        }
        Ok(paths
            .iter()
            .map(|p| by_path.get(p.as_str()).cloned())
            .collect())
    }
}

/// Paths per `hash` request (all requests of a run share one session).
pub const AGENT_HASH_BATCH: usize = 1000;

/// An `{"error":{..}}` frame as an error.
fn agent_error(frame: &serde_json::Value) -> Option<io::Error> {
    let e = frame.get("error")?;
    let code = e["code"].as_str().unwrap_or("error");
    let kind = match code {
        "not_found" => io::ErrorKind::NotFound,
        "version" => io::ErrorKind::Unsupported,
        _ => io::ErrorKind::Other,
    };
    Some(io::Error::new(
        kind,
        format!("agent: {code}: {}", e["msg"].as_str().unwrap_or("")),
    ))
}

/// What [`remote_listing`] found under one `ssh://` folder.
#[derive(Debug, Clone)]
pub struct RemoteListing {
    /// The host as given (`host` or `user@host`).
    pub host: String,
    /// The folder's canonical absolute path on the host.
    pub root: String,
    pub listing: Listing,
    /// `agent` or `shell`: which tier listed it.
    pub transport: &'static str,
    /// Why the agent tier was not used, when it was wanted.
    pub agent_note: Option<String>,
    /// The remote filesystem's (total, free) bytes; `None`: the host was
    /// not asked (neither tier reports it today).
    pub disk: Option<(u64, u64)>,
    /// The scan was cancelled before the listing finished.
    pub cancelled: bool,
}

impl RemoteListing {
    /// `ssh://host//abs/root`: the tree's root — a URL that scans the same
    /// folder again and can never be mistaken for a local path.
    pub fn url(&self) -> String {
        format!("ssh://{}/{}", self.host, self.root)
    }

    /// The listing as a browsable tree rooted at [`RemoteListing::url`].
    pub fn tree(&self) -> crate::ScanTree {
        let mut t = crate::ScanTree::from_remote(
            std::path::Path::new(&self.url()),
            &self.listing.files,
            self.listing.skipped,
            self.listing.complete,
            self.disk,
        );
        if self.cancelled {
            t.mark_cancelled();
        }
        t
    }
}

/// List every file under `url` (`ssh://host/path`) on the host itself —
/// the agent tier when it runs there, else the host's own tools — exactly
/// as `index add` does, without touching any index.
pub fn remote_listing(
    url: &str,
    mode: AgentMode,
    progress: &crate::Progress,
) -> io::Result<RemoteListing> {
    let bin = match mode {
        AgentMode::Auto => AgentBinary::current(),
        AgentMode::Never => None,
    };
    remote_listing_using(url, mode, bin.as_ref(), progress)
}

/// [`remote_listing`] offering `bin` as the agent (`None`: the shell tier,
/// with a note saying why).
pub fn remote_listing_using(
    url: &str,
    mode: AgentMode,
    bin: Option<&AgentBinary>,
    progress: &crate::Progress,
) -> io::Result<RemoteListing> {
    let (host, _) =
        parse_ssh_url(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    remote_listing_with(url, &SshExec::new(&host), mode, bin, progress)
}

/// [`remote_listing`] running its commands through `x`.
pub(crate) fn remote_listing_with(
    url: &str,
    x: &dyn RemoteExec,
    mode: AgentMode,
    bin: Option<&AgentBinary>,
    progress: &crate::Progress,
) -> io::Result<RemoteListing> {
    use std::sync::atomic::Ordering;
    let opened = open_remote(url, x, mode, bin)?;
    // One `find` (or one agent stream) lists everything: cancelling can
    // only stop the scan before it starts.
    let cancelled = progress.cancelled();
    let listing = if cancelled {
        Listing::default()
    } else {
        opened.enumerate(progress)?
    };
    progress
        .files
        .store(listing.files.len() as u64, Ordering::Relaxed);
    progress.bytes.store(
        listing.files.iter().map(|f| f.size).sum(),
        Ordering::Relaxed,
    );
    Ok(RemoteListing {
        host: opened.host.clone(),
        root: opened.rr.root.clone(),
        listing,
        transport: opened.transport(),
        agent_note: opened.agent_note.clone(),
        // Neither `resolve_root` nor the agent's `stat_fs` reports space.
        disk: None,
        cancelled,
    })
}

/// Which tier lists and hashes a remote folder.
pub(crate) enum Tier<'a> {
    /// Our own agent on the host.
    Agent(AgentExec<'a>),
    /// The host's own `find` / `stat` / hash tools.
    Shell(Capabilities),
}

/// A remote folder ready to list: host probed, tier chosen, root resolved.
pub(crate) struct Opened<'a> {
    pub(crate) host: String,
    x: &'a dyn RemoteExec,
    tier: Tier<'a>,
    pub(crate) rr: RemoteRoot,
    /// Whether the filesystem is copy-on-write, when known.
    pub(crate) cow: Option<bool>,
    /// Why the agent tier was not used, when it was wanted.
    pub(crate) agent_note: Option<String>,
}

impl Opened<'_> {
    /// `agent` or `shell`.
    pub(crate) fn transport(&self) -> &'static str {
        match self.tier {
            Tier::Agent(_) => "agent",
            Tier::Shell(_) => "shell",
        }
    }

    /// The digest [`Opened::hash`] makes.
    pub(crate) fn algo(&self) -> HashAlgo {
        match &self.tier {
            Tier::Agent(_) => HashAlgo::Blake3,
            Tier::Shell(c) => c.hash,
        }
    }

    /// Every regular file under the root, on its filesystem, reporting
    /// `progress` as the listing arrives.
    pub(crate) fn enumerate(&self, progress: &crate::Progress) -> io::Result<Listing> {
        match &self.tier {
            Tier::Agent(a) => a.enumerate_live(&self.rr.root, progress),
            Tier::Shell(c) => enum_remote_live(self.x, c, &self.rr.root, progress),
        }
    }

    /// Digest of each of `abs` (absolute paths), made on the host.
    pub(crate) fn hash(&self, abs: &[String]) -> io::Result<Vec<Option<Vec<u8>>>> {
        match &self.tier {
            Tier::Agent(a) => a.hash(abs),
            Tier::Shell(c) => hash_remote(self.x, c, abs),
        }
    }
}

/// Probe the host of `url`, pick the tier (the agent when `mode` allows and
/// it runs there, else the shell tools) and resolve the folder. Read-only
/// apart from the agent upload into `~/.cache/diskghost/`.
pub(crate) fn open_remote<'a>(
    url: &str,
    x: &'a dyn RemoteExec,
    mode: AgentMode,
    bin: Option<&AgentBinary>,
) -> io::Result<Opened<'a>> {
    let (host, path) =
        parse_ssh_url(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let (info, caps) = probe_host(x).map_err(|e| {
        if e.kind() == io::ErrorKind::PermissionDenied {
            shell_unavailable(&host, &e.to_string())
        } else {
            e
        }
    })?;
    // The top tier: our own agent on the host, when it runs there.
    let (agent_path, agent_note) = match (mode, bin) {
        (AgentMode::Never, _) => (None, None),
        (AgentMode::Auto, None) => (
            None,
            Some("this build cannot serve as an agent (only a Linux x86_64 build can)".into()),
        ),
        (AgentMode::Auto, Some(b)) => match ensure_agent_why(x, &info, b) {
            Ok(p) => (Some(p), None),
            Err(why) => (None, Some(why)),
        },
    };
    // Without the agent, the host's own tools (and without those nothing:
    // the probe's "unusable" error).
    let tier = match agent_path.as_deref().and_then(std::path::Path::to_str) {
        Some(p) => Tier::Agent(AgentExec {
            exec: x,
            agent: p.to_string(),
        }),
        None => Tier::Shell(caps?),
    };
    let (rr, cow) = match &tier {
        Tier::Agent(a) => {
            let fs = a.stat_fs(&path)?;
            (fs.root, Some(fs.cow))
        }
        Tier::Shell(caps) => {
            let rr = resolve_root(x, caps, &path)?;
            let cow = rr.fstype.as_deref().map(|t| {
                matches!(
                    t,
                    "btrfs" | "zfs" | "xfs" | "bcachefs" | "overlayfs" | "overlay"
                )
            });
            (rr, cow)
        }
    };
    Ok(Opened {
        host,
        x,
        tier,
        rr,
        cow,
        agent_note,
    })
}

/// The folder `url` (`ssh://host/path`) as a browsable tree, listed on the
/// host (see [`remote_listing`]).
pub fn remote_tree(
    url: &str,
    mode: AgentMode,
    progress: &crate::Progress,
) -> io::Result<crate::ScanTree> {
    remote_listing(url, mode, progress).map(|l| l.tree())
}
