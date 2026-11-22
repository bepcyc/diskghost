//! Duplicate and single-copy reports built from the persistent index — over
//! every registered volume, the unplugged ones included — instead of from a
//! fresh walk.
//!
//! The report completes hashes lazily and writes them back: a file whose
//! size collides is sampled (both ends) and, while its sample still matches
//! another file's, hashed whole — once; every later report reads nothing.
//! Files on offline volumes are never read; their stored hashes count.
//!
//! Copies are *physical*: names sharing one inode (hard links, a second
//! mount of the same filesystem) are one copy, listed with its other names.
//! Offline copies count but are never suggested for any action.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rayon::prelude::*;
use rusqlite::params;
use serde::Serialize;

use crate::dupes::{full_hash_with, sample_file};
use crate::index::{db_err, stat_of, to_sql, Index, Probe, Role};
use crate::rotational::ROTATIONAL_HASH_THREADS;
use crate::{phase, Progress};

/// Bytes [`sample_file`] reads from a file of `size` at most.
const SAMPLE_READ: u64 = 2 * 16 * 1024;

/// What the report should cover.
#[derive(Debug, Clone, Default)]
pub struct IndexReportOptions {
    /// Ignore files smaller than this (bytes). Empty files never count.
    pub min_size: u64,
    /// Add the per-location single-copy section.
    pub singles: bool,
    /// List single-copy paths too (otherwise counts and bytes only).
    pub singles_paths: bool,
}

/// `dupes --index`: duplicate groups across all locations, optionally the
/// single-copy files, plus what the run had to read.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexDupesReport {
    pub groups: Vec<IndexGroup>,
    /// Per location, files with exactly one physical copy in the estate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub singles: Option<Vec<LocationSingles>>,
    pub locations: Vec<ReportLocation>,
    pub stats: HashStats,
    /// Problems that did not stop the report (missing or unreadable files).
    pub warnings: Vec<String>,
    /// Same-size files whose digests were made with different algorithms
    /// (BLAKE3 here, MD5 or SHA-1 on a remote host without `b3sum`): whether
    /// they are copies of each other cannot be told — never a group.
    pub unverifiable: Vec<Unverifiable>,
}

/// Same-size files hashed with different algorithms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unverifiable {
    pub size: u64,
    /// Algorithms present, sorted (`blake3`, `md5`, `sha1`).
    pub algos: Vec<String>,
    pub copies: Vec<UnverifiableCopy>,
}

/// One file of an [`Unverifiable`] set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnverifiableCopy {
    pub location_uuid: String,
    pub path: PathBuf,
    pub algo: String,
    /// Its whole-file digest, hex, when known.
    pub hash: Option<String>,
    pub available: bool,
}

/// Files of one content (size + whole-file hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexGroup {
    /// Whole-file digest, hex (`algo`). `None` when no copy has one yet
    /// (all members are offline and only their ends were ever read).
    pub hash: Option<String>,
    /// The algorithm of `hash`: `blake3`, or `md5` / `sha1` for a group of
    /// remote files hashed by a host without `b3sum`.
    pub algo: String,
    /// BLAKE3 of both ends, hex, when known.
    pub sample: Option<String>,
    pub size: u64,
    /// One entry per physical copy.
    pub copies: Vec<GroupCopy>,
    /// Confirmed physical copies (same whole-file hash), offline included.
    pub unique_copies: u64,
    /// Of those, reachable now.
    pub available_copies: u64,
    /// Copies only probably alike: offline, whole file never hashed.
    pub probable_copies: u64,
    /// `size × (unique_copies − 1)`: what one copy per content would free.
    pub reclaimable_bytes: u64,
    /// `size × (available_copies − 1)`: the part reachable now, without
    /// relying on any offline copy.
    pub actionable_bytes: u64,
    /// A copy lives on a copy-on-write filesystem (or one whose kind is not
    /// known): copies may already share their blocks, so deleting one may
    /// free nothing.
    pub savings_may_be_shared: bool,
}

/// One physical copy in a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupCopy {
    pub location_uuid: String,
    /// The copy's name (absolute, under the location's last known root).
    pub path: PathBuf,
    /// Its volume is reachable now and the file is there.
    pub available: bool,
    /// Same size (and ends, when read) but whole file never hashed.
    pub probable: bool,
    /// Further names of this very inode: removing `path` alone frees nothing.
    pub shared_inodes: Vec<PathBuf>,
    /// What to do with it; `None` for offline and probable copies — they
    /// are never suggested for any action.
    pub suggest: Option<Suggest>,
}

/// An action suggestion for one available, confirmed copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Suggest {
    /// The copy to keep (a `master` location first, a `scratch` one last).
    Keep,
    /// A redundant copy: all its names would go.
    Reclaimable,
}

/// Single-copy files of one location: the under-protected ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocationSingles {
    pub location_uuid: String,
    pub root: PathBuf,
    pub available: bool,
    pub files: u64,
    pub bytes: u64,
    /// Only with [`IndexReportOptions::singles_paths`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<PathBuf>>,
}

/// A location as the report saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportLocation {
    pub uuid: String,
    pub root: PathBuf,
    pub available: bool,
    pub rotational: bool,
    pub cow: bool,
}

/// What this run read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct HashStats {
    /// Files whose two ends were read.
    pub sampled: u64,
    /// Files hashed whole.
    pub full_hashed: u64,
    /// Of those read, files that had changed since their row.
    pub changed: u64,
    /// Indexed files on an online volume that are gone (left out).
    pub missing: u64,
    /// Bytes read for hashing.
    pub bytes_read: u64,
}

/// Build the duplicate report from the index at `idx`.
pub fn index_dupes(
    idx: &Index,
    opts: &IndexReportOptions,
    progress: &Progress,
) -> io::Result<IndexDupesReport> {
    report_with(idx, opts, progress, &Probe::REAL)
}

