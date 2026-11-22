"use strict";

// Diskghost UI. Pure helpers come from lib.js, strings from i18n.js, the
// icon set from icons.js — all as globals (no bundler, CSP default-src 'self').
//
// Information architecture, mirroring what the engine can actually do:
//   Sidebar  · Drives (start a scan) · Scans · Duplicate searches (one entry per job)
//   Scan     · map + composition + tabs Folders / Files / Extensions / Size ranges / Changes
//              (Changes = the snapshot history of that scanned folder: compare, delete)
//   Search   · groups / folder pairs + the reclaim bar (what a reclaim would act on)
//   Settings · dialog

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

let settings = mergeSettings(null);
let elevated = false; // running as administrator (the MFT engine needs it)
let drives = [];
// The MFT engine exists on Windows only (helper.rs refuses elsewhere).
const IS_WINDOWS = /Windows/i.test(navigator.userAgent);

// Every scan / duplicate search the user started, oldest first. A job is
// queued until a slot is free (settings.parallel), then running, then done,
// cancelled or error. A finished scan keeps its current view, so the user can
// switch between several scans without losing their place.
const jobs = [];
let activeId = null;
let nextId = 1;
// The one removal (delete / trash of a path) in flight, shown in the status bar.
let op = null;

// ---------------------------------- i18n ----------------------------------
const lang = () => {
  if (settings.language !== "auto") return settings.language;
  return (navigator.language || "en").toLowerCase().startsWith("de") ? "de" : "en";
};
const t = (k) => I18N[lang()][k] ?? I18N.en[k] ?? k;
// A string with {name} placeholders filled in.
const tf = (k, vars) => String(t(k)).replace(/\{(\w+)\}/g, (_, n) => (vars[n] ?? ""));
const num = (n) => Number(n || 0).toLocaleString(lang() === "de" ? "de-DE" : "en-US");

// Why the numbers on screen are lower bounds: every cause that applies
// (cancel, folders on other filesystems, the depth limit the scan ran with),
// or an explicit "not reported" — never a guessed cause.
function partialNotice(v, j) {
  const pc = partialCauses(v, j?.opts?.max_depth ?? null);
  if (!pc.partial) return pc.cancelledWalk ? t("cancelledWalkNotice") : "";
  const why = pc.causes.map((c) =>
    c.kind === "cancelled" ? t("causeCancelled")
      : c.kind === "boundary" ? t("causeBoundary").replace("{n}", num(c.n))
      : c.kind === "depth" ? t("causeDepth").replace("{n}", num(c.n))
      : t("causeUnknown"));
  return `${why.join(" ")} ${t("lowerBoundNote")}`;
}

// Started as administrator: Explorer cannot drop into this window, so the
// UI stops inviting it and points at what works instead.
function adaptToElevation() {
  if (!elevated) return;
  $("drop-hint").textContent = t("dropBlocked");
  $("path").placeholder = t("placeholderAdmin");
}

function applyI18n() {
  document.documentElement.lang = lang();
  document.querySelectorAll("[data-i18n]").forEach((e) => {
    // Buttons with an icon keep it: icon + label span.
    if (e.dataset.icon) e.replaceChildren(icon(e.dataset.icon), el("span", { text: t(e.dataset.i18n) }));
    else e.textContent = t(e.dataset.i18n);
  });
  document.querySelectorAll("[data-icon]:not([data-i18n])").forEach((e) => e.replaceChildren(icon(e.dataset.icon)));
  document.querySelectorAll("[data-i18n-ph]").forEach((e) => (e.placeholder = t(e.dataset.i18nPh)));
  document.querySelectorAll("[data-i18n-title]").forEach((e) => (e.title = t(e.dataset.i18nTitle)));
  document.querySelectorAll("[data-i18n-label]").forEach((e) => e.setAttribute("aria-label", t(e.dataset.i18nLabel)));
  adaptToElevation();
}

// Settings → Theme: "auto" leaves it to prefers-color-scheme.
function applyTheme() {
  const root = document.documentElement;
  if (settings.theme === "auto") delete root.dataset.theme;
  else root.dataset.theme = settings.theme;
}

// ---------------------------- safe DOM builder ----------------------------
function el(tag, attrs, ...kids) {
  const n = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v == null || v === false) continue;
      if (k === "class") n.className = v;
      else if (k === "text") n.textContent = v;
      else if (k === "vars") for (const [name, val] of Object.entries(v)) n.style.setProperty(`--${name}`, String(val));
      else if (k.startsWith("on") && typeof v === "function") n.addEventListener(k.slice(2), v);
      else n.setAttribute(k, v === true ? "" : v);
    }
    // A non-button element acting as a button answers Enter and Space too.
    if (attrs.role === "button" && tag !== "button" && typeof attrs.onclick === "function") {
      n.addEventListener("keydown", (e) => {
        if (e.target !== n || (e.key !== "Enter" && e.key !== " ")) return;
        e.preventDefault();
        attrs.onclick(e);
      });
    }
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    n.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return n;
}

// A button with an icon and a label (or only an icon with an accessible name).
function btn(iconName, label, attrs = {}) {
  const onlyIcon = attrs.iconOnly;
  delete attrs.iconOnly;
  return el("button", { type: "button", ...attrs, ...(onlyIcon ? { "aria-label": label, title: attrs.title || label } : {}) },
    iconName ? icon(iconName) : null, onlyIcon ? null : el("span", { text: label }));
}

// A share bar: `frac` 0..1 of the track.
const bar = (frac, cls = "") => el("span", { class: `bar ${cls}` }, el("span", { class: "fill", vars: { w: `${Math.max(0, Math.min(1, frac || 0)) * 100}%` } }));

function status(msg, isError = false) {
  const s = $("status");
  s.textContent = msg || "";
  s.classList.toggle("is-error", isError);
}

const errText = (e) => (typeof e === "string" ? e : e?.message ?? JSON.stringify(e));

// ------------------------------- tooltip -------------------------------
// One shared hover card, built lazily. Hover follows the pointer after
// `delay` ms (800 for controls; 0 for map tiles and chart columns, whose
// card IS the data being explored); keyboard focus shows it at once.
let tipTimer = 0;
function tip(node, content, delay = 800) {
  const box = $("tooltip");
  let w = 0, h = 0, at = [0, 0];
  const place = (x, y) => {
    const pad = 14;
    let px = x + pad, py = y + pad;
    if (px + w > innerWidth - 8) px = x - w - pad;
    if (py + h > innerHeight - 8) py = y - h - pad;
    box.style.transform = `translate(${Math.max(8, px)}px, ${Math.max(8, py)}px)`;
  };
  const show = () => {
    box.replaceChildren(...[content()].flat().filter(Boolean));
    box.classList.add("show");
    ({ width: w, height: h } = box.getBoundingClientRect());
  };
  node.addEventListener("mouseenter", (ev) => {
    at = [ev.clientX, ev.clientY];
    clearTimeout(tipTimer);
    if (!delay) { show(); place(...at); return; }
    tipTimer = setTimeout(() => { show(); place(...at); }, delay);
  });
  node.addEventListener("mousemove", (ev) => { at = [ev.clientX, ev.clientY]; if (box.classList.contains("show")) place(...at); });
  node.addEventListener("mouseleave", hideTip);
  node.addEventListener("focus", () => {
    if (!node.matches(":focus-visible")) return;
    show();
    const r = node.getBoundingClientRect();
    place(r.left, r.bottom);
  });
  node.addEventListener("blur", hideTip);
}
function hideTip() { clearTimeout(tipTimer); $("tooltip").classList.remove("show"); }
const tipRow = (label, value) => el("div", { class: "tip-row" }, el("span", { text: label }), el("span", { text: value }));

// ------------------------------- dialogs -------------------------------
// A modal question. Resolves exactly once: true for the confirm button,
// false for Cancel, Escape or any other close.
function ask(question, detail, okLabel, danger) {
  const dlg = $("confirm-dlg");
  return new Promise((resolve) => {
    let answered = false;
    const finish = (v) => {
      if (answered) return;
      answered = true;
      dlg.onclose = null;
      if (dlg.open) dlg.close();
      resolve(v);
    };
    const okBtn = el("button", { type: "button", class: danger ? "danger" : "primary", onclick: () => finish(true) }, okLabel);
    dlg.replaceChildren(el("div", { class: "dlg-in" },
      el("header", { class: "dlg-head" }, el("h2", { text: question })),
      el("div", { class: "dlg-body" }, detail.map((d) => el("p", { class: d.cls || "", text: d.text }))),
      el("footer", { class: "dlg-foot" },
        el("button", { type: "button", onclick: () => finish(false) }, t("cancel")),
        okBtn)));
    dlg.onclose = () => finish(false);
    dlg.showModal();
    // Destructive: the safe answer has the focus; otherwise the action.
    (danger ? dlg.querySelector(".dlg-foot button") : okBtn).focus();
  });
}

// ------------------------------- settings -------------------------------
let saveTimer = null;
function saveSettings() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => invoke("save_settings", { settings }).catch((e) => status(errText(e), true)), 250);
  updateExcludeChip();
}

function walkOpts() {
  return {
    exclude: activeExcludes(settings),
    max_depth: settings.max_depth,
    follow_symlinks: settings.follow_symlinks,
    // "gentle" keeps a spinning disk from seeking itself to death and leaves
    // the machine responsive; "fast" lets the walker pick (2 x CPU threads).
    threads: settings.engine === "gentle" ? 3 : null,
    mft: IS_WINDOWS && settings.engine === "mft",
  };
}

function updateExcludeChip() {
  const list = activeExcludes(settings);
  const b = $("btn-excludes");
  b.replaceChildren(icon("filter"), el("span", { text: list.length ? `${num(list.length)} ${t("excludesOn")}` : t("excludesNone") }));
  b.classList.toggle("is-on", list.length > 0);
  b.title = list.length ? `${t("excludesFuture")}\n${list.join(", ")}` : t("excludesFuture");
}

function field(label, desc, input) {
  return el("label", { class: "field" }, el("span", { class: "field-label", text: label }), input, desc ? el("span", { class: "field-help", text: desc }) : null);
}
function checkField(label, desc, checked, onchange) {
  const box = el("input", { type: "checkbox", onchange: (e) => onchange(e.target.checked) });
  box.checked = checked;
  return el("label", { class: "field check" }, box,
    el("span", {}, el("span", { class: "field-label", text: label }), desc ? el("span", { class: "field-help", text: desc }) : null));
}
function numberInput(value, min, max, onchange, placeholder) {
  return el("input", {
    type: "number", min, max, value: value ?? "", placeholder,
    onchange: (e) => {
      const v = clampInt(e.target.value, min, max);
      // Show what is actually saved, not what was typed.
      e.target.value = v ?? "";
      onchange(v);
    },
  });
}
function selectInput(value, options, onchange, attrs = {}) {
  const s = el("select", { ...attrs, onchange: (e) => onchange(e.target.value) });
  for (const [v, label] of options) {
    const o = el("option", { value: v, text: label });
    if (v === value) o.selected = true;
    s.append(o);
  }
  return s;
}

function openSettings(focusExcludes = false) {
  const dlg = $("settings-dlg");
  renderSettings();
  dlg.showModal();
  if (focusExcludes) dlg.querySelector("#set-excl input")?.focus();
}

