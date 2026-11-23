"use strict";

// Pure helpers shared by the UI (as globals) and its unit tests (as a Node
// module). Nothing in here touches the DOM or Tauri.
(function (root, factory) {
  const lib = factory();
  if (typeof module !== "undefined" && module.exports) module.exports = lib;
  else Object.assign(root, lib);
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  // Bytes as a short human string: 1536 -> "1.5 KB".
  function human(bytes) {
    const u = ["B", "KB", "MB", "GB", "TB", "PB"];
    let s = bytes, i = 0;
    while (s >= 1024 && i < u.length - 1) { s /= 1024; i++; }
    return i === 0 ? `${bytes} B` : `${s.toFixed(1)} ${u[i]}`;
  }

  // Whether a folder entry was not read at all (beyond the depth limit). An
  // entry without `complete` was read in full.
  function unmeasured(item) {
    return item.complete === false && !item.size;
  }

  // A folder's size as far as it is known: "≥ 1.5 KB" when part of it was not
  // read, `notMeasured` when none of it was.
  function sizeText(item, notMeasured) {
    if (item.complete !== false) return human(item.size);
    return unmeasured(item) ? notMeasured : `≥ ${human(item.size)}`;
  }

  // A folder's file count, formatted by `fmt`, the same way ("—" when not
  // read at all).
  function countText(item, fmt) {
    const n = fmt(item.file_count);
    if (item.complete !== false) return n;
    return unmeasured(item) ? "—" : `≥ ${n}`;
  }

  // A signed byte difference: +1.5 KB / -1.5 KB / 0 B.
  function humanDelta(delta) {
    if (!delta) return "0 B";
    return (delta > 0 ? "+" : "-") + human(Math.abs(delta));
  }

  function fmtMs(ms) {
    if (ms < 1000) return `${Math.round(ms)} ms`;
    const s = ms / 1000;
    if (s < 60) return `${s.toFixed(1)} s`;
    const m = Math.floor(s / 60);
    return `${m}m ${Math.round(s - m * 60)}s`;
  }

  function baseName(p) {
    const parts = String(p).split(/[\\/]/).filter(Boolean);
    return parts[parts.length - 1] || p;
  }

  // Unix seconds -> "YYYY-MM-DD HH:MM" in the viewer's local time.
  function fmtDate(secs) {
    const d = new Date(secs * 1000);
    const p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
  }

  // Breadcrumb segments for a path: [{label, target}], where `target` is the
  // full path to scan when that segment is clicked. A drive root becomes
  // "C:\" (not "C:", which Windows resolves to the drive's working directory),
  // a POSIX root "/", and a UNC share "\\server\share" stays one segment.
  function crumbs(path) {
    const p = String(path);
    // A remote scan, `ssh://host//abs/path`: the host's own POSIX crumbs,
    // each target again an ssh:// URL (the form the scan's root has).
    const ssh = p.match(/^(ssh:\/\/[^/]+)\/(.*)$/);
    if (ssh) {
      const rest = ssh[2].startsWith("/") ? ssh[2] : `/${ssh[2]}`;
      return crumbs(rest).map((c, i) => ({
        label: i === 0 ? `${ssh[1].slice(6)}:/` : c.label,
        target: `${ssh[1]}/${c.target}`,
      }));
    }
    const win = p.includes("\\") || /^[A-Za-z]:/.test(p);
    const sep = win ? "\\" : "/";
    let parts = p.split(/[\\/]+/).filter((s, i) => s !== "" || i === 0);
    const out = [];
    let acc = "";
    if (win && p.startsWith("\\\\") && parts.length >= 3) {
      acc = `\\\\${parts[1]}\\${parts[2]}`;
      out.push({ label: acc, target: acc });
      parts = parts.slice(3);
    } else if (parts.length && parts[0] === "") {
      acc = "/";
      out.push({ label: "/", target: "/" });
      parts = parts.slice(1);
    } else if (parts.length && win && /^[A-Za-z]:$/.test(parts[0])) {
      acc = `${parts[0]}\\`;
      out.push({ label: parts[0], target: acc });
      parts = parts.slice(1);
    }
    for (const part of parts) {
      acc = acc === "" || acc.endsWith(sep) ? acc + part : acc + sep + part;
      out.push({ label: part, target: acc });
    }
    return out;
  }

  // Classic squarified treemap layout. Returns [{item,x,y,w,h}].
  function squarify(items, x, y, w, h) {
    const total = items.reduce((a, i) => a + i.size, 0) || 1;
    const scale = (w * h) / total;
    const boxes = items.map((it) => ({ item: it, area: it.size * scale }));
    const out = [];
    let cx = x, cy = y, cw = w, ch = h;

    const worst = (row, len) => {
      const s = row.reduce((a, r) => a + r.area, 0);
      const mx = Math.max(...row.map((r) => r.area));
      const mn = Math.min(...row.map((r) => r.area));
      return Math.max((len * len * mx) / (s * s), (s * s) / (len * len * mn));
    };
    const layout = (row, horizontal) => {
      const s = row.reduce((a, r) => a + r.area, 0);
      if (horizontal) {
        const rh = s / cw;
        let px = cx;
        for (const r of row) { const rw = r.area / rh; out.push({ item: r.item, x: px, y: cy, w: rw, h: rh }); px += rw; }
        cy += rh; ch -= rh;
      } else {
        const rw = s / ch;
        let py = cy;
        for (const r of row) { const rh = r.area / rw; out.push({ item: r.item, x: cx, y: py, w: rw, h: rh }); py += rh; }
        cx += rw; cw -= rw;
      }
    };

    let i = 0;
    while (i < boxes.length && cw > 0.5 && ch > 0.5) {
      const horizontal = cw >= ch;
      const len = horizontal ? cw : ch;
      const row = [boxes[i]];
      let j = i + 1;
      while (j < boxes.length && worst(row.concat(boxes[j]), len) <= worst(row, len)) {
        row.push(boxes[j]); j++;
      }
      layout(row, horizontal);
      i = j;
    }
    return out;
  }

  // How to colour a folder by its change: a CSS class and a 0..1 strength
  // relative to the biggest change on screen.
  function deltaTint(delta, maxAbs) {
    if (!delta) return { cls: "", strength: 0 };
    const strength = maxAbs > 0 ? Math.min(1, Math.abs(delta) / maxAbs) : 1;
    return { cls: delta > 0 ? "grow" : "shrink", strength };
  }

  // Index a diff report's folder changes by path so rows/tiles can look
  // theirs up: {path -> {delta}} for grown/shrunk, {path -> {added:true}}
  // for new folders. Also returns the biggest |delta| for tint scaling.
  function changesByPath(diff) {
    const map = new Map();
    let maxAbs = 0;
    for (const g of diff.grown || []) { map.set(String(g.path), { delta: g.delta }); maxAbs = Math.max(maxAbs, Math.abs(g.delta)); }
    for (const s of diff.shrunk || []) { map.set(String(s.path), { delta: s.delta }); maxAbs = Math.max(maxAbs, Math.abs(s.delta)); }
    for (const a of diff.added || []) { map.set(String(a.path), { added: true, delta: a.size }); maxAbs = Math.max(maxAbs, a.size); }
    return { map, maxAbs };
  }

  // `part` as a share of `whole`: "42%", "0.4%", "<0.1%"; "" if whole is 0.
  function pct(part, whole) {
    if (!whole) return "";
    const p = (100 * part) / whole;
    if (p > 0 && p < 0.1) return "<0.1%";
    return p < 10 ? `${p.toFixed(1)}%` : `${Math.round(p)}%`;
  }

  // Exclude templates by ecosystem. A pattern matches a whole path, any path
  // component or a file name (see the core's `Options::exclude`), so
  // `node_modules` drops every such folder and `*.pyc` every such file.
  const EXCLUDE_PRESETS = [
    { id: "vcs", patterns: [".git", ".svn", ".hg"] },
    { id: "node", patterns: ["node_modules", ".next", ".nuxt", ".svelte-kit", ".parcel-cache", ".turbo", "bower_components"] },
    { id: "python", patterns: ["__pycache__", ".venv", "venv", ".tox", ".pytest_cache", ".mypy_cache", ".ruff_cache", "*.pyc"] },
    { id: "rust", patterns: ["target"] },
    { id: "jvm", patterns: [".gradle", "build", "out"] },
    { id: "dotnet", patterns: ["bin", "obj", "packages"] },
    { id: "vendor", patterns: ["vendor"] },
    { id: "cpp", patterns: ["CMakeFiles", "cmake-build-*", "*.o", "*.obj", "*.pch", "*.ipch"] },
    { id: "dart", patterns: [".dart_tool", ".pub-cache"] },
    { id: "ide", patterns: [".idea", ".vs", ".vscode", "*.swp"] },
    { id: "caches", patterns: [".cache", "Cache", "cache", "Code Cache", "GPUCache"] },
    { id: "temp", patterns: ["Temp", "tmp", "*.tmp", "*.temp", "*.dmp"] },
    { id: "logs", patterns: ["*.log"] },
    { id: "winsys", patterns: ["$Recycle.Bin", "System Volume Information", "pagefile.sys", "hiberfil.sys", "swapfile.sys"] },
    { id: "macos", patterns: [".DS_Store", ".Trash", ".Spotlight-V100", ".fseventsd"] },
  ];

  // Bumped when a default changes in a way saved settings should pick up.
  const SETTINGS_VERSION = 2;

  const DEFAULT_SETTINGS = {
    version: SETTINGS_VERSION,
    top: 50, // folders / files listed per view
    min_mb: 1, // duplicate search ignores smaller files
    max_depth: null,
    follow_symlinks: false,
    trash: true, // delete -> Recycle Bin
    parallel: 4, // scans running at once; the rest wait in the queue
    engine: "fast", // "fast" (SSD/NVMe, many threads) | "gentle" (HDD / background)
    keep_snapshots: 10, // snapshots kept per scanned folder
    presets: [], // enabled exclude template ids
    preset_off: [], // "id:pattern" switched off inside an enabled template
    custom: [], // [{ pattern, on }]
    window_size: "auto",
    language: "auto",
    theme: "auto", // "auto" follows the OS; "dark" | "light" pin it
    drive_view: "flat", // sidebar drives: "flat" list | "tree" grouped by media
    remotes: [], // remembered ssh:// scan roots: [{ url, label }]
  };

  // Saved settings over the defaults: unknown keys are dropped, wrong types
  // fall back, numbers are clamped — a hand-edited file can't break the UI.
  function mergeSettings(saved) {
    const s = { ...DEFAULT_SETTINGS, presets: [], preset_off: [], custom: [] };
    if (!saved || typeof saved !== "object") return s;
    const int = (v, lo, hi, dflt) => (Number.isInteger(v) ? Math.min(hi, Math.max(lo, v)) : dflt);
    s.top = int(saved.top, 1, 1000, s.top);
    s.min_mb = int(saved.min_mb, 0, 1_000_000, s.min_mb);
    s.max_depth = saved.max_depth == null ? null : int(saved.max_depth, 1, 100, null);
    // Before version 2 at most 2 scans ran at once; that default is gone.
    if (saved.version >= 2) s.parallel = int(saved.parallel, 1, 8, s.parallel);
    s.keep_snapshots = int(saved.keep_snapshots, 1, 100, s.keep_snapshots);
    if (["fast", "gentle", "mft"].includes(saved.engine)) s.engine = saved.engine;
    if (typeof saved.follow_symlinks === "boolean") s.follow_symlinks = saved.follow_symlinks;
    if (typeof saved.trash === "boolean") s.trash = saved.trash;
    const ids = new Set(EXCLUDE_PRESETS.map((p) => p.id));
    if (Array.isArray(saved.presets)) s.presets = saved.presets.filter((id) => ids.has(id));
    if (Array.isArray(saved.preset_off)) s.preset_off = saved.preset_off.filter((k) => typeof k === "string");
    if (Array.isArray(saved.custom)) {
      s.custom = saved.custom
        .filter((c) => c && typeof c.pattern === "string" && c.pattern.trim())
        .map((c) => ({ pattern: c.pattern.trim(), on: c.on !== false }));
    }
    if (["auto", "maximized", "1080p", "1440p", "4k"].includes(saved.window_size)) s.window_size = saved.window_size;
    if (["auto", "en", "de"].includes(saved.language)) s.language = saved.language;
    if (["auto", "dark", "light"].includes(saved.theme)) s.theme = saved.theme;
    if (["flat", "tree"].includes(saved.drive_view)) s.drive_view = saved.drive_view;
    if (Array.isArray(saved.remotes)) {
      s.remotes = saved.remotes
        .filter((r) => r && typeof r.url === "string" && r.url.startsWith("ssh://"))
        .map((r) => ({ url: r.url, label: typeof r.label === "string" ? r.label : "" }));
    }
    return s;
  }

  // Every exclude pattern the settings switch on, once each, in a stable order.
  function activeExcludes(s) {
    const out = [];
    const off = new Set(s.preset_off || []);
    for (const p of EXCLUDE_PRESETS) {
      if (!(s.presets || []).includes(p.id)) continue;
      for (const pat of p.patterns) if (!off.has(`${p.id}:${pat}`)) out.push(pat);
    }
    for (const c of s.custom || []) if (c.on) out.push(c.pattern);
    return [...new Set(out)];
  }

  // File extension -> category, for the "what is it made of" chart.
  const CATEGORY_EXTS = {
    video: "mp4 mkv avi mov wmv flv webm m4v mpg mpeg m2ts mts vob 3gp ts",
    image: "jpg jpeg png gif bmp tif tiff webp heic heif avif raw cr2 cr3 nef arw dng psd xcf svg ico",
    audio: "mp3 wav flac aac ogg oga m4a wma opus aiff aif mid midi",
    archive: "zip rar 7z tar gz tgz bz2 xz zst lz4 cab",
    diskimage: "iso img vhd vhdx vmdk vdi qcow2 dmg wim esd",
    document: "pdf doc docx xls xlsx ppt pptx odt ods odp rtf txt md csv epub",
    code: "js mjs cjs ts tsx jsx py rs go java kt c cc cpp h hpp cs php rb swift dart lua json xml yaml yml toml html htm css scss sql sh ps1 bat cmd vue map",
    program: "exe dll sys msi msix so dylib lib a jar apk appx pdb node",
    gamedata: "pak uasset ubulk utoc ucas vpk bsa ba2 unity3d assets resource bundle forge",
    database: "db sqlite sqlite3 mdb accdb mdf ldf ndf",
  };
  const EXT_CATEGORY = new Map();
  for (const [cat, list] of Object.entries(CATEGORY_EXTS)) {
    for (const ext of list.split(" ")) EXT_CATEGORY.set(ext, cat);
  }
  function categoryOf(ext) {
    return EXT_CATEGORY.get(String(ext).toLowerCase()) || "other";
  }

  // Fold per-extension stats into categories, largest first. Each category
  // keeps its extensions (largest first) for the hover detail.
  function categorize(extensions) {
    const cats = new Map();
    for (const e of extensions || []) {
      const id = categoryOf(e.ext);
      const c = cats.get(id) || { id, size: 0, count: 0, exts: [] };
      c.size += e.size;
      c.count += e.count;
      c.exts.push(e);
      cats.set(id, c);
    }
    const out = [...cats.values()];
    for (const c of out) c.exts.sort((a, b) => b.size - a.size);
    return out.sort((a, b) => b.size - a.size);
  }

  // "< 4 KB", "4 KB – 64 KB", "≥ 4 GB" for the size-distribution buckets.
  function bucketLabel(buckets, i) {
    const short = (b) => human(b).replace(".0 ", " ");
    const lo = i > 0 ? buckets[i - 1].upper : null;
    const hi = buckets[i].upper;
    if (lo == null) return `< ${short(hi)}`;
    if (hi == null) return `≥ ${short(lo)}`;
    return `${short(lo)} – ${short(hi)}`;
  }

  // Folders whose files belong to installed programs and games: duplicates in
  // there are on purpose (each game ships its own copy of a DLL, a game keeps
  // two copies of a sound bank) and deleting one breaks the program.
  const PROTECTED_DIRS = new Set([
    "steamapps", "program files", "program files (x86)", "programdata", "windows", "windowsapps",
    "xboxgames", "wpsystem", "epic games", "riot games", "battle.net", "ubisoft", "ubisoft game launcher",
    "ea games", "electronic arts", "origin games", "gog galaxy", "gog games", "rockstar games",
    "roberts space industries", "bluestacks", "appdata", "node_modules", "site-packages", ".venv", "venv", ".git",
  ]);
  const PROTECTED_EXTS = new Set(["pck", "cab", "sld", "bk2", "bik", "vtf", "mdl", "pdb", "ttf", "otf"]);

  // Whether `path` is part of an installed program or game (by folder or by
  // file type) rather than one of the user's own files.
  function isProtectedPath(path) {
    const parts = String(path).split(/[\\/]+/);
    const name = parts[parts.length - 1] || "";
    const ext = (name.match(/\.([^.]+)$/) || [])[1] || "";
    const cat = categoryOf(ext);
    if (cat === "program" || cat === "gamedata" || PROTECTED_EXTS.has(ext.toLowerCase())) return true;
    return parts.slice(0, -1).some((p) => PROTECTED_DIRS.has(p.toLowerCase()));
  }

  // The same protection as exclude patterns, so a duplicate search does not
  // even read program and game files: their folders as path components,
  // their file types as `*.ext`.
  function protectedExcludes() {
    const exts = new Set(PROTECTED_EXTS);
    for (const [ext, cat] of EXT_CATEGORY) if (cat === "program" || cat === "gamedata") exts.add(ext);
    return [...PROTECTED_DIRS, ...[...exts].sort().map((e) => `*.${e}`)];
  }

  // Duplicate groups by the set of folders their copies sit in, so "these
  // two folders share 300 identical files" is one line instead of 300.
  function folderSets(groups) {
    const sets = new Map();
    for (const g of groups) {
      const dirs = [...new Set(g.files.map((f) => parentPath(f) || f))].sort();
      const key = dirs.join("\n");
      const s = sets.get(key) || { key, dirs, groups: 0, files: 0, wasted: 0 };
      s.groups += 1;
      s.files += g.files.length;
      s.wasted += g.wasted;
      sets.set(key, s);
    }
    return [...sets.values()].sort((a, b) => b.wasted - a.wasted || a.key.localeCompare(b.key));
  }

  // Duplicate groups after a reclaim: the copies in `done` are gone (or are
  // hard links now), groups left with one file are no duplicates any more,
  // and what each group wastes is counted again.
  function dropReclaimed(groups, done) {
    const gone = new Set(done || []);
    return (groups || [])
      .map((g) => {
        const files = g.files.filter((f) => !gone.has(f));
        return { ...g, files, wasted: g.size * Math.max(0, files.length - 1) };
      })
      .filter((g) => g.files.length > 1);
  }

  // The media bucket a drive row falls into for the tree view: the
  // backend reports `ssd`/`hdd`; a remembered remote or a network drive
  // is `remote`; anything unlabeled lands in `other`.
  function driveMedia(d) {
    if (d && typeof d.media === "string" && MEDIA_ORDER.includes(d.media)) return d.media;
    return d && d.kind === "network" ? "remote" : "other";
  }

  // Tree order of the media groups: solid state first, then spinning
  // disks, then remote locations, then whatever could not be told.
  const MEDIA_ORDER = ["ssd", "hdd", "remote", "other"];
  const MEDIA_LABEL_KEY = { ssd: "grpSsd", hdd: "grpHdd", remote: "grpRemote", other: "grpOther" };

  // `drives` folded into media groups for the tree view — empty groups
  // are dropped, items keep the backend's order.
  function groupDrives(drives) {
    const out = MEDIA_ORDER.map((key) => ({ key, labelKey: MEDIA_LABEL_KEY[key], items: [] }));
    for (const d of drives || []) out[MEDIA_ORDER.indexOf(driveMedia(d))].items.push(d);
    return out.filter((g) => g.items.length > 0);
  }

  // A remembered ssh:// scan root rendered like a drive row: the label
  // is the host so the row reads "nas" not a bare URL.
  function remoteDrive(r) {
    const host = String(r.url).match(/^ssh:\/\/([^/]+)/)?.[1] || "";
    return {
      path: r.url,
      label: r.label || host,
      fs: "ssh",
      kind: "network",
      media: "remote",
      total: 0,
      free: 0,
    };
  }

  // The whole-disk device a mounted source belongs to, by kernel block-name
  // rules: `nvme0n1p5` → `nvme0n1`, `mmcblk0p2` → `mmcblk0`, `md0p1` → `md0`
  // (a `pN` suffix after a name ending in a digit), `sda2` → `sda`,
  // `vda1` → `vda` (a trailing number on a letters-only base). Names that
  // already are whole disks (`sda`, `nvme0n1`) or stacked/virtual devices
  // (`dm-0`, `md0`, `loop0`, `nbd0`) keep theirs. A source that is no
  // `/dev` node — a ZFS dataset, FUSE, overlay — has no disk: null. This
  // mirrors the backend's sysfs resolution; it is the fallback for drives
  // reported without `device` (Windows, macOS, an older backend).
  function diskOf(source) {
    const s = String(source || "");
    if (!s.startsWith("/dev/")) return null;
    const base = baseName(s);
    if (!base) return null;
    const p = base.match(/^(.+\d)p\d+$/);
    if (p) return p[1];
    if (/^(dm-\d+|md\d+|loop\d+|nbd\d+|mmcblk\d+|sr\d+|zd\d+|pmem\d+|rbd\d+|mtdblock\d+|zram\d+)$/.test(base)) return base;
    const d = base.match(/^([a-zA-Z]+)\d+$/);
    if (d) return d[1];
    return base;
  }

  // The device key one drive groups under in the hardware tree: the
  // backend's resolved `device` first, then the disk its `source` implies.
  // A volume without either keeps its own identity (`self:` — the
  // Windows/macOS fallback, each drive its own device) or groups by
  // filesystem type (`fs:` — ZFS datasets, FUSE: they share no disk).
  function deviceKey(d) {
    if (d && typeof d.device === "string" && d.device) return d.device;
    const disk = d && diskOf(d.source);
    if (disk) return disk;
    if (d && d.source) return `fs:${d.fs || "other"}`;
    return `self:${d && d.path ? d.path : "other"}`;
  }

  // The name a volume row shows: the partition (`nvme0n1p5`, `sda2`), else
  // the mount's label or basename (ZFS datasets, Windows letters).
  function volumeName(d) {
    const src = String(d && d.source || "");
    return (src.startsWith("/dev/") && baseName(src)) || String(d && d.label || "") || baseName(String(d && d.path || ""));
  }

  // The sidebar's physical hardware tree: every drive grouped under the
  // disk its volume sits on — one Drive is one volume, and a volume
  // mounted more than once lists every mountpoint as a sibling row.
  // Returns { disks: [node], network: [drive] } where a node is
  //   { key, name, model, transport, size, total, free, media, virtual,
  //     volumes: [{ name, drive, mounts: [{ path, alias }] }] }
  // — the first mount is the drive's own path, later ones the same
  // filesystem seen again (`alias`). Disks sort by name (nvme0n1 before
  // nvme1n1 before sda); virtual groups (`fs:`/`self:`) come after real
  // disks; remote locations never enter the tree — they are `network`.
  function deviceTree(drives) {
    const order = [];
    const byKey = new Map();
    const network = [];
    for (const d of drives || []) {
      if (driveMedia(d) === "remote") { network.push(d); continue; }
      const key = deviceKey(d);
      let node = byKey.get(key);
      if (!node) {
        node = {
          key,
          name: key.startsWith("fs:") ? String(d.fs || "other")
            : key.startsWith("self:") ? String(d.label || "") || baseName(String(d.path || ""))
            : key,
          model: "", transport: "", size: 0, total: 0, free: 0,
          media: driveMedia(d),
          virtual: key.startsWith("fs:") || key.startsWith("self:"),
          volumes: [],
        };
        byKey.set(key, node);
        order.push(node);
      }
      if (d.device_model && !node.model) node.model = String(d.device_model);
      if (d.device_size && !node.size) node.size = Number(d.device_size) || 0;
      if (d.transport && !node.transport) node.transport = String(d.transport);
      const points = (Array.isArray(d.mounts) && d.mounts.length ? d.mounts : [d.path])
        .map((p, i) => ({ path: String(p), alias: i > 0 }));
      node.volumes.push({ name: volumeName(d), drive: d, mounts: points });
      node.total += Number(d.total) || 0;
      node.free += Number(d.free) || 0;
    }
    const nat = (a, b) => a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" });
    return {
      disks: [
        ...order.filter((n) => !n.virtual).sort(nat),
        ...order.filter((n) => n.virtual).sort(nat),
      ],
      network,
    };
  }

  // The two lines a sidebar drive row shows, split so the part that tells
  // mounts apart survives the 17 rem rail: `title` is the volume label when
  // there is one (the mount's basename on POSIX, the host on a remote, a
  // Windows volume name), else the basename; `sub` is the whole path. One
  // truncated "/media/<user>/…" prefix used to be every row's name.
  function driveLines(d) {
    const path = String(d.path || "");
    return { title: String(d.label || "") || baseName(path), sub: path };
  }

  // Filesystems where stored (logical) size can exceed physically used
  // space — transparent compression and reflinks make a "% of the used
  // space" estimate saturate early and look frozen.
  const COMPRESS_FS = new Set(["btrfs", "zfs", "bcachefs"]);

  // Estimated fraction (0..0.99) of a drive-root scan: bytes found vs the
  // drive's used space — null where the estimate would lie.
  function estimateScan(bytes, drive) {
    if (!drive) return null;
    if (COMPRESS_FS.has(String(drive.fs || "").toLowerCase())) return null;
    const used = drive.total - drive.free;
    return used > 0 ? Math.min(0.99, bytes / used) : null;
  }

  // A scanned logical size larger than the drive's physical used space —
  // compression, reflinks or sparse files, not an error.
  function exceedsUsed(scanned, used) {
    return used > 0 && scanned > used;
  }

  // The target an "add location" accepts: an absolute local path or
  // `ssh://host//abs/path` — the gate the backend and `diskghost index add`
  // apply (a relative remote path could be read as an option by the host's
  // own `find`). {kind, path|url, host?} or {error: "<i18n key>"}.
  function locationTarget(input) {
    const raw = String(input || "");
    if (/[\x00-\x1f\x7f]/.test(raw)) return { error: "locBadPath" };
    const p = raw.trim();
    if (!p) return { error: "locBadPath" };
    if (p.startsWith("ssh://")) {
      const m = p.match(/^ssh:\/\/([^/]*)(?:\/(.*))?$/s);
      const host = (m && m[1]) || "";
      const word = (w) => /^[A-Za-z0-9._-]+$/.test(w) && !w.startsWith("-");
      const ok = host.includes("@") ? host.split("@").length === 2 && host.split("@").every(word) : word(host);
      if (!ok) return { error: "locBadHost" };
      const rel = (m && m[2]) || "";
      if (!rel.startsWith("/")) return { error: "locNotAbsolute" };
      return { kind: "remote", url: p, host };
    }
    const abs = p.startsWith("/") || /^[A-Za-z]:[\\/]/.test(p) || p.startsWith("\\\\");
    return abs ? { kind: "local", path: p } : { error: "locNotAbsolute" };
  }

  // The drive a path lives on ("c:", "\\server\share", "/"), for scheduling.
  function driveKey(path) {
    const p = String(path);
    const m = p.match(/^([A-Za-z]:)/);
    if (m) return m[1].toLowerCase();
    if (p.startsWith("\\\\")) return crumbs(p)[0].target.toLowerCase();
    const ssh = p.match(/^ssh:\/\/[^/]+/);
    if (ssh) return ssh[0].toLowerCase();
    return "/";
  }

  // Which queued jobs may start now so that no more than `maxParallel` run at
  // once. Jobs on a drive nothing is reading yet go first (oldest first among
  // them): different disks scan in parallel for free, the same disk shares
  // its bandwidth.
  function jobsToStart(jobs, maxParallel) {
    const running = jobs.filter((j) => j.state === "running");
    let free = Math.max(0, maxParallel - running.length);
    const busy = new Set(running.map((j) => driveKey(j.root)));
    const queued = jobs.filter((j) => j.state === "queued");
    const out = [];
    for (const idleFirst of [true, false]) {
      for (const j of queued) {
        if (!free || out.includes(j)) continue;
        if (idleFirst && busy.has(driveKey(j.root))) continue;
        out.push(j);
        busy.add(driveKey(j.root));
        free--;
      }
    }
    return out;
  }

  // The tiles of one map level: real folders plus, so the areas add up to the
  // folder's total, the files sitting directly in it and the folders that
  // didn't make the list. Pseudo tiles carry `rest` and no path.
  function mapItems(total, children, rootFilesSize) {
    const items = children.filter((c) => c.size > 0).map((c) => ({ ...c }));
    const listed = items.reduce((a, c) => a + c.size, 0);
    if (rootFilesSize > 0) items.push({ path: null, size: rootFilesSize, rest: "files" });
    const other = total - listed - (rootFilesSize || 0);
    if (other > 0) items.push({ path: null, size: other, rest: "other" });
    return items.sort((a, b) => b.size - a.size);
  }

  // The folder above `path`, or null at a root ("C:\", "/", a UNC share).
  function parentPath(path) {
    const parts = crumbs(path);
    return parts.length > 1 ? parts[parts.length - 2].target : null;
  }

  // Whether `path` is `root` or lies below it. Windows paths compare
  // case-insensitively and with either separator.
  function isUnder(path, root) {
    const win = /\\|^[A-Za-z]:/.test(String(path) + String(root));
    const norm = (s) => {
      let x = String(s);
      if (win) x = x.replace(/\//g, "\\").toLowerCase();
      return x.replace(/[\\/]+$/, "");
    };
    const p = norm(path), r = norm(root);
    return p === r || p.startsWith(r + (win ? "\\" : "/"));
  }

  // Why a view's numbers are lower bounds — every cause that applies, never a
  // guess. `complete` and `boundary_cuts` describe the viewed folder's own
  // subtree; `cancelled` the whole walk; the depth limit is what the scan was
  // started with (`maxDepth`, null = unlimited). A cancelled walk can still
  // have read the viewed folder in full: `partial` is then false and only
  // `cancelledWalk` says that the rest of the scan is incomplete.
  function partialCauses(view, maxDepth) {
    const v = view || {};
    const partial = v.complete === false;
    const causes = [];
    if (partial) {
      if (v.cancelled) causes.push({ kind: "cancelled" });
      if (v.boundary_cuts > 0) causes.push({ kind: "boundary", n: v.boundary_cuts });
      if (maxDepth != null) causes.push({ kind: "depth", n: maxDepth });
      if (!causes.length) causes.push({ kind: "unknown" });
    }
    return { partial, causes, cancelledWalk: Boolean(v.cancelled) };
  }

  // The honest outcome of a duplicate search. The engine returns no groups at
  // all once it sees a cancel, so groups that did arrive are a complete
  // result even if Cancel was pressed meanwhile; an empty answer after a
  // cancel is "cancelled", never "no duplicates". Hashes learned before the
  // stop stay in the cache (checkpointed while the search runs, saved after),
  // but only from the sampling phase (1) on — a cancel while listing (0)
  // learned nothing.
  function dupesOutcome(cancelRequested, groups, lastPhase) {
    const got = Array.isArray(groups) ? groups.length : 0;
    if (got > 0 || !cancelRequested) return { state: "done", hashesKept: false };
    return { state: "cancelled", hashesKept: (lastPhase || 0) >= 1 };
  }

  // Guards async results against arriving out of order: take a ticket before
  // the request, apply the answer only if it is still the newest ticket.
  function latestOnly() {
    let seq = 0;
    return { next: () => ++seq, current: (ticket) => ticket === seq };
  }

  // A typed number clamped into [min, max]; null for an empty or non-numeric
  // field (the caller decides what null means).
  function clampInt(raw, min, max) {
    const v = parseInt(String(raw ?? "").trim(), 10);
    return Number.isFinite(v) ? Math.min(max, Math.max(min, v)) : null;
  }

  // What "reclaim" would do with `groups`: per group the copy kept (chosen in
  // `keep` by hash, else a program's own copy, else the first) and the copies
  // removed — never a program/game file unless `withProt`. Totals cover every
  // group passed in, rendered or not, so a confirmation can name them all.
  function reclaimPlan(groups, keep, withProt) {
    const jobs = [];
    let files = 0, bytes = 0, prot = false;
    for (const g of groups || []) {
      const flags = g.prot || g.files.map(isProtectedPath);
      const chosen = keep && keep.get ? keep.get(g.hash) : null;
      const pi = flags.indexOf(true);
      const kept = chosen && g.files.includes(chosen) ? chosen : g.files[pi >= 0 ? pi : 0];
      const remove = g.files.filter((f, i) => f !== kept && (withProt || !flags[i]));
      if (!remove.length) continue;
      const touchesProt = remove.some(isProtectedPath);
      prot = prot || touchesProt;
      files += remove.length;
      bytes += g.size * remove.length;
      jobs.push({ keep: kept, remove, size: g.size, prot: touchesProt });
    }
    return { jobs, files, bytes, groups: jobs.length, prot };
  }

  return {
    partialCauses, dupesOutcome, latestOnly, clampInt, reclaimPlan,
    human, humanDelta, unmeasured, sizeText, countText, fmtMs, baseName, fmtDate, crumbs, squarify, deltaTint, changesByPath,
    pct, EXCLUDE_PRESETS, DEFAULT_SETTINGS, mergeSettings, activeExcludes, categoryOf,
    categorize, bucketLabel, jobsToStart, driveKey, driveMedia, groupDrives, remoteDrive, driveLines, mapItems, parentPath, isUnder, SETTINGS_VERSION,
    diskOf, deviceKey, volumeName, deviceTree,
    locationTarget,
    estimateScan, exceedsUsed, COMPRESS_FS,
    isProtectedPath, folderSets, protectedExcludes, dropReclaimed,
  };
});