/// [`index_dupes`] asking `probe` instead of the machine.
pub(crate) fn report_with(
    idx: &Index,
    opts: &IndexReportOptions,
    progress: &Progress,
    probe: &Probe,
) -> io::Result<IndexDupesReport> {
    let min = opts.min_size.max(1);
    let c = compute_groups(idx, min, progress, probe)?;
    let mut groups: Vec<IndexGroup> = c
        .drafts
        .iter()
        .map(|g| render(&c.locs, &c.rows, &c.phys, g))
        .collect();
    groups.sort_by(|a, b| {
        b.reclaimable_bytes
            .cmp(&a.reclaimable_bytes)
            .then(b.size.cmp(&a.size))
            .then_with(|| a.hash.cmp(&b.hash))
            .then_with(|| a.copies[0].path.cmp(&b.copies[0].path))
    });

    let singles = if opts.singles {
        Some(singles(
            idx,
            min,
            opts.singles_paths,
            &c.locs,
            &c.rows,
            &c.phys,
            &c.grouped,
            &c.at,
        )?)
    } else {
        None
    };

    Ok(IndexDupesReport {
        groups,
        singles,
        locations: c
            .locs
            .iter()
            .map(|l| ReportLocation {
                uuid: l.uuid.clone(),
                root: l.root.clone(),
                available: l.available,
                rotational: l.rotational,
                cow: l.cow,
            })
            .collect(),
        stats: c.stats,
        warnings: c.warnings,
        unverifiable: Vec::new(),
    })
}

/// The duplicate groups of the whole index, before any rendering: what the
/// `dupes --index` report and `plan` are both built from.
pub(crate) struct Groups {
    pub(crate) locs: Vec<Loc>,
    pub(crate) rows: Vec<Row>,
    pub(crate) phys: Vec<Phys>,
    /// One per group, in size order.
    pub(crate) drafts: Vec<Draft>,
    /// Physical files that are in some group.
    pub(crate) grouped: HashSet<usize>,
    /// Location row id → index into `locs`.
    pub(crate) at: HashMap<i64, usize>,
    pub(crate) stats: HashStats,
    pub(crate) warnings: Vec<String>,
}

/// Complete the hashes the groups need (writing them back to the index) and
/// form the groups of every file of at least `min` bytes.
pub(crate) fn compute_groups(
    idx: &Index,
    min: u64,
    progress: &Progress,
    probe: &Probe,
) -> io::Result<Groups> {
    let locs = load_locations(idx, probe)?;
    let at: HashMap<i64, usize> = locs.iter().enumerate().map(|(i, l)| (l.id, i)).collect();
    let mut rows = load_colliding(idx, min, &at)?;
    let mut warnings = Vec::new();
    let stats = Counters::default();

    // 1. Every candidate on a reachable volume: still the file of its row?
    // Re-read what changed, read the ends of what was never sampled.
    refresh(&locs, &mut rows, &stats, progress, probe);
    persist(idx, &locs, &rows)?;
    if progress.cancelled() {
        return Err(cancelled());
    }

    // 2. Physical files: names of one inode are one copy.
    let mut phys = physical(&locs, &rows);
    let buckets = by_size(&phys);

    // 3. Whole-file hashes, only where a copy is still possible.
    // Planned files only — all reachable, so no two share a path (an
    // offline volume's last root may well be a reachable one's mountpoint:
    // its rows must never take a hash read from another disk).
    let mut items = Vec::new();
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    for members in buckets.values() {
        for &p in members {
            if needs_full(&phys, members, p) {
                let r = &rows[phys[p].rows[0]];
                let path = abs(&locs[r.loc].root, &r.path);
                if by_path.insert(path.clone(), p).is_none() {
                    items.push(HashItem {
                        path,
                        size: phys[p].size,
                        rotational: locs[r.loc].rotational,
                        location: r.loc,
                    });
                }
            }
        }
    }
    let hashed = hash_all(items, &stats, progress, probe);
    let mut unreadable = 0u64;
    for (path, h) in hashed {
        if let Some(&p) = by_path.get(&path) {
            match h {
                Some(full) => {
                    phys[p].full = Some(full);
                    // Every reachable name of that inode; an offline name
                    // keeps what it had (it was not read).
                    for &r in &phys[p].rows {
                        if locs[rows[r].loc].available {
                            rows[r].full = Some(full);
                            rows[r].dirty = true;
                        }
                    }
                }
                None => {
                    phys[p].excluded = true;
                    unreadable += 1;
                }
            }
        }
    }
    persist(idx, &locs, &rows)?;
    if progress.cancelled() {
        return Err(cancelled());
    }

    // 4. Groups.
    let mut grouped: HashSet<usize> = HashSet::new();
    let mut drafts = Vec::new();
    for members in buckets.values() {
        let members: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&p| !phys[p].excluded)
            .collect();
        for g in group_bucket(&phys, &members) {
            grouped.extend(g.confirmed.iter().chain(&g.probable));
            drafts.push(g);
        }
    }

    let missing = stats.missing.load(Ordering::Relaxed);
    if missing > 0 {
        warnings.push(format!(
            "{missing} indexed file(s) on reachable volumes are gone — left out; \
             `diskghost index add` on their folder prunes the index"
        ));
    }
    unreadable += stats.unreadable.load(Ordering::Relaxed);
    if unreadable > 0 {
        warnings.push(format!("{unreadable} file(s) could not be read — left out"));
    }
    Ok(Groups {
        locs,
        rows,
        phys,
        drafts,
        grouped,
        at,
        stats: HashStats {
            sampled: stats.sampled.load(Ordering::Relaxed),
            full_hashed: stats.full_hashed.load(Ordering::Relaxed),
            changed: stats.changed.load(Ordering::Relaxed),
            missing,
            bytes_read: stats.bytes_read.load(Ordering::Relaxed),
        },
        warnings,
    })
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

#[derive(Default)]
struct Counters {
    sampled: AtomicU64,
    full_hashed: AtomicU64,
    changed: AtomicU64,
    missing: AtomicU64,
    unreadable: AtomicU64,
    bytes_read: AtomicU64,
}

/// A location as this report sees it.
pub(crate) struct Loc {
    pub(crate) id: i64,
    pub(crate) uuid: String,
    /// `local`, or a remote host (a later milestone).
    pub(crate) host: String,
    pub(crate) root: PathBuf,
    pub(crate) role: Role,
    pub(crate) available: bool,
    pub(crate) rotational: bool,
    pub(crate) cow: bool,
}