function renderSettings() {
  const dlg = $("settings-dlg");
  const scrollTop = dlg.querySelector(".dlg-body")?.scrollTop ?? 0;
  const focusKey = dlg.contains(document.activeElement) ? document.activeElement.dataset.fk : null;
  const set = (patch, rerender = false) => {
    Object.assign(settings, patch);
    saveSettings();
    if (rerender) renderSettings();
  };

  // Exclusion templates: the whole template, or single patterns inside it.
  const presets = el("div", { class: "presets" });
  for (const p of EXCLUDE_PRESETS) {
    const on = settings.presets.includes(p.id);
    const box = el("input", {
      type: "checkbox", "data-fk": `pre-${p.id}`, autofocus: p === EXCLUDE_PRESETS[0],
      onchange: (e) => set({ presets: e.target.checked ? [...settings.presets, p.id] : settings.presets.filter((x) => x !== p.id) }, true),
    });
    box.checked = on;
    const chips = el("div", { class: "chips" }, p.patterns.map((pat) => {
      const key = `${p.id}:${pat}`;
      const off = settings.preset_off.includes(key);
      return el("button", {
        type: "button", class: `chip${!on || off ? " is-off" : ""}`, disabled: !on, "aria-pressed": String(on && !off), "data-fk": `chip-${key}`,
        onclick: () => set({ preset_off: off ? settings.preset_off.filter((k) => k !== key) : [...settings.preset_off, key] }, true),
      }, pat);
    }));
    presets.append(el("div", { class: `preset${on ? " is-on" : ""}` }, el("label", { class: "preset-head" }, box, t(`pre_${p.id}`)), chips));
  }

  // Custom patterns: toggle, remove, add (validated by the core's glob parser).
  const customChips = el("div", { class: "chips" }, settings.custom.map((c, i) =>
    el("span", { class: `chip${c.on ? "" : " is-off"}` },
      el("button", {
        type: "button", class: "chip-label", "aria-pressed": String(c.on), "data-fk": `cus-${c.pattern}`,
        onclick: () => { settings.custom[i].on = !c.on; set({}, true); },
      }, c.pattern),
      btn("close", t("del"), { class: "chip-x", iconOnly: true, onclick: () => { settings.custom.splice(i, 1); set({}, true); } }))));
  const addInput = el("input", { type: "text", placeholder: "*.iso, Downloads, node_modules", spellcheck: "false", "data-fk": "add-pattern", "aria-label": t("setCustom") });
  const addMsg = el("span", { class: "field-help", "aria-live": "polite" });
  const add = async () => {
    const pats = addInput.value.split(",").map((s) => s.trim()).filter(Boolean);
    if (!pats.length) return;
    let bad;
    try { bad = await invoke("check_globs", { patterns: pats }); } catch (e) {
      addMsg.textContent = `${t("globCheckFailed")} ${errText(e)}`; addMsg.classList.add("is-error"); return;
    }
    if (bad.length) { addMsg.textContent = `${t("setBadGlob")} ${bad.join(", ")}`; addMsg.classList.add("is-error"); addInput.setAttribute("aria-invalid", "true"); return; }
    const fresh = pats.filter((p) => !settings.custom.some((c) => c.pattern === p));
    if (!fresh.length) { addMsg.textContent = t("setDup"); addMsg.classList.remove("is-error"); return; }
    settings.custom.push(...fresh.map((pattern) => ({ pattern, on: true })));
    set({}, true);
    $("settings-dlg").querySelector("[data-fk=add-pattern]")?.focus();
  };
  addInput.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); add(); } });

  const engines = [["fast", t("engineFast")], ["gentle", t("engineGentle")]];
  if (IS_WINDOWS) engines.push(["mft", t("engineMft")]);
  const engine = IS_WINDOWS || settings.engine !== "mft" ? settings.engine : "fast";

  const body = el("div", { class: "dlg-body" },
    el("section", { class: "set-sec", id: "set-excl" },
      el("h3", { text: `${t("setExcl")} — ${num(activeExcludes(settings).length)} ${t("excludesOn")}` }),
      el("p", { class: "set-desc", text: t("setExclDesc") }),
      presets,
      el("h4", { text: t("setCustom") }),
      el("p", { class: "set-desc", text: t("setCustomDesc") }),
      customChips,
      el("div", { class: "addrow" }, addInput, el("button", { type: "button", onclick: add }, t("setAdd"))),
      addMsg),
    el("section", { class: "set-sec" },
      el("h3", { text: t("setScan") }),
      el("div", { class: "set-grid" },
        field(t("setTop"), t("setTopD"), numberInput(settings.top, 1, 1000, (v) => set({ top: v ?? 50 }))),
        field(t("setDepth"), t("setDepthD"), numberInput(settings.max_depth, 1, 100, (v) => set({ max_depth: v }), "∞")),
        field(t("setParallel"), t("setParallelD"), numberInput(settings.parallel, 1, 8, (v) => { set({ parallel: v ?? 4 }); pump(); })),
        field(t("setKeep"), t("setKeepD"), numberInput(settings.keep_snapshots, 1, 100, (v) => set({ keep_snapshots: v ?? 10 }))),
        field(t("setEngine"), t({ fast: "engineFastD", gentle: "engineGentleD", mft: "engineMftD" }[engine]),
          selectInput(engine, engines, (v) => set({ engine: v }, true), { "data-fk": "engine" })),
        settings.engine === "mft" && IS_WINDOWS && !elevated
          ? el("div", { class: "field" }, el("span", { class: "field-label", text: t("mftHelperTitle") }), el("span", { class: "field-help", text: t("mftHelperText") }))
          : null,
        checkField(t("setFollow"), t("setFollowD"), settings.follow_symlinks, (v) => set({ follow_symlinks: v })))),
    el("section", { class: "set-sec" },
      el("h3", { text: `${t("setDupes")} & ${t("setDelete")}` }),
      el("div", { class: "set-grid" },
        field(t("setMinMb"), t("setMinMbD"), numberInput(settings.min_mb, 0, 1_000_000, (v) => set({ min_mb: v ?? 0 }))),
        checkField(t("setTrash"), t("setTrashD"), settings.trash, (v) => set({ trash: v })))),
    el("section", { class: "set-sec" },
      el("h3", { text: t("setLook") }),
      el("div", { class: "set-grid" },
        field(t("setTheme"), null, selectInput(settings.theme,
          [["auto", t("themeAuto")], ["dark", t("themeDark")], ["light", t("themeLight")]],
          (v) => { set({ theme: v }); applyTheme(); }, { "data-fk": "theme" })),
        field(t("setWin"), t("setWinD"), selectInput(settings.window_size,
          [["auto", t("winAuto")], ["maximized", t("winMax")], ["1080p", t("win1080")], ["1440p", t("win1440")], ["4k", t("win4k")]],
          (v) => set({ window_size: v }), { "data-fk": "win" })),
        field(t("setLang"), null, selectInput(settings.language,
          [["auto", t("langAuto")], ["en", t("langEn")], ["de", t("langDe")]],
          (v) => { set({ language: v }, true); applyI18n(); renderAll(); }, { "data-fk": "lang" })))));

  dlg.replaceChildren(el("div", { class: "dlg-in" },
    el("header", { class: "dlg-head" }, el("h2", { text: t("setTitle") }),
      btn("close", t("close"), { class: "icon-btn quiet", iconOnly: true, onclick: () => dlg.close() })),
    body,
    el("footer", { class: "dlg-foot" },
      el("button", {
        type: "button", class: "quiet push-left",
        onclick: async () => {
          if (!(await ask(t("setResetQ"), [{ text: t("setResetD") }], t("setReset"), true))) return;
          settings = mergeSettings(null); saveSettings(); applyTheme(); renderSettings(); applyI18n(); renderAll();
        },
      }, t("setReset")),
      el("button", { type: "button", class: "primary", onclick: () => dlg.close() }, t("setDone")))));
  body.scrollTop = scrollTop;
  if (focusKey) dlg.querySelector(`[data-fk="${CSS.escape(focusKey)}"]`)?.focus();
}

// --------------------------------- drives ---------------------------------
async function loadDrives() {
  try {
    drives = await invoke("list_drives");
  } catch (e) {
    drives = [];
    status(`${t("drivesFailed")} ${errText(e)}`, true);
  }
  renderDrives();
  if (!active()) renderMain();
}

const kindLabel = (k) => t({ fixed: "kindFixed", removable: "kindRemovable", network: "kindNetwork" }[k] || "kindOther");
const driveIcon = (k) => ({ removable: "usb", network: "network" }[k] || "drive");
const driveName = (d) => (d.label && d.label !== baseName(d.path) ? `${d.path.replace(/\\$/, "")} ${d.label}` : d.path);

function driveTip(d) {
  const used = d.total - d.free;
  return () => [el("b", { text: driveName(d) }),
    tipRow(kindLabel(d.kind), d.fs || ""),
    tipRow(t("used"), `${human(used)} (${pct(used, d.total)})`),
    tipRow(t("free"), human(d.free)),
    tipRow(t("colSize"), human(d.total))];
}

function driveRow(d) {
  const used = d.total - d.free;
  const b = el("button", { type: "button", class: "drive", onclick: () => enqueue("scan", d.path), "aria-label": `${t("scan")} ${driveName(d)}` },
    icon(driveIcon(d.kind)),
    el("span", { class: "drive-name", text: driveName(d) }),
    el("span", { class: "drive-free num", text: `${human(d.free)} ${t("free")}` }),
    bar(used / d.total, used / d.total > 0.9 ? "is-hot" : ""));
  tip(b, driveTip(d));
  return b;
}

function renderDrives() {
  $("drives").replaceChildren(...drives.map(driveRow));
  $("btn-scan-all").disabled = drives.length === 0;
}

// ---------------------------------- jobs ----------------------------------
const active = () => jobs.find((j) => j.id === activeId) || null;
const byId = (id) => jobs.find((j) => j.id === id);

// Walk options of a duplicate search: unless program and game files are
// included on purpose, they are not even read.
function dupeOpts(includeProt) {
  const o = walkOpts();
  if (!includeProt) o.exclude = [...new Set([...o.exclude, ...protectedExcludes()])];
  return o;
}

function enqueue(kind, path, { select = true, includeProt = false } = {}) {
  path = String(path || "").trim();
  if (!path) {
    status(t("enterPath"), true);
    $("path").focus();
    return null;
  }
  const opts = kind === "dupes" ? dupeOpts(includeProt) : walkOpts();
  // The same scan again: just show it (Rescan walks it afresh).
  const same = jobs.find((j) => j.kind === kind && j.root === path && j.state !== "error" &&
    JSON.stringify(j.opts) === JSON.stringify(opts));
  if (same) {
    if (select) selectJob(same.id);
    // Asking again for a scan that was cancelled means: scan it for real.
    if (same.state === "cancelled") rescan(same);
    return same;
  }
  const j = {
    id: nextId++, kind, root: path, path, opts, top: settings.top, mb: settings.min_mb,
    state: "queued", files: 0, bytes: 0, startedAt: 0, elapsedMs: null,
    view: null, groups: null, error: null, tab: "folders",
    includeProt,
    cache: new Map(), // folder -> view, so going back is instant
    back: [], // folders visited before, for Back
  };
  jobs.push(j);
  if (select) activeId = j.id;
  pump();
  renderAll();
  return j;
}

function pump() {
  for (const j of jobsToStart(jobs, settings.parallel)) runJob(j);
  renderQueueInfo();
}

// The sidebar always; the main area only if `j` is the job on screen — a scan
// finishing in the background never rebuilds the view you are looking at.
function renderFor(j) {
  renderJobs();
  renderQueueInfo();
  if (!j || j.id === activeId) renderMain();
}

async function runJob(j) {
  j.state = "running";
  j.files = 0;
  j.bytes = 0;
  j.phase = 0;
  j.lastPhase = 0;
  j.totalFiles = 0;
  j.totalBytes = 0;
  j.cancelRequested = false;
  j.cancelAt = 0;
  j.run = (j.run || 0) + 1; // results of an earlier run of this job are stale
  j.startedAt = performance.now();
  j.scanUnix = Math.floor(Date.now() / 1000);
  j.error = null;
  renderFor(j);
  try {
    if (j.kind === "scan") {
      const view = await invoke("scan_dir", {
        job: j.id, path: j.root, top: j.top, opts: j.opts, keepSnapshots: settings.keep_snapshots,
      });
      j.elapsedMs = performance.now() - j.startedAt;
      j.root = String(view.root);
      j.cache = new Map([[j.root, view]]);
      j.back = [];
      j.view = view;
      j.path = j.root;
      j.diff = null;
      j.diffError = null;
      j.history = null;
      j.historyError = null;
      j.state = view.cancelled ? "cancelled" : "done";
      // Cancelled, cut at a filesystem boundary or by the depth limit:
      // browsable, not saved as a snapshot, so not comparable.
      j.partial = view.complete === false;
      const back = j.returnTo;
      j.returnTo = null;
      if (back && back !== j.root && !j.closed) navigate(j, back, { keepTab: true, record: false });
      if (!j.partial && !j.closed) autoCompare(j);
      if (view.mft_fallback) status(`${t("mftFallback")}: ${view.mft_fallback}`, true);
    } else {
      j.groups = await invoke("find_dupes", { job: j.id, path: j.root, mb: j.mb, opts: j.opts });
      j.elapsedMs = performance.now() - j.startedAt;
      j.keep = new Map();
      j.tagged = false;
      j.dupSet = null;
      const out = dupesOutcome(j.cancelRequested, j.groups, j.lastPhase);
      j.state = out.state;
      j.hashesKept = out.hashesKept;
    }
  } catch (e) {
    j.state = "error";
    j.error = errText(e);
  }
  // Closed while it ran: the backend stored the tree only now — free it.
  if (j.closed) {
    if (j.kind === "scan") invoke("forget_tree", { job: j.id }).catch(() => {});
    pump();
    return;
  }
  renderFor(j);
  pump();
  if (j.kind === "scan" && j.state === "done") loadDrives();
}

async function cancelJob(j) {
  if (j.state === "queued") return closeJob(j);
  if (j.state === "running" && !j.cancelRequested) {
    j.cancelRequested = true;
    j.cancelAt = performance.now();
    renderFor(j);
    try { await invoke("cancel", { job: j.id }); } catch (e) { status(errText(e), true); }
  }
}

