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
//!   * the **cloud** queue — a few workers, fed from two sources: the cloud-only
//!     photos the user has scrolled to (promoted to the priority lane so what's
//!     on screen always downloads first), and a slow background backfill
//!     (`spawn_cloud_backfill`) that works through the rest of the library so it
//!     eventually becomes browsable without waiting on the network. Nothing is
//!     fetched twice: every downloaded original is thumbnailed and cached forever.

mod cluster;
mod db;
mod faces;
mod meta;
mod prof;
mod recognition;
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
use recognition::{
    auto_fold_confident, build_review_queue, compute_identity_growth, compute_merge_suggestions,
    photo_constraints, IdentityGrowth, PersonLook, ReviewItem, ReviewQueue,
};

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
    /// Guards against two re-clusters running at once (migration + manual + sweep).
    reclustering: Arc<AtomicBool>,
    /// Set when a re-cluster is requested while one is already running, so the request
    /// isn't dropped — the running pass re-runs once on finish.
    recluster_pending: Arc<AtomicBool>,
    /// Set when a self-heal fold is requested while a fold/re-cluster is already
    /// running, so the correction that requested it still gets its re-derive.
    fold_pending: Arc<AtomicBool>,
    /// Monotonic clustering generation: bumped ONLY when a full re-cluster renumbers
    /// the positive (appearance) group keys. Identity groups (negative keys) are
    /// durable and never invalidated, and fold passes move no ids at all — so
    /// suggestion payloads carry the generation they were computed at, and mutations
    /// verify it (see `ensure_generation`) against genuinely rare renumbering.
    cluster_gen: Arc<AtomicI64>,
    /// People suggestions computed at the end of the last clustering pass (see
    /// `refresh_suggestion_cache`). The get_* commands read this instantly instead of
    /// recomputing full-library passes per tab-open while holding the DB lock.
    suggestion_cache: Arc<Mutex<SuggestionCache>>,
    /// Debounce token for `schedule_refold`: only the newest pending request fires.
    recluster_epoch: Arc<AtomicU64>,
    /// True while a focus-review session is open. The debounced self-heal pass is
    /// held during a session — it re-derives tentative folds, which would change the
    /// remaining cards' contents mid-answer; answers apply instantly either way.
    review_active: Arc<AtomicBool>,
    /// Set when a due self-heal pass was held by an active review session, so it
    /// runs as soon as the session ends.
    recluster_deferred: Arc<AtomicBool>,
}

