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
/** Cloud-only original, not downloaded — shown as a placeholder until visited. */
export const STATUS_CLOUD = 3;
/** Cloud original we're fetching now (user scrolled to it). */
export const STATUS_DOWNLOADING = 4;

export interface PhotoRow {
  id: number;
  status: number;
  /** Capture date if known, else file mtime — Unix seconds. Sorts + labels. */
  ts: number;
}

export interface LibraryStats {
  total: number;
  ready: number;
}

/** Counts for the whole library — used to render the grid skeleton at once. */
export function getLibraryStats(): Promise<LibraryStats> {
  return invoke("get_library_stats");
}

/** Fetch a contiguous window of photo rows. `byDate` = newest-first timeline
 *  order; otherwise discovery order (used while a scan is still running). */
export function getPhotosRange(
  offset: number,
  limit: number,
  byDate: boolean,
): Promise<PhotoRow[]> {
  return invoke("get_photos_range", { offset, limit, byDate });
}

/** Add a folder to the library (remembered as a root) and scan it. Returns
 *  immediately; progress streams via events. */
export function addFolder(path: string): Promise<void> {
  return invoke("add_folder", { path });
}

/** Reconcile the whole library with disk in the background (add new, prune gone). */
export function rescan(): Promise<void> {
  return invoke("rescan");
}

/** The folders the library is built from. */
export function listRoots(): Promise<string[]> {
  return invoke("list_roots");
}

/** Remove a folder from the library (drops its photos + cached files). */
export function removeFolder(path: string): Promise<void> {
  return invoke("remove_folder", { path });
}

export interface ScanProgress {
  found: number;
  done: boolean;
}

/** Subscribe to scan progress. Fires per batch as photos are discovered. */
export function onScanProgress(cb: (p: ScanProgress) => void): Promise<UnlistenFn> {
  return listen<ScanProgress>("scan-progress", (e) => cb(e.payload));
}

/** Subscribe to "started downloading these cloud photos" events. */
export function onThumbDownloading(cb: (ids: number[]) => void): Promise<UnlistenFn> {
  return listen<number[]>("thumb-downloading", (e) => cb(e.payload));
}

/** Tell the backend which photo ids are on screen so they jump the queue. */
export function setVisibleRange(ids: number[]): Promise<void> {
  return invoke("set_visible_range", { ids });
}

export interface ThumbDone {
  id: number;
  /** True if a thumbnail now exists; false if the attempt failed/was abandoned. */
  ok: boolean;
}

/** Subscribe to "thumbnail finished" events (success or failure). */
export function onThumbReady(cb: (done: ThumbDone) => void): Promise<UnlistenFn> {
  return listen<ThumbDone>("thumb-ready", (e) => cb(e.payload));
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

/** The custom-protocol URL for a photo's large viewer preview. Requesting it
 *  generates + caches the preview on first view (and downloads cloud originals). */
export function photoUrl(id: number): string {
  return `thumb://localhost/preview/${id}`;
}

export interface PhotoDetail {
  filename: string;
  timestamp: number;
}

/** Filename + timestamp for the viewer chrome. */
export function getPhotoDetail(id: number): Promise<PhotoDetail | null> {
  return invoke("get_photo_detail", { id });
}

export interface FaceProgress {
  scanned: number;
  eligible: number;
}

/** How many local photos have been analyzed for faces so far. */
export function getFaceProgress(): Promise<FaceProgress> {
  return invoke("get_face_progress");
}

/** Subscribe to background face-sweep progress. */
export function onFaceProgress(cb: (p: FaceProgress) => void): Promise<UnlistenFn> {
  return listen<FaceProgress>("faces-progress", (e) => cb(e.payload));
}

/** Pause or resume the background face sweep. */
export function setFacesPaused(paused: boolean): Promise<void> {
  return invoke("set_faces_paused", { paused });
}

export interface Cluster {
  cluster_id: number;
  count: number;
  cover_face_id: number;
  name: string | null;
}

/** The detected people (clusters), biggest first. */
export function getClusters(): Promise<Cluster[]> {
  return invoke("get_clusters");
}

/** Name (or rename, or clear with "") a person-cluster. */
export function nameCluster(clusterId: number, name: string): Promise<void> {
  return invoke("name_cluster", { clusterId, name });
}

/** Merge one cluster into another (folds `from`'s faces into `into`). */
export function mergeClusters(into: number, from: number): Promise<void> {
  return invoke("merge_clusters", { into, from });
}

export interface MergeSuggestion {
  into: number;
  from: number;
  into_cover: number;
  from_cover: number;
  into_name: string | null;
  similarity: number;
}

/** "Same person?" suggestions — likely over-splits to fold together. */
export function getMergeSuggestions(): Promise<MergeSuggestion[]> {
  return invoke("get_merge_suggestions");
}

/** The custom-protocol URL for a face's cover crop. */
export function faceCropUrl(faceId: number): string {
  return `thumb://localhost/face/${faceId}`;
}

/** Every photo containing this person, newest first (a filtered timeline). */
export function getPersonPhotos(clusterId: number): Promise<PhotoRow[]> {
  return invoke("get_person_photos", { clusterId });
}

/** "Not this person": detach their face(s) in one photo. Returns the affected
 *  face ids so the removal can be undone. */
export function removePersonFace(photoId: number, clusterId: number): Promise<number[]> {
  return invoke("remove_person_face", { photoId, clusterId });
}

/** Undo a "not this person": re-attach the given faces to the cluster. */
export function restorePersonFaces(faceIds: number[], clusterId: number): Promise<void> {
  return invoke("restore_person_faces", { faceIds, clusterId });
}
