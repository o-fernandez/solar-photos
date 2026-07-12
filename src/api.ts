// Thin typed wrappers around the Rust backend. Everything the UI needs from the
// native side goes through here: commands (request/response) and events (the
// backend pushing "this thumbnail is ready" without the UI having to poll).

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";

/** Thumbnail status, mirrored from the Rust side (0 = pending is the implicit
 *  default; the UI only ever branches on the states below). */
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
  /** The favorite star (defaults false for queries that don't select it). */
  favorite?: boolean;
}

export interface LibraryStats {
  total: number;
  ready: number;
  favorites: number;
  hidden: number;
}

/** Which curation slice of the library a grid shows. */
export type PhotoFilter = "visible" | "favorites" | "hidden";

/** Counts for the whole library — used to render the grid skeleton at once. */
export function getLibraryStats(): Promise<LibraryStats> {
  return invoke("get_library_stats");
}

/** Fetch a contiguous window of photo rows. `byDate` = newest-first timeline
 *  order; otherwise discovery order (used while a scan is still running).
 *  `filter` selects the curation slice (default the visible timeline). */
export function getPhotosRange(
  offset: number,
  limit: number,
  byDate: boolean,
  filter: PhotoFilter = "visible",
): Promise<PhotoRow[]> {
  return invoke("get_photos_range", { offset, limit, byDate, filter });
}

/** Photos taken on today's month-and-day in past years — the Home shelf.
 *  Newest first; empty when nothing was captured on this date. */
export function getOnThisDay(): Promise<PhotoRow[]> {
  return invoke("get_on_this_day");
}

/** Toggle a photo's favorite star. */
export function setPhotoFavorite(id: number, favorite: boolean): Promise<void> {
  return invoke("set_photo_favorite", { id, favorite });
}

/** Soft-archive (or restore) a photo — a flag only; the file is untouched. */
export function setPhotoHidden(id: number, hidden: boolean): Promise<void> {
  return invoke("set_photo_hidden", { id, hidden });
}

/** Write favorites + hidden to a JSON file the user picks (curation backup that
 *  outlives the cache dir). Returns how many flagged photos were written. */
export async function exportCuration(): Promise<number | null> {
  const path = await save({
    title: "Export favorites & hidden",
    defaultPath: "solar-curation.json",
    filters: [{ name: "JSON", extensions: ["json"] }],
  });
  if (!path) return null;
  return invoke("export_curation", { path });
}

/** Read a curation file and merge its flags back in by path (OR-merge — never
 *  clears a star). Returns how many entries matched a photo here. */
export async function importCuration(): Promise<number | null> {
  const selected = await open({
    title: "Import favorites & hidden",
    multiple: false,
    filters: [{ name: "JSON", extensions: ["json"] }],
  });
  if (typeof selected !== "string") return null;
  return invoke("import_curation", { path: selected });
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
  /** Full path on disk — backs the viewer's "Show in Finder". */
  path: string;
  timestamp: number;
  favorite: boolean;
  hidden: boolean;
}

/** Filename + path + timestamp for the viewer chrome. */
export function getPhotoDetail(id: number): Promise<PhotoDetail | null> {
  return invoke("get_photo_detail", { id });
}

