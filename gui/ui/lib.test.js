"use strict";

// Unit tests for the pure UI helpers. Run with: node --test gui/ui/lib.test.js
const test = require("node:test");
const assert = require("node:assert/strict");
const lib = require("./lib.js");

test("human formats bytes like the Rust side", () => {
  assert.equal(lib.human(0), "0 B");
  assert.equal(lib.human(1023), "1023 B");
  assert.equal(lib.human(1024), "1.0 KB");
  assert.equal(lib.human(1536), "1.5 KB");
  assert.equal(lib.human(1024 ** 3 * 2.25), "2.3 GB");
  assert.equal(lib.human(1024 ** 5), "1.0 PB");
  assert.equal(lib.human(1024 ** 6), "1024.0 PB"); // capped at PB
});

test("sizes of folders the depth limit cut off are never shown as empty", () => {
  const fmt = (n) => String(n);
  const full = { size: 1536, file_count: 3 };
  assert.equal(lib.sizeText(full, "n/m"), "1.5 KB");
  assert.equal(lib.countText(full, fmt), "3");
  assert.equal(lib.unmeasured(full), false);
  const empty = { size: 0, file_count: 0, complete: true };
  assert.equal(lib.sizeText(empty, "n/m"), "0 B");
  assert.equal(lib.countText(empty, fmt), "0");
  const partly = { size: 1536, file_count: 3, complete: false };
  assert.equal(lib.sizeText(partly, "n/m"), "≥ 1.5 KB");
  assert.equal(lib.countText(partly, fmt), "≥ 3");
  assert.equal(lib.unmeasured(partly), false);
  const unread = { size: 0, file_count: 0, complete: false };
  assert.equal(lib.sizeText(unread, "n/m"), "n/m");
  assert.equal(lib.countText(unread, fmt), "—");
  assert.equal(lib.unmeasured(unread), true);
});

test("humanDelta carries the sign", () => {
  assert.equal(lib.humanDelta(0), "0 B");
  assert.equal(lib.humanDelta(1536), "+1.5 KB");
  assert.equal(lib.humanDelta(-1536), "-1.5 KB");
  assert.equal(lib.humanDelta(-1), "-1 B");
});

test("fmtMs picks a unit", () => {
  assert.equal(lib.fmtMs(12.4), "12 ms");
  assert.equal(lib.fmtMs(1500), "1.5 s");
  assert.equal(lib.fmtMs(61_000), "1m 1s");
});

test("baseName takes the last segment on either separator", () => {
  assert.equal(lib.baseName("C:\\Users\\me\\Downloads"), "Downloads");
  assert.equal(lib.baseName("/home/me/x/"), "x");
  assert.equal(lib.baseName("/"), "/");
});

