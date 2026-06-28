# Solar

> A photograph is captured light. **Solar** is where your light lives — on your
> machine, in your hands, owned by you. The name's soul is *güey* (*gway*), the
> Taíno word for the sun. *El sol es Taíno.* See **[NAME.md](NAME.md)**.

A local-first photo library manager in the spirit of Picasa — fast, opinionated,
and respectful of the person using it. Your photos stay on your machine, in your
formats. No account, no cloud, no subscription.

See **[VISION.md](VISION.md)** for the philosophy and **[PRINCIPLES.md](PRINCIPLES.md)**
for the responsiveness rules that govern every feature.

> **Status:** v1 MVP — crosses the "daily use" floor. Add your folders, browse a
> newest-first timeline of 100,000+ photos, open any one full-screen, and trust
> that a relaunch shows the truth.

## What works today

- **Add folders** (multiple); JPEG, HEIC, PNG and WebP are found **recursively**
  and remembered as library roots.
- Thumbnails are generated in the **Rust backend, off the UI thread**, and cached
  as files on disk so a second launch is instant.
- The grid is **virtualized** — only on-screen cells are real DOM nodes — for
  60fps scrolling at large library sizes, and **streams in progressively**
  without disturbing your scroll position (what's on screen is generated first).
- **Chronological timeline:** newest-first by EXIF capture date (file mtime
  fallback), with a right-edge **scrubber** that shows the current month while
  you scroll. EXIF orientation is applied everywhere, so nothing is sideways.
- **Viewer:** click a photo for a full-screen, upright preview; ←/→ to move
  (neighbors prefetched), Esc to return to your exact place.
- **Cloud-aware:** cloud-only originals (e.g. Proton Drive / iCloud "dataless"
  files) are indexed instantly as placeholders and downloaded **on demand** as
  you scroll to them — never a bulk download — then cached forever.
- **A library you can trust:** folders are remembered; every launch quietly
  reconciles with disk (and there's a manual **Rescan**), adding new files and
  pruning deleted ones — never deleting anything if a drive is unreachable.

## Architecture at a glance

| Concern | Where | Notes |
| --- | --- | --- |
| Streaming folder scan | `src-tauri/src/scan.rs` | Metadata only (path/size/mtime); no decode. Detects cloud-only files. Fast at 100k. |
| EXIF capture date | `src-tauri/src/meta.rs` | Reads DateTimeOriginal — only from local files (never forces a cloud download). |
| Database | `src-tauri/src/db.rs` | SQLite (WAL). Source of truth: photos, roots, dates, thumbnail status. |
| Thumbnail + preview pipeline | `src-tauri/src/thumbs.rs` | Priority queue + worker pools (local eager, cloud on-demand); orientation; HEIC via libheif. |
| Wiring / commands / `thumb://` protocol | `src-tauri/src/lib.rs` | Also serves viewer previews and runs the auto-rescan. |
| UI shell | `src/App.tsx` | Toolbar, progress, folders popover, cold-start render. |
| Virtualized grid + scrubber | `src/PhotoGrid.tsx` | `@tanstack/react-virtual`; fixed cells = no reflow; timeline scrubber. |
| Viewer | `src/Lightbox.tsx` | Full-screen preview, keyboard nav, neighbor prefetch. |
| Backend bridge | `src/api.ts` | Typed command + event wrappers. |

**Caches and the library index** live in the OS app-data directory (on macOS
`~/Library/Application Support/com.solarphotos.desktop/`: `library.db`,
`thumbnails/`, `previews/`) — **never** inside your photo folders, so your
originals stay untouched. Because this is keyed by the app's bundle id, your
library persists across launches, rebuilds, and reinstalls. Thumbnails are 256px
JPEGs bucketed into subfolders of ~1000 so no directory ever holds 100k files.

## Prerequisites

- **Node.js** 18+ and npm
- **Rust** (stable) — <https://rustup.rs>
- **libheif** (for HEIC) and **pkg-config**, e.g. on macOS:
  ```sh
  brew install libheif pkg-config
  ```

## Run it

```sh
npm install          # one time: install frontend deps
npm run tauri dev    # launches the desktop app (first build compiles Rust — a few minutes)
```

The first `npm run tauri dev` compiles the whole Rust dependency tree, so it
takes a few minutes. Subsequent launches are fast.

## Install for daily use

For day-to-day use you want the optimized release app, not `tauri dev`:

```sh
npm run tauri build
```

This produces (on Apple Silicon):

- `src-tauri/target/release/bundle/macos/Solar.app` — the app
- `src-tauri/target/release/bundle/dmg/Solar_<version>_aarch64.dmg` — a disk image

Drag `Solar.app` into `/Applications` (or open the `.dmg`). Then:

- **First launch:** it's a self-built, unsigned app, so macOS Gatekeeper will warn
  about an "unidentified developer." Right-click `Solar.app` → **Open** once (or
  System Settings → Privacy & Security → "Open Anyway"). It launches normally
  afterward.
- **Your library persists.** It lives in app-data (see above), not inside the
  `.app`, so you can rebuild and replace the app anytime without losing anything.
- **Upgrading:** after code changes, `npm run tauri build` again and drag the new
  `Solar.app` over the old one.

> **Portability caveat:** the app links Homebrew's `libheif` at
> `/opt/homebrew/opt/libheif`. It runs fine on the machine you built it on, but
> the `.app` is **not yet self-contained** — copied to a Mac without Homebrew +
> libheif, HEIC support (and possibly launch) would fail. Bundling that dylib is
> a packaging task to do before distributing to others.

## Out of scope for v1

RAW formats, cloud sync, plugin systems, and video — see VISION.md. These are
protected focus, not permanent rejections.
