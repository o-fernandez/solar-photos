//! Curation commands: the favorite / hidden flags (single and bulk) and their
//! export/import — the one slice of state that isn't re-derivable from the
//! photos themselves.

use crate::{db, AppState};

/// Toggle a photo's favorite star.
#[tauri::command]
pub(crate) fn set_photo_favorite(state: tauri::State<'_, AppState>, id: i64, favorite: bool) -> Result<(), String> {
    let conn = state.conn.lock().unwrap();
    db::set_favorite(&conn, id, favorite).map_err(|e| e.to_string())
}

/// Soft-archive (or restore) a photo — a flag only; the file is never touched.
#[tauri::command]
pub(crate) fn set_photo_hidden(state: tauri::State<'_, AppState>, id: i64, hidden: bool) -> Result<(), String> {
    let conn = state.conn.lock().unwrap();
    db::set_hidden(&conn, id, hidden).map_err(|e| e.to_string())
}

/// Toggle the favorite star on a whole selection at once.
#[tauri::command]
pub(crate) fn set_photos_favorite(
    state: tauri::State<'_, AppState>,
    ids: Vec<i64>,
    favorite: bool,
) -> Result<(), String> {
    let conn = state.conn.lock().unwrap();
    db::set_favorite_many(&conn, &ids, favorite).map_err(|e| e.to_string())
}

/// Soft-archive (or restore) a whole selection at once — flags only.
#[tauri::command]
pub(crate) fn set_photos_hidden(
    state: tauri::State<'_, AppState>,
    ids: Vec<i64>,
    hidden: bool,
) -> Result<(), String> {
    let conn = state.conn.lock().unwrap();
    db::set_hidden_many(&conn, &ids, hidden).map_err(|e| e.to_string())
}

/// One JSON line per flagged photo — the curation snapshot the user owns.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CurationEntry {
    path: String,
    favorite: bool,
    hidden: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CurationFile {
    /// Bumped if the shape ever changes; readers tolerate what they understand.
    version: u32,
    entries: Vec<CurationEntry>,
}

/// Write the favorites + hidden flags to a JSON file the user chooses, so their
/// curation outlives the app's cache directory (it's the one thing here that
/// isn't re-derivable from the photos). Keyed by path.
#[tauri::command]
pub(crate) fn export_curation(state: tauri::State<'_, AppState>, path: String) -> Result<usize, String> {
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
pub(crate) fn import_curation(state: tauri::State<'_, AppState>, path: String) -> Result<usize, String> {
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