/// The People suggestions as of one clustering generation. Served only while
/// `generation` still matches `cluster_gen` — a mismatch means clustering moved on,
/// and serving nothing beats serving cards whose cluster ids now point elsewhere.
#[derive(Default)]
struct SuggestionCache {
    generation: i64,
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

/// Remove the cached cover-crop files for a set of faces. Call BEFORE deleting the
/// face rows (the crop path is keyed by face id, found via the rows) — pruned and
/// removed photos used to leave their crops on disk forever.
fn delete_face_crop_files(faces_dir: &Path, face_ids: &[i64]) {
    for &fid in face_ids {
        let _ = std::fs::remove_file(faces::face_crop_path(faces_dir, fid));
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
    let faces_dir = state.faces_dir.clone();
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
            if scan::run_scan(&db_path, root, gen, queue.clone(), &preview_dir, &faces_dir, move |found, _| {
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
                // Crops first — they're found via the face rows about to go.
                let face_ids = db::face_ids_of_photos(&conn, &removed).unwrap_or_default();
                delete_face_crop_files(&faces_dir, &face_ids);
                let _ = db::delete_faces_for_photos(&conn, &removed);
                delete_cache_files(&cache_dir, &preview_dir, &removed);
            }
            let total = db::stats(&conn).map(|s| s.0).unwrap_or(0);
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
    favorites: i64,
    hidden: i64,
}

#[tauri::command]
fn get_library_stats(state: tauri::State<'_, AppState>) -> Result<LibraryStats, String> {
    let conn = state.conn.lock().unwrap();
    let (total, ready, favorites, hidden) = db::stats(&conn).map_err(|e| e.to_string())?;
    Ok(LibraryStats { total, ready, favorites, hidden })
}

/// Fetch a contiguous window of photo rows (id + thumbnail status) under a
/// curation filter ("visible" | "favorites" | "hidden"), in discovery or date
/// order. The virtualized grid asks for only the ranges it is about to display.
#[tauri::command]
fn get_photos_range(
    state: tauri::State<'_, AppState>,
    offset: i64,
    limit: i64,
    by_date: bool,
    filter: Option<String>,
) -> Result<Vec<db::PhotoRow>, String> {
    let f = db::PhotoFilter::parse(filter.as_deref().unwrap_or("visible"));
    let conn = state.conn.lock().unwrap();
    db::photos_range(&conn, offset, limit, by_date, f).map_err(|e| e.to_string())
}

/// "On this day" — photos taken on today's month-and-day in past years, for the
/// Home shelf. Empty when nothing was captured on this date.
#[tauri::command]
fn get_on_this_day(state: tauri::State<'_, AppState>) -> Result<Vec<db::PhotoRow>, String> {
    let conn = state.conn.lock().unwrap();
    db::on_this_day(&conn).map_err(|e| e.to_string())
}

/// Toggle a photo's favorite star.
#[tauri::command]
fn set_photo_favorite(state: tauri::State<'_, AppState>, id: i64, favorite: bool) -> Result<(), String> {
    let conn = state.conn.lock().unwrap();
    db::set_favorite(&conn, id, favorite).map_err(|e| e.to_string())
}

/// Soft-archive (or restore) a photo — a flag only; the file is never touched.
#[tauri::command]
fn set_photo_hidden(state: tauri::State<'_, AppState>, id: i64, hidden: bool) -> Result<(), String> {
    let conn = state.conn.lock().unwrap();
    db::set_hidden(&conn, id, hidden).map_err(|e| e.to_string())
}

/// One JSON line per flagged photo — the curation snapshot the user owns.
#[derive(serde::Serialize, serde::Deserialize)]
struct CurationEntry {
    path: String,
    favorite: bool,
    hidden: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CurationFile {
    /// Bumped if the shape ever changes; readers tolerate what they understand.
    version: u32,
    entries: Vec<CurationEntry>,
}

/// Write the favorites + hidden flags to a JSON file the user chooses, so their
/// curation outlives the app's cache directory (it's the one thing here that
/// isn't re-derivable from the photos). Keyed by path.
#[tauri::command]
fn export_curation(state: tauri::State<'_, AppState>, path: String) -> Result<usize, String> {
    let rows = {
        let conn = state.conn.lock().unwrap();
        db::curation_rows(&conn).map_err(|e| e.to_string())?
    };
    let file = CurationFile {
        version: 1,
        entries: rows
            .into_iter()
            .map(|(path, favorite, hidden)| CurationEntry { path, favorite, hidden })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    let n = file.entries.len();
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(n)
}

/// Read a previously exported curation file and merge it into the library by path
/// (flags are OR-merged — an import never clears a star). Returns how many entries
/// matched a photo present in this library.
#[tauri::command]
fn import_curation(state: tauri::State<'_, AppState>, path: String) -> Result<usize, String> {
    let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let file: CurationFile = serde_json::from_str(&json)
        .map_err(|_| "that isn't a Solar curation file".to_string())?;
    let rows: Vec<(String, bool, bool)> = file
        .entries
        .into_iter()
        .map(|e| (e.path, e.favorite, e.hidden))
        .collect();
    let mut conn = state.conn.lock().unwrap();
    db::apply_curation(&mut conn, &rows).map_err(|e| e.to_string())
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
    let preview_dir = state.preview_dir.clone();
    let faces_dir = state.faces_dir.clone();
    std::thread::spawn(move || {
        let gen = now_gen();
        let progress = |found: i64, done: bool| {
            let _ = app.emit("scan-progress", ScanProgress { found, done });
        };
        if let Err(e) = scan::run_scan(&db_path, &path, gen, queue, &preview_dir, &faces_dir, progress) {
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
        // Crops first — they're found via the face rows about to go.
        let face_ids = db::face_ids_of_photos(&conn, &ids).unwrap_or_default();
        delete_face_crop_files(&state.faces_dir, &face_ids);
        let _ = db::delete_faces_for_photos(&conn, &ids);
        ids
    };
    delete_cache_files(&state.cache_dir, &state.preview_dir, &removed);
    let total = {
        let conn = state.conn.lock().unwrap();
        db::stats(&conn).map(|s| s.0).unwrap_or(0)
    };
    let _ = app.emit("scan-progress", ScanProgress { found: total, done: true });
}

/// Detail for the viewer chrome: filename + a timestamp (capture date when we
/// have it, else file mtime).
#[derive(serde::Serialize)]
struct PhotoDetail {
    filename: String,
    /// Full path on disk — backs the viewer's "Show in Finder".
    path: String,
    timestamp: i64,
    favorite: bool,
    hidden: bool,
}

#[tauri::command]
fn get_photo_detail(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<Option<PhotoDetail>, String> {
    let conn = state.conn.lock().unwrap();
    let detail = db::detail(&conn, id).map_err(|e| e.to_string())?;
    Ok(detail.map(|(path, timestamp, favorite, hidden)| {
        let filename = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        PhotoDetail { filename, path, timestamp, favorite, hidden }
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
    // the priority lane so they load before the background backfill (which stays
    // queued in the normal lane rather than being dropped).
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

/// The detected people (clusters), biggest first, with a cover face each.
#[tauri::command]
fn get_clusters(state: tauri::State<'_, AppState>) -> Result<Vec<db::ClusterRow>, String> {
    let conn = state.conn.lock().unwrap();
    db::clusters_overview(&conn).map_err(|e| e.to_string())
}

/// What naming returns: the canonical group key to keep following (naming a
/// positive group promotes it to a durable negative key) plus the undo token.
#[derive(serde::Serialize)]
struct NameOutcome {
    group: i64,
    undo: CorrectionUndo,
}

/// Naming is the highest-stakes mutation — it confirms every face in the cluster
/// as user-vouched exemplars — and the cluster id was loaded from an earlier
/// people list, so it needs the same staleness guard as the suggestion paths: a
/// re-cluster between load and commit renumbers ids, and naming whatever cluster
/// now holds the stale id would durably confirm a stranger's faces under the name.
#[tauri::command]
fn name_cluster(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
    name: String,
    expected_generation: Option<i64>,
) -> Result<NameOutcome, String> {
    let outcome = {
        let conn = lock_checked(&state, expected_generation)?;
        // Captured BEFORE the write: naming confirms the group's faces (and may
        // bind them to a fresh identity), and the identity may already carry a
        // name — both must round-trip through undo.
        let prior = capture_group_states(&conn, &[cluster_id]).map_err(|e| e.to_string())?;
        let prior_name = db::group_name(&conn, cluster_id).map_err(|e| e.to_string())?;
        let group = db::name_group(&conn, cluster_id, &name).map_err(|e| e.to_string())?;
        // A negative result means an identity's name was written (or cleared);
        // a positive one means nothing happened (clearing an unnamed group).
        let renamed = (group < 0).then(|| (-group, prior_name));
        NameOutcome { group, undo: CorrectionUndo { renamed, ..CorrectionUndo::faces_only(prior) } }
    };
    // Confirming a person adds exemplars, which can re-home other people's
    // wrongly-folded look-alikes — so re-derive the folds competitively (self-heal).
    if !name.trim().is_empty() {
        schedule_refold(app);
    }
    Ok(outcome)
}

#[tauri::command]
fn merge_clusters(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    into: i64,
    from: i64,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        // If exactly one side carries a name, that side survives — folding a named
        // person INTO an unnamed pile would silently un-name them.
        let (into, from) = if db::group_name(&conn, from).map_err(|e| e.to_string())?.is_some()
            && db::group_name(&conn, into).map_err(|e| e.to_string())?.is_none()
        {
            (from, into)
        } else {
            (into, from)
        };
        let prior = capture_group_states(&conn, &[into, from]).map_err(|e| e.to_string())?;
        // A user merge vouches for BOTH sides as one person — everything under the
        // surviving identity is confirmed (sticky exemplars + must-links) after the
        // fold. Confirming only one side let the next pass split the other right
        // back off, and the same "same person?" card returned — the "didn't my
        // answer register?" bug.
        let into_identity =
            db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
        db::merge_group_into_identity(&conn, into_identity, from).map_err(|e| e.to_string())?;
        db::confirm_identity_faces(&conn, into_identity).map_err(|e| e.to_string())?;
        CorrectionUndo::faces_only(prior)
    };
    prune_suggestion_cache(&state, &[into, from]);
    // The merge added exemplars — re-derive the folds competitively (self-heal).
    schedule_refold(app);
    Ok(undo)
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

/// The person's "looks" strip (see `recognition::person_looks`). Runs on a
/// blocking-pool thread with its OWN connection: the leader-clustering over a big
/// person's thousands of embeddings takes real time, and computing it while
/// holding the shared UI connection stalled every avatar request behind the lock
/// (the same disease the suggestion cache cured for the People tab).
#[tauri::command]
async fn get_person_looks(
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
) -> Result<Vec<PersonLook>, String> {
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db::open(&db_path).map_err(|e| e.to_string())?;
        recognition::person_looks(&conn, cluster_id)
    })
    .await
    .map_err(|e| e.to_string())?
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
/// the new person's group key when one was created, any cannot-link we added, and
/// any same-photo exceptions we added (cluster-level review answers use these too).
#[derive(Clone, serde::Serialize)]
struct CorrectionUndo {
    prior: Vec<db::FaceState>,
    new_cluster_id: Option<i64>,
    added_cannot_link: Option<(i64, i64)>,
    /// Multi-pair form (a "neither of them" answer cannot-links against each
    /// candidate); kept alongside the single-pair field the older paths use.
    added_cannot_links: Vec<(i64, i64)>,
    added_same_photo_ok: Vec<(i64, i64)>,
    /// A name this action wrote: the identity and its name BEFORE the action
    /// (`None` = it was unnamed). Restoring face states alone would leave the
    /// label behind — and naming is what confirms faces as user-vouched, so an
    /// undo that kept it would forge exemplars out of a taken-back action.
    renamed: Option<(i64, Option<String>)>,
}

impl CorrectionUndo {
    fn faces_only(prior: Vec<db::FaceState>) -> Self {
        CorrectionUndo {
            prior,
            new_cluster_id: None,
            added_cannot_link: None,
            added_cannot_links: Vec::new(),
            added_same_photo_ok: Vec::new(),
            renamed: None,
        }
    }
}

/// Snapshot the face states of whole groups — what a cluster-level answer (merge /
/// absorb / reject / not-this-person / same-photo) needs captured for exact undo.
/// Chunked: a big person can hold thousands of faces, and SQLite caps the variables
/// one `IN (…)` may carry.
fn capture_group_states(
    conn: &rusqlite::Connection,
    groups: &[i64],
) -> anyhow::Result<Vec<db::FaceState>> {
    let mut ids: Vec<i64> = Vec::new();
    for &g in groups {
        ids.extend(db::cluster_face_ids(conn, g)?);
    }
    ids.sort_unstable();
    ids.dedup();
    let mut states = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(900) {
        states.extend(db::capture_face_states(conn, chunk)?);
    }
    Ok(states)
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
    let mut conn = lock_checked(&state, expected_generation)?;
    let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
    // Both sides become durable identities; record "not the same" between them.
    let source_id =
        db::ensure_identity_for_group(&conn, source_cluster_id).map_err(|e| e.to_string())?;
    let target_id =
        db::ensure_identity_for_group(&conn, target_cluster_id).map_err(|e| e.to_string())?;
    db::set_faces_person(&mut conn, &face_ids, target_id).map_err(|e| e.to_string())?;
    let added = record_cannot_link_if_new(&conn, source_id, target_id).map_err(|e| e.to_string())?;
    Ok(CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) })
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
    let mut conn = lock_checked(&state, expected_generation)?;
    let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
    let source_id =
        db::ensure_identity_for_group(&conn, source_cluster_id).map_err(|e| e.to_string())?;
    // If the typed name is already a person, merge into them instead of minting a
    // duplicate — moving "this is someone else: Mía" twice shouldn't make two Mías.
    let trimmed = name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(nm) = trimmed {
        if let Some(target) = db::group_for_name(&conn, nm).map_err(|e| e.to_string())? {
            if target != source_cluster_id {
                let target_id =
                    db::ensure_identity_for_group(&conn, target).map_err(|e| e.to_string())?;
                db::set_faces_person(&mut conn, &face_ids, target_id).map_err(|e| e.to_string())?;
                let added = record_cannot_link_if_new(&conn, source_id, target_id).map_err(|e| e.to_string())?;
                return Ok(CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) });
            }
        }
    }
    // Mint the durable identity for the split person and bind the faces to it —
    // the new tile lives under the identity's stable (negative) group key.
    let new_id = db::new_identity(&conn).map_err(|e| e.to_string())?;
    db::set_faces_person(&mut conn, &face_ids, new_id).map_err(|e| e.to_string())?;
    if let Some(nm) = trimmed {
        let _ = db::name_group(&conn, -new_id, nm).map_err(|e| e.to_string())?;
    }
    let added = record_cannot_link_if_new(&conn, source_id, new_id).map_err(|e| e.to_string())?;
    Ok(CorrectionUndo {
        new_cluster_id: Some(-new_id),
        added_cannot_link: added,
        // The fresh identity's name (when one was given) is this action's write —
        // undo clears it so no named ghost lingers for merge-by-name lookups.
        renamed: trimmed.map(|_| (new_id, None)),
        ..CorrectionUndo::faces_only(prior)
    })
}

/// Every face in a cluster (face ids, best first) — backs the "Who is this?" split
/// grid, where the user tags each contested face as one candidate or the other and so
/// needs the whole cluster on screen, not the 3-face sample the card ships with.
#[tauri::command]
fn get_cluster_faces(
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
) -> Result<Vec<i64>, String> {
    let conn = state.conn.lock().unwrap();
    db::cluster_face_ids(&conn, cluster_id).map_err(|e| e.to_string())
}

/// Confirm a subset of faces into an existing person, leaving the rest of their
/// current cluster untouched. Backs the "Who is this?" split: a contested cluster
/// holds two people, so the user tags some faces as A and some as B and each batch is
/// confirmed into that person. Unlike [`reassign_faces_to_cluster`] this records **no**
/// cannot-link against the source — the source is an ephemeral contested cluster, and
/// cannot-linking its untagged remainder from both people would strand faces that are
/// in fact one of them, just not tagged this round. Kicks a (review-deferred)
/// re-cluster so the remainder re-folds. Returns prior state for exact undo.
#[tauri::command]
fn confirm_faces_into_cluster(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
    target_cluster_id: i64,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    {
        let mut conn = lock_checked(&state, expected_generation)?;
        let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
        let target_id =
            db::ensure_identity_for_group(&conn, target_cluster_id).map_err(|e| e.to_string())?;
        db::set_faces_person(&mut conn, &face_ids, target_id).map_err(|e| e.to_string())?;
        drop(conn);
        schedule_refold(app);
        Ok(CorrectionUndo::faces_only(prior))
    }
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
    Ok(CorrectionUndo::faces_only(prior))
}

/// "Not this person" without naming who they are: unbind the faces from their
/// current person and let the self-heal pass re-home each by appearance (possibly
/// several people, or none). Distinct from "move to a new person" (which forces
/// them together) and "ignore" (which hides them). Returns prior state for exact
/// undo — nothing but the identity layer moved.
#[tauri::command]
fn detach_faces(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = state.conn.lock().unwrap();
        let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
        db::detach_faces(&conn, &face_ids).map_err(|e| e.to_string())?;
        CorrectionUndo::faces_only(prior)
    };
    schedule_refold(app);
    Ok(undo)
}

/// Undo any correction: restore the faces' prior grouping and drop any cannot-link
/// or same-photo exceptions the correction added. Re-derives the folds afterward so
/// the display reflects the restored state (deferred while a review session holds).
#[tauri::command]
fn undo_correction(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    undo: CorrectionUndoArg,
) -> Result<(), String> {
    {
        let mut conn = state.conn.lock().unwrap();
        db::restore_face_states(&mut conn, &undo.prior).map_err(|e| e.to_string())?;
        if let Some((a, b)) = undo.added_cannot_link {
            db::remove_cannot_link(&conn, a, b).map_err(|e| e.to_string())?;
        }
        for &(a, b) in &undo.added_cannot_links {
            db::remove_cannot_link(&conn, a, b).map_err(|e| e.to_string())?;
        }
        db::remove_same_photo_ok(&conn, &undo.added_same_photo_ok).map_err(|e| e.to_string())?;
        if let Some((identity, name)) = &undo.renamed {
            db::restore_identity_name(&conn, *identity, name.as_deref())
                .map_err(|e| e.to_string())?;
        }
    }
    schedule_refold(app);
    Ok(())
}

/// Inbound form of [`CorrectionUndo`] (the frontend hands back what a correction
/// returned). `new_cluster_id` isn't needed to undo, so it's omitted.
#[derive(serde::Deserialize)]
struct CorrectionUndoArg {
    prior: Vec<db::FaceState>,
    added_cannot_link: Option<(i64, i64)>,
    #[serde(default)]
    added_cannot_links: Vec<(i64, i64)>,
    #[serde(default)]
    added_same_photo_ok: Vec<(i64, i64)>,
    #[serde(default)]
    renamed: Option<(i64, Option<String>)>,
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

/// The cached growth cards from the last clustering pass. Instant — the heavy
/// pass ran in the background when clustering settled. Empty while a pass is
/// running or the cache is from an older generation (no stale cards).
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
) -> Result<CorrectionUndo, String> {
    let mut touched = clusters.clone();
    touched.push(into);
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &touched).map_err(|e| e.to_string())?;
        let into_identity =
            db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
        for from in clusters {
            if from == into {
                continue;
            }
            // Defense in depth (the suggestion pass already filters these): never
            // absorb a group that IS a different named person. Unnamed-competitor
            // confirmations are adopted instead — the user is explicitly assigning
            // this group, which outranks that bookkeeping.
            if db::group_is_other_named_person(&conn, from, Some(into_identity))
                .map_err(|e| e.to_string())?
            {
                continue;
            }
            db::adopt_unnamed_confirmed(&conn, from, into_identity).map_err(|e| e.to_string())?;
            // The user vouched for each absorbed group — confirm, then fold in.
            db::confirm_group_faces(&conn, from, Some(into_identity))
                .map_err(|e| e.to_string())?;
            db::merge_group_into_identity(&conn, into_identity, from)
                .map_err(|e| e.to_string())?;
        }
        CorrectionUndo::faces_only(prior)
    };
    prune_suggestion_cache(&state, &touched);
    // Bulk-merging added exemplars — re-derive the folds competitively (self-heal).
    schedule_refold(app);
    Ok(undo)
}

/// "Not the same" on a merge prompt: record a durable cannot-link so the pair is
/// never suggested again (survives re-clusters, unlike a dismissed-in-memory card).
/// Both sides become durable *competitors* — their faces are confirmed under their
/// (possibly unnamed) identities. Without that, the minted identity bindings were
/// unconfirmed, `clear_unconfirmed_identities` wiped them on the very next pass,
/// the cannot-link no longer matched either cluster, and the same "same person?"
/// card came straight back — rejections between unnamed groups never stuck.
#[tauri::command]
fn reject_merge(
    state: tauri::State<'_, AppState>,
    into: i64,
    from: i64,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &[into, from]).map_err(|e| e.to_string())?;
        let ia = db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
        let ib = db::ensure_identity_for_group(&conn, from).map_err(|e| e.to_string())?;
        let added = record_cannot_link_if_new(&conn, ia, ib).map_err(|e| e.to_string())?;
        db::confirm_identity_faces(&conn, ia).map_err(|e| e.to_string())?;
        db::confirm_identity_faces(&conn, ib).map_err(|e| e.to_string())?;
        CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) }
    };
    prune_suggestion_cache(&state, &[into, from]);
    Ok(undo)
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
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &[person_cluster_id, other_cluster_id])
            .map_err(|e| e.to_string())?;
        // Mint identities for both sides + cannot-link, then confirm the rejected group
        // so it's a durable, competing exemplar (not wiped as a tentative machine label).
        let person =
            db::ensure_identity_for_group(&conn, person_cluster_id).map_err(|e| e.to_string())?;
        let other =
            db::ensure_identity_for_group(&conn, other_cluster_id).map_err(|e| e.to_string())?;
        let added = record_cannot_link_if_new(&conn, person, other).map_err(|e| e.to_string())?;
        db::confirm_identity_faces(&conn, other).map_err(|e| e.to_string())?;
        CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) }
    };
    prune_suggestion_cache(&state, &[person_cluster_id, other_cluster_id]);
    schedule_refold(app);
    Ok(undo)
}

