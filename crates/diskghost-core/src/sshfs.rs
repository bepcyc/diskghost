//! Folders on an `sshfs` mount, and the `ssh://` location that indexes the
//! same files without per-file network round trips.
//!
//! Indexing through sshfs costs one FUSE round trip per `stat` (and per
//! directory read); indexed as `ssh://host/path` the host lists the folder
//! itself with one `find -printf` / agent call — the same files, the same
//! change predicate (see `docs/rescan-delta.md`). The two are different
//! locations (`dev:M:m` vs `ssh://host/<fs id>`), so nothing is redirected
//! silently: `index add` refuses an sshfs root and names the URL to use.
//!
//! `/proc/self/mountinfo` shows an sshfs mount as fstype `fuse.sshfs` with
//! the source exactly as given to `sshfs`. Verified on live mounts:
//! `host:/abs/path`, `host:` (the remote home), `user@host:…`, a space
//! octal-escaped as `\040` (decoded by the mountinfo parser). Not verified
//! live, handled by the same rule: `host:rel` (relative to the home) and
//! `[v6addr]:…` for an IPv6 address (sshfs's bracket syntax).

use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::walk::MountEntry;

/// The fstype the kernel reports for an sshfs mount.
pub const SSHFS_FSTYPE: &str = "fuse.sshfs";

/// A folder on an sshfs mount, refused by `index add` unless allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshfsRoot {
    /// The folder given.
    pub path: PathBuf,
    /// The mount's source, as `sshfs` was given it (`user@host:path`).
    pub source: String,
    /// The `ssh://` location of the same folder, when one can be written:
    /// `None` when the host is not a name `ssh://` accepts (an IPv6
    /// address) — `why` then says what to do instead.
    pub url: Option<String>,
    /// Why no URL could be made.
    pub why: Option<String>,
}

impl fmt::Display for SshfsRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is on an sshfs mount ({}): indexing it costs a network round trip per file. ",
            self.path.display(),
            self.source
        )?;
        match (&self.url, &self.why) {
            (Some(url), _) => write!(
                f,
                "Use {} instead: the host lists the folder itself (ssh options such as a \
                 port or key must come from ~/.ssh/config). Pass --allow-sshfs to index the \
                 mount anyway.",
                shell_word(url)
            ),
            (None, Some(why)) => write!(f, "{why} Pass --allow-sshfs to index the mount anyway."),
            (None, None) => write!(f, "Pass --allow-sshfs to index the mount anyway."),
        }
    }
}

impl std::error::Error for SshfsRoot {}

/// `s` as one shell word, ready to paste: quoted only when it needs it.
fn shell_word(s: &str) -> String {
    let plain = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-~".contains(c));
    if plain {
        s.to_string()
    } else {
        crate::remote::shell_quote(s)
    }
}

/// The [`SshfsRoot`] of `path` (canonical, inside `m.mount_point`) on the
/// sshfs mount `m`. Pure: the mount comes from the caller.
pub(crate) fn equivalent(m: &MountEntry, path: &Path) -> SshfsRoot {
    let mut out = SshfsRoot {
        path: path.to_path_buf(),
        source: m.source.clone(),
        url: None,
        why: None,
    };
    let Some((host, remote)) = split_source(&m.source) else {
        out.why = Some(format!(
            "Its source {:?} is not host:path, so no ssh:// URL can be named.",
            m.source
        ));
        return out;
    };
    if let Err(e) = crate::remote::validate_remote_host(host) {
        out.why = Some(format!(
            "Its host {host} cannot be written in an ssh:// URL ({e}): add a `Host <alias>` \
             with `HostName {}` to ~/.ssh/config and use ssh://<alias>/<path>.",
            host.rsplit('@')
                .next()
                .unwrap_or(host)
                .trim_matches(|c| c == '[' || c == ']')
        ));
        return out;
    }
    // Path below the mount: the mount's own root (a bind of a subfolder)
    // and then the folder's place under the mountpoint.
    let mut below: Vec<String> = Vec::new();
    for part in m.root.split('/').filter(|p| !p.is_empty()) {
        below.push(part.to_string());
    }
    if let Ok(rest) = path.strip_prefix(&m.mount_point) {
        for c in rest.components() {
            if let Component::Normal(s) = c {
                below.push(s.to_string_lossy().into_owned());
            }
        }
    }
    // `~` and `~/x` are the home, as a relative path is.
    let remote = remote
        .strip_prefix("~/")
        .or_else(|| (remote == "~").then_some(""))
        .unwrap_or(remote);
    let absolute = remote.starts_with('/');
    let mut parts: Vec<String> = remote
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .map(str::to_string)
        .collect();
    parts.extend(below);
    let joined = parts.join("/");
    out.url = Some(if absolute {
        // ssh://h//abs — `//` marks an absolute remote path.
        format!("ssh://{host}//{joined}")
    } else {
        // ssh://h/rel — relative to the remote home; ssh://h/ is the home.
        format!("ssh://{host}/{joined}")
    });
    out
}

