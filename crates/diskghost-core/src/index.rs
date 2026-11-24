//! The persistent duplicate-search index: one SQLite database that knows
//! every file on every registered volume, so a re-scan only re-reads what
//! changed (the git-annex model).
//!
//! # Model
//!
//! A **location** is a volume — a filesystem — not a folder. It is named by
//! the filesystem UUID (see [`volume_uuid`]), so a disk keeps its identity
//! whatever it is mounted as, and an unplugged disk keeps its rows: its
//! copies still count while it is away. Paths are stored relative to the
//! volume's mountpoint (`locations.root`), so scanning two folders of one
//! disk fills one location, and each scan only answers for the subtree it
//! walked.
//!
//! # Schema (version 5, `PRAGMA user_version`)
//!
//! ```sql
//! meta      (key TEXT PRIMARY KEY, value INTEGER NOT NULL)
//!           -- 'schema_version', 'scan_generation'
//! locations (id INTEGER PRIMARY KEY,
//!            uuid TEXT NOT NULL UNIQUE,   -- fs UUID[:subtree] | dev:<major>:<minor>
//!            host TEXT NOT NULL DEFAULT 'local',  -- local | ssh://<host>
//!            root TEXT NOT NULL,          -- last known mountpoint
//!            role TEXT NOT NULL DEFAULT 'normal'  -- normal | master | scratch
//!            online INTEGER NOT NULL,     -- reachable at its last scan
//!            last_seen INTEGER NOT NULL,  -- unix seconds of that scan
//!            cow INTEGER,                 -- v2: copy-on-write fs at that scan
//!            transport TEXT)              -- v5: shell | agent (remote: how its
//!                                         -- last scan listed and hashed; local: 'shell')
//! files     (location_id → locations(id) ON DELETE CASCADE,
//!            path TEXT,                   -- relative to locations.root
//!            size, mtime_ns, dev, ino INTEGER,
//!            sample BLOB,  -- BLAKE3 of first+last 16 KiB; NULL = not computed
//!            algo TEXT,    -- v4: algorithm of `full`: blake3 | md5 | sha1
//!            full BLOB,    -- whole-file digest (32/16/20 bytes); NULL = not computed
//!            seen INTEGER, -- scan generation that last saw the row
//!            PRIMARY KEY (location_id, path)) WITHOUT ROWID
//! INDEX files_size ON files(size); INDEX files_full ON files(full)
//! scan_roots (location_id → locations(id) ON DELETE CASCADE,  -- v3
//!            path TEXT,                   -- relative to locations.root;
//!                                         -- '' = the mountpoint itself
//!            UNIQUE (location_id, path))
//! ```
//!
//! A **remote location** (`index add ssh://host/path`, see [`crate::remote`])
//! is, like a local one, a filesystem: `host = 'ssh://host'`, uuid
//! `ssh://host/<fs id>`, `root` = the remote mountpoint of that filesystem,
//! paths relative to it, the folder given recorded in `scan_roots` relative
//! to the mountpoint, `dev` 0. So two folders of one remote filesystem share
//! one row, and a rescan of one folder drops vanished rows only below it.
//! When the host tells no fs id, or no mountpoint that is a path prefix of
//! the folder, the uuid is `ssh://host:<folder>`, `root` = the folder, scan
//! root `''`. Its files are listed and
//! hashed by the remote host; this machine never reads them, and no local
//! check ever looks at its `root` (that path may exist here too, holding
//! other files). Its `full` digest is BLAKE3 when the host has `b3sum` or
//! runs the uploaded agent (`transport = 'agent'`), else MD5 / SHA-1
//! (`algo`) — comparable only with digests of the same algorithm. `sample`
//! stays NULL.
//!
//! A **scan root** is a folder `index add` was given. One volume may have
//! several. `apply` quarantines a file into the longest scan root holding
//! it (the user's own scope), and only falls back to the mountpoint when
//! none does — e.g. an index from before v3, which recorded no scan roots
//! (re-run `index add` to record them). Like `files.path`, a scan root is
//! stored relative to the mountpoint, so it survives a remount elsewhere.
//! A scan root whose folder was removed later keeps its row: harmless, as
//! it then covers no file; `forget` drops it with its location.
//!
//! # Safety
//!
//! The index only ever *reads* user files. The only deletions in this module
//! are of index rows (vanished files, `forget`); no user file is unlinked,
//! renamed or written here. Later milestones that act on duplicates
//! quarantine — they never unlink.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::dupes::{sample_file, Sampled};
use crate::walk::{walk, MountEntry};
use crate::{phase, Options, Progress};

/// Schema version this build reads and writes.
const SCHEMA_VERSION: i64 = 5;

/// Version 0 → 1. Each later migration is one more entry; never edit one
/// that has shipped.
pub(crate) const MIGRATIONS: &[&str] = &[
    r"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
INSERT INTO meta (key, value) VALUES ('schema_version', 1), ('scan_generation', 0);
CREATE TABLE locations (
    id        INTEGER PRIMARY KEY,
    uuid      TEXT NOT NULL UNIQUE,
    host      TEXT NOT NULL DEFAULT 'local',
    root      TEXT NOT NULL,
    role      TEXT NOT NULL DEFAULT 'normal'
              CHECK (role IN ('normal', 'master', 'scratch')),
    online    INTEGER NOT NULL DEFAULT 0 CHECK (online IN (0, 1)),
    last_seen INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE files (
    location_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    mtime_ns    INTEGER NOT NULL,
    dev         INTEGER NOT NULL,
    ino         INTEGER NOT NULL,
    sample      BLOB CHECK (sample IS NULL OR length(sample) = 32),
    full        BLOB CHECK (full IS NULL OR length(full) = 32),
    seen        INTEGER NOT NULL,
    PRIMARY KEY (location_id, path)
) WITHOUT ROWID;
CREATE INDEX files_size ON files(size);
CREATE INDEX files_full ON files(full) WHERE full IS NOT NULL;
",
    r"
ALTER TABLE locations ADD COLUMN cow INTEGER CHECK (cow IS NULL OR cow IN (0, 1));
",
    r"
CREATE TABLE scan_roots (
    location_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    UNIQUE (location_id, path)
);
",
    // v4: `full` may be an MD5 or SHA-1 digest made on a remote host, so it
    // carries its algorithm. A CHECK cannot be altered in SQLite: rebuild.
    r"
CREATE TABLE files_v4 (
    location_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    mtime_ns    INTEGER NOT NULL,
    dev         INTEGER NOT NULL,
    ino         INTEGER NOT NULL,
    sample      BLOB CHECK (sample IS NULL OR length(sample) = 32),
    algo        TEXT NOT NULL DEFAULT 'blake3' CHECK (algo IN ('blake3', 'md5', 'sha1')),
    full        BLOB CHECK (full IS NULL OR length(full) =
                    CASE algo WHEN 'md5' THEN 16 WHEN 'sha1' THEN 20 ELSE 32 END),
    seen        INTEGER NOT NULL,
    PRIMARY KEY (location_id, path)
) WITHOUT ROWID;
INSERT INTO files_v4 (location_id, path, size, mtime_ns, dev, ino, sample, algo, full, seen)
    SELECT location_id, path, size, mtime_ns, dev, ino, sample, 'blake3', full, seen FROM files;
DROP TABLE files;
ALTER TABLE files_v4 RENAME TO files;
CREATE INDEX files_size ON files(size);
CREATE INDEX files_full ON files(full) WHERE full IS NOT NULL;
",
    // v5: how a remote location was last listed and hashed — the uploaded
    // agent or the host's shell tools. Local rows keep the default.
    r"
ALTER TABLE locations ADD COLUMN transport TEXT NOT NULL DEFAULT 'shell'
    CHECK (transport IN ('shell', 'agent'));
",
];

/// Role of a location in later copy policies: which copy to keep
/// (`master`), which copies are expendable (`scratch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Normal,
    Master,
    Scratch,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Role::Normal => "normal",
            Role::Master => "master",
            Role::Scratch => "scratch",
        }
    }

    pub(crate) fn parse(s: &str) -> Role {
        match s {
            "master" => Role::Master,
            "scratch" => Role::Scratch,
            _ => Role::Normal,
        }
    }
}

/// The persistent index: an open SQLite database.
pub struct Index {
    pub(crate) conn: Connection,
}

/// What one [`index_scan`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScanSummary {
    /// The location (volume) the scanned folder belongs to.
    pub uuid: String,
    /// That volume's mountpoint.
    pub root: PathBuf,
    /// Files the walk met (and indexed).
    pub seen: u64,
    /// Of those, not in the index before.
    pub new: u64,
    /// Of those, indexed with another size, time or inode: hashes reset.
    pub changed: u64,
    /// Of those, unchanged: not read, only marked seen.
    pub unchanged: u64,
    /// Rows dropped because their file is gone (index rows only).
    pub vanished: u64,
    /// Files whose two ends were read because their size collides.
    pub sampled: u64,
    /// Entries not indexed: unreadable, a path that is not UTF-8, or on
    /// another device than the scanned folder.
    pub skipped: u64,
    /// A bounded sample of the skipped paths — enough to point a report at
    /// what coverage was missed, never a whole tree of names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_paths: Vec<String>,
    /// The walk covered the whole subtree. Vanished rows are only dropped
    /// after a complete walk: unreadable or cut-off is not gone.
    pub complete: bool,
    /// Remote locations: files hashed whole on the remote host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hashed: Option<u64>,
    /// Remote locations: the digest algorithm the host offers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algo: Option<String>,
    /// Remote locations: `agent` or `shell` (see [`crate::remote::Transport`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Remote locations: why the agent was asked for (`--agent auto`) but
    /// not used — the shell tier ran instead. Never silent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_note: Option<String>,
}

