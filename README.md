# Solar

**A fast, local-first photo manager for people who want to own their photos.**
No cloud, no account, no subscription — just your photos, on your machine, the
way Picasa used to feel.

<!--
  ▲ ABOVE THE FOLD: replace this comment with a GIF of the thumbnail grid
  scrolling smoothly through a large library. For a performance-and-feel
  product, the GIF *is* the pitch — it shows what words can't. Drop it at
  docs/media/grid-scroll.gif and uncomment:

  ![Solar — scrolling a large library at 60fps](docs/media/grid-scroll.gif)
-->

---

Solar is an open-source desktop photo library manager in the spirit of Picasa:
fast, opinionated, and respectful of the person using it. It reads your photos
from your own drives, indexes a library of 100,000+ images, and shows them in a
buttery-smooth newest-first timeline. Your originals never move, never upload,
and never get locked into a proprietary format.

It's named for **güey** (*gway*), the Taíno word for the sun. A photograph is
captured light; Solar is where your light lives — on your machine, in your
hands, owned by you. *El sol es Taíno.* See **[NAME.md](NAME.md)**.

> **Status:** v1 MVP — crosses the "daily use" floor. Add your folders, browse a
> newest-first timeline of 100,000+ photos, open any one full-screen, and trust
> that a relaunch shows the truth.

## What it does

- **Add your folders** (multiple); JPEG, HEIC, PNG and WebP are found
  **recursively** and remembered as library roots.
- **A timeline of your whole life in photos** — newest-first by EXIF capture
  date (file mtime fallback), with a right-edge **scrubber** that shows the
  current month as you fly through it.
- **60fps scrolling at 100k+ photos.** The grid is virtualized and streams in
  progressively without ever disturbing your scroll position.
- **Instant repeat launches.** Thumbnails are generated off the UI thread in a
  Rust backend and cached to disk, so the second launch shows your library
  immediately — no "loading your photos" wall.
- **A full-screen viewer.** Click any photo for an upright preview; ←/→ to move
  (neighbors prefetched), Esc to return to your exact place.
- **Cloud-aware, never cloud-dependent.** Cloud-only originals (Proton Drive,
  iCloud "dataless" files) are indexed instantly as placeholders and downloaded
  **on demand** as you scroll to them — never a bulk download — then cached.
- **A library you can trust.** Folders are remembered; every launch quietly
  reconciles with disk (plus a manual **Rescan**), adding new files and pruning
  deleted ones — and never deleting anything if a drive is unreachable.

<!--
  Drop real feature screenshots here once the grid feels great, e.g.:
  ![Face clustering](docs/media/faces.png)
  ![The timeline grid](docs/media/grid.png)
-->

## What it will never do

Stating the limits is part of the promise. **Solar will never:**

- Phone home, send telemetry, or require an account.
- Upload your photos to a cloud or hold them for a subscription.
- Move or modify your originals, or lock them in a proprietary database.
- Become a full RAW darkroom or a professional cataloging suite.

These aren't missing features — they're the point. Staying opinionated is how
this stays good. (See [VISION.md](VISION.md) for *how we decide what not to
build*.)

## Why it exists

For the people currently underserved by Big Photo: the "I don't want my data on
Google or iCloud" crowd, the "I won't pay monthly for storage when I have a
backup drive" crowd, the "I just want to own my photos" crowd. The first user is
the person building it — the floor is that its own maker reaches for it daily.

The full philosophy lives in **[VISION.md](VISION.md)**, and the responsiveness
rules that govern every feature — *the user's place is sacred* — live in
**[PRINCIPLES.md](PRINCIPLES.md)**.

## Install & run

### Prerequisites

- **Node.js** 18+ and npm
- **Rust** (stable) — <https://rustup.rs>
- **libheif** (for HEIC) and **pkg-config**, e.g. on macOS:
  ```sh
  brew install libheif pkg-config
  ```

### Run it (development)

```sh
npm install          # one time: install frontend deps
npm run tauri dev    # launches the desktop app (first build compiles Rust — a few minutes)
```

The first `npm run tauri dev` compiles the whole Rust dependency tree, so it
takes a few minutes. Subsequent launches are fast.

### Install for daily use

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
- **Your library persists.** It lives in app-data (see below), not inside the
  `.app`, so you can rebuild and replace the app anytime without losing anything.
- **Upgrading:** after code changes, `npm run tauri build` again and drag the new
  `Solar.app` over the old one.

> **Portability caveat:** the app links Homebrew's `libheif` at
> `/opt/homebrew/opt/libheif`. It runs fine on the machine you built it on, but
> the `.app` is **not yet self-contained** — copied to a Mac without Homebrew +
> libheif, HEIC support (and possibly launch) would fail. Bundling that dylib is
> a packaging task to do before distributing to others.

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

## Out of scope for v1

RAW formats, cloud sync, plugin systems, and video — see [VISION.md](VISION.md).
These are protected focus, not permanent rejections.