function closeJob(j) {
  const i = jobs.indexOf(j);
  if (i < 0) return;
  jobs.splice(i, 1);
  j.closed = true;
  if (j.state === "running") {
    // The tree is stored when scan_dir returns; runJob frees it then.
    invoke("cancel", { job: j.id }).catch(() => {});
  } else if (j.kind === "scan") {
    invoke("forget_tree", { job: j.id }).catch(() => {}); // free the tree's memory
  }
  if (activeId === j.id) activeId = (jobs[i] || jobs[i - 1])?.id ?? null;
  pump();
  renderAll();
}

function selectJob(id) {
  if (activeId === id) return;
  activeId = id;
  const j = active();
  if (j && j.stale && j.kind === "scan" && j.view) {
    j.stale = false;
    navigate(j, j.path, { keepTab: true, record: false, force: true });
  }
  renderAll();
}

function rescan(j) {
  if (j.state === "running" || j.state === "queued") return;
  j.opts = j.kind === "dupes" ? dupeOpts(j.includeProt) : walkOpts();
  j.top = settings.top;
  j.mb = settings.min_mb;
  j.returnTo = j.kind === "scan" ? j.path : null;
  j.auto = null;
  j.state = "queued";
  pump();
  renderAll();
}

// Open `path` inside job `j` — from the job's view cache (instant) or from
// its tree in memory (a fraction of a second). Never walks the disk.
async function navigate(j, path, { keepTab = false, record = true, force = false } = {}) {
  if (!force && j.view && path === j.path) return;
  const pushed = record && j.path !== path;
  if (pushed) j.back.push(j.path);
  j.navGuard = j.navGuard || latestOnly();
  const ticket = j.navGuard.next();
  if (!keepTab && j.tab === "changes") j.tab = "folders";
  const show = (v) => {
    if (!j.navGuard.current(ticket)) return; // a newer navigation won
    j.view = v;
    j.path = String(v.root);
    j.diff = null;
    j.diffError = null;
    j.fileFilter = "";
    j.catFilter = "";
    j.loading = false;
    if (byId(j.id)) renderFor(j);
  };
  const cached = !force && j.cache.get(path);
  if (cached) return show(cached);
  j.loading = true;
  if (j.id === activeId) $("main").classList.add("is-busy");
  try {
    const v = await invoke("view", { job: j.id, path, top: j.top });
    j.cache.set(String(v.root), v);
    show(v);
  } catch (e) {
    if (!j.navGuard.current(ticket)) return;
    j.loading = false;
    if (pushed) j.back.pop();
    status(errText(e), true);
    if (byId(j.id)) renderFor(j);
  }
}

function goBack(j) {
  if (j?.view && j.back.length) navigate(j, j.back.pop(), { record: false });
}

function goUp(j) {
  const up = j?.view && parentPath(j.path);
  if (up && isUnder(up, j.root)) navigate(j, up);
}

// ------------------------------ rendering ------------------------------
function renderAll() {
  hideTip();
  renderJobs();
  renderMain();
  renderQueueInfo();
}

function renderQueueInfo() {
  const r = jobs.filter((j) => j.state === "running").length;
  const q = jobs.filter((j) => j.state === "queued").length;
  $("queue-info").textContent = r || q ? t("queueInfo")(r, q, settings.parallel) : "";
}

// State of a job as an icon name + short label (always words, never colour alone).
function jobState(j) {
  if (j.state === "error") return ["alert", t("stError")];
  if (j.state === "queued") return ["queue", t("stQueued")];
  if (j.state === "running") return ["clock", j.cancelRequested ? t("cancelling") : j.kind === "dupes" ? t("stSearching") : t("stRunning")];
  if (j.state === "cancelled") return ["stop", t("stCancelled")];
  if (j.kind === "scan" && j.partial) return ["alert", t("stPartial")];
  return ["check", t("stDone")];
}

function jobName(j) {
  return /^[A-Za-z]:\\?$/.test(j.root) || j.root === "/" ? j.root : baseName(j.root);
}

function jobSub(j) {
  if (j.state === "queued") return j.root;
  if (j.state === "running") {
    if (j.kind === "dupes") return `${j.phase + 1}/3 · ${num(j.files)} ${t("files")}`;
    return `${num(j.files)} ${t("files")} · ${human(j.bytes)}`;
  }
  if (j.state === "error") return j.error;
  if (j.kind === "dupes") {
    if (j.state === "cancelled") return t("noResultsCancelled");
    tagGroups(j);
    const own = j.groups.filter((g) => !g.allProt);
    return `${num(own.length)} ${t("groups")} · ${human(own.reduce((a, g) => a + g.wasted, 0))}`;
  }
  if (j.view) {
    const root = j.cache.get(j.root) || j.view;
    return `${j.partial ? "≥ " : ""}${human(root.total_size)} · ${num(root.total_files)} ${t("files")}`;
  }
  return "";
}

// Estimated progress of a drive-root scan: bytes found vs the drive's used space.
function estimate(j) {
  const d = drives.find((x) => x.path.toLowerCase() === j.root.toLowerCase() || x.path === `${j.root}\\`);
  if (!d || j.kind !== "scan") return null;
  const used = d.total - d.free;
  return used > 0 ? Math.min(0.99, j.bytes / used) : null;
}

// Sidebar rows are created once per job and then only updated in place:
// progress repaints them several times a second, and replacing the element
// under the pointer between mouse-down and mouse-up swallows the click.
const jobRows = new Map(); // job id -> { row, open, ic, name, sub, close, meter }

function jobRow(j) {
  let r = jobRows.get(j.id);
  if (r) return r;
  const id = j.id;
  r = {
    ic: el("span", { class: "job-ic" }),
    name: el("span", { class: "job-name" }),
    sub: el("span", { class: "job-sub" }),
    meter: bar(0, "job-meter"),
  };
  r.open = el("button", { type: "button", class: "job-open", "data-job": String(id), onclick: () => selectJob(id) },
    r.ic, r.name, r.sub);
  r.close = el("button", {
    type: "button", class: "job-x icon-btn quiet",
    onclick: () => {
      const job = byId(id);
      if (job) job.state === "running" ? cancelJob(job) : closeJob(job);
    },
  });
  r.row = el("div", { class: "job" }, r.open, r.close, r.meter);
  jobRows.set(id, r);
  return r;
}

// Set a property only when it changes, so unchanged rows cost nothing.
function put(node, key, value) {
  if (node[key] !== value) node[key] = value;
}

function renderJobs() {
  for (const id of jobRows.keys()) if (!byId(id)) jobRows.delete(id);
  for (const [kind, boxId, emptyKey] of [["scan", "jobs-scan", "noScans"], ["dupes", "jobs-dupes", "noSearches"]]) {
    const box = $(boxId);
    const list = jobs.filter((j) => j.kind === kind);
    list.forEach((j, i) => {
      const r = jobRow(j);
      const busy = j.state === "running";
      const [ic, label] = jobState(j);
      put(r.row, "className", `job is-${j.state}${j.partial ? " is-partial" : ""}${j.id === activeId ? " is-active" : ""}`);
      if (r.open.getAttribute("aria-current") !== (j.id === activeId ? "page" : "false")) r.open.setAttribute("aria-current", j.id === activeId ? "page" : "false");
      put(r.open, "title", `${label} — ${j.root}`);
      if (r.ic.dataset.ic !== ic) { r.ic.dataset.ic = ic; r.ic.replaceChildren(icon(ic, label)); }
      put(r.name, "textContent", jobName(j));
      put(r.sub, "textContent", jobSub(j));
      const closeIc = busy ? "stop" : "close";
      if (r.close.dataset.ic !== closeIc) { r.close.dataset.ic = closeIc; r.close.replaceChildren(icon(closeIc)); }
      const closeLabel = busy ? t("cancel") : j.state === "queued" ? t("removeFromQueue") : t("close");
      put(r.close, "title", closeLabel);
      if (r.close.getAttribute("aria-label") !== closeLabel) r.close.setAttribute("aria-label", closeLabel);
      put(r.close, "disabled", busy && j.cancelRequested);
      const est = busy ? estimate(j) : null;
      const frac = j.kind === "dupes" && busy && j.totalFiles ? j.files / j.totalFiles : est;
      put(r.meter.style, "display", busy ? "" : "none");
      put(r.meter, "className", frac == null ? "bar job-meter is-indeterminate" : "bar job-meter");
      r.meter.firstChild.style.setProperty("--w", frac == null ? "30%" : `${Math.min(1, frac) * 100}%`);
      if (box.children[i] !== r.row) box.insertBefore(r.row, box.children[i] || null);
    });
    while (box.children.length > list.length) box.lastChild.remove();
    if (!list.length) box.replaceChildren(el("p", { class: "side-empty", text: t(emptyKey) }));
  }
  $("btn-clear-done").disabled = !jobs.some((j) => ["done", "cancelled", "error"].includes(j.state));
}

// Re-render the main area, keeping keyboard focus (and a text field's caret)
// on the same logical control — background renders (a comparison landing, a
// snapshot saved) must not throw the user out of a filter box.
function renderMain() {
  const main = $("main");
  hideTip();
  if (mapObserver) { mapObserver.disconnect(); mapObserver = null; }
  const j = active();
  main.classList.toggle("is-busy", Boolean(j?.loading));
  if (j?.loading && j.view) return; // keep the current view until the next one is ready
  const a = document.activeElement;
  const hadFocus = a && main.contains(a);
  const fk = hadFocus ? a.dataset.fk : null;
  const sel = fk && "selectionStart" in a ? [a.selectionStart, a.selectionEnd] : null;
  let next;
  if (!j) next = welcomeView();
  else if (j.state === "queued") next = queuedView(j);
  else if (j.state === "running") next = progressView(j);
  else if (j.state === "error") next = errorView(j);
  else if (j.kind === "dupes") next = dupesView(j);
  else next = scanView(j);
  main.replaceChildren(next);
  if (hadFocus) {
    const target = fk ? main.querySelector(`[data-fk="${CSS.escape(fk)}"]`) : null;
    if (target) {
      target.focus();
      if (sel && "setSelectionRange" in target) try { target.setSelectionRange(...sel); } catch (_) { /* not a text field */ }
    } else main.focus();
  }
  // The map needs its laid-out size, so it is drawn once in the DOM.
  const map = main.querySelector(".treemap");
  if (map && j) {
    drawTreemap(map, j);
    let last = `${map.clientWidth}x${map.clientHeight}`;
    mapObserver = new ResizeObserver(() => {
      const now = `${map.clientWidth}x${map.clientHeight}`;
      if (now !== last) { last = now; drawTreemap(map, j); }
    });
    mapObserver.observe(map);
  }
}
let mapObserver = null;

// A banner: warning/info/danger with an icon and text (never colour alone).
function banner(kind, text, ...actions) {
  return el("div", { class: `banner is-${kind}`, role: kind === "danger" ? "alert" : "status" },
    icon(kind === "info" ? "clock" : "alert"), el("p", { text }), actions.length ? el("div", { class: "banner-acts" }, actions) : null);
}

function welcomeView() {
  const rows = drives.map((d) => {
    const used = d.total - d.free;
    return el("tr", {},
      el("td", { class: "cell-name" }, el("span", { class: "with-ic" }, icon(driveIcon(d.kind)), el("span", { text: driveName(d) }))),
      el("td", { class: "muted", text: kindLabel(d.kind) }),
      el("td", { class: "cell-bar" }, bar(used / d.total, used / d.total > 0.9 ? "is-hot" : "")),
      el("td", { class: "num", text: `${human(d.free)} ${t("free")}` }),
      el("td", { class: "num muted", text: human(d.total) }),
      el("td", { class: "cell-acts" },
        btn("scan", t("scan"), { class: "small", onclick: () => enqueue("scan", d.path) }),
        btn("copies", t("dupes"), { class: "small quiet", onclick: () => enqueue("dupes", d.path) })));
  });
  return el("div", { class: "welcome" },
    el("header", { class: "welcome-head" },
      el("h1", { text: t("welcomeTitle") }),
      el("p", { text: t(elevated ? "welcomeTextAdmin" : "welcomeText") }),
      el("ul", { class: "keys" },
        el("li", {}, el("kbd", { text: "Ctrl+L" }), ` ${t("keyPath")}`),
        el("li", {}, el("kbd", { text: "Ctrl+O" }), ` ${t("keyBrowse")}`),
        el("li", {}, el("kbd", { text: "Alt+←" }), " / ", el("kbd", { text: "Alt+↑" }), ` ${t("keyNav")}`),
        el("li", {}, el("kbd", { text: "Ctrl+," }), ` ${t("settings")}`))),
    drives.length
      ? el("section", { class: "panel" },
        el("header", { class: "panel-head" }, el("h2", { text: t("drives") })),
        el("table", { class: "table" }, el("tbody", {}, rows)))
      : el("p", { class: "muted", text: t("noDrives") }));
}