/// [`Index::find_files`] page size when the caller names none.
pub const FIND_DEFAULT_LIMIT: usize = 200;
/// The largest page [`Index::find_files`] callers may ask for.
pub const FIND_MAX_LIMIT: usize = 1000;

/// One indexed file found by [`Index::find_files`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FoundFile {
    /// Full path: the location's `root` joined with the indexed relative path.
    pub path: PathBuf,
    pub location_uuid: String,
    pub host: String,
    pub root: PathBuf,
    pub size: u64,
    /// Whether a full-content hash is stored for the file.
    pub hashed: bool,
}

/// One location as `diskghost index status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocationStatus {
    pub uuid: String,
    pub host: String,
    pub role: Role,
    /// Last known mountpoint.
    pub root: PathBuf,
    /// Reachable now: the mountpoint holds this very volume.
    pub online: bool,
    /// Indexed files and their bytes — kept while the volume is away.
    pub files: u64,
    pub bytes: u64,
    /// Unix seconds of the last scan.
    pub last_seen: i64,
    /// Folders `index add` was given on this volume (absolute, under the
    /// current `root`): apply quarantines into the longest one covering
    /// a file. A root whose folder was later removed may stay listed.
    pub roots: Vec<PathBuf>,
}

pub(crate) fn db_err(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

/// How long a statement waits for a lock another process holds.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether `e` is the index being locked by another writer (SQLite BUSY or
/// LOCKED) — as opposed to any other failure.
pub fn is_busy(e: &io::Error) -> bool {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<rusqlite::Error>())
        .is_some_and(|re| {
            matches!(
                re,
                rusqlite::Error::SqliteFailure(f, _)
                    if matches!(
                        f.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    )
            )
        })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// SQLite integers are signed 64-bit: device and inode numbers are stored by
/// bit pattern, so every `u64` round-trips.
pub(crate) fn to_sql(v: u64) -> i64 {
    v as i64
}

impl Index {
    /// Open the index at `path`, creating it (and migrating an older one).
    pub fn open(path: &Path) -> io::Result<Index> {
        Index::open_with_busy_timeout(path, BUSY_TIMEOUT)
    }

    /// [`Index::open`] waiting `busy` (instead of 10 s) for a lock another
    /// process holds before giving up with an error [`is_busy`] recognises.
    pub fn open_with_busy_timeout(path: &Path, busy: std::time::Duration) -> io::Result<Index> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).map_err(db_err)?;
        Index::init(conn, busy)
    }

    fn init(mut conn: Connection, busy: std::time::Duration) -> io::Result<Index> {
        conn.busy_timeout(busy).map_err(db_err)?;
        // `journal_mode` answers with the mode it set: read, don't execute.
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .map_err(db_err)?;
        // Per connection, not stored: without it `forget` would not cascade.
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(db_err)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(db_err)?;
        if version > SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "index schema version {version} is newer than this diskghost \
                     understands ({SCHEMA_VERSION}); upgrade diskghost"
                ),
            ));
        }
        let from = usize::try_from(version).unwrap_or(0);
        if from < MIGRATIONS.len() {
            let tx = conn.transaction().map_err(db_err)?;
            for sql in &MIGRATIONS[from..] {
                tx.execute_batch(sql).map_err(db_err)?;
            }
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(db_err)?;
            tx.execute(
                "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                [SCHEMA_VERSION],
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
        }
        Ok(Index { conn })
    }

    /// Every location with its file count and bytes, and whether it is
    /// reachable *now*. Read-only: an offline location keeps all its rows.
    pub fn status(&self) -> io::Result<Vec<LocationStatus>> {
        self.status_with(&Probe::REAL)
    }

    /// [`Index::status`] without asking any remote host: `online` is `false`
    /// for every `ssh://` location because it was *not checked* (an ssh
    /// round trip, up to the connect timeout per unreachable host). Local
    /// locations are probed as usual.
    pub fn status_without_remote_probe(&self) -> io::Result<Vec<LocationStatus>> {
        self.status_with(&Probe {
            remote_online: &|_, _| false,
            ..Probe::REAL
        })
    }

    /// Whether this process could take the index's write lock right now:
    /// takes it and releases it again at once, changing nothing. Fails with
    /// an [`is_busy`] error when another process holds it for longer than
    /// the busy timeout.
    pub fn probe_write_lock(&self) -> io::Result<()> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
            .map_err(db_err)
    }

    /// [`Index::status`] asking `probe` whether a location is reachable.
    pub(crate) fn status_with(&self, probe: &Probe) -> io::Result<Vec<LocationStatus>> {
        let mut st = self
            .conn
            .prepare(
                "SELECT l.uuid, l.host, l.role, l.root, l.last_seen, \
                        COUNT(f.path), COALESCE(SUM(f.size), 0), l.id \
                 FROM locations l LEFT JOIN files f ON f.location_id = l.id \
                 GROUP BY l.id ORDER BY l.root, l.uuid",
            )
            .map_err(db_err)?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (uuid, host, role, root, last_seen, files, bytes, id) = row.map_err(db_err)?;
            let root = PathBuf::from(root);
            let roots = scan_roots(&self.conn, id)?
                .iter()
                .map(|rel| under(&root, rel))
                .collect();
            let online = if crate::remote::is_remote_host(&host) {
                root.to_str()
                    .is_some_and(|r| (probe.remote_online)(&host, r))
            } else {
                (probe.online)(&root, &uuid)
            };
            out.push(LocationStatus {
                online,
                uuid,
                host,
                role: Role::parse(&role),
                root,
                files: files.max(0) as u64,
                bytes: bytes.max(0) as u64,
                last_seen,
                roots,
            });
        }
        Ok(out)
    }

    /// Drop a location and all its rows from the index. Files on disk are
    /// not touched. `false` when no such location exists.
    pub fn forget(&self, uuid: &str) -> io::Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM locations WHERE uuid = ?1", [uuid])
            .map_err(db_err)?;
        Ok(n > 0)
    }

    /// Search the indexed paths. `pattern` is a plain substring (no glob, no
    /// regex), matched ASCII-case-insensitively against the full path
    /// (`root/relative`) with SQL `LIKE` — `%`, `_` and `\` in it are
    /// literal. `location`, when given, restricts to the location with that
    /// uuid or to full paths at or under that folder (`/data` covers
    /// `/data/x`, never `/database`). Ordered by location, then path, so
    /// `limit`/`offset` paging is stable. Read-only; offline locations are
    /// searched too. Callers cap `limit` at [`FIND_MAX_LIMIT`].
    pub fn find_files(
        &self,
        pattern: &str,
        location: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> io::Result<Vec<FoundFile>> {
        fn like_escape(s: &str) -> String {
            let mut o = String::with_capacity(s.len());
            for c in s.chars() {
                if matches!(c, '%' | '_' | '\\') {
                    o.push('\\');
                }
                o.push(c);
            }
            o
        }
        const FULL: &str = "(rtrim(l.root, '/') || '/' || f.path)";
        let contains = format!("%{}%", like_escape(pattern));
        // A folder restriction: the folder itself or anything below it.
        let (uuid, folder, below) = match location {
            Some(l) => {
                let f = l.trim_end_matches('/');
                (
                    l.to_string(),
                    f.to_string(),
                    format!("{}/%", like_escape(f)),
                )
            }
            None => (String::new(), String::new(), String::new()),
        };
        let sql = format!(
            "SELECT l.uuid, l.host, l.root, f.path, f.size, f.full IS NOT NULL \
             FROM files f JOIN locations l ON l.id = f.location_id \
             WHERE {FULL} LIKE ?1 ESCAPE '\\' \
               AND (?2 = 0 OR l.uuid = ?3 OR {FULL} = ?7 OR {FULL} LIKE ?4 ESCAPE '\\') \
             ORDER BY l.uuid, f.path LIMIT ?5 OFFSET ?6"
        );
        let mut st = self.conn.prepare_cached(&sql).map_err(db_err)?;
        let rows = st
            .query_map(
                params![
                    contains,
                    i64::from(location.is_some()),
                    uuid,
                    below,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    i64::try_from(offset).unwrap_or(i64::MAX),
                    folder,
                ],
                |r| {
                    let root = PathBuf::from(r.get::<_, String>(2)?);
                    let rel: String = r.get(3)?;
                    Ok(FoundFile {
                        location_uuid: r.get(0)?,
                        host: r.get(1)?,
                        path: under(&root, &rel),
                        root,
                        size: r.get::<_, i64>(4)?.max(0) as u64,
                        hashed: r.get(5)?,
                    })
                },
            )
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    /// Set only a location's role (host and rows untouched).
    pub fn set_role(&self, uuid: &str, role: Role) -> io::Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE locations SET role = ?2 WHERE uuid = ?1",
                params![uuid, role.as_str()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no location {uuid} in the index"),
            ));
        }
        Ok(())
    }

    /// Set a location's host (stored verbatim) and, when given, its role.
    pub fn configure(&self, uuid: &str, role: Option<Role>, host: &str) -> io::Result<()> {
        validate_host(host).map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e))?;
        let n = self
            .conn
            .execute(
                "UPDATE locations SET host = ?2, role = COALESCE(?3, role) WHERE uuid = ?1",
                params![uuid, host, role.map(Role::as_str)],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no location {uuid} in the index"),
            ));
        }
        Ok(())
    }
}

