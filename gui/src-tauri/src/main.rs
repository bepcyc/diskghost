// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use diskghost_core::{
    diff, disk_space, find_duplicates_cached, index_remote, index_scan, is_mount_point, reclaim,
    remove_path, validate_globs, DiffReport, DirSize, DupGroup, FileStats, HashCache, Index,
    Options, Progress, ReclaimAction, ReclaimReport, RemoveMode, RemoveReport, Role, ScanReport,
    ScanSummary, ScanTree, Snapshot, CHECKPOINT_INTERVAL, FIND_DEFAULT_LIMIT, FIND_MAX_LIMIT,
};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, LogicalSize, Manager};
use tauri_plugin_dialog::DialogExt;

mod helper;
mod path_env;

/// Sub-folders shown inside each folder on the map's second level.
const NESTED_PER_FOLDER: usize = 12;

/// Snapshots kept per scanned root unless the UI asks otherwise.
const DEFAULT_SNAPSHOTS_PER_ROOT: usize = 10;

/// The tree of one scan job plus what has been derived from it.
struct Cached {
    tree: ScanTree,
    /// Views already computed, by folder and list length — going back to a
    /// folder is answered without touching the tree again.
    views: HashMap<(PathBuf, usize), Arc<View>>,
    /// The tree captured as a snapshot (for comparisons), once taken. A
    /// mutex of its own, so it can be filled under the tree's read lock and a
    /// second caller waits for the first capture instead of repeating it.
    current: Mutex<Option<Arc<Snapshot>>>,
}

impl Cached {
    /// Forget everything derived from the tree (it was changed).
    fn changed(&mut self) {
        self.views.clear();
        if let Ok(c) = self.current.get_mut() {
            *c = None;
        }
    }
}

type Shared = Arc<RwLock<Cached>>;

/// The `Progress` of every running operation by job id (so `cancel` can flag
/// one of them), and the tree of every scan job by job id. Each tree has its
/// own lock: reading one scan never waits for another.
struct AppState {
    jobs: Mutex<HashMap<u64, Progress>>,
    trees: Mutex<HashMap<u64, Shared>>,
    /// File hashes of earlier duplicate searches (loaded on first use, saved
    /// after each search), so searching again only reads changed files.
    hashes: Mutex<Option<Arc<HashCache>>>,
    /// The elevated MFT helper, once started.
    helper: Mutex<Option<Arc<helper::Helper>>>,
}

/// Payload pushed to the frontend as a scan/search runs.
#[derive(Clone, Serialize)]
struct ProgressPayload {
    job: u64,
    files: u64,
    bytes: u64,
    /// Duplicate search phase (0 listing, 1 comparing ends, 2 hashing).
    phase: u64,
    total_files: u64,
    total_bytes: u64,
}

impl ProgressPayload {
    fn of(job: u64, p: &Progress) -> ProgressPayload {
        ProgressPayload {
            job,
            files: p.files(),
            bytes: p.bytes(),
            phase: p.phase(),
            total_files: p.total_files(),
            total_bytes: p.total_bytes(),
        }
    }
}

/// Walk options coming from the frontend.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(default)]
struct WalkOpts {
    exclude: Vec<String>,
    max_depth: Option<usize>,
    follow_symlinks: bool,
    /// `None` = automatic.
    threads: Option<usize>,
    /// Read the NTFS Master File Table (needs administrator rights).
    mft: bool,
    /// Set by the backend per scan root: a scan rooted on a mount point
    /// (every "drive" is one) stays on that filesystem.
    same_file_system: bool,
}

impl WalkOpts {
    fn to_options(&self) -> Options {
        Options {
            max_depth: self.max_depth,
            follow_symlinks: self.follow_symlinks,
            exclude: self.exclude.clone(),
            threads: self.threads,
            mft: self.mft,
            same_file_system: self.same_file_system,
            // Every GUI walk (scan, drill-in, duplicates) stays off network
            // and FUSE shares met below its root or through a link: one such
            // scan reached gvfs sftp and walked a remote `/sys` for ten hours.
            // A share chosen as the root is still scanned.
            skip_remote: true,
            // The index refuses an sshfs root and names its ssh:// URL.
            allow_sshfs: false,
        }
    }
}

/// A scan report plus what the dashboard draws next to it.
#[derive(Serialize)]
struct View {
    #[serde(flatten)]
    report: ScanReport,
    /// The biggest sub-folders of each of `children`, in the same order — the
    /// second level of the map.
    nested: Vec<Vec<DirSize>>,
    /// Bytes per extension and per size range below the viewed folder.
    stats: FileStats,
    /// How the tree was read: "mft" or "walk".
    engine: &'static str,
    /// Why the MFT was asked for but not used.
    mft_fallback: Option<String>,
}

fn view_of(tree: &ScanTree, path: &Path, top: usize) -> Option<View> {
    let report = tree.report(path, top)?;
    let nested = report
        .children
        .iter()
        .map(|c| {
            tree.children(&c.path, NESTED_PER_FOLDER)
                .unwrap_or_default()
        })
        .collect();
    let stats = tree.file_stats(path)?;
    Some(View {
        report,
        nested,
        stats,
        engine: tree.engine(),
        mft_fallback: tree.mft_fallback().map(str::to_string),
    })
}

/// Run blocking work off the async runtime, so no command ever stalls the UI.
async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

/// The tree of job `job`.
fn tree_of(app: &tauri::AppHandle, job: u64) -> Result<Shared, String> {
    let state = app.state::<AppState>();
    let trees = state.trees.lock().map_err(|e| e.to_string())?;
    trees
        .get(&job)
        .cloned()
        .ok_or_else(|| "this scan is no longer in memory — rescan it".to_string())
}

/// Every cached tree, for operations that touch all of them.
fn all_trees(app: &tauri::AppHandle) -> Vec<Shared> {
    let state = app.state::<AppState>();
    let trees = state.trees.lock();
    trees
        .map(|t| t.values().cloned().collect())
        .unwrap_or_default()
}

/// Emit a `progress` event every ~120 ms until `done` is set.
fn spawn_emitter(app: tauri::AppHandle, job: u64, progress: Progress, done: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            let _ = app.emit("progress", ProgressPayload::of(job, &progress));
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });
}

/// A running operation: registered as cancellable and reporting progress
/// until dropped — even if the job panics and the command returns early.
struct Running {
    app: tauri::AppHandle,
    job: u64,
    progress: Progress,
    done: Arc<AtomicBool>,
}

impl Running {
    fn start(app: &tauri::AppHandle, job: u64) -> Running {
        let progress = Progress::default();
        if let Ok(mut jobs) = app.state::<AppState>().jobs.lock() {
            jobs.insert(job, progress.clone());
        }
        let done = Arc::new(AtomicBool::new(false));
        spawn_emitter(app.clone(), job, progress.clone(), done.clone());
        Running {
            app: app.clone(),
            job,
            progress,
            done,
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Ok(mut jobs) = self.app.state::<AppState>().jobs.lock() {
            jobs.remove(&self.job);
        }
        // A last event with the final counts.
        let _ = self
            .app
            .emit("progress", ProgressPayload::of(self.job, &self.progress));
    }
}

/// Where snapshots of `root` live: one folder per scanned directory under the
/// app's data dir, files named by their Unix timestamp.
fn snapshot_dir(app: &tauri::AppHandle, root: &Path) -> Result<PathBuf, String> {
    let base = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(base.join("snapshots").join(Snapshot::root_key(root)))
}

/// Stored snapshots in `dir`: (timestamp, file), newest first. Format 2
/// snapshots end in `.dgs`, older JSON ones in `.json`.
fn snapshot_files(dir: &Path) -> Vec<(u64, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension()?.to_str()?;
            if ext != "dgs" && ext != "json" {
                return None;
            }
            Some((path.file_stem()?.to_str()?.parse().ok()?, path))
        })
        .collect();
    out.sort_unstable_by_key(|x| std::cmp::Reverse(x.0));
    out
}

/// Save `snap` into `dir` and drop the oldest ones beyond `keep`.
fn store_snapshot(dir: &Path, snap: &Snapshot, keep: usize) -> Result<(), String> {
    snap.save(&dir.join(format!("{}.dgs", snap.taken_at)))
        .map_err(|e| e.to_string())?;
    for (_, old) in snapshot_files(dir).into_iter().skip(keep.max(1)) {
        let _ = std::fs::remove_file(old);
    }
    Ok(())
}