function queuedView(j) {
  const pos = jobs.filter((x) => x.state === "queued").indexOf(j) + 1;
  return el("div", { class: "stage" }, el("section", { class: "panel stage-panel" },
    el("header", { class: "panel-head" }, el("h2", { text: t("stQueued") })),
    el("p", { class: "path", text: j.root }),
    el("p", { class: "figure", text: `${t("queuedPos")} ${num(pos)}` }),
    el("p", { class: "muted", text: `${t("queuedWhy")} ${num(settings.parallel)}` }),
    el("div", { class: "row-acts" }, btn("close", t("removeFromQueue"), { onclick: () => closeJob(j) }))));
}

// Title, 0..1 fraction (or null) and detail line of a running job: a scan
// estimates against the drive's used space, a duplicate search reports its
// three phases against their known totals.
function progressInfo(j) {
  if (j.kind !== "dupes") {
    const est = estimate(j);
    return {
      // Only "requested": the helper may still fall back to the walk.
      title: `${t("stRunning")}${j.opts?.mft ? ` · ${t("mftRequested")}` : ""}`,
      frac: est,
      detail: est == null ? t("noEstimate") : `≈ ${pct(est, 1)} ${t("estimated")}`,
    };
  }
  const ph = j.phase || 0;
  const title = t(["phaseWalk", "phaseSample", "phaseHash"][ph]);
  if (ph === 0 || !j.totalFiles) return { title, frac: null, detail: t("phaseWalkD") };
  if (ph === 1) {
    return { title, frac: j.files / j.totalFiles, detail: `${num(j.files)} / ${num(j.totalFiles)} ${t("files")} · ${t("phaseSampleD")}` };
  }
  return {
    title,
    frac: j.totalBytes ? j.bytes / j.totalBytes : j.files / j.totalFiles,
    detail: `${human(j.bytes)} / ${human(j.totalBytes)} · ${num(j.files)} / ${num(j.totalFiles)} ${t("files")}`,
  };
}

function stat(label, value, sub, attrs = {}, cls = "") {
  return el("div", { ...attrs, class: ["stat", attrs.class, cls].filter(Boolean).join(" ") },
    el("dt", { text: label }),
    el("dd", { class: "stat-v", text: value, title: value }),
    sub ? el("dd", { class: "stat-s", text: sub, title: sub }) : null);
}

function progressView(j) {
  const secs = (performance.now() - j.startedAt) / 1000;
  const p = progressInfo(j);
  const steps = j.kind === "dupes"
    ? el("ol", { class: "steps", id: "pv-steps" }, ["phaseWalk", "phaseSample", "phaseHash"].map((k, i) =>
      el("li", { class: i < (j.phase || 0) ? "is-done" : i === (j.phase || 0) ? "is-now" : "", "aria-current": i === (j.phase || 0) ? "step" : null, text: t(k) })))
    : null;
  return el("div", { class: "stage" }, el("section", { class: "panel stage-panel", "aria-busy": "true" },
    el("header", { class: "panel-head" }, el("h2", { id: "pv-title", text: p.title })),
    el("p", { class: "path", text: j.root }),
    steps,
    el("p", { class: "figure num", id: "pv-bytes", text: human(j.bytes) }),
    el("div", { class: `bar is-big${p.frac == null ? " is-indeterminate" : ""}`, id: "pv-meter", role: "progressbar", "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": p.frac == null ? null : String(Math.round(p.frac * 100)) },
      el("span", { class: "fill", vars: { w: p.frac == null ? "30%" : `${Math.min(1, p.frac) * 100}%` } })),
    el("p", { class: "muted", id: "pv-est", text: j.cancelRequested ? tf("cancelWaiting", { s: fmtMs(performance.now() - j.cancelAt) }) : p.detail }),
    el("dl", { class: "stats is-compact" },
      stat(t("kFiles"), num(j.files), null, { id: "pv-files" }),
      stat(t("elapsed"), fmtMs(secs * 1000), null, { id: "pv-time" }),
      stat(t("rate"), secs > 0.5 ? num(Math.round(j.files / secs)) : "–", null, { id: "pv-rate" })),
    j.kind === "dupes" ? el("p", { class: "note", text: t("hashCacheNote") }) : null,
    el("div", { class: "row-acts" },
      btn("stop", j.cancelRequested ? t("cancelling") : t("cancel"), { class: "danger", id: "pv-cancel", disabled: j.cancelRequested, onclick: () => cancelJob(j) }))));
}

// Cheap in-place update of the progress panel (no rebuild per event).
function updateProgressView(j) {
  const secs = (performance.now() - j.startedAt) / 1000;
  const setText = (id, v) => {
    const n = $(id);
    if (!n) return;
    const target = n.querySelector(".stat-v") || n;
    if (target.textContent !== v) target.textContent = v;
  };
  const p = progressInfo(j);
  setText("pv-title", p.title);
  setText("pv-bytes", human(j.bytes));
  setText("pv-files", num(j.files));
  setText("pv-time", fmtMs(secs * 1000));
  setText("pv-rate", secs > 0.5 ? num(Math.round(j.files / secs)) : "–");
  setText("pv-est", j.cancelRequested ? tf("cancelWaiting", { s: fmtMs(performance.now() - j.cancelAt) }) : p.detail);
  const steps = $("pv-steps");
  if (steps) [...steps.children].forEach((li, i) => {
    li.className = i < j.phase ? "is-done" : i === j.phase ? "is-now" : "";
    if (i === j.phase) li.setAttribute("aria-current", "step"); else li.removeAttribute("aria-current");
  });
  const meter = $("pv-meter");
  if (meter) {
    meter.classList.toggle("is-indeterminate", p.frac == null);
    meter.firstChild.style.setProperty("--w", p.frac == null ? "30%" : `${Math.min(1, p.frac) * 100}%`);
    if (p.frac == null) meter.removeAttribute("aria-valuenow"); else meter.setAttribute("aria-valuenow", String(Math.round(p.frac * 100)));
  }
}

function errorView(j) {
  return el("div", { class: "stage" }, el("section", { class: "panel stage-panel" },
    el("header", { class: "panel-head" }, el("h2", { text: t("stError") })),
    el("p", { class: "path", text: j.root }),
    banner("danger", j.error),
    el("div", { class: "row-acts" },
      btn("refresh", t("retry"), { class: "primary", onclick: () => rescan(j) }),
      btn("close", t("close"), { onclick: () => closeJob(j) }))));
}

// ------------------------------ scan view ------------------------------
function driveOf(path) {
  return drives
    .filter((d) => isUnder(path, d.path))
    .sort((a, b) => b.path.length - a.path.length)[0] || null;
}

function scanView(j) {
  const v = j.view;
  const diffMap = j.diff ? changesByPath(j.diff) : null;
  const parent = parentPath(j.path);
  const head = el("header", { class: "viewhead" },
    btn("back", t("back"), { class: "icon-btn", iconOnly: true, title: t("backHint"), disabled: !j.back.length, "data-fk": "nav-back", onclick: () => goBack(j) }),
    btn("up", t("up"), { class: "icon-btn", iconOnly: true, title: t("upHint"), disabled: !parent || !isUnder(parent, j.root), "data-fk": "nav-up", onclick: () => goUp(j) }),
    crumbBar(j),
    el("div", { class: "viewhead-acts" },
      btn("compare", t("compare"), { class: "quiet", "data-fk": "act-compare", disabled: j.partial, title: j.partial ? t("compareNeedsComplete") : null, onclick: () => openCompare(j) }),
      btn("copies", t("dupesHere"), { class: "quiet", "data-fk": "act-dupes", onclick: () => enqueue("dupes", j.path) }),
      btn("refresh", t("rescan"), { "data-fk": "act-rescan", title: "F5", onclick: () => rescan(j) })));
  const notice = partialNotice(v, j);
  return el("div", { class: "view scan-view" },
    head,
    notice ? banner(partialCauses(v, j.opts?.max_depth ?? null).partial ? "warn" : "info", notice) : null,
    statsRow(j, v),
    el("div", { class: "split" },
      el("section", { class: "panel map-panel", "aria-label": t("map") },
        el("header", { class: "panel-head" }, el("h2", { text: t("map") }), el("span", { class: "muted", text: t("mapSub") })),
        el("div", { class: "treemap", oncontextmenu: (e) => { e.preventDefault(); goUp(j); } })),
      el("div", { class: "side-col" }, diskPanel(v), typesPanel(j, v))),
    detailsPanel(j, v, diffMap));
}

function crumbBar(j) {
  const nav = el("nav", { class: "crumbs", "aria-label": t("pathLabel") });
  const parts = crumbs(j.path);
  parts.forEach((c, i) => {
    const last = i === parts.length - 1;
    const inTree = isUnder(c.target, j.root);
    if (last) nav.append(el("span", { class: "crumb is-current", "aria-current": "location", text: c.label }));
    else if (inTree) nav.append(el("button", { type: "button", class: "crumb", onclick: () => navigate(j, c.target) }, c.label));
    else nav.append(el("span", { class: "crumb is-outside", title: t("outsideScan"), text: c.label }));
    if (!last) nav.append(el("span", { class: "crumb-sep", "aria-hidden": "true", text: "/" }));
  });
  return nav;
}

function statsRow(j, v) {
  const d = driveOf(v.root);
  const used = v.disk_total - v.disk_free;
  const atLeast = v.complete === false ? "≥ " : "";
  const secs = (j.elapsedMs || 0) / 1000;
  const engine = v.engine === "mft" ? "NTFS MFT" : t("engineWalk");
  const nExcl = j.opts?.exclude?.length || 0;
  const row = el("dl", { class: "stats" },
    stat(t("kSize"), atLeast + human(v.total_size),
      used > 0 ? `${pct(v.total_size, used)} ${t("kSizeOfUsed")} ${d ? d.path : t("kSizeOfDisk")}` : null),
    stat(t("kFiles"), atLeast + num(v.total_files),
      v.total_files ? `${t("kAvg")} ${human(Math.round(v.total_size / v.total_files))} ${t("kPerFile")}` : null),
    stat(t("kFolders"), atLeast + num(v.total_dirs), `${num(v.root_files_count)} ${t("kDirect")}`),
    v.disk_total > 0 ? stat(t("kFree"), human(v.disk_free), `${t("of")} ${human(v.disk_total)} · ${pct(used, v.disk_total)} ${t("kUsedPct")}`) : null,
    stat(t("kScan"), fmtMs(j.elapsedMs || 0),
      [engine, secs > 0 ? `${num(Math.round((j.cache.get(j.root) || v).total_files / secs))} ${t("rate")}` : null,
        v.skipped ? `${num(v.skipped)} ${t("kSkipped")}` : t("kNoSkip"),
        nExcl ? `${num(nExcl)} ${t("excludesUsed")}` : null].filter(Boolean).join(" · "),
      nExcl ? { title: j.opts.exclude.join(", ") } : {}),
    sinceStat(j));
  return row;
}

// "Since last scan": filled in by autoCompare() right after a scan; a click
// opens the full comparison.
function sinceStat(j) {
  if (j.diff) {
    const tint = deltaTint(j.diff.delta, Math.abs(j.diff.delta));
    return stat(`${t("kSince")} ${fmtDate(j.diff.old_taken_at)}`, humanDelta(j.diff.delta),
      `${human(j.diff.old_size)} → ${human(j.diff.new_size)}`, {}, tint.cls ? `is-${tint.cls}` : "");
  }
  if (j.path !== j.root || !j.auto) return null;
  const open = { role: "button", tabindex: "0", class: "is-link", "data-fk": "since", onclick: () => openCompare(j) };
  const a = j.auto;
  if (a.state === "loading") return stat(t("sinceLast"), "…", t("comparing"));
  if (a.state === "none") return stat(t("sinceLast"), "—", t("firstScanShort"));
  if (a.state !== "done") return stat(t("sinceLast"), "?", `${t("diffFailed")} ${a.error || ""}`, {}, "is-error");
  const d = a.diff;
  const files = d.files_compared
    ? ` · ${num(d.new_files_count)} ${t("newShort")}, ${num(d.removed_files_count)} ${t("goneShort")}, ${num(d.changed_files_count)} ${t("changedShort")}`
    : "";
  const tint = deltaTint(d.delta, Math.abs(d.delta)).cls;
  return stat(`${t("sinceLast")} · ${fmtDate(d.old_taken_at)}`, humanDelta(d.delta), `${t("clickDetails")}${files}`, open, tint ? `is-${tint}` : "");
}