fn load_locations(idx: &Index, probe: &Probe) -> io::Result<Vec<Loc>> {
    let mut st = idx
        .conn
        .prepare("SELECT id, uuid, root, role, cow, host FROM locations ORDER BY root, uuid")
        .map_err(db_err)?;
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, String>(5)?,
            ))
        })
        .map_err(db_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, uuid, root, role, cow, host) = row.map_err(db_err)?;
        let root = PathBuf::from(root);
        let available = (probe.online)(&root, &uuid);
        out.push(Loc {
            id,
            available,
            rotational: available && (probe.rotational)(&root),
            // Recorded at its last scan; never recorded and out of reach:
            // it may be CoW, so savings may be shared.
            cow: match cow {
                Some(c) => c != 0,
                None if available => (probe.cow)(&root),
                None => true,
            },
            role: Role::parse(&role),
            uuid,
            host,
            root,
        });
    }
    Ok(out)
}

/// One index row of a size that occurs more than once.
pub(crate) struct Row {
    pub(crate) loc: usize,
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) mtime_ns: u64,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) sample: Option<[u8; 32]>,
    pub(crate) full: Option<[u8; 32]>,
    /// On a reachable volume but gone or unreadable: left out.
    pub(crate) excluded: bool,
    /// Differs from its stored row.
    pub(crate) dirty: bool,
}

fn hash32(b: Option<Vec<u8>>) -> Option<[u8; 32]> {
    b.and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
}

/// SQL condition: the row is not in a quarantine directory. Files `apply`
/// moved there are no copies — counting them would let a later plan keep
/// the quarantined copy and move the last real one.
const NOT_QUARANTINED: &str = "path NOT LIKE '.diskghost-quarantine/%' \
     AND path NOT LIKE '%/.diskghost-quarantine/%'";

fn load_colliding(idx: &Index, min: u64, at: &HashMap<i64, usize>) -> io::Result<Vec<Row>> {
    let mut st = idx
        .conn
        .prepare(&format!(
            "SELECT location_id, path, size, mtime_ns, dev, ino, sample, full FROM files \
                 WHERE size >= ?1 AND {NOT_QUARANTINED} AND size IN \
                   (SELECT size FROM files WHERE size >= ?1 AND {NOT_QUARANTINED} \
                    GROUP BY size HAVING COUNT(*) > 1) \
                 ORDER BY location_id, path"
        ))
        .map_err(db_err)?;
    let rows = st
        .query_map([to_sql(min)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<Vec<u8>>>(6)?,
                r.get::<_, Option<Vec<u8>>>(7)?,
            ))
        })
        .map_err(db_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (loc, path, size, mtime, dev, ino, sample, full) = row.map_err(db_err)?;
        let Some(&loc) = at.get(&loc) else { continue };
        out.push(Row {
            loc,
            path,
            size: size as u64,
            mtime_ns: mtime as u64,
            dev: dev as u64,
            ino: ino as u64,
            sample: hash32(sample),
            full: hash32(full),
            excluded: false,
            dirty: false,
        });
    }
    Ok(out)
}