/// The scan roots of location `id`, relative to its mountpoint ('' = the
/// mountpoint itself), sorted.
pub(crate) fn scan_roots(conn: &Connection, id: i64) -> io::Result<Vec<String>> {
    let mut st = conn
        .prepare_cached("SELECT path FROM scan_roots WHERE location_id = ?1 ORDER BY path")
        .map_err(db_err)?;
    let rows = st.query_map([id], |r| r.get(0)).map_err(db_err)?;
    rows.collect::<Result<_, _>>().map_err(db_err)
}

/// `root` joined with a mountpoint-relative path; '' is `root` itself
/// (`join("")` would add a trailing slash).
pub(crate) fn under(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

/// A location is online when its mountpoint exists *and* holds the same
/// volume: an unplugged disk usually leaves its empty mountpoint directory
/// behind on the parent filesystem, so existence alone proves nothing.
fn is_online(root: &Path, uuid: &str) -> bool {
    root.is_dir() && volume(root).is_ok_and(|v| v.key == uuid && v.mount_point == root)
}

/// Hosts a location may live on: this machine (`local`, `localhost`) or a
/// remote one reached over ssh (`ssh://host`).
pub fn validate_host(host: &str) -> Result<(), String> {
    match host {
        "local" | "localhost" => Ok(()),
        other => match other.strip_prefix("ssh://") {
            Some(h) => crate::remote::validate_remote_host(h),
            None => Err(format!(
                "host {other:?}: a remote location is given as an ssh://{other}/path \
                 folder, not with --host"
            )),
        },
    }
}

/// The index's name for the volume holding `path`: the filesystem UUID when
/// the volume is a block device listed in `/dev/disk/by-uuid`, followed by
/// `:<subtree>` when the mount shows only part of the filesystem (a bind
/// mount, a btrfs subvolume); otherwise `dev:<major>:<minor>`.
///
/// Resolution: `stat(path).st_dev` → the `/proc/self/mountinfo` line of that
/// device whose mountpoint is the longest prefix of `path` → its mount
/// source → the `/dev/disk/by-uuid` link pointing at the same device node.
/// A nested btrfs subvolume (an anonymous `st_dev` mountinfo never lists)
/// belongs to the btrfs mount enclosing it.
///
/// The `dev:` fallback (tmpfs, overlay, ZFS datasets, NFS…) is only as
/// stable as the device number, which may change across reboots.
pub fn volume_uuid(path: &Path) -> io::Result<String> {
    volume(path).map(|v| v.key)
}

/// A volume as named by the index, with the mountpoint paths are relative to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Volume {
    pub(crate) key: String,
    pub(crate) mount_point: PathBuf,
}

/// This process's mount table (empty where there is none).
#[cfg(unix)]
fn mount_table() -> Vec<MountEntry> {
    std::fs::read("/proc/self/mountinfo")
        .map(|b| {
            String::from_utf8_lossy(&b)
                .lines()
                .filter_map(crate::walk::parse_mountinfo_entry)
                .collect()
        })
        .unwrap_or_default()
}

/// The mount holding `path`, as [`volume_uuid`] picks it.
#[cfg(unix)]
pub(crate) fn mount_of(path: &Path) -> Option<MountEntry> {
    use std::os::unix::fs::MetadataExt;
    let path = std::fs::canonicalize(path).ok()?;
    let dev = std::fs::metadata(&path).ok()?.dev();
    pick_mount(&mount_table(), dev, &path).cloned()
}

#[cfg(not(unix))]
pub(crate) fn mount_of(_path: &Path) -> Option<MountEntry> {
    None
}

#[cfg(unix)]
fn volume(path: &Path) -> io::Result<Volume> {
    use std::os::unix::fs::MetadataExt;
    let path = std::fs::canonicalize(path)?;
    let dev = std::fs::metadata(&path)?.dev();
    let mounts = mount_table();
    if let Some(v) = resolve_volume(&mounts, dev, &path, &uuid_of_source) {
        return Ok(v);
    }
    // No mount table (not Linux, or a sandbox without /proc): the device
    // number names the volume, and its top is where the device changes.
    let mut top = path.as_path();
    while let Some(parent) = top.parent() {
        match std::fs::metadata(parent) {
            Ok(m) if m.dev() == dev => top = parent,
            _ => break,
        }
    }
    Ok(Volume {
        key: dev_key(dev_major(dev), dev_minor(dev)),
        mount_point: top.to_path_buf(),
    })
}

#[cfg(not(unix))]
fn volume(path: &Path) -> io::Result<Volume> {
    let _ = path;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the persistent index needs a Unix volume table",
    ))
}

#[cfg(any(unix, test))]
fn dev_key(major: u64, minor: u64) -> String {
    format!("dev:{major}:{minor}")
}

/// glibc's `major()` (inverse of [`crate::walk::makedev`]).
#[cfg(unix)]
fn dev_major(dev: u64) -> u64 {
    ((dev >> 8) & 0x0000_0fff) | ((dev >> 32) & 0xffff_f000)
}

/// glibc's `minor()`.
#[cfg(unix)]
fn dev_minor(dev: u64) -> u64 {
    (dev & 0x0000_00ff) | ((dev >> 12) & 0xffff_ff00)
}

/// The pure half of [`volume_uuid`]: pick the mount of `path` (canonical)
/// on device `dev` among `mounts`, and name it, asking `uuid_of` for the
/// filesystem UUID of a mount source. `None` when no mount fits.
#[cfg(any(unix, test))]
fn resolve_volume(
    mounts: &[MountEntry],
    dev: u64,
    path: &Path,
    uuid_of: &dyn Fn(&str) -> Option<String>,
) -> Option<Volume> {
    let m = pick_mount(mounts, dev, path)?;
    let key = match uuid_of(&m.source) {
        Some(uuid) if m.root == "/" => uuid,
        Some(uuid) => format!("{uuid}:{}", m.root),
        None => dev_key(m.major, m.minor),
    };
    Some(Volume {
        key,
        mount_point: m.mount_point.clone(),
    })
}

/// The mount of `path` (canonical) on device `dev` among `mounts`.
#[cfg(any(unix, test))]
fn pick_mount<'m>(mounts: &'m [MountEntry], dev: u64, path: &Path) -> Option<&'m MountEntry> {
    // Longest mountpoint that is a prefix of `path`; later lines win ties
    // (a later mount covers an earlier one on the same directory).
    let longest = |pred: &dyn Fn(&MountEntry) -> bool| {
        let mut best: Option<&MountEntry> = None;
        for m in mounts
            .iter()
            .filter(|m| pred(m) && path.starts_with(&m.mount_point))
        {
            if best
                .is_none_or(|b| m.mount_point.as_os_str().len() >= b.mount_point.as_os_str().len())
            {
                best = Some(m);
            }
        }
        best
    };
    longest(&|m| m.dev == dev).or_else(|| {
        // A nested btrfs subvolume has its own anonymous st_dev that no
        // mountinfo line carries; it is part of the enclosing btrfs mount.
        longest(&|_| true).filter(|m| m.fstype == "btrfs")
    })
}

/// Filesystem UUID of a mount source such as `/dev/sda1` or
/// `/dev/mapper/root`: the `/dev/disk/by-uuid` entry resolving to the same
/// device node. Pseudo sources (`tmpfs`, `overlay`, `pool/dataset`) have none.
#[cfg(unix)]
fn uuid_of_source(source: &str) -> Option<String> {
    if !source.starts_with("/dev/") {
        return None;
    }
    let node = std::fs::canonicalize(source).ok()?;
    std::fs::read_dir("/dev/disk/by-uuid")
        .ok()?
        .filter_map(Result::ok)
        .find(|e| std::fs::canonicalize(e.path()).is_ok_and(|t| t == node))
        .and_then(|e| e.file_name().into_string().ok())
}

/// Whether `path` sits on a copy-on-write filesystem (btrfs, ZFS, XFS with
/// reflinks, bcachefs) or on overlayfs, where two "copies" may already share
/// their blocks — so deleting one may free nothing. `false` when unknown.
pub fn is_cow_fs(path: &Path) -> bool {
    statfs_magic(path).is_some_and(is_cow_magic)
}

#[cfg(target_os = "linux")]
fn statfs_magic(path: &Path) -> Option<u64> {
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
    // `f_type` is signed on some targets: keep the low 32 bits the magic uses.
    #[allow(clippy::unnecessary_cast)]
    Some((st.f_type as u64) & 0xffff_ffff)
}

#[cfg(not(target_os = "linux"))]
fn statfs_magic(_path: &Path) -> Option<u64> {
    None
}

/// `statfs.f_type` magics of copy-on-write (and overlay) filesystems.
fn is_cow_magic(f_type: u64) -> bool {
    matches!(
        f_type,
        0x9123_683e // btrfs
            | 0x2fc1_2fc1 // zfs
            | 0x5846_5342 // xfs (reflink-capable)
            | 0xca45_1a4e // bcachefs
            | 0x794c_7630 // overlayfs
    )
}

/// Index the files under `root` (a folder on one volume): record new and
/// changed files, mark unchanged ones seen without reading them, drop rows of
/// files that vanished from the walked subtree, then read the two ends of
/// every indexed file whose size collides with another indexed file.
///
/// A file whose (size, mtime_ns, inode) matches its row is never re-read.
/// The walk stays on `root`'s filesystem (a location is one volume) and
/// reuses the shared walker with its remote/FUSE guards.
pub fn index_scan(
    idx: &Index,
    root: &Path,
    opts: &Options,
    progress: &Progress,
) -> io::Result<ScanSummary> {
    scan_with(idx, root, opts, progress, &Probe::REAL)
}