// Compare a finished scan with the newest earlier snapshot of its root.
async function autoCompare(j) {
  const run = j.run;
  j.auto = { state: "loading" };
  renderFor(j);
  try {
    const history = (await invoke("list_snapshots", { job: j.id })).snapshots || [];
    if (j.run !== run) return;
    j.history = history;
    j.historyError = null;
    const older = history.filter((s) => s.taken_at < j.scanUnix);
    if (!older.length) {
      j.auto = { state: "none" };
    } else {
      const diff = await invoke("diff_since", { job: j.id, path: j.root, takenAt: older[0].taken_at, top: j.top });
      if (j.run !== run) return;
      j.auto = { state: "done", diff };
    }
  } catch (e) {
    if (j.run !== run) return;
    j.auto = { state: "error", error: errText(e) };
  }
  if (byId(j.id)) renderFor(j);
}

// The Compare button / card: show the changes tab with a comparison of the
// folder on screen against the newest earlier snapshot.
async function openCompare(j) {
  j.tab = "changes";
  if (j.partial) return renderMain();
  if (j.path === j.root && j.auto?.state === "done") {
    j.diff = j.auto.diff;
    j.diffError = null;
    j.sinceAt = j.diff.old_taken_at;
    return renderMain();
  }
  renderMain();
  if (!j.history) {
    try {
      j.history = (await invoke("list_snapshots", { job: j.id })).snapshots || [];
      j.historyError = null;
    } catch (e) {
      j.history = [];
      j.historyError = errText(e);
      return renderMain();
    }
  }
  const older = j.history.filter((s) => s.taken_at < j.scanUnix);
  if (older.length) await showChanges(j, j.sinceAt ?? older[0].taken_at);
  else renderMain();
}

function diskPanel(v) {
  if (!(v.disk_total > 0)) return null;
  const used = v.disk_total - v.disk_free;
  const other = Math.max(0, used - v.total_size);
  const d = driveOf(v.root);
  const seg = (cls, size, label) => {
    const s = el("span", { class: `seg ${cls}`, vars: { w: `${(100 * size) / v.disk_total}%` } });
    tip(s, () => [el("b", { text: label }), tipRow(t("colSize"), human(size)), tipRow(t("colPct"), pct(size, v.disk_total))]);
    return s;
  };
  return el("section", { class: "panel" },
    el("header", { class: "panel-head" },
      el("h2", { text: t("disk") }),
      el("span", { class: "muted", text: d ? `${d.path}${d.label ? ` · ${d.label}` : ""}${d.fs ? ` · ${d.fs}` : ""}` : human(v.disk_total) })),
    el("div", { class: "stack-bar" }, seg("is-this", v.total_size, t("diskThis")), seg("is-other", other, t("diskOther"))),
    el("ul", { class: "legend" },
      el("li", {}, el("i", { class: "key is-this" }), `${t("diskThis")} ${human(v.total_size)} (${pct(v.total_size, v.disk_total)})`),
      el("li", {}, el("i", { class: "key is-other" }), `${t("diskOther")} ${human(other)}`),
      el("li", {}, el("i", { class: "key is-free" }), `${t("diskFree")} ${human(v.disk_free)}`)));
}

function typesPanel(j, v) {
  const cats = categorize(v.stats.extensions);
  const max = cats[0]?.size || 1;
  const list = el("div", { class: "hbars" }, cats.map((c) => {
    const row = el("button", {
      type: "button", class: "hbar",
      onclick: () => { j.tab = "types"; j.catFilter = c.id; renderMain(); },
    },
    el("span", { class: "hbar-label", text: t(`cat_${c.id}`) }),
    bar(c.size / max),
    el("span", { class: "hbar-v num" }, el("b", { text: human(c.size) }), ` ${pct(c.size, v.total_size)}`));
    tip(row, () => [
      el("b", { text: t(`cat_${c.id}`) }),
      tipRow(t("colSize"), `${human(c.size)} (${pct(c.size, v.total_size)})`),
      tipRow(t("colFiles"), num(c.count)),
      el("div", { class: "tip-path", text: c.exts.slice(0, 6).map((e) => `${e.ext ? `.${e.ext}` : t("noExt")} ${human(e.size)}`).join(" · ") }),
    ]);
    return row;
  }));
  return el("section", { class: "panel grow" },
    el("header", { class: "panel-head" }, el("h2", { text: t("types") }), el("span", { class: "muted", text: t("typesSub") })),
    list);
}

// Two-level squarified map: each big folder frames its own biggest
// sub-folders; small ones are a single tile. Pseudo tiles ("files directly
// here", "smaller folders") make the areas add up to the folder's total.
function drawTreemap(container, j) {
  const v = j.view;
  container.replaceChildren();
  const W = container.clientWidth, H = container.clientHeight;
  if (!W || !H) return;
  if (!v.total_size) {
    container.append(el("p", { class: "map-empty", text: t("mapEmpty") }));
    return;
  }
  const diffMap = j.diff ? changesByPath(j.diff) : null;
  const kids = v.children.map((c, i) => ({ ...c, nested: v.nested?.[i] || [] }));
  const items = mapItems(v.total_size, kids, v.root_files_size);
  const rects = squarify(items, 0, 0, W, H);
  const maxL1 = Math.max(1, ...items.filter((i) => i.path).map((i) => i.size));
  const maxL2 = Math.max(1, ...kids.flatMap((k) => k.nested.map((n) => n.size)));
  const GAP = 2, HEAD = 24;

  const place = (node, x, y, w, h) => {
    node.style.left = `${x}px`;
    node.style.top = `${y}px`;
    node.style.width = `${Math.max(0, w)}px`;
    node.style.height = `${Math.max(0, h)}px`;
    container.append(node);
  };
  const label = (name, item, share, w, h, stacked) => {
    if (w < 46 || h < 18) return [];
    const out = [el("span", { class: "tile-name", text: name })];
    if (stacked ? h > 34 : w > 150) out.push(el("span", { class: "tile-size", text: `${sizeText(item, t("notMeasured"))}${share ? ` · ${share}` : ""}` }));
    return out;
  };
  const decorate = (node, path) => {
    const c = diffMap?.map.get(String(path));
    if (!c) return;
    const { cls, strength } = c.added ? { cls: "added", strength: 1 } : deltaTint(c.delta, diffMap.maxAbs);
    node.classList.add(`is-${cls}`);
    node.style.setProperty("--tint", String(0.25 + 0.75 * strength));
  };
  const folderTip = (item, parentSize) => () => {
    const out = [el("b", { text: item.path ? baseName(item.path) : item.label }), item.path ? el("div", { class: "tip-path", text: item.path }) : null,
      tipRow(t("colSize"), sizeText(item, t("notMeasured"))),
      tipRow(`${t("colPct")} ${t("shareOfView")}`, pct(item.size, v.total_size))];
    if (item.path && parentSize && parentSize !== v.total_size) out.push(tipRow(`${t("colPct")} ${baseName(parentPath(item.path) || "")}`, pct(item.size, parentSize)));
    if (item.file_count != null) out.push(tipRow(t("colFiles"), countText(item, num)));
    const c = diffMap?.map.get(String(item.path));
    if (c) out.push(tipRow(t("colDelta"), c.added ? t("newBadge") : humanDelta(c.delta)));
    return out;
  };

  for (const rc of rects) {
    const it = rc.item;
    const x = rc.x + GAP / 2, y = rc.y + GAP / 2, w = rc.w - GAP, h = rc.h - GAP;
    if (!it.path) {
      it.label = it.rest === "files" ? `${num(v.root_files_count)} ${t("filesHere")}` : t("otherFolders");
      const node = el("div", { class: "tile l1 is-leaf is-rest" });
      node.append(el("div", { class: "tile-head" }, label(it.label, it, pct(it.size, v.total_size), w, h, true)));
      tip(node, folderTip(it), 0);
      place(node, x, y, w, h);
      continue;
    }
    const framed = w > 150 && h > 90 && it.nested.length > 0;
    const node = el("button", { type: "button", class: `tile l1${framed ? "" : " is-leaf"}`, "aria-label": `${baseName(it.path)} ${sizeText(it, t("notMeasured"))}`, onclick: () => navigate(j, it.path) });
    node.style.setProperty("--s", String(Math.sqrt(it.size / maxL1)));
    node.append(el("div", { class: "tile-head" }, label(baseName(it.path), it, pct(it.size, v.total_size), w, h, !framed)));
    decorate(node, it.path);
    tip(node, folderTip(it), 0);
    place(node, x, y, w, h);
    if (!framed) continue;

    const inner = mapItems(it.size, it.nested, 0);
    const ix = x + 3, iy = y + HEAD, iw = w - 6, ih = h - HEAD - 3;
    for (const r2 of squarify(inner, ix, iy, iw, ih)) {
      const n2 = r2.item;
      const x2 = r2.x + 1, y2 = r2.y + 1, w2 = r2.w - 2, h2 = r2.h - 2;
      if (!n2.path) {
        n2.label = t("filesSmaller");
        const rest = el("div", { class: "tile l2 is-rest" }, label(n2.label, n2, null, w2, h2, true));
        tip(rest, folderTip(n2, it.size), 0);
        place(rest, x2, y2, w2, h2);
        continue;
      }
      const t2 = el("button", {
        type: "button", class: "tile l2", tabindex: "-1", "aria-label": `${baseName(n2.path)} ${sizeText(n2, t("notMeasured"))}`,
        onclick: (e) => { e.stopPropagation(); navigate(j, n2.path); },
      }, label(baseName(n2.path), n2, null, w2, h2, true));
      t2.style.setProperty("--s", String(Math.sqrt(n2.size / maxL2)));
      tip(t2, folderTip(n2, it.size), 0);
      place(t2, x2, y2, w2, h2);
    }
  }
}

// Tabs that follow the ARIA tabs pattern: one tab stop, arrows move and select.
function tabStrip(items, current, onpick, fkPrefix) {
  const list = el("div", { class: "tabs", role: "tablist" });
  const tabs = items.map(([id, label, count]) => el("button", {
    type: "button", role: "tab", class: "tab", id: `${fkPrefix}-${id}`, "data-fk": `${fkPrefix}-${id}`,
    "aria-selected": String(current === id), tabindex: current === id ? "0" : "-1",
    onclick: () => onpick(id),
  }, label, count != null ? el("span", { class: "tab-count num", text: num(count) }) : null));
  list.addEventListener("keydown", (e) => {
    const i = tabs.indexOf(document.activeElement);
    if (i < 0) return;
    const to = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[e.key];
    if (to == null) return;
    e.preventDefault();
    const n = (to + tabs.length) % tabs.length;
    onpick(items[n][0]);
    // The strip was rebuilt: keep the keyboard on the tab just selected.
    document.querySelector(`[data-fk="${fkPrefix}-${items[n][0]}"]`)?.focus();
  });
  list.append(...tabs);
  return list;
}

// Details: folders / files / extensions / size ranges / changes.
function detailsPanel(j, v, diffMap) {
  const tabs = [
    ["folders", t("tabFolders"), v.children.length],
    ["files", t("tabFiles"), v.top_files.length],
    ["types", t("tabTypes"), v.stats.extensions.length],
    ["sizes", t("tabSizes"), null],
    ["changes", t("tabChanges"), null],
  ];
  const body = el("div", { class: "tabbody", role: "tabpanel", "aria-labelledby": `tab-${j.tab}` });
  const tools = el("div", { class: "tabtools" });
  const panel = el("section", { class: "panel details" },
    tabStrip(tabs, j.tab, (id) => { if (id === "changes" && j.tab !== "changes") return openCompare(j); j.tab = id; renderMain(); }, "tab"),
    tools, body);
  if (j.tab === "folders") foldersTab(j, v, body, diffMap);
  else if (j.tab === "files") filesTab(j, v, body, tools);
  else if (j.tab === "types") typesTab(j, v, body, tools);
  else if (j.tab === "sizes") sizesTab(v, body);
  else changesTab(j, v, body, tools);
  if (!tools.childNodes.length) tools.remove();
  return panel;
}

function delButton(path) {
  return btn("trash", `${t(settings.trash ? "actTrash" : "del")}: ${baseName(path)}`, {
    class: "icon-btn quiet is-danger-hover", iconOnly: true,
    onclick: (e) => { e.stopPropagation(); deletePath(path); },
  });
}

