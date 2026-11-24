# Faster rescans without cheating: what is sound, and where the floor is

Question: can `index add` find the changed fileset on a rescan without a full
readdir + stat walk of the tree? Everything below is judged by one rule: a
shortcut is allowed only if it never reports "unchanged" for a file the full
walk would report as changed (no false negatives), never invents progress,
and never declares a delta it did not observe.

## The baseline predicate (what "changed" means today)

A file is unchanged when its `(size, mtime_ns, inode)` equals its row
(`index.rs`, `scan_with` / `remote_scan_with`). Rows the walk did not see
under the walked subtree are dropped, only after a *complete* walk. Known
blind spot of the predicate itself, independent of any shortcut: a write
that keeps size and inode and then restores the old mtime (`touch -r`,
`utimes`) is invisible. Only ctime (not settable by users) or reading the
content would close it. No shortcut below may be weaker than this predicate.

## 0. Reusing a saved snapshot as the rescan baseline — no gain, rejected

Snapshots (`snapshot.rs`, the GUI store read by the MCP tools
`list_snapshots` / `diff_since`) record, per entry, only the kind, the
front-coded path and the size (plus a file count for folders) —
`snapshot.rs` writer: `put_varint(shared)`, `put_varint(len)`, name,
`put_varint(size)`. No mtime, no inode. As a change baseline that is
strictly weaker than an index row `(size, mtime_ns, inode)`: a same-size
rewrite is invisible to it. And `diff_since` compares two snapshots that
were *each* produced by a complete walk; it reports a delta, it does not
avoid the walk that produces the newer side. The index rows are already
the stronger baseline, so a snapshot adds nothing a rescan could skip on.
Folder entries of a snapshot carry subtree totals, not times, so they
cannot prune either (and §1 shows times would not suffice).

## Racy rows (every local filesystem)

A file written in the same timestamp tick as the scan's `stat`, keeping
its size, leaves `(size, mtime[, inode])` unchanged — invisible to the next
scan (git's "racy" case). exFAT ticks are 10 ms (`s_time_gran`), FAT 2 s,
and the kernel's clock tick is coarse on any filesystem. So a row whose
mtime is within `RACY_WINDOW_NS` (2 s) of its scan's start, or in the
future, is stored with `racy = 1`; the next scan re-reads it even when its
`stat` is unchanged (counted as `racy_rechecked`, not `changed`; of those,
`racy_changed` counts rows whose sample before and after the re-read
differ — a change `stat` missed, verified by the bytes; racy rows are
sampled whatever their size, and one the sampling pass skips is read
directly for the comparison), and the
index report re-reads it too. Remote (`ssh://`) rows are not covered: the
remote clock is not known here.

## 1. Folder mtime / "subtree max mtime" pruning — UNSOUND, rejected

A folder's mtime (POSIX, and NTFS/SMB LastWriteTime alike) changes when an
entry in *that* folder is created, removed or renamed. A write to an
existing file changes only the file's own mtime/ctime: `a/b/c/d/f` rewritten
in place leaves `a`, `a/b`, `a/b/c`, `a/b/c/d` untouched. A stored
per-subtree "max mtime + child count" is a value from the *last* scan; it
cannot bound a write that happened since, and learning the *current* max
needs a stat of every file — the walk we wanted to avoid. Additionally,
folder mtimes can be set back by any user (`touch -d`), and coarse
timestamps (FAT 2 s, some SMB servers) make "same tick" changes invisible.

Even the weaker form — skip `readdir` of a folder whose mtime is unchanged
but still stat every known child — saves only the cheap batched listing,
not the per-file stats, and inherits the same `touch -d` / granularity
holes for new and deleted names. Not worth its risk.

Regression guard: `rescan_detects_deep_in_place_write_with_ancestor_mtimes_unchanged`
(index.rs) rewrites a file four folders deep in place, same size and inode,
and restores every ancestor folder's mtime; the rescan must report
`changed == 1` and re-sample it. Any future subtree prune fails this test.

## 2. "Newer than the last scan" watermark (`find -newermt`) — UNSOUND, rejected

- `touch -d`, `cp -p`, `rsync -t`, tar/zip extraction set mtime into the
  past: a `>` watermark misses them; the equality predicate does not
  (guard: `rescan_detects_deep_mtime_moved_back_same_size`).
- `mv dir1 dir2` changes the path of every file below without touching any
  file's mtime or ctime (only the moved folder's own inode changes).
- Deletions never appear in a "newer" list, while `drop_vanished` needs the
  full set of names under the scan root.

A ctime watermark (`-cnewer`) fixes the first point only; renames of whole
subtrees and deletions still require the full name list.

## 3. Saving the stat we already paid for — SOUND, implemented

The walker (`walk.rs::visit`) already `stat`s every regular file
(`DirEntry::metadata`, same follow/no-follow rule) to size it. The index
then `stat`ed every file **again** (`symlink_metadata` in `record`), and so
did the remote agent's `enumerate`. Now the walker hands its stat over
(`Batch::stats`, `Batch::names_with_stats`) and the index/agent use it; they
`stat` themselves only when the walk took none (the MFT engine, unused by
the index). The decision is unchanged: the same `(size, mtime, inode)`
from the same kind of `stat`, compared with the same row. The observation is
taken a few microseconds earlier, which changes nothing about soundness —
a change after the observation is caught by the next scan either way, and
content is only ever hashed after a fresh re-`stat` still matches the row
(`sample_collisions`, remote `hash` with the `size/mtime/ino` guard).