/// "Someone else" WITHOUT saying who: the contested group is none of the offered
/// candidates, and the user can't (or won't) name them right now. Cannot-link the
/// group from every candidate and confirm it as its own durable *unnamed*
/// competitor — it stops being suggested as any of them, pulls its look-alikes
/// away, and sits in People as an unnamed tile to name later (or never). The
/// answer that was missing between "it's X" and "skip forever".
#[tauri::command]
fn not_these_people(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    other_cluster_id: i64,
    person_cluster_ids: Vec<i64>,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let mut touched = person_cluster_ids.clone();
    touched.push(other_cluster_id);
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &touched).map_err(|e| e.to_string())?;
        let other =
            db::ensure_identity_for_group(&conn, other_cluster_id).map_err(|e| e.to_string())?;
        let mut added = Vec::new();
        for p in &person_cluster_ids {
            let pid = db::ensure_identity_for_group(&conn, *p).map_err(|e| e.to_string())?;
            if let Some(pair) =
                record_cannot_link_if_new(&conn, other, pid).map_err(|e| e.to_string())?
            {
                added.push(pair);
            }
        }
        db::confirm_identity_faces(&conn, other).map_err(|e| e.to_string())?;
        CorrectionUndo { added_cannot_links: added, ..CorrectionUndo::faces_only(prior) }
    };
    prune_suggestion_cache(&state, &touched);
    schedule_refold(app);
    Ok(undo)
}