/// Everything the index asks the machine, in one place — the seam tests use
/// to give two folders of one disk two volume identities, take a volume
/// offline, or pretend a disk spins.
pub(crate) struct Probe<'a> {
    /// The volume holding a folder (see [`volume_uuid`]).
    pub(crate) volume: &'a dyn Fn(&Path) -> io::Result<Volume>,
    /// Whether a location (last known root, uuid) is reachable now.
    pub(crate) online: &'a dyn Fn(&Path, &str) -> bool,
    /// Whether a volume's root sits on a copy-on-write filesystem.
    pub(crate) cow: &'a dyn Fn(&Path) -> bool,
    /// Whether a volume's root sits on a spinning disk.
    pub(crate) rotational: &'a dyn Fn(&Path) -> bool,
    /// Told every file whose content is read.
    pub(crate) on_read: &'a (dyn Fn(&Path) + Sync),
    /// Whether a remote location (`host` column, root) is reachable now.
    pub(crate) remote_online: &'a dyn Fn(&str, &str) -> bool,
}

fn no_read(_: &Path) {}

impl Probe<'static> {
    /// The machine itself.
    pub(crate) const REAL: Probe<'static> = Probe {
        volume: &volume,
        online: &is_online,
        cow: &is_cow_fs,
        rotational: &crate::rotational::is_rotational,
        on_read: &no_read,
        remote_online: &crate::remote::ssh_online,
    };
}

