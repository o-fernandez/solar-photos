# Solar

> A photograph is captured light. **Solar** is where your light lives — on your
> machine, in your hands, owned by you. The name's soul is *güey* (*gway*), the
> Taíno word for the sun. *El sol es Taíno.* See **[NAME.md](NAME.md)**.

A local-first photo library manager in the spirit of Picasa — fast, opinionated,
and respectful of the person using it. Your photos stay on your machine, in your
formats. No account, no cloud, no subscription.

See **[VISION.md](VISION.md)** for the philosophy and **[PRINCIPLES.md](PRINCIPLES.md)**
for the responsiveness rules that govern every feature.

> **Status:** v1, first milestone — a virtualized thumbnail grid that points at a
> local folder and scrolls smoothly through 10,000+ (designed for 100,000+) images.

## What works today

- Pick a folder; JPEG, HEIC, PNG and WebP are found **recursively**.
- Thumbnails are generated in the **Rust backend, off the UI thread**, and cached
  as files on disk so a second launch is instant.
- The grid is **virtualized** — only on-screen cells are real DOM nodes — for
  60fps scrolling at large library sizes.
- Thumbnails **stream in progressively** without disturbing your scroll position;
  whatever is on screen is generated first.

## Architecture at a glance

| Concern | Where | Notes |
| --- | --- | --- |
| Folder scan | `src-tauri/src/scan.rs` | Metadata only (path/size/mtime); no decode. Fast at 100k. |
| Database | `src-tauri/src/db.rs` | SQLite (WAL). Source of truth for *what exists* + thumbnail status. |
| Thumbnail pipeline | `src-tauri/src/thumbs.rs` | Priority queue + worker pool; HEIC via libheif. |
| Wiring / commands / `thumb://` protocol | `src-tauri/src/lib.rs` | |
| UI shell | `src/App.tsx` | Toolbar, progress, cold-start render. |
| Virtualized grid | `src/PhotoGrid.tsx` | `@tanstack/react-virtual`; fixed cells = no reflow. |
| Backend bridge | `src/api.ts` | Typed command + event wrappers. |

**Thumbnail cache** lives in the OS app-data directory (e.g. on macOS
`~/Library/Application Support/com.solarphotos.desktop/`) — **never** inside your
photo folders, so your originals stay untouched. Thumbnails are 256px JPEGs,
bucketed into subfolders of ~1000 so no directory ever holds 100k files.

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

## Build a release binary

```sh
npm run tauri build
```

## Out of scope for v1

RAW formats, cloud sync, plugin systems, and video — see VISION.md. These are
protected focus, not permanent rejections.