/// Resolve the bundled offline basemap (a PMTiles archive shipped as a Tauri
/// resource — see scripts/fetch-basemap.sh). The Places map reads it by byte
/// range, so no tile server is ever contacted: pans and zooms stay on-device.
fn basemap_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .resolve("basemap/world.pmtiles", tauri::path::BaseDirectory::Resource)
        .map_err(|e| e.to_string())
}

/// Size of the bundled basemap in bytes — also the frontend's "is a basemap
/// bundled at all?" probe (0 = missing; the tab explains instead of breaking).
#[tauri::command]
fn basemap_size(app: tauri::AppHandle) -> Result<u64, String> {
    Ok(basemap_path(&app)
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0))
}

/// One byte range of the bundled basemap, returned raw (`tauri::ipc::Response`,
/// no JSON encode). The PMTiles reader asks for tiny slices — header,
/// directories, then one tile at a time — so this is called per tile view.
#[tauri::command]
fn read_basemap_range(
    app: tauri::AppHandle,
    offset: u64,
    length: u64,
) -> Result<tauri::ipc::Response, String> {
    use std::io::{Read, Seek, SeekFrom};
    // A vector tile is at most a few MB; anything bigger is a confused caller.
    if length > 32 * 1024 * 1024 {
        return Err("range too large".into());
    }
    let path = basemap_path(&app)?;
    let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; length as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..]).map_err(|e| e.to_string())?;
        if n == 0 {
            break; // EOF — final range of the file may run short
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(tauri::ipc::Response::new(buf))
}

