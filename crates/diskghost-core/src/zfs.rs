//! ZFS rescan delta: what changed in a dataset since a snapshot, from
//! `zfs diff -F -H <snapshot> <dataset>`, instead of a walk of the tree.
//!
//! Formats, from the OpenZFS sources (`lib/libzfs/libzfs_diff.c`, master)
//! and `zfs-diff(8)`:
//!
//! - one line per change, tab-separated with `-H`:
//!   `<-|+|M|R>\t<type>\t<path>` — `R` adds `\t<new path>`, a link-count
//!   change (`M`) adds `\t(+N)` / `\t(-N)`; `<type>` (`-F`) is `F` regular
//!   file, `/` directory, `@` symlink, `B`, `C`, `|`, `=`, `>`, `P`, `?`;
//! - paths are absolute (dataset mountpoint + path) and every byte that is
//!   not printable ASCII, a space or a backslash is written `\ooo` with four
//!   octal digits (`stream_bytes`: `"\\%04hho"`), so a tab or newline in a
//!   name can never split a field or a line.
//!
//! Diffing a snapshot against the live filesystem makes `zfs` create a
//! just-in-time temporary snapshot (`make_temp_snapshot`,
//! `ZFS_IOC_TMP_SNAPSHOT`, released when `zfs` exits), and needs root or the
//! delegated `diff` permission (`zfs-allow(8)`). That is why the delta is
//! opt-in ([`crate::Options::zfs_diff`]): by default diskghost changes
//! nothing on a pool.

use std::path::PathBuf;

/// What happened to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Removed,
    Created,
    Modified,
    Renamed,
}

/// One line of `zfs diff -F -H`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    pub(crate) kind: Kind,
    /// The `-F` type character: `F` file, `/` directory, `@` link, ….
    pub(crate) ftype: char,
    pub(crate) path: PathBuf,
    /// `R` only: where it went.
    pub(crate) new_path: Option<PathBuf>,
}

/// Parse the whole output of `zfs diff -F -H`. Any line that does not have
/// this shape is an error: a delta read wrong would be a delta made up.
pub(crate) fn parse_diff(out: &str) -> Result<Vec<Change>, String> {
    let mut changes = Vec::new();
    for (n, line) in out.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let bad = |why: &str| format!("zfs diff line {}: {why}: {line:?}", n + 1);
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 3 {
            return Err(bad("fewer than 3 fields"));
        }
        let kind = match f[0] {
            "-" => Kind::Removed,
            "+" => Kind::Created,
            "M" => Kind::Modified,
            "R" => Kind::Renamed,
            _ => return Err(bad("unknown change type")),
        };
        let mut t = f[1].chars();
        let ftype = match (t.next(), t.next()) {
            (Some(c), None) => c,
            _ => return Err(bad("no single-character file type (run with -F)")),
        };
        let path = unescape(f[2]).map_err(|e| bad(&e))?;
        let new_path = match (kind, f.get(3)) {
            (Kind::Renamed, Some(p)) if f.len() == 4 => Some(unescape(p).map_err(|e| bad(&e))?),
            (Kind::Renamed, _) => return Err(bad("a rename needs exactly one new path")),
            (Kind::Modified, Some(d)) if f.len() == 4 && is_link_delta(d) => None,
            (_, None) => None,
            _ => return Err(bad("unexpected extra field")),
        };
        changes.push(Change {
            kind,
            ftype,
            path,
            new_path,
        });
    }
    Ok(changes)
}

/// `(+1)`, `(-2)`: a link-count change.
fn is_link_delta(s: &str) -> bool {
    s.strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .and_then(|s| s.strip_prefix(['+', '-']))
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Undo `stream_bytes`: `\ooo` (a backslash and four octal digits) is one
/// byte; any other backslash is malformed (a real one is `\0134`).
fn unescape(s: &str) -> Result<PathBuf, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            let digits = b.get(i + 1..i + 5).ok_or("cut-off escape")?;
            if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                return Err("bad escape".into());
            }
            let v = digits
                .iter()
                .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
            out.push(u8::try_from(v).map_err(|_| "escape above one byte")?);
            i += 5;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    bytes_to_path(out)
}

#[cfg(unix)]
fn bytes_to_path(b: Vec<u8>) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(b)))
}

#[cfg(not(unix))]
fn bytes_to_path(b: Vec<u8>) -> Result<PathBuf, String> {
    String::from_utf8(b)
        .map(PathBuf::from)
        .map_err(|_| "not UTF-8".to_string())
}

/// One snapshot of a dataset, as `zfs list -Hp -o name,guid,creation`
/// shows it (`creation` in seconds since the epoch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) name: String,
    pub(crate) guid: String,
    pub(crate) creation: i64,
}