/// Stat every row on a reachable volume; a row whose file changed gets the
/// new size/time/inode and loses its hashes; every row without a sample is
/// sampled. Offline rows are left as they are — never touched.
fn refresh(locs: &[Loc], rows: &mut [Row], stats: &Counters, progress: &Progress, probe: &Probe) {
    let online: Vec<usize> = (0..rows.len())
        .filter(|&i| locs[rows[i].loc].available)
        .collect();
    let bytes = online.iter().map(|&i| rows[i].size.min(SAMPLE_READ)).sum();
    progress.enter(phase::SAMPLE, online.len() as u64, bytes);
    let on_read = probe.on_read;
    rows.par_iter_mut()
        .filter(|r| locs[r.loc].available)
        .for_each(|r| {
            if progress.cancelled() {
                r.excluded = true;
                return;
            }
            let p = abs(&locs[r.loc].root, &r.path);
            let meta = match std::fs::metadata(&p) {
                Ok(m) if m.is_file() => m,
                _ => {
                    r.excluded = true;
                    stats.missing.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            let st = stat_of(&meta);
            if (st.size, st.mtime_ns, st.ino) != (r.size, r.mtime_ns, r.ino) {
                r.size = st.size;
                r.mtime_ns = st.mtime_ns;
                r.ino = st.ino;
                r.sample = None;
                r.full = None;
                r.dirty = true;
                stats.changed.fetch_add(1, Ordering::Relaxed);
            }
            if st.dev != r.dev {
                r.dev = st.dev; // device numbers move across reboots
                r.dirty = true;
            }
            if r.sample.is_none() {
                on_read(&p);
                match sample_file(&p, r.size) {
                    Some(s) => {
                        r.sample = Some(s.sample);
                        r.full = s.full;
                        r.dirty = true;
                        stats.sampled.fetch_add(1, Ordering::Relaxed);
                        stats
                            .bytes_read
                            .fetch_add(r.size.min(SAMPLE_READ), Ordering::Relaxed);
                    }
                    None => {
                        r.excluded = true;
                        stats.unreadable.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            progress.files.fetch_add(1, Ordering::Relaxed);
            progress
                .bytes
                .fetch_add(r.size.min(SAMPLE_READ), Ordering::Relaxed);
        });
}

/// Write back every row this run learned something about.
fn persist(idx: &Index, locs: &[Loc], rows: &[Row]) -> io::Result<()> {
    if !rows.iter().any(|r| r.dirty && !r.excluded) {
        return Ok(());
    }
    let tx = idx.conn.unchecked_transaction().map_err(db_err)?;
    {
        let mut set = tx
            .prepare_cached(
                "UPDATE files SET size = ?3, mtime_ns = ?4, dev = ?5, ino = ?6, \
                 sample = ?7, full = ?8 WHERE location_id = ?1 AND path = ?2",
            )
            .map_err(db_err)?;
        for r in rows.iter().filter(|r| r.dirty && !r.excluded) {
            set.execute(params![
                locs[r.loc].id,
                r.path,
                to_sql(r.size),
                to_sql(r.mtime_ns),
                to_sql(r.dev),
                to_sql(r.ino),
                r.sample.as_ref().map(|h| &h[..]),
                r.full.as_ref().map(|h| &h[..]),
            ])
            .map_err(db_err)?;
        }
    }
    tx.commit().map_err(db_err)
}

/// One physical file: every name of one inode on one filesystem.
pub(crate) struct Phys {
    /// Row indices, the representative name first.
    pub(crate) rows: Vec<usize>,
    pub(crate) size: u64,
    pub(crate) sample: Option<[u8; 32]>,
    pub(crate) full: Option<[u8; 32]>,
    pub(crate) available: bool,
    pub(crate) excluded: bool,
}

/// The filesystem part of a location key: a bind mount or subvolume
/// (`UUID:/sub`) shares inodes with the whole filesystem (`UUID`).
pub(crate) fn filesystem_of(uuid: &str) -> &str {
    if uuid.starts_with("dev:") {
        uuid
    } else {
        uuid.split_once(':').map_or(uuid, |(fs, _)| fs)
    }
}

fn physical(locs: &[Loc], rows: &[Row]) -> Vec<Phys> {
    let mut at: HashMap<(&str, u64, u64, &str), usize> = HashMap::new();
    let mut out: Vec<Phys> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if r.excluded {
            continue;
        }
        // No inode number (not unix): every name is its own file.
        let key = if r.ino == 0 {
            (locs[r.loc].uuid.as_str(), r.dev, 0, r.path.as_str())
        } else {
            (filesystem_of(&locs[r.loc].uuid), r.dev, r.ino, "")
        };
        let available = locs[r.loc].available;
        match at.get(&key) {
            Some(&p) => {
                let ph = &mut out[p];
                ph.rows.push(i);
                ph.available |= available;
                ph.sample = ph.sample.or(r.sample);
                ph.full = ph.full.or(r.full);
            }
            None => {
                at.insert(key, out.len());
                out.push(Phys {
                    rows: vec![i],
                    size: r.size,
                    sample: r.sample,
                    full: r.full,
                    available,
                    excluded: false,
                });
            }
        }
    }
    // Representative: a reachable name first, then by path.
    for ph in &mut out {
        ph.rows.sort_by(|&a, &b| {
            let (ra, rb) = (&rows[a], &rows[b]);
            locs[rb.loc]
                .available
                .cmp(&locs[ra.loc].available)
                .then_with(|| {
                    abs(&locs[ra.loc].root, &ra.path).cmp(&abs(&locs[rb.loc].root, &rb.path))
                })
        });
    }
    out
}

/// Physical files by size, only sizes with more than one.
fn by_size(phys: &[Phys]) -> BTreeMap<u64, Vec<usize>> {
    let mut m: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, p) in phys.iter().enumerate() {
        m.entry(p.size).or_default().push(i);
    }
    m
}

/// A reachable file without a whole-file hash needs one when another
/// physical file of its size may hold the same bytes: same ends, or ends
/// never read (offline, never sampled).
fn needs_full(phys: &[Phys], members: &[usize], p: usize) -> bool {
    let me = &phys[p];
    if !me.available || me.full.is_some() || me.excluded {
        return false;
    }
    members.iter().any(|&o| {
        o != p
            && !phys[o].excluded
            && match (phys[o].sample, me.sample) {
                (Some(a), Some(b)) => a == b,
                (None, _) => !phys[o].available,
                _ => false,
            }
    })
}

/// Read every planned file whole; `(path, hash)` per item, `None` when it
/// could not be read.
fn hash_all(
    items: Vec<HashItem>,
    stats: &Counters,
    progress: &Progress,
    probe: &Probe,
) -> Vec<(PathBuf, Option<[u8; 32]>)> {
    let bytes = items.iter().map(|i| i.size).sum();
    progress.enter(phase::HASH, items.len() as u64, bytes);
    let on_read = probe.on_read;
    let one = |it: &HashItem| {
        if progress.cancelled() {
            return (it.path.clone(), None);
        }
        on_read(&it.path);
        let h = full_hash_with(&it.path, it.size, !it.rotational);
        if h.is_some() {
            stats.full_hashed.fetch_add(1, Ordering::Relaxed);
            stats.bytes_read.fetch_add(it.size, Ordering::Relaxed);
        }
        progress.files.fetch_add(1, Ordering::Relaxed);
        progress.bytes.fetch_add(it.size, Ordering::Relaxed);
        (it.path.clone(), h)
    };
    let mut out = Vec::new();
    for batch in hash_plan(items) {
        match batch.threads {
            None => out.extend(batch.items.par_iter().map(one).collect::<Vec<_>>()),
            Some(n) => out.extend(in_order(n, &batch.items, &one)),
        }
    }
    out
}

/// Run `f` over `items` with `n` threads that each take the next item in
/// order — so a spinning disk sees (nearly) one front-to-back pass.
fn in_order<T: Send>(n: usize, items: &[HashItem], f: &(dyn Fn(&HashItem) -> T + Sync)) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let mut out = Vec::new();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..n.max(1))
            .map(|_| {
                s.spawn(|| {
                    let mut got = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(it) = items.get(i) else { break };
                        got.push(f(it));
                    }
                    got
                })
            })
            .collect();
        for w in workers {
            if let Ok(got) = w.join() {
                out.extend(got);
            }
        }
    });
    out
}

/// One group before rendering.
pub(crate) struct Draft {
    pub(crate) size: u64,
    pub(crate) full: Option<[u8; 32]>,
    pub(crate) sample: Option<[u8; 32]>,
    pub(crate) confirmed: Vec<usize>,
    pub(crate) probable: Vec<usize>,
}