/// One located photo on the Places map: position + the timeline's sort stamp
/// (so the map can be time-filtered later without a second query).
#[derive(serde::Serialize)]
struct GeoPoint {
    id: i64,
    lat: f64,
    lon: f64,
    ts: i64,
}

/// Every photo with a GPS fix — the Places map's whole dataset in one read.
/// Compact by design (four numbers a row): 100k points is a ~3MB payload, and
/// clustering happens client-side where the viewport lives (Principle 6).
#[tauri::command]
fn get_geo_points(state: tauri::State<'_, AppState>) -> Result<Vec<GeoPoint>, String> {
    let conn = state.conn.lock().unwrap();
    Ok(db::geo_points(&conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(id, lat, lon, ts)| GeoPoint { id, lat, lon, ts })
        .collect())
}

/// The photo behind a face crop, plus the face's normalized box — backs the
/// "peek at the full picture" affordance on review chips and cards, where a
/// tight crop alone often isn't enough to tell who someone is (the context —
/// who else is in the frame, where — is the identifying signal).
#[derive(serde::Serialize)]
struct FacePhoto {
    photo_id: i64,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

#[tauri::command]
fn get_face_photo(
    state: tauri::State<'_, AppState>,
    face_id: i64,
) -> Result<Option<FacePhoto>, String> {
    let conn = state.conn.lock().unwrap();
    Ok(db::face_box(&conn, face_id)
        .map_err(|e| e.to_string())?
        .map(|(photo_id, x1, y1, x2, y2)| FacePhoto { photo_id, x1, y1, x2, y2 }))
}

/// Name (or assign to an existing person, matched by exact name) a handful of
/// faces — WITHOUT touching the rest of their cluster and WITHOUT a cannot-link.
/// The lightbox's "just this face" scope: on a junk cluster (pose-blended
/// profiles), naming one face must not vouch for hundreds of strangers along
/// with it. The rest of the cluster re-homes competitively on later passes; the
/// named face becomes one confirmed exemplar (no magnet authority until
/// MIN_ANCHOR confirmed faces accumulate — see recognition::MIN_ANCHOR).
#[tauri::command]
fn name_faces(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
    name: String,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("a name is required".into());
    }
    let undo = {
        let mut conn = lock_checked(&state, expected_generation)?;
        let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
        // An existing person with this exact name adopts the faces; otherwise a
        // fresh identity is minted and named — and that name is this action's
        // write, so undo clears it (no named ghost for merge-by-name lookups).
        let (identity, renamed) = if let Some(group) =
            db::group_for_name(&conn, trimmed).map_err(|e| e.to_string())?
        {
            (db::ensure_identity_for_group(&conn, group).map_err(|e| e.to_string())?, None)
        } else {
            let id = db::new_identity(&conn).map_err(|e| e.to_string())?;
            let _ = db::name_group(&conn, -id, trimmed).map_err(|e| e.to_string())?;
            (id, Some((id, None)))
        };
        db::set_faces_person(&mut conn, &face_ids, identity).map_err(|e| e.to_string())?;
        CorrectionUndo {
            new_cluster_id: Some(-identity),
            renamed,
            ..CorrectionUndo::faces_only(prior)
        }
    };
    schedule_refold(app);
    Ok(undo)
}

/// "Not this person" for a whole batch of candidate groups at once — the person
/// page's review band offers "none of these are <name>". Same semantics as
/// [`not_this_person`] per group (cannot-link + durable competitor), but captured
/// as ONE undoable action, and the person's own face states are snapshotted once
/// instead of per group.
#[tauri::command]
fn not_this_person_many(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    person_cluster_id: i64,
    other_cluster_ids: Vec<i64>,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let mut touched = other_cluster_ids.clone();
    touched.push(person_cluster_id);
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &touched).map_err(|e| e.to_string())?;
        let person =
            db::ensure_identity_for_group(&conn, person_cluster_id).map_err(|e| e.to_string())?;
        let mut added = Vec::new();
        for o in &other_cluster_ids {
            let oid = db::ensure_identity_for_group(&conn, *o).map_err(|e| e.to_string())?;
            if let Some(pair) =
                record_cannot_link_if_new(&conn, person, oid).map_err(|e| e.to_string())?
            {
                added.push(pair);
            }
            db::confirm_identity_faces(&conn, oid).map_err(|e| e.to_string())?;
        }
        CorrectionUndo { added_cannot_links: added, ..CorrectionUndo::faces_only(prior) }
    };
    prune_suggestion_cache(&state, &touched);
    schedule_refold(app);
    Ok(undo)
}