function foldersTab(j, v, body, diffMap) {
  const table = el("table", { class: "table" },
    el("thead", {}, el("tr", {},
      el("th", { text: t("colName") }), el("th", { text: t("colShare") }), el("th", { class: "num", text: t("colSize") }),
      el("th", { class: "num", text: t("colPct") }), el("th", { class: "num", text: t("colFiles") }),
      diffMap ? el("th", { class: "num", text: t("colDelta") }) : null, el("th", { class: "cell-acts" }, el("span", { class: "sr-only", text: t("colActions") })))));
  const tb = el("tbody");
  for (const d of v.children) {
    const c = diffMap?.map.get(String(d.path));
    let cls = "is-click", delta = "";
    if (c?.added) { cls += " is-added"; delta = t("newBadge"); } else if (c) { const tc = deltaTint(c.delta, diffMap.maxAbs).cls; if (tc) cls += ` is-${tc}`; delta = humanDelta(c.delta); }
    const open = el("button", { type: "button", class: "row-link", onclick: () => navigate(j, d.path) },
      el("span", { class: "with-ic" }, icon("folder"), el("span", { class: "cell-title", text: baseName(d.path) })));
    tb.append(el("tr", { class: cls, onclick: (e) => { if (e.target === e.currentTarget || !e.target.closest("button")) navigate(j, d.path); } },
      el("td", { class: "cell-name" }, open, el("span", { class: "cell-path", text: d.path })),
      el("td", { class: "cell-bar" }, bar(v.total_size ? d.size / v.total_size : 0)),
      el("td", { class: "num", text: sizeText(d, t("notMeasured")) }),
      el("td", { class: "num", text: unmeasured(d) ? "—" : pct(d.size, v.total_size) }),
      el("td", { class: "num", text: countText(d, num) }),
      diffMap ? el("td", { class: "num" }, el("span", { class: "delta", text: delta })) : null,
      el("td", { class: "cell-acts" }, delButton(d.path))));
  }
  const pseudo = (label, size, count) => el("tr", { class: "is-pseudo" },
    el("td", { class: "cell-name" }, el("span", { class: "with-ic muted" }, icon("file"), el("span", { text: label }))),
    el("td", { class: "cell-bar" }, bar(v.total_size ? size / v.total_size : 0, "is-muted")),
    el("td", { class: "num", text: human(size) }),
    el("td", { class: "num", text: pct(size, v.total_size) }),
    el("td", { class: "num", text: count == null ? "" : num(count) }),
    diffMap ? el("td") : null, el("td"));
  if (v.root_files_count > 0) tb.append(pseudo(`${num(v.root_files_count)} ${t("filesHere")}`, v.root_files_size, v.root_files_count));
  const listed = v.children.reduce((a, d) => a + d.size, 0) + v.root_files_size;
  if (v.total_size > listed) tb.append(pseudo(t("otherFolders"), v.total_size - listed, null));
  if (!v.children.length && !v.root_files_count) tb.append(el("tr", {}, el("td", { colspan: "7", class: "muted", text: t("emptyFolder") })));
  table.append(tb);
  body.append(table);
}

function filesTab(j, v, body, tools) {
  const input = el("input", { type: "search", class: "filter", placeholder: t("filterFiles"), "aria-label": t("filterFiles"), value: j.fileFilter || "", "data-fk": "file-filter" });
  const count = el("span", { class: "muted num", "aria-live": "polite" });
  tools.append(input, count);
  const box = el("div", { class: "vlist" });
  body.append(box);
  const draw = () => {
    const f = (j.fileFilter || "").toLowerCase();
    const items = f ? v.top_files.filter((x) => String(x.path).toLowerCase().includes(f)) : v.top_files;
    count.textContent = f ? `${num(items.length)} / ${num(v.top_files.length)}` : "";
    const head = el("div", { class: "vrow is-head" },
      el("span", { text: t("colName") }), el("span", { text: t("colType") }),
      el("span", { class: "num", text: t("colSize") }), el("span", { class: "num", text: t("colPct") }), el("span"));
    if (!items.length) {
      box.replaceChildren(head, el("p", { class: "empty", text: f ? t("noMatch") : t("noFiles") }));
      return;
    }
    virtualList(box, head, items, 44, (x) => {
      const ext = (String(x.path).match(/\.([^.\\/]+)$/) || [])[1] || "";
      return el("div", { class: "vrow" },
        el("div", { class: "cell-name" },
          el("span", { class: "cell-title", text: baseName(x.path) }),
          el("span", { class: "cell-path", text: parentPath(x.path) || "" })),
        el("span", {}, ext ? el("span", { class: "ext", text: `.${ext.toLowerCase()}` }) : null),
        el("span", { class: "num", text: human(x.size) }),
        el("span", { class: "num", text: pct(x.size, v.total_size) }),
        delButton(x.path));
    });
  };
  input.addEventListener("input", () => { j.fileFilter = input.value; draw(); });
  draw();
}

// Windowed list: only rows in view are built.
function virtualList(container, head, items, rowH, renderRow) {
  const inner = el("div", { class: "vlist-inner" });
  inner.style.height = `${items.length * rowH}px`;
  container.replaceChildren(head, inner);
  const draw = () => {
    const top = Math.max(0, container.scrollTop - head.offsetHeight);
    const vh = container.clientHeight || 400;
    const start = Math.max(0, Math.floor(top / rowH) - 6);
    const end = Math.min(items.length, Math.ceil((top + vh) / rowH) + 6);
    const frag = document.createDocumentFragment();
    for (let k = start; k < end; k++) {
      const row = renderRow(items[k], k);
      row.style.top = `${k * rowH}px`;
      frag.append(row);
    }
    inner.replaceChildren(frag);
  };
  container.onscroll = draw;
  draw();
}

function typesTab(j, v, body, tools) {
  const cats = categorize(v.stats.extensions);
  tools.append(selectInput(j.catFilter || "", [["", t("allCategories")], ...cats.map((c) => [c.id, t(`cat_${c.id}`)])],
    (val) => { j.catFilter = val; renderMain(); }, { "data-fk": "cat-filter", "aria-label": t("colCategory") }));
  const rows = v.stats.extensions.filter((e) => !j.catFilter || categoryOf(e.ext) === j.catFilter);
  const max = rows[0]?.size || 1;
  const shown = rows.slice(0, 500);
  const table = el("table", { class: "table" },
    el("thead", {}, el("tr", {},
      el("th", { text: t("colExt") }), el("th", { text: t("colCategory") }), el("th", { text: t("colShare") }),
      el("th", { class: "num", text: t("colSize") }), el("th", { class: "num", text: t("colPct") }),
      el("th", { class: "num", text: t("colCount") }), el("th", { class: "num", text: `Ø ${t("colSize")}` }))),
    el("tbody", {}, shown.map((e) => el("tr", {},
      el("td", {}, el("span", { class: "ext", text: e.ext ? `.${e.ext}` : t("noExt") })),
      el("td", { class: "muted", text: t(`cat_${categoryOf(e.ext)}`) }),
      el("td", { class: "cell-bar" }, bar(e.size / max)),
      el("td", { class: "num", text: human(e.size) }),
      el("td", { class: "num", text: pct(e.size, v.total_size) }),
      el("td", { class: "num", text: num(e.count) }),
      el("td", { class: "num", text: human(Math.round(e.size / Math.max(1, e.count))) })))));
  body.append(table);
  if (rows.length > shown.length) body.append(el("p", { class: "list-more", text: `${t("showing")} ${num(shown.length)} / ${num(rows.length)}` }));
}

// Two single-measure column charts side by side (never one chart, two axes).
function sizesTab(v, body) {
  const b = v.stats.size_buckets;
  const chart = (title, values, fmt) => {
    const max = Math.max(1, ...values);
    return el("figure", { class: "cols" },
      el("figcaption", { text: title }),
      el("div", { class: "plot" }, values.map((val, i) => {
        const col = el("div", { class: "col", tabindex: "0", "aria-label": `${bucketLabel(b, i)}: ${fmt(val)}` },
          el("span", { class: "col-v num", text: val ? fmt(val) : "" }),
          el("span", { class: "col-bar", vars: { h: `${(100 * val) / max}%` } }));
        tip(col, () => [el("b", { text: bucketLabel(b, i) }),
          tipRow(t("colFiles"), `${num(b[i].count)} (${pct(b[i].count, v.total_files)})`),
          tipRow(t("colSize"), `${human(b[i].size)} (${pct(b[i].size, v.total_size)})`)], 0);
        return col;
      })),
      el("div", { class: "axis" }, b.map((_, i) => el("span", { text: bucketLabel(b, i) }))));
  };
  body.append(el("div", { class: "sizes" },
    el("p", { class: "muted", text: t("sizesHint") }),
    el("div", { class: "cols-wrap" },
      chart(t("sizesCount"), b.map((x) => x.count), num),
      chart(t("sizesBytes"), b.map((x) => x.size), human))));
}

// ------------------------------ what grew ---------------------------------
// Every complete scan is stored as a snapshot of its root (every folder and
// file, compressed). Compare the folder on screen against an earlier one.
async function changesTab(j, v, body, tools) {
  if (j.partial) return body.append(banner("warn", `${t("compareNeedsComplete")} ${partialNotice(j.cache.get(j.root) || v, j)}`));
  if (!j.history) {
    body.append(el("p", { class: "muted", text: t("loadingHistory") }));
    try {
      j.history = (await invoke("list_snapshots", { job: j.id })).snapshots || [];
      j.historyError = null;
    } catch (e) {
      j.history = [];
      j.historyError = errText(e);
    }
    if (active() === j && j.tab === "changes") renderMain();
    return;
  }
  // A failed read is not "no history": say what failed.
  if (j.historyError) return body.append(banner("danger", `${t("historyFailed")} ${j.historyError}`));
  // Snapshots older than this scan; the newest one is usually the scan itself.
  const older = j.history.filter((s) => s.taken_at < j.scanUnix);
  if (older.length) {
    const chosen = older.some((s) => s.taken_at === j.sinceAt) ? j.sinceAt : older[0].taken_at;
    const sel = selectInput(String(chosen), older.map((s) => [String(s.taken_at), `${fmtDate(s.taken_at)}${s.files ? "" : ` (${t("foldersOnly")})`}`]),
      (val) => { j.sinceAt = parseInt(val, 10); }, { "data-fk": "since-pick", id: "since-pick" });
    tools.append(el("label", { class: "inline-label", for: "since-pick" }, t("since")), sel,
      btn("compare", t("showChanges"), { class: "small primary", "data-fk": "since-go", onclick: () => showChanges(j, parseInt(sel.value, 10)) }));
  }
  if (j.path !== j.root) body.append(el("p", { class: "muted", text: tf("comparingFolder", { p: j.path }) }));
  if (j.diffError) body.append(banner("danger", `${t("diffFailed")} ${j.diffError}`));
  if (j.diff) renderChanges(j.diff, body, j);
  else if (!older.length) body.append(el("p", { class: "empty", text: t("firstScan") }));
  snapshotList(j, body);
}

async function showChanges(j, takenAt) {
  j.sinceAt = takenAt;
  j.diffGuard = j.diffGuard || latestOnly();
  const ticket = j.diffGuard.next();
  const path = j.path;
  status(t("comparing"));
  let diff = null, error = null;
  try {
    diff = await invoke("diff_since", { job: j.id, path, takenAt, top: j.top });
    status("");
  } catch (e) {
    error = errText(e);
    status(error, true);
  }
  // Only the newest comparison for the folder still on screen counts; a
  // failure clears the old diff instead of leaving it under the new date.
  if (!j.diffGuard.current(ticket) || j.path !== path) return;
  j.diff = diff;
  j.diffError = error;
  if (active() === j) renderMain();
}

// The stored snapshots of this scan's root: when, how big on disk, compare
// or delete.
function snapshotList(j, box) {
  if (!j.history.length) return;
  const total = j.history.reduce((a, s) => a + s.bytes, 0);
  box.append(el("h3", { class: "sub-head", text: `${t("snapshots")} — ${num(j.history.length)} · ${human(total)} ${t("onDisk")}` }));
  box.append(el("p", { class: "muted", text: t("snapshotsHint")(settings.keep_snapshots) }));
  box.append(el("table", { class: "table" }, el("tbody", {}, j.history.map((s) => {
    const current = s.taken_at >= j.scanUnix;
    return el("tr", {},
      el("td", { class: "cell-date num", text: fmtDate(s.taken_at) }),
      el("td", { class: "muted", text: current ? t("thisScan") : s.files ? t("filesAndFolders") : t("foldersOnly") }),
      el("td", { class: "num", text: human(s.bytes) }),
      el("td", { class: "cell-acts" },
        current ? null : btn("compare", t("compare"), { class: "small quiet", onclick: () => showChanges(j, s.taken_at) }),
        btn("trash", `${t("deleteSnapshot")} ${fmtDate(s.taken_at)}`, {
          class: "icon-btn quiet is-danger-hover", iconOnly: true,
          onclick: async () => {
            if (!(await ask(t("deleteSnapshot"), [{ text: fmtDate(s.taken_at) }], t("del"), true))) return;
            try { await invoke("delete_snapshot", { job: j.id, takenAt: s.taken_at }); } catch (e) { status(errText(e), true); }
            j.history = null;
            if (j.diff?.old_taken_at === s.taken_at) j.diff = null;
            if (active() === j) renderMain();
          },
        })));
  }))));
}