/// The groups among the physical files of one size: one per whole-file
/// hash that has a second confirmed or a probable copy, plus offline files
/// sharing ends that were never hashed whole.
fn group_bucket(phys: &[Phys], members: &[usize]) -> Vec<Draft> {
    let mut out = Vec::new();
    let mut by_full: BTreeMap<[u8; 32], Vec<usize>> = BTreeMap::new();
    for &p in members {
        if let Some(f) = phys[p].full {
            by_full.entry(f).or_default().push(p);
        }
    }
    // Offline, never hashed whole: probably a copy of a same-size file
    // whose ends match (or whose ends were never read).
    let unhashed: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&p| phys[p].full.is_none() && !phys[p].available)
        .collect();
    let mut sample_of_groups: HashSet<[u8; 32]> = HashSet::new();
    for (full, confirmed) in by_full {
        let sample = confirmed.iter().find_map(|&p| phys[p].sample);
        let probable: Vec<usize> = unhashed
            .iter()
            .copied()
            .filter(|&p| phys[p].sample.is_none() || phys[p].sample == sample)
            .collect();
        if confirmed.len() >= 2 || !probable.is_empty() {
            sample_of_groups.extend(sample);
            out.push(Draft {
                size: phys[confirmed[0]].size,
                full: Some(full),
                sample,
                confirmed,
                probable,
            });
        }
    }
    let mut by_sample: BTreeMap<[u8; 32], Vec<usize>> = BTreeMap::new();
    for &p in &unhashed {
        if let Some(s) = phys[p].sample {
            if !sample_of_groups.contains(&s) {
                by_sample.entry(s).or_default().push(p);
            }
        }
    }
    for (sample, probable) in by_sample {
        if probable.len() >= 2 {
            out.push(Draft {
                size: phys[probable[0]].size,
                full: None,
                sample: Some(sample),
                confirmed: Vec::new(),
                probable,
            });
        }
    }
    out
}

pub(crate) fn role_rank(r: Role) -> u8 {
    match r {
        Role::Master => 0,
        Role::Normal => 1,
        Role::Scratch => 2,
    }
}

fn render(locs: &[Loc], rows: &[Row], phys: &[Phys], g: &Draft) -> IndexGroup {
    let names = |p: usize| -> Vec<PathBuf> {
        phys[p]
            .rows
            .iter()
            .map(|&r| abs(&locs[rows[r].loc].root, &rows[r].path))
            .collect()
    };
    let loc_of = |p: usize| &locs[rows[phys[p].rows[0]].loc];
    // Reachable confirmed copies, best keeper first.
    let mut reachable: Vec<usize> = g
        .confirmed
        .iter()
        .copied()
        .filter(|&p| phys[p].available)
        .collect();
    reachable.sort_by_key(|&p| (role_rank(loc_of(p).role), names(p)[0].clone()));
    let mut away: Vec<usize> = g
        .confirmed
        .iter()
        .copied()
        .filter(|&p| !phys[p].available)
        .collect();
    away.sort_by_key(|&p| names(p)[0].clone());
    let mut probable = g.probable.clone();
    probable.sort_by_key(|&p| names(p)[0].clone());

    let copy = |p: usize, probable: bool, suggest: Option<Suggest>| {
        let mut n = names(p);
        let path = n.remove(0);
        GroupCopy {
            location_uuid: loc_of(p).uuid.clone(),
            path,
            available: phys[p].available,
            probable,
            shared_inodes: n,
            suggest,
        }
    };
    let mut copies = Vec::new();
    for (i, &p) in reachable.iter().enumerate() {
        let s = if i == 0 {
            Suggest::Keep
        } else {
            Suggest::Reclaimable
        };
        copies.push(copy(p, false, Some(s)));
    }
    copies.extend(away.iter().map(|&p| copy(p, false, None)));
    copies.extend(probable.iter().map(|&p| copy(p, true, None)));

    let unique = g.confirmed.len() as u64;
    let available = reachable.len() as u64;
    let hex = |h: [u8; 32]| blake3::Hash::from(h).to_hex().to_string();
    IndexGroup {
        algo: "blake3".into(),
        hash: g.full.map(hex),
        sample: g.sample.map(hex),
        size: g.size,
        copies,
        unique_copies: unique,
        available_copies: available,
        probable_copies: g.probable.len() as u64,
        reclaimable_bytes: g.size.saturating_mul(unique.saturating_sub(1)),
        actionable_bytes: g.size.saturating_mul(available.saturating_sub(1)),
        savings_may_be_shared: g.confirmed.iter().any(|&p| loc_of(p).cow),
    }
}

/// Per location, the physical files no other file can be a copy of: a
/// size no other indexed file has, or colliding but in no group.
#[allow(clippy::too_many_arguments)]
fn singles(
    idx: &Index,
    min: u64,
    with_paths: bool,
    locs: &[Loc],
    rows: &[Row],
    phys: &[Phys],
    grouped: &HashSet<usize>,
    at: &HashMap<i64, usize>,
) -> io::Result<Vec<LocationSingles>> {
    let mut out: Vec<LocationSingles> = locs
        .iter()
        .map(|l| LocationSingles {
            location_uuid: l.uuid.clone(),
            root: l.root.clone(),
            available: l.available,
            files: 0,
            bytes: 0,
            paths: with_paths.then(Vec::new),
        })
        .collect();
    let mut add = |loc: usize, size: u64, path: PathBuf| {
        let s = &mut out[loc];
        s.files += 1;
        s.bytes += size;
        if let Some(p) = s.paths.as_mut() {
            p.push(path);
        }
    };
    for (i, p) in phys.iter().enumerate() {
        if p.excluded || grouped.contains(&i) {
            continue;
        }
        let r = &rows[p.rows[0]];
        add(r.loc, p.size, abs(&locs[r.loc].root, &r.path));
    }
    let mut st = idx
        .conn
        .prepare(&format!(
            "SELECT location_id, path, size FROM files \
                 WHERE size >= ?1 AND {NOT_QUARANTINED} AND size IN \
                   (SELECT size FROM files WHERE size >= ?1 AND {NOT_QUARANTINED} \
                    GROUP BY size HAVING COUNT(*) = 1) \
                 ORDER BY location_id, path"
        ))
        .map_err(db_err)?;
    let unique = st
        .query_map([to_sql(min)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(db_err)?;
    for row in unique {
        let (loc, path, size) = row.map_err(db_err)?;
        if let Some(&l) = at.get(&loc) {
            add(l, size as u64, abs(&locs[l].root, &path));
        }
    }
    for s in &mut out {
        if let Some(p) = s.paths.as_mut() {
            p.sort();
        }
    }
    Ok(out)
}

/// One file to hash whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HashItem {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    /// Its volume spins.
    pub(crate) rotational: bool,
    /// Which location (one batch per rotational location).
    pub(crate) location: usize,
}

/// Files hashed together: `threads == None` means the shared pool (full
/// parallelism, SSD); `Some(n)` a pool of `n` readers in `items` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HashBatch {
    pub(crate) threads: Option<usize>,
    pub(crate) items: Vec<HashItem>,
}