/// Capture the tree as a snapshot once and keep it for comparisons.
fn current_snapshot(shared: &Shared) -> Result<Arc<Snapshot>, String> {
    let c = shared.read().map_err(|e| e.to_string())?;
    let mut current = c.current.lock().map_err(|e| e.to_string())?;
    if let Some(s) = current.as_ref() {
        return Ok(s.clone());
    }
    let snap = Arc::new(Snapshot::capture(&c.tree));
    *current = Some(snap.clone());
    Ok(snap)
}

/// Walk `path` as job `job` (live progress, cancellable), keep the tree for
/// the job and return the view of its root. A complete walk is also recorded
/// as a snapshot — in the background, so the result shows up at once.
#[tauri::command]
async fn scan_dir(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    top: usize,
    opts: WalkOpts,
    keep_snapshots: Option<usize>,
) -> Result<View, String> {
    let running = Running::start(&app, job);
    blocking(move || {
        if is_remote_url(&path) {
            // Listed on the host itself (one filesystem, like a drive);
            // the walk options do not apply there. The host's own error
            // ("not found", unreachable, no shell) is shown as it is.
            let bin = agent_binary();
            let listing = diskghost_core::remote_listing_using(
                &path,
                diskghost_core::remote::AgentMode::Auto,
                bin.as_ref(),
                &running.progress,
            )
            .map_err(|e| e.to_string())?;
            let tree = listing.tree();
            drop(running);
            return keep_tree(&app, job, tree, None, top, keep_snapshots);
        }
        let requested = PathBuf::from(&path);
        if !requested.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        // A drive stays on its own filesystem: scanning `/` must not walk the
        // FUSE/network mounts below it (a scan once descended into gvfs
        // sftp/smb mounts, counted remote trees and sat in remote `read_dir`s
        // long past Cancel).
        let mut opts = opts;
        opts.same_file_system = is_mount_point(&requested);
        let (tree, snap) = build_tree(&app, job, &requested, &opts, &running.progress);
        drop(running);
        keep_tree(&app, job, tree, snap, top, keep_snapshots)
    })
    .await
}

/// Keep `tree` as job `job`'s and return the view of its root. A complete
/// tree is also recorded as a snapshot — in the background, so the result
/// shows up at once.
fn keep_tree(
    app: &tauri::AppHandle,
    job: u64,
    tree: ScanTree,
    snap: Option<Snapshot>,
    top: usize,
    keep_snapshots: Option<usize>,
) -> Result<View, String> {
    let view = view_of(&tree, tree.root(), top).ok_or("scan produced no tree")?;
    let complete = tree.complete();
    let root = tree.root().to_path_buf();
    let shared: Shared = Arc::new(RwLock::new(Cached {
        tree,
        views: HashMap::new(),
        current: Mutex::new(snap.map(Arc::new)),
    }));
    if let Ok(mut trees) = app.state::<AppState>().trees.lock() {
        trees.insert(job, shared.clone());
    }
    // A cancelled, partial or depth-limited scan is a partial view:
    // browsable, but never recorded as a point in the folder's history.
    if complete {
        let app = app.clone();
        std::thread::spawn(move || {
            let Ok(snap) = current_snapshot(&shared) else {
                return;
            };
            let keep = keep_snapshots.unwrap_or(DEFAULT_SNAPSHOTS_PER_ROOT);
            let saved = snapshot_dir(&app, &root).and_then(|dir| store_snapshot(&dir, &snap, keep));
            let _ = app.emit("snapshot-saved", (job, saved.err()));
        });
    }
    Ok(view)
}

/// Walk `path`. The MFT engine needs administrator rights, which this window
/// never has (Windows would block drag & drop into it): unless the app was
/// started elevated anyway, the scan runs in the elevated helper, which also
/// hands back the snapshot it made. Any problem there falls back to the
/// directory walk, with the reason recorded on the tree.
fn build_tree(
    app: &tauri::AppHandle,
    job: u64,
    path: &Path,
    opts: &WalkOpts,
    progress: &Progress,
) -> (ScanTree, Option<Snapshot>) {
    if !opts.mft || is_elevated() {
        return (ScanTree::build(path, &opts.to_options(), progress), None);
    }
    match mft_helper(app).and_then(|h| h.scan(job, path, opts, progress)) {
        Ok((tree, snap, _)) => (tree, Some(snap)),
        Err(why) => {
            let walk_opts = Options {
                mft: false,
                ..opts.to_options()
            };
            let mut tree = ScanTree::build(path, &walk_opts, progress);
            tree.set_engine(false, Some(why));
            (tree, None)
        }
    }
}

/// The running elevated helper, started (with the user's consent) on first use.
fn mft_helper(app: &tauri::AppHandle) -> Result<Arc<helper::Helper>, String> {
    let state = app.state::<AppState>();
    let mut slot = state.helper.lock().map_err(|e| e.to_string())?;
    if let Some(h) = slot.as_ref().filter(|h| h.alive()) {
        return Ok(h.clone());
    }
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("mft-helper");
    let h = Arc::new(helper::Helper::start(dir)?);
    *slot = Some(h.clone());
    Ok(h)
}

/// The view of `path` inside job `job`'s tree — never walks the disk.
#[tauri::command]
async fn view(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    top: usize,
) -> Result<Arc<View>, String> {
    blocking(move || {
        let shared = tree_of(&app, job)?;
        let key = (PathBuf::from(&path), top);
        if let Some(v) = shared.read().map_err(|e| e.to_string())?.views.get(&key) {
            return Ok(v.clone());
        }
        let v = {
            let c = shared.read().map_err(|e| e.to_string())?;
            Arc::new(
                view_of(&c.tree, &key.0, top).ok_or_else(|| format!("not in this scan: {path}"))?,
            )
        };
        if let Ok(mut c) = shared.write() {
            c.views.insert(key, v.clone());
        }
        Ok(v)
    })
    .await
}

/// Drop job `job`'s tree (its scan was closed), freeing its memory.
#[tauri::command]
async fn forget_tree(app: tauri::AppHandle, job: u64) {
    // The last reference may hold millions of nodes: free them off-thread.
    let removed = app
        .state::<AppState>()
        .trees
        .lock()
        .ok()
        .and_then(|mut t| t.remove(&job));
    let _ = tauri::async_runtime::spawn_blocking(move || drop(removed)).await;
}

/// One stored snapshot.
#[derive(Serialize)]
struct SnapshotInfo {
    taken_at: u64,
    /// Size of the file on disk.
    bytes: u64,
    /// Whether it records files (format 2) or only folders.
    files: bool,
}

/// Snapshot history of the root job `job` scanned.
#[derive(Serialize)]
struct SnapshotList {
    root: String,
    /// Newest first.
    snapshots: Vec<SnapshotInfo>,
}

#[tauri::command]
async fn list_snapshots(app: tauri::AppHandle, job: u64) -> Result<SnapshotList, String> {
    blocking(move || {
        let root = tree_of(&app, job)?
            .read()
            .map_err(|e| e.to_string())?
            .tree
            .root()
            .to_path_buf();
        let dir = snapshot_dir(&app, &root)?;
        let snapshots = snapshot_files(&dir)
            .into_iter()
            .map(|(taken_at, file)| SnapshotInfo {
                taken_at,
                bytes: std::fs::metadata(&file).map_or(0, |m| m.len()),
                files: file.extension().is_some_and(|e| e == "dgs"),
            })
            .collect();
        Ok(SnapshotList {
            root: root.to_string_lossy().into_owned(),
            snapshots,
        })
    })
    .await
}