Measured (2000-file tree, rescan, `strace -f -c`): `statx` 4116 → 2116;
`seen/unchanged/changed/vanished/complete` identical (2000/2000/0/0/true).
On a FUSE volume (gvfs, sshfs) each uncached `stat` is a network round trip,
so this removes up to half of the per-file round trips of a rescan. Not
measured on a real gvfs/sshfs mount: how much the kernel/FUSE attribute
cache already absorbed the second `stat` there is unknown.

## 4. Per volume class: where a cheaper honest delta exists

| Class | Today | Sound shortcut? |
|---|---|---|
| Local disk | parallel walk, one `stat` per file (§3) | Only via a filesystem change journal (below). Walk is seconds. |
| `ssh://host/...` | the walk already runs **on the host** (`find -printf` / agent); only the listing crosses the wire | Walk cost is remote CPU already. A smaller wire delta (host diffs against a baseline whose digest the client verifies) is sound but needs protocol + schema work; worth it only if a measurement shows transfer, not the remote `find`, dominates. Not measured. |
| sshfs mount | refused by `index add` / MCP (`sshfs_root`) with the equivalent URL; `--allow-sshfs` walks it over FUSE (per-file round trips) | Index the same folder as `ssh://host/path` instead: the walk moves to the host. Sound (same predicate, host-side `stat`). The URL is derived from the mount's source (`[user@]host:path`, verified on a live mount; `host:` = remote home); an IPv6 host cannot be written in `ssh://` and gets a `~/.ssh/config` alias hint. No silent redirect: the two are different locations. |
| exFAT / FAT (`exfat`, `vfat`, `msdos`) | walk; no inodes, no ctime, no journal | **No delta shortcut; floor = walk.** The Linux drivers assign `iunique()` inode numbers per in-core inode (`fs/exfat/inode.c` `exfat_build_inode`, `fs/fat/inode.c` `fat_build_inode`): a file gets a new number after a remount (shown on a scratch exFAT image: same numbers after `drop_caches`, new ones after remount). The source also implies a new number whenever an inode is dropped from the cache and looked up again; the owner's X9 Pro1 (298 of 300 unchanged files with new numbers a day later, no remount in the udisks log) fits that, but a controlled eviction was not reproduced. Such locations (`locations.ino_stable = 0`, decided from the live mount table's fstype) store inode 0 and compare **(size, mtime) only** — weaker: a different file renamed over one with the same size and mtime (`cp -p` + rename) is not seen. Without this every rescan reset every hash. `apply` treats inode 0 as unknown (size + mtime + device, `--paranoid` byte-compares). |
| gvfs SMB / MTP (e.g. Shield) | local walk over gvfs FUSE | **No honest shortcut.** No remote exec, no recursive change indicator, folder mtimes do not cover in-place writes, SMB CHANGE_NOTIFY only sees changes while connected. A full listing (+ one `stat` per file, §3) is the floor. Run a host-side indexer (ssh/agent) on the device if it has a shell; otherwise accept the walk. |
| inotify / fanotify | — | Sound only with a continuity proof: watcher running since the last complete scan, no queue overflow (`IN_Q_OVERFLOW`), volume never mounted elsewhere or written offline. Impossible to prove for removable, offline or network volumes; not implemented. |
| ZFS dataset | `index add --zfs-diff` (opt-in) | **Sound delta, implemented.** At a complete scan the newest *existing* snapshot S taken ≥ 2 s before the scan began is recorded (name, guid, creation, dataset guid, scan folder). The next rescan runs `zfs diff -F -H S <dataset>` and looks again at every path it names (both names of a rename; rows of a renamed folder are moved first, so they keep their hashes, then the folder is walked); a folder only "modified" is not walked (each entry has its own line). A file created after S, indexed and deleted again is in neither S nor the live fs — no diff line — so rows whose inode change time is ≥ S's creation − 2 s, rows with unknown ctime (pre-v7) and racy rows are always stat'ed too. Walk instead, with the reason in `delta_note`, when: no snapshot older than the last scan, S pruned or remade (guid), dataset guid changed, `zfs diff` fails (needs root or the delegated `diff` permission), output not parsable, paths outside the mountpoint, `--exclude`/`--max-depth`/`--follow-symlinks`, or the last scan was incomplete. Opt-in because diffing against the live fs makes zfs create (and drop) a temporary snapshot `zfs-diff-<pid>`; diskghost never creates snapshots itself. Assumes the wall clock does not jump back between S and the scan (ctime vs creation). Tested against a fake `zfs`; no pool in CI. |
| btrfs / NTFS / CephFS | — | Filesystem-maintained journals are sound sources: `btrfs subvolume find-new <gen>` (content changes; deletions need the name list), NTFS USN journal (with journal-id + next-USN continuity check), CephFS `ceph.dir.rctime`. Each is a per-filesystem integration with its own continuity check; none implemented here. |

Bottom line: for generic POSIX, FUSE and SMB volumes there is no
mechanism that detects in-place writes without asking every file; the
honest wins are (a) asking each file once (§3, done), and (b) asking on the
machine that owns the disk (`ssh://`, already supported).