/// Split the whole-file hashing work: everything not rotational in one
/// fully parallel batch; each rotational location its own batch of
/// [`crate::rotational::ROTATIONAL_HASH_THREADS`] readers, sorted by path —
/// files written together sit together, so path order is the cheap stand-in
/// for physical order (no FIEMAP).
pub(crate) fn hash_plan(items: Vec<HashItem>) -> Vec<HashBatch> {
    let mut fast = Vec::new();
    let mut spinning: BTreeMap<usize, Vec<HashItem>> = BTreeMap::new();
    for it in items {
        if it.rotational {
            spinning.entry(it.location).or_default().push(it);
        } else {
            fast.push(it);
        }
    }
    let mut out = Vec::new();
    if !fast.is_empty() {
        out.push(HashBatch {
            threads: None,
            items: fast,
        });
    }
    for (_, mut items) in spinning {
        items.sort_by(|a, b| a.path.cmp(&b.path));
        out.push(HashBatch {
            threads: Some(ROTATIONAL_HASH_THREADS),
            items,
        });
    }
    out
}

/// The path of a row: location root joined with the stored relative path.
pub(crate) fn abs(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::*;
    use std::collections::BTreeSet;
    use std::time::{Duration, SystemTime};

    fn group_of(r: &IndexDupesReport, size: u64) -> Vec<&IndexGroup> {
        r.groups.iter().filter(|g| g.size == size).collect()
    }

    fn copy_at<'a>(g: &'a IndexGroup, path: &Path) -> &'a GroupCopy {
        g.copies
            .iter()
            .find(|c| c.path == path || c.shared_inodes.iter().any(|s| s == path))
            .unwrap_or_else(|| panic!("{path:?} not in {g:#?}"))
    }

    // ---- hard links reach the index -------------------------------------

    #[test]
    fn every_name_of_a_hard_link_is_indexed() {
        let e = Estate::new("names");
        let a = e.loc("A");
        write(&a.join("one.bin"), &content(1));
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::hard_link(a.join("one.bin"), a.join("sub/two.bin")).unwrap();
        e.add("A");
        let (s1, ..) = e.row("A", "one.bin");
        let (s2, ..) = e.row("A", "sub/two.bin");
        assert_eq!((s1, s2), (BIG as i64, BIG as i64));
    }

    // ---- groups, physical copies, lazy persistent hashing ---------------

    /// A: dup.bin + a hard link to it + a same-ends decoy + a unique file;
    /// B: a second copy of dup.bin.
    fn two_volumes(label: &str) -> Estate {
        let e = Estate::new(label);
        let (a, b) = (e.loc("A"), e.loc("B"));
        write(&a.join("dup.bin"), &content(1));
        std::fs::create_dir_all(a.join("links")).unwrap();
        std::fs::hard_link(a.join("dup.bin"), a.join("links/dup-link.bin")).unwrap();
        write(&a.join("decoy.bin"), &same_ends(1));
        write(&a.join("unique.bin"), &content(2)[..1000]);
        write(&b.join("copy/dup.bin"), &content(1));
        e.add("A");
        e.add("B");
        e
    }

    #[test]
    fn groups_count_physical_copies_and_persist_hashes() {
        let e = two_volumes("groups");
        let (a, b) = (e.loc("A"), e.loc("B"));
        let r = e.report(&Estate::opts());

        let gs = group_of(&r, BIG as u64);
        assert_eq!(gs.len(), 1, "the decoy only shares ends: {:#?}", r.groups);
        let g = gs[0];
        let want = blake3::hash(&content(1)).to_hex().to_string();
        assert_eq!(g.hash.as_deref(), Some(want.as_str()));
        // Three names, two physical copies: the hard link is one copy.
        assert_eq!(g.copies.len(), 2, "{g:#?}");
        assert_eq!(g.unique_copies, 2);
        assert_eq!(g.available_copies, 2);
        assert_eq!(g.probable_copies, 0);
        assert_eq!(g.reclaimable_bytes, BIG as u64);
        assert_eq!(g.actionable_bytes, BIG as u64);
        let ca = copy_at(g, &a.join("dup.bin"));
        let mut names: Vec<PathBuf> = ca.shared_inodes.clone();
        names.push(ca.path.clone());
        names.sort();
        assert_eq!(names, vec![a.join("dup.bin"), a.join("links/dup-link.bin")]);
        assert_eq!(ca.location_uuid, "vol-A");
        let cb = copy_at(g, &b.join("copy/dup.bin"));
        assert!(cb.shared_inodes.is_empty());
        assert!(ca.available && cb.available && !ca.probable && !cb.probable);
        // One keeper, one reclaimable.
        let mut s: Vec<Option<Suggest>> = g.copies.iter().map(|c| c.suggest).collect();
        s.sort_by_key(|x| format!("{x:?}"));
        assert_eq!(s, vec![Some(Suggest::Keep), Some(Suggest::Reclaimable)]);
        assert!(!g.savings_may_be_shared);

        // The whole-file hashes went into the index.
        let full = blake3::hash(&content(1)).as_bytes().to_vec();
        assert_eq!(e.row("A", "dup.bin").3.as_deref(), Some(&full[..]));
        assert_eq!(e.row("B", "copy/dup.bin").3.as_deref(), Some(&full[..]));
        assert!(r.stats.full_hashed >= 2);

        // Every later report reads nothing at all.
        let again = e.report(&Estate::opts());
        assert_eq!(e.reads(), BTreeSet::new());
        assert_eq!(again.groups, r.groups);
        assert_eq!(again.stats.full_hashed, 0);
        assert_eq!(again.stats.sampled, 0);
    }

    #[test]
    fn a_hard_link_pair_alone_is_no_duplicate() {
        let e = Estate::new("linkonly");
        let a = e.loc("A");
        write(&a.join("x.bin"), &content(3));
        std::fs::hard_link(a.join("x.bin"), a.join("y.bin")).unwrap();
        e.add("A");
        let r = e.report(&IndexReportOptions {
            singles: true,
            ..Estate::opts()
        });
        assert!(r.groups.is_empty(), "{:#?}", r.groups);
        let s = &r.singles.unwrap()[0];
        assert_eq!((s.files, s.bytes), (1, BIG as u64), "one physical file");
    }

    #[test]
    fn copy_on_write_flags_savings_as_maybe_shared() {
        let mut e = Estate::new("cow");
        e.cow = true;
        write(&e.loc("A").join("a.bin"), &content(4));
        write(&e.loc("B").join("b.bin"), &content(4));
        e.add("A");
        e.add("B");
        let r = e.report(&Estate::opts());
        assert_eq!(r.groups.len(), 1);
        assert!(r.groups[0].savings_may_be_shared);
        assert!(r.locations.iter().all(|l| l.cow));
    }

    #[test]
    fn rotational_volumes_give_the_same_groups() {
        let mut e = Estate::new("rot");
        e.rotational = true;
        for i in 0..5 {
            write(&e.loc("A").join(format!("{i}.bin")), &content(5));
        }
        write(&e.loc("B").join("x.bin"), &content(5));
        e.add("A");
        e.add("B");
        let r = e.report(&Estate::opts());
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].unique_copies, 6);
        assert!(r.locations.iter().all(|l| l.rotational));
    }

    // ---- offline locations ----------------------------------------------

    #[test]
    fn offline_copies_count_but_are_never_suggested() {
        let e = two_volumes("offline");
        e.report(&Estate::opts()); // both online: hashes stored
        e.unplug("B");
        let r = e.report(&Estate::opts());
        assert!(
            e.reads().iter().all(|p| !p.starts_with(e.loc("B"))),
            "an offline volume is never read"
        );
        let g = group_of(&r, BIG as u64)[0];
        assert_eq!(g.unique_copies, 2, "the offline copy counts");
        assert_eq!(g.available_copies, 1);
        assert_eq!(g.reclaimable_bytes, BIG as u64);
        assert_eq!(g.actionable_bytes, 0, "nothing reachable is redundant");
        let off: Vec<&GroupCopy> = g.copies.iter().filter(|c| !c.available).collect();
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].location_uuid, "vol-B");
        assert_eq!(off[0].suggest, None, "offline copies are never suggested");
        for c in &g.copies {
            if c.suggest == Some(Suggest::Reclaimable) {
                panic!("a lone reachable copy was offered for removal: {g:#?}");
            }
        }
        let loc = r.locations.iter().find(|l| l.uuid == "vol-B").unwrap();
        assert!(!loc.available);
        // A volume whose kind cannot be checked now may be CoW.
        assert!(g.savings_may_be_shared == r.locations.iter().any(|l| l.cow));
    }

    #[test]
    fn offline_rows_without_a_whole_hash_are_probable() {
        // B's copy was sampled (both volumes online at its scan) but never
        // hashed whole; then B was unplugged.
        let e = Estate::new("probable-sample");
        write(&e.loc("A").join("a.bin"), &content(6));
        write(&e.loc("B").join("b.bin"), &content(6));
        e.add("A");
        e.add("B");
        assert!(e.row("B", "b.bin").2.is_some() && e.row("B", "b.bin").3.is_none());
        e.unplug("B");
        let r = e.report(&Estate::opts());
        let g = &r.groups[0];
        assert_eq!(g.unique_copies, 1);
        assert_eq!(g.probable_copies, 1);
        assert_eq!(g.reclaimable_bytes, 0, "a probable copy frees nothing");
        let b = g
            .copies
            .iter()
            .find(|c| c.location_uuid == "vol-B")
            .unwrap();
        assert!(b.probable && !b.available && b.suggest.is_none());
        assert!(e.reads().iter().all(|p| !p.starts_with(e.loc("B"))));
    }

    #[test]
    fn offline_rows_never_sampled_but_same_size_are_probable() {
        // B was indexed alone (unique size → never sampled), then unplugged;
        // A's same-size file arrived later.
        let e = Estate::new("probable-size");
        write(&e.loc("B").join("b.bin"), &content(7));
        e.add("B");
        assert_eq!(e.row("B", "b.bin").2, None);
        e.unplug("B");
        write(&e.loc("A").join("a.bin"), &content(8));
        e.add("A");
        let r = e.report(&IndexReportOptions {
            singles: true,
            ..Estate::opts()
        });
        assert_eq!(r.groups.len(), 1, "{:#?}", r.groups);
        let g = &r.groups[0];
        let b = g
            .copies
            .iter()
            .find(|c| c.location_uuid == "vol-B")
            .unwrap();
        assert!(b.probable && !b.available && b.suggest.is_none());
        let a = g
            .copies
            .iter()
            .find(|c| c.location_uuid == "vol-A")
            .unwrap();
        assert!(!a.probable && a.available);
        assert_eq!(g.hash, Some(blake3::hash(&content(8)).to_hex().to_string()));
        // Possibly backed up: not a single.
        assert!(r.singles.unwrap().iter().all(|s| s.files == 0));
    }

    #[test]
    fn an_offline_volume_at_the_same_mountpoint_never_takes_the_online_hash() {
        // Two backup disks take turns at one mountpoint: B's rows keep
        // their paths while A sits at B's old place. A's file must be
        // hashed for itself only — never written onto B's row.
        let e = Estate::new("same-root");
        write(&e.loc("A").join("same.bin"), &content(11));
        write(&e.loc("B").join("same.bin"), &content(12));
        e.add("A");
        e.add("B");
        e.remount_over("B", "A");
        // B was indexed by an older run that never sampled it.
        e.idx
            .conn
            .execute(
                "UPDATE files SET sample = NULL, full = NULL WHERE location_id = \
                 (SELECT id FROM locations WHERE uuid = 'vol-B')",
                [],
            )
            .unwrap();

        let r = e.report(&Estate::opts());
        assert_eq!(e.reads(), BTreeSet::from([e.loc("A").join("same.bin")]));
        let (_, _, sample, full) = e.row("B", "same.bin");
        assert_eq!(
            (sample, full),
            (None, None),
            "the offline row is never written"
        );
        let want = blake3::hash(&content(11)).as_bytes().to_vec();
        assert_eq!(e.row("A", "same.bin").3.as_deref(), Some(&want[..]));
        let g = &r.groups[0];
        assert_eq!((g.unique_copies, g.probable_copies), (1, 1), "{g:#?}");
        let b = g
            .copies
            .iter()
            .find(|c| c.location_uuid == "vol-B")
            .unwrap();
        assert!(b.probable && !b.available && b.suggest.is_none());
    }

    // ---- changed files --------------------------------------------------

    #[test]
    fn a_file_changed_since_its_row_is_rehashed_and_its_row_updated() {
        let e = two_volumes("changed");
        let first = e.report(&Estate::opts());
        assert_eq!(first.groups.len(), 1);
        let b = e.loc("B").join("copy/dup.bin");
        let old = e.row("B", "copy/dup.bin");
        // Same size, new content and time.
        write(&b, &content(9));
        let f = std::fs::File::options().write(true).open(&b).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        drop(f);

        let r = e.report(&Estate::opts());
        assert!(e.reads().contains(&b), "the changed file is read again");
        assert!(r.stats.changed >= 1);
        let new = e.row("B", "copy/dup.bin");
        assert_ne!(new.1, old.1, "mtime updated");
        assert_ne!(new.2, old.2, "sample updated");
        assert!(r.groups.is_empty(), "no longer a copy: {:#?}", r.groups);
        assert!(r.stats.full_hashed == 0 || new.3.is_some());
    }

    #[test]
    fn a_file_gone_from_an_online_volume_is_left_out_with_a_warning() {
        let e = two_volumes("gone");
        std::fs::remove_file(e.loc("B").join("copy/dup.bin")).unwrap();
        let r = e.report(&Estate::opts());
        assert_eq!(r.stats.missing, 1);
        assert!(!r.warnings.is_empty());
        // Only A's (hard-linked) copy is left: no group.
        assert!(r.groups.is_empty(), "{:#?}", r.groups);
        // The row stays: pruning is `index add`'s job.
        let _ = e.row("B", "copy/dup.bin");
    }

    // ---- singles --------------------------------------------------------

    #[test]
    fn singles_are_counted_per_location_with_paths_on_request() {
        let e = two_volumes("singles");
        write(&e.loc("B").join("only-b.bin"), &content(10)[..777]);
        e.add("B");
        let r = e.report(&IndexReportOptions {
            singles: true,
            ..Estate::opts()
        });
        let s = r.singles.as_ref().unwrap();
        let a = s.iter().find(|s| s.location_uuid == "vol-A").unwrap();
        // decoy.bin (same ends, other middle) + unique.bin (1000 bytes).
        assert_eq!((a.files, a.bytes), (2, BIG as u64 + 1000));
        assert!(a.paths.is_none(), "paths only on request");
        let b = s.iter().find(|s| s.location_uuid == "vol-B").unwrap();
        assert_eq!((b.files, b.bytes), (1, 777));

        let r = e.report(&IndexReportOptions {
            singles: true,
            singles_paths: true,
            ..Estate::opts()
        });
        let s = r.singles.unwrap();
        let a = s.iter().find(|s| s.location_uuid == "vol-A").unwrap();
        let mut p = a.paths.clone().unwrap();
        p.sort();
        assert_eq!(
            p,
            vec![e.loc("A").join("decoy.bin"), e.loc("A").join("unique.bin")]
        );
        // Without --singles, no section at all.
        assert!(e.report(&Estate::opts()).singles.is_none());
    }

    #[test]
    fn offline_singles_still_count() {
        let e = two_volumes("offline-singles");
        write(&e.loc("B").join("only-b.bin"), &content(10)[..777]);
        e.add("B");
        e.unplug("B");
        let r = e.report(&IndexReportOptions {
            singles: true,
            ..Estate::opts()
        });
        let b = r
            .singles
            .unwrap()
            .into_iter()
            .find(|s| s.location_uuid == "vol-B")
            .unwrap();
        assert!(!b.available);
        assert_eq!((b.files, b.bytes), (1, 777));
    }

    // ---- hash plan ------------------------------------------------------

    fn item(path: &str, rotational: bool, location: usize) -> HashItem {
        HashItem {
            path: PathBuf::from(path),
            size: 1,
            rotational,
            location,
        }
    }

    #[test]
    fn spinning_disks_get_few_readers_in_path_order() {
        let plan = hash_plan(vec![
            item("/hdd/b/2", true, 1),
            item("/ssd/x", false, 0),
            item("/hdd/a/9", true, 1),
            item("/usb/z", true, 2),
            item("/ssd/y", false, 3),
            item("/hdd/a/1", true, 1),
        ]);
        let ssd: Vec<&HashBatch> = plan.iter().filter(|b| b.threads.is_none()).collect();
        assert_eq!(ssd.len(), 1);
        assert_eq!(ssd[0].items.len(), 2);
        let hdd: Vec<&HashBatch> = plan.iter().filter(|b| b.threads.is_some()).collect();
        assert_eq!(hdd.len(), 2, "one batch per spinning location");
        for b in &hdd {
            assert_eq!(b.threads, Some(ROTATIONAL_HASH_THREADS));
            let paths: Vec<&Path> = b.items.iter().map(|i| i.path.as_path()).collect();
            let mut sorted = paths.clone();
            sorted.sort();
            assert_eq!(paths, sorted, "path order");
        }
        let h1 = hdd.iter().find(|b| b.items[0].location == 1).unwrap();
        assert_eq!(h1.items.len(), 3);
        assert!(hash_plan(Vec::new()).is_empty());
    }
}