/// Delete the snapshot taken at `taken_at` of job `job`'s root.
#[tauri::command]
async fn delete_snapshot(app: tauri::AppHandle, job: u64, taken_at: u64) -> Result<(), String> {
    blocking(move || {
        let root = tree_of(&app, job)?
            .read()
            .map_err(|e| e.to_string())?
            .tree
            .root()
            .to_path_buf();
        let dir = snapshot_dir(&app, &root)?;
        for (ts, file) in snapshot_files(&dir) {
            if ts == taken_at {
                std::fs::remove_file(file).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    })
    .await
}

/// What changed under `path` in job `job`'s tree since the snapshot taken at
/// `taken_at`: folders and — for snapshots that record them — single files.
#[tauri::command]
async fn diff_since(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    taken_at: u64,
    top: usize,
) -> Result<DiffReport, String> {
    blocking(move || {
        let shared = tree_of(&app, job)?;
        let root = shared
            .read()
            .map_err(|e| e.to_string())?
            .tree
            .root()
            .to_path_buf();
        let dir = snapshot_dir(&app, &root)?;
        let file = snapshot_files(&dir)
            .into_iter()
            .find(|(ts, _)| *ts == taken_at)
            .map(|(_, f)| f)
            .ok_or("that snapshot no longer exists")?;
        let old = Snapshot::load(&file).map_err(|e| e.to_string())?;
        let now = current_snapshot(&shared)?;
        let mut report = diff(&old, &now, Path::new(&path)).map_err(|e| e.to_string())?;
        report.truncate(top);
        Ok(report)
    })
    .await
}

/// Find duplicate files as job `job` (progress emitted live).
#[tauri::command]
async fn find_dupes(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    mb: u64,
    opts: WalkOpts,
) -> Result<Vec<DupGroup>, String> {
    let running = Running::start(&app, job);
    blocking(move || {
        if is_remote_url(&path) {
            return Err(format!(
                "{path}: duplicates on a remote host are found with \
                 `diskghost index add {path}` and `diskghost dupes --index`"
            ));
        }
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        let file = hash_cache_file(&app)?;
        let cache = {
            let state = app.state::<AppState>();
            let mut slot = state.hashes.lock().map_err(|e| e.to_string())?;
            slot.get_or_insert_with(|| Arc::new(HashCache::load(&file)))
                .clone()
        };
        // Checkpoint hashes.bin while the search runs, so a crash or kill
        // keeps what it hashed; the checkpoint thread ends with the search.
        let groups = cache.checkpoint_while(&file, CHECKPOINT_INTERVAL, || {
            find_duplicates_cached(
                &p,
                mb * 1024 * 1024,
                &opts.to_options(),
                &running.progress,
                &cache,
            )
        });
        drop(running);
        // Persist in the background; the result is shown right away.
        std::thread::spawn(move || {
            let _ = cache.save(&file);
        });
        Ok(groups)
    })
    .await
}

/// Where duplicate-search hashes are kept between runs.
fn hash_cache_file(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let base = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(base.join("hashes.bin"))
}

/// Request cancellation of job `job`, if it is still running.
#[tauri::command]
fn cancel(state: tauri::State<'_, AppState>, job: u64) {
    if let Ok(jobs) = state.jobs.lock() {
        if let Some(p) = jobs.get(&job) {
            p.cancel();
        }
    }
}

/// Take paths that are gone from disk out of every tree holding them.
fn forget_paths(app: &tauri::AppHandle, paths: &[PathBuf]) {
    for shared in all_trees(app) {
        let Ok(mut c) = shared.write() else { continue };
        let root = c.tree.root().to_path_buf();
        let mut touched = false;
        for p in paths.iter().filter(|p| p.starts_with(&root) && !p.exists()) {
            touched |= c.tree.remove(p);
        }
        if touched {
            c.tree.refresh_disk_space();
            c.changed();
        }
    }
}

/// One reclaim job: keep `keep`, act on each path in `remove` (all `size` bytes).
#[derive(Deserialize)]
struct ReclaimJob {
    keep: String,
    remove: Vec<String>,
    size: u64,
}

/// Reclaim space across many duplicate groups. `action` is delete/trash/hardlink.
/// Deleted copies are taken out of every open scan.
#[tauri::command]
async fn reclaim_dupes(
    app: tauri::AppHandle,
    jobs: Vec<ReclaimJob>,
    action: String,
    dry_run: bool,
) -> Result<ReclaimOutcome, String> {
    let act = match action.as_str() {
        "delete" => ReclaimAction::Delete,
        "trash" => ReclaimAction::Trash,
        "hardlink" => ReclaimAction::Hardlink,
        other => return Err(format!("unknown action: {other}")),
    };
    blocking(move || {
        let mut removed = 0usize;
        let mut reclaimed = 0u64;
        let mut errors = Vec::new();
        let mut done = Vec::new();
        // One path at a time, so the outcome of each is known exactly: the
        // view drops just those copies instead of searching again.
        for j in &jobs {
            refuse_remote(&j.keep)?;
            for p in &j.remove {
                refuse_remote(p)?;
            }
        }
        for j in jobs {
            let keep = PathBuf::from(&j.keep);
            for p in j.remove {
                let path = PathBuf::from(&p);
                let r = reclaim(&keep, std::slice::from_ref(&path), j.size, act, dry_run);
                removed += r.removed;
                reclaimed += r.reclaimed;
                errors.extend(r.errors);
                if r.removed > 0 {
                    done.push(p);
                }
            }
        }
        if !dry_run && act != ReclaimAction::Hardlink {
            let gone: Vec<PathBuf> = done.iter().map(PathBuf::from).collect();
            forget_paths(&app, &gone);
        }
        Ok(ReclaimOutcome {
            report: ReclaimReport {
                removed,
                reclaimed,
                errors,
                dry_run,
            },
            done,
        })
    })
    .await
}

/// A reclaim's report plus the copies it actually dealt with.
#[derive(Serialize)]
struct ReclaimOutcome {
    #[serde(flatten)]
    report: ReclaimReport,
    /// Paths deleted, sent to the trash or replaced by a hard link.
    done: Vec<String>,
}

/// Remove a file or folder as job `job` — permanently or to the OS trash.
/// `apply=false` is a dry run that only reports what would go. Progress is
/// emitted live and the operation is cancellable. Refuses a filesystem root as
/// a safety net. Whatever is gone afterwards is taken out of every open scan
/// in place (no re-walk).
#[tauri::command]
async fn remove_path_cmd(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    trash: bool,
    apply: bool,
) -> Result<RemoveReport, String> {
    let running = Running::start(&app, job);
    blocking(move || {
        refuse_remote(&path)?;
        let p = PathBuf::from(&path);
        if !p.exists() {
            return Err(format!("no such path: {path}"));
        }
        if p.parent().is_none() {
            return Err(format!("refusing to remove a filesystem root: {path}"));
        }
        let mode = if trash {
            RemoveMode::Trash
        } else {
            RemoveMode::Delete
        };
        let report = remove_path(&p, mode, !apply, &running.progress);
        if apply {
            forget_paths(&app, std::slice::from_ref(&p));
        }
        Ok(report)
    })
    .await
}

/// Open a native folder picker that allows several folders. Returns the
/// chosen paths (empty if cancelled).
#[tauri::command]
async fn pick_folders(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    blocking(move || {
        Ok(app
            .dialog()
            .file()
            .blocking_pick_folders()
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_string())
            .collect())
    })
    .await
}

/// The exclude patterns that are not valid globs.
#[tauri::command]
async fn check_globs(patterns: Vec<String>) -> Vec<String> {
    validate_globs(&patterns)
}

/// Whether the app runs with administrator rights (needed for the MFT engine).
#[tauri::command]
fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        #[link(name = "shell32")]
        extern "system" {
            fn IsUserAnAdmin() -> i32;
        }
        unsafe { IsUserAnAdmin() != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The app's version, for the title bar.
#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A drive / mounted volume that can be scanned as a whole.
#[derive(Serialize)]
struct Drive {
    /// Root path to scan (`C:\`, `/`, `/Volumes/Backup`).
    path: String,
    /// Volume label, if it has one.
    label: String,
    /// Filesystem (`NTFS`, …), if known.
    fs: String,
    /// `fixed`, `removable`, `network` or `other`.
    kind: String,
    /// Physical media: `ssd`, `hdd`, `remote` or `other` — the sidebar's
    /// tree view groups drives by it.
    media: String,
    total: u64,
    free: u64,
    /// What was mounted (`/dev/nvme0n1p5`, a ZFS dataset, a share), empty
    /// where it is unknown — the sidebar names the volume by it.
    source: String,
    /// Kernel name of the physical device the volume sits on (`nvme0n1`,
    /// `sda`), empty where it cannot be told — the UI then groups the
    /// volume under a virtual device instead.
    device: String,
    /// The physical device's model string, if sysfs reports one.
    device_model: String,
    /// The physical device's size in bytes (`/sys/block/<dev>/size` × 512);
    /// 0 where unknown — the UI then sums the volumes under it.
    device_size: u64,
    /// The bus the device hangs on (`nvme`, `sata`, `usb`, `virtio`, …),
    /// empty where unknown.
    transport: String,
    /// Every place this filesystem is mounted, mount-table order — `path`
    /// is always the first.
    mounts: Vec<String>,
}

/// Every drive with a readable filesystem, in drive-letter / mount order.
/// Off the UI thread: a sleeping network drive can take seconds to answer.
#[tauri::command]
async fn list_drives() -> Result<Vec<Drive>, String> {
    blocking(|| Ok(drives().into_iter().filter(|d| d.total > 0).collect())).await
}

#[cfg(windows)]
fn drives() -> Vec<Drive> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_len: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_name_len: u32,
        ) -> i32;
    }
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;

    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let path = format!("{}:\\", (b'A' + i) as char);
        let wide: Vec<u16> = std::ffi::OsStr::new(&path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let kind = match unsafe { GetDriveTypeW(wide.as_ptr()) } {
            DRIVE_CDROM => continue,
            DRIVE_FIXED => "fixed",
            DRIVE_REMOVABLE => "removable",
            DRIVE_REMOTE => "network",
            _ => "other",
        };
        let mut name = [0u16; 261];
        let mut fs = [0u16; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        } != 0;
        let text = |buf: &[u16]| {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        };
        let (total, free) = disk_space(Path::new(&path));
        out.push(Drive {
            label: if ok { text(&name) } else { String::new() },
            fs: if ok { text(&fs) } else { String::new() },
            kind: kind.into(),
            media: if kind == "network" {
                "remote".into()
            } else {
                drive_media(diskghost_core::is_rotational(Path::new(&path))).into()
            },
            mounts: vec![path.clone()],
            path,
            total,
            free,
            // Windows has no sysfs: each drive is its own device level —
            // the sidebar degrades to a one-level tree.
            source: String::new(),
            device: String::new(),
            device_model: String::new(),
            device_size: 0,
            transport: String::new(),
        });
    }
    out
}