/** Reveal a photo's file in Finder. */
export function revealInFinder(path: string): Promise<void> {
  return revealItemInDir(path);
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

/** One person-group tile. `cluster_id` is the display-group key: negative for a
 *  durable identity (a person — stable across every pass), positive for an
 *  unassigned appearance cluster (renumbered only by a full re-cluster). Opaque
 *  to the UI beyond that — pass it back to person/merge/name commands as-is. */
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

/** What naming returns: the canonical group key to keep following (naming a
 *  fresh positive group promotes it to a durable NEGATIVE key), plus an undo
 *  token that reverts both the name and the confirmed-face flags it wrote. */
export interface NameOutcome {
  group: number;
  undo: CorrectionUndo;
}

/** Name (or rename, or clear with "") a person-group. Callers keeping the page
 *  open must adopt the returned canonical key. `expectedGeneration` guards a
 *  positive id against a background re-cluster renumbering it between load and
 *  commit — naming confirms every face in the group, so acting on a stale id
 *  would mislabel a stranger durably. */
export function nameCluster(
  clusterId: number,
  name: string,
  expectedGeneration?: number,
): Promise<NameOutcome> {
  return invoke("name_cluster", {
    clusterId,
    name,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** Merge one cluster into another (folds `from`'s faces into `into`).
 *  `expectedGeneration` (from a suggestion payload) lets the backend refuse the
 *  merge if clustering has been renumbered since the card was computed — acting on
 *  a stale card would merge whatever cluster now holds that id. Omit for paths fed
 *  by fresh data (the name typeahead). Resolves to an undo token. */
export function mergeClusters(
  into: number,
  from: number,
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("merge_clusters", { into, from, expectedGeneration: expectedGeneration ?? null });
}

/** One less-certain growth candidate — reviewed on its own as a yes/no chip. */
export interface GrowthCluster {
  cluster_id: number;
  face_id: number | null;
  photos: number;
  similarity: number;
}

export interface IdentityGrowth {
  identity_id: number;
  name: string;
  /** The cluster everything folds into (the person's largest current cluster). */
  into: number;
  /** Example faces of the confirmed person. */
  anchor_faces: number[];
  /** Strong matches, folded in as one bulk merge. */
  strong_clusters: number[];
  /** Per-group chip data for the strong matches (review-queue batch card). */
  strong_groups: GrowthCluster[];
  /** Example faces drawn from the strong matches. */
  strong_faces: number[];
  /** Total photos across the strong matches. */
  strong_photos: number;
  /** The less-certain tail, reviewed one at a time (biggest payoff first). */
  maybe: GrowthCluster[];
  /** Total photos across strong + maybe (for ranking people). */
  photos: number;
  /** Clustering generation this card was computed at — pass back to mutations. */
  generation: number;
}

/** Per confirmed person: the over-split fragments the magnet is confident are the
 *  same person, ready to fold in with one click. */
export function getIdentityGrowth(): Promise<IdentityGrowth[]> {
  return invoke("get_identity_growth");
}

/** One candidate answer on a "Who is this?" card. */
export interface WhoCandidate {
  identity_id: number;
  name: string;
  /** The cluster an "it's them" answer folds the group into. */
  into: number;
  anchor_faces: number[];
  similarity: number;
}

/** One decision in the unified review queue — every suggestion engine normalized
 *  to a single grammar (yes / no / who), sorted biggest-payoff first. */
export type ReviewItem =
  | {
      kind: "strong_batch";
      photos: number;
      name: string;
      into: number;
      anchor_faces: number[];
      groups: GrowthCluster[];
    }
  | {
      kind: "maybe";
      photos: number;
      name: string;
      into: number;
      anchor_faces: number[];
      group: GrowthCluster;
    }
  | {
      kind: "who_is_this";
      photos: number;
      cluster_id: number;
      group_faces: number[];
      candidates: WhoCandidate[];
    }
  | {
      kind: "pairwise";
      photos: number;
      into: number;
      from: number;
      into_name: string | null;
      into_faces: number[];
      from_faces: number[];
    }
  | {
      kind: "same_photo_twin";
      photos: number;
      /** The shared photo; every contested pair in it rides on this one card. */
      photo_id: number;
      pairs: TwinPair[];
    };

/** One contested pair on a same-photo card. */
export interface TwinPair {
  into: number;
  from: number;
  into_name: string | null;
  /** The co-occurring face from each side, cropped from the shared photo. */
  face_a: number;
  face_b: number;
  similarity: number;
  photos: number;
}

export interface ReviewQueue {
  /** Clustering generation the queue was computed at — pass into every action. */
  generation: number;
  items: ReviewItem[];
}

/** The unified review queue (instant — computed when clustering last settled). */
export function getReviewQueue(): Promise<ReviewQueue> {
  return invoke("get_review_queue");
}

/** The current clustering generation, for guarding actions on loaded cluster ids. */
export function getClusterGeneration(): Promise<number> {
  return invoke("get_cluster_generation");
}

/** Focus-review session lifecycle: while active, due re-clusters are deferred so
 *  the session's cards stay valid; ending it runs any deferred pass. */
export function setReviewActive(active: boolean): Promise<void> {
  return invoke("set_review_active", { active });
}

/** Resolve a same-photo contradiction: `samePerson` = it's a collage/mirror
 *  (record durable per-pair exceptions + merge); otherwise they're two
 *  look-alikes — durable cannot-link so they never re-merge. Resolves to an
 *  undo token. */
export function resolveSamePhoto(
  into: number,
  from: number,
  samePerson: boolean,
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("resolve_same_photo", {
    into,
    from,
    samePerson,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** Fold a batch of look-alike clusters into a confirmed person (durable).
 *  Resolves to an undo token. */
export function absorbClusters(
  into: number,
  clusters: number[],
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("absorb_clusters", {
    into,
    clusters,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** "Not the same": record a durable cannot-link so the pair is never re-suggested.
 *  Resolves to an undo token. */
export function rejectMerge(
  into: number,
  from: number,
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("reject_merge", { into, from, expectedGeneration: expectedGeneration ?? null });
}

/** "Not <person>" on a review candidate: make the rejected group a durable competitor
 *  (its own confirmed identity, cannot-linked) so similar faces get pulled toward it
 *  and away from the person — the rejection generalizes. Triggers a re-cluster. */
export function notThisPerson(
  personClusterId: number,
  otherClusterId: number,
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("not_this_person", {
    personClusterId,
    otherClusterId,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** "Not this person" for a whole batch of candidate groups at once (the review
 *  band's "none of these are them") — one undoable action. */
export function notThisPersonMany(
  personClusterId: number,
  otherClusterIds: number[],
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("not_this_person_many", {
    personClusterId,
    otherClusterIds,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** "Someone else" without saying who: the group is none of the offered people.
 *  Cannot-links it from each and confirms it as its own unnamed competitor — it
 *  stops being suggested as any of them, and can be named later in People. */
export function notThesePeople(
  otherClusterId: number,
  personClusterIds: number[],
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("not_these_people", {
    otherClusterId,
    personClusterIds,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** Fast "start people over": clear all names/groups/decisions, keep detected faces,
 *  re-cluster from scratch. Backs up the DB first; resolves to the backup path. */
export function resetFaceDecisions(): Promise<string> {
  return invoke("reset_face_decisions");
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

/** One located photo on the Places map. */
export interface GeoPoint {
  id: number;
  lat: number;
  lon: number;
  ts: number;
}

/** Every photo with a GPS fix — the Places map's whole dataset in one read. */
export function getGeoPoints(): Promise<GeoPoint[]> {
  return invoke("get_geo_points");
}

/** Size of the bundled offline basemap (0 = not bundled — see
 *  scripts/fetch-basemap.sh). */
export function basemapSize(): Promise<number> {
  return invoke("basemap_size");
}

/** One raw byte range of the bundled basemap — the PMTiles reader's transport.
 *  The backend returns raw bytes (`tauri::ipc::Response`); coerce whatever shape
 *  the IPC hands back (ArrayBuffer, typed array, or a JSON byte array) into the
 *  ArrayBuffer the PMTiles reader requires. */
export async function readBasemapRange(offset: number, length: number): Promise<ArrayBuffer> {
  const r = await invoke<unknown>("read_basemap_range", { offset, length });
  if (r instanceof ArrayBuffer) return r;
  if (r instanceof Uint8Array) {
    return r.buffer.slice(r.byteOffset, r.byteOffset + r.byteLength) as ArrayBuffer;
  }
  if (Array.isArray(r)) return new Uint8Array(r).buffer;
  throw new Error(`unexpected basemap response type: ${Object.prototype.toString.call(r)}`);
}

/** The photo a face was cropped from, plus its normalized box within it —
 *  for peeking at the full picture from a review chip or card. */
export interface FacePhoto {
  photo_id: number;
  x1: number;
  y1: number;
  x2: number;
  y2: number;
}

export function getFacePhoto(faceId: number): Promise<FacePhoto | null> {
  return invoke("get_face_photo", { faceId });
}

/** Every photo containing this person, newest first (a filtered timeline). */
export function getPersonPhotos(clusterId: number): Promise<PhotoRow[]> {
  return invoke("get_person_photos", { clusterId });
}

/** A coarse "look" of a person — an appearance sub-cluster of their own faces used
 *  to filter their photos, and (when it matches a different named person) to move a
 *  misclassified batch out. */
export interface PersonLook {
  cover_face_id: number;
  photos: number;
  from_ts: number;
  to_ts: number;
  photo_ids: number[];
  /** Set when this look looks more like a different named person: their name and the
   *  cluster to move the batch into. Absent for a genuine look of this person. */
  likely_other_name: string | null;
  likely_other_cluster: number | null;
}

/** The person's "looks" strip (empty unless there are at least two worth showing). */
export function getPersonLooks(clusterId: number): Promise<PersonLook[]> {
  return invoke("get_person_looks", { clusterId });
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
  identity_id: number | null;
  ignored: boolean;
  confirmed: boolean;
}

/** What a correction returns so it can be undone exactly. */
export interface CorrectionUndo {
  prior: FaceState[];
  /** Set when the correction created a new person (so the UI can focus it). */
  new_cluster_id: number | null;
  added_cannot_link: [number, number] | null;
  /** Multi-pair form — a "neither of them" answer cannot-links each candidate. */
  added_cannot_links?: [number, number][];
  /** Same-photo exceptions added by a "same person — collage" answer. */
  added_same_photo_ok: [number, number][];
  /** A name the action wrote: the identity and its name before (null = unnamed). */
  renamed?: [number, string | null] | null;
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
 *  target and cannot-links from the source so they never re-merge. The generation
 *  guards the *target*: face ids are stable, but a re-cluster renumbers cluster ids,
 *  and confirming faces into whatever cluster now holds a stale id mislabels them. */
export function reassignFacesToCluster(
  faceIds: number[],
  sourceClusterId: number,
  targetClusterId: number,
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("reassign_faces_to_cluster", {
    faceIds,
    sourceClusterId,
    targetClusterId,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** Reassign faces to a brand-new person (optionally named). */
export function reassignFacesToNewPerson(
  faceIds: number[],
  sourceClusterId: number,
  name?: string,
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("reassign_faces_to_new_person", {
    faceIds,
    sourceClusterId,
    name: name ?? null,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** Ignore faces — drop them from People for good. */
export function ignoreFaces(faceIds: number[]): Promise<CorrectionUndo> {
  return invoke("ignore_faces", { faceIds });
}

/** Every face in a cluster (face ids, best first) — the full set behind a "Who is
 *  this?" card, so the split grid can show every contested face. */
export function getClusterFaces(clusterId: number): Promise<number[]> {
  return invoke("get_cluster_faces", { clusterId });
}

/** Name (or fold into the person with this exact name) specific faces only —
 *  never their whole cluster, and no cannot-link. The lightbox's "just this
 *  face" scope, for when the surrounding cluster can't be vouched for. */
export function nameFaces(
  faceIds: number[],
  name: string,
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("name_faces", {
    faceIds,
    name,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** Confirm a subset of faces into an existing person, leaving the rest of the source
 *  cluster untouched. Backs the "Who is this?" split — the user tags some faces as A
 *  and some as B, and each batch is confirmed into that person. No cannot-link against
 *  the (ephemeral, contested) source, so untagged faces aren't stranded. */
export function confirmFacesIntoCluster(
  faceIds: number[],
  targetClusterId: number,
  expectedGeneration?: number,
): Promise<CorrectionUndo> {
  return invoke("confirm_faces_into_cluster", {
    faceIds,
    targetClusterId,
    expectedGeneration: expectedGeneration ?? null,
  });
}

/** "Not this person" without naming who they are: detach the faces and let the
 *  re-cluster re-home each by appearance (they may land in several people, or none).
 *  Unlike a new-person split they aren't forced together; unlike ignore they aren't
 *  hidden. Kicks a re-cluster. */
export function detachFaces(faceIds: number[]): Promise<CorrectionUndo> {
  return invoke("detach_faces", { faceIds });
}

/** Undo any correction with the token it returned. */
export function undoCorrection(undo: CorrectionUndo): Promise<void> {
  return invoke("undo_correction", {
    undo: {
      prior: undo.prior,
      added_cannot_link: undo.added_cannot_link,
      added_cannot_links: undo.added_cannot_links ?? [],
      added_same_photo_ok: undo.added_same_photo_ok ?? [],
      renamed: undo.renamed ?? null,
    },
  });
}
