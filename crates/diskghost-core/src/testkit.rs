//! Test fixtures shared by the index report, plan and apply tests: a
//! database plus folders that each pretend to be a volume of their own.
//! Tests create and delete files only inside their own temp directory —
//! never anywhere else (owner's hard safety rule).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::index::{scan_with, Index, Probe, Volume};
use crate::index_report::{report_with, IndexDupesReport, IndexReportOptions};
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
        let probe = Probe {
            volume: &volume,
            online: &online,
            cow: &cow,
            rotational: &rotational,
            on_read: &on_read,
        };
        f(&probe)
    }

    pub(crate) fn add(&self, name: &str) {
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
