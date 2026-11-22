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

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::index::{Index, Probe};
use crate::Progress;

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
}

/// Files of one content (size + whole-file hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexGroup {
    /// BLAKE3 of the whole file, hex. `None` when no copy has one yet (all
    /// members are offline and only their ends were ever read).
    pub hash: Option<String>,
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
    let _ = (idx, opts, progress, probe);
    unimplemented!("M2 report")
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
    let _ = items;
    unimplemented!("M2 hash plan")
}

/// The path of a row: location root joined with the stored relative path.
fn abs(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{scan_with, Volume};
    use crate::rotational::ROTATIONAL_HASH_THREADS;
    use crate::Options;
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime};

    // ---- fixtures -------------------------------------------------------

    /// A fresh directory owned by this test alone. Tests create and delete
    /// files only inside it — never anywhere else (owner's hard safety rule).
    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-report-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Larger than two samples (2 × 16 KiB), so `index add` stores only the
    /// sample and the whole-file hash is the report's job.
    const BIG: usize = 40 * 1024;

    fn content(tag: u8) -> Vec<u8> {
        (0..BIG).map(|i| (i % 251) as u8 ^ tag).collect()
    }

    /// Same size and same two ends as `content(tag)`, different middle.
    fn same_ends(tag: u8) -> Vec<u8> {
        let mut c = content(tag);
        c[BIG / 2] ^= 0xff;
        c
    }

    /// A test estate: a database plus folders that each pretend to be a
    /// volume of their own (`vol-<name>`), online while the folder exists.
    struct Estate {
        dir: PathBuf,
        idx: Index,
        cow: bool,
        rotational: bool,
        reads: Mutex<Vec<PathBuf>>,
    }

    impl Estate {
        fn new(label: &str) -> Estate {
            let dir = tmpdir(label);
            let idx = Index::open(&dir.join("db/index.db")).unwrap();
            Estate {
                dir,
                idx,
                cow: false,
                rotational: false,
                reads: Mutex::new(Vec::new()),
            }
        }

        fn loc(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }

        fn uuid(name: &str) -> String {
            format!("vol-{name}")
        }

        fn with_probe<T>(&self, f: impl FnOnce(&Probe) -> T) -> T {
            let base = self.dir.clone();
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
                })
            };
            let online = |root: &Path, _uuid: &str| root.is_dir();
            let cow = self.cow;
            let cow = move |_: &Path| cow;
            let rot = self.rotational;
            let rotational = move |_: &Path| rot;
            let reads = &self.reads;
            let on_read = move |p: &Path| reads.lock().unwrap().push(p.to_path_buf());
            let probe = Probe {
                volume: &volume,
                online: &online,
                cow: &cow,
                rotational: &rotational,
                on_read: &on_read,
            };
            f(&probe)
        }

        fn add(&self, name: &str) {
            let root = self.loc(name);
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

        fn report(&self, opts: &IndexReportOptions) -> IndexDupesReport {
            self.reads.lock().unwrap().clear();
            self.with_probe(|p| report_with(&self.idx, opts, &Progress::default(), p))
                .unwrap()
        }

        fn reads(&self) -> BTreeSet<PathBuf> {
            self.reads.lock().unwrap().iter().cloned().collect()
        }

        /// Pretend a location's disk was unplugged: its last known root no
        /// longer exists. Only the index row changes — no file is touched.
        fn unplug(&self, name: &str) {
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

        /// (size, mtime_ns, sample, full) of the row of `name`/`rel`.
        fn row(&self, name: &str, rel: &str) -> (i64, i64, Option<Vec<u8>>, Option<Vec<u8>>) {
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

        fn opts() -> IndexReportOptions {
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

    fn group_of<'a>(r: &'a IndexDupesReport, size: u64) -> Vec<&'a IndexGroup> {
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
