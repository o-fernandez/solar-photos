//! Places commands: the bundled offline basemap (served by byte range) and the
//! geo points that populate the map.

use tauri::{AppHandle, Manager};

use crate::{db, AppState};

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
pub(crate) fn basemap_size(app: tauri::AppHandle) -> Result<u64, String> {
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
pub(crate) fn read_basemap_range(
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
pub(crate) struct GeoPoint {
    id: i64,
    lat: f64,
    lon: f64,
    ts: i64,
}

/// Every photo with a GPS fix — the Places map's whole dataset in one read.
/// Compact by design (four numbers a row): 100k points is a ~3MB payload, and
/// clustering happens client-side where the viewport lives (Principle 6).
#[tauri::command]
pub(crate) fn get_geo_points(state: tauri::State<'_, AppState>) -> Result<Vec<GeoPoint>, String> {
    let conn = state.conn.lock().unwrap();
    Ok(db::geo_points(&conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(id, lat, lon, ts)| GeoPoint { id, lat, lon, ts })
        .collect())
}