function renderChanges(d, box, j) {
  const tint = deltaTint(d.delta, Math.abs(d.delta));
  box.append(el("div", { class: "change-head" },
    el("span", { class: `delta-big${tint.cls ? ` is-${tint.cls}` : ""}`, text: humanDelta(d.delta) }),
    el("span", { class: "muted", text: `${t("changes")} ${fmtDate(d.old_taken_at)}: ${human(d.old_size)} → ${human(d.new_size)}, ${d.files_delta >= 0 ? "+" : ""}${num(d.files_delta)} ${t("files")}` })));
  if (d.files_compared) {
    box.append(el("dl", { class: "stats is-compact" },
      stat(t("newFiles"), num(d.new_files_count), `+${human(d.new_files_bytes)}`, {}, d.new_files_count ? "is-grow" : ""),
      stat(t("removedFiles"), num(d.removed_files_count), `-${human(d.removed_files_bytes)}`, {}, d.removed_files_count ? "is-shrink" : ""),
      stat(t("changedFiles"), num(d.changed_files_count), humanDelta(d.changed_files_delta), {}, deltaTint(d.changed_files_delta, 1).cls ? `is-${deltaTint(d.changed_files_delta, 1).cls}` : "")));
  } else {
    box.append(el("p", { class: "muted", text: t("oldSnapshotNoFiles") }));
  }
  const unchanged = !d.grown.length && !d.shrunk.length && !d.added.length && !d.removed.length &&
    !d.new_files_count && !d.removed_files_count && !d.changed_files_count;
  if (unchanged) {
    box.append(el("p", { class: "empty", text: t("unchanged") }));
    return;
  }
  const section = (title, items, cols, total) => {
    if (!items.length) return;
    const more = total > items.length ? ` — ${t("biggest")} ${num(items.length)} / ${num(total)}` : "";
    box.append(el("h3", { class: "sub-head", text: `${title}${more}` }));
    box.append(el("table", { class: "table" }, el("tbody", {}, items.map(cols))));
  };
  const nameCell = (x, clickable) => el("td", { class: "cell-name" },
    clickable ? el("button", { type: "button", class: "row-link", onclick: () => navigate(j, x.path) }, el("span", { class: "cell-title", text: baseName(x.path) }))
      : el("span", { class: "cell-title", text: baseName(x.path) }),
    el("span", { class: "cell-path", text: x.path }));
  const deltaRow = (clickable) => (x) => el("tr", { class: clickable ? "is-click" : "" },
    el("td", { class: "num cell-delta" }, el("span", { class: `delta ${x.delta > 0 ? "is-grow" : "is-shrink"}`, text: humanDelta(x.delta) })),
    nameCell(x, clickable),
    el("td", { class: "num", text: `${human(x.old_size)} → ${human(x.new_size)}` }));
  const sizeRow = (clickable) => (x) => el("tr", { class: clickable ? "is-click" : "" },
    el("td", { class: "num cell-delta", text: human(x.size) }),
    nameCell(x, clickable),
    el("td"));
  section(t("grew"), d.grown, deltaRow(true));
  section(t("shrank"), d.shrunk, deltaRow(true));
  section(t("newFolders"), d.added, sizeRow(true));
  section(t("removedFolders"), d.removed, sizeRow(false));
  section(t("newFiles"), d.new_files_list, deltaRow(false), d.new_files_count);
  section(t("changedFiles"), d.changed_files_list, deltaRow(false), d.changed_files_count);
  section(t("removedFiles"), d.removed_files_list, deltaRow(false), d.removed_files_count);
}

// -------------------------------- delete ----------------------------------
// Remove a file/folder — to the Recycle Bin by default (settings). It runs
// as its own job: live progress and a Stop button in the status bar. Every
// open scan holding the path has it taken out of its tree by the backend, so
// the views just re-read their folder (instant) and show the freed space.
async function deletePath(path) {
  if (op) return status(t("removeBusy"), true);
  const trash = settings.trash;
  const ok = await ask(trash ? t("confirmTrash") : t("confirmDelete"),
    [{ text: path, cls: "path" }, trash ? { text: t("toTrashNote") } : { text: t("confirmDeleteWarn"), cls: "is-error" }],
    trash ? t("actTrash") : t("del"), !trash);
  if (!ok) return;
  const id = nextId++;
  op = { id, path, files: 0, bytes: 0, stopping: false, t0: performance.now() };
  renderOp();
  let msg, isErr = false;
  try {
    const r = await invoke("remove_path_cmd", { job: id, path, trash, apply: true });
    msg = `${trash ? t("trashed") : t("deleted")}: ${num(r.files)} ${t("files")}, ${human(r.bytes)} (${fmtMs(performance.now() - op.t0)})`;
    if (op.stopping) msg += ` — ${t("removeStopped")}`;
    if (r.errors?.length) { msg += ` — ${num(r.errors.length)} ${t("errors")}: ${r.errors[0]}`; isErr = true; }
  } catch (e) {
    msg = errText(e);
    isErr = true;
  }
  op = null;
  renderOp();
  status(msg, isErr);
  for (const j of [...jobs]) {
    if (j.kind !== "scan" || !j.view) continue;
    // The scanned folder itself is gone: its tree cannot be browsed any more.
    if (isUnder(j.root, path)) {
      closeJob(j);
      status(`${msg} — ${t("scanClosedGone")}`, isErr);
      continue;
    }
    if (!isUnder(path, j.root)) continue;
    // The backend took the path out of the tree; cached views are outdated.
    j.cache.clear();
    j.back = j.back.filter((p) => !isUnder(p, path));
    // Stay in the deleted folder's parent if the view was inside it.
    if (isUnder(j.path, path)) j.path = parentPath(path) || j.root;
    if (j === active()) navigate(j, j.path, { keepTab: true, record: false, force: true });
    else j.stale = true;
  }
  loadDrives();
}

function renderOp() {
  const box = $("op");
  if (!op) { box.hidden = true; box.replaceChildren(); return; }
  box.hidden = false;
  box.replaceChildren(
    el("span", { class: "op-text", text: tf("removingProgress", { p: baseName(op.path), n: num(op.files), b: human(op.bytes) }) }),
    btn("stop", op.stopping ? t("cancelling") : t("stop"), {
      class: "small quiet", disabled: op.stopping,
      onclick: () => { op.stopping = true; renderOp(); invoke("cancel", { job: op.id }).catch((e) => status(errText(e), true)); },
    }));
}

// -------------------------------- duplicates --------------------------------
// A group is files with byte-identical content (BLAKE3 over the whole file,
// hard links collapsed) — whatever they are called. Copies inside installed
// programs and games are on purpose (every game ships its own DLLs, some ship
// sound banks twice) and deleting one breaks the program, so groups made only
// of such files are hidden unless asked for, and they are never removed while
// hidden.
function tagGroups(j) {
  if (j.tagged || !j.groups) return;
  for (const g of j.groups) {
    g.prot = g.files.map(isProtectedPath);
    g.allProt = g.prot.every(Boolean);
    g.anyProt = g.prot.some(Boolean);
    g.cat = categoryOf((String(g.files[0]).match(/\.([^.\\/]+)$/) || [])[1] || "");
  }
  j.tagged = true;
}

// Groups that pass the program/game switch, the category and the folder filter.
function baseGroups(j) {
  tagGroups(j);
  return j.groups.filter((g) => (j.dupShowProt || !g.allProt) && (!j.dupCat || g.cat === j.dupCat) &&
    (!j.dupSet || folderSets([g])[0].key === j.dupSet));
}

const DUP_RENDER_CAP = 1000;

function dupesView(j) {
  const groups = j.groups || [];
  const wrap = el("div", { class: "view dupes-view" });
  wrap.append(el("header", { class: "viewhead" },
    el("h1", { class: "view-title" }, icon("copies"), el("span", { text: `${t("dupesIn")} ` }), el("span", { class: "path", text: j.root })),
    el("div", { class: "viewhead-acts" }, btn("refresh", t("rescan"), { "data-fk": "act-rescan", title: "F5", onclick: () => rescan(j) }))));
  if (j.state === "cancelled") {
    wrap.append(banner("warn", `${t("dupesCancelled")} ${t(j.hashesKept ? "dupesHashesKept" : "dupesNothingKept")}`,
      btn("refresh", t("searchAgain"), { class: "small", onclick: () => rescan(j) })));
    return wrap;
  }
  tagGroups(j);
  const own = groups.filter((g) => !g.allProt);
  const prot = groups.filter((g) => g.allProt);
  const sum = (list) => list.reduce((a, g) => a + g.wasted, 0);
  wrap.append(el("dl", { class: "stats" },
    stat(t("ownReclaim"), human(sum(own)), `${num(own.length)} ${t("groups")} · ${num(own.reduce((a, g) => a + g.files.length, 0))} ${t("files")}`),
    j.includeProt
      ? stat(t("protFound"), human(sum(prot)), `${num(prot.length)} ${t("groups")} · ${t(j.dupShowProt ? "protShownSub" : "protHiddenSub")}`,
        { role: "button", tabindex: "0", class: "is-link", "data-fk": "prot-toggle", onclick: () => { j.dupShowProt = !j.dupShowProt; renderMain(); } })
      : stat(t("protSkipped"), "—", t("protSkippedSub"),
        { role: "button", tabindex: "0", class: "is-link", "data-fk": "prot-include", onclick: () => { j.includeProt = true; j.dupShowProt = true; rescan(j); } }),
    stat(t("kScan"), fmtMs(j.elapsedMs || 0), `${t("minSizeUsed")} ${num(j.mb)} MB`)));
  if (!groups.length) {
    wrap.append(el("p", { class: "empty", text: t("noDupes") }));
    return wrap;
  }
  wrap.append(el("p", { class: "muted", text: t("dupHint") }));

  const filter = el("input", { type: "search", class: "filter", value: j.dupFilter || "", placeholder: t("filterFiles"), "aria-label": t("filterFiles"), "data-fk": "dup-filter", oninput: (e) => { j.dupFilter = e.target.value; fill(); } });
  const showProt = el("input", { type: "checkbox", id: "dup-prot", "data-fk": "dup-prot", onchange: (e) => { j.dupShowProt = e.target.checked; renderMain(); } });
  showProt.checked = Boolean(j.dupShowProt);
  const cats = [...new Set(groups.map((g) => g.cat))];
  const sort = selectInput(j.dupSort || "wasted", [["wasted", t("sortWasted")], ["size", t("sortSize")], ["count", t("sortCount")]],
    (val) => { j.dupSort = val; fill(); }, { "data-fk": "dup-sort", id: "dup-sort" });
  const tab = j.dupTab || "groups";
  wrap.append(el("div", { class: "toolbar" },
    tabStrip([["groups", t("tabGroups")], ["sets", t("tabFolderSets")]], tab, (id) => { j.dupTab = id; renderMain(); }, "dtab"),
    el("span", { class: "grow" }),
    j.includeProt ? el("label", { class: "inline-label" }, showProt, t("showProt")) : null,
    selectInput(j.dupCat || "", [["", t("allCategories")], ...cats.map((c) => [c, t(`cat_${c}`)])],
      (val) => { j.dupCat = val; renderMain(); }, { "data-fk": "dup-cat", "aria-label": t("colCategory") }),
    filter,
    el("label", { class: "inline-label", for: "dup-sort" }, t("sort")), sort));

  if (j.dupSet) {
    wrap.append(el("div", { class: "chips" },
      el("span", { class: "chip is-on" }, icon("folder"), `${t("folderFilter")}: ${j.dupSetDirs.map((d) => baseName(d)).join(" ⇄ ")}`,
        btn("close", t("clearFilter"), { class: "chip-x", iconOnly: true, onclick: () => { j.dupSet = null; renderMain(); } }))));
  }

  const list = el("div", { class: "dupes", role: "tabpanel", "aria-labelledby": `dtab-${tab}` });
  wrap.append(list);
  const barText = el("span", { class: "reclaim-text", "aria-live": "polite" });
  const action = selectInput(j.dupAction || "trash", [["trash", t("actTrash")], ["delete", t("actDelete")], ["hardlink", t("actHardlink")]],
    (val) => { j.dupAction = val; fill(); }, { "data-fk": "dup-action", "aria-label": t("reclaimHow") });
  const go = el("button", { type: "button", "data-fk": "dup-go", onclick: () => reclaimShown(j, false) });
  wrap.append(el("div", { class: "reclaim-bar" },
    barText,
    el("span", { class: "grow" }),
    action,
    el("button", { type: "button", class: "quiet", "data-fk": "dup-dry", onclick: () => reclaimShown(j, true) }, t("dryRun")),
    go));

  function fill() {
    const shown = computeShown(j);
    const plan = reclaimPlan(shown, j.keep, j.dupShowProt);
    const hidden = Math.max(0, shown.length - DUP_RENDER_CAP);
    barText.textContent = plan.groups
      ? `${tf("reclaimSummary", { n: num(plan.files), g: num(plan.groups), b: human(plan.bytes) })}${hidden && tab === "groups" ? ` · ${tf("reclaimIncludesHidden", { n: num(hidden) })}` : ""}`
      : t("nothingToReclaim");
    const act = j.dupAction || "trash";
    go.className = act === "trash" ? "primary" : "danger";
    go.disabled = !plan.groups;
    go.textContent = `${t("reclaimShown")} · ${human(plan.bytes)}`;
    const frag = document.createDocumentFragment();
    if (tab === "sets") {
      const sets = folderSets(shown);
      const table = el("table", { class: "table" },
        el("thead", {}, el("tr", {}, el("th", { text: t("colFolders") }), el("th", { class: "num", text: t("groups") }),
          el("th", { class: "num", text: t("colFiles") }), el("th", { class: "num", text: t("reclaimable") }))),
        el("tbody", {}, sets.slice(0, 500).map((s) => el("tr", { class: "is-click" },
          el("td", { class: "cell-name" },
            el("button", { type: "button", class: "row-link", onclick: () => { j.dupSet = s.key; j.dupSetDirs = s.dirs; j.dupTab = "groups"; renderMain(); } },
              el("span", { class: "cell-title", text: s.dirs.length === 1 ? `${baseName(s.dirs[0])} — ${t("sameFolder")}` : s.dirs.map(baseName).join(" ⇄ ") })),
            s.dirs.map((d) => el("span", { class: "cell-path", text: d }))),
          el("td", { class: "num", text: num(s.groups) }),
          el("td", { class: "num", text: num(s.files) }),
          el("td", { class: "num", text: human(s.wasted) })))));
      frag.append(table);
      if (sets.length > 500) frag.append(el("p", { class: "list-more", text: `${t("showing")} 500 / ${num(sets.length)}` }));
    } else {
      for (const g of shown.slice(0, DUP_RENDER_CAP)) {
        const kept = keptPath(j, g);
        const box = el("fieldset", { class: "dup" },
          el("legend", { class: "dup-head" },
            el("span", { class: "dup-count num", text: `${g.files.length}×` }),
            el("span", { class: "num", text: human(g.size) }),
            el("span", { class: "muted", text: t(`cat_${g.cat}`) }),
            g.anyProt ? el("span", { class: "tag is-warn", title: t("protWhy") }, icon("lock"), t("protBadge")) : null,
            el("span", { class: "tag", title: `BLAKE3 ${g.hash}` }, icon("check"), t("identical")),
            el("span", { class: "dup-waste num", text: `${human(g.wasted)} ${t("reclaimable")}` })));
        g.files.forEach((f, i) => {
          const radio = el("input", { type: "radio", name: `keep-${g.hash}`, "aria-label": `${t("keep")} ${f}`, onchange: () => { j.keep.set(g.hash, f); fill(); } });
          if (f === kept) radio.checked = true;
          const goes = f !== kept && (j.dupShowProt || !g.prot[i]);
          box.append(el("label", { class: `dup-file${f === kept ? " is-kept" : goes ? " is-goes" : " is-protected"}` }, radio,
            el("span", { class: "dup-role", text: f === kept ? t("keep") : goes ? t("goes") : t("protKept") }),
            el("span", { class: "with-ic dup-path" }, g.prot[i] ? icon("lock", t("protWhy")) : null, el("span", { class: "path", text: f }))));
        });
        frag.append(box);
      }
      if (shown.length > DUP_RENDER_CAP) frag.append(el("p", { class: "list-more", text: `${t("showing")} ${num(DUP_RENDER_CAP)} / ${num(shown.length)} ${t("groups")} — ${t("reclaimHiddenShort")}` }));
      if (!shown.length) frag.append(el("p", { class: "empty", text: j.dupShowProt ? t("noMatch") : t("noOwnDupes") }));
    }
    list.replaceChildren(frag);
  }
  fill();
  return wrap;
}

