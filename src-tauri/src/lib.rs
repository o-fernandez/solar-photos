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

mod db;
mod meta;
mod scan;
mod thumbs;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::{Emitter, Manager};

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
    /// A single connection for the (UI-driven) command handlers.
    conn: Mutex<Connection>,
    /// Local-file thumbnail queue (drained eagerly).
    local_queue: Arc<ThumbQueue>,
    /// Cloud-file queue (fed on demand with what's currently visible).
    cloud_queue: Arc<ThumbQueue>,
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

/// Start scanning a folder. Returns immediately — the walk runs on a background
/// thread, registering photos in batches and emitting `scan-progress` events so
/// the grid grows live. This is what keeps the UI from freezing on a huge (or
/// cloud-backed) folder (Principle 1).
#[tauri::command]
fn scan_folder(app: tauri::AppHandle, state: tauri::State<'_, AppState>, path: String) {
    let db_path = state.db_path.clone();
    let queue = state.local_queue.clone();
    std::thread::spawn(move || {
        let progress = |found: i64, done: bool| {
            let _ = app.emit("scan-progress", ScanProgress { found, done });
        };
        if let Err(e) = scan::run_scan(&db_path, &path, queue, progress) {
            eprintln!("scan failed: {e}");
            let _ = app.emit("scan-progress", ScanProgress { found: 0, done: true });
        }
    });
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
///   * cloud-only photos among them are fetched on demand — we move them to
///     DOWNLOADING and feed the cloud queue with exactly the visible set, so we
///     download what the user is looking at and abandon what they scrolled past.
#[tauri::command]
fn set_visible_range(app: tauri::AppHandle, state: tauri::State<'_, AppState>, ids: Vec<i64>) {
    // Prioritize visible local thumbnails (ignores ids not in the local queue).
    state.local_queue.set_priority(ids.clone());

    // Figure out which visible photos are cloud-only and (re)feed the cloud lane.
    let conn = state.conn.lock().unwrap();
    let rows = match db::lookup(&conn, &ids) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut cloud_jobs: Vec<Job> = Vec::new();
    let mut newly_downloading: Vec<i64> = Vec::new();
    for (id, status, path) in rows {
        if status == db::STATUS_CLOUD || status == db::STATUS_DOWNLOADING {
            // Keep both freshly-cloud and already-downloading-but-still-visible
            // photos queued; the queue dedupes anything already in flight.
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

    state.cloud_queue.replace_pending(cloud_jobs);
    if !newly_downloading.is_empty() {
        let _ = app.emit("thumb-downloading", newly_downloading);
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
        .register_uri_scheme_protocol("thumb", |ctx, request| {
            use tauri::http::Response;

            let app = ctx.app_handle();
            let ok = |bytes: Vec<u8>| {
                Response::builder()
                    .header("Content-Type", "image/jpeg")
                    .header("Cache-Control", "no-cache")
                    .body(bytes)
                    .unwrap()
            };
            let not_found = || Response::builder().status(404).body(Vec::new()).unwrap();

            // Path is `/<id>` or `/preview/<id>`.
            let full = request.uri().path().trim_matches('/').to_string();
            let is_preview = full.starts_with("preview/");
            let id: Option<i64> = full.rsplit('/').next().and_then(|s| s.parse().ok());
            let id = match id {
                Some(id) => id,
                None => return not_found(),
            };

            let state = app.state::<AppState>();

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
        })
        .setup(|app| {
            // All cached state lives under the OS app-data directory — never
            // inside the user's photo folders (their originals stay pristine).
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db_path = data_dir.join("library.db");
            let cache_dir = data_dir.join("thumbnails");
            std::fs::create_dir_all(&cache_dir)?;
            let preview_dir = data_dir.join("previews");
            std::fs::create_dir_all(&preview_dir)?;

            let conn = db::open(&db_path)?;
            db::init(&conn)?;

            // A download interrupted by a previous quit is no longer running;
            // reset those placeholders so they show as cloud again (and can be
            // re-fetched when next visible).
            db::set_status_many_where_downloading(&conn)?;

            // Local pool: one worker per core minus one, so the machine (and UI)
            // stays responsive while indexing. At least one.
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2);
            let local_workers = cores.saturating_sub(1).max(1);

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
                db::STATUS_FAILED,
                false,
                notify.clone(),
            );
            thumbs::spawn_workers(
                CLOUD_WORKERS,
                cloud_queue.clone(),
                db_path.clone(),
                cache_dir.clone(),
                db::STATUS_CLOUD,
                true,
                notify,
            );

            // Resume local thumbnails left pending from a previous session.
            let pending = db::pending_jobs(&conn)?;
            local_queue.enqueue(pending);

            app.manage(AppState {
                db_path,
                cache_dir,
                preview_dir,
                conn: Mutex::new(conn),
                local_queue,
                cloud_queue,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_library_stats,
            get_photos_range,
            get_photo_detail,
            scan_folder,
            set_visible_range
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