#[cfg(not(windows))]
fn drives() -> Vec<Drive> {
    // Every real local filesystem, wherever it is mounted (`/d/store2`,
    // `/home`, …). Where the mount table cannot be read (macOS), the
    // folders volumes are usually mounted in.
    let mounts = diskghost_core::local_mounts();
    let by_point: HashMap<&Path, &diskghost_core::LocalMount> = mounts
        .iter()
        .map(|m| (m.mount_point.as_path(), m))
        .collect();
    let list = if mounts.is_empty() {
        mount_folder_drives()
            .into_iter()
            .map(|p| {
                let kind = if p == Path::new("/") {
                    "fixed"
                } else {
                    "other"
                };
                (p, String::new(), kind)
            })
            .map(|(p, fs, kind)| {
                let media = drive_media(diskghost_core::is_rotational(&p));
                (p, fs, kind, media)
            })
            .collect()
    } else {
        drive_list(&mounts, &diskghost_core::is_rotational)
    };
    // The unfiltered mount table, once: a drive `local_mounts` dropped (a
    // btrfs `/` on a subvolume) still has a device the tree wants.
    let table = diskghost_core::mount_table();
    list.into_iter()
        .map(|(p, fs, kind, media)| {
            let (total, free) = disk_space(&p);
            let m = by_point.get(p.as_path());
            let (source, dev, points) = match m {
                Some(m) => (m.source.clone(), m.dev, m.mounts.clone()),
                None => {
                    let (source, dev) = table
                        .iter()
                        .find(|m| m.mount_point == p)
                        .map(|m| (m.source.clone(), m.dev))
                        .unwrap_or_default();
                    (source, dev, vec![p.clone()])
                }
            };
            let info = device_info(&source, dev);
            Drive {
                label: p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                fs,
                kind: kind.into(),
                media: media.into(),
                mounts: points
                    .into_iter()
                    .map(|m| m.to_string_lossy().into_owned())
                    .collect(),
                path: p.to_string_lossy().into_owned(),
                total,
                free,
                source,
                device: info.name,
                device_model: info.model,
                device_size: info.size,
                transport: info.transport,
            }
        })
        .collect()
}

/// The physical device a volume sits on, for the sidebar's hardware tree —
/// name, model, size and bus. Empty fields where sysfs cannot tell
/// (macOS, or a source that is no block device at all).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Default)]
struct DeviceInfo {
    name: String,
    model: String,
    size: u64,
    transport: String,
}

/// [`DeviceInfo`] of the disk behind a mount (`source` + `st_dev` from the
/// mount table). Off Linux there is no sysfs to ask: the frontend then
/// builds a one-level tree, the drive is its own device.
#[cfg(target_os = "linux")]
fn device_info(source: &str, dev: u64) -> DeviceInfo {
    let Some(part) = block_device(Path::new("/sys"), source, dev) else {
        return DeviceInfo::default();
    };
    let Some(disk) = disk_of(Path::new("/sys"), &part) else {
        return DeviceInfo::default();
    };
    let (model, size, transport) = disk_facts(Path::new("/sys"), &disk);
    DeviceInfo {
        name: disk,
        model,
        size,
        transport,
    }
}

#[cfg(not(target_os = "linux"))]
fn device_info(_source: &str, _dev: u64) -> DeviceInfo {
    DeviceInfo::default()
}

/// glibc's `major()` — the inverse of the `makedev` encoding mountinfo's
/// `major:minor` fields go through.
#[cfg(target_os = "linux")]
fn dev_major(dev: u64) -> u64 {
    ((dev >> 8) & 0x0000_0fff) | ((dev >> 32) & 0xffff_f000)
}

/// glibc's `minor()`.
#[cfg(target_os = "linux")]
fn dev_minor(dev: u64) -> u64 {
    (dev & 0x0000_00ff) | ((dev >> 12) & 0xffff_ff00)
}

/// Kernel name of the block device behind a mount (`nvme0n1p5`, `dm-0`):
/// the node `source` resolves to, else `major:minor` under `sys/dev/block`.
/// `None` for pseudo filesystems — a ZFS dataset, fuse, overlay, tmpfs has
/// no block device. `sys` is the sysfs root (tests build a fake one).
#[cfg(target_os = "linux")]
fn block_device(sys: &Path, source: &str, dev: u64) -> Option<String> {
    let name_of = |p: &Path| {
        std::fs::canonicalize(p)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    };
    if source.starts_with("/dev/") {
        if let Some(n) = name_of(Path::new(source)) {
            return Some(n);
        }
    }
    let (major, minor) = (dev_major(dev), dev_minor(dev));
    // Anonymous devices (major 0: tmpfs, btrfs subvolumes, FUSE…) are no
    // block device at all.
    if major == 0 {
        return None;
    }
    name_of(&sys.join("dev/block").join(format!("{major}:{minor}")))
}