/// A file as `stat` sees it, in the index's terms.
pub(crate) struct Stat {
    pub(crate) size: u64,
    pub(crate) mtime_ns: u64,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

#[cfg(unix)]
pub(crate) fn stat_of(meta: &std::fs::Metadata) -> Stat {
    use std::os::unix::fs::MetadataExt;
    Stat {
        size: meta.len(),
        mtime_ns: crate::walk::modified_ns(meta),
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

#[cfg(not(unix))]
pub(crate) fn stat_of(meta: &std::fs::Metadata) -> Stat {
    Stat {
        size: meta.len(),
        mtime_ns: crate::walk::modified_ns(meta),
        dev: 0,
        ino: 0,
    }
}

/// [`index_scan`] with `on_read` called for every file whose content is
/// read — the seam that lets tests prove what is (not) re-read.
#[cfg(all(test, unix))]
fn scan_observed(
    idx: &Index,
    root: &Path,
    opts: &Options,
    progress: &Progress,
    on_read: &(dyn Fn(&Path) + Sync),
) -> io::Result<ScanSummary> {
    let probe = Probe {
        on_read,
        ..Probe::REAL
    };
    scan_with(idx, root, opts, progress, &probe)
}

/// Index a remote folder `ssh://host/path` over ssh (see [`crate::remote`]):
/// the host enumerates and hashes its own files; no content is copied.
pub fn index_remote(
    idx: &Index,
    url: &str,
    mode: crate::remote::AgentMode,
    progress: &Progress,
) -> io::Result<ScanSummary> {
    let (host, _) = crate::remote::parse_ssh_url(url)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // The binary is read only when it may be uploaded.
    let bin = match mode {
        crate::remote::AgentMode::Auto => crate::remote::AgentBinary::current(),
        crate::remote::AgentMode::Never => None,
    };
    remote_scan_with(
        idx,
        url,
        &crate::remote::SshExec::new(&host),
        mode,
        bin.as_ref(),
        progress,
    )
}

/// [`index_remote`] running its commands through `x`.
pub(crate) fn remote_scan_with(
    idx: &Index,
    url: &str,
    x: &dyn crate::remote::RemoteExec,
    mode: crate::remote::AgentMode,
    bin: Option<&crate::remote::AgentBinary>,
    progress: &Progress,
) -> io::Result<ScanSummary> {
    // Everything the host is asked before the index changes: an
    // unreachable or unusable host records nothing.
    let opened = crate::remote::open_remote(url, x, mode, bin)?;
    let (host, rr, cow) = (opened.host.clone(), opened.rr.clone(), opened.cow);
    let (algo, transport) = (opened.algo(), opened.transport());
    let agent_note = opened.agent_note.clone();
    // A filesystem with its mountpoint, like a local volume; else the
    // folder alone. `base` is the `root` column, `prefix` the scan root.
    let (uuid, base, prefix) = match (&rr.fsid, &rr.mount) {
        (Some(id), Some(mount)) => {
            // resolve_root checked that `mount` is a path prefix of root.
            let prefix = rr
                .root
                .strip_prefix(mount.as_str())
                .unwrap_or("")
                .trim_start_matches('/')
                .to_string();
            (format!("ssh://{host}/{id}"), mount.clone(), prefix)
        }
        _ => (
            format!("ssh://{host}:{}", rr.root),
            rr.root.clone(),
            String::new(),
        ),
    };
    let mut listing = opened.enumerate(progress)?;
    if !prefix.is_empty() {
        for f in &mut listing.files {
            f.path = format!("{prefix}/{}", f.path);
        }
    }
    let mut sum = ScanSummary {
        uuid: uuid.clone(),
        root: PathBuf::from(&base),
        skipped: listing.skipped,
        skipped_paths: listing.skip_paths.clone(),
        algo: Some(algo.as_str().to_string()),
        transport: Some(transport.to_string()),
        agent_note,
        ..ScanSummary::default()
    };

    let tx = idx.conn.unchecked_transaction().map_err(db_err)?;
    let loc: i64 = tx
        .query_row(
            "INSERT INTO locations (uuid, host, root, online, last_seen, cow, transport) \
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6) \
             ON CONFLICT(uuid) DO UPDATE SET host = excluded.host, root = excluded.root, \
             online = 1, last_seen = excluded.last_seen, cow = excluded.cow, \
             transport = excluded.transport RETURNING id",
            params![
                uuid,
                format!("ssh://{host}"),
                base,
                now_secs(),
                cow,
                transport
            ],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    // The folder given, relative to the mountpoint (as for a local volume).
    tx.execute(
        "INSERT OR IGNORE INTO scan_roots (location_id, path) VALUES (?1, ?2)",
        params![loc, prefix],
    )
    .map_err(db_err)?;
    let gen: i64 = tx
        .query_row(
            "UPDATE meta SET value = value + 1 WHERE key = 'scan_generation' RETURNING value",
            [],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    {
        let mut get = tx
            .prepare_cached(
                "SELECT size, mtime_ns, ino FROM files WHERE location_id = ?1 AND path = ?2",
            )
            .map_err(db_err)?;
        let mut insert = tx
            .prepare_cached(
                "INSERT INTO files (location_id, path, size, mtime_ns, dev, ino, seen) \
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            )
            .map_err(db_err)?;
        let mut touch = tx
            .prepare_cached("UPDATE files SET seen = ?3 WHERE location_id = ?1 AND path = ?2")
            .map_err(db_err)?;
        let mut reset = tx
            .prepare_cached(
                "UPDATE files SET size = ?3, mtime_ns = ?4, dev = 0, ino = ?5, sample = NULL, \
                 full = NULL, algo = 'blake3', seen = ?6 WHERE location_id = ?1 AND path = ?2",
            )
            .map_err(db_err)?;
        for f in &listing.files {
            if progress.cancelled() {
                break;
            }
            let (size, mtime, ino) = (to_sql(f.size), to_sql(f.mtime_ns), to_sql(f.ino));
            sum.seen += 1;
            let old: Option<(i64, i64, i64)> = get
                .query_row(params![loc, f.path], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()
                .map_err(db_err)?;
            match old {
                None => {
                    insert
                        .execute(params![loc, f.path, size, mtime, ino, gen])
                        .map_err(db_err)?;
                    sum.new += 1;
                }
                Some(o) if o == (size, mtime, ino) => {
                    touch.execute(params![loc, f.path, gen]).map_err(db_err)?;
                    sum.unchanged += 1;
                }
                Some(_) => {
                    reset
                        .execute(params![loc, f.path, size, mtime, ino, gen])
                        .map_err(db_err)?;
                    sum.changed += 1;
                }
            }
        }
    }
    sum.complete = listing.complete && !progress.cancelled();
    if sum.complete {
        sum.vanished = drop_vanished(&tx, loc, gen, &prefix, &[]).map_err(db_err)? as u64;
    }
    tx.commit().map_err(db_err)?;
    if progress.cancelled() {
        return Ok(sum);
    }

    // Hash, on the host, every file of this location whose size collides
    // with another indexed file and that has no digest yet.
    let pending: Vec<(String, i64, i64, i64)> = {
        let mut st = idx
            .conn
            .prepare(
                "SELECT path, size, mtime_ns, ino FROM files \
                 WHERE location_id = ?1 AND full IS NULL AND size > 0 AND size IN \
                   (SELECT size FROM files WHERE size > 0 GROUP BY size HAVING COUNT(*) > 1) \
                 ORDER BY path",
            )
            .map_err(db_err)?;
        let rows = st
            .query_map([loc], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)?
    };
    let abs: Vec<String> = pending
        .iter()
        .map(|(p, ..)| crate::remote::join_remote(&base, p))
        .collect();
    progress.enter(
        phase::HASH,
        abs.len() as u64,
        pending.iter().map(|(_, s, ..)| *s as u64).sum(),
    );
    let digests = opened.hash(&abs)?;
    let tx = idx.conn.unchecked_transaction().map_err(db_err)?;
    let mut hashed = 0;
    {
        // Only while the row is still the listed file.
        let mut set = tx
            .prepare_cached(
                "UPDATE files SET full = ?3, algo = ?4 WHERE location_id = ?1 AND path = ?2 \
                 AND size = ?5 AND mtime_ns = ?6 AND ino = ?7",
            )
            .map_err(db_err)?;
        for ((path, size, mtime, ino), d) in pending.iter().zip(digests) {
            progress.files.fetch_add(1, Ordering::Relaxed);
            progress.bytes.fetch_add(*size as u64, Ordering::Relaxed);
            if let Some(d) = d {
                hashed += set
                    .execute(params![loc, path, d, algo.as_str(), size, mtime, ino])
                    .map_err(db_err)? as u64;
            }
        }
    }
    tx.commit().map_err(db_err)?;
    sum.hashed = Some(hashed);
    Ok(sum)
}

/// [`index_scan`] asking `probe` instead of the machine.
pub(crate) fn scan_with(
    idx: &Index,
    root: &Path,
    opts: &Options,
    progress: &Progress,
    probe: &Probe,
) -> io::Result<ScanSummary> {
    let root = std::fs::canonicalize(root)?;
    let root_meta = std::fs::metadata(&root)?;
    if !root_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a directory: {}", root.display()),
        ));
    }
    let root_dev = stat_of(&root_meta).dev;
    let vol = (probe.volume)(&root)?;
    let mount = vol
        .mount_point
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mountpoint is not UTF-8"))?;
    let prefix = root
        .strip_prefix(&vol.mount_point)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path is not UTF-8"))?
        .to_string();
    let mut sum = ScanSummary {
        uuid: vol.key.clone(),
        root: vol.mount_point.clone(),
        ..ScanSummary::default()
    };

    // One transaction for the whole walk: per-row autocommit would fsync per
    // file. The walk sink runs on this thread, so the writes overlap the I/O.
    let tx = idx.conn.unchecked_transaction().map_err(db_err)?;
    let loc: i64 = tx
        .query_row(
            "INSERT INTO locations (uuid, root, online, last_seen, cow) VALUES (?1, ?2, 1, ?3, ?4) \
             ON CONFLICT(uuid) DO UPDATE SET root = excluded.root, online = 1, \
             last_seen = excluded.last_seen, cow = excluded.cow RETURNING id",
            params![vol.key, mount, now_secs(), (probe.cow)(&vol.mount_point)],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    // The folder given, so `apply` can quarantine inside it.
    tx.execute(
        "INSERT OR IGNORE INTO scan_roots (location_id, path) VALUES (?1, ?2)",
        params![loc, prefix],
    )
    .map_err(db_err)?;
    let gen: i64 = tx
        .query_row(
            "UPDATE meta SET value = value + 1 WHERE key = 'scan_generation' RETURNING value",
            [],
            |r| r.get(0),
        )
        .map_err(db_err)?;

    // A location is one volume: never cross into another mount. The MFT
    // engine is Windows-only and yields no inode identity.
    let walk_opts = Options {
        same_file_system: true,
        mft: false,
        ..opts.clone()
    };
    let mut failed: Option<rusqlite::Error> = None;
    // Folders listed but not entered: other filesystems below the root.
    let mut bound: Vec<String> = Vec::new();
    {
        let mut get = tx
            .prepare_cached(
                "SELECT size, mtime_ns, ino FROM files WHERE location_id = ?1 AND path = ?2",
            )
            .map_err(db_err)?;
        let mut insert = tx
            .prepare_cached(
                "INSERT INTO files (location_id, path, size, mtime_ns, dev, ino, seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(db_err)?;
        let mut touch = tx
            .prepare_cached(
                "UPDATE files SET seen = ?3, dev = ?4 WHERE location_id = ?1 AND path = ?2",
            )
            .map_err(db_err)?;
        let mut reset = tx
            .prepare_cached(
                "UPDATE files SET size = ?3, mtime_ns = ?4, dev = ?5, ino = ?6, \
                 sample = NULL, full = NULL, seen = ?7 WHERE location_id = ?1 AND path = ?2",
            )
            .map_err(db_err)?;
        let follow = walk_opts.follow_symlinks;
        let note_skip = |sum: &mut ScanSummary, path: &Path| {
            sum.skipped += 1;
            if sum.skipped_paths.len() < 32 {
                sum.skipped_paths.push(path.to_string_lossy().into_owned());
            }
        };
        let mut record = |path: &Path, sum: &mut ScanSummary| -> rusqlite::Result<()> {
            let meta = if follow {
                std::fs::metadata(path)
            } else {
                std::fs::symlink_metadata(path)
            };
            let Ok(meta) = meta else {
                note_skip(sum, path);
                return Ok(());
            };
            let st = stat_of(&meta);
            let rel = path
                .strip_prefix(&vol.mount_point)
                .ok()
                .and_then(Path::to_str);
            // A followed link may lead off the volume; a non-UTF-8 name cannot
            // be stored losslessly in a TEXT column. Neither is indexed.
            let (Some(rel), true) = (rel, st.dev == root_dev) else {
                note_skip(sum, path);
                return Ok(());
            };
            sum.seen += 1;
            let old: Option<(i64, i64, i64)> = get
                .query_row(params![loc, rel], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            let (size, mtime, dev, ino) = (
                to_sql(st.size),
                to_sql(st.mtime_ns),
                to_sql(st.dev),
                to_sql(st.ino),
            );
            match old {
                None => {
                    insert.execute(params![loc, rel, size, mtime, dev, ino, gen])?;
                    sum.new += 1;
                }
                Some(o) if o == (size, mtime, ino) => {
                    touch.execute(params![loc, rel, gen, dev])?;
                    sum.unchanged += 1;
                }
                Some(_) => {
                    reset.execute(params![loc, rel, size, mtime, dev, ino, gen])?;
                    sum.changed += 1;
                }
            }
            Ok(())
        };
        let mut unreadable = 0u64;
        let _ = walk(&root, &walk_opts, progress, |b| {
            unreadable += b.skipped;
            for (p, _) in &b.skips {
                if sum.skipped_paths.len() < 32 {
                    sum.skipped_paths.push(p.to_string_lossy().into_owned());
                }
            }
            for dir in &b.bound {
                match dir
                    .strip_prefix(&vol.mount_point)
                    .ok()
                    .and_then(Path::to_str)
                {
                    Some(rel) => bound.push(rel.to_string()),
                    // Cannot be protected by name: keep every row instead.
                    None => {
                        unreadable += 1;
                        if sum.skipped_paths.len() < 32 {
                            sum.skipped_paths.push(dir.to_string_lossy().into_owned());
                        }
                    }
                }
            }
            if failed.is_some() {
                return;
            }
            // Every name of a hard-linked file is a row: the report needs
            // them all to tell one physical copy from several.
            for p in b.files.iter().map(|f| &f.path).chain(&b.aliases) {
                if let Err(e) = record(p, &mut sum) {
                    failed = Some(e);
                    progress.cancel(); // stop the walk; the error is reported
                    return;
                }
            }
        });
        if let Some(e) = failed.take() {
            return Err(db_err(e)); // the transaction rolls back on drop
        }
        sum.skipped += unreadable;
        sum.complete = !progress.cancelled() && unreadable == 0 && opts.max_depth.is_none();
    }

    // Vanished: rows under the walked subtree this walk did not see. Only
    // after a complete walk — a folder that could not be read is not gone —
    // and never under a mount the walk listed without entering (a nested
    // btrfs subvolume shares this location but is not walked from here).
    if sum.complete {
        sum.vanished = drop_vanished(&tx, loc, gen, &prefix, &bound).map_err(db_err)? as u64;
    }
    tx.commit().map_err(db_err)?;

    if !progress.cancelled() {
        sum.sampled = sample_collisions(idx, progress, probe)?;
    }
    Ok(sum)
}

/// Drop rows of location `loc` under `prefix` that scan generation `gen`
/// did not see, except under `bound` (relative paths the walk listed but did
/// not enter). Index rows only.
fn drop_vanished(
    conn: &Connection,
    loc: i64,
    gen: i64,
    prefix: &str,
    bound: &[String],
) -> rusqlite::Result<usize> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS bound (p TEXT PRIMARY KEY); DELETE FROM temp.bound;",
    )?;
    {
        let mut add = conn.prepare_cached("INSERT OR IGNORE INTO temp.bound (p) VALUES (?1)")?;
        for b in bound {
            add.execute([b])?;
        }
    }
    // Prefix match by `substr`, not LIKE: `_` and `%` occur in file names.
    conn.execute(
        "DELETE FROM files WHERE location_id = ?1 AND seen <> ?2 \
         AND (?3 = '' OR path = ?3 OR substr(path, 1, length(?3) + 1) = ?3 || '/') \
         AND NOT EXISTS (SELECT 1 FROM temp.bound b WHERE files.path = b.p \
                         OR substr(files.path, 1, length(b.p) + 1) = b.p || '/')",
        params![loc, gen, prefix],
    )
}

/// A row whose content has to be sampled.
struct Pending {
    location_id: i64,
    path: String,
    abs: PathBuf,
    size: i64,
    mtime_ns: i64,
    ino: i64,
}

/// Read the two ends of every indexed file that has no sample yet and whose
/// size collides with another indexed file (same size = potential copy),
/// on every location that is online now. Each file is re-`stat`ed first and
/// only read while it still matches its row; anything else waits for the
/// next scan of its volume. Returns how many files were sampled.
fn sample_collisions(idx: &Index, progress: &Progress, probe: &Probe) -> io::Result<u64> {
    let mut pending = Vec::new();
    {
        let mut st = idx
            .conn
            .prepare(
                "SELECT l.id, l.uuid, l.root, f.path, f.size, f.mtime_ns, f.ino \
                 FROM files f JOIN locations l ON l.id = f.location_id \
                 WHERE l.host NOT LIKE 'ssh://%' \
                   AND f.sample IS NULL AND f.size > 0 AND f.size IN \
                   (SELECT size FROM files WHERE size > 0 GROUP BY size HAVING COUNT(*) > 1) \
                 ORDER BY l.id, f.path",
            )
            .map_err(db_err)?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })
            .map_err(db_err)?;
        let mut online: HashMap<i64, bool> = HashMap::new();
        for row in rows {
            let (location_id, uuid, root, path, size, mtime_ns, ino) = row.map_err(db_err)?;
            let root = PathBuf::from(root);
            if !*online
                .entry(location_id)
                .or_insert_with(|| (probe.online)(&root, &uuid))
            {
                continue; // an offline volume keeps its NULLs until it is back
            }
            pending.push(Pending {
                location_id,
                abs: root.join(&path),
                path,
                size,
                mtime_ns,
                ino,
            });
        }
    }
    if pending.is_empty() {
        return Ok(0);
    }
    let bytes = pending
        .iter()
        .map(|p| (p.size as u64).min(2 * 16 * 1024))
        .sum();
    progress.enter(phase::SAMPLE, pending.len() as u64, bytes);
    let read: Vec<(&Pending, Sampled)> = pending
        .par_iter()
        .filter_map(|p| {
            if progress.cancelled() {
                return None;
            }
            // Still the file the row describes? Otherwise leave it NULL.
            let st = stat_of(&std::fs::metadata(&p.abs).ok()?);
            if (to_sql(st.size), to_sql(st.mtime_ns), to_sql(st.ino)) != (p.size, p.mtime_ns, p.ino)
            {
                return None;
            }
            (probe.on_read)(&p.abs);
            let sampled = sample_file(&p.abs, st.size)?;
            progress.files.fetch_add(1, Ordering::Relaxed);
            progress
                .bytes
                .fetch_add(st.size.min(2 * 16 * 1024), Ordering::Relaxed);
            Some((p, sampled))
        })
        .collect();
    let tx = idx.conn.unchecked_transaction().map_err(db_err)?;
    let mut n = 0;
    {
        // The row must still be the one that was read (a concurrent scan may
        // have reset it meanwhile).
        let mut set = tx
            .prepare_cached(
                "UPDATE files SET sample = ?3, full = COALESCE(?4, full) \
                 WHERE location_id = ?1 AND path = ?2 AND size = ?5 AND mtime_ns = ?6 \
                 AND ino = ?7",
            )
            .map_err(db_err)?;
        for (p, s) in &read {
            n += set
                .execute(params![
                    p.location_id,
                    p.path,
                    &s.sample[..],
                    s.full.as_ref().map(|f| &f[..]),
                    p.size,
                    p.mtime_ns,
                    p.ino
                ])
                .map_err(db_err)? as u64;
        }
    }
    tx.commit().map_err(db_err)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::parse_mountinfo_entry;

    // ---- fixtures -------------------------------------------------------

    /// A fresh directory owned by this test alone. Tests create and delete
    /// files only inside it — never anywhere else (owner's hard safety rule).
    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-index-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    fn open_db(dir: &Path) -> Index {
        Index::open(&dir.join("db").join("index.db")).unwrap()
    }

    fn row_count(idx: &Index) -> i64 {
        idx.conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap()
    }

    // ---- volume identity (pure, fixture mountinfo) ----------------------

    const MOUNTINFO: &str = "\
35 2 0:30 / / rw,relatime shared:1 - btrfs /dev/nvme1n1p2 rw,ssd,subvolid=5,subvol=/
36 35 0:44 /@home /home rw,relatime shared:2 - btrfs /dev/nvme1n1p2 rw,subvolid=257,subvol=/@home
31 35 0:27 / /run rw,nosuid,nodev - tmpfs tmpfs rw,size=13179404k
2309 35 8:2 / /media/u/Big\\040Disk\\0401 rw,nosuid,nodev,relatime shared:1706 - exfat /dev/sda2 rw
2702 35 8:3 / /media/u/usb_stick rw,relatime shared:1858 - ext4 /dev/sda3 rw
2703 35 8:3 /data /srv/data rw,relatime - ext4 /dev/sda3 rw
2840 35 0:85 / /var/lib/docker/merged rw,relatime - overlay overlay rw,lowerdir=/x
";

    fn mounts() -> Vec<MountEntry> {
        MOUNTINFO
            .lines()
            .filter_map(parse_mountinfo_entry)
            .collect()
    }

    fn by_uuid(source: &str) -> Option<String> {
        match source {
            "/dev/nvme1n1p2" => Some("1111-root".into()),
            "/dev/sda2" => Some("2222-BIG".into()),
            "/dev/sda3" => Some("3333-portable".into()),
            _ => None,
        }
    }

    #[test]
    fn mountinfo_entry_decodes_octal_escapes() {
        let m = mounts();
        let x9 = m.iter().find(|e| e.source == "/dev/sda2").unwrap();
        assert_eq!(x9.mount_point, PathBuf::from("/media/u/Big Disk 1"));
        assert_eq!((x9.major, x9.minor), (8, 2));
        assert_eq!(x9.fstype, "exfat");
        assert_eq!(x9.root, "/");
    }

    #[test]
    fn plain_mount_resolves_to_filesystem_uuid() {
        let dev = crate::walk::makedev(8, 2);
        let v = resolve_volume(
            &mounts(),
            dev,
            Path::new("/media/u/Big Disk 1/photos/a.jpg"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(v.key, "2222-BIG");
        assert_eq!(v.mount_point, PathBuf::from("/media/u/Big Disk 1"));
    }

    #[test]
    fn bind_mount_or_subvolume_keeps_its_filesystem_root_in_the_key() {
        // Same filesystem, different visible subtree: the files under /srv/data
        // are not the files under /media/u/usb_stick, so the key differs.
        let dev = crate::walk::makedev(8, 3);
        let v = resolve_volume(&mounts(), dev, Path::new("/srv/data/x"), &by_uuid).unwrap();
        assert_eq!(v.key, "3333-portable:/data");
        assert_eq!(v.mount_point, PathBuf::from("/srv/data"));
        let home = resolve_volume(
            &mounts(),
            crate::walk::makedev(0, 44),
            Path::new("/home/u/f"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(home.key, "1111-root:/@home");
    }

    #[test]
    fn pseudo_filesystems_fall_back_to_the_device_number() {
        let v = resolve_volume(
            &mounts(),
            crate::walk::makedev(0, 27),
            Path::new("/run/x"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(v.key, "dev:0:27");
        assert_eq!(v.mount_point, PathBuf::from("/run"));
        let o = resolve_volume(
            &mounts(),
            crate::walk::makedev(0, 85),
            Path::new("/var/lib/docker/merged/etc"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(o.key, "dev:0:85");
    }

    #[test]
    fn btrfs_nested_subvolume_belongs_to_the_enclosing_mount() {
        // `stat` on a nested btrfs subvolume reports an anonymous device that
        // mountinfo never lists; the volume is still the enclosing filesystem.
        let v = resolve_volume(
            &mounts(),
            crate::walk::makedev(0, 31),
            Path::new("/tmp/x"),
            &by_uuid,
        )
        .unwrap();
        assert_eq!(v.key, "1111-root");
        assert_eq!(v.mount_point, PathBuf::from("/"));
    }

    #[test]
    fn unknown_device_on_a_non_btrfs_mount_is_not_guessed() {
        assert_eq!(
            resolve_volume(
                &mounts(),
                crate::walk::makedev(9, 9),
                Path::new("/media/u/usb_stick/x"),
                &by_uuid
            ),
            None
        );
    }

    #[test]
    fn cow_filesystem_magics() {
        for m in [
            0x9123_683e,
            0x2fc1_2fc1,
            0x5846_5342,
            0xca45_1a4e,
            0x794c_7630,
        ] {
            assert!(is_cow_magic(m), "{m:#x}");
        }
        for m in [0xef53, 0x0102_1994, 0x6969, 0x5346_544e] {
            assert!(!is_cow_magic(m), "{m:#x}");
        }
    }

    // ---- schema ---------------------------------------------------------

    #[test]
    fn a_version_2_index_migrates_to_3_keeping_its_rows() {
        let d = tmpdir("migrate-v2");
        let path = d.join("db/index.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(MIGRATIONS[0]).unwrap();
            c.execute_batch(MIGRATIONS[1]).unwrap();
            c.pragma_update(None, "user_version", 2).unwrap();
            c.execute("UPDATE meta SET value = 2 WHERE key = 'schema_version'", [])
                .unwrap();
        }
        {
            let idx = Index::open(&path).unwrap();
            insert_location(&idx, "old-vol", "/nowhere-old");
        }
        // Downgrade the version mark again to replay v2 → v3 on real rows.
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("DROP TABLE IF EXISTS scan_roots").unwrap();
            // v5 added a column: take it away again, as v2 had none.
            c.execute_batch("ALTER TABLE locations DROP COLUMN transport")
                .unwrap();
            c.pragma_update(None, "user_version", 2).unwrap();
        }
        let idx = Index::open(&path).unwrap();
        let v: i64 = idx
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let mv: i64 = idx
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mv, SCHEMA_VERSION);
        assert_eq!(row_count(&idx), 3, "files survive the migration");
        let st = idx.status().unwrap();
        assert_eq!(st.len(), 1);
        assert!(st[0].roots.is_empty(), "an old index knows no scan roots");
        let n: i64 = idx
            .conn
            .query_row("SELECT COUNT(*) FROM scan_roots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let d = tmpdir("newer");
        drop(open_db(&d));
        let c = Connection::open(d.join("db/index.db")).unwrap();
        c.pragma_update(None, "user_version", 99).unwrap();
        drop(c);
        assert!(Index::open(&d.join("db/index.db")).is_err());
    }

    // ---- scanning -------------------------------------------------------

    #[test]
    fn rows_under_unentered_mounts_are_not_vanished() {
        // A nested btrfs subvolume (or any mount the walk lists but does not
        // enter) shares the location of its parent on this design: its rows
        // were not seen because they were not walked, not because they are
        // gone. Also: `a/mm` is no child of `a/m`.
        let d = tmpdir("bound");
        let idx = open_db(&d);
        insert_location(&idx, "vol", "/nowhere");
        let loc: i64 = idx
            .conn
            .query_row("SELECT id FROM locations WHERE uuid = 'vol'", [], |r| {
                r.get(0)
            })
            .unwrap();
        idx.conn.execute("DELETE FROM files", []).unwrap();
        for (i, p) in [
            "a/x",
            "a/m",
            "a/m/y",
            "a/m/deep/z",
            "a/mm/z",
            "b/q",
            "a/seen",
        ]
        .iter()
        .enumerate()
        {
            let seen = if *p == "a/seen" { 2 } else { 1 };
            idx.conn
                .execute(
                    "INSERT INTO files (location_id, path, size, mtime_ns, dev, ino, seen) \
                     VALUES (?1, ?2, 1, 0, 0, ?3, ?4)",
                    rusqlite::params![loc, p, i as i64, seen],
                )
                .unwrap();
        }
        let n = drop_vanished(&idx.conn, loc, 2, "a", &["a/m".to_string()]).unwrap();
        let mut left: Vec<String> = idx
            .conn
            .prepare("SELECT path FROM files ORDER BY path")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        left.sort();
        assert_eq!(left, ["a/m", "a/m/deep/z", "a/m/y", "a/seen", "b/q"]);
        assert_eq!(n, 2, "a/x and a/mm/z are gone");
    }

    // ---- locations ------------------------------------------------------

    fn insert_location(idx: &Index, uuid: &str, root: &str) {
        idx.conn
            .execute(
                "INSERT INTO locations (uuid, root, online, last_seen) VALUES (?1, ?2, 1, 1)",
                rusqlite::params![uuid, root],
            )
            .unwrap();
        let id = idx.conn.last_insert_rowid();
        for (i, size) in [10i64, 20, 30].into_iter().enumerate() {
            idx.conn
                .execute(
                    "INSERT INTO files (location_id, path, size, mtime_ns, dev, ino, seen) \
                     VALUES (?1, ?2, ?3, 0, 0, ?4, 1)",
                    rusqlite::params![id, format!("f{i}"), size, i as i64],
                )
                .unwrap();
        }
    }

    #[test]
    fn unplugged_volume_is_offline_and_keeps_its_rows() {
        let d = tmpdir("offline");
        let idx = open_db(&d);
        insert_location(&idx, "dead-beef", &d.join("not-mounted").to_string_lossy());
        let st = idx.status().unwrap();
        let l = st.iter().find(|l| l.uuid == "dead-beef").unwrap();
        assert!(!l.online);
        assert_eq!((l.files, l.bytes), (3, 60));
        assert_eq!(row_count(&idx), 3, "status never deletes");
    }

    #[test]
    fn an_existing_mountpoint_of_another_volume_is_offline() {
        // An unplugged disk leaves its (empty) mountpoint directory behind.
        let d = tmpdir("emptymp");
        let idx = open_db(&d);
        insert_location(&idx, "not-this-volume", &d.to_string_lossy());
        let st = idx.status().unwrap();
        assert!(!st[0].online);
    }

    #[test]
    fn forget_drops_the_location_and_cascades_to_its_files() {
        let d = tmpdir("forget");
        let idx = open_db(&d);
        insert_location(&idx, "a-uuid", "/nowhere-a");
        insert_location(&idx, "b-uuid", "/nowhere-b");
        assert!(idx.forget("a-uuid").unwrap());
        assert!(!idx.forget("a-uuid").unwrap());
        assert_eq!(row_count(&idx), 3);
        assert_eq!(idx.status().unwrap().len(), 1);
    }

    #[test]
    fn find_files_matches_substrings_literally_and_pages() {
        let d = tmpdir("find");
        let idx = open_db(&d);
        insert_location(&idx, "a-uuid", "/data");
        insert_location(&idx, "b-uuid", "/database");
        let id_a: i64 = idx
            .conn
            .query_row("SELECT id FROM locations WHERE uuid = 'a-uuid'", [], |r| {
                r.get(0)
            })
            .unwrap();
        for p in ["Photos/IMG_1.JPG", "photos/img%1.jpg", "docs/x.txt"] {
            idx.conn
                .execute(
                    "INSERT INTO files (location_id, path, size, mtime_ns, dev, ino, seen) \
                     VALUES (?1, ?2, 5, 0, 0, 99, 1)",
                    rusqlite::params![id_a, p],
                )
                .unwrap();
        }
        let paths = |pat: &str, loc: Option<&str>, limit: usize, offset: usize| -> Vec<String> {
            idx.find_files(pat, loc, limit, offset)
                .unwrap()
                .into_iter()
                .map(|f| f.path.to_string_lossy().into_owned())
                .collect()
        };
        // Case-insensitive substring over root/relative.
        assert_eq!(
            paths("photos/", None, 10, 0),
            ["/data/Photos/IMG_1.JPG", "/data/photos/img%1.jpg"]
        );
        // `%` and `_` are literal, not wildcards.
        assert_eq!(paths("img%1", None, 10, 0), ["/data/photos/img%1.jpg"]);
        assert_eq!(paths("img_1", None, 10, 0), ["/data/Photos/IMG_1.JPG"]);
        assert!(paths("i_g", None, 10, 0).is_empty());
        // Every location has f0..f2: six hits, paged two at a time.
        assert_eq!(paths("/f", None, 10, 0).len(), 6);
        assert_eq!(paths("/f", None, 2, 0), ["/data/f0", "/data/f1"]);
        assert_eq!(paths("/f", None, 2, 4), ["/database/f1", "/database/f2"]);
        // A uuid or a folder restricts; `/data` is not a prefix of `/database`.
        assert_eq!(paths("/f", Some("b-uuid"), 10, 0).len(), 3);
        assert_eq!(paths("/f", Some("/data"), 10, 0).len(), 3);
        assert_eq!(paths("/f", Some("/data/"), 10, 0).len(), 3);
        assert_eq!(paths("", Some("/data/docs"), 10, 0), ["/data/docs/x.txt"]);
        assert!(paths("/f", Some("nope"), 10, 0).is_empty());
        let f = &idx.find_files("x.txt", None, 1, 0).unwrap()[0];
        assert_eq!(
            (f.location_uuid.as_str(), f.root.as_path(), f.size, f.hashed),
            ("a-uuid", Path::new("/data"), 5, false)
        );
    }

    #[test]
    fn configure_sets_role_and_host() {
        let d = tmpdir("configure");
        let idx = open_db(&d);
        insert_location(&idx, "c-uuid", "/nowhere-c");
        idx.configure("c-uuid", Some(Role::Master), "localhost")
            .unwrap();
        let l = &idx.status().unwrap()[0];
        assert_eq!((l.role, l.host.as_str()), (Role::Master, "localhost"));
        idx.configure("c-uuid", None, "local").unwrap();
        assert_eq!(idx.status().unwrap()[0].role, Role::Master);
        assert!(idx.configure("missing", None, "local").is_err());
    }

    #[test]
    fn only_local_and_ssh_hosts_are_accepted() {
        assert!(validate_host("local").is_ok());
        assert!(validate_host("localhost").is_ok());
        let e = validate_host("nas.lan").unwrap_err();
        assert!(e.contains("ssh://nas.lan/path"), "{e}");
        assert!(validate_host("ssh://nas.lan").is_ok());
    }

    /// Unix-only cases: `scan()`/`scan_observed()` run the real `volume()`
    /// probe, which answers `Unsupported` on non-unix (the persistent
    /// index needs a Unix volume table) — the whole group sits behind
    /// one `cfg` instead of a gate per test or helper.
    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::collections::BTreeSet;
        use std::sync::Mutex;
        use std::time::{Duration, SystemTime};

        fn write(path: &Path, content: &[u8]) {
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }

        /// Move a file's mtime to a time it cannot already have.
        fn touch(path: &Path, secs_ago: u64) {
            let f = std::fs::File::options().write(true).open(path).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
                .unwrap();
        }
        /// Scan `root`, recording every file whose content was read.
        fn scan(idx: &Index, root: &Path) -> (ScanSummary, BTreeSet<PathBuf>) {
            let reads = Mutex::new(BTreeSet::new());
            let s = scan_observed(
                idx,
                root,
                &Options::default(),
                &Progress::default(),
                &|p: &Path| {
                    reads.lock().unwrap().insert(p.to_path_buf());
                },
            )
            .unwrap();
            (s, reads.into_inner().unwrap())
        }
        /// (sample, full) of one indexed file, by absolute path.
        /// (sample, full) as stored.
        type Hashes = (Option<Vec<u8>>, Option<Vec<u8>>);

        fn hashes(idx: &Index, abs: &Path) -> Option<Hashes> {
            let mut st = idx
                .conn
                .prepare(
                    "SELECT f.sample, f.full, l.root, f.path FROM files f \
                     JOIN locations l ON l.id = f.location_id",
                )
                .unwrap();
            let rows = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, Option<Vec<u8>>>(0)?,
                        r.get::<_, Option<Vec<u8>>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .unwrap();
            for row in rows {
                let (s, f, root, path) = row.unwrap();
                if Path::new(&root).join(path) == abs {
                    return Some((s, f));
                }
            }
            None
        }
        #[test]
        fn live_volume_uuid_and_cow_probe_do_not_fail() {
            let d = tmpdir("live");
            assert!(!volume_uuid(&d).unwrap().is_empty());
            let _ = is_cow_fs(&d);
        }
        #[test]
        fn schema_is_created_versioned_and_reopened() {
            let d = tmpdir("schema");
            let idx = open_db(&d);
            let v: i64 = idx
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, SCHEMA_VERSION);
            // Version 2 records whether a location's filesystem is copy-on-write.
            let cow: Option<i64> = idx
                .conn
                .query_row("SELECT cow FROM locations LIMIT 1", [], |r| r.get(0))
                .optional()
                .unwrap()
                .flatten();
            assert_eq!(cow, None, "no location yet");
            let mode: String = idx
                .conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            let fk: i64 = idx
                .conn
                .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                .unwrap();
            assert_eq!(fk, 1);
            write(&d.join("t/a"), b"x");
            scan(&idx, &d.join("t"));
            drop(idx);
            let idx = open_db(&d);
            assert_eq!(row_count(&idx), 1);
        }
        #[test]
        fn scans_record_their_roots_listed_by_status_and_dropped_by_forget() {
            let d = tmpdir("scanroots");
            write(&d.join("t/a/x"), b"x");
            write(&d.join("t/b/y"), b"y");
            let idx = open_db(&d);
            let s = scan(&idx, &d.join("t/a")).0;
            scan(&idx, &d.join("t/b"));
            // Re-adding a root records it once; a non-canonical spelling too.
            scan(&idx, &d.join("t/b/../a"));
            let st = idx.status().unwrap();
            let l = st.iter().find(|l| l.uuid == s.uuid).unwrap();
            let mut roots = l.roots.clone();
            roots.sort();
            assert_eq!(roots, vec![d.join("t/a"), d.join("t/b")]);
            // Stored relative to the mountpoint, like file paths.
            let stored: Vec<String> = {
                let mut q = idx
                    .conn
                    .prepare("SELECT path FROM scan_roots ORDER BY path")
                    .unwrap();
                q.query_map([], |r| r.get(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            };
            for p in &stored {
                assert!(!p.starts_with('/'), "{p}");
                assert_eq!(s.root.join(p).parent(), Some(d.join("t").as_path()));
            }
            assert_eq!(stored.len(), 2);
            assert!(idx.forget(&s.uuid).unwrap());
            let n: i64 = idx
                .conn
                .query_row("SELECT COUNT(*) FROM scan_roots", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "forget cascades to scan roots");
        }
        #[test]
        fn first_scan_of_unique_sizes_reads_nothing() {
            let d = tmpdir("unique");
            let t = d.join("t");
            write(&t.join("a"), b"1");
            write(&t.join("sub/b"), b"22");
            write(&t.join("c"), b"333");
            let idx = open_db(&d);
            let (s, reads) = scan(&idx, &t);
            assert!(
                reads.is_empty(),
                "nothing collides, nothing is read: {reads:?}"
            );
            assert_eq!((s.seen, s.new, s.sampled), (3, 3, 0));
            assert!(s.complete);
            for f in ["a", "sub/b", "c"] {
                assert_eq!(hashes(&idx, &t.join(f)), Some((None, None)), "{f}");
            }
        }
        #[test]
        fn an_unchanged_file_is_never_read_again() {
            let d = tmpdir("unchanged");
            let t = d.join("t");
            write(&t.join("a"), &[7u8; 40_000]);
            write(&t.join("b"), &[7u8; 40_000]);
            write(&t.join("c"), b"unique");
            let idx = open_db(&d);
            let (s1, r1) = scan(&idx, &t);
            assert_eq!(r1, BTreeSet::from([t.join("a"), t.join("b")]));
            assert_eq!(s1.sampled, 2);
            let (s2, r2) = scan(&idx, &t);
            assert!(r2.is_empty(), "nothing changed, nothing re-read: {r2:?}");
            assert_eq!((s2.unchanged, s2.new, s2.changed), (3, 0, 0));
        }
        #[test]
        fn touching_one_file_rereads_only_that_file() {
            let d = tmpdir("touch");
            let t = d.join("t");
            write(&t.join("a"), &[7u8; 40_000]);
            write(&t.join("b"), &[7u8; 40_000]);
            write(&t.join("c"), b"unique");
            let idx = open_db(&d);
            scan(&idx, &t);
            let b_before = hashes(&idx, &t.join("b")).unwrap();
            assert!(b_before.0.is_some());
            touch(&t.join("a"), 3600);
            let (s, reads) = scan(&idx, &t);
            assert_eq!(reads, BTreeSet::from([t.join("a")]));
            assert_eq!((s.changed, s.unchanged, s.sampled), (1, 2, 1));
            assert_eq!(hashes(&idx, &t.join("b")).unwrap(), b_before);
            let a = hashes(&idx, &t.join("a")).unwrap();
            assert_eq!(a.0, b_before.0, "same content, same sample");
        }
        #[test]
        fn a_copy_found_later_samples_both_sides() {
            let d = tmpdir("twotrees");
            let (ta, tb) = (d.join("a"), d.join("b"));
            write(&ta.join("movie"), &[9u8; 50_000]);
            write(&tb.join("movie copy"), &[9u8; 50_000]);
            write(&tb.join("unique"), &[1u8; 123]);
            let idx = open_db(&d);
            // Alone, A's file collides with nothing: not read.
            let (_, r1) = scan(&idx, &ta);
            assert!(r1.is_empty());
            assert_eq!(hashes(&idx, &ta.join("movie")), Some((None, None)));
            // B brings a same-size file: both sides get their sample.
            let (s, r2) = scan(&idx, &tb);
            assert_eq!(
                r2,
                BTreeSet::from([ta.join("movie"), tb.join("movie copy")])
            );
            assert_eq!(s.sampled, 2);
            let a = hashes(&idx, &ta.join("movie")).unwrap();
            let b = hashes(&idx, &tb.join("movie copy")).unwrap();
            assert!(a.0.is_some());
            assert_eq!(a.0, b.0);
            assert_eq!(hashes(&idx, &tb.join("unique")), Some((None, None)));
            // A's rows survive B's scan: vanished is scoped to the walked subtree.
            assert_eq!(row_count(&idx), 3);
        }
        #[test]
        fn small_samples_are_also_full_hashes() {
            let d = tmpdir("smallfull");
            let t = d.join("t");
            write(&t.join("a"), b"same bytes");
            write(&t.join("b"), b"same bytes");
            let idx = open_db(&d);
            scan(&idx, &t);
            let (s, f) = hashes(&idx, &t.join("a")).unwrap();
            assert_eq!(s, f);
            assert_eq!(f.unwrap(), blake3::hash(b"same bytes").as_bytes().to_vec());
        }
        #[test]
        fn rescan_marks_seen_and_drops_vanished_rows() {
            let d = tmpdir("vanish");
            let t = d.join("t");
            write(&t.join("keep"), b"k");
            write(&t.join("gone"), b"gg");
            let idx = open_db(&d);
            scan(&idx, &t);
            std::fs::remove_file(t.join("gone")).unwrap(); // inside own tempdir
            let (s, _) = scan(&idx, &t);
            assert_eq!((s.vanished, s.unchanged), (1, 1));
            assert_eq!(hashes(&idx, &t.join("gone")), None);
            let (gen, seen): (i64, i64) = idx
                .conn
                .query_row(
                    "SELECT (SELECT value FROM meta WHERE key = 'scan_generation'), \
                     (SELECT MAX(seen) FROM files)",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(gen, seen, "the surviving row is marked seen by this scan");
        }
        #[test]
        fn an_unreadable_dir_is_named_in_the_summary() {
            use std::os::unix::fs::PermissionsExt;
            let d = tmpdir("unreadable");
            let t = d.join("t");
            write(&t.join("a"), b"a");
            write(&t.join("locked").join("inside"), b"x");
            std::fs::set_permissions(t.join("locked"), std::fs::Permissions::from_mode(0o0))
                .unwrap();
            let readable = std::fs::read_dir(t.join("locked")).is_ok(); // running as root
            let idx = open_db(&d);
            let (s, _) = scan(&idx, &t);
            std::fs::set_permissions(t.join("locked"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            if readable {
                return;
            }
            assert!(!s.complete);
            assert!(s.skipped > 0);
            assert!(
                s.skipped_paths.iter().any(|p| p.ends_with("locked")),
                "the skipped dir is named: {:?}",
                s.skipped_paths
            );
        }
        #[test]
        fn an_incomplete_scan_deletes_nothing() {
            let d = tmpdir("cancelled");
            let t = d.join("t");
            write(&t.join("a"), b"a");
            write(&t.join("b"), b"bb");
            let idx = open_db(&d);
            scan(&idx, &t);
            let p = Progress::default();
            p.cancel();
            let s = scan_observed(&idx, &t, &Options::default(), &p, &|_| {}).unwrap();
            assert!(!s.complete);
            assert_eq!(s.vanished, 0);
            assert_eq!(row_count(&idx), 2);
        }
        #[test]
        fn a_scanned_volume_is_online() {
            let d = tmpdir("online");
            write(&d.join("t/a"), b"a");
            let idx = open_db(&d);
            let s = scan(&idx, &d.join("t")).0;
            let st = idx.status().unwrap();
            let l = st.iter().find(|l| l.uuid == s.uuid).unwrap();
            assert!(l.online);
            assert_eq!((l.files, l.bytes, l.role), (1, 1, Role::Normal));
            assert_eq!(l.host, "local");
        }
    }
}
