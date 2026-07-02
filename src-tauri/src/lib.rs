//! Solar — Rust backend.
//!
//! Responsibilities split across modules:
//!   * `db`     — SQLite: the list of photos and each thumbnail's status.
//!   * `scan`   — walk a folder and record supported images (cheap, no decode).
//!   * `thumbs` — the priority queue + worker pool that decode & cache thumbs.
//!
//! This file wires those together: it owns shared state, exposes commands the
//! React frontend can call, serves cached thumbnails over a custom `thumb://`
//! URI scheme, and starts the background workers.
//!
//! Two thumbnail pipelines run side by side:
//!   * the **local** queue — many workers, draining local files eagerly;
//!   * the **cloud** queue — a few workers, fed only on demand with the
//!     cloud-only photos the user has scrolled to. We never bulk-download a
//!     cloud library; we fetch-and-thumbnail what's on screen, then cache it
//!     forever.

mod cluster;
mod db;
mod faces;
mod meta;
mod prof;
mod scan;
mod thumbs;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use tauri::{AppHandle, Emitter, Manager};

use db::Job;
use thumbs::ThumbQueue;

/// How many concurrent cloud downloads we allow. Bounded so a slow network
/// can't saturate the machine and so foreground work always has headroom.
const CLOUD_WORKERS: usize = 3;

/// Application-wide state, shared across command handlers and the protocol.
struct AppState {
    /// Path to the SQLite file (the scan thread opens its own connection here).
    db_path: PathBuf,
    /// Directory holding cached thumbnail JPEGs.
    cache_dir: PathBuf,
    /// Directory holding cached large viewer previews.
    preview_dir: PathBuf,
    /// Directory holding cached cover-face crops.
    faces_dir: PathBuf,
    /// A single connection for the (UI-driven) command handlers.
    conn: Mutex<Connection>,
    /// Local-file thumbnail queue (drained eagerly).
    local_queue: Arc<ThumbQueue>,
    /// Cloud-file queue (fed on demand with what's currently visible).
    cloud_queue: Arc<ThumbQueue>,
    /// Guards against two full rescans running at once (e.g. launch + manual).
    rescanning: Arc<AtomicBool>,
    /// When true, the face worker pauses (e.g. user toggled it off).
    faces_paused: Arc<AtomicBool>,
    /// Guards against two re-clusters running at once (migration + manual + sweep).
    reclustering: Arc<AtomicBool>,
    /// Set when a re-cluster is requested while one is already running, so the request
    /// isn't dropped — the running pass re-runs once on finish (e.g. naming several
    /// people in a row each needs its fold applied).
    recluster_pending: Arc<AtomicBool>,
    /// Monotonic clustering generation: bumped whenever a pass rewrites clusters
    /// (re-cluster or auto-fold). Cluster ids are ephemeral — a re-cluster renumbers
    /// them all — so suggestion payloads carry the generation they were computed at,
    /// and suggestion-driven mutations verify it (see `ensure_generation`).
    cluster_gen: Arc<AtomicI64>,
    /// People suggestions computed at the end of the last clustering pass (see
    /// `refresh_suggestion_cache`). The get_* commands read this instantly instead of
    /// recomputing full-library passes per tab-open while holding the DB lock.
    suggestion_cache: Arc<Mutex<SuggestionCache>>,
    /// Debounce token for `schedule_recluster`: only the newest pending request fires.
    recluster_epoch: Arc<AtomicU64>,
}

/// The People suggestions as of one clustering generation. Served only while
/// `generation` still matches `cluster_gen` — a mismatch means clustering moved on,
/// and serving nothing beats serving cards whose cluster ids now point elsewhere.
#[derive(Default)]
struct SuggestionCache {
    generation: i64,
    merges: Vec<MergeSuggestion>,
    growth: Vec<IdentityGrowth>,
    queue: Vec<ReviewItem>,
}

/// A monotonic-ish generation stamp for mark-and-sweep pruning.
fn now_gen() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Drop the calling thread — and every thread it later spawns, including ONNX
/// Runtime's inference pool — to UTILITY QoS on macOS. The webview/UI runs at a
/// higher QoS, so the scheduler always lets the foreground preempt indexing: the
/// background pools still soak up idle cores, but never at the UI's expense. This
/// is how the heavy decode + face work honors "foreground always wins"
/// (PRINCIPLES #1/#3) instead of pegging every core and freezing the window.
pub(crate) fn background_qos() {
    #[cfg(target_os = "macos")]
    {
        // QOS_CLASS_UTILITY = 0x11. `pthread_set_qos_class_self_np` lives in
        // libSystem, which is always linked, so no extra dependency is needed.
        extern "C" {
            fn pthread_set_qos_class_self_np(
                qos_class: std::os::raw::c_uint,
                relative_priority: std::os::raw::c_int,
            ) -> std::os::raw::c_int;
        }
        unsafe {
            pthread_set_qos_class_self_np(0x11, 0);
        }
    }
}

/// Remove a set of photos' cached files (thumbnail + preview) after they've been
/// pruned or their root removed.
fn delete_cache_files(cache_dir: &Path, preview_dir: &Path, ids: &[i64]) {
    for &id in ids {
        let _ = std::fs::remove_file(thumbs::thumb_path(cache_dir, id));
        let _ = std::fs::remove_file(thumbs::preview_path(preview_dir, id));
    }
}

/// Quietly download every cloud-only photo in the background, working through the
/// library in id order so the user doesn't have to scroll everywhere to trigger
/// on-demand fetches. Uses the same cloud queue as the on-demand path — the
/// `set_visible_range` handler promotes currently-visible photos to the priority
/// lane so they always load before the backfill, regardless of queue depth.
///
/// Backfill photos stay STATUS_CLOUD in the DB until the worker finishes and
/// sets STATUS_READY (no DOWNLOADING spinner — less noise for off-screen work).
/// When the pass reaches the end it idles for 60 s then restarts from id 0,
/// picking up any new cloud photos added by a background rescan.
fn spawn_cloud_backfill(db_path: PathBuf, cloud_queue: Arc<ThumbQueue>) {
    const BATCH: i64 = 50;
    const BATCH_PAUSE: std::time::Duration = std::time::Duration::from_secs(3);
    const IDLE_PAUSE: std::time::Duration = std::time::Duration::from_secs(60);

    std::thread::spawn(move || {
        background_qos();
        let mut after_id: i64 = 0;
        loop {
            let batch = match db::open(&db_path).and_then(|c| db::cloud_jobs_after(&c, after_id, BATCH)) {
                Ok(b) => b,
                Err(_) => {
                    std::thread::sleep(IDLE_PAUSE);
                    continue;
                }
            };
            if batch.is_empty() {
                // Reached the end of the library; reset and wait before checking again
                // (a rescan may have added new cloud photos in the meantime).
                after_id = 0;
                std::thread::sleep(IDLE_PAUSE);
            } else {
                after_id = batch.last().map(|j| j.id).unwrap_or(after_id);
                cloud_queue.enqueue(batch);
                std::thread::sleep(BATCH_PAUSE);
            }
        }
    });
}

/// Walk every root, mark what still exists, then prune what doesn't (deleted
/// files, or files under a removed root) — including their cached thumbnails.
/// This is the "second launch shows the truth" reconciliation (Principle 4). It
/// runs in the background and never blocks the UI; a guard prevents overlap.
fn rescan_all(app: AppHandle) {
    let state = app.state::<AppState>();
    if state.rescanning.swap(true, Ordering::SeqCst) {
        return; // a rescan is already in progress
    }
    let db_path = state.db_path.clone();
    let cache_dir = state.cache_dir.clone();
    let preview_dir = state.preview_dir.clone();
    let queue = state.local_queue.clone();
    let rescanning = state.rescanning.clone();
    drop(state);

    std::thread::spawn(move || {
        let gen = now_gen();
        let roots = match db::open(&db_path).and_then(|c| db::list_roots(&c)) {
            Ok(r) => r,
            Err(_) => {
                rescanning.store(false, Ordering::SeqCst);
                return;
            }
        };
        // Track whether every root was actually reachable. If any root is
        // missing (drive unplugged, cloud not mounted) or errored mid-walk, we
        // must NOT prune — otherwise a temporary outage would delete the library.
        let mut all_roots_ok = true;
        for root in &roots {
            if !Path::new(root).exists() {
                all_roots_ok = false;
                continue;
            }
            let app = app.clone();
            if scan::run_scan(&db_path, root, gen, queue.clone(), move |found, _| {
                let _ = app.emit("scan-progress", ScanProgress { found, done: false });
            })
            .is_err()
            {
                all_roots_ok = false;
            }
        }
        // Prune only when we trust the pass was complete.
        if let Ok(conn) = db::open(&db_path) {
            if all_roots_ok {
                let removed = db::take_unseen(&conn, gen).unwrap_or_default();
                let _ = db::delete_faces_for_photos(&conn, &removed);
                delete_cache_files(&cache_dir, &preview_dir, &removed);
            }
            let total = db::stats(&conn).map(|(t, _)| t).unwrap_or(0);
            let _ = app.emit("scan-progress", ScanProgress { found: total, done: true });
        }
        rescanning.store(false, Ordering::SeqCst);
    });
}

/// Library counts for the header readout. On a repeat launch this returns the
/// already-indexed totals immediately, so the grid can render without a rescan.
#[derive(serde::Serialize)]
struct LibraryStats {
    total: i64,
    ready: i64,
}

#[tauri::command]
fn get_library_stats(state: tauri::State<'_, AppState>) -> Result<LibraryStats, String> {
    let conn = state.conn.lock().unwrap();
    let (total, ready) = db::stats(&conn).map_err(|e| e.to_string())?;
    Ok(LibraryStats { total, ready })
}

/// Fetch a contiguous window of photo rows (id + thumbnail status), in discovery
/// order. The virtualized grid asks for only the ranges it is about to display.
#[tauri::command]
fn get_photos_range(
    state: tauri::State<'_, AppState>,
    offset: i64,
    limit: i64,
    by_date: bool,
) -> Result<Vec<db::PhotoRow>, String> {
    let conn = state.conn.lock().unwrap();
    db::photos_range(&conn, offset, limit, by_date).map_err(|e| e.to_string())
}

/// Progress payload for the streaming scan: how many photos are registered so
/// far, and whether the walk has finished.
#[derive(Clone, serde::Serialize)]
struct ScanProgress {
    found: i64,
    done: bool,
}

/// `thumb-ready` payload: which photo finished, and whether a thumbnail now
/// exists (success) or the attempt failed/was abandoned.
#[derive(Clone, serde::Serialize)]
struct ThumbDone {
    id: i64,
    ok: bool,
}

/// Add a folder to the library: remember it as a root and scan it. Returns
/// immediately — the walk runs on a background thread, registering photos in
/// batches and emitting `scan-progress` events so the grid grows live. This is
/// what keeps the UI from freezing on a huge (or cloud-backed) folder (P1).
#[tauri::command]
fn add_folder(app: tauri::AppHandle, state: tauri::State<'_, AppState>, path: String) {
    {
        let conn = state.conn.lock().unwrap();
        let _ = db::add_root(&conn, &path);
    }
    let db_path = state.db_path.clone();
    let queue = state.local_queue.clone();
    std::thread::spawn(move || {
        let gen = now_gen();
        let progress = |found: i64, done: bool| {
            let _ = app.emit("scan-progress", ScanProgress { found, done });
        };
        if let Err(e) = scan::run_scan(&db_path, &path, gen, queue, progress) {
            eprintln!("scan failed: {e}");
            let _ = app.emit("scan-progress", ScanProgress { found: 0, done: true });
        }
    });
}

/// Reconcile the whole library with disk (add new, prune deleted) in the
/// background. Safe to call anytime; overlapping calls are ignored.
#[tauri::command]
fn rescan(app: tauri::AppHandle) {
    rescan_all(app);
}

/// The folders the library is built from.
#[tauri::command]
fn list_roots(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    let conn = state.conn.lock().unwrap();
    db::list_roots(&conn).map_err(|e| e.to_string())
}

/// Remove a folder from the library: drop its photos and their cached files,
/// then tell the frontend the new total so it can refresh.
#[tauri::command]
fn remove_folder(app: tauri::AppHandle, state: tauri::State<'_, AppState>, path: String) {
    let removed = {
        let conn = state.conn.lock().unwrap();
        let ids = db::remove_root(&conn, &path).unwrap_or_default();
        let _ = db::delete_faces_for_photos(&conn, &ids);
        ids
    };
    delete_cache_files(&state.cache_dir, &state.preview_dir, &removed);
    let total = {
        let conn = state.conn.lock().unwrap();
        db::stats(&conn).map(|(t, _)| t).unwrap_or(0)
    };
    let _ = app.emit("scan-progress", ScanProgress { found: total, done: true });
}