test("fmtDate renders local YYYY-MM-DD HH:MM", () => {
  const s = lib.fmtDate(1_600_000_000);
  assert.match(s, /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
  assert.equal(s.slice(0, 4), String(new Date(1_600_000_000 * 1000).getFullYear()));
});

test("crumbs keeps a drive root scannable", () => {
  assert.deepEqual(lib.crumbs("C:\\Users\\me"), [
    { label: "C:", target: "C:\\" },
    { label: "Users", target: "C:\\Users" },
    { label: "me", target: "C:\\Users\\me" },
  ]);
  assert.deepEqual(lib.crumbs("C:\\"), [{ label: "C:", target: "C:\\" }]);
  assert.deepEqual(lib.crumbs("D:\\Media\\"), [
    { label: "D:", target: "D:\\" },
    { label: "Media", target: "D:\\Media" },
  ]);
});

test("crumbs handles POSIX, UNC and relative paths", () => {
  assert.deepEqual(lib.crumbs("/home/me"), [
    { label: "/", target: "/" },
    { label: "home", target: "/home" },
    { label: "me", target: "/home/me" },
  ]);
  assert.deepEqual(lib.crumbs("/"), [{ label: "/", target: "/" }]);
  assert.deepEqual(lib.crumbs("\\\\server\\share\\x"), [
    { label: "\\\\server\\share", target: "\\\\server\\share" },
    { label: "x", target: "\\\\server\\share\\x" },
  ]);
  assert.deepEqual(lib.crumbs("foo/bar"), [
    { label: "foo", target: "foo" },
    { label: "bar", target: "foo/bar" },
  ]);
});

test("squarify tiles the whole area proportionally", () => {
  const items = [{ size: 6 }, { size: 3 }, { size: 1 }];
  const rects = lib.squarify(items, 0, 0, 100, 50);
  assert.equal(rects.length, 3);
  const area = rects.reduce((a, r) => a + r.w * r.h, 0);
  assert.ok(Math.abs(area - 5000) < 1e-6, `area ${area}`);
  assert.ok(Math.abs(rects[0].w * rects[0].h - 3000) < 1e-6);
  for (const r of rects) {
    assert.ok(r.x >= 0 && r.y >= 0 && r.x + r.w <= 100 + 1e-9 && r.y + r.h <= 50 + 1e-9);
  }
  assert.deepEqual(lib.squarify([], 0, 0, 10, 10), []);
});

test("deltaTint scales against the biggest change", () => {
  assert.deepEqual(lib.deltaTint(0, 100), { cls: "", strength: 0 });
  assert.deepEqual(lib.deltaTint(50, 100), { cls: "grow", strength: 0.5 });
  assert.deepEqual(lib.deltaTint(-100, 100), { cls: "shrink", strength: 1 });
  assert.deepEqual(lib.deltaTint(500, 100), { cls: "grow", strength: 1 });
  assert.deepEqual(lib.deltaTint(7, 0), { cls: "grow", strength: 1 });
});

test("changesByPath indexes grown, shrunk and added folders", () => {
  const { map, maxAbs } = lib.changesByPath({
    grown: [{ path: "/a", delta: 300 }],
    shrunk: [{ path: "/b", delta: -50 }],
    added: [{ path: "/c", size: 900 }],
    removed: [{ path: "/d", size: 10 }],
  });
  assert.equal(maxAbs, 900);
  assert.deepEqual(map.get("/a"), { delta: 300 });
  assert.deepEqual(map.get("/b"), { delta: -50 });
  assert.deepEqual(map.get("/c"), { added: true, delta: 900 });
  assert.equal(map.has("/d"), false);
  assert.equal(lib.changesByPath({}).maxAbs, 0);
});

test("pct rounds by magnitude and handles empty wholes", () => {
  assert.equal(lib.pct(1, 0), "");
  assert.equal(lib.pct(50, 100), "50%");
  assert.equal(lib.pct(5, 100), "5.0%");
  assert.equal(lib.pct(1, 100_000), "<0.1%");
  assert.equal(lib.pct(0, 100), "0.0%");
});

test("mergeSettings keeps valid values and repairs the rest", () => {
  assert.deepEqual(lib.mergeSettings(null), lib.DEFAULT_SETTINGS);
  const s = lib.mergeSettings({
    version: 2, top: 5000, parallel: 0, max_depth: 3, trash: false, presets: ["node", "nope"],
    custom: [{ pattern: " *.iso " }, { pattern: "", on: true }, { pattern: "x", on: false }, null],
    window_size: "8k", language: "de", bogus: 1, engine: "gentle", keep_snapshots: 0,
  });
  assert.equal(s.top, 1000);
  assert.equal(s.parallel, 1);
  assert.equal(s.engine, "gentle");
  assert.equal(s.keep_snapshots, 1);
  assert.equal(s.max_depth, 3);
  assert.equal(s.trash, false);
  assert.deepEqual(s.presets, ["node"]);
  assert.deepEqual(s.custom, [{ pattern: "*.iso", on: true }, { pattern: "x", on: false }]);
  assert.equal(s.window_size, "auto");
  assert.equal(s.language, "de");
  assert.equal("bogus" in s, false);
  // The defaults themselves are never shared or mutated.
  s.presets.push("rust");
  assert.deepEqual(lib.DEFAULT_SETTINGS.presets, []);
  // Settings saved before version 2 drop the old "2 at once" default.
  assert.equal(lib.mergeSettings({ parallel: 2 }).parallel, lib.DEFAULT_SETTINGS.parallel);
  assert.equal(lib.mergeSettings({ version: 2, parallel: 2 }).parallel, 2);
  assert.equal(lib.mergeSettings({ engine: "turbo" }).engine, "fast");
});

test("activeExcludes unions templates and custom patterns", () => {
  const s = lib.mergeSettings({
    presets: ["rust", "vcs"],
    preset_off: ["vcs:.hg"],
    custom: [{ pattern: "target", on: true }, { pattern: "*.iso", on: true }, { pattern: "*.bak", on: false }],
  });
  assert.deepEqual(lib.activeExcludes(s), [".git", ".svn", "target", "*.iso"]);
  assert.deepEqual(lib.activeExcludes(lib.mergeSettings({})), []);
  // Every template has a unique id and at least one pattern.
  const ids = lib.EXCLUDE_PRESETS.map((p) => p.id);
  assert.equal(new Set(ids).size, ids.length);
  assert.ok(lib.EXCLUDE_PRESETS.every((p) => p.patterns.length > 0));
});

test("categorize folds extensions into categories", () => {
  assert.equal(lib.categoryOf("MKV"), "video");
  assert.equal(lib.categoryOf(""), "other");
  const cats = lib.categorize([
    { ext: "mkv", size: 100, count: 1 },
    { ext: "jpg", size: 30, count: 3 },
    { ext: "mp4", size: 200, count: 2 },
    { ext: "weird", size: 5, count: 1 },
  ]);
  assert.deepEqual(cats.map((c) => [c.id, c.size, c.count]), [["video", 300, 3], ["image", 30, 3], ["other", 5, 1]]);
  assert.deepEqual(cats[0].exts.map((e) => e.ext), ["mp4", "mkv"]);
  assert.deepEqual(lib.categorize(undefined), []);
});

test("bucketLabel names open and closed ranges", () => {
  const b = [{ upper: 4096 }, { upper: 65536 }, { upper: null }];
  assert.equal(lib.bucketLabel(b, 0), "< 4 KB");
  assert.equal(lib.bucketLabel(b, 1), "4 KB – 64 KB");
  assert.equal(lib.bucketLabel(b, 2), "≥ 64 KB");
});

test("jobsToStart fills free slots oldest first", () => {
  const jobs = [
    { id: 1, state: "done", root: "C:\\" }, { id: 2, state: "running", root: "C:\\" },
    { id: 3, state: "queued", root: "C:\\a" }, { id: 4, state: "queued", root: "C:\\b" }, { id: 5, state: "queued", root: "C:\\c" },
  ];
  assert.deepEqual(lib.jobsToStart(jobs, 3).map((j) => j.id), [3, 4]);
  assert.deepEqual(lib.jobsToStart(jobs, 1), []);
  assert.deepEqual(lib.jobsToStart([], 2), []);
});

test("jobsToStart prefers drives nothing is reading", () => {
  // "Scan all drives" while C: is busy: D: and G: start before more C: work.
  const jobs = [
    { id: 1, state: "running", root: "C:\\" },
    { id: 2, state: "queued", root: "C:\\Users" },
    { id: 3, state: "queued", root: "D:\\" },
    { id: 4, state: "queued", root: "g:\\Games" },
    { id: 5, state: "queued", root: "D:\\x" },
  ];
  assert.deepEqual(lib.jobsToStart(jobs, 3).map((j) => j.id), [3, 4]);
  assert.deepEqual(lib.jobsToStart(jobs, 5).map((j) => j.id), [3, 4, 2, 5]);
  assert.equal(lib.driveKey("c:\\x"), "c:");
  assert.equal(lib.driveKey("/home/me"), "/");
  assert.equal(lib.driveKey("\\\\srv\\share\\x"), "\\\\srv\\share");
});

test("mapItems adds direct files and unlisted folders so areas add up", () => {
  const items = lib.mapItems(100, [{ path: "/a", size: 60 }, { path: "/b", size: 0 }], 15);
  assert.deepEqual(items.map((i) => [i.path, i.size, i.rest]), [
    ["/a", 60, undefined], [null, 25, "other"], [null, 15, "files"],
  ]);
  assert.equal(items.reduce((a, i) => a + i.size, 0), 100);
  assert.deepEqual(lib.mapItems(10, [{ path: "/a", size: 10 }], 0).length, 1);
});

test("parentPath walks up to the root and stops there", () => {
  assert.equal(lib.parentPath("C:\\Users\\me"), "C:\\Users");
  assert.equal(lib.parentPath("C:\\Users"), "C:\\");
  assert.equal(lib.parentPath("C:\\"), null);
  assert.equal(lib.parentPath("/home/me"), "/home");
  assert.equal(lib.parentPath("/home"), "/");
  assert.equal(lib.parentPath("/"), null);
  assert.equal(lib.parentPath("\\\\srv\\share\\x"), "\\\\srv\\share");
});

test("isUnder respects separators and Windows case rules", () => {
  assert.ok(lib.isUnder("C:\\Users\\me", "C:\\"));
  assert.ok(lib.isUnder("c:\\users\\ME\\x", "C:\\Users\\me"));
  assert.ok(lib.isUnder("C:\\Users", "C:\\Users\\"));
  assert.ok(lib.isUnder("C:/Users/me", "C:\\Users"));
  assert.ok(!lib.isUnder("C:\\Users2", "C:\\Users"));
  assert.ok(!lib.isUnder("D:\\x", "C:\\"));
  assert.ok(lib.isUnder("/home/me", "/"));
  assert.ok(lib.isUnder("/home/me", "/home"));
  assert.ok(!lib.isUnder("/home/Me", "/home/me"));
  assert.ok(!lib.isUnder("/homer", "/home"));
});

test("isProtectedPath tells program and game files from the user's own", () => {
  // By folder (any depth, any case) ...
  assert.ok(lib.isProtectedPath("G:\\Steam\\steamapps\\common\\Patron\\Data\\Maps\\Map_16\\Forest.bmp"));
  assert.ok(lib.isProtectedPath("C:\\Program Files\\App\\readme.txt"));
  assert.ok(lib.isProtectedPath("G:\\Epic Games\\Games\\GTAV\\x64\\metadata.dat"));
  assert.ok(lib.isProtectedPath("/home/me/proj/node_modules/a/index.js"));
  // ... or by file type, wherever it sits.
  assert.ok(lib.isProtectedPath("D:\\Downloads\\setup\\libcef.DLL"));
  assert.ok(lib.isProtectedPath("D:\\Stuff\\sound.pck"));
  // The user's own files are not.
  assert.ok(!lib.isProtectedPath("D:\\Photos\\2024\\IMG_0001.jpg"));
  assert.ok(!lib.isProtectedPath("C:\\Users\\me\\Videos\\clip.mp4"));
  assert.ok(!lib.isProtectedPath("/home/me/Documents/report.pdf"));
});

test("folderSets groups duplicates by the folders their copies are in", () => {
  const a = "G:\\Halo\\Sound\\SFX", b = "G:\\Halo\\subgames\\CampaignS1\\Sound\\SFX";
  const sets = lib.folderSets([
    { files: [`${a}\\x.pck`, `${b}\\x.pck`], wasted: 100 },
    { files: [`${b}\\y.pck`, `${a}\\y.pck`], wasted: 50 },
    { files: ["D:\\P\\1.jpg", "D:\\P\\copy of 1.jpg"], wasted: 500 },
  ]);
  assert.equal(sets.length, 2);
  assert.deepEqual(sets[0].dirs, ["D:\\P"]);
  assert.deepEqual(sets[1].dirs, [a, b]);
  assert.equal(sets[1].groups, 2);
  assert.equal(sets[1].files, 4);
  assert.equal(sets[1].wasted, 150);
  assert.deepEqual(lib.folderSets([]), []);
});

test("protectedExcludes covers the protected folders and program file types", () => {
  const ex = lib.protectedExcludes();
  assert.ok(ex.includes("steamapps"));
  assert.ok(ex.includes("program files"));
  assert.ok(ex.includes("*.dll"));
  assert.ok(ex.includes("*.pak"));
  assert.ok(ex.includes("*.pck"));
  assert.ok(!ex.includes("*.jpg"));
  assert.equal(new Set(ex).size, ex.length);
  assert.equal(lib.mergeSettings({ engine: "mft" }).engine, "mft");
});

test("dropReclaimed removes handled copies and finished groups", () => {
  const groups = [
    { hash: "a", size: 10, files: ["/k/a", "/x/a", "/y/a"], wasted: 20 },
    { hash: "b", size: 5, files: ["/k/b", "/x/b"], wasted: 5 },
    { hash: "c", size: 7, files: ["/k/c", "/x/c"], wasted: 7 },
  ];
  const left = lib.dropReclaimed(groups, ["/x/a", "/x/b"]);
  assert.deepEqual(left.map((g) => [g.hash, g.files, g.wasted]), [
    ["a", ["/k/a", "/y/a"], 10],
    ["c", ["/k/c", "/x/c"], 7],
  ]);
  assert.equal(groups[0].files.length, 3); // the input is not modified
  assert.deepEqual(lib.dropReclaimed(groups, []).length, 3);
});

test("partial views name every real cause and never default to the depth limit", () => {
  const whole = lib.partialCauses({ complete: true }, null);
  assert.equal(whole.partial, false);
  assert.deepEqual(whole.causes, []);
  // Cut by a filesystem boundary only: no depth limit was set, so none is named.
  const cut = lib.partialCauses({ complete: false, boundary_cuts: 3 }, null);
  assert.equal(cut.partial, true);
  assert.deepEqual(cut.causes, [{ kind: "boundary", n: 3 }]);
  // Boundary cuts and a depth limit at once: both are reported.
  const both = lib.partialCauses({ complete: false, boundary_cuts: 2 }, 4);
  assert.deepEqual(both.causes.map((c) => c.kind), ["boundary", "depth"]);
  // Incomplete but nothing the UI knows explains it: say so, do not invent.
  assert.deepEqual(lib.partialCauses({ complete: false }, null).causes, [{ kind: "unknown" }]);
  // A cancelled walk whose viewed folder was read in full is not partial here.
  const cw = lib.partialCauses({ complete: true, cancelled: true }, null);
  assert.equal(cw.partial, false);
  assert.equal(cw.cancelledWalk, true);
  assert.deepEqual(lib.partialCauses({ complete: false, cancelled: true }, null).causes, [{ kind: "cancelled" }]);
});

test("a cancelled duplicate search is never reported as 'no duplicates'", () => {
  // Engine returns [] on cancel: with Cancel pressed that is a cancelled search.
  assert.deepEqual(lib.dupesOutcome(true, [], 2), { state: "cancelled", hashesKept: true });
  // Cancelled while still listing files: nothing was hashed, nothing kept.
  assert.deepEqual(lib.dupesOutcome(true, [], 0), { state: "cancelled", hashesKept: false });
  // Groups arrived: the engine finished before it saw the cancel — complete.
  assert.equal(lib.dupesOutcome(true, [{ hash: "a" }], 2).state, "done");
  // No cancel and no groups: genuinely no duplicates.
  assert.equal(lib.dupesOutcome(false, [], 2).state, "done");
});

test("latestOnly drops answers that arrive after a newer request", () => {
  const g = lib.latestOnly();
  const first = g.next();
  const second = g.next();
  assert.equal(g.current(first), false);
  assert.equal(g.current(second), true);
});

test("clampInt clamps typed numbers and maps junk to null", () => {
  assert.equal(lib.clampInt("5000", 1, 1000), 1000);
  assert.equal(lib.clampInt("0", 1, 8), 1);
  assert.equal(lib.clampInt(" 12 ", 1, 100), 12);
  assert.equal(lib.clampInt("", 1, 100), null);
  assert.equal(lib.clampInt("abc", 1, 100), null);
  assert.equal(lib.clampInt(null, 1, 100), null);
});

test("reclaimPlan counts every passed group and never removes protected copies while hidden", () => {
  const groups = [
    { hash: "a", size: 10, files: ["/u/a1.jpg", "/u/a2.jpg", "/u/a3.jpg"] },
    { hash: "b", size: 100, files: ["/u/b.dll.bak", "/Program Files/x/b.jpg"] },
    { hash: "c", size: 7, files: ["/u/c1.txt", "/u/c2.txt"] },
  ];
  const keep = new Map([["c", "/u/c2.txt"]]);
  const p = lib.reclaimPlan(groups, keep, false);
  assert.equal(p.groups, 3);
  // a: keep first, remove 2; b: keep the program's copy, remove the user's; c: keep chosen.
  assert.deepEqual(p.jobs.map((j) => j.keep), ["/u/a1.jpg", "/Program Files/x/b.jpg", "/u/c2.txt"]);
  assert.equal(p.files, 4);
  assert.equal(p.bytes, 10 * 2 + 100 + 7);
  assert.equal(p.prot, false);
  // A chosen copy that is no longer in the group falls back to the default.
  const stale = lib.reclaimPlan([groups[2]], new Map([["c", "/gone"]]), false);
  assert.equal(stale.jobs[0].keep, "/u/c1.txt");
  // Only protected copies to remove and protection on: nothing to do.
  const prot = lib.reclaimPlan([{ hash: "p", size: 5, files: ["/steamapps/a.pak", "/steamapps/b.pak"] }], new Map(), false);
  assert.equal(prot.groups, 0);
  const protOn = lib.reclaimPlan([{ hash: "p", size: 5, files: ["/steamapps/a.pak", "/steamapps/b.pak"] }], new Map(), true);
  assert.equal(protOn.files, 1);
  assert.equal(protOn.prot, true);
});

test("theme setting survives a round trip and rejects junk", () => {
  assert.equal(lib.mergeSettings(null).theme, "auto");
  assert.equal(lib.mergeSettings({ theme: "light" }).theme, "light");
  assert.equal(lib.mergeSettings({ theme: "neon" }).theme, "auto");
});

test("English and German carry the same UI strings", () => {
  const I18N = require("./i18n.js");
  const en = Object.keys(I18N.en).sort();
  const de = Object.keys(I18N.de).sort();
  assert.deepEqual(de.filter((k) => !en.includes(k)), [], "keys only in de");
  assert.deepEqual(en.filter((k) => !de.includes(k)), [], "keys only in en");
});

test("every string key app.js asks for exists", () => {
  const I18N = require("./i18n.js");
  const src = require("node:fs").readFileSync(require("node:path").join(__dirname, "app.js"), "utf8");
  // t("key") / tf("key", …) and string keys picked by a ternary inside t(…)
  const keys = new Set([
    ...[...src.matchAll(/\btf?\(\s*"([A-Za-z_]+)"/g)].map((m) => m[1]),
    ...[...src.matchAll(/\bt\([^()]*\?\s*"([A-Za-z_]+)"\s*:\s*"([A-Za-z_]+)"\)/g)].flatMap((m) => [m[1], m[2]]),
  ]);
  const missing = [...keys].filter((k) => !(k in I18N.en));
  assert.deepEqual(missing, []);
  const html = require("node:fs").readFileSync(require("node:path").join(__dirname, "index.html"), "utf8");
  const htmlKeys = [...html.matchAll(/data-i18n(?:-ph|-title|-label)?="([A-Za-z_]+)"/g)].map((m) => m[1]);
  assert.deepEqual(htmlKeys.filter((k) => !(k in I18N.en)), []);
});

test("no emoji left in UI strings (icons come from icons.js)", () => {
  const I18N = require("./i18n.js");
  const emoji = /[\u{1F300}-\u{1FAFF}\u{2600}-\u{27BF}]/u;
  for (const lang of ["en", "de"]) {
    for (const [k, v] of Object.entries(I18N[lang])) {
      if (typeof v === "string") assert.ok(!emoji.test(v), `${lang}.${k} has an emoji: ${v}`);
    }
  }
});
