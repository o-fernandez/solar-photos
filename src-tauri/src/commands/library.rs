//! Library commands: stats, the timeline's paged reads + search, folder
//! (root) management, and viewport priority hints. Moved verbatim from lib.rs
//! — shared state, background work and the thumb protocol stay there.

use tauri::Emitter;

use crate::db::{self, Job};
use crate::{delete_cache_files, delete_face_crop_files, meta, rescan_all, AppState, ScanProgress};

/// The viewer info card's payload: file size + everything EXIF offers, read on
/// demand. `cloud` = the original is a placeholder we refuse to read (its bytes
/// would download), so only the DB-known basics are present — the card says so.
#[derive(serde::Serialize)]
pub(crate) struct PhotoExif {
    bytes: i64,
    cloud: bool,
    #[serde(flatten)]
    detail: meta::ExifDetail,
}

#[tauri::command]
pub(crate) fn get_photo_exif(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<Option<PhotoExif>, String> {
    let info = {
        let conn = state.conn.lock().unwrap();
        db::photo_file_info(&conn, id).map_err(|e| e.to_string())?
    };
    Ok(info.map(|(path, bytes, status)| {
        let cloud = status == db::STATUS_CLOUD || status == db::STATUS_DOWNLOADING;
        let detail = if cloud {
            meta::ExifDetail::default()
        } else {
            meta::read_exif_detail(std::path::Path::new(&path))
        };
        PhotoExif { bytes, cloud, detail }
    }))
}

/// Library counts for the header readout. On a repeat launch this returns the
/// already-indexed totals immediately, so the grid can render without a rescan.
#[derive(serde::Serialize)]
pub(crate) struct LibraryStats {
    total: i64,
    ready: i64,
    favorites: i64,
    hidden: i64,
}

#[tauri::command]
pub(crate) fn get_library_stats(state: tauri::State<'_, AppState>) -> Result<LibraryStats, String> {
    let conn = state.conn.lock().unwrap();
    let (total, ready, favorites, hidden) = db::stats(&conn).map_err(|e| e.to_string())?;
    Ok(LibraryStats { total, ready, favorites, hidden })
}

/// Fetch a contiguous window of photo rows (id + thumbnail status) under a
/// curation filter ("visible" | "favorites" | "hidden") and an optional search
/// query, in discovery or date order. The virtualized grid asks for only the
/// ranges it is about to display.
#[tauri::command]
pub(crate) fn get_photos_range(
    state: tauri::State<'_, AppState>,
    offset: i64,
    limit: i64,
    by_date: bool,
    filter: Option<String>,
    search: Option<String>,
) -> Result<Vec<db::PhotoRow>, String> {
    let f = db::PhotoFilter::parse(filter.as_deref().unwrap_or("visible"));
    let conn = state.conn.lock().unwrap();
    db::photos_range(&conn, offset, limit, by_date, f, normalized_search(&search))
        .map_err(|e| e.to_string())
}

/// How many photos a filter + search match — the grid's cell count while a
/// search narrows the timeline.
#[tauri::command]
pub(crate) fn count_photos(
    state: tauri::State<'_, AppState>,
    filter: Option<String>,
    search: Option<String>,
) -> Result<i64, String> {
    let f = db::PhotoFilter::parse(filter.as_deref().unwrap_or("visible"));
    let conn = state.conn.lock().unwrap();
    db::photos_count(&conn, f, normalized_search(&search)).map_err(|e| e.to_string())
}

/// A search argument worth passing down: non-empty after trimming.
fn normalized_search(search: &Option<String>) -> Option<&str> {
    search.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// "On this day" — photos taken on today's month-and-day in past years, for the
/// Home shelf. Empty when nothing was captured on this date.
#[tauri::command]
pub(crate) fn get_on_this_day(state: tauri::State<'_, AppState>) -> Result<Vec<db::PhotoRow>, String> {
    let conn = state.conn.lock().unwrap();
    db::on_this_day(&conn).map_err(|e| e.to_string())
}

/// Add a folder to the library: remember it as a root and scan it. Returns
/// immediately — the shared rescan coordinator walks it in the background. Using
/// that one coordinator is important: an independent scan generation could race
/// the full rescan's mark-and-sweep and have its newly inserted rows pruned.
#[tauri::command]
pub(crate) fn add_folder(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    {
        let conn = state.conn.lock().unwrap();
        db::add_root(&conn, &path).map_err(|e| e.to_string())?;
    }
    state.library_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    rescan_all(app);
    Ok(())
}

/// Reconcile the whole library with disk (add new, prune deleted) in the
/// background. Safe to call anytime; overlapping requests coalesce into one
/// follow-up pass with a fresh root snapshot.
#[tauri::command]
pub(crate) fn rescan(app: tauri::AppHandle) {
    rescan_all(app);
}

/// The folders the library is built from.
#[tauri::command]
pub(crate) fn list_roots(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    let conn = state.conn.lock().unwrap();
    db::list_roots(&conn).map_err(|e| e.to_string())
}

/// Remove a folder from the library: drop its photos and their cached files,
/// then tell the frontend the new total so it can refresh.
#[tauri::command]
pub(crate) fn remove_folder(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let removed = {
        let mut conn = state.conn.lock().unwrap();
        let ids = db::remove_root(&mut conn, &path).map_err(|e| e.to_string())?;
        // Crops first — they're found via the face rows about to go.
        let face_ids = db::face_ids_of_photos(&conn, &ids).map_err(|e| e.to_string())?;
        delete_face_crop_files(&state.faces_dir, &face_ids);
        db::delete_faces_for_photos(&conn, &ids).map_err(|e| e.to_string())?;
        ids
    };
    state.local_queue.cancel(&removed);
    state.cloud_queue.cancel(&removed);
    delete_cache_files(&state.cache_dir, &state.preview_dir, &removed);
    state.library_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let total = {
        let conn = state.conn.lock().unwrap();
        db::stats(&conn).map_err(|e| e.to_string())?.0
    };
    let _ = app.emit("scan-progress", ScanProgress { found: total, done: true });
    // If another scan still had the removed root in its snapshot, request a
    // follow-up pass; the coordinator coalesces it and prunes any reinsertion.
    rescan_all(app);
    Ok(())
}

/// Detail for the viewer chrome: filename + a timestamp (capture date when we
/// have it, else file mtime).
#[derive(serde::Serialize)]
pub(crate) struct PhotoDetail {
    filename: String,
    /// Full path on disk — backs the viewer's "Show in Finder".
    path: String,
    timestamp: i64,
    favorite: bool,
    hidden: bool,
}

#[tauri::command]
pub(crate) fn get_photo_detail(
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
pub(crate) fn set_visible_range(app: tauri::AppHandle, state: tauri::State<'_, AppState>, ids: Vec<i64>) {
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
    for (id, status, path, cache_key) in rows {
        if status == db::STATUS_CLOUD || status == db::STATUS_DOWNLOADING {
            cloud_jobs.push(Job { id, path, cache_key });
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
