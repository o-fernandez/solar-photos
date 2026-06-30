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
  /** Example face ids per side (highest confidence) — the merge card's strips. */
  into_faces: number[];
  from_faces: number[];
  into_name: string | null;
  similarity: number;
}

/** "Same person?" suggestions — likely over-splits to fold together. */
export function getMergeSuggestions(): Promise<MergeSuggestion[]> {
  return invoke("get_merge_suggestions");
}

export interface IdentityGrowth {
  identity_id: number;
  name: string;
  /** The cluster everything folds into (the person's largest current cluster). */
  into: number;
  /** Example faces of the confirmed person. */
  anchor_faces: number[];
  /** The look-alike clusters offered for absorption. */
  candidate_clusters: number[];
  /** Example faces drawn from those candidates. */
  candidate_faces: number[];
  /** Total photos across the candidate clusters. */
  photos: number;
}

/** Per confirmed person: the over-split fragments the magnet is confident are the
 *  same person, ready to fold in with one click. */
export function getIdentityGrowth(): Promise<IdentityGrowth[]> {
  return invoke("get_identity_growth");
}

/** Fold a batch of look-alike clusters into a confirmed person (durable). */
export function absorbClusters(into: number, clusters: number[]): Promise<void> {
  return invoke("absorb_clusters", { into, clusters });
}

/** "Not the same": record a durable cannot-link so the pair is never re-suggested. */
export function rejectMerge(into: number, from: number): Promise<void> {
  return invoke("reject_merge", { into, from });
}

/** Wipe all face data and re-scan from scratch (for testing the experience clean). */
export function resetFaceRecognition(): Promise<void> {
  return invoke("reset_face_recognition");
}

/** Rebuild all clusters from scratch (purity-biased) in the background. */
export function recluster(): Promise<void> {
  return invoke("recluster");
}

export interface ClusterProgress {
  running: boolean;
  fraction: number;
}

/** Subscribe to background re-cluster progress. `running` flips false when done,
 *  the cue for People to reload once (never mid-rebuild → no reflow). */
export function onClusterProgress(cb: (p: ClusterProgress) => void): Promise<UnlistenFn> {
  return listen<ClusterProgress>("cluster-progress", (e) => cb(e.payload));
}

/** The custom-protocol URL for a face's cover crop. */
export function faceCropUrl(faceId: number): string {
  return `thumb://localhost/face/${faceId}`;
}

/** Every photo containing this person, newest first (a filtered timeline). */
export function getPersonPhotos(clusterId: number): Promise<PhotoRow[]> {
  return invoke("get_person_photos", { clusterId });
}

// --- Face corrections (reassign / ignore), shared by the person page and the
// in-photo overlay. Every correction returns a CorrectionUndo for exact undo. ---

/** A detected face within one photo, with the person it currently belongs to. */
export interface PhotoFace {
  face_id: number;
  cluster_id: number | null;
  name: string | null;
  x1: number;
  y1: number;
  x2: number;
  y2: number;
}

/** A face's grouping before a correction — opaque to the UI, passed back to undo. */
export interface FaceState {
  face_id: number;
  cluster_id: number | null;
  identity_id: number | null;
  ignored: boolean;
}

/** What a correction returns so it can be undone exactly. */
export interface CorrectionUndo {
  prior: FaceState[];
  /** Set when the correction created a new person (so the UI can focus it). */
  new_cluster_id: number | null;
  added_cannot_link: [number, number] | null;
}

/** Faces detected in one photo (for the in-photo overlay), highest score first. */
export function getFacesInPhoto(photoId: number): Promise<PhotoFace[]> {
  return invoke("get_faces_in_photo", { photoId });
}

/** Resolve a person-page multi-selection (photo ids + the person's cluster) to
 *  the face ids to act on. */
export function faceIdsForPhotos(photoIds: number[], clusterId: number): Promise<number[]> {
  return invoke("face_ids_for_photos", { photoIds, clusterId });
}

/** Reassign faces to an existing person (their cluster). Durable: must-links to the
 *  target and cannot-links from the source so they never re-merge. */
export function reassignFacesToCluster(
  faceIds: number[],
  sourceClusterId: number,
  targetClusterId: number,
): Promise<CorrectionUndo> {
  return invoke("reassign_faces_to_cluster", { faceIds, sourceClusterId, targetClusterId });
}

/** Reassign faces to a brand-new person (optionally named). */
export function reassignFacesToNewPerson(
  faceIds: number[],
  sourceClusterId: number,
  name?: string,
): Promise<CorrectionUndo> {
  return invoke("reassign_faces_to_new_person", {
    faceIds,
    sourceClusterId,
    name: name ?? null,
  });
}

/** Ignore faces — drop them from People for good. */
export function ignoreFaces(faceIds: number[]): Promise<CorrectionUndo> {
  return invoke("ignore_faces", { faceIds });
}

/** Undo any correction with the token it returned. */
export function undoCorrection(undo: CorrectionUndo): Promise<void> {
  return invoke("undo_correction", {
    undo: { prior: undo.prior, added_cannot_link: undo.added_cannot_link },
  });
}