/// The disk a block device belongs to in the hardware tree: a partition's
/// sysfs parent (`nvme0n1p5` → `nvme0n1`, `mmcblk0p2` → `mmcblk0`,
/// `sda2` → `sda`); a stacked device (`dm-0`, `md0`) the disk below it when
/// every slave shares one — else itself, a device spanning disks or having
/// none is its own group. `None` when `name` is no block device.
#[cfg(target_os = "linux")]
fn disk_of(sys: &Path, name: &str) -> Option<String> {
    // A partition's class entry resolves inside its disk's device dir.
    let parent_of = |d: &Path| -> Option<String> {
        if !d.join("partition").is_file() {
            return None;
        }
        d.parent()?.file_name()?.to_str().map(String::from)
    };
    let dev = std::fs::canonicalize(sys.join("class/block").join(name)).ok()?;
    if let Some(p) = parent_of(&dev) {
        return Some(p);
    }
    // A stacked device: the disks below it, from sysfs `slaves`.
    if let Ok(slaves) = std::fs::read_dir(dev.join("slaves")) {
        let mut disks: Vec<String> = slaves
            .filter_map(Result::ok)
            .filter_map(|e| std::fs::canonicalize(e.path()).ok())
            .map(|d| {
                parent_of(&d).unwrap_or_else(|| {
                    d.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
            })
            .filter(|s| !s.is_empty())
            .collect();
        disks.sort_unstable();
        disks.dedup();
        // One disk below: the stack belongs to it. Several (a RAID, a
        // span): no honest single parent — the device groups itself.
        return Some(if disks.len() == 1 {
            disks.remove(0)
        } else {
            name.to_string()
        });
    }
    Some(name.to_string())
}

/// What sysfs knows about a whole disk: its model string (or `device/name`
/// for the buses that use that), its size in bytes (`size` × 512) and the
/// bus it hangs on. `("", 0, "")` where nothing can be told.
#[cfg(target_os = "linux")]
fn disk_facts(sys: &Path, disk: &str) -> (String, u64, String) {
    let Ok(dev) = std::fs::canonicalize(sys.join("class/block").join(disk)) else {
        return (String::new(), 0, String::new());
    };
    let read = |f: &str| {
        std::fs::read_to_string(dev.join(f))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let model = ["device/model", "device/name", "name"]
        .into_iter()
        .map(read)
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let size = read("size").parse::<u64>().unwrap_or(0).saturating_mul(512);
    (model, size, transport_of(disk, &dev))
}

/// The bus `disk` hangs on: from the kernel name first (nvme, mmc, virtio,
/// the stacked/virtual devices), then the sysfs path — a USB stick's path
/// passes a `usb` component, a SATA disk's an `ata` host bridge.
#[cfg(target_os = "linux")]
fn transport_of(disk: &str, dev: &Path) -> String {
    for (prefix, t) in [
        ("nvme", "nvme"),
        ("mmcblk", "mmc"),
        ("vd", "virtio"),
        ("xvd", "xen"),
        ("hd", "ide"),
        ("dm-", "dm"),
        ("md", "md"),
        ("loop", "loop"),
        ("nbd", "nbd"),
        ("pmem", "pmem"),
        ("zd", "zram"),
        ("sr", "optical"),
    ] {
        if disk.starts_with(prefix) {
            return t.to_string();
        }
    }
    for c in dev.components() {
        let c = c.as_os_str().to_string_lossy();
        // Bus directories carry a number: `usb3`, `ata1`, `virtio2`.
        if c.strip_prefix("usb")
            .is_some_and(|r| r.chars().all(|c| c.is_ascii_digit()))
        {
            return "usb".into();
        }
        if c.strip_prefix("ata")
            .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
        {
            return "sata".into();
        }
        if c.starts_with("virtio") {
            return "virtio".into();
        }
    }
    String::new()
}

/// `/` plus the mount points directly in the folders volumes are usually
/// mounted in — for systems without `/proc/self/mountinfo`.
#[cfg(not(windows))]
fn mount_folder_drives() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/")];
    let user = std::env::var("USER").unwrap_or_default();
    for parent in [
        PathBuf::from("/Volumes"),
        PathBuf::from("/media"),
        PathBuf::from("/media").join(&user),
        PathBuf::from("/run/media").join(&user),
        PathBuf::from("/mnt"),
    ] {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for e in entries.flatten() {
            // Skip symlinks (macOS links the boot volume back to `/`),
            // the per-user folders that only hold the real mounts, and
            // plain directories that are not mount points — listing one of
            // those as a "drive" scans content the `/` scan already covers.
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && e.file_name().to_string_lossy() != user && is_mount_point(&e.path()) {
                roots.push(e.path());
            }
        }
    }
    roots
}

/// A remote folder (`ssh://host/path`): scanned over ssh, never touched
/// as a local path.
fn is_remote_url(path: &str) -> bool {
    path.starts_with("ssh://")
}

/// Refuse to delete, trash or link anything of a remote scan: its paths
/// name files on another machine, never local ones.
fn refuse_remote(path: &str) -> Result<(), String> {
    if is_remote_url(path) {
        Err(format!(
            "{path} is on a remote host: Diskghost never changes remote files"
        ))
    } else {
        Ok(())
    }
}

/// The agent to run on a remote host: the command-line `diskghost` shipped
/// next to this app, else the one on the `PATH`. `None`: the host's own
/// shell tools list the folder.
fn agent_binary() -> Option<diskghost_core::remote::AgentBinary> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("diskghost")));
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        // An empty or relative entry would pick a `diskghost` from the
        // current directory — never upload and run that.
        .filter(|d| d.is_absolute())
        .map(|d| d.join("diskghost"));
    beside
        .into_iter()
        .chain(on_path)
        .find(|p| p.is_file())
        .and_then(|p| diskghost_core::remote::AgentBinary::from_file(&p))
}

/// A folder `add_location` is allowed to index — an absolute local directory
/// or `ssh://host//absolute/path`: the same gate `diskghost index add` and
/// the MCP `add_location` apply. A relative remote path is refused: the
/// host's own tools could read one (`-delete`, `-fprintf`…) as an option.
enum LocationTarget {
    Local(PathBuf),
    Remote { url: String, host: String },
}

fn location_target(path: &str) -> Result<LocationTarget, String> {
    if path.is_empty() || path.chars().any(char::is_control) {
        return Err("a path must be non-empty and free of control characters".to_string());
    }
    if is_remote_url(path) {
        let (host, p) = diskghost_core::remote::parse_ssh_url(path)?;
        if !p.starts_with('/') {
            return Err(format!(
                "{path}: the remote path must be absolute — write ssh://{host}//absolute/path"
            ));
        }
        return Ok(LocationTarget::Remote {
            url: path.to_string(),
            host,
        });
    }
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(format!("not an absolute path: {path}"));
    }
    if !p.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    Ok(LocationTarget::Local(p.to_path_buf()))
}

/// The index the command-line `diskghost index` uses — the locations added
/// here and by the CLI or the MCP server are the same ones.
fn index_db() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))?;
    Some(base.join("diskghost").join("index.db"))
}

/// The shared job registry of `db` (`index-jobs.db` next to the index):
/// every job any `diskghost mcp` server on this machine recorded, with
/// dead writers' live-looking rows already reported `interrupted`. Read
/// side only — the window owns no writer jobs, and a machine that never ran
/// one has no registry at all (an empty list, no new file).
fn read_index_jobs(db: &Path) -> Result<Vec<diskghost_core::jobs::JobRow>, String> {
    match diskghost_core::jobs::JobRegistry::for_index_reader(db)
        .map_err(|e| format!("cannot open job registry of {}: {e}", db.display()))?
    {
        Some(reg) => reg
            .list(diskghost_core::jobs::now_ms())
            .map_err(|e| e.to_string()),
        None => Ok(Vec::new()),
    }
}

/// The jobs queued through `diskghost mcp` servers (or any other registry
/// writer) — the SCANS section shows them next to this window's own jobs.
#[tauri::command]
async fn list_index_jobs() -> Result<Vec<diskghost_core::jobs::JobRow>, String> {
    blocking(|| {
        let Some(db) = index_db() else {
            return Ok(Vec::new());
        };
        read_index_jobs(&db)
    })
    .await
}

/// Index a folder as job `job` — `diskghost index add` from the GUI: new and
/// changed files are recorded in the persistent index, so a re-scan only
/// re-reads what changed. `exclude` holds walk globs, applied to local
/// folders only (a remote host lists its own files); `agent` is the remote
/// agent policy — `auto` runs the agent where the host can, `never` uses the
/// host's shell tools.
#[tauri::command]
async fn add_location(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    exclude: Vec<String>,
    agent: Option<String>,
) -> Result<ScanSummary, String> {
    let mode = match agent.as_deref() {
        None | Some("auto") => diskghost_core::remote::AgentMode::Auto,
        Some("never") => diskghost_core::remote::AgentMode::Never,
        Some(other) => return Err(format!("agent {other:?}: expected auto or never")),
    };
    let target = location_target(&path)?;
    if matches!(target, LocationTarget::Remote { .. }) && !exclude.is_empty() {
        return Err(
            "exclude applies to local folders only: the host lists its own files".to_string(),
        );
    }
    let bad = validate_globs(&exclude);
    if !bad.is_empty() {
        return Err(format!("invalid exclude glob(s): {}", bad.join(", ")));
    }
    let running = Running::start(&app, job);
    blocking(move || {
        let db = index_db()
            .ok_or_else(|| "no data directory (set HOME or XDG_DATA_HOME)".to_string())?;
        let idx =
            Index::open(&db).map_err(|e| format!("cannot open index {}: {e}", db.display()))?;
        match target {
            LocationTarget::Local(p) => {
                let opts = Options {
                    exclude,
                    skip_remote: true,
                    ..Options::default()
                };
                index_scan(&idx, &p, &opts, &running.progress)
                    .and_then(|s| idx.configure(&s.uuid, None, "local").map(|()| s))
            }
            LocationTarget::Remote { url, host } => {
                // `index_remote` picks the agent itself (`AgentBinary::current`
                // is the GUI binary, which cannot serve the protocol): on
                // `auto` the hello check falls back to the host's shell tools
                // and `agent_note` on the summary says why.
                index_remote(&idx, &url, mode, &running.progress).and_then(|s| {
                    idx.configure(&s.uuid, None, &format!("ssh://{host}"))
                        .map(|()| s)
                })
            }
        }
        .map_err(|e| e.to_string())
    })
    .await
}

/// One indexed location, as the sidebar lists it.
#[derive(Serialize)]
struct LocationInfo {
    uuid: String,
    /// `local` or `ssh://<host>`.
    host: String,
    /// The volume's last known mountpoint (a path on its own machine).
    root: String,
    /// The folders `index add` was given on the volume.
    roots: Vec<String>,
    role: Role,
    files: u64,
    bytes: u64,
    /// Unix seconds of the location's last scan.
    last_seen: i64,
    /// `None` for a remote location: it is not probed here, so nothing is
    /// claimed about its reachability.
    online: Option<bool>,
}