/// The arguments listing `dataset`'s own snapshots, oldest first.
pub(crate) fn list_args(dataset: &str) -> Vec<String> {
    [
        "list",
        "-H",
        "-p",
        "-t",
        "snapshot",
        "-o",
        "name,guid,creation",
        "-s",
        "creation",
        "-d",
        "1",
        dataset,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// The arguments asking a dataset's guid.
pub(crate) fn guid_args(dataset: &str) -> Vec<String> {
    ["get", "-H", "-p", "-o", "value", "guid", dataset]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// The arguments diffing `snapshot` against the live `dataset`.
pub(crate) fn diff_args(snapshot: &str, dataset: &str) -> Vec<String> {
    ["diff", "-F", "-H", snapshot, dataset]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// Parse `zfs list -H -p -o name,guid,creation` output.
pub(crate) fn parse_snapshots(out: &str) -> Result<Vec<Snapshot>, String> {
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            match f.as_slice() {
                [name, guid, creation] if name.contains('@') => Ok(Snapshot {
                    name: (*name).to_string(),
                    guid: (*guid).to_string(),
                    creation: creation
                        .parse()
                        .map_err(|_| format!("zfs list: bad creation {creation:?}"))?,
                }),
                _ => Err(format!("zfs list: unexpected line {l:?}")),
            }
        })
        .collect()
}

/// The newest snapshot created at least 2 s before `start_ns` (a scan's
/// start): everything the scan saw happened after it, or is in it.
pub(crate) fn newest_before(snaps: &[Snapshot], start_ns: u64) -> Option<&Snapshot> {
    let start_s = i64::try_from(start_ns / 1_000_000_000).unwrap_or(i64::MAX);
    snaps
        .iter()
        .filter(|s| s.creation + 2 <= start_s)
        .max_by_key(|s| s.creation)
}

/// Run `zfs` with `args`; its stdout, or an error carrying its stderr.
pub(crate) fn run_zfs(args: &[String]) -> std::io::Result<String> {
    let out = std::process::Command::new("zfs")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    if out.status.success() {
        String::from_utf8(out.stdout).map_err(|_| std::io::Error::other("zfs output is not UTF-8"))
    } else {
        Err(std::io::Error::other(format!(
            "zfs {}: {}",
            args.first().map_or("", String::as_str),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped exactly as `print_file` / `print_rename` / `print_link_change`
    /// write it with `-F -H` (tab-separated, `\ooo` escapes).
    const DIFF: &str = "M\t/\t/tank/test/\n\
M\tF\t/tank/test/linked\t(+1)\n\
R\tF\t/tank/test/oldname\t/tank/test/newname\n\
-\tF\t/tank/test/deleted\n\
+\tF\t/tank/test/created\n\
M\tF\t/tank/test/modified\n\
+\tF\t/tank/test/with\\0040space\\0011tab\\0012newline\n\
+\tF\t/tank/test/back\\0134slash\n\
+\tF\t/tank/test/caf\\0303\\0251\n\
R\t/\t/tank/test/dir\\0040a\t/tank/test/dir\\0040b\n\
-\t@\t/tank/test/link\n";

    #[test]
    fn every_kind_and_type_parses() {
        let c = parse_diff(DIFF).unwrap();
        assert_eq!(c.len(), 11);
        assert_eq!(
            c[0],
            Change {
                kind: Kind::Modified,
                ftype: '/',
                path: "/tank/test/".into(),
                new_path: None
            }
        );
        assert_eq!((c[1].kind, c[1].new_path.as_ref()), (Kind::Modified, None));
        assert_eq!(
            (c[2].kind, c[2].path.clone(), c[2].new_path.clone()),
            (
                Kind::Renamed,
                PathBuf::from("/tank/test/oldname"),
                Some(PathBuf::from("/tank/test/newname"))
            )
        );
        assert_eq!(c[3].kind, Kind::Removed);
        assert_eq!(c[4].kind, Kind::Created);
        assert_eq!(c[10].ftype, '@');
    }

    #[test]
    fn escapes_decode_to_the_bytes_of_the_name() {
        let c = parse_diff(DIFF).unwrap();
        assert_eq!(
            c[6].path,
            PathBuf::from("/tank/test/with space\ttab\nnewline")
        );
        assert_eq!(c[7].path, PathBuf::from("/tank/test/back\\slash"));
        assert_eq!(c[8].path, PathBuf::from("/tank/test/café"));
        assert_eq!(
            (c[9].path.clone(), c[9].new_path.clone()),
            (
                PathBuf::from("/tank/test/dir a"),
                Some(PathBuf::from("/tank/test/dir b"))
            )
        );
    }

    #[test]
    fn malformed_lines_are_errors_not_guesses() {
        for bad in [
            "X\tF\t/a",       // unknown change
            "M\t/a",          // too few fields
            "M\tFF\t/a",      // no -F type
            "R\tF\t/a",       // rename without target
            "-\tF\t/a\t/b",   // extra field
            "M\tF\t/a\t(+x)", // not a link delta
            "+\tF\t/a\\08",   // bad octal
            "+\tF\t/a\\01",   // cut-off escape
            "+\tF\t/a\\0777", // above one byte
        ] {
            assert!(parse_diff(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_diff("").unwrap(), Vec::new());
    }

    #[test]
    fn snapshots_and_the_newest_one_before_a_scan() {
        let s =
            parse_snapshots("tank/ds@a\t111\t1000\ntank/ds@b\t222\t2000\ntank/ds@c\t333\t2999\n")
                .unwrap();
        assert_eq!(s.len(), 3);
        let at = |sec: u64| newest_before(&s, sec * 1_000_000_000).map(|x| x.name.as_str());
        assert_eq!(at(3001), Some("tank/ds@c"));
        assert_eq!(at(3000), Some("tank/ds@b"), "2 s margin");
        assert_eq!(at(1001), None);
        assert!(
            parse_snapshots("tank/ds\t1\t2\n").is_err(),
            "not a snapshot"
        );
        assert!(parse_snapshots("tank/ds@a\t1\tx\n").is_err());
    }

    #[test]
    fn the_commands_asked() {
        assert_eq!(
            diff_args("tank/ds@a", "tank/ds"),
            ["diff", "-F", "-H", "tank/ds@a", "tank/ds"]
        );
        assert_eq!(
            list_args("tank/ds").last().map(String::as_str),
            Some("tank/ds")
        );
        assert_eq!(guid_args("tank/ds")[5], "guid");
    }
}
