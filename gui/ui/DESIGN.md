# Design — Diskghost GUI

The locked design system of the desktop UI. Every change to `gui/ui` reads
this first. Extend it here; never improvise a value in a rule.

## Genre and shape
- Genre: modern-minimal, technical. A dense file utility: tables, ruled strips
  and one map, not cards of cards.
- App shell (Workbench): top bar (path field + actions) · left sidebar
  (Drives · Scans · Duplicate searches) · main view · status bar.
- One sidebar entry per job. Scans and duplicate searches are listed separately.
- Views mirror the engine's objects:
  - **Scan**: breadcrumb header, honest-state banner, stats strip, map plus
    drive/composition column, then the tabs Folders / Biggest files /
    Extensions / Size ranges / Changes.
  - **Changes**: the snapshot history of the scanned folder. It is a tab,
    because every snapshot command needs an open scan job.
  - **Duplicate search**: stats strip, Groups / Folder pairs, and a sticky
    reclaim bar. The bar states exactly what a reclaim would act on.
  - **Settings**: a modal dialog.

## Tokens (`styles.css` `:root`)
- Colour: OKLCH only. Paper is cool slate (hue 250) in four surface steps.
  Elevation comes from lightness, not shadow.
  - Ink comes in 3 steps.
  - One accent, signal amber (`--color-accent`), used only for: primary
    action, active job, current tab, focus ring, live progress, and "this
    folder" on the drive bar.
  - Data bars are neutral ink.
  - Semantic colours are danger, ok, warn, grow and shrink. They always come
    with a word, sign or icon.
  - The map ramp `--color-map-lo` → `--color-map-hi` is one cool hue.
- Mixing: `color-mix(in oklab, …)`. In oklch, hue interpolation passes
  through green between warm and cool.
- Type: platform UI sans and platform mono. The CSP (`default-src 'self'`)
  allows no web fonts.
  - Mono is for paths, the wordmark and keyboard keys.
  - Scale ≈1.2 over a viewport-tracking root: `html` clamps the root
    16 → 19 px between ~1k and ~2.5k widths, and every rem follows it.
    Token floors at 16 px: `--text-2xs` 12 (charts/tooltip paths),
    `--text-xs` 13 (meta), `--text-sm` 15 (controls, tables), `--text-md`
    16 (body) … `--text-2xl` 30 (figures).
  - All numbers use `font-variant-numeric: tabular-nums`.
- Space: 4 pt scale, `--space-3xs` (2) … `--space-2xl` (64).
- Radius: `--radius-xs` 3 · `sm` 5 · `md` 8 · `lg` 12.
  Controls share one height, `--control-h` (the small voice is
  `--control-h-sm`; no click target goes under ~28 px).
- Motion: `--ease-out`, `--ease-in`, `--ease-in-out`; `--dur-micro` 120 ms,
  `--dur-short` 220 ms.
  - Only functional motion: the progress sweep and the 1 px button press.
  - `prefers-reduced-motion` collapses it.
- Layers: `--z-raised` < `--z-sticky` < `--z-modal` < `--z-tooltip`.
- Theme: dark by default. Light follows `prefers-color-scheme`, or Settings →
  Theme, which sets `:root[data-theme]`. Only lightness and chroma move.

## Components
- Buttons have four voices:
  - default (bordered)
  - `primary` (accent fill, accent-ink text)
  - `danger` (danger outline)
  - `quiet` (borderless)
  
  Size modifiers are `small` and `icon-btn`. An icon-only button always has
  an `aria-label`.
- Icons come only from `icons.js`: a line set drawn on a 16-unit grid in
  `currentColor`, rendered at `--icon` (≥20 px). No emoji
  anywhere in the UI (a unit test enforces this).
- Every share and meter is the one `.bar` primitive, with its width in
  `--w`. `.is-hot` marks danger, `.is-indeterminate` marks unknown totals.
- `.banner.is-warn|is-danger|is-info` carries every honest-state message:
  an icon plus words, with the actions inside the banner.
- `.stats` is one ruled row of figures. A clickable stat is `role=button`;
  `el()` gives it Enter and Space.
- Tabs follow the ARIA tabs pattern: one tab stop, arrows and Home/End move
  and select.
- Tables: sticky headers. Rows show the name in bold plus the full path in
  mono below it. Numbers are right-aligned. Row actions sit last.

## Honesty rules (P0)
- A partial view names every real cause: cancel, boundary cuts, and the
  depth limit the scan ran with. If none is known it says "not reported".
  It never guesses.
- A cancelled duplicate search is never shown as "no duplicates".
- Progress names the real phase (listing / comparing ends / hashing) and its
  real totals.
- "MFT" counts as requested until the result names the engine.
- A failed backend call is shown as a failure, never as an empty state.
- A destructive confirmation states the full count it acts on, including
  rows not rendered.

## States
Every interactive control has these states: default, hover (only under
`@media (hover: hover)`), `:focus-visible` (2 px focus outline, never
animated), active, disabled (opacity plus `not-allowed`), loading (live
progress plus a Stop or Cancel button), and error (a banner or `is-error`
text).

A re-render keeps focus and caret on the same logical control through
`data-fk`.
