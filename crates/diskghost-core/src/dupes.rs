//! Duplicate detection: files with byte-identical content.
//!
//! 1. The walk lists every file at least `min_size` bytes long; only sizes
//!    that occur more than once can hold duplicates.
//! 2. Every same-size candidate is opened once — in parallel, file by file
//!    — to learn its identity (so hard links to one physical file are never
//!    called duplicates) and to hash its first and last 16 KB. Files up to
//!    32 KB are thereby hashed completely.
//! 3. Only files whose size and both ends match another one are hashed in
//!    full with BLAKE3, again in parallel per file (big files additionally
//!    use several threads each, through a memory map).
//!
//! A [`HashCache`] remembers identity and hashes by path, size and
//! modification time, so searching the same place again reads only files
//! that changed. A running search hands what it learns to the cache every
//! [`MERGE_INTERVAL`], so a frontend saving the cache meanwhile (see
//! [`HashCache::checkpoint_while`]) keeps a killed or crashed search's work.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::snapshot::{get_varint, put_varint};
use crate::walk::walk;
use crate::{phase, DupGroup, Options, Progress};

/// Folders a duplicate search never enters (below its root): the recycle bin
/// and trash of each platform, and Windows' restore-point store. Files there
/// are copies of something that was just deleted, not duplicates to clean up.
pub const TRASH_DIRS: [&str; 6] = [
    "$Recycle.Bin",
    "RECYCLER",
    "System Volume Information",
    ".Trash",
    ".Trashes",
    ".Trash-*",
];

/// Bytes read from each end of a candidate for the quick comparison.
const SAMPLE: u64 = 16 * 1024;

/// Files at least this big are hashed with several threads each.
pub(crate) const PARALLEL_HASH_MIN: u64 = 64 << 20;

const CACHE_MAGIC: &[u8; 8] = b"DGHASH01";

/// How often a running search hands what it has learned to the shared
/// [`HashCache`] — one batch per interval, never one lock per file. Kept well
/// below [`CHECKPOINT_INTERVAL`] so each checkpoint sees recent work.
pub const MERGE_INTERVAL: Duration = Duration::from_secs(10);

/// How often a frontend writes the [`HashCache`] to disk while a search runs
/// (see [`HashCache::checkpoint_while`]). A kill or crash loses at most about
/// this plus [`MERGE_INTERVAL`] of hashing; a shorter interval would rewrite
/// a cache of possibly hundreds of MB more often for little gain on a search
/// that runs for hours.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// What is known about one file's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Known {
    size: u64,
    /// Modification time (ns since the epoch); a cached entry only counts
    /// while size and time are unchanged.
    modified: u64,
    /// (volume, file index) on Windows, (device, inode) elsewhere.
    id: (u64, u64),
    /// BLAKE3 of the first and last [`SAMPLE`] bytes.
    sample: [u8; 32],
    /// BLAKE3 of the whole file, once computed.
    full: Option<[u8; 32]>,
}

/// Hashes of files seen by earlier duplicate searches, keyed by path. Share
/// one between searches (it is internally synchronised) and persist it with
/// [`HashCache::save`] / [`HashCache::load`].
///
/// Locking order: `saved` before `entries`, never the other way round.
/// Lookups and merges take only `entries`; a save holds `saved` throughout
/// and `entries` only while it encodes, so a checkpoint racing a running
/// search cannot deadlock and saves never interleave.
#[derive(Default)]
pub struct HashCache {
    entries: Mutex<HashMap<PathBuf, Known>>,
    /// Bumped (while `entries` is held) whenever an entry changes.
    generation: AtomicU64,
    /// The `generation` last written by a save; held for a whole save so
    /// saves run one at a time and the newest knowledge is written last.
    saved: Mutex<u64>,
}

impl HashCache {
    /// An empty cache.
    pub fn new() -> HashCache {
        HashCache::default()
    }

