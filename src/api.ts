// Thin typed wrappers around the Rust backend. Everything the UI needs from the
// native side goes through here: commands (request/response) and events (the
// backend pushing "this thumbnail is ready" without the UI having to poll).

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

/** Thumbnail status, mirrored from the Rust side. */
export const STATUS_PENDING = 0;
export const STATUS_READY = 1;
export const STATUS_FAILED = 2;

export interface PhotoRow {
  id: number;
  status: number;
}

export interface LibraryStats {
  total: number;
  ready: number;
}

/** Counts for the whole library — used to render the grid skeleton at once. */
export function getLibraryStats(): Promise<LibraryStats> {
  return invoke("get_library_stats");
}

/** Fetch a contiguous window of photo rows, ordered by path. */
export function getPhotosRange(offset: number, limit: number): Promise<PhotoRow[]> {
  return invoke("get_photos_range", { offset, limit });
}

/** Index a folder. Returns the new library total. Thumbnailing runs after. */
export function scanFolder(path: string): Promise<number> {
  return invoke("scan_folder", { path });
}

/** Tell the backend which photo ids are on screen so they jump the queue. */
export function setVisibleRange(ids: number[]): Promise<void> {
  return invoke("set_visible_range", { ids });
}

/** Subscribe to "thumbnail ready" events. Payload is the photo id. */
export function onThumbReady(cb: (id: number) => void): Promise<UnlistenFn> {
  return listen<number>("thumb-ready", (e) => cb(e.payload));
}

/** Native folder picker. Returns the chosen path, or null if cancelled. */
export async function pickFolder(): Promise<string | null> {
  const selected = await open({ directory: true, multiple: false });
  return typeof selected === "string" ? selected : null;
}

/** The custom-protocol URL an <img> uses to load a cached thumbnail. */
export function thumbUrl(id: number): string {
  return `thumb://localhost/${id}`;
}