/// Every location in the index, as `diskghost index status` reports them.
/// Remote hosts are not probed — each ssh answer could cost seconds.
#[tauri::command]
async fn list_locations() -> Result<Vec<LocationInfo>, String> {
    blocking(|| {
        let Some(db) = index_db() else {
            return Ok(Vec::new());
        };
        if !db.exists() {
            return Ok(Vec::new());
        }
        let idx =
            Index::open(&db).map_err(|e| format!("cannot open index {}: {e}", db.display()))?;
        let mut st = idx
            .status_without_remote_probe()
            .map_err(|e| e.to_string())?;
        st.sort_by(|a, b| (&a.host, &a.root, &a.uuid).cmp(&(&b.host, &b.root, &b.uuid)));
        Ok(st
            .into_iter()
            .map(|l| {
                let remote = diskghost_core::remote::is_remote_host(&l.host);
                LocationInfo {
                    uuid: l.uuid,
                    host: l.host,
                    root: l.root.to_string_lossy().into_owned(),
                    roots: l
                        .roots
                        .iter()
                        .map(|r| r.to_string_lossy().into_owned())
                        .collect(),
                    role: l.role,
                    files: l.files,
                    bytes: l.bytes,
                    last_seen: l.last_seen,
                    online: if remote { None } else { Some(l.online) },
                }
            })
            .collect())
    })
    .await
}

/// What `forget_location` answers.
#[derive(Serialize)]
struct ForgetResult {
    /// `false` when the index had no such location (already forgotten).
    forgotten: bool,
}

/// Drop location `uuid` and its rows from the index at `db` — what
/// `diskghost index forget <uuid>` does. Index rows only: no file on any disk
/// is touched.
fn forget_in(db: &Path, uuid: &str) -> Result<ForgetResult, String> {
    if !db.exists() {
        return Ok(ForgetResult { forgotten: false });
    }
    let idx = Index::open(db).map_err(|e| format!("cannot open index {}: {e}", db.display()))?;
    let forgotten = idx.forget(uuid).map_err(|e| e.to_string())?;
    Ok(ForgetResult { forgotten })
}

/// Forget one location of the shared index (the CLI's `index forget`). The
/// location's index entries go; its files on disk are never touched.
#[tauri::command]
async fn forget_location(uuid: String) -> Result<ForgetResult, String> {
    blocking(move || {
        let db = index_db()
            .ok_or_else(|| "no data directory (set HOME or XDG_DATA_HOME)".to_string())?;
        forget_in(&db, &uuid)
    })
    .await
}

/// One page of `find_indexed_files`.
#[derive(Serialize)]
struct FoundPage {
    files: Vec<diskghost_core::FoundFile>,
    offset: usize,
    limit: usize,
    /// Another page follows this one.
    more: bool,
}

/// Search the index at `db` for paths containing `pattern` (the same query
/// as the MCP `find_files`): `limit` defaults to `FIND_DEFAULT_LIMIT` and is
/// capped at `FIND_MAX_LIMIT`. Reads the index only; no index yet = no hit.
fn find_in(
    db: &Path,
    pattern: &str,
    limit: Option<usize>,
    offset: Option<usize>,
    location: Option<&str>,
) -> Result<FoundPage, String> {
    let limit = limit.unwrap_or(FIND_DEFAULT_LIMIT).clamp(1, FIND_MAX_LIMIT);
    let offset = offset.unwrap_or(0);
    if pattern.is_empty() || !db.exists() {
        return Ok(FoundPage {
            files: Vec::new(),
            offset,
            limit,
            more: false,
        });
    }
    let idx = Index::open(db).map_err(|e| format!("cannot open index {}: {e}", db.display()))?;
    // One row past the page tells whether another page follows.
    let mut files = idx
        .find_files(pattern, location, limit + 1, offset)
        .map_err(|e| e.to_string())?;
    let more = files.len() > limit;
    files.truncate(limit);
    Ok(FoundPage {
        files,
        offset,
        limit,
        more,
    })
}

/// Find indexed files whose path contains `pattern` (case-insensitive, no
/// wildcards), optionally only on one location (uuid or folder). Runs off
/// the UI thread; reads the index, never the disks.
#[tauri::command]
async fn find_indexed_files(
    pattern: String,
    limit: Option<usize>,
    offset: Option<usize>,
    location: Option<String>,
) -> Result<FoundPage, String> {
    blocking(move || {
        // No data directory: an empty path, which does not exist — no hit.
        let db = index_db().unwrap_or_default();
        find_in(&db, &pattern, limit, offset, location.as_deref())
    })
    .await
}

/// The drives to offer for the real local filesystems `mounts`: `/` first
/// (always, even when the table names it differently), then the others by
/// mount point. (path, filesystem type, kind, media). `rotational` is the
/// per-mount spin probe — injected so tests need no real disks.
#[cfg(unix)]
fn drive_list(
    mounts: &[diskghost_core::LocalMount],
    rotational: &dyn Fn(&Path) -> bool,
) -> Vec<(PathBuf, String, &'static str, &'static str)> {
    let root = Path::new("/");
    let root_fs = mounts
        .iter()
        .find(|m| m.mount_point == root)
        .map(|m| m.fstype.clone())
        .unwrap_or_default();
    let mut rest: Vec<&diskghost_core::LocalMount> =
        mounts.iter().filter(|m| m.mount_point != root).collect();
    rest.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    std::iter::once((
        root.to_path_buf(),
        root_fs,
        "fixed",
        drive_media(rotational(root)),
    ))
    .chain(rest.into_iter().map(|m| {
        let p = &m.mount_point;
        let kind = if p.starts_with("/media") || p.starts_with("/run/media") {
            "removable"
        } else {
            "other"
        };
        (
            p.clone(),
            m.fstype.clone(),
            kind,
            drive_media(rotational(p)),
        )
    }))
    .collect()
}

/// Media label for the sidebar grouping: spinning disks are `hdd`,
/// anything else — solid state, virtual, unknown — is `ssd`.
fn drive_media(rotational: bool) -> &'static str {
    if rotational {
        "hdd"
    } else {
        "ssd"
    }
}

