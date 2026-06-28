//! Solar Photos — Rust backend.
//!
//! Responsibilities split across modules:
//!   * `db`     — SQLite: the list of photos and each thumbnail's status.
//!   * `scan`   — walk a folder and record supported images (cheap, no decode).
//!   * `thumbs` — the priority queue + worker pool that decode & cache thumbs.
//!
//! This file wires those together: it owns shared state, exposes commands the
//! React frontend can call, serves cached thumbnails over a custom `thumb://`
//! URI scheme, and starts the background workers.

mod db;
mod scan;
mod thumbs;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::{Emitter, Manager};

use thumbs::ThumbQueue;

/// Application-wide state, shared across command handlers and the protocol.
struct AppState {
    /// Directory holding cached thumbnail JPEGs.
    cache_dir: PathBuf,
    /// A single connection for the (UI-driven) command handlers.
    conn: Mutex<Connection>,
    /// The shared thumbnail work-queue.
    queue: Arc<ThumbQueue>,
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

/// Fetch a contiguous window of photo rows (id + thumbnail status), ordered by
/// path. The virtualized grid asks for only the ranges it is about to display.
#[tauri::command]
fn get_photos_range(
    state: tauri::State<'_, AppState>,
    offset: i64,
    limit: i64,
) -> Result<Vec<db::PhotoRow>, String> {
    let conn = state.conn.lock().unwrap();
    db::photos_range(&conn, offset, limit).map_err(|e| e.to_string())
}

/// Scan a folder: record supported images and queue any that need thumbnails.
/// Returns the new library total so the grid can lay out its cells at once.
/// The actual thumbnailing happens afterwards on the worker threads.
#[tauri::command]
fn scan_folder(state: tauri::State<'_, AppState>, path: String) -> Result<i64, String> {
    let mut conn = state.conn.lock().unwrap();
    let jobs = scan::scan(&mut conn, &path).map_err(|e| e.to_string())?;
    let (total, _) = db::stats(&conn).map_err(|e| e.to_string())?;
    drop(conn);
    state.queue.enqueue(jobs);
    Ok(total)
}

/// Tell the backend which photos are currently on screen so their thumbnails
/// jump the queue (Principle 3). Called, debounced, on every scroll.
#[tauri::command]
fn set_visible_range(state: tauri::State<'_, AppState>, ids: Vec<i64>) {
    state.queue.set_priority(ids);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // Serve cached thumbnails directly from disk. The frontend points an
        // <img> at `thumb://localhost/<id>` and we stream back the JPEG bytes.
        // No base64, no DB lookup — just id -> file path -> bytes.
        .register_uri_scheme_protocol("thumb", |ctx, request| {
            use tauri::http::Response;

            let app = ctx.app_handle();
            let not_found = || Response::builder().status(404).body(Vec::new()).unwrap();

            // URI looks like `thumb://localhost/<id>`; take the last path segment.
            let id: Option<i64> = request
                .uri()
                .path()
                .trim_matches('/')
                .split('/')
                .last()
                .and_then(|s| s.parse().ok());

            let id = match id {
                Some(id) => id,
                None => return not_found(),
            };

            let state = app.state::<AppState>();
            let path = thumbs::thumb_path(&state.cache_dir, id);
            match std::fs::read(&path) {
                Ok(bytes) => Response::builder()
                    .header("Content-Type", "image/jpeg")
                    .header("Cache-Control", "no-cache")
                    .body(bytes)
                    .unwrap(),
                Err(_) => not_found(),
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

            let conn = db::open(&db_path)?;
            db::init(&conn)?;

            // One worker per core, minus one, so the machine (and UI) stays
            // responsive while indexing. At least one worker.
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2);
            let workers = cores.saturating_sub(1).max(1);

            let queue = ThumbQueue::new();

            // Resume any thumbnails left pending from a previous session.
            let pending = db::pending_jobs(&conn)?;

            // Workers emit `thumb-ready` so the frontend can refresh one cell.
            let app_handle = app.handle().clone();
            let notify = move |id: i64| {
                let _ = app_handle.emit("thumb-ready", id);
            };
            thumbs::spawn_workers(workers, queue.clone(), db_path.clone(), cache_dir.clone(), notify);
            queue.enqueue(pending);

            app.manage(AppState {
                cache_dir,
                conn: Mutex::new(conn),
                queue,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_library_stats,
            get_photos_range,
            scan_folder,
            set_visible_range
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