/// Detail for the viewer chrome: filename + a timestamp (capture date when we
/// have it, else file mtime).
#[derive(serde::Serialize)]
struct PhotoDetail {
    filename: String,
    timestamp: i64,
}

#[tauri::command]
fn get_photo_detail(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<Option<PhotoDetail>, String> {
    let conn = state.conn.lock().unwrap();
    let detail = db::detail(&conn, id).map_err(|e| e.to_string())?;
    Ok(detail.map(|(path, timestamp)| {
        let filename = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(path);
        PhotoDetail { filename, timestamp }
    }))
}

/// Tell the backend which photos are currently on screen. Two effects:
///   * local pending thumbnails for those photos jump the queue (Principle 3);
///   * cloud-only photos among them are marked DOWNLOADING and promoted to the
///     priority lane of the cloud queue, so visible cloud photos always load ahead
///     of the background backfill working through the rest of the library.
#[tauri::command]
fn set_visible_range(app: tauri::AppHandle, state: tauri::State<'_, AppState>, ids: Vec<i64>) {
    // Prioritize visible local thumbnails (ignores ids not in the local queue).
    state.local_queue.set_priority(ids.clone());

    // Figure out which visible photos are cloud-only.
    let conn = state.conn.lock().unwrap();
    let rows = match db::lookup(&conn, &ids) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut cloud_jobs: Vec<Job> = Vec::new();
    let mut newly_downloading: Vec<i64> = Vec::new();
    for (id, status, path) in rows {
        if status == db::STATUS_CLOUD || status == db::STATUS_DOWNLOADING {
            cloud_jobs.push(Job { id, path });
            if status == db::STATUS_CLOUD {
                newly_downloading.push(id);
            }
        }
    }
    if !newly_downloading.is_empty() {
        let _ = db::set_status_many(&conn, &newly_downloading, db::STATUS_DOWNLOADING);
    }
    drop(conn);

    // Enqueue any not yet in the queue, then bump all visible cloud photos to
    // the priority lane so they load before the background backfill.
    // (Unlike replace_pending, this leaves backfill items in the normal lane.)
    let cloud_ids: Vec<i64> = cloud_jobs.iter().map(|j| j.id).collect();
    state.cloud_queue.enqueue(cloud_jobs);
    state.cloud_queue.set_priority(cloud_ids);
    if !newly_downloading.is_empty() {
        let _ = app.emit("thumb-downloading", newly_downloading);
    }
}

/// Progress of the background face sweep (drives the "Finding people…" readout).
#[derive(Clone, serde::Serialize)]
struct FaceProgress {
    scanned: i64,
    eligible: i64,
}

#[tauri::command]
fn get_face_progress(state: tauri::State<'_, AppState>) -> Result<FaceProgress, String> {
    let conn = state.conn.lock().unwrap();
    let (scanned, eligible) = db::face_progress(&conn).map_err(|e| e.to_string())?;
    Ok(FaceProgress { scanned, eligible })
}

#[tauri::command]
fn set_faces_paused(state: tauri::State<'_, AppState>, paused: bool) {
    state.faces_paused.store(paused, Ordering::Relaxed);
}

/// The detected people (clusters), biggest first, with a cover face each.
#[tauri::command]
fn get_clusters(state: tauri::State<'_, AppState>) -> Result<Vec<db::ClusterRow>, String> {
    let conn = state.conn.lock().unwrap();
    db::clusters_overview(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
fn name_cluster(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
    name: String,
) -> Result<(), String> {
    {
        let conn = state.conn.lock().unwrap();
        db::name_cluster(&conn, cluster_id, &name).map_err(|e| e.to_string())?;
    }
    // Confirming a person adds an exemplar, which can re-home other people's
    // wrongly-folded look-alikes — so re-cluster + re-fold competitively (self-heal).
    if !name.trim().is_empty() {
        schedule_recluster(app);
    }
    Ok(())
}

#[tauri::command]
fn merge_clusters(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    into: i64,
    from: i64,
    expected_generation: Option<i64>,
) -> Result<(), String> {
    ensure_generation(&state, expected_generation)?;
    {
        let conn = state.conn.lock().unwrap();
        // A user merge vouches for the moved faces — confirm them (sticky exemplars).
        db::confirm_cluster_faces(&conn, from).map_err(|e| e.to_string())?;
        db::merge_clusters(&conn, into, from).map_err(|e| e.to_string())?;
    }
    // The merge added exemplars — re-cluster + re-fold competitively (self-heal).
    schedule_recluster(app);
    Ok(())
}

/// Every photo containing this person, newest first (same ordering as the
/// timeline) — backs the person page.
#[tauri::command]
fn get_person_photos(
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
) -> Result<Vec<db::PhotoRow>, String> {
    let conn = state.conn.lock().unwrap();
    db::person_photos(&conn, cluster_id).map_err(|e| e.to_string())
}

/// Intersection-over-union of two face boxes — detects double detections of one
/// face (the only case where two same-photo boxes may be the same person).
fn box_iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let ix = (a.2.min(b.2) - a.0.max(b.0)).max(0.0);
    let iy = (a.3.min(b.3) - a.1.max(b.1)).max(0.0);
    let inter = ix * iy;
    let area_a = (a.2 - a.0).max(0.0) * (a.3 - a.1).max(0.0);
    let area_b = (b.2 - b.0).max(0.0) * (b.3 - b.1).max(0.0);
    let union = (area_a + area_b - inter).max(1e-9);
    inter / union
}

/// Boxes overlapping at least this much are one face detected twice, not two people.
const DOUBLE_DETECTION_IOU: f32 = 0.4;

/// Same-photo constraint data for clustering: `face -> photo` (multi-face photos
/// only — singletons can't conflict) and the double-detection exception pairs.
fn photo_constraints(
    conn: &Connection,
) -> anyhow::Result<(std::collections::HashMap<i64, i64>, std::collections::HashSet<(i64, i64)>)> {
    let rows = db::multi_face_boxes(conn)?; // ordered by photo_id
    let mut photo_of = std::collections::HashMap::new();
    let mut ok = std::collections::HashSet::new();
    let mut i = 0;
    while i < rows.len() {
        let mut j = i;
        while j < rows.len() && rows[j].0 == rows[i].0 {
            j += 1;
        }
        for a in i..j {
            photo_of.insert(rows[a].1, rows[a].0);
            for b in (a + 1)..j {
                let ba = (rows[a].2, rows[a].3, rows[a].4, rows[a].5);
                let bb = (rows[b].2, rows[b].3, rows[b].4, rows[b].5);
                if box_iou(ba, bb) >= DOUBLE_DETECTION_IOU {
                    let (x, y) = (rows[a].1, rows[b].1);
                    ok.insert(if x < y { (x, y) } else { (y, x) });
                }
            }
        }
        i = j;
    }
    Ok((photo_of, ok))
}

/// Per-cluster and per-confirmed-identity photo sets, for the co-occurrence veto:
/// a candidate group photographed *alongside* a person cannot BE that person.
fn cooccurrence_maps(
    conn: &Connection,
) -> anyhow::Result<(
    std::collections::HashMap<i64, std::collections::HashSet<i64>>,
    std::collections::HashMap<i64, std::collections::HashSet<i64>>,
)> {
    let mut cluster_photos: std::collections::HashMap<i64, std::collections::HashSet<i64>> =
        std::collections::HashMap::new();
    for (cid, pid) in db::cluster_photo_pairs(conn)? {
        cluster_photos.entry(cid).or_default().insert(pid);
    }
    let mut identity_photos: std::collections::HashMap<i64, std::collections::HashSet<i64>> =
        std::collections::HashMap::new();
    for (ident, pid) in db::confirmed_identity_photos(conn)? {
        identity_photos.entry(ident).or_default().insert(pid);
    }
    Ok((cluster_photos, identity_photos))
}

/// True if the cluster shares at least one photo with the identity's confirmed
/// faces — they appear together, so they're two different people.
fn cooccurs(
    cluster_photos: &std::collections::HashMap<i64, std::collections::HashSet<i64>>,
    cid: i64,
    identity_photos: &std::collections::HashMap<i64, std::collections::HashSet<i64>>,
    identity: i64,
) -> bool {
    match (cluster_photos.get(&cid), identity_photos.get(&identity)) {
        (Some(cp), Some(ip)) => {
            let (small, big) = if cp.len() <= ip.len() { (cp, ip) } else { (ip, cp) };
            small.iter().any(|p| big.contains(p))
        }
        _ => false,
    }
}

/// L2-normalized mean of a set of embeddings — a robust single-vector summary of a
/// look or an identity's anchor. (Cosine of two of these is their centroid cosine.)
fn mean_normalized(v: &[Vec<f32>]) -> Vec<f32> {
    if v.is_empty() {
        return Vec::new();
    }
    let dim = v[0].len();
    let mut s = vec![0f32; dim];
    for e in v {
        for (k, x) in e.iter().enumerate() {
            s[k] += *x;
        }
    }
    let n = s.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    s.iter().map(|x| x / n).collect()
}

/// Cosine of two already-normalized vectors (a dot product).
fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// One "look" of a person on their page: a coarse appearance sub-cluster of their own
/// faces, used both to filter their photos (baby / kid / adult) and — when the look
/// actually matches a *different* named person — to move a misclassified batch out.
#[derive(Clone, serde::Serialize)]
struct PersonLook {
    /// Representative face (highest detector score in the look).
    cover_face_id: i64,
    /// Distinct photos in this look (drives the count and the grid filter).
    photos: i64,
    from_ts: i64,
    to_ts: i64,
    photo_ids: Vec<i64>,
    /// Set when the look looks more like a different named person than like this one:
    /// their name, and the cluster to move the batch into. The repair suggestion.
    likely_other_name: Option<String>,
    likely_other_cluster: Option<i64>,
}

// Look-grouping tuning. Raw leader-clustering only ("stage 1"): no centroid merge —
// within one person the look centroids sit at 0.70–0.95 cosine (measured on a real
// 5k-face person), so any merge threshold that fuses pose variants also chains every
// era into one blob that the filters then suppress, and the strip shows nothing.
// Raw fine looks with an absolute floor + cap is what actually surfaces "kid Omar".
const LOOK_TAU: f32 = 0.5; // leader-cluster threshold for the fine grouping
const LOOK_ABS_MIN: i64 = 10; // a genuine look needs at least this many photos
                              // (no relative floor: a childhood is a tiny share of a
                              // lifetime library — that's the point of the feature)
const MAX_LOOKS: usize = 8; // genuine looks shown, biggest first (flagged bypass)
const LOOK_FLAG_ABS: f32 = 0.5; // a look must match another anchor at least this well…
const LOOK_FLAG_MARGIN: f32 = 0.08; // …and beat its match to *this* person by this much

/// Group a person's faces into coarse "looks" for the person page: appearance-and-date
/// sub-clusters to filter by, and — where a look matches a different confirmed person
/// better than this one — a one-click "move the batch" repair. Empty (no strip) unless
/// there are at least two looks worth showing.
#[tauri::command]
fn get_person_looks(
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
) -> Result<Vec<PersonLook>, String> {
    let conn = state.conn.lock().unwrap();
    let faces = db::person_faces(&conn, cluster_id).map_err(|e| e.to_string())?;
    if faces.len() < 16 {
        return Ok(Vec::new());
    }
    let embs: Vec<Vec<f32>> = faces.iter().map(|f| f.4.clone()).collect();

    // Fine leader grouping only. No centroid-merge pass: within one person every
    // look is "similar" (they're the same face), so merging chains eras together
    // transitively and collapses the strip to a single suppressed blob.
    let groups = cluster::group_looks(&embs, LOOK_TAU);

    // This person's own reference: their anchor if named (robust to a little pollution),
    // else the dominant look's centroid.
    let own_identity = db::identity_of_cluster(&conn, cluster_id).ok().flatten();
    let centroids: Vec<Vec<f32>> = groups
        .iter()
        .map(|g| mean_normalized(&g.iter().map(|&i| embs[i].clone()).collect::<Vec<_>>()))
        .collect();
    let dominant = groups
        .iter()
        .enumerate()
        .max_by_key(|(_, g)| g.len())
        .map(|(i, _)| i);
    let own_ref: Option<Vec<f32>> = match own_identity {
        Some(id) => {
            let a = db::confirmed_anchor_embeddings(&conn, id, 64).map_err(|e| e.to_string())?;
            if a.is_empty() { dominant.map(|i| centroids[i].clone()) } else { Some(mean_normalized(&anchor_core(a))) }
        }
        None => dominant.map(|i| centroids[i].clone()),
    };

    // Every *other* named person we may flag a look against: enough confirmed evidence
    // to be a trustworthy target (MIN_ANCHOR), and not already declared "not the same"
    // as this person — once you've said Omar isn't Xiao Xiao, we stop suggesting it.
    let blocked: std::collections::HashSet<(i64, i64)> =
        db::cannot_link_pairs(&conn).map_err(|e| e.to_string())?.into_iter().collect();
    struct Other {
        name: String,
        cluster: i64,
        anchor: Vec<f32>,
    }
    let mut others: Vec<Other> = Vec::new();
    for (id, name) in db::named_identities(&conn).map_err(|e| e.to_string())? {
        if Some(id) == own_identity {
            continue;
        }
        if let Some(oid) = own_identity {
            let key = if oid < id { (oid, id) } else { (id, oid) };
            if blocked.contains(&key) {
                continue;
            }
        }
        let a = db::confirmed_anchor_embeddings(&conn, id, 48).map_err(|e| e.to_string())?;
        if a.len() < MIN_ANCHOR {
            continue;
        }
        let cl = db::clusters_of_identity(&conn, id).map_err(|e| e.to_string())?.into_iter().next();
        if let Some(cl) = cl {
            others.push(Other { name, cluster: cl, anchor: mean_normalized(&anchor_core(a)) });
        }
    }

    let mut looks: Vec<PersonLook> = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        let mut photo_set = std::collections::BTreeSet::new();
        let (mut from_ts, mut to_ts) = (i64::MAX, i64::MIN);
        let mut cover = (f32::MIN, faces[g[0]].0);
        for &i in g {
            let f = &faces[i];
            photo_set.insert(f.1);
            from_ts = from_ts.min(f.2);
            to_ts = to_ts.max(f.2);
            if f.3 > cover.0 {
                cover = (f.3, f.0);
            }
        }
        // Does this look match a different confirmed person better than it matches this
        // one (by a clear margin, above an absolute bar)? Then it's likely misclassified.
        let own_sim = own_ref.as_ref().map(|r| cos(&centroids[gi], r)).unwrap_or(1.0);
        let mut flag: Option<(String, i64, f32)> = None;
        for o in &others {
            let s = cos(&centroids[gi], &o.anchor);
            if s >= LOOK_FLAG_ABS
                && s > own_sim + LOOK_FLAG_MARGIN
                && flag.as_ref().map_or(true, |(_, _, bs)| s > *bs)
            {
                flag = Some((o.name.clone(), o.cluster, s));
            }
        }
        // A genuine look shows only if it's substantial (absolute floor). A flagged
        // one shows however big or small — it's a repair prompt, not a filter.
        if flag.is_none() && (photo_set.len() as i64) < LOOK_ABS_MIN {
            continue;
        }
        looks.push(PersonLook {
            cover_face_id: cover.1,
            photos: photo_set.len() as i64,
            from_ts,
            to_ts,
            photo_ids: photo_set.into_iter().collect(),
            likely_other_name: flag.as_ref().map(|(n, _, _)| n.clone()),
            likely_other_cluster: flag.as_ref().map(|(_, c, _)| *c),
        });
    }
    // Genuine looks first (biggest first, capped so the strip stays glanceable),
    // flagged repair looks last (never capped). Only worth a strip with two or more.
    let (mut flagged, mut genuine): (Vec<PersonLook>, Vec<PersonLook>) =
        looks.into_iter().partition(|l| l.likely_other_name.is_some());
    genuine.sort_by(|a, b| b.photos.cmp(&a.photos));
    genuine.truncate(MAX_LOOKS);
    flagged.sort_by(|a, b| b.photos.cmp(&a.photos));
    genuine.extend(flagged);
    if genuine.len() < 2 {
        return Ok(Vec::new());
    }
    Ok(genuine)
}

/// The faces detected in one photo, with the person each belongs to — backs the
/// in-photo overlay (name / reassign / ignore per face).
#[tauri::command]
fn get_faces_in_photo(
    state: tauri::State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<db::PhotoFace>, String> {
    let conn = state.conn.lock().unwrap();
    db::faces_in_photo(&conn, photo_id).map_err(|e| e.to_string())
}

/// Resolve a person-page multi-selection (photo ids + the person's cluster) to the
/// actual face ids, so the frontend can hand them to reassign/ignore.
#[tauri::command]
fn face_ids_for_photos(
    state: tauri::State<'_, AppState>,
    photo_ids: Vec<i64>,
    cluster_id: i64,
) -> Result<Vec<i64>, String> {
    let conn = state.conn.lock().unwrap();
    db::face_ids_in_photos_for_cluster(&conn, &photo_ids, cluster_id).map_err(|e| e.to_string())
}

/// What a correction returns so it can be undone exactly: the faces' prior state,
/// the new cluster id when a new person was created, and any cannot-link we added.
#[derive(Clone, serde::Serialize)]
struct CorrectionUndo {
    prior: Vec<db::FaceState>,
    new_cluster_id: Option<i64>,
    added_cannot_link: Option<(i64, i64)>,
}

/// Reassign faces to an **existing** person (their cluster). Binds them to that
/// person's identity (must-link) and records a cannot-link from the source person,
/// so the move is durable and the two never re-merge (§4/§5 of the spec).
///
/// The generation check matters here even though face ids are stable: the *target*
/// cluster id came from a people list loaded earlier, and a re-cluster in between
/// renumbers ids — binding the faces (confirmed!) to whatever cluster now holds
/// that id would label them as the wrong person.
#[tauri::command]
fn reassign_faces_to_cluster(
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
    source_cluster_id: i64,
    target_cluster_id: i64,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    ensure_generation(&state, expected_generation)?;
    let mut conn = state.conn.lock().unwrap();
    let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
    // Both sides become durable identities; record "not the same" between them.
    let source_id = db::ensure_identity_for_cluster(&conn, source_cluster_id).map_err(|e| e.to_string())?;
    let target_id = db::ensure_identity_for_cluster(&conn, target_cluster_id).map_err(|e| e.to_string())?;
    db::set_faces_person(&mut conn, &face_ids, target_cluster_id, target_id).map_err(|e| e.to_string())?;
    let added = record_cannot_link_if_new(&conn, source_id, target_id).map_err(|e| e.to_string())?;
    Ok(CorrectionUndo { prior, new_cluster_id: None, added_cannot_link: added })
}

/// Reassign faces to a **new** person (an optional name). Splits them into a fresh
/// identity + cluster and cannot-links them from the source person.
#[tauri::command]
fn reassign_faces_to_new_person(
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
    source_cluster_id: i64,
    name: Option<String>,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    ensure_generation(&state, expected_generation)?;
    let mut conn = state.conn.lock().unwrap();
    let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
    let source_id = db::ensure_identity_for_cluster(&conn, source_cluster_id).map_err(|e| e.to_string())?;
    // If the typed name is already a person, merge into them instead of minting a
    // duplicate — moving "this is someone else: Mía" twice shouldn't make two Mías.
    let trimmed = name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(nm) = trimmed {
        if let Some(target) = db::cluster_for_name(&conn, nm).map_err(|e| e.to_string())? {
            if target != source_cluster_id {
                let target_id = db::ensure_identity_for_cluster(&conn, target).map_err(|e| e.to_string())?;
                db::set_faces_person(&mut conn, &face_ids, target, target_id).map_err(|e| e.to_string())?;
                let added = record_cannot_link_if_new(&conn, source_id, target_id).map_err(|e| e.to_string())?;
                return Ok(CorrectionUndo { prior, new_cluster_id: None, added_cannot_link: added });
            }
        }
    }
    let new_cluster = db::next_cluster_id(&conn).map_err(|e| e.to_string())?;
    // Mint the identity on the (empty) new cluster, then bind the faces to both —
    // so the split person is a durable identity that survives the next re-cluster.
    let new_id = db::ensure_identity_for_cluster(&conn, new_cluster).map_err(|e| e.to_string())?;
    db::set_faces_person(&mut conn, &face_ids, new_cluster, new_id).map_err(|e| e.to_string())?;
    if let Some(nm) = trimmed {
        db::name_cluster(&conn, new_cluster, nm).map_err(|e| e.to_string())?;
    }
    let added = record_cannot_link_if_new(&conn, source_id, new_id).map_err(|e| e.to_string())?;
    Ok(CorrectionUndo { prior, new_cluster_id: Some(new_cluster), added_cannot_link: added })
}

/// Ignore faces (drop from People for good). Returns prior state for undo.
#[tauri::command]
fn ignore_faces(
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
) -> Result<CorrectionUndo, String> {
    let conn = state.conn.lock().unwrap();
    let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
    db::ignore_faces(&conn, &face_ids).map_err(|e| e.to_string())?;
    Ok(CorrectionUndo { prior, new_cluster_id: None, added_cannot_link: None })
}

/// "Not this person" without naming who they are: detach the faces from their current
/// person and let each re-home by appearance. Distinct from "move to a new person"
/// (which forces them together) and "ignore" (which hides them) — here they scatter
/// and the re-cluster re-groups them wherever they belong (possibly several people, or
/// none). Kicks a re-cluster so it happens now. Returns prior state for a best-effort
/// undo (a full undo would need the pre-detach clustering, which the re-cluster rewrote).
#[tauri::command]
fn detach_faces(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let mut conn = state.conn.lock().unwrap();
        let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
        let base = db::next_cluster_id(&conn).map_err(|e| e.to_string())?;
        db::detach_faces(&mut conn, &face_ids, base).map_err(|e| e.to_string())?;
        CorrectionUndo { prior, new_cluster_id: None, added_cannot_link: None }
    };
    schedule_recluster(app);
    Ok(undo)
}

/// Undo any correction: restore the faces' prior grouping and drop any cannot-link
/// the correction added.
#[tauri::command]
fn undo_correction(
    state: tauri::State<'_, AppState>,
    undo: CorrectionUndoArg,
) -> Result<(), String> {
    let mut conn = state.conn.lock().unwrap();
    db::restore_face_states(&mut conn, &undo.prior).map_err(|e| e.to_string())?;
    if let Some((a, b)) = undo.added_cannot_link {
        db::remove_cannot_link(&conn, a, b).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Inbound form of [`CorrectionUndo`] (the frontend hands back what a correction
/// returned). `new_cluster_id` isn't needed to undo, so it's omitted.
#[derive(serde::Deserialize)]
struct CorrectionUndoArg {
    prior: Vec<db::FaceState>,
    added_cannot_link: Option<(i64, i64)>,
}

/// Record a cannot-link between two identities unless it already exists or they're
/// the same identity. Returns the pair when newly added (so undo can remove it).
fn record_cannot_link_if_new(
    conn: &rusqlite::Connection,
    a: i64,
    b: i64,
) -> anyhow::Result<Option<(i64, i64)>> {
    if a == b || db::cannot_link_exists(conn, a, b)? {
        return Ok(None);
    }
    db::add_cannot_link_ids(conn, a, b)?;
    Ok(Some((a, b)))
}

/// A "same person?" suggestion: two clusters with several near-neighbor face pairs
/// across them (face-to-face evidence, not centroid angles). The card shows a strip
/// of example faces from each side so one glance decides.
#[derive(Clone, serde::Serialize)]
struct MergeSuggestion {
    into: i64,
    from: i64,
    /// Example face ids from each side (highest detector confidence), for the card.
    into_faces: Vec<i64>,
    from_faces: Vec<i64>,
    into_name: Option<String>,
    similarity: f32,
    /// Faces on the smaller side — the payoff of resolving this suggestion.
    photos: i64,
    /// Clustering generation this card was computed at (checked by mutations).
    generation: i64,
}

/// Find likely over-splits from **face-to-face** evidence: cluster pairs with at
/// least a few cross-cluster face pairs above the suggestion threshold (see
/// `cluster::merge_evidence`). Ranked by leverage — strength × combined size —
/// so the most worthwhile, most confident merges come first. The larger cluster
/// is the "into" side, so merging folds the small group into the person.
///
/// Heavy (a kNN pass over every clustered face) — runs only from the background
/// cache refresh at the end of a clustering pass, never from a UI command. Empty
/// until the sweep has settled: no prompts off half-built clusters.
fn compute_merge_suggestions(conn: &Connection) -> anyhow::Result<Vec<MergeSuggestion>> {
    match db::face_progress(conn)? {
        (scanned, eligible) if eligible > 0 && scanned >= eligible => {}
        _ => return Ok(Vec::new()),
    }
    let overview = db::clusters_overview(conn)?;
    let faces = db::face_cluster_embeddings(conn)?;
    // Declared "not the same" identity pairs — suggestions that name them are skipped.
    let blocked: std::collections::HashSet<(i64, i64)> =
        db::cannot_link_pairs(conn)?.into_iter().collect();

    use std::collections::HashMap;
    let info: HashMap<i64, &db::ClusterRow> = overview.iter().map(|c| (c.cluster_id, c)).collect();

    let evidence = cluster::merge_evidence(&faces);
    // Rank by leverage: evidence strength × impact. Strength is the best cross-pair
    // similarity weighted by how many pairs corroborate it; impact is the combined
    // size (sqrt-damped so a few huge clusters don't crowd out confident small ones).
    let mut ranked: Vec<(cluster::PairEvidence, f32)> = evidence
        .into_iter()
        .filter_map(|e| {
            let (ca, cb) = (info.get(&e.a)?, info.get(&e.b)?);
            let leverage = e.max_sim * e.pairs as f32 * ((ca.count + cb.count) as f32).sqrt();
            Some((e, leverage))
        })
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    ranked.truncate(20);

    // Two clusters that appear in the same photo are two people — never suggest them.
    let mut cluster_photos: std::collections::HashMap<i64, std::collections::HashSet<i64>> =
        std::collections::HashMap::new();
    for (cid, pid) in db::cluster_photo_pairs(conn)? {
        cluster_photos.entry(cid).or_default().insert(pid);
    }
    let share_photo = |a: i64, b: i64| -> bool {
        match (cluster_photos.get(&a), cluster_photos.get(&b)) {
            (Some(pa), Some(pb)) => {
                let (small, big) = if pa.len() <= pb.len() { (pa, pb) } else { (pb, pa) };
                small.iter().any(|p| big.contains(p))
            }
            _ => false,
        }
    };

    let mut out = Vec::with_capacity(ranked.len());
    for (e, _) in ranked {
        if share_photo(e.a, e.b) {
            continue;
        }
        let (big, small) = {
            let (ca, cb) = (info[&e.a], info[&e.b]);
            if ca.count >= cb.count { (ca, cb) } else { (cb, ca) }
        };
        // Skip a pair the user has already declared "not the same".
        if let (Ok(Some(ia)), Ok(Some(ib))) = (
            db::identity_of_cluster(conn, big.cluster_id),
            db::identity_of_cluster(conn, small.cluster_id),
        ) {
            let key = if ia < ib { (ia, ib) } else { (ib, ia) };
            if blocked.contains(&key) {
                continue;
            }
        }
        out.push(MergeSuggestion {
            into: big.cluster_id,
            from: small.cluster_id,
            into_faces: db::top_face_ids(conn, big.cluster_id, 4).unwrap_or_default(),
            from_faces: db::top_face_ids(conn, small.cluster_id, 4).unwrap_or_default(),
            into_name: big.name.clone(),
            similarity: e.max_sim,
            photos: small.count,
            generation: 0, // stamped by refresh_suggestion_cache
        });
    }
    Ok(out)
}

/// The cached "same person?" suggestions from the last clustering pass. Instant —
/// the heavy pass ran in the background when clustering settled. Empty while a
/// pass is running or the cache is from an older generation (no stale cards).
#[tauri::command]
fn get_merge_suggestions(state: tauri::State<'_, AppState>) -> Result<Vec<MergeSuggestion>, String> {
    if state.reclustering.load(Ordering::SeqCst) {
        return Ok(Vec::new());
    }
    let cache = state.suggestion_cache.lock().unwrap();
    if cache.generation == state.cluster_gen.load(Ordering::SeqCst) {
        Ok(cache.merges.clone())
    } else {
        Ok(Vec::new())
    }
}

/// A single less-certain growth candidate, reviewed on its own in the card's tail.
/// Carries its own example face and photo count so it renders as one yes/no chip.
#[derive(Clone, serde::Serialize)]
struct GrowthCluster {
    cluster_id: i64,
    face_id: Option<i64>,
    photos: i64,
    similarity: f32,
}

/// One candidate answer on a "Who is this?" card.
#[derive(Clone, serde::Serialize)]
struct WhoCandidate {
    identity_id: i64,
    name: String,
    /// The cluster an "it's them" answer folds the group into.
    into: i64,
    anchor_faces: Vec<i64>,
    similarity: f32,
}

/// One decision in the unified review queue (the focus-mode flow). Every engine's
/// output — strong batches, uncertain growth, contested clusters, pairwise
/// evidence — is normalized to this shape and sorted by payoff (photos), so the
/// user answers the biggest questions first with one grammar: yes / no / who.
#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReviewItem {
    /// "N groups strongly match <name>" — bulk-mergeable, with per-group verbs.
    StrongBatch {
        photos: i64,
        name: String,
        into: i64,
        anchor_faces: Vec<i64>,
        groups: Vec<GrowthCluster>,
    },
    /// "Might also be <name>?" — one group, yes / no / someone else.
    Maybe {
        photos: i64,
        name: String,
        into: i64,
        anchor_faces: Vec<i64>,
        group: GrowthCluster,
    },
    /// Two+ named people both plausibly match this group — the near-tie the model
    /// can't resolve (babies). One answer teaches the winner *and* the loser, the
    /// highest information-per-click question in the app.
    WhoIsThis {
        photos: i64,
        cluster_id: i64,
        group_faces: Vec<i64>,
        candidates: Vec<WhoCandidate>,
    },
    /// "Same person?" — face-to-face pairwise evidence between two clusters.
    Pairwise {
        photos: i64,
        into: i64,
        from: i64,
        into_name: Option<String>,
        into_faces: Vec<i64>,
        from_faces: Vec<i64>,
    },
}

/// The review queue as of one clustering generation. All items share the payload's
/// generation — mutations pass it back so stale answers are refused.
#[derive(Clone, serde::Serialize, Default)]
struct ReviewQueue {
    generation: i64,
    items: Vec<ReviewItem>,
}

/// A batch offer from a confirmed person, split by confidence. The `strong` matches
/// ("N groups are a strong match for <name>") fold in with one bulk click; the
/// less-certain `maybe` tail is reviewed one face at a time. That tail is exactly
/// where infants land — the model barely separates babies, so their look-alike
/// groups clear the linkage floor but not the strong bar — which is why the whole
/// point of the split is to keep a human glance on the risky few, not the safe many.
#[derive(Clone, serde::Serialize)]
struct IdentityGrowth {
    identity_id: i64,
    name: String,
    /// The cluster everything folds into (the identity's largest current cluster).
    into: i64,
    /// Example faces of the confirmed person, for the card.
    anchor_faces: Vec<i64>,
    /// Strong matches, offered as a single bulk merge.
    strong_clusters: Vec<i64>,
    /// Per-group chip data for the strong matches (review-queue batch card).
    strong_groups: Vec<GrowthCluster>,
    /// Example faces drawn from the strong matches, for the card strip.
    strong_faces: Vec<i64>,
    /// Total photos across the strong matches.
    strong_photos: i64,
    /// The less-certain tail, each reviewed individually.
    maybe: Vec<GrowthCluster>,
    /// Total photos across strong + maybe (ranks the most impactful person first).
    photos: i64,
    /// Clustering generation this card was computed at (checked by mutations).
    generation: i64,
}

/// For each named person, find the over-split fragments the magnet is confident are
/// the same person (see `cluster::identity_candidates`). Anchored to the confirmed
/// identity and filtered by "not the same" — never free-chaining — so a single
/// click can reunite a person scattered across dozens of clusters. Gated on a
/// settled state, like the pairwise suggestions.
///
/// Because each identity's magnet is computed independently, the *same* look-alike
/// cluster can clear the bar against two different anchors — most visibly with
/// infants, whose embeddings the model barely separates (two babies both matching
/// each other's parent's anchor). Blanket "Merge all" would then silently hand that
/// cluster to whichever card was clicked first, writing a durable must-link. So we
/// run a two-pass conflict guard: gather every identity's candidates, then drop any
/// cluster claimed by more than one identity from *all* growth cards. Those
/// ambiguous groups aren't lost — they stay reachable through the reviewable
/// pairwise "same person?" path, where you decide one at a time.
///
/// Heavy (a full-library pass per confirmed identity) — runs only from the
/// background cache refresh, never from a UI command; the old per-tab-open compute
/// held the shared DB lock through seconds of matrix math, stalling every avatar.
///
/// Also returns the "Who is this?" review items: the clusters *dropped* from the
/// growth cards because two or more named people claim them. Those near-ties used
/// to fall silently into limbo; now they're the queue's best question.
fn compute_identity_growth(
    conn: &Connection,
) -> anyhow::Result<(Vec<IdentityGrowth>, Vec<ReviewItem>)> {
    match db::face_progress(conn)? {
        (scanned, eligible) if eligible > 0 && scanned >= eligible => {}
        _ => return Ok((Vec::new(), Vec::new())),
    }
    let named = db::named_identities(conn)?;
    if named.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let all_faces = db::face_cluster_embeddings(conn)?;
    let blocked: std::collections::HashSet<(i64, i64)> =
        db::cannot_link_pairs(conn)?.into_iter().collect();
    // How well every cluster matches each confirmed identity (incl. "not X" splits) —
    // so we don't suggest a cluster that's decisively someone else's.
    let matches = cluster_identity_matches(conn)?;
    // Clusters holding anyone's *confirmed* faces are never growth candidates. An
    // identity's own clusters are already excluded below, so anything left in this
    // set belongs to a different person (or a "not X" competitor) — offering it as
    // a bulk merge is how Camila's whole cluster once became "5 strong matches for
    // Mía" and got absorbed. Merging two named people stays possible, but only via
    // the explicit rename/typeahead path, never a one-click suggestion.
    let confirmed_clusters: std::collections::HashSet<i64> =
        db::confirmed_clusters(conn)?.into_iter().collect();
    // Co-occurrence veto for candidates (see auto_fold_confident).
    let (cluster_photos, identity_photos) = cooccurrence_maps(conn)?;

    // Pass 1: gather each identity's candidate clusters (already filtered by
    // "not the same"), and tally how many distinct identities claim each cluster.
    use std::collections::HashMap;
    struct Pending {
        identity_id: i64,
        name: String,
        into: i64,
        candidates: Vec<(i64, i64, f32)>, // strongest-first (cluster_id, size, max_sim)
    }
    let mut pending: Vec<Pending> = Vec::new();
    // cluster_id -> the identities claiming it (with match strength) + its size.
    let mut claims: HashMap<i64, Vec<(i64, f32)>> = HashMap::new();
    let mut claim_size: HashMap<i64, i64> = HashMap::new();
    for (identity_id, name) in named {
        let anchor = db::confirmed_anchor_embeddings(conn, identity_id, 64)?;
        if anchor.len() < MIN_ANCHOR {
            continue; // too little confirmed evidence to suggest look-alikes yet
        }
        // Match against the anchor's dominant core, not a possibly-polluted full set.
        let core = anchor_core(anchor);
        // The clusters this identity already occupies are excluded from the search;
        // the largest is the fold-in target.
        let own: Vec<i64> = db::clusters_of_identity(conn, identity_id)?;
        let into = match own.first() {
            Some(&c) => c,
            None => continue,
        };
        let own_set: std::collections::HashSet<i64> = own.iter().cloned().collect();
        let others: Vec<(i64, i64, Vec<f32>)> = all_faces
            .iter()
            .filter(|(_, c, _)| !own_set.contains(c))
            .cloned()
            .collect();

        let mut cands = cluster::identity_candidates(&core, &others);
        // Strongest matches first, and drop any cluster the user said isn't this person.
        cands.sort_by(|a, b| b.max_sim.partial_cmp(&a.max_sim).unwrap());
        let mut candidates = Vec::new();
        for c in cands {
            if confirmed_clusters.contains(&c.cluster_id) {
                continue; // someone's confirmed group — never offered for absorption
            }
            if cooccurs(&cluster_photos, c.cluster_id, &identity_photos, identity_id) {
                continue; // photographed together — cannot be this person
            }
            if let Ok(Some(other_id)) = db::identity_of_cluster(conn, c.cluster_id) {
                let key = if identity_id < other_id { (identity_id, other_id) } else { (other_id, identity_id) };
                if blocked.contains(&key) {
                    continue;
                }
            }
            // Competitive: skip a cluster a confirmed competitor matches decisively
            // better — it's someone else's, so don't keep offering it as this person.
            if let Some(ms) = matches.get(&c.cluster_id) {
                let best_other = ms
                    .iter()
                    .filter(|(id, _)| *id != identity_id)
                    .map(|(_, s)| *s)
                    .fold(f32::MIN, f32::max);
                if best_other > c.max_sim + AUTO_FOLD_MARGIN {
                    continue;
                }
            }
            claims.entry(c.cluster_id).or_default().push((identity_id, c.max_sim));
            claim_size.insert(c.cluster_id, c.size as i64);
            candidates.push((c.cluster_id, c.size as i64, c.max_sim));
        }
        if candidates.is_empty() {
            continue;
        }
        pending.push(Pending { identity_id, name, into, candidates });
    }

    // Pass 2: build the cards, excluding any cluster claimed by 2+ identities, and
    // split the survivors by confidence. Above STRONG the match is folded in by the
    // bulk button; below it (but still past the linkage floor `identity_candidates`
    // enforced) the cluster goes to the reviewable tail, ranked by payoff and capped
    // so the chip row stays glanceable.
    const STRONG: f32 = 0.6;
    const MAX_MAYBE: usize = 12;
    // Identity display info for the who-is-this cards, captured before pass 2
    // consumes `pending`.
    let ident_info: HashMap<i64, (String, i64)> =
        pending.iter().map(|p| (p.identity_id, (p.name.clone(), p.into))).collect();
    let mut out = Vec::new();
    for p in pending {
        let mut strong_clusters = Vec::new();
        let mut strong_groups: Vec<GrowthCluster> = Vec::new();
        let mut strong_faces = Vec::new();
        let mut strong_photos: i64 = 0;
        let mut maybe: Vec<GrowthCluster> = Vec::new();
        let mut photos: i64 = 0;
        for (cid, size, sim) in p.candidates {
            if claims.get(&cid).map_or(0, |v| v.len()) > 1 {
                continue; // contested between people — becomes a who-is-this card
            }
            photos += size;
            if sim >= STRONG {
                strong_clusters.push(cid);
                let face_id = db::top_face_ids(conn, cid, 1).ok().and_then(|v| v.into_iter().next());
                strong_groups.push(GrowthCluster { cluster_id: cid, face_id, photos: size, similarity: sim });
                strong_photos += size;
                if strong_faces.len() < 4 {
                    if let Some(f) = face_id {
                        strong_faces.push(f);
                    }
                }
            } else {
                let face_id = db::top_face_ids(conn, cid, 1).ok().and_then(|v| v.into_iter().next());
                maybe.push(GrowthCluster { cluster_id: cid, face_id, photos: size, similarity: sim });
            }
        }
        if strong_clusters.is_empty() && maybe.is_empty() {
            continue;
        }
        // The review tail is ranked by payoff (photos), not similarity — a glance
        // costs the same for a 1-photo fragment as for a 40-photo group, and
        // similarity-ordering let high-sim singletons crowd big clusters out of
        // the cap (the "twelve 1-photo chips" screenshot). Cap after sorting so
        // the biggest candidates always make the strip.
        maybe.sort_by(|a, b| b.photos.cmp(&a.photos));
        maybe.truncate(MAX_MAYBE);
        out.push(IdentityGrowth {
            identity_id: p.identity_id,
            name: p.name,
            into: p.into,
            anchor_faces: db::top_face_ids(conn, p.into, 4).unwrap_or_default(),
            strong_clusters,
            strong_groups,
            strong_faces,
            strong_photos,
            maybe,
            photos,
            generation: 0, // stamped by refresh_suggestion_cache
        });
    }
    // Most impactful person first.
    out.sort_by(|a, b| b.photos.cmp(&a.photos));

    // The contested clusters (claimed by 2+ named people) become who-is-this cards.
    let mut who: Vec<ReviewItem> = Vec::new();
    for (cid, claimants) in &claims {
        if claimants.len() < 2 {
            continue;
        }
        let mut cands: Vec<WhoCandidate> = claimants
            .iter()
            .filter_map(|(id, sim)| {
                ident_info.get(id).map(|(name, into)| WhoCandidate {
                    identity_id: *id,
                    name: name.clone(),
                    into: *into,
                    anchor_faces: db::top_face_ids(conn, *into, 2).unwrap_or_default(),
                    similarity: *sim,
                })
            })
            .collect();
        if cands.len() < 2 {
            continue;
        }
        cands.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
        cands.truncate(3);
        who.push(ReviewItem::WhoIsThis {
            photos: claim_size.get(cid).copied().unwrap_or(0),
            cluster_id: *cid,
            group_faces: db::top_face_ids(conn, *cid, 3).unwrap_or_default(),
            candidates: cands,
        });
    }
    Ok((out, who))
}

/// The cached growth cards from the last clustering pass — see
/// [`get_merge_suggestions`] for the caching rationale.
#[tauri::command]
fn get_identity_growth(state: tauri::State<'_, AppState>) -> Result<Vec<IdentityGrowth>, String> {
    if state.reclustering.load(Ordering::SeqCst) {
        return Ok(Vec::new());
    }
    let cache = state.suggestion_cache.lock().unwrap();
    if cache.generation == state.cluster_gen.load(Ordering::SeqCst) {
        Ok(cache.growth.clone())
    } else {
        Ok(Vec::new())
    }
}

/// The unified review queue from the last clustering pass — the focus flow's feed.
#[tauri::command]
fn get_review_queue(state: tauri::State<'_, AppState>) -> Result<ReviewQueue, String> {
    if state.reclustering.load(Ordering::SeqCst) {
        return Ok(ReviewQueue::default());
    }
    let cache = state.suggestion_cache.lock().unwrap();
    if cache.generation == state.cluster_gen.load(Ordering::SeqCst) {
        Ok(ReviewQueue { generation: cache.generation, items: cache.queue.clone() })
    } else {
        Ok(ReviewQueue::default())
    }
}

/// Fold a batch of look-alike clusters into a confirmed person in one action (the
/// "merge all" button). Each absorb writes the durable must-link, so the whole
/// person stays together through future re-clusters.
#[tauri::command]
fn absorb_clusters(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    into: i64,
    clusters: Vec<i64>,
    expected_generation: Option<i64>,
) -> Result<(), String> {
    ensure_generation(&state, expected_generation)?;
    {
        let conn = state.conn.lock().unwrap();
        let into_identity = db::identity_of_cluster(&conn, into).map_err(|e| e.to_string())?;
        for from in clusters {
            if from == into {
                continue;
            }
            // Defense in depth (the suggestion pass already filters these): never
            // absorb a group holding a different person's confirmed faces.
            if db::cluster_has_foreign_confirmed(&conn, from, into_identity)
                .map_err(|e| e.to_string())?
            {
                continue;
            }
            // The user vouched for each absorbed group — confirm before folding in.
            db::confirm_cluster_faces(&conn, from).map_err(|e| e.to_string())?;
            db::merge_clusters(&conn, into, from).map_err(|e| e.to_string())?;
        }
    }
    // Bulk-merging added exemplars — re-cluster + re-fold competitively (self-heal).
    schedule_recluster(app);
    Ok(())
}

/// "Not the same" on a merge prompt: record a durable cannot-link so the pair is
/// never suggested again (survives re-clusters, unlike a dismissed-in-memory card).
#[tauri::command]
fn reject_merge(
    state: tauri::State<'_, AppState>,
    into: i64,
    from: i64,
    expected_generation: Option<i64>,
) -> Result<(), String> {
    ensure_generation(&state, expected_generation)?;
    let conn = state.conn.lock().unwrap();
    db::add_cannot_link(&conn, into, from).map_err(|e| e.to_string())
}

/// "Not <person>" on a review candidate: instead of a weak, per-group cannot-link, make
/// the rejected group a *durable competitor* — confirm its faces as their own identity
/// (an unnamed "someone else") and cannot-link it from the person. Because confirmed
/// identities compete for faces, this generalizes: other look-alikes now get pulled
/// toward the competitor and away from the person. Re-cluster so it takes effect.
#[tauri::command]
fn not_this_person(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    person_cluster_id: i64,
    other_cluster_id: i64,
    expected_generation: Option<i64>,
) -> Result<(), String> {
    ensure_generation(&state, expected_generation)?;
    {
        let conn = state.conn.lock().unwrap();
        // Mint identities for both sides + cannot-link, then confirm the rejected group
        // so it's a durable, competing exemplar (not wiped as a tentative machine label).
        db::add_cannot_link(&conn, person_cluster_id, other_cluster_id).map_err(|e| e.to_string())?;
        db::confirm_cluster_faces(&conn, other_cluster_id).map_err(|e| e.to_string())?;
    }
    schedule_recluster(app);
    Ok(())
}

/// Wipe all face data — detections, clusters, names, identities, decisions — and
/// re-arm the sweep so the whole library is analyzed from scratch. For testing the
/// recognition experience end-to-end on a clean slate.
#[tauri::command]
fn reset_face_recognition(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    {
        let conn = state.conn.lock().unwrap();
        db::reset_faces_for_recompute(&conn).map_err(|e| e.to_string())?;
    }
    // Drop cached cover crops too; they point at now-deleted faces.
    let _ = std::fs::remove_dir_all(&state.faces_dir);
    let _ = std::fs::create_dir_all(&state.faces_dir);
    let _ = app.emit("faces-progress", FaceProgress { scanned: 0, eligible: 0 });
    Ok(())
}

/// Fast "start people over": clear every decision (identities, names, cannot-links)
/// but keep the detected faces and their embeddings, then re-cluster from scratch,
/// unsupervised. No re-detection — seconds, not the full sweep. Snapshots the database
/// to `<db>.pre-reset.bak` first (via VACUUM INTO, a consistent copy) so a regretted
/// reset is recoverable. Returns the backup path.
#[tauri::command]
fn reset_face_decisions(app: tauri::AppHandle) -> Result<String, String> {
    let state = app.state::<AppState>();
    let backup = state.db_path.with_extension("pre-reset.bak");
    {
        let conn = state.conn.lock().unwrap();
        // Snapshot first (best-effort restore point), then wipe decisions.
        let _ = std::fs::remove_file(&backup);
        conn.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])
            .map_err(|e| format!("backup failed: {e}"))?;
        db::clear_face_decisions(&conn).map_err(|e| e.to_string())?;
    }
    // Rebuild clusters from embeddings, unsupervised, in the background.
    run_recluster(app);
    Ok(backup.to_string_lossy().into_owned())
}

/// Set once the one-time migration off the old greedy clustering has run.
const RECLUSTER_FLAG: &str = "reclustered_v1";
/// Set once faces have been re-detected with the fixed alignment (see the
/// migration in `setup`). Bumping this string forces a one-time face re-sweep.
const FACES_ALIGNED_FLAG: &str = "faces_aligned_v2";

/// Progress of a background re-cluster. `running` flips false when it finishes, so
/// the People view can reload exactly once (and never mid-rebuild → no reflow).
#[derive(Clone, serde::Serialize)]
struct ClusterProgress {
    running: bool,
    fraction: f32,
}

/// Rewrite `assignments` (face → fresh cluster id) so every face sharing an
/// `identity_id` lands in one cluster. Union-find over the new cluster ids: faces
/// of the same identity union their clusters, then every face is remapped to its
/// component root. Identities are disjoint, so this only ever *joins* groups the
/// user confirmed — it never splits or crosses people.
fn apply_must_links(face_identity: &[(i64, i64)], assignments: &mut [(i64, i64)]) {
    use std::collections::HashMap;
    let face_to_cluster: HashMap<i64, i64> = assignments.iter().cloned().collect();
    let mut parent: HashMap<i64, i64> = HashMap::new();
    fn find(parent: &mut HashMap<i64, i64>, x: i64) -> i64 {
        let mut r = x;
        while let Some(&p) = parent.get(&r) {
            if p == r {
                break;
            }
            r = p;
        }
        r
    }
    let mut ident_first: HashMap<i64, i64> = HashMap::new();
    for (face, ident) in face_identity {
        let Some(&c) = face_to_cluster.get(face) else { continue };
        parent.entry(c).or_insert(c);
        match ident_first.get(ident) {
            None => {
                ident_first.insert(*ident, c);
            }
            Some(&first) => {
                let (ra, rb) = (find(&mut parent, first), find(&mut parent, c));
                if ra != rb {
                    parent.insert(ra, rb);
                }
            }
        }
    }
    if parent.is_empty() {
        return;
    }
    for a in assignments.iter_mut() {
        a.1 = find(&mut parent, a.1);
    }
}

/// Recompute the suggestion caches (pairwise merges + identity growth) after a
/// clustering pass, and bump the cluster generation. Runs on the pass's background
/// thread with its own connection, so the UI's shared connection is never held
/// through the matrix math (the old per-tab-open compute stalled every avatar
/// request behind that lock). The get_* commands then serve instant reads.
fn refresh_suggestion_cache(app: &AppHandle, conn: &Connection) {
    let state = app.state::<AppState>();
    let generation = state.cluster_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let mut merges = compute_merge_suggestions(conn).unwrap_or_default();
    let (mut growth, who) = compute_identity_growth(conn).unwrap_or_default();
    for s in &mut merges {
        s.generation = generation;
    }
    for g in &mut growth {
        g.generation = generation;
    }
    let queue = build_review_queue(&merges, &growth, who);
    *state.suggestion_cache.lock().unwrap() = SuggestionCache { generation, merges, growth, queue };
}

/// Normalize every engine's suggestions into the single payoff-sorted review queue
/// the focus flow walks: strong batches and uncertain growth per person, contested
/// who-is-this clusters, and pairwise same-person evidence — biggest photos first,
/// capped so a session has a visible end.
fn build_review_queue(
    merges: &[MergeSuggestion],
    growth: &[IdentityGrowth],
    who: Vec<ReviewItem>,
) -> Vec<ReviewItem> {
    const MAX_QUEUE: usize = 60;
    let mut items = who;
    for g in growth {
        if !g.strong_groups.is_empty() {
            items.push(ReviewItem::StrongBatch {
                photos: g.strong_photos,
                name: g.name.clone(),
                into: g.into,
                anchor_faces: g.anchor_faces.clone(),
                groups: g.strong_groups.clone(),
            });
        }
        for m in &g.maybe {
            items.push(ReviewItem::Maybe {
                photos: m.photos,
                name: g.name.clone(),
                into: g.into,
                anchor_faces: g.anchor_faces.clone(),
                group: m.clone(),
            });
        }
    }
    for s in merges {
        items.push(ReviewItem::Pairwise {
            photos: s.photos,
            into: s.into,
            from: s.from,
            into_name: s.into_name.clone(),
            into_faces: s.into_faces.clone(),
            from_faces: s.from_faces.clone(),
        });
    }
    let photos_of = |i: &ReviewItem| match i {
        ReviewItem::StrongBatch { photos, .. }
        | ReviewItem::Maybe { photos, .. }
        | ReviewItem::WhoIsThis { photos, .. }
        | ReviewItem::Pairwise { photos, .. } => *photos,
    };
    items.sort_by(|a, b| photos_of(b).cmp(&photos_of(a)));
    items.truncate(MAX_QUEUE);
    items
}

/// Guard for suggestion-driven mutations. Cluster ids are reassigned from scratch by
/// every re-cluster, so a card computed before one completes may now point at a
/// different group of faces — acting on it would confirm/merge the wrong people
/// (durably: absorbing writes confirmed must-links). The payload carries the
/// generation it was computed at; on mismatch we refuse and the frontend refreshes.
/// `None` (paths not fed by suggestions, e.g. the name typeahead) is not checked.
fn ensure_generation(state: &AppState, expected: Option<i64>) -> Result<(), String> {
    match expected {
        Some(g) if g != state.cluster_gen.load(Ordering::SeqCst) => {
            Err("stale suggestion: people were reorganized since it was shown".into())
        }
        _ => Ok(()),
    }
}

/// How long a burst of corrections may extend before the self-heal pass runs.
const RECLUSTER_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(4);

/// Debounced [`run_recluster`]: a correction's DB writes apply immediately, but the
/// expensive re-cluster waits for a quiet moment, so a review session (answer,
/// answer, answer) pays for one pass instead of one per click. Each call supersedes
/// any still-pending one. Reset, startup, and the sweep-drain consolidation still
/// call [`run_recluster`] directly — they're one-shot, not bursts.
fn schedule_recluster(app: AppHandle) {
    let state = app.state::<AppState>();
    let epoch = state.recluster_epoch.clone();
    let mine = epoch.fetch_add(1, Ordering::SeqCst) + 1;
    drop(state);
    std::thread::spawn(move || {
        std::thread::sleep(RECLUSTER_DEBOUNCE);
        if epoch.load(Ordering::SeqCst) == mine {
            run_recluster(app);
        }
    });
}

/// The evidence floor: an identity earns "magnet authority" — the right to auto-fold
/// look-alikes in, to generate "N groups might also be…" suggestions, and to be a
/// "looks like X" flag target — only once it has at least this many *confirmed* faces.
/// One face (worse, a profile shot) defines a point, not a person; extrapolating a
/// whole identity from it is what pulls in swarms of unrelated pose/lighting matches.
/// Naming a real cluster clears this instantly; naming a single stray face does not,
/// until you confirm a few more. (`identity_anchor_embeddings` returns min(faces, N),
/// so `anchor.len()` is a direct read of confirmed evidence.)
const MIN_ANCHOR: usize = 4;

/// Minimum confirmed faces for an identity to *compete* (pull look-alikes toward it).
/// Lower than [`MIN_ANCHOR`] on purpose: a competitor can only ever push a face into
/// *review* (never silently claim it — that still needs `MIN_ANCHOR`), so it's safe to
/// let even a one-group "not Mía" rejection start defending its faces immediately.
const COMPETITOR_MIN: usize = 1;

/// How similar a candidate must be to a confirmed anchor before auto-fold reunites it
/// *without asking*. Above this, the match is safe to apply silently; below it (down to
/// the linkage floor) the match is real but uncertain — it goes to the review path
/// instead of being folded. Adults cluster tight and clear this easily, so they still
/// auto-reunite; two different babies rarely clear it against each other, so naming one
/// baby no longer vacuums up the others — those land in review, where a human decides.
const AUTO_FOLD_MIN: f32 = 0.6;

/// How much the best-matching person must beat the runner-up before a cluster is
/// auto-assigned. Below this the match is a near-tie — two people the model can't
/// separate (two babies) — so it's left for the human to resolve, never guessed.
const AUTO_FOLD_MARGIN: f32 = 0.06;

/// Similarity used to find an anchor's dominant appearance when trimming it to a core.
const ANCHOR_CORE_TAU: f32 = 0.5;

/// A cleaned "core" of an identity's anchor: cluster the exemplars and keep only the
/// dominant appearance group, so a few outliers — a wrong fold, an off-angle shot —
/// can't drag the anchor off the person and cascade (one bad fold poisoning every
/// future match). Falls back to the full set when there's no clear majority to trust,
/// or too few exemplars to bother.
fn anchor_core(embs: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
    if embs.len() < 6 {
        return embs;
    }
    let groups = cluster::group_looks(&embs, ANCHOR_CORE_TAU);
    if let Some(biggest) = groups.iter().max_by_key(|g| g.len()) {
        // Only trust a core when it's a clear majority of the exemplars.
        if biggest.len() * 2 >= embs.len() {
            return biggest.iter().map(|&i| embs[i].clone()).collect();
        }
    }
    embs
}

/// Fold every cluster that confidently matches a *confirmed* identity's anchor into
/// that identity — the automatic reunification behind "you named them, so we gather
/// their scattered fragments for you." This is safe where unsupervised clustering
/// isn't, for the same reasons the growth prompt relied on: every match is to a
/// human-confirmed anchor (never chained cluster→cluster), covers a majority of the
/// candidate cluster, is conflict-guarded (a fragment two confirmed people both match
/// — two babies — is left untouched, never guessed), and touches only *unclaimed*
/// fragments (anything already bound to another identity is left alone). Runs only on
/// a settled library, where anchors are complete. Returns how many clusters folded in.
///
/// This is what turns "merge dozens of 1-photo clusters by hand" into "already done":
/// naming a person, or the sweep settling, reunites their scattered fragments with no
/// clicks. The manual review path remains only for the genuinely ambiguous residual.
/// For every candidate cluster, how well it matches *each confirmed identity* (named or
/// not), best-first — the shared basis for auto-fold and review. Because unnamed
/// "someone else" splits are confirmed identities too, they compete here: a face that
/// looks like a rejected look-alike is pulled toward that competitor and away from the
/// person, which is how a "not Mía" generalizes to similar faces.
fn cluster_identity_matches(
    conn: &Connection,
) -> anyhow::Result<std::collections::HashMap<i64, Vec<(i64, f32)>>> {
    use std::collections::{HashMap, HashSet};
    let all_faces = db::face_cluster_embeddings(conn)?;
    let mut per_cluster: HashMap<i64, Vec<(i64, f32)>> = HashMap::new();
    for identity_id in db::confirmed_identity_ids(conn)? {
        let anchor = db::confirmed_anchor_embeddings(conn, identity_id, 64)?;
        if anchor.len() < COMPETITOR_MIN {
            continue; // no confirmed evidence to compete with
        }
        let core = anchor_core(anchor);
        let own: HashSet<i64> =
            db::clusters_of_identity(conn, identity_id)?.into_iter().collect();
        let others: Vec<(i64, i64, Vec<f32>)> =
            all_faces.iter().filter(|(_, c, _)| !own.contains(c)).cloned().collect();
        for c in cluster::identity_candidates(&core, &others) {
            per_cluster.entry(c.cluster_id).or_default().push((identity_id, c.max_sim));
        }
    }
    for v in per_cluster.values_mut() {
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    }
    Ok(per_cluster)
}

fn auto_fold_confident(conn: &Connection) -> anyhow::Result<usize> {
    use std::collections::HashSet;
    // Mid-sweep anchors are incomplete and would misfire — wait until scanning settles.
    match db::face_progress(conn) {
        Ok((scanned, eligible)) if eligible > 0 && scanned >= eligible => {}
        _ => return Ok(0),
    }
    if db::confirmed_identity_ids(conn)?.is_empty() {
        return Ok(0);
    }
    // Wipe the machine's previous tentative labels and re-derive them from scratch —
    // competitively — against the *confirmed* exemplars. This is what makes a wrong fold
    // self-correcting: nothing auto is welded on, so every pass reconsiders it against
    // whatever people (and "not X" competitors) you've since confirmed.
    db::clear_unconfirmed_identities(conn)?;

    // A person's own cluster (holds their confirmed faces) is never a fold target.
    let confirmed_clusters: HashSet<i64> = db::confirmed_clusters(conn)?.into_iter().collect();
    // Only identities with enough confirmed evidence may *claim* a cluster; a thin
    // competitor can still win the ranking (and thereby push a face off someone else)
    // but can't silently absorb it — that face just stays unassigned for review.
    let fold_eligible: HashSet<i64> =
        db::fold_eligible_identities(conn, MIN_ANCHOR as i64)?.into_iter().collect();
    let matches = cluster_identity_matches(conn)?;
    // Co-occurrence veto: a group photographed alongside the person's confirmed
    // faces cannot be them (siblings in one frame), however similar the embeddings.
    let (cluster_photos, identity_photos) = cooccurrence_maps(conn)?;

    // Assign each candidate to the identity it matches *decisively* best: the top match
    // must clear AUTO_FOLD_MIN and beat the runner-up by AUTO_FOLD_MARGIN. A near-tie
    // (two babies both plausible) is ambiguous — left unassigned for the review path,
    // never guessed. Confirmed people-clusters are never folded away.
    let mut folded = 0usize;
    for (cid, m) in matches {
        if confirmed_clusters.contains(&cid) {
            continue;
        }
        let (best_id, best_sim) = m[0];
        if best_sim < AUTO_FOLD_MIN {
            continue;
        }
        if m.len() > 1 && best_sim - m[1].1 < AUTO_FOLD_MARGIN {
            continue; // ambiguous between people — hold for review
        }
        if !fold_eligible.contains(&best_id) {
            continue; // best match is only a thin competitor — don't let it absorb
        }
        if cooccurs(&cluster_photos, cid, &identity_photos, best_id) {
            continue; // photographed together — two people, never fold
        }
        let into = match db::clusters_of_identity(conn, best_id)?.first() {
            Some(&c) => c,
            None => continue,
        };
        if cid != into {
            db::merge_clusters(conn, into, cid)?;
            folded += 1;
        }
    }
    Ok(folded)
}

/// Run [`auto_fold_confident`] in the background (Principle 1: off the UI thread),
/// then signal the People view to refresh via `cluster-progress`. Cheap next to a
/// full re-cluster — just anchor matching and merges — so it's fine to fire after
/// every naming/merge. Shares the re-cluster guard, so it never overlaps one.
fn run_auto_fold(app: AppHandle) {
    let state = app.state::<AppState>();
    if state.reclustering.swap(true, Ordering::SeqCst) {
        return; // a re-cluster or fold is already running
    }
    let db_path = state.db_path.clone();
    let reclustering = state.reclustering.clone();
    drop(state);

    std::thread::spawn(move || {
        background_qos();
        let _ = app.emit("cluster-progress", ClusterProgress { running: true, fraction: 0.0 });
        let folded = (|| -> anyhow::Result<usize> {
            let conn = db::open(&db_path)?;
            let n = auto_fold_confident(&conn)?;
            // Clusters may have changed — recompute the suggestion caches (and bump
            // the generation) before the UI is told to reload.
            refresh_suggestion_cache(&app, &conn);
            Ok(n)
        })();
        if let Err(e) = folded {
            eprintln!("auto-fold failed: {e}");
        }
        let _ = app.emit("cluster-progress", ClusterProgress { running: false, fraction: 1.0 });
        reclustering.store(false, Ordering::SeqCst);
    });
}

/// Re-cluster every face from scratch, in the background (Principle 1: off the UI
/// thread, no focus steal). Order-independent and purity-biased. Names are carried
/// across by re-anchoring each to the new cluster holding the plurality of its old
/// faces. A guard prevents overlap; progress streams via `cluster-progress`.
fn run_recluster(app: AppHandle) {
    let state = app.state::<AppState>();
    if state.reclustering.swap(true, Ordering::SeqCst) {
        // Already running — don't drop this request; have the running pass re-run once
        // it finishes, so a fold triggered mid-recluster (naming several people) lands.
        state.recluster_pending.store(true, Ordering::SeqCst);
        return;
    }
    let db_path = state.db_path.clone();
    let reclustering = state.reclustering.clone();
    let recluster_pending = state.recluster_pending.clone();
    recluster_pending.store(false, Ordering::SeqCst);
    let app_for_rerun = app.clone();
    drop(state);

    std::thread::spawn(move || {
        background_qos();
        use std::collections::HashMap;
        let _ = app.emit("cluster-progress", ClusterProgress { running: true, fraction: 0.0 });
        let result = (|| -> anyhow::Result<()> {
            let mut conn = db::open(&db_path)?;
            let faces = db::all_face_embeddings(&conn)?;
            if faces.is_empty() {
                db::set_meta(&conn, RECLUSTER_FLAG, "1")?;
                return Ok(());
            }

            // The user's durable "not the same person" decisions, enforced as hard
            // constraints in the agglomeration: a merge that would co-locate two
            // cannot-linked identities is refused, so embedding-close strangers
            // (e.g. two babies) the user pulled apart never drift back together.
            // Only *confirmed* (user-labeled) faces are must-links; auto-folded ones
            // are left free to re-cluster by appearance, so a wrongly-folded look-alike
            // isn't welded on — it re-homes once a better-matching person exists.
            let (photo_of, same_photo_ok) = photo_constraints(&conn)?;
            let constraints = cluster::LinkConstraints {
                face_identity: db::confirmed_face_identities(&conn)?.into_iter().collect(),
                cannot_link: db::cannot_link_pairs(&conn)?.into_iter().collect(),
                photo_of,
                same_photo_ok,
            };

            // Throttle progress events to ~every 2% so we don't flood the channel.
            let app2 = app.clone();
            let mut last = 0.0f32;
            let mut assignments = cluster::recluster(&faces, &constraints, |f| {
                if f - last >= 0.02 || f >= 1.0 {
                    last = f;
                    let _ = app2.emit("cluster-progress", ClusterProgress { running: true, fraction: f });
                }
            });
            // Honor must-links: every face the user *confirmed* under one identity is
            // forced into one cluster, however the embeddings split it. This is what
            // makes a confirmed merge durable — the re-cluster can no longer undo it.
            apply_must_links(&db::confirmed_face_identities(&conn)?, &mut assignments);
            db::set_face_clusters(&mut conn, &assignments)?;

            // Re-derive each cluster's display name from the durable *identity* whose
            // confirmed faces landed in it — read fresh here (post-clustering), so a
            // name added while this pass ran is honored, not wiped by a stale snapshot.
            // (Names live on the identity; cluster_names is just a per-cluster cache.)
            let new_of: HashMap<i64, i64> = assignments.iter().cloned().collect();
            let named_idents: HashMap<i64, String> =
                db::named_identities(&conn)?.into_iter().collect();
            let mut tally: HashMap<i64, HashMap<i64, usize>> = HashMap::new(); // identity -> cluster -> n
            for (face, ident) in db::confirmed_face_identities(&conn)? {
                if named_idents.contains_key(&ident) {
                    if let Some(&newc) = new_of.get(&face) {
                        *tally.entry(ident).or_default().entry(newc).or_insert(0) += 1;
                    }
                }
            }
            // Two identities can (transiently) have their plurality in the SAME new
            // cluster — e.g. after a merge that combined two named people. Assign
            // greedily by descending confirmed-face count, one name per cluster and
            // one cluster per identity, so the runner-up re-anchors to its next-best
            // cluster instead of being silently overwritten (the old ON CONFLICT
            // upsert ate a name and the person vanished from the grid).
            let mut claims: Vec<(usize, i64, i64)> = Vec::new(); // (count, identity, cluster)
            for (ident, m) in tally {
                for (newc, n) in m {
                    claims.push((n, ident, newc));
                }
            }
            claims.sort_by(|a, b| b.0.cmp(&a.0));
            let mut cluster_taken: std::collections::HashSet<i64> = std::collections::HashSet::new();
            let mut ident_named: std::collections::HashSet<i64> = std::collections::HashSet::new();
            let mut names: Vec<(i64, String)> = Vec::new();
            for (_, ident, newc) in claims {
                if ident_named.contains(&ident) || cluster_taken.contains(&newc) {
                    continue;
                }
                ident_named.insert(ident);
                cluster_taken.insert(newc);
                names.push((newc, named_idents[&ident].clone()));
            }
            db::replace_cluster_names(&mut conn, &names)?;
            // Now that clusters and names are settled, reunite each confirmed person's
            // scattered look-alike fragments automatically (see `auto_fold_confident`).
            let _ = auto_fold_confident(&conn)?;
            db::set_meta(&conn, RECLUSTER_FLAG, "1")?;
            // Everything is renumbered — recompute the suggestion caches (and bump
            // the generation) before the UI is told to reload.
            refresh_suggestion_cache(&app, &conn);
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("recluster failed: {e}");
        }
        let _ = app.emit("cluster-progress", ClusterProgress { running: false, fraction: 1.0 });
        reclustering.store(false, Ordering::SeqCst);
        // A fold was requested while we ran — honor it now so nothing is dropped.
        if recluster_pending.swap(false, Ordering::SeqCst) {
            run_recluster(app_for_rerun);
        }
    });
}

/// Rebuild all clusters from scratch (purity-biased), in the background. Safe to
/// call anytime; overlapping calls are ignored.
#[tauri::command]
fn recluster(app: tauri::AppHandle) {
    run_recluster(app);
}

/// The current clustering generation — fetched with a people list so later
/// mutations can prove their cluster ids are from the same clustering.
#[tauri::command]
fn get_cluster_generation(state: tauri::State<'_, AppState>) -> i64 {
    state.cluster_gen.load(Ordering::SeqCst)
}

/// Debug-only: print the cosine distribution of mutual-kNN edges over the whole
/// face set. This is the *measurement* that sets `TAU_LINK` from a real library
/// rather than from vibes — a clean separation shows up as a trough between the
/// within-person mass (high) and the across-person tail (low); put `TAU_LINK` in
/// the trough. Returns the report as a string (also printed to the log).
#[tauri::command]
fn cluster_debug(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let faces = {
        let conn = state.conn.lock().unwrap();
        db::all_face_embeddings(&conn).map_err(|e| e.to_string())?
    };
    let sims = cluster::mutual_edge_sims(&faces);
    let mut report = format!(
        "cluster_debug: {} faces, {} mutual-kNN edges\n",
        faces.len(),
        sims.len()
    );
    if !sims.is_empty() {
        // 0.30..1.00 in 0.05-wide buckets — the band where TAU_LINK lives.
        let mut buckets = [0usize; 14];
        for &s in &sims {
            let b = (((s - 0.30) / 0.05).floor() as isize).clamp(0, 13) as usize;
            buckets[b] += 1;
        }
        let max = buckets.iter().copied().max().unwrap_or(1).max(1);
        for (b, &c) in buckets.iter().enumerate() {
            let lo = 0.30 + 0.05 * b as f32;
            let bar = "#".repeat(c * 40 / max);
            report.push_str(&format!("  {lo:.2}-{:.2} | {bar} {c}\n", lo + 0.05));
        }
    }
    eprintln!("{report}");
    Ok(report)
}

/// Locate a bundled model: prefer the packaged resource dir, fall back to the
/// source tree for `tauri dev`.
fn resolve_model(app: &AppHandle, name: &str) -> PathBuf {
    if let Ok(p) = app
        .path()
        .resolve(format!("models/{name}"), tauri::path::BaseDirectory::Resource)
    {
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models").join(name)
}

/// How many face workers detect + embed in parallel. The per-photo cost is
/// dominated by reading the (cloud-backed) original off ProtonDrive, which is
/// I/O-latency-bound — overlapping several reads is the main lever on sweep
/// throughput. CoreML serializes at the hardware level, so this is sized for
/// I/O overlap, not CPU. Kept modest (and UTILITY-QoS'd, see `background_qos`) so
/// the pool overlaps reads without crowding the foreground off the CPU.
const FACE_WORKERS: usize = 4;

/// The background face sweep: a pool of `FACE_WORKERS` that decode + detect +
/// embed in parallel, plus a single coordinator thread that hands out work and
/// does all the clustering and DB writes. Splitting it this way keeps the
/// expensive, parallelizable part (I/O + inference) concurrent while clustering
/// stays single-threaded and deterministic — the online cluster index is mutated
/// in exactly one place, in a well-defined order.
///
/// It only touches photos that already have a thumbnail (i.e. are local), so the
/// first pass over a cloud library lags thumbnailing, and it's resumable across
/// launches via `faces_scanned` (claims are reset on startup, see below).
fn spawn_face_workers(
    app: AppHandle,
    db_path: PathBuf,
    preview_dir: PathBuf,
    yunet: PathBuf,
    sface: PathBuf,
    paused: Arc<AtomicBool>,
) {
    use std::sync::mpsc;

    // Keep ~2 jobs in flight per worker so a worker rarely waits for the
    // coordinator to refill, but a crash/quit leaves only a handful of photos in
    // the "claimed" state to recover.
    let target_outstanding = FACE_WORKERS * 2;
    let claim_batch = (FACE_WORKERS * 2) as i64;

    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Arc::new(Mutex::new(job_rx));
    let (res_tx, res_rx) = mpsc::channel::<(i64, Vec<db::DetectedFace>)>();

    // Worker pool: parallel decode + detect + embed. Each owns its own models.
    for _ in 0..FACE_WORKERS {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let preview_dir = preview_dir.clone();
        let yunet = yunet.clone();
        let sface = sface.clone();
        std::thread::spawn(move || {
            // Set QoS before loading the models so ONNX Runtime's own inference
            // threads inherit UTILITY QoS and yield to the UI (PRINCIPLES #3).
            background_qos();
            let mut models = match faces::FaceModels::load(&yunet, &sface) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("faces: model load failed: {e}");
                    return;
                }
            };
            loop {
                // Block until a job is available (or the coordinator is gone).
                let job = {
                    let rx = job_rx.lock().unwrap();
                    rx.recv()
                };
                let job = match job {
                    Ok(j) => j,
                    Err(_) => break, // coordinator dropped the sender → shut down
                };
                // On any decode/inference error, fall through to an empty result —
                // the coordinator still marks the photo scanned so we never re-loop.
                let found = thumbs::load_face_source(&preview_dir, job.id, &job.path)
                    .ok()
                    .map(|img| models.process(&img).unwrap_or_default())
                    .unwrap_or_default();
                if res_tx.send((job.id, found)).is_err() {
                    break; // coordinator gone
                }
            }
        });
    }
    drop(res_tx); // only the workers hold result senders now

    // Coordinator: claims work, feeds the pool, and serially clusters + saves.
    std::thread::spawn(move || {
        background_qos();
        let mut conn = match db::open(&db_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("faces: cannot open db: {e}");
                return;
            }
        };
        // Recover any photos a previous run claimed but didn't finish.
        let _ = db::reset_claimed_faces(&conn);
        // Rebuild the cluster index from any faces clustered in a prior session.
        let mut index = cluster::ClusterIndex::load(db::clustered_embeddings(&conn).unwrap_or_default());
        let mut outstanding = 0usize;
        // Faces assigned by the (cheap, approximate) incremental path since the last
        // full consolidation. When the sweep drains we run a purity-first rebuild to
        // tidy them up — the incremental path keeps things usable in the meantime.
        let mut pending_consolidation: u64 = 0;
        loop {
            if paused.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
            // Top up the pool's backlog.
            while outstanding < target_outstanding {
                let batch = db::claim_faces_batch(&mut conn, claim_batch).unwrap_or_default();
                if batch.is_empty() {
                    break;
                }
                for job in batch {
                    outstanding += 1;
                    if job_tx.send(job).is_err() {
                        return; // all workers gone
                    }
                }
            }
            // Drain a finished photo: assign clusters (online, incremental) and
            // persist. Time out so we periodically re-check pause / refill / idle.
            match res_rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok((id, found)) => {
                    let cluster_ids: Vec<i64> =
                        found.iter().map(|f| index.assign(&f.embedding, id)).collect();
                    pending_consolidation += found.len() as u64;
                    let _ = db::save_faces(&mut conn, id, &found, &cluster_ids);
                    outstanding -= 1;
                    if let Ok((scanned, eligible)) = db::face_progress(&conn) {
                        let _ = app.emit("faces-progress", FaceProgress { scanned, eligible });
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Nothing finished this tick. When the whole library is scanned,
                    // there's no backlog to claim and nothing in flight — the sweep
                    // has drained. If new faces accreted via the incremental path,
                    // consolidate once with the full batch algorithm and refresh our
                    // in-memory index so later incremental assignments don't drift.
                    // Skipped while a re-cluster (e.g. the startup migration) runs —
                    // we retry on the next drain.
                    if outstanding == 0 {
                        if pending_consolidation > 0
                            && !app.state::<AppState>().reclustering.load(Ordering::SeqCst)
                        {
                            run_recluster(app.clone());
                            let guard = app.state::<AppState>().reclustering.clone();
                            while guard.load(Ordering::SeqCst) {
                                std::thread::sleep(std::time::Duration::from_millis(200));
                            }
                            index = cluster::ClusterIndex::load(
                                db::clustered_embeddings(&conn).unwrap_or_default(),
                            );
                            pending_consolidation = 0;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break, // workers gone
            }
        }
    });
}

/// Serve one `thumb://` request: a cached grid thumbnail (`/<id>`), a viewer
/// preview (`/preview/<id>`), or a cover-face crop (`/face/<id>`). On a cache miss
/// this **decodes a full-resolution original** (preview / face crop), which is far
/// too heavy to run on the webview's main thread — a burst of requests (e.g. opening
/// People, which asks for a crop per person) would freeze the UI. It is therefore
/// always invoked off-thread by the asynchronous protocol handler below.
fn serve_thumb(app: &AppHandle, full: &str) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::Response;
    let ok = |bytes: Vec<u8>| {
        Response::builder()
            .header("Content-Type", "image/jpeg")
            .header("Cache-Control", "no-cache")
            .body(bytes)
            .unwrap()
    };
    let not_found = || Response::builder().status(404).body(Vec::new()).unwrap();

    let is_preview = full.starts_with("preview/");
    let is_face = full.starts_with("face/");
    let id: Option<i64> = full.rsplit('/').next().and_then(|s| s.parse().ok());
    let id = match id {
        Some(id) => id,
        None => return not_found(),
    };

    let state = app.state::<AppState>();

    // Cover face for the People screen: a crisp crop from the original, generated
    // on first request and cached forever.
    if is_face {
        let out = faces::face_crop_path(&state.faces_dir, id);
        if let Ok(bytes) = std::fs::read(&out) {
            return ok(bytes);
        }
        let (photo_id, x1, y1, x2, y2) = {
            let conn = state.conn.lock().unwrap();
            match db::face_box(&conn, id).ok().flatten() {
                Some(b) => b,
                None => return not_found(),
            }
        };
        let original = {
            let conn = state.conn.lock().unwrap();
            db::path_for_id(&conn, photo_id).ok().flatten()
        };
        let img = match original.and_then(|p| thumbs::load_oriented(std::path::Path::new(&p)).ok()) {
            Some(i) => i.to_rgb8(),
            None => return not_found(),
        };
        return match faces::crop_face_jpeg(&img, (x1, y1, x2, y2)) {
            Ok(bytes) => {
                if let Some(parent) = out.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&out, &bytes);
                ok(bytes)
            }
            Err(_) => not_found(),
        };
    }

    if !is_preview {
        let path = thumbs::thumb_path(&state.cache_dir, id);
        return match std::fs::read(&path) {
            Ok(bytes) => ok(bytes),
            Err(_) => not_found(),
        };
    }

    let out = thumbs::preview_path(&state.preview_dir, id);
    if let Ok(bytes) = std::fs::read(&out) {
        return ok(bytes);
    }
    let original = {
        let conn = state.conn.lock().unwrap();
        db::path_for_id(&conn, id).ok().flatten()
    };
    let original = match original {
        Some(p) => p,
        None => return not_found(),
    };
    match thumbs::generate_preview(&out, &original) {
        Ok(bytes) => ok(bytes),
        Err(e) => {
            eprintln!("preview failed for {original}: {e}");
            not_found()
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // Serve cached images directly from disk over the `thumb` scheme (a
        // single custom scheme keeps things inside the webview's CSP img-src):
        //   thumb://localhost/<id>          -> the 256px grid thumbnail
        //   thumb://localhost/preview/<id>  -> the large viewer preview
        // The preview is generated on demand the first time it's requested
        // (decoded, EXIF-oriented, downscaled) and cached forever; for a
        // cloud-only original that first read triggers the on-demand download.
        // Asynchronous so the (potentially full-res-decoding) handler runs on a
        // blocking-pool thread, never the webview's main thread. A synchronous
        // handler here froze the UI ("App Not Responding") whenever a screen asked
        // for many uncached images at once — opening People decodes a face crop per
        // person. Off-thread, the event loop stays free (PRINCIPLES #1).
        .register_asynchronous_uri_scheme_protocol("thumb", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            // Path is `/<id>`, `/preview/<id>`, or `/face/<face_id>`.
            let full = request.uri().path().trim_matches('/').to_string();
            tauri::async_runtime::spawn_blocking(move || {
                responder.respond(serve_thumb(&app, &full));
            });
        })
        .setup(|app| {
            prof::init();
            // All cached state lives under the OS app-data directory — never
            // inside the user's photo folders (their originals stay pristine).
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db_path = data_dir.join("library.db");
            let cache_dir = data_dir.join("thumbnails");
            std::fs::create_dir_all(&cache_dir)?;
            let preview_dir = data_dir.join("previews");
            std::fs::create_dir_all(&preview_dir)?;
            let faces_dir = data_dir.join("faces");
            std::fs::create_dir_all(&faces_dir)?;

            let conn = db::open(&db_path)?;
            db::init(&conn)?;

            // One-time migration for the face-alignment fix: faces detected before it
            // have collapsed embeddings (landmarks were swapped, mangling every
            // aligned crop). Embeddings can't be repaired in place — landmarks aren't
            // stored — so discard the old faces and let the sweep re-detect them
            // correctly. Cached crops are stale too; clear them.
            if db::get_meta(&conn, FACES_ALIGNED_FLAG).ok().flatten().is_none() {
                db::reset_faces_for_recompute(&conn)?;
                let _ = std::fs::remove_dir_all(&faces_dir);
                std::fs::create_dir_all(&faces_dir)?;
                db::set_meta(&conn, FACES_ALIGNED_FLAG, "1")?;
            }

            // Has the one-time migration off the old greedy clustering run yet?
            // (Checked now, before `conn` moves into the shared state.)
            let needs_recluster = db::get_meta(&conn, RECLUSTER_FLAG)
                .ok()
                .flatten()
                .is_none();

            // A download interrupted by a previous quit is no longer running;
            // reset those placeholders so they show as cloud again (and can be
            // re-fetched when next visible).
            db::set_status_many_where_downloading(&conn)?;

            // Local pool: thumbnail decoding is CPU-bound, so we cap it at roughly
            // half the cores and leave the rest for the foreground (the workers are
            // also UTILITY-QoS'd, see `background_qos`). Together with the face pool
            // this keeps total background decode well under the core count, so the
            // UI never gets starved the way 1-per-core did.
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2);
            let local_workers = (cores / 2).max(2);

            let local_queue = ThumbQueue::new();
            let cloud_queue = ThumbQueue::new();

            // Workers emit `thumb-ready` so the frontend can refresh one cell.
            // `ok` says whether a thumbnail actually exists now (success) versus
            // a failed/abandoned attempt that should stop the spinner only.
            let app_handle = app.handle().clone();
            let notify = move |id: i64, ok: bool| {
                let _ = app_handle.emit("thumb-ready", ThumbDone { id, ok });
            };

            // Local files that fail to decode are FAILED; cloud files that fail to
            // download fall back to CLOUD so a later visit can retry.
            // Local files already had their date read at scan time; cloud files
            // get it here, once downloaded (extract_date = true for the cloud pool).
            thumbs::spawn_workers(
                local_workers,
                local_queue.clone(),
                db_path.clone(),
                cache_dir.clone(),
                preview_dir.clone(),
                db::STATUS_FAILED,
                false,
                notify.clone(),
            );
            thumbs::spawn_workers(
                CLOUD_WORKERS,
                cloud_queue.clone(),
                db_path.clone(),
                cache_dir.clone(),
                preview_dir.clone(),
                db::STATUS_CLOUD,
                true,
                notify,
            );

            // Resume local thumbnails left pending from a previous session.
            let pending = db::pending_jobs(&conn)?;
            local_queue.enqueue(pending);

            // Background face sweep. Resolve the bundled models and start the
            // worker pool + coordinator; paused-flag shared with commands.
            let faces_paused = Arc::new(AtomicBool::new(false));
            let yunet = resolve_model(app.handle(), "yunet.onnx");
            let sface = resolve_model(app.handle(), "sface.onnx");
            spawn_face_workers(
                app.handle().clone(),
                db_path.clone(),
                preview_dir.clone(),
                yunet,
                sface,
                faces_paused.clone(),
            );

            // One-time backfill: give photos missing a capture date one parsed
            // from their filename, so a cloud library (whose EXIF we can't read
            // without downloading) still gets a sensible timeline. Then nudge the
            // frontend to re-sort.
            {
                let app2 = app.handle().clone();
                let db_path2 = db_path.clone();
                std::thread::spawn(move || {
                    if let Ok(mut c) = db::open(&db_path2) {
                        let rows = db::null_date_photos(&c).unwrap_or_default();
                        let pairs: Vec<(i64, i64)> = rows
                            .into_iter()
                            .filter_map(|(id, path)| {
                                std::path::Path::new(&path)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .and_then(meta::parse_filename_date)
                                    .map(|ts| (id, ts))
                            })
                            .collect();
                        if !pairs.is_empty() {
                            let _ = db::set_taken_ts_batch(&mut c, &pairs);
                            let total = db::stats(&c).map(|(t, _)| t).unwrap_or(0);
                            let _ = app2.emit("scan-progress", ScanProgress { found: total, done: true });
                        }
                    }
                });
            }

            app.manage(AppState {
                db_path,
                cache_dir,
                preview_dir,
                faces_dir,
                conn: Mutex::new(conn),
                local_queue,
                cloud_queue,
                rescanning: Arc::new(AtomicBool::new(false)),
                faces_paused,
                reclustering: Arc::new(AtomicBool::new(false)),
                recluster_pending: Arc::new(AtomicBool::new(false)),
                cluster_gen: Arc::new(AtomicI64::new(0)),
                suggestion_cache: Arc::new(Mutex::new(SuggestionCache::default())),
                recluster_epoch: Arc::new(AtomicU64::new(0)),
            });

            // One-time migration of the existing (greedy-clustered) mess to the
            // purity-first algorithm. Runs in the background; sets a flag so it
            // happens exactly once. New libraries simply set the flag on an empty
            // pass and rely on the incremental path + periodic consolidation.
            if needs_recluster {
                run_recluster(app.handle().clone());
            } else {
                // Settled library from a prior session: reunite any look-alike fragments
                // that belong to already-confirmed people (the fold a fresh install now
                // does on naming). No-op if the library is still scanning or empty.
                run_auto_fold(app.handle().clone());
            }

            // Proactively download every cloud-only photo in the background so the
            // user doesn't have to scroll the whole library to trigger on-demand fetches.
            // Visible photos are always prioritized over backfill (see set_visible_range).
            spawn_cloud_backfill(
                app.state::<AppState>().db_path.clone(),
                app.state::<AppState>().cloud_queue.clone(),
            );

            // Auto-sync on launch: reconcile every remembered root with disk in
            // the background, so a repeat launch shows the truth (Principle 4)
            // without the user having to ask. Cached content is already on screen.
            rescan_all(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_library_stats,
            get_photos_range,
            get_photo_detail,
            add_folder,
            rescan,
            list_roots,
            remove_folder,
            set_visible_range,
            get_face_progress,
            set_faces_paused,
            get_clusters,
            name_cluster,
            merge_clusters,
            get_merge_suggestions,
            get_identity_growth,
            get_review_queue,
            get_cluster_generation,
            absorb_clusters,
            reject_merge,
            not_this_person,
            reset_face_recognition,
            reset_face_decisions,
            recluster,
            cluster_debug,
            get_person_photos,
            get_person_looks,
            get_faces_in_photo,
            face_ids_for_photos,
            reassign_faces_to_cluster,
            reassign_faces_to_new_person,
            ignore_faces,
            detach_faces,
            undo_correction
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