/// Resolve a same-photo contradiction (see [`ReviewItem::SamePhotoTwin`]).
/// `same_person = true`: it's a collage/mirror — record durable per-pair exceptions
/// for every co-occurring face pair between the two clusters, then confirm + merge
/// (the exceptions are what let the next re-cluster keep them together).
/// `same_person = false`: they're two look-alikes (twins) — durable cannot-link.
#[tauri::command]
fn resolve_same_photo(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    into: i64,
    from: i64,
    same_person: bool,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &[into, from]).map_err(|e| e.to_string())?;
        if same_person {
            // Resolve the blocked face pairs BEFORE any identity minting shifts
            // the positive group key out from under `cooccurring_face_pairs`.
            let pairs: Vec<(i64, i64)> = db::cooccurring_face_pairs(&conn, into, from)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|(_, a, b)| (a, b))
                .collect();
            let into_identity =
                db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
            // Only a *named* other person blocks the assignment. Unnamed
            // competitors (minted by earlier rejections) are adopted instead —
            // refusing on them made this card unanswerable forever: every click
            // failed, the queue refreshed, and the same card came back on top.
            if db::group_is_other_named_person(&conn, from, Some(into_identity))
                .map_err(|e| e.to_string())?
            {
                return Err("that group already belongs to another named person".into());
            }
            let added_ok =
                db::add_same_photo_ok_returning_new(&conn, &pairs).map_err(|e| e.to_string())?;
            db::adopt_unnamed_confirmed(&conn, from, into_identity).map_err(|e| e.to_string())?;
            db::merge_group_into_identity(&conn, into_identity, from)
                .map_err(|e| e.to_string())?;
            // Vouch for the united person so the pairing survives self-heal.
            db::confirm_identity_faces(&conn, into_identity).map_err(|e| e.to_string())?;
            CorrectionUndo { added_same_photo_ok: added_ok, ..CorrectionUndo::faces_only(prior) }
        } else {
            // Two look-alikes: durable cannot-link, both sides durable competitors
            // (same rationale as reject_merge — unconfirmed bindings evaporate).
            let ia = db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
            let ib = db::ensure_identity_for_group(&conn, from).map_err(|e| e.to_string())?;
            let added = record_cannot_link_if_new(&conn, ia, ib).map_err(|e| e.to_string())?;
            db::confirm_identity_faces(&conn, ia).map_err(|e| e.to_string())?;
            db::confirm_identity_faces(&conn, ib).map_err(|e| e.to_string())?;
            CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) }
        }
    };
    prune_suggestion_cache(&state, &[into, from]);
    schedule_refold(app);
    Ok(undo)
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
/// Set once HEIC faces have been re-detected from the correctly-oriented decode.
/// Bumping this string re-runs the orientation repair (see the migration in `setup`).
const HEIC_ORIENT_FLAG: &str = "heic_orient_v1";
/// Set once the library has been re-clustered under identity-centric grouping.
/// Before it, auto-folds physically merged clusters, so the appearance layer of an
/// existing library carries welded multi-person piles; one full re-cluster rebuilds
/// it pure (display groups are identity-keyed and unaffected throughout).
const GROUPING_FLAG: &str = "identity_grouping_v1";

/// Progress of a background re-cluster. `running` flips false when it finishes, so
/// the People view can reload exactly once (and never mid-rebuild → no reflow).
#[derive(Clone, serde::Serialize)]
struct ClusterProgress {
    running: bool,
    fraction: f32,
}

/// Recompute the suggestion caches (pairwise merges + identity growth) after a
/// clustering pass. Runs on the pass's background thread with its own connection,
/// so the UI's shared connection is never held through the matrix math (the old
/// per-tab-open compute stalled every avatar request behind that lock). The get_*
/// commands then serve instant reads. Stamps the CURRENT generation: only a full
/// re-cluster (which renumbers positive appearance keys) bumps it — a fold pass
/// changes group memberships but moves no ids, so cards stay actionable across it.
fn refresh_suggestion_cache(app: &AppHandle, conn: &Connection) {
    let state = app.state::<AppState>();
    let generation = state.cluster_gen.load(Ordering::SeqCst);
    let (mut merges, twins) = compute_merge_suggestions(conn).unwrap_or_default();
    let (mut growth, who) = compute_identity_growth(conn).unwrap_or_default();
    for s in &mut merges {
        s.generation = generation;
    }
    for g in &mut growth {
        g.generation = generation;
    }
    let mut special = who;
    special.extend(twins);
    // The pairwise merges feed the queue but aren't served on their own, so they
    // aren't cached — the queue items carry everything the review flow needs.
    let queue = build_review_queue(&merges, &growth, special);
    *state.suggestion_cache.lock().unwrap() = SuggestionCache { generation, growth, queue };
}

/// Guard for mutations holding positive (appearance) group keys: a full re-cluster
/// reassigns those from scratch, so a card computed before one completes may now
/// point at a different group of faces — acting on it would confirm/merge the wrong
/// people (durably). Identity keys (negative) never go stale, and fold passes move
/// no ids, so with identity-centric grouping this guard only ever fires around the
/// rare consolidation re-cluster. `None` is not checked.
/// Drop cached suggestions touching any of these clusters, immediately after a user
/// decision lands. The full cache only refreshes when the (debounced) clustering
/// pass completes seconds later; until then the stale cards would re-offer
/// decisions the user already made — closing and reopening Review showed the same
/// "merge all?" again, reading as "my answer didn't register." Over-pruning is
/// safe: anything still relevant is regenerated by the next pass.
fn prune_suggestion_cache(state: &AppState, clusters: &[i64]) {
    use std::collections::HashSet;
    let set: HashSet<i64> = clusters.iter().copied().collect();
    let touches = |item: &ReviewItem| -> bool {
        match item {
            ReviewItem::StrongBatch { into, groups, .. } => {
                set.contains(into) || groups.iter().any(|g| set.contains(&g.cluster_id))
            }
            ReviewItem::Maybe { into, group, .. } => {
                set.contains(into) || set.contains(&group.cluster_id)
            }
            ReviewItem::WhoIsThis { cluster_id, candidates, .. } => {
                set.contains(cluster_id) || candidates.iter().any(|c| set.contains(&c.into))
            }
            ReviewItem::Pairwise { into, from, .. } => set.contains(into) || set.contains(from),
            ReviewItem::SamePhotoTwin { pairs, .. } => {
                pairs.iter().any(|p| set.contains(&p.into) || set.contains(&p.from))
            }
        }
    };
    let mut cache = state.suggestion_cache.lock().unwrap();
    cache.queue.retain(|i| !touches(i));
    for g in cache.growth.iter_mut() {
        g.strong_groups.retain(|x| !set.contains(&x.cluster_id));
        g.strong_clusters.retain(|c| !set.contains(c));
        g.maybe.retain(|x| !set.contains(&x.cluster_id));
    }
    cache
        .growth
        .retain(|g| !set.contains(&g.into) && !(g.strong_clusters.is_empty() && g.maybe.is_empty()));
}

fn ensure_generation(state: &AppState, expected: Option<i64>) -> Result<(), String> {
    match expected {
        Some(g) if g != state.cluster_gen.load(Ordering::SeqCst) => {
            Err("stale suggestion: people were reorganized since it was shown".into())
        }
        _ => Ok(()),
    }
}

/// Acquire the UI connection AND verify the generation *under that lock*. The
/// re-cluster bumps the generation and commits its renumbering while holding
/// this same lock (see `run_recluster`), so a mutation either ran entirely
/// before the renumbering or sees the new generation and is refused — checking
/// before locking left a window where a stale card slipped through and wrote
/// against freshly-renumbered ids.
fn lock_checked<'a>(
    state: &'a AppState,
    expected: Option<i64>,
) -> Result<std::sync::MutexGuard<'a, Connection>, String> {
    let conn = state.conn.lock().unwrap();
    ensure_generation(state, expected)?;
    Ok(conn)
}

