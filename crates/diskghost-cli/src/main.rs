//! Diskghost CLI — fast disk usage & duplicate finder.
//!
//! Human-readable by default; `--json` emits machine-readable output for
//! scripts and agents (headless use).

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod mcp;

use clap::{Args, Parser, Subcommand, ValueEnum};
use diskghost_core::remote::{parse_ssh_url, AgentMode};
use diskghost_core::{
    apply as apply_plan, journal_path, make_plan, undo as undo_journal, ApplyOptions, Plan, Policy,
    RunReport, Status,
};
use diskghost_core::{
    diff, find_duplicates_cached, format_timestamp, human_delta, human_size, index_dupes,
    index_remote, index_scan, phase, reclaim, remote_listing, remove_path, validate_globs,
    validate_host, DiffReport, DupGroup, HashCache, Index, IndexDupesReport, IndexReportOptions,
    LocationStatus, Options, Progress, ReclaimAction, ReclaimReport, RemoveMode, RemoveReport,
    Role, ScanReport, ScanSummary, ScanTree, Snapshot, Suggest, CHECKPOINT_INTERVAL,
};
use serde::Serialize;

/// Run `job` with a `Progress`, printing a live line to stderr while it works
/// (only when stderr is a terminal, so JSON/pipe output stays clean).
fn with_progress<T>(job: impl FnOnce(&Progress) -> T) -> T {
    let progress = Progress::default();
    if !std::io::stderr().is_terminal() {
        return job(&progress);
    }
    let done = Arc::new(AtomicBool::new(false));
    let (p, d) = (progress.clone(), done.clone());
    let handle = std::thread::spawn(move || {
        while !d.load(Ordering::Relaxed) {
            match p.phase() {
                phase::SAMPLE => eprint!(
                    "\r  comparing file ends… {} / {} files   ",
                    p.files(),
                    p.total_files()
                ),
                phase::HASH => eprint!(
                    "\r  hashing… {} / {} files, {} / {}   ",
                    p.files(),
                    p.total_files(),
                    human_size(p.bytes()),
                    human_size(p.total_bytes())
                ),
                _ => eprint!(
                    "\r  working… {} files, {}   ",
                    p.files(),
                    human_size(p.bytes())
                ),
            }
            let _ = std::io::stderr().flush();
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });
    let out = job(&progress);
    done.store(true, Ordering::Relaxed);
    let _ = handle.join();
    eprint!("\r\x1b[2K"); // clear the progress line
    let _ = std::io::stderr().flush();
    out
}

/// Refuse invalid exclude globs up front: skipping one would scan, or reclaim,
/// exactly what the caller meant to leave out.
fn check_globs(patterns: &[String]) -> bool {
    let bad = validate_globs(patterns);
    if !bad.is_empty() {
        eprintln!("error: invalid exclude glob(s): {}", bad.join(", "));
    }
    bad.is_empty()
}

#[derive(Parser)]
#[command(
    name = "diskghost",
    version,
    about = "Fast disk usage & duplicate finder 👻"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Emit machine-readable JSON (for scripts / agents).
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the remote agent protocol on stdin/stdout (run by `index add
    /// ssh://…` on the remote host, not by hand): line-delimited JSON
    /// requests `hello`, `enumerate`, `stat_fs`, `hash` — read-only by
    /// construction. Nothing else is opened, created or written.
    #[command(hide = true)]
    Agent,
    /// Serve an MCP (Model Context Protocol) server on stdin/stdout, so an AI
    /// agent can list drives, index folders and ssh:// hosts as background
    /// jobs, find duplicates and write a dedup plan. Read-only by
    /// construction: no tool moves, deletes, renames or writes a user file —
    /// a plan is a JSON file under $XDG_STATE_HOME/diskghost/plans/ that you
    /// carry out yourself with `diskghost apply`. Not for interactive use:
    /// register it with the agent (`claude mcp add diskghost -- diskghost mcp`).
    Mcp {
        /// Index database (default: as `diskghost index`).
        #[arg(long, value_name = "FILE")]
        db: Option<PathBuf>,
    },
    /// Scan a directory: total size, biggest sub-folders and biggest files.
    Scan {
        /// Directory to scan.
        path: PathBuf,
        /// How many top entries to show.
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[command(flatten)]
        walk: WalkArgs,
        /// Save the scan as a snapshot (every folder and file, compressed) for a
        /// later `--since` or `diff`. Needs the whole tree: not with --max-depth.
        #[arg(long, value_name = "FILE", conflicts_with = "max_depth")]
        save: Option<PathBuf>,
        /// Compare against a snapshot saved earlier: what grew, shrank,
        /// appeared or vanished since then. Not with --max-depth.
        #[arg(long, value_name = "FILE", conflicts_with = "max_depth")]
        since: Option<PathBuf>,
        /// A remote folder (`ssh://host/path`) is listed on the host: `auto`
        /// runs this program there when it can (see `index add --agent`),
        /// `never` uses the host's shell tools.
        #[arg(long, value_enum, default_value = "auto")]
        agent: AgentArg,
    },
    /// Compare two snapshots saved with `scan --save`.
    Diff {
        /// The older snapshot.
        old: PathBuf,
        /// The newer snapshot.
        new: PathBuf,
        /// Compare at this sub-folder instead of the scanned root.
        #[arg(long, value_name = "DIR")]
        at: Option<PathBuf>,
        /// How many entries to show per list.
        #[arg(long, default_value_t = 20)]
        top: usize,
    },
    /// Find duplicate (byte-identical) files; optionally reclaim their space.
    /// With --index: report from the persistent index across every indexed
    /// volume, unplugged ones included (see `diskghost index`).
    Dupes {
        /// Directory to scan (not with --index).
        #[arg(required_unless_present = "index", conflicts_with = "index")]
        path: Option<PathBuf>,
        /// Ignore files smaller than this many megabytes.
        #[arg(long, default_value_t = 1)]
        min_mb: u64,
        #[command(flatten)]
        walk: WalkArgs,
        /// Reclaim redundant copies (one file per group is always kept).
        /// Nothing is changed unless you also pass --apply.
        #[arg(long, value_enum, conflicts_with = "index")]
        reclaim: Option<ReclaimArg>,
        /// Actually perform the reclaim (default: dry-run).
        #[arg(long, conflicts_with = "index")]
        apply: bool,
        /// Keep file hashes in this file between runs: a repeated search only
        /// reads files whose size or modification time changed.
        #[arg(long, value_name = "FILE", conflicts_with = "index")]
        cache: Option<PathBuf>,
        /// Report from the persistent index instead of walking a folder:
        /// copies on every indexed volume count, offline ones too (never
        /// read, never suggested for action). Hashes still missing are
        /// computed once and stored. Report only: nothing is changed.
        #[arg(long)]
        index: bool,
        /// Index database (default: as `diskghost index`).
        #[arg(long, value_name = "FILE", requires = "index")]
        db: Option<PathBuf>,
        /// Also count, per volume, the files that exist only once anywhere —
        /// the ones no other copy protects.
        #[arg(long, requires = "index")]
        singles: bool,
        /// List those single-copy files too: into FILE (one path per line),
        /// or in the output when no FILE is given. Implies --singles.
        #[arg(
            long,
            value_name = "FILE",
            num_args = 0..=1,
            default_missing_value = "-",
            requires = "index"
        )]
        singles_paths: Option<PathBuf>,
    },
    /// Persistent index of every file on registered volumes: a re-scan only
    /// re-reads files whose size, time or inode changed (exFAT/FAT: size or
    /// time; also files written within 2 s of the last scan). Volumes are known by
    /// filesystem UUID, so an unplugged disk keeps its entries. Read-only:
    /// indexed files are never modified or deleted.
    Index {
        /// Index database (default: $XDG_DATA_HOME/diskghost/index.db, else
        /// ~/.local/share/diskghost/index.db).
        #[arg(long, value_name = "FILE", global = true)]
        db: Option<PathBuf>,
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Plan which duplicate copies to quarantine under a copy policy
    /// (min copies, distinct devices, master/scratch locations) — written to
    /// a JSON file to read, edit, then `apply`. Touches no file.
    Plan {
        /// Plan from the persistent index (the only mode).
        #[arg(long, required = true)]
        index: bool,
        /// Index database (default: as `diskghost index`).
        #[arg(long, value_name = "FILE")]
        db: Option<PathBuf>,
        /// Copy policy (TOML). Without it: min_copies = 2 on distinct
        /// physical devices, roles as stored in the index.
        #[arg(long, value_name = "FILE")]
        policy: Option<PathBuf>,
        /// Where to write the plan.
        #[arg(short = 'o', long = "output", value_name = "FILE", required = true)]
        output: PathBuf,
    },
    /// Carry out a plan: re-check every file (size, time, inode, full hash)
    /// and move it into `.diskghost-quarantine/` inside the folder `index
    /// add` was given that holds it (its volume's root only when none does)
    /// — a same-volume rename, never a delete. Journaled to
    /// `<plan>.journal.jsonl`; a re-run resumes, `undo` reverses.
    Apply {
        /// The plan file.
        plan: PathBuf,
        /// Index database (default: as `diskghost index`).
        #[arg(long, value_name = "FILE")]
        db: Option<PathBuf>,
        /// Check and report only: no move, no folder, no journal.
        #[arg(long)]
        dry_run: bool,
        /// Also compare each file byte by byte with a kept copy first.
        #[arg(long)]
        paranoid: bool,
    },
    /// Reverse an apply: move every quarantined file of a journal back.
    Undo {
        /// The journal (`<plan>.journal.jsonl`).
        journal: PathBuf,
        /// Report only; move nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete a file or folder — permanently, or to the OS trash. Dry-run by default.
    Rm {
        /// File or directory to remove.
        path: PathBuf,
        /// Send to the OS trash / recycle bin (reversible) instead of deleting.
        #[arg(long)]
        trash: bool,
        /// Actually remove (default: dry-run that only reports what would go).
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum IndexAction {
    /// Index folders (each on one volume; other mounts below are not
    /// entered): new and changed files are recorded, unchanged ones are not
    /// read, files gone from the folder are dropped from the index, and files
    /// whose size matches another indexed file get their ends hashed.
    ///
    /// A folder may be remote: `ssh://host/path` (`ssh://host//abs/path`
    /// for an absolute path). The host lists and hashes its own files — no
    /// file content crosses the network. On a Linux x86_64 host this program
    /// copies itself to ~/.cache/diskghost/ there (once per version) and runs
    /// as a read-only agent (BLAKE3); elsewhere, or with `--agent never`, the
    /// host's own tools are used (find, stat, b3sum / md5sum / sha1sum).
    /// Without b3sum those MD5 / SHA-1 digests are only compared with each
    /// other.
    Add {
        /// Folders to index (local paths or ssh://host/path).
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Role of the volume in later copy policies.
        #[arg(long, value_enum)]
        role: Option<RoleArg>,
        /// Host the volume lives on: local / localhost. A remote folder is
        /// given as ssh://host/path instead.
        #[arg(long, default_value = "local")]
        host: String,
        /// Remote folders: `auto` uploads this program to
        /// ~/.cache/diskghost/ on a Linux x86_64 host and lets it list and
        /// hash there (falling back to the host's shell tools when it cannot
        /// run); `never` always uses the shell tools.
        #[arg(long, value_enum, default_value = "auto")]
        agent: AgentArg,
        /// Index a folder on an sshfs mount anyway. Without it such a folder
        /// is refused with the ssh://host/path that indexes the same files:
        /// the host lists them itself instead of one network round trip per
        /// file through the mount.
        #[arg(long)]
        allow_sshfs: bool,
        /// On a ZFS dataset, rescan from `zfs diff <snapshot> <dataset>`
        /// (the newest snapshot older than the last complete scan) instead
        /// of walking. Opt-in: zfs makes a temporary snapshot for the diff
        /// and needs root or the `diff` permission. Falls back to the walk,
        /// and says why, whenever the delta cannot be proven complete. Not
        /// detected: a `zfs rollback` to that snapshot (or a later one) can
        /// bring back files the last scan never saw — rescan without
        /// --zfs-diff after a rollback.
        #[arg(long)]
        zfs_diff: bool,
        #[command(flatten)]
        walk: WalkArgs,
    },
    /// List indexed volumes: role, whether reachable now, files and bytes.
    /// An unplugged volume is shown offline; its entries are kept.
    Status,
    /// Drop a volume and all its entries from the index (files untouched).
    Forget {
        /// The volume's id as `status` shows it.
        uuid: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum AgentArg {
    Auto,
    Never,
}

impl From<AgentArg> for AgentMode {
    fn from(a: AgentArg) -> Self {
        match a {
            AgentArg::Auto => AgentMode::Auto,
            AgentArg::Never => AgentMode::Never,
        }
    }
}

#[derive(Clone, ValueEnum)]
enum RoleArg {
    Normal,
    /// The copy to keep.
    Master,
    /// Copies here are expendable.
    Scratch,
}

impl From<RoleArg> for Role {
    fn from(r: RoleArg) -> Self {
        match r {
            RoleArg::Normal => Role::Normal,
            RoleArg::Master => Role::Master,
            RoleArg::Scratch => Role::Scratch,
        }
    }
}

/// Flags shared by scan + dupes that control how the tree is walked.
#[derive(Args)]
struct WalkArgs {
    /// Glob(s) to exclude (repeatable), matched on path/component/name.
    /// e.g. --exclude "*.tmp" --exclude node_modules
    #[arg(long)]
    exclude: Vec<String>,
    /// Read only this many levels below the root (1 = the root's own entries).
    /// Folders the limit cuts off are not measured: their sizes are reported
    /// as lower bounds, or as not measured at all.
    #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    max_depth: Option<usize>,
    /// Follow symbolic links / junctions.
    #[arg(long)]
    follow_symlinks: bool,
    /// Threads listing directories (default: twice the CPU cores; 2-4 suits a
    /// spinning disk).
    #[arg(long)]
    threads: Option<usize>,
    /// Read the NTFS Master File Table instead of listing directories (much
    /// faster on Windows; needs an administrator console, falls back to the
    /// directory walk otherwise).
    #[arg(long)]
    mft: bool,
    /// Stay on the filesystem of the scan root — never descend into other
    /// mounts (network shares, FUSE folders, drives mounted inside the tree).
    #[arg(short = 'x', long = "one-file-system")]
    one_file_system: bool,
    /// Descend into remote and FUSE filesystems met below the root (NFS, SMB,
    /// sshfs, GNOME gvfs, rclone…). Off by default because a remote tree
    /// cannot be bounded — gvfs serves a fresh inode for every path, so a
    /// remote `/sys` loops forever. A remote path chosen as the scan root is
    /// still scanned; for that prefer `ssh://host//path`.
    #[arg(long)]
    remote: bool,
}

impl WalkArgs {
    fn to_options(&self) -> Options {
        Options {
            max_depth: self.max_depth,
            follow_symlinks: self.follow_symlinks,
            exclude: self.exclude.clone(),
            threads: self.threads,
            mft: self.mft,
            same_file_system: self.one_file_system,
            skip_remote: !self.remote,
            allow_sshfs: false,
            zfs_diff: false,
        }
    }
}

#[derive(Clone, ValueEnum)]
enum ReclaimArg {
    /// Permanently delete redundant copies.
    Delete,
    /// Move redundant copies to the OS trash / recycle bin.
    Trash,
    /// Replace redundant copies with a hard link to the kept file.
    Hardlink,
}

impl From<ReclaimArg> for ReclaimAction {
    fn from(a: ReclaimArg) -> Self {
        match a {
            ReclaimArg::Delete => ReclaimAction::Delete,
            ReclaimArg::Trash => ReclaimAction::Trash,
            ReclaimArg::Hardlink => ReclaimAction::Hardlink,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        // Before anything else: the agent opens no index, no cache, no
        // config — stdout carries protocol frames only.
        Command::Agent => {
            let stdin = std::io::stdin();
            return match diskghost_core::agent::serve(stdin.lock(), std::io::stdout().lock()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("agent: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Mcp { db } => return mcp::run(db),
        Command::Scan {
            path,
            top,
            walk,
            save,
            since,
            agent,
        } => {
            let remote_url = ssh_url(&path).map(str::to_string);
            if remote_url.is_none() && !path.is_dir() {
                eprintln!("error: not a directory: {}", path.display());
                return ExitCode::FAILURE;
            }
            if !check_globs(&walk.exclude) {
                return ExitCode::FAILURE;
            }
            let opts = walk.to_options();
            // Load the old snapshot before scanning, so a bad file fails fast.
            let old = match since.as_deref().map(Snapshot::load) {
                Some(Ok(s)) => Some(s),
                Some(Err(e)) => {
                    eprintln!("error: cannot read snapshot: {e}");
                    return ExitCode::FAILURE;
                }
                None => None,
            };
            let started = std::time::Instant::now();
            // A remote folder is listed on its host (one filesystem; the
            // walk options do not apply there).
            let (tree, remote) = match &remote_url {
                Some(url) => match with_progress(|p| remote_listing(url, agent.into(), p)) {
                    Ok(l) => (l.tree(), Some((l.transport, l.agent_note.clone()))),
                    Err(e) => {
                        eprintln!("error: scanning {url}: {e}");
                        return ExitCode::FAILURE;
                    }
                },
                None => (with_progress(|p| ScanTree::build(&path, &opts, p)), None),
            };
            let elapsed = started.elapsed();
            let report = tree.root_report(top);
            let snapshot = (old.is_some() || save.is_some()).then(|| Snapshot::capture(&tree));
            let changes = match (&old, &snapshot) {
                (Some(old), Some(new)) => match diff(old, new, &report.root) {
                    Ok(mut d) => {
                        d.truncate(top);
                        Some(d)
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                },
                _ => None,
            };
            // Save before printing, so a failure leaves stdout empty.
            let saved = match (&save, &snapshot) {
                (Some(file), Some(snap)) => {
                    if let Err(e) = snap.save(file) {
                        eprintln!("error: cannot save snapshot: {e}");
                        return ExitCode::FAILURE;
                    }
                    Some(file.as_path())
                }
                _ => None,
            };
            if cli.json {
                let out = ScanOutput {
                    report: &report,
                    diff: changes.as_ref(),
                    engine: tree.engine(),
                    mft_fallback: tree.mft_fallback(),
                    snapshot_saved: saved,
                    transport: remote.as_ref().map(|r| r.0),
                    agent_note: remote.as_ref().and_then(|r| r.1.as_deref()),
                };
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else {
                print_scan(&report, walk.max_depth);
                if let Some(d) = &changes {
                    println!();
                    print_diff(d);
                }
                let engine = if tree.engine() == "mft" {
                    " (NTFS MFT)"
                } else {
                    ""
                };
                println!("\nScanned in {}{engine}", fmt_duration(elapsed));
                if let Some(why) = tree.mft_fallback() {
                    eprintln!("note: --mft not used: {why}");
                }
                if let Some((_, Some(n))) = &remote {
                    eprintln!("note: agent not used — {n}; the host's shell tools were used");
                }
                if let Some(file) = saved {
                    eprintln!("snapshot saved to {}", file.display());
                }
            }
        }
        Command::Diff { old, new, at, top } => {
            let (old, new) = match (Snapshot::load(&old), Snapshot::load(&new)) {
                (Ok(o), Ok(n)) => (o, n),
                (Err(e), _) | (_, Err(e)) => {
                    eprintln!("error: cannot read snapshot: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let at = at.unwrap_or_else(|| new.root.clone());
            let mut report = match diff(&old, &new, &at) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            report.truncate(top);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print_diff(&report);
            }
        }
        Command::Dupes {
            path,
            min_mb,
            walk,
            reclaim: reclaim_arg,
            apply,
            cache,
            index,
            db,
            singles,
            singles_paths,
        } => {
            if index {
                let opts = IndexReportOptions {
                    min_size: min_mb.saturating_mul(1024 * 1024),
                    singles: singles || singles_paths.is_some(),
                    singles_paths: singles_paths.is_some(),
                };
                let to_file = singles_paths.filter(|p| p.as_os_str() != "-");
                return run_index_dupes(cli.json, db, &opts, to_file.as_deref());
            }
            let Some(path) = path else {
                eprintln!("error: give a directory, or --index");
                return ExitCode::FAILURE;
            };
            if !path.is_dir() {
                eprintln!("error: not a directory: {}", path.display());
                return ExitCode::FAILURE;
            }
            if !check_globs(&walk.exclude) {
                return ExitCode::FAILURE;
            }
            let opts = walk.to_options();
            let hashes = cache.as_deref().map(HashCache::load).unwrap_or_default();
            let groups = with_progress(|p| {
                let search =
                    || find_duplicates_cached(&path, min_mb * 1024 * 1024, &opts, p, &hashes);
                // Checkpoint the cache while the search runs, so an
                // interrupted search keeps what it hashed.
                match &cache {
                    Some(file) => hashes.checkpoint_while(file, CHECKPOINT_INTERVAL, search),
                    None => search(),
                }
            });
            // With --json, warnings go into the output instead of stderr.
            let mut warnings = Vec::new();
            if let Some(file) = &cache {
                if let Err(e) = hashes.save(file) {
                    let w = format!("cannot save hash cache: {e}");
                    if cli.json {
                        warnings.push(w);
                    } else {
                        eprintln!("warning: {w}");
                    }
                }
            }
            if !cli.json {
                print_dupes(&groups);
            }
            let reclaimed = reclaim_arg.map(|arg| run_reclaim(&groups, arg.into(), !apply));
            if cli.json {
                let out = DupesOutput {
                    groups: &groups,
                    reclaim: reclaimed.as_ref(),
                    warnings,
                };
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else if let Some(r) = &reclaimed {
                print_reclaim(r);
            }
            if reclaimed.is_some_and(|r| !r.errors.is_empty()) {
                return ExitCode::FAILURE;
            }
        }
        Command::Index { db, action } => return run_index(cli.json, db, action),
        Command::Plan {
            index: _,
            db,
            policy,
            output,
        } => return run_plan(cli.json, db, policy.as_deref(), &output),
        Command::Apply {
            plan,
            db,
            dry_run,
            paranoid,
        } => return run_apply(cli.json, db, &plan, ApplyOptions { dry_run, paranoid }),
        Command::Undo { journal, dry_run } => match undo_journal(&journal, dry_run) {
            Ok(r) => return finish_run(cli.json, &r, "undo"),
            Err(e) => {
                eprintln!("error: {}: {e}", journal.display());
                return ExitCode::FAILURE;
            }
        },
        Command::Rm { path, trash, apply } => {
            if !path.exists() {
                eprintln!("error: no such path: {}", path.display());
                return ExitCode::FAILURE;
            }
            // Refuse a filesystem / drive root (e.g. `/` or `C:\`) — a path with
            // no parent — so a stray `rm C:\ --apply` can't nuke the volume.
            if path.parent().is_none() {
                eprintln!(
                    "error: refusing to remove a filesystem root: {}",
                    path.display()
                );
                return ExitCode::FAILURE;
            }
            let mode = if trash {
                RemoveMode::Trash
            } else {
                RemoveMode::Delete
            };
            let started = std::time::Instant::now();
            let report = with_progress(|p| remove_path(&path, mode, !apply, p));
            let elapsed = started.elapsed();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print_remove(&report, trash);
                if !report.dry_run {
                    println!("  done in {}", fmt_duration(elapsed));
                }
            }
            if !report.errors.is_empty() {
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

/// Open the index at `--db` or the default place.
fn open_index(db: Option<PathBuf>) -> Result<Index, ExitCode> {
    let Some(db) = db.or_else(default_index_db) else {
        eprintln!("error: no --db given and no data directory ($XDG_DATA_HOME / $HOME) known");
        return Err(ExitCode::FAILURE);
    };
    Index::open(&db).map_err(|e| {
        eprintln!("error: cannot open index {}: {e}", db.display());
        ExitCode::FAILURE
    })
}

/// A journal beside the output belongs to an earlier plan of that name: a
/// new plan there would mix its records with this one's. Shared by `plan -o`
/// and the MCP server's plan files.
pub(crate) fn check_no_journal(output: &Path) -> Result<(), String> {
    let journal = journal_path(output);
    if journal.exists() {
        return Err(format!(
            "{} exists (an earlier apply of a plan at this path); write the \
             new plan elsewhere or move that journal away first",
            journal.display()
        ));
    }
    Ok(())
}

/// `plan --index`: write the plan file, print its summary.
fn run_plan(json: bool, db: Option<PathBuf>, policy: Option<&Path>, output: &Path) -> ExitCode {
    let policy = match policy.map(Policy::load) {
        None => Policy::default(),
        Some(Ok(p)) => p,
        Some(Err(e)) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = check_no_journal(output) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    let idx = match open_index(db) {
        Ok(i) => i,
        Err(code) => return code,
    };
    let plan = match with_progress(|p| make_plan(&idx, &policy, p)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = serde_json::to_string_pretty(&plan).unwrap();
    if let Err(e) = std::fs::write(output, text + "\n") {
        eprintln!("error: cannot write {}: {e}", output.display());
        return ExitCode::FAILURE;
    }
    if json {
        let out = serde_json::json!({
            "plan": output,
            "summary": plan.summary,
            "warnings": plan.warnings,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        print_plan(&plan, output);
    }
    ExitCode::SUCCESS
}

fn print_plan(p: &Plan, output: &Path) {
    for w in &p.warnings {
        eprintln!("warning: {w}");
    }
    let s = &p.summary;
    println!(
        "plan written to {} (min_copies {}, distinct {})",
        output.display(),
        p.policy.min_copies,
        serde_json::to_value(p.policy.distinct)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    );
    println!(
        "  {} group(s), {} file(s) to quarantine ({}), {} group(s) under-protected",
        s.groups,
        s.actions,
        human_size(s.quarantine_bytes),
        s.under_protected_groups
    );
    if s.remote_actions > 0 {
        println!(
            "  {} redundant cop(ies) on remote hosts: listed only — apply never acts there",
            s.remote_actions
        );
    }
    for g in p.groups.iter().take(20) {
        let tag = if g.under_protected {
            "  under-protected: every copy stays"
        } else {
            ""
        };
        println!(
            "\n  {}  {} × {}{}",
            &g.hash[..16],
            g.unique_copies,
            human_size(g.size),
            tag
        );
        for k in &g.kept {
            let why = serde_json::to_value(k.why)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            println!("    keep        {}  ({why})", k.path.display());
        }
        for a in &g.actions {
            if a.remote {
                println!("    remote      {}  (not applied)", a.path.display());
            } else {
                println!("    quarantine  {}", a.path.display());
            }
        }
    }
    if p.groups.len() > 20 {
        println!(
            "\n  … {} more group(s) in the plan file",
            p.groups.len() - 20
        );
    }
    if s.actions > 0 {
        println!(
            "\nreview it, then: diskghost apply {} [--dry-run] [--paranoid]",
            output.display()
        );
    }
}

/// `apply PLAN`.
fn run_apply(json: bool, db: Option<PathBuf>, plan: &Path, opts: ApplyOptions) -> ExitCode {
    let idx = match open_index(db) {
        Ok(i) => i,
        Err(code) => return code,
    };
    match apply_plan(plan, &idx, &opts) {
        Ok(r) => finish_run(json, &r, "apply"),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Print an apply/undo report; failure when anything was refused or failed.
fn finish_run(json: bool, r: &RunReport, what: &str) -> ExitCode {
    if json {
        println!("{}", serde_json::to_string_pretty(r).unwrap());
    } else {
        for o in &r.outcomes {
            let st = serde_json::to_value(o.status)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
                .unwrap_or_default();
            let to =
                o.to.as_ref()
                    .map(|t| format!(" -> {}", t.display()))
                    .unwrap_or_default();
            let detail = o
                .detail
                .as_ref()
                .map(|d| format!("  ({d})"))
                .unwrap_or_default();
            println!("{st:<20} {}{to}{detail}", o.from.display());
        }
        let n = |s: Status| r.count(s);
        if r.dry_run {
            println!(
                "{what} dry run: {} would move, nothing touched",
                n(Status::WouldQuarantine) + n(Status::WouldRestore)
            );
        } else {
            println!(
                "{what}: {} moved, {} restored, {} skipped, {} remote (never applied), {} left \
                 alone (changed or unverifiable), {} failed — journal {}",
                n(Status::Done),
                n(Status::Undone),
                n(Status::Skipped),
                n(Status::RemoteSkipped),
                n(Status::Stale)
                    + n(Status::HashMismatch)
                    + n(Status::ParanoidMismatch)
                    + n(Status::ParanoidNoReference)
                    + n(Status::Offline),
                n(Status::Error) + n(Status::Conflict),
                r.journal.display()
            );
        }
    }
    if r.has_errors() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `dupes --index`: the report from the persistent index.
fn run_index_dupes(
    json: bool,
    db: Option<PathBuf>,
    opts: &IndexReportOptions,
    singles_file: Option<&Path>,
) -> ExitCode {
    let Some(db) = db.or_else(default_index_db) else {
        eprintln!("error: no --db given and no data directory ($XDG_DATA_HOME / $HOME) known");
        return ExitCode::FAILURE;
    };
    let idx = match Index::open(&db) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: cannot open index {}: {e}", db.display());
            return ExitCode::FAILURE;
        }
    };
    let mut report = match with_progress(|p| index_dupes(&idx, opts, p)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Paths into a file keep stdout short: millions of single files would
    // flood it.
    if let (Some(file), Some(singles)) = (singles_file, report.singles.as_mut()) {
        let mut text = String::new();
        for s in singles.iter_mut() {
            for p in s.paths.take().unwrap_or_default() {
                text.push_str(&p.to_string_lossy());
                text.push('\n');
            }
        }
        if let Err(e) = std::fs::write(file, text) {
            eprintln!("error: cannot write {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        print_index_dupes(&report, singles_file);
    }
    ExitCode::SUCCESS
}

fn print_index_dupes(r: &IndexDupesReport, singles_file: Option<&Path>) {
    for w in &r.warnings {
        eprintln!("warning: {w}");
    }
    if r.locations.is_empty() {
        println!("index is empty — add a folder with `diskghost index add PATH`");
        return;
    }
    println!("Volumes:");
    for l in &r.locations {
        let mut tags = vec![if l.available { "online" } else { "offline" }];
        if l.rotational {
            tags.push("spinning");
        }
        if l.cow {
            tags.push("copy-on-write");
        }
        println!("  {}  {}  [{}]", l.uuid, l.root.display(), tags.join(", "));
    }
    println!();
    if r.groups.is_empty() {
        println!("No duplicates found.");
    } else {
        let total: u64 = r.groups.iter().map(|g| g.reclaimable_bytes).sum();
        let now: u64 = r.groups.iter().map(|g| g.actionable_bytes).sum();
        println!(
            "Found {} duplicate group(s) — {} reclaimable, {} of it on volumes online now:\n",
            r.groups.len(),
            human_size(total),
            human_size(now)
        );
        for g in &r.groups {
            let id = g
                .hash
                .as_deref()
                .or(g.sample.as_deref())
                .map_or("", |h| &h[..12]);
            let algo = if g.algo == "blake3" {
                String::new()
            } else {
                format!(" {}", g.algo)
            };
            print!(
                "  {} cop{} x {} ({} reclaimable) [{id}{algo}]",
                g.unique_copies,
                if g.unique_copies == 1 { "y" } else { "ies" },
                human_size(g.size),
                human_size(g.reclaimable_bytes),
            );
            if g.probable_copies > 0 {
                print!(" +{} probable", g.probable_copies);
            }
            if g.savings_may_be_shared {
                print!(" — copy-on-write: copies may share blocks, deleting may free nothing");
            }
            println!();
            for c in &g.copies {
                let what = match (c.suggest, c.available, c.probable) {
                    (Some(Suggest::Keep), ..) => "keep",
                    (Some(Suggest::Reclaimable), ..) => "reclaimable",
                    (None, _, true) => "probable (offline, not verified)",
                    (None, false, _) => "offline",
                    (None, true, _) => "",
                };
                println!("      {:<12} {}", what, c.path.display());
                for n in &c.shared_inodes {
                    println!("      {:<12} {} (same file, another name)", "", n.display());
                }
            }
            println!();
        }
    }
    if !r.unverifiable.is_empty() {
        println!(
            "Unverifiable: {} set(s) of same-size files hashed with different algorithms \
             (install b3sum on the remote host to compare them):",
            r.unverifiable.len()
        );
        for u in &r.unverifiable {
            println!("  {} [{}]", human_size(u.size), u.algos.join(" vs "));
            for c in &u.copies {
                println!("      {:<7} {}", c.algo, c.path.display());
            }
        }
        println!();
    }
    if let Some(singles) = &r.singles {
        let files: u64 = singles.iter().map(|s| s.files).sum();
        let bytes: u64 = singles.iter().map(|s| s.bytes).sum();
        println!(
            "Single-copy files (no other copy anywhere): {files}, {}",
            human_size(bytes)
        );
        for s in singles {
            let state = if s.available { "online" } else { "offline" };
            println!(
                "  {}  {}  [{state}]: {} file(s), {}",
                s.location_uuid,
                s.root.display(),
                s.files,
                human_size(s.bytes)
            );
            for p in s.paths.iter().flatten() {
                println!("      {}", p.display());
            }
        }
        if let Some(f) = singles_file {
            println!("  paths written to {}", f.display());
        }
    }
    let st = &r.stats;
    if st.sampled + st.full_hashed > 0 {
        println!(
            "\nRead now (stored for next time): {} file end(s), {} whole file(s), {}",
            st.sampled,
            st.full_hashed,
            human_size(st.bytes_read)
        );
    }
}

/// Where the index lives without `--db`: the XDG data directory.
fn default_index_db() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))?;
    Some(base.join("diskghost").join("index.db"))
}

fn run_index(json: bool, db: Option<PathBuf>, action: IndexAction) -> ExitCode {
    // Check what can be checked before the database is even opened.
    if let IndexAction::Add {
        paths, host, walk, ..
    } = &action
    {
        if let Err(e) = validate_host(host) {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
        if !check_globs(&walk.exclude) {
            return ExitCode::FAILURE;
        }
        for p in paths {
            match ssh_url(p) {
                Some(url) => {
                    if let Err(e) = parse_ssh_url(url) {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                    if !matches!(host.as_str(), "local" | "localhost") {
                        eprintln!("error: --host does not apply to {url}: its host is in the URL");
                        return ExitCode::FAILURE;
                    }
                }
                None if !p.is_dir() => {
                    eprintln!("error: not a directory: {}", p.display());
                    return ExitCode::FAILURE;
                }
                None => {}
            }
        }
    }
    let Some(db) = db.or_else(default_index_db) else {
        eprintln!("error: no data directory (set HOME or XDG_DATA_HOME, or pass --db)");
        return ExitCode::FAILURE;
    };
    let idx = match Index::open(&db) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: cannot open index {}: {e}", db.display());
            return ExitCode::FAILURE;
        }
    };
    match action {
        IndexAction::Add {
            paths,
            role,
            host,
            agent,
            allow_sshfs,
            zfs_diff,
            walk,
        } => {
            let opts = Options {
                allow_sshfs,
                zfs_diff,
                ..walk.to_options()
            };
            let role = role.map(Role::from);
            let mut done = Vec::new();
            for path in &paths {
                let scanned = match ssh_url(path) {
                    Some(url) => {
                        let h = parse_ssh_url(url).map(|(h, _)| h).unwrap_or_default();
                        with_progress(|p| index_remote(&idx, url, agent.into(), p)).and_then(|s| {
                            idx.configure(&s.uuid, role, &format!("ssh://{h}"))
                                .map(|()| s)
                        })
                    }
                    None => with_progress(|p| index_scan(&idx, path, &opts, p))
                        .and_then(|s| idx.configure(&s.uuid, role, &host).map(|()| s)),
                };
                match scanned {
                    Ok(s) => {
                        if !json {
                            print_index_add(path, &s);
                        }
                        done.push(s);
                    }
                    Err(e) => {
                        eprintln!("error: indexing {}: {e}", path.display());
                        return ExitCode::FAILURE;
                    }
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&done).unwrap());
            }
        }
        IndexAction::Status => match idx.status() {
            Ok(st) if json => println!("{}", serde_json::to_string_pretty(&st).unwrap()),
            Ok(st) => print_index_status(&st),
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        },
        IndexAction::Forget { uuid } => match idx.forget(&uuid) {
            Ok(true) => {
                if json {
                    println!("{}", serde_json::json!({ "forgotten": uuid }));
                } else {
                    println!("forgot {uuid} (index entries only; no file was touched)");
                }
            }
            Ok(false) => {
                eprintln!("error: no location {uuid} in the index");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        },
    }
    ExitCode::SUCCESS
}

/// The `ssh://…` URL a path argument is, if it is one.
fn ssh_url(p: &Path) -> Option<&str> {
    p.to_str().filter(|s| s.starts_with("ssh://"))
}

fn print_index_add(path: &Path, s: &ScanSummary) {
    if let Some(n) = &s.agent_note {
        eprintln!(
            "note: agent not used for {} — {n}; the host's shell tools were used",
            path.display()
        );
    }
    println!(
        "{}  →  volume {} ({})",
        path.display(),
        s.uuid,
        s.root.display()
    );
    match (s.hashed, &s.algo) {
        (Some(h), Some(algo)) => println!(
            "  {} files: {} new, {} changed, {} unchanged; {} vanished; {} hashed on the host ({algo}, via {})",
            s.seen, s.new, s.changed, s.unchanged, s.vanished, h,
            match s.transport.as_deref() {
                Some("agent") => "the diskghost agent",
                _ => "its shell tools",
            }
        ),
        _ => println!(
            "  {} files: {} new, {} changed, {} unchanged; {} vanished; {} sampled",
            s.seen, s.new, s.changed, s.unchanged, s.vanished, s.sampled
        ),
    }
    if let Some(d) = &s.delta {
        println!(
            "  delta: {d} — only the paths it names (and rows it cannot vouch for) were looked at"
        );
    }
    if let Some(n) = &s.delta_note {
        println!("  note: {n}");
    }
    if s.racy_rechecked > 0 {
        println!(
            "  {} re-read although their size and time are unchanged: written within 2 s \
             of the last scan, too close to trust; {} of them had other bytes",
            s.racy_rechecked, s.racy_changed
        );
    }
    if s.algo.as_deref().is_some_and(|a| a != "blake3") {
        println!(
            "  note: no b3sum on the host — its digests are compared only with each other; \
             same-size local files are reported unverifiable"
        );
    }
    if s.skipped > 0 {
        println!(
            "  {} entries skipped (unreadable, non-UTF-8 name, or another device)",
            s.skipped
        );
    }
    if !s.complete {
        println!("  incomplete walk: vanished entries were kept");
    }
}

fn print_index_status(st: &[LocationStatus]) {
    if st.is_empty() {
        println!("index is empty — add a folder with `diskghost index add PATH`");
        return;
    }
    println!(
        "{:<40} {:<8} {:<8} {:>10} {:>10}  {:<20}  ROOT",
        "UUID", "ROLE", "ONLINE", "FILES", "SIZE", "LAST SEEN"
    );
    for l in st {
        let role = match l.role {
            Role::Normal => "normal",
            Role::Master => "master",
            Role::Scratch => "scratch",
        };
        println!(
            "{:<40} {:<8} {:<8} {:>10} {:>10}  {:<20}  {}{}",
            l.uuid,
            role,
            if l.online { "yes" } else { "no" },
            l.files,
            human_size(l.bytes),
            format_timestamp(u64::try_from(l.last_seen).unwrap_or(0)),
            l.root.display(),
            if l.host == "local" {
                String::new()
            } else {
                format!("  (host {})", l.host)
            },
        );
        for r in &l.roots {
            println!("    scan root: {}", r.display());
        }
    }
}

/// `scan --json` output: the report's fields at the top level, plus `diff`
/// when `--since` was given.
#[derive(Serialize)]
struct ScanOutput<'a> {
    #[serde(flatten)]
    report: &'a ScanReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<&'a DiffReport>,
    /// "walk" or "mft".
    engine: &'a str,
    /// Why --mft was asked for but not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    mft_fallback: Option<&'a str>,
    /// Where `--save` wrote the snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_saved: Option<&'a Path>,
    /// A remote scan: `agent` or `shell`.
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<&'a str>,
    /// A remote scan: why the agent was not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_note: Option<&'a str>,
}

/// `dupes --json` output: the groups, plus `reclaim` when `--reclaim` was
/// given and `warnings` for problems that did not stop the search.
#[derive(Serialize)]
struct DupesOutput<'a> {
    groups: &'a [DupGroup],
    #[serde(skip_serializing_if = "Option::is_none")]
    reclaim: Option<&'a ReclaimReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

fn print_diff(d: &DiffReport) {
    println!(
        "Changes in {} since {} (now {})",
        d.root.display(),
        format_timestamp(d.old_taken_at),
        format_timestamp(d.new_taken_at)
    );
    println!(
        "  total: {} ({} -> {}), {:+} file(s)",
        human_delta(d.delta),
        human_size(d.old_size),
        human_size(d.new_size),
        d.files_delta
    );
    if d.is_unchanged() {
        println!("  nothing changed");
        return;
    }
    if !d.grown.is_empty() {
        println!("\nGrew:");
        for g in &d.grown {
            println!(
                "  {:>10}  {}  ({} -> {})",
                human_delta(g.delta),
                g.path.display(),
                human_size(g.old_size),
                human_size(g.new_size)
            );
        }
    }
    if !d.shrunk.is_empty() {
        println!("\nShrank:");
        for s in &d.shrunk {
            println!(
                "  {:>10}  {}  ({} -> {})",
                human_delta(s.delta),
                s.path.display(),
                human_size(s.old_size),
                human_size(s.new_size)
            );
        }
    }
    if !d.added.is_empty() {
        println!("\nNew folders:");
        for a in &d.added {
            println!("  {:>10}  {}", human_size(a.size), a.path.display());
        }
    }
    if !d.removed.is_empty() {
        println!("\nRemoved folders:");
        for r in &d.removed {
            println!("  {:>10}  {}", human_size(r.size), r.path.display());
        }
    }
}

fn print_remove(r: &RemoveReport, trash: bool) {
    let dest = if trash { "trash" } else { "delete" };
    let what = format!(
        "{} file(s), {} dir(s), {}",
        r.files,
        r.dirs,
        human_size(r.bytes)
    );
    if r.dry_run {
        println!("DRY-RUN [{dest}] {}: would remove {what}", r.root.display());
        println!("  pass --apply to actually remove");
    } else {
        println!("[{dest}] {}: removed {what}", r.root.display());
    }
    for e in &r.errors {
        eprintln!("  ! {e}");
    }
    if !r.errors.is_empty() {
        eprintln!("  {} error(s)", r.errors.len());
    }
}

/// Human-friendly elapsed time: sub-second in ms, seconds with one decimal,
/// minutes+seconds when it runs long.
fn fmt_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{} ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let m = (secs / 60.0).floor() as u64;
        let s = secs - (m as f64) * 60.0;
        format!("{m}m {s:.0}s")
    }
}

/// A folder's size as far as it is known: `≥` when part of it was not read,
/// `?` when none of it was.
fn dir_size_text(size: u64, complete: bool) -> String {
    match (complete, size) {
        (true, _) => human_size(size),
        (false, 0) => "?".to_string(),
        (false, _) => format!("≥ {}", human_size(size)),
    }
}

fn print_scan(r: &ScanReport, max_depth: Option<usize>) {
    println!("Scan of {}", r.root.display());
    let at_least = if r.complete { "" } else { "≥ " };
    println!(
        "  total: {at_least}{} across {at_least}{} files in {at_least}{} dirs",
        human_size(r.total_size),
        r.total_files,
        r.total_dirs
    );
    if r.disk_total > 0 {
        let used = r.disk_total.saturating_sub(r.disk_free);
        let pct = (used as f64 / r.disk_total as f64) * 100.0;
        println!(
            "  disk: {} free of {} ({:.0}% used)",
            human_size(r.disk_free),
            human_size(r.disk_total),
            pct
        );
    }
    if r.skipped > 0 {
        println!("  skipped: {} unreadable entries (permissions?)", r.skipped);
    }
    if r.cancelled {
        println!("  CANCELLED: partial result, the numbers cover only what was scanned");
    } else if !r.complete {
        if r.boundary_cuts > 0 {
            println!(
                "  BOUNDARY: {} folder(s) on other filesystems were listed but not read",
                r.boundary_cuts
            );
        }
        if max_depth.is_some() {
            let limit = max_depth.map_or(String::new(), |m| format!(" (--max-depth {m})"));
            println!(
                "  DEPTH LIMIT{limit}: deeper folders were not read; ≥ marks a lower bound, ? a folder not measured"
            );
        }
        if r.boundary_cuts == 0 && max_depth.is_none() {
            println!(
                "  INCOMPLETE: some folders were not read; ≥ marks a lower bound, ? a folder not measured"
            );
        }
    }
    println!();

    println!("Biggest sub-folders:");
    for d in &r.children {
        let note = if !d.complete && d.size == 0 {
            "  (not measured)"
        } else {
            ""
        };
        println!(
            "  {:>10}  {}{note}",
            dir_size_text(d.size, d.complete),
            d.path.display()
        );
    }
    if r.root_files_count > 0 {
        println!(
            "  {:>10}  ({} file(s) directly in root)",
            human_size(r.root_files_size),
            r.root_files_count
        );
    }

    println!("\nBiggest files:");
    for f in &r.top_files {
        println!("  {:>10}  {}", human_size(f.size), f.path.display());
    }
}

fn print_dupes(groups: &[DupGroup]) {
    if groups.is_empty() {
        println!("No duplicates found.");
        return;
    }
    let total: u64 = groups.iter().map(|g| g.wasted).sum();
    println!(
        "Found {} duplicate group(s) — {} reclaimable:\n",
        groups.len(),
        human_size(total)
    );
    for g in groups {
        println!(
            "  {} x {} ({} wasted) [{}]",
            g.files.len(),
            human_size(g.size),
            human_size(g.wasted),
            &g.hash[..12]
        );
        for f in &g.files {
            println!("      {}", f.display());
        }
        println!();
    }
}

/// Reclaim space across all groups, keeping the first file of each group.
fn run_reclaim(groups: &[DupGroup], action: ReclaimAction, dry_run: bool) -> ReclaimReport {
    let mut total = ReclaimReport {
        removed: 0,
        reclaimed: 0,
        errors: Vec::new(),
        dry_run,
    };
    for g in groups {
        if g.files.len() < 2 {
            continue;
        }
        let keep = &g.files[0];
        let report = reclaim(keep, &g.files[1..], g.size, action, dry_run);
        total.removed += report.removed;
        total.reclaimed += report.reclaimed;
        total.errors.extend(report.errors);
    }
    total
}

fn print_reclaim(r: &ReclaimReport) {
    for e in &r.errors {
        eprintln!("  ! {e}");
    }
    let mode = if r.dry_run {
        "DRY-RUN — nothing changed; pass --apply to act"
    } else {
        "applied"
    };
    print!(
        "\nReclaim [{mode}]: {} file(s), {} reclaimable",
        r.removed,
        human_size(r.reclaimed)
    );
    if !r.errors.is_empty() {
        print!(", {} error(s)", r.errors.len());
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote/FUSE mount below the scan root is never walked by default:
    /// gvfs serves fresh inodes per path, so an unbounded `scan /` loops on a
    /// remote `/sys` forever (observed: an hour inside sftp:host=example).
    /// `--remote` opts out deliberately.
    #[test]
    fn walks_stay_off_remote_filesystems_below_the_root() {
        let base = WalkArgs {
            exclude: vec![],
            max_depth: None,
            follow_symlinks: false,
            threads: None,
            mft: false,
            one_file_system: false,
            remote: false,
        };
        assert!(base.to_options().skip_remote);
        let crossed = WalkArgs {
            remote: true,
            ..base
        };
        assert!(!crossed.to_options().skip_remote);
    }
}