function computeShown(j) {
  const filter = (j.dupFilter || "").toLowerCase();
  const sort = j.dupSort || "wasted";
  const groups = baseGroups(j).filter((g) => !filter || g.files.some((f) => f.toLowerCase().includes(filter)));
  groups.sort((a, b) => (sort === "size" ? b.size - a.size : sort === "count" ? b.files.length - a.files.length : b.wasted - a.wasted));
  return groups;
}

// The copy kept by default: a program's own file if the group has one (so
// the user's stray copy is what goes), else the first. Same rule as reclaimPlan.
function keptPath(j, g) {
  const chosen = j.keep.get(g.hash);
  if (chosen && g.files.includes(chosen)) return chosen;
  const i = g.prot ? g.prot.indexOf(true) : -1;
  return g.files[i >= 0 ? i : 0];
}

async function reclaimShown(j, dryRun) {
  const action = j.dupAction || "trash";
  // While program and game files are hidden, none of them is ever removed.
  const shown = computeShown(j);
  const plan = reclaimPlan(shown, j.keep, j.dupShowProt);
  if (!plan.jobs.length) return status(t("nothingToReclaim"));
  const hidden = Math.max(0, shown.length - DUP_RENDER_CAP);
  const label = { trash: t("actTrash"), delete: t("actDelete"), hardlink: t("actHardlink") }[action];
  const warn = [
    { text: tf("reclaimSummary", { n: num(plan.files), g: num(plan.groups), b: human(plan.bytes) }) },
    hidden ? { text: t("reclaimHiddenNote")(num(hidden)), cls: "is-error" } : null,
    action === "trash" ? { text: t("toTrashNote") } : { text: t("irreversible"), cls: "is-error" },
    plan.prot ? { text: t("protWarn"), cls: "is-error" } : null,
  ].filter(Boolean);
  if (!dryRun && !(await ask(t("reclaimConfirm")(label, num(plan.files)), warn, label, action !== "trash" || plan.prot))) return;
  status(dryRun ? `${t("dryRun")}…` : t("deleting"));
  try {
    const rep = await invoke("reclaim_dupes", { jobs: plan.jobs.map(({ keep, remove, size }) => ({ keep, remove, size })), action, dryRun });
    const where = action === "trash" && !rep.dry_run ? ` ${t("toTrashNote")}` : "";
    status(`${rep.dry_run ? t("dryRunResult") : t("reclaimResult")}: ${num(rep.removed)} ${t("files")}, ${human(rep.reclaimed)}${where}` +
      (rep.errors.length ? ` — ${num(rep.errors.length)} ${t("errors")}: ${rep.errors[0]}` : ""), rep.errors.length > 0);
    if (!rep.dry_run) {
      // Drop exactly the copies that were dealt with; stay in this view.
      j.groups = dropReclaimed(j.groups, rep.done);
      for (const [hash, kept] of j.keep) if (!j.groups.some((g) => g.hash === hash && g.files.includes(kept))) j.keep.delete(hash);
      j.tagged = false;
      for (const o of jobs) {
        if (o.kind !== "scan" || !(isUnder(j.root, o.root) || isUnder(o.root, j.root))) continue;
        o.cache.clear(); // the backend took the files out of its tree
        o.stale = true;
      }
      renderFor(j);
      loadDrives();
    }
  } catch (e) {
    status(errText(e), true);
  }
}

// -------------------------------- wiring --------------------------------
async function browse() {
  try {
    const picked = await invoke("pick_folders");
    picked.forEach((p, i) => enqueue("scan", p, { select: i === 0 }));
  } catch (e) {
    status(errText(e), true);
  }
}

$("pathform").addEventListener("submit", (e) => { e.preventDefault(); enqueue("scan", $("path").value); });
$("btn-dupes").addEventListener("click", () => enqueue("dupes", $("path").value.trim() || active()?.path || ""));
$("btn-browse").addEventListener("click", browse);
$("btn-settings").addEventListener("click", () => openSettings());
$("btn-excludes").addEventListener("click", () => openSettings(true));
$("btn-scan-all").addEventListener("click", () => drives.forEach((d, i) => enqueue("scan", d.path, { select: i === 0 })));
$("btn-drives-refresh").addEventListener("click", loadDrives);
$("btn-clear-done").addEventListener("click", () => {
  for (const j of jobs.filter((x) => ["done", "cancelled", "error"].includes(x.state) && !x.loading)) closeJob(j);
});
$("settings-dlg").addEventListener("close", () => renderAll());

// Sidebar job lists: Up/Down move between entries, Delete closes one.
for (const id of ["jobs-scan", "jobs-dupes"]) {
  $(id).addEventListener("keydown", (e) => {
    const cur = e.target.closest?.(".job-open");
    if (!cur) return;
    const all = [...document.querySelectorAll(".job-open")];
    const i = all.indexOf(cur);
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const n = all[i + (e.key === "ArrowDown" ? 1 : -1)];
      if (n) { n.focus(); n.click(); }
    } else if (e.key === "Delete") {
      const j = byId(Number(cur.dataset.job));
      if (j && j.state !== "running") { e.preventDefault(); closeJob(j); document.querySelector(".job-open")?.focus(); }
    }
  });
}

// Live progress for every running job (and a removal); repaint at most once per frame.
let paintQueued = false;
try {
  window.__TAURI__?.event?.listen?.("progress", (e) => {
    const p = e.payload || {};
    if (op && p.job === op.id) {
      op.files = p.files;
      op.bytes = p.bytes;
    } else {
      const j = byId(p.job);
      if (!j || j.state !== "running") return;
      j.files = p.files;
      j.phase = p.phase;
      j.lastPhase = Math.max(j.lastPhase || 0, p.phase || 0);
      j.totalFiles = p.total_files;
      j.totalBytes = p.total_bytes;
      j.bytes = p.bytes;
    }
    if (paintQueued) return;
    paintQueued = true;
    requestAnimationFrame(() => {
      paintQueued = false;
      renderJobs();
      if (op) renderOp();
      const a = active();
      if (a && a.state === "running") updateProgressView(a);
    });
  });
} catch (_) { /* events unavailable */ }

// Drag & drop: every dropped folder becomes its own queued scan.
try {
  const wv = window.__TAURI__?.webview?.getCurrentWebview?.();
  if (wv && wv.onDragDropEvent) {
    wv.onDragDropEvent((e) => {
      const p = e.payload || {};
      if (p.type === "enter" || p.type === "over") $("dropzone").classList.add("is-shown");
      else $("dropzone").classList.remove("is-shown");
      if (p.type === "drop" && p.paths?.length) p.paths.forEach((path, i) => enqueue("scan", path, { select: i === 0 }));
    });
  }
} catch (_) { /* drag-drop unavailable */ }

// Keyboard: back / up without the toolbar (mouse back button, Alt+Left,
// Alt+Up, Backspace outside text fields), Ctrl+L path field, Ctrl+O browse,
// Ctrl+, settings, F5 / Ctrl+R rescan — a reload of the page would silently
// drop every open scan, so it is never let through.
window.addEventListener("mouseup", (e) => {
  if (e.button === 3) { e.preventDefault(); goBack(active()); }
});
document.addEventListener("keydown", (e) => {
  const ctrl = e.ctrlKey || e.metaKey;
  const key = e.key.toLowerCase();
  if (e.key === "F5" || (ctrl && key === "r")) {
    e.preventDefault();
    const j = active();
    if (j && !document.querySelector("dialog[open]")) rescan(j);
    return;
  }
  if (document.querySelector("dialog[open]")) return;
  if (ctrl && key === "l") { e.preventDefault(); $("path").focus(); $("path").select(); return; }
  if (ctrl && key === "o") { e.preventDefault(); browse(); return; }
  if (ctrl && e.key === ",") { e.preventDefault(); openSettings(); return; }
  if (e.target.closest?.("input, textarea, select")) return;
  const j = active();
  if (!j?.view || j.kind !== "scan") return;
  if (e.altKey && e.key === "ArrowLeft") { e.preventDefault(); goBack(j); }
  else if ((e.altKey && e.key === "ArrowUp") || (e.key === "Backspace" && !e.altKey && !ctrl)) { e.preventDefault(); goUp(j); }
});

// A finished scan's snapshot is written in the background; refresh the list.
try {
  window.__TAURI__?.event?.listen?.("snapshot-saved", (e) => {
    const [id, err] = e.payload || [];
    const j = byId(id);
    if (err) status(`${t("snapshotFailed")} ${err}`, true);
    if (!j) return;
    j.history = null;
    if (j === active() && j.tab === "changes") renderMain();
  });
} catch (_) { /* events unavailable */ }

(async function start() {
  Promise.all([invoke("app_version").catch(() => ""), invoke("is_elevated").catch(() => false)]).then(([v, admin]) => {
    elevated = Boolean(admin);
    $("version").textContent = `${v ? `v${v}` : ""}${elevated ? ` · ${t("adminBadge")}` : ""}`;
    adaptToElevation();
    if (!active()) renderMain();
  });
  try { settings = mergeSettings(await invoke("load_settings")); } catch (e) { status(`${t("settingsFailed")} ${errText(e)}`, true); }
  applyTheme();
  applyI18n();
  updateExcludeChip();
  renderAll();
  loadDrives();
})();