/// `[user@]host:path` → (`[user@]host`, `path`); an IPv6 host is bracketed
/// (`[::1]:path`, `user@[::1]:path`) and keeps its brackets.
fn split_source(source: &str) -> Option<(&str, &str)> {
    let at = source.find('@').map_or(0, |i| i + 1);
    let colon = if source[at..].starts_with('[') {
        at + source[at..].find("]:")? + 1
    } else {
        source.find(':')?
    };
    let (host, path) = (&source[..colon], &source[colon + 1..]);
    (!host.is_empty() && !host.ends_with('@')).then_some((host, path))
}

/// The [`SshfsRoot`] of `path` when it lies on an sshfs mount, from this
/// process's mount table. `None` elsewhere (and where there is no table).
pub fn sshfs_root(path: &Path) -> Option<SshfsRoot> {
    let m = crate::index::mount_of(path)?;
    if m.fstype != SSHFS_FSTYPE {
        return None;
    }
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Some(equivalent(&m, &path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::parse_mountinfo_entry;

    /// Lines as a live kernel writes them (the first three copied from a
    /// real `sshfs` mount, the rest in the same shape).
    const MOUNTINFO: &str = "\
496 35 0:121 / /tmp/s/mnt-abs ro,nosuid,nodev,relatime shared:1399 - fuse.sshfs adroid:/tmp/dg-sshfs-602604 ro,user_id=1000,group_id=1000
1431 35 0:129 / /tmp/s/mnt-home ro,nosuid,nodev,relatime shared:1550 - fuse.sshfs adroid: ro,user_id=1000,group_id=1000
3164 35 0:130 / /tmp/s/mnt\\040sp ro,nosuid,nodev,relatime shared:1811 - fuse.sshfs bepcyc@adroid:/tmp/dg-sshfs-602604/dir\\040with\\040space ro,user_id=1000,group_id=1000
3165 35 0:131 / /mnt/rel rw,nosuid,nodev - fuse.sshfs nas:media/films rw,user_id=1000,group_id=1000
3166 35 0:132 / /mnt/v6 rw,nosuid,nodev - fuse.sshfs [fe80::1]:/srv rw,user_id=1000,group_id=1000
3167 35 0:133 / /mnt/v6u rw,nosuid,nodev - fuse.sshfs me@[::1]:data rw,user_id=1000,group_id=1000
3168 35 0:134 / /mnt/tilde rw,nosuid,nodev - fuse.sshfs nas:~/x rw,user_id=1000,group_id=1000
3169 35 0:135 /sub /mnt/bind rw,nosuid,nodev - fuse.sshfs nas:/srv rw,user_id=1000,group_id=1000
3170 35 0:136 / /run/user/1000/gvfs rw,nosuid,nodev - fuse.gvfsd-fuse gvfsd-fuse rw,user_id=1000,group_id=1000
3171 35 0:137 / /mnt/rclone rw,nosuid,nodev - fuse.rclone remote:bucket rw,user_id=1000,group_id=1000
";

    fn mount(at: &str) -> MountEntry {
        MOUNTINFO
            .lines()
            .filter_map(parse_mountinfo_entry)
            .find(|m| m.mount_point == Path::new(at))
            .unwrap()
    }

    fn url(at: &str, path: &str) -> Option<String> {
        equivalent(&mount(at), Path::new(path)).url
    }

    #[test]
    fn absolute_remote_path_and_a_folder_below_the_mount() {
        assert_eq!(
            url("/tmp/s/mnt-abs", "/tmp/s/mnt-abs").as_deref(),
            Some("ssh://adroid//tmp/dg-sshfs-602604")
        );
        assert_eq!(
            url("/tmp/s/mnt-abs", "/tmp/s/mnt-abs/a/b").as_deref(),
            Some("ssh://adroid//tmp/dg-sshfs-602604/a/b")
        );
    }

    #[test]
    fn remote_home_and_home_relative_paths() {
        assert_eq!(
            url("/tmp/s/mnt-home", "/tmp/s/mnt-home").as_deref(),
            Some("ssh://adroid/")
        );
        assert_eq!(
            url("/tmp/s/mnt-home", "/tmp/s/mnt-home/Music").as_deref(),
            Some("ssh://adroid/Music")
        );
        assert_eq!(
            url("/mnt/rel", "/mnt/rel/2024").as_deref(),
            Some("ssh://nas/media/films/2024")
        );
        assert_eq!(
            url("/mnt/tilde", "/mnt/tilde").as_deref(),
            Some("ssh://nas/x")
        );
    }

    #[test]
    fn user_and_octal_escaped_spaces() {
        let m = mount("/tmp/s/mnt sp");
        assert_eq!(
            m.source,
            "bepcyc@adroid:/tmp/dg-sshfs-602604/dir with space"
        );
        assert_eq!(
            url("/tmp/s/mnt sp", "/tmp/s/mnt sp/sub dir").as_deref(),
            Some("ssh://bepcyc@adroid//tmp/dg-sshfs-602604/dir with space/sub dir")
        );
    }

    #[test]
    fn ipv6_hosts_get_no_url_but_an_alias_hint() {
        for (at, addr) in [("/mnt/v6", "fe80::1"), ("/mnt/v6u", "::1")] {
            let r = equivalent(&mount(at), Path::new(at));
            assert_eq!(r.url, None, "{at}");
            let why = r.why.unwrap();
            assert!(why.contains(&format!("HostName {addr}")), "{why}");
            assert!(r.path == Path::new(at));
        }
        assert_eq!(split_source("me@[::1]:data"), Some(("me@[::1]", "data")));
        assert_eq!(split_source("[fe80::1]:/srv"), Some(("[fe80::1]", "/srv")));
    }

    #[test]
    fn a_bind_of_a_subfolder_keeps_the_mount_root() {
        assert_eq!(
            url("/mnt/bind", "/mnt/bind/x").as_deref(),
            Some("ssh://nas//srv/sub/x")
        );
    }

    #[test]
    fn the_message_names_the_url_and_the_override() {
        let r = equivalent(&mount("/tmp/s/mnt-abs"), Path::new("/tmp/s/mnt-abs/a"));
        let msg = r.to_string();
        assert!(
            msg.contains("Use ssh://adroid//tmp/dg-sshfs-602604/a instead"),
            "{msg}"
        );
        assert!(msg.contains("--allow-sshfs"), "{msg}");
    }

    #[test]
    fn a_url_with_a_space_is_quoted_in_the_message() {
        let r = equivalent(&mount("/tmp/s/mnt sp"), Path::new("/tmp/s/mnt sp"));
        let msg = r.to_string();
        assert!(
            msg.contains("Use 'ssh://bepcyc@adroid//tmp/dg-sshfs-602604/dir with space' instead"),
            "{msg}"
        );
    }

    #[test]
    fn other_fuse_filesystems_are_not_sshfs() {
        for at in ["/run/user/1000/gvfs", "/mnt/rclone"] {
            assert_ne!(mount(at).fstype, SSHFS_FSTYPE, "{at}");
        }
        assert_eq!(split_source("gvfsd-fuse"), None);
    }

    #[test]
    fn the_urls_parse_back_to_host_and_path() {
        for (at, p, host, path) in [
            (
                "/tmp/s/mnt-abs",
                "/tmp/s/mnt-abs/a",
                "adroid",
                "/tmp/dg-sshfs-602604/a",
            ),
            ("/tmp/s/mnt-home", "/tmp/s/mnt-home", "adroid", "."),
            ("/mnt/rel", "/mnt/rel", "nas", "media/films"),
        ] {
            let u = url(at, p).unwrap();
            assert_eq!(
                crate::remote::parse_ssh_url(&u).unwrap(),
                (host.to_string(), path.to_string()),
                "{u}"
            );
        }
    }
}