/// How long a burst of corrections may extend before the self-heal pass runs.
/// Sized for the *rhythm of reviewing*, not a single click: after accepting a few
/// suggestions on one person and returning to the grid, the user needs time to read
/// the badges and open the next person before the pass fires and "Reorganizing
/// people…" wipes the grid out from under them. A short window fired between people
/// (the "I can only check one person at a time" complaint); this lets a whole batch
/// of 5–10 people flow, coalescing into one reorganize once they actually pause.
/// The badges themselves stay correct throughout regardless — each correction prunes
/// the suggestion cache synchronously; only the deeper re-derive waits for this.
/// (While a person's page or the focus-review session is open, the pass is deferred
/// outright via `review_active`, so this window only governs idle time on the grid.)
const REFOLD_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(20);

/// Debounced [`run_auto_fold`]: a correction's DB writes apply immediately, but the
/// self-heal re-derive waits for a quiet moment, so a review session (answer,
/// answer, answer) pays for one pass instead of one per click. Each call supersedes
/// any still-pending one. This is the cheap identity-layer pass — the full
/// re-cluster now runs only when the appearance layer itself must change (sweep
/// drain, reset, migration, the manual command), never as the price of a rename.
fn schedule_refold(app: AppHandle) {
    let state = app.state::<AppState>();
    let epoch = state.recluster_epoch.clone();
    let review_active = state.review_active.clone();
    let deferred = state.recluster_deferred.clone();
    let mine = epoch.fetch_add(1, Ordering::SeqCst) + 1;
    drop(state);
    std::thread::spawn(move || {
        std::thread::sleep(REFOLD_DEBOUNCE);
        if epoch.load(Ordering::SeqCst) == mine {
            // Mid-review-session: hold the pass (it re-derives folds under the
            // session's remaining cards); set_review_active(false) fires it the
            // moment the session ends.
            if review_active.load(Ordering::SeqCst) {
                deferred.store(true, Ordering::SeqCst);
                return;
            }
            run_auto_fold(app);
        }
    });
}

/// Focus-review session lifecycle: while active, due self-heal passes are deferred
/// so the session's cards stay valid; ending the session runs any deferred pass.
#[tauri::command]
fn set_review_active(app: tauri::AppHandle, state: tauri::State<'_, AppState>, active: bool) {
    state.review_active.store(active, Ordering::SeqCst);
    if !active && state.recluster_deferred.swap(false, Ordering::SeqCst) {
        schedule_refold(app);
    }
}

/// Run [`auto_fold_confident`] in the background (Principle 1: off the UI thread),
/// then signal the People view to refresh via `cluster-progress`. This IS the
/// self-heal pass now: it re-derives every tentative identity assignment against
/// the current confirmed exemplars, at identity-layer cost (anchor matmuls), with
/// no re-cluster. Shares the re-cluster guard so the two never overlap; a request
/// arriving while busy is queued via `fold_pending` so corrections aren't dropped.
fn run_auto_fold(app: AppHandle) {
    let state = app.state::<AppState>();
    if state.reclustering.swap(true, Ordering::SeqCst) {
        state.fold_pending.store(true, Ordering::SeqCst);
        return;
    }
    let db_path = state.db_path.clone();
    let reclustering = state.reclustering.clone();
    let recluster_pending = state.recluster_pending.clone();
    let fold_pending = state.fold_pending.clone();
    drop(state);

    std::thread::spawn(move || {
        background_qos();
        let app_for_rerun = app.clone();
        let _ = app.emit("cluster-progress", ClusterProgress { running: true, fraction: 0.0 });
        let folded = (|| -> anyhow::Result<usize> {
            let conn = db::open(&db_path)?;
            let n = auto_fold_confident(&conn)?;
            // Memberships may have changed — recompute the suggestion caches
            // before the UI is told to reload.
            refresh_suggestion_cache(&app, &conn);
            Ok(n)
        })();
        if let Err(e) = folded {
            eprintln!("auto-fold failed: {e}");
        }
        let _ = app.emit("cluster-progress", ClusterProgress { running: false, fraction: 1.0 });
        reclustering.store(false, Ordering::SeqCst);
        // Anything queued while we ran — a full re-cluster outranks a fold (it
        // ends with one), and folds are frequent now, so dropping a queued
        // consolidation here would starve the sweep's cleanup.
        if recluster_pending.swap(false, Ordering::SeqCst) {
            run_recluster(app_for_rerun);
        } else if fold_pending.swap(false, Ordering::SeqCst) {
            run_auto_fold(app_for_rerun);
        }
    });
}