/// Where the UI's settings are kept.
fn settings_file(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

fn read_settings(app: &tauri::AppHandle) -> serde_json::Value {
    settings_file(app)
        .ok()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// The saved settings as the UI stored them (`null` before the first save).
#[tauri::command]
async fn load_settings(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    blocking(move || Ok(read_settings(&app))).await
}

/// Persist the UI's settings. The backend only reads `window_size` from them.
#[tauri::command]
async fn save_settings(app: tauri::AppHandle, settings: serde_json::Value) -> Result<(), String> {
    blocking(move || {
        let file = settings_file(&app)?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
        std::fs::write(file, json).map_err(|e| e.to_string())
    })
    .await
}

/// Size the main window for the screen it opens on, then show it. The
/// `window_size` setting picks a size in logical pixels: `auto` (the
/// default) and `1440p` ask for 2560×1440, `1080p` and `4k` for theirs. A size
/// that does not fit the monitor's work area — or `maximized` — maximizes.
fn place_window(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let settings = read_settings(app);
    let wanted = match settings.get("window_size").and_then(|v| v.as_str()) {
        Some("maximized") => None,
        Some("1080p") => Some((1920.0, 1080.0)),
        Some("4k") => Some((3840.0, 2160.0)),
        _ => Some((2560.0, 1440.0)),
    };
    let monitor = win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| win.primary_monitor().ok().flatten());
    if let Some(m) = monitor {
        let scale = m.scale_factor();
        let area = m.work_area().size;
        let (aw, ah) = (area.width as f64 / scale, area.height as f64 / scale);
        match wanted {
            Some((w, h)) if w <= aw && h <= ah => {
                let _ = win.set_size(LogicalSize::new(w, h));
                let _ = win.center();
            }
            _ => {
                let _ = win.maximize();
            }
        }
    }
    let _ = win.show();
}

fn main() {
    // The same executable doubles as the elevated MFT helper (no window).
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "--mft-helper" {
        helper::serve(Path::new(&args[2]), &args[3]);
        return;
    }
    // Run by the Windows setup: the CLI next to the app goes on/off the PATH.
    if args.len() == 2 && (args[1] == "--add-to-path" || args[1] == "--remove-from-path") {
        let code = match path_env::update(args[1] == "--add-to-path") {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        };
        std::process::exit(code);
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            app.manage(AppState {
                jobs: Mutex::new(HashMap::new()),
                trees: Mutex::new(HashMap::new()),
                hashes: Mutex::new(None),
                helper: Mutex::new(None),
            });
            place_window(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_dir,
            view,
            find_dupes,
            cancel,
            reclaim_dupes,
            remove_path_cmd,
            pick_folders,
            list_snapshots,
            delete_snapshot,
            diff_since,
            forget_tree,
            list_drives,
            check_globs,
            load_settings,
            save_settings,
            app_version,
            is_elevated,
            add_location,
            list_locations,
            forget_location,
            find_indexed_files,
            list_index_jobs
        ])
        .run(tauri::generate_context!())
        .expect("error while running Diskghost");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_index_jobs_returns_another_writers_job() {
        // The SCANS read path (`list_index_jobs` → `read_index_jobs`): a row
        // written by a different process shows up, including its reported
        // `interrupted` state once the writer's heartbeat went stale.
        let d = std::env::temp_dir().join(format!("diskghost-gui-jobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("index.db");
        // No registry yet: the read path answers empty, creating nothing.
        assert!(read_index_jobs(&db).unwrap().is_empty());
        assert!(!diskghost_core::jobs::jobs_db_path(&db).exists());
        let reg = diskghost_core::jobs::JobRegistry::for_index(&db).unwrap();
        let now = diskghost_core::jobs::now_ms();
        reg.announce("job-99-1", "scan", "/media/disk", 99, now - 2)
            .unwrap();
        reg.mark_running("job-99-1", now - 1).unwrap();
        let rows = read_index_jobs(&db).unwrap();
        assert_eq!(rows.len(), 1);
        let j = &rows[0];
        assert_eq!(
            (
                j.job_id.as_str(),
                j.state.as_str(),
                j.target.as_str(),
                j.writer_pid
            ),
            ("job-99-1", "running", "/media/disk", 99)
        );
        // Kill simulation: the last heartbeat is far in the past.
        reg.beat(
            &[diskghost_core::jobs::Beat {
                job_id: "job-99-1".into(),
                phase: "walk".into(),
                files: 10,
                bytes: 100,
                total_files: 0,
                total_bytes: 0,
            }],
            now - diskghost_core::jobs::STALE_MS - 1,
        )
        .unwrap();
        let rows = read_index_jobs(&db).unwrap();
        assert_eq!(rows[0].state, "interrupted", "{rows:?}");
        assert_eq!(rows[0].files, 10);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn forget_location_drops_index_rows_and_never_touches_files() {
        // `forget_location` → `forget_in` on a scratch index: the location
        // and its rows go, the scanned file stays on disk.
        let d = std::env::temp_dir().join(format!("diskghost-gui-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let tree = d.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("a.txt"), b"hello").unwrap();
        let db = d.join("index.db");
        // No index yet: nothing to forget, and nothing is created.
        assert!(!forget_in(&db, "nope").unwrap().forgotten);
        assert!(!db.exists());
        let idx = Index::open(&db).unwrap();
        let s = index_scan(&idx, &tree, &Options::default(), &Progress::default()).unwrap();
        drop(idx);
        assert!(!forget_in(&db, "no-such-uuid").unwrap().forgotten);
        assert!(forget_in(&db, &s.uuid).unwrap().forgotten);
        assert!(!forget_in(&db, &s.uuid).unwrap().forgotten, "already gone");
        let idx = Index::open(&db).unwrap();
        assert!(idx.status_without_remote_probe().unwrap().is_empty());
        assert!(tree.join("a.txt").exists(), "files on disk stay");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn find_indexed_files_pages_through_the_index() {
        // `find_indexed_files` → `find_in` on a scratch index.
        let d = std::env::temp_dir().join(format!("diskghost-gui-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let tree = d.join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        for f in ["a.TXT", "b.txt", "sub/c.txt", "d.bin"] {
            std::fs::write(tree.join(f), b"x").unwrap();
        }
        let db = d.join("index.db");
        // No index yet: no hit, and nothing is created.
        assert!(find_in(&db, "txt", None, None, None).unwrap().files.is_empty());
        assert!(!db.exists());
        let idx = Index::open(&db).unwrap();
        let s = index_scan(&idx, &tree, &Options::default(), &Progress::default()).unwrap();
        drop(idx);
        let all = find_in(&db, "TXT", None, None, None).unwrap();
        assert_eq!((all.files.len(), all.limit, all.more), (3, FIND_DEFAULT_LIMIT, false));
        assert!(all.files.iter().all(|f| f.location_uuid == s.uuid));
        let p1 = find_in(&db, ".txt", Some(2), Some(0), None).unwrap();
        assert_eq!((p1.files.len(), p1.more), (2, true));
        let p2 = find_in(&db, ".txt", Some(2), Some(2), None).unwrap();
        assert_eq!((p2.files.len(), p2.more), (1, false));
        let sub = std::fs::canonicalize(tree.join("sub")).unwrap();
        let sub = sub.to_string_lossy().into_owned();
        let only = find_in(&db, "txt", None, None, Some(&sub)).unwrap();
        assert_eq!(only.files.len(), 1, "only under {sub}");
        assert!(find_in(&db, "txt", None, None, Some("no-such-uuid")).unwrap().files.is_empty());
        assert_eq!(find_in(&db, "x", Some(99_999), None, None).unwrap().limit, FIND_MAX_LIMIT);
        assert!(find_in(&db, "", None, None, None).unwrap().files.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ssh_urls_are_remote_and_local_paths_are_not() {
        assert!(is_remote_url("ssh://host//storage"));
        assert!(is_remote_url("ssh://u@host/data"));
        assert!(!is_remote_url("/home/u"));
        assert!(!is_remote_url("C:\\"));
        assert!(!is_remote_url("./ssh://x"));
    }

    #[test]
    fn add_location_takes_only_absolute_targets() {
        // A remote folder is accepted only as ssh://host//abs/path: a
        // relative path could be read as an option by the host's `find`.
        match location_target("ssh://nas//storage").unwrap() {
            LocationTarget::Remote { url, host } => {
                assert_eq!(url, "ssh://nas//storage");
                assert_eq!(host, "nas");
            }
            LocationTarget::Local(_) => panic!("an ssh URL read as local"),
        }
        match location_target("ssh://u@nas//").unwrap() {
            LocationTarget::Remote { host, .. } => assert_eq!(host, "u@nas"),
            LocationTarget::Local(_) => panic!("an ssh URL read as local"),
        }
        for bad in [
            "ssh://nas",
            "ssh://nas/",
            "ssh://nas/rel",
            "ssh://",
            "ssh://-oProxyCommand=x//a",
            "ssh://a b//x",
            "",
            "rel/path",
            "nas//x",
            " \t",
        ] {
            assert!(location_target(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[cfg(unix)]
    #[test]
    fn add_location_takes_only_existing_local_dirs() {
        match location_target("/").unwrap() {
            LocationTarget::Local(p) => assert_eq!(p, Path::new("/")),
            LocationTarget::Remote { .. } => panic!("a local dir read as remote"),
        }
        assert!(location_target("home/user").is_err());
        assert!(location_target("/definitely-not-a-dir-xyz").is_err());
    }

    #[cfg(unix)]
    fn mount(dev: u64, at: &str, fstype: &str) -> diskghost_core::LocalMount {
        diskghost_core::LocalMount {
            dev,
            mount_point: PathBuf::from(at),
            fstype: fstype.into(),
            source: format!("/dev/x{dev}"),
            mounts: vec![PathBuf::from(at)],
        }
    }

    #[cfg(unix)]
    #[test]
    fn every_local_filesystem_is_a_drive_root_first_then_by_mount_point() {
        let ssd = |_: &Path| false;
        let got = drive_list(
            &[
                mount(1, "/", "btrfs"),
                mount(5, "/mnt/fast", "xfs"),
                mount(2, "/d/store2", "ext4"),
                mount(3, "/run/media/u/stick", "exfat"),
                mount(4, "/media/u/games", "ntfs3"),
                mount(6, "/home", "xfs"),
            ],
            &ssd,
        );
        let want: Vec<(PathBuf, String, &str, &str)> = [
            ("/", "btrfs", "fixed", "ssd"),
            ("/d/store2", "ext4", "other", "ssd"),
            ("/home", "xfs", "other", "ssd"),
            ("/media/u/games", "ntfs3", "removable", "ssd"),
            ("/mnt/fast", "xfs", "other", "ssd"),
            ("/run/media/u/stick", "exfat", "removable", "ssd"),
        ]
        .into_iter()
        .map(|(p, f, k, m)| (PathBuf::from(p), f.to_string(), k, m))
        .collect();
        assert_eq!(got, want);
    }

    #[cfg(unix)]
    #[test]
    fn a_spinning_mount_is_an_hdd_drive() {
        // The media label follows the drive's own rotational flag, not its
        // mount point or filesystem: the injected probe names /data spinning.
        let rot = |p: &Path| p == Path::new("/data");
        let got = drive_list(&[mount(1, "/", "ext4"), mount(2, "/data", "ext4")], &rot);
        assert_eq!(
            got[1],
            (PathBuf::from("/data"), "ext4".into(), "other", "hdd")
        );
        assert_eq!(got[0].3, "ssd");
    }

    #[cfg(unix)]
    #[test]
    fn the_root_is_a_drive_even_when_the_table_does_not_name_it() {
        // btrfs installs mount a subvolume (`/@`) at `/`: its mountinfo root
        // is not `/`, so `local_mounts` drops it — `/` must stay listed.
        let ssd = |_: &Path| false;
        let got = drive_list(&[mount(2, "/d/store2", "ext4")], &ssd);
        assert_eq!(got[0], (PathBuf::from("/"), String::new(), "fixed", "ssd"));
        assert_eq!(got.len(), 2);
        assert_eq!(
            drive_list(&[], &ssd),
            [(PathBuf::from("/"), String::new(), "fixed", "ssd")]
        );
    }

    /// A fake `/sys`: `devices/<bus path>/block/<disk>[/<part>]` directories
    /// with the `/sys/class/block/<name>` symlinks the kernel makes.
    #[cfg(target_os = "linux")]
    struct FakeSys(PathBuf);

    #[cfg(target_os = "linux")]
    impl FakeSys {
        fn new(label: &str) -> FakeSys {
            let mut d = std::env::temp_dir();
            d.push(format!("diskghost-sys-{}-{label}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join("class/block")).unwrap();
            std::fs::create_dir_all(d.join("dev/block")).unwrap();
            FakeSys(d)
        }

        /// A whole disk under `bus` (part of the sysfs device path:
        /// "pci0/ata1/host0" for SATA, "pci0/usb1/1-2" for USB), with a
        /// model string and a sector count.
        fn disk(&self, name: &str, bus: &str, model: &str, sectors: u64) -> PathBuf {
            let dir = self.0.join("devices").join(bus).join("block").join(name);
            std::fs::create_dir_all(dir.join("device")).unwrap();
            std::fs::write(dir.join("device/model"), format!("{model}\n")).unwrap();
            std::fs::write(dir.join("size"), format!("{sectors}\n")).unwrap();
            std::os::unix::fs::symlink(&dir, self.0.join("class/block").join(name)).unwrap();
            dir
        }

        /// A partition inside `disk`'s dir, as sysfs lays it out.
        fn partition(&self, disk: &Path, name: &str) {
            let dir = disk.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("partition"), "2\n").unwrap();
            std::os::unix::fs::symlink(&dir, self.0.join("class/block").join(name)).unwrap();
        }

        /// A stacked device (dm/md) over `slaves`.
        fn stacked(&self, name: &str, slaves: &[&str]) {
            let dir = self.0.join("devices/virtual/block").join(name);
            std::fs::create_dir_all(dir.join("slaves")).unwrap();
            for s in slaves {
                std::os::unix::fs::symlink(
                    self.0.join("class/block").join(s),
                    dir.join("slaves").join(s),
                )
                .unwrap();
            }
            std::os::unix::fs::symlink(&dir, self.0.join("class/block").join(name)).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn partitions_group_under_their_disk() {
        let sys = FakeSys::new("parts");
        let nvme = sys.disk(
            "nvme0n1",
            "pci0/nvme/nvme0",
            "Lexar NM790 4TB",
            7_814_168_576,
        );
        sys.partition(&nvme, "nvme0n1p5");
        let sda = sys.disk(
            "sda",
            "pci0/ata1/host0/target0:0:0/0:0:0:0",
            "CT4000",
            7_814_168_576,
        );
        sys.partition(&sda, "sda2");
        let mmc = sys.disk("mmcblk0", "platform/mmc", "SD64G", 125_000_000);
        sys.partition(&mmc, "mmcblk0p2");

        assert_eq!(disk_of(&sys.0, "nvme0n1p5").as_deref(), Some("nvme0n1"));
        assert_eq!(disk_of(&sys.0, "sda2").as_deref(), Some("sda"));
        assert_eq!(disk_of(&sys.0, "mmcblk0p2").as_deref(), Some("mmcblk0"));
        // Whole disks answer themselves; unknown names answer nothing.
        assert_eq!(disk_of(&sys.0, "nvme0n1").as_deref(), Some("nvme0n1"));
        assert_eq!(disk_of(&sys.0, "sdz"), None);
        std::fs::remove_dir_all(&sys.0).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stacked_devices_group_under_the_disk_below() {
        let sys = FakeSys::new("stacked");
        let sdb = sys.disk("sdb", "pci0/usb1/1-2/host2", "stick", 1_000);
        sys.partition(&sdb, "sdb1");
        let sdc = sys.disk("sdc", "pci0/ata2/host1", "other", 2_000);
        sys.partition(&sdc, "sdc1");
        sys.stacked("dm-0", &["sdb1"]); // LUKS/LVM on one disk
        sys.stacked("md0", &["sdb1", "sdc1"]); // a span across two disks
        sys.stacked("dm-9", &[]); // no slaves at all

        assert_eq!(disk_of(&sys.0, "dm-0").as_deref(), Some("sdb"));
        // A RAID over several disks has no single parent: its own group.
        assert_eq!(disk_of(&sys.0, "md0").as_deref(), Some("md0"));
        assert_eq!(disk_of(&sys.0, "dm-9").as_deref(), Some("dm-9"));
        std::fs::remove_dir_all(&sys.0).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disk_facts_reads_model_size_and_bus() {
        let sys = FakeSys::new("facts");
        sys.disk("nvme0n1", "pci0/nvme/nvme0", "Lexar NM790 4TB", 2_000_000);
        sys.disk("sda", "pci0/ata1/host0", "CT4000X9PROSSD9", 4_000);
        sys.disk("sdc", "pci0/usb3/3-2/3-2:1.0/host4", "TerraMaster", 8_000);
        sys.disk("vda", "pci0/virtio2", "", 512);
        sys.disk("loop0", "virtual", "", 0);

        let (model, size, transport) = disk_facts(&sys.0, "nvme0n1");
        assert_eq!(model, "Lexar NM790 4TB");
        assert_eq!(size, 2_000_000 * 512);
        assert_eq!(transport, "nvme");
        assert_eq!(disk_facts(&sys.0, "sda").2, "sata");
        assert_eq!(disk_facts(&sys.0, "sdc").2, "usb");
        assert_eq!(disk_facts(&sys.0, "vda").2, "virtio");
        assert_eq!(disk_facts(&sys.0, "loop0").2, "loop");
        assert_eq!(
            disk_facts(&sys.0, "none"),
            (String::new(), 0, String::new())
        );
        std::fs::remove_dir_all(&sys.0).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn block_device_resolves_the_node_or_the_device_number() {
        let sys = FakeSys::new("blockdev");
        let sdc = sys.disk("sdc", "pci0/usb3/3-2/host4", "TerraMaster", 8_000);
        sys.partition(&sdc, "sdc1");
        std::os::unix::fs::symlink(sdc.join("sdc1"), sys.0.join("dev/block").join("8:33")).unwrap();

        // No device node: the major:minor lookup names it anyway.
        assert_eq!(
            block_device(&sys.0, "whatever", 8 << 8 | 33).as_deref(),
            Some("sdc1")
        );
        // Pseudo filesystems (anonymous st_dev) have no block device.
        assert_eq!(block_device(&sys.0, "tmpfs", 27), None);
        assert_eq!(block_device(&sys.0, "tank/data", 0), None);
        std::fs::remove_dir_all(&sys.0).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dev_major_minor_invert_makedev() {
        // glibc's encoding, as mountinfo's major:minor fields go through it.
        for (major, minor) in [(0, 26), (8, 33), (259, 10), (7, 200)] {
            let dev = ((major & 0xffff_f000) << 32)
                | ((major & 0x0000_0fff) << 8)
                | ((minor & 0xffff_ff00) << 12)
                | (minor & 0x0000_00ff);
            assert_eq!((dev_major(dev), dev_minor(dev)), (major, minor));
        }
    }
}