    /// Number of files the cache knows.
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }

    /// Whether the cache knows no file.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, path: &Path, size: u64, modified: u64) -> Option<Known> {
        if modified == 0 {
            return None; // no reliable time: always look at the file
        }
        let e = self.entries.lock().ok()?;
        e.get(path)
            .filter(|k| k.size == size && k.modified == modified)
            .copied()
    }

    /// Replace everything known below `root` with `fresh` — files that are
    /// gone or changed drop out, entries elsewhere stay.
    fn replace_under(&self, root: &Path, fresh: Vec<(PathBuf, Known)>) {
        if let Ok(mut e) = self.entries.lock() {
            e.retain(|p, _| !p.starts_with(root));
            e.extend(fresh);
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Add `fresh` to what is known, replacing older entries for the same
    /// paths; everything else stays. Unlike [`HashCache::replace_under`] it
    /// never forgets, so it is safe to call while a search is half done.
    fn merge(&self, fresh: Vec<(PathBuf, Known)>) {
        if fresh.is_empty() {
            return;
        }
        if let Ok(mut e) = self.entries.lock() {
            let mut changed = false;
            for (p, k) in fresh {
                changed |= e.insert(p, k) != Some(k);
            }
            if changed {
                self.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// [`HashCache::save`], skipped when nothing changed since the last save
    /// (to any file) or since [`HashCache::load`]. Returns whether the file
    /// was written.
    pub fn save_if_changed(&self, path: &Path) -> std::io::Result<bool> {
        self.save_inner(path, true)
    }

    /// Run `job` while a scoped thread saves the cache to `path` every
    /// `every` (only when it changed, see [`HashCache::save_if_changed`]) —
    /// the frontend side of incremental checkpoints. The thread stops as soon
    /// as `job` returns or panics and is joined before this returns (at worst
    /// after the save it is in the middle of). Checkpoint errors are ignored:
    /// the caller's final [`HashCache::save`] reports a lasting problem.
    pub fn checkpoint_while<T>(&self, path: &Path, every: Duration, job: impl FnOnce() -> T) -> T {
        let (stop, stopped) = mpsc::channel::<()>();
        std::thread::scope(|s| {
            s.spawn(move || {
                // Dropping `stop` (job done or unwinding) ends the wait at once.
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(every) {
                    let _ = self.save_if_changed(path);
                }
            });
            let out = job();
            drop(stop);
            out
        })
    }

    /// Read a cache written by [`HashCache::save`]. A missing, foreign or
    /// damaged file yields an empty cache: it only ever costs a re-read.
    pub fn load(path: &Path) -> HashCache {
        let cache = HashCache::new();
        if let Ok(map) = Self::read(path) {
            if let Ok(mut e) = cache.entries.lock() {
                *e = map;
            }
        }
        cache
    }

    fn read(path: &Path) -> std::io::Result<HashMap<PathBuf, Known>> {
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad hash cache");
        let bytes = std::fs::read(path)?;
        let body = bytes.strip_prefix(CACHE_MAGIC).ok_or_else(bad)?;
        let mut r = BufReader::new(zstd::stream::read::Decoder::with_buffer(body)?);
        let n = get_varint(&mut r).map_err(|_| bad())?;
        let mut map = HashMap::with_capacity(n.min(1 << 24) as usize);
        let mut name = Vec::new();
        for _ in 0..n {
            let mut num = || get_varint(&mut r).map_err(|_| bad());
            let len = num()? as usize;
            let (size, modified, id0, id1, has_full) = (num()?, num()?, num()?, num()?, num()?);
            if len > 1 << 16 {
                return Err(bad());
            }
            name.resize(len, 0);
            r.read_exact(&mut name)?;
            let mut sample = [0u8; 32];
            r.read_exact(&mut sample)?;
            let full = if has_full == 1 {
                let mut f = [0u8; 32];
                r.read_exact(&mut f)?;
                Some(f)
            } else {
                None
            };
            let path = PathBuf::from(String::from_utf8(name.clone()).map_err(|_| bad())?);
            map.insert(
                path,
                Known {
                    size,
                    modified,
                    id: (id0, id1),
                    sample,
                    full,
                },
            );
        }
        let _ = r.fill_buf();
        Ok(map)
    }

    /// Write the cache (zstd-compressed), replacing `path` atomically: the
    /// bytes go to a temporary file next to it, are flushed to disk and only
    /// then renamed over `path`, so a crash at any moment leaves either the
    /// old or the new cache, never a torn one. Concurrent saves (a checkpoint
    /// timer and a final save) are serialised.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        self.save_inner(path, false).map(|_| ())
    }

    fn save_inner(&self, path: &Path, only_if_changed: bool) -> std::io::Result<bool> {
        let poisoned = || std::io::Error::other("hash cache poisoned");
        let mut saved = self.saved.lock().map_err(|_| poisoned())?;
        let entries = self.entries.lock().map_err(|_| poisoned())?;
        let generation = self.generation.load(Ordering::Relaxed);
        if only_if_changed && generation == *saved {
            return Ok(false);
        }
        let mut raw = Vec::with_capacity(entries.len() * 96);
        let named: Vec<(&str, &Known)> = entries
            .iter()
            .filter_map(|(p, k)| Some((p.to_str()?, k)))
            .collect();
        put_varint(&mut raw, named.len() as u64);
        for (p, k) in named {
            for v in [
                p.len() as u64,
                k.size,
                k.modified,
                k.id.0,
                k.id.1,
                u64::from(k.full.is_some()),
            ] {
                put_varint(&mut raw, v);
            }
            raw.extend_from_slice(p.as_bytes());
            raw.extend_from_slice(&k.sample);
            if let Some(f) = k.full {
                raw.extend_from_slice(&f);
            }
        }
        drop(entries);
        let mut out = CACHE_MAGIC.to_vec();
        out.extend(zstd::bulk::compress(&raw, 3)?);
        write_atomically(path, &out)?;
        *saved = generation;
        Ok(true)
    }
}

/// Replace `path` with `bytes` so that a crash or power loss at any point
/// leaves either the old or the new content: write a temporary file in the
/// same directory, flush it to disk, rename it over `path`, then (on Unix)
/// flush the directory so the rename itself is durable. The temporary name
/// is unique per process and call; it is removed again if anything fails.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }
    let mut name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("hash cache path has no file name"))?
        .to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(name);
    let written = (|| {
        let mut f = std::fs::File::create_new(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return written;
    }
    // The new content is in place; making the rename survive a power cut is
    // best effort (some filesystems cannot sync a directory).
    #[cfg(unix)]
    if let Some(dir) = dir {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// One same-size candidate.
struct Cand {
    path: PathBuf,
    size: u64,
    modified: u64,
    known: Option<Known>,
}

/// Identity of an open file: (volume, file index) / (device, inode).
#[cfg(windows)]
fn file_id(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume_serial: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            file: *mut std::ffi::c_void,
            info: *mut ByHandleFileInformation,
        ) -> i32;
    }
    let mut info = ByHandleFileInformation::default();
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0;
    ok.then(|| {
        (
            u64::from(info.volume_serial),
            (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        )
    })
}

#[cfg(unix)]
fn file_id(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata().ok()?;
    Some((m.dev(), m.ino()))
}

#[cfg(not(any(windows, unix)))]
fn file_id(_file: &std::fs::File) -> Option<(u64, u64)> {
    None
}

/// Open the file once: identity plus the hash of both ends (the whole file
/// when it is small enough).
fn sample(c: &Cand) -> Option<Known> {
    let s = sample_file(&c.path, c.size)?;
    Some(Known {
        size: c.size,
        modified: c.modified,
        id: s.id,
        sample: s.sample,
        full: s.full,
    })
}

/// What one read of a file's two ends yields.
pub(crate) struct Sampled {
    /// (volume, file index) on Windows, (device, inode) elsewhere.
    pub(crate) id: (u64, u64),
    /// BLAKE3 of the first and last [`SAMPLE`] bytes.
    pub(crate) sample: [u8; 32],
    /// BLAKE3 of the whole file, when the file is at most two samples long
    /// (so the read covered all of it).
    pub(crate) full: Option<[u8; 32]>,
}

/// Read a file's two ends (the whole file when it is small enough). `None`
/// when the file cannot be read or its length is no longer `size` (it
/// changed under us). Shared by the duplicate search and the persistent
/// index, so both compute the very same sample.
pub(crate) fn sample_file(path: &Path, size: u64) -> Option<Sampled> {
    let mut f = std::fs::File::open(path).ok()?;
    let id = file_id(&f).unwrap_or((u64::MAX, 0));
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; SAMPLE as usize];
    let whole = size <= 2 * SAMPLE;
    if whole {
        let n = std::io::copy(&mut f, &mut h).ok()?;
        if n != size {
            return None; // changed under us
        }
    } else {
        f.read_exact(&mut buf).ok()?;
        h.update(&buf);
        f.seek(SeekFrom::Start(size - SAMPLE)).ok()?;
        f.read_exact(&mut buf).ok()?;
        h.update(&buf);
    }
    let sample = *h.finalize().as_bytes();
    Some(Sampled {
        id,
        sample,
        full: whole.then_some(sample),
    })
}

/// BLAKE3 of the whole file: memory-mapped (several threads for big files),
/// streamed where mapping is not possible.
pub(crate) fn full_hash(path: &Path, size: u64) -> Option<[u8; 32]> {
    full_hash_with(path, size, true)
}

/// [`full_hash`], with several threads per big file only when `parallel`
/// (a spinning disk reads one file front to back, not at scattered offsets).
/// A big file's pages are then dropped from the page cache: a multi-terabyte
/// search must not evict the owner's working set.
pub(crate) fn full_hash_with(path: &Path, size: u64, parallel: bool) -> Option<[u8; 32]> {
    let mut h = blake3::Hasher::new();
    let mapped = if parallel && size >= PARALLEL_HASH_MIN {
        h.update_mmap_rayon(path)
    } else {
        h.update_mmap(path)
    };
    if mapped.is_err() {
        h = blake3::Hasher::new();
        let mut f = std::fs::File::open(path).ok()?;
        std::io::copy(&mut f, &mut h).ok()?;
    }
    if size > PARALLEL_HASH_MIN {
        drop_from_page_cache(path, size);
    }
    Some(*h.finalize().as_bytes())
}

/// Best effort `posix_fadvise(POSIX_FADV_DONTNEED)` over a file just read
/// (the mapping is gone by now). Errors are ignored: this is hygiene only.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn drop_from_page_cache(path: &Path, size: u64) {
    use std::os::unix::io::AsRawFd;
    if let Ok(f) = std::fs::File::open(path) {
        let len = libc::off_t::try_from(size).unwrap_or(0); // 0 = to the end
                                                            // SAFETY: `f` is an open file descriptor for the whole call.
        unsafe {
            libc::posix_fadvise(f.as_raw_fd(), 0, len, libc::POSIX_FADV_DONTNEED);
        }
    }
}

/// No `posix_fadvise` here (macOS, Windows): nothing to do.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn drop_from_page_cache(_path: &Path, _size: u64) {}

/// Run `run` on a scoped thread, handing it a sender for the entries it
/// learns, while the calling thread merges them into `cache`: in one batch at
/// most `every` after the oldest unmerged entry arrived (at once for
/// [`Duration::ZERO`]), and whatever is left when `run` is done. Workers thus
/// pay a channel send per file, never the cache lock — a per-file lock on the
/// shared cache serialises the parallel phases into a convoy. With nothing
/// pending the merger sleeps on the channel instead of waking on a timer.
fn with_merger<R: Send>(
    cache: &HashCache,
    every: Duration,
    run: impl FnOnce(Sender<(PathBuf, Known)>) -> R + Send,
) -> R {
    let (learned, arrivals) = mpsc::channel();
    std::thread::scope(|s| {
        let worker = s.spawn(move || run(learned));
        let mut batch = Vec::new();
        let mut due = Instant::now();
        loop {
            let next = if batch.is_empty() {
                arrivals.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                arrivals.recv_timeout(due.saturating_duration_since(Instant::now()))
            };
            match next {
                Ok(entry) => {
                    if batch.is_empty() {
                        due = Instant::now() + every;
                    }
                    batch.push(entry);
                }
                Err(RecvTimeoutError::Timeout) => {}
                // Every sender is gone: `run` returned or unwound.
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if !batch.is_empty() && Instant::now() >= due {
                cache.merge(std::mem::take(&mut batch));
            }
        }
        cache.merge(batch);
        worker
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// See the module docs. `cache` is read and updated while the search runs:
/// what it learns is merged every [`MERGE_INTERVAL`], so even a cancelled
/// search keeps every sample and full hash it finished. Only a complete
/// search also drops entries under `root` that it no longer saw.
pub(crate) fn find(
    root: &Path,
    min_size: u64,
    opts: &Options,
    progress: &Progress,
    cache: &HashCache,
) -> Vec<DupGroup> {
    find_inner(
        root,
        min_size,
        opts,
        progress,
        cache,
        MERGE_INTERVAL,
        &|_| {},
    )
}

/// [`find`] with the merge interval and a hook told each [`phase`] as it is
/// entered (the seam tests use to cancel at a known point).
fn find_inner(
    root: &Path,
    min_size: u64,
    opts: &Options,
    progress: &Progress,
    cache: &HashCache,
    merge_every: Duration,
    entered: &dyn Fn(u64),
) -> Vec<DupGroup> {
    // 1. Every file big enough; duplicates group by *logical* (content) size.
    // Trash folders hold what was just reclaimed: never duplicates to act on.
    let opts = &Options {
        exclude: opts
            .exclude
            .iter()
            .cloned()
            .chain(TRASH_DIRS.iter().map(|d| d.to_string()))
            .collect(),
        ..opts.clone()
    };
    let mut cands: Vec<Cand> = Vec::new();
    let _ = walk(root, opts, progress, |b| {
        for (f, modified) in b.files.into_iter().zip(b.modified) {
            if f.logical_size >= min_size && f.logical_size > 0 {
                cands.push(Cand {
                    path: f.path,
                    size: f.logical_size,
                    modified,
                    known: None,
                });
            }
        }
    });
    if progress.cancelled() {
        return Vec::new();
    }
    let mut per_size: HashMap<u64, u32> = HashMap::new();
    for c in &cands {
        *per_size.entry(c.size).or_default() += 1;
    }
    cands.retain(|c| per_size[&c.size] > 1);
    drop(per_size);

    // 2. Identity and both ends, file by file in parallel (cache first);
    // what had to be read is handed to the cache as it is learned.
    let sample_bytes = cands.iter().map(|c| c.size.min(2 * SAMPLE)).sum();
    progress.enter(phase::SAMPLE, cands.len() as u64, sample_bytes);
    entered(phase::SAMPLE);
    with_merger(cache, merge_every, |learned| {
        cands.par_iter_mut().for_each_with(learned, |learned, c| {
            if progress.cancelled() {
                return;
            }
            c.known = cache.get(&c.path, c.size, c.modified).or_else(|| {
                let k = sample(c)?;
                let _ = learned.send((c.path.clone(), k));
                Some(k)
            });
            progress.files.fetch_add(1, Ordering::Relaxed);
            progress
                .bytes
                .fetch_add(c.size.min(2 * SAMPLE), Ordering::Relaxed);
        });
    });
    if progress.cancelled() {
        return Vec::new();
    }

    // Collapse hard links (one physical file reached by several paths), then
    // keep only files whose size and both ends match another file.
    let mut seen_ids: HashMap<(u64, (u64, u64)), ()> = HashMap::new();
    let mut by_sample: HashMap<(u64, [u8; 32]), Vec<usize>> = HashMap::new();
    for (i, c) in cands.iter().enumerate() {
        let Some(k) = c.known else { continue };
        if k.id.0 != u64::MAX && seen_ids.insert((c.size, k.id), ()).is_some() {
            continue;
        }
        by_sample.entry((c.size, k.sample)).or_default().push(i);
    }
    let mut alike: Vec<usize> = by_sample
        .into_values()
        .filter(|v| v.len() > 1)
        .flatten()
        .collect();

    // 3. Whole-file hashes for whatever still looks alike, in parallel.
    let need: Vec<usize> = alike
        .iter()
        .copied()
        .filter(|&i| cands[i].known.is_some_and(|k| k.full.is_none()))
        .collect();
    let need_bytes = need.iter().map(|&i| cands[i].size).sum();
    progress.enter(phase::HASH, need.len() as u64, need_bytes);
    entered(phase::HASH);
    let fulls: Vec<(usize, Option<[u8; 32]>)> = with_merger(cache, merge_every, |learned| {
        need.par_iter()
            .map_with(learned, |learned, &i| {
                if progress.cancelled() {
                    return (i, None);
                }
                let c = &cands[i];
                let h = full_hash(&c.path, c.size);
                if let (Some(full), Some(k)) = (h, c.known) {
                    let k = Known {
                        full: Some(full),
                        ..k
                    };
                    let _ = learned.send((c.path.clone(), k));
                }
                progress.files.fetch_add(1, Ordering::Relaxed);
                progress.bytes.fetch_add(c.size, Ordering::Relaxed);
                (i, h)
            })
            .collect()
    });
    for (i, h) in fulls {
        if let Some(k) = cands[i].known.as_mut() {
            k.full = h;
        }
    }

    // Everything learned is in the cache by now. A cancelled search has not
    // seen everything under `root`, so nothing it missed counts as stale;
    // a complete one replaces what is known there with what it found (also
    // for files that turned out unique), dropping files gone or changed.
    if progress.cancelled() {
        return Vec::new();
    }
    cache.replace_under(
        root,
        cands
            .iter()
            .filter_map(|c| Some((c.path.clone(), c.known?)))
            .collect(),
    );

    // Group by size and full hash.
    alike.sort_unstable();
    let mut by_full: HashMap<(u64, [u8; 32]), Vec<PathBuf>> = HashMap::new();
    for i in alike {
        let c = &cands[i];
        if let Some(full) = c.known.and_then(|k| k.full) {
            by_full
                .entry((c.size, full))
                .or_default()
                .push(c.path.clone());
        }
    }
    let mut groups: Vec<DupGroup> = by_full
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|((size, full), mut files)| {
            files.sort(); // stable order so "keep the first" is deterministic
            DupGroup {
                hash: blake3::Hash::from(full).to_hex().to_string(),
                size,
                wasted: size.saturating_mul(files.len() as u64 - 1),
                files,
            }
        })
        .collect();
    groups.sort_by(|a, b| b.wasted.cmp(&a.wasted).then_with(|| a.files.cmp(&b.files)));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-dupes-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn same_ends_different_middle_is_not_a_duplicate() {
        // Big files that agree in their first and last 16 KB but differ in
        // the middle must survive the quick check and fail the full hash.
        let d = tmpdir("middle");
        let mut a = vec![7u8; 200_000];
        let b = a.clone();
        write(&d.join("a.bin"), &a);
        write(&d.join("b.bin"), &b); // true duplicate of a
        a[100_000] = 8;
        write(&d.join("c.bin"), &a); // same ends, other middle
        let cache = HashCache::new();
        let g = find(&d, 1, &Options::default(), &Progress::default(), &cache);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].files, vec![d.join("a.bin"), d.join("b.bin")]);
        assert_eq!(g[0].wasted, 200_000);
        assert_eq!(cache.len(), 3);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_recycle_bin_is_not_searched() {
        // A copy sent to the recycle bin must not reappear as a duplicate of
        // the file that was kept.
        let d = tmpdir("trash");
        write(&d.join("keep/a.bin"), &[3u8; 50_000]);
        write(
            &d.join("$Recycle.Bin/S-1-5-21/$RABC123.bin"),
            &[3u8; 50_000],
        );
        write(&d.join(".Trash-1000/files/a.bin"), &[3u8; 50_000]);
        let g = find(
            &d,
            1,
            &Options::default(),
            &Progress::default(),
            &HashCache::new(),
        );
        assert!(g.is_empty(), "{g:?}");
        // Searching inside the trash itself still works.
        write(
            &d.join("$Recycle.Bin/S-1-5-21/$RDEF456.bin"),
            &[3u8; 50_000],
        );
        let inside = find(
            &d.join("$Recycle.Bin"),
            1,
            &Options::default(),
            &Progress::default(),
            &HashCache::new(),
        );
        assert_eq!(inside.len(), 1);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn cache_round_trip_and_reuse() {
        let d = tmpdir("cache");
        write(&d.join("x/1.bin"), &[1u8; 50_000]);
        write(&d.join("y/1.bin"), &[1u8; 50_000]);
        write(&d.join("small1"), b"hello");
        write(&d.join("small2"), b"hello");
        let cache = HashCache::new();
        let first = find(&d, 1, &Options::default(), &Progress::default(), &cache);
        assert_eq!(first.len(), 2);

        let file = d.join("cache/hashes.bin");
        cache.save(&file).unwrap();
        let back = HashCache::load(&file);
        assert_eq!(back.len(), 4);

        // With everything cached the second search reads no file content:
        // the hash phase has nothing to do and the result is the same.
        let p = Progress::default();
        let again = find(&d, 1, &Options::default(), &p, &back);
        assert_eq!(again, first);
        assert_eq!(p.phase(), phase::HASH);
        assert_eq!(p.total_files(), 0);

        // A changed file is looked at again.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&d.join("y/1.bin"), &[2u8; 50_000]);
        let changed = find(&d, 1, &Options::default(), &Progress::default(), &back);
        assert_eq!(changed.len(), 1);

        // Damaged or foreign cache files just mean an empty cache.
        std::fs::write(&file, b"garbage").unwrap();
        assert!(HashCache::load(&file).is_empty());
        assert!(HashCache::load(&d.join("missing")).is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    /// A made-up entry: only the fields the cache compares matter here.
    fn known(size: u64, tag: u8) -> Known {
        Known {
            size,
            modified: 1,
            id: (1, u64::from(tag)),
            sample: [tag; 32],
            full: None,
        }
    }

    fn paths(cache: &HashCache) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = cache.entries.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    /// Poll `cond` until it holds or `limit` passes — the limit only bounds
    /// a failing test, a passing one returns as soon as `cond` is true.
    fn eventually(limit: Duration, cond: impl Fn() -> bool) -> bool {
        let end = std::time::Instant::now() + limit;
        while std::time::Instant::now() < end {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        cond()
    }

    #[test]
    fn merge_keeps_entries_it_does_not_mention() {
        // A checkpoint must not forget what the run has not reached yet;
        // only the final clean-up (replace_under) drops entries under root.
        let root = Path::new("/r");
        let seed = || {
            let c = HashCache::new();
            c.replace_under(
                Path::new("/"),
                vec![
                    (root.join("a"), known(10, 1)),
                    (root.join("b"), known(10, 2)),
                    (PathBuf::from("/elsewhere/c"), known(10, 3)),
                ],
            );
            c
        };
        let merged = seed();
        merged.merge(vec![(root.join("a"), known(10, 9))]);
        assert_eq!(
            paths(&merged),
            vec![
                PathBuf::from("/elsewhere/c"),
                root.join("a"),
                root.join("b")
            ]
        );
        assert_eq!(merged.get(&root.join("a"), 10, 1), Some(known(10, 9)));

        let replaced = seed();
        replaced.replace_under(root, vec![(root.join("a"), known(10, 9))]);
        assert_eq!(
            paths(&replaced),
            vec![PathBuf::from("/elsewhere/c"), root.join("a")]
        );
    }

    /// Three 200 KB candidates: a and b are duplicates, c shares their ends
    /// but not its middle, so all three need the full hash.
    fn big_trio(label: &str) -> PathBuf {
        let d = tmpdir(label);
        let mut a = vec![7u8; 200_000];
        write(&d.join("a.bin"), &a);
        write(&d.join("b.bin"), &a);
        a[100_000] = 8;
        write(&d.join("c.bin"), &a);
        d
    }

    #[test]
    fn cancel_after_sampling_keeps_the_samples() {
        // Sampling a big tree is hours of seeks; a cancel (or crash) after it
        // must not throw that away just because no full hash was taken yet.
        // Cancelling as the hash phase starts: every sample is taken, no
        // full hash is.
        let d = big_trio("cancel-sample");
        let cache = HashCache::new();
        let p = Progress::default();
        let g = find_inner(
            &d,
            1,
            &Options::default(),
            &p,
            &cache,
            MERGE_INTERVAL,
            &|ph| {
                if ph == phase::HASH {
                    p.cancel()
                }
            },
        );
        assert!(g.is_empty());
        assert_eq!(cache.len(), 3, "{:?}", paths(&cache));
        for name in ["a.bin", "b.bin", "c.bin"] {
            let path = d.join(name);
            let k = cache.entries.lock().unwrap()[&path];
            assert_eq!(k.size, 200_000);
            assert_eq!(k.full, None, "{name} was never fully hashed");
            // The next search takes it from the cache instead of reopening it.
            assert_eq!(cache.get(&path, k.size, k.modified), Some(k));
        }

        // Finishing later gives exactly the uncached result.
        let resumed = find(&d, 1, &Options::default(), &Progress::default(), &cache);
        let fresh = find(
            &d,
            1,
            &Options::default(),
            &Progress::default(),
            &HashCache::new(),
        );
        assert_eq!(resumed, fresh);
        assert_eq!(resumed.len(), 1);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_cancelled_search_never_drops_earlier_knowledge() {
        // Without a complete walk and hash pass nothing proves an old entry
        // under the root stale, so a cancelled run only ever adds.
        let d = big_trio("cancel-keep");
        let old = d.join("sub/old.bin");
        for cancel_at in [phase::SAMPLE, phase::HASH] {
            let cache = HashCache::new();
            cache.replace_under(Path::new("/"), vec![(old.clone(), known(5, 4))]);
            let p = Progress::default();
            find_inner(
                &d,
                1,
                &Options::default(),
                &p,
                &cache,
                MERGE_INTERVAL,
                &|ph| {
                    if ph == cancel_at {
                        p.cancel()
                    }
                },
            );
            assert!(
                paths(&cache).contains(&old),
                "cancel in phase {cancel_at}: {:?}",
                paths(&cache)
            );

            // A complete search is the clean-up: what is gone drops out.
            find(&d, 1, &Options::default(), &Progress::default(), &cache);
            assert!(!paths(&cache).contains(&old));
            assert_eq!(cache.len(), 3);
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn merging_more_often_changes_no_result() {
        let d = big_trio("merge-often");
        write(&d.join("s1"), b"tiny");
        write(&d.join("s2"), b"tiny");
        let plain = find(
            &d,
            1,
            &Options::default(),
            &Progress::default(),
            &HashCache::new(),
        );
        let cache = HashCache::new();
        let eager = find_inner(
            &d,
            1,
            &Options::default(),
            &Progress::default(),
            &cache,
            Duration::ZERO,
            &|_| {},
        );
        assert_eq!(eager, plain);
        assert_eq!(eager.len(), 2);
        assert_eq!(cache.len(), 5);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn merger_hands_knowledge_over_while_work_runs() {
        let cache = HashCache::new();
        let seen_mid_run = with_merger(&cache, Duration::ZERO, |tx| {
            tx.send((PathBuf::from("/m/1"), known(1, 1))).unwrap();
            eventually(Duration::from_secs(5), || cache.len() == 1)
        });
        assert!(seen_mid_run, "learned entry only landed after the run");
    }

    #[test]
    fn merger_hands_the_rest_over_at_the_end() {
        // A long interval must neither lose the tail nor delay the return.
        let cache = HashCache::new();
        let start = std::time::Instant::now();
        with_merger(&cache, Duration::from_secs(3600), |tx| {
            for i in 0..3u8 {
                tx.send((PathBuf::from(format!("/m/{i}")), known(1, i)))
                    .unwrap();
            }
        });
        assert_eq!(cache.len(), 3);
        assert!(start.elapsed() < Duration::from_secs(60));
    }

    #[test]
    fn save_if_changed_writes_only_news() {
        let d = tmpdir("ifchanged");
        let file = d.join("hashes.bin");
        let cache = HashCache::new();
        cache.merge(vec![(PathBuf::from("/x/1"), known(1, 1))]);
        assert!(cache.save_if_changed(&file).unwrap());
        assert!(!cache.save_if_changed(&file).unwrap());
        // Merging what is already known is no news either.
        cache.merge(vec![(PathBuf::from("/x/1"), known(1, 1))]);
        assert!(!cache.save_if_changed(&file).unwrap());
        cache.merge(vec![(PathBuf::from("/x/2"), known(1, 2))]);
        assert!(cache.save_if_changed(&file).unwrap());
        assert_eq!(HashCache::load(&file).len(), 2);
        // A loaded cache matches its file until it learns something.
        assert!(!HashCache::load(&file).save_if_changed(&file).unwrap());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn concurrent_saves_and_merges_leave_a_valid_file() {
        // Checkpoint timer, final save and the search's merges all meet on
        // one cache: every save must succeed and the file must stay whole.
        let d = tmpdir("concurrent");
        let file = d.join("hashes.bin");
        let cache = HashCache::new();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..25 {
                        cache.save(&file).unwrap();
                        assert!(HashCache::read(&file).is_ok());
                    }
                });
            }
            s.spawn(|| {
                for i in 0..200u32 {
                    let p = PathBuf::from(format!("/c/{i}"));
                    cache.merge(vec![(p, known(u64::from(i) + 1, 1))]);
                }
            });
        });
        cache.save(&file).unwrap();
        assert_eq!(HashCache::read(&file).unwrap().len(), 200);
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("hashes.bin")]);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_failed_save_leaves_no_temporary_file() {
        let d = tmpdir("savefail");
        let taken = d.join("taken");
        std::fs::create_dir_all(&taken).unwrap();
        let cache = HashCache::new();
        cache.merge(vec![(PathBuf::from("/x/1"), known(1, 1))]);
        assert!(cache.save(&taken).is_err());
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("taken")]);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn checkpoints_are_written_while_the_job_runs_and_stop_with_it() {
        let d = tmpdir("checkpoint");
        let file = d.join("hashes.bin");
        let cache = HashCache::new();
        let every = Duration::from_millis(5);
        let answer = cache.checkpoint_while(&file, every, || {
            cache.merge(vec![(PathBuf::from("/k/1"), known(1, 1))]);
            let saved = eventually(Duration::from_secs(5), || HashCache::read(&file).is_ok());
            assert!(saved, "no checkpoint was written during the job");
            42
        });
        assert_eq!(answer, 42);
        assert_eq!(HashCache::load(&file).len(), 1);

        // The checkpoint thread ends with the job: nothing writes any more.
        std::fs::remove_file(&file).unwrap();
        cache.merge(vec![(PathBuf::from("/k/2"), known(1, 2))]);
        std::thread::sleep(every * 10);
        assert!(!file.exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_panicking_job_does_not_hang_its_checkpoints() {
        let d = tmpdir("checkpoint-panic");
        let cache = HashCache::new();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.checkpoint_while(&d.join("h.bin"), Duration::from_secs(3600), || {
                panic!("job failed")
            })
        }));
        assert!(r.is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    /// A spinning disk dictates the full-hash phase: a small reader pool,
    /// not the machine-wide parallel storm an SSD can take.
    #[test]
    fn a_rotational_root_gets_a_small_reader_pool() {
        assert_eq!(
            full_hash_workers(true),
            Some(crate::rotational::ROTATIONAL_HASH_THREADS)
        );
        assert_eq!(full_hash_workers(false), None);
    }

    /// The read order on a spinning disk is the path order: files written
    /// together sit together, the cheap stand-in for physical order (no
    /// FIEMAP) — the same rule the index batches follow.
    #[test]
    fn rotational_reads_follow_path_order() {
        let mk = |p: &str| Cand {
            path: PathBuf::from(p),
            size: 1,
            modified: 0,
            known: None,
        };
        let cands = vec![mk("b/2"), mk("a/1"), mk("c/3")];
        let mut need = vec![0, 1, 2];
        order_for_seeks(&mut need, &cands);
        assert_eq!(need, vec![1, 0, 2]);
    }
}