/// Re-cluster every face from scratch, in the background (Principle 1: off the UI
/// thread, no focus steal). Order-independent and purity-biased. This rebuilds the
/// *appearance* layer only: identity groups (names, confirmations, the tiles the
/// user curated) are keyed by durable identity ids and pass through untouched — no
/// name re-anchoring, no must-link welding. A guard prevents overlap; progress
/// streams via `cluster-progress`.
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
    let fold_pending = state.fold_pending.clone();
    recluster_pending.store(false, Ordering::SeqCst);
    let app_for_rerun = app.clone();
    drop(state);

    std::thread::spawn(move || {
        background_qos();
        let _ = app.emit("cluster-progress", ClusterProgress { running: true, fraction: 0.0 });
        let result = (|| -> anyhow::Result<()> {
            let mut conn = db::open(&db_path)?;
            let faces = db::all_face_embeddings(&conn)?;
            if faces.is_empty() {
                db::set_meta(&conn, RECLUSTER_FLAG, "1")?;
                db::set_meta(&conn, GROUPING_FLAG, "1")?;
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
            // Two identities the user gave *different* names are, by definition,
            // different people — they must never agglomerate into one cluster (which
            // would bury the smaller under the larger's name, e.g. "Mía" vanishing
            // into "Arnaldo"). Naming is the user's decision, so treat every distinct-
            // name pair as an implicit cannot-link, on top of the explicit "not the
            // same" ones. Same-name identities (one person split across two clusters)
            // are left mergeable, so this also helps re-fuse an over-split person.
            let mut cannot_link: std::collections::HashSet<(i64, i64)> =
                db::cannot_link_pairs(&conn)?.into_iter().collect();
            {
                let named = db::named_identities(&conn)?;
                for (i, (ida, na)) in named.iter().enumerate() {
                    for (idb, nb) in named.iter().skip(i + 1) {
                        if na != nb {
                            let key = if ida < idb { (*ida, *idb) } else { (*idb, *ida) };
                            cannot_link.insert(key);
                        }
                    }
                }
            }
            let constraints = cluster::LinkConstraints {
                face_identity: db::confirmed_face_identities(&conn)?.into_iter().collect(),
                cannot_link,
                photo_of,
                same_photo_ok,
            };

            // Throttle progress events to ~every 2% so we don't flood the channel.
            let app2 = app.clone();
            let mut last = 0.0f32;
            let assignments = cluster::recluster(&faces, &constraints, |f| {
                if f - last >= 0.02 || f >= 1.0 {
                    last = f;
                    let _ = app2.emit("cluster-progress", ClusterProgress { running: true, fraction: f });
                }
            });
            // Committing renumbered positive (appearance) keys is the only event
            // that can invalidate a group id the UI holds, so this is the one
            // place the generation bumps. It bumps BEFORE the commit, under the
            // UI connection's lock: every mutation re-checks the generation under
            // that same lock (`lock_checked`), so it either finished before the
            // renumbering or is refused — never both check against the old world
            // and write into the new one. Identity keys pass through untouched:
            // names, confirmations and tiles need no re-anchoring at all.
            {
                let state = app.state::<AppState>();
                let _ui = state.conn.lock().unwrap();
                state.cluster_gen.fetch_add(1, Ordering::SeqCst);
                db::set_face_clusters(&mut conn, &assignments)?;
            }
            // Re-derive the tentative folds against the fresh appearance layer
            // (see `auto_fold_confident`).
            let _ = auto_fold_confident(&conn)?;
            db::set_meta(&conn, RECLUSTER_FLAG, "1")?;
            db::set_meta(&conn, GROUPING_FLAG, "1")?;
            // Recompute the suggestion caches before the UI is told to reload.
            refresh_suggestion_cache(&app, &conn);
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("recluster failed: {e}");
        }
        let _ = app.emit("cluster-progress", ClusterProgress { running: false, fraction: 1.0 });
        reclustering.store(false, Ordering::SeqCst);
        // Anything requested while we ran — honor it now so nothing is dropped.
        // (A queued fold after a re-cluster is cheap belt-and-braces: the pass
        // above already re-derived folds, but a correction may have landed since.)
        if recluster_pending.swap(false, Ordering::SeqCst) {
            run_recluster(app_for_rerun);
        } else if fold_pending.swap(false, Ordering::SeqCst) {
            run_auto_fold(app_for_rerun);
        }
    });
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
                            // Wait for the pass — including one that got QUEUED
                            // because a fold snuck in between the check above and
                            // the call (recluster_pending) — before reloading the
                            // in-memory index; reloading early would resurrect
                            // pre-consolidation cluster ids for new faces.
                            let st = app.state::<AppState>();
                            let guard = st.reclustering.clone();
                            let queued = st.recluster_pending.clone();
                            drop(st);
                            while guard.load(Ordering::SeqCst) || queued.load(Ordering::SeqCst) {
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

            // One-time migration for the HEIC orientation fix: iPhone HEICs store
            // rotation in the container's `irot` box (not an EXIF tag), and earlier
            // builds detected faces before that transform was applied (orientation
            // handling changed across 1c955bf → dc5df8f → 6b811c9). Those faces' boxes
            // — and the crops they were embedded from — sit in the un-rotated space,
            // so boxes land off-target and the embeddings are poor. Re-detect every
            // HEIC from the current, correctly-oriented decode, dropping the stale
            // preview + crop caches (and re-queuing thumbnails) so nothing re-detects
            // on the old pixels. Skipped cleanly once done (or when there are no HEICs).
            if db::get_meta(&conn, HEIC_ORIENT_FLAG).ok().flatten().is_none() {
                let ids = db::heic_photo_ids(&conn).unwrap_or_default();
                let face_ids = db::face_ids_of_photos(&conn, &ids).unwrap_or_default();
                db::rearm_photos_for_redetect(&conn, &ids)?;
                for fid in &face_ids {
                    let _ = std::fs::remove_file(faces::face_crop_path(&faces_dir, *fid));
                }
                for id in &ids {
                    let _ = std::fs::remove_file(thumbs::preview_path(&preview_dir, *id));
                }
                db::set_meta(&conn, HEIC_ORIENT_FLAG, "1")?;
            }

            // Has the one-time migration off the old greedy clustering run yet?
            // And has the appearance layer been rebuilt pure since identity-centric
            // grouping landed (older auto-folds physically welded clusters)?
            // (Checked now, before `conn` moves into the shared state.)
            let needs_recluster = db::get_meta(&conn, RECLUSTER_FLAG).ok().flatten().is_none()
                || db::get_meta(&conn, GROUPING_FLAG).ok().flatten().is_none();

            // A download interrupted by a previous quit is no longer running;
            // reset those placeholders so they show as cloud again (and can be
            // re-fetched when next visible).
            db::set_status_many_where_downloading(&conn)?;

            // Backfill GPS for photos indexed before the Places map existed:
            // their EXIF was never checked for a fix (geo_scanned = 0). Local
            // files only — cloud originals get theirs after an on-demand
            // download, like capture dates do. EXIF header reads on a UTILITY
            // thread: a 30k-photo library takes minutes, once; on later
            // launches the worklist is empty and the thread exits immediately.
            {
                let db_path = db_path.clone();
                std::thread::spawn(move || {
                    background_qos();
                    let Ok(conn) = db::open(&db_path) else { return };
                    loop {
                        let batch = db::geo_backfill_batch(&conn, 500).unwrap_or_default();
                        if batch.is_empty() {
                            break;
                        }
                        for (id, path) in batch {
                            let gps = meta::read_exif_meta(std::path::Path::new(&path)).gps;
                            let _ = db::set_geo_scanned(&conn, id, gps);
                        }
                        // Yield between batches — this is idle-time work.
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                });
            }

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
            // worker pool + coordinator.
            let yunet = resolve_model(app.handle(), "yunet.onnx");
            let sface = resolve_model(app.handle(), "sface.onnx");
            spawn_face_workers(
                app.handle().clone(),
                db_path.clone(),
                preview_dir.clone(),
                yunet,
                sface,
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
                            let total = db::stats(&c).map(|s| s.0).unwrap_or(0);
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
                reclustering: Arc::new(AtomicBool::new(false)),
                recluster_pending: Arc::new(AtomicBool::new(false)),
                fold_pending: Arc::new(AtomicBool::new(false)),
                cluster_gen: Arc::new(AtomicI64::new(0)),
                suggestion_cache: Arc::new(Mutex::new(SuggestionCache::default())),
                recluster_epoch: Arc::new(AtomicU64::new(0)),
                review_active: Arc::new(AtomicBool::new(false)),
                recluster_deferred: Arc::new(AtomicBool::new(false)),
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
            get_clusters,
            name_cluster,
            merge_clusters,
            get_identity_growth,
            get_review_queue,
            get_cluster_generation,
            resolve_same_photo,
            set_review_active,
            absorb_clusters,
            reject_merge,
            not_this_person,
            not_these_people,
            not_this_person_many,
            name_faces,
            get_face_photo,
            get_geo_points,
            basemap_size,
            read_basemap_range,
            set_photo_favorite,
            set_photo_hidden,
            export_curation,
            import_curation,
            get_on_this_day,
            reset_face_decisions,
            cluster_debug,
            get_person_photos,
            get_person_looks,
            get_faces_in_photo,
            face_ids_for_photos,
            get_cluster_faces,
            confirm_faces_into_cluster,
            reassign_faces_to_cluster,
            reassign_faces_to_new_person,
            ignore_faces,
            detach_faces,
            undo_correction
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
